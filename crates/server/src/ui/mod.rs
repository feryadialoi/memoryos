//! `memoryos ui`: a page on this machine for seeing and reviewing what is remembered.
//!
//! The page never holds an API key. It calls `/api/...` here, and this forwards a fixed list of
//! operations to the server with the key from `credentials.json`. That key can be an admin key and
//! the listener answers any local program, so three checks stand in front of it:
//!
//! - It binds to loopback only.
//! - The `Host` header must name this listener, which stops DNS rebinding.
//! - `/api` needs a token created per run, in a custom header. The page reads it from the URL
//!   fragment, which is never sent over the network, and another site can neither read it nor send
//!   that header without a CORS preflight, which this never approves.
//!
//! Fact, page and insight text is untrusted: agents write it, and it can quote hostile pages. `app.js` only ever sets it
//! as text, and the content security policy below refuses inline scripts as a second layer.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::extract::{Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use memoryos_core::{validate_slug, Scope};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::auth::generate_token;

const DEFAULT_PORT: u16 = 7878;
const TOKEN_HEADER: &str = "x-memoryos-ui";
const MAX_BODY: usize = 1024 * 1024;
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(30);
/// `style-src 'unsafe-inline'` is for Mermaid, which styles the SVG it draws inline. Scripts stay
/// `'self'` only, so page text still cannot run code.
const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self'; \
    style-src 'self' 'unsafe-inline'; \
    connect-src 'self'; img-src data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

const INDEX_HTML: &str = include_str!("assets/index.html");
const APP_JS: &str = include_str!("assets/app.js");
const MARKDOWN_JS: &str = include_str!("assets/markdown.js");
/// Mermaid 11.4.1 (MIT, `assets/vendor/mermaid.LICENSE`), loaded only when a page has a diagram.
const MERMAID_JS: &str = include_str!("assets/vendor/mermaid.min.js");
/// d3-force 3.0.0 with d3-quadtree 3.0.1, d3-dispatch 3.0.1 and d3-timer 3.0.1 (ISC,
/// `assets/vendor/d3-force.LICENSE`), loaded when the Map tab opens.
const D3_FORCE_JS: &str = include_str!("assets/vendor/d3-force.min.js");
const STYLE_CSS: &str = include_str!("assets/style.css");

/// Page edits by `scope/slug`: `None` while the editor is open, then the outcome.
type Edits = Arc<Mutex<HashMap<String, Option<Result<String, String>>>>>;

#[derive(Clone)]
pub struct UiState {
    upstream_url: String,
    upstream_key: String,
    token_hash: [u8; 32],
    allowed_hosts: [String; 2],
    client: reqwest::Client,
    /// The command "Open in editor" runs, from this process's environment.
    editor: Option<String>,
    edits: Edits,
}

impl UiState {
    pub fn new(
        upstream_url: String,
        upstream_key: String,
        token: &str,
        port: u16,
    ) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(UPSTREAM_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| format!("cannot start the HTTP client: {e}"))?;
        Ok(Self {
            upstream_url,
            upstream_key,
            token_hash: digest(token),
            allowed_hosts: [format!("127.0.0.1:{port}"), format!("localhost:{port}")],
            client,
            editor: memoryos_hooks::editor_command().ok(),
            edits: Arc::default(),
        })
    }
}

fn digest(text: &str) -> [u8; 32] {
    Sha256::digest(text.as_bytes()).into()
}

pub fn router(state: UiState) -> Router {
    Router::new()
        .route("/", get(|| asset(INDEX_HTML, "text/html; charset=utf-8")))
        .route(
            "/app.js",
            get(|| asset(APP_JS, "text/javascript; charset=utf-8")),
        )
        .route(
            "/markdown.js",
            get(|| asset(MARKDOWN_JS, "text/javascript; charset=utf-8")),
        )
        .route(
            "/vendor/mermaid.min.js",
            get(|| asset(MERMAID_JS, "text/javascript; charset=utf-8")),
        )
        .route(
            "/vendor/d3-force.min.js",
            get(|| asset(D3_FORCE_JS, "text/javascript; charset=utf-8")),
        )
        .route(
            "/style.css",
            get(|| asset(STYLE_CSS, "text/css; charset=utf-8")),
        )
        .route("/api/{*target}", any(forward))
        .route("/local/edit", post(start_edit).get(edit_status))
        .layer(middleware::from_fn_with_state(state.clone(), check_host))
        .with_state(state)
}

/// Runs until Ctrl+C. Only a failure to reach the server is left for the page to show.
pub async fn run(port: Option<u16>, open: bool) -> Result<(), String> {
    let (upstream_url, upstream_key) = memoryos_hooks::ui_connection()?;
    let listener = bind(port).await?;
    let local = listener
        .local_addr()
        .map_err(|e| format!("cannot read the local address: {e}"))?;
    let token = generate_token()?;
    let state = UiState::new(upstream_url, upstream_key, &token, local.port())?;
    let address = format!("http://127.0.0.1:{}/#t={token}", local.port());
    println!("MemoryOS is open at {address}");
    println!("Press Ctrl+C to stop.");
    if open {
        open_browser(&address);
    }
    let shutdown = CancellationToken::new();
    tokio::spawn(crate::cli::cancel_on_signal(shutdown.clone()));
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await
        .map_err(|e| format!("the page server stopped: {e}"))
}

async fn bind(port: Option<u16>) -> Result<TcpListener, String> {
    let wanted = SocketAddr::from(([127, 0, 0, 1], port.unwrap_or(DEFAULT_PORT)));
    match TcpListener::bind(wanted).await {
        Ok(listener) => Ok(listener),
        // A port the user named is a requirement. The default is only a preference.
        Err(error) if port.is_some() => Err(format!("cannot bind {wanted}: {error}")),
        Err(_) => TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .map_err(|e| format!("cannot bind a local port: {e}")),
    }
}

/// Best effort: the address is already printed, so a machine with no browser loses nothing.
fn open_browser(address: &str) {
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![address])
    } else if cfg!(target_os = "windows") {
        ("rundll32", vec!["url.dll,FileProtocolHandler", address])
    } else {
        ("xdg-open", vec![address])
    };
    let spawned = std::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if spawned.is_err() {
        println!("Could not open a browser. Open the address above yourself.");
    }
}

async fn asset(body: &'static str, content_type: &'static str) -> Response {
    let mut response = body.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn check_host(State(state): State<UiState>, request: Request, next: Next) -> Response {
    let allowed = request
        .headers()
        .get(header::HOST)
        .and_then(|host| host.to_str().ok())
        .is_some_and(|host| state.allowed_hosts.iter().any(|name| name == host));
    if allowed {
        next.run(request).await
    } else {
        failure(StatusCode::FORBIDDEN, "forbidden", "wrong Host header")
    }
}

fn failure(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({"error": {"code": code, "message": message}})),
    )
        .into_response()
}

#[derive(serde::Deserialize)]
struct EditTarget {
    scope: String,
    slug: String,
}

fn edit_key(target: &EditTarget) -> Result<(Scope, String), Box<Response>> {
    let bad = |message: &str| Box::new(failure(StatusCode::BAD_REQUEST, "invalid_input", message));
    let scope: Scope = target.scope.parse().map_err(|_| bad("invalid scope"))?;
    validate_slug(&target.slug).map_err(|_| bad("invalid slug"))?;
    Ok((scope, target.slug.clone()))
}

/// "Open in editor": runs `memoryos page edit` on this machine with the UI's key. The request names
/// only the page; the command comes from this process's environment. Returns at once.
async fn start_edit(
    State(state): State<UiState>,
    headers: HeaderMap,
    Json(target): Json<EditTarget>,
) -> Response {
    if !token_matches(&state, &headers) {
        return failure(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "this page's token is missing or old",
        );
    }
    let (scope, slug) = match edit_key(&target) {
        Ok(found) => found,
        Err(response) => return *response,
    };
    let Some(editor) = state.editor.clone() else {
        return failure(
            StatusCode::BAD_REQUEST,
            "no_editor",
            "no editor: start `memoryos ui` with MEMORYOS_EDITOR set, such as `code --wait` or `idea --wait`",
        );
    };
    let key = format!("{scope}/{slug}");
    {
        let mut edits = state.edits.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(edits.get(&key), Some(None)) {
            return failure(
                StatusCode::CONFLICT,
                "editing",
                "this page is already open in your editor",
            );
        }
        edits.insert(key.clone(), None);
    }
    let (url, api_key, edits) = (
        state.upstream_url.clone(),
        state.upstream_key.clone(),
        state.edits.clone(),
    );
    tokio::spawn(async move {
        let outcome =
            memoryos_hooks::edit_page_in_editor(&url, api_key, &scope, &slug, &editor).await;
        edits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, Some(outcome));
    });
    Json(json!({"editing": true, "editor": state.editor})).into_response()
}

/// Whether the editor is still open, and once it closed, what was saved. The outcome is reported once.
async fn edit_status(
    State(state): State<UiState>,
    headers: HeaderMap,
    Query(target): Query<EditTarget>,
) -> Response {
    if !token_matches(&state, &headers) {
        return failure(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "this page's token is missing or old",
        );
    }
    let (scope, slug) = match edit_key(&target) {
        Ok(found) => found,
        Err(response) => return *response,
    };
    let key = format!("{scope}/{slug}");
    let mut edits = state.edits.lock().unwrap_or_else(|e| e.into_inner());
    match edits.get(&key) {
        Some(None) => Json(json!({"editing": true})).into_response(),
        Some(Some(_)) => match edits.remove(&key) {
            Some(Some(Ok(message))) => {
                Json(json!({"editing": false, "saved": message})).into_response()
            }
            Some(Some(Err(message))) => {
                Json(json!({"editing": false, "error": message})).into_response()
            }
            _ => Json(json!({"editing": false})).into_response(),
        },
        None => Json(json!({"editing": false})).into_response(),
    }
}

fn token_matches(state: &UiState, headers: &HeaderMap) -> bool {
    headers
        .get(TOKEN_HEADER)
        .and_then(|token| token.to_str().ok())
        .is_some_and(|token| digest(token) == state.token_hash)
}

/// The server path for a page request, or `None` when the page has no business making it. Purge,
/// user deletion, retraction, commits, block edits and page writes or deletes are not here on
/// purpose. Knowledge pages are read and reviewed only. Insights are read, reviewed and retired,
/// never written or confirmed: that is the agent's work. `target` is already
/// percent-decoded, so every variable segment is checked before it goes into a URL.
pub(crate) fn upstream_path(method: &Method, target: &str) -> Option<String> {
    let segments: Vec<&str> = target.split('/').collect();
    let id = |text: &str| text.parse::<Uuid>().is_ok();
    match (method.as_str(), segments.as_slice()) {
        ("GET", ["status"]) => Some("/readyz".to_owned()),
        ("GET", ["scopes"]) => Some("/v1/scopes".to_owned()),
        ("GET", ["facts"]) => Some("/v1/facts".to_owned()),
        ("GET", ["quarantine"]) => Some("/v1/quarantine".to_owned()),
        ("POST", ["recall"]) => Some("/v1/recall".to_owned()),
        ("GET", ["facts", fact]) if id(fact) => Some(format!("/v1/facts/{fact}")),
        ("GET", ["facts", fact, "history"]) if id(fact) => {
            Some(format!("/v1/facts/{fact}/history"))
        }
        ("GET", ["episodes", episode]) if id(episode) => Some(format!("/v1/episodes/{episode}")),
        ("POST", ["quarantine", fact, action @ ("approve" | "reject")]) if id(fact) => {
            Some(format!("/v1/quarantine/{fact}/{action}"))
        }
        ("GET", ["agents", agent, "blocks"]) if safe_agent_id(agent) => {
            Some(format!("/v1/agents/{agent}/blocks"))
        }
        ("GET", ["pages"]) => Some("/v1/pages".to_owned()),
        ("GET", ["pages", "quarantine"]) => Some("/v1/pages/quarantine".to_owned()),
        ("POST", ["pages", "quarantine", page, version, action @ ("approve" | "reject")])
            if id(page) && page_version(version) =>
        {
            Some(format!("/v1/pages/quarantine/{page}/{version}/{action}"))
        }
        ("GET", ["pages", scope, slug]) if page_address(scope, slug) => {
            Some(format!("/v1/pages/{scope}/{slug}"))
        }
        ("GET", ["pages", scope, slug, "versions"]) if page_address(scope, slug) => {
            Some(format!("/v1/pages/{scope}/{slug}/versions"))
        }
        ("GET", ["insights"]) => Some("/v1/insights".to_owned()),
        ("GET", ["insights", "quarantine"]) => Some("/v1/insights/quarantine".to_owned()),
        ("GET", ["insights", insight]) if id(insight) => Some(format!("/v1/insights/{insight}")),
        ("POST", ["insights", "quarantine", insight, action @ ("approve" | "reject")])
            if id(insight) =>
        {
            Some(format!("/v1/insights/quarantine/{insight}/{action}"))
        }
        ("POST", ["insights", insight, "retire"]) if id(insight) => {
            Some(format!("/v1/insights/{insight}/retire"))
        }
        _ => None,
    }
}

/// A scope such as `shared:team` and a slug, both in the forms the server accepts, so neither can
/// carry a separator or a dot segment into the server path.
fn page_address(scope: &str, slug: &str) -> bool {
    scope.parse::<Scope>().is_ok() && validate_slug(slug).is_ok()
}

/// A version number as the server takes it: digits only, at least 1.
fn page_version(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 18
        && text.bytes().all(|b| b.is_ascii_digit())
        && text.parse::<i64>().is_ok_and(|n| n >= 1)
}

fn safe_agent_id(text: &str) -> bool {
    !text.is_empty()
        && text != "."
        && text != ".."
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
}

async fn forward(State(state): State<UiState>, uri: Uri, request: Request) -> Response {
    if !token_matches(&state, request.headers()) {
        return failure(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "this page's token is missing or old. Open the address `memoryos ui` printed",
        );
    }
    let method = request.method().clone();
    let target = uri.path().strip_prefix("/api/").unwrap_or_default();
    let decoded = match percent_decoded(target) {
        Some(decoded) => decoded,
        None => return not_allowed(),
    };
    let Some(path) = upstream_path(&method, &decoded) else {
        return not_allowed();
    };
    let body = match to_bytes(request.into_body(), MAX_BODY).await {
        Ok(body) => body,
        Err(_) => {
            return failure(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                "the request body is too large",
            )
        }
    };
    let mut url = format!("{}{path}", state.upstream_url);
    if let Some(query) = uri.query() {
        url.push('?');
        url.push_str(query);
    }
    let sent = state
        .client
        .request(method, url)
        .bearer_auth(&state.upstream_key)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await;
    let response = match sent {
        Ok(response) => response,
        Err(_) => {
            return failure(
                StatusCode::BAD_GATEWAY,
                "server_unreachable",
                "cannot reach the MemoryOS server. Start it with `memoryos setup`",
            )
        }
    };
    let status = response.status();
    let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
    match response.bytes().await {
        Ok(bytes) => {
            let mut built = Response::new(Body::from(bytes));
            *built.status_mut() = status;
            if let Some(content_type) = content_type {
                built
                    .headers_mut()
                    .insert(header::CONTENT_TYPE, content_type);
            }
            built
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            built
        }
        Err(_) => failure(
            StatusCode::BAD_GATEWAY,
            "server_unreachable",
            "the MemoryOS server stopped while answering",
        ),
    }
}

fn not_allowed() -> Response {
    failure(
        StatusCode::NOT_FOUND,
        "not_found",
        "this page cannot make that request",
    )
}

/// `None` for invalid encoding. A `/` that was encoded becomes a separator, so it cannot hide a
/// path inside one segment.
fn percent_decoded(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let pair = std::str::from_utf8(bytes.get(at + 1..at + 3)?).ok()?;
            out.push(u8::from_str_radix(pair, 16).ok()?);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "01a1b4ba-3cf2-71f1-870c-be296a1913de";

    fn allowed(method: Method, target: &str) -> bool {
        upstream_path(&method, target).is_some()
    }

    #[test]
    fn the_page_reaches_its_reads_and_review_only() {
        assert!(allowed(Method::GET, "scopes"));
        assert!(allowed(Method::GET, "facts"));
        assert!(allowed(Method::GET, &format!("facts/{ID}/history")));
        assert!(allowed(Method::POST, "recall"));
        assert!(allowed(Method::POST, &format!("quarantine/{ID}/approve")));
        assert!(allowed(Method::POST, &format!("quarantine/{ID}/reject")));
        assert!(allowed(Method::GET, "agents/claude-code/blocks"));
    }

    #[test]
    fn anything_that_deletes_or_writes_is_refused() {
        assert!(!allowed(Method::POST, &format!("episodes/{ID}/purge")));
        assert!(!allowed(Method::POST, &format!("facts/{ID}/invalidate")));
        assert!(!allowed(Method::DELETE, "users/alice/memory"));
        assert!(!allowed(Method::POST, "commits"));
        assert!(!allowed(Method::POST, "facts"));
        assert!(!allowed(Method::GET, &format!("quarantine/{ID}/approve")));
        assert!(!allowed(Method::POST, &format!("quarantine/{ID}/delete")));
    }

    #[test]
    fn the_page_reads_and_reviews_knowledge_pages() {
        assert_eq!(
            upstream_path(&Method::GET, "pages").as_deref(),
            Some("/v1/pages")
        );
        assert_eq!(
            upstream_path(&Method::GET, "pages/quarantine").as_deref(),
            Some("/v1/pages/quarantine")
        );
        assert_eq!(
            upstream_path(&Method::GET, "pages/shared:team/run-book.v2").as_deref(),
            Some("/v1/pages/shared:team/run-book.v2")
        );
        assert_eq!(
            upstream_path(&Method::GET, "pages/agent:claude-code/guide/versions").as_deref(),
            Some("/v1/pages/agent:claude-code/guide/versions")
        );
        for action in ["approve", "reject"] {
            assert_eq!(
                upstream_path(&Method::POST, &format!("pages/quarantine/{ID}/2/{action}")),
                Some(format!("/v1/pages/quarantine/{ID}/2/{action}"))
            );
        }
    }

    #[test]
    fn pages_are_never_written_or_deleted_through_the_page() {
        for method in [Method::PUT, Method::POST, Method::DELETE, Method::PATCH] {
            assert!(
                !allowed(method.clone(), "pages/shared:team/guide"),
                "{method}"
            );
            assert!(!allowed(method.clone(), "pages"), "{method}");
        }
        assert!(!allowed(Method::POST, "pages/quarantine"));
        assert!(!allowed(
            Method::DELETE,
            &format!("pages/quarantine/{ID}/2/reject")
        ));
        assert!(!allowed(
            Method::GET,
            &format!("pages/quarantine/{ID}/2/approve")
        ));
        assert!(!allowed(
            Method::POST,
            &format!("pages/quarantine/{ID}/2/delete")
        ));
        assert!(!allowed(Method::POST, &format!("pages/quarantine/{ID}/2")));
        assert!(!allowed(Method::POST, "pages/shared:team/guide/versions"));
        assert!(!allowed(Method::GET, "pages/shared:team/guide/sections"));
    }

    #[test]
    fn the_page_reads_reviews_and_retires_insights() {
        assert_eq!(
            upstream_path(&Method::GET, "insights").as_deref(),
            Some("/v1/insights")
        );
        assert_eq!(
            upstream_path(&Method::GET, "insights/quarantine").as_deref(),
            Some("/v1/insights/quarantine")
        );
        assert_eq!(
            upstream_path(&Method::GET, &format!("insights/{ID}")),
            Some(format!("/v1/insights/{ID}"))
        );
        for action in ["approve", "reject"] {
            assert_eq!(
                upstream_path(&Method::POST, &format!("insights/quarantine/{ID}/{action}")),
                Some(format!("/v1/insights/quarantine/{ID}/{action}"))
            );
        }
        assert_eq!(
            upstream_path(&Method::POST, &format!("insights/{ID}/retire")),
            Some(format!("/v1/insights/{ID}/retire"))
        );
    }

    #[test]
    fn insights_are_never_written_or_confirmed_through_the_page() {
        assert!(!allowed(Method::POST, "insights"));
        assert!(!allowed(Method::PUT, &format!("insights/{ID}")));
        assert!(!allowed(Method::DELETE, &format!("insights/{ID}")));
        assert!(!allowed(Method::POST, &format!("insights/{ID}/confirm")));
        assert!(!allowed(Method::GET, &format!("insights/{ID}/retire")));
        assert!(!allowed(
            Method::GET,
            &format!("insights/quarantine/{ID}/approve")
        ));
        assert!(!allowed(
            Method::POST,
            &format!("insights/quarantine/{ID}/delete")
        ));
        assert!(!allowed(Method::POST, "reflect/batch"));
        assert!(!allowed(Method::GET, "insights/../facts"));
        assert!(!allowed(Method::POST, "insights/not-a-uuid/retire"));
        assert!(!allowed(
            Method::POST,
            "insights/quarantine/not-a-uuid/approve"
        ));
        let decoded = percent_decoded(&format!("insights/{ID}%2fconfirm")).expect("decodes");
        assert!(!allowed(Method::GET, &decoded));
    }

    #[test]
    fn page_segments_cannot_change_the_path() {
        // Scopes and slugs in forms the server would refuse never reach it.
        assert!(!allowed(Method::GET, "pages/../guide"));
        assert!(!allowed(Method::GET, "pages/shared:team/.."));
        assert!(!allowed(Method::GET, "pages/shared:team/.hidden"));
        assert!(!allowed(Method::GET, "pages/shared:team/Guide"));
        assert!(!allowed(Method::GET, "pages/team/guide"));
        assert!(!allowed(Method::GET, "pages/other:team/guide"));
        assert!(!allowed(Method::GET, "pages/shared:/guide"));
        assert!(!allowed(Method::GET, "pages/shared:te?am/guide"));
        assert!(!allowed(Method::GET, "pages/shared:team/gu?ide"));
        assert!(!allowed(Method::GET, "pages/shared:team/guide/../../facts"));
        let decoded =
            percent_decoded("pages/shared:team/guide%2f..%2f..%2fversions").expect("decodes");
        assert!(!allowed(Method::GET, &decoded));
        let decoded = percent_decoded("pages/shared%3Ateam/guide").expect("decodes");
        assert!(allowed(Method::GET, &decoded));
        for version in ["0", "-1", "+2", "1.5", "x", "", "99999999999999999999"] {
            assert!(
                !allowed(
                    Method::POST,
                    &format!("pages/quarantine/{ID}/{version}/approve")
                ),
                "{version}"
            );
        }
        assert!(!allowed(
            Method::POST,
            "pages/quarantine/not-a-uuid/2/approve"
        ));
    }

    #[test]
    fn a_variable_segment_cannot_change_the_path() {
        assert!(!allowed(Method::GET, "facts/../users"));
        assert!(!allowed(Method::GET, "agents/../blocks"));
        assert!(!allowed(Method::GET, "agents/a/../../x/blocks"));
        assert!(!allowed(Method::GET, "agents/a?b/blocks"));
        let decoded = percent_decoded("agents/%2e%2e%2fv1/blocks").expect("decodes");
        assert!(!allowed(Method::GET, &decoded));
        assert!(percent_decoded("facts/%zz").is_none());
    }

    #[test]
    fn the_token_must_match() {
        let state = UiState::new("http://x".into(), "k".into(), "right", 1).expect("state");
        let mut headers = HeaderMap::new();
        assert!(!token_matches(&state, &headers));
        headers.insert(TOKEN_HEADER, HeaderValue::from_static("wrong"));
        assert!(!token_matches(&state, &headers));
        headers.insert(TOKEN_HEADER, HeaderValue::from_static("right"));
        assert!(token_matches(&state, &headers));
    }

    fn edit_request(scope: &str, slug: &str) -> EditTarget {
        EditTarget {
            scope: scope.into(),
            slug: slug.into(),
        }
    }

    async fn body_json(response: Response) -> serde_json::Value {
        let bytes = to_bytes(response.into_body(), 1 << 20).await.expect("body");
        serde_json::from_slice(&bytes).expect("json")
    }

    #[tokio::test]
    async fn open_in_editor_needs_the_token_a_valid_page_and_an_editor() {
        let mut state =
            UiState::new("http://127.0.0.1:9".into(), "k".into(), "right", 1).expect("state");
        state.editor = None;
        let mut headers = HeaderMap::new();
        let refused = start_edit(
            State(state.clone()),
            headers.clone(),
            Json(edit_request("shared:t", "a")),
        )
        .await;
        assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
        headers.insert(TOKEN_HEADER, HeaderValue::from_static("right"));
        let bad = start_edit(
            State(state.clone()),
            headers.clone(),
            Json(edit_request("shared:t", "../x")),
        )
        .await;
        assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
        let none = start_edit(State(state), headers, Json(edit_request("shared:t", "a"))).await;
        assert_eq!(none.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(none).await["error"]["code"], "no_editor");
    }

    #[tokio::test]
    async fn an_edit_reports_its_outcome_once() {
        let mut state =
            UiState::new("http://127.0.0.1:9".into(), "k".into(), "right", 1).expect("state");
        state.editor = Some("true".into());
        let mut headers = HeaderMap::new();
        headers.insert(TOKEN_HEADER, HeaderValue::from_static("right"));
        let started = start_edit(
            State(state.clone()),
            headers.clone(),
            Json(edit_request("shared:t", "a")),
        )
        .await;
        assert_eq!(started.status(), StatusCode::OK);
        let mut outcome = serde_json::Value::Null;
        for _ in 0..100 {
            let status = edit_status(
                State(state.clone()),
                headers.clone(),
                Query(edit_request("shared:t", "a")),
            )
            .await;
            outcome = body_json(status).await;
            if outcome["editing"] == false {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // Nothing listens on port 9, so reading the page fails before any editor runs.
        assert!(
            outcome["error"]
                .as_str()
                .is_some_and(|e| e.contains("cannot reach")),
            "{outcome}"
        );
        let again = edit_status(State(state), headers, Query(edit_request("shared:t", "a"))).await;
        assert_eq!(body_json(again).await, json!({"editing": false}));
    }

    #[test]
    fn page_assets_load_nothing_from_outside_and_set_no_inline_code() {
        for asset in [INDEX_HTML, APP_JS, MARKDOWN_JS, STYLE_CSS] {
            assert!(!asset.contains("https://"), "external URL in an asset");
            assert!(!asset.contains("innerHTML"), "innerHTML in an asset");
        }
        assert!(!INDEX_HTML.contains("<script>"), "inline script");
        assert!(!INDEX_HTML.contains("<style>"), "inline style");
    }
}
