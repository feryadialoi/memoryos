//! Knowledge pages on PostgreSQL (docs/SPEC.md section 8). SQL text is constant; every value is a
//! bind parameter. Writes lock the `pages` row first, so writes, approvals and rejections of one
//! page are serialized.

use memoryos_core::{
    source_requires_quarantine, Error, NewPageVersion, Page, PageSectionHit, PageSummary,
    PageVersion, PageVersionStatus, PendingPageVersion, RequestContext, Result, Scope, Trust,
    PAGE_PENDING_MAX, PAGE_VERSIONS_KEPT,
};
use pgvector::Vector;
use serde_json::json;
use sqlx::postgres::PgRow;
use sqlx::{AssertSqlSafe, PgConnection, Row};
use uuid::Uuid;

use crate::facts::audit;
use crate::rows::{self, PAGE_COLS, PAGE_SECTION_HIT_COLS, PAGE_SUMMARY_COLS, PAGE_VERSION_COLS};
use crate::store_impl::tune_vector_scan;
use crate::{db, validate, PgStore};

/// The status of a new version, from the rules of SPEC section 8, write step 5. Shared with SQLite.
pub(crate) fn write_status(
    page: &NewPageVersion,
    current_trust: Option<Trust>,
) -> Result<PageVersionStatus> {
    if let Some(max_trust) = page.approve_up_to {
        if current_trust.is_some_and(|current| current > max_trust) {
            return Err(Error::Forbidden(format!(
                "the current version of page {} has higher trust than this key may approve",
                page.slug
            )));
        }
        return Ok(PageVersionStatus::Active);
    }
    if source_requires_quarantine(&page.scope, page.trust)
        || current_trust.is_some_and(|current| current > page.trust)
    {
        Ok(PageVersionStatus::Quarantined)
    } else {
        Ok(PageVersionStatus::Active)
    }
}

/// The page row of a write: its id and current version.
struct LockedPage {
    id: Uuid,
    current: Option<i64>,
}

async fn lock_page_by_slug(
    conn: &mut PgConnection,
    scope: &Scope,
    slug: &str,
) -> Result<Option<LockedPage>> {
    let row = sqlx::query(
        "SELECT id, current_version FROM pages WHERE scope = $1 AND slug = $2 FOR UPDATE",
    )
    .bind(scope.to_string())
    .bind(slug)
    .fetch_optional(conn)
    .await
    .map_err(db)?;
    row.map(|row| {
        Ok(LockedPage {
            id: row.try_get("id").map_err(db)?,
            current: row.try_get("current_version").map_err(db)?,
        })
    })
    .transpose()
}

async fn lock_page_by_id(conn: &mut PgConnection, page_id: Uuid) -> Result<LockedPage> {
    let current = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT current_version FROM pages WHERE id = $1 FOR UPDATE",
    )
    .bind(page_id)
    .fetch_optional(conn)
    .await
    .map_err(db)?
    .ok_or_else(|| Error::NotFound(format!("page {page_id}")))?;
    Ok(LockedPage {
        id: page_id,
        current,
    })
}

async fn version_trust(conn: &mut PgConnection, page_id: Uuid, version: i64) -> Result<Trust> {
    let trust: String =
        sqlx::query_scalar("SELECT trust FROM page_versions WHERE page_id = $1 AND version = $2")
            .bind(page_id)
            .bind(version)
            .fetch_one(conn)
            .await
            .map_err(db)?;
    trust
        .parse()
        .map_err(|_| Error::Storage("page version has an invalid trust".into()))
}

async fn load_version(conn: &mut PgConnection, page_id: Uuid, version: i64) -> Result<PageVersion> {
    let sql = format!(
        "SELECT {PAGE_VERSION_COLS} FROM page_versions v JOIN pages p ON p.id = v.page_id \
         WHERE v.page_id = $1 AND v.version = $2"
    );
    let row = sqlx::query(AssertSqlSafe(sql))
        .bind(page_id)
        .bind(version)
        .fetch_optional(conn)
        .await
        .map_err(db)?
        .ok_or_else(|| Error::NotFound(format!("page {page_id} version {version}")))?;
    rows::page_version(&row)
}

async fn load_current(conn: &mut PgConnection, page_id: Uuid) -> Result<Page> {
    let sql = format!(
        "SELECT {PAGE_COLS} FROM pages p \
         JOIN page_versions v ON v.page_id = p.id AND v.version = p.current_version \
         WHERE p.id = $1"
    );
    let row = sqlx::query(AssertSqlSafe(sql))
        .bind(page_id)
        .fetch_optional(conn)
        .await
        .map_err(db)?
        .ok_or_else(|| Error::NotFound(format!("page {page_id}")))?;
    rows::page(&row)
}

/// The previous current version becomes superseded and loses its sections. Runs before the new
/// version turns active, because at most one version per page may be active.
async fn retire_current(
    conn: &mut PgConnection,
    page_id: Uuid,
    current: Option<i64>,
) -> Result<()> {
    let Some(previous) = current else {
        return Ok(());
    };
    sqlx::query(
        "UPDATE page_versions SET status = 'superseded' WHERE page_id = $1 AND version = $2",
    )
    .bind(page_id)
    .bind(previous)
    .execute(&mut *conn)
    .await
    .map_err(db)?;
    sqlx::query("DELETE FROM page_sections WHERE page_id = $1 AND version = $2")
        .bind(page_id)
        .bind(previous)
        .execute(&mut *conn)
        .await
        .map_err(db)?;
    Ok(())
}

async fn set_current(conn: &mut PgConnection, page_id: Uuid, version: i64) -> Result<()> {
    sqlx::query("UPDATE pages SET current_version = $2 WHERE id = $1")
        .bind(page_id)
        .bind(version)
        .execute(conn)
        .await
        .map_err(db)?;
    Ok(())
}

/// Deletes superseded and rejected versions past the newest `PAGE_VERSIONS_KEPT` of them. The
/// current version and quarantined versions are never deleted.
async fn prune(conn: &mut PgConnection, page_id: Uuid) -> Result<()> {
    sqlx::query(
        "DELETE FROM page_versions WHERE page_id = $1 \
             AND status IN ('superseded', 'rejected') \
             AND version NOT IN ( \
                 SELECT version FROM page_versions \
                 WHERE page_id = $1 AND status IN ('superseded', 'rejected') \
                 ORDER BY version DESC LIMIT $2)",
    )
    .bind(page_id)
    .bind(PAGE_VERSIONS_KEPT as i64)
    .execute(conn)
    .await
    .map_err(db)?;
    Ok(())
}

pub(crate) async fn list_pages(
    store: &PgStore,
    ctx: &RequestContext,
    scope: &Scope,
    prefix: Option<&str>,
    limit: i64,
) -> Result<Vec<PageSummary>> {
    let limit = validate::page_list_limit(limit)?;
    // `starts_with` and not `LIKE`, because `_` and `.` are legal in a slug. "C" collation orders
    // by bytes, as SQLite does, whatever the database's default collation is.
    let sql = format!(
        "SELECT {PAGE_SUMMARY_COLS} FROM pages p \
         JOIN page_versions v ON v.page_id = p.id AND v.version = p.current_version \
         WHERE p.scope = $1 AND ($2::text IS NULL OR starts_with(p.slug, $2)) \
         ORDER BY p.slug COLLATE \"C\" LIMIT $3"
    );
    let mut tx = store.tenant_tx(ctx).await?;
    let found = sqlx::query(AssertSqlSafe(sql))
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

pub(crate) async fn get_page(
    store: &PgStore,
    ctx: &RequestContext,
    scope: &Scope,
    slug: &str,
    version: Option<i64>,
) -> Result<Page> {
    memoryos_core::validate_slug(slug)?;
    // Without a version, the current one. With one, only versions readers may see.
    let sql = format!(
        "SELECT {PAGE_COLS} FROM pages p \
         JOIN page_versions v ON v.page_id = p.id AND v.version = COALESCE($3, p.current_version) \
         WHERE p.scope = $1 AND p.slug = $2 \
           AND ($3::bigint IS NULL OR v.status IN ('active', 'superseded'))"
    );
    let mut tx = store.tenant_tx(ctx).await?;
    let row = sqlx::query(AssertSqlSafe(sql))
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

pub(crate) async fn get_page_version(
    store: &PgStore,
    ctx: &RequestContext,
    page_id: Uuid,
    version: i64,
) -> Result<PageVersion> {
    let mut tx = store.tenant_tx(ctx).await?;
    let result = load_version(&mut tx, page_id, version).await?;
    tx.commit().await.map_err(db)?;
    Ok(result)
}

pub(crate) async fn put_page(
    store: &PgStore,
    ctx: &RequestContext,
    page: NewPageVersion,
) -> Result<PageVersion> {
    validate::page(&page)?;
    let embeddings = page
        .sections
        .iter()
        .map(|section| validate::embedding(&section.embedding))
        .collect::<Result<Vec<Vector>>>()?;
    let scope_text = page.scope.to_string();
    let mut tx = store.tenant_tx(ctx).await?;

    let locked = lock_page_by_slug(&mut tx, &page.scope, &page.slug).await?;
    let locked = match (locked, page.expected_version) {
        (None, None) => {
            let id = Uuid::now_v7();
            let created = sqlx::query(
                "INSERT INTO pages (id, tenant, scope, slug) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (tenant, scope, slug) DO NOTHING",
            )
            .bind(id)
            .bind(&ctx.tenant)
            .bind(&scope_text)
            .bind(&page.slug)
            .execute(&mut *tx)
            .await
            .map_err(db)?
            .rows_affected()
                > 0;
            if !created {
                // A concurrent create committed first. This statement's snapshot sees it.
                let current: Option<i64> = sqlx::query_scalar(
                    "SELECT current_version FROM pages WHERE scope = $1 AND slug = $2",
                )
                .bind(&scope_text)
                .bind(&page.slug)
                .fetch_one(&mut *tx)
                .await
                .map_err(db)?;
                return Err(Error::VersionConflict {
                    expected: 0,
                    current: current.unwrap_or(0),
                });
            }
            LockedPage { id, current: None }
        }
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

    let current_trust = match locked.current {
        Some(current) => Some(version_trust(&mut tx, locked.id, current).await?),
        None => None,
    };
    let status = write_status(&page, current_trust)?;
    if status == PageVersionStatus::Quarantined {
        let pending: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM page_versions WHERE page_id = $1 AND status = 'quarantined'",
        )
        .bind(locked.id)
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
        "SELECT COALESCE(max(version), 0) + 1 FROM page_versions WHERE page_id = $1",
    )
    .bind(locked.id)
    .fetch_one(&mut *tx)
    .await
    .map_err(db)?;

    if status == PageVersionStatus::Active {
        retire_current(&mut tx, locked.id, locked.current).await?;
    }
    sqlx::query(
        "INSERT INTO page_versions (page_id, tenant, version, base_version, title, body, summary, \
         trust, status, edited_by) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(locked.id)
    .bind(&ctx.tenant)
    .bind(version)
    .bind(page.expected_version)
    .bind(&page.title)
    .bind(&page.body)
    .bind(&page.summary)
    .bind(page.trust.as_str())
    .bind(status.as_str())
    .bind(&ctx.actor)
    .execute(&mut *tx)
    .await
    .map_err(db)?;
    for (section, embedding) in page.sections.iter().zip(embeddings) {
        sqlx::query(
            "INSERT INTO page_sections (id, tenant, page_id, version, scope, ordinal, heading, \
             text, embedding) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(Uuid::now_v7())
        .bind(&ctx.tenant)
        .bind(locked.id)
        .bind(version)
        .bind(&scope_text)
        .bind(section.ordinal)
        .bind(&section.heading)
        .bind(&section.text)
        .bind(embedding)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    }
    if status == PageVersionStatus::Active {
        set_current(&mut tx, locked.id, version).await?;
    }
    prune(&mut tx, locked.id).await?;

    let mut detail = json!({
        "scope": scope_text,
        "slug": page.slug,
        "version": version,
        "status": status.as_str(),
    });
    if page.approve_up_to.is_some() {
        detail["approved"] = json!(true);
    }
    audit(&mut tx, ctx, "page.write", Some(locked.id), detail).await?;
    let result = load_version(&mut tx, locked.id, version).await?;
    tx.commit().await.map_err(db)?;
    Ok(result)
}

pub(crate) async fn page_versions(
    store: &PgStore,
    ctx: &RequestContext,
    scope: &Scope,
    slug: &str,
) -> Result<Vec<PageVersion>> {
    memoryos_core::validate_slug(slug)?;
    let mut tx = store.tenant_tx(ctx).await?;
    let page_id: Uuid = sqlx::query_scalar("SELECT id FROM pages WHERE scope = $1 AND slug = $2")
        .bind(scope.to_string())
        .bind(slug)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?
        .ok_or_else(|| Error::NotFound(format!("page {scope}/{slug}")))?;
    let sql = format!(
        "SELECT {PAGE_VERSION_COLS} FROM page_versions v JOIN pages p ON p.id = v.page_id \
         WHERE v.page_id = $1 ORDER BY v.version DESC"
    );
    let found = sqlx::query(AssertSqlSafe(sql))
        .bind(page_id)
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

pub(crate) async fn delete_page(
    store: &PgStore,
    ctx: &RequestContext,
    scope: &Scope,
    slug: &str,
) -> Result<()> {
    memoryos_core::validate_slug(slug)?;
    let mut tx = store.tenant_tx(ctx).await?;
    // Versions and sections go by cascade.
    let page_id: Uuid =
        sqlx::query_scalar("DELETE FROM pages WHERE scope = $1 AND slug = $2 RETURNING id")
            .bind(scope.to_string())
            .bind(slug)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?
            .ok_or_else(|| Error::NotFound(format!("page {scope}/{slug}")))?;
    audit(
        &mut tx,
        ctx,
        "page.delete",
        Some(page_id),
        json!({ "scope": scope.to_string(), "slug": slug }),
    )
    .await?;
    tx.commit().await.map_err(db)
}

pub(crate) async fn list_quarantined_pages(
    store: &PgStore,
    ctx: &RequestContext,
    scopes: &[Scope],
    limit: i64,
) -> Result<Vec<PendingPageVersion>> {
    let limit = validate::limit(limit)?;
    if scopes.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT {PAGE_VERSION_COLS}, v.body, b.body AS base_body \
         FROM page_versions v JOIN pages p ON p.id = v.page_id \
         LEFT JOIN page_versions b ON b.page_id = v.page_id AND b.version = v.base_version \
         WHERE v.status = 'quarantined' AND p.scope = ANY($1) \
         ORDER BY v.edited_at, v.page_id, v.version LIMIT $2"
    );
    let mut tx = store.tenant_tx(ctx).await?;
    let found = sqlx::query(AssertSqlSafe(sql))
        .bind(validate::scope_strings(scopes))
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

/// Status and base of a version, after its page row is locked.
async fn review_state(conn: &mut PgConnection, page_id: Uuid, version: i64) -> Result<Option<i64>> {
    let row = sqlx::query(
        "SELECT status, base_version FROM page_versions WHERE page_id = $1 AND version = $2",
    )
    .bind(page_id)
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

pub(crate) async fn approve_page_version(
    store: &PgStore,
    ctx: &RequestContext,
    page_id: Uuid,
    version: i64,
) -> Result<Page> {
    let mut tx = store.tenant_tx(ctx).await?;
    let locked = lock_page_by_id(&mut tx, page_id).await?;
    let base = review_state(&mut tx, page_id, version).await?;
    if base != locked.current {
        return Err(Error::VersionConflict {
            expected: base.unwrap_or(0),
            current: locked.current.unwrap_or(0),
        });
    }
    retire_current(&mut tx, page_id, locked.current).await?;
    sqlx::query("UPDATE page_versions SET status = 'active' WHERE page_id = $1 AND version = $2")
        .bind(page_id)
        .bind(version)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    set_current(&mut tx, page_id, version).await?;
    prune(&mut tx, page_id).await?;
    let result = load_current(&mut tx, page_id).await?;
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
    )
    .await?;
    tx.commit().await.map_err(db)?;
    Ok(result)
}

pub(crate) async fn reject_page_version(
    store: &PgStore,
    ctx: &RequestContext,
    page_id: Uuid,
    version: i64,
) -> Result<PageVersion> {
    let mut tx = store.tenant_tx(ctx).await?;
    lock_page_by_id(&mut tx, page_id).await?;
    review_state(&mut tx, page_id, version).await?;
    sqlx::query("UPDATE page_versions SET status = 'rejected' WHERE page_id = $1 AND version = $2")
        .bind(page_id)
        .bind(version)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query("DELETE FROM page_sections WHERE page_id = $1 AND version = $2")
        .bind(page_id)
        .bind(version)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    prune(&mut tx, page_id).await?;
    let result = load_version(&mut tx, page_id, version).await?;
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
    )
    .await?;
    tx.commit().await.map_err(db)?;
    Ok(result)
}

pub(crate) async fn search_page_sections_vector(
    store: &PgStore,
    ctx: &RequestContext,
    scopes: &[Scope],
    embedding: &[f32],
    limit: i64,
) -> Result<Vec<PageSectionHit>> {
    let limit = validate::limit(limit)?;
    let query: Vector = validate::embedding(embedding)?;
    if scopes.is_empty() {
        return Ok(Vec::new());
    }
    let mut tx = store.tenant_tx(ctx).await?;
    tune_vector_scan(&mut tx, limit).await?;
    // Only sections of the current version count: those of quarantined versions are stored too.
    // The re-sort of the materialized result follows search_facts_vector.
    let sql = format!(
        "WITH r AS MATERIALIZED ( \
             SELECT s.id AS rid, s.embedding <=> $1 AS distance FROM page_sections s \
             WHERE s.scope = ANY($2) AND EXISTS ( \
                 SELECT 1 FROM pages p WHERE p.id = s.page_id AND p.current_version = s.version) \
             ORDER BY distance LIMIT $3) \
         SELECT {PAGE_SECTION_HIT_COLS}, 1 - r.distance AS score \
         FROM r JOIN page_sections s ON s.id = r.rid \
         JOIN pages p ON p.id = s.page_id \
         JOIN page_versions v ON v.page_id = s.page_id AND v.version = s.version \
         ORDER BY r.distance + 0, s.id"
    );
    let found = sqlx::query(AssertSqlSafe(sql))
        .bind(query)
        .bind(validate::scope_strings(scopes))
        .bind(limit)
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?;
    let result = section_hits(&found)?;
    tx.commit().await.map_err(db)?;
    Ok(result)
}

pub(crate) async fn search_page_sections_text(
    store: &PgStore,
    ctx: &RequestContext,
    scopes: &[Scope],
    query: &str,
    limit: i64,
) -> Result<Vec<PageSectionHit>> {
    let limit = validate::limit(limit)?;
    if scopes.is_empty() {
        return Ok(Vec::new());
    }
    // GIN is unusable under RLS, so the DEFINER function finds ids and rank for app.tenant, and
    // the join loads the rows under RLS in the same statement.
    let sql = format!(
        "SELECT {PAGE_SECTION_HIT_COLS}, h.hit_rank::float8 AS score \
         FROM search_page_section_ids_text($2, $1, $3) AS h(hit_id, hit_rank) \
         JOIN page_sections s ON s.id = h.hit_id \
         JOIN pages p ON p.id = s.page_id AND p.current_version = s.version \
         JOIN page_versions v ON v.page_id = s.page_id AND v.version = s.version \
         ORDER BY h.hit_rank DESC, s.id"
    );
    let mut tx = store.tenant_tx(ctx).await?;
    let found = sqlx::query(AssertSqlSafe(sql))
        .bind(query)
        .bind(validate::scope_strings(scopes))
        .bind(limit)
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?;
    let result = section_hits(&found)?;
    tx.commit().await.map_err(db)?;
    Ok(result)
}

fn section_hits(found: &[PgRow]) -> Result<Vec<PageSectionHit>> {
    found.iter().map(rows::page_section_hit).collect()
}

/// `pages` count per scope for `scope_summaries`: pages with a current version.
pub(crate) async fn count_by_scope(conn: &mut PgConnection) -> Result<Vec<(String, i64)>> {
    sqlx::query_as(
        "SELECT scope, count(*) FROM pages WHERE current_version IS NOT NULL GROUP BY scope",
    )
    .fetch_all(conn)
    .await
    .map_err(db)
}
