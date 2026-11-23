//! Reflect batch and topic declines on SQLite (docs/SPEC.md section 8, Insights, "Reflect
//! batch"). Input checks, the fact set digest and topic grouping are shared with PostgreSQL in
//! `crate::reflect`. Every query carries an explicit tenant predicate.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use memoryos_core::{
    Error, Insight, InsightSourceRef, ReflectBatch, ReflectTopic, RequestContext, Result, Scope,
    StaleSource, REFLECT_TOPIC_FACTS_MAX,
};
use sqlx::sqlite::SqliteRow;
use sqlx::{AssertSqlSafe, Row, SqliteConnection};
use uuid::Uuid;

use super::rows::{self, FACT_COLS};
use super::{db, json_array, uuid_strings, SqliteStore};
use crate::reflect::{
    batch_limit, chunk_topics, fact_set, min_facts as check_min_facts, SUBJECT_CHUNK,
    TOPIC_INSIGHTS_MAX,
};

/// SQL condition, for `facts f`, that no active, stale or quarantined insight cites the fact.
const UNCITED: &str = "NOT EXISTS (SELECT 1 FROM insight_sources s \
    JOIN insights i ON i.tenant = s.tenant AND i.id = s.insight_id \
    WHERE s.tenant = f.tenant AND s.source_kind = 'fact' AND s.source_id = f.id \
      AND i.status IN ('active', 'stale', 'quarantined'))";

/// Insight columns for `insights i`, with its sources ordered as `InsightSourceRef` orders them.
const INSIGHT_COLS: &str = "i.id, i.scope, i.level, i.statement, i.trust, i.status, \
    i.stale_reason, i.valid_from, i.valid_to, i.superseded_by, i.pending_supersede_of, \
    i.written_by, i.recorded_at, \
    (SELECT json_group_array(json_object('kind', x.source_kind, 'id', x.source_id)) FROM ( \
        SELECT s.source_kind, s.source_id FROM insight_sources s \
        WHERE s.tenant = i.tenant AND s.insight_id = i.id \
        ORDER BY s.source_kind, s.source_id) x) AS sources";

fn get<'r, T>(row: &'r SqliteRow, column: &str) -> Result<T>
where
    T: sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>,
{
    row.try_get(column).map_err(db)
}

fn parsed<T: std::str::FromStr>(row: &SqliteRow, column: &str) -> Result<T> {
    let text: String = get(row, column)?;
    text.parse()
        .map_err(|_| Error::Storage(format!("column {column} holds an invalid value")))
}

fn optional_uuid(row: &SqliteRow, column: &str) -> Result<Option<Uuid>> {
    let text: Option<String> = get(row, column)?;
    text.map(|value| {
        Uuid::parse_str(&value)
            .map_err(|_| Error::Storage(format!("column {column} holds an invalid UUID")))
    })
    .transpose()
}

fn uuid(row: &SqliteRow, column: &str) -> Result<Uuid> {
    optional_uuid(row, column)?.ok_or_else(|| Error::Storage(format!("column {column} is null")))
}

fn optional_time(row: &SqliteRow, column: &str) -> Result<Option<DateTime<Utc>>> {
    let micros: Option<i64> = get(row, column)?;
    micros
        .map(|value| {
            DateTime::from_timestamp_micros(value).ok_or_else(|| {
                Error::Storage(format!("column {column} holds an invalid timestamp"))
            })
        })
        .transpose()
}

fn time(row: &SqliteRow, column: &str) -> Result<DateTime<Utc>> {
    optional_time(row, column)?.ok_or_else(|| Error::Storage(format!("column {column} is null")))
}

fn insight(row: &SqliteRow) -> Result<Insight> {
    let sources: String = get(row, "sources")?;
    let sources: Vec<InsightSourceRef> = serde_json::from_str(&sources)
        .map_err(|e| Error::Storage(format!("insight sources: {e}")))?;
    let stale_reason: Option<String> = get(row, "stale_reason")?;
    let stale_reason = stale_reason
        .map(|text| serde_json::from_str::<Vec<StaleSource>>(&text))
        .transpose()
        .map_err(|e| Error::Storage(format!("insight stale_reason: {e}")))?;
    Ok(Insight {
        id: uuid(row, "id")?,
        scope: parsed(row, "scope")?,
        level: parsed(row, "level")?,
        statement: get(row, "statement")?,
        trust: parsed(row, "trust")?,
        status: parsed(row, "status")?,
        stale_reason,
        sources,
        valid_from: time(row, "valid_from")?,
        valid_to: optional_time(row, "valid_to")?,
        superseded_by: optional_uuid(row, "superseded_by")?,
        pending_supersede_of: optional_uuid(row, "pending_supersede_of")?,
        written_by: get(row, "written_by")?,
        recorded_at: time(row, "recorded_at")?,
    })
}

pub(super) async fn reflect_batch(
    store: &SqliteStore,
    ctx: &RequestContext,
    scope: &Scope,
    limit: i64,
    min_facts: i64,
) -> Result<ReflectBatch> {
    let limit = batch_limit(limit)?;
    let min_facts = check_min_facts(min_facts)?;
    let now = rows::timestamp(Utc::now());
    let mut tx = store.tenant_tx(ctx).await?;
    let topics = topics(&mut tx, ctx, scope, now, limit, min_facts).await?;
    tx.commit().await.map_err(db)?;
    Ok(ReflectBatch {
        stale: Vec::new(),
        topics,
    })
}

/// Up to `room` topics of `scope`: subjects ordered by their oldest uncited current fact, so the
/// longest waiting topic comes first, then by subject id.
async fn topics(
    conn: &mut SqliteConnection,
    ctx: &RequestContext,
    scope: &Scope,
    now: i64,
    room: usize,
    min_facts: i64,
) -> Result<Vec<ReflectTopic>> {
    if room == 0 {
        return Ok(Vec::new());
    }
    let current = rows::visible(3, 3);
    let sql = format!(
        "SELECT f.subject_id FROM facts f \
         WHERE f.tenant = ?1 AND f.scope = ?2 AND {current} AND {UNCITED} \
         GROUP BY f.subject_id HAVING count(*) >= ?4 \
         ORDER BY min(f.recorded_at), f.subject_id"
    );
    let subjects: Vec<String> = sqlx::query_scalar(AssertSqlSafe(sql))
        .bind(&ctx.tenant)
        .bind(scope.to_string())
        .bind(now)
        .bind(min_facts)
        .fetch_all(&mut *conn)
        .await
        .map_err(db)?;
    let subjects = subjects
        .iter()
        .map(|id| {
            Uuid::parse_str(id)
                .map_err(|_| Error::Storage("column subject_id holds an invalid UUID".into()))
        })
        .collect::<Result<Vec<Uuid>>>()?;

    let facts_sql = format!(
        "SELECT * FROM ( \
             SELECT {FACT_COLS}, \
                 row_number() OVER (PARTITION BY f.subject_id \
                     ORDER BY f.recorded_at DESC, f.id DESC) AS rn, \
                 count(*) OVER (PARTITION BY f.subject_id) AS n \
             FROM facts f \
             WHERE f.tenant = ?1 AND f.scope = ?2 \
               AND f.subject_id IN (SELECT value FROM json_each(?3)) \
               AND {current} AND {UNCITED} \
         ) x WHERE x.rn <= ?4 AND x.n >= ?5 \
         ORDER BY x.subject_id, x.rn",
        current = rows::visible(6, 6)
    );
    let mut topics = Vec::new();
    for chunk in subjects.chunks(SUBJECT_CHUNK) {
        if topics.len() == room {
            break;
        }
        let ids = json_array(&uuid_strings(chunk.iter().copied()))?;
        let found = sqlx::query(AssertSqlSafe(facts_sql.clone()))
            .bind(&ctx.tenant)
            .bind(scope.to_string())
            .bind(&ids)
            .bind(REFLECT_TOPIC_FACTS_MAX as i64)
            .bind(min_facts)
            .bind(now)
            .fetch_all(&mut *conn)
            .await
            .map_err(db)?;
        let facts = found.iter().map(rows::fact).collect::<Result<Vec<_>>>()?;
        let names = id_map(
            sqlx::query_as::<_, (String, String)>(
                "SELECT id, name FROM entities \
                 WHERE tenant = ?1 AND id IN (SELECT value FROM json_each(?2))",
            )
            .bind(&ctx.tenant)
            .bind(&ids)
            .fetch_all(&mut *conn)
            .await
            .map_err(db)?,
        )?;
        let declines = id_map(
            sqlx::query_as::<_, (String, String)>(
                "SELECT topic_key, fact_set FROM reflect_declines \
                 WHERE tenant = ?1 AND scope = ?2 \
                   AND topic_key IN (SELECT value FROM json_each(?3))",
            )
            .bind(&ctx.tenant)
            .bind(scope.to_string())
            .bind(&ids)
            .fetch_all(&mut *conn)
            .await
            .map_err(db)?,
        )?;
        topics.extend(chunk_topics(
            chunk,
            facts,
            &names,
            &declines,
            room - topics.len(),
        )?);
    }
    attach_insights(conn, ctx, scope, &mut topics).await?;
    Ok(topics)
}

fn id_map(pairs: Vec<(String, String)>) -> Result<HashMap<Uuid, String>> {
    pairs
        .into_iter()
        .map(|(id, value)| {
            Uuid::parse_str(&id)
                .map(|id| (id, value))
                .map_err(|_| Error::Storage("an id column holds an invalid UUID".into()))
        })
        .collect()
}

/// Fills each topic's active insights of `scope` that cite any fact of its subject, newest first,
/// at most `TOPIC_INSIGHTS_MAX` each.
async fn attach_insights(
    conn: &mut SqliteConnection,
    ctx: &RequestContext,
    scope: &Scope,
    topics: &mut [ReflectTopic],
) -> Result<()> {
    let sql = format!(
        "SELECT {INSIGHT_COLS} FROM insights i \
         WHERE i.tenant = ?1 AND i.scope = ?2 AND i.status = 'active' AND EXISTS ( \
             SELECT 1 FROM insight_sources s \
             JOIN facts f ON f.tenant = s.tenant AND f.id = s.source_id \
             WHERE s.tenant = i.tenant AND s.insight_id = i.id AND s.source_kind = 'fact' \
               AND f.subject_id = ?3) \
         ORDER BY i.recorded_at DESC, i.id DESC LIMIT ?4"
    );
    for topic in topics {
        let found = sqlx::query(AssertSqlSafe(sql.clone()))
            .bind(&ctx.tenant)
            .bind(scope.to_string())
            .bind(topic.topic_key.to_string())
            .bind(TOPIC_INSIGHTS_MAX)
            .fetch_all(&mut *conn)
            .await
            .map_err(db)?;
        topic.insights = found.iter().map(insight).collect::<Result<_>>()?;
    }
    Ok(())
}

pub(super) async fn decline_topic(
    store: &SqliteStore,
    ctx: &RequestContext,
    scope: &Scope,
    topic_key: Uuid,
    fact_set_: &str,
) -> Result<()> {
    fact_set(fact_set_)?;
    let now = rows::timestamp(Utc::now());
    let mut tx = store.write_tx(ctx).await?;
    sqlx::query(
        "INSERT INTO reflect_declines (tenant, scope, topic_key, fact_set, declined_at) \
         VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT (tenant, scope, topic_key) \
         DO UPDATE SET fact_set = excluded.fact_set, declined_at = excluded.declined_at",
    )
    .bind(&ctx.tenant)
    .bind(scope.to_string())
    .bind(topic_key.to_string())
    .bind(fact_set_)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(db)?;
    tx.commit().await.map_err(db)
}
