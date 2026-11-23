//! `memoryos mcp`: a stdio MCP server for the agent, forwarding each JSON-RPC line to the server's
//! `/mcp` with the agent key. The agent's configuration therefore holds no key and no reference to
//! one. The admin key is never used here.
//!
//! A `commit_facts` call the server cannot take goes to the [`outbox`] and is sent again after the
//! server next answers, so facts saved during an outage are not lost.

use std::io::{BufRead, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use url::Url;

use crate::config::{self, process_env};
use crate::outbox;
use crate::settings;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the check for a running local server waits on a loopback port.
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);
/// Longer than any tool call should take, so a hung server cannot stall the agent forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// The least time between two sends of the outbox, so a queue the server keeps failing on does not
/// slow every message.
const FLUSH_EVERY: Duration = Duration::from_secs(30);
const INTERNAL_ERROR: i64 = -32603;
const PARSE_ERROR: i64 = -32700;

const META: &str = "_meta";
const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";

/// Tools whose scope argument the proxy fills from `MEMORYOS_HOOK_SCOPES` when the model omits it,
/// with the field's name and whether it takes every scope or one.
const SCOPED_TOOLS: &[(&str, &str, Takes)] = &[
    ("recall", "scopes", Takes::All),
    ("recall_candidates", "scopes", Takes::All),
    ("graph_neighbors", "scopes", Takes::All),
    ("commit_facts", "scope", Takes::First),
    ("page_read", "scope", Takes::First),
    ("page_write", "scope", Takes::First),
    ("page_list", "scope", Takes::First),
    ("reflect_batch", "scope", Takes::First),
    ("insight_write", "scope", Takes::First),
    ("reflect_decline", "scope", Takes::First),
];

#[derive(Debug, Clone, Copy)]
enum Takes {
    All,
    First,
}

pub fn run() -> Result<(), String> {
    let key = config::agent_key(&process_env)?;
    let base = config::base_url(&process_env)?;
    start_local_server(&base);
    let endpoint = format!("{base}/mcp");
    // A bad value must not take the tools away: the model then has to name scopes itself.
    // Claude Code starts this in the project directory, so its project file applies.
    let cwd = std::env::current_dir().ok();
    let defaults: Vec<String> = match config::project_scopes(&process_env, cwd.as_deref()) {
        Ok(scopes) => scopes.iter().map(ToString::to_string).collect(),
        Err(message) => {
            eprintln!("memoryos mcp: {message}. Scopes are not filled in.");
            Vec::new()
        }
    };
    let http = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| format!("cannot start the HTTP client: {e}"))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the runtime: {e}"))?;
    // Without it, calls are forwarded as before and nothing is queued.
    let outbox_dir = match config::outbox_dir(&process_env) {
        Ok(dir) => Some(dir),
        Err(message) => {
            eprintln!("memoryos mcp: {message}. Failed saves are not queued.");
            None
        }
    };
    let mut last_flush: Option<Instant> = None;

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| format!("cannot read stdin: {e}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let forwarded = runtime.block_on(forward(
            &http,
            &endpoint,
            &key,
            &line,
            &defaults,
            outbox_dir.as_deref(),
        ));
        if let Some(reply) = forwarded.reply {
            // The host closed the pipe, so there is nobody left to serve.
            if writeln!(stdout, "{reply}")
                .and_then(|()| stdout.flush())
                .is_err()
            {
                return Ok(());
            }
        }
        // After the reply, so the agent never waits on the queue for its own call.
        if let Some(dir) = outbox_dir.as_deref() {
            if forwarded.server_answered && last_flush.is_none_or(|at| at.elapsed() >= FLUSH_EVERY)
            {
                last_flush = Some(Instant::now());
                flush_outbox(&runtime, &http, &endpoint, &key, dir);
            }
        }
    }
    Ok(())
}

fn flush_outbox(
    runtime: &tokio::runtime::Runtime,
    http: &reqwest::Client,
    endpoint: &str,
    key: &str,
    dir: &Path,
) {
    if outbox::pending(dir).is_empty() {
        return;
    }
    let report = runtime.block_on(outbox::flush(dir, |arguments| {
        outbox::send(http, endpoint, key, arguments)
    }));
    if report.saved > 0 || report.failed > 0 {
        eprintln!(
            "memoryos mcp: sent queued saves: {} saved, {} moved to {}, {} still queued",
            report.saved,
            report.failed,
            dir.join("failed").display(),
            report.left
        );
    }
}

/// What one forwarded line produced.
struct Forwarded {
    reply: Option<String>,
    /// The server answered and did not fail on its side, so the outbox may be sent.
    server_answered: bool,
}

/// Queues a `commit_facts` call and answers the model that it was queued. `None` when the call
/// cannot be queued, and the caller then sends the error it had.
fn queue(dir: Option<&Path>, id: Option<&Value>, message: &Value, key: &str) -> Option<String> {
    let dir = dir?;
    let id = id?;
    match outbox::enqueue(
        dir,
        &message["params"]["arguments"],
        key,
        chrono::Utc::now(),
    ) {
        Ok(_) => Some(outbox::queued_reply(id, key)),
        Err(error) => {
            eprintln!("memoryos mcp: cannot queue a save: {error}");
            None
        }
    }
}

/// After a reboot nothing runs the server of a local install, so the proxy the agent starts does.
/// Only when `server.env` exists and the server URL is on this machine with nothing listening.
/// `memoryos start` runs in the background, so calls fail until the server is ready instead of the
/// agent waiting on a model download.
fn start_local_server(base: &str) {
    if !settings::path().is_some_and(|path| path.is_file()) || !needs_start(base) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut command = Command::new(exe);
    command
        .arg("start")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    match command.spawn() {
        // Reaped on its own thread, so it leaves no zombie while the proxy runs.
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(error) => eprintln!("memoryos mcp: cannot start the local server: {error}"),
    }
}

/// A loopback URL whose port takes no connection.
fn needs_start(base: &str) -> bool {
    let Ok(url) = Url::parse(base) else {
        return false;
    };
    let loopback = match url.host() {
        Some(url::Host::Domain(name)) => name == "localhost",
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    let (Some(host), Some(port)) = (url.host_str(), url.port_or_known_default()) else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let Ok(addresses) = (host, port).to_socket_addrs() else {
        return false;
    };
    loopback
        && !addresses
            .into_iter()
            .any(|address| TcpStream::connect_timeout(&address, PROBE_TIMEOUT).is_ok())
}

/// What to write back for one line, if anything.
async fn forward(
    http: &reqwest::Client,
    endpoint: &str,
    key: &str,
    line: &str,
    defaults: &[String],
    outbox_dir: Option<&Path>,
) -> Forwarded {
    let Ok(mut message) = serde_json::from_str::<Value>(line) else {
        return Forwarded {
            reply: Some(error_reply(&Value::Null, PARSE_ERROR, "invalid JSON")),
            server_answered: false,
        };
    };
    let id = message.get("id").cloned();
    let filled = fill_scopes(&mut message, defaults);
    // Before sending: a call that reached the server before its answer was lost is then saved once
    // when the queued copy is sent again.
    let commit_key = outbox::ensure_key(&mut message);
    let body = if filled || commit_key.is_some() {
        message.to_string()
    } else {
        line.to_owned()
    };
    let lists_tools = message["method"] == "tools/list";
    let mut request = http
        .post(endpoint)
        .bearer_auth(key)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(body);
    for (name, value) in modern_headers(&message) {
        request = request.header(name, value);
    }
    match request.send().await {
        Ok(response) => {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            if let Some(commit_key) = &commit_key {
                if outbox::classify(status, &body).should_queue() {
                    if let Some(reply) = queue(outbox_dir, id.as_ref(), &message, commit_key) {
                        return Forwarded {
                            reply: Some(reply),
                            server_answered: false,
                        };
                    }
                }
            }
            let reply = reply_for(id.as_ref(), status, &body, endpoint);
            let reply = if lists_tools {
                reply.map(|reply| describe_defaults(reply, defaults))
            } else {
                reply
            };
            Forwarded {
                reply,
                server_answered: status < 500,
            }
        }
        Err(error) => {
            if let Some(commit_key) = &commit_key {
                if let Some(reply) = queue(outbox_dir, id.as_ref(), &message, commit_key) {
                    return Forwarded {
                        reply: Some(reply),
                        server_answered: false,
                    };
                }
            }
            let cause = if error.is_timeout() {
                "did not answer in time"
            } else {
                "cannot be reached"
            };
            Forwarded {
                reply: failure(
                    id.as_ref(),
                    &format!("the MemoryOS server at {endpoint} {cause}. Is it running?"),
                ),
                server_answered: false,
            }
        }
    }
}

fn scoped_tool(name: &str) -> Option<(&'static str, Takes)> {
    SCOPED_TOOLS
        .iter()
        .find(|(tool, _, _)| *tool == name)
        .map(|(_, field, takes)| (*field, *takes))
}

/// Fills a missing or null scope argument of a `tools/call` with the default scopes. Never replaces
/// a value the model sent. True when it changed the message.
fn fill_scopes(message: &mut Value, defaults: &[String]) -> bool {
    let Some(first) = defaults.first() else {
        return false;
    };
    if message["method"] != "tools/call" {
        return false;
    }
    let Some(params) = message.get_mut("params").and_then(Value::as_object_mut) else {
        return false;
    };
    let Some((field, takes)) = params
        .get("name")
        .and_then(Value::as_str)
        .and_then(scoped_tool)
    else {
        return false;
    };
    let arguments = params
        .entry("arguments")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if arguments.is_null() {
        *arguments = Value::Object(serde_json::Map::new());
    }
    let Some(arguments) = arguments.as_object_mut() else {
        return false;
    };
    if arguments.get(field).is_some_and(|value| !value.is_null()) {
        return false;
    }
    let value = match takes {
        Takes::All => json!(defaults),
        Takes::First => json!(first),
    };
    arguments.insert(field.to_owned(), value);
    true
}

/// In a `tools/list` reply, makes the scope argument of [`SCOPED_TOOLS`] optional and says what an
/// omitted one becomes. Anything that does not have the expected shape passes through unchanged.
fn describe_defaults(reply: String, defaults: &[String]) -> String {
    if defaults.is_empty() {
        return reply;
    }
    let Ok(mut value) = serde_json::from_str::<Value>(&reply) else {
        return reply;
    };
    // `get_mut`, not indexing, which would panic on an unexpected shape or insert keys.
    let Some(tools) = value
        .get_mut("result")
        .and_then(|result| result.get_mut("tools"))
        .and_then(Value::as_array_mut)
    else {
        return reply;
    };
    let note = format!(
        "Optional: omit to use your default scopes ({}).",
        defaults.join(", ")
    );
    for tool in tools {
        let Some((field, _)) = tool["name"].as_str().and_then(scoped_tool) else {
            continue;
        };
        let Some(schema) = tool.get_mut("inputSchema").and_then(Value::as_object_mut) else {
            continue;
        };
        if let Some(required) = schema.get_mut("required").and_then(Value::as_array_mut) {
            required.retain(|name| name != field);
        }
        let property = schema
            .get_mut("properties")
            .and_then(|properties| properties.get_mut(field))
            .and_then(Value::as_object_mut);
        if let Some(property) = property {
            let text = match property.get("description").and_then(Value::as_str) {
                Some(existing) if !existing.is_empty() => format!("{existing} {note}"),
                _ => note.clone(),
            };
            property.insert("description".to_owned(), Value::String(text));
        }
    }
    value.to_string()
}

/// Revision 2026-07-28 mirrors the method and name in headers, and the server checks them. Older
/// revisions, which send no version in `_meta`, take none.
fn modern_headers(message: &Value) -> Vec<(&'static str, String)> {
    let Some(version) = message["params"][META][META_VERSION].as_str() else {
        return Vec::new();
    };
    let mut headers = vec![("mcp-protocol-version", version.to_owned())];
    if let Some(method) = message["method"].as_str() {
        headers.push(("mcp-method", method.to_owned()));
    }
    let params = &message["params"];
    if let Some(name) = params["name"].as_str().or(params["uri"].as_str()) {
        headers.push(("mcp-name", name.to_owned()));
    }
    headers
}

/// A JSON-RPC body passes through whatever its HTTP status. Anything else on a request becomes a
/// JSON-RPC error, because the host waits for an answer to every id. A notification gets no answer.
fn reply_for(id: Option<&Value>, status: u16, body: &str, endpoint: &str) -> Option<String> {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        if value.get("jsonrpc").is_some() {
            return Some(value.to_string());
        }
    }
    if (200..300).contains(&status) && (id.is_none() || body.trim().is_empty() && status == 202) {
        return None;
    }
    let hint = if status == 401 {
        ". The key was refused: run `memoryos setup`"
    } else {
        ""
    };
    failure(
        id,
        &format!("{endpoint} answered HTTP {status} without a JSON-RPC body{hint}"),
    )
}

fn failure(id: Option<&Value>, message: &str) -> Option<String> {
    match id {
        Some(id) => Some(error_reply(id, INTERNAL_ERROR, message)),
        None => {
            eprintln!("memoryos mcp: {message}");
            None
        }
    }
}

fn error_reply(id: &Value, code: i64, message: &str) -> String {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const AT: &str = "http://h/mcp";

    #[test]
    fn only_a_loopback_url_with_nothing_listening_needs_a_start() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let busy = listener.local_addr().expect("address").port();
        let free = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            probe.local_addr().expect("address").port()
        };
        assert!(!needs_start(&format!("http://127.0.0.1:{busy}")));
        assert!(needs_start(&format!("http://127.0.0.1:{free}")));
        assert!(needs_start(&format!("http://localhost:{free}")));
        assert!(!needs_start("https://memory.example.com"));
        assert!(!needs_start("not a url"));
    }

    fn parsed(reply: Option<String>) -> Value {
        serde_json::from_str(&reply.expect("a reply")).expect("JSON")
    }

    #[test]
    fn a_jsonrpc_body_passes_through_on_one_line_at_any_status() {
        let body =
            "{\n\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32602,\"message\":\"x\"}\n}";
        let reply = reply_for(Some(&json!(1)), 400, body, AT).expect("reply");
        assert!(!reply.contains('\n'), "{reply}");
        assert_eq!(parsed(Some(reply))["error"]["code"], -32602);
    }

    #[test]
    fn an_accepted_notification_gets_no_reply() {
        assert_eq!(reply_for(None, 202, "", AT), None);
    }

    #[test]
    fn a_request_always_gets_an_answer() {
        let refused = parsed(reply_for(Some(&json!(7)), 401, "{}", AT));
        assert_eq!(refused["id"], 7);
        assert!(refused["error"]["message"]
            .as_str()
            .expect("message")
            .contains("memoryos setup"));
        let empty = parsed(reply_for(Some(&json!("a")), 200, "", AT));
        assert_eq!(empty["id"], "a");
    }

    #[test]
    fn a_failed_notification_is_not_answered() {
        assert_eq!(reply_for(None, 500, "oops", AT), None);
    }

    fn defaults() -> Vec<String> {
        vec!["agent:coding-agent".to_owned(), "user:ada".to_owned()]
    }

    fn call(name: &str, arguments: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
            "name": name, "arguments": arguments,
        }})
    }

    #[test]
    fn missing_scopes_are_filled_with_every_default() {
        for name in ["recall", "recall_candidates", "graph_neighbors"] {
            let mut message = call(name, json!({"query": "q"}));
            assert!(fill_scopes(&mut message, &defaults()), "{name}");
            assert_eq!(
                message["params"]["arguments"],
                json!({"query": "q", "scopes": ["agent:coding-agent", "user:ada"]}),
                "{name}"
            );
        }
    }

    #[test]
    fn a_missing_commit_scope_takes_the_first_default() {
        let mut message = call("commit_facts", json!({"source": "chat"}));
        assert!(fill_scopes(&mut message, &defaults()));
        assert_eq!(
            message["params"]["arguments"],
            json!({"source": "chat", "scope": "agent:coding-agent"})
        );
    }

    #[test]
    fn a_missing_page_scope_takes_the_first_default() {
        for (name, arguments) in [
            ("page_read", json!({"slug": "deploy"})),
            (
                "page_write",
                json!({"slug": "deploy", "title": "T", "body": "b"}),
            ),
            ("page_list", json!({})),
            ("page_list", json!({"scope": null, "prefix": "d"})),
            ("reflect_batch", json!({})),
            (
                "insight_write",
                json!({"level": "context", "statement": "s", "sources": []}),
            ),
            (
                "reflect_decline",
                json!({"topic_key": "k", "fact_set": "f"}),
            ),
        ] {
            let mut message = call(name, arguments.clone());
            assert!(fill_scopes(&mut message, &defaults()), "{name}");
            let mut expected = arguments;
            expected["scope"] = json!("agent:coding-agent");
            assert_eq!(message["params"]["arguments"], expected, "{name}");
        }
        let mut absent = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "page_list"}});
        assert!(fill_scopes(&mut absent, &defaults()));
        assert_eq!(
            absent["params"]["arguments"],
            json!({"scope": "agent:coding-agent"})
        );
    }

    #[test]
    fn a_page_scope_the_model_sent_is_kept() {
        for name in ["page_read", "page_write", "page_list"] {
            let arguments = json!({"scope": "shared:team", "slug": "deploy"});
            let mut message = call(name, arguments.clone());
            assert!(!fill_scopes(&mut message, &defaults()), "{name}");
            assert_eq!(message["params"]["arguments"], arguments, "{name}");
        }
    }

    #[test]
    fn tools_list_makes_the_page_scopes_optional() {
        let reply = json!({"jsonrpc": "2.0", "id": 3, "result": {"tools": [
            {"name": "page_read", "inputSchema": {"type": "object",
                "properties": {"scope": {"type": "string", "description": "Page scope."},
                               "slug": {"type": "string"}},
                "required": ["scope", "slug"]}},
            {"name": "page_write", "inputSchema": {"type": "object",
                "properties": {"scope": {"type": "string"}, "slug": {"type": "string"},
                               "title": {"type": "string"}, "body": {"type": "string"}},
                "required": ["scope", "slug", "title", "body"]}},
            {"name": "page_list", "inputSchema": {"type": "object",
                "properties": {"scope": {"type": "string"}, "prefix": {"type": "string"}},
                "required": ["scope"]}},
        ]}});
        let after = describe_defaults(reply.to_string(), &defaults());
        let tools = serde_json::from_str::<Value>(&after).expect("JSON")["result"]["tools"].clone();
        let note = "Optional: omit to use your default scopes (agent:coding-agent, user:ada).";
        assert_eq!(tools[0]["inputSchema"]["required"], json!(["slug"]));
        assert_eq!(
            tools[0]["inputSchema"]["properties"]["scope"]["description"],
            format!("Page scope. {note}")
        );
        assert_eq!(
            tools[1]["inputSchema"]["required"],
            json!(["slug", "title", "body"])
        );
        assert_eq!(tools[2]["inputSchema"]["required"], json!([]));
        for tool in [&tools[1], &tools[2]] {
            assert_eq!(
                tool["inputSchema"]["properties"]["scope"]["description"],
                note
            );
        }
    }

    #[test]
    fn null_or_absent_arguments_are_filled_too() {
        let mut absent = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "recall"}});
        assert!(fill_scopes(&mut absent, &defaults()));
        assert_eq!(absent["params"]["arguments"]["scopes"], json!(defaults()));

        let mut null = call("commit_facts", json!({"scope": null}));
        assert!(fill_scopes(&mut null, &defaults()));
        assert_eq!(null["params"]["arguments"]["scope"], "agent:coding-agent");
    }

    #[test]
    fn a_value_the_model_sent_is_never_replaced() {
        for (name, arguments) in [
            ("recall", json!({"scopes": ["user:x"], "query": "q"})),
            ("recall", json!({"scopes": [], "query": "q"})),
            ("graph_neighbors", json!({"scopes": "odd"})),
            ("commit_facts", json!({"scope": "shared:team"})),
        ] {
            let mut message = call(name, arguments.clone());
            assert!(!fill_scopes(&mut message, &defaults()), "{name}");
            assert_eq!(message["params"]["arguments"], arguments);
        }
    }

    #[test]
    fn other_messages_and_tools_are_left_alone() {
        let untouched = [
            call("block_read", json!({"agent_id": "a", "label": "l"})),
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"name": "recall"}}),
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
                "name": "recall", "arguments": "not an object"}}),
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": "odd"}),
            json!([1, 2]),
        ];
        for original in untouched {
            let mut message = original.clone();
            assert!(!fill_scopes(&mut message, &defaults()), "{original}");
            assert_eq!(message, original);
        }
        let mut no_defaults = call("recall", json!({"query": "q"}));
        assert!(!fill_scopes(&mut no_defaults, &[]));
    }

    fn tools_reply() -> Value {
        json!({"jsonrpc": "2.0", "id": 2, "result": {"tools": [
            {"name": "recall", "inputSchema": {"type": "object",
                "properties": {"scopes": {"type": "array", "description": "Scopes to search."},
                               "query": {"type": "string"}},
                "required": ["scopes", "query"]}},
            {"name": "commit_facts", "inputSchema": {"type": "object",
                "properties": {"scope": {"type": "string"}, "facts": {"type": "array"}},
                "required": ["scope", "facts"]}},
            {"name": "block_read", "inputSchema": {"type": "object",
                "properties": {"agent_id": {"type": "string"}},
                "required": ["agent_id"]}},
        ]}})
    }

    #[test]
    fn tools_list_makes_the_scope_optional_and_names_the_defaults() {
        let reply = describe_defaults(tools_reply().to_string(), &defaults());
        let tools = serde_json::from_str::<Value>(&reply).expect("JSON")["result"]["tools"].clone();
        let note = "Optional: omit to use your default scopes (agent:coding-agent, user:ada).";

        assert_eq!(tools[0]["inputSchema"]["required"], json!(["query"]));
        assert_eq!(
            tools[0]["inputSchema"]["properties"]["scopes"]["description"],
            format!("Scopes to search. {note}")
        );
        assert_eq!(tools[1]["inputSchema"]["required"], json!(["facts"]));
        assert_eq!(
            tools[1]["inputSchema"]["properties"]["scope"]["description"],
            note
        );
        assert_eq!(tools[2], tools_reply()["result"]["tools"][2], "untouched");
    }

    #[test]
    fn replies_of_another_shape_pass_through_unchanged() {
        for reply in [
            "not json".to_owned(),
            json!([1]).to_string(),
            json!({"jsonrpc": "2.0", "id": 2, "error": {"code": 1, "message": "x"}}).to_string(),
            json!({"jsonrpc": "2.0", "id": 2, "result": {"tools": [
                {"name": "recall", "inputSchema": "odd"},
                {"name": "commit_facts"},
            ]}})
            .to_string(),
        ] {
            let parsed_before = serde_json::from_str::<Value>(&reply).ok();
            let after = describe_defaults(reply.clone(), &defaults());
            assert_eq!(serde_json::from_str::<Value>(&after).ok(), parsed_before);
        }
        let original = tools_reply().to_string();
        assert_eq!(describe_defaults(original.clone(), &[]), original);
    }

    #[test]
    fn headers_are_mirrored_only_for_the_modern_revision() {
        let legacy = json!({"method": "tools/list", "params": {}});
        assert!(modern_headers(&legacy).is_empty());
        let modern = json!({"method": "tools/call", "params": {
            "name": "recall",
            "_meta": {META_VERSION: "2026-07-28"},
        }});
        assert_eq!(
            modern_headers(&modern),
            [
                ("mcp-protocol-version", "2026-07-28".to_owned()),
                ("mcp-method", "tools/call".to_owned()),
                ("mcp-name", "recall".to_owned()),
            ]
        );
    }
}
