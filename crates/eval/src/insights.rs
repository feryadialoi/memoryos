//! The insights suite (SPEC section 8, Insights): a scripted host writes insights at each level
//! citing facts, then drives every staleness event and checks that exactly the insights citing the
//! changed source went stale, each with the right change. It also checks the trust gate, confirm,
//! insights in recall, and the reflect batch.
//!
//! No model is called: every statement is scripted. Each case runs in its own fresh scope, so the
//! lazy expiry of one scope or the topics of another cannot interfere. The facts comparison and the
//! latency measurement share one agent scope holding the recall corpus and an insight per case.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use chrono::Utc;
use futures::stream::{self, StreamExt};
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::client::{
    CaseError, CommitFact, CommitResponse, Insight, InsightWrite, InsightWriter, RecallOptions,
    RecallResponse, ReflectBatch, SourceRef, StaleEntry, Trust,
};
use crate::dataset::{EntityRef, FactSpec, RecallCase, ScopeKind};
use crate::score::{contains_ci, percentile};
use crate::suites::{finish, run_all, CaseMeta, CaseResult, RunContext, Verdict};

pub const INSIGHTS_DATASET: &str = "insights.v1";

pub const CLASS_STALENESS: &str = "staleness";
pub const CLASS_TRUST: &str = "trust";
pub const CLASS_CONFIRM: &str = "confirm";
pub const CLASS_RECALL: &str = "recall";
pub const CLASS_REFLECT: &str = "reflect";
pub const CLASS_FACTS: &str = "facts_unchanged";

/// The server default, asked for explicitly so the request states what is measured.
const INSIGHT_LIMIT: u32 = 3;
const FACT_LIMIT: u32 = 10;
/// Every insight a probe scope holds fits under this.
const ALL_INSIGHTS: u32 = 10;
/// How far ahead the expiring fact's `expires_at` is set.
const EXPIRY_SECS: i64 = 3;
/// Most items a reflect batch may return (SPEC section 8).
const BATCH_MAX: u32 = 20;
const HOST_SOURCE: &str = "eval-host";
const EXTERNAL_SOURCE: &str = "eval-external";

const ACTIVE: &str = "active";
const STALE: &str = "stale";
const QUARANTINED: &str = "quarantined";
const REJECTED: &str = "rejected";
const RETIRED: &str = "retired";
const SUPERSEDED: &str = "superseded";

const CONTEXT: &str = "context";
const UNDERSTANDING: &str = "understanding";
const GUIDANCE: &str = "guidance";

/// Facts a reflect topic case can commit on one subject, so `--reflect-min-facts` is at most this
/// minus one: the decline case adds one more fact past the threshold.
pub const TOPIC_FACTS: [&str; 9] = [
    "bakes rye bread every morning",
    "sells sourdough on Saturdays",
    "closes at three on Sundays",
    "sources flour from a mill in Kent",
    "runs a bread class for children",
    "gives day-old loaves to the shelter",
    "takes orders by phone only",
    "roasts its own coffee beans",
    "hires apprentices every spring",
];

/// Everything one run of the suite produced.
pub struct InsightsRun {
    pub cases: Vec<CaseResult<InsightDetail>>,
    pub facts: Vec<CaseResult<FactsDetail>>,
    /// Insights written over the recall corpus for the facts comparison and the timings.
    pub corpus_insights: usize,
    pub latency: Option<Latency>,
}

// Scored cases

#[derive(Debug, Default, Serialize)]
pub struct InsightDetail {
    /// Comparisons made against what SPEC section 8 requires.
    pub checks: usize,
    pub problems: Vec<String>,
    /// Set when every step ran, so a case cut short gets no credit.
    pub completed: bool,
}

impl InsightDetail {
    fn check(&mut self, ok: bool, problem: impl FnOnce() -> String) {
        self.checks += 1;
        if !ok {
            self.problems.push(problem());
        }
    }

    /// One check made of several findings, each prefixed with what was looked at.
    fn record(&mut self, label: &str, problems: Vec<String>) {
        self.checks += 1;
        self.problems
            .extend(problems.into_iter().map(|p| format!("{label}: {p}")));
    }
}

impl Verdict for InsightDetail {
    fn problems(&self) -> Vec<String> {
        let mut problems = self.problems.clone();
        if !self.completed && problems.is_empty() {
            problems.push("case did not finish".to_owned());
        }
        problems
    }
}

fn short(id: Uuid) -> String {
    id.simple().to_string()[..8].to_owned()
}

fn describe(entry: &StaleEntry) -> String {
    let mut text = format!("{} {} {}", entry.kind, short(entry.id), entry.change);
    if let Some(by) = entry.by {
        text.push_str(&format!(" by {}", short(by)));
    }
    if let Some(status) = &entry.status {
        text.push_str(&format!(" ({status})"));
    }
    text
}

fn describe_all(entries: &[StaleEntry]) -> String {
    if entries.is_empty() {
        return "none".to_owned();
    }
    entries.iter().map(describe).collect::<Vec<_>>().join(", ")
}

pub fn superseded(fact: Uuid, by: Uuid) -> StaleEntry {
    StaleEntry {
        kind: "fact".to_owned(),
        id: fact,
        change: "superseded".to_owned(),
        by: Some(by),
        status: None,
    }
}

/// A fact source changed with `invalidated`, `expired` or `purged`.
pub fn fact_changed(fact: Uuid, change: &str) -> StaleEntry {
    StaleEntry {
        kind: "fact".to_owned(),
        id: fact,
        change: change.to_owned(),
        by: None,
        status: None,
    }
}

pub fn insight_changed(insight: Uuid, status: &str) -> StaleEntry {
    StaleEntry {
        kind: "insight".to_owned(),
        id: insight,
        change: "insight_changed".to_owned(),
        by: None,
        status: Some(status.to_owned()),
    }
}

/// How `insight` differs from the expected status and stale reason. SPEC section 8: `stale_reason`
/// lists each changed source and is set exactly while the status is `stale`. Pure, so the rules
/// are unit tested.
pub fn state_problems(insight: &Insight, status: &str, reason: &[StaleEntry]) -> Vec<String> {
    let mut problems = Vec::new();
    if insight.status != status {
        problems.push(format!("status {}, expected {status}", insight.status));
    }
    match (&insight.stale_reason, insight.status == STALE) {
        (None, true) => problems.push("stale without a stale_reason".to_owned()),
        (Some(actual), false) => problems.push(format!(
            "stale_reason set while {}: {}",
            insight.status,
            describe_all(actual)
        )),
        _ => {}
    }
    if status == STALE {
        let actual = insight.stale_reason.as_deref().unwrap_or_default();
        let actual_set: HashSet<&StaleEntry> = actual.iter().collect();
        let expected_set: HashSet<&StaleEntry> = reason.iter().collect();
        if actual_set != expected_set || actual.len() != actual_set.len() {
            problems.push(format!(
                "stale_reason is [{}], expected [{}]",
                describe_all(actual),
                describe_all(reason)
            ));
        }
    }
    problems
}

/// Hex SHA-256 of the sorted fact ids, each hyphenated and followed by a newline, as the reflect
/// batch computes `fact_set`.
pub fn fact_set_digest(ids: &[Uuid]) -> String {
    let mut sorted = ids.to_vec();
    sorted.sort_unstable();
    let mut hasher = Sha256::new();
    for id in sorted {
        hasher.update(id.hyphenated().to_string().as_bytes());
        hasher.update(b"\n");
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Problems with the insights of a recall response against the ids expected, in any order.
pub fn recalled_problems(response: &RecallResponse, expected: &HashSet<Uuid>) -> Vec<String> {
    let mut problems = Vec::new();
    for scored in &response.insights {
        if scored.insight.status != ACTIVE {
            problems.push(format!(
                "recall returned insight {} with status {}",
                short(scored.insight.id),
                scored.insight.status
            ));
        }
    }
    let got: HashSet<Uuid> = response.insights.iter().map(|s| s.insight.id).collect();
    for id in got.difference(expected) {
        problems.push(format!("recall returned insight {}", short(*id)));
    }
    for id in expected.difference(&got) {
        problems.push(format!("recall missed active insight {}", short(*id)));
    }
    problems
}

// The scripted cases

#[derive(Debug, Clone, Copy)]
enum Scenario {
    SupersedeByCommit,
    SingleValuedReplace,
    ApprovalClosesHead,
    Invalidate,
    PurgeDelete,
    PurgeReopen,
    PurgeRelink,
    PurgeRequarantine,
    RetireCascade,
    InsightSupersede,
    Expiry,
    UnrelatedChanges,
    GuidanceOutsideAgentScope,
    ExternalWriter,
    ExternalSource,
    WriteRules,
    Confirm,
    RecallActiveOnly,
    RecallLimit,
    RecallTimeTravel,
    ReflectThreshold,
    ReflectDecline,
    ReflectStaleFirst,
}

struct CaseSpec {
    id: &'static str,
    class: &'static str,
    kind: ScopeKind,
    scenario: Scenario,
}

const CASES: [CaseSpec; 23] = [
    CaseSpec {
        id: "supersede-by-commit",
        class: CLASS_STALENESS,
        kind: ScopeKind::Agent,
        scenario: Scenario::SupersedeByCommit,
    },
    CaseSpec {
        id: "single-valued-replace",
        class: CLASS_STALENESS,
        kind: ScopeKind::Agent,
        scenario: Scenario::SingleValuedReplace,
    },
    CaseSpec {
        id: "approval-closes-head",
        class: CLASS_STALENESS,
        kind: ScopeKind::Agent,
        scenario: Scenario::ApprovalClosesHead,
    },
    CaseSpec {
        id: "invalidate",
        class: CLASS_STALENESS,
        kind: ScopeKind::Agent,
        scenario: Scenario::Invalidate,
    },
    CaseSpec {
        id: "purge-delete",
        class: CLASS_STALENESS,
        kind: ScopeKind::Agent,
        scenario: Scenario::PurgeDelete,
    },
    CaseSpec {
        id: "purge-reopen",
        class: CLASS_STALENESS,
        kind: ScopeKind::Agent,
        scenario: Scenario::PurgeReopen,
    },
    CaseSpec {
        id: "purge-relink",
        class: CLASS_STALENESS,
        kind: ScopeKind::Agent,
        scenario: Scenario::PurgeRelink,
    },
    CaseSpec {
        id: "purge-requarantine",
        class: CLASS_STALENESS,
        kind: ScopeKind::Agent,
        scenario: Scenario::PurgeRequarantine,
    },
    CaseSpec {
        id: "retire-cascade",
        class: CLASS_STALENESS,
        kind: ScopeKind::Agent,
        scenario: Scenario::RetireCascade,
    },
    CaseSpec {
        id: "insight-supersede",
        class: CLASS_STALENESS,
        kind: ScopeKind::Agent,
        scenario: Scenario::InsightSupersede,
    },
    CaseSpec {
        id: "lazy-expiry",
        class: CLASS_STALENESS,
        kind: ScopeKind::Agent,
        scenario: Scenario::Expiry,
    },
    CaseSpec {
        id: "unrelated-changes",
        class: CLASS_STALENESS,
        kind: ScopeKind::Agent,
        scenario: Scenario::UnrelatedChanges,
    },
    CaseSpec {
        id: "guidance-outside-agent-scope",
        class: CLASS_TRUST,
        kind: ScopeKind::Shared,
        scenario: Scenario::GuidanceOutsideAgentScope,
    },
    CaseSpec {
        id: "external-writer",
        class: CLASS_TRUST,
        kind: ScopeKind::Agent,
        scenario: Scenario::ExternalWriter,
    },
    CaseSpec {
        id: "external-source",
        class: CLASS_TRUST,
        kind: ScopeKind::Agent,
        scenario: Scenario::ExternalSource,
    },
    CaseSpec {
        id: "write-rules",
        class: CLASS_TRUST,
        kind: ScopeKind::Agent,
        scenario: Scenario::WriteRules,
    },
    CaseSpec {
        id: "confirm",
        class: CLASS_CONFIRM,
        kind: ScopeKind::Agent,
        scenario: Scenario::Confirm,
    },
    CaseSpec {
        id: "recall-active-only",
        class: CLASS_RECALL,
        kind: ScopeKind::Agent,
        scenario: Scenario::RecallActiveOnly,
    },
    CaseSpec {
        id: "recall-insight-limit",
        class: CLASS_RECALL,
        kind: ScopeKind::Agent,
        scenario: Scenario::RecallLimit,
    },
    CaseSpec {
        id: "recall-time-travel",
        class: CLASS_RECALL,
        kind: ScopeKind::Agent,
        scenario: Scenario::RecallTimeTravel,
    },
    CaseSpec {
        id: "reflect-threshold",
        class: CLASS_REFLECT,
        kind: ScopeKind::Agent,
        scenario: Scenario::ReflectThreshold,
    },
    CaseSpec {
        id: "reflect-decline",
        class: CLASS_REFLECT,
        kind: ScopeKind::Agent,
        scenario: Scenario::ReflectDecline,
    },
    CaseSpec {
        id: "reflect-stale-first",
        class: CLASS_REFLECT,
        kind: ScopeKind::Agent,
        scenario: Scenario::ReflectStaleFirst,
    },
];

/// The ids of the scripted cases, in run order.
pub fn case_ids() -> Vec<&'static str> {
    CASES.iter().map(|case| case.id).collect()
}

/// One case's place to work: its own scope, and labels unique within the case.
struct Probe<'a> {
    ctx: &'a RunContext,
    case: &'a str,
    scope: &'a str,
    kind: ScopeKind,
    reflect_min_facts: usize,
}

/// A committed fact and the episode it came from.
#[derive(Debug, Clone, Copy)]
struct Seeded {
    fact: Uuid,
    episode: Uuid,
}

fn spec(subject: (&str, &str), statement: &str, predicate: &str) -> FactSpec {
    FactSpec {
        statement: statement.to_owned(),
        subject: EntityRef {
            name: subject.0.to_owned(),
            kind: subject.1.to_owned(),
        },
        predicate: predicate.to_owned(),
        valid_from: None,
    }
}

fn facts(ids: &[Uuid]) -> Vec<SourceRef> {
    ids.iter().copied().map(SourceRef::fact).collect()
}

fn first_fact(committed: &CommitResponse) -> Result<Uuid, CaseError> {
    committed
        .facts
        .first()
        .map(|fact| fact.id)
        .ok_or_else(|| CaseError::Earlier("commit returned no facts".to_owned()))
}

impl Probe<'_> {
    /// Commits one fact as the host and gets it active, approving it where the scope holds `agent`
    /// trust for review.
    async fn seed(
        &self,
        label: &str,
        subject: (&str, &str),
        statement: &str,
        predicate: &str,
        detail: &mut InsightDetail,
    ) -> Result<Seeded, CaseError> {
        let spec = spec(subject, statement, predicate);
        let committed = self
            .ctx
            .establish(
                (self.case, label),
                self.kind,
                (self.scope, statement),
                vec![CommitFact::new(&spec, None)],
                &mut detail.problems,
            )
            .await?;
        Ok(Seeded {
            fact: first_fact(&committed)?,
            episode: committed.episode_id,
        })
    }

    /// Several facts on one subject, one commit each, so each has its own episode.
    async fn seed_all(
        &self,
        prefix: &str,
        subject: (&str, &str),
        statements: &[(&str, &str)],
        detail: &mut InsightDetail,
    ) -> Result<Vec<Seeded>, CaseError> {
        let mut seeded = Vec::with_capacity(statements.len());
        for (index, (statement, predicate)) in statements.iter().enumerate() {
            seeded.push(
                self.seed(
                    &format!("{prefix}{index}"),
                    subject,
                    statement,
                    predicate,
                    detail,
                )
                .await?,
            );
        }
        Ok(seeded)
    }

    /// A commit that is not simply established: a supersede, a corroboration, an external write or
    /// an expiring fact. Its status is the caller's to check.
    async fn commit(
        &self,
        trust: Trust,
        label: &str,
        fact: CommitFact<'_>,
    ) -> Result<CommitResponse, CaseError> {
        let source = if trust == Trust::External {
            EXTERNAL_SOURCE
        } else {
            HOST_SOURCE
        };
        let content = fact.statement.to_owned();
        self.ctx
            .commit(
                trust,
                (self.case, label),
                (self.scope, source, &content),
                vec![fact],
            )
            .await
    }

    /// Supersedes `old` with a new statement as the host, expecting the new fact active.
    async fn supersede(
        &self,
        label: &str,
        old: Uuid,
        fact: &FactSpec,
        detail: &mut InsightDetail,
    ) -> Result<Seeded, CaseError> {
        let committed = self
            .commit(Trust::Agent, label, CommitFact::new(fact, Some(old)))
            .await?;
        let new = committed
            .facts
            .first()
            .ok_or_else(|| CaseError::Earlier("commit returned no facts".to_owned()))?;
        detail.check(new.status == ACTIVE, || {
            format!("{label}: superseding fact has status {}", new.status)
        });
        Ok(Seeded {
            fact: new.id,
            episode: committed.episode_id,
        })
    }

    /// Writes an insight and checks the status it was given.
    #[allow(clippy::too_many_arguments)]
    async fn write_as(
        &self,
        writer: InsightWriter,
        level: &str,
        statement: &str,
        sources: Vec<SourceRef>,
        supersedes: Option<Uuid>,
        expected: &str,
        detail: &mut InsightDetail,
    ) -> Result<Insight, CaseError> {
        let response = self
            .ctx
            .client
            .write_insight(
                writer,
                &InsightWrite {
                    scope: self.scope,
                    level,
                    statement,
                    sources,
                    supersedes,
                },
            )
            .await?;
        let insight = response.insight;
        detail.check(insight.status == expected, || {
            format!(
                "{level} insight {} written with status {}, expected {expected}",
                short(insight.id),
                insight.status
            )
        });
        Ok(insight)
    }

    /// The host writes an insight in an agent scope, where it goes active.
    async fn write(
        &self,
        level: &str,
        statement: &str,
        sources: Vec<SourceRef>,
        detail: &mut InsightDetail,
    ) -> Result<Insight, CaseError> {
        self.write_as(
            InsightWriter::Agent,
            level,
            statement,
            sources,
            None,
            ACTIVE,
            detail,
        )
        .await
    }

    /// Reads an insight and compares its status and stale reason.
    async fn expect(
        &self,
        label: &str,
        id: Uuid,
        status: &str,
        reason: &[StaleEntry],
        detail: &mut InsightDetail,
    ) -> Result<Insight, CaseError> {
        let insight = self.ctx.client.get_insight(id).await?;
        detail.record(label, state_problems(&insight, status, reason));
        Ok(insight)
    }

    /// A write the server must refuse with 400.
    async fn refused(
        &self,
        label: &str,
        level: &str,
        sources: Vec<SourceRef>,
        detail: &mut InsightDetail,
    ) -> Result<(), CaseError> {
        let outcome = self
            .ctx
            .client
            .write_insight(
                InsightWriter::Agent,
                &InsightWrite {
                    scope: self.scope,
                    level,
                    statement: label,
                    sources,
                    supersedes: None,
                },
            )
            .await;
        match outcome {
            Err(CaseError::Status { status: 400, .. }) => {
                detail.check(true, String::new);
                Ok(())
            }
            Err(other) => Err(other),
            Ok(written) => {
                detail.check(false, || {
                    format!(
                        "{label}: accepted with status {}, expected 400",
                        written.insight.status
                    )
                });
                Ok(())
            }
        }
    }

    async fn recall(
        &self,
        query: &str,
        options: RecallOptions,
    ) -> Result<RecallResponse, CaseError> {
        self.ctx
            .client
            .recall_with(self.scope, query, FACT_LIMIT, options)
            .await
    }
}

pub async fn run_insights(
    ctx: &RunContext,
    recall_cases: &[RecallCase],
    concurrency: usize,
    latency_calls: usize,
    reflect_min_facts: usize,
) -> InsightsRun {
    let cases = run_all(&CASES, concurrency, |case| async move {
        let meta = CaseMeta {
            id: case.id,
            lang: "en",
            class: Some(case.class.to_owned()),
            scope: ctx.scope(case.kind, case.id),
            started: Instant::now(),
        };
        let probe = Probe {
            ctx,
            case: case.id,
            scope: &meta.scope,
            kind: case.kind,
            reflect_min_facts,
        };
        let mut detail = InsightDetail::default();
        let outcome = run_scenario(&probe, case.scenario, &mut detail).await;
        if outcome.is_ok() {
            detail.completed = true;
        }
        finish(meta, outcome, detail)
    })
    .await;

    let scope = ctx.scope(ScopeKind::Agent, "insights-recall");
    let corpus = build_corpus(ctx, recall_cases, &scope, concurrency).await;
    // One at a time: the comparison should not depend on how busy the reranker is.
    let facts = run_all(recall_cases, 1, |case| {
        let scope = scope.as_str();
        let corpus = &corpus;
        async move {
            let meta = CaseMeta {
                id: &case.id,
                lang: &case.lang,
                class: Some(CLASS_FACTS.to_owned()),
                scope: scope.to_owned(),
                started: Instant::now(),
            };
            let mut detail = FactsDetail::default();
            let outcome = match corpus {
                Err(error) => Err(CaseError::Earlier(error.clone())),
                Ok(_) => compare(ctx, &case.query, scope, &mut detail).await,
            };
            finish(meta, outcome, detail)
        }
    })
    .await;

    let latency = match (&corpus, latency_calls) {
        (Ok(_), calls) if calls > 0 => {
            let queries: Vec<&str> = recall_cases.iter().map(|c| c.query.as_str()).collect();
            Some(measure_latency(ctx, &scope, &queries, calls).await)
        }
        _ => None,
    };

    InsightsRun {
        cases,
        facts,
        corpus_insights: corpus.unwrap_or_default(),
        latency,
    }
}

async fn run_scenario(
    probe: &Probe<'_>,
    scenario: Scenario,
    detail: &mut InsightDetail,
) -> Result<(), CaseError> {
    match scenario {
        Scenario::SupersedeByCommit => supersede_by_commit(probe, detail).await,
        Scenario::SingleValuedReplace => single_valued_replace(probe, detail).await,
        Scenario::ApprovalClosesHead => approval_closes_head(probe, detail).await,
        Scenario::Invalidate => invalidate(probe, detail).await,
        Scenario::PurgeDelete => purge_delete(probe, detail).await,
        Scenario::PurgeReopen => purge_reopen(probe, detail).await,
        Scenario::PurgeRelink => purge_relink(probe, detail).await,
        Scenario::PurgeRequarantine => purge_requarantine(probe, detail).await,
        Scenario::RetireCascade => retire_cascade(probe, detail).await,
        Scenario::InsightSupersede => insight_supersede(probe, detail).await,
        Scenario::Expiry => lazy_expiry(probe, detail).await,
        Scenario::UnrelatedChanges => unrelated_changes(probe, detail).await,
        Scenario::GuidanceOutsideAgentScope => guidance_outside_agent_scope(probe, detail).await,
        Scenario::ExternalWriter => external_writer(probe, detail).await,
        Scenario::ExternalSource => external_source(probe, detail).await,
        Scenario::WriteRules => write_rules(probe, detail).await,
        Scenario::Confirm => confirm(probe, detail).await,
        Scenario::RecallActiveOnly => recall_active_only(probe, detail).await,
        Scenario::RecallLimit => recall_limit(probe, detail).await,
        Scenario::RecallTimeTravel => recall_time_travel(probe, detail).await,
        Scenario::ReflectThreshold => reflect_threshold(probe, detail).await,
        Scenario::ReflectDecline => reflect_decline(probe, detail).await,
        Scenario::ReflectStaleFirst => reflect_stale_first(probe, detail).await,
    }
}

// Staleness

/// A commit supersedes a cited fact: its context goes stale naming the successor, and the change
/// climbs through understanding to guidance. An insight on other facts stays active.
async fn supersede_by_commit(p: &Probe<'_>, d: &mut InsightDetail) -> Result<(), CaseError> {
    let who = ("Rhea Calloway", "person");
    let f = p
        .seed_all(
            "f",
            who,
            &[
                ("Rhea Calloway writes services in Go", "writes_services_in"),
                ("Rhea Calloway runs the payments queue on Kafka", "operates"),
                (
                    "Rhea Calloway deploys to Kubernetes every week",
                    "deploys_to",
                ),
                (
                    "Rhea Calloway asks for failure modes in design reviews",
                    "asks_for",
                ),
            ],
            d,
        )
        .await?;
    let context = p
        .write(
            CONTEXT,
            "Rhea Calloway is a backend engineer on streaming payment systems.",
            facts(&[f[0].fact, f[1].fact]),
            d,
        )
        .await?;
    let control = p
        .write(
            CONTEXT,
            "Rhea Calloway ships often and reviews designs for failure handling.",
            facts(&[f[2].fact, f[3].fact]),
            d,
        )
        .await?;
    let understanding = p
        .write(
            UNDERSTANDING,
            "Rhea Calloway values operational simplicity in distributed systems.",
            vec![SourceRef::insight(context.id), SourceRef::fact(f[2].fact)],
            d,
        )
        .await?;
    let guidance = p
        .write(
            GUIDANCE,
            "With Rhea Calloway, cover failure modes before proposing a new abstraction.",
            vec![
                SourceRef::insight(understanding.id),
                SourceRef::fact(f[3].fact),
            ],
            d,
        )
        .await?;

    let successor = spec(
        who,
        "Rhea Calloway writes services in Rust",
        "writes_services_in",
    );
    let new = p.supersede("successor", f[0].fact, &successor, d).await?;

    p.expect(
        "context",
        context.id,
        STALE,
        &[superseded(f[0].fact, new.fact)],
        d,
    )
    .await?;
    p.expect(
        "understanding",
        understanding.id,
        STALE,
        &[insight_changed(context.id, STALE)],
        d,
    )
    .await?;
    p.expect(
        "guidance",
        guidance.id,
        STALE,
        &[insight_changed(understanding.id, STALE)],
        d,
    )
    .await?;
    p.expect("control", control.id, ACTIVE, &[], d).await?;
    Ok(())
}

/// A new value of a single-valued predicate closes the old fact without `supersedes`.
async fn single_valued_replace(p: &Probe<'_>, d: &mut InsightDetail) -> Result<(), CaseError> {
    let who = ("Mira Okafor", "person");
    let f = p
        .seed_all(
            "f",
            who,
            &[
                ("Mira Okafor lives in Porto", "lives_in"),
                ("Mira Okafor cycles to work every day", "commutes_by"),
            ],
            d,
        )
        .await?;
    let insight = p
        .write(
            CONTEXT,
            "Mira Okafor lives in a compact city and commutes by bike.",
            facts(&[f[0].fact, f[1].fact]),
            d,
        )
        .await?;
    let moved = spec(who, "Mira Okafor lives in Lisbon", "lives_in");
    let committed = p
        .commit(Trust::Agent, "moved", CommitFact::new(&moved, None))
        .await?;
    let new = committed
        .facts
        .first()
        .ok_or_else(|| CaseError::Earlier("commit returned no facts".to_owned()))?;
    d.check(new.superseded == Some(f[0].fact), || {
        format!(
            "single-valued commit closed {:?}, expected {}",
            new.superseded.map(short),
            short(f[0].fact)
        )
    });
    p.expect(
        "insight",
        insight.id,
        STALE,
        &[superseded(f[0].fact, new.id)],
        d,
    )
    .await?;
    Ok(())
}

/// An external supersede waits for review and leaves the insight active; approval closes the head
/// and makes it stale.
async fn approval_closes_head(p: &Probe<'_>, d: &mut InsightDetail) -> Result<(), CaseError> {
    let who = ("Tomas Reyes", "person");
    let f = p
        .seed_all(
            "f",
            who,
            &[
                (
                    "Tomas Reyes keeps the release calendar in Notion",
                    "keeps_calendar_in",
                ),
                ("Tomas Reyes cuts releases on Thursdays", "releases_on"),
            ],
            d,
        )
        .await?;
    let insight = p
        .write(
            CONTEXT,
            "Tomas Reyes runs a weekly release train planned in Notion.",
            facts(&[f[0].fact, f[1].fact]),
            d,
        )
        .await?;
    let claim = spec(
        who,
        "Tomas Reyes keeps the release calendar in Linear",
        "keeps_calendar_in",
    );
    let committed = p
        .commit(
            Trust::External,
            "external-supersede",
            CommitFact::new(&claim, Some(f[0].fact)),
        )
        .await?;
    let pending = committed
        .facts
        .first()
        .ok_or_else(|| CaseError::Earlier("commit returned no facts".to_owned()))?;
    d.check(pending.status == QUARANTINED, || {
        format!("external supersede has status {}", pending.status)
    });
    p.expect("while held", insight.id, ACTIVE, &[], d).await?;
    p.ctx.client.approve(pending.id).await?;
    p.expect(
        "after approval",
        insight.id,
        STALE,
        &[superseded(f[0].fact, pending.id)],
        d,
    )
    .await?;
    Ok(())
}

async fn invalidate(p: &Probe<'_>, d: &mut InsightDetail) -> Result<(), CaseError> {
    let who = ("Lena Fischer", "person");
    let f = p
        .seed_all(
            "f",
            who,
            &[
                ("Lena Fischer is allergic to peanuts", "allergic_to"),
                ("Lena Fischer brings lunch from home", "brings"),
                ("Lena Fischer cooks for the week on Sundays", "cooks_on"),
            ],
            d,
        )
        .await?;
    let insight = p
        .write(
            CONTEXT,
            "Lena Fischer avoids catered food because of a peanut allergy.",
            facts(&[f[0].fact, f[1].fact]),
            d,
        )
        .await?;
    let control = p
        .write(
            CONTEXT,
            "Lena Fischer plans her meals ahead.",
            facts(&[f[1].fact, f[2].fact]),
            d,
        )
        .await?;
    p.ctx
        .client
        .invalidate_fact(f[0].fact, "recorded for the wrong person")
        .await?;
    p.expect(
        "insight",
        insight.id,
        STALE,
        &[fact_changed(f[0].fact, "invalidated")],
        d,
    )
    .await?;
    p.expect("control", control.id, ACTIVE, &[], d).await?;
    Ok(())
}

/// Purging the only source of a cited fact deletes it.
async fn purge_delete(p: &Probe<'_>, d: &mut InsightDetail) -> Result<(), CaseError> {
    let who = ("Omar Haddad", "person");
    let f = p
        .seed_all(
            "f",
            who,
            &[
                ("Omar Haddad maintains the billing service", "maintains"),
                ("Omar Haddad is on call this week", "on_call"),
                ("Omar Haddad reviews billing pull requests", "reviews"),
            ],
            d,
        )
        .await?;
    let insight = p
        .write(
            CONTEXT,
            "Omar Haddad owns billing and answers its pages.",
            facts(&[f[0].fact, f[1].fact]),
            d,
        )
        .await?;
    let control = p
        .write(
            CONTEXT,
            "Omar Haddad is the person to ask about billing this week.",
            facts(&[f[1].fact, f[2].fact]),
            d,
        )
        .await?;
    let report = p.ctx.client.purge_episode(f[0].episode).await?;
    d.check(report.facts_deleted >= 1, || {
        "purge reported no deleted fact".to_owned()
    });
    p.expect(
        "insight",
        insight.id,
        STALE,
        &[fact_changed(f[0].fact, "purged")],
        d,
    )
    .await?;
    p.expect("control", control.id, ACTIVE, &[], d).await?;
    Ok(())
}

/// Purging the successor reopens the old fact. Its insight keeps the entry it already has, and an
/// insight citing the deleted successor goes stale with `purged`.
async fn purge_reopen(p: &Probe<'_>, d: &mut InsightDetail) -> Result<(), CaseError> {
    let who = ("Priya Nair", "person");
    let f = p
        .seed_all(
            "f",
            who,
            &[
                ("Priya Nair ships releases on Fridays", "ships_on"),
                ("Priya Nair tags every release in git", "tags"),
            ],
            d,
        )
        .await?;
    let first = p
        .write(
            CONTEXT,
            "Priya Nair runs a Friday release with tagged builds.",
            facts(&[f[0].fact, f[1].fact]),
            d,
        )
        .await?;
    let later = spec(who, "Priya Nair ships releases on Tuesdays", "ships_on");
    let successor = p.supersede("successor", f[0].fact, &later, d).await?;
    let reason = [superseded(f[0].fact, successor.fact)];
    p.expect("first, after the supersede", first.id, STALE, &reason, d)
        .await?;
    let second = p
        .write(
            CONTEXT,
            "Priya Nair runs a Tuesday release with tagged builds.",
            facts(&[successor.fact, f[1].fact]),
            d,
        )
        .await?;

    p.ctx.client.purge_episode(successor.episode).await?;
    p.expect(
        "second, citing the purged fact",
        second.id,
        STALE,
        &[fact_changed(successor.fact, "purged")],
        d,
    )
    .await?;
    p.expect("first, after the purge", first.id, STALE, &reason, d)
        .await?;
    let reopened = p.ctx.client.get_fact(f[0].fact).await?;
    d.check(
        reopened.valid_to.is_none() && reopened.superseded_by.is_none(),
        || "purging the successor did not reopen the old fact".to_owned(),
    );
    Ok(())
}

/// In a chain A, B, C, purging B relinks A to C, and the insight on A names C.
async fn purge_relink(p: &Probe<'_>, d: &mut InsightDetail) -> Result<(), CaseError> {
    let who = ("Jonas Berg", "person");
    let f = p
        .seed_all(
            "f",
            who,
            &[
                ("Jonas Berg drinks espresso at work", "drinks"),
                ("Jonas Berg works from the harbour office", "works_from"),
            ],
            d,
        )
        .await?;
    let insight = p
        .write(
            CONTEXT,
            "Jonas Berg keeps an office routine built around coffee.",
            facts(&[f[0].fact, f[1].fact]),
            d,
        )
        .await?;
    let b = p
        .supersede(
            "b",
            f[0].fact,
            &spec(who, "Jonas Berg drinks green tea at work", "drinks"),
            d,
        )
        .await?;
    let c = p
        .supersede(
            "c",
            b.fact,
            &spec(who, "Jonas Berg drinks mint tea at work", "drinks"),
            d,
        )
        .await?;
    p.expect(
        "before the purge",
        insight.id,
        STALE,
        &[superseded(f[0].fact, b.fact)],
        d,
    )
    .await?;
    p.ctx.client.purge_episode(b.episode).await?;
    p.expect(
        "after the purge",
        insight.id,
        STALE,
        &[superseded(f[0].fact, c.fact)],
        d,
    )
    .await?;
    let relinked = p.ctx.client.get_fact(f[0].fact).await?;
    d.check(relinked.superseded_by == Some(c.fact), || {
        format!(
            "old fact superseded_by {:?} after the purge, expected {}",
            relinked.superseded_by.map(short),
            short(c.fact)
        )
    });
    Ok(())
}

/// An external corroboration changes nothing. Purging the host's source leaves the fact with only
/// external trust, which quarantines it again, so the insight goes stale with `purged`.
async fn purge_requarantine(p: &Probe<'_>, d: &mut InsightDetail) -> Result<(), CaseError> {
    let who = ("Ana Silva", "person");
    let f = p
        .seed_all(
            "f",
            who,
            &[
                ("Ana Silva speaks Portuguese at home", "speaks"),
                ("Ana Silva teaches evening classes", "teaches"),
            ],
            d,
        )
        .await?;
    let insight = p
        .write(
            CONTEXT,
            "Ana Silva is a Portuguese speaking teacher.",
            facts(&[f[0].fact, f[1].fact]),
            d,
        )
        .await?;
    let repeat = spec(who, "Ana Silva speaks Portuguese at home", "speaks");
    let mut fact = CommitFact::new(&repeat, None);
    fact.corroborates = Some(f[0].fact);
    p.commit(Trust::External, "external-corroboration", fact)
        .await?;
    p.expect("after corroboration", insight.id, ACTIVE, &[], d)
        .await?;

    p.ctx.client.purge_episode(f[0].episode).await?;
    let left = p.ctx.client.get_fact(f[0].fact).await?;
    d.check(
        left.status == QUARANTINED && left.trust == Trust::External,
        || {
            format!(
                "fact left with status {} and trust {:?}, expected quarantined and External",
                left.status, left.trust
            )
        },
    );
    p.expect(
        "after the purge",
        insight.id,
        STALE,
        &[fact_changed(f[0].fact, "purged")],
        d,
    )
    .await?;
    Ok(())
}

/// Retiring a context makes the understanding on it stale, and the guidance above that.
async fn retire_cascade(p: &Probe<'_>, d: &mut InsightDetail) -> Result<(), CaseError> {
    let who = ("Kenji Watanabe", "person");
    let f = p
        .seed_all(
            "f",
            who,
            &[
                ("Kenji Watanabe leads the mobile team", "leads"),
                (
                    "Kenji Watanabe writes the iOS app in Swift",
                    "writes_app_in",
                ),
                (
                    "Kenji Watanabe asks for screenshots in bug reports",
                    "asks_for",
                ),
                (
                    "Kenji Watanabe reviews pull requests in the morning",
                    "reviews_in",
                ),
            ],
            d,
        )
        .await?;
    let context = p
        .write(
            CONTEXT,
            "Kenji Watanabe is the lead of a Swift mobile team.",
            facts(&[f[0].fact, f[1].fact]),
            d,
        )
        .await?;
    let understanding = p
        .write(
            UNDERSTANDING,
            "Kenji Watanabe works visually and wants evidence he can see.",
            vec![SourceRef::insight(context.id), SourceRef::fact(f[2].fact)],
            d,
        )
        .await?;
    let guidance = p
        .write(
            GUIDANCE,
            "Send Kenji Watanabe screenshots with bug reports, before noon.",
            vec![
                SourceRef::insight(understanding.id),
                SourceRef::fact(f[3].fact),
            ],
            d,
        )
        .await?;
    let retired = p
        .ctx
        .client
        .retire_insight(context.id, "he moved to the platform team")
        .await?;
    d.check(
        retired.status == RETIRED && retired.valid_to.is_some(),
        || format!("retire returned status {}", retired.status),
    );
    p.expect("context", context.id, RETIRED, &[], d).await?;
    p.expect(
        "understanding",
        understanding.id,
        STALE,
        &[insight_changed(context.id, RETIRED)],
        d,
    )
    .await?;
    p.expect(
        "guidance",
        guidance.id,
        STALE,
        &[insight_changed(understanding.id, STALE)],
        d,
    )
    .await?;
    Ok(())
}

/// A write with `supersedes` closes the old insight, and what cited it goes stale.
async fn insight_supersede(p: &Probe<'_>, d: &mut InsightDetail) -> Result<(), CaseError> {
    let who = ("Sofia Marin", "person");
    let f = p
        .seed_all(
            "f",
            who,
            &[
                ("Sofia Marin runs the data warehouse", "runs"),
                ("Sofia Marin writes dbt models", "writes"),
                ("Sofia Marin presents metrics every Monday", "presents"),
            ],
            d,
        )
        .await?;
    let old = p
        .write(
            CONTEXT,
            "Sofia Marin is an analytics engineer.",
            facts(&[f[0].fact, f[1].fact]),
            d,
        )
        .await?;
    let understanding = p
        .write(
            UNDERSTANDING,
            "Sofia Marin cares about numbers people can trust.",
            vec![SourceRef::insight(old.id), SourceRef::fact(f[2].fact)],
            d,
        )
        .await?;
    let new = p
        .write_as(
            InsightWriter::Agent,
            CONTEXT,
            "Sofia Marin is the analytics engineer who owns weekly metrics.",
            facts(&[f[0].fact, f[1].fact, f[2].fact]),
            Some(old.id),
            ACTIVE,
            d,
        )
        .await?;
    let closed = p.expect("old", old.id, SUPERSEDED, &[], d).await?;
    d.check(
        closed.superseded_by == Some(new.id) && closed.valid_to.is_some(),
        || {
            format!(
                "old insight superseded_by {:?}, valid_to {:?}",
                closed.superseded_by.map(short),
                closed.valid_to
            )
        },
    );
    p.expect(
        "understanding",
        understanding.id,
        STALE,
        &[insight_changed(old.id, SUPERSEDED)],
        d,
    )
    .await?;
    p.expect("new", new.id, ACTIVE, &[], d).await?;
    Ok(())
}

/// A fact source reaching `expires_at` makes its insight stale at the next read, with no write.
async fn lazy_expiry(p: &Probe<'_>, d: &mut InsightDetail) -> Result<(), CaseError> {
    let who = ("Dara Quinn", "person");
    let leave = spec(who, "Dara Quinn is on leave this week", "on_leave");
    let expires_at = Utc::now() + chrono::Duration::seconds(EXPIRY_SECS);
    let mut fact = CommitFact::new(&leave, None);
    fact.expires_at = Some(expires_at);
    let committed = p.commit(Trust::Agent, "leave", fact).await?;
    let leave_id = first_fact(&committed)?;
    let cover = p
        .seed(
            "cover",
            who,
            "Dara Quinn covers the support rota",
            "covers",
            d,
        )
        .await?;
    let insight = p
        .write(
            CONTEXT,
            "Dara Quinn's support rota needs cover while she is away.",
            facts(&[leave_id, cover.fact]),
            d,
        )
        .await?;
    p.expect("before expiry", insight.id, ACTIVE, &[], d)
        .await?;

    let wait = (expires_at - Utc::now()).to_std().unwrap_or_default() + Duration::from_millis(1500);
    tokio::time::sleep(wait).await;
    let listed = p.ctx.client.list_insights(p.scope, None).await?;
    match listed.iter().find(|i| i.id == insight.id) {
        Some(found) => d.record(
            "listed after expiry",
            state_problems(found, STALE, &[fact_changed(leave_id, "expired")]),
        ),
        None => d.check(false, || "insight missing from the list".to_owned()),
    }
    p.expect(
        "read after expiry",
        insight.id,
        STALE,
        &[fact_changed(leave_id, "expired")],
        d,
    )
    .await?;
    Ok(())
}

/// Corroboration, and changes to facts an insight does not cite, leave it active.
async fn unrelated_changes(p: &Probe<'_>, d: &mut InsightDetail) -> Result<(), CaseError> {
    let who = ("Yusuf Demir", "person");
    let f = p
        .seed_all(
            "f",
            who,
            &[
                ("Yusuf Demir plays the cello", "plays"),
                (
                    "Yusuf Demir rehearses on Wednesday evenings",
                    "rehearses_on",
                ),
                ("Yusuf Demir drives a blue van", "drives"),
                ("Yusuf Demir parks on Elm Street", "parks_on"),
            ],
            d,
        )
        .await?;
    let insight = p
        .write(
            CONTEXT,
            "Yusuf Demir is an amateur cellist with a weekly rehearsal.",
            facts(&[f[0].fact, f[1].fact]),
            d,
        )
        .await?;
    // The same statement again: the server matches it to the cited fact as another source.
    let repeat = spec(who, "Yusuf Demir plays the cello", "plays");
    let committed = p
        .commit(Trust::Agent, "repeat", CommitFact::new(&repeat, None))
        .await?;
    d.check(first_fact(&committed)? == f[0].fact, || {
        "a repeated statement was not matched to the existing fact".to_owned()
    });
    p.supersede(
        "other-supersede",
        f[2].fact,
        &spec(who, "Yusuf Demir drives a green van", "drives"),
        d,
    )
    .await?;
    p.ctx
        .client
        .invalidate_fact(f[3].fact, "wrong street")
        .await?;
    p.write(
        CONTEXT,
        "Yusuf Demir makes music every week.",
        facts(&[f[0].fact, f[1].fact]),
        d,
    )
    .await?;
    p.expect("insight", insight.id, ACTIVE, &[], d).await?;
    Ok(())
}

// Trust and quarantine

/// In a shared scope the host's insights wait for review, guidance included; approval activates
/// and rejection rejects. The host's key has `agent` trust, which the source rule already holds in
/// a shared scope, so this cannot tell the guidance rule apart from the trust rule.
async fn guidance_outside_agent_scope(
    p: &Probe<'_>,
    d: &mut InsightDetail,
) -> Result<(), CaseError> {
    let who = ("Ivy Laurent", "person");
    let f = p
        .seed_all(
            "f",
            who,
            &[
                ("Ivy Laurent prefers written proposals", "prefers"),
                ("Ivy Laurent rejects meetings without an agenda", "rejects"),
            ],
            d,
        )
        .await?;
    let context = p
        .write_as(
            InsightWriter::Agent,
            CONTEXT,
            "Ivy Laurent works through documents rather than meetings.",
            facts(&[f[0].fact, f[1].fact]),
            None,
            QUARANTINED,
            d,
        )
        .await?;
    let guidance = p
        .write_as(
            InsightWriter::Agent,
            GUIDANCE,
            "Send Ivy Laurent a written proposal instead of booking a meeting.",
            facts(&[f[0].fact, f[1].fact]),
            None,
            QUARANTINED,
            d,
        )
        .await?;
    let held: HashSet<Uuid> = p
        .ctx
        .client
        .insight_quarantine(p.scope)
        .await?
        .iter()
        .map(|i| i.id)
        .collect();
    d.check(
        held.contains(&context.id) && held.contains(&guidance.id),
        || "held insights missing from the insight quarantine".to_owned(),
    );
    let recalled = p
        .recall(
            "Ivy Laurent proposal meeting",
            RecallOptions {
                insight_limit: Some(ALL_INSIGHTS),
                ..RecallOptions::default()
            },
        )
        .await?;
    d.record(
        "recall while held",
        recalled_problems(&recalled, &HashSet::new()),
    );

    let approved = p.ctx.client.approve_insight(context.id).await?;
    d.check(approved.status == ACTIVE, || {
        format!("approved insight has status {}", approved.status)
    });
    let rejected = p.ctx.client.reject_insight(guidance.id).await?;
    d.check(rejected.status == REJECTED, || {
        format!("rejected insight has status {}", rejected.status)
    });
    let recalled = p
        .recall(
            "Ivy Laurent proposal meeting",
            RecallOptions {
                insight_limit: Some(ALL_INSIGHTS),
                ..RecallOptions::default()
            },
        )
        .await?;
    d.record(
        "recall after review",
        recalled_problems(&recalled, &HashSet::from([context.id])),
    );
    Ok(())
}

/// An insight from a key of `external` trust is held, with that trust, until approved.
async fn external_writer(p: &Probe<'_>, d: &mut InsightDetail) -> Result<(), CaseError> {
    let who = ("Bruno Costa", "person");
    let f = p
        .seed_all(
            "f",
            who,
            &[
                ("Bruno Costa repairs vintage radios", "repairs"),
                ("Bruno Costa sells parts at the Sunday market", "sells"),
            ],
            d,
        )
        .await?;
    let insight = p
        .write_as(
            InsightWriter::External,
            CONTEXT,
            "Bruno Costa runs a small radio repair trade.",
            facts(&[f[0].fact, f[1].fact]),
            None,
            QUARANTINED,
            d,
        )
        .await?;
    d.check(insight.trust == Trust::External, || {
        format!("external writer's insight has trust {:?}", insight.trust)
    });
    let query = "Bruno Costa radio repair";
    let options = RecallOptions {
        insight_limit: Some(ALL_INSIGHTS),
        ..RecallOptions::default()
    };
    let held = p.recall(query, options).await?;
    d.record(
        "recall while held",
        recalled_problems(&held, &HashSet::new()),
    );
    let approved = p.ctx.client.approve_insight(insight.id).await?;
    d.check(approved.status == ACTIVE, || {
        format!("approved insight has status {}", approved.status)
    });
    let after = p.recall(query, options).await?;
    d.record(
        "recall after approval",
        recalled_problems(&after, &HashSet::from([insight.id])),
    );
    Ok(())
}

/// An insight is never more trusted than its evidence: citing an approved external fact lowers
/// it to `external`, which holds it for review.
async fn external_source(p: &Probe<'_>, d: &mut InsightDetail) -> Result<(), CaseError> {
    let who = ("Felix Grant", "person");
    let claim = spec(who, "Felix Grant's blog says he moved to Denver", "wrote");
    let committed = p
        .commit(
            Trust::External,
            "external-fact",
            CommitFact::new(&claim, None),
        )
        .await?;
    let external = first_fact(&committed)?;
    let approved = p.ctx.client.approve(external).await?;
    d.check(
        approved.status == ACTIVE && approved.trust == Trust::External,
        || {
            format!(
                "approved external fact has status {} and trust {:?}",
                approved.status, approved.trust
            )
        },
    );
    let own = p
        .seed(
            "own",
            who,
            "Felix Grant asked about Denver schools",
            "asked_about",
            d,
        )
        .await?;
    let insight = p
        .write_as(
            InsightWriter::Agent,
            CONTEXT,
            "Felix Grant is settling in Denver with a family.",
            facts(&[external, own.fact]),
            None,
            QUARANTINED,
            d,
        )
        .await?;
    d.check(insight.trust == Trust::External, || {
        format!(
            "insight citing an external fact has trust {:?}",
            insight.trust
        )
    });
    Ok(())
}

/// Sources are checked on write: at least two, each current and active, in the insight's own scope,
/// and an insight source of a strictly lower level.
async fn write_rules(p: &Probe<'_>, d: &mut InsightDetail) -> Result<(), CaseError> {
    let who = ("Hana Sato", "person");
    let f = p
        .seed_all(
            "f",
            who,
            &[
                ("Hana Sato climbs on weekends", "climbs_on"),
                ("Hana Sato trains at the Northside gym", "trains_at"),
                ("Hana Sato owns a red rope", "owns"),
            ],
            d,
        )
        .await?;
    let held_claim = spec(who, "Hana Sato climbed Everest last year", "climbed");
    let held = first_fact(
        &p.commit(Trust::External, "held", CommitFact::new(&held_claim, None))
            .await?,
    )?;
    p.supersede(
        "closer",
        f[2].fact,
        &spec(who, "Hana Sato owns a blue rope", "owns"),
        d,
    )
    .await?;
    let other_scope = format!("{}-other", p.scope);
    let other_spec = spec(who, "Hana Sato teaches belay classes", "teaches");
    let other = first_fact(
        &p.ctx
            .commit(
                Trust::Agent,
                (p.case, "other-scope"),
                (&other_scope, HOST_SOURCE, &other_spec.statement),
                vec![CommitFact::new(&other_spec, None)],
            )
            .await?,
    )?;
    let context = p
        .write(
            CONTEXT,
            "Hana Sato is a regular climber at Northside.",
            facts(&[f[0].fact, f[1].fact]),
            d,
        )
        .await?;

    p.refused("one source", CONTEXT, facts(&[f[0].fact]), d)
        .await?;
    p.refused("a quarantined fact", CONTEXT, facts(&[f[0].fact, held]), d)
        .await?;
    p.refused(
        "a superseded fact",
        CONTEXT,
        facts(&[f[0].fact, f[2].fact]),
        d,
    )
    .await?;
    p.refused(
        "a fact of another scope",
        CONTEXT,
        facts(&[f[0].fact, other]),
        d,
    )
    .await?;
    p.refused(
        "a context citing a context",
        CONTEXT,
        vec![SourceRef::insight(context.id), SourceRef::fact(f[1].fact)],
        d,
    )
    .await?;
    // The control: the same insight source at a higher level is accepted.
    p.write(
        UNDERSTANDING,
        "Hana Sato builds her week around climbing.",
        vec![SourceRef::insight(context.id), SourceRef::fact(f[1].fact)],
        d,
    )
    .await?;
    Ok(())
}

// Confirm

/// A stale insight confirmed with current sources is active again, with the new sources and no
/// stale reason. A confirm citing a changed fact is refused and changes nothing.
async fn confirm(p: &Probe<'_>, d: &mut InsightDetail) -> Result<(), CaseError> {
    let who = ("Leo Brandt", "person");
    let f = p
        .seed_all(
            "f",
            who,
            &[
                ("Leo Brandt manages the Berlin warehouse", "manages"),
                ("Leo Brandt orders stock every Tuesday", "orders_on"),
                ("Leo Brandt counts inventory every Friday", "counts_on"),
            ],
            d,
        )
        .await?;
    let statement = "Leo Brandt runs the Berlin warehouse on a fixed weekly rhythm.";
    let insight = p
        .write(CONTEXT, statement, facts(&[f[0].fact, f[1].fact]), d)
        .await?;
    p.ctx
        .client
        .invalidate_fact(f[1].fact, "orders moved to the supplier")
        .await?;
    p.expect(
        "after invalidation",
        insight.id,
        STALE,
        &[fact_changed(f[1].fact, "invalidated")],
        d,
    )
    .await?;
    let options = RecallOptions {
        insight_limit: Some(ALL_INSIGHTS),
        ..RecallOptions::default()
    };
    let while_stale = p.recall(statement, options).await?;
    d.record(
        "recall while stale",
        recalled_problems(&while_stale, &HashSet::new()),
    );

    match p
        .ctx
        .client
        .confirm_insight(insight.id, &facts(&[f[1].fact, f[2].fact]))
        .await
    {
        Err(CaseError::Status { status: 400, .. }) => d.check(true, String::new),
        Err(other) => return Err(other),
        Ok(confirmed) => d.check(false, || {
            format!(
                "confirm citing an invalidated fact accepted with status {}",
                confirmed.status
            )
        }),
    }
    p.expect(
        "after the refused confirm",
        insight.id,
        STALE,
        &[fact_changed(f[1].fact, "invalidated")],
        d,
    )
    .await?;

    let sources = facts(&[f[0].fact, f[2].fact]);
    p.ctx.client.confirm_insight(insight.id, &sources).await?;
    let confirmed = p
        .expect("after confirm", insight.id, ACTIVE, &[], d)
        .await?;
    let got: HashSet<&SourceRef> = confirmed.sources.iter().collect();
    let want: HashSet<&SourceRef> = sources.iter().collect();
    d.check(got == want, || {
        "confirmed insight does not cite the new sources".to_owned()
    });
    let after = p.recall(statement, options).await?;
    d.record(
        "recall after confirm",
        recalled_problems(&after, &HashSet::from([insight.id])),
    );
    Ok(())
}

// Recall

/// Recall returns active insights only: not stale, quarantined or retired ones.
async fn recall_active_only(p: &Probe<'_>, d: &mut InsightDetail) -> Result<(), CaseError> {
    let who = ("Nadia Petrova", "person");
    let f = p
        .seed_all(
            "f",
            who,
            &[
                ("Nadia Petrova grows tomatoes in her garden", "grows"),
                ("Nadia Petrova waters the garden at dawn", "waters"),
                ("Nadia Petrova keeps bees in the garden", "keeps"),
                ("Nadia Petrova composts garden waste", "composts"),
                ("Nadia Petrova sells garden honey at the market", "sells"),
                ("Nadia Petrova built raised garden beds", "built"),
            ],
            d,
        )
        .await?;
    let active = p
        .write(
            CONTEXT,
            "Nadia Petrova is a devoted vegetable gardener.",
            facts(&[f[0].fact, f[1].fact]),
            d,
        )
        .await?;
    let stale = p
        .write(
            CONTEXT,
            "Nadia Petrova runs an organic garden with bees.",
            facts(&[f[2].fact, f[3].fact]),
            d,
        )
        .await?;
    let retired = p
        .write(
            CONTEXT,
            "Nadia Petrova earns money from her garden.",
            facts(&[f[4].fact, f[5].fact]),
            d,
        )
        .await?;
    p.write_as(
        InsightWriter::External,
        CONTEXT,
        "Nadia Petrova's garden is the best in the city.",
        facts(&[f[0].fact, f[5].fact]),
        None,
        QUARANTINED,
        d,
    )
    .await?;
    p.ctx
        .client
        .invalidate_fact(f[3].fact, "she stopped composting")
        .await?;
    p.ctx
        .client
        .retire_insight(retired.id, "she gives the honey away now")
        .await?;
    p.expect(
        "stale",
        stale.id,
        STALE,
        &[fact_changed(f[3].fact, "invalidated")],
        d,
    )
    .await?;

    let response = p
        .recall(
            "Nadia Petrova garden",
            RecallOptions {
                insight_limit: Some(ALL_INSIGHTS),
                ..RecallOptions::default()
            },
        )
        .await?;
    d.record(
        "recall",
        recalled_problems(&response, &HashSet::from([active.id])),
    );
    d.check(response.insights_skipped.is_none(), || {
        format!(
            "insights_skipped {:?} at the present",
            response.insights_skipped
        )
    });
    Ok(())
}

/// `insight_limit` caps the list, defaults to 3, and 0 turns insight search off.
async fn recall_limit(p: &Probe<'_>, d: &mut InsightDetail) -> Result<(), CaseError> {
    let who = ("Elif Kaya", "person");
    let f = p
        .seed_all(
            "f",
            who,
            &[
                ("Elif Kaya cooks Turkish food", "cooks"),
                ("Elif Kaya bakes bread on Sundays", "bakes_on"),
                ("Elif Kaya grows herbs for cooking", "grows"),
                ("Elif Kaya hosts dinners for friends", "hosts"),
                ("Elif Kaya collects cookbooks", "collects"),
                ("Elif Kaya teaches a cooking class", "teaches"),
            ],
            d,
        )
        .await?;
    let pairs = [(0, 1), (2, 3), (4, 5), (0, 3), (1, 4)];
    let statements = [
        "Elif Kaya is a home cook rooted in Turkish food.",
        "Elif Kaya cooks with what she grows and shares it.",
        "Elif Kaya studies cooking and teaches it.",
        "Elif Kaya cooks Turkish dinners for friends.",
        "Elif Kaya bakes and reads about cooking.",
    ];
    for ((a, b), statement) in pairs.iter().zip(statements) {
        p.write(CONTEXT, statement, facts(&[f[*a].fact, f[*b].fact]), d)
            .await?;
    }
    let query = "Elif Kaya cooking";
    for (limit, expected) in [
        (Some(2), 2),
        (None, 3),
        (Some(0), 0),
        (Some(ALL_INSIGHTS), 5),
    ] {
        let response = p
            .recall(
                query,
                RecallOptions {
                    insight_limit: limit,
                    ..RecallOptions::default()
                },
            )
            .await?;
        let got = response.insights.len();
        d.check(got == expected, || {
            format!("insight_limit {limit:?} returned {got} insights, expected {expected}")
        });
    }
    Ok(())
}

/// With `as_of` or `known_at`, insights are skipped as `time_travel`. At the present they are not.
async fn recall_time_travel(p: &Probe<'_>, d: &mut InsightDetail) -> Result<(), CaseError> {
    let who = ("Rosa Lima", "person");
    let f = p
        .seed_all(
            "f",
            who,
            &[
                ("Rosa Lima paints watercolours", "paints"),
                ("Rosa Lima exhibits at the town library", "exhibits_at"),
            ],
            d,
        )
        .await?;
    let insight = p
        .write(
            CONTEXT,
            "Rosa Lima is a local watercolour painter.",
            facts(&[f[0].fact, f[1].fact]),
            d,
        )
        .await?;
    let query = "Rosa Lima watercolour painter";
    let present = p
        .recall(
            query,
            RecallOptions {
                insight_limit: Some(INSIGHT_LIMIT),
                ..RecallOptions::default()
            },
        )
        .await?;
    d.record(
        "present",
        recalled_problems(&present, &HashSet::from([insight.id])),
    );
    let now = Utc::now();
    let travels = [
        (
            "as_of",
            RecallOptions {
                as_of: Some(now),
                insight_limit: Some(INSIGHT_LIMIT),
                ..RecallOptions::default()
            },
        ),
        (
            "known_at",
            RecallOptions {
                known_at: Some(now),
                insight_limit: Some(INSIGHT_LIMIT),
                ..RecallOptions::default()
            },
        ),
    ];
    for (label, options) in travels {
        let response = p.recall(query, options).await?;
        d.check(
            response.insights.is_empty()
                && response.insights_skipped.as_deref() == Some("time_travel"),
            || {
                format!(
                    "with {label}: {} insights, insights_skipped {:?}",
                    response.insights.len(),
                    response.insights_skipped
                )
            },
        );
    }
    Ok(())
}

// Reflect

fn topic_statements(subject: &str, count: usize) -> Vec<(String, &'static str)> {
    TOPIC_FACTS
        .iter()
        .take(count)
        .map(|tail| (format!("{subject} {tail}"), "does"))
        .collect()
}

async fn seed_topic(
    p: &Probe<'_>,
    prefix: &str,
    subject: (&str, &str),
    range: std::ops::Range<usize>,
    d: &mut InsightDetail,
) -> Result<Vec<Uuid>, CaseError> {
    let statements = topic_statements(subject.0, range.end);
    let mut ids = Vec::new();
    for index in range {
        let (statement, predicate) = &statements[index];
        ids.push(
            p.seed(
                &format!("{prefix}{index}"),
                subject,
                statement,
                predicate,
                d,
            )
            .await?
            .fact,
        );
    }
    Ok(ids)
}

fn topic_for<'b>(
    batch: &'b ReflectBatch,
    subject: &str,
) -> Option<&'b crate::client::ReflectTopic> {
    batch
        .topics
        .iter()
        .find(|t| contains_ci(&t.subject, subject))
}

/// The topic's facts and `fact_set` match what was committed.
fn topic_problems(topic: &crate::client::ReflectTopic, expected: &[Uuid]) -> Vec<String> {
    let mut problems = Vec::new();
    let got: HashSet<Uuid> = topic.facts.iter().map(|f| f.id).collect();
    let want: HashSet<Uuid> = expected.iter().copied().collect();
    if got != want {
        problems.push(format!(
            "topic offers {} facts, expected the {} committed",
            got.len(),
            want.len()
        ));
    }
    let digest = fact_set_digest(expected);
    if topic.fact_set != digest {
        problems.push(format!(
            "fact_set {} is not the digest of the committed ids",
            topic.fact_set
        ));
    }
    problems
}

/// A subject is offered once it has `MEMORYOS_REFLECT_MIN_FACTS` uncited facts, not before.
async fn reflect_threshold(p: &Probe<'_>, d: &mut InsightDetail) -> Result<(), CaseError> {
    let subject = ("Harbor Lane Bakery", "organization");
    let min = p.reflect_min_facts;
    let mut ids = seed_topic(p, "t", subject, 0..min - 1, d).await?;
    let below = p.ctx.client.reflect_batch(p.scope, BATCH_MAX).await?;
    d.check(topic_for(&below, subject.0).is_none(), || {
        format!(
            "topic offered with {} facts, below the threshold of {min}",
            min - 1
        )
    });
    ids.extend(seed_topic(p, "t", subject, min - 1..min, d).await?);
    let at = p.ctx.client.reflect_batch(p.scope, BATCH_MAX).await?;
    match topic_for(&at, subject.0) {
        Some(topic) => d.record("topic at the threshold", topic_problems(topic, &ids)),
        None => d.check(false, || format!("topic not offered with {min} facts")),
    }
    Ok(())
}

/// A declined topic is not offered again until its fact set changes.
async fn reflect_decline(p: &Probe<'_>, d: &mut InsightDetail) -> Result<(), CaseError> {
    let subject = ("Juniper Row Library", "organization");
    let min = p.reflect_min_facts;
    let mut ids = seed_topic(p, "t", subject, 0..min, d).await?;
    let first = p.ctx.client.reflect_batch(p.scope, BATCH_MAX).await?;
    let Some(topic) = topic_for(&first, subject.0).cloned() else {
        d.check(false, || "topic not offered at the threshold".to_owned());
        return Ok(());
    };
    d.record("topic before the decline", topic_problems(&topic, &ids));
    p.ctx
        .client
        .reflect_decline(p.scope, topic.topic_key, &topic.fact_set)
        .await?;
    let declined = p.ctx.client.reflect_batch(p.scope, BATCH_MAX).await?;
    d.check(topic_for(&declined, subject.0).is_none(), || {
        "declined topic offered again with the same facts".to_owned()
    });
    ids.extend(seed_topic(p, "t", subject, min..min + 1, d).await?);
    let changed = p.ctx.client.reflect_batch(p.scope, BATCH_MAX).await?;
    match topic_for(&changed, subject.0) {
        Some(again) => {
            d.record("topic after a new fact", topic_problems(again, &ids));
            d.check(
                again.fact_set != topic.fact_set && again.topic_key == topic.topic_key,
                || "topic came back with the declined fact_set or another key".to_owned(),
            );
        }
        None => d.check(false, || {
            "declined topic not offered after its fact set changed".to_owned()
        }),
    }
    Ok(())
}

/// Stale insights come before topics, oldest first, and facts cited by a stale insight are no topic.
async fn reflect_stale_first(p: &Probe<'_>, d: &mut InsightDetail) -> Result<(), CaseError> {
    let who = ("Corin Vale", "person");
    let f = p
        .seed_all(
            "f",
            who,
            &[
                ("Corin Vale sails a small dinghy", "sails"),
                ("Corin Vale races on Saturday mornings", "races_on"),
                ("Corin Vale repairs sails in winter", "repairs"),
                ("Corin Vale coaches junior sailors", "coaches"),
            ],
            d,
        )
        .await?;
    let older = p
        .write(
            CONTEXT,
            "Corin Vale is a weekend dinghy racer.",
            facts(&[f[0].fact, f[1].fact]),
            d,
        )
        .await?;
    let newer = p
        .write(
            CONTEXT,
            "Corin Vale keeps sailing all year and teaches it.",
            facts(&[f[2].fact, f[3].fact]),
            d,
        )
        .await?;
    let topic_subject = ("Saltmarsh Ferry Company", "organization");
    seed_topic(p, "t", topic_subject, 0..p.reflect_min_facts, d).await?;
    // Newer first, so the order checked below comes from the insights' age, not the change's.
    p.ctx
        .client
        .invalidate_fact(f[3].fact, "he stopped coaching")
        .await?;
    p.ctx
        .client
        .invalidate_fact(f[1].fact, "he races on Sundays")
        .await?;

    let one = p.ctx.client.reflect_batch(p.scope, 1).await?;
    let one_ids: Vec<Uuid> = one.stale.iter().map(|i| i.id).collect();
    d.check(one_ids == [older.id] && one.topics.is_empty(), || {
        format!(
            "batch of 1 held {} stale and {} topics, expected the older stale insight only",
            one.stale.len(),
            one.topics.len()
        )
    });
    let full = p.ctx.client.reflect_batch(p.scope, BATCH_MAX).await?;
    let ids: Vec<Uuid> = full.stale.iter().map(|i| i.id).collect();
    d.check(ids == [older.id, newer.id], || {
        format!(
            "stale list {:?}, expected oldest first",
            ids.iter().map(|id| short(*id)).collect::<Vec<_>>()
        )
    });
    for insight in &full.stale {
        d.check(
            insight.status == STALE && insight.stale_reason.is_some(),
            || {
                format!(
                    "batch lists {} with status {}",
                    short(insight.id),
                    insight.status
                )
            },
        );
    }
    d.check(topic_for(&full, topic_subject.0).is_some(), || {
        "ready topic missing after the stale insights".to_owned()
    });
    d.check(topic_for(&full, who.0).is_none(), || {
        "facts cited by stale insights offered as a topic".to_owned()
    });
    let again = p.ctx.client.reflect_batch(p.scope, BATCH_MAX).await?;
    let same = again
        .stale
        .iter()
        .map(|i| i.id)
        .eq(full.stale.iter().map(|i| i.id))
        && again
            .topics
            .iter()
            .map(|t| (t.topic_key, &t.fact_set))
            .eq(full.topics.iter().map(|t| (t.topic_key, &t.fact_set)));
    d.check(same, || "two identical batches differ".to_owned());
    Ok(())
}

// Facts unchanged with insight search

#[derive(Debug, Default, Serialize)]
pub struct FactsDetail {
    /// Fact ids in order with `insight_limit` 0, then with 3.
    pub without_insights: Vec<Uuid>,
    pub with_insights: Vec<Uuid>,
    pub insights_when_off: usize,
    pub insights_when_on: usize,
    /// Set once both responses were compared, so a case that never ran gets no credit.
    pub compared: bool,
}

impl Verdict for FactsDetail {
    fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if !self.compared {
            problems.push("not compared".to_owned());
        } else if self.without_insights != self.with_insights {
            problems.push("fact results differ between insight_limit 0 and 3".to_owned());
        }
        if self.insights_when_off > 0 {
            problems.push(format!(
                "{} insights with insight_limit 0",
                self.insights_when_off
            ));
        }
        problems
    }
}

pub fn compare_facts(off: &RecallResponse, on: &RecallResponse, detail: &mut FactsDetail) {
    detail.without_insights = off.facts.iter().map(|s| s.fact.id).collect();
    detail.with_insights = on.facts.iter().map(|s| s.fact.id).collect();
    detail.insights_when_off = off.insights.len();
    detail.insights_when_on = on.insights.len();
    detail.compared = true;
}

fn options(insight_limit: u32) -> RecallOptions {
    // Pages off, so the comparison and the timings are about insights only.
    RecallOptions {
        page_limit: Some(0),
        insight_limit: Some(insight_limit),
        ..RecallOptions::default()
    }
}

async fn compare(
    ctx: &RunContext,
    query: &str,
    scope: &str,
    detail: &mut FactsDetail,
) -> Result<(), CaseError> {
    let off = ctx
        .client
        .recall_with(scope, query, FACT_LIMIT, options(0))
        .await?;
    let on = ctx
        .client
        .recall_with(scope, query, FACT_LIMIT, options(INSIGHT_LIMIT))
        .await?;
    compare_facts(&off, &on, detail);
    Ok(())
}

/// Commits the recall corpus and writes one context per case over its first two current facts,
/// so insight search has something to find for every query. Returns the insights written.
async fn build_corpus(
    ctx: &RunContext,
    cases: &[RecallCase],
    scope: &str,
    concurrency: usize,
) -> Result<usize, String> {
    let writes = cases.iter().map(|case| async move {
        let mut problems = Vec::new();
        let mut committed: Vec<(Uuid, String)> = Vec::new();
        let mut closed: HashSet<Uuid> = HashSet::new();
        for (index, commit) in case.commits.iter().enumerate() {
            let facts = commit
                .facts
                .iter()
                .map(|fact| CommitFact::new(fact, None))
                .collect();
            // The label differs from the other suites', so the idempotency keys never collide.
            let response = ctx
                .establish(
                    (&case.id, &format!("insights-{index}")),
                    ScopeKind::Agent,
                    (scope, &commit.content),
                    facts,
                    &mut problems,
                )
                .await
                .map_err(|e| e.to_string())?;
            for (fact, spec) in response.facts.iter().zip(&commit.facts) {
                closed.extend(fact.superseded);
                committed.push((fact.id, spec.statement.clone()));
            }
        }
        if !problems.is_empty() {
            return Err(problems.join("; "));
        }
        let mut seen = HashSet::new();
        let current: Vec<&(Uuid, String)> = committed
            .iter()
            .filter(|(id, _)| !closed.contains(id) && seen.insert(*id))
            .take(2)
            .collect();
        if current.len() < 2 {
            return Ok(0);
        }
        let statement = format!("{} {}", current[0].1, current[1].1);
        ctx.client
            .write_insight(
                InsightWriter::Agent,
                &InsightWrite {
                    scope,
                    level: CONTEXT,
                    statement: &statement,
                    sources: facts(&[current[0].0, current[1].0]),
                    supersedes: None,
                },
            )
            .await
            .map(|_| 1)
            .map_err(|e| format!("{}: {e}", case.id))
    });
    let results: Vec<Result<usize, String>> = stream::iter(writes)
        .buffered(concurrency.max(1))
        .collect()
        .await;
    results.into_iter().sum()
}

// Latency

#[derive(Debug, Clone, Serialize)]
pub struct Latency {
    /// Timed calls per `insight_limit` setting, after the warm-up.
    pub calls: usize,
    pub warmup_calls: usize,
    pub errors: usize,
    pub p50_ms_insight_limit_0: f64,
    pub p95_ms_insight_limit_0: f64,
    pub p50_ms_insight_limit_3: f64,
    pub p95_ms_insight_limit_3: f64,
}

/// Times `calls` recalls with `insight_limit` 0 and as many with 3, pages off, one at a time. The
/// setting that goes first alternates, so neither always meets a warmer cache.
async fn measure_latency(ctx: &RunContext, scope: &str, queries: &[&str], calls: usize) -> Latency {
    let mut errors = 0;
    let mut warmup_calls = 0;
    for query in queries {
        for limit in [0, INSIGHT_LIMIT] {
            warmup_calls += 1;
            if ctx
                .client
                .recall_with(scope, query, FACT_LIMIT, options(limit))
                .await
                .is_err()
            {
                errors += 1;
            }
        }
    }
    let mut samples: HashMap<u32, Vec<f64>> = HashMap::new();
    for index in 0..calls {
        let query = queries[index % queries.len()];
        let order = if index % 2 == 0 {
            [0, INSIGHT_LIMIT]
        } else {
            [INSIGHT_LIMIT, 0]
        };
        for limit in order {
            let started = Instant::now();
            let outcome = ctx
                .client
                .recall_with(scope, query, FACT_LIMIT, options(limit))
                .await;
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            match outcome {
                Err(_) => errors += 1,
                Ok(_) => samples.entry(limit).or_default().push(ms),
            }
        }
    }
    let off = samples.remove(&0).unwrap_or_default();
    let on = samples.remove(&INSIGHT_LIMIT).unwrap_or_default();
    Latency {
        calls,
        warmup_calls,
        errors,
        p50_ms_insight_limit_0: percentile(&off, 0.50),
        p95_ms_insight_limit_0: percentile(&off, 0.95),
        p50_ms_insight_limit_3: percentile(&on, 0.50),
        p95_ms_insight_limit_3: percentile(&on, 0.95),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{ScoredInsight, SourceRef};

    fn insight(status: &str, reason: Option<Vec<StaleEntry>>) -> Insight {
        Insight {
            id: Uuid::from_u128(1),
            scope: "agent:x".to_owned(),
            level: CONTEXT.to_owned(),
            statement: "s".to_owned(),
            trust: Trust::Agent,
            status: status.to_owned(),
            stale_reason: reason,
            sources: vec![SourceRef::fact(Uuid::from_u128(2))],
            valid_to: None,
            superseded_by: None,
            pending_supersede_of: None,
            recorded_at: Utc::now(),
        }
    }

    #[test]
    fn a_stale_insight_needs_exactly_the_expected_reason() {
        let a = superseded(Uuid::from_u128(5), Uuid::from_u128(6));
        let b = fact_changed(Uuid::from_u128(7), "purged");
        let found = insight(STALE, Some(vec![b.clone(), a.clone()]));
        assert!(state_problems(&found, STALE, &[a.clone(), b.clone()]).is_empty());

        // A missing, extra or different entry fails.
        assert_eq!(
            state_problems(&found, STALE, std::slice::from_ref(&a)).len(),
            1
        );
        let wrong_by = superseded(Uuid::from_u128(5), Uuid::from_u128(9));
        assert_eq!(
            state_problems(&found, STALE, &[wrong_by, b.clone()]).len(),
            1
        );
        let doubled = insight(STALE, Some(vec![a.clone(), a.clone()]));
        assert_eq!(state_problems(&doubled, STALE, &[a]).len(), 1);
    }

    #[test]
    fn stale_reason_is_set_exactly_while_stale() {
        assert!(state_problems(&insight(ACTIVE, None), ACTIVE, &[]).is_empty());
        let leftover = insight(ACTIVE, Some(vec![fact_changed(Uuid::nil(), "expired")]));
        assert_eq!(state_problems(&leftover, ACTIVE, &[]).len(), 1);
        let bare = insight(STALE, None);
        // No reason at all, and so not the expected one either.
        assert_eq!(
            state_problems(&bare, STALE, &[fact_changed(Uuid::nil(), "expired")]).len(),
            2
        );
    }

    #[test]
    fn an_active_insight_where_stale_was_expected_fails() {
        let problems = state_problems(
            &insight(ACTIVE, None),
            STALE,
            &[fact_changed(Uuid::nil(), "invalidated")],
        );
        assert!(
            problems[0].contains("status active, expected stale"),
            "{problems:?}"
        );
    }

    #[test]
    fn stale_entries_decode_from_the_contract_shape() {
        let entries: Vec<StaleEntry> = serde_json::from_value(serde_json::json!([
            { "kind": "fact", "id": Uuid::from_u128(1), "change": "superseded", "by": Uuid::from_u128(2) },
            { "kind": "insight", "id": Uuid::from_u128(3), "change": "insight_changed", "status": "retired" },
            { "kind": "fact", "id": Uuid::from_u128(4), "change": "expired" },
        ]))
        .expect("decodes");
        assert_eq!(
            entries[0],
            superseded(Uuid::from_u128(1), Uuid::from_u128(2))
        );
        assert_eq!(entries[1], insight_changed(Uuid::from_u128(3), RETIRED));
        assert_eq!(entries[2], fact_changed(Uuid::from_u128(4), "expired"));
    }

    #[test]
    fn the_fact_set_digest_sorts_and_hashes_hyphenated_ids() {
        let a = Uuid::from_u128(2);
        let b = Uuid::from_u128(1);
        assert_eq!(fact_set_digest(&[a, b]), fact_set_digest(&[b, a]));
        let mut hasher = Sha256::new();
        hasher.update(format!("{}\n{}\n", b.hyphenated(), a.hyphenated()).as_bytes());
        let expected: String = hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(fact_set_digest(&[a, b]), expected);
        assert_eq!(expected.len(), 64);
    }

    fn recalled(insights: Vec<Insight>) -> RecallResponse {
        RecallResponse {
            facts: Vec::new(),
            pages: Vec::new(),
            pages_skipped: None,
            insights: insights
                .into_iter()
                .map(|insight| ScoredInsight {
                    insight,
                    score: 0.1,
                })
                .collect(),
            insights_skipped: None,
        }
    }

    #[test]
    fn recall_must_return_exactly_the_active_insights() {
        let mut active = insight(ACTIVE, None);
        active.id = Uuid::from_u128(10);
        let expected = HashSet::from([active.id]);
        assert!(recalled_problems(&recalled(vec![active.clone()]), &expected).is_empty());
        assert_eq!(recalled_problems(&recalled(Vec::new()), &expected).len(), 1);

        let mut stale = insight(STALE, Some(Vec::new()));
        stale.id = Uuid::from_u128(11);
        // Not expected, and not active: two findings.
        assert_eq!(
            recalled_problems(&recalled(vec![active, stale]), &expected).len(),
            2
        );
    }

    #[test]
    fn facts_must_match_and_insights_must_stay_off_at_zero() {
        let mut same = FactsDetail::default();
        compare_facts(&recalled(Vec::new()), &recalled(Vec::new()), &mut same);
        assert!(same.problems().is_empty());

        let mut leaked = FactsDetail::default();
        compare_facts(
            &recalled(vec![insight(ACTIVE, None)]),
            &recalled(Vec::new()),
            &mut leaked,
        );
        assert_eq!(leaked.problems().len(), 1);
        assert_eq!(FactsDetail::default().problems().len(), 1);
    }

    #[test]
    fn an_unfinished_case_gets_no_credit() {
        assert_eq!(InsightDetail::default().problems().len(), 1);
        let done = InsightDetail {
            checks: 3,
            completed: true,
            ..InsightDetail::default()
        };
        assert!(done.problems().is_empty());
    }

    #[test]
    fn every_case_has_a_unique_id_and_a_known_class() {
        let ids: HashSet<&str> = CASES.iter().map(|c| c.id).collect();
        assert_eq!(ids.len(), CASES.len());
        let classes = [
            CLASS_STALENESS,
            CLASS_TRUST,
            CLASS_CONFIRM,
            CLASS_RECALL,
            CLASS_REFLECT,
        ];
        assert!(CASES.iter().all(|c| classes.contains(&c.class)));
    }

    #[test]
    fn topic_statements_name_the_subject_and_stay_distinct() {
        let statements = topic_statements("Harbor Lane Bakery", TOPIC_FACTS.len());
        let distinct: HashSet<&String> = statements.iter().map(|(s, _)| s).collect();
        assert_eq!(distinct.len(), TOPIC_FACTS.len());
        assert!(statements
            .iter()
            .all(|(s, _)| s.starts_with("Harbor Lane Bakery ")));
    }
}
