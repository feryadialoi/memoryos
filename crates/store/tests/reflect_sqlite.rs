//! Reflect batch on real SQLite files. Each test owns a temporary database; insight fixtures are
//! written with direct SQL on a second connection to the same file.

#![cfg(feature = "integration-sqlite")]

mod reflect_common;

use chrono::Utc;
use memoryos_core::{InsightStatus, Scope};
use memoryos_store::SqliteStore;
use reflect_common::{stale_reason, Harness, Res, TestResult};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tempfile::TempDir;
use uuid::Uuid;

struct Db {
    // Dropped before `_dir`, so SQLite has closed its file handles before cleanup.
    store: SqliteStore,
    path: String,
    _dir: TempDir,
}

async fn db() -> Res<Db> {
    let dir = tempfile::tempdir()?;
    let path = dir
        .path()
        .join("memoryos.sqlite3")
        .to_string_lossy()
        .into_owned();
    let store = SqliteStore::connect(&path).await?;
    store.migrate().await?;
    Ok(Db {
        store,
        path,
        _dir: dir,
    })
}

impl Harness for Db {
    type Store = SqliteStore;

    fn store(&self) -> &SqliteStore {
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
        let now = Utc::now().timestamp_micros();
        let reason = (status == InsightStatus::Stale).then(|| stale_reason(facts[0]).to_string());
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&self.path)
                    .foreign_keys(true),
            )
            .await?;
        let mut tx = pool.begin().await?;
        sqlx::query(
            "INSERT INTO insights (id, tenant, scope, level, statement, trust, status, \
             stale_reason, valid_from, written_by, recorded_at) \
             VALUES (?1, ?2, ?3, 'context', 'fixture', 'agent', ?4, ?5, ?6, 'tester', ?6)",
        )
        .bind(id.to_string())
        .bind(tenant)
        .bind(scope.to_string())
        .bind(status.as_str())
        .bind(reason)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        for fact in facts {
            sqlx::query(
                "INSERT INTO insight_sources (insight_id, tenant, source_kind, source_id) \
                 VALUES (?1, ?2, 'fact', ?3)",
            )
            .bind(id.to_string())
            .bind(tenant)
            .bind(fact.to_string())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        pool.close().await;
        Ok(id)
    }

    async fn expire_fact(&self, id: Uuid) -> Res<()> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::new().filename(&self.path))
            .await?;
        sqlx::query("UPDATE facts SET expires_at = ?1 WHERE id = ?2")
            .bind((Utc::now() - chrono::Duration::minutes(1)).timestamp_micros())
            .bind(id.to_string())
            .execute(&pool)
            .await?;
        pool.close().await;
        Ok(())
    }
}

#[tokio::test]
async fn a_topic_needs_min_facts_and_shows_the_newest_30() -> TestResult {
    reflect_common::a_topic_needs_min_facts_and_shows_the_newest_30(&db().await?).await
}

#[tokio::test]
async fn only_current_active_uncited_facts_count() -> TestResult {
    reflect_common::only_current_active_uncited_facts_count(&db().await?).await
}

#[tokio::test]
async fn a_topic_carries_the_subjects_active_insights() -> TestResult {
    reflect_common::a_topic_carries_the_subjects_active_insights(&db().await?).await
}

#[tokio::test]
async fn a_decline_holds_until_the_fact_set_changes() -> TestResult {
    reflect_common::a_decline_holds_until_the_fact_set_changes(&db().await?).await
}

#[tokio::test]
async fn declined_topics_do_not_use_up_the_limit() -> TestResult {
    reflect_common::declined_topics_do_not_use_up_the_limit(&db().await?).await
}

#[tokio::test]
async fn the_order_is_deterministic_and_limited() -> TestResult {
    reflect_common::the_order_is_deterministic_and_limited(&db().await?).await
}

#[tokio::test]
async fn inputs_are_validated() -> TestResult {
    reflect_common::inputs_are_validated(&db().await?).await
}

#[tokio::test]
async fn tenants_are_isolated() -> TestResult {
    reflect_common::tenants_are_isolated(&db().await?).await
}

#[tokio::test]
async fn stale_insights_come_first_oldest_first_with_current_sources() -> TestResult {
    reflect_common::stale_insights_come_first_oldest_first_with_current_sources(&db().await?).await
}

#[tokio::test]
async fn the_batch_marks_expired_sources_stale_first() -> TestResult {
    reflect_common::the_batch_marks_expired_sources_stale_first(&db().await?).await
}
