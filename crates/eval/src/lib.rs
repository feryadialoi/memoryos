//! Black-box evaluation harness for MemoryOS.
//!
//! Talks to a running server only through the HTTP contract in `docs/openapi.yaml`, and scores it
//! against the production gates in `docs/SPEC.md` section 12. Every fact is a scripted commit, so a
//! run makes no model call of its own and costs nothing beyond what the server does for itself.

pub mod client;
pub mod dataset;
pub mod insights;
pub mod pages;
pub mod report;
pub mod score;
pub mod suites;

use std::path::PathBuf;

use chrono::Utc;
use clap::{Parser, ValueEnum};

use client::{ApiKeys, Client};
use report::{Report, SuiteReport};
use suites::RunContext;

#[derive(Parser)]
#[command(
    name = "memoryos-eval",
    about = "Black-box recall, staleness, poisoning, pages and insights evaluation of a MemoryOS server"
)]
pub struct Args {
    #[arg(long)]
    pub base_url: String,
    /// Bearer token of the host: scopes `user:*`, `shared:*`, `agent:*`, `max_trust` of `agent`.
    #[arg(long)]
    pub api_key: String,
    /// Bearer token for low-trust commits: the same scopes, `max_trust` of `external`.
    #[arg(long)]
    pub external_api_key: String,
    /// Bearer token with `is_admin` and the same scopes: quarantine lookup, approval and purge.
    #[arg(long)]
    pub admin_api_key: String,
    /// One suite, a comma-separated list, or `all`.
    #[arg(long, value_enum, value_delimiter = ',', required = true)]
    pub suite: Vec<Suite>,
    #[arg(long)]
    pub out: PathBuf,
    /// How long one HTTP request may take before the case counts as an error.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..))]
    pub timeout_secs: u64,
    #[arg(long, default_value_t = 0.95, value_parser = parse_ratio)]
    pub min_staleness: f64,
    /// Minimum recall@5. SPEC section 12 sets no target, so by default recall is only reported.
    #[arg(long, default_value_t = 0.0, value_parser = parse_ratio)]
    pub min_recall: f64,
    /// Minimum share of page questions with their page in the top 3 page hits. No target is set,
    /// so by default it is only reported.
    #[arg(long, default_value_t = 0.0, value_parser = parse_ratio)]
    pub min_page_hit: f64,
    /// Timed recalls per page_limit setting (0 and 3) in the pages suite, and per insight_limit
    /// setting (0 and 3) in the insights suite, after one warm-up pass. 0 skips the measurement.
    #[arg(long, default_value_t = 0)]
    pub latency_calls: usize,
    /// The server's `MEMORYOS_REFLECT_MIN_FACTS`, so the insights suite knows when a topic is due.
    #[arg(long, default_value_t = 3, value_parser = parse_min_facts)]
    pub reflect_min_facts: usize,
    /// Cases run at the same time within a suite.
    #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u64).range(1..))]
    pub concurrency: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Suite {
    Recall,
    Staleness,
    Poisoning,
    Pages,
    Insights,
    All,
}

impl Args {
    /// Whether `suite` was asked for, by name or through `all`.
    pub fn wants(&self, suite: Suite) -> bool {
        self.suite.contains(&suite) || self.suite.contains(&Suite::All)
    }
}

fn parse_min_facts(raw: &str) -> Result<usize, String> {
    let value: usize = raw.parse().map_err(|e| format!("{e}"))?;
    let max = insights::TOPIC_FACTS.len() - 1;
    if (2..=max).contains(&value) {
        Ok(value)
    } else {
        Err(format!("{value} is not between 2 and {max}"))
    }
}

fn parse_ratio(raw: &str) -> Result<f64, String> {
    let value: f64 = raw.parse().map_err(|e| format!("{e}"))?;
    if (0.0..=1.0).contains(&value) {
        Ok(value)
    } else {
        Err(format!("{value} is not between 0 and 1"))
    }
}

/// Runs the selected suites and writes the report. `Report::passed` says whether every gate held.
pub async fn run(args: &Args) -> Result<Report, Box<dyn std::error::Error>> {
    let started_at = Utc::now();
    let keys = ApiKeys {
        agent: args.api_key.clone(),
        external: args.external_api_key.clone(),
        admin: args.admin_api_key.clone(),
    };
    let client = Client::new(&args.base_url, keys, args.timeout_secs)?;
    let ctx = RunContext::new(client);
    let concurrency = usize::try_from(args.concurrency)?;

    let mut suites: Vec<SuiteReport> = Vec::new();
    if args.wants(Suite::Recall) {
        let cases = dataset::load_recall()?;
        let results = suites::run_recall(&ctx, &cases, concurrency).await;
        suites.push(score::recall_report(&results, args.min_recall)?);
    }
    if args.wants(Suite::Staleness) {
        let cases = dataset::load_staleness()?;
        let results = suites::run_staleness(&ctx, &cases, concurrency).await;
        suites.push(score::staleness_report(&results, args.min_staleness)?);
    }
    if args.wants(Suite::Poisoning) {
        let cases = dataset::load_poisoning()?;
        let results = suites::run_poisoning(&ctx, &cases, concurrency).await;
        suites.push(score::poisoning_report(&results)?);
    }
    if args.wants(Suite::Pages) {
        let data = dataset::load_pages()?;
        let recall_cases = dataset::load_recall()?;
        let run =
            pages::run_pages(&ctx, &data, &recall_cases, concurrency, args.latency_calls).await;
        suites.push(score::pages_report(&run, args.min_page_hit)?);
    }
    if args.wants(Suite::Insights) {
        let recall_cases = dataset::load_recall()?;
        let run = insights::run_insights(
            &ctx,
            &recall_cases,
            concurrency,
            args.latency_calls,
            args.reflect_min_facts,
        )
        .await;
        suites.push(score::insights_report(&run)?);
    }

    let report = Report::new(
        ctx.run_id.clone(),
        args.base_url.clone(),
        started_at,
        Utc::now(),
        suites,
    );
    report.write(&args.out)?;
    Ok(report)
}
