use std::fmt;
use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::time::Duration;

use chrono::{DateTime, Utc};
use memoryos_core::{
    Block, BlockVersion, Entity, Episode, Fact, GraphNeighborhood, Insight, InsightDetail,
    InsightLevel, InsightSourceRef, InsightStatus, Page, PageSummary, PageVersion,
    PendingPageVersion, PurgeReport, ReflectBatch, Scope, ScoredFact,
};
use reqwest::header::RETRY_AFTER;
use reqwest::{Client, Method, RequestBuilder, Response, StatusCode, Url};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::error::{from_response, Error, Result};
use crate::types::{
    Commit, CommitResponse, InsightWrite, InsightWriteResponse, PageWrite, PageWriteResponse,
    PutBlock, RecallResponse,
};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_READ_RETRIES: u32 = 2;
const RETRY_BASE: Duration = Duration::from_millis(200);
const MAX_RETRY_WAIT: Duration = Duration::from_secs(10);
const DEFAULT_RECALL_LIMIT: i64 = 10;

#[derive(Debug, Clone)]
pub struct Options {
    /// Limit for one attempt of any call.
    pub timeout: Duration,
    /// Extra attempts for reads. Writes never retry.
    pub read_retries: u32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            read_retries: DEFAULT_READ_RETRIES,
        }
    }
}

/// Cheap to clone: clones share a connection pool.
#[derive(Clone)]
pub struct MemoryOs {
    http: Client,
    base_url: Url,
    api_key: String,
    options: Options,
}

impl fmt::Debug for MemoryOs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryOs")
            .field("base_url", &self.base_url.as_str())
            .field("api_key", &"[REDACTED]")
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Retry {
    Yes,
    No,
}

impl MemoryOs {
    /// Checks that the server answers. It does not check the key, which any later call will.
    pub async fn connect(base_url: impl AsRef<str>, api_key: impl Into<String>) -> Result<Self> {
        Self::connect_with(base_url, api_key, Options::default()).await
    }

    pub async fn connect_with(
        base_url: impl AsRef<str>,
        api_key: impl Into<String>,
        options: Options,
    ) -> Result<Self> {
        let client = Self::new(base_url, api_key, options)?;
        client.health().await?;
        Ok(client)
    }

    /// Does no I/O.
    pub fn new(
        base_url: impl AsRef<str>,
        api_key: impl Into<String>,
        options: Options,
    ) -> Result<Self> {
        let base_url =
            Url::parse(base_url.as_ref()).map_err(|error| Error::InvalidUrl(error.to_string()))?;
        if base_url.cannot_be_a_base() {
            return Err(Error::InvalidUrl("not a base URL".to_owned()));
        }
        let http = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(Error::Transport)?;
        Ok(Self {
            http,
            base_url,
            api_key: api_key.into(),
            options,
        })
    }

    // ---- Service ------------------------------------------------------------------------------

    pub async fn health(&self) -> Result<()> {
        let request = self.request(Method::GET, &["healthz"]);
        self.send_empty(request, Retry::Yes).await
    }

    /// `Ok` when the server is ready for traffic. A 503 is an error carrying the status.
    pub async fn ready(&self) -> Result<()> {
        let request = self.request(Method::GET, &["readyz"]);
        let response = request.send().await?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        Err(Error::UnexpectedResponse {
            status: status.as_u16(),
            detail: "the server is not ready".to_owned(),
        })
    }

    /// The Prometheus text of `/metrics`.
    pub async fn metrics(&self) -> Result<String> {
        let request = self.request(Method::GET, &["metrics"]);
        let response = self.execute(request, Retry::Yes).await?;
        Ok(response.text().await?)
    }

    // ---- Episodes -----------------------------------------------------------------------------

    pub async fn get_episode(&self, id: Uuid) -> Result<Episode> {
        let request = self.request(Method::GET, &["v1", "episodes", &id.to_string()]);
        self.send(request, Retry::Yes).await
    }

    /// Admin only.
    pub async fn purge_episode(&self, id: Uuid) -> Result<PurgeReport> {
        let request = self.request(Method::POST, &["v1", "episodes", &id.to_string(), "purge"]);
        self.send(request, Retry::No).await
    }

    // ---- Search and commit --------------------------------------------------------------------

    /// Await it directly, or refine first: `.as_of(t)`, `.known_at(t)`, `.limit(n)`, `.scope(s)`.
    pub fn recall(&self, scope: Scope, query: impl Into<String>) -> RecallBuilder<'_> {
        RecallBuilder {
            client: self,
            body: RecallBody {
                scopes: vec![scope],
                query: query.into(),
                limit: DEFAULT_RECALL_LIMIT,
                as_of: None,
                known_at: None,
                page_limit: None,
                insight_limit: None,
            },
        }
    }

    /// Current facts related to a statement you may commit.
    pub async fn candidates(
        &self,
        scopes: &[Scope],
        statement: impl Into<String>,
        limit: Option<i64>,
    ) -> Result<Vec<ScoredFact>> {
        let mut body = json!({"scopes": scopes, "statement": statement.into()});
        if let Some(limit) = limit {
            body["limit"] = json!(limit);
        }
        let request = self
            .request(Method::POST, &["v1", "candidates"])
            .json(&body);
        let response: FactsScored = self.send(request, Retry::Yes).await?;
        Ok(response.facts)
    }

    pub async fn commit(&self, commit: &Commit) -> Result<CommitResponse> {
        let request = self.request(Method::POST, &["v1", "commits"]).json(commit);
        self.send(request, Retry::No).await
    }

    // ---- Facts --------------------------------------------------------------------------------

    pub async fn get_fact(&self, id: Uuid) -> Result<Fact> {
        let request = self.request(Method::GET, &["v1", "facts", &id.to_string()]);
        self.send(request, Retry::Yes).await
    }

    pub async fn fact_history(&self, id: Uuid) -> Result<Vec<Fact>> {
        let request = self.request(Method::GET, &["v1", "facts", &id.to_string(), "history"]);
        let response: Facts = self.send(request, Retry::Yes).await?;
        Ok(response.facts)
    }

    pub async fn invalidate_fact(&self, id: Uuid, reason: impl Into<String>) -> Result<Fact> {
        let request = self
            .request(
                Method::POST,
                &["v1", "facts", &id.to_string(), "invalidate"],
            )
            .json(&json!({"reason": reason.into()}));
        self.send(request, Retry::No).await
    }

    // ---- Graph --------------------------------------------------------------------------------

    pub async fn get_entity(&self, id: Uuid) -> Result<Entity> {
        let request = self.request(Method::GET, &["v1", "entities", &id.to_string()]);
        self.send(request, Retry::Yes).await
    }

    pub async fn neighbors(
        &self,
        id: Uuid,
        scopes: &[Scope],
        depth: Option<i32>,
        as_of: Option<DateTime<Utc>>,
    ) -> Result<GraphNeighborhood> {
        let mut query = vec![("scopes", join_scopes(scopes))];
        if let Some(depth) = depth {
            query.push(("depth", depth.to_string()));
        }
        if let Some(as_of) = as_of {
            query.push(("as_of", as_of.to_rfc3339()));
        }
        let request = self
            .request(
                Method::GET,
                &["v1", "entities", &id.to_string(), "neighbors"],
            )
            .query(&query);
        self.send(request, Retry::Yes).await
    }

    // ---- Quarantine: admin only ---------------------------------------------------------------

    pub async fn quarantine(&self, scopes: &[Scope], limit: Option<i64>) -> Result<Vec<Fact>> {
        let mut query = vec![("scopes", join_scopes(scopes))];
        if let Some(limit) = limit {
            query.push(("limit", limit.to_string()));
        }
        let request = self
            .request(Method::GET, &["v1", "quarantine"])
            .query(&query);
        let response: Facts = self.send(request, Retry::Yes).await?;
        Ok(response.facts)
    }

    /// `Error::InvalidInput` asking for another review means the chain moved to a higher-trust fact
    /// while this one waited. Look at that fact, then approve again.
    pub async fn approve(&self, id: Uuid) -> Result<Fact> {
        let request = self.request(
            Method::POST,
            &["v1", "quarantine", &id.to_string(), "approve"],
        );
        self.send(request, Retry::No).await
    }

    pub async fn reject(&self, id: Uuid) -> Result<Fact> {
        let request = self.request(
            Method::POST,
            &["v1", "quarantine", &id.to_string(), "reject"],
        );
        self.send(request, Retry::No).await
    }

    // ---- Blocks -------------------------------------------------------------------------------

    pub async fn list_blocks(&self, agent_id: &str) -> Result<Vec<Block>> {
        let request = self.request(Method::GET, &["v1", "agents", agent_id, "blocks"]);
        let response: Blocks = self.send(request, Retry::Yes).await?;
        Ok(response.blocks)
    }

    pub async fn get_block(&self, agent_id: &str, label: &str) -> Result<Block> {
        let request = self.request(Method::GET, &["v1", "agents", agent_id, "blocks", label]);
        self.send(request, Retry::Yes).await
    }

    pub async fn put_block(&self, agent_id: &str, label: &str, block: &PutBlock) -> Result<Block> {
        let request = self
            .request(Method::PUT, &["v1", "agents", agent_id, "blocks", label])
            .json(block);
        self.send(request, Retry::No).await
    }

    pub async fn block_versions(&self, agent_id: &str, label: &str) -> Result<Vec<BlockVersion>> {
        let request = self.request(
            Method::GET,
            &["v1", "agents", agent_id, "blocks", label, "versions"],
        );
        let response: Versions = self.send(request, Retry::Yes).await?;
        Ok(response.versions)
    }

    // ---- Pages --------------------------------------------------------------------------------

    /// Pages of `scope` that have a current version, by slug. `limit` is 1 to 500, default 100.
    pub async fn list_pages(
        &self,
        scope: &Scope,
        prefix: Option<&str>,
        limit: Option<i64>,
    ) -> Result<Vec<PageSummary>> {
        let mut query = vec![("scope", scope.to_string())];
        if let Some(prefix) = prefix {
            query.push(("prefix", prefix.to_owned()));
        }
        if let Some(limit) = limit {
            query.push(("limit", limit.to_string()));
        }
        let request = self.request(Method::GET, &["v1", "pages"]).query(&query);
        let response: Pages = self.send(request, Retry::Yes).await?;
        Ok(response.pages)
    }

    /// The current version, or with `version` an older active or superseded one.
    pub async fn get_page(&self, scope: &Scope, slug: &str, version: Option<i64>) -> Result<Page> {
        let mut request = self.request(Method::GET, &["v1", "pages", &scope.to_string(), slug]);
        if let Some(version) = version {
            request = request.query(&[("version", version)]);
        }
        self.send(request, Retry::Yes).await
    }

    /// Writes a new version. Check `status` in the response: a version that waits for review
    /// leaves the current version in place. `Error::VersionConflict` means the page changed.
    pub async fn put_page(
        &self,
        scope: &Scope,
        slug: &str,
        page: &PageWrite,
    ) -> Result<PageWriteResponse> {
        let request = self
            .request(Method::PUT, &["v1", "pages", &scope.to_string(), slug])
            .json(page);
        self.send(request, Retry::No).await
    }

    /// Hard-deletes the page with every version.
    pub async fn delete_page(&self, scope: &Scope, slug: &str) -> Result<()> {
        let request = self.request(Method::DELETE, &["v1", "pages", &scope.to_string(), slug]);
        self.send_empty(request, Retry::No).await
    }

    /// Kept versions in any status, newest first, without bodies.
    pub async fn page_versions(&self, scope: &Scope, slug: &str) -> Result<Vec<PageVersion>> {
        let request = self.request(
            Method::GET,
            &["v1", "pages", &scope.to_string(), slug, "versions"],
        );
        let response: PageVersions = self.send(request, Retry::Yes).await?;
        Ok(response.versions)
    }

    /// Quarantined page versions, oldest first. Admin only. `scopes` holds exact scopes or
    /// patterns such as `shared:*`, with the rules of `quarantine`.
    pub async fn page_quarantine<S: ToString>(
        &self,
        scopes: &[S],
        limit: Option<i64>,
    ) -> Result<Vec<PendingPageVersion>> {
        let scopes = scopes
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let mut query = vec![("scopes", scopes)];
        if let Some(limit) = limit {
            query.push(("limit", limit.to_string()));
        }
        let request = self
            .request(Method::GET, &["v1", "pages", "quarantine"])
            .query(&query);
        let response: PendingPageVersions = self.send(request, Retry::Yes).await?;
        Ok(response.versions)
    }

    /// Makes a quarantined version current. Admin only. `Error::VersionConflict` means the version
    /// it edited is no longer current: reject it so the writer can rewrite.
    pub async fn approve_page(&self, page_id: Uuid, version: i64) -> Result<Page> {
        let request = self.request(
            Method::POST,
            &[
                "v1",
                "pages",
                "quarantine",
                &page_id.to_string(),
                &version.to_string(),
                "approve",
            ],
        );
        self.send(request, Retry::No).await
    }

    /// Admin only.
    pub async fn reject_page(&self, page_id: Uuid, version: i64) -> Result<PageVersion> {
        let request = self.request(
            Method::POST,
            &[
                "v1",
                "pages",
                "quarantine",
                &page_id.to_string(),
                &version.to_string(),
                "reject",
            ],
        );
        self.send(request, Retry::No).await
    }

    // ---- Insights -----------------------------------------------------------------------------

    /// Writes an insight. Check `status` in the response: one that needs review is quarantined.
    pub async fn write_insight(&self, insight: &InsightWrite) -> Result<InsightWriteResponse> {
        let request = self
            .request(Method::POST, &["v1", "insights"])
            .json(insight);
        self.send(request, Retry::No).await
    }

    /// Insights in `scopes`, newest first. `limit` is 1 to 500, default 100.
    pub async fn list_insights(
        &self,
        scopes: &[Scope],
        level: Option<InsightLevel>,
        status: Option<InsightStatus>,
        limit: Option<i64>,
    ) -> Result<Vec<Insight>> {
        let mut query = vec![("scopes", join_scopes(scopes))];
        if let Some(level) = level {
            query.push(("level", level.as_str().to_owned()));
        }
        if let Some(status) = status {
            query.push(("status", status.as_str().to_owned()));
        }
        if let Some(limit) = limit {
            query.push(("limit", limit.to_string()));
        }
        let request = self.request(Method::GET, &["v1", "insights"]).query(&query);
        let response: Insights = self.send(request, Retry::Yes).await?;
        Ok(response.insights)
    }

    /// The insight in any status, its sources as they are now, and the insights it replaced.
    pub async fn get_insight(&self, id: Uuid) -> Result<InsightDetail> {
        let request = self.request(Method::GET, &["v1", "insights", &id.to_string()]);
        self.send(request, Retry::Yes).await
    }

    /// Makes a stale insight current again with new sources, checked as a write.
    pub async fn confirm_insight(&self, id: Uuid, sources: &[InsightSourceRef]) -> Result<Insight> {
        let request = self
            .request(
                Method::POST,
                &["v1", "insights", &id.to_string(), "confirm"],
            )
            .json(&json!({"sources": sources}));
        self.send(request, Retry::No).await
    }

    /// Withdraws an active or stale insight. Insights citing it become stale.
    pub async fn retire_insight(&self, id: Uuid, reason: impl Into<String>) -> Result<Insight> {
        let request = self
            .request(Method::POST, &["v1", "insights", &id.to_string(), "retire"])
            .json(&json!({"reason": reason.into()}));
        self.send(request, Retry::No).await
    }

    /// Quarantined insights, oldest first. Admin only. `scopes` holds exact scopes or patterns
    /// such as `shared:*`, with the rules of `quarantine`.
    pub async fn insight_quarantine<S: ToString>(
        &self,
        scopes: &[S],
        limit: Option<i64>,
    ) -> Result<Vec<Insight>> {
        let scopes = scopes
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let mut query = vec![("scopes", scopes)];
        if let Some(limit) = limit {
            query.push(("limit", limit.to_string()));
        }
        let request = self
            .request(Method::GET, &["v1", "insights", "quarantine"])
            .query(&query);
        let response: Insights = self.send(request, Retry::Yes).await?;
        Ok(response.insights)
    }

    /// Admin only. The insight becomes stale instead when a source is no longer current.
    pub async fn approve_insight(&self, id: Uuid) -> Result<Insight> {
        let request = self.request(
            Method::POST,
            &["v1", "insights", "quarantine", &id.to_string(), "approve"],
        );
        self.send(request, Retry::No).await
    }

    /// Admin only.
    pub async fn reject_insight(&self, id: Uuid) -> Result<Insight> {
        let request = self.request(
            Method::POST,
            &["v1", "insights", "quarantine", &id.to_string(), "reject"],
        );
        self.send(request, Retry::No).await
    }

    /// What to synthesize next in `scope`: stale insights, then topics. `limit` is 1 to 20,
    /// default 5.
    pub async fn reflect_batch(&self, scope: &Scope, limit: Option<i64>) -> Result<ReflectBatch> {
        let mut body = json!({"scope": scope});
        if let Some(limit) = limit {
            body["limit"] = json!(limit);
        }
        let request = self
            .request(Method::POST, &["v1", "reflect", "batch"])
            .json(&body);
        // A POST, but a read: it changes nothing, so it can be retried.
        self.send(request, Retry::Yes).await
    }

    /// Stops offering a topic until its facts change. `fact_set` is the one the batch returned.
    pub async fn reflect_decline(
        &self,
        scope: &Scope,
        topic_key: Uuid,
        fact_set: &str,
    ) -> Result<()> {
        let request = self
            .request(Method::POST, &["v1", "reflect", "decline"])
            .json(&json!({"scope": scope, "topic_key": topic_key, "fact_set": fact_set}));
        self.send_empty(request, Retry::No).await
    }

    // ---- Users --------------------------------------------------------------------------------

    /// Hard-deletes everything under `user:{user_id}`. Admin only.
    pub async fn delete_user_memory(&self, user_id: &str) -> Result<PurgeReport> {
        let request = self.request(Method::DELETE, &["v1", "users", user_id, "memory"]);
        self.send(request, Retry::No).await
    }

    // ---- Transport ----------------------------------------------------------------------------

    fn url(&self, segments: &[&str]) -> Url {
        let mut url = self.base_url.clone();
        if let Ok(mut path) = url.path_segments_mut() {
            path.pop_if_empty().extend(segments);
        }
        url
    }

    fn request(&self, method: Method, segments: &[&str]) -> RequestBuilder {
        self.http
            .request(method, self.url(segments))
            .bearer_auth(&self.api_key)
            .timeout(self.options.timeout)
    }

    async fn send<T: DeserializeOwned>(&self, request: RequestBuilder, retry: Retry) -> Result<T> {
        let response = self.execute(request, retry).await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        serde_json::from_slice(&bytes).map_err(|error| Error::UnexpectedResponse {
            status: status.as_u16(),
            detail: format!("the body does not match the API: {error}"),
        })
    }

    async fn send_empty(&self, request: RequestBuilder, retry: Retry) -> Result<()> {
        self.execute(request, retry).await.map(drop)
    }

    async fn execute(&self, request: RequestBuilder, retry: Retry) -> Result<Response> {
        let retries = match retry {
            Retry::Yes => self.options.read_retries,
            Retry::No => 0,
        };
        let mut attempt = 0;
        loop {
            let outcome = match request.try_clone() {
                Some(copy) => attempt_once(copy).await,
                // Only a streaming body cannot be cloned, and this client sends none.
                None => return attempt_once(request).await,
            };
            match outcome {
                Err(error) if attempt < retries && error.is_retryable() => {
                    let wait = error
                        .retry_after()
                        .unwrap_or(RETRY_BASE * 2u32.saturating_pow(attempt))
                        .min(MAX_RETRY_WAIT);
                    tokio::time::sleep(wait).await;
                    attempt += 1;
                }
                other => return other,
            }
        }
    }
}

async fn attempt_once(request: RequestBuilder) -> Result<Response> {
    let response = request.send().await?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let retry_after = retry_after_of(&response, status);
    let body = response.bytes().await?;
    Err(from_response(status, retry_after, &body))
}

fn retry_after_of(response: &Response, status: StatusCode) -> Option<Duration> {
    if status != StatusCode::TOO_MANY_REQUESTS {
        return None;
    }
    let seconds = response.headers().get(RETRY_AFTER)?.to_str().ok()?;
    seconds.trim().parse().ok().map(Duration::from_secs)
}

fn join_scopes(scopes: &[Scope]) -> String {
    scopes
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

#[derive(Serialize)]
struct RecallBody {
    scopes: Vec<Scope>,
    query: String,
    limit: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    as_of: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    known_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    page_limit: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    insight_limit: Option<i64>,
}

/// A recall that has not been sent. Await it to send.
pub struct RecallBuilder<'a> {
    client: &'a MemoryOs,
    body: RecallBody,
}

impl RecallBuilder<'_> {
    /// What was true at this time.
    pub fn as_of(mut self, at: DateTime<Utc>) -> Self {
        self.body.as_of = Some(at);
        self
    }

    /// What MemoryOS knew at this time.
    pub fn known_at(mut self, at: DateTime<Utc>) -> Self {
        self.body.known_at = Some(at);
        self
    }

    pub fn limit(mut self, limit: i64) -> Self {
        self.body.limit = limit;
        self
    }

    /// Most page hits to return, 0 to 10, default 3. 0 turns page search off.
    pub fn page_limit(mut self, page_limit: i64) -> Self {
        self.body.page_limit = Some(page_limit);
        self
    }

    /// Most insights to return, 0 to 10, default 3. 0 turns insight search off. An older server
    /// ignores it and returns no insights.
    pub fn insight_limit(mut self, insight_limit: i64) -> Self {
        self.body.insight_limit = Some(insight_limit);
        self
    }

    /// Searches this scope as well.
    pub fn scope(mut self, scope: Scope) -> Self {
        self.body.scopes.push(scope);
        self
    }
}

impl<'a> IntoFuture for RecallBuilder<'a> {
    type Output = Result<RecallResponse>;
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send + 'a>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let request = self
                .client
                .request(Method::POST, &["v1", "recall"])
                .json(&self.body);
            // A POST, but a read: it changes nothing, so it can be retried.
            self.client.send(request, Retry::Yes).await
        })
    }
}

#[derive(Deserialize)]
struct Facts {
    facts: Vec<Fact>,
}

#[derive(Deserialize)]
struct FactsScored {
    facts: Vec<ScoredFact>,
}

#[derive(Deserialize)]
struct Blocks {
    blocks: Vec<Block>,
}

#[derive(Deserialize)]
struct Versions {
    versions: Vec<BlockVersion>,
}

#[derive(Deserialize)]
struct Pages {
    pages: Vec<PageSummary>,
}

#[derive(Deserialize)]
struct PageVersions {
    versions: Vec<PageVersion>,
}

#[derive(Deserialize)]
struct PendingPageVersions {
    versions: Vec<PendingPageVersion>,
}

#[derive(Deserialize)]
struct Insights {
    insights: Vec<Insight>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(base: &str) -> MemoryOs {
        MemoryOs::new(base, "mos_secret", Options::default()).expect("client")
    }

    #[test]
    fn urls_keep_a_base_path_and_encode_segments() {
        assert_eq!(
            client("http://h:1/").url(&["v1", "recall"]).as_str(),
            "http://h:1/v1/recall"
        );
        assert_eq!(
            client("http://h:1/base/").url(&["v1", "recall"]).as_str(),
            "http://h:1/base/v1/recall"
        );
        assert_eq!(
            client("http://h:1").url(&["v1", "a/b c"]).as_str(),
            "http://h:1/v1/a%2Fb%20c"
        );
    }

    #[test]
    fn debug_output_hides_the_key() {
        assert!(!format!("{:?}", client("http://h:1")).contains("mos_secret"));
    }

    #[test]
    fn a_bad_base_url_is_an_error() {
        assert!(MemoryOs::new("not a url", "k", Options::default()).is_err());
        assert!(MemoryOs::new("mailto:a@b", "k", Options::default()).is_err());
    }

    #[test]
    fn recall_serializes_only_what_is_set() {
        let body = RecallBody {
            scopes: vec![Scope::user("u")],
            query: "q".into(),
            limit: 10,
            as_of: None,
            known_at: None,
            page_limit: None,
            insight_limit: None,
        };
        assert_eq!(
            serde_json::to_value(&body).expect("serializes"),
            json!({"scopes": ["user:u"], "query": "q", "limit": 10})
        );
        let client = client("http://h:1");
        let with_pages = client.recall(Scope::user("u"), "q").page_limit(0);
        assert_eq!(
            serde_json::to_value(&with_pages.body).expect("serializes")["page_limit"],
            0
        );
        let with_insights = client.recall(Scope::user("u"), "q").insight_limit(7);
        assert_eq!(
            serde_json::to_value(&with_insights.body).expect("serializes")["insight_limit"],
            7
        );
    }

    #[tokio::test]
    async fn insight_calls_use_the_documented_paths_and_writes_are_not_retried() {
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        // Records each request line and answers 503, which a read retries and a write does not.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let record = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut request = [0u8; 4096];
                let read = socket.read(&mut request).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&request[..read]).into_owned();
                let line = text.lines().next().unwrap_or_default().to_owned();
                record.lock().expect("lock").push(line);
                let reply = "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\
                             connection: close\r\n\r\n";
                let _ = socket.write_all(reply.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        let options = Options {
            read_retries: 1,
            ..Options::default()
        };
        let mem = MemoryOs::new(format!("http://{address}"), "k", options).expect("client");
        let scope = Scope::agent("a1");
        let id = Uuid::nil();
        let write = InsightWrite::new(scope.clone(), InsightLevel::Context, "s", Vec::new());
        assert!(mem.write_insight(&write).await.is_err());
        assert!(mem
            .list_insights(
                std::slice::from_ref(&scope),
                Some(InsightLevel::Guidance),
                Some(InsightStatus::Stale),
                Some(5)
            )
            .await
            .is_err());
        assert!(mem.get_insight(id).await.is_err());
        assert!(mem.confirm_insight(id, &[]).await.is_err());
        assert!(mem.retire_insight(id, "r").await.is_err());
        assert!(mem.insight_quarantine(&["shared:*"], None).await.is_err());
        assert!(mem.approve_insight(id).await.is_err());
        assert!(mem.reject_insight(id).await.is_err());
        assert!(mem.reflect_batch(&scope, Some(2)).await.is_err());
        assert!(mem.reflect_decline(&scope, id, "ab").await.is_err());
        let nil = "00000000-0000-0000-0000-000000000000";
        let list =
            "GET /v1/insights?scopes=agent%3Aa1&level=guidance&status=stale&limit=5 HTTP/1.1";
        let quarantine = "GET /v1/insights/quarantine?scopes=shared%3A* HTTP/1.1";
        assert_eq!(
            *seen.lock().expect("lock"),
            [
                "POST /v1/insights HTTP/1.1".to_owned(),
                list.to_owned(),
                list.to_owned(),
                format!("GET /v1/insights/{nil} HTTP/1.1"),
                format!("GET /v1/insights/{nil} HTTP/1.1"),
                format!("POST /v1/insights/{nil}/confirm HTTP/1.1"),
                format!("POST /v1/insights/{nil}/retire HTTP/1.1"),
                quarantine.to_owned(),
                quarantine.to_owned(),
                format!("POST /v1/insights/quarantine/{nil}/approve HTTP/1.1"),
                format!("POST /v1/insights/quarantine/{nil}/reject HTTP/1.1"),
                "POST /v1/reflect/batch HTTP/1.1".to_owned(),
                "POST /v1/reflect/batch HTTP/1.1".to_owned(),
                "POST /v1/reflect/decline HTTP/1.1".to_owned(),
            ]
        );
    }

    #[test]
    fn page_urls_keep_the_scope_in_one_segment() {
        let scope = Scope::shared("team").to_string();
        assert_eq!(
            client("http://h:1")
                .url(&["v1", "pages", &scope, "runbook", "versions"])
                .as_str(),
            "http://h:1/v1/pages/shared:team/runbook/versions"
        );
        assert_eq!(
            client("http://h:1")
                .url(&["v1", "pages", "quarantine", "x", "2", "approve"])
                .as_str(),
            "http://h:1/v1/pages/quarantine/x/2/approve"
        );
    }

    #[tokio::test]
    async fn page_calls_use_the_documented_paths_and_a_page_write_is_not_retried() {
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        // Records each request line and answers 503, which a read retries and a write does not.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let record = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut request = [0u8; 4096];
                let read = socket.read(&mut request).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&request[..read]).into_owned();
                let line = text.lines().next().unwrap_or_default().to_owned();
                record.lock().expect("lock").push(line);
                let reply = "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\
                             connection: close\r\n\r\n";
                let _ = socket.write_all(reply.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        let options = Options {
            read_retries: 0,
            ..Options::default()
        };
        let mem = MemoryOs::new(format!("http://{address}"), "k", options).expect("client");
        let scope = Scope::agent("a1");
        let id = Uuid::nil();
        assert!(mem.list_pages(&scope, Some("run"), Some(5)).await.is_err());
        assert!(mem.get_page(&scope, "s", Some(2)).await.is_err());
        assert!(mem
            .put_page(&scope, "s", &PageWrite::create("T", "B"))
            .await
            .is_err());
        assert!(mem.delete_page(&scope, "s").await.is_err());
        assert!(mem.page_versions(&scope, "s").await.is_err());
        assert!(mem.page_quarantine(&["shared:*"], None).await.is_err());
        assert!(mem.approve_page(id, 2).await.is_err());
        assert!(mem.reject_page(id, 2).await.is_err());
        let nil = "00000000-0000-0000-0000-000000000000";
        assert_eq!(
            *seen.lock().expect("lock"),
            [
                "GET /v1/pages?scope=agent%3Aa1&prefix=run&limit=5 HTTP/1.1".to_owned(),
                "GET /v1/pages/agent:a1/s?version=2 HTTP/1.1".to_owned(),
                "PUT /v1/pages/agent:a1/s HTTP/1.1".to_owned(),
                "DELETE /v1/pages/agent:a1/s HTTP/1.1".to_owned(),
                "GET /v1/pages/agent:a1/s/versions HTTP/1.1".to_owned(),
                "GET /v1/pages/quarantine?scopes=shared%3A* HTTP/1.1".to_owned(),
                format!("POST /v1/pages/quarantine/{nil}/2/approve HTTP/1.1"),
                format!("POST /v1/pages/quarantine/{nil}/2/reject HTTP/1.1"),
            ]
        );
    }

    #[tokio::test]
    async fn a_write_is_not_retried_and_a_read_is() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        // Answers every connection with a 503, which a read retries and a write does not.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = accepted.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                // Closing with the request unread would reset the connection instead of answering.
                let mut request = [0u8; 4096];
                let _ = socket.read(&mut request).await;
                let reply = "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\
                             connection: close\r\n\r\n";
                let _ = socket.write_all(reply.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        let options = Options {
            read_retries: 2,
            ..Options::default()
        };
        let mem = MemoryOs::new(format!("http://{address}"), "k", options).expect("client");

        let id = Uuid::now_v7();
        assert!(mem.get_fact(id).await.is_err());
        assert_eq!(
            accepted.swap(0, Ordering::SeqCst),
            3,
            "a read tries 3 times"
        );

        assert!(mem.invalidate_fact(id, "r").await.is_err());
        assert_eq!(accepted.swap(0, Ordering::SeqCst), 1, "a write tries once");
    }
}
