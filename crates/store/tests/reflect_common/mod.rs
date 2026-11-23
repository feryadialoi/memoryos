//! Reflect batch tests shared by both backends (docs/SPEC.md section 8, Insights, "Reflect
//! batch"). Each backend's test file supplies a `Harness`: the store, and a direct SQL insert for
//! insight fixtures.

use chrono::{DateTime, TimeZone, Utc};
use memoryos_core::{
    Error, FactOp, FactStatus, Insight, InsightLevel, InsightSourceKind, InsightSourceRef,
    InsightStatus, MemoryStore, NewEntity, NewEpisode, NewFact, NewInsight, ReflectBatch,
    RequestContext, Scope, SourceChange, StaleSource, Trust, EMBEDDING_DIMS, REFLECT_BATCH_MAX,
    REFLECT_MIN_FACTS_DEFAULT, REFLECT_TOPIC_FACTS_MAX,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;
pub type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const MIN: i64 = REFLECT_MIN_FACTS_DEFAULT;

pub trait Harness {
    type Store: MemoryStore;
    fn store(&self) -> &Self::Store;
    /// Inserts a context insight of trust `agent` citing `facts`, bypassing the store's checks.
    /// A stale one gets a `stale_reason` naming its first fact as invalidated.
    async fn insert_insight(
        &self,
        tenant: &str,
        scope: &Scope,
        status: InsightStatus,
        facts: &[Uuid],
    ) -> Res<Uuid>;
    /// Moves the fact's `expires_at` a minute into the past.
    async fn expire_fact(&self, id: Uuid) -> Res<()>;
}

pub fn ctx(tenant: &str) -> RequestContext {
    RequestContext {
        tenant: tenant.into(),
        actor: "tester".into(),
    }
}

pub fn scope() -> Scope {
    Scope::user("u1")
}

fn date(year: i32, month: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, 1, 0, 0, 0)
        .single()
        .expect("valid date")
}

pub fn axis(i: usize) -> Vec<f32> {
    let mut v = vec![0.0; EMBEDDING_DIMS];
    v[i] = 1.0;
    v
}

/// The stale reason a fixture gets, in the stored JSON form.
pub fn stale_reason(fact: Uuid) -> serde_json::Value {
    json!([{"kind": "fact", "id": fact, "change": "invalidated"}])
}

fn digest(ids: &[Uuid]) -> String {
    let mut ids = ids.to_vec();
    ids.sort();
    let text: String = ids.iter().map(|id| format!("{id}\n")).collect();
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn is_invalid<T>(result: memoryos_core::Result<T>) -> bool {
    matches!(result, Err(Error::InvalidInput(_)))
}

async fn entity<S: MemoryStore>(store: &S, c: &RequestContext, s: &Scope, name: &str) -> Res<Uuid> {
    Ok(store
        .upsert_entity(
            c,
            NewEntity {
                scope: s.clone(),
                kind: "thing".into(),
                name: name.into(),
                aliases: vec![],
                embedding: axis(1),
            },
        )
        .await?
        .id)
}

fn new_fact(s: &Scope, subject_id: Uuid, statement: &str) -> NewFact {
    NewFact {
        scope: s.clone(),
        subject_id,
        predicate: "says".into(),
        object_id: None,
        statement: statement.into(),
        embedding: axis(0),
        trust: Trust::User,
        status: FactStatus::Active,
        valid_from: date(2020, 1),
        valid_to: None,
        expires_at: None,
    }
}

async fn commit<S: MemoryStore>(
    store: &S,
    c: &RequestContext,
    s: &Scope,
    trust: Trust,
    op: FactOp,
) -> Res<Uuid> {
    let (episode, _) = store
        .claim_episode(
            c,
            NewEpisode {
                scope: s.clone(),
                source: "test".into(),
                trust,
                content: "source text".into(),
                occurred_at: None,
                metadata: json!({}),
                idempotency_key: None,
            },
        )
        .await?;
    Ok(store.apply_commit(c, episode.id, vec![op]).await?[0])
}

async fn add<S: MemoryStore>(store: &S, c: &RequestContext, fact: NewFact) -> Res<Uuid> {
    let s = fact.scope.clone();
    let trust = fact.trust;
    commit(store, c, &s, trust, FactOp::Add(fact)).await
}

/// Adds `n` facts about `subject`, returning their ids oldest first.
async fn add_n<S: MemoryStore>(
    store: &S,
    c: &RequestContext,
    s: &Scope,
    subject: Uuid,
    n: usize,
) -> Res<Vec<Uuid>> {
    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        ids.push(
            add(
                store,
                c,
                new_fact(s, subject, &format!("{subject} fact {i}")),
            )
            .await?,
        );
    }
    Ok(ids)
}

fn topic_keys(batch: &ReflectBatch) -> Vec<Uuid> {
    batch.topics.iter().map(|t| t.topic_key).collect()
}

fn newest_first(ids: &[Uuid]) -> Vec<Uuid> {
    ids.iter().rev().copied().collect()
}

pub async fn a_topic_needs_min_facts_and_shows_the_newest_30<H: Harness>(h: &H) -> TestResult {
    let store = h.store();
    let c = ctx("t1");
    let s = scope();
    let two = entity(store, &c, &s, "two").await?;
    let three = entity(store, &c, &s, "three").await?;
    let many = entity(store, &c, &s, "many").await?;
    add_n(store, &c, &s, two, 2).await?;
    let three_ids = add_n(store, &c, &s, three, 3).await?;
    let many_ids = add_n(store, &c, &s, many, REFLECT_TOPIC_FACTS_MAX + 5).await?;

    let batch = store.reflect_batch(&c, &s, 5, MIN).await?;
    assert!(batch.stale.is_empty());
    assert_eq!(topic_keys(&batch), vec![three, many]);
    let topic = &batch.topics[0];
    assert_eq!(topic.subject, "three");
    let shown: Vec<Uuid> = topic.facts.iter().map(|f| f.id).collect();
    assert_eq!(shown, newest_first(&three_ids));
    assert_eq!(topic.fact_set, digest(&three_ids));
    assert!(topic.insights.is_empty());

    let capped = &batch.topics[1];
    let shown: Vec<Uuid> = capped.facts.iter().map(|f| f.id).collect();
    let newest = &many_ids[5..];
    assert_eq!(shown, newest_first(newest));
    assert_eq!(capped.fact_set, digest(newest));

    // A lower threshold offers the two-fact subject too, first, since its facts are oldest.
    let batch = store.reflect_batch(&c, &s, 5, 2).await?;
    assert_eq!(topic_keys(&batch), vec![two, three, many]);
    Ok(())
}

pub async fn only_current_active_uncited_facts_count<H: Harness>(h: &H) -> TestResult {
    let store = h.store();
    let c = ctx("t1");
    let s = scope();
    let subject = entity(store, &c, &s, "subject").await?;
    let kept = add_n(store, &c, &s, subject, 2).await?;

    // Not current: closed by a supersede (its successor counts), invalidated, expired.
    let closed = add(store, &c, new_fact(&s, subject, "closed")).await?;
    let mut successor = new_fact(&s, subject, "successor");
    successor.valid_from = date(2021, 1);
    let successor = commit(
        store,
        &c,
        &s,
        Trust::User,
        FactOp::Supersede {
            old_fact_id: closed,
            new: successor,
        },
    )
    .await?;
    let invalidated = add(store, &c, new_fact(&s, subject, "invalidated")).await?;
    store.invalidate_fact(&c, invalidated, "wrong").await?;
    let mut expired = new_fact(&s, subject, "expired");
    expired.expires_at = Some(date(2021, 1));
    add(store, &c, expired).await?;
    // Not active: quarantined.
    let mut held = new_fact(&s, subject, "held");
    held.trust = Trust::External;
    held.status = FactStatus::Quarantined;
    add(store, &c, held).await?;
    // Another scope.
    let other = Scope::user("u2");
    let other_subject = entity(store, &c, &other, "subject").await?;
    add_n(store, &c, &other, other_subject, 3).await?;

    // Cited by insights that no longer stand: still uncited for reflect.
    let mut free = Vec::new();
    for status in [
        InsightStatus::Retired,
        InsightStatus::Rejected,
        InsightStatus::Superseded,
    ] {
        let fact = add(store, &c, new_fact(&s, subject, status.as_str())).await?;
        h.insert_insight("t1", &s, status, &[fact, kept[0]]).await?;
        free.push(fact);
    }

    let batch = store.reflect_batch(&c, &s, 5, MIN).await?;
    assert_eq!(topic_keys(&batch), vec![subject]);
    let mut expected: Vec<Uuid> = kept.clone();
    expected.push(successor);
    expected.extend(&free);
    let shown: Vec<Uuid> = batch.topics[0].facts.iter().map(|f| f.id).collect();
    assert_eq!(shown, newest_first(&expected));
    // Retired, rejected and superseded insights are not offered as the subject's insights.
    assert!(batch.topics[0].insights.is_empty());

    // Each standing status takes its cited facts out of the topic.
    for (status, cited) in [
        (InsightStatus::Active, [kept[0], kept[1]]),
        (InsightStatus::Stale, [successor, free[0]]),
        (InsightStatus::Quarantined, [free[1], free[2]]),
    ] {
        h.insert_insight("t1", &s, status, &cited).await?;
        let batch = store.reflect_batch(&c, &s, 5, 2).await?;
        let shown: Vec<Uuid> = batch
            .topics
            .iter()
            .flat_map(|t| t.facts.iter().map(|f| f.id))
            .collect();
        assert!(
            cited.iter().all(|id| !shown.contains(id)),
            "{status:?} insight's facts still offered"
        );
    }
    assert!(store.reflect_batch(&c, &s, 5, 2).await?.topics.is_empty());
    Ok(())
}

pub async fn a_topic_carries_the_subjects_active_insights<H: Harness>(h: &H) -> TestResult {
    let store = h.store();
    let c = ctx("t1");
    let s = scope();
    let subject = entity(store, &c, &s, "subject").await?;
    let other = entity(store, &c, &s, "other").await?;
    let cited = add_n(store, &c, &s, subject, 2).await?;
    let other_cited = add_n(store, &c, &s, other, 2).await?;
    add_n(store, &c, &s, subject, 3).await?;
    let older = h
        .insert_insight("t1", &s, InsightStatus::Active, &cited)
        .await?;
    let newer = h
        .insert_insight("t1", &s, InsightStatus::Active, &[cited[1], other_cited[0]])
        .await?;
    h.insert_insight("t1", &s, InsightStatus::Active, &other_cited)
        .await?;
    h.insert_insight("t1", &s, InsightStatus::Stale, &[cited[0], cited[1]])
        .await?;

    let batch = store.reflect_batch(&c, &s, 5, MIN).await?;
    assert_eq!(topic_keys(&batch), vec![subject]);
    let insights = &batch.topics[0].insights;
    let ids: Vec<Uuid> = insights.iter().map(|i| i.id).collect();
    assert_eq!(ids, vec![newer, older], "active only, newest first");
    let mut sources: Vec<InsightSourceRef> = cited
        .iter()
        .map(|&id| InsightSourceRef {
            kind: InsightSourceKind::Fact,
            id,
        })
        .collect();
    sources.sort();
    assert_eq!(insights[1].sources, sources);
    assert_eq!(insights[1].status, InsightStatus::Active);
    assert_eq!(insights[1].scope, s);
    Ok(())
}

pub async fn a_decline_holds_until_the_fact_set_changes<H: Harness>(h: &H) -> TestResult {
    let store = h.store();
    let c = ctx("t1");
    let s = scope();
    let first = entity(store, &c, &s, "first").await?;
    let second = entity(store, &c, &s, "second").await?;
    add_n(store, &c, &s, first, 3).await?;
    add_n(store, &c, &s, second, 3).await?;

    let batch = store.reflect_batch(&c, &s, 5, MIN).await?;
    assert_eq!(topic_keys(&batch), vec![first, second]);
    let declined = batch.topics[0].fact_set.clone();
    store.decline_topic(&c, &s, first, &declined).await?;
    // Declining twice is an upsert, and a decline in another scope changes nothing here.
    store.decline_topic(&c, &s, first, &declined).await?;
    store
        .decline_topic(&c, &Scope::user("u2"), second, &batch.topics[1].fact_set)
        .await?;
    assert_eq!(
        topic_keys(&store.reflect_batch(&c, &s, 5, MIN).await?),
        vec![second]
    );

    // A new fact changes the fact set, so the topic comes back.
    add(store, &c, new_fact(&s, first, "news")).await?;
    let batch = store.reflect_batch(&c, &s, 5, MIN).await?;
    assert_eq!(topic_keys(&batch), vec![first, second]);
    let changed = batch.topics[0].fact_set.clone();
    assert_ne!(changed, declined);

    // The upsert replaces the stored fact set.
    store.decline_topic(&c, &s, first, &changed).await?;
    assert_eq!(
        topic_keys(&store.reflect_batch(&c, &s, 5, MIN).await?),
        vec![second]
    );
    Ok(())
}

pub async fn declined_topics_do_not_use_up_the_limit<H: Harness>(h: &H) -> TestResult {
    let store = h.store();
    let c = ctx("t1");
    let s = scope();
    // More subjects than one chunk, so collecting skips into a second round trip.
    let mut subjects = Vec::new();
    for i in 0..(REFLECT_BATCH_MAX as usize + 3) {
        let subject = entity(store, &c, &s, &format!("subject {i:02}")).await?;
        add_n(store, &c, &s, subject, 3).await?;
        subjects.push(subject);
    }
    let all = store.reflect_batch(&c, &s, REFLECT_BATCH_MAX, MIN).await?;
    assert_eq!(topic_keys(&all), subjects[..REFLECT_BATCH_MAX as usize]);
    for topic in &all.topics[..REFLECT_BATCH_MAX as usize - 1] {
        store
            .decline_topic(&c, &s, topic.topic_key, &topic.fact_set)
            .await?;
    }
    let rest = store.reflect_batch(&c, &s, 3, MIN).await?;
    let skipped = REFLECT_BATCH_MAX as usize - 1;
    assert_eq!(topic_keys(&rest), subjects[skipped..skipped + 3]);
    Ok(())
}

pub async fn the_order_is_deterministic_and_limited<H: Harness>(h: &H) -> TestResult {
    let store = h.store();
    let c = ctx("t1");
    let s = scope();
    let a = entity(store, &c, &s, "a").await?;
    let b = entity(store, &c, &s, "b").await?;
    let d = entity(store, &c, &s, "d").await?;
    // b's first fact is the oldest, then d's, then a's.
    add(store, &c, new_fact(&s, b, "b0")).await?;
    add(store, &c, new_fact(&s, d, "d0")).await?;
    add_n(store, &c, &s, a, 3).await?;
    add_n(store, &c, &s, d, 2).await?;
    add_n(store, &c, &s, b, 2).await?;

    let batch = store.reflect_batch(&c, &s, 5, MIN).await?;
    assert_eq!(topic_keys(&batch), vec![b, d, a]);
    let again = store.reflect_batch(&c, &s, 5, MIN).await?;
    assert_eq!(serde_json::to_value(&batch)?, serde_json::to_value(&again)?);
    assert_eq!(
        topic_keys(&store.reflect_batch(&c, &s, 2, MIN).await?),
        vec![b, d]
    );
    assert_eq!(
        topic_keys(&store.reflect_batch(&c, &s, 1, MIN).await?),
        vec![b]
    );
    Ok(())
}

pub async fn inputs_are_validated<H: Harness>(h: &H) -> TestResult {
    let store = h.store();
    let c = ctx("t1");
    let s = scope();
    assert!(is_invalid(store.reflect_batch(&c, &s, 0, MIN).await));
    assert!(is_invalid(
        store
            .reflect_batch(&c, &s, REFLECT_BATCH_MAX + 1, MIN)
            .await
    ));
    assert!(is_invalid(store.reflect_batch(&c, &s, 5, 1).await));
    assert!(is_invalid(store.reflect_batch(&ctx(""), &s, 5, MIN).await));
    let id = Uuid::now_v7();
    for bad in ["", "abc", &"A".repeat(64), &"g".repeat(64), &"a".repeat(65)] {
        assert!(
            is_invalid(store.decline_topic(&c, &s, id, bad).await),
            "{bad}"
        );
    }
    assert!(is_invalid(
        store.decline_topic(&ctx(""), &s, id, &"a".repeat(64)).await
    ));
    Ok(())
}

pub async fn tenants_are_isolated<H: Harness>(h: &H) -> TestResult {
    let store = h.store();
    let a = ctx("tenant-a");
    let b = ctx("tenant-b");
    let s = scope();
    let subject_a = entity(store, &a, &s, "subject").await?;
    let facts_a = add_n(store, &a, &s, subject_a, 3).await?;
    let subject_b = entity(store, &b, &s, "subject").await?;
    let facts_b = add_n(store, &b, &s, subject_b, 4).await?;

    let batch_a = store.reflect_batch(&a, &s, 5, MIN).await?;
    assert_eq!(topic_keys(&batch_a), vec![subject_a]);
    assert_eq!(batch_a.topics[0].fact_set, digest(&facts_a));
    let batch_b = store.reflect_batch(&b, &s, 5, MIN).await?;
    assert_eq!(topic_keys(&batch_b), vec![subject_b]);
    assert_eq!(batch_b.topics[0].fact_set, digest(&facts_b));

    // A's insight and decline change nothing for B, even naming B's facts and topic.
    h.insert_insight("tenant-a", &s, InsightStatus::Active, &facts_b[..2])
        .await?;
    store
        .decline_topic(&a, &s, subject_b, &batch_b.topics[0].fact_set)
        .await?;
    let again = store.reflect_batch(&b, &s, 5, MIN).await?;
    assert_eq!(topic_keys(&again), vec![subject_b]);
    assert_eq!(again.topics[0].facts.len(), 4);
    assert!(again.topics[0].insights.is_empty());

    // B's decline hides B's topic only.
    store
        .decline_topic(&b, &s, subject_b, &batch_b.topics[0].fact_set)
        .await?;
    assert!(store.reflect_batch(&b, &s, 5, MIN).await?.topics.is_empty());
    assert_eq!(
        topic_keys(&store.reflect_batch(&a, &s, 5, MIN).await?),
        vec![subject_a]
    );
    Ok(())
}

fn fact_ref(id: Uuid) -> InsightSourceRef {
    InsightSourceRef {
        kind: InsightSourceKind::Fact,
        id,
    }
}

/// Writes an active context insight in an agent scope through `put_insight`.
async fn write<S: MemoryStore>(
    store: &S,
    c: &RequestContext,
    s: &Scope,
    facts: &[Uuid],
) -> Res<Insight> {
    let insight = store
        .put_insight(
            c,
            NewInsight {
                scope: s.clone(),
                level: InsightLevel::Context,
                statement: "what it means".into(),
                embedding: axis(2),
                trust: Trust::Agent,
                sources: facts.iter().copied().map(fact_ref).collect(),
                supersedes: None,
                written_by: "agent-key".into(),
            },
        )
        .await?;
    assert_eq!(insight.status, InsightStatus::Active);
    Ok(insight)
}

fn stale_ids(batch: &ReflectBatch) -> Vec<Uuid> {
    batch.stale.iter().map(|d| d.insight.id).collect()
}

pub async fn stale_insights_come_first_oldest_first_with_current_sources<H: Harness>(
    h: &H,
) -> TestResult {
    let store = h.store();
    let c = ctx("t1");
    let s = Scope::agent("a1");
    let subject = entity(store, &c, &s, "subject").await?;
    let f = add_n(store, &c, &s, subject, 7).await?;
    let older = write(store, &c, &s, &f[0..2]).await?;
    let newer = write(store, &c, &s, &f[2..4]).await?;
    let fine = write(store, &c, &s, &f[4..6]).await?;
    // The newer one goes stale first: the order is by when each was written.
    store.invalidate_fact(&c, f[3], "wrong").await?;
    store.invalidate_fact(&c, f[0], "wrong").await?;
    // A stale insight of another scope is not offered here.
    let other = Scope::agent("a2");
    let other_subject = entity(store, &c, &other, "subject").await?;
    let o = add_n(store, &c, &other, other_subject, 2).await?;
    write(store, &c, &other, &o).await?;
    store.invalidate_fact(&c, o[0], "wrong").await?;

    let batch = store.reflect_batch(&c, &s, 5, 2).await?;
    assert_eq!(stale_ids(&batch), vec![older.id, newer.id]);
    let first = &batch.stale[0];
    assert_eq!(first.insight.status, InsightStatus::Stale);
    assert_eq!(
        first.insight.stale_reason,
        Some(vec![StaleSource {
            source: fact_ref(f[0]),
            change: SourceChange::Invalidated,
        }])
    );
    let mut views: Vec<(Uuid, Option<String>, bool)> = first
        .source_views
        .iter()
        .map(|v| (v.source.id, v.statement.clone(), v.current))
        .collect();
    views.sort();
    let mut expected = vec![
        (f[0], Some(format!("{subject} fact 0")), false),
        (f[1], Some(format!("{subject} fact 1")), true),
    ];
    expected.sort();
    assert_eq!(views, expected);
    assert!(first.history.is_empty());

    // Stale insights still cite their facts, so only f[6] is uncited: no topic at 2.
    assert!(batch.topics.is_empty());
    add(store, &c, new_fact(&s, subject, "more")).await?;
    let batch = store.reflect_batch(&c, &s, 5, 2).await?;
    assert_eq!(topic_keys(&batch), vec![subject]);
    // The active insight citing the subject's facts comes with the topic.
    let carried: Vec<Uuid> = batch.topics[0].insights.iter().map(|i| i.id).collect();
    assert_eq!(carried, vec![fine.id]);

    // Stale insights fill the limit before topics.
    let batch = store.reflect_batch(&c, &s, 1, 2).await?;
    assert_eq!(stale_ids(&batch), vec![older.id]);
    assert!(batch.topics.is_empty());
    let batch = store.reflect_batch(&c, &s, 2, 2).await?;
    assert_eq!(stale_ids(&batch), vec![older.id, newer.id]);
    assert!(batch.topics.is_empty());
    let batch = store.reflect_batch(&c, &s, 3, 2).await?;
    assert_eq!(batch.stale.len(), 2);
    assert_eq!(topic_keys(&batch), vec![subject]);

    // Another tenant sees none of it.
    let batch = store.reflect_batch(&ctx("t2"), &s, 5, 2).await?;
    assert!(batch.stale.is_empty() && batch.topics.is_empty());
    Ok(())
}

pub async fn the_batch_marks_expired_sources_stale_first<H: Harness>(h: &H) -> TestResult {
    let store = h.store();
    let c = ctx("t1");
    let s = Scope::agent("a1");
    let subject = entity(store, &c, &s, "subject").await?;
    let f = add_n(store, &c, &s, subject, 2).await?;
    let insight = write(store, &c, &s, &f).await?;
    assert!(store.reflect_batch(&c, &s, 5, 2).await?.stale.is_empty());

    h.expire_fact(f[1]).await?;
    let batch = store.reflect_batch(&c, &s, 5, 2).await?;
    assert_eq!(stale_ids(&batch), vec![insight.id]);
    assert_eq!(
        batch.stale[0].insight.stale_reason,
        Some(vec![StaleSource {
            source: fact_ref(f[1]),
            change: SourceChange::Expired,
        }])
    );
    let expired = batch.stale[0]
        .source_views
        .iter()
        .find(|v| v.source.id == f[1])
        .expect("expired source");
    assert!(!expired.current);
    Ok(())
}
