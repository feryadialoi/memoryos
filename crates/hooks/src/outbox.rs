//! The outbox: `commit_facts` calls the server could not take, kept on this machine and sent again
//! later with the same idempotency key. A call that did reach the server before its answer was lost
//! therefore saves nothing twice. `memoryos mcp` queues a call when the server cannot be reached or
//! fails on its side, and sends the queue again once the server answers. `memoryos outbox` shows
//! the queue and sends it on request.
//!
//! Each call is one file named by a hash of its key, written whole (temporary file and rename) and
//! readable by the owner only. Several proxies may send the same file at once: the key makes the
//! second send a no-op on the server, and a file already removed is not an error.

use std::fs::{self, Permissions};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::config::{self, process_env};
use crate::files::write_atomic;

pub(crate) const TOOL: &str = "commit_facts";
const KEY_FIELD: &str = "idempotency_key";
const KEY_PREFIX: &str = "memoryos-mcp-";
const FAILED_DIR: &str = "failed";
/// Failures the server reports on its side before a call moves to `failed`. Sends that never
/// reached the server are not counted, so an outage of any length loses nothing.
pub(crate) const MAX_ATTEMPTS: u32 = 20;
/// Tool error codes of a server that is up but could not finish: the store, the embedder, a
/// deadline or the rate limit. Any other code is about the call itself, and resending cannot help.
const SERVER_FAULTS: &[&str] = &["internal", "provider_error", "timeout", "rate_limited"];
/// One send while flushing. Shorter than the proxy's own limit, so a slow server stalls the next
/// message of the agent for less.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// One queued call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Entry {
    /// The `commit_facts` arguments as sent, scope and idempotency key included.
    pub arguments: Value,
    pub queued_at: DateTime<Utc>,
    /// Failures the server reported on its side.
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub last_error: Option<String>,
}

/// What became of one `commit_facts` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    Saved,
    /// No answer, or an HTTP error from something in front of the server.
    Unreachable(String),
    /// The server answered that it failed on its side.
    Faulted(String),
    /// The server refused the call itself.
    Refused(String),
}

impl Outcome {
    /// Whether the call should wait in the outbox rather than reach the model as an error.
    pub(crate) fn should_queue(&self) -> bool {
        matches!(self, Self::Unreachable(_) | Self::Faulted(_))
    }
}

/// Reads the HTTP answer to a `commit_facts` call.
pub(crate) fn classify(status: u16, body: &str) -> Outcome {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        if value.get("jsonrpc").is_some() {
            if let Some(error) = value.get("error") {
                let message = error["message"].as_str().unwrap_or("JSON-RPC error");
                return Outcome::Refused(message.to_owned());
            }
            let result = &value["result"];
            if result["isError"] != json!(true) {
                return Outcome::Saved;
            }
            let text = result["content"][0]["text"].as_str().unwrap_or_default();
            let parsed = serde_json::from_str::<Value>(text).unwrap_or(Value::Null);
            let code = parsed["error"]["code"].as_str().unwrap_or_default();
            let message = parsed["error"]["message"].as_str().unwrap_or(text);
            let detail = format!("{code}: {message}");
            return if SERVER_FAULTS.contains(&code) {
                Outcome::Faulted(detail)
            } else {
                Outcome::Refused(detail)
            };
        }
    }
    match status {
        401 | 403 => Outcome::Refused(format!("HTTP {status}: the key was refused")),
        429 | 500.. => Outcome::Unreachable(format!("HTTP {status}")),
        _ => Outcome::Faulted(format!("HTTP {status} without a JSON-RPC body")),
    }
}

/// For a `tools/call` of `commit_facts` with object arguments, the call's idempotency key, after
/// adding one when it had none. `None` for any other message, and for a key that is not a string,
/// which the server will refuse anyway.
pub(crate) fn ensure_key(message: &mut Value) -> Option<String> {
    if message["method"] != "tools/call" || message["params"]["name"] != TOOL {
        return None;
    }
    let arguments = message
        .get_mut("params")?
        .get_mut("arguments")?
        .as_object_mut()?;
    match arguments.get(KEY_FIELD) {
        Some(Value::String(key)) => Some(key.clone()),
        None | Some(Value::Null) => {
            let key = format!("{KEY_PREFIX}{}", uuid::Uuid::now_v7());
            arguments.insert(KEY_FIELD.to_owned(), Value::String(key.clone()));
            Some(key)
        }
        Some(_) => None,
    }
}

/// The reply to the model for a queued call: a result, not an error, so it does not retry.
pub(crate) fn queued_reply(id: &Value, key: &str) -> String {
    let text = json!({
        "queued": true,
        "idempotency_key": key,
        "message": "The MemoryOS server could not take this call, so it was queued on this machine \
                    and will be saved when the server is back. Do not retry it.",
    });
    json!({"jsonrpc": "2.0", "id": id, "result": {
        "content": [{"type": "text", "text": text.to_string()}],
        "isError": false,
    }})
    .to_string()
}

fn file_name(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{}.json", &hex[..32])
}

fn private_file() -> Option<Permissions> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Some(Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        None
    }
}

fn make_private_dir(dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, Permissions::from_mode(0o700))
            .map_err(|e| format!("cannot restrict {}: {e}", dir.display()))?;
    }
    Ok(())
}

fn write_entry(path: &Path, entry: &Entry) -> Result<(), String> {
    let text = serde_json::to_vec_pretty(entry).map_err(|e| format!("cannot encode: {e}"))?;
    write_atomic(path, &text, private_file())
}

/// Queues a call. A call already queued under the same key is kept as it is.
pub(crate) fn enqueue(
    dir: &Path,
    arguments: &Value,
    key: &str,
    now: DateTime<Utc>,
) -> Result<PathBuf, String> {
    make_private_dir(dir)?;
    let path = dir.join(file_name(key));
    if path.exists() {
        return Ok(path);
    }
    let entry = Entry {
        arguments: arguments.clone(),
        queued_at: now,
        attempts: 0,
        last_error: None,
    };
    write_entry(&path, &entry)?;
    Ok(path)
}

/// The queued calls of `dir`, oldest first. A file that cannot be read is skipped and left alone.
pub(crate) fn pending(dir: &Path) -> Vec<(PathBuf, Entry)> {
    let Ok(read) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries: Vec<(PathBuf, Entry)> = read
        .filter_map(Result::ok)
        .map(|item| item.path())
        .filter(|path| {
            path.is_file()
                && path.extension().is_some_and(|ext| ext == "json")
                && !path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with('.'))
        })
        .filter_map(|path| {
            let text = fs::read(&path).ok()?;
            let entry = serde_json::from_slice::<Entry>(&text).ok()?;
            Some((path, entry))
        })
        .collect();
    entries.sort_by(|a, b| a.1.queued_at.cmp(&b.1.queued_at).then(a.0.cmp(&b.0)));
    entries
}

/// Records what a send did: removes a saved call, moves a refused one, or one that failed on the
/// server [`MAX_ATTEMPTS`] times, to `failed`, and leaves one that never reached the server.
pub(crate) fn settle(
    dir: &Path,
    path: &Path,
    entry: &Entry,
    outcome: &Outcome,
) -> Result<(), String> {
    let remove = |path: &Path| match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot remove {}: {error}", path.display())),
    };
    let fail = |entry: &Entry| -> Result<(), String> {
        let failed = dir.join(FAILED_DIR);
        make_private_dir(&failed)?;
        let name = path.file_name().ok_or("an outbox file has no name")?;
        write_entry(&failed.join(name), entry)?;
        remove(path)
    };
    match outcome {
        Outcome::Saved => remove(path),
        Outcome::Unreachable(_) => Ok(()),
        Outcome::Refused(message) => fail(&Entry {
            last_error: Some(message.clone()),
            ..entry.clone()
        }),
        Outcome::Faulted(message) => {
            let next = Entry {
                attempts: entry.attempts + 1,
                last_error: Some(message.clone()),
                ..entry.clone()
            };
            if next.attempts >= MAX_ATTEMPTS {
                fail(&next)
            } else {
                write_entry(path, &next)
            }
        }
    }
}

/// What one flush did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Report {
    pub saved: usize,
    pub failed: usize,
    pub left: usize,
    /// Why the flush stopped early, when it did.
    pub stopped: Option<String>,
}

/// Sends the queued calls, oldest first. Stops at the first one the server could not take, since
/// the rest would fare no better, and leaves them for the next flush.
pub(crate) async fn flush<F, Fut>(dir: &Path, mut send: F) -> Report
where
    F: FnMut(Value) -> Fut,
    Fut: Future<Output = Outcome>,
{
    let queue = pending(dir);
    let mut report = Report {
        left: queue.len(),
        ..Report::default()
    };
    for (path, entry) in queue {
        let outcome = send(entry.arguments.clone()).await;
        if let Err(error) = settle(dir, &path, &entry, &outcome) {
            report.stopped = Some(error);
            return report;
        }
        match &outcome {
            Outcome::Saved => {
                report.saved += 1;
                report.left -= 1;
            }
            Outcome::Refused(_) => {
                report.failed += 1;
                report.left -= 1;
            }
            Outcome::Faulted(message) | Outcome::Unreachable(message) => {
                if matches!(outcome, Outcome::Faulted(_)) && entry.attempts + 1 >= MAX_ATTEMPTS {
                    report.failed += 1;
                    report.left -= 1;
                }
                report.stopped = Some(message.clone());
                return report;
            }
        }
    }
    report
}

/// Sends one queued call to the server's `/mcp` as a plain `tools/call`. The legacy form needs no
/// handshake and no headers, so a queued call does not depend on the session that queued it.
pub(crate) async fn send(
    http: &reqwest::Client,
    endpoint: &str,
    key: &str,
    arguments: Value,
) -> Outcome {
    let body = json!({"jsonrpc": "2.0", "id": "memoryos-outbox", "method": "tools/call",
        "params": {"name": TOOL, "arguments": arguments}});
    let response = http
        .post(endpoint)
        .bearer_auth(key)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .timeout(SEND_TIMEOUT)
        .body(body.to_string())
        .send()
        .await;
    match response {
        Ok(response) => {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            classify(status, &body)
        }
        Err(error) if error.is_timeout() => Outcome::Unreachable("no answer in time".to_owned()),
        Err(_) => Outcome::Unreachable("the server cannot be reached".to_owned()),
    }
}

/// `memoryos outbox`: what is queued and what failed, or with `flush`, sends the queue now.
pub fn run(flush_now: bool, as_json: bool) -> Result<(), String> {
    let dir = config::outbox_dir(&process_env)?;
    let report = if flush_now {
        let key = config::agent_key(&process_env)?;
        let endpoint = format!("{}/mcp", config::base_url(&process_env)?);
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|e| format!("cannot start the HTTP client: {e}"))?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("cannot start the runtime: {e}"))?;
        Some(runtime.block_on(flush(&dir, |arguments| {
            send(&http, &endpoint, &key, arguments)
        })))
    } else {
        None
    };
    let queued = pending(&dir);
    let failed = pending(&dir.join(FAILED_DIR));
    if as_json {
        let list = |entries: &[(PathBuf, Entry)]| -> Vec<Value> {
            entries
                .iter()
                .map(|(path, entry)| summary(path, entry))
                .collect()
        };
        let mut out = Map::new();
        out.insert("dir".into(), json!(dir.display().to_string()));
        out.insert("queued".into(), json!(list(&queued)));
        out.insert("failed".into(), json!(list(&failed)));
        if let Some(report) = &report {
            out.insert(
                "flush".into(),
                json!({"saved": report.saved, "failed": report.failed, "left": report.left,
                       "stopped": report.stopped}),
            );
        }
        println!("{}", Value::Object(out));
        return Ok(());
    }
    if let Some(report) = &report {
        let mut line = format!(
            "Sent: {} saved, {} moved to failed, {} still queued.",
            report.saved, report.failed, report.left
        );
        if let Some(reason) = &report.stopped {
            line.push_str(&format!(" Stopped: {reason}."));
        }
        println!("{line}");
    }
    println!(
        "{} queued, {} failed, in {}",
        queued.len(),
        failed.len(),
        dir.display()
    );
    for (label, entries) in [("queued", &queued), ("failed", &failed)] {
        for (path, entry) in entries {
            let item = summary(path, entry);
            println!(
                "- {label} {} {} fact(s) in {}, {} attempt(s){}",
                entry.queued_at.to_rfc3339(),
                item["facts"],
                item["scope"].as_str().unwrap_or("?"),
                entry.attempts,
                entry
                    .last_error
                    .as_deref()
                    .map(|error| format!(", last error: {error}"))
                    .unwrap_or_default()
            );
        }
    }
    Ok(())
}

/// Counts and names only, never the statements.
fn summary(path: &Path, entry: &Entry) -> Value {
    json!({
        "file": path.file_name().map(|name| name.to_string_lossy().into_owned()),
        "queued_at": entry.queued_at,
        "scope": entry.arguments["scope"],
        "facts": entry.arguments["facts"].as_array().map_or(0, Vec::len),
        "attempts": entry.attempts,
        "last_error": entry.last_error,
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    fn tool_result(is_error: bool, text: &str) -> String {
        json!({"jsonrpc": "2.0", "id": 1, "result": {
            "content": [{"type": "text", "text": text}], "isError": is_error}})
        .to_string()
    }

    fn tool_error(code: &str) -> String {
        tool_result(
            true,
            &json!({"error": {"code": code, "message": "m"}}).to_string(),
        )
    }

    #[test]
    fn answers_are_classified_by_who_failed() {
        assert_eq!(classify(200, &tool_result(false, "{}")), Outcome::Saved);
        for code in ["internal", "provider_error", "timeout", "rate_limited"] {
            assert!(
                matches!(classify(200, &tool_error(code)), Outcome::Faulted(_)),
                "{code}"
            );
        }
        for code in ["invalid_input", "forbidden", "not_found"] {
            assert!(
                matches!(classify(200, &tool_error(code)), Outcome::Refused(_)),
                "{code}"
            );
        }
        let rpc_error =
            json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32602, "message": "x"}});
        assert_eq!(
            classify(400, &rpc_error.to_string()),
            Outcome::Refused("x".into())
        );
        for status in [429, 500, 502, 503, 504] {
            assert!(
                matches!(classify(status, "<html>"), Outcome::Unreachable(_)),
                "{status}"
            );
        }
        assert!(matches!(classify(401, ""), Outcome::Refused(_)));
        assert!(matches!(classify(404, "nope"), Outcome::Faulted(_)));
    }

    #[test]
    fn only_server_side_trouble_is_queued() {
        assert!(Outcome::Unreachable(String::new()).should_queue());
        assert!(Outcome::Faulted(String::new()).should_queue());
        assert!(!Outcome::Refused(String::new()).should_queue());
        assert!(!Outcome::Saved.should_queue());
    }

    fn commit(arguments: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
               "params": {"name": "commit_facts", "arguments": arguments}})
    }

    #[test]
    fn a_commit_without_a_key_gets_one_and_a_given_key_is_kept() {
        let mut message = commit(json!({"scope": "agent:a"}));
        let key = ensure_key(&mut message).expect("key");
        assert!(key.starts_with(KEY_PREFIX));
        assert_eq!(message["params"]["arguments"][KEY_FIELD], key);

        let mut null = commit(json!({"idempotency_key": null}));
        assert!(ensure_key(&mut null).is_some());

        let mut given = commit(json!({"idempotency_key": "mine"}));
        assert_eq!(ensure_key(&mut given).as_deref(), Some("mine"));
        assert_eq!(given["params"]["arguments"][KEY_FIELD], "mine");
    }

    #[test]
    fn other_messages_get_no_key() {
        for original in [
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                   "params": {"name": "recall", "arguments": {}}}),
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
            commit(json!("not an object")),
            commit(json!({"idempotency_key": 5})),
        ] {
            let mut message = original.clone();
            assert_eq!(ensure_key(&mut message), None, "{original}");
            assert_eq!(message, original);
        }
    }

    #[test]
    fn the_queued_reply_is_a_result_that_names_the_key() {
        let reply: Value = serde_json::from_str(&queued_reply(&json!(9), "k")).expect("JSON");
        assert_eq!(reply["id"], 9);
        assert_eq!(reply["result"]["isError"], false);
        let text: Value = serde_json::from_str(
            reply["result"]["content"][0]["text"]
                .as_str()
                .expect("text"),
        )
        .expect("JSON");
        assert_eq!(text["queued"], true);
        assert_eq!(text["idempotency_key"], "k");
    }

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    #[test]
    fn enqueue_writes_one_private_file_per_key() {
        let home = tempfile::tempdir().expect("dir");
        let dir = home.path().join("outbox");
        let first = enqueue(&dir, &json!({"n": 1}), "k", now()).expect("enqueue");
        let again = enqueue(&dir, &json!({"n": 2}), "k", now()).expect("enqueue");
        assert_eq!(first, again);
        let queued = pending(&dir);
        assert_eq!(queued.len(), 1);
        assert_eq!(
            queued[0].1.arguments,
            json!({"n": 1}),
            "the first copy is kept"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| fs::metadata(path).expect("meta").permissions().mode() & 0o777;
            assert_eq!(mode(&first), 0o600);
            assert_eq!(mode(&dir), 0o700);
        }
    }

    #[test]
    fn pending_is_oldest_first_and_skips_what_it_cannot_read() {
        let home = tempfile::tempdir().expect("dir");
        let dir = home.path();
        let early = Utc::now() - chrono::Duration::minutes(5);
        enqueue(dir, &json!({"n": "late"}), "b", now()).expect("enqueue");
        enqueue(dir, &json!({"n": "early"}), "a", early).expect("enqueue");
        fs::write(dir.join("junk.json"), "not json").expect("write");
        fs::write(dir.join(".partial.json.1.0.tmp"), "{}").expect("write");
        let order: Vec<Value> = pending(dir)
            .into_iter()
            .map(|(_, e)| e.arguments["n"].clone())
            .collect();
        assert_eq!(order, [json!("early"), json!("late")]);
        assert!(pending(&dir.join("absent")).is_empty());
    }

    #[test]
    fn settle_removes_moves_or_counts() {
        let home = tempfile::tempdir().expect("dir");
        let dir = home.path();
        let path = enqueue(dir, &json!({}), "saved", now()).expect("enqueue");
        let entry = pending(dir).remove(0).1;
        settle(dir, &path, &entry, &Outcome::Saved).expect("settle");
        assert!(!path.exists());
        // Another proxy may have removed it first.
        settle(dir, &path, &entry, &Outcome::Saved).expect("settle again");

        let path = enqueue(dir, &json!({}), "down", now()).expect("enqueue");
        settle(dir, &path, &entry, &Outcome::Unreachable("x".into())).expect("settle");
        assert_eq!(pending(dir)[0].1.attempts, 0, "an outage is not counted");

        let entry = pending(dir).remove(0).1;
        settle(dir, &path, &entry, &Outcome::Faulted("internal: m".into())).expect("settle");
        let counted = pending(dir).remove(0).1;
        assert_eq!(counted.attempts, 1);
        assert_eq!(counted.last_error.as_deref(), Some("internal: m"));

        let worn = Entry {
            attempts: MAX_ATTEMPTS - 1,
            ..counted
        };
        settle(dir, &path, &worn, &Outcome::Faulted("internal: m".into())).expect("settle");
        assert!(pending(dir).is_empty());
        assert_eq!(pending(&dir.join(FAILED_DIR))[0].1.attempts, MAX_ATTEMPTS);

        let path = enqueue(dir, &json!({}), "refused", now()).expect("enqueue");
        let entry = pending(dir).remove(0).1;
        settle(
            dir,
            &path,
            &entry,
            &Outcome::Refused("invalid_input: m".into()),
        )
        .expect("settle");
        assert!(pending(dir).is_empty());
        assert_eq!(pending(&dir.join(FAILED_DIR)).len(), 2);
    }

    fn block_on<T>(future: impl Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(future)
    }

    #[test]
    fn flush_sends_in_order_and_stops_when_the_server_cannot_take_more() {
        let home = tempfile::tempdir().expect("dir");
        let dir = home.path();
        let base = Utc::now() - chrono::Duration::minutes(10);
        for n in 0..4 {
            enqueue(
                dir,
                &json!({"n": n}),
                &format!("k{n}"),
                base + chrono::Duration::seconds(n),
            )
            .expect("enqueue");
        }
        let sent = RefCell::new(Vec::new());
        let answers = RefCell::new(vec![
            Outcome::Saved,
            Outcome::Refused("invalid_input: m".into()),
            Outcome::Unreachable("down".into()),
        ]);
        let report = block_on(flush(dir, |arguments| {
            sent.borrow_mut().push(arguments["n"].clone());
            let outcome = answers.borrow_mut().remove(0);
            async move { outcome }
        }));
        assert_eq!(*sent.borrow(), [json!(0), json!(1), json!(2)]);
        assert_eq!(
            report,
            Report {
                saved: 1,
                failed: 1,
                left: 2,
                stopped: Some("down".into())
            }
        );
        let left: Vec<Value> = pending(dir)
            .into_iter()
            .map(|(_, e)| e.arguments["n"].clone())
            .collect();
        assert_eq!(left, [json!(2), json!(3)]);
    }

    #[test]
    fn an_empty_outbox_sends_nothing() {
        let home = tempfile::tempdir().expect("dir");
        let report = block_on(flush(home.path(), |_| async { panic!("nothing to send") }));
        assert_eq!(report, Report::default());
    }
}
