#![cfg(feature = "integration-local")]
//! Runs the real reranking model, downloading 571 MB on first use, so it needs
//! `--features integration-local`. Run it with `--release` for meaningful timings. Tests take turns
//! on `SERIAL` so the download happens once.

use std::time::{Duration, Instant};

use memoryos_core::Reranker;
use memoryos_embed::bench::{self, best};
use memoryos_embed::{LocalReranker, RerankModel};
use tokio::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::const_new(());

fn reranker() -> LocalReranker {
    LocalReranker::new(None, RerankModel::default()).expect("cache directory is available")
}

fn resident_mb() -> f64 {
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps runs");
    let kb: f64 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .expect("ps prints a number");
    kb / 1024.0
}

#[tokio::test]
async fn score_fails_at_once_until_warm_up() {
    let _serial = SERIAL.lock().await;
    let reranker = reranker();
    let started = Instant::now();
    let result = reranker.score("q", &["a".to_owned()]).await;
    assert!(result.is_err());
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn the_right_fact_comes_first_in_english_and_chinese() {
    let _serial = SERIAL.lock().await;
    let before = resident_mb();
    let report = bench::run(RerankModel::default(), None)
        .await
        .expect("model loads and scores");
    for (query, top) in &report.misses {
        println!("MISS {query} -> {top}");
    }
    println!(
        "right fact first {}/{}, 10 pairs per query: median {:.0} ms, max {:.0} ms; \
         load {:.1} s; resident {before:.0} MB before, {:.0} MB after",
        report.passed,
        report.cases,
        report.median_ms,
        report.max_ms,
        report.load_secs,
        resident_mb()
    );
    assert!(
        report.passed >= 14,
        "right fact first only {}/15",
        report.passed
    );
}

/// The worst case recall can send: 20 statements, each past the 512 token cut.
#[tokio::test]
async fn long_statements_are_truncated_not_refused() {
    let _serial = SERIAL.lock().await;
    let reranker = reranker();
    reranker.warm_up().await.expect("model loads");
    let mut statements = vec!["Ada lives in Bandung. ".repeat(400)];
    statements.extend((0..19).map(|_| "The office closes at 6 pm on Fridays. ".repeat(300)));
    let started = Instant::now();
    let scores = reranker
        .score("where does Ada live?", &statements)
        .await
        .expect("scores");
    println!(
        "20 pairs of 512 tokens: {:.0} ms, resident {:.0} MB",
        started.elapsed().as_secs_f64() * 1000.0,
        resident_mb()
    );
    assert_eq!(best(&scores), 0, "{scores:?}");
}
