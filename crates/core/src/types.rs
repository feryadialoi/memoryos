use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{Error, Result};

/// Authenticated caller. Every store call runs under `tenant`.
#[derive(Debug, Clone)]
pub struct RequestContext {
    pub tenant: String,
    pub actor: String,
}

/// Who a memory belongs to. Serialized as `user:<id>`, `agent:<id>` or `shared:<id>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Scope {
    User(String),
    Agent(String),
    Shared(String),
}

impl Scope {
    pub fn user(id: impl Into<String>) -> Self {
        Self::User(id.into())
    }
    pub fn agent(id: impl Into<String>) -> Self {
        Self::Agent(id.into())
    }
    pub fn shared(id: impl Into<String>) -> Self {
        Self::Shared(id.into())
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::User(id) => write!(f, "user:{id}"),
            Self::Agent(id) => write!(f, "agent:{id}"),
            Self::Shared(id) => write!(f, "shared:{id}"),
        }
    }
}

impl FromStr for Scope {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let invalid = || Error::InvalidInput(format!("invalid scope: {s}"));
        let (kind, id) = s.split_once(':').ok_or_else(invalid)?;
        let valid_id = !id.is_empty()
            && id.len() <= 128
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
        if !valid_id {
            return Err(invalid());
        }
        match kind {
            "user" => Ok(Self::User(id.to_owned())),
            "agent" => Ok(Self::Agent(id.to_owned())),
            "shared" => Ok(Self::Shared(id.to_owned())),
            _ => Err(invalid()),
        }
    }
}

impl TryFrom<String> for Scope {
    type Error = Error;
    fn try_from(s: String) -> Result<Self> {
        s.parse()
    }
}

impl From<Scope> for String {
    fn from(s: Scope) -> Self {
        s.to_string()
    }
}

/// Provenance trust, ordered lowest to highest. See docs/SPEC.md section 5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trust {
    External,
    Agent,
    User,
    System,
}

impl Trust {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::External => "external",
            Self::Agent => "agent",
            Self::User => "user",
            Self::System => "system",
        }
    }
}

impl FromStr for Trust {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "external" => Ok(Self::External),
            "agent" => Ok(Self::Agent),
            "user" => Ok(Self::User),
            "system" => Ok(Self::System),
            _ => Err(Error::InvalidInput(format!("invalid trust: {s}"))),
        }
    }
}

/// SPEC section 5: facts from this source are quarantined whatever they say, because `external`
/// content is untrusted anywhere and an agent cannot plant claims in user or shared memory.
pub fn source_requires_quarantine(scope: &Scope, trust: Trust) -> bool {
    match trust {
        Trust::External => true,
        Trust::Agent => !matches!(scope, Scope::Agent(_)),
        Trust::User | Trust::System => false,
    }
}

/// Key for entity identity: case-insensitive, with whitespace runs collapsed.
pub fn normalize_name(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FactStatus {
    Active,
    Quarantined,
    Rejected,
}

impl FactStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Quarantined => "quarantined",
            Self::Rejected => "rejected",
        }
    }
}

impl FromStr for FactStatus {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "active" => Ok(Self::Active),
            "quarantined" => Ok(Self::Quarantined),
            "rejected" => Ok(Self::Rejected),
            _ => Err(Error::InvalidInput(format!("invalid fact status: {s}"))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewEpisode {
    pub scope: Scope,
    pub source: String,
    pub trust: Trust,
    pub content: String,
    pub occurred_at: Option<DateTime<Utc>>,
    #[serde(default = "empty_object")]
    pub metadata: serde_json::Value,
    pub idempotency_key: Option<String>,
}

fn empty_object() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Episode {
    pub id: Uuid,
    pub scope: Scope,
    pub source: String,
    pub trust: Trust,
    pub content: String,
    pub metadata: serde_json::Value,
    pub occurred_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub processed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entity {
    pub id: Uuid,
    pub scope: Scope,
    pub kind: String,
    pub name: String,
    pub aliases: Vec<String>,
}

/// A fact is both a searchable statement and a graph edge.
///
/// Time semantics (docs/SPEC.md section 4):
/// - `valid_from`/`valid_to`: when the fact was true in the world. Superseding sets `valid_to`.
/// - `recorded_at`/`invalidated_at`: when we learned it, and when we retracted it as wrong.
/// - `expires_at`: when it stops being true with nothing replacing it. From then on it is not current.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fact {
    pub id: Uuid,
    pub scope: Scope,
    pub subject_id: Uuid,
    pub predicate: String,
    pub object_id: Option<Uuid>,
    pub statement: String,
    pub trust: Trust,
    pub status: FactStatus,
    pub valid_from: DateTime<Utc>,
    pub valid_to: Option<DateTime<Utc>>,
    /// Absent in data from before expiry existed, which means no expiry.
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    pub recorded_at: DateTime<Utc>,
    pub invalidated_at: Option<DateTime<Utc>>,
    pub superseded_by: Option<Uuid>,
    /// Set on a quarantined fact: the fact that approval will close.
    pub pending_supersede_of: Option<Uuid>,
    pub source_episode_ids: Vec<Uuid>,
}

impl Fact {
    /// Whether the fact has expired by `at`. A fact with no `expires_at` never expires.
    pub fn expired_at(&self, at: DateTime<Utc>) -> bool {
        self.expires_at.is_some_and(|expires| expires <= at)
    }
}

/// A fact with the names of what it connects, for listing and drawing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactView {
    #[serde(flatten)]
    pub fact: Fact,
    pub subject_name: String,
    pub subject_kind: String,
    pub object_name: Option<String>,
    pub object_kind: Option<String>,
}

/// Which active facts a listing returns. `Current` is what recall can return now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FactListState {
    Current,
    Past,
    All,
}

/// What one scope holds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopeSummary {
    pub scope: Scope,
    pub current: i64,
    /// Active facts that recall does not return now: closed, retracted or starting later.
    pub past: i64,
    pub quarantined: i64,
    pub entities: i64,
    pub blocks: i64,
    /// Pages with a current version.
    #[serde(default)]
    pub pages: i64,
    /// Active insights.
    #[serde(default)]
    pub insights: i64,
    pub last_recorded_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct NewEntity {
    pub scope: Scope,
    pub kind: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub embedding: Vec<f32>,
}

#[derive(Debug, Clone)]
pub struct NewFact {
    pub scope: Scope,
    pub subject_id: Uuid,
    pub predicate: String,
    pub object_id: Option<Uuid>,
    pub statement: String,
    pub embedding: Vec<f32>,
    pub trust: Trust,
    pub status: FactStatus,
    pub valid_from: DateTime<Utc>,
    pub valid_to: Option<DateTime<Utc>>,
    /// Must be after `valid_from`.
    pub expires_at: Option<DateTime<Utc>>,
}

/// One decision of a commit, applied atomically by `MemoryStore::apply_commit`.
#[derive(Debug, Clone)]
pub enum FactOp {
    /// Insert a new fact, linked to the episode.
    Add(NewFact),
    /// Insert `new`, and close `old_fact_id` with `valid_to = new.valid_from`.
    Supersede { old_fact_id: Uuid, new: NewFact },
    /// The episode restates an existing fact: only link the episode as another source.
    Corroborate { fact_id: Uuid },
    /// The episode replaces `old_fact_id` with a statement that the current fact `fact_id` already
    /// holds: link the episode to `fact_id`, and close `old_fact_id` as a supersede by `fact_id`
    /// would, at `fact_id`'s `valid_from`. Only for an episode whose new fact would be active.
    SupersedeWithExisting { old_fact_id: Uuid, fact_id: Uuid },
}

/// Time and visibility filter for fact reads.
#[derive(Debug, Clone, Default)]
pub struct FactFilter {
    /// Valid-time point. `None` means now.
    pub as_of: Option<DateTime<Utc>>,
    /// Knowledge-time point: only facts recorded by then and not yet retracted by then.
    pub known_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoredFact {
    pub fact: Fact,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNeighborhood {
    pub root: Uuid,
    pub entities: Vec<Entity>,
    pub edges: Vec<Fact>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Block {
    pub id: Uuid,
    pub agent_id: String,
    pub label: String,
    pub content: String,
    pub max_chars: i32,
    pub version: i64,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockVersion {
    pub version: i64,
    pub content: String,
    pub edited_by: String,
    pub edited_at: DateTime<Utc>,
}

/// Longest page slug, title, change summary and body, and most sections and pending versions.
/// See docs/SPEC.md section 8.
pub const PAGE_SLUG_MAX: usize = 128;
pub const PAGE_TITLE_MAX: usize = 200;
pub const PAGE_SUMMARY_MAX: usize = 500;
pub const PAGE_BODY_MAX: usize = 100_000;
pub const PAGE_SECTION_TEXT_MAX: usize = 1_000;
pub const PAGE_SECTIONS_MAX: usize = 400;
pub const PAGE_PENDING_MAX: usize = 20;
/// Versions kept per page besides the current one and pending ones. Older ones are pruned.
pub const PAGE_VERSIONS_KEPT: usize = 100;

/// A page slug: 1 to 128 of lowercase ASCII letters, digits, `-`, `_` and `.`, starting with a
/// letter or digit. No `/`, so a slug is always one path segment.
pub fn validate_slug(slug: &str) -> Result<()> {
    let valid = !slug.is_empty()
        && slug.len() <= PAGE_SLUG_MAX
        && slug
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'));
    if valid {
        Ok(())
    } else {
        Err(Error::InvalidInput(format!("invalid page slug: {slug}")))
    }
}

/// Review state of one page version. Only the page's current version is `Active`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PageVersionStatus {
    /// The current version: what readers, recall and hooks see.
    Active,
    /// Waiting for an admin. Readers keep the current version meanwhile.
    Quarantined,
    Rejected,
    /// Was current, and a later version replaced it.
    Superseded,
}

impl PageVersionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Quarantined => "quarantined",
            Self::Rejected => "rejected",
            Self::Superseded => "superseded",
        }
    }
}

impl FromStr for PageVersionStatus {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "active" => Ok(Self::Active),
            "quarantined" => Ok(Self::Quarantined),
            "rejected" => Ok(Self::Rejected),
            "superseded" => Ok(Self::Superseded),
            _ => Err(Error::InvalidInput(format!(
                "invalid page version status: {s}"
            ))),
        }
    }
}

/// One version of a page, read in full.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page {
    pub id: Uuid,
    pub scope: Scope,
    pub slug: String,
    pub title: String,
    /// Markdown.
    pub body: String,
    pub summary: Option<String>,
    pub version: i64,
    pub trust: Trust,
    pub status: PageVersionStatus,
    pub edited_by: String,
    pub edited_at: DateTime<Utc>,
}

/// A page in a listing: its current version without the body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageSummary {
    pub id: Uuid,
    pub scope: Scope,
    pub slug: String,
    pub title: String,
    pub version: i64,
    pub edited_at: DateTime<Utc>,
}

/// A version's metadata without its body, as history and write results return it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageVersion {
    pub page_id: Uuid,
    pub scope: Scope,
    pub slug: String,
    pub version: i64,
    /// The version the writer edited. `None` for a page's first version.
    pub base_version: Option<i64>,
    pub title: String,
    pub summary: Option<String>,
    pub trust: Trust,
    pub status: PageVersionStatus,
    pub edited_by: String,
    pub edited_at: DateTime<Utc>,
}

/// A quarantined version with what a reviewer needs to diff it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingPageVersion {
    #[serde(flatten)]
    pub version: PageVersion,
    pub body: String,
    /// Body of `base_version`, or `None` when there is no base or it was pruned.
    pub base_body: Option<String>,
}

/// One searchable section of a version, already split and embedded by the caller.
#[derive(Debug, Clone)]
pub struct NewPageSection {
    pub ordinal: i32,
    /// Title path, such as `Upgrade MemoryOS > Native install`.
    pub heading: String,
    pub text: String,
    pub embedding: Vec<f32>,
}

/// A page write. The store decides the version's status atomically, see `MemoryStore::put_page`.
#[derive(Debug, Clone)]
pub struct NewPageVersion {
    pub scope: Scope,
    pub slug: String,
    pub title: String,
    pub body: String,
    pub summary: Option<String>,
    pub trust: Trust,
    /// `None` creates the page. `Some(v)` edits current version `v`.
    pub expected_version: Option<i64>,
    /// Set for an admin write that approves itself: the caller's `max_trust`.
    pub approve_up_to: Option<Trust>,
    pub sections: Vec<NewPageSection>,
}

/// A section of a page's current version found by search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageSectionHit {
    pub section_id: Uuid,
    pub page_id: Uuid,
    pub scope: Scope,
    pub slug: String,
    pub title: String,
    pub version: i64,
    pub heading: String,
    pub text: String,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: Uuid,
    pub tenant: String,
    pub name: String,
    /// Scope strings the key may use. `*` allows all; a trailing `*` matches a prefix (`user:*`).
    pub scopes: Vec<String>,
    pub max_trust: Trust,
    pub is_admin: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PurgeReport {
    pub facts_deleted: u64,
    pub episodes_deleted: u64,
    pub entities_deleted: u64,
    #[serde(default)]
    pub pages_deleted: u64,
    #[serde(default)]
    pub insights_deleted: u64,
}

/// Longest insight statement, fewest and most sources, and the reflect batch limits. See
/// docs/SPEC.md section 8, Insights.
pub const INSIGHT_STATEMENT_MAX: usize = 1_000;
pub const INSIGHT_SOURCES_MIN: usize = 2;
pub const INSIGHT_SOURCES_MAX: usize = 50;
/// Items in one reflect batch: the default and the most a caller may ask for.
pub const REFLECT_BATCH_DEFAULT: i64 = 5;
pub const REFLECT_BATCH_MAX: i64 = 20;
/// Fewest uncited facts on one subject before it is offered as a topic, unless configured.
pub const REFLECT_MIN_FACTS_DEFAULT: i64 = 3;
/// Facts shown per topic, newest first.
pub const REFLECT_TOPIC_FACTS_MAX: usize = 30;

/// How far an insight is from its evidence. Ordered lowest to highest: an insight may cite only
/// facts and insights of a lower level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InsightLevel {
    /// Who or what this is: "Ada is a backend engineer on distributed systems".
    Context,
    /// What it means: "Ada values operational simplicity over clever abstractions".
    Understanding,
    /// What to do about it: "Before proposing an abstraction, cover failure modes first".
    Guidance,
}

impl InsightLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Context => "context",
            Self::Understanding => "understanding",
            Self::Guidance => "guidance",
        }
    }

    /// Whether an insight of this level may cite an insight of `source`'s level. Facts can be
    /// cited by every level.
    pub fn may_cite(self, source: InsightLevel) -> bool {
        source < self
    }
}

impl FromStr for InsightLevel {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "context" => Ok(Self::Context),
            "understanding" => Ok(Self::Understanding),
            "guidance" => Ok(Self::Guidance),
            _ => Err(Error::InvalidInput(format!("invalid insight level: {s}"))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InsightStatus {
    /// Current: hooks print it and recall returns it.
    Active,
    /// A source changed. Waits in the reflect batch until confirmed, superseded or retired.
    Stale,
    /// Waiting for an admin.
    Quarantined,
    Rejected,
    /// Withdrawn as no longer true.
    Retired,
    /// Replaced by a later insight, named in `superseded_by`.
    Superseded,
}

impl InsightStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Stale => "stale",
            Self::Quarantined => "quarantined",
            Self::Rejected => "rejected",
            Self::Retired => "retired",
            Self::Superseded => "superseded",
        }
    }
}

impl FromStr for InsightStatus {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "active" => Ok(Self::Active),
            "stale" => Ok(Self::Stale),
            "quarantined" => Ok(Self::Quarantined),
            "rejected" => Ok(Self::Rejected),
            "retired" => Ok(Self::Retired),
            "superseded" => Ok(Self::Superseded),
            _ => Err(Error::InvalidInput(format!("invalid insight status: {s}"))),
        }
    }
}

/// SPEC section 8, Insights: an insight is quarantined when any of its sources would quarantine a
/// fact, and guidance outside an agent's own scope always waits for review, because it changes
/// how agents act. `trust` is the insight's trust, the lowest of the writer's and every source's.
pub fn insight_requires_quarantine(scope: &Scope, level: InsightLevel, trust: Trust) -> bool {
    source_requires_quarantine(scope, trust)
        || (level == InsightLevel::Guidance && !matches!(scope, Scope::Agent(_)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InsightSourceKind {
    Fact,
    Insight,
}

impl InsightSourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fact => "fact",
            Self::Insight => "insight",
        }
    }
}

impl FromStr for InsightSourceKind {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "fact" => Ok(Self::Fact),
            "insight" => Ok(Self::Insight),
            _ => Err(Error::InvalidInput(format!(
                "invalid insight source kind: {s}"
            ))),
        }
    }
}

/// One piece of evidence an insight cites.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct InsightSourceRef {
    pub kind: InsightSourceKind,
    pub id: Uuid,
}

/// What happened to a source that made its insight stale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "change", rename_all = "snake_case")]
pub enum SourceChange {
    /// A fact was closed by a later fact, `by`.
    Superseded {
        by: Uuid,
    },
    Invalidated,
    Expired,
    /// The fact was deleted with its last source episode.
    Purged,
    /// A cited insight went stale, was retired or was superseded.
    InsightChanged {
        status: InsightStatus,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaleSource {
    #[serde(flatten)]
    pub source: InsightSourceRef,
    #[serde(flatten)]
    pub change: SourceChange,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Insight {
    pub id: Uuid,
    pub scope: Scope,
    pub level: InsightLevel,
    pub statement: String,
    pub trust: Trust,
    pub status: InsightStatus,
    /// Set while `status` is `Stale`: which sources changed and how.
    pub stale_reason: Option<Vec<StaleSource>>,
    pub sources: Vec<InsightSourceRef>,
    pub valid_from: DateTime<Utc>,
    /// Set when retired or superseded.
    pub valid_to: Option<DateTime<Utc>>,
    pub superseded_by: Option<Uuid>,
    /// Set on a quarantined insight: the insight approval will close.
    pub pending_supersede_of: Option<Uuid>,
    pub written_by: String,
    pub recorded_at: DateTime<Utc>,
}

/// A source as `insight_read` shows it: the evidence itself and whether it is still current.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InsightSourceView {
    #[serde(flatten)]
    pub source: InsightSourceRef,
    /// The fact's or insight's statement. `None` when the source was purged.
    pub statement: Option<String>,
    /// Active and, for a fact, valid now.
    pub current: bool,
}

/// An insight with its evidence and the insights it replaced, newest first.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InsightDetail {
    #[serde(flatten)]
    pub insight: Insight,
    pub source_views: Vec<InsightSourceView>,
    pub history: Vec<Insight>,
}

/// An insight write. The store checks the sources and decides trust and status atomically, see
/// `MemoryStore::put_insight`.
#[derive(Debug, Clone)]
pub struct NewInsight {
    pub scope: Scope,
    pub level: InsightLevel,
    pub statement: String,
    pub embedding: Vec<f32>,
    /// The writer's trust, before the sources lower it.
    pub trust: Trust,
    pub sources: Vec<InsightSourceRef>,
    /// An active or stale insight of the same scope that this one replaces.
    pub supersedes: Option<Uuid>,
    pub written_by: String,
}

#[derive(Debug, Clone, Default)]
pub struct InsightFilter {
    pub level: Option<InsightLevel>,
    pub status: Option<InsightStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoredInsight {
    pub insight: Insight,
    pub score: f64,
}

/// Uncited current facts sharing one subject, offered for synthesis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReflectTopic {
    /// The subject entity's id.
    pub topic_key: Uuid,
    pub subject: String,
    /// At most `REFLECT_TOPIC_FACTS_MAX`, newest first.
    pub facts: Vec<Fact>,
    /// Hex SHA-256 of the sorted ids of `facts`, passed back by a decline.
    pub fact_set: String,
    /// Active insights already citing facts of this subject, to extend rather than duplicate.
    pub insights: Vec<Insight>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReflectBatch {
    /// Stale insights with their current evidence, oldest first.
    pub stale: Vec<InsightDetail>,
    pub topics: Vec<ReflectTopic>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_round_trips_and_rejects_bad_input() {
        let parsed: Scope = "user:u_123".parse().expect("valid scope");
        assert_eq!(parsed, Scope::user("u_123"));
        assert_eq!(parsed.to_string(), "user:u_123");
        for bad in ["user", "user:", "team:x", "user:a b", "user:a'b"] {
            assert!(bad.parse::<Scope>().is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn normalization_collapses_case_and_whitespace() {
        assert_eq!(normalize_name("  Ada \t LOVELACE "), "ada lovelace");
    }

    #[test]
    fn source_quarantine_covers_external_anywhere_and_agent_outside_its_scope() {
        for scope in [Scope::user("u"), Scope::agent("a"), Scope::shared("s")] {
            assert!(source_requires_quarantine(&scope, Trust::External));
            assert!(!source_requires_quarantine(&scope, Trust::User));
            assert!(!source_requires_quarantine(&scope, Trust::System));
        }
        assert!(source_requires_quarantine(&Scope::user("u"), Trust::Agent));
        assert!(source_requires_quarantine(
            &Scope::shared("s"),
            Trust::Agent
        ));
        assert!(!source_requires_quarantine(
            &Scope::agent("a"),
            Trust::Agent
        ));
    }

    #[test]
    fn slugs_are_one_lowercase_path_segment() {
        for good in ["memoryos-upgrade", "a", "0.1_notes", &"a".repeat(128)] {
            assert!(validate_slug(good).is_ok(), "{good} should be accepted");
        }
        for bad in [
            "",
            "Upgrade",
            "a/b",
            "-a",
            ".a",
            "a b",
            "a:b",
            &"a".repeat(129),
        ] {
            assert!(validate_slug(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn page_version_status_round_trips() {
        for s in [
            PageVersionStatus::Active,
            PageVersionStatus::Quarantined,
            PageVersionStatus::Rejected,
            PageVersionStatus::Superseded,
        ] {
            assert_eq!(
                s.as_str()
                    .parse::<PageVersionStatus>()
                    .expect("known status"),
                s
            );
        }
        assert!("pending".parse::<PageVersionStatus>().is_err());
    }

    #[test]
    fn an_insight_cites_only_lower_levels() {
        use InsightLevel::*;
        assert!(!Context.may_cite(Context));
        assert!(Understanding.may_cite(Context));
        assert!(!Understanding.may_cite(Understanding));
        assert!(Guidance.may_cite(Context));
        assert!(Guidance.may_cite(Understanding));
        assert!(!Guidance.may_cite(Guidance));
        assert!(!Context.may_cite(Guidance));
    }

    #[test]
    fn insight_quarantine_follows_facts_and_holds_guidance_outside_agent_scopes() {
        use InsightLevel::*;
        let agent = Scope::agent("a");
        for level in [Context, Understanding, Guidance] {
            assert!(
                !insight_requires_quarantine(&agent, level, Trust::Agent),
                "{level:?}"
            );
            assert!(
                insight_requires_quarantine(&agent, level, Trust::External),
                "{level:?}"
            );
        }
        for scope in [Scope::user("u"), Scope::shared("s")] {
            assert!(insight_requires_quarantine(&scope, Context, Trust::Agent));
            assert!(!insight_requires_quarantine(
                &scope,
                Understanding,
                Trust::User
            ));
            assert!(insight_requires_quarantine(&scope, Guidance, Trust::User));
            assert!(insight_requires_quarantine(&scope, Guidance, Trust::System));
        }
    }

    #[test]
    fn insight_enums_round_trip() {
        for level in [
            InsightLevel::Context,
            InsightLevel::Understanding,
            InsightLevel::Guidance,
        ] {
            assert_eq!(
                level.as_str().parse::<InsightLevel>().expect("level"),
                level
            );
        }
        for status in [
            InsightStatus::Active,
            InsightStatus::Stale,
            InsightStatus::Quarantined,
            InsightStatus::Rejected,
            InsightStatus::Retired,
            InsightStatus::Superseded,
        ] {
            assert_eq!(
                status.as_str().parse::<InsightStatus>().expect("status"),
                status
            );
        }
        for kind in [InsightSourceKind::Fact, InsightSourceKind::Insight] {
            assert_eq!(
                kind.as_str().parse::<InsightSourceKind>().expect("kind"),
                kind
            );
        }
        assert!("knowledge".parse::<InsightLevel>().is_err());
        assert!("pending".parse::<InsightStatus>().is_err());
    }

    #[test]
    fn a_stale_reason_serializes_flat() {
        let id = Uuid::nil();
        let reason = StaleSource {
            source: InsightSourceRef {
                kind: InsightSourceKind::Fact,
                id,
            },
            change: SourceChange::Superseded { by: id },
        };
        let value = serde_json::to_value(&reason).expect("json");
        assert_eq!(
            value,
            serde_json::json!({"kind": "fact", "id": id, "change": "superseded", "by": id})
        );
        let back: StaleSource = serde_json::from_value(value).expect("parse");
        assert_eq!(back, reason);
    }

    #[test]
    fn trust_orders_low_to_high() {
        assert!(Trust::External < Trust::Agent);
        assert!(Trust::Agent < Trust::User);
        assert!(Trust::User < Trust::System);
    }
}
