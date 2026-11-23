//! Insight search for recall: active insights of the request's scopes, fused like facts and returned
//! as their own list. See docs/SPEC.md section 8, Insights.

use memoryos_core::{Insight, Result, ScoredInsight};

use crate::fusion;

pub const DEFAULT_INSIGHT_LIMIT: i64 = 3;
pub const MAX_INSIGHT_LIMIT: i64 = 10;
/// Candidates per search list, vector and text.
pub(crate) const INSIGHT_CANDIDATES_PER_LIST: i64 = 20;

/// Reciprocal rank fusion over insight lists with `k = 60`, highest score first, at most `limit`.
/// Equal scores order by insight id, so the result is deterministic. No reranker reads insights:
/// the SPEC fuses them only, and the facts' reranker call stays exactly as without insights.
pub(crate) fn fuse_insights(lists: &[&[ScoredInsight]], limit: usize) -> Vec<Insight> {
    fusion::fuse_by(lists, |hit| hit.insight.id)
        .into_iter()
        .take(limit)
        .map(|(hit, _)| hit.insight.clone())
        .collect()
}

/// Checks `insight_limit` and returns it as a count.
pub(crate) fn validate_insight_limit(insight_limit: i64) -> Result<usize> {
    fusion::validate_side_limit("insight_limit", insight_limit, MAX_INSIGHT_LIMIT)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use memoryos_core::{InsightLevel, InsightStatus, Scope, Trust};
    use uuid::Uuid;

    use super::*;

    fn hit(id: u128) -> ScoredInsight {
        ScoredInsight {
            insight: Insight {
                id: Uuid::from_u128(id),
                scope: Scope::user("u"),
                level: InsightLevel::Understanding,
                statement: format!("insight {id}"),
                trust: Trust::Agent,
                status: InsightStatus::Active,
                stale_reason: None,
                sources: vec![],
                valid_from: Utc::now(),
                valid_to: None,
                superseded_by: None,
                pending_supersede_of: None,
                written_by: "test".into(),
                recorded_at: Utc::now(),
            },
            score: 0.0,
        }
    }

    fn hits(ids: &[u128]) -> Vec<ScoredInsight> {
        ids.iter().map(|&id| hit(id)).collect()
    }

    fn ids(fused: &[Insight]) -> Vec<u128> {
        fused.iter().map(|insight| insight.id.as_u128()).collect()
    }

    #[test]
    fn an_insight_in_both_lists_ranks_first() {
        let (vector, text) = (hits(&[1, 2]), hits(&[2]));
        assert_eq!(ids(&fuse_insights(&[&vector, &text], 10)), [2, 1]);
    }

    #[test]
    fn repeats_count_once_and_ties_order_by_id() {
        let (vector, text) = (hits(&[9, 9]), hits(&[4]));
        assert_eq!(ids(&fuse_insights(&[&vector, &text], 10)), [4, 9]);
    }

    #[test]
    fn the_limit_cuts_the_fused_list() {
        let vector = hits(&[1, 2, 3, 4]);
        assert_eq!(ids(&fuse_insights(&[&vector], 2)), [1, 2]);
        assert!(fuse_insights(&[&vector], 0).is_empty());
        assert!(fuse_insights(&[], 3).is_empty());
    }

    #[test]
    fn insight_limit_is_0_to_10() {
        assert!(validate_insight_limit(-1).is_err());
        assert!(validate_insight_limit(11).is_err());
        assert_eq!(validate_insight_limit(0).ok(), Some(0));
        assert_eq!(validate_insight_limit(10).ok(), Some(10));
    }
}
