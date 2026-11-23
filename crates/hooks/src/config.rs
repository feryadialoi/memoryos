//! Reading the environment and `credentials.json`. Every function takes a lookup, so tests never
//! read or change the real environment. `MEMORYOS_URL` and `MEMORYOS_API_KEY` override the file.

use std::path::{Path, PathBuf};

use memoryos_sdk::{MemoryOs, Options, Scope};
use url::Url;

use crate::credentials;

pub const DEFAULT_URL: &str = "http://localhost:6366";
const DEFAULT_HOOK_SCOPES: &str = "agent:coding-agent";
const DEFAULT_STOP_EVERY: u64 = 20;
/// Stale insights and ready topics in the hook scopes at which `hook stop` asks for a reflect.
const DEFAULT_REFLECT_BACKLOG: usize = 3;

/// A variable's value. Empty counts as unset.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

pub fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// The server URL, without a trailing slash. Refuses credentials in the URL, because it gets
/// written to configuration files.
pub fn base_url(env: Env<'_>) -> Result<String, String> {
    let stored = || credentials::load(env).map(|c| c.map(|c| c.url).filter(|u| !u.is_empty()));
    let raw = match env("MEMORYOS_URL") {
        Some(url) => url,
        None => stored()?.unwrap_or_else(|| DEFAULT_URL.to_owned()),
    };
    let url = Url::parse(raw.trim()).map_err(|e| format!("MEMORYOS_URL is not a URL: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("MEMORYOS_URL must use http or https".to_owned());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("MEMORYOS_URL must not contain credentials".to_owned());
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

const NO_KEY: &str =
    "MEMORYOS_API_KEY is not set and there are no stored credentials. Run `memoryos \
                      setup`";

/// The agent key. Hooks, the plain commands and `memoryos mcp` use only this.
pub fn agent_key(env: Env<'_>) -> Result<String, String> {
    match env("MEMORYOS_API_KEY") {
        Some(key) => Ok(key),
        None => credentials::load(env)?
            .and_then(|stored| stored.agent_key)
            .ok_or_else(|| NO_KEY.to_owned()),
    }
}

pub fn client(env: Env<'_>, options: Options) -> Result<MemoryOs, String> {
    MemoryOs::new(base_url(env)?, agent_key(env)?, options).map_err(|e| e.to_string())
}

/// For `memoryos quarantine` alone: the stored admin key, or `MEMORYOS_API_KEY` when it is set.
pub fn admin_client(env: Env<'_>, options: Options) -> Result<MemoryOs, String> {
    MemoryOs::new(base_url(env)?, admin_key(env)?, options).map_err(|e| e.to_string())
}

/// The key of [`admin_client`], which `memoryos page write --approve` also uses.
pub fn admin_key(env: Env<'_>) -> Result<String, String> {
    match env("MEMORYOS_API_KEY") {
        Some(key) => Ok(key),
        None => credentials::load(env)?
            .and_then(|stored| stored.admin_key)
            .ok_or_else(|| NO_KEY.to_owned()),
    }
}

/// For `memoryos ui`: the stored admin key when there is one, so review works, else the agent key.
/// `MEMORYOS_API_KEY` overrides both.
pub fn ui_key(env: Env<'_>) -> Result<String, String> {
    match env("MEMORYOS_API_KEY") {
        Some(key) => Ok(key),
        None => credentials::load(env)?
            .and_then(|stored| stored.admin_key.or(stored.agent_key))
            .ok_or_else(|| NO_KEY.to_owned()),
    }
}

pub fn hook_scopes(env: Env<'_>) -> Result<Vec<Scope>, String> {
    let raw = env("MEMORYOS_HOOK_SCOPES").unwrap_or_else(|| DEFAULT_HOOK_SCOPES.to_owned());
    let scopes = raw
        .split(',')
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .map(|scope| {
            scope
                .parse()
                .map_err(|_| format!("MEMORYOS_HOOK_SCOPES has an invalid scope: {scope}"))
        })
        .collect::<Result<Vec<Scope>, String>>()?;
    if scopes.is_empty() {
        return Err("MEMORYOS_HOOK_SCOPES names no scope".to_owned());
    }
    Ok(scopes)
}

/// The project file naming the shared spaces a project's agents read.
pub const PROJECT_FILE: &str = ".memoryos.toml";

/// [`hook_scopes`], then the shared spaces named in the nearest [`PROJECT_FILE`] at or above `cwd`.
/// Only `shared:` spaces count, so a repository can never point an agent at a user's or another
/// agent's memory. The first scope stays the first hook scope, where saves go by default.
pub fn project_scopes(env: Env<'_>, cwd: Option<&Path>) -> Result<Vec<Scope>, String> {
    let mut scopes = hook_scopes(env)?;
    for space in cwd.and_then(project_spaces).unwrap_or_default() {
        if !scopes.contains(&space) {
            scopes.push(space);
        }
    }
    Ok(scopes)
}

/// The spaces of the nearest project file, or `None` when there is none.
pub fn project_spaces(cwd: &Path) -> Option<Vec<Scope>> {
    let text = cwd
        .ancestors()
        .find_map(|dir| std::fs::read_to_string(dir.join(PROJECT_FILE)).ok())?;
    Some(parse_spaces(&text))
}

/// Reads `spaces = ["ops", "shared:team"]`, on one line or several. A bare name means `shared:name`.
/// Anything else in the file is ignored, and so is an entry that is not a valid shared space.
fn parse_spaces(text: &str) -> Vec<Scope> {
    let text: String = text
        .lines()
        .map(|line| line.split('#').next().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    let Some(start) = text.find("spaces") else {
        return Vec::new();
    };
    let rest = text[start + "spaces".len()..].trim_start();
    let Some(rest) = rest.strip_prefix('=') else {
        return Vec::new();
    };
    let Some(list) = rest.trim_start().strip_prefix('[') else {
        return Vec::new();
    };
    let list = list.split(']').next().unwrap_or_default();
    list.split(',')
        .map(|item| item.trim().trim_matches('"').trim_matches('\'').trim())
        .filter(|item| !item.is_empty())
        .filter_map(|item| {
            let full = if item.contains(':') {
                item.to_owned()
            } else {
                format!("shared:{item}")
            };
            full.parse::<Scope>()
                .ok()
                .filter(|scope| matches!(scope, Scope::Shared(_)))
        })
        .collect()
}

pub fn stop_every(env: Env<'_>) -> Result<u64, String> {
    let Some(raw) = env("MEMORYOS_HOOK_STOP_EVERY") else {
        return Ok(DEFAULT_STOP_EVERY);
    };
    match raw.trim().parse() {
        Ok(every) if every > 0 => Ok(every),
        _ => Err("MEMORYOS_HOOK_STOP_EVERY must be a positive integer".to_owned()),
    }
}

/// `MEMORYOS_REFLECT_BACKLOG`: how many stale insights and ready topics make `hook stop` ask for a
/// reflect instead of the save reminder.
pub fn reflect_backlog(env: Env<'_>) -> Result<usize, String> {
    let Some(raw) = env("MEMORYOS_REFLECT_BACKLOG") else {
        return Ok(DEFAULT_REFLECT_BACKLOG);
    };
    match raw.trim().parse() {
        Ok(backlog) if backlog > 0 => Ok(backlog),
        _ => Err("MEMORYOS_REFLECT_BACKLOG must be a positive integer".to_owned()),
    }
}

pub fn home(env: Env<'_>) -> Result<PathBuf, String> {
    env("HOME")
        .or_else(|| env("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".to_owned())
}

/// `memoryos` under `$XDG_STATE_HOME`, or under `~/.local/state`. The nightly reflect logs here.
pub fn state_home(env: Env<'_>) -> Result<PathBuf, String> {
    // The XDG specification says a relative value must be ignored.
    let base = match env("XDG_STATE_HOME").map(PathBuf::from) {
        Some(path) if path.is_absolute() => path,
        _ => home(env)?.join(".local").join("state"),
    };
    Ok(base.join("memoryos"))
}

/// Where hook state lives.
pub fn state_dir(env: Env<'_>) -> Result<PathBuf, String> {
    Ok(state_home(env)?.join("hook-turns"))
}

/// Where `commit_facts` calls wait while the server cannot take them.
pub fn outbox_dir(env: Env<'_>) -> Result<PathBuf, String> {
    Ok(state_home(env)?.join("outbox"))
}

/// `$XDG_CONFIG_HOME`, or `~/.config`.
fn config_home(env: Env<'_>) -> Result<PathBuf, String> {
    match env("XDG_CONFIG_HOME").map(PathBuf::from) {
        Some(path) if path.is_absolute() => Ok(path),
        _ => Ok(home(env)?.join(".config")),
    }
}

/// Where OpenCode's global configuration lives.
pub fn opencode_config_dir(env: Env<'_>) -> Result<PathBuf, String> {
    Ok(config_home(env)?.join("opencode"))
}

/// Where systemd looks for the user's own units.
pub fn systemd_user_dir(env: Env<'_>) -> Result<PathBuf, String> {
    Ok(config_home(env)?.join("systemd").join("user"))
}

/// Where `memoryos setup` keeps credentials, the Compose file and the database.
pub fn memoryos_dir(env: Env<'_>) -> Result<PathBuf, String> {
    Ok(config_home(env)?.join("memoryos"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    #[test]
    fn reflect_backlog_defaults_to_three_and_must_be_positive() {
        assert_eq!(reflect_backlog(&with(&[])).expect("default"), 3);
        let env = with(&[("MEMORYOS_REFLECT_BACKLOG", " 7 ")]);
        assert_eq!(reflect_backlog(&env).expect("set"), 7);
        for bad in ["0", "-1", "x"] {
            let pairs = [("MEMORYOS_REFLECT_BACKLOG", bad)];
            assert!(reflect_backlog(&with(&pairs)).is_err(), "{bad}");
        }
    }

    #[test]
    fn url_defaults_and_loses_its_trailing_slash() {
        assert_eq!(base_url(&with(&[])).expect("url"), "http://localhost:6366");
        let env = with(&[("MEMORYOS_URL", "https://m.example/base/")]);
        assert_eq!(base_url(&env).expect("url"), "https://m.example/base");
    }

    #[test]
    fn url_rejects_credentials_and_other_schemes() {
        for bad in ["http://u:p@h", "http://u@h", "ftp://h", "not a url"] {
            let pairs = [("MEMORYOS_URL", bad)];
            assert!(base_url(&with(&pairs)).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_missing_key_is_an_error() {
        let error = client(&with(&[]), Options::default()).expect_err("no key");
        assert!(error.contains("MEMORYOS_API_KEY"));
    }

    #[test]
    fn stored_credentials_are_used_and_the_environment_overrides_them() {
        let dir = tempfile::tempdir().expect("dir");
        let home = dir.path().display().to_string();
        let stored = credentials::Credentials {
            url: "http://stored:1".to_owned(),
            agent_key: Some("mos_agent".to_owned()),
            admin_key: Some("mos_admin".to_owned()),
        };
        let pairs = [("HOME", home.as_str())];
        credentials::save(&with(&pairs), &stored).expect("save");

        assert_eq!(base_url(&with(&pairs)).expect("url"), "http://stored:1");
        assert_eq!(agent_key(&with(&pairs)).expect("key"), "mos_agent");

        let over = [
            ("HOME", home.as_str()),
            ("MEMORYOS_URL", "http://env:2"),
            ("MEMORYOS_API_KEY", "mos_env"),
        ];
        assert_eq!(base_url(&with(&over)).expect("url"), "http://env:2");
        assert_eq!(agent_key(&with(&over)).expect("key"), "mos_env");
    }

    #[test]
    fn the_agent_key_never_falls_back_to_the_admin_key() {
        let dir = tempfile::tempdir().expect("dir");
        let home = dir.path().display().to_string();
        let stored = credentials::Credentials {
            url: "http://stored:1".to_owned(),
            agent_key: None,
            admin_key: Some("mos_admin".to_owned()),
        };
        let pairs = [("HOME", home.as_str())];
        credentials::save(&with(&pairs), &stored).expect("save");
        assert!(agent_key(&with(&pairs)).is_err());
        assert!(admin_client(&with(&pairs), Options::default()).is_ok());
    }

    #[test]
    fn hook_scopes_default_and_parse() {
        assert_eq!(
            hook_scopes(&with(&[])).expect("scopes"),
            [Scope::agent("coding-agent")]
        );
        let env = with(&[("MEMORYOS_HOOK_SCOPES", "user:a, agent:b,")]);
        assert_eq!(
            hook_scopes(&env).expect("scopes"),
            [Scope::user("a"), Scope::agent("b")]
        );
        assert!(hook_scopes(&with(&[("MEMORYOS_HOOK_SCOPES", "bad")])).is_err());
        assert!(hook_scopes(&with(&[("MEMORYOS_HOOK_SCOPES", ",")])).is_err());
    }

    #[test]
    fn project_files_add_shared_spaces_only() {
        let spaces = parse_spaces(
            "# knowledge for this repo\nname = \"x\"\nspaces = [\n  \"ops\",  # runbooks\n  'shared:team',\n  \"user:alice\", \"agent:other\", \"bad name\",\n]\n",
        );
        assert_eq!(spaces, [Scope::shared("ops"), Scope::shared("team")]);
        assert!(parse_spaces("spaces = []").is_empty());
        assert!(parse_spaces("other = 1").is_empty());
    }

    #[test]
    fn project_scopes_come_from_the_nearest_file_up_the_tree() {
        let dir = tempfile::tempdir().expect("dir");
        let nested = dir.path().join("crates/server");
        std::fs::create_dir_all(&nested).expect("mkdir");
        std::fs::write(
            dir.path().join(PROJECT_FILE),
            "spaces = [\"ops\", \"coding-agent\"]",
        )
        .expect("write");
        let scopes = project_scopes(&with(&[]), Some(&nested)).expect("scopes");
        assert_eq!(
            scopes,
            [
                Scope::agent("coding-agent"),
                Scope::shared("ops"),
                Scope::shared("coding-agent")
            ]
        );
        let elsewhere = tempfile::tempdir().expect("dir");
        assert_eq!(
            project_scopes(&with(&[]), Some(elsewhere.path())).expect("scopes"),
            [Scope::agent("coding-agent")]
        );
        assert_eq!(
            project_scopes(&with(&[]), None).expect("scopes"),
            [Scope::agent("coding-agent")]
        );
    }

    #[test]
    fn stop_every_must_be_positive() {
        assert_eq!(stop_every(&with(&[])).expect("default"), 20);
        assert_eq!(
            stop_every(&with(&[("MEMORYOS_HOOK_STOP_EVERY", "3")])).expect("set"),
            3
        );
        for bad in ["0", "-1", "x"] {
            assert!(stop_every(&with(&[("MEMORYOS_HOOK_STOP_EVERY", bad)])).is_err());
        }
    }

    #[test]
    fn state_dir_follows_xdg_when_absolute() {
        let home = [("HOME", "/h")];
        assert_eq!(
            state_dir(&with(&home)).expect("dir"),
            PathBuf::from("/h/.local/state/memoryos/hook-turns")
        );
        let xdg = [("HOME", "/h"), ("XDG_STATE_HOME", "/s")];
        assert_eq!(
            state_dir(&with(&xdg)).expect("dir"),
            PathBuf::from("/s/memoryos/hook-turns")
        );
        let relative = [("HOME", "/h"), ("XDG_STATE_HOME", "s")];
        assert_eq!(
            state_dir(&with(&relative)).expect("dir"),
            PathBuf::from("/h/.local/state/memoryos/hook-turns")
        );
        assert!(state_dir(&with(&[])).is_err());
    }

    #[test]
    fn opencode_dir_follows_xdg_when_absolute() {
        assert_eq!(
            opencode_config_dir(&with(&[("HOME", "/h")])).expect("dir"),
            PathBuf::from("/h/.config/opencode")
        );
        let xdg = [("HOME", "/h"), ("XDG_CONFIG_HOME", "/c")];
        assert_eq!(
            opencode_config_dir(&with(&xdg)).expect("dir"),
            PathBuf::from("/c/opencode")
        );
    }
}
