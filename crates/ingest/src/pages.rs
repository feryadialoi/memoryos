//! Page writes (SPEC section 8): mask secrets, split the body into sections, embed every section in
//! one provider call, and hand the version to the store, which decides its status atomically.

use std::sync::Arc;

use memoryos_core::{
    validate_slug, Embedder, Error, MemoryStore, NewPageSection, NewPageVersion, PageVersion,
    RequestContext, Result, Scope, Trust, PAGE_BODY_MAX, PAGE_SECTIONS_MAX, PAGE_SECTION_TEXT_MAX,
    PAGE_SUMMARY_MAX, PAGE_TITLE_MAX,
};

use crate::redact::redact;

/// Longest section heading, in characters. A heading is a title path built from the page's own
/// headings, and nothing else bounds it, so one huge heading line is cut here.
pub const PAGE_HEADING_MAX: usize = 500;

/// Separates the levels of a section's title path.
const PATH_SEPARATOR: &str = " > ";

/// One section of a page body, before it is embedded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitSection {
    pub ordinal: i32,
    /// Title path, such as `Upgrade MemoryOS > Native install`.
    pub heading: String,
    /// At most `PAGE_SECTION_TEXT_MAX` characters, never blank.
    pub text: String,
}

/// Splits a markdown body into sections (SPEC section 8, Write step 2).
///
/// - A section starts at each ATX heading of level 1 to 3 (`#`, `##`, `###` after at most three
///   spaces, then a space, a tab or the line end) outside fenced code blocks. `####` and deeper are
///   body text.
/// - A section's heading is its title path starting with `title`. A heading at level n replaces
///   the path's levels n and deeper. Text before the first heading has the title alone.
/// - The heading line itself is not part of the text, and blank lines around the text are dropped.
/// - A section over `PAGE_SECTION_TEXT_MAX` characters splits at blank lines, then line ends, then
///   characters, packing greedily, each part keeping the heading.
/// - Blank sections are dropped. Ordinals count from 0 in order after dropping.
pub fn split_sections(title: &str, body: &str) -> Vec<SplitSection> {
    let title = title.trim();
    let mut path: [Option<String>; 3] = [None, None, None];
    let mut raw: Vec<(String, Vec<&str>)> = vec![(heading_for(title, &path), Vec::new())];
    let mut fence: Option<Fence> = None;
    for line in body.lines() {
        if let Some(open) = &fence {
            if open.is_closed_by(line) {
                fence = None;
            }
        } else if let Some(open) = Fence::opened_by(line) {
            fence = Some(open);
        } else if let Some((level, text)) = atx_heading(line) {
            path[level - 1] = (!text.is_empty()).then(|| text.to_owned());
            for deeper in &mut path[level..] {
                *deeper = None;
            }
            raw.push((heading_for(title, &path), Vec::new()));
            continue;
        }
        if let Some((_, lines)) = raw.last_mut() {
            lines.push(line);
        }
    }

    let mut sections = Vec::new();
    for (heading, lines) in raw {
        for text in pack(&trim_blank_lines(&lines), Level::Paragraphs) {
            if text.trim().is_empty() {
                continue;
            }
            sections.push(SplitSection {
                ordinal: i32::try_from(sections.len()).unwrap_or(i32::MAX),
                heading: heading.clone(),
                text,
            });
        }
    }
    sections
}

/// `title > h1 > h2 > h3`, skipping levels that are not set, cut to `PAGE_HEADING_MAX` characters.
fn heading_for(title: &str, path: &[Option<String>; 3]) -> String {
    let mut heading = title.to_owned();
    for part in path.iter().flatten() {
        heading.push_str(PATH_SEPARATOR);
        heading.push_str(part);
    }
    truncate_chars(heading, PAGE_HEADING_MAX)
}

fn truncate_chars(mut text: String, max: usize) -> String {
    if let Some((index, _)) = text.char_indices().nth(max) {
        text.truncate(index);
    }
    text
}

/// Strips up to three leading spaces, as markdown allows before a heading or a fence.
fn strip_indent(line: &str) -> Option<&str> {
    let spaces = line.len() - line.trim_start_matches(' ').len();
    (spaces <= 3).then(|| &line[spaces..])
}

/// The level and text of an ATX heading of level 1 to 3, without its closing `#` run.
fn atx_heading(line: &str) -> Option<(usize, &str)> {
    let rest = strip_indent(line)?;
    let level = rest.len() - rest.trim_start_matches('#').len();
    if !(1..=3).contains(&level) {
        return None;
    }
    let after = &rest[level..];
    if !(after.is_empty() || after.starts_with([' ', '\t'])) {
        return None;
    }
    let text = after.trim();
    // A closing run of `#` counts only when a space separates it from the text, or it is all there is.
    let without_closing = text.trim_end_matches('#');
    let text = if without_closing.is_empty() {
        ""
    } else if without_closing.ends_with([' ', '\t']) {
        without_closing.trim_end()
    } else {
        text
    };
    Some((level, text))
}

/// An open fenced code block: its fence character and run length.
struct Fence {
    marker: char,
    len: usize,
}

impl Fence {
    /// Three or more backticks or tildes. A backtick fence's info string has no backtick.
    fn opened_by(line: &str) -> Option<Self> {
        let rest = strip_indent(line)?;
        let marker = rest.chars().next().filter(|c| matches!(c, '`' | '~'))?;
        let len = rest.len() - rest.trim_start_matches(marker).len();
        if len < 3 || (marker == '`' && rest[len..].contains('`')) {
            return None;
        }
        Some(Self { marker, len })
    }

    /// The same character, at least as many times, and nothing after it but whitespace.
    fn is_closed_by(&self, line: &str) -> bool {
        let Some(rest) = strip_indent(line) else {
            return false;
        };
        let len = rest.len() - rest.trim_start_matches(self.marker).len();
        len >= self.len && rest[len..].trim().is_empty()
    }
}

fn is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

/// The lines joined with `\n`, without blank lines at either end.
fn trim_blank_lines(lines: &[&str]) -> String {
    let start = lines
        .iter()
        .position(|l| !is_blank(l))
        .unwrap_or(lines.len());
    let end = lines
        .iter()
        .rposition(|l| !is_blank(l))
        .map_or(start, |i| i + 1);
    lines[start..end].join("\n")
}

/// How an oversized piece of text is cut, coarsest first.
#[derive(Clone, Copy)]
enum Level {
    Paragraphs,
    Lines,
    Chars,
}

/// Cuts `text` into parts of at most `PAGE_SECTION_TEXT_MAX` characters. Pieces at one level are
/// packed greedily, rejoined with their separator. A piece too long on its own is cut at the next
/// level.
fn pack(text: &str, level: Level) -> Vec<String> {
    if char_len(text) <= PAGE_SECTION_TEXT_MAX {
        return vec![text.to_owned()];
    }
    let (pieces, separator, next) = match level {
        Level::Paragraphs => (paragraphs(text), "\n\n", Level::Lines),
        Level::Lines => (
            text.lines().map(str::to_owned).collect(),
            "\n",
            Level::Chars,
        ),
        Level::Chars => return char_chunks(text),
    };
    let separator_len = char_len(separator);
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut current_len = 0;
    for piece in pieces {
        let piece_len = char_len(&piece);
        if piece_len > PAGE_SECTION_TEXT_MAX {
            if !current.is_empty() {
                parts.push(std::mem::take(&mut current));
                current_len = 0;
            }
            parts.extend(pack(&piece, next));
        } else if current.is_empty() {
            current = piece;
            current_len = piece_len;
        } else if current_len + separator_len + piece_len <= PAGE_SECTION_TEXT_MAX {
            current.push_str(separator);
            current.push_str(&piece);
            current_len += separator_len + piece_len;
        } else {
            parts.push(std::mem::replace(&mut current, piece));
            current_len = piece_len;
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// Runs of non-blank lines.
fn paragraphs(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut lines: Vec<&str> = Vec::new();
    for line in text.lines() {
        if is_blank(line) {
            if !lines.is_empty() {
                found.push(lines.join("\n"));
                lines.clear();
            }
        } else {
            lines.push(line);
        }
    }
    if !lines.is_empty() {
        found.push(lines.join("\n"));
    }
    found
}

fn char_chunks(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(PAGE_SECTION_TEXT_MAX)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

fn char_len(text: &str) -> usize {
    text.chars().count()
}

/// A page write as the caller decided it. `trust` and `approve_up_to` come from the calling key, as
/// `MemoryStore::put_page` describes.
#[derive(Debug, Clone)]
pub struct PageWrite {
    pub scope: Scope,
    pub slug: String,
    pub title: String,
    /// Markdown.
    pub body: String,
    pub summary: Option<String>,
    pub trust: Trust,
    /// `None` creates the page. `Some(v)` edits current version `v`.
    pub expected_version: Option<i64>,
    /// Set for an admin write that approves itself: the caller's `max_trust`.
    pub approve_up_to: Option<Trust>,
}

/// The stored version, and whether a secret in the title, body or summary was masked first.
#[derive(Debug, Clone)]
pub struct PageWriteOutcome {
    pub version: PageVersion,
    pub redacted: bool,
}

pub struct PageWriter {
    store: Arc<dyn MemoryStore>,
    embedder: Arc<dyn Embedder>,
}

impl PageWriter {
    pub fn new(store: Arc<dyn MemoryStore>, embedder: Arc<dyn Embedder>) -> Self {
        Self { store, embedder }
    }

    /// Validates, masks secrets, splits and embeds, then stores the version. Nothing is stored when
    /// validation or the embedder fails.
    pub async fn write(&self, ctx: &RequestContext, req: PageWrite) -> Result<PageWriteOutcome> {
        validate_page(&req)?;
        let mut req = req;
        let mut redacted = redact(&mut req.title);
        redacted |= redact(&mut req.body);
        if let Some(summary) = &mut req.summary {
            redacted |= redact(summary);
        }
        // A mask can be longer than the secret it replaces, and the store refuses an oversized
        // title, body or summary.
        if redacted {
            validate_page(&req)?;
        }

        let split = split_sections(&req.title, &req.body);
        if split.len() > PAGE_SECTIONS_MAX {
            return Err(Error::InvalidInput(format!(
                "page has {} sections, at most {PAGE_SECTIONS_MAX} are allowed",
                split.len()
            )));
        }
        let embeddings = self.embed(&split).await?;
        let sections = split
            .into_iter()
            .zip(embeddings)
            .map(|(section, embedding)| NewPageSection {
                ordinal: section.ordinal,
                heading: section.heading,
                text: section.text,
                embedding,
            })
            .collect();

        let version = self
            .store
            .put_page(
                ctx,
                NewPageVersion {
                    scope: req.scope,
                    slug: req.slug,
                    title: req.title,
                    body: req.body,
                    summary: req.summary,
                    trust: req.trust,
                    expected_version: req.expected_version,
                    approve_up_to: req.approve_up_to,
                    sections,
                },
            )
            .await?;
        Ok(PageWriteOutcome { version, redacted })
    }

    /// One provider call for every section, `heading + "\n" + text` each. No call without sections.
    async fn embed(&self, sections: &[SplitSection]) -> Result<Vec<Vec<f32>>> {
        if sections.is_empty() {
            return Ok(Vec::new());
        }
        let inputs: Vec<String> = sections
            .iter()
            .map(|s| format!("{}\n{}", s.heading, s.text))
            .collect();
        let vectors = self
            .embedder
            .embed(&inputs)
            .await
            .map_err(|err| match err {
                Error::Provider(message) => Error::Provider(message),
                other => Error::Provider(other.to_string()),
            })?;
        if vectors.len() != inputs.len() {
            return Err(Error::Provider(format!(
                "embedder returned {} vectors for {} inputs",
                vectors.len(),
                inputs.len()
            )));
        }
        Ok(vectors)
    }
}

/// The limits of SPEC section 8, counted in characters. NUL is refused, since PostgreSQL cannot
/// store it.
fn validate_page(req: &PageWrite) -> Result<()> {
    validate_slug(&req.slug)?;
    if req.title.trim().is_empty() {
        return Err(Error::InvalidInput("title must not be empty".into()));
    }
    within("title", &req.title, PAGE_TITLE_MAX)?;
    within("body", &req.body, PAGE_BODY_MAX)?;
    if let Some(summary) = &req.summary {
        within("summary", summary, PAGE_SUMMARY_MAX)?;
    }
    Ok(())
}

fn within(field: &str, value: &str, max: usize) -> Result<()> {
    if value.contains('\0') {
        return Err(Error::InvalidInput(format!("{field} must not contain NUL")));
    }
    if char_len(value) > max {
        return Err(Error::InvalidInput(format!(
            "{field} exceeds {max} characters"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use chrono::Utc;
    use memoryos_core::{
        ApiKey, Block, BlockVersion, Entity, Episode, Fact, FactFilter, FactOp, GraphNeighborhood,
        NewEntity, NewEpisode, Page, PageSectionHit, PageSummary, PageVersionStatus,
        PendingPageVersion, PurgeReport, ScoredFact,
    };
    use memoryos_embed::TestEmbedder;
    use uuid::Uuid;

    use super::*;

    fn split(body: &str) -> Vec<(String, String)> {
        split_sections("Guide", body)
            .into_iter()
            .map(|s| (s.heading, s.text))
            .collect()
    }

    fn pair(heading: &str, text: &str) -> (String, String) {
        (heading.to_owned(), text.to_owned())
    }

    #[test]
    fn text_before_the_first_heading_belongs_to_the_title() {
        assert_eq!(
            split("\n\nIntro line\n\n# Install\nRun it.\n"),
            [
                pair("Guide", "Intro line"),
                pair("Guide > Install", "Run it.")
            ]
        );
    }

    #[test]
    fn title_path_replaces_levels_at_and_below_a_new_heading() {
        let body = "# A\na\n## B\nb\n### C\nc\n## D\nd\n### E\ne\n# F\nf\n### G\ng\n";
        assert_eq!(
            split(body),
            [
                pair("Guide > A", "a"),
                pair("Guide > A > B", "b"),
                pair("Guide > A > B > C", "c"),
                pair("Guide > A > D", "d"),
                pair("Guide > A > D > E", "e"),
                pair("Guide > F", "f"),
                pair("Guide > F > G", "g"),
            ]
        );
    }

    #[test]
    fn deeper_headings_and_non_headings_are_body_text() {
        let body = "# A\n#### Deep\nx\n#hashtag\n    # indented code\n";
        assert_eq!(
            split(body),
            [pair(
                "Guide > A",
                "#### Deep\nx\n#hashtag\n    # indented code"
            )]
        );
    }

    #[test]
    fn heading_syntax_variants_are_recognized() {
        let body = "   ## Spaced ##\na\n#\tTabbed\nb\n### Closing # run ###\nc\n";
        assert_eq!(
            split(body),
            [
                pair("Guide > Spaced", "a"),
                pair("Guide > Tabbed", "b"),
                pair("Guide > Tabbed > Closing # run", "c"),
            ]
        );
    }

    #[test]
    fn headings_inside_code_fences_are_ignored() {
        let body = "# Setup\n```bash\n# not a heading\n## nor this\n```\nafter\n~~~~\n# still code\n~~~\n# also code\n~~~~\n# Next\nn\n";
        assert_eq!(
            split(body),
            [
                pair(
                    "Guide > Setup",
                    "```bash\n# not a heading\n## nor this\n```\nafter\n~~~~\n# still code\n~~~\n# also code\n~~~~"
                ),
                pair("Guide > Next", "n"),
            ]
        );
        // A backtick fence does not close a tilde fence, and an unclosed fence runs to the end.
        let open = "# A\n~~~\n```\n# B\n";
        assert_eq!(split(open), [pair("Guide > A", "~~~\n```\n# B")]);
    }

    #[test]
    fn empty_sections_are_dropped_and_ordinals_stay_contiguous() {
        let sections = split_sections("Guide", "\n  \n# A\n\n   \n# B\nb\n## C\n\n# D\nd\n");
        let got: Vec<(i32, &str, &str)> = sections
            .iter()
            .map(|s| (s.ordinal, s.heading.as_str(), s.text.as_str()))
            .collect();
        assert_eq!(got, [(0, "Guide > B", "b"), (1, "Guide > D", "d")]);
        assert!(split_sections("Guide", "").is_empty());
        assert!(split_sections("Guide", "# Only\n").is_empty());
    }

    fn assert_parts_fit(sections: &[SplitSection]) {
        for (index, section) in sections.iter().enumerate() {
            assert_eq!(section.ordinal, i32::try_from(index).expect("small"));
            assert!(section.text.chars().count() <= PAGE_SECTION_TEXT_MAX);
            assert!(!section.text.trim().is_empty());
        }
    }

    #[test]
    fn long_sections_split_at_blank_lines_first() {
        let paragraph = "a".repeat(400);
        let body = format!("# Long\n{paragraph}\n\n{paragraph}\n\n{paragraph}\n");
        let sections = split_sections("Guide", &body);
        assert_parts_fit(&sections);
        let texts: Vec<&str> = sections.iter().map(|s| s.text.as_str()).collect();
        let two = format!("{paragraph}\n\n{paragraph}");
        assert_eq!(texts, [two.as_str(), paragraph.as_str()]);
        assert!(sections.iter().all(|s| s.heading == "Guide > Long"));
    }

    #[test]
    fn a_long_paragraph_splits_at_line_ends() {
        let line = "b".repeat(300);
        let body = [line.as_str(); 5].join("\n");
        let sections = split_sections("Guide", &body);
        assert_parts_fit(&sections);
        let texts: Vec<String> = sections.into_iter().map(|s| s.text).collect();
        let three = [line.as_str(); 3].join("\n");
        let two = [line.as_str(); 2].join("\n");
        assert_eq!(texts, [three, two]);
    }

    #[test]
    fn a_long_line_splits_at_characters() {
        let body = format!("short\n\n{}", "c".repeat(2_500));
        let sections = split_sections("Guide", &body);
        assert_parts_fit(&sections);
        let lens: Vec<usize> = sections.iter().map(|s| s.text.chars().count()).collect();
        assert_eq!(lens, [5, 1_000, 1_000, 500]);
    }

    #[test]
    fn cjk_text_is_counted_in_characters() {
        // 1,000 CJK characters are 3,000 bytes but fit one section.
        let fits = "字".repeat(PAGE_SECTION_TEXT_MAX);
        let sections = split_sections("指南", &fits);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].heading, "指南");
        let over = "字".repeat(PAGE_SECTION_TEXT_MAX + 1);
        let sections = split_sections("指南", &over);
        assert_parts_fit(&sections);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[1].text, "字");
    }

    #[test]
    fn huge_headings_are_cut_by_characters() {
        let body = format!("# {}\ntext\n", "標".repeat(2_000));
        let sections = split_sections("T", &body);
        assert_eq!(sections[0].heading.chars().count(), PAGE_HEADING_MAX);
        assert!(sections[0].heading.starts_with("T > 標"));
    }

    /// Records `put_page`. Every other method is unreachable from the writer, and each one is
    /// implemented so the fake keeps compiling once the trait's page defaults are removed.
    #[derive(Default)]
    struct FakeStore {
        puts: Mutex<Vec<NewPageVersion>>,
    }

    impl FakeStore {
        fn puts(&self) -> Vec<NewPageVersion> {
            self.puts.lock().expect("puts").clone()
        }
    }

    #[async_trait]
    impl MemoryStore for FakeStore {
        async fn claim_episode(
            &self,
            _: &RequestContext,
            _: NewEpisode,
        ) -> Result<(Episode, bool)> {
            unimplemented!()
        }
        async fn discard_unprocessed_episode(&self, _: &RequestContext, _: Uuid) -> Result<bool> {
            unimplemented!()
        }
        async fn get_episode(&self, _: &RequestContext, _: Uuid) -> Result<Episode> {
            unimplemented!()
        }
        async fn purge_episode(&self, _: &RequestContext, _: Uuid) -> Result<PurgeReport> {
            unimplemented!()
        }
        async fn purge_scope(&self, _: &RequestContext, _: &Scope) -> Result<PurgeReport> {
            unimplemented!()
        }
        async fn apply_commit(
            &self,
            _: &RequestContext,
            _: Uuid,
            _: Vec<FactOp>,
        ) -> Result<Vec<Uuid>> {
            unimplemented!()
        }
        async fn upsert_entity(&self, _: &RequestContext, _: NewEntity) -> Result<Entity> {
            unimplemented!()
        }
        async fn get_entity(&self, _: &RequestContext, _: Uuid) -> Result<Entity> {
            unimplemented!()
        }
        async fn similar_entities(
            &self,
            _: &RequestContext,
            _: &Scope,
            _: &[f32],
            _: i64,
        ) -> Result<Vec<(Entity, f64)>> {
            unimplemented!()
        }
        async fn add_entity_alias(
            &self,
            _: &RequestContext,
            _: Uuid,
            _: &str,
            _: Option<Uuid>,
        ) -> Result<()> {
            unimplemented!()
        }
        async fn find_entity(
            &self,
            _: &RequestContext,
            _: &Scope,
            _: &str,
            _: &str,
        ) -> Result<Option<Entity>> {
            unimplemented!()
        }
        async fn get_fact(&self, _: &RequestContext, _: Uuid) -> Result<Fact> {
            unimplemented!()
        }
        async fn facts_for_episode(&self, _: &RequestContext, _: Uuid) -> Result<Vec<Fact>> {
            unimplemented!()
        }
        async fn fact_history(&self, _: &RequestContext, _: Uuid) -> Result<Vec<Fact>> {
            unimplemented!()
        }
        async fn invalidate_fact(&self, _: &RequestContext, _: Uuid, _: &str) -> Result<Fact> {
            unimplemented!()
        }
        async fn current_fact(
            &self,
            _: &RequestContext,
            _: &Scope,
            _: Uuid,
        ) -> Result<Option<Fact>> {
            unimplemented!()
        }
        async fn current_facts_for_subject(
            &self,
            _: &RequestContext,
            _: &Scope,
            _: Uuid,
        ) -> Result<Vec<Fact>> {
            unimplemented!()
        }
        async fn search_facts_vector(
            &self,
            _: &RequestContext,
            _: &[Scope],
            _: &[f32],
            _: &FactFilter,
            _: i64,
        ) -> Result<Vec<ScoredFact>> {
            unimplemented!()
        }
        async fn search_facts_text(
            &self,
            _: &RequestContext,
            _: &[Scope],
            _: &str,
            _: &FactFilter,
            _: i64,
        ) -> Result<Vec<ScoredFact>> {
            unimplemented!()
        }
        async fn list_quarantined(
            &self,
            _: &RequestContext,
            _: &[Scope],
            _: i64,
        ) -> Result<Vec<Fact>> {
            unimplemented!()
        }
        async fn approve_fact(&self, _: &RequestContext, _: Uuid) -> Result<Fact> {
            unimplemented!()
        }
        async fn reject_fact(&self, _: &RequestContext, _: Uuid) -> Result<Fact> {
            unimplemented!()
        }
        async fn neighbors(
            &self,
            _: &RequestContext,
            _: &[Scope],
            _: Uuid,
            _: i32,
            _: &FactFilter,
            _: i64,
        ) -> Result<GraphNeighborhood> {
            unimplemented!()
        }
        async fn list_blocks(&self, _: &RequestContext, _: &str) -> Result<Vec<Block>> {
            unimplemented!()
        }
        async fn get_block(&self, _: &RequestContext, _: &str, _: &str) -> Result<Block> {
            unimplemented!()
        }
        async fn put_block(
            &self,
            _: &RequestContext,
            _: &str,
            _: &str,
            _: &str,
            _: i32,
            _: Option<i64>,
        ) -> Result<Block> {
            unimplemented!()
        }
        async fn block_versions(
            &self,
            _: &RequestContext,
            _: &str,
            _: &str,
        ) -> Result<Vec<BlockVersion>> {
            unimplemented!()
        }
        async fn list_pages(
            &self,
            _: &RequestContext,
            _: &Scope,
            _: Option<&str>,
            _: i64,
        ) -> Result<Vec<PageSummary>> {
            unimplemented!()
        }
        async fn get_page(
            &self,
            _: &RequestContext,
            _: &Scope,
            _: &str,
            _: Option<i64>,
        ) -> Result<Page> {
            unimplemented!()
        }
        async fn get_page_version(
            &self,
            _: &RequestContext,
            _: Uuid,
            _: i64,
        ) -> Result<PageVersion> {
            unimplemented!()
        }
        async fn put_page(&self, _: &RequestContext, page: NewPageVersion) -> Result<PageVersion> {
            let version = PageVersion {
                page_id: Uuid::now_v7(),
                scope: page.scope.clone(),
                slug: page.slug.clone(),
                version: page.expected_version.unwrap_or(0) + 1,
                base_version: page.expected_version,
                title: page.title.clone(),
                summary: page.summary.clone(),
                trust: page.trust,
                status: PageVersionStatus::Active,
                edited_by: "test".into(),
                edited_at: Utc::now(),
            };
            self.puts.lock().expect("puts").push(page);
            Ok(version)
        }
        async fn page_versions(
            &self,
            _: &RequestContext,
            _: &Scope,
            _: &str,
        ) -> Result<Vec<PageVersion>> {
            unimplemented!()
        }
        async fn delete_page(&self, _: &RequestContext, _: &Scope, _: &str) -> Result<()> {
            unimplemented!()
        }
        async fn list_quarantined_pages(
            &self,
            _: &RequestContext,
            _: &[Scope],
            _: i64,
        ) -> Result<Vec<PendingPageVersion>> {
            unimplemented!()
        }
        async fn approve_page_version(&self, _: &RequestContext, _: Uuid, _: i64) -> Result<Page> {
            unimplemented!()
        }
        async fn reject_page_version(
            &self,
            _: &RequestContext,
            _: Uuid,
            _: i64,
        ) -> Result<PageVersion> {
            unimplemented!()
        }
        async fn search_page_sections_vector(
            &self,
            _: &RequestContext,
            _: &[Scope],
            _: &[f32],
            _: i64,
        ) -> Result<Vec<PageSectionHit>> {
            unimplemented!()
        }
        async fn search_page_sections_text(
            &self,
            _: &RequestContext,
            _: &[Scope],
            _: &str,
            _: i64,
        ) -> Result<Vec<PageSectionHit>> {
            unimplemented!()
        }
        async fn find_api_key(&self, _: &str) -> Result<Option<ApiKey>> {
            unimplemented!()
        }
        async fn create_api_key(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &[String],
            _: Trust,
            _: bool,
        ) -> Result<ApiKey> {
            unimplemented!()
        }
        async fn revoke_api_key(&self, _: Uuid) -> Result<()> {
            unimplemented!()
        }
        async fn ping(&self) -> Result<()> {
            unimplemented!()
        }
    }

    struct Harness {
        store: Arc<FakeStore>,
        embedder: Arc<TestEmbedder>,
        writer: PageWriter,
    }

    fn harness() -> Harness {
        let store = Arc::new(FakeStore::default());
        let embedder = Arc::new(TestEmbedder::new());
        let writer = PageWriter::new(store.clone(), embedder.clone());
        Harness {
            store,
            embedder,
            writer,
        }
    }

    fn ctx() -> RequestContext {
        RequestContext {
            tenant: "t".into(),
            actor: "a".into(),
        }
    }

    fn write(body: &str) -> PageWrite {
        PageWrite {
            scope: Scope::agent("a"),
            slug: "upgrade".into(),
            title: "Upgrade".into(),
            body: body.into(),
            summary: Some("first draft".into()),
            trust: Trust::Agent,
            expected_version: None,
            approve_up_to: None,
        }
    }

    #[tokio::test]
    async fn a_write_embeds_every_section_in_one_call_and_stores_them() {
        let h = harness();
        let outcome = h
            .writer
            .write(&ctx(), write("Intro\n# Native\nRun the binary.\n"))
            .await
            .expect("write");
        assert!(!outcome.redacted);
        assert_eq!(outcome.version.slug, "upgrade");
        assert_eq!(
            h.embedder.batches(),
            [vec![
                "Upgrade\nIntro".to_owned(),
                "Upgrade > Native\nRun the binary.".to_owned()
            ]]
        );
        let puts = h.store.puts();
        assert_eq!(puts.len(), 1);
        let put = &puts[0];
        assert_eq!(put.scope, Scope::agent("a"));
        assert_eq!(
            (put.slug.as_str(), put.title.as_str()),
            ("upgrade", "Upgrade")
        );
        assert_eq!(put.summary.as_deref(), Some("first draft"));
        assert_eq!(put.trust, Trust::Agent);
        let sections: Vec<(i32, &str, &str)> = put
            .sections
            .iter()
            .map(|s| (s.ordinal, s.heading.as_str(), s.text.as_str()))
            .collect();
        assert_eq!(
            sections,
            [
                (0, "Upgrade", "Intro"),
                (1, "Upgrade > Native", "Run the binary.")
            ]
        );
        assert_eq!(
            put.sections[1].embedding,
            memoryos_embed::vector_for("Upgrade > Native\nRun the binary.")
        );
    }

    #[tokio::test]
    async fn a_write_passes_versioning_and_approval_through() {
        let h = harness();
        let mut req = write("text");
        req.expected_version = Some(3);
        req.approve_up_to = Some(Trust::User);
        req.trust = Trust::External;
        h.writer.write(&ctx(), req).await.expect("write");
        let put = &h.store.puts()[0];
        assert_eq!(put.expected_version, Some(3));
        assert_eq!(put.approve_up_to, Some(Trust::User));
        assert_eq!(put.trust, Trust::External);
    }

    #[tokio::test]
    async fn an_empty_body_stores_no_sections_and_calls_no_embedder() {
        let h = harness();
        h.writer.write(&ctx(), write("  \n")).await.expect("write");
        assert!(h.embedder.batches().is_empty());
        assert!(h.store.puts()[0].sections.is_empty());
    }

    #[tokio::test]
    async fn secrets_are_masked_in_title_body_and_summary_before_anything_else() {
        let h = harness();
        let mut req = write("# Keys\nset password=hunter2 now\n");
        req.title = "Key sk-abcdefghijklmnopqrstuvwxyz".into();
        req.summary = Some("rotated AKIAIOSFODNN7EXAMPLE".into());
        let outcome = h.writer.write(&ctx(), req).await.expect("write");
        assert!(outcome.redacted);
        let put = &h.store.puts()[0];
        assert_eq!(put.title, "Key [redacted]");
        assert_eq!(put.body, "# Keys\nset password=[redacted] now\n");
        assert_eq!(put.summary.as_deref(), Some("rotated [redacted]"));
        assert_eq!(
            h.embedder.batches(),
            [vec![
                "Key [redacted] > Keys\nset password=[redacted] now".to_owned()
            ]]
        );
    }

    #[tokio::test]
    async fn invalid_input_is_refused_before_the_embedder_and_the_store() {
        type Change = fn(&mut PageWrite);
        let changes: [Change; 9] = [
            |r| r.slug = "Bad Slug".into(),
            |r| r.slug = String::new(),
            |r| r.title = "  ".into(),
            |r| r.title = "t".repeat(PAGE_TITLE_MAX + 1),
            |r| r.title = "a\0b".into(),
            |r| r.body = "é".repeat(PAGE_BODY_MAX + 1),
            |r| r.body = "a\0b".into(),
            |r| r.summary = Some("s".repeat(PAGE_SUMMARY_MAX + 1)),
            // One section more than allowed.
            |r| r.body = "# h\nx\n".repeat(PAGE_SECTIONS_MAX + 1),
        ];
        for (index, change) in changes.iter().enumerate() {
            let h = harness();
            let mut req = write("text");
            change(&mut req);
            let result = h.writer.write(&ctx(), req).await;
            assert!(
                matches!(result, Err(Error::InvalidInput(_))),
                "change {index}: {result:?}"
            );
            assert!(h.embedder.batches().is_empty(), "change {index}");
            assert!(h.store.puts().is_empty(), "change {index}");
        }
    }

    #[tokio::test]
    async fn limits_are_inclusive_and_counted_in_characters() {
        let h = harness();
        let mut req = write(&"é".repeat(PAGE_BODY_MAX));
        req.title = "標".repeat(PAGE_TITLE_MAX);
        req.summary = Some("s".repeat(PAGE_SUMMARY_MAX));
        h.writer.write(&ctx(), req).await.expect("at the limits");
        h.writer
            .write(&ctx(), write(&"# h\nx\n".repeat(PAGE_SECTIONS_MAX)))
            .await
            .expect("exactly the most sections");
        assert_eq!(h.store.puts()[1].sections.len(), PAGE_SECTIONS_MAX);
    }

    #[tokio::test]
    async fn a_title_that_masking_grows_past_the_limit_is_refused() {
        let h = harness();
        let mut req = write("text");
        // `password=x` becomes `password=[redacted]`, nine characters longer.
        req.title = format!("{}password=x", "t".repeat(PAGE_TITLE_MAX - 10));
        let result = h.writer.write(&ctx(), req).await;
        assert!(matches!(result, Err(Error::InvalidInput(_))), "{result:?}");
        assert!(h.embedder.batches().is_empty());
        assert!(h.store.puts().is_empty());
    }

    #[tokio::test]
    async fn an_embedder_failure_stores_nothing() {
        let h = harness();
        h.embedder.set_failure("Upgrade > B\nb");
        let result = h.writer.write(&ctx(), write("# A\na\n# B\nb\n")).await;
        assert!(matches!(result, Err(Error::Provider(_))), "{result:?}");
        assert_eq!(h.embedder.batches().len(), 1);
        assert!(h.store.puts().is_empty());
    }

    /// Answers with the wrong error kind, or with one vector too few.
    struct BrokenEmbedder {
        short: bool,
    }

    #[async_trait]
    impl Embedder for BrokenEmbedder {
        async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
            if self.short {
                Ok(vec![memoryos_embed::vector_for("x"); inputs.len() - 1])
            } else {
                Err(Error::Internal("model crashed".into()))
            }
        }
    }

    #[tokio::test]
    async fn any_embedder_error_or_a_short_answer_is_a_provider_error() {
        for short in [false, true] {
            let store = Arc::new(FakeStore::default());
            let writer = PageWriter::new(store.clone(), Arc::new(BrokenEmbedder { short }));
            let result = writer.write(&ctx(), write("# A\na\n# B\nb\n")).await;
            assert!(matches!(result, Err(Error::Provider(_))), "{result:?}");
            assert!(store.puts().is_empty());
        }
    }
}
