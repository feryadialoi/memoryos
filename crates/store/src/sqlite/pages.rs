//! Knowledge pages on SQLite (docs/SPEC.md section 8). Every statement carries an explicit tenant
//! predicate. Writes run in `BEGIN IMMEDIATE`, which serializes them as the PostgreSQL row lock
//! does. Section vectors are written with their rows; the migration's triggers remove vector and
//! full-text rows whenever a section row goes, including by cascade.

use chrono::Utc;
use memoryos_core::{
    Error, NewPageVersion, Page, PageSectionHit, PageSummary, PageVersion, PageVersionStatus,
    PendingPageVersion, RequestContext, Result, Scope, Trust, PAGE_PENDING_MAX, PAGE_VERSIONS_KEPT,
};
use serde_json::json;
use sqlx::sqlite::SqliteRow;
use sqlx::{AssertSqlSafe, Row, SqliteConnection};
use uuid::Uuid;

use super::facts::audit;
use super::rows::{self, PAGE_COLS, PAGE_SECTION_HIT_COLS, PAGE_SUMMARY_COLS, PAGE_VERSION_COLS};
use super::store_impl::safe_match_query;
use super::{db, embedding, json_array, storable, SqliteStore};
use crate::pages::write_status;
use crate::validate;

struct PageRow {
    id: Uuid,
    current: Option<i64>,
}

fn page_row(row: &SqliteRow) -> Result<PageRow> {
    let id: String = row.try_get("id").map_err(db)?;
    Ok(PageRow {
        id: Uuid::parse_str(&id)
            .map_err(|_| Error::Storage("column id holds an invalid UUID".into()))?,
        current: row.try_get("current_version").map_err(db)?,
    })
}

async fn find_page(
    conn: &mut SqliteConnection,
    ctx: &RequestContext,
    scope: &Scope,
    slug: &str,
) -> Result<Option<PageRow>> {
    let row = sqlx::query(
        "SELECT id, current_version FROM pages WHERE tenant = ?1 AND scope = ?2 AND slug = ?3",
    )
    .bind(&ctx.tenant)
    .bind(scope.to_string())
    .bind(slug)
    .fetch_optional(conn)
    .await
    .map_err(db)?;
    row.as_ref().map(page_row).transpose()
}

async fn find_page_by_id(
    conn: &mut SqliteConnection,
    ctx: &RequestContext,
    page_id: Uuid,
) -> Result<PageRow> {
    let row = sqlx::query("SELECT id, current_version FROM pages WHERE tenant = ?1 AND id = ?2")
        .bind(&ctx.tenant)
        .bind(page_id.to_string())
        .fetch_optional(conn)
        .await
        .map_err(db)?
        .ok_or_else(|| Error::NotFound(format!("page {page_id}")))?;
    page_row(&row)
}

async fn version_trust(
    conn: &mut SqliteConnection,
    ctx: &RequestContext,
    page_id: Uuid,
    version: i64,
) -> Result<Trust> {
    let trust: String = sqlx::query_scalar(
        "SELECT trust FROM page_versions WHERE tenant = ?1 AND page_id = ?2 AND version = ?3",
    )
    .bind(&ctx.tenant)
    .bind(page_id.to_string())
    .bind(version)
    .fetch_one(conn)
    .await
    .map_err(db)?;
    trust
        .parse()
        .map_err(|_| Error::Storage("page version has an invalid trust".into()))
}

async fn load_version(
    conn: &mut SqliteConnection,
    ctx: &RequestContext,
    page_id: Uuid,
    version: i64,
) -> Result<PageVersion> {
    let sql = format!(
        "SELECT {PAGE_VERSION_COLS} FROM page_versions v \
         JOIN pages p ON p.tenant = v.tenant AND p.id = v.page_id \
         WHERE v.tenant = ?1 AND v.page_id = ?2 AND v.version = ?3"
    );
    let row = sqlx::query(AssertSqlSafe(sql))
        .bind(&ctx.tenant)
        .bind(page_id.to_string())
        .bind(version)
        .fetch_optional(conn)
        .await
        .map_err(db)?
        .ok_or_else(|| Error::NotFound(format!("page {page_id} version {version}")))?;
    rows::page_version(&row)
}

async fn load_current(
    conn: &mut SqliteConnection,
    ctx: &RequestContext,
    page_id: Uuid,
) -> Result<Page> {
    let sql = format!(
        "SELECT {PAGE_COLS} FROM pages p \
         JOIN page_versions v ON v.tenant = p.tenant AND v.page_id = p.id \
             AND v.version = p.current_version \
         WHERE p.tenant = ?1 AND p.id = ?2"
    );
    let row = sqlx::query(AssertSqlSafe(sql))
        .bind(&ctx.tenant)
        .bind(page_id.to_string())
        .fetch_optional(conn)
        .await
        .map_err(db)?
        .ok_or_else(|| Error::NotFound(format!("page {page_id}")))?;
    rows::page(&row)
}

async fn delete_sections(
    conn: &mut SqliteConnection,
    ctx: &RequestContext,
    page_id: Uuid,
    version: i64,
) -> Result<()> {
    sqlx::query("DELETE FROM page_sections WHERE tenant = ?1 AND page_id = ?2 AND version = ?3")
        .bind(&ctx.tenant)
        .bind(page_id.to_string())
        .bind(version)
        .execute(conn)
        .await
        .map_err(db)?;
    Ok(())
}

async fn set_version_status(
    conn: &mut SqliteConnection,
    ctx: &RequestContext,
    page_id: Uuid,
    version: i64,
    status: PageVersionStatus,
) -> Result<()> {
    sqlx::query(
        "UPDATE page_versions SET status = ?4 WHERE tenant = ?1 AND page_id = ?2 AND version = ?3",
    )
    .bind(&ctx.tenant)
    .bind(page_id.to_string())
    .bind(version)
    .bind(status.as_str())
    .execute(conn)
    .await
    .map_err(db)?;
    Ok(())
}

/// The previous current version becomes superseded and loses its sections. Runs before the new
/// version turns active, because at most one version per page may be active.
async fn retire_current(
    conn: &mut SqliteConnection,
    ctx: &RequestContext,
    page_id: Uuid,
    current: Option<i64>,
) -> Result<()> {
    let Some(previous) = current else {
        return Ok(());
    };
    set_version_status(
        &mut *conn,
        ctx,
        page_id,
        previous,
        PageVersionStatus::Superseded,
    )
    .await?;
    delete_sections(conn, ctx, page_id, previous).await
}

async fn set_current(
    conn: &mut SqliteConnection,
    ctx: &RequestContext,
    page_id: Uuid,
    version: i64,
) -> Result<()> {
    sqlx::query("UPDATE pages SET current_version = ?3 WHERE tenant = ?1 AND id = ?2")
        .bind(&ctx.tenant)
        .bind(page_id.to_string())
        .bind(version)
        .execute(conn)
        .await
        .map_err(db)?;
    Ok(())
}

/// Deletes superseded and rejected versions past the newest `PAGE_VERSIONS_KEPT` of them. The
/// current version and quarantined versions are never deleted.
async fn prune(conn: &mut SqliteConnection, ctx: &RequestContext, page_id: Uuid) -> Result<()> {
    sqlx::query(
        "DELETE FROM page_versions WHERE tenant = ?1 AND page_id = ?2 \
             AND status IN ('superseded', 'rejected') \
             AND version NOT IN ( \
                 SELECT version FROM page_versions \
                 WHERE tenant = ?1 AND page_id = ?2 AND status IN ('superseded', 'rejected') \
                 ORDER BY version DESC LIMIT ?3)",
    )
    .bind(&ctx.tenant)
    .bind(page_id.to_string())
    .bind(PAGE_VERSIONS_KEPT as i64)
    .execute(conn)
    .await
    .map_err(db)?;
    Ok(())
}

pub(super) async fn list_pages(
    store: &SqliteStore,
    ctx: &RequestContext,
    scope: &Scope,
    prefix: Option<&str>,
    limit: i64,
) -> Result<Vec<PageSummary>> {
    let limit = validate::page_list_limit(limit)?;
    if let Some(prefix) = prefix {
        storable(prefix)?;
    }
    // A literal prefix comparison, not LIKE or GLOB, because `_` and `.` are legal in a slug.
    let sql = format!(
        "SELECT {PAGE_SUMMARY_COLS} FROM pages p \
         JOIN page_versions v ON v.tenant = p.tenant AND v.page_id = p.id \
             AND v.version = p.current_version \
         WHERE p.tenant = ?1 AND p.scope = ?2 \
           AND (?3 IS NULL OR substr(p.slug, 1, length(?3)) = ?3) \
         ORDER BY p.slug LIMIT ?4"
    );
    let mut tx = store.tenant_tx(ctx).await?;
    let found = sqlx::query(AssertSqlSafe(sql))
        .bind(&ctx.tenant)
        .bind(scope.to_string())
        .bind(prefix)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?;
    let result = found
        .iter()
        .map(rows::page_summary)
        .collect::<Result<_>>()?;
    tx.commit().await.map_err(db)?;
    Ok(result)
}

pub(super) async fn get_page(
    store: &SqliteStore,
    ctx: &RequestContext,
    scope: &Scope,
    slug: &str,
    version: Option<i64>,
) -> Result<Page> {
    memoryos_core::validate_slug(slug)?;
    // Without a version, the current one. With one, only versions readers may see.
    let sql = format!(
        "SELECT {PAGE_COLS} FROM pages p \
         JOIN page_versions v ON v.tenant = p.tenant AND v.page_id = p.id \
             AND v.version = COALESCE(?4, p.current_version) \
         WHERE p.tenant = ?1 AND p.scope = ?2 AND p.slug = ?3 \
           AND (?4 IS NULL OR v.status IN ('active', 'superseded'))"
    );
    let mut tx = store.tenant_tx(ctx).await?;
    let row = sqlx::query(AssertSqlSafe(sql))
        .bind(&ctx.tenant)
        .bind(scope.to_string())
        .bind(slug)
        .bind(version)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?
        .ok_or_else(|| Error::NotFound(format!("page {scope}/{slug}")))?;
    let result = rows::page(&row)?;
    tx.commit().await.map_err(db)?;
    Ok(result)
}

pub(super) async fn get_page_version(
    store: &SqliteStore,
    ctx: &RequestContext,
    page_id: Uuid,
    version: i64,
) -> Result<PageVersion> {
    let mut tx = store.tenant_tx(ctx).await?;
    let result = load_version(&mut tx, ctx, page_id, version).await?;
    tx.commit().await.map_err(db)?;
    Ok(result)
}

pub(super) async fn put_page(
    store: &SqliteStore,
    ctx: &RequestContext,
    page: NewPageVersion,
) -> Result<PageVersion> {
    validate::page(&page)?;
    storable(&page.title)?;
    storable(&page.body)?;
    if let Some(summary) = &page.summary {
        storable(summary)?;
    }
    let mut embeddings = Vec::with_capacity(page.sections.len());
    for section in &page.sections {
        storable(&section.heading)?;
        storable(&section.text)?;
        embeddings.push(embedding(&section.embedding)?);
    }
    let scope_text = page.scope.to_string();
    let mut tx = store.write_tx(ctx).await?;
    let now = Utc::now();
    let now_micros = rows::timestamp(now);

    let existing = find_page(&mut tx, ctx, &page.scope, &page.slug).await?;
    let target = match (existing, page.expected_version) {
        (None, None) => {
            let id = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO pages (id, tenant, scope, slug, created_at) VALUES (?2, ?1, ?3, ?4, ?5)",
            )
            .bind(&ctx.tenant)
            .bind(id.to_string())
            .bind(&scope_text)
            .bind(&page.slug)
            .bind(now_micros)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
            PageRow { id, current: None }
        }
        // A page with no current version, because its only versions are pending or rejected, takes
        // another first version, so a rejected first draft never leaves its slug stuck.
        (Some(existing), None) if existing.current.is_none() => existing,
        (Some(existing), None) => {
            return Err(Error::VersionConflict {
                expected: 0,
                current: existing.current.unwrap_or(0),
            })
        }
        (None, Some(_)) => return Err(Error::NotFound(format!("page {scope_text}/{}", page.slug))),
        (Some(existing), Some(expected)) => {
            if existing.current != Some(expected) {
                return Err(Error::VersionConflict {
                    expected,
                    current: existing.current.unwrap_or(0),
                });
            }
            existing
        }
    };

    let current_trust = match target.current {
        Some(current) => Some(version_trust(&mut tx, ctx, target.id, current).await?),
        None => None,
    };
    let status = write_status(&page, current_trust)?;
    if status == PageVersionStatus::Quarantined {
        let pending: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM page_versions \
             WHERE tenant = ?1 AND page_id = ?2 AND status = 'quarantined'",
        )
        .bind(&ctx.tenant)
        .bind(target.id.to_string())
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
        if pending >= PAGE_PENDING_MAX as i64 {
            return Err(Error::InvalidInput(format!(
                "page {scope_text}/{} already has {PAGE_PENDING_MAX} versions waiting for review",
                page.slug
            )));
        }
    }
    let version: i64 = sqlx::query_scalar(
        "SELECT COALESCE(max(version), 0) + 1 FROM page_versions \
         WHERE tenant = ?1 AND page_id = ?2",
    )
    .bind(&ctx.tenant)
    .bind(target.id.to_string())
    .fetch_one(&mut *tx)
    .await
    .map_err(db)?;

    if status == PageVersionStatus::Active {
        retire_current(&mut tx, ctx, target.id, target.current).await?;
    }
    sqlx::query(
        "INSERT INTO page_versions (page_id, tenant, version, base_version, title, body, summary, \
         trust, status, edited_by, edited_at) \
         VALUES (?2, ?1, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )
    .bind(&ctx.tenant)
    .bind(target.id.to_string())
    .bind(version)
    .bind(page.expected_version)
    .bind(&page.title)
    .bind(&page.body)
    .bind(&page.summary)
    .bind(page.trust.as_str())
    .bind(status.as_str())
    .bind(&ctx.actor)
    .bind(now_micros)
    .execute(&mut *tx)
    .await
    .map_err(db)?;
    for (section, embedding) in page.sections.iter().zip(embeddings) {
        let section_id = Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO page_sections (id, tenant, page_id, version, scope, ordinal, heading, \
             text) VALUES (?2, ?1, ?3, ?4, ?5, ?6, ?7, ?8)",
        )
        .bind(&ctx.tenant)
        .bind(&section_id)
        .bind(target.id.to_string())
        .bind(version)
        .bind(&scope_text)
        .bind(section.ordinal)
        .bind(&section.heading)
        .bind(&section.text)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        sqlx::query(
            "INSERT INTO page_section_embeddings (rowid, embedding) \
             SELECT row_id, ?3 FROM page_sections WHERE tenant = ?1 AND id = ?2",
        )
        .bind(&ctx.tenant)
        .bind(&section_id)
        .bind(embedding)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    }
    if status == PageVersionStatus::Active {
        set_current(&mut tx, ctx, target.id, version).await?;
    }
    prune(&mut tx, ctx, target.id).await?;

    let mut detail = json!({
        "scope": scope_text,
        "slug": page.slug,
        "version": version,
        "status": status.as_str(),
    });
    if page.approve_up_to.is_some() {
        detail["approved"] = json!(true);
    }
    audit(&mut tx, ctx, "page.write", Some(target.id), detail, now).await?;
    let result = load_version(&mut tx, ctx, target.id, version).await?;
    tx.commit().await.map_err(db)?;
    Ok(result)
}

pub(super) async fn page_versions(
    store: &SqliteStore,
    ctx: &RequestContext,
    scope: &Scope,
    slug: &str,
) -> Result<Vec<PageVersion>> {
    memoryos_core::validate_slug(slug)?;
    let mut tx = store.tenant_tx(ctx).await?;
    let target = find_page(&mut tx, ctx, scope, slug)
        .await?
        .ok_or_else(|| Error::NotFound(format!("page {scope}/{slug}")))?;
    let sql = format!(
        "SELECT {PAGE_VERSION_COLS} FROM page_versions v \
         JOIN pages p ON p.tenant = v.tenant AND p.id = v.page_id \
         WHERE v.tenant = ?1 AND v.page_id = ?2 ORDER BY v.version DESC"
    );
    let found = sqlx::query(AssertSqlSafe(sql))
        .bind(&ctx.tenant)
        .bind(target.id.to_string())
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?;
    let result = found
        .iter()
        .map(rows::page_version)
        .collect::<Result<_>>()?;
    tx.commit().await.map_err(db)?;
    Ok(result)
}

pub(super) async fn delete_page(
    store: &SqliteStore,
    ctx: &RequestContext,
    scope: &Scope,
    slug: &str,
) -> Result<()> {
    memoryos_core::validate_slug(slug)?;
    let mut tx = store.write_tx(ctx).await?;
    let now = Utc::now();
    let target = find_page(&mut tx, ctx, scope, slug)
        .await?
        .ok_or_else(|| Error::NotFound(format!("page {scope}/{slug}")))?;
    // Versions and sections go by cascade, and the section triggers remove their vector and
    // full-text rows.
    sqlx::query("DELETE FROM pages WHERE tenant = ?1 AND id = ?2")
        .bind(&ctx.tenant)
        .bind(target.id.to_string())
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    audit(
        &mut tx,
        ctx,
        "page.delete",
        Some(target.id),
        json!({ "scope": scope.to_string(), "slug": slug }),
        now,
    )
    .await?;
    tx.commit().await.map_err(db)
}

pub(super) async fn list_quarantined_pages(
    store: &SqliteStore,
    ctx: &RequestContext,
    scopes: &[Scope],
    limit: i64,
) -> Result<Vec<PendingPageVersion>> {
    let limit = validate::limit(limit)?;
    if scopes.is_empty() {
        return Ok(Vec::new());
    }
    let scopes = json_array(&validate::scope_strings(scopes))?;
    let sql = format!(
        "SELECT {PAGE_VERSION_COLS}, v.body, b.body AS base_body \
         FROM page_versions v JOIN pages p ON p.tenant = v.tenant AND p.id = v.page_id \
         LEFT JOIN page_versions b ON b.tenant = v.tenant AND b.page_id = v.page_id \
             AND b.version = v.base_version \
         WHERE v.tenant = ?1 AND v.status = 'quarantined' \
           AND p.scope IN (SELECT value FROM json_each(?2)) \
         ORDER BY v.edited_at, v.page_id, v.version LIMIT ?3"
    );
    let mut tx = store.tenant_tx(ctx).await?;
    let found = sqlx::query(AssertSqlSafe(sql))
        .bind(&ctx.tenant)
        .bind(scopes)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?;
    let result = found
        .iter()
        .map(|row| {
            Ok(PendingPageVersion {
                version: rows::page_version(row)?,
                body: row.try_get("body").map_err(db)?,
                base_body: row.try_get("base_body").map_err(db)?,
            })
        })
        .collect::<Result<_>>()?;
    tx.commit().await.map_err(db)?;
    Ok(result)
}

/// Base of a quarantined version. `InvalidInput` for any other status.
async fn review_state(
    conn: &mut SqliteConnection,
    ctx: &RequestContext,
    page_id: Uuid,
    version: i64,
) -> Result<Option<i64>> {
    let row = sqlx::query(
        "SELECT status, base_version FROM page_versions \
         WHERE tenant = ?1 AND page_id = ?2 AND version = ?3",
    )
    .bind(&ctx.tenant)
    .bind(page_id.to_string())
    .bind(version)
    .fetch_optional(conn)
    .await
    .map_err(db)?
    .ok_or_else(|| Error::NotFound(format!("page {page_id} version {version}")))?;
    let status: String = row.try_get("status").map_err(db)?;
    if status != PageVersionStatus::Quarantined.as_str() {
        return Err(Error::InvalidInput(format!(
            "page {page_id} version {version} is {status}, not quarantined"
        )));
    }
    row.try_get("base_version").map_err(db)
}

pub(super) async fn approve_page_version(
    store: &SqliteStore,
    ctx: &RequestContext,
    page_id: Uuid,
    version: i64,
) -> Result<Page> {
    let mut tx = store.write_tx(ctx).await?;
    let now = Utc::now();
    let target = find_page_by_id(&mut tx, ctx, page_id).await?;
    let base = review_state(&mut tx, ctx, page_id, version).await?;
    if base != target.current {
        return Err(Error::VersionConflict {
            expected: base.unwrap_or(0),
            current: target.current.unwrap_or(0),
        });
    }
    retire_current(&mut tx, ctx, page_id, target.current).await?;
    set_version_status(&mut tx, ctx, page_id, version, PageVersionStatus::Active).await?;
    set_current(&mut tx, ctx, page_id, version).await?;
    prune(&mut tx, ctx, page_id).await?;
    let result = load_current(&mut tx, ctx, page_id).await?;
    audit(
        &mut tx,
        ctx,
        "page.approve",
        Some(page_id),
        json!({
            "scope": result.scope.to_string(),
            "slug": result.slug,
            "version": version,
            "status": PageVersionStatus::Active.as_str(),
        }),
        now,
    )
    .await?;
    tx.commit().await.map_err(db)?;
    Ok(result)
}

pub(super) async fn reject_page_version(
    store: &SqliteStore,
    ctx: &RequestContext,
    page_id: Uuid,
    version: i64,
) -> Result<PageVersion> {
    let mut tx = store.write_tx(ctx).await?;
    let now = Utc::now();
    find_page_by_id(&mut tx, ctx, page_id).await?;
    review_state(&mut tx, ctx, page_id, version).await?;
    set_version_status(&mut tx, ctx, page_id, version, PageVersionStatus::Rejected).await?;
    delete_sections(&mut tx, ctx, page_id, version).await?;
    prune(&mut tx, ctx, page_id).await?;
    let result = load_version(&mut tx, ctx, page_id, version).await?;
    audit(
        &mut tx,
        ctx,
        "page.reject",
        Some(page_id),
        json!({
            "scope": result.scope.to_string(),
            "slug": result.slug,
            "version": version,
            "status": PageVersionStatus::Rejected.as_str(),
        }),
        now,
    )
    .await?;
    tx.commit().await.map_err(db)?;
    Ok(result)
}

pub(super) async fn search_page_sections_vector(
    store: &SqliteStore,
    ctx: &RequestContext,
    scopes: &[Scope],
    embedding_: &[f32],
    limit: i64,
) -> Result<Vec<PageSectionHit>> {
    let limit = validate::limit(limit)?;
    let query = embedding(embedding_)?;
    if scopes.is_empty() {
        return Ok(Vec::new());
    }
    let scopes = json_array(&validate::scope_strings(scopes))?;
    // Only sections of the current version count: those of quarantined versions are stored too.
    let sql = format!(
        "SELECT {PAGE_SECTION_HIT_COLS}, 1.0 - vec_distance_cosine(e.embedding, ?2) AS score \
         FROM page_section_embeddings e \
         JOIN page_sections s ON s.row_id = e.rowid AND s.tenant = ?1 \
         JOIN pages p ON p.tenant = s.tenant AND p.id = s.page_id \
             AND p.current_version = s.version \
         JOIN page_versions v ON v.tenant = s.tenant AND v.page_id = s.page_id \
             AND v.version = s.version \
         WHERE s.scope IN (SELECT value FROM json_each(?3)) \
         ORDER BY vec_distance_cosine(e.embedding, ?2), s.id LIMIT ?4"
    );
    let mut tx = store.tenant_tx(ctx).await?;
    let found = sqlx::query(AssertSqlSafe(sql))
        .bind(&ctx.tenant)
        .bind(query)
        .bind(scopes)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?;
    let result = found
        .iter()
        .map(|row| rows::page_section_hit(row, row.try_get("score").map_err(db)?))
        .collect::<Result<_>>()?;
    tx.commit().await.map_err(db)?;
    Ok(result)
}

pub(super) async fn search_page_sections_text(
    store: &SqliteStore,
    ctx: &RequestContext,
    scopes: &[Scope],
    query: &str,
    limit: i64,
) -> Result<Vec<PageSectionHit>> {
    let limit = validate::limit(limit)?;
    if scopes.is_empty() {
        return Ok(Vec::new());
    }
    storable(query)?;
    let Some(match_query) = safe_match_query(query) else {
        return Ok(Vec::new());
    };
    let scopes = json_array(&validate::scope_strings(scopes))?;
    // As for facts: filters apply before the newest-candidate cap, and the score counts matched
    // terms per section, so another tenant's corpus never affects it as bm25 statistics would.
    let sql = format!(
        "WITH candidates AS MATERIALIZED (\
             SELECT s.row_id AS rid FROM page_sections s \
             JOIN page_sections_fts ON page_sections_fts.rowid = s.row_id \
             JOIN pages p ON p.tenant = s.tenant AND p.id = s.page_id \
                 AND p.current_version = s.version \
             WHERE s.tenant = ?1 AND page_sections_fts MATCH ?2 \
               AND s.scope IN (SELECT value FROM json_each(?3)) \
             ORDER BY s.row_id DESC LIMIT MAX(1000, ?4)),\
         ranked AS (\
             SELECT c.rid, \
                 highlight(page_sections_fts, 0, char(1), char(2)) AS heading_marked, \
                 highlight(page_sections_fts, 1, char(1), char(2)) AS text_marked \
             FROM candidates c JOIN page_sections_fts ON page_sections_fts.rowid = c.rid \
             WHERE page_sections_fts MATCH ?2)\
         SELECT {PAGE_SECTION_HIT_COLS}, r.heading_marked, r.text_marked FROM ranked r \
         JOIN page_sections s ON s.row_id = r.rid AND s.tenant = ?1 \
         JOIN pages p ON p.tenant = s.tenant AND p.id = s.page_id \
         JOIN page_versions v ON v.tenant = s.tenant AND v.page_id = s.page_id \
             AND v.version = s.version"
    );
    let mut tx = store.tenant_tx(ctx).await?;
    let found = sqlx::query(AssertSqlSafe(sql))
        .bind(&ctx.tenant)
        .bind(match_query)
        .bind(scopes)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?;
    let mut result = Vec::with_capacity(found.len());
    for row in &found {
        let heading_marked: String = row.try_get("heading_marked").map_err(db)?;
        let text_marked: String = row.try_get("text_marked").map_err(db)?;
        let mut hit = rows::page_section_hit(row, 0.0)?;
        let inserted =
            heading_marked.matches('\u{1}').count() + text_marked.matches('\u{1}').count();
        let existing = hit.heading.matches('\u{1}').count() + hit.text.matches('\u{1}').count();
        hit.score = inserted.saturating_sub(existing) as f64;
        result.push(hit);
    }
    result.sort_by(|a: &PageSectionHit, b: &PageSectionHit| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.section_id.cmp(&b.section_id))
    });
    result.truncate(limit as usize);
    tx.commit().await.map_err(db)?;
    Ok(result)
}

/// `pages` count per scope for `scope_summaries`: pages with a current version.
pub(super) async fn count_by_scope(
    conn: &mut SqliteConnection,
    ctx: &RequestContext,
) -> Result<Vec<(String, i64)>> {
    sqlx::query_as(
        "SELECT scope, count(*) FROM pages \
         WHERE tenant = ?1 AND current_version IS NOT NULL GROUP BY scope",
    )
    .bind(&ctx.tenant)
    .fetch_all(conn)
    .await
    .map_err(db)
}
