//! Reflect batch on real Postgres. One container per test binary; each test clones a migrated
//! template into its own database. The harness is a trimmed copy of `integration.rs`'s, which is
//! not in a shared module. Tests log in as the superuser, so the insight fixtures written directly
//! bypass row-level security while the store's reads go through it.

#![cfg(feature = "integration")]

mod reflect_common;

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use memoryos_core::{InsightStatus, Scope};
use memoryos_store::PgStore;
use pgvector::Vector;
use reflect_common::{axis, stale_reason, Harness, Res, TestResult};
use sqlx::postgres::PgPoolOptions;
use sqlx::{AssertSqlSafe, PgPool};
use testcontainers::core::{IntoContainerPort, Mount, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};
use uuid::Uuid;

const TEMPLATE_DB: &str = "memoryos_template";

fn server_url() -> &'static str {
    static URL: OnceLock<String> = OnceLock::new();
    URL.get_or_init(|| {
        std::thread::spawn(|| {
            tokio::runtime::Runtime::new()
                .expect("runtime")
                .block_on(start_server())
        })
        .join()
        .expect("container thread")
    })
}

async fn start_server() -> String {
    let container = GenericImage::new("pgvector/pgvector", "pg17")
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_env_var("POSTGRES_PASSWORD", "postgres")
        .with_mount(Mount::tmpfs_mount("/var/lib/postgresql/data"))
        .with_cmd(["postgres", "-c", "max_connections=200", "-c", "fsync=off"])
        .start()
        .await
        .expect("start pgvector container");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("mapped port");
    reap_on_exit(container.id());
    std::mem::forget(container);

    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}");
    let admin = connect_with_retry(&format!("{url}/postgres")).await;
    sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {TEMPLATE_DB}")))
        .execute(&admin)
        .await
        .expect("create template");
    admin.close().await;
    let store = PgStore::connect(&format!("{url}/{TEMPLATE_DB}"), 2)
        .await
        .expect("connect template");
    store.migrate().await.expect("migrate template");
    drop(store);
    let admin = connect_with_retry(&format!("{url}/postgres")).await;
    for _ in 0..100 {
        let open: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname = $1")
                .bind(TEMPLATE_DB)
                .fetch_one(&admin)
                .await
                .expect("count connections");
        if open == 0 {
            break;
        }
        sqlx::query("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = $1")
            .bind(TEMPLATE_DB)
            .execute(&admin)
            .await
            .expect("terminate");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    admin.close().await;
    url
}

/// Removes the container when this test binary exits, however it exits.
#[allow(clippy::zombie_processes)] // The child must outlive this process, so it is never waited on.
fn reap_on_exit(container_id: &str) {
    let script = format!(
        "while kill -0 {pid} 2>/dev/null; do sleep 1; done; docker rm -f {container_id}",
        pid = std::process::id()
    );
    Command::new("sh")
        .args(["-c", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn reaper");
}

async fn connect_with_retry(url: &str) -> PgPool {
    for _ in 0..120 {
        if let Ok(pool) = PgPoolOptions::new().max_connections(2).connect(url).await {
            return pool;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("postgres did not accept connections");
}

struct Db {
    store: PgStore,
    admin: PgPool,
    name: String,
}

/// Drops the test's database when the test ends, on a thread with its own runtime.
impl Drop for Db {
    fn drop(&mut self) {
        let admin = format!("{}/postgres", server_url());
        let sql = format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", self.name);
        let dropped = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime")
                .block_on(async {
                    let pool = connect_with_retry(&admin).await;
                    let result = sqlx::query(AssertSqlSafe(sql)).execute(&pool).await;
                    pool.close().await;
                    result
                })
        })
        .join();
        if !std::thread::panicking() {
            dropped
                .expect("drop thread")
                .expect("drop the test database");
        }
    }
}

async fn db() -> Db {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let server = server_url();
    let name = format!("r{}", NEXT.fetch_add(1, Ordering::SeqCst));
    let bootstrap = connect_with_retry(&format!("{server}/postgres")).await;
    sqlx::query(AssertSqlSafe(format!(
        "CREATE DATABASE {name} TEMPLATE {TEMPLATE_DB}"
    )))
    .execute(&bootstrap)
    .await
    .expect("clone template");
    bootstrap.close().await;
    let url = format!("{server}/{name}");
    Db {
        store: PgStore::connect(&url, 4).await.expect("connect"),
        admin: connect_with_retry(&url).await,
        name,
    }
}

impl Harness for Db {
    type Store = PgStore;

    fn store(&self) -> &PgStore {
        &self.store
    }

    async fn insert_insight(
        &self,
        tenant: &str,
        scope: &Scope,
        status: InsightStatus,
        facts: &[Uuid],
    ) -> Res<Uuid> {
        let id = Uuid::now_v7();
        let reason = (status == InsightStatus::Stale).then(|| stale_reason(facts[0]));
        let mut tx = self.admin.begin().await?;
        sqlx::query(
            "INSERT INTO insights (id, tenant, scope, level, statement, trust, status, \
             stale_reason, valid_from, written_by, embedding) \
             VALUES ($1, $2, $3, 'context', 'fixture', 'agent', $4, $5, now(), 'tester', $6)",
        )
        .bind(id)
        .bind(tenant)
        .bind(scope.to_string())
        .bind(status.as_str())
        .bind(reason)
        .bind(Vector::from(axis(2)))
        .execute(&mut *tx)
        .await?;
        for fact in facts {
            sqlx::query(
                "INSERT INTO insight_sources (insight_id, tenant, source_kind, source_id) \
                 VALUES ($1, $2, 'fact', $3)",
            )
            .bind(id)
            .bind(tenant)
            .bind(fact)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(id)
    }

    async fn expire_fact(&self, id: Uuid) -> Res<()> {
        sqlx::query("UPDATE facts SET expires_at = now() - interval '1 minute' WHERE id = $1")
            .bind(id)
            .execute(&self.admin)
            .await?;
        Ok(())
    }
}

#[tokio::test]
async fn a_topic_needs_min_facts_and_shows_the_newest_30() -> TestResult {
    reflect_common::a_topic_needs_min_facts_and_shows_the_newest_30(&db().await).await
}

#[tokio::test]
async fn only_current_active_uncited_facts_count() -> TestResult {
    reflect_common::only_current_active_uncited_facts_count(&db().await).await
}

#[tokio::test]
async fn a_topic_carries_the_subjects_active_insights() -> TestResult {
    reflect_common::a_topic_carries_the_subjects_active_insights(&db().await).await
}

#[tokio::test]
async fn a_decline_holds_until_the_fact_set_changes() -> TestResult {
    reflect_common::a_decline_holds_until_the_fact_set_changes(&db().await).await
}

#[tokio::test]
async fn declined_topics_do_not_use_up_the_limit() -> TestResult {
    reflect_common::declined_topics_do_not_use_up_the_limit(&db().await).await
}

#[tokio::test]
async fn the_order_is_deterministic_and_limited() -> TestResult {
    reflect_common::the_order_is_deterministic_and_limited(&db().await).await
}

#[tokio::test]
async fn inputs_are_validated() -> TestResult {
    reflect_common::inputs_are_validated(&db().await).await
}

#[tokio::test]
async fn tenants_are_isolated() -> TestResult {
    reflect_common::tenants_are_isolated(&db().await).await
}

#[tokio::test]
async fn stale_insights_come_first_oldest_first_with_current_sources() -> TestResult {
    reflect_common::stale_insights_come_first_oldest_first_with_current_sources(&db().await).await
}

#[tokio::test]
async fn the_batch_marks_expired_sources_stale_first() -> TestResult {
    reflect_common::the_batch_marks_expired_sources_stale_first(&db().await).await
}
