//! Backend-independent retrieval pipeline scenarios, run against PostgreSQL and SQLite fixtures.

use std::collections::BTreeSet;
use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use memoryos_core::{
    FactFilter, FactOp, FactStatus, InsightLevel, InsightSourceKind, InsightSourceRef,
    InsightStatus, MemoryStore, NewEntity, NewEpisode, NewFact, NewInsight, NewPageSection,
    NewPageVersion, PageVersionStatus, RequestContext, Scope, ScoredFact, ScoredInsight, Trust,
    EMBEDDING_DIMS,
};
use memoryos_embed::TestEmbedder;
use memoryos_retrieve::{RecallRequest, RecallResponse, Retriever};
use serde_json::json;
use uuid::Uuid;

pub const QUERY: &str = "zebra";

pub struct Scenario {
    store: Arc<dyn MemoryStore>,
    embedder: Arc<TestEmbedder>,
    ctx: RequestContext,
    scope: Scope,
}

impl Scenario {
    pub fn new(
        store: Arc<dyn MemoryStore>,
        embedder: Arc<TestEmbedder>,
        ctx: RequestContext,
        scope: Scope,
    ) -> Self {
        Self {
            store,
            embedder,
            ctx,
            scope,
        }
    }

    async fn entity_in(&self, ctx: &RequestContext, scope: &Scope, name: &str) -> Uuid {
        self.store
            .upsert_entity(
                ctx,
                NewEntity {
                    scope: scope.clone(),
                    kind: "thing".into(),
                    name: name.into(),
                    aliases: Vec::new(),
                    embedding: at_angle(0.0),
                },
            )
            .await
            .expect("upsert entity")
            .id
    }

    async fn entity(&self, name: &str) -> Uuid {
        self.entity_in(&self.ctx, &self.scope, name).await
    }

    async fn apply_in(
        &self,
        ctx: &RequestContext,
        scope: &Scope,
        trust: Trust,
        ops: Vec<FactOp>,
    ) -> Vec<Uuid> {
        let (source, _) = self
            .store
            .claim_episode(ctx, episode(scope, trust, "source"))
            .await
            .expect("insert episode");
        self.store
            .apply_commit(ctx, source.id, ops)
            .await
            .expect("apply commit")
    }

    async fn add(&self, subject: Uuid, statement: &str, degrees: f32) -> Uuid {
        let fact = new_fact(&self.scope, subject, statement, degrees);
        self.apply_in(&self.ctx, &self.scope, Trust::User, vec![FactOp::Add(fact)])
            .await[0]
    }

    fn request(&self, query: &str) -> RecallRequest {
        RecallRequest {
            scopes: vec![self.scope.clone()],
            query: query.into(),
            limit: 100,
            as_of: None,
            known_at: None,
            page_limit: memoryos_retrieve::DEFAULT_PAGE_LIMIT,
            insight_limit: memoryos_retrieve::DEFAULT_INSIGHT_LIMIT,
        }
    }

    pub async fn recall(&self, query: &str) -> RecallResponse {
        Retriever::new(self.store.clone(), self.embedder.clone())
            .recall(&self.ctx, self.request(query))
            .await
            .expect("recall")
    }

    async fn vector_hits(&self, filter: &FactFilter) -> BTreeSet<Uuid> {
        let found = self
            .store
            .search_facts_vector(
                &self.ctx,
                std::slice::from_ref(&self.scope),
                &at_angle(0.0),
                filter,
                50,
            )
            .await
            .expect("vector search");
        ids(&found)
    }

    async fn text_hits(&self, filter: &FactFilter) -> BTreeSet<Uuid> {
        let found = self
            .store
            .search_facts_text(
                &self.ctx,
                std::slice::from_ref(&self.scope),
                QUERY,
                filter,
                50,
            )
            .await
            .expect("text search");
        ids(&found)
    }

    pub async fn finds_a_fact_only_by_vector(&self) -> (Uuid, RecallResponse) {
        let subject = self.entity("a").await;
        let target = self
            .add(subject, "nothing in common with the query", 1.0)
            .await;

        let filter = FactFilter::default();
        assert!(self.vector_hits(&filter).await.contains(&target));
        assert!(!self.text_hits(&filter).await.contains(&target));

        let response = self.recall(QUERY).await;
        assert!(ids(&response.facts).contains(&target));
        (target, response)
    }

    pub async fn superseded_fact_is_absent_now_and_present_with_as_of(&self) {
        let subject = self.entity("a").await;
        let old = self.add(subject, "the zebra lives in Paris", 1.0).await;
        let mut replacement = new_fact(&self.scope, subject, "the zebra lives in Rome", 1.0);
        replacement.valid_from = date(2022);
        let op = FactOp::Supersede {
            old_fact_id: old,
            new: replacement,
        };
        let new = self
            .apply_in(&self.ctx, &self.scope, Trust::User, vec![op])
            .await[0];

        let now = ids(&self.recall(QUERY).await.facts);
        assert!(now.contains(&new) && !now.contains(&old));

        let mut past = self.request(QUERY);
        past.as_of = Some(date(2021));
        let then = Retriever::new(self.store.clone(), self.embedder.clone())
            .recall(&self.ctx, past)
            .await
            .expect("recall");
        let then = ids(&then.facts);
        assert!(then.contains(&old) && !then.contains(&new));
    }

    pub async fn other_tenant_is_absent(&self) {
        let other = RequestContext {
            tenant: "tenant-b".into(),
            actor: "test".into(),
        };
        let mine = self
            .add(self.entity("a").await, "a zebra was seen", 1.0)
            .await;
        let subject = self.entity_in(&other, &self.scope, "a").await;
        let fact = new_fact(&self.scope, subject, "a zebra was seen", 1.0);
        let theirs = self
            .apply_in(&other, &self.scope, Trust::User, vec![FactOp::Add(fact)])
            .await[0];

        let response = self.recall(QUERY).await;
        assert_eq!(ids(&response.facts), BTreeSet::from([mine]));

        let response = Retriever::new(self.store.clone(), self.embedder.clone())
            .recall(&other, self.request(QUERY))
            .await
            .expect("recall");
        assert_eq!(ids(&response.facts), BTreeSet::from([theirs]));
    }
}

impl Scenario {
    /// Writes a one-section page at `degrees` from the query vector.
    async fn page(&self, slug: &str, text: &str, degrees: f32, trust: Trust) -> PageVersionStatus {
        self.store
            .put_page(
                &self.ctx,
                NewPageVersion {
                    scope: self.scope.clone(),
                    slug: slug.into(),
                    title: slug.into(),
                    body: text.into(),
                    summary: None,
                    trust,
                    expected_version: None,
                    approve_up_to: None,
                    sections: vec![NewPageSection {
                        ordinal: 0,
                        heading: format!("{slug} > Notes"),
                        text: text.into(),
                        embedding: at_angle(degrees),
                    }],
                },
            )
            .await
            .expect("write page")
            .status
    }

    /// Pages come back as their own list, only current versions, and leave facts unchanged.
    pub async fn pages_are_ranked_apart_and_only_current(&self) {
        let fact = self
            .add(self.entity("a").await, "a zebra was seen", 1.0)
            .await;
        // The user writes at user trust, so these are current at once.
        assert_eq!(
            self.page("zebra-care", "feed the zebra twice a day", 0.0, Trust::User)
                .await,
            PageVersionStatus::Active
        );
        assert_eq!(
            self.page("far-away", "nothing in common", 85.0, Trust::User)
                .await,
            PageVersionStatus::Active
        );
        // An agent writing to a user scope is held for review, so recall never shows it.
        assert_eq!(
            self.page("held", "zebra zebra zebra", 0.0, Trust::Agent)
                .await,
            PageVersionStatus::Quarantined
        );

        let response = self.recall(QUERY).await;
        let slugs: Vec<&str> = response.pages.iter().map(|hit| hit.slug.as_str()).collect();
        assert_eq!(slugs.first(), Some(&"zebra-care"));
        assert!(!slugs.contains(&"held"));
        assert!(response.pages.len() <= 3);
        assert_eq!(response.pages[0].section, "zebra-care > Notes");
        assert_eq!(response.pages[0].snippet, "feed the zebra twice a day");
        assert_eq!(response.pages_skipped, None);

        let mut without = self.request(QUERY);
        without.page_limit = 0;
        let without = Retriever::new(self.store.clone(), self.embedder.clone())
            .recall(&self.ctx, without)
            .await
            .expect("recall without pages");
        assert!(without.pages.is_empty());
        assert_eq!(ids(&without.facts), ids(&response.facts));
        assert!(ids(&response.facts).contains(&fact));

        let mut then = self.request(QUERY);
        then.as_of = Some(Utc::now());
        let then = Retriever::new(self.store.clone(), self.embedder.clone())
            .recall(&self.ctx, then)
            .await
            .expect("recall as of now");
        assert!(then.pages.is_empty());
        assert_eq!(then.pages_skipped, Some("time_travel"));
    }
}

impl Scenario {
    /// Writes an insight at user trust citing `facts`, at `degrees` from the query vector.
    async fn insight(
        &self,
        level: InsightLevel,
        statement: &str,
        degrees: f32,
        facts: &[Uuid],
    ) -> (Uuid, InsightStatus) {
        let sources = facts
            .iter()
            .map(|&id| InsightSourceRef {
                kind: InsightSourceKind::Fact,
                id,
            })
            .collect();
        let insight = self
            .store
            .put_insight(
                &self.ctx,
                NewInsight {
                    scope: self.scope.clone(),
                    level,
                    statement: statement.into(),
                    embedding: at_angle(degrees),
                    trust: Trust::User,
                    sources,
                    supersedes: None,
                    written_by: "test".into(),
                },
            )
            .await
            .expect("write insight");
        (insight.id, insight.status)
    }

    async fn recall_with(&self, request: RecallRequest) -> RecallResponse {
        Retriever::new(self.store.clone(), self.embedder.clone())
            .recall(&self.ctx, request)
            .await
            .expect("recall")
    }

    /// Insights come back as their own list, only active ones, cut at `insight_limit`, skipped
    /// with time travel, and leave facts exactly as they rank without insights.
    pub async fn insights_are_recalled_apart_and_only_active(&self) {
        let subject = self.entity("a").await;
        let mut facts = Vec::new();
        for n in 0..4 {
            facts.push(
                self.add(subject, &format!("a zebra was seen {n}"), 1.0)
                    .await,
            );
        }
        let cited = &facts[..2];

        use InsightLevel::*;
        let (best, status) = self
            .insight(Context, "the zebra lives nearby", 0.0, cited)
            .await;
        assert_eq!(status, InsightStatus::Active);
        let mut active = BTreeSet::from([best]);
        for n in 0..5 {
            let statement = format!("unrelated pattern {n}");
            let (id, status) = self
                .insight(Understanding, &statement, 30.0 + n as f32 * 10.0, cited)
                .await;
            assert_eq!(status, InsightStatus::Active);
            active.insert(id);
        }
        // Guidance outside an agent scope is held for review.
        let (held, status) = self
            .insight(Guidance, "zebra zebra zebra", 0.0, cited)
            .await;
        assert_eq!(status, InsightStatus::Quarantined);
        // An insight whose source fact is invalidated goes stale.
        let (stale, status) = self
            .insight(Context, "the zebra herd", 0.0, &facts[2..])
            .await;
        assert_eq!(status, InsightStatus::Active);
        self.store
            .invalidate_fact(&self.ctx, facts[2], "wrong")
            .await
            .expect("invalidate source");

        let response = self.recall(QUERY).await;
        assert_eq!(response.insights.len(), 3, "insight_limit defaults to 3");
        assert_eq!(response.insights[0].insight.id, best);
        assert_eq!(response.insights_skipped, None);
        let scores: Vec<f64> = response.insights.iter().map(|hit| hit.score).collect();
        assert!(
            scores.windows(2).all(|pair| pair[0] >= pair[1]),
            "{scores:?}"
        );

        let mut all = self.request(QUERY);
        all.insight_limit = 10;
        let all = self.recall_with(all).await;
        assert_eq!(insight_ids(&all.insights), active);
        assert!(!insight_ids(&all.insights).contains(&held));
        assert!(!insight_ids(&all.insights).contains(&stale));
        assert!(all
            .insights
            .iter()
            .all(|hit| hit.insight.status == InsightStatus::Active));

        let mut two = self.request(QUERY);
        two.insight_limit = 2;
        assert_eq!(self.recall_with(two).await.insights.len(), 2);

        let mut off = self.request(QUERY);
        off.insight_limit = 0;
        let off = self.recall_with(off).await;
        assert!(off.insights.is_empty());
        assert_eq!(off.insights_skipped, None);
        assert_eq!(fact_shape(&off.facts), fact_shape(&response.facts));
        assert!(!response.facts.is_empty());

        let mut as_of = self.request(QUERY);
        as_of.as_of = Some(Utc::now());
        let mut known_at = self.request(QUERY);
        known_at.known_at = Some(Utc::now());
        for past in [as_of, known_at] {
            let past = self.recall_with(past).await;
            assert!(past.insights.is_empty());
            assert_eq!(past.insights_skipped, Some("time_travel"));
        }
    }
}

impl Scenario {
    /// After `break_insights` makes both insight searches fail, recall still returns facts and
    /// pages, with no insights.
    pub async fn recall_survives_failing_insight_searches(
        &self,
        break_insights: impl std::future::Future<Output = ()>,
    ) {
        let subject = self.entity("a").await;
        let facts = [
            self.add(subject, "a zebra was seen", 1.0).await,
            self.add(subject, "a zebra ran", 2.0).await,
        ];
        self.insight(InsightLevel::Context, "the zebra lives nearby", 0.0, &facts)
            .await;
        self.page("zebra-care", "feed the zebra", 0.0, Trust::User)
            .await;
        assert_eq!(self.recall(QUERY).await.insights.len(), 1);

        break_insights.await;
        let response = self.recall(QUERY).await;
        assert!(response.insights.is_empty());
        assert_eq!(response.insights_skipped, None);
        assert_eq!(ids(&response.facts), BTreeSet::from(facts));
        assert_eq!(response.pages.len(), 1);
    }
}

fn insight_ids(insights: &[ScoredInsight]) -> BTreeSet<Uuid> {
    insights.iter().map(|hit| hit.insight.id).collect()
}

/// Fact ids in order with their exact scores.
fn fact_shape(facts: &[ScoredFact]) -> Vec<(Uuid, u64)> {
    facts
        .iter()
        .map(|scored| (scored.fact.id, scored.score.to_bits()))
        .collect()
}

fn at_angle(degrees: f32) -> Vec<f32> {
    let mut vector = vec![0.0; EMBEDDING_DIMS];
    vector[0] = degrees.to_radians().cos();
    vector[1] = degrees.to_radians().sin();
    vector
}

fn date(year: i32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, 1, 1, 0, 0, 0)
        .single()
        .expect("valid date")
}

fn new_fact(scope: &Scope, subject: Uuid, statement: &str, degrees: f32) -> NewFact {
    NewFact {
        scope: scope.clone(),
        subject_id: subject,
        predicate: "says".into(),
        object_id: None,
        statement: statement.into(),
        embedding: at_angle(degrees),
        trust: Trust::User,
        status: FactStatus::Active,
        valid_from: date(2020),
        valid_to: None,
        expires_at: None,
    }
}

fn episode(scope: &Scope, trust: Trust, content: &str) -> NewEpisode {
    NewEpisode {
        scope: scope.clone(),
        source: "test".into(),
        trust,
        content: content.into(),
        occurred_at: None,
        metadata: json!({}),
        idempotency_key: None,
    }
}

fn ids(facts: &[ScoredFact]) -> BTreeSet<Uuid> {
    facts.iter().map(|scored| scored.fact.id).collect()
}
