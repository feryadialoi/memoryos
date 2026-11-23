//! The calls the SDK does not make for this crate: recall with page hits and insights that falls
//! back for an older server, and the page routes of `docs/openapi.yaml`. Errors are one line that never holds the key.

use std::time::Duration;

use memoryos_sdk::{Scope, ScoredFact, ScoredInsight};
use reqwest::{Method, RequestBuilder, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use url::Url;
use uuid::Uuid;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Api {
    http: reqwest::Client,
    base: Url,
    key: String,
}

/// `POST /v1/recall` as the hooks read it.
#[derive(Debug, Default, Deserialize)]
pub struct Recall {
    pub facts: Vec<ScoredFact>,
    /// Absent from a server that predates pages.
    #[serde(default)]
    pub pages: Vec<PageHit>,
    /// Absent from a server that predates insights.
    #[serde(default)]
    pub insights: Vec<ScoredInsight>,
}

/// Recall fields an older server may refuse as unknown. Each is dropped and the request sent again.
const OPTIONAL_RECALL_FIELDS: [&str; 2] = ["page_limit", "insight_limit"];

/// The fields of a recall page hit that the hooks print. Never the snippet.
#[derive(Debug, Clone, Deserialize)]
pub struct PageHit {
    pub page_id: Uuid,
    pub scope: Scope,
    pub slug: String,
    pub title: String,
    /// The heading path, starting with the title.
    pub section: String,
}

/// The body of `PUT /v1/pages/{scope}/{slug}`.
#[derive(Debug, Clone, Serialize)]
pub struct PageWrite {
    pub title: String,
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<i64>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub approve: bool,
}

#[derive(Deserialize)]
struct Envelope {
    error: ErrorBody,
}

#[derive(Deserialize)]
struct ErrorBody {
    code: String,
    message: String,
}

impl Api {
    /// Does no I/O. `timeout` bounds each request.
    pub fn new(base: &str, key: String, timeout: Duration) -> Result<Self, String> {
        let base = Url::parse(base).map_err(|e| format!("MEMORYOS_URL is not a URL: {e}"))?;
        if base.cannot_be_a_base() {
            return Err("MEMORYOS_URL is not a base URL".to_owned());
        }
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT.min(timeout))
            .timeout(timeout)
            .build()
            .map_err(|e| format!("cannot start the HTTP client: {e}"))?;
        Ok(Self { http, base, key })
    }

    pub async fn recall(
        &self,
        scopes: &[Scope],
        query: &str,
        limit: i64,
        page_limit: i64,
        insight_limit: i64,
    ) -> Result<Recall, String> {
        if scopes.is_empty() {
            return Ok(Recall::default());
        }
        let mut body = json!({"scopes": scopes, "query": query, "limit": limit,
            "page_limit": page_limit, "insight_limit": insight_limit});
        let value = loop {
            let request = self.request(Method::POST, &["v1", "recall"]).json(&body);
            match self.send(request).await {
                // A server from before pages or insights refuses the unknown field. Facts still
                // help, so ask again without it, and show none of what it does not know.
                Err(error) if error.starts_with("invalid input") => {
                    let refused = OPTIONAL_RECALL_FIELDS.iter().find(|field| {
                        error.contains(*field)
                            && body
                                .as_object()
                                .is_some_and(|map| map.contains_key(**field))
                    });
                    match (refused, body.as_object_mut()) {
                        (Some(field), Some(map)) => {
                            map.remove(*field);
                        }
                        _ => return Err(error),
                    }
                }
                other => break other?,
            }
        };
        serde_json::from_value(value).map_err(|e| format!("unexpected recall response: {e}"))
    }

    pub async fn list_pages(&self, scope: &Scope, prefix: Option<&str>) -> Result<Value, String> {
        let scope = scope.to_string();
        let mut query = vec![("scope", scope.as_str())];
        if let Some(prefix) = prefix {
            query.push(("prefix", prefix));
        }
        let request = self.request(Method::GET, &["v1", "pages"]).query(&query);
        self.send(request).await
    }

    pub async fn read_page(
        &self,
        scope: &Scope,
        slug: &str,
        version: Option<i64>,
    ) -> Result<Value, String> {
        let scope = scope.to_string();
        let mut request = self.request(Method::GET, &["v1", "pages", &scope, slug]);
        if let Some(version) = version {
            request = request.query(&[("version", version)]);
        }
        self.send(request).await
    }

    pub async fn write_page(
        &self,
        scope: &Scope,
        slug: &str,
        page: &PageWrite,
    ) -> Result<Value, String> {
        let scope = scope.to_string();
        let request = self
            .request(Method::PUT, &["v1", "pages", &scope, slug])
            .json(page);
        self.send(request).await
    }

    pub async fn delete_page(&self, scope: &Scope, slug: &str) -> Result<(), String> {
        let scope = scope.to_string();
        let request = self.request(Method::DELETE, &["v1", "pages", &scope, slug]);
        self.send_raw(request).await.map(drop)
    }

    /// Each segment is percent-encoded, so a slug cannot change the route.
    fn request(&self, method: Method, segments: &[&str]) -> RequestBuilder {
        let mut url = self.base.clone();
        if let Ok(mut path) = url.path_segments_mut() {
            path.pop_if_empty().extend(segments);
        }
        self.http.request(method, url).bearer_auth(&self.key)
    }

    async fn send(&self, request: RequestBuilder) -> Result<Value, String> {
        let (status, bytes) = self.send_raw(request).await?;
        serde_json::from_slice(&bytes)
            .map_err(|_| format!("unexpected response (HTTP {}): not JSON", status.as_u16()))
    }

    async fn send_raw(&self, request: RequestBuilder) -> Result<(StatusCode, Vec<u8>), String> {
        let response = request.send().await.map_err(|error| describe(&error))?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(|error| describe(&error))?;
        if status.is_success() {
            return Ok((status, bytes.to_vec()));
        }
        Err(error_message(status, &bytes))
    }
}

fn describe(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "the server did not answer in time. Check MEMORYOS_URL".to_owned()
    } else {
        "cannot reach the server. Is it running at MEMORYOS_URL?".to_owned()
    }
}

/// `not found: message` for a MemoryOS error. Another body, as from a proxy, is not quoted.
fn error_message(status: StatusCode, body: &[u8]) -> String {
    match serde_json::from_slice::<Envelope>(body) {
        Ok(Envelope { error }) => format!("{}: {}", error.code.replace('_', " "), error.message),
        Err(_) => format!(
            "unexpected response (HTTP {}): the body is not a MemoryOS error",
            status.as_u16()
        ),
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! A one-thread HTTP server that answers each connection with the next canned reply and keeps
    //! what it received.

    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;

    #[derive(Debug, Clone)]
    pub struct Received {
        pub method: String,
        /// Path and query, as sent.
        pub target: String,
        pub authorization: Option<String>,
        pub body: String,
    }

    pub struct Server {
        pub url: String,
        received: Arc<Mutex<Vec<Received>>>,
        thread: Option<JoinHandle<()>>,
    }

    impl Server {
        /// Serves `replies` in order, one per connection, then stops.
        pub fn start(replies: Vec<(u16, String)>) -> Self {
            let count = replies.len();
            let queue = Mutex::new(replies.into_iter());
            Self::routed(count, move |_| {
                queue.lock().expect("lock").next().expect("a reply")
            })
        }

        /// Serves `count` connections, answering each with what `respond` makes of the request.
        /// For requests sent concurrently, whose order is not known.
        pub fn routed(
            count: usize,
            respond: impl Fn(&Received) -> (u16, String) + Send + 'static,
        ) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            let url = format!("http://{}", listener.local_addr().expect("address"));
            let received = Arc::new(Mutex::new(Vec::new()));
            let log = Arc::clone(&received);
            let thread = std::thread::spawn(move || {
                for _ in 0..count {
                    let Ok((stream, _)) = listener.accept() else {
                        return;
                    };
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("request line");
                    let mut parts = line.split_whitespace();
                    let method = parts.next().unwrap_or_default().to_owned();
                    let target = parts.next().unwrap_or_default().to_owned();
                    let (mut length, mut authorization) = (0, None);
                    loop {
                        let mut header = String::new();
                        reader.read_line(&mut header).expect("header");
                        let header = header.trim_end();
                        if header.is_empty() {
                            break;
                        }
                        let (name, value) = header.split_once(':').unwrap_or((header, ""));
                        match name.to_ascii_lowercase().as_str() {
                            "content-length" => length = value.trim().parse().unwrap_or(0),
                            "authorization" => authorization = Some(value.trim().to_owned()),
                            _ => {}
                        }
                    }
                    let mut bytes = vec![0; length];
                    reader.read_exact(&mut bytes).expect("body");
                    let request = Received {
                        method,
                        target,
                        authorization,
                        body: String::from_utf8(bytes).expect("UTF-8"),
                    };
                    let (status, body) = respond(&request);
                    log.lock().expect("lock").push(request);
                    let reply = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let mut stream = reader.into_inner();
                    let _ = stream
                        .write_all(reply.as_bytes())
                        .and_then(|()| stream.flush());
                }
            });
            Self {
                url,
                received,
                thread: Some(thread),
            }
        }

        /// Waits for every reply to be served.
        pub fn finish(mut self) -> Vec<Received> {
            if let Some(thread) = self.thread.take() {
                thread.join().expect("server thread");
            }
            self.received.lock().expect("lock").clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::Server;
    use super::*;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    fn api(url: &str) -> Api {
        Api::new(url, "mos_k".to_owned(), Duration::from_secs(5)).expect("api")
    }

    #[test]
    fn recall_sends_the_page_limit_and_reads_pages() {
        let hit = json!({"page_id": Uuid::nil(), "scope": "shared:m", "slug": "s", "title": "T",
            "version": 1, "section": "T > A", "snippet": "body text", "score": 0.5});
        let server = Server::start(vec![(
            200,
            json!({"facts": [], "pages": [hit], "pages_skipped": null}).to_string(),
        )]);
        let recall = runtime()
            .block_on(api(&server.url).recall(&[Scope::agent("a")], "q", 10, 5, 2))
            .expect("recall");
        assert_eq!(recall.pages.len(), 1);
        assert_eq!(recall.pages[0].section, "T > A");
        let received = server.finish();
        assert_eq!(received[0].method, "POST");
        assert_eq!(received[0].target, "/v1/recall");
        assert_eq!(received[0].authorization.as_deref(), Some("Bearer mos_k"));
        let body: Value = serde_json::from_str(&received[0].body).expect("json");
        assert_eq!(
            body,
            json!({"scopes": ["agent:a"], "query": "q", "limit": 10, "page_limit": 5,
                "insight_limit": 2})
        );
    }

    #[test]
    fn a_server_without_pages_is_asked_again_without_page_limit() {
        let refused = json!({"error": {"code": "invalid_input",
            "message": "unknown field `page_limit`, expected one of `scopes`, `query`"}});
        let server = Server::start(vec![
            (400, refused.to_string()),
            (200, json!({"facts": []}).to_string()),
        ]);
        let recall = runtime()
            .block_on(api(&server.url).recall(&[Scope::agent("a")], "q", 10, 5, 2))
            .expect("recall falls back");
        assert!(recall.pages.is_empty());
        let received = server.finish();
        assert_eq!(received.len(), 2);
        let retry: Value = serde_json::from_str(&received[1].body).expect("json");
        assert_eq!(
            retry,
            json!({"scopes": ["agent:a"], "query": "q", "limit": 10, "insight_limit": 2})
        );
    }

    #[test]
    fn a_server_before_pages_and_insights_is_asked_without_either() {
        let refused = |field: &str| {
            json!({"error": {"code": "invalid_input",
                "message": format!("unknown field `{field}`, expected one of `scopes`")}})
            .to_string()
        };
        let server = Server::start(vec![
            (400, refused("insight_limit")),
            (400, refused("page_limit")),
            (200, json!({"facts": []}).to_string()),
        ]);
        let recall = runtime()
            .block_on(api(&server.url).recall(&[Scope::agent("a")], "q", 10, 5, 2))
            .expect("recall falls back twice");
        assert!(recall.pages.is_empty() && recall.insights.is_empty());
        let received = server.finish();
        let last: Value = serde_json::from_str(&received[2].body).expect("json");
        assert_eq!(
            last,
            json!({"scopes": ["agent:a"], "query": "q", "limit": 10})
        );
    }

    #[test]
    fn a_refusal_that_names_no_optional_field_is_an_error() {
        let refused = json!({"error": {"code": "invalid_input", "message": "query is empty"}});
        let server = Server::start(vec![(400, refused.to_string())]);
        let error = runtime()
            .block_on(api(&server.url).recall(&[Scope::agent("a")], "q", 10, 5, 2))
            .expect_err("no retry");
        assert_eq!(error, "invalid input: query is empty");
        assert_eq!(server.finish().len(), 1);
    }

    #[test]
    fn a_recall_without_pages_still_parses() {
        let server = Server::start(vec![(200, json!({"facts": []}).to_string())]);
        let recall = runtime()
            .block_on(api(&server.url).recall(&[Scope::agent("a")], "q", 5, 0, 0))
            .expect("recall");
        assert!(recall.pages.is_empty());
        assert!(recall.insights.is_empty());
        server.finish();
    }

    #[test]
    fn errors_name_the_code_and_never_quote_a_foreign_body() {
        let server = Server::start(vec![
            (
                409,
                json!({"error": {"code": "version_conflict", "message": "current is 3"}})
                    .to_string(),
            ),
            (502, "<html>secret</html>".to_owned()),
        ]);
        let api = api(&server.url);
        let runtime = runtime();
        let error = runtime
            .block_on(api.read_page(&Scope::agent("a"), "s", None))
            .expect_err("conflict");
        assert_eq!(error, "version conflict: current is 3");
        let error = runtime
            .block_on(api.delete_page(&Scope::agent("a"), "s"))
            .expect_err("proxy");
        assert!(error.contains("HTTP 502"), "{error}");
        assert!(!error.contains("secret"));
        server.finish();
    }

    #[test]
    fn a_slug_cannot_change_the_route() {
        let server = Server::start(vec![(200, "{}".to_owned())]);
        runtime()
            .block_on(api(&server.url).read_page(&Scope::shared("m"), "../keys?x=1", None))
            .expect("read");
        let received = server.finish();
        assert_eq!(received[0].target, "/v1/pages/shared:m/..%2Fkeys%3Fx=1");
    }
}
