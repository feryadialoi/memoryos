//! Reflect batch and topic declines (docs/SPEC.md section 8, Insights, "Reflect batch"). The
//! input checks, the fact set digest and the grouping of facts into topics are shared with SQLite;
//! the queries below are PostgreSQL's. SQL text is constant; every value is a bind parameter.

use std::collections::{BTreeMap, HashMap};

use memoryos_core::{
    Error, Fact, Insight, InsightSourceRef, ReflectBatch, ReflectTopic, RequestContext, Result,
    Scope, StaleSource, INSIGHT_SOURCES_MIN, REFLECT_BATCH_MAX, REFLECT_TOPIC_FACTS_MAX,
};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgRow;
use sqlx::{AssertSqlSafe, PgConnection, Row};
use uuid::Uuid;

use crate::rows::{self, FACT_COLS};
use crate::{db, PgStore};

/// Most active insights shown with one topic, newest first, so a subject with many insights
/// cannot crowd the batch.
pub(crate) const TOPIC_INSIGHTS_MAX: i64 = 10;
/// Subjects whose facts are loaded per round trip while topics are collected. Declined topics are
/// skipped, so a batch may need more than one round.
pub(crate) const SUBJECT_CHUNK: usize = REFLECT_BATCH_MAX as usize;
/// Upper bound for `min_facts`, as for any limit argument.
const MIN_FACTS_MAX: i64 = 1000;

/// `limit` must be 1 to `REFLECT_BATCH_MAX`. The API applies `REFLECT_BATCH_DEFAULT` when the
/// caller gives none.
pub(crate) fn batch_limit(limit: i64) -> Result<usize> {
    if (1..=REFLECT_BATCH_MAX).contains(&limit) {
        Ok(limit as usize)
    } else {
        Err(Error::InvalidInput(format!(
            "limit must be in 1..={REFLECT_BATCH_MAX}, got {limit}"
        )))
    }
}

/// A topic is only worth offering when its facts can be cited by one insight, which needs at
/// least `INSIGHT_SOURCES_MIN` sources.
pub(crate) fn min_facts(value: i64) -> Result<i64> {
    let min = INSIGHT_SOURCES_MIN as i64;
    if (min..=MIN_FACTS_MAX).contains(&value) {
        Ok(value)
    } else {
        Err(Error::InvalidInput(format!(
            "min_facts must be in {min}..={MIN_FACTS_MAX}, got {value}"
        )))
    }
}

/// A `fact_set` as `fact_set_digest` makes it: 64 lowercase hex characters.
pub(crate) fn fact_set(value: &str) -> Result<()> {
    let valid = value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if valid {
        Ok(())
    } else {
        Err(Error::InvalidInput(
            "fact_set must be lowercase SHA-256 hex".into(),
        ))
    }
}

/// Hex SHA-256 of the sorted fact ids, each in canonical lowercase hyphenated form and followed
/// by a newline. The same on both backends, so a decline means the same thing on either.
pub(crate) fn fact_set_digest(ids: impl IntoIterator<Item = Uuid>) -> String {
    let mut ids: Vec<Uuid> = ids.into_iter().collect();
    ids.sort_unstable();
    let mut hasher = Sha256::new();
    for id in ids {
        hasher.update(id.hyphenated().to_string().as_bytes());
        hasher.update(b"\n");
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Builds the topics of one chunk of subjects, in the chunk's order, skipping subjects with no
/// facts and topics whose current fact set was declined. `facts` holds at most
/// `REFLECT_TOPIC_FACTS_MAX` facts per subject, each subject's newest first. The topics' insights
/// are left empty for the caller to fill.
pub(crate) fn chunk_topics(
    chunk: &[Uuid],
    facts: Vec<Fact>,
    names: &HashMap<Uuid, String>,
    declines: &HashMap<Uuid, String>,
    room: usize,
) -> Result<Vec<ReflectTopic>> {
    let mut by_subject: BTreeMap<Uuid, Vec<Fact>> = BTreeMap::new();
    for fact in facts {
        by_subject.entry(fact.subject_id).or_default().push(fact);
    }
    let mut topics = Vec::new();
    for subject_id in chunk {
        if topics.len() == room {
            break;
        }
        let Some(facts) = by_subject.remove(subject_id) else {
            continue;
        };
        debug_assert!(facts.len() <= REFLECT_TOPIC_FACTS_MAX);
        let fact_set = fact_set_digest(facts.iter().map(|fact| fact.id));
        if declines.get(subject_id) == Some(&fact_set) {
            continue;
        }
        let subject = names
            .get(subject_id)
            .ok_or_else(|| Error::Storage(format!("fact subject {subject_id} has no entity")))?
            .clone();
        topics.push(ReflectTopic {
            topic_key: *subject_id,
            subject,
            facts,
            fact_set,
            insights: Vec::new(),
        });
    }
    Ok(topics)
}

/// SQL condition, for `facts f`, that no active, stale or quarantined insight cites the fact.
const UNCITED: &str = "NOT EXISTS (SELECT 1 FROM insight_sources s \
    JOIN insights i ON i.id = s.insight_id \
    WHERE s.source_kind = 'fact' AND s.source_id = f.id \
      AND i.status IN ('active', 'stale', 'quarantined'))";

/// Insight columns for `insights i`, with its sources ordered as `InsightSourceRef` orders them.
const INSIGHT_COLS: &str = "i.id, i.scope, i.level, i.statement, i.trust, i.status, \
    i.stale_reason, i.valid_from, i.valid_to, i.superseded_by, i.pending_supersede_of, \
    i.written_by, i.recorded_at, \
    (SELECT COALESCE(json_agg(json_build_object('kind', s.source_kind, 'id', s.source_id) \
        ORDER BY s.source_kind, s.source_id), '[]'::json) \
     FROM insight_sources s WHERE s.insight_id = i.id) AS sources";

fn insight(row: &PgRow) -> Result<Insight> {
    fn parsed<T: std::str::FromStr>(row: &PgRow, column: &str) -> Result<T> {
        let text: String = row.try_get(column).map_err(db)?;
        text.parse()
            .map_err(|_| Error::Storage(format!("column {column} holds an invalid value")))
    }
    let sources: serde_json::Value = row.try_get("sources").map_err(db)?;
    let sources: Vec<InsightSourceRef> = serde_json::from_value(sources)
        .map_err(|e| Error::Storage(format!("insight sources: {e}")))?;
    let stale_reason: Option<serde_json::Value> = row.try_get("stale_reason").map_err(db)?;
    let stale_reason = stale_reason
        .map(serde_json::from_value::<Vec<StaleSource>>)
        .transpose()
        .map_err(|e| Error::Storage(format!("insight stale_reason: {e}")))?;
    Ok(Insight {
        id: row.try_get("id").map_err(db)?,
        scope: parsed(row, "scope")?,
        level: parsed(row, "level")?,
        statement: row.try_get("statement").map_err(db)?,
        trust: parsed(row, "trust")?,
        status: parsed(row, "status")?,
        stale_reason,
        sources,
        valid_from: row.try_get("valid_from").map_err(db)?,
        valid_to: row.try_get("valid_to").map_err(db)?,
        superseded_by: row.try_get("superseded_by").map_err(db)?,
        pending_supersede_of: row.try_get("pending_supersede_of").map_err(db)?,
        written_by: row.try_get("written_by").map_err(db)?,
        recorded_at: row.try_get("recorded_at").map_err(db)?,
    })
}

pub(crate) async fn reflect_batch(
    store: &PgStore,
    ctx: &RequestContext,
    scope: &Scope,
    limit: i64,
    min_facts: i64,
) -> Result<ReflectBatch> {
    let limit = batch_limit(limit)?;
    let min_facts = self::min_facts(min_facts)?;
    let mut tx = store.tenant_tx(ctx).await?;
    let topics = topics(&mut tx, scope, limit, min_facts).await?;
    tx.commit().await.map_err(db)?;
    Ok(ReflectBatch {
        stale: Vec::new(),
        topics,
    })
}

/// Up to `room` topics of `scope`: subjects ordered by their oldest uncited current fact, so the
/// longest waiting topic comes first, then by subject id.
async fn topics(
    conn: &mut PgConnection,
    scope: &Scope,
    room: usize,
    min_facts: i64,
) -> Result<Vec<ReflectTopic>> {
    if room == 0 {
        return Ok(Vec::new());
    }
    let current = rows::visible_now();
    let sql = format!(
        "SELECT f.subject_id FROM facts f \
         WHERE f.scope = $1 AND {current} AND {UNCITED} \
         GROUP BY f.subject_id HAVING count(*) >= $2 \
         ORDER BY min(f.recorded_at), f.subject_id"
    );
    let subjects: Vec<Uuid> = sqlx::query_scalar(AssertSqlSafe(sql))
        .bind(scope.to_string())
        .bind(min_facts)
        .fetch_all(&mut *conn)
        .await
        .map_err(db)?;

    // The count is checked again with the facts: each statement reads its own snapshot.
    let facts_sql = format!(
        "SELECT * FROM ( \
             SELECT {FACT_COLS}, \
                 row_number() OVER (PARTITION BY f.subject_id \
                     ORDER BY f.recorded_at DESC, f.id DESC) AS rn, \
                 count(*) OVER (PARTITION BY f.subject_id) AS n \
             FROM facts f \
             WHERE f.scope = $1 AND f.subject_id = ANY($2) AND {current} AND {UNCITED} \
         ) x WHERE x.rn <= $3 AND x.n >= $4 \
         ORDER BY x.subject_id, x.rn"
    );
    let mut topics = Vec::new();
    for chunk in subjects.chunks(SUBJECT_CHUNK) {
        if topics.len() == room {
            break;
        }
        let found = sqlx::query(AssertSqlSafe(facts_sql.as_str()))
            .bind(scope.to_string())
            .bind(chunk)
            .bind(REFLECT_TOPIC_FACTS_MAX as i64)
            .bind(min_facts)
            .fetch_all(&mut *conn)
            .await
            .map_err(db)?;
        let facts = found.iter().map(rows::fact).collect::<Result<Vec<_>>>()?;
        let names: HashMap<Uuid, String> =
            sqlx::query_as::<_, (Uuid, String)>("SELECT id, name FROM entities WHERE id = ANY($1)")
                .bind(chunk)
                .fetch_all(&mut *conn)
                .await
                .map_err(db)?
                .into_iter()
                .collect();
        let declines: HashMap<Uuid, String> = sqlx::query_as::<_, (Uuid, String)>(
            "SELECT topic_key, fact_set FROM reflect_declines \
             WHERE scope = $1 AND topic_key = ANY($2)",
        )
        .bind(scope.to_string())
        .bind(chunk)
        .fetch_all(&mut *conn)
        .await
        .map_err(db)?
        .into_iter()
        .collect();
        topics.extend(chunk_topics(
            chunk,
            facts,
            &names,
            &declines,
            room - topics.len(),
        )?);
    }
    attach_insights(conn, scope, &mut topics).await?;
    Ok(topics)
}

/// Fills each topic's active insights of `scope` that cite any fact of its subject, newest first,
/// at most `TOPIC_INSIGHTS_MAX` each.
async fn attach_insights(
    conn: &mut PgConnection,
    scope: &Scope,
    topics: &mut [ReflectTopic],
) -> Result<()> {
    if topics.is_empty() {
        return Ok(());
    }
    let subjects: Vec<Uuid> = topics.iter().map(|topic| topic.topic_key).collect();
    let sql = format!(
        "SELECT t.subject_id AS topic_subject, x.* \
         FROM unnest($2::uuid[]) AS t(subject_id) \
         CROSS JOIN LATERAL ( \
             SELECT {INSIGHT_COLS} FROM insights i \
             WHERE i.scope = $1 AND i.status = 'active' AND EXISTS ( \
                 SELECT 1 FROM insight_sources s JOIN facts f ON f.id = s.source_id \
                 WHERE s.insight_id = i.id AND s.source_kind = 'fact' \
                   AND f.subject_id = t.subject_id) \
             ORDER BY i.recorded_at DESC, i.id DESC LIMIT $3 \
         ) x"
    );
    let found = sqlx::query(AssertSqlSafe(sql))
        .bind(scope.to_string())
        .bind(&subjects)
        .bind(TOPIC_INSIGHTS_MAX)
        .fetch_all(&mut *conn)
        .await
        .map_err(db)?;
    let mut by_subject: HashMap<Uuid, Vec<Insight>> = HashMap::new();
    for row in &found {
        let subject: Uuid = row.try_get("topic_subject").map_err(db)?;
        by_subject.entry(subject).or_default().push(insight(row)?);
    }
    for topic in topics {
        topic.insights = by_subject.remove(&topic.topic_key).unwrap_or_default();
    }
    Ok(())
}

pub(crate) async fn decline_topic(
    store: &PgStore,
    ctx: &RequestContext,
    scope: &Scope,
    topic_key: Uuid,
    fact_set: &str,
) -> Result<()> {
    self::fact_set(fact_set)?;
    let mut tx = store.tenant_tx(ctx).await?;
    sqlx::query(
        "INSERT INTO reflect_declines (tenant, scope, topic_key, fact_set) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (tenant, scope, topic_key) \
         DO UPDATE SET fact_set = EXCLUDED.fact_set, declined_at = now()",
    )
    .bind(&ctx.tenant)
    .bind(scope.to_string())
    .bind(topic_key)
    .bind(fact_set)
    .execute(&mut *tx)
    .await
    .map_err(db)?;
    tx.commit().await.map_err(db)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fact_set_ignores_order_and_is_lowercase_hex() {
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let digest = fact_set_digest([b, a]);
        assert_eq!(digest, fact_set_digest([a, b]));
        assert!(fact_set(&digest).is_ok());
        assert_ne!(digest, fact_set_digest([a]));
        // Fixed input format: sorted canonical ids, each followed by a newline.
        let expected: String = Sha256::digest(format!("{a}\n{b}\n").as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(digest, expected);
    }

    #[test]
    fn inputs_are_bounded() {
        assert!(batch_limit(0).is_err());
        assert!(batch_limit(REFLECT_BATCH_MAX + 1).is_err());
        assert_eq!(batch_limit(REFLECT_BATCH_MAX).ok(), Some(20));
        assert!(min_facts(1).is_err());
        assert_eq!(min_facts(2).ok(), Some(2));
        assert!(fact_set(&"A".repeat(64)).is_err());
        assert!(fact_set(&"a".repeat(63)).is_err());
    }
}
