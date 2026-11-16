//! Editing knowledge pages outside MemoryOS (docs/knowledge-editing-design.md): `page edit` opens a
//! page in the user's editor, and `page sync` / `page pull` map a folder of markdown files, usually
//! in git, to one scope.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use memoryos_sdk::Scope;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::api::{Api, PageWrite};

/// The sync state file, committed next to the pages.
pub const STATE_FILE: &str = ".memoryos-sync.json";

/// The editor command: `MEMORYOS_EDITOR`, then `VISUAL`, then `EDITOR`.
pub fn editor_command(env: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    ["MEMORYOS_EDITOR", "VISUAL", "EDITOR"]
        .iter()
        .filter_map(|name| env(name))
        .map(|value| value.trim().to_owned())
        .find(|value| !value.is_empty())
        .ok_or_else(|| {
            "no editor: set MEMORYOS_EDITOR, VISUAL or EDITOR, such as `code --wait` or \
             `idea --wait`"
                .to_owned()
        })
}

/// A page file: front matter with `title` and `summary`, then the markdown body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageFile {
    pub title: Option<String>,
    pub summary: Option<String>,
    pub body: String,
}

impl PageFile {
    /// Front matter is optional. Without a `title` in it, a first `# ` line is the title and is
    /// removed from the body.
    pub fn parse(text: &str) -> PageFile {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let (mut title, mut summary, rest) = match split_front_matter(text) {
            Some((head, rest)) => {
                let mut title = None;
                let mut summary = None;
                for line in head.lines() {
                    if let Some((key, value)) = line.split_once(':') {
                        let value = value.trim();
                        let value = (!value.is_empty()).then(|| value.to_owned());
                        match key.trim() {
                            "title" => title = value,
                            "summary" => summary = value,
                            _ => {}
                        }
                    }
                }
                (title, summary, rest)
            }
            None => (None, None, text),
        };
        let mut body = rest.trim_start_matches('\n').to_owned();
        if title.is_none() {
            let first = body.lines().next().unwrap_or_default();
            if let Some(heading) = first.strip_prefix("# ") {
                title = Some(heading.trim().to_owned());
                body = body[first.len()..].trim_start_matches('\n').to_owned();
            }
        }
        if summary.as_deref() == Some("") {
            summary = None;
        }
        PageFile {
            title,
            summary,
            body,
        }
    }

    /// Front matter, then the body. `summary` is written only when set or when `ask_summary` is set,
    /// as for `page edit`, which offers it as the place to say what changed.
    pub fn render(&self, ask_summary: bool) -> String {
        let mut out = String::from("---\n");
        out.push_str(&format!("title: {}\n", self.title.as_deref().unwrap_or("")));
        match self.summary.as_deref() {
            Some(summary) => out.push_str(&format!("summary: {summary}\n")),
            None if ask_summary => out.push_str("summary:\n"),
            None => {}
        }
        out.push_str("---\n");
        out.push_str(&self.body);
        if !self.body.ends_with('\n') {
            out.push('\n');
        }
        out
    }
}

fn split_front_matter(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("---\n")?;
    let end = rest.find("\n---\n").map(|i| (i, i + 5)).or_else(|| {
        rest.strip_suffix("\n---")
            .map(|head| (head.len(), rest.len()))
    })?;
    Some((&rest[..end.0], &rest[end.1..]))
}

fn sha256(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn is_not_found(error: &str) -> bool {
    error.starts_with("not found")
}

fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn version(value: &Value) -> Option<i64> {
    value.get("version").and_then(Value::as_i64)
}

/// The current version, or `None` when the page has none.
async fn current(api: &Api, scope: &Scope, slug: &str) -> Result<Option<Value>, String> {
    match api.read_page(scope, slug, None).await {
        Ok(page) => Ok(Some(page)),
        Err(error) if is_not_found(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn write_note(reply: &Value) -> String {
    let mut out = format!("version {}", version(reply).unwrap_or_default());
    if text(reply, "status") == "quarantined" {
        out.push_str(", waiting for review");
    }
    if reply.get("redacted").and_then(Value::as_bool) == Some(true) {
        out.push_str(", a secret was masked as [redacted]");
    }
    out
}

/// How an edit is approved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approve {
    No,
    Yes,
    /// Approve when the key may, otherwise write it for review.
    IfAllowed,
}

/// `page edit`: opens the current version in `editor` and writes the result as a new version based
/// on the version opened. On any failure after editing, the file is kept and its path is in the
/// error.
pub async fn edit(
    api: &Api,
    scope: &Scope,
    slug: &str,
    approve: Approve,
    editor: &str,
) -> Result<String, String> {
    let page = current(api, scope, slug).await?;
    let opened = PageFile {
        title: Some(
            page.as_ref()
                .map(|p| text(p, "title"))
                .unwrap_or_else(|| slug.to_owned()),
        ),
        summary: None,
        body: page.as_ref().map(|p| text(p, "body")).unwrap_or_default(),
    };
    let base = page.as_ref().and_then(version);
    let dir = temp_dir()?;
    let path = dir.join(format!("{slug}.md"));
    let original = opened.render(true);
    std::fs::write(&path, &original)
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;

    let status = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("memoryos-edit")
        .arg(&path)
        .status()
        .await
        .map_err(|e| format!("cannot run the editor `{editor}`: {e}"))?;
    let kept = |error: String| format!("{error}. Your edit is kept at {}", path.display());
    if !status.success() {
        return Err(kept(format!("the editor `{editor}` exited with {status}")));
    }
    let edited =
        std::fs::read_to_string(&path).map_err(|e| kept(format!("cannot read the edit: {e}")))?;
    if edited == original {
        let _ = std::fs::remove_dir_all(&dir);
        return Ok(format!("{scope} {slug}: no change"));
    }
    let file = PageFile::parse(&edited);
    let mut request = PageWrite {
        title: file.title.unwrap_or_else(|| slug.to_owned()),
        body: file.body,
        summary: file.summary,
        expected_version: base,
        approve: approve != Approve::No,
    };
    let reply = match api.write_page(scope, slug, &request).await {
        Err(error) if error.starts_with("forbidden") && approve == Approve::IfAllowed => {
            request.approve = false;
            api.write_page(scope, slug, &request).await
        }
        other => other,
    }
    .map_err(kept)?;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(format!("{scope} {slug}: {}", write_note(&reply)))
}

/// A private directory for one edit.
fn temp_dir() -> Result<PathBuf, String> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let dir = std::env::temp_dir().join(format!("memoryos-edit-{}-{nanos}", std::process::id()));
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(&dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    Ok(dir)
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SyncState {
    scope: Option<String>,
    #[serde(default)]
    pages: BTreeMap<String, Synced>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Synced {
    version: i64,
    sha256: String,
}

impl SyncState {
    fn load(dir: &Path, scope: &Scope) -> Result<SyncState, String> {
        let path = dir.join(STATE_FILE);
        let state: SyncState = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| format!("{} is not valid: {e}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => SyncState::default(),
            Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
        };
        if let Some(saved) = &state.scope {
            if *saved != scope.to_string() {
                return Err(format!(
                    "{} belongs to scope {saved}, not {scope}",
                    path.display()
                ));
            }
        }
        Ok(state)
    }

    fn save(&mut self, dir: &Path, scope: &Scope) -> Result<(), String> {
        self.scope = Some(scope.to_string());
        let path = dir.join(STATE_FILE);
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())? + "\n";
        std::fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
    }
}

fn valid_slug(slug: &str) -> bool {
    let mut chars = slug.chars();
    slug.len() <= 128
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

/// Top-level `*.md` files by slug, and the names that were skipped.
fn page_files(dir: &Path) -> Result<(BTreeMap<String, PathBuf>, Vec<String>), String> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    let mut files = BTreeMap::new();
    let mut skipped = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !path.is_file() || !name.ends_with(".md") {
            continue;
        }
        let slug = name.trim_end_matches(".md");
        if valid_slug(slug) {
            files.insert(slug.to_owned(), path);
        } else {
            skipped.push(name);
        }
    }
    Ok((files, skipped))
}

async fn remote_slugs(api: &Api, scope: &Scope) -> Result<BTreeMap<String, i64>, String> {
    let list = api.list_pages(scope, None).await?;
    Ok(list
        .get("pages")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|page| Some((text(page, "slug"), version(page)?)))
        .collect())
}

/// `page sync`: files to MemoryOS. Writes are approved, so `api` holds the admin key.
pub async fn sync(
    api: &Api,
    dir: &Path,
    scope: &Scope,
    force: bool,
    prune: bool,
) -> Result<String, String> {
    let mut state = SyncState::load(dir, scope)?;
    let (files, skipped) = page_files(dir)?;
    let mut lines = Vec::new();
    for (slug, path) in &files {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let sha = sha256(&raw);
        let synced = state.pages.get(slug).cloned();
        if synced.as_ref().is_some_and(|s| s.sha256 == sha) {
            continue;
        }
        let now = current(api, scope, slug).await?.as_ref().and_then(version);
        let conflict = match (&synced, now) {
            (_, None) => None,
            (Some(s), Some(v)) if s.version == v => None,
            (Some(s), Some(v)) => Some(format!(
                "edited in MemoryOS since the last sync (now version {v}, synced {}). Run \
                 `memoryos page pull`, or `sync --force` to make the file win",
                s.version
            )),
            (None, Some(v)) => Some(format!(
                "already in MemoryOS (version {v}) and never synced. Run `memoryos page pull`, \
                 or `sync --force` to make the file win"
            )),
        };
        if let Some(conflict) = conflict.filter(|_| !force) {
            lines.push(format!("conflict  {slug}: {conflict}"));
            continue;
        }
        let file = PageFile::parse(&raw);
        let request = PageWrite {
            title: file.title.unwrap_or_else(|| slug.clone()),
            body: file.body,
            summary: Some(
                file.summary
                    .unwrap_or_else(|| format!("sync from {}", file_name(path))),
            ),
            expected_version: now,
            approve: true,
        };
        let reply = api.write_page(scope, slug, &request).await;
        match reply {
            Ok(reply) => {
                if let Some(v) = version(&reply) {
                    state.pages.insert(
                        slug.clone(),
                        Synced {
                            version: v,
                            sha256: sha,
                        },
                    );
                }
                lines.push(format!("synced    {slug}: {}", write_note(&reply)));
            }
            Err(error) => lines.push(format!("failed    {slug}: {error}")),
        }
    }
    let remote = remote_slugs(api, scope).await?;
    for slug in remote.keys().filter(|slug| !files.contains_key(*slug)) {
        if prune && state.pages.contains_key(slug) {
            match api.delete_page(scope, slug).await {
                Ok(()) => {
                    state.pages.remove(slug);
                    lines.push(format!("deleted   {slug}: its file was removed"));
                }
                Err(error) => lines.push(format!("failed    {slug}: {error}")),
            }
        } else if state.pages.contains_key(slug) {
            lines.push(format!(
                "missing   {slug}: file removed; `sync --prune` deletes the page"
            ));
        } else {
            lines.push(format!(
                "remote    {slug}: only in MemoryOS; `pull` fetches it"
            ));
        }
    }
    for name in skipped {
        lines.push(format!("skipped   {name}: not a valid slug"));
    }
    state.save(dir, scope)?;
    Ok(summary(lines, "everything is in sync"))
}

/// `page pull`: MemoryOS to files, for pages whose current version moved past the state.
pub async fn pull(api: &Api, dir: &Path, scope: &Scope, force: bool) -> Result<String, String> {
    let mut state = SyncState::load(dir, scope)?;
    let mut lines = Vec::new();
    for (slug, now) in remote_slugs(api, scope).await? {
        let path = dir.join(format!("{slug}.md"));
        let synced = state.pages.get(&slug).cloned();
        let local = std::fs::read_to_string(&path).ok();
        if synced.as_ref().is_some_and(|s| s.version == now) && local.is_some() {
            continue;
        }
        let local_changed = match (&local, &synced) {
            (None, _) => false,
            (Some(raw), Some(s)) => sha256(raw) != s.sha256,
            (Some(_), None) => true,
        };
        if local_changed && !force {
            lines.push(format!(
                "conflict  {slug}: the file has changes not synced. Sync it first, or `pull \
                 --force` to overwrite the file"
            ));
            continue;
        }
        let Some(page) = current(api, scope, &slug).await? else {
            continue;
        };
        let file = PageFile {
            title: Some(text(&page, "title")),
            summary: None,
            body: text(&page, "body"),
        };
        let raw = file.render(false);
        std::fs::write(&path, &raw).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        let v = version(&page).unwrap_or(now);
        state.pages.insert(
            slug.clone(),
            Synced {
                version: v,
                sha256: sha256(&raw),
            },
        );
        lines.push(format!("pulled    {slug}: version {v}"));
    }
    state.save(dir, scope)?;
    Ok(summary(lines, "everything is in sync"))
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn summary(lines: Vec<String>, when_empty: &str) -> String {
    if lines.is_empty() {
        when_empty.to_owned()
    } else {
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn front_matter_round_trips() {
        let file = PageFile {
            title: Some("Deploy runbook".into()),
            summary: Some("why".into()),
            body: "## Deploy\nMerge.\n".into(),
        };
        assert_eq!(PageFile::parse(&file.render(false)), file);
        assert!(!PageFile {
            summary: None,
            ..file.clone()
        }
        .render(false)
        .contains("summary"));
        assert!(PageFile {
            summary: None,
            ..file
        }
        .render(true)
        .contains("summary:\n"));
    }

    #[test]
    fn a_first_heading_is_the_title_without_front_matter() {
        let file = PageFile::parse("# Deploy runbook\n\n## Deploy\nMerge.\n");
        assert_eq!(file.title.as_deref(), Some("Deploy runbook"));
        assert_eq!(file.body, "## Deploy\nMerge.\n");
        assert_eq!(file.summary, None);
    }

    #[test]
    fn empty_summary_and_colons_in_titles() {
        let file = PageFile::parse("---\ntitle: Ops: deploy\nsummary:\n---\nbody\n");
        assert_eq!(file.title.as_deref(), Some("Ops: deploy"));
        assert_eq!(file.summary, None);
        assert_eq!(file.body, "body\n");
    }

    #[test]
    fn editor_comes_from_the_first_set_variable() {
        let env = |name: &str| match name {
            "VISUAL" => Some("code --wait".to_owned()),
            "EDITOR" => Some("vim".to_owned()),
            _ => None,
        };
        assert_eq!(editor_command(&env).as_deref(), Ok("code --wait"));
        assert!(editor_command(&|_: &str| None).is_err());
    }

    #[test]
    fn slugs_follow_the_page_rules() {
        assert!(valid_slug("deploy-runbook"));
        assert!(valid_slug("v1.2_notes"));
        assert!(!valid_slug("Deploy"));
        assert!(!valid_slug("-x"));
        assert!(!valid_slug(""));
    }

    #[test]
    fn page_files_skips_bad_names_and_subfolders() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join("ok.md"), "x").expect("write");
        std::fs::write(dir.path().join("Bad Name.md"), "x").expect("write");
        std::fs::write(dir.path().join("notes.txt"), "x").expect("write");
        std::fs::create_dir(dir.path().join("sub.md")).expect("dir");
        let (files, skipped) = page_files(dir.path()).expect("list");
        assert_eq!(files.keys().collect::<Vec<_>>(), vec!["ok"]);
        assert_eq!(skipped, vec!["Bad Name.md".to_owned()]);
    }
}
