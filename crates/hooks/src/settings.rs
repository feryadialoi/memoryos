//! `~/.config/memoryos/server.env`: `KEY=VALUE` lines the server reads at startup, under the
//! process environment, which wins. `memoryos reranker` writes the reranker lines and keeps the rest
//! of the file as it was.

use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{self, process_env};
use crate::credentials;

pub const FILE: &str = "server.env";

/// The file's path, or `None` when there is no home directory to put it in.
pub fn path() -> Option<PathBuf> {
    config::memoryos_dir(&process_env)
        .ok()
        .map(|dir| dir.join(FILE))
}

/// Looks a name up in the process environment, then in `server.env`. Empty counts as unset in both.
pub struct Lookup {
    file: Vec<(String, String)>,
}

impl Lookup {
    /// A missing file is no settings. A file that cannot be read is an error, because the server
    /// would otherwise start with settings other than the ones written there.
    pub fn load() -> Result<Self, String> {
        let file = match path() {
            Some(path) => read(&path)?,
            None => Vec::new(),
        };
        Ok(Self { file })
    }

    pub fn get(&self, name: &str) -> Option<String> {
        let set = |value: &String| !value.trim().is_empty();
        std::env::var(name)
            .ok()
            .filter(set)
            .or_else(|| self.in_file(name).filter(set))
    }

    fn in_file(&self, name: &str) -> Option<String> {
        self.file
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    }
}

pub fn read(path: &Path) -> Result<Vec<(String, String)>, String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(parse(&text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(format!("cannot read {}: {error}", path.display())),
    }
}

/// Blank lines and `#` comments are skipped, as are lines with no `=`. Values are taken as written,
/// without quote handling, the way the Makefile and `set -a; source` read them for these values.
fn parse(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            Some((key.trim().to_owned(), value.trim().to_owned()))
        })
        .collect()
}

/// Sets each `(name, value)` in the file: replaces the first line for that name, drops any later
/// ones, and appends names it did not have. Other lines, comments included, stay as they were. The
/// file is written with mode 0600, since it can hold the database password.
pub fn write(path: &Path, updates: &[(&str, String)]) -> Result<(), String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };
    let updated = update(&text, updates);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    credentials::write_private(path, updated.as_bytes())
}

fn update(text: &str, updates: &[(&str, String)]) -> String {
    let mut done = vec![false; updates.len()];
    let mut lines = Vec::new();
    for line in text.lines() {
        let key = line
            .trim()
            .split_once('=')
            .filter(|_| !line.trim_start().starts_with('#'))
            .map(|(key, _)| key.trim());
        match key.and_then(|key| updates.iter().position(|(name, _)| *name == key)) {
            Some(index) if done[index] => {}
            Some(index) => {
                done[index] = true;
                lines.push(format!("{}={}", updates[index].0, updates[index].1));
            }
            None => lines.push(line.to_owned()),
        }
    }
    for (index, (name, value)) in updates.iter().enumerate() {
        if !done[index] {
            lines.push(format!("{name}={value}"));
        }
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_skips_comments_blanks_and_lines_without_a_value() {
        let pairs = parse("# note\n\nA=1\n  B = two words \nnot a setting\nC=x=y\n");
        let expected = [("A", "1"), ("B", "two words"), ("C", "x=y")];
        let expected: Vec<(String, String)> = expected
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        assert_eq!(pairs, expected);
    }

    #[test]
    fn update_replaces_in_place_drops_duplicates_and_appends_new_names() {
        let text =
            "# keep me\nDATABASE_URL=postgres://x\nMEMORYOS_RERANK_TOP=10\nMEMORYOS_RERANK_TOP=4\n";
        let out = update(
            text,
            &[
                ("MEMORYOS_RERANK_TOP", "5".to_owned()),
                ("MEMORYOS_RERANK_MODEL", "bge-reranker-base".to_owned()),
            ],
        );
        assert_eq!(
            out,
            "# keep me\nDATABASE_URL=postgres://x\nMEMORYOS_RERANK_TOP=5\n\
             MEMORYOS_RERANK_MODEL=bge-reranker-base\n"
        );
    }

    #[test]
    fn a_commented_out_setting_is_left_alone() {
        let out = update(
            "#MEMORYOS_RERANK_TOP=3\n",
            &[("MEMORYOS_RERANK_TOP", "5".to_owned())],
        );
        assert_eq!(out, "#MEMORYOS_RERANK_TOP=3\nMEMORYOS_RERANK_TOP=5\n");
    }

    #[test]
    fn write_creates_a_private_file_and_read_gets_it_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(FILE);
        assert!(read(&path).unwrap().is_empty());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        }
        write(&path, &[("A", "1".to_owned())]).unwrap();
        write(&path, &[("B", "2".to_owned()), ("A", "3".to_owned())]).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "A=3\nB=2\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn the_last_line_for_a_name_wins_when_reading() {
        let lookup = Lookup {
            file: parse("MEMORYOS_TEST_ONLY_SETTING=1\nMEMORYOS_TEST_ONLY_SETTING=2\n"),
        };
        assert_eq!(
            lookup.get("MEMORYOS_TEST_ONLY_SETTING").as_deref(),
            Some("2")
        );
        assert_eq!(lookup.get("MEMORYOS_TEST_ONLY_MISSING"), None);
    }
}
