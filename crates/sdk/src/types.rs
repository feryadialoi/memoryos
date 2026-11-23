//! Request and response types the API defines beyond `memoryos-core`. Declared here so the SDK
//! depends on nothing server-side.

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use memoryos_core::{
    FactStatus, Insight, InsightLevel, InsightSourceRef, PageVersion, Scope, ScoredFact,
    ScoredInsight,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Midnight UTC on this date, for `.as_of()`.
///
/// # Panics
/// On a date that does not exist, such as month 13: this is for dates written in code.
pub fn date(year: i32, month: u32, day: u32) -> DateTime<Utc> {
    let naive = NaiveDate::from_ymd_opt(year, month, day)
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .expect("a valid calendar date");
    Utc.from_utc_datetime(&naive)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityRef {
    pub name: String,
    pub kind: String,
}

impl EntityRef {
    pub fn new(name: impl Into<String>, kind: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind: kind.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CommitFact {
    pub statement: String,
    pub subject: EntityRef,
    pub predicate: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object: Option<EntityRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_from: Option<DateTime<Utc>>,
    /// When the fact stops being true with nothing replacing it, such as the end of a leave.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub corroborates: Option<Uuid>,
}

impl CommitFact {
    pub fn new(
        statement: impl Into<String>,
        subject: EntityRef,
        predicate: impl Into<String>,
    ) -> Self {
        Self {
            statement: statement.into(),
            subject,
            predicate: predicate.into(),
            object: None,
            valid_from: None,
            expires_at: None,
            supersedes: None,
            corroborates: None,
        }
    }

    pub fn object(mut self, object: EntityRef) -> Self {
        self.object = Some(object);
        self
    }

    pub fn valid_from(mut self, at: DateTime<Utc>) -> Self {
        self.valid_from = Some(at);
        self
    }

    /// After this time the fact is no longer current, though it stays in history.
    pub fn expires_at(mut self, at: DateTime<Utc>) -> Self {
        self.expires_at = Some(at);
        self
    }

    pub fn supersedes(mut self, fact_id: Uuid) -> Self {
        self.supersedes = Some(fact_id);
        self
    }

    pub fn corroborates(mut self, fact_id: Uuid) -> Self {
        self.corroborates = Some(fact_id);
        self
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Commit {
    pub scope: Scope,
    pub source: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    pub facts: Vec<CommitFact>,
}

impl Commit {
    pub fn new(
        scope: Scope,
        source: impl Into<String>,
        content: impl Into<String>,
        facts: Vec<CommitFact>,
    ) -> Self {
        Self {
            scope,
            source: source.into(),
            content: content.into(),
            idempotency_key: None,
            facts,
        }
    }

    pub fn idempotency_key(mut self, key: impl Into<String>) -> Self {
        self.idempotency_key = Some(key.into());
        self
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CommitResponse {
    pub episode_id: Uuid,
    pub facts: Vec<CommittedFact>,
    /// The server masked a secret in a statement, an entity name or the content before storing it.
    /// False when absent, as from an older server.
    #[serde(default)]
    pub redacted: bool,
}

/// The fields after `status` default when absent, so a response from an older server still parses.
#[derive(Debug, Clone, Deserialize)]
pub struct CommittedFact {
    pub id: Uuid,
    pub status: FactStatus,
    /// No fact was created: `id` is an existing fact that gained this source.
    #[serde(default)]
    pub corroborated: bool,
    /// For a created fact, up to three current facts of the scope that resemble it, best first.
    #[serde(default)]
    pub related: Vec<RelatedFact>,
    /// With `corroborated`: the fact this request superseded, now closed with `id` as successor.
    /// On a created fact with a single-valued predicate, such as `lives_in`: the current fact it
    /// replaced, closed now if `status` is active and on approval if quarantined.
    #[serde(default)]
    pub superseded: Option<Uuid>,
}

/// A current fact that resembles a created fact without repeating it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RelatedFact {
    pub id: Uuid,
    pub statement: String,
    pub score: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RecallResponse {
    pub facts: Vec<ScoredFact>,
    /// Best section per page, of current versions only, ranked apart from facts. Empty when
    /// absent, as from an older server.
    #[serde(default)]
    pub pages: Vec<PageHit>,
    /// `time_travel` when `as_of` or `known_at` was set, since pages are searched at the present
    /// only.
    #[serde(default)]
    pub pages_skipped: Option<String>,
    /// Active insights with their fused scores, ranked apart from facts and pages. Empty when
    /// absent, as from an older server.
    #[serde(default)]
    pub insights: Vec<ScoredInsight>,
    /// `time_travel` when `as_of` or `known_at` was set, since insights are searched at the
    /// present only.
    #[serde(default)]
    pub insights_skipped: Option<String>,
}

/// Input to `MemoryOs::write_insight`.
#[derive(Debug, Clone, Serialize)]
pub struct InsightWrite {
    pub scope: Scope,
    pub level: InsightLevel,
    pub statement: String,
    /// 2 to 50 current facts, or active insights of a lower level, of `scope`.
    pub sources: Vec<InsightSourceRef>,
    /// An active or stale insight of `scope` that this one replaces.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<Uuid>,
}

impl InsightWrite {
    pub fn new(
        scope: Scope,
        level: InsightLevel,
        statement: impl Into<String>,
        sources: Vec<InsightSourceRef>,
    ) -> Self {
        Self {
            scope,
            level,
            statement: statement.into(),
            sources,
            supersedes: None,
        }
    }

    pub fn supersedes(mut self, insight_id: Uuid) -> Self {
        self.supersedes = Some(insight_id);
        self
    }
}

/// The insight an insight write stored, with its trust and status.
#[derive(Debug, Clone, Deserialize)]
pub struct InsightWriteResponse {
    pub insight: Insight,
    /// The server masked a secret in the statement.
    #[serde(default)]
    pub redacted: bool,
}

/// A page section that matched a recall query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PageHit {
    pub page_id: Uuid,
    pub scope: Scope,
    pub slug: String,
    pub title: String,
    pub version: i64,
    /// Heading path, such as `Upgrade MemoryOS > Native install`.
    pub section: String,
    /// The section's first 300 characters.
    pub snippet: String,
    pub score: f64,
}

/// Input to `MemoryOs::put_page`.
#[derive(Debug, Clone, Serialize)]
pub struct PageWrite {
    pub title: String,
    /// Markdown.
    pub body: String,
    /// What changed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// `None` creates the page. `Some(v)` edits current version `v`.
    pub expected_version: Option<i64>,
    /// Admin keys only: write and approve in one step.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub approve: bool,
}

impl PageWrite {
    pub fn create(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            summary: None,
            expected_version: None,
            approve: false,
        }
    }

    pub fn update(
        title: impl Into<String>,
        body: impl Into<String>,
        expected_version: i64,
    ) -> Self {
        Self {
            expected_version: Some(expected_version),
            ..Self::create(title, body)
        }
    }

    pub fn summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = Some(summary.into());
        self
    }

    /// Admin keys only. Forbidden when the current version's trust is above the key's `max_trust`.
    pub fn approve(mut self) -> Self {
        self.approve = true;
        self
    }
}

/// The version a page write stored, with its status.
#[derive(Debug, Clone, Deserialize)]
pub struct PageWriteResponse {
    #[serde(flatten)]
    pub version: PageVersion,
    /// The server masked a secret in the title, body or summary.
    pub redacted: bool,
}

/// Input to `MemoryOs::put_block`.
#[derive(Debug, Clone, Serialize)]
pub struct PutBlock {
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_chars: Option<i32>,
    pub expected_version: Option<i64>,
}

impl PutBlock {
    pub fn create(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            max_chars: None,
            expected_version: None,
        }
    }

    pub fn update(content: impl Into<String>, expected_version: i64) -> Self {
        Self {
            content: content.into(),
            max_chars: None,
            expected_version: Some(expected_version),
        }
    }

    pub fn max_chars(mut self, max_chars: i32) -> Self {
        self.max_chars = Some(max_chars);
        self
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn date_is_midnight_utc() {
        assert_eq!(date(2025, 1, 2).to_rfc3339(), "2025-01-02T00:00:00+00:00");
    }

    #[test]
    fn commit_omits_what_is_unset() {
        let commit = Commit::new(
            Scope::user("u"),
            "chat",
            "text",
            vec![CommitFact::new(
                "Ada likes tea",
                EntityRef::new("Ada", "person"),
                "likes",
            )],
        );
        assert_eq!(
            serde_json::to_value(&commit).expect("serializes"),
            json!({
                "scope": "user:u",
                "source": "chat",
                "content": "text",
                "facts": [{
                    "statement": "Ada likes tea",
                    "subject": {"name": "Ada", "kind": "person"},
                    "predicate": "likes",
                }],
            })
        );
    }

    #[test]
    fn commit_fact_sends_expires_at_when_set() {
        let fact = CommitFact::new("Ada is on leave", EntityRef::new("Ada", "person"), "is_on")
            .expires_at(date(2030, 1, 4));
        assert_eq!(
            serde_json::to_value(&fact).expect("serializes")["expires_at"],
            "2030-01-04T00:00:00Z"
        );
    }

    #[test]
    fn page_write_sends_only_what_is_set() {
        assert_eq!(
            serde_json::to_value(PageWrite::create("T", "# B")).expect("serializes"),
            json!({"title": "T", "body": "# B", "expected_version": null})
        );
        assert_eq!(
            serde_json::to_value(PageWrite::update("T", "B", 3).summary("why").approve())
                .expect("serializes"),
            json!({
                "title": "T",
                "body": "B",
                "summary": "why",
                "expected_version": 3,
                "approve": true,
            })
        );
    }

    #[test]
    fn page_write_response_is_a_flattened_version() {
        let id = Uuid::now_v7();
        let response: PageWriteResponse = serde_json::from_value(json!({
            "page_id": id,
            "scope": "shared:team",
            "slug": "runbook",
            "version": 2,
            "base_version": 1,
            "title": "Runbook",
            "summary": null,
            "trust": "agent",
            "status": "quarantined",
            "edited_by": "key:x",
            "edited_at": "2026-01-01T00:00:00Z",
            "redacted": true,
        }))
        .expect("parses");
        assert_eq!(response.version.page_id, id);
        assert_eq!(response.version.base_version, Some(1));
        assert_eq!(
            response.version.status,
            memoryos_core::PageVersionStatus::Quarantined
        );
        assert!(response.redacted);
    }

    #[test]
    fn recall_response_parses_with_and_without_pages() {
        let old: RecallResponse = serde_json::from_value(json!({"facts": []})).expect("old shape");
        assert!(old.pages.is_empty() && old.pages_skipped.is_none());

        let id = Uuid::now_v7();
        let new: RecallResponse = serde_json::from_value(json!({
            "facts": [],
            "pages": [{
                "page_id": id,
                "scope": "shared:team",
                "slug": "runbook",
                "title": "Runbook",
                "version": 4,
                "section": "Runbook > Restart",
                "snippet": "Stop the service first.",
                "score": 0.25,
            }],
            "pages_skipped": null,
        }))
        .expect("new shape");
        assert_eq!(new.pages[0].page_id, id);
        assert_eq!(new.pages[0].section, "Runbook > Restart");
        let skipped: RecallResponse = serde_json::from_value(
            json!({"facts": [], "pages": [], "pages_skipped": "time_travel"}),
        )
        .expect("skipped");
        assert_eq!(skipped.pages_skipped.as_deref(), Some("time_travel"));
    }

    #[test]
    fn recall_response_parses_with_and_without_insights() {
        let old: RecallResponse = serde_json::from_value(json!({"facts": []})).expect("old shape");
        assert!(old.insights.is_empty() && old.insights_skipped.is_none());

        let id = Uuid::now_v7();
        let source = Uuid::now_v7();
        let new: RecallResponse = serde_json::from_value(json!({
            "facts": [],
            "insights": [{
                "insight": {
                    "id": id,
                    "scope": "agent:a",
                    "level": "understanding",
                    "statement": "Ada prefers simple systems",
                    "trust": "agent",
                    "status": "active",
                    "stale_reason": null,
                    "sources": [{"kind": "fact", "id": source}],
                    "valid_from": "2026-01-01T00:00:00Z",
                    "valid_to": null,
                    "superseded_by": null,
                    "pending_supersede_of": null,
                    "written_by": "key:x",
                    "recorded_at": "2026-01-01T00:00:00Z",
                },
                "score": 0.03,
            }],
            "insights_skipped": null,
        }))
        .expect("new shape");
        assert_eq!(new.insights[0].insight.id, id);
        assert_eq!(new.insights[0].insight.level, InsightLevel::Understanding);
        assert_eq!(new.insights[0].insight.sources[0].id, source);
        let skipped: RecallResponse = serde_json::from_value(
            json!({"facts": [], "insights": [], "insights_skipped": "time_travel"}),
        )
        .expect("skipped");
        assert_eq!(skipped.insights_skipped.as_deref(), Some("time_travel"));
    }

    #[test]
    fn insight_write_sends_supersedes_only_when_set() {
        let id = Uuid::nil();
        let sources = vec![
            InsightSourceRef {
                kind: memoryos_core::InsightSourceKind::Fact,
                id,
            };
            2
        ];
        let write = InsightWrite::new(Scope::agent("a"), InsightLevel::Context, "s", sources);
        let value = serde_json::to_value(&write).expect("serializes");
        assert_eq!(value["level"], "context");
        assert_eq!(value["sources"][0]["kind"], "fact");
        assert!(value.get("supersedes").is_none());
        let value = serde_json::to_value(write.supersedes(id)).expect("serializes");
        assert_eq!(value["supersedes"], id.to_string());
    }

    #[test]
    fn committed_fact_parses_old_and_new_responses() {
        let id = Uuid::now_v7();
        let old: CommittedFact =
            serde_json::from_value(json!({"id": id, "status": "active"})).expect("old shape");
        assert!(!old.corroborated && old.related.is_empty() && old.superseded.is_none());

        let new: CommittedFact = serde_json::from_value(json!({
            "id": id,
            "status": "active",
            "corroborated": true,
            "superseded": id,
            "related": [{"id": id, "statement": "s", "score": 0.5}],
        }))
        .expect("new shape");
        assert!(new.corroborated);
        assert_eq!(new.superseded, Some(id));
        assert_eq!(
            new.related,
            [RelatedFact {
                id,
                statement: "s".into(),
                score: 0.5,
            }]
        );
    }
}
