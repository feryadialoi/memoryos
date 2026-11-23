//! Insight rules shared by both backends (docs/SPEC.md section 8, Insights): input checks that run
//! before any SQL, the source checks of a write, what a changed fact source reads as, and how a
//! stale reason grows when another source changes.

use std::collections::{BTreeMap, BTreeSet};

use memoryos_core::{
    Error, FactOp, InsightLevel, InsightSourceKind, InsightSourceRef, InsightStatus, Result, Scope,
    SourceChange, StaleSource, Trust, INSIGHT_SOURCES_MAX, INSIGHT_SOURCES_MIN,
    INSIGHT_STATEMENT_MAX,
};
use uuid::Uuid;

/// Upper bound for `list_insights`.
const MAX_INSIGHT_LIST: i64 = 500;

pub(crate) fn list_limit(value: i64) -> Result<i64> {
    if (1..=MAX_INSIGHT_LIST).contains(&value) {
        Ok(value)
    } else {
        Err(Error::InvalidInput(format!(
            "limit must be in 1..={MAX_INSIGHT_LIST}, got {value}"
        )))
    }
}

/// 1 to `INSIGHT_STATEMENT_MAX` characters, counted as `char_length` and SQLite `length` count
/// them, so this agrees with the CHECK constraints. The caller has already masked secrets.
pub(crate) fn statement(value: &str) -> Result<()> {
    crate::validate::non_empty("statement", value)?;
    let len = value.chars().count();
    if len > INSIGHT_STATEMENT_MAX {
        return Err(Error::InvalidInput(format!(
            "statement has {len} characters, at most {INSIGHT_STATEMENT_MAX} allowed"
        )));
    }
    Ok(())
}

/// The distinct sources, sorted, when there are `INSIGHT_SOURCES_MIN` to `INSIGHT_SOURCES_MAX` of
/// them. A source named twice counts once.
pub(crate) fn sources(sources: &[InsightSourceRef]) -> Result<Vec<InsightSourceRef>> {
    let distinct: BTreeSet<InsightSourceRef> = sources.iter().copied().collect();
    if !(INSIGHT_SOURCES_MIN..=INSIGHT_SOURCES_MAX).contains(&distinct.len()) {
        return Err(Error::InvalidInput(format!(
            "an insight cites {INSIGHT_SOURCES_MIN} to {INSIGHT_SOURCES_MAX} distinct sources, got {}",
            distinct.len()
        )));
    }
    Ok(distinct.into_iter().collect())
}

pub(crate) fn ids_of(sources: &[InsightSourceRef], kind: InsightSourceKind) -> Vec<Uuid> {
    sources
        .iter()
        .filter(|source| source.kind == kind)
        .map(|source| source.id)
        .collect()
}

/// A cited fact as a write sees it.
pub(crate) struct FoundFact {
    pub(crate) id: Uuid,
    pub(crate) scope: String,
    pub(crate) trust: Trust,
    /// Active and visible now (SPEC section 4).
    pub(crate) current: bool,
}

/// A cited insight as a write sees it.
pub(crate) struct FoundInsight {
    pub(crate) id: Uuid,
    pub(crate) scope: String,
    pub(crate) level: InsightLevel,
    pub(crate) trust: Trust,
    pub(crate) status: InsightStatus,
}

/// Checks every source of a write or confirm and returns the lowest source trust. Another tenant's
/// source is simply not found, so the message never says whether it exists.
pub(crate) fn check_sources(
    scope: &Scope,
    level: InsightLevel,
    sources: &[InsightSourceRef],
    facts: &[FoundFact],
    insights: &[FoundInsight],
) -> Result<Trust> {
    let scope = scope.to_string();
    let facts: BTreeMap<Uuid, &FoundFact> = facts.iter().map(|f| (f.id, f)).collect();
    let insights: BTreeMap<Uuid, &FoundInsight> = insights.iter().map(|i| (i.id, i)).collect();
    let mut lowest = Trust::System;
    for source in sources {
        let (found_scope, trust) = match source.kind {
            InsightSourceKind::Fact => {
                let fact = facts.get(&source.id).ok_or_else(|| {
                    Error::InvalidInput(format!("source fact {} was not found", source.id))
                })?;
                if fact.scope == scope && !fact.current {
                    return Err(Error::InvalidInput(format!(
                        "source fact {} is not current",
                        source.id
                    )));
                }
                (&fact.scope, fact.trust)
            }
            InsightSourceKind::Insight => {
                let insight = insights.get(&source.id).ok_or_else(|| {
                    Error::InvalidInput(format!("source insight {} was not found", source.id))
                })?;
                if insight.scope == scope {
                    if insight.status != InsightStatus::Active {
                        return Err(Error::InvalidInput(format!(
                            "source insight {} is {}, not active",
                            source.id,
                            insight.status.as_str()
                        )));
                    }
                    if !level.may_cite(insight.level) {
                        return Err(Error::InvalidInput(format!(
                            "a {} insight cannot cite the {} insight {}",
                            level.as_str(),
                            insight.level.as_str(),
                            source.id
                        )));
                    }
                }
                (&insight.scope, insight.trust)
            }
        };
        if *found_scope != scope {
            return Err(Error::InvalidInput(format!(
                "source {} {} is outside the insight's scope",
                source.kind.as_str(),
                source.id
            )));
        }
        lowest = lowest.min(trust);
    }
    Ok(lowest)
}

/// What decides whether a cited fact changed. `None` in `fact_change` means the row is gone.
pub(crate) struct FactState {
    pub(crate) invalidated: bool,
    pub(crate) superseded_by: Option<Uuid>,
    pub(crate) active: bool,
    pub(crate) trust: Trust,
    pub(crate) expired: bool,
}

/// How a cited fact stands now, for an insight of `insight_trust`, or `None` while it is unchanged.
/// Only a purge sends an active fact back to quarantine or lowers its trust below an insight that
/// cites it (insight trust is at most every source's at write), so both read as `purged`.
pub(crate) fn fact_change(state: Option<&FactState>, insight_trust: Trust) -> Option<SourceChange> {
    let Some(state) = state else {
        return Some(SourceChange::Purged);
    };
    if state.invalidated {
        Some(SourceChange::Invalidated)
    } else if let Some(by) = state.superseded_by {
        Some(SourceChange::Superseded { by })
    } else if !state.active || state.trust < insight_trust {
        Some(SourceChange::Purged)
    } else if state.expired {
        Some(SourceChange::Expired)
    } else {
        None
    }
}

/// How a cited insight stands now, or `None` while it is active.
pub(crate) fn insight_change(status: InsightStatus) -> Option<SourceChange> {
    (status != InsightStatus::Active).then_some(SourceChange::InsightChanged { status })
}

pub(crate) fn stale(kind: InsightSourceKind, id: Uuid, change: SourceChange) -> StaleSource {
    StaleSource {
        source: InsightSourceRef { kind, id },
        change,
    }
}

/// One entry per source: a source that changed again replaces its entry, a new one is added.
/// Returns the merged reason and whether it differs from `existing`.
pub(crate) fn merge_reason(
    existing: Option<Vec<StaleSource>>,
    changes: &[StaleSource],
) -> (Vec<StaleSource>, bool) {
    let mut reason = existing.unwrap_or_default();
    let mut changed = false;
    for change in changes {
        match reason
            .iter_mut()
            .find(|entry| entry.source == change.source)
        {
            Some(entry) if entry.change == change.change => {}
            Some(entry) => {
                entry.change = change.change.clone();
                changed = true;
            }
            None => {
                reason.push(change.clone());
                changed = true;
            }
        }
    }
    (reason, changed)
}

pub(crate) fn encode_reason(reason: &[StaleSource]) -> Result<serde_json::Value> {
    serde_json::to_value(reason).map_err(|e| Error::Internal(e.to_string()))
}

/// Facts an apply names, whose state the commit may change. The facts these close are found
/// after the apply, from `superseded_by`.
pub(crate) fn op_fact_ids(ops: &[FactOp]) -> Vec<Uuid> {
    ops.iter()
        .flat_map(|op| match op {
            FactOp::Add(_) => Vec::new(),
            FactOp::Supersede { old_fact_id, .. } => vec![*old_fact_id],
            FactOp::Corroborate { fact_id } => vec![*fact_id],
            FactOp::SupersedeWithExisting {
                old_fact_id,
                fact_id,
            } => vec![*old_fact_id, *fact_id],
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(kind_id: u128) -> InsightSourceRef {
        InsightSourceRef {
            kind: InsightSourceKind::Fact,
            id: Uuid::from_u128(kind_id),
        }
    }

    #[test]
    fn sources_count_distinct_and_sort() {
        assert!(sources(&[fact(1)]).is_err());
        assert!(sources(&[fact(1), fact(1)]).is_err());
        assert_eq!(
            sources(&[fact(2), fact(1), fact(2)]).expect("valid"),
            vec![fact(1), fact(2)]
        );
        let many: Vec<_> = (0..51).map(fact).collect();
        assert!(sources(&many).is_err());
        assert!(sources(&many[..50]).is_ok());
    }

    #[test]
    fn statements_are_counted_in_characters() {
        assert!(statement("  ").is_err());
        assert!(statement(&"é".repeat(INSIGHT_STATEMENT_MAX)).is_ok());
        assert!(statement(&"é".repeat(INSIGHT_STATEMENT_MAX + 1)).is_err());
    }

    #[test]
    fn a_fact_change_reads_as_it_now_stands() {
        let base = || FactState {
            invalidated: false,
            superseded_by: None,
            active: true,
            trust: Trust::User,
            expired: false,
        };
        assert_eq!(fact_change(None, Trust::User), Some(SourceChange::Purged));
        assert_eq!(fact_change(Some(&base()), Trust::User), None);
        let by = Uuid::from_u128(9);
        let mut state = base();
        state.superseded_by = Some(by);
        assert_eq!(
            fact_change(Some(&state), Trust::User),
            Some(SourceChange::Superseded { by })
        );
        state.invalidated = true;
        assert_eq!(
            fact_change(Some(&state), Trust::User),
            Some(SourceChange::Invalidated)
        );
        let mut state = base();
        state.expired = true;
        assert_eq!(
            fact_change(Some(&state), Trust::User),
            Some(SourceChange::Expired)
        );
        let mut state = base();
        state.trust = Trust::Agent;
        assert_eq!(
            fact_change(Some(&state), Trust::User),
            Some(SourceChange::Purged)
        );
        assert_eq!(fact_change(Some(&state), Trust::Agent), None);
        let mut state = base();
        state.active = false;
        assert_eq!(
            fact_change(Some(&state), Trust::User),
            Some(SourceChange::Purged)
        );
    }

    #[test]
    fn merging_replaces_a_changed_entry_and_ignores_a_repeat() {
        let first = stale(
            InsightSourceKind::Fact,
            Uuid::from_u128(1),
            SourceChange::Expired,
        );
        let (reason, changed) = merge_reason(None, std::slice::from_ref(&first));
        assert!(changed);
        let (reason, changed) = merge_reason(Some(reason), std::slice::from_ref(&first));
        assert!(!changed);
        let again = stale(
            InsightSourceKind::Fact,
            Uuid::from_u128(1),
            SourceChange::Invalidated,
        );
        let other = stale(
            InsightSourceKind::Fact,
            Uuid::from_u128(2),
            SourceChange::Purged,
        );
        let (reason, changed) = merge_reason(Some(reason), &[again.clone(), other.clone()]);
        assert!(changed);
        assert_eq!(reason, vec![again, other]);
    }
}
