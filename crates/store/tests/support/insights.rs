//! Insight store tests shared by the PostgreSQL and SQLite suites (docs/SPEC.md section 8,
//! Insights). Each backend implements `Harness` and expands `insight_tests!` once, so both run the
//! same contract. Everything goes through `MemoryStore`, except counts of side tables and moving a
//! fact's expiry into the past, which need direct SQL.

use chrono::{DateTime, TimeZone, Utc};
use memoryos_core::{
    source_requires_quarantine, Error, FactOp, FactStatus, Insight, InsightFilter, InsightLevel,
    InsightSourceKind, InsightSourceRef, InsightStatus, MemoryStore, NewEntity, NewEpisode,
    NewFact, NewInsight, RequestContext, Scope, ScopeSummary, SourceChange, StaleSource, Trust,
    EMBEDDING_DIMS,
};
use serde_json::json;
use uuid::Uuid;

pub(crate) type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub(crate) trait Harness {
    fn store(&self) -> &dyn MemoryStore;
    /// One integer from direct SQL, valid on both backends.
    async fn count(&self, sql: &str) -> Res<i64>;
    /// Moves the fact's `expires_at` a minute into the past.
    async fn expire_fact(&self, id: Uuid) -> Res<()>;
    async fn summaries(&self, ctx: &RequestContext) -> Res<Vec<ScopeSummary>>;
}

/// Expands one `#[tokio::test]` per shared test. `$make` is an expression yielding a future of
/// `Res<impl Harness>`.
macro_rules! insight_tests {
    ($make:expr) => {
        insight_tests!(@each $make;
            insight_write_reads_back_lists_and_audits,
            insight_trust_and_status_follow_the_gate,
            insight_bad_input_is_refused,
            insight_a_superseded_fact_makes_citing_insights_stale,
            insight_supersede_with_existing_makes_citing_insights_stale,
            insight_approval_closing_a_head_makes_citing_insights_stale,
            insight_invalidation_makes_citing_insights_stale_and_reasons_merge,
            insight_purge_marks_deleted_and_relinked_sources,
            insight_purge_requarantine_and_lowered_trust_make_insights_stale,
            insight_changes_carry_up_every_level,
            insight_supersede_and_retire_close_and_carry_up,
            insight_expiry_is_marked_lazily_on_reads,
            insight_confirm_reactivates_a_stale_insight,
            insight_quarantine_approve_closes_the_pending_supersede,
            insight_approve_turns_stale_when_a_source_changed_meanwhile,
            insight_search_returns_only_active_insights_of_the_scopes,
            insight_purge_scope_deletes_and_summaries_count_active,
            insight_another_tenant_sees_and_changes_nothing
        );
    };
    (@each $make:expr; $($name:ident),* $(,)?) => {
        $(
            #[tokio::test]
            async fn $name() -> TestResult {
                let harness = $make.await?;
                insight_suite::$name(&harness).await
            }
        )*
    };
}

// ---- Helpers ---------------------------------------------------------------------------------

fn ctx(tenant: &str) -> RequestContext {
    RequestContext {
        tenant: tenant.into(),
        actor: "tester".into(),
    }
}

fn agent() -> Scope {
    Scope::agent("a1")
}

fn date(year: i32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, 1, 1, 0, 0, 0)
        .single()
        .expect("valid date")
}

fn axis(i: usize) -> Vec<f32> {
    let mut v = vec![0.0; EMBEDDING_DIMS];
    v[i] = 1.0;
    v
}

fn f(id: Uuid) -> InsightSourceRef {
    InsightSourceRef {
        kind: InsightSourceKind::Fact,
        id,
    }
}

fn i(id: Uuid) -> InsightSourceRef {
    InsightSourceRef {
        kind: InsightSourceKind::Insight,
        id,
    }
}

fn stale(source: InsightSourceRef, change: SourceChange) -> StaleSource {
    StaleSource { source, change }
}

fn is_invalid<T: std::fmt::Debug>(r: Result<T, Error>) -> bool {
    matches!(r, Err(Error::InvalidInput(_)))
}

fn is_not_found<T: std::fmt::Debug>(r: Result<T, Error>) -> bool {
    matches!(r, Err(Error::NotFound(_)))
}

async fn subject<H: Harness>(h: &H, c: &RequestContext, scope: &Scope) -> Res<Uuid> {
    let entity = h
        .store()
        .upsert_entity(
            c,
            NewEntity {
                scope: scope.clone(),
                kind: "person".into(),
                name: "ada".into(),
                aliases: vec![],
                embedding: axis(1),
            },
        )
        .await?;
    Ok(entity.id)
}

async fn episode<H: Harness>(h: &H, c: &RequestContext, scope: &Scope, trust: Trust) -> Res<Uuid> {
    let (episode, _) = h
        .store()
        .claim_episode(
            c,
            NewEpisode {
                scope: scope.clone(),
                source: "test".into(),
                trust,
                content: "source text".into(),
                occurred_at: None,
                metadata: json!({}),
                idempotency_key: None,
            },
        )
        .await?;
    Ok(episode.id)
}

/// A fact whose predicate is its statement, so no two facts share subject and predicate. Its
/// status follows the source rule.
fn new_fact(
    scope: &Scope,
    subject: Uuid,
    statement: &str,
    trust: Trust,
    from: DateTime<Utc>,
) -> NewFact {
    NewFact {
        scope: scope.clone(),
        subject_id: subject,
        predicate: statement.into(),
        object_id: None,
        statement: statement.into(),
        embedding: axis(0),
        trust,
        status: if source_requires_quarantine(scope, trust) {
            FactStatus::Quarantined
        } else {
            FactStatus::Active
        },
        valid_from: from,
        valid_to: None,
        expires_at: None,
    }
}

/// Adds one fact through its own episode. Returns (episode, fact).
async fn fact<H: Harness>(
    h: &H,
    c: &RequestContext,
    scope: &Scope,
    subject: Uuid,
    statement: &str,
    trust: Trust,
) -> Res<(Uuid, Uuid)> {
    let ep = episode(h, c, scope, trust).await?;
    let ids = h
        .store()
        .apply_commit(
            c,
            ep,
            vec![FactOp::Add(new_fact(
                scope,
                subject,
                statement,
                trust,
                date(2020),
            ))],
        )
        .await?;
    Ok((ep, ids[0]))
}

/// Supersedes `old` with a new fact through its own episode. Returns (episode, fact).
#[allow(clippy::too_many_arguments)]
async fn supersede<H: Harness>(
    h: &H,
    c: &RequestContext,
    scope: &Scope,
    subject: Uuid,
    old: Uuid,
    statement: &str,
    trust: Trust,
    from: DateTime<Utc>,
) -> Res<(Uuid, Uuid)> {
    let ep = episode(h, c, scope, trust).await?;
    let ids = h
        .store()
        .apply_commit(
            c,
            ep,
            vec![FactOp::Supersede {
                old_fact_id: old,
                new: new_fact(scope, subject, statement, trust, from),
            }],
        )
        .await?;
    Ok((ep, ids[0]))
}

async fn corroborate<H: Harness>(
    h: &H,
    c: &RequestContext,
    scope: &Scope,
    trust: Trust,
    id: Uuid,
) -> Res<Uuid> {
    let ep = episode(h, c, scope, trust).await?;
    h.store()
        .apply_commit(c, ep, vec![FactOp::Corroborate { fact_id: id }])
        .await?;
    Ok(ep)
}

fn new_insight(
    scope: &Scope,
    level: InsightLevel,
    statement: &str,
    sources: Vec<InsightSourceRef>,
    trust: Trust,
) -> NewInsight {
    NewInsight {
        scope: scope.clone(),
        level,
        statement: statement.into(),
        embedding: axis(2),
        trust,
        sources,
        supersedes: None,
        written_by: "agent-key".into(),
    }
}

async fn write<H: Harness>(
    h: &H,
    c: &RequestContext,
    scope: &Scope,
    level: InsightLevel,
    sources: Vec<InsightSourceRef>,
) -> Res<Insight> {
    Ok(h.store()
        .put_insight(
            c,
            new_insight(scope, level, "what it means", sources, Trust::Agent),
        )
        .await?)
}

async fn status<H: Harness>(h: &H, c: &RequestContext, id: Uuid) -> Res<Insight> {
    Ok(h.store().get_insight(c, id).await?.insight)
}

async fn audits<H: Harness>(h: &H, action: &str) -> Res<i64> {
    h.count(&format!(
        "SELECT count(*) FROM audit_events WHERE action = '{action}'"
    ))
    .await
}

/// An agent scope with a subject and two current user-trust facts.
struct Setup {
    c: RequestContext,
    scope: Scope,
    subject: Uuid,
    f1: Uuid,
    f2: Uuid,
}

async fn setup<H: Harness>(h: &H) -> Res<Setup> {
    let c = ctx("t1");
    let scope = agent();
    let subject = subject(h, &c, &scope).await?;
    let (_, f1) = fact(h, &c, &scope, subject, "ada uses go", Trust::User).await?;
    let (_, f2) = fact(h, &c, &scope, subject, "ada uses kafka", Trust::User).await?;
    Ok(Setup {
        c,
        scope,
        subject,
        f1,
        f2,
    })
}

// ---- Writes and reads ------------------------------------------------------------------------

pub(crate) async fn insight_write_reads_back_lists_and_audits<H: Harness>(h: &H) -> Res<()> {
    let s = setup(h).await?;
    // A source named twice counts once.
    let context = write(
        h,
        &s.c,
        &s.scope,
        InsightLevel::Context,
        vec![f(s.f2), f(s.f1), f(s.f1)],
    )
    .await?;
    assert_eq!(context.status, InsightStatus::Active);
    assert_eq!(context.trust, Trust::Agent);
    assert_eq!(context.sources, vec![f(s.f1), f(s.f2)]);
    assert_eq!(context.stale_reason, None);
    assert_eq!(context.written_by, "agent-key");
    assert_eq!(context.scope, s.scope);
    assert!(context.valid_to.is_none() && context.superseded_by.is_none());

    let detail = h.store().get_insight(&s.c, context.id).await?;
    assert_eq!(detail.source_views.len(), 2);
    assert!(detail.source_views.iter().all(|v| v.current));
    assert_eq!(
        detail.source_views[0].statement.as_deref(),
        Some("ada uses go")
    );
    assert!(detail.history.is_empty());

    let understanding = write(
        h,
        &s.c,
        &s.scope,
        InsightLevel::Understanding,
        vec![i(context.id), f(s.f1)],
    )
    .await?;
    assert_eq!(understanding.status, InsightStatus::Active);
    let detail = h.store().get_insight(&s.c, understanding.id).await?;
    assert_eq!(
        detail.source_views[1].statement.as_deref(),
        Some("what it means")
    );

    let all = h
        .store()
        .list_insights(
            &s.c,
            std::slice::from_ref(&s.scope),
            &InsightFilter::default(),
            10,
        )
        .await?;
    let ids: Vec<Uuid> = all.iter().map(|x| x.id).collect();
    assert_eq!(ids, vec![understanding.id, context.id], "newest first");
    let only_context = h
        .store()
        .list_insights(
            &s.c,
            std::slice::from_ref(&s.scope),
            &InsightFilter {
                level: Some(InsightLevel::Context),
                status: None,
            },
            10,
        )
        .await?;
    assert_eq!(only_context.len(), 1);
    assert_eq!(only_context[0].id, context.id);
    let none_stale = h
        .store()
        .list_insights(
            &s.c,
            std::slice::from_ref(&s.scope),
            &InsightFilter {
                level: None,
                status: Some(InsightStatus::Stale),
            },
            10,
        )
        .await?;
    assert!(none_stale.is_empty());
    let limited = h
        .store()
        .list_insights(
            &s.c,
            std::slice::from_ref(&s.scope),
            &InsightFilter::default(),
            1,
        )
        .await?;
    assert_eq!(limited.len(), 1);
    assert!(h
        .store()
        .list_insights(
            &s.c,
            &[Scope::agent("other")],
            &InsightFilter::default(),
            10
        )
        .await?
        .is_empty());
    assert!(h
        .store()
        .list_insights(&s.c, &[], &InsightFilter::default(), 10)
        .await?
        .is_empty());
    for bad in [0, 501] {
        assert!(is_invalid(
            h.store()
                .list_insights(
                    &s.c,
                    std::slice::from_ref(&s.scope),
                    &InsightFilter::default(),
                    bad
                )
                .await
        ));
    }
    assert!(is_not_found(
        h.store().get_insight(&s.c, Uuid::now_v7()).await
    ));
    assert_eq!(audits(h, "insight.write").await?, 2);
    Ok(())
}

pub(crate) async fn insight_trust_and_status_follow_the_gate<H: Harness>(h: &H) -> Res<()> {
    let s = setup(h).await?;
    let store = h.store();
    // An external writer is quarantined anywhere.
    let held = store
        .put_insight(
            &s.c,
            new_insight(
                &s.scope,
                InsightLevel::Context,
                "x",
                vec![f(s.f1), f(s.f2)],
                Trust::External,
            ),
        )
        .await?;
    assert_eq!(held.status, InsightStatus::Quarantined);
    assert_eq!(held.trust, Trust::External);

    // A source lowers trust: an approved external fact makes the insight external.
    let (_, ext) = fact(h, &s.c, &s.scope, s.subject, "ada said x", Trust::External).await?;
    store.approve_fact(&s.c, ext).await?;
    let weak = write(
        h,
        &s.c,
        &s.scope,
        InsightLevel::Context,
        vec![f(s.f1), f(ext)],
    )
    .await?;
    assert_eq!(weak.trust, Trust::External);
    assert_eq!(weak.status, InsightStatus::Quarantined);

    // Guidance in an agent's own scope is active.
    let guidance = write(
        h,
        &s.c,
        &s.scope,
        InsightLevel::Guidance,
        vec![f(s.f1), f(s.f2)],
    )
    .await?;
    assert_eq!(guidance.status, InsightStatus::Active);

    // In a user scope: agent trust is held, user trust is active, guidance is always held.
    let user = Scope::user("u1");
    let us = subject(h, &s.c, &user).await?;
    let (_, u1) = fact(h, &s.c, &user, us, "ada likes tea", Trust::User).await?;
    let (_, u2) = fact(h, &s.c, &user, us, "ada likes rain", Trust::User).await?;
    let by_agent = write(h, &s.c, &user, InsightLevel::Context, vec![f(u1), f(u2)]).await?;
    assert_eq!(by_agent.status, InsightStatus::Quarantined);
    let by_user = store
        .put_insight(
            &s.c,
            new_insight(
                &user,
                InsightLevel::Understanding,
                "x",
                vec![f(u1), f(u2)],
                Trust::User,
            ),
        )
        .await?;
    assert_eq!(by_user.status, InsightStatus::Active);
    assert_eq!(by_user.trust, Trust::User);
    let user_guidance = store
        .put_insight(
            &s.c,
            new_insight(
                &user,
                InsightLevel::Guidance,
                "x",
                vec![f(u1), f(u2)],
                Trust::System,
            ),
        )
        .await?;
    assert_eq!(user_guidance.status, InsightStatus::Quarantined);
    assert_eq!(
        user_guidance.trust,
        Trust::User,
        "lowest of writer and sources"
    );

    let pending = store
        .list_quarantined_insights(&s.c, &[s.scope.clone(), user.clone()], 10)
        .await?;
    let ids: Vec<Uuid> = pending.iter().map(|x| x.id).collect();
    assert_eq!(
        ids,
        vec![held.id, weak.id, by_agent.id, user_guidance.id],
        "oldest first"
    );
    assert!(is_invalid(
        store.list_quarantined_insights(&s.c, &[user], 0).await
    ));
    Ok(())
}

pub(crate) async fn insight_bad_input_is_refused<H: Harness>(h: &H) -> Res<()> {
    let s = setup(h).await?;
    let store = h.store();
    let put = |sources: Vec<InsightSourceRef>, level: InsightLevel| {
        new_insight(&s.scope, level, "x", sources, Trust::Agent)
    };
    use InsightLevel::*;
    // Counts.
    assert!(is_invalid(
        store.put_insight(&s.c, put(vec![f(s.f1)], Context)).await
    ));
    assert!(is_invalid(
        store
            .put_insight(&s.c, put(vec![f(s.f1), f(s.f1)], Context))
            .await
    ));
    let many: Vec<_> = (0..51).map(|_| f(Uuid::now_v7())).collect();
    assert!(is_invalid(
        store.put_insight(&s.c, put(many, Context)).await
    ));
    // Statement and embedding.
    for statement in ["", "   ", &"x".repeat(1001)] {
        let mut new = put(vec![f(s.f1), f(s.f2)], Context);
        new.statement = statement.to_owned();
        assert!(
            is_invalid(store.put_insight(&s.c, new).await),
            "{statement:?}"
        );
    }
    let mut longest = put(vec![f(s.f1), f(s.f2)], Context);
    longest.statement = "é".repeat(1000);
    store.put_insight(&s.c, longest).await?;
    let mut zero = put(vec![f(s.f1), f(s.f2)], Context);
    zero.embedding = vec![0.0; EMBEDDING_DIMS];
    assert!(is_invalid(store.put_insight(&s.c, zero).await));
    // A missing source, and a fact of another scope.
    assert!(is_invalid(
        store
            .put_insight(&s.c, put(vec![f(s.f1), f(Uuid::now_v7())], Context))
            .await
    ));
    let other = Scope::agent("other");
    let os = subject(h, &s.c, &other).await?;
    let (_, foreign) = fact(h, &s.c, &other, os, "elsewhere", Trust::User).await?;
    assert!(is_invalid(
        store
            .put_insight(&s.c, put(vec![f(s.f1), f(foreign)], Context))
            .await
    ));
    // Levels: only strictly lower insights.
    let context = write(h, &s.c, &s.scope, Context, vec![f(s.f1), f(s.f2)]).await?;
    let understanding = write(
        h,
        &s.c,
        &s.scope,
        Understanding,
        vec![i(context.id), f(s.f1)],
    )
    .await?;
    assert!(is_invalid(
        store
            .put_insight(&s.c, put(vec![i(context.id), f(s.f1)], Context))
            .await
    ));
    assert!(is_invalid(
        store
            .put_insight(&s.c, put(vec![i(understanding.id), f(s.f1)], Understanding))
            .await
    ));
    assert!(is_invalid(
        store
            .put_insight(&s.c, put(vec![i(understanding.id), f(s.f1)], Context))
            .await
    ));
    write(
        h,
        &s.c,
        &s.scope,
        Guidance,
        vec![i(understanding.id), i(context.id)],
    )
    .await?;
    // An insight cited as a fact, or a fact cited as an insight, is not found.
    assert!(is_invalid(
        store
            .put_insight(&s.c, put(vec![f(context.id), f(s.f1)], Understanding))
            .await
    ));
    assert!(is_invalid(
        store
            .put_insight(&s.c, put(vec![i(s.f2), f(s.f1)], Understanding))
            .await
    ));
    // Inactive sources: a quarantined fact, a superseded fact, a retracted fact, an expired fact.
    let (_, held) = fact(h, &s.c, &s.scope, s.subject, "held", Trust::External).await?;
    assert!(is_invalid(
        store
            .put_insight(&s.c, put(vec![f(s.f1), f(held)], Context))
            .await
    ));
    let (_, old) = fact(h, &s.c, &s.scope, s.subject, "old", Trust::User).await?;
    supersede(
        h,
        &s.c,
        &s.scope,
        s.subject,
        old,
        "new",
        Trust::User,
        date(2021),
    )
    .await?;
    assert!(is_invalid(
        store
            .put_insight(&s.c, put(vec![f(s.f1), f(old)], Context))
            .await
    ));
    let (_, retracted) = fact(h, &s.c, &s.scope, s.subject, "wrong", Trust::User).await?;
    store.invalidate_fact(&s.c, retracted, "wrong").await?;
    assert!(is_invalid(
        store
            .put_insight(&s.c, put(vec![f(s.f1), f(retracted)], Context))
            .await
    ));
    let (_, expired) = fact(h, &s.c, &s.scope, s.subject, "until friday", Trust::User).await?;
    h.expire_fact(expired).await?;
    assert!(is_invalid(
        store
            .put_insight(&s.c, put(vec![f(s.f1), f(expired)], Context))
            .await
    ));
    // Inactive insight sources: quarantined, stale, retired.
    let quarantined = store
        .put_insight(
            &s.c,
            new_insight(
                &s.scope,
                Context,
                "x",
                vec![f(s.f1), f(s.f2)],
                Trust::External,
            ),
        )
        .await?;
    assert!(is_invalid(
        store
            .put_insight(&s.c, put(vec![i(quarantined.id), f(s.f1)], Understanding))
            .await
    ));
    let (_, f3) = fact(h, &s.c, &s.scope, s.subject, "ada uses rust", Trust::User).await?;
    let doomed = write(h, &s.c, &s.scope, Context, vec![f(s.f1), f(f3)]).await?;
    store.invalidate_fact(&s.c, f3, "wrong").await?;
    assert_eq!(
        status(h, &s.c, doomed.id).await?.status,
        InsightStatus::Stale
    );
    assert!(is_invalid(
        store
            .put_insight(&s.c, put(vec![i(doomed.id), f(s.f1)], Understanding))
            .await
    ));
    let retired = write(h, &s.c, &s.scope, Context, vec![f(s.f1), f(s.f2)]).await?;
    store
        .retire_insight(&s.c, retired.id, "no longer true")
        .await?;
    assert!(is_invalid(
        store
            .put_insight(&s.c, put(vec![i(retired.id), f(s.f1)], Understanding))
            .await
    ));
    // Supersedes: missing, retired, another scope's, or also a source.
    for target in [Uuid::now_v7(), retired.id] {
        let mut new = put(vec![f(s.f1), f(s.f2)], Context);
        new.supersedes = Some(target);
        assert!(is_invalid(store.put_insight(&s.c, new).await));
    }
    let os2 = fact(h, &s.c, &other, os, "elsewhere too", Trust::User)
        .await?
        .1;
    let foreign_insight = write(h, &s.c, &other, Context, vec![f(foreign), f(os2)]).await?;
    let mut new = put(vec![f(s.f1), f(s.f2)], Context);
    new.supersedes = Some(foreign_insight.id);
    assert!(is_invalid(store.put_insight(&s.c, new).await));
    let mut new = put(vec![i(context.id), f(s.f1)], Understanding);
    new.supersedes = Some(context.id);
    assert!(is_invalid(store.put_insight(&s.c, new).await));
    // Transitions from the wrong status, and unknown ids.
    assert!(is_invalid(
        store
            .confirm_insight(&s.c, context.id, &[f(s.f1), f(s.f2)])
            .await
    ));
    assert!(is_invalid(
        store.retire_insight(&s.c, quarantined.id, "x").await
    ));
    assert!(is_invalid(store.approve_insight(&s.c, context.id).await));
    assert!(is_invalid(store.reject_insight(&s.c, context.id).await));
    assert!(is_not_found(
        store.retire_insight(&s.c, Uuid::now_v7(), "x").await
    ));
    assert!(is_not_found(
        store.approve_insight(&s.c, Uuid::now_v7()).await
    ));
    assert!(is_not_found(
        store
            .confirm_insight(&s.c, Uuid::now_v7(), &[f(s.f1), f(s.f2)])
            .await
    ));
    // Nothing above changed the insight it was refused for.
    assert_eq!(
        status(h, &s.c, context.id).await?.status,
        InsightStatus::Active
    );
    Ok(())
}

// ---- Staleness -------------------------------------------------------------------------------

pub(crate) async fn insight_a_superseded_fact_makes_citing_insights_stale<H: Harness>(
    h: &H,
) -> Res<()> {
    let s = setup(h).await?;
    let insight = write(
        h,
        &s.c,
        &s.scope,
        InsightLevel::Context,
        vec![f(s.f1), f(s.f2)],
    )
    .await?;
    // Corroboration changes nothing.
    corroborate(h, &s.c, &s.scope, Trust::Agent, s.f2).await?;
    assert_eq!(
        status(h, &s.c, insight.id).await?.status,
        InsightStatus::Active
    );
    assert_eq!(audits(h, "insight.stale").await?, 0);

    let (_, f3) = supersede(
        h,
        &s.c,
        &s.scope,
        s.subject,
        s.f1,
        "ada uses rust",
        Trust::User,
        date(2021),
    )
    .await?;
    let after = status(h, &s.c, insight.id).await?;
    assert_eq!(after.status, InsightStatus::Stale);
    assert_eq!(
        after.stale_reason,
        Some(vec![stale(f(s.f1), SourceChange::Superseded { by: f3 })])
    );
    assert_eq!(audits(h, "insight.stale").await?, 1);
    let detail = h.store().get_insight(&s.c, insight.id).await?;
    assert!(!detail.source_views[0].current);
    assert!(detail.source_views[1].current);
    Ok(())
}

pub(crate) async fn insight_supersede_with_existing_makes_citing_insights_stale<H: Harness>(
    h: &H,
) -> Res<()> {
    let s = setup(h).await?;
    let (_, f3) = fact(h, &s.c, &s.scope, s.subject, "ada uses rust", Trust::User).await?;
    let insight = write(
        h,
        &s.c,
        &s.scope,
        InsightLevel::Context,
        vec![f(s.f1), f(s.f2)],
    )
    .await?;
    let ep = episode(h, &s.c, &s.scope, Trust::User).await?;
    h.store()
        .apply_commit(
            &s.c,
            ep,
            vec![FactOp::SupersedeWithExisting {
                old_fact_id: s.f1,
                fact_id: f3,
            }],
        )
        .await?;
    assert_eq!(
        status(h, &s.c, insight.id).await?.stale_reason,
        Some(vec![stale(f(s.f1), SourceChange::Superseded { by: f3 })])
    );
    Ok(())
}

pub(crate) async fn insight_approval_closing_a_head_makes_citing_insights_stale<H: Harness>(
    h: &H,
) -> Res<()> {
    let s = setup(h).await?;
    let insight = write(
        h,
        &s.c,
        &s.scope,
        InsightLevel::Context,
        vec![f(s.f1), f(s.f2)],
    )
    .await?;
    // An external supersede waits for review and leaves the old fact current.
    let (_, pending) = supersede(
        h,
        &s.c,
        &s.scope,
        s.subject,
        s.f1,
        "ada uses zig",
        Trust::External,
        date(2021),
    )
    .await?;
    assert_eq!(
        status(h, &s.c, insight.id).await?.status,
        InsightStatus::Active
    );
    h.store().approve_fact(&s.c, pending).await?;
    assert_eq!(
        status(h, &s.c, insight.id).await?.stale_reason,
        Some(vec![stale(
            f(s.f1),
            SourceChange::Superseded { by: pending }
        )])
    );
    Ok(())
}

pub(crate) async fn insight_invalidation_makes_citing_insights_stale_and_reasons_merge<
    H: Harness,
>(
    h: &H,
) -> Res<()> {
    let s = setup(h).await?;
    let insight = write(
        h,
        &s.c,
        &s.scope,
        InsightLevel::Context,
        vec![f(s.f1), f(s.f2)],
    )
    .await?;
    h.store().invalidate_fact(&s.c, s.f1, "wrong").await?;
    assert_eq!(
        status(h, &s.c, insight.id).await?.stale_reason,
        Some(vec![stale(f(s.f1), SourceChange::Invalidated)])
    );
    // A repeat changes nothing; another source is added to the reason.
    h.store().invalidate_fact(&s.c, s.f1, "wrong").await?;
    assert_eq!(audits(h, "insight.stale").await?, 1);
    h.store().invalidate_fact(&s.c, s.f2, "wrong too").await?;
    let after = status(h, &s.c, insight.id).await?;
    assert_eq!(after.status, InsightStatus::Stale);
    assert_eq!(
        after.stale_reason,
        Some(vec![
            stale(f(s.f1), SourceChange::Invalidated),
            stale(f(s.f2), SourceChange::Invalidated),
        ])
    );
    assert_eq!(audits(h, "insight.stale").await?, 2);
    Ok(())
}

pub(crate) async fn insight_purge_marks_deleted_and_relinked_sources<H: Harness>(h: &H) -> Res<()> {
    let s = setup(h).await?;
    let store = h.store();
    // A chain a -> b -> c. Each insight is written while its fact is current.
    let (_, a) = fact(h, &s.c, &s.scope, s.subject, "lives in paris", Trust::User).await?;
    let on_a = write(
        h,
        &s.c,
        &s.scope,
        InsightLevel::Context,
        vec![f(a), f(s.f1)],
    )
    .await?;
    let (eb, b) = supersede(
        h,
        &s.c,
        &s.scope,
        s.subject,
        a,
        "lives in rome",
        Trust::User,
        date(2021),
    )
    .await?;
    let on_b = write(
        h,
        &s.c,
        &s.scope,
        InsightLevel::Context,
        vec![f(b), f(s.f2)],
    )
    .await?;
    let (_, c) = supersede(
        h,
        &s.c,
        &s.scope,
        s.subject,
        b,
        "lives in oslo",
        Trust::User,
        date(2022),
    )
    .await?;
    assert_eq!(
        status(h, &s.c, on_a.id).await?.stale_reason,
        Some(vec![stale(f(a), SourceChange::Superseded { by: b })])
    );
    // Purging b deletes it and relinks a to c.
    store.purge_episode(&s.c, eb).await?;
    assert_eq!(
        status(h, &s.c, on_b.id).await?.stale_reason,
        Some(vec![stale(f(b), SourceChange::Purged)])
    );
    assert_eq!(
        status(h, &s.c, on_a.id).await?.stale_reason,
        Some(vec![stale(f(a), SourceChange::Superseded { by: c })])
    );
    let detail = store.get_insight(&s.c, on_b.id).await?;
    let purged = detail
        .source_views
        .iter()
        .find(|view| view.source == f(b))
        .expect("b is a source");
    assert_eq!(purged.statement, None, "a purged source");
    assert!(!purged.current);

    // Reopened: purging the only successor makes the old fact current again; the insight citing
    // it stays stale, since it was already, and keeps its entry.
    let (_, x) = fact(h, &s.c, &s.scope, s.subject, "works at acme", Trust::User).await?;
    let on_x = write(
        h,
        &s.c,
        &s.scope,
        InsightLevel::Context,
        vec![f(x), f(s.f1)],
    )
    .await?;
    let (ey, y) = supersede(
        h,
        &s.c,
        &s.scope,
        s.subject,
        x,
        "works at initech",
        Trust::User,
        date(2021),
    )
    .await?;
    store.purge_episode(&s.c, ey).await?;
    assert!(store.current_fact(&s.c, &s.scope, x).await?.is_some());
    let after = status(h, &s.c, on_x.id).await?;
    assert_eq!(after.status, InsightStatus::Stale);
    assert_eq!(
        after.stale_reason,
        Some(vec![stale(f(x), SourceChange::Superseded { by: y })])
    );
    Ok(())
}

pub(crate) async fn insight_purge_requarantine_and_lowered_trust_make_insights_stale<H: Harness>(
    h: &H,
) -> Res<()> {
    let s = setup(h).await?;
    let store = h.store();
    // Left only on an external source, the fact goes back to quarantine.
    let (good, held) = fact(h, &s.c, &s.scope, s.subject, "ada leads", Trust::User).await?;
    corroborate(h, &s.c, &s.scope, Trust::External, held).await?;
    let on_held = write(
        h,
        &s.c,
        &s.scope,
        InsightLevel::Context,
        vec![f(held), f(s.f1)],
    )
    .await?;
    store.purge_episode(&s.c, good).await?;
    assert_eq!(
        store.get_fact(&s.c, held).await?.status,
        FactStatus::Quarantined
    );
    assert_eq!(
        status(h, &s.c, on_held.id).await?.stale_reason,
        Some(vec![stale(f(held), SourceChange::Purged)])
    );

    // Left on an agent source, the fact stays active with lower trust than a user insight on it.
    let (strong, lowered) = fact(h, &s.c, &s.scope, s.subject, "ada ships", Trust::User).await?;
    corroborate(h, &s.c, &s.scope, Trust::Agent, lowered).await?;
    let on_lowered = store
        .put_insight(
            &s.c,
            new_insight(
                &s.scope,
                InsightLevel::Context,
                "x",
                vec![f(lowered), f(s.f2)],
                Trust::User,
            ),
        )
        .await?;
    assert_eq!(on_lowered.trust, Trust::User);
    // An agent insight on the same fact is not above its new trust and stays active.
    let on_lowered_agent = write(
        h,
        &s.c,
        &s.scope,
        InsightLevel::Context,
        vec![f(lowered), f(s.f2)],
    )
    .await?;
    store.purge_episode(&s.c, strong).await?;
    let fact_now = store.get_fact(&s.c, lowered).await?;
    assert_eq!(
        (fact_now.status, fact_now.trust),
        (FactStatus::Active, Trust::Agent)
    );
    assert_eq!(
        status(h, &s.c, on_lowered.id).await?.stale_reason,
        Some(vec![stale(f(lowered), SourceChange::Purged)])
    );
    assert_eq!(
        status(h, &s.c, on_lowered_agent.id).await?.status,
        InsightStatus::Active
    );
    Ok(())
}

pub(crate) async fn insight_changes_carry_up_every_level<H: Harness>(h: &H) -> Res<()> {
    let s = setup(h).await?;
    use InsightLevel::*;
    let context = write(h, &s.c, &s.scope, Context, vec![f(s.f1), f(s.f2)]).await?;
    let understanding = write(
        h,
        &s.c,
        &s.scope,
        Understanding,
        vec![i(context.id), f(s.f2)],
    )
    .await?;
    let guidance = write(
        h,
        &s.c,
        &s.scope,
        Guidance,
        vec![i(understanding.id), f(s.f2)],
    )
    .await?;
    // Guidance citing context directly as well.
    let wide = write(
        h,
        &s.c,
        &s.scope,
        Guidance,
        vec![i(context.id), i(understanding.id)],
    )
    .await?;
    h.store().invalidate_fact(&s.c, s.f1, "wrong").await?;
    let changed = |id| {
        stale(
            i(id),
            SourceChange::InsightChanged {
                status: InsightStatus::Stale,
            },
        )
    };
    assert_eq!(
        status(h, &s.c, understanding.id).await?.stale_reason,
        Some(vec![changed(context.id)])
    );
    assert_eq!(
        status(h, &s.c, guidance.id).await?.stale_reason,
        Some(vec![changed(understanding.id)])
    );
    let mut wide_reason = status(h, &s.c, wide.id).await?.stale_reason.expect("stale");
    wide_reason.sort_by_key(|entry| entry.source);
    assert_eq!(
        wide_reason,
        vec![changed(context.id), changed(understanding.id)]
    );
    // Retiring the stale context updates the entry above it.
    h.store().retire_insight(&s.c, context.id, "wrong").await?;
    assert_eq!(
        status(h, &s.c, understanding.id).await?.stale_reason,
        Some(vec![stale(
            i(context.id),
            SourceChange::InsightChanged {
                status: InsightStatus::Retired
            }
        )])
    );
    Ok(())
}

pub(crate) async fn insight_supersede_and_retire_close_and_carry_up<H: Harness>(h: &H) -> Res<()> {
    let s = setup(h).await?;
    let store = h.store();
    use InsightLevel::*;
    let old = write(h, &s.c, &s.scope, Context, vec![f(s.f1), f(s.f2)]).await?;
    let above = write(h, &s.c, &s.scope, Understanding, vec![i(old.id), f(s.f1)]).await?;
    let mut replacement = new_insight(
        &s.scope,
        Context,
        "better",
        vec![f(s.f1), f(s.f2)],
        Trust::Agent,
    );
    replacement.supersedes = Some(old.id);
    let new = store.put_insight(&s.c, replacement).await?;
    assert_eq!(new.status, InsightStatus::Active);
    let closed = status(h, &s.c, old.id).await?;
    assert_eq!(closed.status, InsightStatus::Superseded);
    assert_eq!(closed.superseded_by, Some(new.id));
    assert!(closed.valid_to.is_some());
    assert_eq!(closed.stale_reason, None);
    assert_eq!(
        status(h, &s.c, above.id).await?.stale_reason,
        Some(vec![stale(
            i(old.id),
            SourceChange::InsightChanged {
                status: InsightStatus::Superseded
            }
        )])
    );
    // A stale insight can be superseded too, and history lists what each replaced, newest first.
    let mut again = new_insight(
        &s.scope,
        Context,
        "best",
        vec![f(s.f1), f(s.f2)],
        Trust::Agent,
    );
    again.supersedes = Some(new.id);
    let newest = store.put_insight(&s.c, again).await?;
    let history: Vec<Uuid> = store
        .get_insight(&s.c, newest.id)
        .await?
        .history
        .iter()
        .map(|x| x.id)
        .collect();
    assert_eq!(history, vec![new.id, old.id]);
    let mut stale_replace = new_insight(
        &s.scope,
        Understanding,
        "x",
        vec![i(newest.id), f(s.f2)],
        Trust::Agent,
    );
    stale_replace.supersedes = Some(above.id);
    assert_eq!(
        store.put_insight(&s.c, stale_replace).await?.status,
        InsightStatus::Active
    );
    assert_eq!(
        status(h, &s.c, above.id).await?.status,
        InsightStatus::Superseded
    );

    // Retire.
    let target = write(h, &s.c, &s.scope, Context, vec![f(s.f1), f(s.f2)]).await?;
    let citing = write(
        h,
        &s.c,
        &s.scope,
        Understanding,
        vec![i(target.id), f(s.f2)],
    )
    .await?;
    let retired = store
        .retire_insight(&s.c, target.id, "no longer true")
        .await?;
    assert_eq!(retired.status, InsightStatus::Retired);
    assert!(retired.valid_to.is_some());
    assert_eq!(
        status(h, &s.c, citing.id).await?.stale_reason,
        Some(vec![stale(
            i(target.id),
            SourceChange::InsightChanged {
                status: InsightStatus::Retired
            }
        )])
    );
    assert!(is_invalid(
        store.retire_insight(&s.c, target.id, "again").await
    ));
    assert_eq!(audits(h, "insight.retire").await?, 1);
    Ok(())
}

pub(crate) async fn insight_expiry_is_marked_lazily_on_reads<H: Harness>(h: &H) -> Res<()> {
    let s = setup(h).await?;
    let (_, temporary) = fact(h, &s.c, &s.scope, s.subject, "on leave", Trust::User).await?;
    let insight = write(
        h,
        &s.c,
        &s.scope,
        InsightLevel::Context,
        vec![f(temporary), f(s.f1)],
    )
    .await?;
    let above = write(
        h,
        &s.c,
        &s.scope,
        InsightLevel::Understanding,
        vec![i(insight.id), f(s.f1)],
    )
    .await?;
    h.expire_fact(temporary).await?;
    // Nothing writes at expiry; the next read of the scope marks it, before it searches.
    let hits = h
        .store()
        .search_insights_vector(&s.c, std::slice::from_ref(&s.scope), &axis(2), 10)
        .await?;
    assert!(hits.is_empty(), "both are stale before the search runs");
    let after = status(h, &s.c, insight.id).await?;
    assert_eq!(
        after.stale_reason,
        Some(vec![stale(f(temporary), SourceChange::Expired)])
    );
    assert_eq!(
        status(h, &s.c, above.id).await?.status,
        InsightStatus::Stale
    );
    // Another scope's read does not touch it; list reads apply it too.
    let other = Scope::agent("b2");
    let os = subject(h, &s.c, &other).await?;
    let (_, o1) = fact(h, &s.c, &other, os, "short", Trust::User).await?;
    let (_, o2) = fact(h, &s.c, &other, os, "long", Trust::User).await?;
    let o = write(h, &s.c, &other, InsightLevel::Context, vec![f(o1), f(o2)]).await?;
    h.expire_fact(o1).await?;
    h.store()
        .list_insights(
            &s.c,
            std::slice::from_ref(&s.scope),
            &InsightFilter::default(),
            10,
        )
        .await?;
    assert_eq!(
        h.count(&format!(
            "SELECT count(*) FROM insights WHERE id = '{}' AND status = 'active'",
            o.id
        ))
        .await?,
        1
    );
    let listed = h
        .store()
        .list_insights(&s.c, &[other], &InsightFilter::default(), 10)
        .await?;
    assert_eq!(listed[0].status, InsightStatus::Stale);
    Ok(())
}

pub(crate) async fn insight_confirm_reactivates_a_stale_insight<H: Harness>(h: &H) -> Res<()> {
    let s = setup(h).await?;
    let store = h.store();
    let insight = write(
        h,
        &s.c,
        &s.scope,
        InsightLevel::Context,
        vec![f(s.f1), f(s.f2)],
    )
    .await?;
    store.invalidate_fact(&s.c, s.f1, "wrong").await?;
    // Bad sources leave it stale.
    assert!(is_invalid(
        store
            .confirm_insight(&s.c, insight.id, &[f(s.f1), f(s.f2)])
            .await
    ));
    assert!(is_invalid(
        store.confirm_insight(&s.c, insight.id, &[f(s.f2)]).await
    ));
    assert_eq!(
        status(h, &s.c, insight.id).await?.status,
        InsightStatus::Stale
    );
    let (_, f3) = fact(h, &s.c, &s.scope, s.subject, "ada uses rust", Trust::User).await?;
    let confirmed = store
        .confirm_insight(&s.c, insight.id, &[f(f3), f(s.f2)])
        .await?;
    assert_eq!(confirmed.status, InsightStatus::Active);
    assert_eq!(confirmed.stale_reason, None);
    assert_eq!(confirmed.sources, vec![f(s.f2), f(f3)]);
    assert_eq!(confirmed.trust, Trust::Agent);
    assert_eq!(audits(h, "insight.confirm").await?, 1);
    // A later change makes it stale again, on its new sources only.
    store.invalidate_fact(&s.c, f3, "wrong").await?;
    assert_eq!(
        status(h, &s.c, insight.id).await?.stale_reason,
        Some(vec![stale(f(f3), SourceChange::Invalidated)])
    );
    // Confirming onto a weaker source applies the gate again.
    let (_, ext) = fact(h, &s.c, &s.scope, s.subject, "ada said y", Trust::External).await?;
    store.approve_fact(&s.c, ext).await?;
    let held = store
        .confirm_insight(&s.c, insight.id, &[f(ext), f(s.f2)])
        .await?;
    assert_eq!(held.status, InsightStatus::Quarantined);
    assert_eq!(held.trust, Trust::External);
    Ok(())
}

pub(crate) async fn insight_quarantine_approve_closes_the_pending_supersede<H: Harness>(
    h: &H,
) -> Res<()> {
    let s = setup(h).await?;
    let store = h.store();
    let old = write(
        h,
        &s.c,
        &s.scope,
        InsightLevel::Context,
        vec![f(s.f1), f(s.f2)],
    )
    .await?;
    let mut held = new_insight(
        &s.scope,
        InsightLevel::Context,
        "x",
        vec![f(s.f1), f(s.f2)],
        Trust::External,
    );
    held.supersedes = Some(old.id);
    let held = store.put_insight(&s.c, held).await?;
    assert_eq!(held.status, InsightStatus::Quarantined);
    assert_eq!(held.pending_supersede_of, Some(old.id));
    assert_eq!(status(h, &s.c, old.id).await?.status, InsightStatus::Active);
    let listed = store
        .list_quarantined_insights(&s.c, std::slice::from_ref(&s.scope), 10)
        .await?;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, held.id);

    let approved = store.approve_insight(&s.c, held.id).await?;
    assert_eq!(approved.status, InsightStatus::Active);
    assert_eq!(approved.pending_supersede_of, None);
    let closed = status(h, &s.c, old.id).await?;
    assert_eq!(closed.status, InsightStatus::Superseded);
    assert_eq!(closed.superseded_by, Some(held.id));
    let history = store.get_insight(&s.c, held.id).await?.history;
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].id, old.id);
    assert!(is_invalid(store.approve_insight(&s.c, held.id).await));
    assert_eq!(audits(h, "insight.approve").await?, 1);

    // Reject leaves what it would have replaced alone.
    let mut other = new_insight(
        &s.scope,
        InsightLevel::Context,
        "y",
        vec![f(s.f1), f(s.f2)],
        Trust::External,
    );
    other.supersedes = Some(held.id);
    let other = store.put_insight(&s.c, other).await?;
    let rejected = store.reject_insight(&s.c, other.id).await?;
    assert_eq!(rejected.status, InsightStatus::Rejected);
    assert_eq!(rejected.pending_supersede_of, None);
    assert_eq!(
        status(h, &s.c, held.id).await?.status,
        InsightStatus::Active
    );
    assert!(is_invalid(store.approve_insight(&s.c, other.id).await));
    assert_eq!(audits(h, "insight.reject").await?, 1);
    Ok(())
}

pub(crate) async fn insight_approve_turns_stale_when_a_source_changed_meanwhile<H: Harness>(
    h: &H,
) -> Res<()> {
    let s = setup(h).await?;
    let store = h.store();
    let old = write(
        h,
        &s.c,
        &s.scope,
        InsightLevel::Context,
        vec![f(s.f1), f(s.f2)],
    )
    .await?;
    let mut held = new_insight(
        &s.scope,
        InsightLevel::Context,
        "x",
        vec![f(s.f1), f(s.f2)],
        Trust::External,
    );
    held.supersedes = Some(old.id);
    let held = store.put_insight(&s.c, held).await?;
    store.invalidate_fact(&s.c, s.f2, "wrong").await?;
    // A quarantined insight is not marked stale; approval checks its sources.
    assert_eq!(
        status(h, &s.c, held.id).await?.status,
        InsightStatus::Quarantined
    );
    let approved = store.approve_insight(&s.c, held.id).await?;
    assert_eq!(approved.status, InsightStatus::Stale);
    assert_eq!(
        approved.stale_reason,
        Some(vec![stale(f(s.f2), SourceChange::Invalidated)])
    );
    assert_eq!(approved.pending_supersede_of, None);
    // The approval accepted the replacement, so the old insight is closed by it.
    let closed = status(h, &s.c, old.id).await?;
    assert_eq!(closed.status, InsightStatus::Superseded);
    assert_eq!(closed.superseded_by, Some(held.id));
    Ok(())
}

pub(crate) async fn insight_search_returns_only_active_insights_of_the_scopes<H: Harness>(
    h: &H,
) -> Res<()> {
    let s = setup(h).await?;
    let store = h.store();
    let mut near = new_insight(
        &s.scope,
        InsightLevel::Context,
        "ada prefers small reviews",
        vec![f(s.f1), f(s.f2)],
        Trust::Agent,
    );
    near.embedding = axis(5);
    let near = store.put_insight(&s.c, near).await?;
    let mut far = new_insight(
        &s.scope,
        InsightLevel::Context,
        "kafka retries need care",
        vec![f(s.f1), f(s.f2)],
        Trust::Agent,
    );
    far.embedding = axis(6);
    let far = store.put_insight(&s.c, far).await?;
    let mut held = new_insight(
        &s.scope,
        InsightLevel::Context,
        "reviews are held",
        vec![f(s.f1), f(s.f2)],
        Trust::External,
    );
    held.embedding = axis(5);
    store.put_insight(&s.c, held).await?;
    let scopes = std::slice::from_ref(&s.scope);

    let hits = store
        .search_insights_vector(&s.c, scopes, &axis(5), 10)
        .await?;
    let ids: Vec<Uuid> = hits.iter().map(|hit| hit.insight.id).collect();
    assert_eq!(ids, vec![near.id, far.id]);
    assert!((hits[0].score - 1.0).abs() < 1e-6);
    assert_eq!(
        store
            .search_insights_vector(&s.c, scopes, &axis(5), 1)
            .await?
            .len(),
        1
    );
    assert!(is_invalid(
        store
            .search_insights_vector(&s.c, scopes, &axis(5), 0)
            .await
    ));
    assert!(is_invalid(
        store
            .search_insights_vector(&s.c, scopes, &[1.0; 3], 5)
            .await
    ));
    assert!(store
        .search_insights_vector(&s.c, &[Scope::agent("none")], &axis(5), 5)
        .await?
        .is_empty());

    let hits = store
        .search_insights_text(&s.c, scopes, "reviews", 10)
        .await?;
    let ids: Vec<Uuid> = hits.iter().map(|hit| hit.insight.id).collect();
    assert_eq!(ids, vec![near.id]);
    assert!(hits[0].score > 0.0);
    assert!(store
        .search_insights_text(&s.c, scopes, "'); drop table insights; --", 10)
        .await?
        .is_empty());
    // Stale and retired insights drop out.
    store.retire_insight(&s.c, near.id, "no").await?;
    assert!(store
        .search_insights_text(&s.c, scopes, "reviews", 10)
        .await?
        .is_empty());
    store.invalidate_fact(&s.c, s.f1, "wrong").await?;
    assert!(store
        .search_insights_vector(&s.c, scopes, &axis(6), 10)
        .await?
        .is_empty());
    Ok(())
}

pub(crate) async fn insight_purge_scope_deletes_and_summaries_count_active<H: Harness>(
    h: &H,
) -> Res<()> {
    let c = ctx("t1");
    let doomed = Scope::agent("doomed");
    let kept = Scope::agent("kept");
    for scope in [&doomed, &kept] {
        let subj = subject(h, &c, scope).await?;
        let (_, a) = fact(h, &c, scope, subj, "a", Trust::User).await?;
        let (_, b) = fact(h, &c, scope, subj, "b", Trust::User).await?;
        let (_, x) = fact(h, &c, scope, subj, "x", Trust::User).await?;
        write(h, &c, scope, InsightLevel::Context, vec![f(a), f(b)]).await?;
        write(h, &c, scope, InsightLevel::Context, vec![f(a), f(x)]).await?;
        h.store()
            .put_insight(
                &c,
                new_insight(
                    scope,
                    InsightLevel::Context,
                    "x",
                    vec![f(a), f(b)],
                    Trust::External,
                ),
            )
            .await?;
        h.store().invalidate_fact(&c, x, "wrong").await?;
    }
    let summaries = h.summaries(&c).await?;
    let active_in = |scope: &Scope| {
        summaries
            .iter()
            .find(|summary| &summary.scope == scope)
            .map(|summary| summary.insights)
    };
    assert_eq!(active_in(&doomed), Some(1));
    assert_eq!(active_in(&kept), Some(1));

    let report = h.store().purge_scope(&c, &doomed).await?;
    assert_eq!(report.insights_deleted, 3);
    assert!(h
        .store()
        .list_insights(
            &c,
            std::slice::from_ref(&doomed),
            &InsightFilter::default(),
            10
        )
        .await?
        .is_empty());
    assert_eq!(
        h.store()
            .list_insights(
                &c,
                std::slice::from_ref(&kept),
                &InsightFilter::default(),
                10
            )
            .await?
            .len(),
        3
    );
    assert_eq!(h.count("SELECT count(*) FROM insights").await?, 3);
    assert_eq!(h.count("SELECT count(*) FROM insight_sources").await?, 6);
    assert_eq!(
        h.count("SELECT count(*) FROM audit_events WHERE action = 'scope.purge'")
            .await?,
        1
    );
    let summaries = h.summaries(&c).await?;
    assert!(summaries.iter().all(|summary| summary.scope != doomed));
    Ok(())
}

pub(crate) async fn insight_another_tenant_sees_and_changes_nothing<H: Harness>(h: &H) -> Res<()> {
    let s = setup(h).await?;
    let store = h.store();
    let insight = write(
        h,
        &s.c,
        &s.scope,
        InsightLevel::Context,
        vec![f(s.f1), f(s.f2)],
    )
    .await?;
    let held = store
        .put_insight(
            &s.c,
            new_insight(
                &s.scope,
                InsightLevel::Context,
                "x",
                vec![f(s.f1), f(s.f2)],
                Trust::External,
            ),
        )
        .await?;
    let b = ctx("t2");
    let scopes = std::slice::from_ref(&s.scope);
    assert!(is_not_found(store.get_insight(&b, insight.id).await));
    assert!(store
        .list_insights(&b, scopes, &InsightFilter::default(), 10)
        .await?
        .is_empty());
    assert!(store
        .list_quarantined_insights(&b, scopes, 10)
        .await?
        .is_empty());
    assert!(store
        .search_insights_vector(&b, scopes, &axis(2), 10)
        .await?
        .is_empty());
    assert!(store
        .search_insights_text(&b, scopes, "what it means", 10)
        .await?
        .is_empty());
    assert!(is_not_found(
        store.retire_insight(&b, insight.id, "x").await
    ));
    assert!(is_not_found(store.approve_insight(&b, held.id).await));
    assert!(is_not_found(store.reject_insight(&b, held.id).await));
    assert!(is_not_found(
        store
            .confirm_insight(&b, insight.id, &[f(s.f1), f(s.f2)])
            .await
    ));
    // Tenant A's facts and insights cannot be cited or superseded from tenant B.
    assert!(is_invalid(
        store
            .put_insight(
                &b,
                new_insight(
                    &s.scope,
                    InsightLevel::Context,
                    "x",
                    vec![f(s.f1), f(s.f2)],
                    Trust::Agent
                )
            )
            .await
    ));
    let bs = subject(h, &b, &s.scope).await?;
    let (_, b1) = fact(h, &b, &s.scope, bs, "b one", Trust::User).await?;
    let (_, b2) = fact(h, &b, &s.scope, bs, "b two", Trust::User).await?;
    assert!(is_invalid(
        store
            .put_insight(
                &b,
                new_insight(
                    &s.scope,
                    InsightLevel::Understanding,
                    "x",
                    vec![i(insight.id), f(b1)],
                    Trust::Agent
                )
            )
            .await
    ));
    let mut over = new_insight(
        &s.scope,
        InsightLevel::Context,
        "x",
        vec![f(b1), f(b2)],
        Trust::Agent,
    );
    over.supersedes = Some(insight.id);
    assert!(is_invalid(store.put_insight(&b, over).await));
    let own = write(h, &b, &s.scope, InsightLevel::Context, vec![f(b1), f(b2)]).await?;
    // B's changes and B's purge leave A alone.
    store.invalidate_fact(&b, b1, "wrong").await?;
    assert_eq!(status(h, &b, own.id).await?.status, InsightStatus::Stale);
    assert_eq!(
        status(h, &s.c, insight.id).await?.status,
        InsightStatus::Active
    );
    assert_eq!(store.purge_scope(&b, &s.scope).await?.insights_deleted, 1);
    assert_eq!(
        store
            .list_insights(&s.c, scopes, &InsightFilter::default(), 10)
            .await?
            .len(),
        2
    );
    assert!(h.summaries(&b).await?.is_empty());
    Ok(())
}
