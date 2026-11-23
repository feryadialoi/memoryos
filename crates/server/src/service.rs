//! Every operation of the API, with its authorization. REST handlers and MCP tools both call these,
//! so the two surfaces cannot disagree on a check (SPEC sections 3 and 6).

use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use memoryos_core::{
    Block, BlockVersion, Embedder, Entity, Episode, Error, Fact, FactFilter, FactListState,
    FactView, GraphNeighborhood, Insight, InsightDetail, InsightFilter, InsightLevel,
    InsightSourceRef, MemoryStore, NewInsight, Page, PageSummary, PageVersion, PendingPageVersion,
    PurgeReport, ReflectBatch, Scope, ScopeSummary, ScoredFact, Trust, INSIGHT_STATEMENT_MAX,
    REFLECT_BATCH_DEFAULT, REFLECT_BATCH_MAX,
};
use memoryos_ingest::{
    redact, CommitRequest, CommitResponse, HostCommitter, PageWrite, PageWriter,
};
use memoryos_retrieve::{RecallRequest, RecallResponse, Retriever};
use memoryos_store::Store;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::Caller;
use crate::error::{ApiError, ApiResult};
use crate::telemetry::Metrics;

const DEFAULT_MAX_BLOCK_CHARS: i32 = 2000;
const MAX_BLOCK_CHARS: i32 = 100_000;
const DEFAULT_CANDIDATES: i64 = 10;
const DEFAULT_QUARANTINE_PAGE: i64 = 50;
const MAX_QUARANTINE_PAGE: i64 = 200;
const DEFAULT_FACT_PAGE: i64 = 100;
const MAX_FACT_PAGE: i64 = 500;
const DEFAULT_PAGE_LIST: i64 = 100;
const MAX_PAGE_LIST: i64 = 500;
const MAX_SCOPE_SUMMARIES: usize = 500;
const DEFAULT_INSIGHT_LIST: i64 = 100;
const MAX_INSIGHT_LIST: i64 = 500;
/// Longest retire reason, in characters (docs/openapi.yaml).
const MAX_RETIRE_REASON: usize = 500;
const DEFAULT_DEPTH: i32 = 1;
/// Edges one neighbors request may read. A hub entity would otherwise answer with the store's cap
/// of 5000, each edge carrying its full statement.
const NEIGHBORS_EDGE_LIMIT: i64 = 500;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidatesRequest {
    pub scopes: Vec<Scope>,
    pub statement: String,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PutBlockRequest {
    pub content: String,
    #[serde(default)]
    pub max_chars: Option<i32>,
    #[serde(default)]
    pub expected_version: Option<i64>,
}

/// Body of `PUT /v1/pages/{scope}/{slug}`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageWriteRequest {
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub expected_version: Option<i64>,
    /// Admin keys only: write and approve in one step.
    #[serde(default)]
    pub approve: bool,
}

/// The version written, with whether a secret was masked first.
#[derive(Debug, Serialize)]
pub struct PageWriteResponse {
    #[serde(flatten)]
    pub version: PageVersion,
    pub redacted: bool,
}

/// Body of `POST /v1/insights`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InsightWriteRequest {
    pub scope: Scope,
    pub level: InsightLevel,
    pub statement: String,
    pub sources: Vec<InsightSourceRef>,
    #[serde(default)]
    pub supersedes: Option<Uuid>,
}

/// The insight written, with whether a secret was masked in its statement first.
#[derive(Debug, Serialize)]
pub struct InsightWriteResponse {
    pub insight: Insight,
    pub redacted: bool,
}

/// Body of `POST /v1/reflect/batch`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReflectBatchRequest {
    pub scope: Scope,
    #[serde(default)]
    pub limit: Option<i64>,
}

/// Body of `POST /v1/reflect/decline`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReflectDeclineRequest {
    pub scope: Scope,
    pub topic_key: Uuid,
    pub fact_set: String,
}

pub struct Services {
    pub store: Arc<Store>,
    pub retriever: Arc<Retriever>,
    pub committer: HostCommitter,
    pub pages: PageWriter,
    /// Embeds insight statements with the model facts and pages use.
    pub embedder: Arc<dyn Embedder>,
    /// `MEMORYOS_REFLECT_MIN_FACTS`.
    pub reflect_min_facts: i64,
    pub metrics: Metrics,
}

impl Services {
    // Episodes

    pub async fn get_episode(&self, caller: &Caller, id: Uuid) -> ApiResult<Episode> {
        let episode = self.store.get_episode(&caller.ctx, id).await?;
        caller.require_visible(&episode.scope)?;
        Ok(episode)
    }

    pub async fn purge_episode(&self, caller: &Caller, id: Uuid) -> ApiResult<PurgeReport> {
        caller.require_admin()?;
        // Episode trust covers what the purge can touch: a fact's trust is at most that of its
        // highest source, and a fact with another source only loses this link.
        let episode = self.get_episode(caller, id).await?;
        require_trust(caller, "episode", episode.trust)?;
        Ok(self.store.purge_episode(&caller.ctx, id).await?)
    }

    // Search

    pub async fn recall(&self, caller: &Caller, req: RecallRequest) -> ApiResult<RecallResponse> {
        caller.require_scopes(&req.scopes)?;
        let started = Instant::now();
        let response = self.retriever.recall(&caller.ctx, req).await?;
        self.metrics
            .recall_finished(started.elapsed().as_secs_f64());
        Ok(response)
    }

    pub async fn candidates(
        &self,
        caller: &Caller,
        req: CandidatesRequest,
    ) -> ApiResult<Vec<ScoredFact>> {
        caller.require_scopes(&req.scopes)?;
        Ok(self
            .retriever
            .candidates(
                &caller.ctx,
                &req.scopes,
                &req.statement,
                req.limit.unwrap_or(DEFAULT_CANDIDATES),
            )
            .await?)
    }

    pub async fn commit(&self, caller: &Caller, req: CommitRequest) -> ApiResult<CommitResponse> {
        caller.require_scope(&req.scope)?;
        Ok(self
            .committer
            .commit(&caller.ctx, req, caller.key.max_trust)
            .await?)
    }

    // Facts

    pub async fn get_fact(&self, caller: &Caller, id: Uuid) -> ApiResult<Fact> {
        let fact = self.store.get_fact(&caller.ctx, id).await?;
        caller.require_visible(&fact.scope)?;
        Ok(fact)
    }

    /// The scopes this key may read, most recently written first, and whether the list was cut.
    /// Only an admin can list quarantined facts, so only an admin sees how many there are.
    pub async fn list_scopes(&self, caller: &Caller) -> ApiResult<(Vec<ScopeSummary>, bool)> {
        let mut scopes = self.store.scope_summaries(&caller.ctx).await?;
        scopes.retain(|summary| caller.may_read(&summary.scope));
        scopes.sort_by_key(|summary| std::cmp::Reverse(summary.last_recorded_at));
        let truncated = scopes.len() > MAX_SCOPE_SUMMARIES;
        scopes.truncate(MAX_SCOPE_SUMMARIES);
        if !caller.key.is_admin {
            for summary in &mut scopes {
                summary.quarantined = 0;
            }
        }
        Ok((scopes, truncated))
    }

    pub async fn list_facts(
        &self,
        caller: &Caller,
        scope: &Scope,
        state: Option<FactListState>,
        limit: Option<i64>,
        before: Option<Uuid>,
    ) -> ApiResult<Vec<FactView>> {
        caller.require_scope(scope)?;
        let limit = limit.unwrap_or(DEFAULT_FACT_PAGE);
        if !(1..=MAX_FACT_PAGE).contains(&limit) {
            return Err(ApiError::invalid_input(format!(
                "limit must be in 1..={MAX_FACT_PAGE}, got {limit}"
            )));
        }
        let state = state.unwrap_or(FactListState::Current);
        Ok(self
            .store
            .list_facts(&caller.ctx, scope, state, limit, before)
            .await?)
    }

    pub async fn fact_history(&self, caller: &Caller, id: Uuid) -> ApiResult<Vec<Fact>> {
        self.get_fact(caller, id).await?;
        Ok(self.store.fact_history(&caller.ctx, id).await?)
    }

    pub async fn invalidate_fact(
        &self,
        caller: &Caller,
        id: Uuid,
        reason: &str,
    ) -> ApiResult<Fact> {
        let fact = self.get_fact(caller, id).await?;
        require_trust(caller, "fact", fact.trust)?;
        // A key that cannot add here without review must not delete here by retraction either.
        if caller.key.max_trust < Trust::User && !matches!(fact.scope, Scope::Agent(_)) {
            return Err(ApiError::forbidden(
                "invalidating a fact in a user or shared scope requires user trust",
            ));
        }
        Ok(self.store.invalidate_fact(&caller.ctx, id, reason).await?)
    }

    // Graph

    pub async fn get_entity(&self, caller: &Caller, id: Uuid) -> ApiResult<Entity> {
        let entity = self.store.get_entity(&caller.ctx, id).await?;
        caller.require_visible(&entity.scope)?;
        Ok(entity)
    }

    pub async fn neighbors(
        &self,
        caller: &Caller,
        id: Uuid,
        scopes: &[Scope],
        depth: Option<i32>,
        as_of: Option<DateTime<Utc>>,
    ) -> ApiResult<GraphNeighborhood> {
        caller.require_scopes(scopes)?;
        self.get_entity(caller, id).await?;
        let filter = FactFilter {
            as_of,
            known_at: None,
        };
        Ok(self
            .store
            .neighbors(
                &caller.ctx,
                scopes,
                id,
                depth.unwrap_or(DEFAULT_DEPTH),
                &filter,
                NEIGHBORS_EDGE_LIMIT,
            )
            .await?)
    }

    // Quarantine

    pub async fn list_quarantine(
        &self,
        caller: &Caller,
        scopes: &[String],
        limit: Option<i64>,
    ) -> ApiResult<Vec<Fact>> {
        caller.require_admin()?;
        let selection = caller.select_scopes(scopes)?;
        let limit = limit.unwrap_or(DEFAULT_QUARANTINE_PAGE);
        if !(1..=MAX_QUARANTINE_PAGE).contains(&limit) {
            return Err(ApiError::invalid_input(format!(
                "limit must be in 1..={MAX_QUARANTINE_PAGE}, got {limit}"
            )));
        }
        let mut facts = self
            .store
            .list_quarantined_matching(&caller.ctx, &selection.exact, &selection.prefixes, limit)
            .await?;
        // `select_scopes` already narrowed the query. This holds even if that logic is ever wrong.
        facts.retain(|fact| caller.may_read(&fact.scope));
        Ok(facts)
    }

    /// Approving closes the fact it is pending on, so the key must reach that fact's trust too.
    /// When the chain moved to a higher-trust head, the store holds the fact and its `InvalidInput`
    /// message tells the caller to review again, and that review is checked here against the new
    /// head (SPEC section 5).
    pub async fn approve(&self, caller: &Caller, id: Uuid) -> ApiResult<Fact> {
        caller.require_admin()?;
        let fact = self.get_fact(caller, id).await?;
        require_trust(caller, "fact", fact.trust)?;
        if let Some(target) = fact.pending_supersede_of {
            let target = self.store.get_fact(&caller.ctx, target).await?;
            require_trust(caller, "superseded fact", target.trust)?;
        }
        Ok(self.store.approve_fact(&caller.ctx, id).await?)
    }

    pub async fn reject(&self, caller: &Caller, id: Uuid) -> ApiResult<Fact> {
        caller.require_admin()?;
        let fact = self.get_fact(caller, id).await?;
        require_trust(caller, "fact", fact.trust)?;
        Ok(self.store.reject_fact(&caller.ctx, id).await?)
    }

    // Blocks

    pub async fn list_blocks(&self, caller: &Caller, agent_id: &str) -> ApiResult<Vec<Block>> {
        require_agent(caller, agent_id)?;
        Ok(self.store.list_blocks(&caller.ctx, agent_id).await?)
    }

    pub async fn get_block(
        &self,
        caller: &Caller,
        agent_id: &str,
        label: &str,
    ) -> ApiResult<Block> {
        require_agent(caller, agent_id)?;
        Ok(self.store.get_block(&caller.ctx, agent_id, label).await?)
    }

    pub async fn put_block(
        &self,
        caller: &Caller,
        agent_id: &str,
        label: &str,
        req: PutBlockRequest,
    ) -> ApiResult<Block> {
        require_agent(caller, agent_id)?;
        if caller.key.max_trust < Trust::Agent {
            return Err(ApiError::forbidden(
                "writing a block requires at least agent trust",
            ));
        }
        if req.max_chars.is_some_and(|chars| chars > MAX_BLOCK_CHARS) {
            return Err(ApiError::invalid_input(format!(
                "max_chars must be at most {MAX_BLOCK_CHARS}"
            )));
        }
        let max_chars = match (req.max_chars, req.expected_version) {
            (Some(max_chars), _) => max_chars,
            (None, None) => DEFAULT_MAX_BLOCK_CHARS,
            // An edit that does not mention the limit must not reset it to the default.
            (None, Some(_)) => {
                self.store
                    .get_block(&caller.ctx, agent_id, label)
                    .await?
                    .max_chars
            }
        };
        Ok(self
            .store
            .put_block(
                &caller.ctx,
                agent_id,
                label,
                &req.content,
                max_chars,
                req.expected_version,
            )
            .await?)
    }

    pub async fn block_versions(
        &self,
        caller: &Caller,
        agent_id: &str,
        label: &str,
    ) -> ApiResult<Vec<BlockVersion>> {
        require_agent(caller, agent_id)?;
        Ok(self
            .store
            .block_versions(&caller.ctx, agent_id, label)
            .await?)
    }

    // Pages (SPEC sections 6 and 8)

    pub async fn list_pages(
        &self,
        caller: &Caller,
        scope: &Scope,
        prefix: Option<&str>,
        limit: Option<i64>,
    ) -> ApiResult<Vec<PageSummary>> {
        caller.require_scope(scope)?;
        let limit = limit.unwrap_or(DEFAULT_PAGE_LIST);
        if !(1..=MAX_PAGE_LIST).contains(&limit) {
            return Err(ApiError::invalid_input(format!(
                "limit must be in 1..={MAX_PAGE_LIST}, got {limit}"
            )));
        }
        Ok(self
            .store
            .list_pages(&caller.ctx, scope, prefix, limit)
            .await?)
    }

    pub async fn get_page(
        &self,
        caller: &Caller,
        scope: &Scope,
        slug: &str,
        version: Option<i64>,
    ) -> ApiResult<Page> {
        caller.require_scope(scope)?;
        validate_slug(slug)?;
        if let Some(version) = version.filter(|version| *version < 1) {
            return Err(ApiError::invalid_input(format!(
                "version must be at least 1, got {version}"
            )));
        }
        Ok(self
            .store
            .get_page(&caller.ctx, scope, slug, version)
            .await?)
    }

    /// Write trust is the commit trust of SPEC section 3, computed as `HostCommitter::commit` does:
    /// `agent`, or `external` for an `external` key. `approve` makes an admin write active up to the
    /// key's `max_trust`.
    pub async fn write_page(
        &self,
        caller: &Caller,
        scope: &Scope,
        slug: &str,
        req: PageWriteRequest,
    ) -> ApiResult<PageWriteResponse> {
        caller.require_scope(scope)?;
        if caller.key.max_trust < Trust::Agent {
            return Err(ApiError::forbidden(
                "writing a page requires at least agent trust",
            ));
        }
        if req.approve && !caller.key.is_admin {
            return Err(ApiError::forbidden(
                "approve on a page write requires an admin key",
            ));
        }
        let write = PageWrite {
            scope: scope.clone(),
            slug: slug.to_owned(),
            title: req.title,
            body: req.body,
            summary: req.summary,
            trust: caller.key.max_trust.min(Trust::Agent),
            expected_version: req.expected_version,
            approve_up_to: req.approve.then_some(caller.key.max_trust),
        };
        let outcome = self.pages.write(&caller.ctx, write).await?;
        Ok(PageWriteResponse {
            version: outcome.version,
            redacted: outcome.redacted,
        })
    }

    pub async fn page_versions(
        &self,
        caller: &Caller,
        scope: &Scope,
        slug: &str,
    ) -> ApiResult<Vec<PageVersion>> {
        caller.require_scope(scope)?;
        validate_slug(slug)?;
        Ok(self.store.page_versions(&caller.ctx, scope, slug).await?)
    }

    /// As with invalidation, a key that cannot add to a user or shared scope without review must
    /// not delete from it either.
    pub async fn delete_page(&self, caller: &Caller, scope: &Scope, slug: &str) -> ApiResult<()> {
        caller.require_scope(scope)?;
        validate_slug(slug)?;
        let (needed, refusal) = match scope {
            Scope::Agent(_) => (Trust::Agent, "deleting an agent page requires agent trust"),
            Scope::User(_) | Scope::Shared(_) => (
                Trust::User,
                "deleting a page in a user or shared scope requires user trust",
            ),
        };
        if caller.key.max_trust < needed {
            return Err(ApiError::forbidden(refusal));
        }
        Ok(self.store.delete_page(&caller.ctx, scope, slug).await?)
    }

    /// Scopes and patterns follow `GET /v1/quarantine`. The store takes exact scopes only, so a
    /// pattern stands for the tenant's page scopes that the pattern covers.
    pub async fn list_page_quarantine(
        &self,
        caller: &Caller,
        scopes: &[String],
        limit: Option<i64>,
    ) -> ApiResult<Vec<PendingPageVersion>> {
        caller.require_admin()?;
        let selection = caller.select_scopes(scopes)?;
        let limit = limit.unwrap_or(DEFAULT_QUARANTINE_PAGE);
        if !(1..=MAX_QUARANTINE_PAGE).contains(&limit) {
            return Err(ApiError::invalid_input(format!(
                "limit must be in 1..={MAX_QUARANTINE_PAGE}, got {limit}"
            )));
        }
        let mut exact = selection.exact;
        if !selection.prefixes.is_empty() {
            // Every scope holding a page, so a scope whose only page waits for its first review
            // is still covered.
            for scope in self.store.page_scopes(&caller.ctx).await? {
                let name = scope.to_string();
                let covered = selection
                    .prefixes
                    .iter()
                    .any(|prefix| name.starts_with(prefix.as_str()));
                if covered && !exact.contains(&scope) {
                    exact.push(scope);
                }
            }
        }
        // `select_scopes` already narrowed the list. This holds even if that logic is ever wrong.
        exact.retain(|scope| caller.may_read(scope));
        let mut versions = self
            .store
            .list_quarantined_pages(&caller.ctx, &exact, limit)
            .await?;
        versions.retain(|pending| caller.may_read(&pending.version.scope));
        Ok(versions)
    }

    /// A version in a scope the key cannot read is 404, as a fact loaded by id is.
    async fn visible_page_version(
        &self,
        caller: &Caller,
        page_id: Uuid,
        version: i64,
    ) -> ApiResult<PageVersion> {
        let found = self
            .store
            .get_page_version(&caller.ctx, page_id, version)
            .await?;
        caller.require_visible(&found.scope)?;
        Ok(found)
    }

    /// Approving replaces the current version, so the key must reach its trust as well.
    pub async fn approve_page(
        &self,
        caller: &Caller,
        page_id: Uuid,
        version: i64,
    ) -> ApiResult<Page> {
        caller.require_admin()?;
        let pending = self.visible_page_version(caller, page_id, version).await?;
        require_trust(caller, "page version", pending.trust)?;
        match self
            .store
            .get_page(&caller.ctx, &pending.scope, &pending.slug, None)
            .await
        {
            Ok(current) => require_trust(caller, "current page version", current.trust)?,
            Err(Error::NotFound(_)) => {}
            Err(error) => return Err(error.into()),
        }
        Ok(self
            .store
            .approve_page_version(&caller.ctx, page_id, version)
            .await?)
    }

    pub async fn reject_page(
        &self,
        caller: &Caller,
        page_id: Uuid,
        version: i64,
    ) -> ApiResult<PageVersion> {
        caller.require_admin()?;
        let pending = self.visible_page_version(caller, page_id, version).await?;
        require_trust(caller, "page version", pending.trust)?;
        Ok(self
            .store
            .reject_page_version(&caller.ctx, page_id, version)
            .await?)
    }

    // Insights (SPEC sections 6 and 8, Insights)

    /// The writer's trust of SPEC section 8: `agent`, or `external` for an `external` key, the
    /// commit trust of section 3. The store lowers it further to the weakest source.
    fn insight_writer_trust(caller: &Caller) -> Trust {
        caller.key.max_trust.min(Trust::Agent)
    }

    /// Masks secrets as commits do, embeds the statement, then lets the store check the sources
    /// and decide trust and status atomically.
    pub async fn write_insight(
        &self,
        caller: &Caller,
        req: InsightWriteRequest,
    ) -> ApiResult<InsightWriteResponse> {
        caller.require_scope(&req.scope)?;
        let mut statement = req.statement;
        let redacted = redact(&mut statement);
        // Checked after masking, so a refused statement costs no embedder call. The store checks
        // it again with the sources.
        validate_statement(&statement)?;
        let embedding = self.embed_statement(&statement).await?;
        let insight = self
            .store
            .put_insight(
                &caller.ctx,
                NewInsight {
                    scope: req.scope,
                    level: req.level,
                    statement,
                    embedding,
                    trust: Self::insight_writer_trust(caller),
                    sources: req.sources,
                    supersedes: req.supersedes,
                    written_by: caller.ctx.actor.clone(),
                },
            )
            .await?;
        Ok(InsightWriteResponse { insight, redacted })
    }

    async fn embed_statement(&self, statement: &str) -> ApiResult<Vec<f32>> {
        let mut vectors = self
            .embedder
            .embed(&[statement.to_owned()])
            .await
            .map_err(|error| match error {
                Error::Provider(message) => Error::Provider(message),
                other => Error::Provider(other.to_string()),
            })?;
        match (vectors.pop(), vectors.is_empty()) {
            (Some(vector), true) => Ok(vector),
            _ => Err(Error::Provider("the embedder did not return one vector".to_owned()).into()),
        }
    }

    /// An insight in a scope the key cannot read is 404, as a fact loaded by id is.
    pub async fn get_insight(&self, caller: &Caller, id: Uuid) -> ApiResult<InsightDetail> {
        let detail = self.store.get_insight(&caller.ctx, id).await?;
        caller.require_visible(&detail.insight.scope)?;
        Ok(detail)
    }

    pub async fn list_insights(
        &self,
        caller: &Caller,
        scopes: &[Scope],
        filter: &InsightFilter,
        limit: Option<i64>,
    ) -> ApiResult<Vec<Insight>> {
        caller.require_scopes(scopes)?;
        let limit = limit.unwrap_or(DEFAULT_INSIGHT_LIST);
        if !(1..=MAX_INSIGHT_LIST).contains(&limit) {
            return Err(ApiError::invalid_input(format!(
                "limit must be in 1..={MAX_INSIGHT_LIST}, got {limit}"
            )));
        }
        Ok(self
            .store
            .list_insights(&caller.ctx, scopes, filter, limit)
            .await?)
    }

    /// Confirming is a write: the store checks the new sources and recomputes trust from the same
    /// writer trust as `write_insight`.
    pub async fn confirm_insight(
        &self,
        caller: &Caller,
        id: Uuid,
        sources: &[InsightSourceRef],
    ) -> ApiResult<Insight> {
        self.get_insight(caller, id).await?;
        Ok(self
            .store
            .confirm_insight(&caller.ctx, id, sources, Self::insight_writer_trust(caller))
            .await?)
    }

    /// Retiring withdraws an insight with no review, so it follows fact invalidation: the key must
    /// reach the insight's trust, and in a user or shared scope it needs user trust. An agent there
    /// replaces an insight with a superseding write instead, which waits for review.
    pub async fn retire_insight(
        &self,
        caller: &Caller,
        id: Uuid,
        reason: &str,
    ) -> ApiResult<Insight> {
        if reason.trim().is_empty()
            || reason.chars().count() > MAX_RETIRE_REASON
            || reason.contains('\0')
        {
            return Err(ApiError::invalid_input(format!(
                "reason must be 1 to {MAX_RETIRE_REASON} characters, with no NUL"
            )));
        }
        let detail = self.get_insight(caller, id).await?;
        require_trust(caller, "insight", detail.insight.trust)?;
        if caller.key.max_trust < Trust::User && !matches!(detail.insight.scope, Scope::Agent(_)) {
            return Err(ApiError::forbidden(
                "retiring an insight in a user or shared scope requires user trust",
            ));
        }
        Ok(self.store.retire_insight(&caller.ctx, id, reason).await?)
    }

    /// Scopes and patterns follow `GET /v1/quarantine`. The store takes exact scopes only, so a
    /// pattern stands for the tenant's scopes holding an insight that the pattern covers.
    pub async fn list_insight_quarantine(
        &self,
        caller: &Caller,
        scopes: &[String],
        limit: Option<i64>,
    ) -> ApiResult<Vec<Insight>> {
        caller.require_admin()?;
        let selection = caller.select_scopes(scopes)?;
        let limit = limit.unwrap_or(DEFAULT_QUARANTINE_PAGE);
        if !(1..=MAX_QUARANTINE_PAGE).contains(&limit) {
            return Err(ApiError::invalid_input(format!(
                "limit must be in 1..={MAX_QUARANTINE_PAGE}, got {limit}"
            )));
        }
        let mut exact = selection.exact;
        if !selection.prefixes.is_empty() {
            // Scope summaries list every scope holding an insight, in any status.
            for summary in self.store.scope_summaries(&caller.ctx).await? {
                let name = summary.scope.to_string();
                let covered = selection
                    .prefixes
                    .iter()
                    .any(|prefix| name.starts_with(prefix.as_str()));
                if covered && !exact.contains(&summary.scope) {
                    exact.push(summary.scope);
                }
            }
        }
        // `select_scopes` already narrowed the list. This holds even if that logic is ever wrong.
        exact.retain(|scope| caller.may_read(scope));
        if exact.is_empty() {
            return Ok(Vec::new());
        }
        let mut insights = self
            .store
            .list_quarantined_insights(&caller.ctx, &exact, limit)
            .await?;
        insights.retain(|insight| caller.may_read(&insight.scope));
        Ok(insights)
    }

    /// As for facts: approving closes the insight it supersedes, so the key must reach that
    /// insight's trust too.
    pub async fn approve_insight(&self, caller: &Caller, id: Uuid) -> ApiResult<Insight> {
        caller.require_admin()?;
        let detail = self.get_insight(caller, id).await?;
        require_trust(caller, "insight", detail.insight.trust)?;
        if let Some(target) = detail.insight.pending_supersede_of {
            let target = self.store.get_insight(&caller.ctx, target).await?;
            require_trust(caller, "superseded insight", target.insight.trust)?;
        }
        Ok(self.store.approve_insight(&caller.ctx, id).await?)
    }

    pub async fn reject_insight(&self, caller: &Caller, id: Uuid) -> ApiResult<Insight> {
        caller.require_admin()?;
        let detail = self.get_insight(caller, id).await?;
        require_trust(caller, "insight", detail.insight.trust)?;
        Ok(self.store.reject_insight(&caller.ctx, id).await?)
    }

    pub async fn reflect_batch(
        &self,
        caller: &Caller,
        req: ReflectBatchRequest,
    ) -> ApiResult<ReflectBatch> {
        caller.require_scope(&req.scope)?;
        let limit = req.limit.unwrap_or(REFLECT_BATCH_DEFAULT);
        if !(1..=REFLECT_BATCH_MAX).contains(&limit) {
            return Err(ApiError::invalid_input(format!(
                "limit must be in 1..={REFLECT_BATCH_MAX}, got {limit}"
            )));
        }
        Ok(self
            .store
            .reflect_batch(&caller.ctx, &req.scope, limit, self.reflect_min_facts)
            .await?)
    }

    pub async fn reflect_decline(
        &self,
        caller: &Caller,
        req: ReflectDeclineRequest,
    ) -> ApiResult<()> {
        caller.require_scope(&req.scope)?;
        let hex = req.fact_set.len() == 64
            && req
                .fact_set
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if !hex {
            return Err(ApiError::invalid_input(
                "fact_set must be 64 lowercase hex characters, as a reflect batch returns it",
            ));
        }
        Ok(self
            .store
            .decline_topic(&caller.ctx, &req.scope, req.topic_key, &req.fact_set)
            .await?)
    }

    // Users

    pub async fn delete_user_memory(
        &self,
        caller: &Caller,
        user_id: &str,
    ) -> ApiResult<PurgeReport> {
        caller.require_admin()?;
        let scope = parse_scope("user", user_id)?;
        caller.require_scope(&scope)?;
        if let Some(highest) = self
            .store
            .highest_trust_in_scope(&caller.ctx, &scope)
            .await?
        {
            require_trust(caller, "stored memory", highest)?;
        }
        Ok(self.store.purge_scope(&caller.ctx, &scope).await?)
    }
}

fn require_trust(caller: &Caller, what: &str, trust: Trust) -> ApiResult<()> {
    if trust > caller.key.max_trust {
        return Err(ApiError::forbidden(format!(
            "{what} trust {} is above this key's maximum of {}",
            trust.as_str(),
            caller.key.max_trust.as_str()
        )));
    }
    Ok(())
}

/// SPEC section 8: 1 to `INSIGHT_STATEMENT_MAX` characters after masking. NUL is refused, since
/// PostgreSQL cannot store it.
fn validate_statement(statement: &str) -> ApiResult<()> {
    if statement.trim().is_empty() || statement.chars().count() > INSIGHT_STATEMENT_MAX {
        return Err(ApiError::invalid_input(format!(
            "statement must be 1 to {INSIGHT_STATEMENT_MAX} characters"
        )));
    }
    if statement.contains('\0') {
        return Err(ApiError::invalid_input("statement must not contain NUL"));
    }
    Ok(())
}

fn validate_slug(slug: &str) -> ApiResult<()> {
    Ok(memoryos_core::validate_slug(slug)?)
}

fn parse_scope(kind: &str, id: &str) -> ApiResult<Scope> {
    Ok(format!("{kind}:{id}").parse::<Scope>()?)
}

fn require_agent(caller: &Caller, agent_id: &str) -> ApiResult<()> {
    caller.require_scope(&parse_scope("agent", agent_id)?)
}
