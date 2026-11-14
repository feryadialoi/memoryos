use async_trait::async_trait;
use uuid::Uuid;

use crate::error::Result;
use crate::types::*;

/// Persistence contract. Frozen after Wave 0: changes need an entry in docs/PROGRESS.md.
///
/// Every `ctx` method must run in a transaction with tenant isolation applied for `ctx.tenant`
/// (see docs/SPEC.md section 6). Reads never return another tenant's rows. Backends use row-level
/// security where available and explicit tenant predicates otherwise.
#[async_trait]
pub trait MemoryStore: Send + Sync {
    // Episodes
    /// Stores the source of a commit. A repeated `idempotency_key` returns the existing episode.
    /// Whether this call created the row is the second value. `false` means a repeated
    /// `idempotency_key` returned an episode some other request owns.
    async fn claim_episode(
        &self,
        ctx: &RequestContext,
        episode: NewEpisode,
    ) -> Result<(Episode, bool)>;
    /// Deletes the episode only if it is still unprocessed, atomically against `apply_commit`,
    /// and returns whether it did. An unprocessed episode has no facts, so nothing else changes.
    async fn discard_unprocessed_episode(&self, ctx: &RequestContext, id: Uuid) -> Result<bool>;
    async fn get_episode(&self, ctx: &RequestContext, id: Uuid) -> Result<Episode>;
    /// Deletes the episode. Facts it alone supports are deleted; facts with other sources lose
    /// this source and stay, with trust recomputed as the highest among the remaining sources. A
    /// surviving active fact whose new trust falls under a source quarantine rule of SPEC section 5
    /// becomes quarantined. Entities left with no facts are deleted.
    async fn purge_episode(&self, ctx: &RequestContext, id: Uuid) -> Result<PurgeReport>;
    /// Hard-deletes everything stored under `scope`, including its pages, and blocks when it is an
    /// agent scope.
    async fn purge_scope(&self, ctx: &RequestContext, scope: &Scope) -> Result<PurgeReport>;

    // Commits
    /// Atomically applies `ops` and sets `episodes.processed_at`. If the episode is already
    /// processed this is a no-op returning an empty list, which makes a replayed commit idempotent.
    /// Returns ids of inserted facts.
    async fn apply_commit(
        &self,
        ctx: &RequestContext,
        episode_id: Uuid,
        ops: Vec<FactOp>,
    ) -> Result<Vec<Uuid>>;

    // Entities
    /// Returns the entity with the same scope, kind and normalized name, or creates it.
    async fn upsert_entity(&self, ctx: &RequestContext, entity: NewEntity) -> Result<Entity>;
    async fn get_entity(&self, ctx: &RequestContext, id: Uuid) -> Result<Entity>;
    /// Nearest entities by cosine similarity, highest first, with similarity in 0..=1.
    async fn similar_entities(
        &self,
        ctx: &RequestContext,
        scope: &Scope,
        embedding: &[f32],
        limit: i64,
    ) -> Result<Vec<(Entity, f64)>>;
    /// `source_episode` records who earned the alias: purging the last such episode removes it.
    async fn add_entity_alias(
        &self,
        ctx: &RequestContext,
        id: Uuid,
        alias: &str,
        source_episode: Option<Uuid>,
    ) -> Result<()>;
    /// The entity in `scope` with exactly this `kind` whose name or a known alias equals `name`
    /// under `normalize_name`. A name match wins over an alias match.
    async fn find_entity(
        &self,
        ctx: &RequestContext,
        scope: &Scope,
        kind: &str,
        name: &str,
    ) -> Result<Option<Entity>>;

    // Facts
    async fn get_fact(&self, ctx: &RequestContext, id: Uuid) -> Result<Fact>;
    /// Every fact linked to the episode as a source, in any status, oldest first. That covers facts
    /// it created and facts it corroborated. Facts it created keep their creation order.
    async fn facts_for_episode(&self, ctx: &RequestContext, episode_id: Uuid) -> Result<Vec<Fact>>;
    /// All versions in the supersession chain containing `id`, oldest first.
    async fn fact_history(&self, ctx: &RequestContext, id: Uuid) -> Result<Vec<Fact>>;
    /// Marks the fact retracted by setting `invalidated_at`. Writes an audit event.
    async fn invalidate_fact(&self, ctx: &RequestContext, id: Uuid, reason: &str) -> Result<Fact>;
    /// The fact if it is current in `scope`: active and visible now, as section 4 defines it.
    async fn current_fact(
        &self,
        ctx: &RequestContext,
        scope: &Scope,
        id: Uuid,
    ) -> Result<Option<Fact>>;
    /// Currently valid, active facts about `subject_id`, oldest first. At most the 200 that started
    /// latest.
    async fn current_facts_for_subject(
        &self,
        ctx: &RequestContext,
        scope: &Scope,
        subject_id: Uuid,
    ) -> Result<Vec<Fact>>;
    /// Vector search over active facts passing `filter`. Must still return up to `limit`
    /// rows under selective filters.
    async fn search_facts_vector(
        &self,
        ctx: &RequestContext,
        scopes: &[Scope],
        embedding: &[f32],
        filter: &FactFilter,
        limit: i64,
    ) -> Result<Vec<ScoredFact>>;
    /// Full-text search over active facts passing `filter`.
    async fn search_facts_text(
        &self,
        ctx: &RequestContext,
        scopes: &[Scope],
        query: &str,
        filter: &FactFilter,
        limit: i64,
    ) -> Result<Vec<ScoredFact>>;
    // Quarantine
    async fn list_quarantined(
        &self,
        ctx: &RequestContext,
        scopes: &[Scope],
        limit: i64,
    ) -> Result<Vec<Fact>>;
    /// Activates the fact and closes the head of the chain it was pending on. If that chain has
    /// moved to a head the fact was not reviewed against, and that head has higher trust, nothing
    /// is activated or closed: the fact stays quarantined, `pending_supersede_of` moves to the new
    /// head, that change is committed, and this returns `InvalidInput` so it can be reviewed again.
    async fn approve_fact(&self, ctx: &RequestContext, id: Uuid) -> Result<Fact>;
    async fn reject_fact(&self, ctx: &RequestContext, id: Uuid) -> Result<Fact>;

    // Graph
    /// Entities and active edges within `depth` hops of `root` through `scopes`. `depth` is 1..=5.
    /// Returns at most `edge_limit` edges in total, nearer hops first and newest first within a
    /// hop. `edge_limit` must be positive, and values above the store's hard cap of 5000 use the cap.
    async fn neighbors(
        &self,
        ctx: &RequestContext,
        scopes: &[Scope],
        root: Uuid,
        depth: i32,
        filter: &FactFilter,
        edge_limit: i64,
    ) -> Result<GraphNeighborhood>;

    // Blocks
    async fn list_blocks(&self, ctx: &RequestContext, agent_id: &str) -> Result<Vec<Block>>;
    async fn get_block(&self, ctx: &RequestContext, agent_id: &str, label: &str) -> Result<Block>;
    /// `expected_version = None` creates the block and fails with `VersionConflict` if it
    /// exists. `Some(v)` updates only when the current version is `v`, and appends a version row.
    async fn put_block(
        &self,
        ctx: &RequestContext,
        agent_id: &str,
        label: &str,
        content: &str,
        max_chars: i32,
        expected_version: Option<i64>,
    ) -> Result<Block>;
    /// The latest 100 versions, oldest first. Writes prune anything older.
    async fn block_versions(
        &self,
        ctx: &RequestContext,
        agent_id: &str,
        label: &str,
    ) -> Result<Vec<BlockVersion>>;

    // Pages (docs/SPEC.md section 8).
    /// Pages in `scope` that have a current version, by slug, at most `limit` (1..=500), optionally
    /// only slugs starting with `prefix`.
    async fn list_pages(
        &self,
        ctx: &RequestContext,
        scope: &Scope,
        prefix: Option<&str>,
        limit: i64,
    ) -> Result<Vec<PageSummary>>;
    /// The current version, or with `version` that version if it is active or superseded. A page
    /// with no current version, or a quarantined or rejected version, is `NotFound`.
    async fn get_page(
        &self,
        ctx: &RequestContext,
        scope: &Scope,
        slug: &str,
        version: Option<i64>,
    ) -> Result<Page>;
    /// One version's metadata in any status, for scope and trust checks before review.
    async fn get_page_version(
        &self,
        ctx: &RequestContext,
        page_id: Uuid,
        version: i64,
    ) -> Result<PageVersion>;
    /// Writes a new version atomically:
    /// - `expected_version = None` creates the page and fails with `VersionConflict` if it exists.
    ///   `Some(v)` fails with `NotFound` if the page does not exist, and with `VersionConflict`
    ///   unless `v` is the current version. A page with no current version reports current 0.
    /// - With `approve_up_to = Some(t)`, the version is active, unless the current version's trust
    ///   is above `t`, which is `Forbidden`. Otherwise it is quarantined when
    ///   `source_requires_quarantine(scope, trust)` holds or the current version's trust is above
    ///   `trust`, and active in every other case.
    /// - An active version becomes current: the previous current one becomes superseded and its
    ///   sections are deleted. A quarantined one keeps its sections for approval.
    /// - `InvalidInput` when the page already has `PAGE_PENDING_MAX` quarantined versions.
    /// - Prunes superseded and rejected versions past the newest `PAGE_VERSIONS_KEPT`, and writes a
    ///   `page.write` audit event, with `approved: true` when `approve_up_to` was used.
    async fn put_page(&self, ctx: &RequestContext, page: NewPageVersion) -> Result<PageVersion>;
    /// Kept versions of the page in any status, newest first, without bodies.
    async fn page_versions(
        &self,
        ctx: &RequestContext,
        scope: &Scope,
        slug: &str,
    ) -> Result<Vec<PageVersion>>;
    /// Hard-deletes the page with every version and section. Writes a `page.delete` audit event.
    async fn delete_page(&self, ctx: &RequestContext, scope: &Scope, slug: &str) -> Result<()>;
    /// Quarantined versions in `scopes`, oldest first.
    async fn list_quarantined_pages(
        &self,
        ctx: &RequestContext,
        scopes: &[Scope],
        limit: i64,
    ) -> Result<Vec<PendingPageVersion>>;
    /// Makes a quarantined version current, as an active write would. `InvalidInput` if it is not
    /// quarantined, `VersionConflict` if its `base_version` is no longer the current version.
    /// Writes a `page.approve` audit event.
    async fn approve_page_version(
        &self,
        ctx: &RequestContext,
        page_id: Uuid,
        version: i64,
    ) -> Result<Page>;
    /// Rejects a quarantined version and deletes its sections. Writes a `page.reject` audit event.
    async fn reject_page_version(
        &self,
        ctx: &RequestContext,
        page_id: Uuid,
        version: i64,
    ) -> Result<PageVersion>;
    /// Vector search over sections of current versions in `scopes`, nearest first.
    async fn search_page_sections_vector(
        &self,
        ctx: &RequestContext,
        scopes: &[Scope],
        embedding: &[f32],
        limit: i64,
    ) -> Result<Vec<PageSectionHit>>;
    /// Full-text search over headings and text of sections of current versions in `scopes`.
    async fn search_page_sections_text(
        &self,
        ctx: &RequestContext,
        scopes: &[Scope],
        query: &str,
        limit: i64,
    ) -> Result<Vec<PageSectionHit>>;

    // API keys: cross-tenant, looked up before the tenant is known
    /// `key_hash` is the hex SHA-256 of the presented token. Revoked keys return `None`.
    async fn find_api_key(&self, key_hash: &str) -> Result<Option<ApiKey>>;
    async fn create_api_key(
        &self,
        tenant: &str,
        name: &str,
        key_hash: &str,
        scopes: &[String],
        max_trust: Trust,
        is_admin: bool,
    ) -> Result<ApiKey>;
    async fn revoke_api_key(&self, id: Uuid) -> Result<()>;

    /// Verifies database connectivity for `/readyz`.
    async fn ping(&self) -> Result<()>;
}
