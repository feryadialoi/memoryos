//! The reranking bench behind `memoryos reranker bench` and `tests/rerank.rs`: 15 questions in
//! English and Chinese, each with the ten facts fused recall returned for it. A model scores the ten
//! and passes a case when its best scored fact answers the question.

use std::path::PathBuf;
use std::time::Instant;

use memoryos_core::{Reranker, Result};

use crate::models::RerankModel;
use crate::rerank::LocalReranker;

pub const FACTS: [&str; 30] = [
    "The project stores its data in PostgreSQL 17 with the pgvector extension.",
    "Redis was considered for caching but rejected because it adds another service to run.",
    "The team deploys the API with Docker Compose on a single VM.",
    "The CI pipeline runs on GitHub Actions.",
    "Ada prefers tabs over spaces in Go code.",
    "Ada's nickname is aloi.",
    "Ada lives in Bandung.",
    "Ada works at Praja as a backend engineer.",
    "Ada uses Zed as her main editor.",
    "Ada drinks oolong tea every morning.",
    "The production deploy happens every Thursday afternoon.",
    "Release notes are written in English and published on the wiki.",
    "The payment service retries failed webhooks three times with exponential backoff.",
    "The search API returns at most 100 results per page.",
    "Login uses one-time passwords sent by SMS.",
    "The mobile app is written in Flutter.",
    "Error logs are shipped to Datadog.",
    "The staging database is reset every Monday.",
    "Kafka topics use a 7 day retention period.",
    "The team decided to drop MongoDB because transactions across collections were unreliable.",
    "项目的数据库是 PostgreSQL。",
    "团队每周四下午发布生产版本。",
    "Ada 喜欢喝乌龙茶。",
    "支付服务的回调失败后会重试三次。",
    "移动端应用使用 Flutter 开发。",
    "日志统一发送到 Datadog。",
    "Ada 住在万隆。",
    "代码评审至少需要两位同事批准。",
    "Code review needs approval from at least two teammates.",
    "The office closes at 6 pm on Fridays.",
];

/// A question, the facts that answer it, and the ten facts fused recall returned for it, in that
/// order. Fused recall put a right fact first for 8 of the 15.
pub struct Case {
    pub query: &'static str,
    pub answers: &'static [usize],
    pub candidates: [usize; 10],
}

#[rustfmt::skip]
pub const CASES: [Case; 15] = [
    Case { query: "what database do we use?", answers: &[0, 20], candidates: [20, 17, 19, 16, 14, 4, 26, 18, 13, 21] },
    Case { query: "why didn't we use Redis?", answers: &[1], candidates: [20, 19, 25, 24, 12, 27, 26, 23, 1, 16] },
    Case { query: "where does Ada live?", answers: &[6, 26], candidates: [26, 20, 5, 9, 6, 16, 8, 4, 7, 13] },
    Case { query: "what is my nickname?", answers: &[5], candidates: [20, 26, 25, 14, 5, 18, 17, 16, 15, 27] },
    Case { query: "which editor does Ada use?", answers: &[8], candidates: [26, 8, 4, 20, 5, 16, 9, 7, 13, 21] },
    Case { query: "when do we release to production?", answers: &[10, 21], candidates: [21, 29, 10, 20, 18, 17, 26, 19, 11, 28] },
    Case { query: "how many times are webhooks retried?", answers: &[12, 23], candidates: [18, 17, 19, 12, 26, 23, 16, 27, 25, 20] },
    Case { query: "where do error logs go?", answers: &[16, 25], candidates: [16, 25, 20, 17, 26, 19, 14, 27, 13, 4] },
    Case { query: "why was MongoDB dropped?", answers: &[19], candidates: [19, 20, 17, 16, 18, 12, 26, 21, 15, 1] },
    Case { query: "我们用什么数据库？", answers: &[0, 20], candidates: [20, 25, 27, 21, 26, 24, 19, 23, 22, 16] },
    Case { query: "Ada 喝什么茶？", answers: &[9, 22], candidates: [26, 20, 21, 25, 22, 27, 9, 24, 5, 23] },
    Case { query: "代码评审需要几个人批准？", answers: &[27, 28], candidates: [27, 28, 20, 21, 26, 25, 24, 23, 19, 22] },
    Case { query: "移动应用用什么框架？", answers: &[15, 24], candidates: [20, 27, 24, 21, 25, 26, 23, 22, 19, 4] },
    Case { query: "how long do Kafka messages stay?", answers: &[18], candidates: [18, 17, 29, 14, 20, 19, 0, 4, 1, 28] },
    Case { query: "how does login work?", answers: &[14], candidates: [25, 26, 16, 14, 20, 15, 17, 24, 18, 19] },
];

pub struct Report {
    pub model: RerankModel,
    /// Cases whose best scored fact answers the question.
    pub passed: usize,
    pub cases: usize,
    /// Scoring ten facts, per case.
    pub median_ms: f64,
    pub max_ms: f64,
    /// Download, if the model is not cached yet, and load.
    pub load_secs: f64,
    /// The questions that failed, each with the fact the model put first.
    pub misses: Vec<(&'static str, &'static str)>,
}

/// Loads `model`, downloading it into the cache first when it is not there, and scores every case.
pub async fn run(model: RerankModel, cache_dir: Option<PathBuf>) -> Result<Report> {
    let reranker = LocalReranker::new(cache_dir, model)?;
    let started = Instant::now();
    reranker.warm_up().await?;
    let load_secs = started.elapsed().as_secs_f64();

    let mut millis = Vec::with_capacity(CASES.len());
    let mut misses = Vec::new();
    for case in &CASES {
        let statements: Vec<String> = case
            .candidates
            .iter()
            .map(|&index| FACTS[index].to_owned())
            .collect();
        let started = Instant::now();
        let scores = reranker.score(case.query, &statements).await?;
        millis.push(started.elapsed().as_secs_f64() * 1000.0);
        let top = case.candidates[best(&scores)];
        if !case.answers.contains(&top) {
            misses.push((case.query, FACTS[top]));
        }
    }
    millis.sort_by(f64::total_cmp);
    Ok(Report {
        model,
        passed: CASES.len() - misses.len(),
        cases: CASES.len(),
        median_ms: millis[millis.len() / 2],
        max_ms: millis[millis.len() - 1],
        load_secs,
        misses,
    })
}

/// The index of the highest score. `score` has already checked every score is finite.
pub fn best(scores: &[f32]) -> usize {
    scores
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map_or(0, |(index, _)| index)
}
