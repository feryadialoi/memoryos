//! `memoryos insight list|read|retire`: insights (SPEC section 8, Insights) through the SDK, with the
//! agent key. The scopes of `list` default to the hook scopes.

use std::io::Write;
use std::time::Duration;

use clap::Subcommand;
use memoryos_sdk::{
    Insight, InsightDetail, InsightLevel, InsightStatus, MemoryOs, Options, Scope, SourceChange,
    StaleSource,
};
use serde_json::json;
use uuid::Uuid;

use crate::client::describe;
use crate::config::{self, process_env, Env};

const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Subcommand)]
pub enum InsightCommand {
    /// List insights, newest first.
    List {
        /// Scope to list. Repeat it or separate with commas. Default: `MEMORYOS_HOOK_SCOPES`.
        #[arg(long = "scope", value_delimiter = ',')]
        scopes: Vec<Scope>,
        /// Only this level: context, understanding or guidance.
        #[arg(long)]
        level: Option<InsightLevel>,
        /// Only this status, such as active or stale. Default: every status.
        #[arg(long)]
        status: Option<InsightStatus>,
        /// 1 to 500, default 100.
        #[arg(long)]
        limit: Option<i64>,
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Print an insight in any status with its sources as they are now and what it replaced.
    Read {
        id: Uuid,
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Withdraw an active or stale insight that is no longer true. Insights citing it go stale.
    Retire {
        id: Uuid,
        /// Why it no longer holds.
        #[arg(long)]
        reason: String,
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
}

pub fn run(command: InsightCommand) -> Result<(), String> {
    let output = execute(command, &process_env)?;
    // A closed pipe, as with `| head`, is not a failure of the command.
    let mut stdout = std::io::stdout();
    let _ = writeln!(stdout, "{output}").and_then(|()| stdout.flush());
    Ok(())
}

/// What the command prints.
fn execute(command: InsightCommand, env: Env<'_>) -> Result<String, String> {
    let client = config::client(
        env,
        Options {
            timeout: TIMEOUT,
            ..Options::default()
        },
    )?;
    match command {
        InsightCommand::List {
            scopes,
            level,
            status,
            limit,
            json,
        } => {
            let scopes = if scopes.is_empty() {
                config::hook_scopes(env)?
            } else {
                scopes
            };
            let insights = block_on(&client, |client| async move {
                client.list_insights(&scopes, level, status, limit).await
            })?;
            Ok(if json {
                json!({"insights": insights}).to_string()
            } else {
                list_text(&insights)
            })
        }
        InsightCommand::Read { id, json } => {
            let detail = block_on(
                &client,
                |client| async move { client.get_insight(id).await },
            )?;
            Ok(if json {
                serde_json::to_string(&detail).map_err(|e| e.to_string())?
            } else {
                read_text(&detail)
            })
        }
        InsightCommand::Retire { id, reason, json } => {
            let insight = block_on(&client, |client| async move {
                client.retire_insight(id, reason).await
            })?;
            Ok(if json {
                serde_json::to_string(&insight).map_err(|e| e.to_string())?
            } else {
                format!(
                    "{} {}: {}",
                    insight.status.as_str(),
                    insight.id,
                    insight.statement
                )
            })
        }
    }
}

fn block_on<'a, T, F>(
    client: &'a MemoryOs,
    work: impl FnOnce(&'a MemoryOs) -> F,
) -> Result<T, String>
where
    F: std::future::Future<Output = memoryos_sdk::Result<T>>,
{
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the runtime: {e}"))?
        .block_on(work(client))
        .map_err(describe)
}

/// One line per insight: id, status, level, scope and statement.
fn list_text(insights: &[Insight]) -> String {
    if insights.is_empty() {
        return "no insights".to_owned();
    }
    insights
        .iter()
        .map(|insight| {
            format!(
                "{}  {}  {}  {}  {}",
                insight.id,
                insight.status.as_str(),
                insight.level.as_str(),
                insight.scope,
                insight.statement
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A header line, the statement, what changed when it is stale, its sources and what it replaced.
fn read_text(detail: &InsightDetail) -> String {
    let insight = &detail.insight;
    let mut out = format!(
        "{} insight {} ({}, {}, trust {}, written by {} at {})\n{}\n",
        insight.level.as_str(),
        insight.id,
        insight.scope,
        insight.status.as_str(),
        insight.trust.as_str(),
        insight.written_by,
        insight.recorded_at.to_rfc3339(),
        insight.statement
    );
    if let Some(reason) = &insight.stale_reason {
        let changes = reason.iter().map(change_text).collect::<Vec<_>>();
        out.push_str(&format!("Stale: {}\n", changes.join("; ")));
    }
    out.push_str("Sources:\n");
    for view in &detail.source_views {
        let state = if view.current {
            "current"
        } else {
            "not current"
        };
        out.push_str(&format!(
            "- {} {} ({state}): {}\n",
            view.source.kind.as_str(),
            view.source.id,
            view.statement.as_deref().unwrap_or("[purged]")
        ));
    }
    if !detail.history.is_empty() {
        out.push_str("Replaced:\n");
        for old in &detail.history {
            out.push_str(&format!(
                "- {} ({}): {}\n",
                old.id,
                old.status.as_str(),
                old.statement
            ));
        }
    }
    out.trim_end().to_owned()
}

/// `fact <id> superseded by <id>`, `fact <id> invalidated`, `insight <id> now stale`.
pub(crate) fn change_text(stale: &StaleSource) -> String {
    let change = match &stale.change {
        SourceChange::Superseded { by } => format!("superseded by {by}"),
        SourceChange::Invalidated => "invalidated".to_owned(),
        SourceChange::Expired => "expired".to_owned(),
        SourceChange::Purged => "purged".to_owned(),
        SourceChange::InsightChanged { status } => format!("now {}", status.as_str()),
    };
    format!(
        "{} {} {change}",
        stale.source.kind.as_str(),
        stale.source.id
    )
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use memoryos_sdk::{InsightSourceKind, InsightSourceRef, InsightSourceView, Trust};
    use serde_json::Value;

    use super::*;
    use crate::api::fake::Server;

    fn insight(status: InsightStatus) -> Insight {
        let now = Utc::now();
        let source = |id| InsightSourceRef {
            kind: InsightSourceKind::Fact,
            id,
        };
        Insight {
            id: Uuid::now_v7(),
            scope: Scope::agent("a"),
            level: InsightLevel::Understanding,
            statement: "Ada values simple deploys".into(),
            trust: Trust::Agent,
            status,
            stale_reason: None,
            sources: vec![source(Uuid::now_v7()), source(Uuid::now_v7())],
            valid_from: now,
            valid_to: None,
            superseded_by: None,
            pending_supersede_of: None,
            written_by: "agent".into(),
            recorded_at: now,
        }
    }

    fn env(url: &str) -> impl Fn(&str) -> Option<String> {
        let url = url.to_owned();
        move |name| match name {
            "MEMORYOS_URL" => Some(url.clone()),
            "MEMORYOS_API_KEY" => Some("mos_agent".to_owned()),
            "MEMORYOS_HOOK_SCOPES" => Some("agent:a,shared:t".to_owned()),
            _ => None,
        }
    }

    #[test]
    fn list_defaults_to_the_hook_scopes_and_prints_one_line_each() {
        let listed = insight(InsightStatus::Active);
        let reply = json!({"insights": [listed.clone()]}).to_string();
        let server = Server::start(vec![
            (200, reply),
            (200, json!({"insights": []}).to_string()),
        ]);
        let env = env(&server.url);
        let list = |status| InsightCommand::List {
            scopes: vec![],
            level: None,
            status,
            limit: None,
            json: false,
        };

        let text = execute(list(None), &env).expect("list");
        assert_eq!(
            text,
            format!(
                "{}  active  understanding  agent:a  Ada values simple deploys",
                listed.id
            )
        );
        let empty = execute(list(Some(InsightStatus::Stale)), &env).expect("list");
        assert_eq!(empty, "no insights");

        let received = server.finish();
        assert_eq!(received[0].method, "GET");
        assert_eq!(
            received[0].target,
            "/v1/insights?scopes=agent%3Aa%2Cshared%3At"
        );
        assert!(
            received[1].target.ends_with("&status=stale"),
            "{}",
            received[1].target
        );
    }

    #[test]
    fn read_shows_what_changed_and_every_source() {
        let mut stale = insight(InsightStatus::Stale);
        let (old, new) = (stale.sources[0].id, Uuid::now_v7());
        stale.stale_reason = Some(vec![StaleSource {
            source: stale.sources[0],
            change: SourceChange::Superseded { by: new },
        }]);
        let detail = InsightDetail {
            source_views: vec![
                InsightSourceView {
                    source: stale.sources[0],
                    statement: Some("Ada deploys by hand".into()),
                    current: false,
                },
                InsightSourceView {
                    source: stale.sources[1],
                    statement: None,
                    current: false,
                },
            ],
            history: vec![insight(InsightStatus::Superseded)],
            insight: stale.clone(),
        };
        let server = Server::start(vec![(200, serde_json::to_string(&detail).expect("json"))]);

        let text = execute(
            InsightCommand::Read {
                id: stale.id,
                json: false,
            },
            &env(&server.url),
        )
        .expect("read");

        assert!(text.starts_with(&format!(
            "understanding insight {} (agent:a, stale",
            stale.id
        )));
        assert!(
            text.contains(&format!("Stale: fact {old} superseded by {new}\n")),
            "{text}"
        );
        assert!(text.contains(&format!("- fact {old} (not current): Ada deploys by hand")));
        assert!(text.contains("(not current): [purged]"), "{text}");
        assert!(text.contains("Replaced:\n- "), "{text}");
        assert_eq!(
            server.finish()[0].target,
            format!("/v1/insights/{}", stale.id)
        );
    }

    #[test]
    fn retire_sends_the_reason_and_prints_the_new_status() {
        let retired = insight(InsightStatus::Retired);
        let server = Server::start(vec![(200, serde_json::to_string(&retired).expect("json"))]);
        let text = execute(
            InsightCommand::Retire {
                id: retired.id,
                reason: "Ada automated deploys".into(),
                json: true,
            },
            &env(&server.url),
        )
        .expect("retire");
        let value: Value = serde_json::from_str(&text).expect("json");
        assert_eq!(value["status"], "retired");
        let received = server.finish();
        assert_eq!(received[0].method, "POST");
        assert_eq!(
            received[0].target,
            format!("/v1/insights/{}/retire", retired.id)
        );
        let body: Value = serde_json::from_str(&received[0].body).expect("json");
        assert_eq!(body, json!({"reason": "Ada automated deploys"}));
    }

    #[test]
    fn an_error_names_the_code() {
        let missing = json!({"error": {"code": "not_found", "message": "no such insight"}});
        let server = Server::start(vec![(404, missing.to_string())]);
        let error = execute(
            InsightCommand::Read {
                id: Uuid::nil(),
                json: false,
            },
            &env(&server.url),
        )
        .expect_err("missing");
        assert!(error.contains("no such insight"), "{error}");
        server.finish();
    }
}
