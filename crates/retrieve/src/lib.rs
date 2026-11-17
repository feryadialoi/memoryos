//! Hybrid retrieval over facts: vector, text and graph lists merged by reciprocal rank fusion.
//! Recall also searches sections of current pages, ranked apart from facts.
//! See docs/SPEC.md sections 2, 7 and 14.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::future::join_all;
use memoryos_core::{
    Embedder, Error, Fact, FactFilter, MemoryStore, PageSectionHit, RequestContext, Reranker,
    Result, Scope, ScoredFact,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

mod fusion;
mod pages;

pub use pages::{PageHit, DEFAULT_PAGE_LIMIT, MAX_PAGE_LIMIT};

pub const DEFAULT_LIMIT: i64 = 10;
pub const MAX_LIMIT: i64 = 100;

const MAX_SCOPES: usize = 50;
const CANDIDATES_PER_LIST: i64 = 50;
const GRAPH_SEED_FACTS: usize = 5;
const GRAPH_DEPTH: i32 = 1;
/// Bounds how many edges the database reads for one subject, however many it has.
const GRAPH_EDGES_PER_SUBJECT: i64 = 50;
/// Bounds how many edges reach fusion, however many a subject has.
const MAX_GRAPH_CANDIDATES: usize = 50;
/// A slow embedder must not hold up recall, since text search can answer without it.
const DEFAULT_EMBED_TIMEOUT: Duration = Duration::from_secs(2);
/// The most fused facts recall sends to the reranker, unless `with_reranker` says otherwise. Each
/// pair costs about 25 ms on the CPU in the container, and on a 15-question test the right fact
/// was always in the fused top 10: 20 gave the same answers at about twice the time.
pub const DEFAULT_RERANK_TOP: usize = 10;
/// A slow reranker must not hold up recall, since the fused order is a usable answer.
const DEFAULT_RERANK_TIMEOUT: Duration = Duration::from_secs(2);

/// What a ranking is for, which sets its time filter and how the fused list is finished.
#[derive(Debug, Clone, Copy)]
enum Purpose<'a> {
    /// Facts at the requested time. Source count nudges the order, the reranker reorders the top
    /// and near copies give way to distinct facts.
    Recall(&'a FactFilter),
    /// Current facts related to a statement. Plain fusion, because finding near copies of the
    /// statement is the point.
    Candidates,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecallRequest {
    pub scopes: Vec<Scope>,
    pub query: String,
    #[serde(default = "default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub as_of: Option<DateTime<Utc>>,
    #[serde(default)]
    pub known_at: Option<DateTime<Utc>>,
    /// Most page hits to return, 0 to 10. 0 turns page search off.
    #[serde(default = "default_page_limit")]
    pub page_limit: i64,
}

fn default_limit() -> i64 {
    DEFAULT_LIMIT
}

fn default_page_limit() -> i64 {
    DEFAULT_PAGE_LIMIT
}

/// Why recall searched no pages although `page_limit` asked for some.
pub const PAGES_SKIPPED_TIME_TRAVEL: &str = "time_travel";

#[derive(Debug, Clone, Serialize)]
pub struct RecallResponse {
    pub facts: Vec<ScoredFact>,
    /// Best section per page, ranked apart from facts.
    pub pages: Vec<PageHit>,
    /// `time_travel` when `as_of` or `known_at` was set, since pages are searched at the present only.
    pub pages_skipped: Option<&'static str>,
}

pub struct Retriever {
    store: Arc<dyn MemoryStore>,
    embedder: Arc<dyn Embedder>,
    embed_timeout: Duration,
    rerank: Rerank,
}

impl Retriever {
    /// Without a reranker. Add one with `with_reranker`.
    pub fn new(store: Arc<dyn MemoryStore>, embedder: Arc<dyn Embedder>) -> Self {
        Self {
            store,
            embedder,
            embed_timeout: DEFAULT_EMBED_TIMEOUT,
            rerank: Rerank::default(),
        }
    }

    /// Recall reorders its first `min(limit * 2, top)` fused facts by `reranker`. `candidates` never
    /// reranks. A `top` below 2 turns reranking off, since one fact has nothing to reorder.
    pub fn with_reranker(mut self, reranker: Arc<dyn Reranker>, top: usize) -> Self {
        self.rerank.reranker = Some(reranker);
        self.rerank.top = top;
        self
    }

    /// How long to wait for the reranker before keeping the fused order. Defaults to 2 seconds.
    pub fn with_rerank_timeout(mut self, timeout: Duration) -> Self {
        self.rerank.timeout = timeout;
        self
    }

    /// How long to wait for the query embedding before using text search only. Defaults to 2 seconds.
    pub fn with_embed_timeout(mut self, timeout: Duration) -> Self {
        self.embed_timeout = timeout;
        self
    }

    /// Ranked facts at the time the request asks about.
    pub async fn recall(&self, ctx: &RequestContext, req: RecallRequest) -> Result<RecallResponse> {
        let limit = validate(&req.scopes, &req.query, req.limit)?;
        let filter = FactFilter {
            as_of: req.as_of,
            known_at: req.known_at,
        };
        let page_limit = pages::validate_page_limit(req.page_limit)?;
        let time_travel = req.as_of.is_some() || req.known_at.is_some();
        let page_limit = if time_travel { 0 } else { page_limit };
        let embedding = self.embed_query(&req.query).await;
        let (facts, pages) = self
            .ranked(
                ctx,
                &req.scopes,
                &req.query,
                embedding.as_deref(),
                limit,
                page_limit,
                Purpose::Recall(&filter),
            )
            .await?;
        Ok(RecallResponse {
            facts,
            pages,
            pages_skipped: time_travel.then_some(PAGES_SKIPPED_TIME_TRAVEL),
        })
    }

    /// Related current facts for a statement the host may commit.
    pub async fn candidates(
        &self,
        ctx: &RequestContext,
        scopes: &[Scope],
        statement: &str,
        limit: i64,
    ) -> Result<Vec<ScoredFact>> {
        let limit = validate(scopes, statement, limit)?;
        let embedding = self.embed_query(statement).await;
        self.ranked(
            ctx,
            scopes,
            statement,
            embedding.as_deref(),
            limit,
            0,
            Purpose::Candidates,
        )
        .await
        .map(|(facts, _)| facts)
    }

    /// `candidates` for a statement the caller has already embedded, so a commit that embeds its
    /// statements anyway does not pay for a second provider call.
    pub async fn candidates_with_embedding(
        &self,
        ctx: &RequestContext,
        scopes: &[Scope],
        statement: &str,
        embedding: &[f32],
        limit: i64,
    ) -> Result<Vec<ScoredFact>> {
        let limit = validate(scopes, statement, limit)?;
        self.ranked(
            ctx,
            scopes,
            statement,
            Some(embedding),
            limit,
            0,
            Purpose::Candidates,
        )
        .await
        .map(|(facts, _)| facts)
    }

    /// Facts, and with `page_limit` above 0 also page hits. Page searches run alongside the fact
    /// searches and share the reranker call; a failing page search only logs and returns no pages.
    #[allow(clippy::too_many_arguments)]
    async fn ranked(
        &self,
        ctx: &RequestContext,
        scopes: &[Scope],
        query: &str,
        embedding: Option<&[f32]>,
        limit: usize,
        page_limit: usize,
        purpose: Purpose<'_>,
    ) -> Result<(Vec<ScoredFact>, Vec<PageHit>)> {
        let current = FactFilter::default();
        let filter = match purpose {
            Purpose::Recall(filter) => filter,
            Purpose::Candidates => &current,
        };
        let (vector, text, (page_vector, page_text)) = tokio::join!(
            self.vector_list(ctx, scopes, embedding, filter),
            self.store
                .search_facts_text(ctx, scopes, query, filter, CANDIDATES_PER_LIST),
            self.page_lists(ctx, scopes, query, embedding, page_limit),
        );
        // Text search is the fallback for vector search, so nothing is left if it fails too.
        let text = facts_only(text?);

        // Fused twice on purpose: the first pass only picks which subjects to expand.
        let subjects = fusion::top_subjects(&fusion::fuse(&[&vector, &text]), GRAPH_SEED_FACTS);
        let graph = self.graph_list(ctx, scopes, &subjects, filter).await;

        let fused = fusion::fuse(&[&vector, &text, &graph]);
        let sections = pages::fuse_sections(&[&page_vector, &page_text]);
        let (ranked, sections) = match purpose {
            Purpose::Recall(_) => {
                let boosted = fusion::boost_by_sources(fused);
                let page_window = page_limit.saturating_mul(2);
                let (facts, sections) = self
                    .rerank
                    .apply_with_pages(query, boosted, limit, sections, page_window)
                    .await;
                (fusion::without_near_copies(facts), sections)
            }
            Purpose::Candidates => (fused, sections),
        };
        let facts = ranked
            .into_iter()
            .take(limit)
            .map(|(fact, score)| ScoredFact {
                fact: fact.clone(),
                score,
            })
            .collect();
        let pages = sections
            .into_iter()
            .take(page_limit)
            .map(|(section, score)| PageHit::new(section, score))
            .collect();
        Ok((facts, pages))
    }

    /// Vector and text section searches for page recall, each logged and empty on failure. Nothing
    /// is searched when `page_limit` is 0.
    async fn page_lists(
        &self,
        ctx: &RequestContext,
        scopes: &[Scope],
        query: &str,
        embedding: Option<&[f32]>,
        page_limit: usize,
    ) -> (Vec<PageSectionHit>, Vec<PageSectionHit>) {
        if page_limit == 0 {
            return (Vec::new(), Vec::new());
        }
        let vector = async {
            let Some(embedding) = embedding else {
                return Ok(Vec::new());
            };
            self.store
                .search_page_sections_vector(
                    ctx,
                    scopes,
                    embedding,
                    pages::PAGE_CANDIDATES_PER_LIST,
                )
                .await
        };
        let text = self.store.search_page_sections_text(
            ctx,
            scopes,
            query,
            pages::PAGE_CANDIDATES_PER_LIST,
        );
        let (vector, text) = tokio::join!(vector, text);
        let usable = |found: Result<Vec<PageSectionHit>>, list: &'static str| match found {
            Ok(found) => found,
            Err(error) => {
                let kind = fusion::error_kind(&error);
                tracing::warn!(kind, list, "page search failed, leaving it out");
                Vec::new()
            }
        };
        (usable(vector, "vector"), usable(text, "text"))
    }

    async fn embed_query(&self, query: &str) -> Option<Vec<f32>> {
        let input = [query.to_owned()];
        let embedded = tokio::time::timeout(self.embed_timeout, self.embedder.embed(&input)).await;
        match embedded {
            Ok(Ok(vectors)) => {
                let first = vectors.into_iter().next();
                if first.is_none() {
                    tracing::warn!("embedder returned no vector, using text search only");
                }
                first
            }
            Ok(Err(error)) => {
                let kind = fusion::error_kind(&error);
                tracing::warn!(kind, "query embedding failed, using text search only");
                None
            }
            Err(_) => {
                let timeout_ms = self.embed_timeout.as_millis() as u64;
                tracing::warn!(
                    timeout_ms,
                    "query embedding timed out, using text search only"
                );
                None
            }
        }
    }

    async fn vector_list(
        &self,
        ctx: &RequestContext,
        scopes: &[Scope],
        embedding: Option<&[f32]>,
        filter: &FactFilter,
    ) -> Vec<Fact> {
        let Some(embedding) = embedding else {
            return Vec::new();
        };
        let found = self
            .store
            .search_facts_vector(ctx, scopes, embedding, filter, CANDIDATES_PER_LIST)
            .await;
        match found {
            Ok(scored) => facts_only(scored),
            Err(error) => {
                let kind = fusion::error_kind(&error);
                tracing::warn!(kind, "vector search failed, using text search only");
                Vec::new()
            }
        }
    }

    async fn graph_list(
        &self,
        ctx: &RequestContext,
        scopes: &[Scope],
        subjects: &[Uuid],
        filter: &FactFilter,
    ) -> Vec<Fact> {
        let expansions = subjects.iter().map(|&subject| async move {
            let neighborhood = self
                .store
                .neighbors(
                    ctx,
                    scopes,
                    subject,
                    GRAPH_DEPTH,
                    filter,
                    GRAPH_EDGES_PER_SUBJECT,
                )
                .await;
            (subject, neighborhood)
        });
        let results = join_all(expansions).await;
        fusion::interleave(fusion::usable_edges(results), MAX_GRAPH_CANDIDATES)
    }
}

/// The reranking step of recall.
struct Rerank {
    reranker: Option<Arc<dyn Reranker>>,
    top: usize,
    timeout: Duration,
}

impl Default for Rerank {
    fn default() -> Self {
        Self {
            reranker: None,
            top: DEFAULT_RERANK_TOP,
            timeout: DEFAULT_RERANK_TIMEOUT,
        }
    }
}

impl Rerank {
    /// Reorders the first `min(limit * 2, top)` facts by the reranker. Twice the limit, so a
    /// relevant fact just below the cut can rise into it, and the near copies dropped next leave
    /// room. With no reranker, or on any failure or timeout, the order stays as it came.
    #[cfg(test)]
    async fn apply<'a>(
        &self,
        query: &str,
        ranked: Vec<(&'a Fact, f64)>,
        limit: usize,
    ) -> Vec<(&'a Fact, f64)> {
        self.apply_with_pages(query, ranked, limit, Vec::new(), 0)
            .await
            .0
    }

    /// `apply` for facts, then a second reranker call for the first `page_window` sections. The
    /// calls stay apart on purpose: a cross-encoder batch is padded to its longest pair, so long
    /// sections in the facts' batch would slow it and shift the facts' scores. With them apart the
    /// facts rank exactly as with no pages. A window under 2 leaves that list as it came, and any
    /// failure or timeout keeps that list's fused order.
    async fn apply_with_pages<'a, 'b>(
        &self,
        query: &str,
        facts: Vec<(&'a Fact, f64)>,
        limit: usize,
        sections: Vec<(&'b PageSectionHit, f64)>,
        page_window: usize,
    ) -> (Vec<(&'a Fact, f64)>, Vec<(&'b PageSectionHit, f64)>) {
        let Some(reranker) = &self.reranker else {
            return (facts, sections);
        };
        let fact_window = limit.saturating_mul(2).min(self.top).min(facts.len());
        let facts = if fact_window < 2 {
            facts
        } else {
            let statements: Vec<String> = facts[..fact_window]
                .iter()
                .map(|(fact, _)| fact.statement.clone())
                .collect();
            match self.score(reranker, query, &statements, "facts").await {
                Some(scores) => fusion::reordered_by(facts, &scores),
                None => facts,
            }
        };
        let page_window = page_window.min(sections.len());
        let sections = if page_window < 2 {
            sections
        } else {
            let statements: Vec<String> = sections[..page_window]
                .iter()
                .map(|(section, _)| pages::rerank_text(section))
                .collect();
            match self.score(reranker, query, &statements, "pages").await {
                Some(scores) => fusion::reordered_by(sections, &scores),
                None => sections,
            }
        };
        (facts, sections)
    }

    /// One reranker call with the timeout. `None`, after a warning, on failure, timeout or a wrong
    /// number of scores.
    async fn score(
        &self,
        reranker: &Arc<dyn Reranker>,
        query: &str,
        statements: &[String],
        list: &'static str,
    ) -> Option<Vec<f32>> {
        let expected = statements.len();
        let scored = tokio::time::timeout(self.timeout, reranker.score(query, statements)).await;
        match scored {
            Ok(Ok(scores)) if scores.len() == expected => Some(scores),
            Ok(Ok(scores)) => {
                let returned = scores.len();
                tracing::warn!(
                    returned,
                    expected,
                    list,
                    "reranker returned the wrong number of scores, keeping the fused order"
                );
                None
            }
            Ok(Err(error)) => {
                let kind = fusion::error_kind(&error);
                tracing::warn!(kind, list, "reranking failed, keeping the fused order");
                None
            }
            Err(_) => {
                let timeout_ms = self.timeout.as_millis() as u64;
                tracing::warn!(
                    timeout_ms,
                    list,
                    "reranking timed out, keeping the fused order"
                );
                None
            }
        }
    }
}

fn facts_only(scored: Vec<ScoredFact>) -> Vec<Fact> {
    scored.into_iter().map(|scored| scored.fact).collect()
}

/// Checks the request and returns `limit` as a count. Messages never quote the query.
fn validate(scopes: &[Scope], query: &str, limit: i64) -> Result<usize> {
    if scopes.is_empty() || scopes.len() > MAX_SCOPES {
        return Err(Error::InvalidInput(format!(
            "scopes must have 1 to {MAX_SCOPES} entries, got {}",
            scopes.len()
        )));
    }
    if query.trim().is_empty() {
        return Err(Error::InvalidInput("query must not be empty".into()));
    }
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(Error::InvalidInput(format!(
            "limit must be in 1..={MAX_LIMIT}, got {limit}"
        )));
    }
    Ok(limit as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scopes(count: usize) -> Vec<Scope> {
        (0..count).map(|i| Scope::user(format!("u{i}"))).collect()
    }

    fn is_invalid(result: Result<usize>) -> bool {
        matches!(result, Err(Error::InvalidInput(_)))
    }

    fn parse(text: &str) -> RecallRequest {
        serde_json::from_str(text).expect("valid request")
    }

    #[test]
    fn unknown_fields_are_refused() {
        let attempt = r#"{"scopes": ["user:u"], "query": "q", "limitt": 5}"#;
        assert!(serde_json::from_str::<RecallRequest>(attempt).is_err());
    }

    #[test]
    fn scope_count_must_be_1_to_50() {
        assert!(is_invalid(validate(&scopes(0), "q", 10)));
        assert!(is_invalid(validate(&scopes(51), "q", 10)));
        assert_eq!(validate(&scopes(1), "q", 10).ok(), Some(10));
        assert_eq!(validate(&scopes(50), "q", 10).ok(), Some(10));
    }

    #[test]
    fn query_must_have_content() {
        for blank in ["", "   ", " \n\t "] {
            assert!(is_invalid(validate(&scopes(1), blank, 10)));
        }
    }

    #[test]
    fn limit_must_be_1_to_100() {
        for bad in [i64::MIN, -1, 0, 101, i64::MAX] {
            assert!(is_invalid(validate(&scopes(1), "q", bad)), "{bad}");
        }
        assert_eq!(validate(&scopes(1), "q", 1).ok(), Some(1));
        assert_eq!(validate(&scopes(1), "q", 100).ok(), Some(100));
    }

    #[test]
    fn request_defaults_limit_and_times() {
        let request = parse(r#"{"scopes":["user:a"],"query":"q"}"#);
        assert_eq!(request.limit, DEFAULT_LIMIT);
        assert!(request.as_of.is_none());
        assert!(request.known_at.is_none());
        assert_eq!(request.scopes, [Scope::user("a")]);

        let request = parse(
            r#"{"scopes":[],"query":"q","limit":3,"as_of":"2024-01-02T03:04:05Z","known_at":null}"#,
        );
        assert_eq!(request.limit, 3);
        assert!(request.as_of.is_some());
        assert!(request.known_at.is_none());
    }

    #[test]
    fn request_rejects_a_bad_scope() {
        let parsed = serde_json::from_str::<RecallRequest>(r#"{"scopes":["team:a"],"query":"q"}"#);
        assert!(parsed.is_err());
    }

    #[test]
    fn response_serializes_the_openapi_fields() {
        let response = RecallResponse {
            facts: vec![],
            pages: vec![],
            pages_skipped: None,
        };
        let json = serde_json::to_value(&response).expect("serializes");
        assert_eq!(
            json,
            serde_json::json!({"facts": [], "pages": [], "pages_skipped": null})
        );
    }

    mod rerank {
        use std::sync::Mutex;

        use async_trait::async_trait;
        use memoryos_core::{FactStatus, Trust};

        use super::*;

        /// Scores a statement by the number after its last space, after `delay`. Remembers what it
        /// was sent.
        struct FakeReranker {
            delay: Duration,
            fail: bool,
            short: bool,
            seen: Mutex<Vec<String>>,
            batches: Mutex<Vec<usize>>,
        }

        impl FakeReranker {
            fn new() -> Self {
                Self {
                    delay: Duration::ZERO,
                    fail: false,
                    short: false,
                    seen: Mutex::new(Vec::new()),
                    batches: Mutex::new(Vec::new()),
                }
            }

            fn seen(&self) -> Vec<String> {
                self.seen.lock().expect("lock").clone()
            }

            fn batches(&self) -> Vec<usize> {
                self.batches.lock().expect("lock").clone()
            }
        }

        #[async_trait]
        impl Reranker for FakeReranker {
            async fn score(&self, _query: &str, statements: &[String]) -> Result<Vec<f32>> {
                self.seen
                    .lock()
                    .expect("lock")
                    .extend_from_slice(statements);
                self.batches.lock().expect("lock").push(statements.len());
                tokio::time::sleep(self.delay).await;
                if self.fail {
                    return Err(Error::Provider("model is not loaded yet".into()));
                }
                let mut scores: Vec<f32> = statements
                    .iter()
                    .map(|s| {
                        s.rsplit(' ')
                            .next()
                            .and_then(|n| n.parse().ok())
                            .unwrap_or(0.0)
                    })
                    .collect();
                if self.short {
                    scores.pop();
                }
                Ok(scores)
            }
        }

        /// Facts `fact 0` to `fact {n-1}` with fused scores falling from 1.0.
        fn facts(scores: &[f32]) -> Vec<Fact> {
            scores
                .iter()
                .enumerate()
                .map(|(n, score)| Fact {
                    id: Uuid::from_u128(n as u128),
                    scope: Scope::user("u"),
                    subject_id: Uuid::from_u128(1000 + n as u128),
                    predicate: "p".into(),
                    object_id: None,
                    statement: format!("fact{n} {score}"),
                    trust: Trust::User,
                    status: FactStatus::Active,
                    valid_from: Utc::now(),
                    valid_to: None,
                    expires_at: None,
                    recorded_at: Utc::now(),
                    invalidated_at: None,
                    superseded_by: None,
                    pending_supersede_of: None,
                    source_episode_ids: vec![],
                })
                .collect()
        }

        fn ranked(facts: &[Fact]) -> Vec<(&Fact, f64)> {
            facts
                .iter()
                .enumerate()
                .map(|(n, fact)| (fact, 1.0 - n as f64 / 100.0))
                .collect()
        }

        fn order(ranked: &[(&Fact, f64)]) -> Vec<u128> {
            ranked.iter().map(|(fact, _)| fact.id.as_u128()).collect()
        }

        fn step(reranker: Option<Arc<FakeReranker>>, top: usize) -> Rerank {
            Rerank {
                reranker: reranker.map(|r| r as Arc<dyn Reranker>),
                top,
                timeout: Duration::from_millis(100),
            }
        }

        #[tokio::test]
        async fn reorders_the_window_by_score_and_leaves_the_rest() {
            let reranker = Arc::new(FakeReranker::new());
            let facts = facts(&[0.1, 0.2, 0.9, 0.3, 5.0]);
            let out = step(Some(reranker.clone()), 20)
                .apply("q", ranked(&facts), 2)
                .await;
            // limit 2 sends four facts, so fact 4 stays last despite its high score.
            assert_eq!(order(&out), [2, 3, 1, 0, 4]);
            assert_eq!(reranker.seen().len(), 4);
            let scores: Vec<f64> = out.iter().map(|(_, score)| *score).collect();
            assert_eq!(scores, [1.0, 0.99, 0.98, 0.97, 0.96]);
        }

        #[tokio::test]
        async fn the_window_is_capped_by_top() {
            let reranker = Arc::new(FakeReranker::new());
            let facts = facts(&[0.1, 0.2, 0.9, 0.3]);
            let out = step(Some(reranker.clone()), 2)
                .apply("q", ranked(&facts), 10)
                .await;
            assert_eq!(order(&out), [1, 0, 2, 3]);
            assert_eq!(reranker.seen(), ["fact0 0.1", "fact1 0.2"]);
        }

        #[tokio::test]
        async fn off_keeps_the_fused_order() {
            let facts = facts(&[0.1, 0.9]);
            let out = step(None, 20).apply("q", ranked(&facts), 10).await;
            assert_eq!(order(&out), [0, 1]);
        }

        #[tokio::test]
        async fn a_window_of_one_calls_nothing() {
            let reranker = Arc::new(FakeReranker::new());
            let facts = facts(&[0.1, 0.9]);
            let out = step(Some(reranker.clone()), 20)
                .apply("q", ranked(&facts), 10)
                .await;
            assert_eq!(order(&out), [1, 0]);
            let out = step(Some(reranker.clone()), 1)
                .apply("q", ranked(&facts), 10)
                .await;
            assert_eq!(order(&out), [0, 1]);
            assert_eq!(reranker.seen().len(), 2, "only the first call reached it");
        }

        #[tokio::test]
        async fn a_timeout_keeps_the_fused_order() {
            let reranker = Arc::new(FakeReranker {
                delay: Duration::from_secs(5),
                ..FakeReranker::new()
            });
            let facts = facts(&[0.1, 0.9, 0.5]);
            let started = std::time::Instant::now();
            let out = step(Some(reranker), 20)
                .apply("q", ranked(&facts), 10)
                .await;
            assert_eq!(order(&out), [0, 1, 2]);
            assert!(started.elapsed() < Duration::from_secs(2));
        }

        #[tokio::test]
        async fn a_failure_or_a_short_answer_keeps_the_fused_order() {
            let facts = facts(&[0.1, 0.9, 0.5]);
            for reranker in [
                FakeReranker {
                    fail: true,
                    ..FakeReranker::new()
                },
                FakeReranker {
                    short: true,
                    ..FakeReranker::new()
                },
            ] {
                let out = step(Some(Arc::new(reranker)), 20)
                    .apply("q", ranked(&facts), 10)
                    .await;
                assert_eq!(order(&out), [0, 1, 2]);
            }
        }

        /// Sections of pages 0 to n-1, whose text ends with the given score, fused scores falling.
        fn sections(scores: &[f32]) -> Vec<PageSectionHit> {
            scores
                .iter()
                .enumerate()
                .map(|(n, score)| PageSectionHit {
                    section_id: Uuid::from_u128(500 + n as u128),
                    page_id: Uuid::from_u128(n as u128),
                    scope: Scope::shared("s"),
                    slug: format!("p{n}"),
                    title: format!("Page {n}"),
                    version: 1,
                    heading: format!("Page {n} > Intro"),
                    text: format!("section{n} {score}"),
                    score: 0.0,
                })
                .collect()
        }

        fn ranked_sections(sections: &[PageSectionHit]) -> Vec<(&PageSectionHit, f64)> {
            sections
                .iter()
                .enumerate()
                .map(|(n, section)| (section, 1.0 - n as f64 / 100.0))
                .collect()
        }

        fn page_order(ranked: &[(&PageSectionHit, f64)]) -> Vec<u128> {
            ranked.iter().map(|(s, _)| s.page_id.as_u128()).collect()
        }

        #[tokio::test]
        async fn facts_and_sections_are_scored_in_separate_calls() {
            let reranker = Arc::new(FakeReranker::new());
            let facts = facts(&[0.1, 0.9]);
            let pages = sections(&[0.2, 0.8, 0.5]);
            let (out, page_out) = step(Some(reranker.clone()), 20)
                .apply_with_pages("q", ranked(&facts), 1, ranked_sections(&pages), 2)
                .await;
            assert_eq!(order(&out), [1, 0]);
            // Only the page window of 2 is reranked; page 2 stays behind it.
            assert_eq!(page_order(&page_out), [1, 0, 2]);
            assert_eq!(
                reranker.seen(),
                [
                    "fact0 0.1",
                    "fact1 0.9",
                    "Page 0 > Intro\nsection0 0.2",
                    "Page 1 > Intro\nsection1 0.8"
                ]
            );
            // Facts first, then sections, never in one batch.
            assert_eq!(reranker.batches(), [2, 2]);
            // Slots keep their fused scores, so each list stays descending.
            assert_eq!(page_out[0].1, 1.0);
        }

        #[tokio::test]
        async fn fact_results_do_not_depend_on_pages() {
            let facts = facts(&[0.1, 0.9, 0.5, 0.7]);
            let pages = sections(&[0.3, 0.6]);
            let without = step(Some(Arc::new(FakeReranker::new())), 20)
                .apply("q", ranked(&facts), 10)
                .await;
            let (with, _) = step(Some(Arc::new(FakeReranker::new())), 20)
                .apply_with_pages("q", ranked(&facts), 10, ranked_sections(&pages), 6)
                .await;
            assert_eq!(order(&with), order(&without));
        }

        #[tokio::test]
        async fn sections_rerank_even_when_facts_are_too_few() {
            let reranker = Arc::new(FakeReranker::new());
            let facts = facts(&[0.1]);
            let pages = sections(&[0.2, 0.8]);
            let (out, page_out) = step(Some(reranker.clone()), 20)
                .apply_with_pages("q", ranked(&facts), 10, ranked_sections(&pages), 6)
                .await;
            assert_eq!(order(&out), [0]);
            assert_eq!(page_order(&page_out), [1, 0]);
            assert_eq!(reranker.seen().len(), 2);
        }

        #[tokio::test]
        async fn a_failure_keeps_both_fused_orders() {
            let reranker = FakeReranker {
                fail: true,
                ..FakeReranker::new()
            };
            let facts = facts(&[0.1, 0.9]);
            let pages = sections(&[0.2, 0.8]);
            let (out, page_out) = step(Some(Arc::new(reranker)), 20)
                .apply_with_pages("q", ranked(&facts), 10, ranked_sections(&pages), 6)
                .await;
            assert_eq!(order(&out), [0, 1]);
            assert_eq!(page_order(&page_out), [0, 1]);
        }
    }
}
