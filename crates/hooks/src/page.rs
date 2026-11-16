//! `memoryos page list|read|write|edit|sync|pull|delete`: knowledge pages (SPEC section 8) through the REST routes,
//! with the agent key. The scope defaults to the first hook scope, where the hooks and the MCP proxy
//! also read and write.

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use clap::Subcommand;
use memoryos_sdk::Scope;
use serde_json::{json, Value};

use crate::api::{Api, PageWrite};
use crate::client::read_input;
use crate::config::{self, process_env, Env};
use crate::knowledge;

const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Subcommand)]
pub enum PageCommand {
    /// List the pages of a scope that have a current version, by slug.
    List {
        /// Default: the first of `MEMORYOS_HOOK_SCOPES`.
        #[arg(long)]
        scope: Option<Scope>,
        /// Only slugs starting with this.
        #[arg(long)]
        prefix: Option<String>,
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Print a page's current version, or an older one.
    Read {
        slug: String,
        /// Default: the first of `MEMORYOS_HOOK_SCOPES`.
        #[arg(long)]
        scope: Option<Scope>,
        #[arg(long, value_parser = clap::value_parser!(i64).range(1..))]
        version: Option<i64>,
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Write a new version from markdown in a file or on stdin. Omit `--expected-version` to
    /// create the page.
    Write {
        slug: String,
        #[arg(long)]
        title: String,
        /// Default: the first of `MEMORYOS_HOOK_SCOPES`.
        #[arg(long)]
        scope: Option<Scope>,
        /// What changed.
        #[arg(long)]
        summary: Option<String>,
        /// The current version being edited.
        #[arg(long, value_parser = clap::value_parser!(i64).range(1..))]
        expected_version: Option<i64>,
        /// Write and approve in one step. Needs an admin key, found as `memoryos quarantine` finds
        /// it.
        #[arg(long)]
        approve: bool,
        /// File holding the markdown body. Reads stdin when absent or `-`.
        #[arg(long)]
        file: Option<PathBuf>,
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Open a page in your editor (`MEMORYOS_EDITOR`, `VISUAL` or `EDITOR`, such as `code --wait`)
    /// and save it as a new version when the editor closes. A new slug starts an empty page.
    Edit {
        slug: String,
        /// Default: the first of `MEMORYOS_HOOK_SCOPES`.
        #[arg(long)]
        scope: Option<Scope>,
        /// Write and approve in one step. Needs an admin key.
        #[arg(long)]
        approve: bool,
    },
    /// Write changed `*.md` files of a folder to the scope, approved with the admin key. State is
    /// kept in `.memoryos-sync.json` in the folder; commit it with the pages.
    Sync {
        dir: PathBuf,
        /// Default: the first of `MEMORYOS_HOOK_SCOPES`.
        #[arg(long)]
        scope: Option<Scope>,
        /// Let the file win over a page edited in MemoryOS since the last sync.
        #[arg(long)]
        force: bool,
        /// Delete pages whose file was removed since the last sync.
        #[arg(long)]
        prune: bool,
    },
    /// Write pages edited in MemoryOS since the last sync back to their files in the folder.
    Pull {
        dir: PathBuf,
        /// Default: the first of `MEMORYOS_HOOK_SCOPES`.
        #[arg(long)]
        scope: Option<Scope>,
        /// Overwrite files that have changes not synced.
        #[arg(long)]
        force: bool,
    },
    /// Delete a page with every version.
    Delete {
        slug: String,
        /// Default: the first of `MEMORYOS_HOOK_SCOPES`.
        #[arg(long)]
        scope: Option<Scope>,
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
}

pub fn run(command: PageCommand) -> Result<(), String> {
    let output = execute(command, &process_env)?;
    // A closed pipe, as with `| head`, is not a failure of the command.
    let mut stdout = std::io::stdout();
    let _ = writeln!(stdout, "{output}").and_then(|()| stdout.flush());
    Ok(())
}

/// What the command prints.
fn execute(command: PageCommand, env: Env<'_>) -> Result<String, String> {
    match command {
        PageCommand::List {
            scope,
            prefix,
            json,
        } => {
            let scope = scope_or_default(scope, env)?;
            let value = block_on(api(env, false)?.list_pages(&scope, prefix.as_deref()))?;
            Ok(if json {
                value.to_string()
            } else {
                list_text(&value)
            })
        }
        PageCommand::Read {
            slug,
            scope,
            version,
            json,
        } => {
            let scope = scope_or_default(scope, env)?;
            let value = block_on(api(env, false)?.read_page(&scope, &slug, version))?;
            Ok(if json {
                value.to_string()
            } else {
                read_text(&value)
            })
        }
        PageCommand::Write {
            slug,
            title,
            scope,
            summary,
            expected_version,
            approve,
            file,
            json,
        } => {
            let scope = scope_or_default(scope, env)?;
            let body = read_input(
                file.as_deref(),
                "no page body: pass --file, or pipe the markdown to stdin",
            )?;
            let page = PageWrite {
                title,
                body,
                summary,
                expected_version,
                approve,
            };
            let value = block_on(api(env, approve)?.write_page(&scope, &slug, &page))?;
            Ok(if json {
                value.to_string()
            } else {
                write_text(&value)
            })
        }
        PageCommand::Edit {
            slug,
            scope,
            approve,
        } => {
            let scope = scope_or_default(scope, env)?;
            let editor = knowledge::editor_command(env)?;
            let api = api(env, approve)?;
            let approve = if approve {
                knowledge::Approve::Yes
            } else {
                knowledge::Approve::No
            };
            block_on(knowledge::edit(&api, &scope, &slug, approve, &editor))
        }
        PageCommand::Sync {
            dir,
            scope,
            force,
            prune,
        } => {
            let scope = scope_or_default(scope, env)?;
            block_on(knowledge::sync(
                &api(env, true)?,
                &dir,
                &scope,
                force,
                prune,
            ))
        }
        PageCommand::Pull { dir, scope, force } => {
            let scope = scope_or_default(scope, env)?;
            block_on(knowledge::pull(&api(env, true)?, &dir, &scope, force))
        }
        PageCommand::Delete { slug, scope, json } => {
            let scope = scope_or_default(scope, env)?;
            block_on(api(env, false)?.delete_page(&scope, &slug))?;
            Ok(if json {
                json!({"deleted": true, "scope": scope, "slug": slug}).to_string()
            } else {
                format!("deleted {scope} {slug}")
            })
        }
    }
}

fn scope_or_default(scope: Option<Scope>, env: Env<'_>) -> Result<Scope, String> {
    match scope {
        Some(scope) => Ok(scope),
        None => config::hook_scopes(env)?
            .into_iter()
            .next()
            .ok_or_else(|| "MEMORYOS_HOOK_SCOPES names no scope".to_owned()),
    }
}

fn api(env: Env<'_>, admin: bool) -> Result<Api, String> {
    let key = if admin {
        config::admin_key(env)?
    } else {
        config::agent_key(env)?
    };
    Api::new(&config::base_url(env)?, key, TIMEOUT)
}

fn block_on<T>(work: impl std::future::Future<Output = Result<T, String>>) -> Result<T, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the runtime: {e}"))?
        .block_on(work)
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn number(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_i64)
        .map(|n| n.to_string())
        .unwrap_or_default()
}

/// One line per page: slug, version, edit time and title.
fn list_text(value: &Value) -> String {
    let pages = value
        .get("pages")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if pages.is_empty() {
        return "no pages".to_owned();
    }
    pages
        .iter()
        .map(|page| {
            format!(
                "{}  v{}  {}  {}",
                text(page, "slug"),
                number(page, "version"),
                text(page, "edited_at"),
                text(page, "title")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A header line, then the markdown body as stored.
fn read_text(page: &Value) -> String {
    format!(
        "{} ({} {}, version {}, {}, trust {}, edited by {} at {})\n\n{}",
        text(page, "title"),
        text(page, "scope"),
        text(page, "slug"),
        number(page, "version"),
        text(page, "status"),
        text(page, "trust"),
        text(page, "edited_by"),
        text(page, "edited_at"),
        text(page, "body").trim_end()
    )
}

fn write_text(version: &Value) -> String {
    let mut out = format!(
        "{} {} version {}: {}",
        text(version, "scope"),
        text(version, "slug"),
        number(version, "version"),
        text(version, "status")
    );
    if text(version, "status") == "quarantined" {
        out.push_str(" (waiting for review; readers still see the current version)");
    }
    if version.get("redacted").and_then(Value::as_bool) == Some(true) {
        out.push_str("\na secret was masked as [redacted]");
    }
    out
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::api::fake::Server;

    #[derive(Debug, Parser)]
    struct Cli {
        #[command(subcommand)]
        command: PageCommand,
    }

    fn parse(args: &[&str]) -> Result<PageCommand, clap::Error> {
        let all = std::iter::once("page").chain(args.iter().copied());
        Cli::try_parse_from(all).map(|cli| cli.command)
    }

    fn env_for(url: &str, scopes: Option<&str>) -> impl Fn(&str) -> Option<String> {
        let (url, scopes) = (url.to_owned(), scopes.map(str::to_owned));
        move |name| match name {
            "MEMORYOS_URL" => Some(url.clone()),
            "MEMORYOS_API_KEY" => Some("mos_agent".to_owned()),
            "MEMORYOS_HOOK_SCOPES" => scopes.clone(),
            _ => None,
        }
    }

    fn version_reply(status: &str, redacted: bool) -> String {
        json!({"page_id": uuid::Uuid::nil(), "scope": "shared:team", "slug": "deploy",
            "version": 2, "base_version": 1, "title": "Deploy", "summary": null, "trust": "agent",
            "status": status, "edited_by": "k", "edited_at": "2026-01-01T00:00:00Z",
            "redacted": redacted})
        .to_string()
    }

    #[test]
    fn arguments_parse_and_validate() {
        assert!(parse(&["list"]).is_ok());
        assert!(parse(&["list", "--scope", "shared:t", "--prefix", "dep", "--json"]).is_ok());
        assert!(parse(&["list", "--scope", "nope"]).is_err());
        assert!(parse(&["read", "s", "--version", "2"]).is_ok());
        assert!(parse(&["read", "s", "--version", "0"]).is_err());
        assert!(parse(&["read"]).is_err(), "a slug is required");
        assert!(parse(&["write", "s"]).is_err(), "a title is required");
        let command = parse(&[
            "write",
            "s",
            "--title",
            "T",
            "--summary",
            "why",
            "--expected-version",
            "3",
            "--approve",
            "--file",
            "-",
        ])
        .expect("write");
        let PageCommand::Write {
            expected_version,
            approve,
            file,
            scope,
            ..
        } = command
        else {
            panic!("wrong command");
        };
        assert_eq!(
            (expected_version, approve, scope),
            (Some(3), true, None::<Scope>)
        );
        assert_eq!(file, Some(PathBuf::from("-")));
        assert!(parse(&["delete", "s", "--scope", "agent:a"]).is_ok());
    }

    #[test]
    fn list_defaults_to_the_first_hook_scope() {
        let reply = json!({"pages": [{"id": uuid::Uuid::nil(), "scope": "user:u", "slug": "a",
            "title": "Alpha", "version": 4, "edited_at": "2026-01-01T00:00:00Z"}]});
        let server = Server::start(vec![
            (200, reply.to_string()),
            (200, json!({"pages": []}).to_string()),
        ]);
        let env = env_for(&server.url, Some("user:u,agent:b"));
        let list = |prefix: Option<&str>| PageCommand::List {
            scope: None,
            prefix: prefix.map(str::to_owned),
            json: false,
        };
        let out = execute(list(Some("a b")), &env).expect("list");
        assert_eq!(out, "a  v4  2026-01-01T00:00:00Z  Alpha");
        assert_eq!(execute(list(None), &env).expect("empty"), "no pages");
        let received = server.finish();
        assert_eq!(received[0].method, "GET");
        assert_eq!(received[0].target, "/v1/pages?scope=user%3Au&prefix=a+b");
        assert_eq!(
            received[0].authorization.as_deref(),
            Some("Bearer mos_agent")
        );
        assert_eq!(received[1].target, "/v1/pages?scope=user%3Au");
    }

    #[test]
    fn the_default_scope_is_agent_coding_agent_without_configuration() {
        let server = Server::start(vec![(200, json!({"pages": []}).to_string())]);
        let env = env_for(&server.url, None);
        let command = PageCommand::List {
            scope: None,
            prefix: None,
            json: true,
        };
        assert_eq!(execute(command, &env).expect("list"), r#"{"pages":[]}"#);
        assert_eq!(
            server.finish()[0].target,
            "/v1/pages?scope=agent%3Acoding-agent"
        );
    }

    #[test]
    fn read_asks_for_a_version_and_prints_the_body() {
        let page = json!({"id": uuid::Uuid::nil(), "scope": "shared:team", "slug": "deploy",
            "title": "Deploy", "body": "# Deploy\n\nRun it.\n", "summary": null, "version": 2,
            "trust": "user", "status": "superseded", "edited_by": "k",
            "edited_at": "2026-01-01T00:00:00Z"});
        let server = Server::start(vec![(200, page.to_string())]);
        let env = env_for(&server.url, None);
        let command = PageCommand::Read {
            slug: "deploy".to_owned(),
            scope: Some(Scope::shared("team")),
            version: Some(2),
            json: false,
        };
        let out = execute(command, &env).expect("read");
        assert_eq!(
            out,
            "Deploy (shared:team deploy, version 2, superseded, trust user, edited by k at \
             2026-01-01T00:00:00Z)\n\n# Deploy\n\nRun it."
        );
        let received = server.finish();
        assert_eq!(received[0].method, "GET");
        assert_eq!(received[0].target, "/v1/pages/shared:team/deploy?version=2");
    }

    #[test]
    fn write_sends_the_documented_body_from_a_file() {
        let dir = tempfile::tempdir().expect("dir");
        let file = dir.path().join("page.md");
        std::fs::write(&file, "# Deploy\n\nRun it.\n").expect("write");
        let server = Server::start(vec![
            (200, version_reply("quarantined", true)),
            (200, version_reply("active", false)),
        ]);
        let env = env_for(&server.url, Some("shared:team"));
        let write =
            |summary: Option<&str>, expected: Option<i64>, approve: bool| PageCommand::Write {
                slug: "deploy".to_owned(),
                title: "Deploy".to_owned(),
                scope: None,
                summary: summary.map(str::to_owned),
                expected_version: expected,
                approve,
                file: Some(file.clone()),
                json: false,
            };
        let out = execute(write(Some("steps"), Some(1), false), &env).expect("write");
        assert!(
            out.starts_with("shared:team deploy version 2: quarantined (waiting"),
            "{out}"
        );
        assert!(out.contains("[redacted]"), "{out}");
        let out = execute(write(None, None, true), &env).expect("create");
        assert_eq!(out, "shared:team deploy version 2: active");

        let received = server.finish();
        assert_eq!(received[0].method, "PUT");
        assert_eq!(received[0].target, "/v1/pages/shared:team/deploy");
        let body: Value = serde_json::from_str(&received[0].body).expect("json");
        assert_eq!(
            body,
            json!({"title": "Deploy", "body": "# Deploy\n\nRun it.\n", "summary": "steps",
                "expected_version": 1})
        );
        let body: Value = serde_json::from_str(&received[1].body).expect("json");
        assert_eq!(
            body,
            json!({"title": "Deploy", "body": "# Deploy\n\nRun it.\n", "approve": true})
        );
    }

    #[test]
    fn approve_uses_the_admin_key() {
        let dir = tempfile::tempdir().expect("dir");
        let home = dir.path().display().to_string();
        let server = Server::start(vec![(200, version_reply("active", false))]);
        let url = server.url.clone();
        let stored = move |name: &str| match name {
            "HOME" => Some(home.clone()),
            "MEMORYOS_URL" => Some(url.clone()),
            _ => None,
        };
        crate::credentials::save(
            &stored,
            &crate::credentials::Credentials {
                url: String::new(),
                agent_key: Some("mos_agent".to_owned()),
                admin_key: Some("mos_admin".to_owned()),
            },
        )
        .expect("save");
        let file = dir.path().join("p.md");
        std::fs::write(&file, "x").expect("write");
        let command = PageCommand::Write {
            slug: "s".to_owned(),
            title: "T".to_owned(),
            scope: None,
            summary: None,
            expected_version: None,
            approve: true,
            file: Some(file),
            json: true,
        };
        execute(command, &stored).expect("write");
        let received = server.finish();
        assert_eq!(
            received[0].authorization.as_deref(),
            Some("Bearer mos_admin")
        );
    }

    #[test]
    fn delete_reports_both_forms_and_errors_pass_through() {
        let not_found = json!({"error": {"code": "not_found", "message": "no such page"}});
        let server = Server::start(vec![
            (204, String::new()),
            (204, String::new()),
            (404, not_found.to_string()),
        ]);
        let env = env_for(&server.url, None);
        let delete = |json| PageCommand::Delete {
            slug: "old".to_owned(),
            scope: None,
            json,
        };
        assert_eq!(
            execute(delete(false), &env).expect("text"),
            "deleted agent:coding-agent old"
        );
        let value: Value =
            serde_json::from_str(&execute(delete(true), &env).expect("json")).expect("json");
        assert_eq!(
            value,
            json!({"deleted": true, "scope": "agent:coding-agent", "slug": "old"})
        );
        assert_eq!(
            execute(delete(false), &env).expect_err("missing"),
            "not found: no such page"
        );
        let received = server.finish();
        assert_eq!(received[0].method, "DELETE");
        assert_eq!(received[0].target, "/v1/pages/agent:coding-agent/old");
    }

    #[test]
    fn a_missing_body_file_and_a_missing_key_are_errors() {
        let command = PageCommand::Write {
            slug: "s".to_owned(),
            title: "T".to_owned(),
            scope: Some(Scope::agent("a")),
            summary: None,
            expected_version: None,
            approve: false,
            file: Some(PathBuf::from("/nonexistent/page.md")),
            json: false,
        };
        let env = env_for("http://127.0.0.1:1", None);
        let error = execute(command, &env).expect_err("missing file");
        assert!(error.contains("/nonexistent/page.md"), "{error}");

        let no_key = |_: &str| None;
        let command = PageCommand::List {
            scope: Some(Scope::agent("a")),
            prefix: None,
            json: false,
        };
        let error = execute(command, &no_key).expect_err("no key");
        assert!(error.contains("MEMORYOS_API_KEY"), "{error}");
    }
}
