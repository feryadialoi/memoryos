//! Insights on PostgreSQL (docs/SPEC.md section 8, Insights). SQL text is constant; every value is
//! a bind parameter.
//!
//! Staleness runs in the transaction of the change that causes it. Locks go facts first, then
//! insights: a write locks its fact sources `FOR SHARE`, so a commit that closes one of them either
//! waits for the write and then finds the new insight, or commits first and the write sees the
//! fact is no longer current. Quarantined insights are not marked stale: approval checks their
//! sources instead.

use std::collections::{BTreeMap, HashMap};

use memoryos_core::{
    insight_requires_quarantine, Error, Insight, InsightDetail, InsightFilter, InsightSourceKind,
    InsightSourceRef, InsightSourceView, InsightStatus, NewInsight, RequestContext, Result, Scope,
    ScoredInsight, SourceChange, StaleSource, Trust,
};
use pgvector::Vector;
use serde_json::json;
use sqlx::postgres::PgRow;
use sqlx::{AssertSqlSafe, PgConnection, Row};
use uuid::Uuid;

use crate::facts::audit;
use crate::insight_rules::{self as rules, FactState, FoundFact, FoundInsight};
use crate::rows::{self, INSIGHT_COLS};
use crate::store_impl::tune_vector_scan;
use crate::{db, validate, PgStore};

fn parse<T: std::str::FromStr>(text: String, what: &str) -> Result<T> {
    text.parse()
        .map_err(|_| Error::Storage(format!("insight {what} is invalid")))
}

async fn load(conn: &mut PgConnection, id: Uuid) -> Result<Insight> {
    let sql = format!("SELECT {INSIGHT_COLS} FROM insights i WHERE i.id = $1");
    let row = sqlx::query(AssertSqlSafe(sql))
        .bind(id)
        .fetch_optional(conn)
        .await
        .map_err(db)?
        .ok_or_else(|| Error::NotFound(format!("insight {id}")))?;
    rows::insight(&row)
}

fn insights(found: &[PgRow]) -> Result<Vec<Insight>> {
    found.iter().map(rows::insight).collect()
}

/// Locks the sources of a write, facts then insights, each in id order, and reads what the source
/// checks need.
async fn lock_sources(
    conn: &mut PgConnection,
    sources: &[InsightSourceRef],
) -> Result<(Vec<FoundFact>, Vec<FoundInsight>)> {
    let fact_ids = rules::ids_of(sources, InsightSourceKind::Fact);
    let insight_ids = rules::ids_of(sources, InsightSourceKind::Insight);
    let sql = format!(
        "SELECT f.id, f.scope, f.trust, ({}) AS current FROM facts f \
         WHERE f.id = ANY($1) ORDER BY f.id FOR SHARE",
        rows::visible_now()
    );
    let found = sqlx::query(AssertSqlSafe(sql))
        .bind(&fact_ids)
        .fetch_all(&mut *conn)
        .await
        .map_err(db)?;
    let facts = found
        .iter()
        .map(|row| {
            Ok(FoundFact {
                id: row.try_get("id").map_err(db)?,
                scope: row.try_get("scope").map_err(db)?,
                trust: parse(row.try_get("trust").map_err(db)?, "source trust")?,
                current: row.try_get("current").map_err(db)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let found = sqlx::query(
        "SELECT id, scope, level, trust, status FROM insights \
         WHERE id = ANY($1) ORDER BY id FOR SHARE",
    )
    .bind(&insight_ids)
    .fetch_all(&mut *conn)
    .await
    .map_err(db)?;
    let insights = found
        .iter()
        .map(|row| {
            Ok(FoundInsight {
                id: row.try_get("id").map_err(db)?,
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
    conn: &mut PgConnection,
    ctx: &RequestContext,
    id: Uuid,
    sources: &[InsightSourceRef],
) -> Result<()> {
    let kinds: Vec<&str> = sources.iter().map(|s| s.kind.as_str()).collect();
    let ids: Vec<Uuid> = sources.iter().map(|s| s.id).collect();
    sqlx::query(
        "INSERT INTO insight_sources (insight_id, tenant, source_kind, source_id) \
         SELECT $1, $2, k, s FROM unnest($3::text[], $4::uuid[]) AS u(k, s)",
    )
    .bind(id)
    .bind(&ctx.tenant)
    .bind(&kinds)
    .bind(&ids)
    .execute(conn)
    .await
    .map_err(db)?;
    Ok(())
}

/// Insights citing any of `ids` as an insight source, each with the change `status` means for it.
async fn citing_insights(
    conn: &mut PgConnection,
    ids: &[Uuid],
    status: InsightStatus,
) -> Result<Vec<(Uuid, StaleSource)>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let found = sqlx::query(
        "SELECT insight_id, source_id FROM insight_sources \
         WHERE source_kind = 'insight' AND source_id = ANY($1)",
    )
    .bind(ids)
    .fetch_all(conn)
    .await
    .map_err(db)?;
    found
        .iter()
        .map(|row| {
            let source: Uuid = row.try_get("source_id").map_err(db)?;
            Ok((
                row.try_get("insight_id").map_err(db)?,
                rules::stale(
                    InsightSourceKind::Insight,
                    source,
                    SourceChange::InsightChanged { status },
                ),
            ))
        })
        .collect()
}

/// Marks insights stale for the given changed sources, merging into the reason of one already
/// stale, and carries each insight that turns stale up to the insights citing it, every level.
/// Only active and stale insights change. Writes `insight.stale` for each insight whose reason
/// changed.
async fn mark_stale(
    conn: &mut PgConnection,
    ctx: &RequestContext,
    mut pending: Vec<(Uuid, StaleSource)>,
) -> Result<()> {
    while !pending.is_empty() {
        let mut by_insight: BTreeMap<Uuid, Vec<StaleSource>> = BTreeMap::new();
        for (id, change) in pending.drain(..) {
            by_insight.entry(id).or_default().push(change);
        }
        let ids: Vec<Uuid> = by_insight.keys().copied().collect();
        let found = sqlx::query(
            "SELECT id, status, stale_reason FROM insights \
             WHERE id = ANY($1) AND status IN ('active', 'stale') ORDER BY id FOR UPDATE",
        )
        .bind(&ids)
        .fetch_all(&mut *conn)
        .await
        .map_err(db)?;
        let mut turned = Vec::new();
        for row in &found {
            let id: Uuid = row.try_get("id").map_err(db)?;
            let status: InsightStatus = parse(row.try_get("status").map_err(db)?, "status")?;
            let existing: Option<serde_json::Value> = row.try_get("stale_reason").map_err(db)?;
            let existing: Option<Vec<StaleSource>> = existing
                .map(serde_json::from_value)
                .transpose()
                .map_err(|e| Error::Storage(format!("insight stale_reason is invalid: {e}")))?;
            let changes = by_insight.get(&id).map(Vec::as_slice).unwrap_or_default();
            let (reason, changed) = rules::merge_reason(existing, changes);
            if !changed {
                continue;
            }
            let encoded = rules::encode_reason(&reason)?;
            sqlx::query("UPDATE insights SET status = 'stale', stale_reason = $2 WHERE id = $1")
                .bind(id)
                .bind(&encoded)
                .execute(&mut *conn)
                .await
                .map_err(db)?;
            audit(
                conn,
                ctx,
                "insight.stale",
                Some(id),
                json!({ "from_status": status.as_str(), "reason": encoded }),
            )
            .await?;
            if status == InsightStatus::Active {
                turned.push(id);
            }
        }
        pending = citing_insights(conn, &turned, InsightStatus::Stale).await?;
    }
    Ok(())
}

/// Marks stale the active and stale insights citing any of `fact_ids` whose state changed, as
/// each fact now stands. A fact whose state did not change, such as a corroborated one, changes
/// nothing.
pub(crate) async fn facts_changed(
    conn: &mut PgConnection,
    ctx: &RequestContext,
    fact_ids: &[Uuid],
) -> Result<()> {
    if fact_ids.is_empty() {
        return Ok(());
    }
    let cites = sqlx::query(
        "SELECT s.insight_id, s.source_id, i.trust FROM insight_sources s \
         JOIN insights i ON i.id = s.insight_id \
         WHERE s.source_kind = 'fact' AND s.source_id = ANY($1) \
           AND i.status IN ('active', 'stale')",
    )
    .bind(fact_ids)
    .fetch_all(&mut *conn)
    .await
    .map_err(db)?;
    if cites.is_empty() {
        return Ok(());
    }
    let cited: Vec<Uuid> = cites
        .iter()
        .map(|row| row.try_get("source_id").map_err(db))
        .collect::<Result<_>>()?;
    let found = sqlx::query(
        "SELECT id, invalidated_at IS NOT NULL AS invalidated, superseded_by, \
             status = 'active' AS active, trust, \
             (expires_at IS NOT NULL AND expires_at <= now()) AS expired \
         FROM facts WHERE id = ANY($1)",
    )
    .bind(&cited)
    .fetch_all(&mut *conn)
    .await
    .map_err(db)?;
    let mut states: HashMap<Uuid, FactState> = HashMap::new();
    for row in &found {
        states.insert(
            row.try_get("id").map_err(db)?,
            FactState {
                invalidated: row.try_get("invalidated").map_err(db)?,
                superseded_by: row.try_get("superseded_by").map_err(db)?,
                active: row.try_get("active").map_err(db)?,
                trust: parse(row.try_get("trust").map_err(db)?, "source trust")?,
                expired: row.try_get("expired").map_err(db)?,
            },
        );
    }
    let mut pending = Vec::new();
    for row in &cites {
        let source: Uuid = row.try_get("source_id").map_err(db)?;
        let trust: Trust = parse(row.try_get("trust").map_err(db)?, "trust")?;
        if let Some(change) = rules::fact_change(states.get(&source), trust) {
            pending.push((
                row.try_get("insight_id").map_err(db)?,
                rules::stale(InsightSourceKind::Fact, source, change),
            ));
        }
    }
    mark_stale(conn, ctx, pending).await
}

/// The facts an apply may have closed: those it names, and those now superseded by a fact it
/// names or inserted. Corroborated facts are included and change nothing.
pub(crate) async fn after_apply(
    conn: &mut PgConnection,
    ctx: &RequestContext,
    named: &[Uuid],
    inserted: &[Uuid],
) -> Result<()> {
    let mut touched: Vec<Uuid> = named.iter().chain(inserted).copied().collect();
    let closed: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM facts WHERE superseded_by = ANY($1)")
            .bind(&touched)
            .fetch_all(&mut *conn)
            .await
            .map_err(db)?;
    touched.extend(closed);
    touched.sort_unstable();
    touched.dedup();
    facts_changed(conn, ctx, &touched).await
}

/// Facts a purge of `episode_id` or a retraction of a fact may change: `ids` and the facts each
/// of them closed, plus those the episode closed through an existing fact. Read after the caller
/// has locked `ids`, before the change.
pub(crate) async fn chain_neighbours(
    conn: &mut PgConnection,
    ids: &[Uuid],
    episode_id: Option<Uuid>,
) -> Result<Vec<Uuid>> {
    let mut touched: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM facts WHERE superseded_by = ANY($1) \
         UNION SELECT id FROM facts WHERE $2::uuid IS NOT NULL AND closed_by_episode = $2",
    )
    .bind(ids)
    .bind(episode_id)
    .fetch_all(conn)
    .await
    .map_err(db)?;
    touched.extend_from_slice(ids);
    touched.sort_unstable();
    touched.dedup();
    Ok(touched)
}

/// Lazy expiry: marks stale, with change `expired`, the active insights of `scopes` citing a fact
/// that has expired. Expiry is a time, so no write marks it.
pub(crate) async fn expire(
    conn: &mut PgConnection,
    ctx: &RequestContext,
    scopes: &[String],
) -> Result<()> {
    let found = sqlx::query(
        "SELECT s.insight_id, s.source_id FROM insights i \
         JOIN insight_sources s ON s.insight_id = i.id AND s.source_kind = 'fact' \
         JOIN facts f ON f.id = s.source_id \
         WHERE i.scope = ANY($1) AND i.status = 'active' AND f.expires_at <= now()",
    )
    .bind(scopes)
    .fetch_all(&mut *conn)
    .await
    .map_err(db)?;
    let pending = found
        .iter()
        .map(|row| {
            Ok((
                row.try_get("insight_id").map_err(db)?,
                rules::stale(
                    InsightSourceKind::Fact,
                    row.try_get("source_id").map_err(db)?,
                    SourceChange::Expired,
                ),
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    mark_stale(conn, ctx, pending).await
}

/// Ends an active or stale insight as retired or superseded, and makes the insights citing it
/// stale.
async fn close(
    conn: &mut PgConnection,
    ctx: &RequestContext,
    id: Uuid,
    status: InsightStatus,
    superseded_by: Option<Uuid>,
) -> Result<()> {
    sqlx::query(
        "UPDATE insights SET status = $2, valid_to = now(), superseded_by = $3, \
         stale_reason = NULL WHERE id = $1",
    )
    .bind(id)
    .bind(status.as_str())
    .bind(superseded_by)
    .execute(&mut *conn)
    .await
    .map_err(db)?;
    let pending = citing_insights(conn, &[id], status).await?;
    mark_stale(conn, ctx, pending).await
}

/// Locks an insight and returns its status.
async fn lock_status(conn: &mut PgConnection, id: Uuid) -> Result<InsightStatus> {
    let status: String = sqlx::query_scalar("SELECT status FROM insights WHERE id = $1 FOR UPDATE")
        .bind(id)
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

pub(crate) async fn put_insight(
    store: &PgStore,
    ctx: &RequestContext,
    new: NewInsight,
) -> Result<Insight> {
    rules::statement(&new.statement)?;
    validate::non_empty("written_by", &new.written_by)?;
    let sources = rules::sources(&new.sources)?;
    let embedding: Vector = validate::embedding(&new.embedding)?;
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
    let mut tx = store.tenant_tx(ctx).await?;
    let (facts, found) = lock_sources(&mut tx, &sources).await?;
    let lowest = rules::check_sources(&new.scope, new.level, &sources, &facts, &found)?;
    let trust = new.trust.min(lowest);
    let status = if insight_requires_quarantine(&new.scope, new.level, trust) {
        InsightStatus::Quarantined
    } else {
        InsightStatus::Active
    };
    if let Some(old) = new.supersedes {
        let row = sqlx::query("SELECT scope, status FROM insights WHERE id = $1 FOR UPDATE")
            .bind(old)
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
         pending_supersede_of, written_by, embedding) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, now(), $8, $9, $10)",
    )
    .bind(id)
    .bind(&ctx.tenant)
    .bind(&scope)
    .bind(new.level.as_str())
    .bind(&new.statement)
    .bind(trust.as_str())
    .bind(status.as_str())
    .bind(pending)
    .bind(&new.written_by)
    .bind(embedding)
    .execute(&mut *tx)
    .await
    .map_err(db)?;
    insert_sources(&mut tx, ctx, id, &sources).await?;
    if let (Some(old), InsightStatus::Active) = (new.supersedes, status) {
        close(&mut tx, ctx, old, InsightStatus::Superseded, Some(id)).await?;
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
    )
    .await?;
    let result = load(&mut tx, id).await?;
    tx.commit().await.map_err(db)?;
    Ok(result)
}

/// An insight's sources as they are now: each fact's or insight's statement, `None` once purged,
/// and whether it is still current, in the order of `sources`. Shared with the reflect batch.
pub(crate) async fn source_views(
    conn: &mut PgConnection,
    sources: &[InsightSourceRef],
) -> Result<Vec<InsightSourceView>> {
    let fact_ids = rules::ids_of(sources, InsightSourceKind::Fact);
    let insight_ids = rules::ids_of(sources, InsightSourceKind::Insight);
    let sql = format!(
        "SELECT f.id, f.statement, ({}) AS current FROM facts f WHERE f.id = ANY($1)",
        rows::visible_now()
    );
    let mut found_views: HashMap<InsightSourceRef, (String, bool)> = HashMap::new();
    for (kind, sql, ids) in [
        (InsightSourceKind::Fact, sql.as_str(), &fact_ids),
        (
            InsightSourceKind::Insight,
            "SELECT id, statement, status = 'active' AS current FROM insights WHERE id = ANY($1)",
            &insight_ids,
        ),
    ] {
        let found = sqlx::query(AssertSqlSafe(sql))
            .bind(ids)
            .fetch_all(&mut *conn)
            .await
            .map_err(db)?;
        for row in &found {
            found_views.insert(
                InsightSourceRef {
                    kind,
                    id: row.try_get("id").map_err(db)?,
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

pub(crate) async fn get_insight(
    store: &PgStore,
    ctx: &RequestContext,
    id: Uuid,
) -> Result<InsightDetail> {
    let mut tx = store.tenant_tx(ctx).await?;
    let scope: String = sqlx::query_scalar("SELECT scope FROM insights WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?
        .ok_or_else(|| Error::NotFound(format!("insight {id}")))?;
    expire(&mut tx, ctx, &[scope]).await?;
    let insight = load(&mut tx, id).await?;

    let source_views = source_views(&mut tx, &insight.sources).await?;

    let sql = format!(
        "WITH RECURSIVE replaced(id) AS ( \
             SELECT id FROM insights WHERE superseded_by = $1 \
             UNION \
             SELECT i.id FROM insights i JOIN replaced r ON i.superseded_by = r.id) \
         SELECT {INSIGHT_COLS} FROM insights i JOIN replaced r ON r.id = i.id \
         ORDER BY i.recorded_at DESC, i.id DESC"
    );
    let found = sqlx::query(AssertSqlSafe(sql))
        .bind(id)
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

pub(crate) async fn list_insights(
    store: &PgStore,
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
    let mut tx = store.tenant_tx(ctx).await?;
    expire(&mut tx, ctx, &scopes).await?;
    let sql = format!(
        "SELECT {INSIGHT_COLS} FROM insights i \
         WHERE i.scope = ANY($1) AND ($2::text IS NULL OR i.level = $2) \
           AND ($3::text IS NULL OR i.status = $3) \
         ORDER BY i.recorded_at DESC, i.id DESC LIMIT $4"
    );
    let found = sqlx::query(AssertSqlSafe(sql))
        .bind(&scopes)
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

pub(crate) async fn confirm_insight(
    store: &PgStore,
    ctx: &RequestContext,
    id: Uuid,
    sources: &[InsightSourceRef],
    writer: Trust,
) -> Result<Insight> {
    let sources = rules::sources(sources)?;
    let mut tx = store.tenant_tx(ctx).await?;
    // Scope and level never change, so they are read before the locks, which go sources first.
    let row = sqlx::query("SELECT scope, level FROM insights WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?
        .ok_or_else(|| Error::NotFound(format!("insight {id}")))?;
    let scope: Scope = parse(row.try_get("scope").map_err(db)?, "scope")?;
    let level = parse(row.try_get("level").map_err(db)?, "level")?;
    let (facts, found) = lock_sources(&mut tx, &sources).await?;
    let lowest = rules::check_sources(&scope, level, &sources, &facts, &found)?;
    let row = sqlx::query("SELECT status FROM insights WHERE id = $1 FOR UPDATE")
        .bind(id)
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
    let status: InsightStatus = parse(row.try_get("status").map_err(db)?, "status")?;
    require(id, status, &[InsightStatus::Stale])?;
    let trust = writer.min(lowest);
    let status = if insight_requires_quarantine(&scope, level, trust) {
        InsightStatus::Quarantined
    } else {
        InsightStatus::Active
    };
    sqlx::query("DELETE FROM insight_sources WHERE insight_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    insert_sources(&mut tx, ctx, id, &sources).await?;
    sqlx::query("UPDATE insights SET status = $2, trust = $3, stale_reason = NULL WHERE id = $1")
        .bind(id)
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
    )
    .await?;
    let result = load(&mut tx, id).await?;
    tx.commit().await.map_err(db)?;
    Ok(result)
}

pub(crate) async fn retire_insight(
    store: &PgStore,
    ctx: &RequestContext,
    id: Uuid,
    reason: &str,
) -> Result<Insight> {
    let mut tx = store.tenant_tx(ctx).await?;
    let status = lock_status(&mut tx, id).await?;
    require(id, status, &[InsightStatus::Active, InsightStatus::Stale])?;
    close(&mut tx, ctx, id, InsightStatus::Retired, None).await?;
    audit(
        &mut tx,
        ctx,
        "insight.retire",
        Some(id),
        json!({ "from_status": status.as_str(), "reason": reason }),
    )
    .await?;
    let result = load(&mut tx, id).await?;
    tx.commit().await.map_err(db)?;
    Ok(result)
}

pub(crate) async fn list_quarantined_insights(
    store: &PgStore,
    ctx: &RequestContext,
    scopes: &[Scope],
    limit: i64,
) -> Result<Vec<Insight>> {
    let limit = validate::limit(limit)?;
    if scopes.is_empty() {
        return Ok(Vec::new());
    }
    let scopes = validate::scope_strings(scopes);
    let mut tx = store.tenant_tx(ctx).await?;
    expire(&mut tx, ctx, &scopes).await?;
    let sql = format!(
        "SELECT {INSIGHT_COLS} FROM insights i \
         WHERE i.scope = ANY($1) AND i.status = 'quarantined' \
         ORDER BY i.recorded_at, i.id LIMIT $2"
    );
    let found = sqlx::query(AssertSqlSafe(sql))
        .bind(&scopes)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?;
    let result = insights(&found)?;
    tx.commit().await.map_err(db)?;
    Ok(result)
}

/// The changes to the sources of a quarantined insight since it was written, as they stand now.
async fn changed_sources(
    conn: &mut PgConnection,
    sources: &[InsightSourceRef],
    trust: Trust,
) -> Result<Vec<StaleSource>> {
    let fact_ids = rules::ids_of(sources, InsightSourceKind::Fact);
    let insight_ids = rules::ids_of(sources, InsightSourceKind::Insight);
    let found = sqlx::query(
        "SELECT id, invalidated_at IS NOT NULL AS invalidated, superseded_by, \
             status = 'active' AS active, trust, \
             (expires_at IS NOT NULL AND expires_at <= now()) AS expired \
         FROM facts WHERE id = ANY($1) ORDER BY id FOR SHARE",
    )
    .bind(&fact_ids)
    .fetch_all(&mut *conn)
    .await
    .map_err(db)?;
    let mut states: HashMap<Uuid, FactState> = HashMap::new();
    for row in &found {
        states.insert(
            row.try_get("id").map_err(db)?,
            FactState {
                invalidated: row.try_get("invalidated").map_err(db)?,
                superseded_by: row.try_get("superseded_by").map_err(db)?,
                active: row.try_get("active").map_err(db)?,
                trust: parse(row.try_get("trust").map_err(db)?, "source trust")?,
                expired: row.try_get("expired").map_err(db)?,
            },
        );
    }
    let found =
        sqlx::query("SELECT id, status FROM insights WHERE id = ANY($1) ORDER BY id FOR SHARE")
            .bind(&insight_ids)
            .fetch_all(&mut *conn)
            .await
            .map_err(db)?;
    let mut statuses: HashMap<Uuid, InsightStatus> = HashMap::new();
    for row in &found {
        statuses.insert(
            row.try_get("id").map_err(db)?,
            parse(row.try_get("status").map_err(db)?, "status")?,
        );
    }
    let mut changes = Vec::new();
    for source in sources {
        let change = match source.kind {
            InsightSourceKind::Fact => rules::fact_change(states.get(&source.id), trust),
            // Insights are deleted only with their whole scope, this one included.
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

pub(crate) async fn approve_insight(
    store: &PgStore,
    ctx: &RequestContext,
    id: Uuid,
) -> Result<Insight> {
    let mut tx = store.tenant_tx(ctx).await?;
    let before = load(&mut tx, id).await?;
    let changes = changed_sources(&mut tx, &before.sources, before.trust).await?;
    let row =
        sqlx::query("SELECT status, pending_supersede_of FROM insights WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_one(&mut *tx)
            .await
            .map_err(db)?;
    let status: InsightStatus = parse(row.try_get("status").map_err(db)?, "status")?;
    require(id, status, &[InsightStatus::Quarantined])?;
    let pending: Option<Uuid> = row.try_get("pending_supersede_of").map_err(db)?;
    let (status, reason) = if changes.is_empty() {
        (InsightStatus::Active, None)
    } else {
        (InsightStatus::Stale, Some(rules::encode_reason(&changes)?))
    };
    sqlx::query(
        "UPDATE insights SET status = $2, stale_reason = $3, pending_supersede_of = NULL \
         WHERE id = $1",
    )
    .bind(id)
    .bind(status.as_str())
    .bind(&reason)
    .execute(&mut *tx)
    .await
    .map_err(db)?;
    // The review accepted the replacement, so it closes what it replaces even if a source has
    // changed meanwhile and it waits for a confirm. One already closed another way stays as is.
    let mut superseded = None;
    if let Some(old) = pending {
        let old_status: Option<String> =
            sqlx::query_scalar("SELECT status FROM insights WHERE id = $1 FOR UPDATE")
                .bind(old)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db)?;
        if matches!(old_status.as_deref(), Some("active" | "stale")) {
            close(&mut tx, ctx, old, InsightStatus::Superseded, Some(id)).await?;
            superseded = Some(old);
        }
    }
    audit(
        &mut tx,
        ctx,
        "insight.approve",
        Some(id),
        json!({ "status": status.as_str(), "superseded": superseded }),
    )
    .await?;
    if let Some(reason) = reason {
        audit(
            &mut tx,
            ctx,
            "insight.stale",
            Some(id),
            json!({ "from_status": InsightStatus::Quarantined.as_str(), "reason": reason }),
        )
        .await?;
    }
    let result = load(&mut tx, id).await?;
    tx.commit().await.map_err(db)?;
    Ok(result)
}

pub(crate) async fn reject_insight(
    store: &PgStore,
    ctx: &RequestContext,
    id: Uuid,
) -> Result<Insight> {
    let mut tx = store.tenant_tx(ctx).await?;
    let status = lock_status(&mut tx, id).await?;
    require(id, status, &[InsightStatus::Quarantined])?;
    sqlx::query(
        "UPDATE insights SET status = 'rejected', pending_supersede_of = NULL WHERE id = $1",
    )
    .bind(id)
    .execute(&mut *tx)
    .await
    .map_err(db)?;
    audit(
        &mut tx,
        ctx,
        "insight.reject",
        Some(id),
        json!({ "from_status": status.as_str() }),
    )
    .await?;
    let result = load(&mut tx, id).await?;
    tx.commit().await.map_err(db)?;
    Ok(result)
}

fn scored(found: &[PgRow]) -> Result<Vec<ScoredInsight>> {
    found
        .iter()
        .map(|row| {
            Ok(ScoredInsight {
                insight: rows::insight(row)?,
                score: row.try_get("score").map_err(db)?,
            })
        })
        .collect()
}

pub(crate) async fn search_insights_vector(
    store: &PgStore,
    ctx: &RequestContext,
    scopes: &[Scope],
    embedding: &[f32],
    limit: i64,
) -> Result<Vec<ScoredInsight>> {
    let limit = validate::limit(limit)?;
    let query: Vector = validate::embedding(embedding)?;
    if scopes.is_empty() {
        return Ok(Vec::new());
    }
    let scopes = validate::scope_strings(scopes);
    let mut tx = store.tenant_tx(ctx).await?;
    expire(&mut tx, ctx, &scopes).await?;
    tune_vector_scan(&mut tx, limit).await?;
    // The re-sort of the materialized result follows search_facts_vector.
    let sql = format!(
        "WITH r AS MATERIALIZED ( \
             SELECT i.id AS rid, i.embedding <=> $1 AS distance FROM insights i \
             WHERE i.scope = ANY($2) AND i.status = 'active' \
             ORDER BY distance LIMIT $3) \
         SELECT {INSIGHT_COLS}, 1 - r.distance AS score \
         FROM r JOIN insights i ON i.id = r.rid \
         ORDER BY r.distance + 0, i.id"
    );
    let found = sqlx::query(AssertSqlSafe(sql))
        .bind(query)
        .bind(&scopes)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?;
    let result = scored(&found)?;
    tx.commit().await.map_err(db)?;
    Ok(result)
}

pub(crate) async fn search_insights_text(
    store: &PgStore,
    ctx: &RequestContext,
    scopes: &[Scope],
    query: &str,
    limit: i64,
) -> Result<Vec<ScoredInsight>> {
    let limit = validate::limit(limit)?;
    if scopes.is_empty() {
        return Ok(Vec::new());
    }
    let scopes = validate::scope_strings(scopes);
    let mut tx = store.tenant_tx(ctx).await?;
    expire(&mut tx, ctx, &scopes).await?;
    // GIN is unusable under RLS, so the DEFINER function finds ids and rank for app.tenant, and
    // the join loads the rows under RLS in the same statement, after the expiry above.
    let sql = format!(
        "SELECT {INSIGHT_COLS}, h.hit_rank::float8 AS score \
         FROM search_insight_ids_text($2, $1, $3) AS h(hit_id, hit_rank) \
         JOIN insights i ON i.id = h.hit_id \
         ORDER BY h.hit_rank DESC, i.id"
    );
    let found = sqlx::query(AssertSqlSafe(sql))
        .bind(query)
        .bind(&scopes)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?;
    let result = scored(&found)?;
    tx.commit().await.map_err(db)?;
    Ok(result)
}

/// `insights` count per scope for `scope_summaries`: active insights. A scope whose insights are
/// all in other states is listed with 0, as pages are.
pub(crate) async fn count_by_scope(conn: &mut PgConnection) -> Result<Vec<(String, i64)>> {
    sqlx::query_as(
        "SELECT scope, count(*) FILTER (WHERE status = 'active') FROM insights GROUP BY scope",
    )
    .fetch_all(conn)
    .await
    .map_err(db)
}

/// Deletes a scope's insights, their sources by cascade, and its reflect declines.
pub(crate) async fn purge_scope(conn: &mut PgConnection, scope: &str) -> Result<u64> {
    let deleted = sqlx::query("DELETE FROM insights WHERE scope = $1")
        .bind(scope)
        .execute(&mut *conn)
        .await
        .map_err(db)?
        .rows_affected();
    sqlx::query("DELETE FROM reflect_declines WHERE scope = $1")
        .bind(scope)
        .execute(conn)
        .await
        .map_err(db)?;
    Ok(deleted)
}
