//! Page search for recall: sections of current page versions, fused like facts and returned as one
//! hit per page. See docs/SPEC.md section 7.

use std::collections::{HashMap, HashSet};

use memoryos_core::{Error, PageSectionHit, Result, Scope};
use serde::Serialize;
use uuid::Uuid;

pub const DEFAULT_PAGE_LIMIT: i64 = 3;
pub const MAX_PAGE_LIMIT: i64 = 10;
/// Candidates per search list, vector and text.
pub(crate) const PAGE_CANDIDATES_PER_LIST: i64 = 20;
const RRF_K: f64 = 60.0;
const SNIPPET_CHARS: usize = 300;

/// The best section of one page for a query.
#[derive(Debug, Clone, Serialize)]
pub struct PageHit {
    pub page_id: Uuid,
    pub scope: Scope,
    pub slug: String,
    pub title: String,
    pub version: i64,
    /// Heading path of the section, such as `Upgrade MemoryOS > Native install`.
    pub section: String,
    /// The section's first 300 characters.
    pub snippet: String,
    pub score: f64,
}

impl PageHit {
    pub(crate) fn new(section: &PageSectionHit, score: f64) -> Self {
        Self {
            page_id: section.page_id,
            scope: section.scope.clone(),
            slug: section.slug.clone(),
            title: section.title.clone(),
            version: section.version,
            section: section.heading.clone(),
            snippet: section.text.chars().take(SNIPPET_CHARS).collect(),
            score,
        }
    }
}

/// What the reranker reads for a section: its heading, then its text.
pub(crate) fn rerank_text(section: &PageSectionHit) -> String {
    format!("{}\n{}", section.heading, section.text)
}

/// Reciprocal rank fusion over section lists with `k = 60`, then only the best section of each
/// page, highest score first. Equal scores order by section id, so the result is deterministic.
pub(crate) fn fuse_sections<'a>(lists: &[&'a [PageSectionHit]]) -> Vec<(&'a PageSectionHit, f64)> {
    let mut scored: HashMap<Uuid, (&'a PageSectionHit, f64)> = HashMap::new();
    for list in lists {
        let mut seen = HashSet::new();
        let unique = list.iter().filter(|hit| seen.insert(hit.section_id));
        for (index, hit) in unique.enumerate() {
            let contribution = 1.0 / (RRF_K + (index + 1) as f64);
            scored.entry(hit.section_id).or_insert((hit, 0.0)).1 += contribution;
        }
    }
    let mut fused: Vec<_> = scored.into_values().collect();
    fused.sort_unstable_by(|(a, a_score), (b, b_score)| {
        b_score
            .total_cmp(a_score)
            .then_with(|| a.section_id.cmp(&b.section_id))
    });
    let mut pages = HashSet::new();
    fused.retain(|(hit, _)| pages.insert(hit.page_id));
    fused
}

/// Checks `page_limit` and returns it as a count.
pub(crate) fn validate_page_limit(page_limit: i64) -> Result<usize> {
    if !(0..=MAX_PAGE_LIMIT).contains(&page_limit) {
        return Err(Error::InvalidInput(format!(
            "page_limit must be in 0..={MAX_PAGE_LIMIT}, got {page_limit}"
        )));
    }
    Ok(page_limit as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(page: u128, id: u128, text: &str) -> PageSectionHit {
        PageSectionHit {
            section_id: Uuid::from_u128(id),
            page_id: Uuid::from_u128(page),
            scope: Scope::shared("s"),
            slug: format!("p{page}"),
            title: format!("Page {page}"),
            version: 1,
            heading: format!("Page {page} > S{id}"),
            text: text.to_owned(),
            score: 0.0,
        }
    }

    fn ids(fused: &[(&PageSectionHit, f64)]) -> Vec<u128> {
        fused
            .iter()
            .map(|(hit, _)| hit.section_id.as_u128())
            .collect()
    }

    #[test]
    fn a_section_in_both_lists_ranks_first() {
        let vector = [section(1, 10, "a"), section(2, 20, "b")];
        let text = [section(2, 20, "b")];
        let fused = fuse_sections(&[&vector, &text]);
        assert_eq!(ids(&fused), [20, 10]);
        assert_eq!(fused[0].1, 1.0 / 62.0 + 1.0 / 61.0);
    }

    #[test]
    fn only_the_best_section_of_a_page_is_kept() {
        let vector = [
            section(1, 10, "a"),
            section(1, 11, "b"),
            section(2, 20, "c"),
        ];
        let fused = fuse_sections(&[&vector]);
        assert_eq!(ids(&fused), [10, 20]);
    }

    #[test]
    fn repeats_in_one_list_count_once() {
        let vector = [section(1, 10, "a"), section(1, 10, "a")];
        assert_eq!(fuse_sections(&[&vector])[0].1, 1.0 / 61.0);
    }

    #[test]
    fn snippet_is_the_first_300_characters() {
        let long = "字".repeat(400);
        let hit = PageHit::new(&section(1, 10, &long), 0.5);
        assert_eq!(hit.snippet.chars().count(), 300);
        assert_eq!(hit.section, "Page 1 > S10");
    }

    #[test]
    fn page_limit_is_0_to_10() {
        assert!(validate_page_limit(-1).is_err());
        assert!(validate_page_limit(11).is_err());
        assert_eq!(validate_page_limit(0).ok(), Some(0));
        assert_eq!(validate_page_limit(10).ok(), Some(10));
    }
}
