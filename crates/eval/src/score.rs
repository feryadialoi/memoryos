//! Pure scoring: matching, leak detection and per-suite metrics and gates.
//!
//! Every rate divides by all cases, including errored ones, so an error can only lower a score.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;
use uuid::Uuid;

use crate::client::{RecallResponse, Trust};
use crate::dataset::{PAGES_DATASET, POISONING_DATASET, RECALL_DATASET, STALENESS_DATASET};
use crate::insights::{
    InsightDetail, InsightsRun, CLASS_CONFIRM, CLASS_RECALL, CLASS_REFLECT, CLASS_STALENESS,
    CLASS_TRUST, INSIGHTS_DATASET,
};
use crate::pages::{PagesRun, QuestionDetail, PAGE_TOP_K};
use crate::report::{Counts, Gate, SuiteReport};
use crate::suites::{
    CaseResult, CaseStatus, PoisoningDetail, RecallDetail, StalenessDetail, Verdict,
};

/// Recall is measured at this depth.
pub const TOP_K: usize = 5;

const RECALL_PROVES: &str =
    "Retrieval quality of hybrid search, with the embedder the server runs, \
over hand-written statements in one shared scope. It does not measure how well a host model writes \
statements, and the corpus is small.";
const STALENESS_PROVES: &str =
    "A supersede found through /v1/candidates closes the old fact, current \
recall excludes it and as_of returns it. These are filters, so results are not ranking evidence.";
const POISONING_PROVES: &str =
    "Commits by low-trust keys stay out of recall and candidates until an \
admin approves them, cannot close a fact of higher trust, and purge removes them and restores what \
they closed. The highest trust a commit can have is agent, so this covers external against agent, \
never user or system. Far-future dating is refused, so sleepers are probed at their later dates. It does not test whether a model would extract or obey the planted text.";

const PAGES_PROVES: &str =
    "Page search finds the page, and the section, that answers a question, among 21 \
hand-written pages in English, Chinese and Indonesian that share a scope with the recall corpus. A \
host's edit to a reviewed page in a shared scope stays out of page reads and recall until approved, \
and shows up once it is. Fact results are the same with page_limit 0 and 3. It does not measure \
how well a model writes pages, the pages are short, and the questions were written by the same \
author as the pages.";

const INSIGHTS_PROVES: &str =
    "Every staleness event of SPEC section 8 marks exactly the insights citing the \
changed source stale, with the right change, and carries it up every level; corroboration and \
changes to uncited facts change nothing. Insights from the external key, citing an external fact, \
or written by the host in a shared scope are held until approved. Confirm returns a stale insight \
to active. Recall returns active insights only, respects insight_limit, skips insights on time \
travel, and returns the same facts with insight_limit 0 and 3. The reflect batch offers a topic at \
the threshold, honours a decline until the fact set changes, and lists stale insights first. Every \
insight is scripted, so it says nothing about how well a model writes insights. The host's key has \
agent trust, so the guidance rule is not separated from the shared scope rule.";

pub fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

/// The 1-based rank at which the statements so far first cover every expected value.
pub fn rank_of_set(statements: &[String], expected: &[String]) -> Option<usize> {
    if expected.is_empty() {
        return None;
    }
    let mut rank = 0;
    for value in expected {
        let first_hit = statements.iter().position(|s| contains_ci(s, value))?;
        rank = rank.max(first_hit + 1);
    }
    Some(rank)
}

#[derive(Debug, Clone, Copy)]
pub enum Stage {
    Held,
    Candidates,
    AsOf(DateTime<Utc>),
    AfterPurge,
}

impl Stage {
    fn label(self) -> String {
        match self {
            Self::Held => "recall while held".to_owned(),
            Self::Candidates => "candidates while held".to_owned(),
            Self::AsOf(at) => format!("recall as_of {}", at.to_rfc3339()),
            Self::AfterPurge => "recall after purge".to_owned(),
        }
    }
}

/// Everything in a recall or candidates response that exposes the poison.
///
/// The planted string alone would miss a paraphrase, so a fact also leaks if it is not `active`,
/// if it has `external` trust, or if the poison is its only source. A fact that lists the poison
/// alongside another source is not a leak: that is corroboration of a fact already known.
pub fn find_leaks(
    response: &RecallResponse,
    planted: &[String],
    poison_episode: Uuid,
    stage: Stage,
) -> Vec<String> {
    let label = stage.label();
    let mut leaks = Vec::new();
    for scored in &response.facts {
        let fact = &scored.fact;
        let has_planted = planted.iter().any(|p| contains_ci(&fact.statement, p));
        let only_from_poison = !fact.source_episode_ids.is_empty()
            && fact
                .source_episode_ids
                .iter()
                .all(|id| *id == poison_episode);
        if has_planted
            || only_from_poison
            || fact.status != "active"
            || fact.trust == Trust::External
        {
            leaks.push(format!(
                "fact {} in {label} (status {}): {}",
                fact.id, fact.status, fact.statement
            ));
        }
    }
    leaks
}

fn count<D>(results: &[CaseResult<D>]) -> Counts {
    let mut counts = Counts {
        total: results.len(),
        ..Counts::default()
    };
    for result in results {
        match result.status {
            CaseStatus::Passed => counts.passed += 1,
            CaseStatus::Failed => counts.failed += 1,
            CaseStatus::Error => counts.errors += 1,
        }
    }
    counts
}

fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        return 0.0;
    }
    numerator as f64 / denominator as f64
}

fn no_errors_gate(suite: &str, counts: Counts) -> Gate {
    Gate::at_most(format!("{suite}.errors"), counts.errors as f64, 0.0)
}

fn build<D: Serialize>(
    (suite, dataset, proves): (&str, &str, &str),
    results: &[CaseResult<D>],
    metrics: serde_json::Value,
    gates: Vec<Gate>,
) -> Result<SuiteReport, serde_json::Error> {
    let cases = results
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(SuiteReport::new(
        suite,
        dataset,
        proves,
        count(results),
        metrics,
        gates,
        cases,
    ))
}

pub fn recall_at_k(results: &[CaseResult<RecallDetail>]) -> f64 {
    ratio(count(results).passed, results.len())
}

pub fn mean_reciprocal_rank(results: &[CaseResult<RecallDetail>]) -> f64 {
    if results.is_empty() {
        return 0.0;
    }
    let sum: f64 = results
        .iter()
        .filter(|r| r.status != CaseStatus::Error)
        .filter_map(|r| r.detail.rank)
        .map(|rank| 1.0 / rank as f64)
        .sum();
    sum / results.len() as f64
}

pub fn recall_report(
    results: &[CaseResult<RecallDetail>],
    min_recall: f64,
) -> Result<SuiteReport, serde_json::Error> {
    let counts = count(results);
    let recall = recall_at_k(results);
    let corpus: usize = results.iter().map(|r| r.detail.committed_facts).sum();
    let metrics = json!({
        "recall_at_5": recall,
        "mrr": mean_reciprocal_rank(results),
        "corpus_facts": corpus,
    });
    let gates = vec![
        no_errors_gate("recall", counts),
        Gate::at_least("recall.recall_at_5", recall, min_recall),
    ];
    build(
        ("recall", RECALL_DATASET, RECALL_PROVES),
        results,
        metrics,
        gates,
    )
}

pub fn staleness_report(
    results: &[CaseResult<StalenessDetail>],
    min_staleness: f64,
) -> Result<SuiteReport, serde_json::Error> {
    let counts = count(results);
    let pass_rate = ratio(counts.passed, counts.total);
    let rate = |check: fn(&StalenessDetail) -> bool| {
        let ok = results
            .iter()
            .filter(|r| r.status != CaseStatus::Error && check(&r.detail))
            .count();
        ratio(ok, counts.total)
    };
    let metrics = json!({
        "pass_rate": pass_rate,
        "candidate_found_rate": rate(|d| d.candidate_found),
        "old_closed_rate": rate(|d| d.old_closed),
        "new_in_current_rate": rate(|d| d.new_in_current),
        "old_absent_from_current_rate": rate(|d| d.old_absent_from_current),
        "old_in_as_of_rate": rate(|d| d.old_in_as_of),
        "new_absent_from_as_of_rate": rate(|d| d.new_absent_from_as_of),
    });
    let gates = vec![
        no_errors_gate("staleness", counts),
        Gate::at_least("staleness.pass_rate", pass_rate, min_staleness),
    ];
    build(
        ("staleness", STALENESS_DATASET, STALENESS_PROVES),
        results,
        metrics,
        gates,
    )
}

pub fn poisoning_report(
    results: &[CaseResult<PoisoningDetail>],
) -> Result<SuiteReport, serde_json::Error> {
    let counts = count(results);
    // Leaks seen before a later step errored still count.
    let leaks: usize = results.iter().map(|r| r.detail.leaks.len()).sum();
    let leaking_cases = results
        .iter()
        .filter(|r| !r.detail.leaks.is_empty())
        .count();
    let true_fact_lost = results
        .iter()
        .filter(|r| !r.detail.true_fact_problems.is_empty())
        .count();
    let purge_failures = results
        .iter()
        .filter(|r| !r.detail.purge_problems.is_empty())
        .count();
    let quarantined_cases = results
        .iter()
        .filter(|r| r.detail.quarantined_found > 0)
        .count();
    let purge_exercised = results
        .iter()
        .filter(|r| r.detail.quarantined_found > 0 && r.detail.purge_completed)
        .count();
    let control_failures = results
        .iter()
        .filter(|r| !r.detail.control_problems.is_empty())
        .count();
    let truth_restored = results.iter().filter(|r| r.detail.truth_restored).count();
    let metrics = json!({
        "leaks": leaks,
        "leaking_cases": leaking_cases,
        "true_fact_lost_cases": true_fact_lost,
        "purge_failure_cases": purge_failures,
        "quarantined_cases": quarantined_cases,
        "purge_exercised_cases": purge_exercised,
        "control_failure_cases": control_failures,
        "truth_restored_cases": truth_restored,
    });
    let gates = vec![
        no_errors_gate("poisoning", counts),
        Gate::at_most("poisoning.leaks", leaks as f64, 0.0),
        Gate::at_most("poisoning.failed_cases", counts.failed as f64, 0.0),
        // Zero would mean purge was never run against a real quarantined fact, so the suite
        // proved nothing about it, even if every case passed.
        Gate::at_least(
            "poisoning.purge_exercised_cases",
            purge_exercised as f64,
            1.0,
        ),
    ];
    build(
        ("poisoning", POISONING_DATASET, POISONING_PROVES),
        results,
        metrics,
        gates,
    )
}

/// Nearest-rank percentile of `values`, `p` between 0 and 1. 0 for no values.
pub fn percentile(values: &[f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = (p * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn round_ms(ms: f64) -> f64 {
    (ms * 10.0).round() / 10.0
}

pub fn pages_report(run: &PagesRun, min_page_hit: f64) -> Result<SuiteReport, serde_json::Error> {
    let questions = count(&run.questions);
    let facts = count(&run.facts);
    let quarantine = count(&run.quarantine);
    let counts = Counts {
        total: questions.total + facts.total + quarantine.total,
        passed: questions.passed + facts.passed + quarantine.passed,
        failed: questions.failed + facts.failed + quarantine.failed,
        errors: questions.errors + facts.errors + quarantine.errors,
    };
    let answered = |r: &&CaseResult<QuestionDetail>| r.status != CaseStatus::Error;
    let ranked_within = |k: usize| {
        run.questions
            .iter()
            .filter(answered)
            .filter(|r| r.detail.rank.is_some_and(|rank| rank <= k))
            .count()
    };
    let mrr = if run.questions.is_empty() {
        0.0
    } else {
        run.questions
            .iter()
            .filter(answered)
            .filter_map(|r| r.detail.rank)
            .filter(|rank| *rank <= PAGE_TOP_K)
            .map(|rank| 1.0 / rank as f64)
            .sum::<f64>()
            / run.questions.len() as f64
    };
    let with_section = run
        .questions
        .iter()
        .filter(|r| r.detail.expected_section.is_some())
        .count();
    let section_hits = run
        .questions
        .iter()
        .filter(answered)
        .filter(|r| r.detail.section_matched == Some(true))
        .count();
    let page_top3 = ratio(ranked_within(PAGE_TOP_K), questions.total);
    let facts_changed = run
        .facts
        .iter()
        .filter(|r| r.status != CaseStatus::Error && !r.detail.problems().is_empty())
        .count();
    let leaks: usize = run.quarantine.iter().map(|r| r.detail.leaks.len()).sum();
    let control_failures = run
        .quarantine
        .iter()
        .filter(|r| !r.detail.control_problems.is_empty() || !r.detail.completed)
        .count();

    let mut metrics = json!({
        "questions": questions.total,
        "page_top1_rate": ratio(ranked_within(1), questions.total),
        "page_top3_rate": page_top3,
        "page_mrr": mrr,
        "section_questions": with_section,
        "section_top1_rate": ratio(section_hits, with_section),
        "facts_cases": facts.total,
        "facts_unchanged_rate": ratio(facts.passed, facts.total),
        "quarantine_leaks": leaks,
        "quarantine_control_failures": control_failures,
    });
    if let Some(latency) = &run.latency {
        metrics["latency_calls"] = json!(latency.calls);
        metrics["latency_errors"] = json!(latency.errors);
        metrics["latency_p50_ms_page_limit_0"] = json!(round_ms(latency.p50_ms_page_limit_0));
        metrics["latency_p95_ms_page_limit_0"] = json!(round_ms(latency.p95_ms_page_limit_0));
        metrics["latency_p50_ms_page_limit_3"] = json!(round_ms(latency.p50_ms_page_limit_3));
        metrics["latency_p95_ms_page_limit_3"] = json!(round_ms(latency.p95_ms_page_limit_3));
    }

    let gates = vec![
        no_errors_gate("pages", counts),
        Gate::at_most("pages.quarantine_leaks", leaks as f64, 0.0),
        // Without a working control, a held edit that never showed proves nothing.
        Gate::at_most(
            "pages.quarantine_control_failures",
            control_failures as f64,
            0.0,
        ),
        // SPEC section 7: fact results are the same with page_limit 0 and 3.
        Gate::at_most("pages.facts_changed_cases", facts_changed as f64, 0.0),
        Gate::at_least("pages.page_top3_rate", page_top3, min_page_hit),
    ];

    let mut cases = Vec::new();
    for value in run.questions.iter().map(serde_json::to_value) {
        cases.push(value?);
    }
    for value in run.facts.iter().map(serde_json::to_value) {
        cases.push(value?);
    }
    for value in run.quarantine.iter().map(serde_json::to_value) {
        cases.push(value?);
    }
    Ok(SuiteReport::new(
        "pages",
        PAGES_DATASET,
        PAGES_PROVES,
        counts,
        metrics,
        gates,
        cases,
    ))
}

pub fn insights_report(run: &InsightsRun) -> Result<SuiteReport, serde_json::Error> {
    let scripted = count(&run.cases);
    let facts = count(&run.facts);
    let counts = Counts {
        total: scripted.total + facts.total,
        passed: scripted.passed + facts.passed,
        failed: scripted.failed + facts.failed,
        errors: scripted.errors + facts.errors,
    };
    let mut metrics = json!({
        "cases": scripted.total,
        "cases_passed": scripted.passed,
        "checks": run.cases.iter().map(|r| r.detail.checks).sum::<usize>(),
        "facts_cases": facts.total,
        "facts_unchanged_rate": ratio(facts.passed, facts.total),
        "corpus_insights": run.corpus_insights,
    });
    for class in [
        CLASS_STALENESS,
        CLASS_TRUST,
        CLASS_CONFIRM,
        CLASS_RECALL,
        CLASS_REFLECT,
    ] {
        let of_class: Vec<&CaseResult<InsightDetail>> = run
            .cases
            .iter()
            .filter(|r| r.class.as_deref() == Some(class))
            .collect();
        let passed = of_class
            .iter()
            .filter(|r| r.status == CaseStatus::Passed)
            .count();
        metrics[format!("{class}_passed")] = json!(passed);
        metrics[format!("{class}_cases")] = json!(of_class.len());
    }
    if let Some(latency) = &run.latency {
        metrics["latency_calls"] = json!(latency.calls);
        metrics["latency_errors"] = json!(latency.errors);
        metrics["latency_p50_ms_insight_limit_0"] = json!(round_ms(latency.p50_ms_insight_limit_0));
        metrics["latency_p95_ms_insight_limit_0"] = json!(round_ms(latency.p95_ms_insight_limit_0));
        metrics["latency_p50_ms_insight_limit_3"] = json!(round_ms(latency.p50_ms_insight_limit_3));
        metrics["latency_p95_ms_insight_limit_3"] = json!(round_ms(latency.p95_ms_insight_limit_3));
    }
    let facts_changed = run
        .facts
        .iter()
        .filter(|r| r.status == CaseStatus::Failed)
        .count();
    let gates = vec![
        no_errors_gate("insights", counts),
        // Every scripted check is a SPEC requirement, so one wrong answer fails the suite.
        Gate::at_most("insights.failed_cases", scripted.failed as f64, 0.0),
        // SPEC section 8: facts are ranked as before.
        Gate::at_most("insights.facts_changed_cases", facts_changed as f64, 0.0),
    ];

    let mut cases = Vec::new();
    for value in run.cases.iter().map(serde_json::to_value) {
        cases.push(value?);
    }
    for value in run.facts.iter().map(serde_json::to_value) {
        cases.push(value?);
    }
    Ok(SuiteReport::new(
        "insights",
        INSIGHTS_DATASET,
        INSIGHTS_PROVES,
        counts,
        metrics,
        gates,
        cases,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::client::{Fact, ScoredFact};
    use crate::suites::CaseStatus;

    fn s(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).to_owned()).collect()
    }

    #[test]
    fn matching_ignores_case() {
        assert!(contains_ci("Lives in DENVER", "denver"));
        assert!(!contains_ci("Lives in Denver", "boulder"));
    }

    #[test]
    fn rank_is_where_the_whole_set_is_first_covered() {
        let statements = s(&["noise", "Ivan Petrov manages it", "ends in November"]);
        assert_eq!(rank_of_set(&statements, &s(&["Ivan"])), Some(2));
        assert_eq!(rank_of_set(&statements, &s(&["November", "Ivan"])), Some(3));
    }

    #[test]
    fn rank_is_none_when_any_value_is_missing() {
        let statements = s(&["Ivan Petrov manages it"]);
        assert_eq!(rank_of_set(&statements, &s(&["Ivan", "November"])), None);
        assert_eq!(rank_of_set(&statements, &[]), None);
    }

    fn result<D>(status: CaseStatus, detail: D) -> CaseResult<D> {
        CaseResult {
            id: "x".to_owned(),
            lang: "en".to_owned(),
            class: None,
            scope: "user:x".to_owned(),
            status,
            reason: None,
            duration_ms: 0,
            detail,
        }
    }

    fn recall_result(status: CaseStatus, rank: Option<usize>) -> CaseResult<RecallDetail> {
        result(
            status,
            RecallDetail {
                rank,
                ..RecallDetail::default()
            },
        )
    }

    #[test]
    fn recall_metrics_count_errors_in_the_denominator() {
        let results = vec![
            recall_result(CaseStatus::Passed, Some(1)),
            recall_result(CaseStatus::Passed, Some(2)),
            recall_result(CaseStatus::Failed, Some(8)),
            recall_result(CaseStatus::Error, None),
        ];
        assert!((recall_at_k(&results) - 0.5).abs() < 1e-9);
        let expected_mrr = (1.0 + 0.5 + 0.125) / 4.0;
        assert!((mean_reciprocal_rank(&results) - expected_mrr).abs() < 1e-9);
    }

    #[test]
    fn an_errored_case_earns_no_reciprocal_rank() {
        let results = vec![recall_result(CaseStatus::Error, Some(1))];
        assert_eq!(mean_reciprocal_rank(&results), 0.0);
    }

    #[test]
    fn empty_results_score_zero_and_fail_a_gate() {
        assert_eq!(recall_at_k(&[]), 0.0);
        let report = staleness_report(&[], 0.95).expect("report");
        assert!(!report.passed);
    }

    #[test]
    fn recall_gate_fails_on_errors_even_with_zero_threshold() {
        let results = vec![
            recall_result(CaseStatus::Passed, Some(1)),
            recall_result(CaseStatus::Error, None),
        ];
        let report = recall_report(&results, 0.0).expect("report");
        assert!(!report.passed);
    }

    #[test]
    fn recall_gate_applies_the_threshold() {
        let results = vec![
            recall_result(CaseStatus::Passed, Some(1)),
            recall_result(CaseStatus::Failed, None),
        ];
        assert!(recall_report(&results, 0.5).expect("report").passed);
        assert!(!recall_report(&results, 0.6).expect("report").passed);
    }

    fn stale_good() -> StalenessDetail {
        StalenessDetail {
            new_in_current: true,
            old_absent_from_current: true,
            old_in_as_of: true,
            new_absent_from_as_of: true,
            ..StalenessDetail::default()
        }
    }

    fn stale_results(
        passed: usize,
        failed: usize,
        errors: usize,
    ) -> Vec<CaseResult<StalenessDetail>> {
        let mut results = Vec::new();
        results.extend((0..passed).map(|_| result(CaseStatus::Passed, stale_good())));
        results.extend((0..failed).map(|_| result(CaseStatus::Failed, StalenessDetail::default())));
        results.extend((0..errors).map(|_| result(CaseStatus::Error, StalenessDetail::default())));
        results
    }

    #[test]
    fn staleness_gate_holds_at_the_threshold() {
        assert!(
            staleness_report(&stale_results(19, 1, 0), 0.95)
                .expect("report")
                .passed
        );
        assert!(
            !staleness_report(&stale_results(18, 2, 0), 0.95)
                .expect("report")
                .passed
        );
    }

    #[test]
    fn staleness_errors_fail_the_gate_even_at_zero_threshold() {
        assert!(
            !staleness_report(&stale_results(19, 0, 1), 0.0)
                .expect("report")
                .passed
        );
    }

    #[test]
    fn staleness_rates_give_errored_cases_no_credit() {
        let report = staleness_report(&stale_results(1, 0, 1), 0.0).expect("report");
        assert_eq!(report.metrics["new_in_current_rate"], json!(0.5));
    }

    fn poison_detail(leaks: &[&str], quarantined_found: usize) -> PoisoningDetail {
        PoisoningDetail {
            leaks: s(leaks),
            quarantined_found,
            purge_completed: true,
            ..PoisoningDetail::default()
        }
    }

    #[test]
    fn poisoning_passes_with_no_leaks_and_purge_exercised() {
        let results = vec![result(CaseStatus::Passed, poison_detail(&[], 1))];
        assert!(poisoning_report(&results).expect("report").passed);
    }

    #[test]
    fn one_leak_fails_the_poisoning_gate() {
        let results = vec![
            result(CaseStatus::Passed, poison_detail(&[], 1)),
            result(CaseStatus::Failed, poison_detail(&["leak"], 1)),
        ];
        let report = poisoning_report(&results).expect("report");
        assert!(!report.passed);
        assert_eq!(report.metrics["leaks"], json!(1));
    }

    #[test]
    fn a_leak_seen_before_an_error_still_counts() {
        let results = vec![
            result(CaseStatus::Passed, poison_detail(&[], 1)),
            result(CaseStatus::Error, poison_detail(&["leak"], 0)),
        ];
        assert_eq!(
            poisoning_report(&results).expect("report").metrics["leaks"],
            json!(1)
        );
    }

    #[test]
    fn a_suite_that_never_exercised_purge_fails() {
        let results = vec![result(CaseStatus::Passed, poison_detail(&[], 0))];
        assert!(!poisoning_report(&results).expect("report").passed);
    }

    fn fact(statement: &str, status: &str, trust: Trust, sources: Vec<Uuid>) -> ScoredFact {
        ScoredFact {
            fact: Fact {
                id: Uuid::nil(),
                scope: "user:x".to_owned(),
                subject_id: Uuid::nil(),
                predicate: "p".to_owned(),
                statement: statement.to_owned(),
                trust,
                status: status.to_owned(),
                valid_from: Utc::now(),
                valid_to: None,
                recorded_at: Utc::now(),
                superseded_by: None,
                pending_supersede_of: None,
                source_episode_ids: sources,
            },
            score: 1.0,
        }
    }

    fn leaks_in(facts: Vec<ScoredFact>, stage: Stage) -> Vec<String> {
        find_leaks(
            &RecallResponse {
                facts,
                pages: Vec::new(),
                pages_skipped: None,
                insights: Vec::new(),
                insights_skipped: None,
            },
            &s(&["Reykjavik"]),
            poison(),
            stage,
        )
    }

    fn poison() -> Uuid {
        Uuid::from_u128(1)
    }

    fn seed() -> Uuid {
        Uuid::from_u128(2)
    }

    #[test]
    fn the_true_fact_is_not_a_leak() {
        let facts = vec![fact(
            "Lives in Semarang",
            "active",
            Trust::Agent,
            vec![seed()],
        )];
        assert!(leaks_in(facts, Stage::Held).is_empty());
    }

    #[test]
    fn a_planted_statement_is_a_leak_in_any_case() {
        let facts = vec![fact(
            "Lives in REYKJAVIK",
            "active",
            Trust::Agent,
            vec![seed()],
        )];
        assert_eq!(leaks_in(facts, Stage::Held).len(), 1);
    }

    #[test]
    fn a_paraphrase_sourced_only_from_the_poison_is_a_leak() {
        let facts = vec![fact(
            "Resides in Iceland",
            "active",
            Trust::Agent,
            vec![poison()],
        )];
        assert_eq!(leaks_in(facts, Stage::Candidates).len(), 1);
    }

    #[test]
    fn a_quarantined_fact_in_recall_is_a_leak() {
        let facts = vec![fact("Anything", "quarantined", Trust::Agent, vec![seed()])];
        assert_eq!(leaks_in(facts, Stage::Held).len(), 1);
    }

    #[test]
    fn an_active_external_fact_is_a_leak() {
        let facts = vec![fact("Anything", "active", Trust::External, vec![seed()])];
        assert_eq!(leaks_in(facts, Stage::AfterPurge).len(), 1);
    }

    #[test]
    fn a_leak_names_where_it_was_seen() {
        let facts = vec![fact("Reykjavik", "active", Trust::Agent, vec![seed()])];
        let leaks = leaks_in(facts, Stage::Candidates);
        assert!(leaks[0].contains("candidates while held"), "{leaks:?}");
    }

    #[test]
    fn corroboration_alongside_the_seed_is_not_a_leak() {
        let facts = vec![fact(
            "Lives in Semarang",
            "active",
            Trust::Agent,
            vec![seed(), poison()],
        )];
        assert!(leaks_in(facts, Stage::Held).is_empty());
    }

    #[test]
    fn a_control_failure_fails_the_poisoning_gate() {
        let mut detail = poison_detail(&[], 1);
        detail.control_problems = s(&["approved claim missing"]);
        let results = vec![result(CaseStatus::Failed, detail)];
        let report = poisoning_report(&results).expect("report");
        assert!(!report.passed);
        assert_eq!(report.metrics["control_failure_cases"], json!(1));
    }

    #[test]
    fn percentile_uses_the_nearest_rank() {
        let values: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&values, 0.50), 50.0);
        assert_eq!(percentile(&values, 0.95), 95.0);
        assert_eq!(percentile(&[7.0], 0.95), 7.0);
        assert_eq!(percentile(&[], 0.5), 0.0);
        assert_eq!(percentile(&[3.0, 1.0, 2.0], 0.5), 2.0);
    }

    use crate::pages::{FactsDetail, Latency, QuarantineDetail};

    fn question_result(
        status: CaseStatus,
        rank: Option<usize>,
        section: Option<bool>,
    ) -> CaseResult<QuestionDetail> {
        result(
            status,
            QuestionDetail {
                expected_slug: "p".to_owned(),
                expected_section: section.map(|_| "P > S".to_owned()),
                rank,
                section_matched: section,
                ..QuestionDetail::default()
            },
        )
    }

    fn good_quarantine() -> CaseResult<QuarantineDetail> {
        result(
            CaseStatus::Passed,
            QuarantineDetail {
                completed: true,
                ..QuarantineDetail::default()
            },
        )
    }

    fn same_facts() -> CaseResult<FactsDetail> {
        result(
            CaseStatus::Passed,
            FactsDetail {
                compared: true,
                ..FactsDetail::default()
            },
        )
    }

    fn pages_run(questions: Vec<CaseResult<QuestionDetail>>) -> PagesRun {
        PagesRun {
            questions,
            facts: vec![same_facts()],
            quarantine: vec![good_quarantine()],
            latency: None,
        }
    }

    #[test]
    fn pages_metrics_count_ranks_sections_and_errors() {
        let run = pages_run(vec![
            question_result(CaseStatus::Passed, Some(1), Some(true)),
            question_result(CaseStatus::Passed, Some(3), None),
            question_result(CaseStatus::Failed, Some(2), Some(false)),
            question_result(CaseStatus::Error, Some(1), Some(true)),
        ]);
        let report = pages_report(&run, 0.0).expect("report");
        assert_eq!(report.metrics["page_top1_rate"], json!(0.25));
        assert_eq!(report.metrics["page_top3_rate"], json!(0.75));
        assert_eq!(report.metrics["section_questions"], json!(3));
        assert!(
            (report.metrics["section_top1_rate"].as_f64().expect("rate") - 1.0 / 3.0).abs() < 1e-9
        );
        let mrr = (1.0 + 1.0 / 3.0 + 0.5) / 4.0;
        assert!((report.metrics["page_mrr"].as_f64().expect("mrr") - mrr).abs() < 1e-9);
        assert_eq!(report.total, 6);
        // The errored question fails the errors gate.
        assert!(!report.passed);
    }

    #[test]
    fn pages_pass_with_no_errors_leaks_or_changed_facts() {
        let run = pages_run(vec![question_result(CaseStatus::Failed, None, None)]);
        assert!(pages_report(&run, 0.0).expect("report").passed);
        assert!(!pages_report(&run, 0.5).expect("report").passed);
    }

    #[test]
    fn a_leaked_edit_or_a_failed_control_fails_pages() {
        let mut run = pages_run(vec![question_result(CaseStatus::Passed, Some(1), None)]);
        run.quarantine[0].detail.leaks.push("leak".to_owned());
        assert!(!pages_report(&run, 0.0).expect("report").passed);

        let mut run = pages_run(vec![question_result(CaseStatus::Passed, Some(1), None)]);
        run.quarantine[0]
            .detail
            .control_problems
            .push("missing".to_owned());
        let report = pages_report(&run, 0.0).expect("report");
        assert!(!report.passed);
        assert_eq!(report.metrics["quarantine_control_failures"], json!(1));
    }

    #[test]
    fn changed_facts_fail_pages_and_latency_is_reported() {
        let mut run = pages_run(vec![question_result(CaseStatus::Passed, Some(1), None)]);
        run.facts[0].detail.with_pages.push(Uuid::nil());
        run.facts[0].status = CaseStatus::Failed;
        run.latency = Some(Latency {
            calls: 100,
            warmup_calls: 10,
            errors: 0,
            p50_ms_page_limit_0: 12.34,
            p95_ms_page_limit_0: 20.0,
            p50_ms_page_limit_3: 15.0,
            p95_ms_page_limit_3: 25.06,
        });
        let report = pages_report(&run, 0.0).expect("report");
        assert!(!report.passed);
        assert_eq!(report.metrics["facts_unchanged_rate"], json!(0.0));
        assert_eq!(report.metrics["latency_p50_ms_page_limit_0"], json!(12.3));
        assert_eq!(report.metrics["latency_p95_ms_page_limit_3"], json!(25.1));
    }

    fn insight_case(class: &str, status: CaseStatus) -> CaseResult<InsightDetail> {
        let mut case = result(
            status,
            InsightDetail {
                checks: 2,
                completed: true,
                ..InsightDetail::default()
            },
        );
        case.class = Some(class.to_owned());
        case
    }

    fn insights_run(
        cases: Vec<CaseResult<InsightDetail>>,
        facts_status: CaseStatus,
    ) -> InsightsRun {
        InsightsRun {
            cases,
            facts: vec![result(
                facts_status,
                crate::insights::FactsDetail {
                    compared: true,
                    ..crate::insights::FactsDetail::default()
                },
            )],
            corpus_insights: 1,
            latency: None,
        }
    }

    #[test]
    fn insights_pass_only_when_every_scripted_case_passes() {
        let good = insights_run(
            vec![
                insight_case(CLASS_STALENESS, CaseStatus::Passed),
                insight_case(CLASS_REFLECT, CaseStatus::Passed),
            ],
            CaseStatus::Passed,
        );
        let report = insights_report(&good).expect("report");
        assert!(report.passed);
        assert_eq!(report.total, 3);
        assert_eq!(report.metrics["staleness_passed"], json!(1));
        assert_eq!(report.metrics["checks"], json!(4));

        let wrong = insights_run(
            vec![insight_case(CLASS_TRUST, CaseStatus::Failed)],
            CaseStatus::Passed,
        );
        assert!(!insights_report(&wrong).expect("report").passed);
        let errored = insights_run(
            vec![insight_case(CLASS_TRUST, CaseStatus::Error)],
            CaseStatus::Passed,
        );
        assert!(!insights_report(&errored).expect("report").passed);
        let facts_changed = insights_run(
            vec![insight_case(CLASS_RECALL, CaseStatus::Passed)],
            CaseStatus::Failed,
        );
        assert!(!insights_report(&facts_changed).expect("report").passed);
    }

    #[test]
    fn every_report_says_what_it_proves() {
        let report = recall_report(&[], 0.0).expect("report");
        assert!(report.proves.contains("hybrid search"), "{}", report.proves);
    }
}
