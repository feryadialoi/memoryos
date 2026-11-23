//! Insights on SQLite (docs/SPEC.md section 8, Insights), with explicit tenant predicates in
//! place of row-level security. Every method that writes, including the reads that apply lazy
//! expiry, runs in one IMMEDIATE transaction, which replaces PostgreSQL's row locks. Staleness runs
//! in the transaction of the change that causes it. Quarantined insights are not marked stale:
//! approval checks their sources instead.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use memoryos_core::{
    insight_requires_quarantine, Error, Insight, InsightDetail, InsightFilter, InsightSourceKind,
    InsightSourceRef, InsightSourceView, InsightStatus, NewInsight, RequestContext, Result, Scope,
    ScoredInsight, SourceChange, StaleSource, Trust,
};
use serde_json::json;
use sqlx::sqlite::SqliteRow;
use sqlx::{AssertSqlSafe, Row, SqliteConnection};
use uuid::Uuid;

use super::facts::audit;
use super::rows::{self, INSIGHT_COLS};
use super::store_impl::safe_match_query;
use super::{db, embedding, json_array, storable, uuid_strings, SqliteStore};
use crate::insight_rules::{self as rules, FactState, FoundFact, FoundInsight};
use crate::validate;

fn parse<T: std::str::FromStr>(text: String, what: &str) -> Result<T> {
    text.parse()
        .map_err(|_| Error::Storage(format!("insight {what} is invalid")))
}

fn uuid(row: &SqliteRow, column: &str) -> Result<Uuid> {
    let text: String = row.try_get(column).map_err(db)?;
    Uuid::parse_str(&text).map_err(|_| Error::Storage(format!("column {column} is not a UUID")))
}

fn optional_uuid(row: &SqliteRow, column: &str) -> Result<Option<Uuid>> {
    let text: Option<String> = row.try_get(column).map_err(db)?;
    text.map(|value| {
        Uuid::parse_str(&value)
            .map_err(|_| Error::Storage(format!("column {column} is not a UUID")))
    })
    .transpose()
}

fn ids_json(ids: impl IntoIterator<Item = Uuid>) -> Result<String> {
    json_array(&uuid_strings(ids))
}

async fn load(conn: &mut SqliteConnection, ctx: &RequestContext, id: Uuid) -> Result<Insight> {
    let sql = format!("SELECT {INSIGHT_COLS} FROM insights i WHERE i.tenant = ?1 AND i.id = ?2");
    let row = sqlx::query(AssertSqlSafe(sql))
        .bind(&ctx.tenant)
        .bind(id.to_string())
        .fetch_optional(conn)
        .await
        .map_err(db)?
        .ok_or_else(|| Error::NotFound(format!("insight {id}")))?;
    rows::insight(&row)
}

fn insights(found: &[SqliteRow]) -> Result<Vec<Insight>> {
    found.iter().map(rows::insight).collect()
}

async fn read_sources(
    conn: &mut SqliteConnection,
    ctx: &RequestContext,
    sources: &[InsightSourceRef],
    now: i64,
) -> Result<(Vec<FoundFact>, Vec<FoundInsight>)> {
    let fact_ids = ids_json(rules::ids_of(sources, InsightSourceKind::Fact))?;
    let insight_ids = ids_json(rules::ids_of(sources, InsightSourceKind::Insight))?;
    let sql = format!(
        "SELECT f.id, f.scope, f.trust, ({}) AS current FROM facts f \
         WHERE f.tenant = ?1 AND f.id IN (SELECT value FROM json_each(?2))",
        rows::visible(3, 3)
    );
    let found = sqlx::query(AssertSqlSafe(sql))
        .bind(&ctx.tenant)
        .bind(&fact_ids)
        .bind(now)
        .fetch_all(&mut *conn)
        .await
        .map_err(db)?;
    let facts = found
        .iter()
        .map(|row| {
            Ok(FoundFact {
                id: uuid(row, "id")?,
                scope: row.try_get("scope").map_err(db)?,
                trust: parse(row.try_get("trust").map_err(db)?, "source trust")?,
                current: row.try_get("current").map_err(db)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let found = sqlx::query(
        "SELECT id, scope, level, trust, status FROM insights \
         WHERE tenant = ?1 AND id IN (SELECT value FROM json_each(?2))",
    )
    .bind(&ctx.tenant)
    .bind(&insight_ids)
    .fetch_all(&mut *conn)
    .await
    .map_err(db)?;
    let insights = found
        .iter()
        .map(|row| {
            Ok(FoundInsight {
                id: uuid(row, "id")?,
                scope: row.try_get("scope").map_err(db)?,
                level: parse(row.try_get("level").map_err(db)?, "level")?,
                trust: parse(row.try_get("trust").map_err(db)?, "trust")?,
                status: parse(row.try_get("status").map_err(db)?, "status")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((facts, insights))
}

async fn insert_sources(
    conn: &mut SqliteConnection,
    ctx: &RequestContext,
    id: Uuid,
    sources: &[InsightSourceRef],
) -> Result<()> {
    for source in sources {
        sqlx::query(
            "INSERT INTO insight_sources (insight_id, tenant, source_kind, source_id) \
             VALUES (?2, ?1, ?3, ?4)",
        )
        .bind(&ctx.tenant)
        .bind(id.to_string())
        .bind(source.kind.as_str())
        .bind(source.id.to_string())
        .execute(&mut *conn)
        .await
        .map_err(db)?;
    }
    Ok(())
}

async fn citing_insights(
    conn: &mut SqliteConnection,
    ctx: &RequestContext,
    ids: &[Uuid],
    status: InsightStatus,
) -> Result<Vec<(Uuid, StaleSource)>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let found = sqlx::query(
        "SELECT insight_id, source_id FROM insight_sources \
         WHERE tenant = ?1 AND source_kind = 'insight' \
           AND source_id IN (SELECT value FROM json_each(?2))",
    )
    .bind(&ctx.tenant)
    .bind(ids_json(ids.iter().copied())?)
    .fetch_all(conn)
    .await
    .map_err(db)?;
    found
        .iter()
        .map(|row| {
            Ok((
                uuid(row, "insight_id")?,
                rules::stale(
                    InsightSourceKind::Insight,
                    uuid(row, "source_id")?,
                    SourceChange::InsightChanged { status },
                ),
            ))
        })
        .collect()
}

/// As on PostgreSQL: marks stale, merges reasons, and carries each insight that turns stale up
/// every level. Only active and stale insights change.
async fn mark_stale(
    conn: &mut SqliteConnection,
    ctx: &RequestContext,
    mut pending: Vec<(Uuid, StaleSource)>,
    now: DateTime<Utc>,
) -> Result<()> {
    while !pending.is_empty() {
        let mut by_insight: BTreeMap<Uuid, Vec<StaleSource>> = BTreeMap::new();
        for (id, change) in pending.drain(..) {
            by_insight.entry(id).or_default().push(change);
        }
        let found = sqlx::query(
            "SELECT id, status, stale_reason FROM insights \
             WHERE tenant = ?1 AND id IN (SELECT value FROM json_each(?2)) \
               AND status IN ('active', 'stale') ORDER BY id",
        )
        .bind(&ctx.tenant)
        .bind(ids_json(by_insight.keys().copied())?)
        .fetch_all(&mut *conn)
        .await
        .map_err(db)?;
        let mut turned = Vec::new();
        for row in &found {
            let id = uuid(row, "id")?;
            let status: InsightStatus = parse(row.try_get("status").map_err(db)?, "status")?;
            let existing: Option<String> = row.try_get("stale_reason").map_err(db)?;
            let existing: Option<Vec<StaleSource>> = existing
                .map(|text| serde_json::from_str(&text))
                .transpose()
                .map_err(|e| Error::Storage(format!("insight stale_reason is invalid: {e}")))?;
            let changes = by_insight.get(&id).map(Vec::as_slice).unwrap_or_default();
            let (reason, changed) = rules::merge_reason(existing, changes);
            if !changed {
                continue;
            }
            let encoded = rules::encode_reason(&reason)?;
            sqlx::query(
                "UPDATE insights SET status = 'stale', stale_reason = ?3 \
                 WHERE tenant = ?1 AND id = ?2",
            )
            .bind(&ctx.tenant)
            .bind(id.to_string())
            .bind(encoded.to_string())
            .execute(&mut *conn)
            .await
            .map_err(db)?;
            audit(
                conn,
                ctx,
                "insight.stale",
                Some(id),
                json!({ "from_status": status.as_str(), "reason": encoded }),
                now,
            )
            .await?;
            if status == InsightStatus::Active {
                turned.push(id);
            }
        }
        pending = citing_insights(conn, ctx, &turned, InsightStatus::Stale).await?;
    }
    Ok(())
}

async fn fact_states(
    conn: &mut SqliteConnection,
    ctx: &RequestContext,
    ids: &[Uuid],
    now: i64,
) -> Result<HashMap<Uuid, FactState>> {
    let found = sqlx::query(
        "SELECT id, invalidated_at IS NOT NULL AS invalidated, superseded_by, \
             status = 'active' AS active, trust, \
             (expires_at IS NOT NULL AND expires_at <= ?3) AS expired \
         FROM facts WHERE tenant = ?1 AND id IN (SELECT value FROM json_each(?2))",
    )
    .bind(&ctx.tenant)
    .bind(ids_json(ids.iter().copied())?)
    .bind(now)
    .fetch_all(conn)
    .await
    .map_err(db)?;
    let mut states = HashMap::new();
    for row in &found {
        states.insert(
            uuid(row, "id")?,
            FactState {
                invalidated: row.try_get("invalidated").map_err(db)?,
                superseded_by: optional_uuid(row, "superseded_by")?,
                active: row.try_get("active").map_err(db)?,
                trust: parse(row.try_get("trust").map_err(db)?, "source trust")?,
                expired: row.try_get("expired").map_err(db)?,
            },
        );
    }
    Ok(states)
}

/// As on PostgreSQL: marks stale the active and stale insights citing any of `fact_ids` whose
/// state changed, as each fact now stands.
pub(super) async fn facts_changed(
    conn: &mut SqliteConnection,
    ctx: &RequestContext,
    fact_ids: &[Uuid],
    now: DateTime<Utc>,
) -> Result<()> {
    if fact_ids.is_empty() {
        return Ok(());
    }
    let cites = sqlx::query(
        "SELECT s.insight_id, s.source_id, i.trust FROM insight_sources s \
         JOIN insights i ON i.tenant = s.tenant AND i.id = s.insight_id \
         WHERE s.tenant = ?1 AND s.source_kind = 'fact' \
           AND s.source_id IN (SELECT value FROM json_each(?2)) \
           AND i.status IN ('active', 'stale')",
    )
    .bind(&ctx.tenant)
    .bind(ids_json(fact_ids.iter().copied())?)
    .fetch_all(&mut *conn)
    .await
    .map_err(db)?;
    if cites.is_empty() {
        return Ok(());
    }
    let cited = cites
        .iter()
        .map(|row| uuid(row, "source_id"))
        .collect::<Result<Vec<_>>>()?;
    let states = fact_states(conn, ctx, &cited, rows::timestamp(now)).await?;
    let mut pending = Vec::new();
    for row in &cites {
        let source = uuid(row, "source_id")?;
        let trust: Trust = parse(row.try_get("trust").map_err(db)?, "trust")?;
        if let Some(change) = rules::fact_change(states.get(&source), trust) {
            pending.push((
                uuid(row, "insight_id")?,
                rules::stale(InsightSourceKind::Fact, source, change),
            ));
        }
    }
    mark_stale(conn, ctx, pending, now).await
}

/// As on PostgreSQL: the facts an apply named or inserted, and those now superseded by them.
pub(super) async fn after_apply(
    conn: &mut SqliteConnection,
    ctx: &RequestContext,
    named: &[Uuid],
    inserted: &[Uuid],
    now: DateTime<Utc>,
) -> Result<()> {
    let mut touched: Vec<Uuid> = named.iter().chain(inserted).copied().collect();
    let found = sqlx::query(
        "SELECT id FROM facts WHERE tenant = ?1 \
           AND superseded_by IN (SELECT value FROM json_each(?2))",
    )
    .bind(&ctx.tenant)
    .bind(ids_json(touched.iter().copied())?)
    .fetch_all(&mut *conn)
    .await
    .map_err(db)?;
    for row in &found {
        touched.push(uuid(row, "id")?);
    }
    touched.sort_unstable();
    touched.dedup();
    facts_changed(conn, ctx, &touched, now).await
}

/// As on PostgreSQL: `ids`, the facts each of them closed, and those `episode_id` closed through an
/// existing fact. Read before the change.
pub(super) async fn chain_neighbours(
    conn: &mut SqliteConnection,
    ctx: &RequestContext,
    ids: &[Uuid],
    episode_id: Option<Uuid>,
) -> Result<Vec<Uuid>> {
    let found = sqlx::query(
        "SELECT id FROM facts WHERE tenant = ?1 \
           AND (superseded_by IN (SELECT value FROM json_each(?2)) \
                OR (?3 IS NOT NULL AND closed_by_episode = ?3))",
    )
    .bind(&ctx.tenant)
    .bind(ids_json(ids.iter().copied())?)
    .bind(episode_id.map(|id| id.to_string()))
    .fetch_all(conn)
    .await
    .map_err(db)?;
    let mut touched = found
        .iter()
        .map(|row| uuid(row, "id"))
        .collect::<Result<Vec<_>>>()?;
    touched.extend_from_slice(ids);
    touched.sort_unstable();
    touched.dedup();
    Ok(touched)
}

/// Lazy expiry, as on PostgreSQL.
pub(crate) async fn expire(
    conn: &mut SqliteConnection,
    ctx: &RequestContext,
    scopes: &[String],
    now: DateTime<Utc>,
) -> Result<()> {
    let found = sqlx::query(
        "SELECT s.insight_id, s.source_id FROM insights i \
         JOIN insight_sources s ON s.tenant = i.tenant AND s.insight_id = i.id \
             AND s.source_kind = 'fact' \
         JOIN facts f ON f.tenant = s.tenant AND f.id = s.source_id \
         WHERE i.tenant = ?1 AND i.scope IN (SELECT value FROM json_each(?2)) \
           AND i.status = 'active' AND f.expires_at <= ?3",
    )
    .bind(&ctx.tenant)
    .bind(json_array(scopes)?)
    .bind(rows::timestamp(now))
    .fetch_all(&mut *conn)
    .await
    .map_err(db)?;
    let pending = found
        .iter()
        .map(|row| {
            Ok((
                uuid(row, "insight_id")?,
                rules::stale(
                    InsightSourceKind::Fact,
                    uuid(row, "source_id")?,
                    SourceChange::Expired,
                ),
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    mark_stale(conn, ctx, pending, now).await
}

async fn close(
    conn: &mut SqliteConnection,
    ctx: &RequestContext,
    id: Uuid,
    status: InsightStatus,
    superseded_by: Option<Uuid>,
    now: DateTime<Utc>,
) -> Result<()> {
    sqlx::query(
        "UPDATE insights SET status = ?3, valid_to = ?4, superseded_by = ?5, \
         stale_reason = NULL WHERE tenant = ?1 AND id = ?2",
    )
    .bind(&ctx.tenant)
    .bind(id.to_string())
    .bind(status.as_str())
    .bind(rows::timestamp(now))
    .bind(superseded_by.map(|id| id.to_string()))
    .execute(&mut *conn)
    .await
    .map_err(db)?;
    let pending = citing_insights(conn, ctx, &[id], status).await?;
    mark_stale(conn, ctx, pending, now).await
}

async fn read_status(
    conn: &mut SqliteConnection,
    ctx: &RequestContext,
    id: Uuid,
) -> Result<InsightStatus> {
    let status: String =
        sqlx::query_scalar("SELECT status FROM insights WHERE tenant = ?1 AND id = ?2")
            .bind(&ctx.tenant)
            .bind(id.to_string())
            .fetch_optional(conn)
            .await
            .map_err(db)?
            .ok_or_else(|| Error::NotFound(format!("insight {id}")))?;
    parse(status, "status")
}

fn require(id: Uuid, status: InsightStatus, allowed: &[InsightStatus]) -> Result<()> {
    if allowed.contains(&status) {
        Ok(())
    } else {
        Err(Error::InvalidInput(format!(
            "insight {id} is {}",
            status.as_str()
        )))
    }
}

pub(super) async fn put_insight(
    store: &SqliteStore,
    ctx: &RequestContext,
    new: NewInsight,
) -> Result<Insight> {
    rules::statement(&new.statement)?;
    storable(&new.statement)?;
    validate::non_empty("written_by", &new.written_by)?;
    storable(&new.written_by)?;
    let sources = rules::sources(&new.sources)?;
    let vector = embedding(&new.embedding)?;
    if let Some(old) = new.supersedes {
        if sources.contains(&InsightSourceRef {
            kind: InsightSourceKind::Insight,
            id: old,
        }) {
            return Err(Error::InvalidInput(format!(
                "insight {old} cannot be both a source and superseded"
            )));
        }
    }
    let scope = new.scope.to_string();
    let mut tx = store.write_tx(ctx).await?;
    let now = Utc::now();
    let (facts, found) = read_sources(&mut tx, ctx, &sources, rows::timestamp(now)).await?;
    let lowest = rules::check_sources(&new.scope, new.level, &sources, &facts, &found)?;
    let trust = new.trust.min(lowest);
    let status = if insight_requires_quarantine(&new.scope, new.level, trust) {
        InsightStatus::Quarantined
    } else {
        InsightStatus::Active
    };
    if let Some(old) = new.supersedes {
        let row = sqlx::query("SELECT scope, status FROM insights WHERE tenant = ?1 AND id = ?2")
            .bind(&ctx.tenant)
            .bind(old.to_string())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?;
        let valid = match &row {
            Some(row) => {
                let old_scope: String = row.try_get("scope").map_err(db)?;
                let old_status: InsightStatus =
                    parse(row.try_get("status").map_err(db)?, "status")?;
                old_scope == scope
                    && matches!(old_status, InsightStatus::Active | InsightStatus::Stale)
            }
            None => false,
        };
        if !valid {
            return Err(Error::InvalidInput(format!(
                "insight {old} is not an active or stale insight of this scope"
            )));
        }
    }
    let id = Uuid::now_v7();
    let pending = new
        .supersedes
        .filter(|_| status == InsightStatus::Quarantined);
    sqlx::query(
        "INSERT INTO insights (id, tenant, scope, level, statement, trust, status, valid_from, \
         pending_supersede_of, written_by, recorded_at) \
         VALUES (?2, ?1, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?8)",
    )
    .bind(&ctx.tenant)
    .bind(id.to_string())
    .bind(&scope)
    .bind(new.level.as_str())
    .bind(&new.statement)
    .bind(trust.as_str())
    .bind(status.as_str())
    .bind(rows::timestamp(now))
    .bind(pending.map(|id| id.to_string()))
    .bind(&new.written_by)
    .execute(&mut *tx)
    .await
    .map_err(db)?;
    sqlx::query(
        "INSERT INTO insight_embeddings (rowid, embedding) \
         SELECT row_id, ?3 FROM insights WHERE tenant = ?1 AND id = ?2",
    )
    .bind(&ctx.tenant)
    .bind(id.to_string())
    .bind(vector)
    .execute(&mut *tx)
    .await
    .map_err(db)?;
    insert_sources(&mut tx, ctx, id, &sources).await?;
    if let (Some(old), InsightStatus::Active) = (new.supersedes, status) {
        close(&mut tx, ctx, old, InsightStatus::Superseded, Some(id), now).await?;
    }
    audit(
        &mut tx,
        ctx,
        "insight.write",
        Some(id),
        json!({
            "scope": scope,
            "level": new.level.as_str(),
            "status": status.as_str(),
            "trust": trust.as_str(),
            "sources": sources.len(),
            "supersedes": new.supersedes,
        }),
        now,
    )
    .await?;
    let result = load(&mut tx, ctx, id).await?;
    tx.commit().await.map_err(db)?;
    Ok(result)
}

/// As on PostgreSQL: an insight's sources as they are now, in the order of `sources`. Shared
/// with the reflect batch.
pub(crate) async fn source_views(
    conn: &mut SqliteConnection,
    ctx: &RequestContext,
    sources: &[InsightSourceRef],
    now: DateTime<Utc>,
) -> Result<Vec<InsightSourceView>> {
    let fact_ids = ids_json(rules::ids_of(sources, InsightSourceKind::Fact))?;
    let insight_ids = ids_json(rules::ids_of(sources, InsightSourceKind::Insight))?;
    let fact_sql = format!(
        "SELECT f.id, f.statement, ({}) AS current FROM facts f \
         WHERE f.tenant = ?1 AND f.id IN (SELECT value FROM json_each(?2))",
        rows::visible(3, 3)
    );
    let mut found_views: HashMap<InsightSourceRef, (String, bool)> = HashMap::new();
    for (kind, sql, ids) in [
        (InsightSourceKind::Fact, fact_sql.as_str(), &fact_ids),
        (
            InsightSourceKind::Insight,
            "SELECT id, statement, status = 'active' AS current FROM insights \
             WHERE tenant = ?1 AND id IN (SELECT value FROM json_each(?2)) AND ?3 IS NOT NULL",
            &insight_ids,
        ),
    ] {
        let found = sqlx::query(AssertSqlSafe(sql))
            .bind(&ctx.tenant)
            .bind(ids)
            .bind(rows::timestamp(now))
            .fetch_all(&mut *conn)
            .await
            .map_err(db)?;
        for row in &found {
            found_views.insert(
                InsightSourceRef {
                    kind,
                    id: uuid(row, "id")?,
                },
                (
                    row.try_get("statement").map_err(db)?,
                    row.try_get("current").map_err(db)?,
                ),
            );
        }
    }
    let views = sources
        .iter()
        .map(|source| {
            let found = found_views.remove(source);
            InsightSourceView {
                source: *source,
                current: found.as_ref().is_some_and(|(_, current)| *current),
                statement: found.map(|(statement, _)| statement),
            }
        })
        .collect();
    Ok(views)
}

pub(super) async fn get_insight(
    store: &SqliteStore,
    ctx: &RequestContext,
    id: Uuid,
) -> Result<InsightDetail> {
    let mut tx = store.write_tx(ctx).await?;
    let now = Utc::now();
    let scope: String =
        sqlx::query_scalar("SELECT scope FROM insights WHERE tenant = ?1 AND id = ?2")
            .bind(&ctx.tenant)
            .bind(id.to_string())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?
            .ok_or_else(|| Error::NotFound(format!("insight {id}")))?;
    expire(&mut tx, ctx, &[scope], now).await?;
    let insight = load(&mut tx, ctx, id).await?;

    let source_views = source_views(&mut tx, ctx, &insight.sources, now).await?;

    let sql = format!(
        "WITH RECURSIVE replaced(id) AS ( \
             SELECT id FROM insights WHERE tenant = ?1 AND superseded_by = ?2 \
             UNION \
             SELECT i.id FROM insights i JOIN replaced r ON i.superseded_by = r.id \
             WHERE i.tenant = ?1) \
         SELECT {INSIGHT_COLS} FROM insights i JOIN replaced r ON r.id = i.id \
         WHERE i.tenant = ?1 ORDER BY i.recorded_at DESC, i.id DESC"
    );
    let found = sqlx::query(AssertSqlSafe(sql))
        .bind(&ctx.tenant)
        .bind(id.to_string())
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?;
    let history = insights(&found)?;
    tx.commit().await.map_err(db)?;
    Ok(InsightDetail {
        insight,
        source_views,
        history,
    })
}

pub(super) async fn list_insights(
    store: &SqliteStore,
    ctx: &RequestContext,
    scopes: &[Scope],
    filter: &InsightFilter,
    limit: i64,
) -> Result<Vec<Insight>> {
    let limit = rules::list_limit(limit)?;
    if scopes.is_empty() {
        return Ok(Vec::new());
    }
    let scopes = validate::scope_strings(scopes);
    let mut tx = store.write_tx(ctx).await?;
    let now = Utc::now();
    expire(&mut tx, ctx, &scopes, now).await?;
    let sql = format!(
        "SELECT {INSIGHT_COLS} FROM insights i \
         WHERE i.tenant = ?1 AND i.scope IN (SELECT value FROM json_each(?2)) \
           AND (?3 IS NULL OR i.level = ?3) AND (?4 IS NULL OR i.status = ?4) \
         ORDER BY i.recorded_at DESC, i.id DESC LIMIT ?5"
    );
    let found = sqlx::query(AssertSqlSafe(sql))
        .bind(&ctx.tenant)
        .bind(json_array(&scopes)?)
        .bind(filter.level.map(|level| level.as_str()))
        .bind(filter.status.map(|status| status.as_str()))
        .bind(limit)
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?;
    let result = insights(&found)?;
    tx.commit().await.map_err(db)?;
    Ok(result)
}

pub(super) async fn confirm_insight(
    store: &SqliteStore,
    ctx: &RequestContext,
    id: Uuid,
    sources: &[InsightSourceRef],
    writer: Trust,
) -> Result<Insight> {
    let sources = rules::sources(sources)?;
    let mut tx = store.write_tx(ctx).await?;
    let now = Utc::now();
    let row =
        sqlx::query("SELECT scope, level, status FROM insights WHERE tenant = ?1 AND id = ?2")
            .bind(&ctx.tenant)
            .bind(id.to_string())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?
            .ok_or_else(|| Error::NotFound(format!("insight {id}")))?;
    let scope: Scope = parse(row.try_get("scope").map_err(db)?, "scope")?;
    let level = parse(row.try_get("level").map_err(db)?, "level")?;
    let status: InsightStatus = parse(row.try_get("status").map_err(db)?, "status")?;
    let (facts, found) = read_sources(&mut tx, ctx, &sources, rows::timestamp(now)).await?;
    let lowest = rules::check_sources(&scope, level, &sources, &facts, &found)?;
    require(id, status, &[InsightStatus::Stale])?;
    let trust = writer.min(lowest);
    let status = if insight_requires_quarantine(&scope, level, trust) {
        InsightStatus::Quarantined
    } else {
        InsightStatus::Active
    };
    sqlx::query("DELETE FROM insight_sources WHERE tenant = ?1 AND insight_id = ?2")
        .bind(&ctx.tenant)
        .bind(id.to_string())
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    insert_sources(&mut tx, ctx, id, &sources).await?;
    sqlx::query(
        "UPDATE insights SET status = ?3, trust = ?4, stale_reason = NULL \
         WHERE tenant = ?1 AND id = ?2",
    )
    .bind(&ctx.tenant)
    .bind(id.to_string())
    .bind(status.as_str())
    .bind(trust.as_str())
    .execute(&mut *tx)
    .await
    .map_err(db)?;
    audit(
        &mut tx,
        ctx,
        "insight.confirm",
        Some(id),
        json!({ "status": status.as_str(), "trust": trust.as_str(), "sources": sources.len() }),
        now,
    )
    .await?;
    let result = load(&mut tx, ctx, id).await?;
    tx.commit().await.map_err(db)?;
    Ok(result)
}

pub(super) async fn retire_insight(
    store: &SqliteStore,
    ctx: &RequestContext,
    id: Uuid,
    reason: &str,
) -> Result<Insight> {
    storable(reason)?;
    let mut tx = store.write_tx(ctx).await?;
    let now = Utc::now();
    let status = read_status(&mut tx, ctx, id).await?;
    require(id, status, &[InsightStatus::Active, InsightStatus::Stale])?;
    close(&mut tx, ctx, id, InsightStatus::Retired, None, now).await?;
    audit(
        &mut tx,
        ctx,
        "insight.retire",
        Some(id),
        json!({ "from_status": status.as_str(), "reason": reason }),
        now,
    )
    .await?;
    let result = load(&mut tx, ctx, id).await?;
    tx.commit().await.map_err(db)?;
    Ok(result)
}

pub(super) async fn list_quarantined_insights(
    store: &SqliteStore,
    ctx: &RequestContext,
    scopes: &[Scope],
    limit: i64,
) -> Result<Vec<Insight>> {
    let limit = validate::limit(limit)?;
    if scopes.is_empty() {
        return Ok(Vec::new());
    }
    let scopes = validate::scope_strings(scopes);
    let mut tx = store.write_tx(ctx).await?;
    expire(&mut tx, ctx, &scopes, Utc::now()).await?;
    let sql = format!(
        "SELECT {INSIGHT_COLS} FROM insights i \
         WHERE i.tenant = ?1 AND i.scope IN (SELECT value FROM json_each(?2)) \
           AND i.status = 'quarantined' ORDER BY i.recorded_at, i.id LIMIT ?3"
    );
    let found = sqlx::query(AssertSqlSafe(sql))
        .bind(&ctx.tenant)
        .bind(json_array(&scopes)?)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?;
    let result = insights(&found)?;
    tx.commit().await.map_err(db)?;
    Ok(result)
}

async fn changed_sources(
    conn: &mut SqliteConnection,
    ctx: &RequestContext,
    sources: &[InsightSourceRef],
    trust: Trust,
    now: DateTime<Utc>,
) -> Result<Vec<StaleSource>> {
    let states = fact_states(
        conn,
        ctx,
        &rules::ids_of(sources, InsightSourceKind::Fact),
        rows::timestamp(now),
    )
    .await?;
    let found = sqlx::query(
        "SELECT id, status FROM insights \
         WHERE tenant = ?1 AND id IN (SELECT value FROM json_each(?2))",
    )
    .bind(&ctx.tenant)
    .bind(ids_json(rules::ids_of(
        sources,
        InsightSourceKind::Insight,
    ))?)
    .fetch_all(&mut *conn)
    .await
    .map_err(db)?;
    let mut statuses: HashMap<Uuid, InsightStatus> = HashMap::new();
    for row in &found {
        statuses.insert(
            uuid(row, "id")?,
            parse(row.try_get("status").map_err(db)?, "status")?,
        );
    }
    let mut changes = Vec::new();
    for source in sources {
        let change = match source.kind {
            InsightSourceKind::Fact => rules::fact_change(states.get(&source.id), trust),
            InsightSourceKind::Insight => statuses
                .get(&source.id)
                .and_then(|status| rules::insight_change(*status)),
        };
        if let Some(change) = change {
            changes.push(rules::stale(source.kind, source.id, change));
        }
    }
    Ok(changes)
}

pub(super) async fn approve_insight(
    store: &SqliteStore,
    ctx: &RequestContext,
    id: Uuid,
) -> Result<Insight> {
    let mut tx = store.write_tx(ctx).await?;
    let now = Utc::now();
    let before = load(&mut tx, ctx, id).await?;
    require(id, before.status, &[InsightStatus::Quarantined])?;
    let changes = changed_sources(&mut tx, ctx, &before.sources, before.trust, now).await?;
    let (status, reason) = if changes.is_empty() {
        (InsightStatus::Active, None)
    } else {
        (InsightStatus::Stale, Some(rules::encode_reason(&changes)?))
    };
    sqlx::query(
        "UPDATE insights SET status = ?3, stale_reason = ?4, pending_supersede_of = NULL \
         WHERE tenant = ?1 AND id = ?2",
    )
    .bind(&ctx.tenant)
    .bind(id.to_string())
    .bind(status.as_str())
    .bind(reason.as_ref().map(ToString::to_string))
    .execute(&mut *tx)
    .await
    .map_err(db)?;
    let mut superseded = None;
    if let Some(old) = before.pending_supersede_of {
        let old_status = read_status(&mut tx, ctx, old).await.ok();
        if matches!(
            old_status,
            Some(InsightStatus::Active | InsightStatus::Stale)
        ) {
            close(&mut tx, ctx, old, InsightStatus::Superseded, Some(id), now).await?;
            superseded = Some(old);
        }
    }
    audit(
        &mut tx,
        ctx,
        "insight.approve",
        Some(id),
        json!({ "status": status.as_str(), "superseded": superseded }),
        now,
    )
    .await?;
    if let Some(reason) = reason {
        audit(
            &mut tx,
            ctx,
            "insight.stale",
            Some(id),
            json!({ "from_status": InsightStatus::Quarantined.as_str(), "reason": reason }),
            now,
        )
        .await?;
    }
    let result = load(&mut tx, ctx, id).await?;
    tx.commit().await.map_err(db)?;
    Ok(result)
}

pub(super) async fn reject_insight(
    store: &SqliteStore,
    ctx: &RequestContext,
    id: Uuid,
) -> Result<Insight> {
    let mut tx = store.write_tx(ctx).await?;
    let now = Utc::now();
    let status = read_status(&mut tx, ctx, id).await?;
    require(id, status, &[InsightStatus::Quarantined])?;
    sqlx::query(
        "UPDATE insights SET status = 'rejected', pending_supersede_of = NULL \
         WHERE tenant = ?1 AND id = ?2",
    )
    .bind(&ctx.tenant)
    .bind(id.to_string())
    .execute(&mut *tx)
    .await
    .map_err(db)?;
    audit(
        &mut tx,
        ctx,
        "insight.reject",
        Some(id),
        json!({ "from_status": status.as_str() }),
        now,
    )
    .await?;
    let result = load(&mut tx, ctx, id).await?;
    tx.commit().await.map_err(db)?;
    Ok(result)
}

pub(super) async fn search_insights_vector(
    store: &SqliteStore,
    ctx: &RequestContext,
    scopes: &[Scope],
    embedding_: &[f32],
    limit: i64,
) -> Result<Vec<ScoredInsight>> {
    let limit = validate::limit(limit)?;
    let query = embedding(embedding_)?;
    if scopes.is_empty() {
        return Ok(Vec::new());
    }
    let scopes = validate::scope_strings(scopes);
    let mut tx = store.write_tx(ctx).await?;
    expire(&mut tx, ctx, &scopes, Utc::now()).await?;
    let sql = format!(
        "SELECT {INSIGHT_COLS}, 1.0 - vec_distance_cosine(e.embedding, ?2) AS score \
         FROM insight_embeddings e \
         JOIN insights i ON i.row_id = e.rowid AND i.tenant = ?1 \
         WHERE i.scope IN (SELECT value FROM json_each(?3)) AND i.status = 'active' \
         ORDER BY vec_distance_cosine(e.embedding, ?2), i.id LIMIT ?4"
    );
    let found = sqlx::query(AssertSqlSafe(sql))
        .bind(&ctx.tenant)
        .bind(query)
        .bind(json_array(&scopes)?)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?;
    let result = found
        .iter()
        .map(|row| {
            Ok(ScoredInsight {
                insight: rows::insight(row)?,
                score: row.try_get("score").map_err(db)?,
            })
        })
        .collect::<Result<_>>()?;
    tx.commit().await.map_err(db)?;
    Ok(result)
}

pub(super) async fn search_insights_text(
    store: &SqliteStore,
    ctx: &RequestContext,
    scopes: &[Scope],
    query: &str,
    limit: i64,
) -> Result<Vec<ScoredInsight>> {
    let limit = validate::limit(limit)?;
    if scopes.is_empty() {
        return Ok(Vec::new());
    }
    storable(query)?;
    let scopes = validate::scope_strings(scopes);
    let mut tx = store.write_tx(ctx).await?;
    expire(&mut tx, ctx, &scopes, Utc::now()).await?;
    let Some(match_query) = safe_match_query(query) else {
        tx.commit().await.map_err(db)?;
        return Ok(Vec::new());
    };
    // As for facts: filters apply before the newest-candidate cap, and the score counts matched
    // terms per insight, so another tenant's corpus never affects it as bm25 statistics would.
    let sql = format!(
        "WITH candidates AS MATERIALIZED (\
             SELECT i.row_id AS rid FROM insights i \
             JOIN insights_fts ON insights_fts.rowid = i.row_id \
             WHERE i.tenant = ?1 AND insights_fts MATCH ?2 \
               AND i.scope IN (SELECT value FROM json_each(?3)) AND i.status = 'active' \
             ORDER BY i.id DESC LIMIT MAX(1000, ?4)),\
         ranked AS (\
             SELECT c.rid, highlight(insights_fts, 0, char(1), char(2)) AS highlighted \
             FROM candidates c JOIN insights_fts ON insights_fts.rowid = c.rid \
             WHERE insights_fts MATCH ?2)\
         SELECT {INSIGHT_COLS}, r.highlighted FROM ranked r \
         JOIN insights i ON i.row_id = r.rid AND i.tenant = ?1"
    );
    let found = sqlx::query(AssertSqlSafe(sql))
        .bind(&ctx.tenant)
        .bind(match_query)
        .bind(json_array(&scopes)?)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?;
    let mut result = Vec::with_capacity(found.len());
    for row in &found {
        let insight = rows::insight(row)?;
        let highlighted: String = row.try_get("highlighted").map_err(db)?;
        let inserted = highlighted.matches('\u{1}').count();
        let existing = insight.statement.matches('\u{1}').count();
        result.push(ScoredInsight {
            insight,
            score: inserted.saturating_sub(existing) as f64,
        });
    }
    result.sort_by(|a: &ScoredInsight, b: &ScoredInsight| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.insight.id.cmp(&b.insight.id))
    });
    result.truncate(limit as usize);
    tx.commit().await.map_err(db)?;
    Ok(result)
}

/// Active insights per scope for `scope_summaries`.
pub(super) async fn count_by_scope(
    conn: &mut SqliteConnection,
    ctx: &RequestContext,
) -> Result<Vec<(String, i64)>> {
    sqlx::query_as(
        "SELECT scope, sum(status = 'active') FROM insights WHERE tenant = ?1 GROUP BY scope",
    )
    .bind(&ctx.tenant)
    .fetch_all(conn)
    .await
    .map_err(db)
}

/// Deletes a scope's insights, with their sources by cascade and their vector and full-text rows
/// by trigger, and its reflect declines.
pub(super) async fn purge_scope(
    conn: &mut SqliteConnection,
    ctx: &RequestContext,
    scope: &str,
) -> Result<u64> {
    let deleted = sqlx::query("DELETE FROM insights WHERE tenant = ?1 AND scope = ?2")
        .bind(&ctx.tenant)
        .bind(scope)
        .execute(&mut *conn)
        .await
        .map_err(db)?
        .rows_affected();
    sqlx::query("DELETE FROM reflect_declines WHERE tenant = ?1 AND scope = ?2")
        .bind(&ctx.tenant)
        .bind(scope)
        .execute(conn)
        .await
        .map_err(db)?;
    Ok(deleted)
}
