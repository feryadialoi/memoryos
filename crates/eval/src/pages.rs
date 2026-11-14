//! The pages suite: knowledge pages are found by the questions they answer, a quarantined edit never
//! reaches recall, and page search leaves fact results as they were.
//!
//! Pages and the recall corpus share one agent scope, where the host's key writes them current at
//! once, so questions compete with every other page and with facts. The quarantine probe runs in
//! its own shared scope, where the host's edit has to wait for review.

use std::collections::HashMap;
use std::time::Instant;

use futures::stream::{self, StreamExt};
use serde::Serialize;
use uuid::Uuid;

use crate::client::{
    CaseError, CommitFact, PageHit, PageVersion, PageWrite, PageWriter, RecallResponse, Trust,
};
use crate::dataset::{PageQuestion, PagesData, QuarantineProbe, RecallCase, ScopeKind};
use crate::score::{contains_ci, percentile};
use crate::suites::{finish, run_all, CaseMeta, CaseResult, RunContext, Verdict};

/// A question passes when its page is among this many page hits.
pub const PAGE_TOP_K: usize = 3;
/// The server default, asked for explicitly so the request states what is scored.
const PAGE_LIMIT: u32 = 3;
/// The API default for facts.
const FACT_LIMIT: u32 = 10;
const ACTIVE: &str = "active";
const QUARANTINED: &str = "quarantined";

pub const CLASS_QUESTION: &str = "question";
pub const CLASS_FACTS: &str = "facts_unchanged";
pub const CLASS_QUARANTINE: &str = "quarantine";

/// Everything one run of the suite produced.
pub struct PagesRun {
    pub questions: Vec<CaseResult<QuestionDetail>>,
    pub facts: Vec<CaseResult<FactsDetail>>,
    pub quarantine: Vec<CaseResult<QuarantineDetail>>,
    pub latency: Option<Latency>,
}

// Questions

#[derive(Debug, Default, Serialize)]
pub struct QuestionDetail {
    pub write_problems: Vec<String>,
    pub expected_slug: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_section: Option<String>,
    /// 1-based rank of the expected page among the page hits.
    pub rank: Option<usize>,
    /// Whether the top hit is the expected page at the expected section. `None` when the question
    /// names no section.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section_matched: Option<bool>,
    pub hits: Vec<HitSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HitSummary {
    pub slug: String,
    pub section: String,
    pub version: i64,
    pub score: f64,
}

impl From<&PageHit> for HitSummary {
    fn from(hit: &PageHit) -> Self {
        Self {
            slug: hit.slug.clone(),
            section: hit.section.clone(),
            version: hit.version,
            score: hit.score,
        }
    }
}

impl Verdict for QuestionDetail {
    fn problems(&self) -> Vec<String> {
        let mut problems = self.write_problems.clone();
        match self.rank {
            Some(rank) if rank <= PAGE_TOP_K => {}
            Some(rank) => problems.push(format!("page {} ranked {rank}", self.expected_slug)),
            None => problems.push(format!("page {} not in page hits", self.expected_slug)),
        }
        if self.section_matched == Some(false) {
            let top = self.hits.first().map_or("no hit".to_owned(), |hit| {
                format!("{}: {}", hit.slug, hit.section)
            });
            problems.push(format!(
                "top hit is {top}, expected section {}",
                self.expected_section.as_deref().unwrap_or_default()
            ));
        }
        problems
    }
}

/// Scores one response against one question. Pure, so the rules are unit tested.
pub fn score_question(
    question: &PageQuestion,
    expected_section: Option<String>,
    hits: &[PageHit],
    detail: &mut QuestionDetail,
) {
    detail.rank = hits
        .iter()
        .position(|hit| hit.slug == question.slug)
        .map(|index| index + 1);
    detail.section_matched = expected_section.as_ref().map(|expected| {
        hits.first()
            .is_some_and(|top| top.slug == question.slug && top.section == *expected)
    });
    detail.expected_section = expected_section;
    detail.hits = hits.iter().map(HitSummary::from).collect();
}

// Facts unchanged

#[derive(Debug, Default, Serialize)]
pub struct FactsDetail {
    /// Fact ids in order with `page_limit` 0, then with 3.
    pub without_pages: Vec<Uuid>,
    pub with_pages: Vec<Uuid>,
    /// Page hits returned with `page_limit` 0, which turns page search off.
    pub pages_when_off: usize,
    pub pages_when_on: usize,
    /// Largest score difference of a fact at the same rank. Reported, not scored.
    pub max_score_delta: f64,
    /// Set once both responses were compared, so a case that never ran gets no credit.
    pub compared: bool,
}

impl Verdict for FactsDetail {
    fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if !self.compared {
            problems.push("not compared".to_owned());
        } else if self.without_pages != self.with_pages {
            problems.push("fact results differ between page_limit 0 and 3".to_owned());
        }
        if self.pages_when_off > 0 {
            problems.push(format!(
                "{} page hits with page_limit 0",
                self.pages_when_off
            ));
        }
        problems
    }
}

pub fn compare_facts(off: &RecallResponse, on: &RecallResponse, detail: &mut FactsDetail) {
    detail.without_pages = off.facts.iter().map(|s| s.fact.id).collect();
    detail.with_pages = on.facts.iter().map(|s| s.fact.id).collect();
    detail.pages_when_off = off.pages.len();
    detail.pages_when_on = on.pages.len();
    detail.max_score_delta = off
        .facts
        .iter()
        .zip(&on.facts)
        .map(|(a, b)| (a.score - b.score).abs())
        .fold(0.0, f64::max);
    detail.compared = true;
}

// Quarantine

#[derive(Debug, Default, Serialize)]
pub struct QuarantineDetail {
    /// The held edit reachable without review. Any entry fails the zero-leak gate.
    pub leaks: Vec<String>,
    pub write_problems: Vec<String>,
    /// The checks that make "absent" mean something: the reviewed page is found while the edit
    /// is held, and the edit is found once approved.
    pub control_problems: Vec<String>,
    pub base_version: Option<i64>,
    pub edit_version: Option<i64>,
    pub held_hits: Vec<HitSummary>,
    pub approved_hits: Vec<HitSummary>,
    /// Set when every step ran, so a case cut short gets no credit.
    pub completed: bool,
}

impl Verdict for QuarantineDetail {
    fn problems(&self) -> Vec<String> {
        let mut problems = self.leaks.clone();
        problems.extend(self.write_problems.iter().cloned());
        problems.extend(self.control_problems.iter().cloned());
        if !self.completed && problems.is_empty() {
            problems.push("probe did not finish".to_owned());
        }
        problems
    }
}

/// Hits that expose a held edit: its version, or its planted text in a snippet.
pub fn held_edit_leaks(
    hits: &[PageHit],
    slug: &str,
    edit_version: i64,
    planted: &[String],
) -> Vec<String> {
    hits.iter()
        .filter(|hit| {
            (hit.slug == slug && hit.version == edit_version)
                || planted.iter().any(|p| contains_ci(&hit.snippet, p))
        })
        .map(|hit| {
            format!(
                "recall returned {} version {} ({}) while the edit was held",
                hit.slug, hit.version, hit.section
            )
        })
        .collect()
}

// Latency

#[derive(Debug, Clone, Serialize)]
pub struct Latency {
    /// Timed calls per `page_limit` setting, after the warm-up.
    pub calls: usize,
    pub warmup_calls: usize,
    pub errors: usize,
    pub p50_ms_page_limit_0: f64,
    pub p95_ms_page_limit_0: f64,
    pub p50_ms_page_limit_3: f64,
    pub p95_ms_page_limit_3: f64,
}

// The run

pub async fn run_pages(
    ctx: &RunContext,
    data: &PagesData,
    recall_cases: &[RecallCase],
    concurrency: usize,
    latency_calls: usize,
) -> PagesRun {
    let scope = ctx.scope(ScopeKind::Agent, "pages");
    let written = write_pages(ctx, data, &scope, concurrency).await;
    let corpus = commit_corpus(ctx, recall_cases, &scope, concurrency).await;

    let questions = run_all(&data.questions, concurrency, |question| {
        let scope = scope.as_str();
        let written = &written;
        async move {
            let meta = CaseMeta {
                id: &question.id,
                lang: &question.lang,
                class: Some(CLASS_QUESTION.to_owned()),
                scope: scope.to_owned(),
                started: Instant::now(),
            };
            let title = data
                .page(&question.slug)
                .map(|page| page.title.as_str())
                .unwrap_or_default();
            // Set before any request, so an errored case still counts as a section question.
            let mut detail = QuestionDetail {
                expected_slug: question.slug.clone(),
                expected_section: question.expected_section(title),
                ..QuestionDetail::default()
            };
            let outcome = match written.get(&question.slug) {
                Some(Err(error)) => Err(CaseError::Earlier(error.clone())),
                None => Err(CaseError::Earlier("page was not written".to_owned())),
                Some(Ok(problems)) => {
                    detail.write_problems.clone_from(problems);
                    ask(ctx, question, scope, &mut detail).await
                }
            };
            finish(meta, outcome, detail)
        }
    })
    .await;

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
                Ok(()) => compare(ctx, &case.query, scope, &mut detail).await,
            };
            finish(meta, outcome, detail)
        }
    })
    .await;

    let quarantine = run_all(&data.quarantine, concurrency, |probe| async move {
        let meta = CaseMeta {
            id: &probe.id,
            lang: &probe.lang,
            class: Some(CLASS_QUARANTINE.to_owned()),
            scope: ctx.scope(ScopeKind::Shared, &probe.id),
            started: Instant::now(),
        };
        let mut detail = QuarantineDetail::default();
        let outcome = quarantine_steps(ctx, probe, &meta.scope, &mut detail).await;
        finish(meta, outcome, detail)
    })
    .await;

    let latency = if latency_calls > 0 {
        let queries: Vec<&str> = data
            .questions
            .iter()
            .map(|q| q.query.as_str())
            .chain(recall_cases.iter().map(|c| c.query.as_str()))
            .collect();
        Some(measure_latency(ctx, &scope, &queries, latency_calls).await)
    } else {
        None
    };

    PagesRun {
        questions,
        facts,
        quarantine,
        latency,
    }
}

/// Writes every page with the host's key. In an agent scope each version is current at once, so
/// a status other than active is a problem for the questions about that page.
async fn write_pages(
    ctx: &RunContext,
    data: &PagesData,
    scope: &str,
    concurrency: usize,
) -> HashMap<String, Result<Vec<String>, String>> {
    let writes = data.pages.iter().map(|page| async move {
        let request = PageWrite {
            title: &page.title,
            body: &page.body,
            expected_version: None,
            approve: false,
        };
        let outcome = ctx
            .client
            .write_page(PageWriter::Agent, (scope, &page.slug), &request)
            .await
            .map(|version| {
                let mut problems = Vec::new();
                if version.status != ACTIVE {
                    problems.push(format!(
                        "page {} written with status {}, expected {ACTIVE}",
                        page.slug, version.status
                    ));
                }
                problems
            })
            .map_err(|e| e.to_string());
        (page.slug.clone(), outcome)
    });
    stream::iter(writes)
        .buffered(concurrency.max(1))
        .collect()
        .await
}

/// Commits the recall corpus beside the pages, so the facts comparison runs where pages compete.
async fn commit_corpus(
    ctx: &RunContext,
    cases: &[RecallCase],
    scope: &str,
    concurrency: usize,
) -> Result<(), String> {
    let commits = cases.iter().map(|case| async move {
        let mut problems = Vec::new();
        for (index, commit) in case.commits.iter().enumerate() {
            let facts = commit
                .facts
                .iter()
                .map(|fact| CommitFact::new(fact, None))
                .collect();
            // The label differs from the recall suite's, so the idempotency keys never collide.
            ctx.establish(
                (&case.id, &format!("pages-{index}")),
                ScopeKind::Agent,
                (scope, &commit.content),
                facts,
                &mut problems,
            )
            .await
            .map_err(|e| e.to_string())?;
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        }
    });
    let results: Vec<Result<(), String>> = stream::iter(commits)
        .buffered(concurrency.max(1))
        .collect()
        .await;
    results.into_iter().collect()
}

async fn ask(
    ctx: &RunContext,
    question: &PageQuestion,
    scope: &str,
    detail: &mut QuestionDetail,
) -> Result<(), CaseError> {
    let response = ctx
        .client
        .recall_pages(scope, &question.query, FACT_LIMIT, PAGE_LIMIT)
        .await?;
    let expected_section = detail.expected_section.take();
    score_question(question, expected_section, &response.pages, detail);
    Ok(())
}

async fn compare(
    ctx: &RunContext,
    query: &str,
    scope: &str,
    detail: &mut FactsDetail,
) -> Result<(), CaseError> {
    let off = ctx.client.recall_pages(scope, query, FACT_LIMIT, 0).await?;
    let on = ctx
        .client
        .recall_pages(scope, query, FACT_LIMIT, PAGE_LIMIT)
        .await?;
    compare_facts(&off, &on, detail);
    Ok(())
}

/// A reviewed page, then the host's edit to it in a shared scope, which SPEC section 8 holds for
/// review. While held, readers and recall see only the reviewed version. Once approved, recall has
/// to show the edit, or its absence before would prove nothing.
async fn quarantine_steps(
    ctx: &RunContext,
    probe: &QuarantineProbe,
    scope: &str,
    detail: &mut QuarantineDetail,
) -> Result<(), CaseError> {
    let client = &ctx.client;
    let base = client
        .write_page(
            PageWriter::Admin,
            (scope, &probe.slug),
            &PageWrite {
                title: &probe.title,
                body: &probe.base_body,
                expected_version: None,
                approve: true,
            },
        )
        .await?;
    expect_status(&base, ACTIVE, "reviewed write", &mut detail.write_problems);
    detail.base_version = Some(base.version);

    let edit = client
        .write_page(
            PageWriter::Agent,
            (scope, &probe.slug),
            &PageWrite {
                title: &probe.title,
                body: &probe.edit_body,
                expected_version: Some(base.version),
                approve: false,
            },
        )
        .await?;
    detail.edit_version = Some(edit.version);
    if edit.status != QUARANTINED {
        detail.leaks.push(format!(
            "edit version {} written with status {} without review",
            edit.version, edit.status
        ));
    }
    if edit.trust != Trust::Agent {
        detail
            .write_problems
            .push(format!("edit stored with trust {:?}", edit.trust));
    }

    let current = client.read_page(scope, &probe.slug).await?;
    if current.version != base.version
        || probe.planted.iter().any(|p| contains_ci(&current.body, p))
    {
        detail.leaks.push(format!(
            "page read returned version {} while the edit was held",
            current.version
        ));
    }

    let held = client
        .recall_pages(scope, &probe.query, FACT_LIMIT, PAGE_LIMIT)
        .await?;
    detail.leaks.extend(held_edit_leaks(
        &held.pages,
        &probe.slug,
        edit.version,
        &probe.planted,
    ));
    if !held.pages.iter().any(|hit| hit.slug == probe.slug) {
        detail
            .control_problems
            .push("reviewed page missing from recall while the edit was held".to_owned());
    }
    detail.held_hits = held.pages.iter().map(HitSummary::from).collect();

    let pending = client.page_quarantine(scope).await?;
    if !pending
        .iter()
        .any(|version| version.page_id == edit.page_id && version.version == edit.version)
    {
        detail.write_problems.push(format!(
            "edit version {} is not in the page quarantine",
            edit.version
        ));
    }

    if edit.status == QUARANTINED {
        client.approve_page(edit.page_id, edit.version).await?;
        let approved = client
            .recall_pages(scope, &probe.query, FACT_LIMIT, PAGE_LIMIT)
            .await?;
        if !approved
            .pages
            .iter()
            .any(|hit| hit.slug == probe.slug && hit.version == edit.version)
        {
            detail
                .control_problems
                .push("approved edit missing from recall".to_owned());
        }
        detail.approved_hits = approved.pages.iter().map(HitSummary::from).collect();
    }
    detail.completed = true;
    Ok(())
}

fn expect_status(version: &PageVersion, expected: &str, label: &str, problems: &mut Vec<String>) {
    if version.status != expected {
        problems.push(format!(
            "{label}: version {} has status {}, expected {expected}",
            version.version, version.status
        ));
    }
}

/// Times `calls` recalls with `page_limit` 0 and as many with 3, over the same queries, one at a
/// time. The setting that goes first alternates, so neither always meets a warmer cache.
async fn measure_latency(ctx: &RunContext, scope: &str, queries: &[&str], calls: usize) -> Latency {
    let mut errors = 0;
    let mut warmup_calls = 0;
    for query in queries {
        for page_limit in [0, PAGE_LIMIT] {
            warmup_calls += 1;
            if ctx
                .client
                .recall_pages(scope, query, FACT_LIMIT, page_limit)
                .await
                .is_err()
            {
                errors += 1;
            }
        }
    }
    let mut off = Vec::with_capacity(calls);
    let mut on = Vec::with_capacity(calls);
    for index in 0..calls {
        let query = queries[index % queries.len()];
        let order = if index % 2 == 0 {
            [0, PAGE_LIMIT]
        } else {
            [PAGE_LIMIT, 0]
        };
        for page_limit in order {
            let started = Instant::now();
            let outcome = ctx
                .client
                .recall_pages(scope, query, FACT_LIMIT, page_limit)
                .await;
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            match outcome {
                Err(_) => errors += 1,
                Ok(_) if page_limit == 0 => off.push(ms),
                Ok(_) => on.push(ms),
            }
        }
    }
    Latency {
        calls,
        warmup_calls,
        errors,
        p50_ms_page_limit_0: percentile(&off, 0.50),
        p95_ms_page_limit_0: percentile(&off, 0.95),
        p50_ms_page_limit_3: percentile(&on, 0.50),
        p95_ms_page_limit_3: percentile(&on, 0.95),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Fact, ScoredFact};
    use chrono::Utc;

    fn hit(slug: &str, section: &str, version: i64, snippet: &str) -> PageHit {
        PageHit {
            page_id: Uuid::nil(),
            scope: "agent:x".to_owned(),
            slug: slug.to_owned(),
            title: "T".to_owned(),
            version,
            section: section.to_owned(),
            snippet: snippet.to_owned(),
            score: 0.1,
        }
    }

    fn question(section: Option<&str>) -> PageQuestion {
        PageQuestion {
            id: "q".to_owned(),
            lang: "en".to_owned(),
            slug: "runbook".to_owned(),
            query: "how".to_owned(),
            section: section.map(str::to_owned),
        }
    }

    fn scored_question(section: Option<&str>, hits: &[PageHit]) -> QuestionDetail {
        let q = question(section);
        let mut detail = QuestionDetail {
            expected_slug: q.slug.clone(),
            ..QuestionDetail::default()
        };
        score_question(&q, q.expected_section("Runbook"), hits, &mut detail);
        detail
    }

    #[test]
    fn a_page_in_the_top_three_without_a_section_passes() {
        let hits = [
            hit("a", "A", 1, ""),
            hit("b", "B", 1, ""),
            hit("runbook", "Runbook > Stop", 1, ""),
        ];
        let detail = scored_question(None, &hits);
        assert_eq!(detail.rank, Some(3));
        assert_eq!(detail.section_matched, None);
        assert!(detail.problems().is_empty());
    }

    #[test]
    fn a_named_section_must_be_the_top_hit() {
        let right = [hit("runbook", "Runbook > Start", 1, "")];
        assert!(scored_question(Some("Start"), &right).problems().is_empty());

        let wrong_section = [hit("runbook", "Runbook > Stop", 1, "")];
        let detail = scored_question(Some("Start"), &wrong_section);
        assert_eq!(detail.section_matched, Some(false));
        assert_eq!(detail.problems().len(), 1);

        // Right page and section, but not on top.
        let second = [
            hit("other", "Other > Start", 1, ""),
            hit("runbook", "Runbook > Start", 1, ""),
        ];
        assert_eq!(scored_question(Some("Start"), &second).problems().len(), 1);
    }

    #[test]
    fn a_missing_page_fails_and_no_hits_fail_a_section() {
        let detail = scored_question(Some("Start"), &[]);
        assert_eq!(detail.rank, None);
        assert_eq!(detail.problems().len(), 2);
    }

    fn response(ids: &[u128], pages: usize) -> RecallResponse {
        let facts = ids
            .iter()
            .map(|id| ScoredFact {
                fact: Fact {
                    id: Uuid::from_u128(*id),
                    scope: "agent:x".to_owned(),
                    subject_id: Uuid::nil(),
                    predicate: "p".to_owned(),
                    statement: "s".to_owned(),
                    trust: Trust::Agent,
                    status: ACTIVE.to_owned(),
                    valid_from: Utc::now(),
                    valid_to: None,
                    recorded_at: Utc::now(),
                    superseded_by: None,
                    pending_supersede_of: None,
                    source_episode_ids: Vec::new(),
                },
                score: 0.5,
            })
            .collect();
        RecallResponse {
            facts,
            pages: (0..pages).map(|_| hit("p", "P", 1, "")).collect(),
            pages_skipped: None,
        }
    }

    #[test]
    fn facts_must_match_in_order() {
        let mut same = FactsDetail::default();
        compare_facts(&response(&[1, 2], 0), &response(&[1, 2], 3), &mut same);
        assert!(same.problems().is_empty());

        let mut swapped = FactsDetail::default();
        compare_facts(&response(&[1, 2], 0), &response(&[2, 1], 3), &mut swapped);
        assert_eq!(swapped.problems().len(), 1);
    }

    #[test]
    fn pages_with_page_limit_zero_fail() {
        let mut detail = FactsDetail::default();
        compare_facts(&response(&[1], 1), &response(&[1], 1), &mut detail);
        assert_eq!(detail.problems().len(), 1);
    }

    #[test]
    fn an_uncompared_case_gets_no_credit() {
        assert_eq!(FactsDetail::default().problems().len(), 1);
        assert_eq!(QuarantineDetail::default().problems().len(), 1);
    }

    #[test]
    fn a_held_edit_leaks_by_version_or_planted_text() {
        let planted = vec!["relay-7731".to_owned()];
        let base = [hit(
            "policy",
            "Policy > Escalation",
            1,
            "goes to the secondary",
        )];
        assert!(held_edit_leaks(&base, "policy", 2, &planted).is_empty());
        let by_version = [hit("policy", "Policy > Escalation", 2, "x")];
        assert_eq!(held_edit_leaks(&by_version, "policy", 2, &planted).len(), 1);
        let by_text = [hit("other", "Other", 1, "send to RELAY-7731")];
        assert_eq!(held_edit_leaks(&by_text, "policy", 2, &planted).len(), 1);
    }
}
