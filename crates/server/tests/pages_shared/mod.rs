//! Knowledge page scenarios (SPEC sections 6 and 8) over REST, MCP and the SDK. Both suites run
//! them: `integration` on Postgres and `integration-sqlite` on a SQLite file. Each scenario uses its
//! own tenant, so it needs nothing from any other.

use std::sync::Arc;

use memoryos_core::{
    MemoryStore, NewPageSection, NewPageVersion, PageVersion, RequestContext, Scope, Trust,
    EMBEDDING_DIMS,
};
use memoryos_sdk::{MemoryOs, Options, PageVersionStatus, PageWrite};
use memoryos_server::auth::{generate_token, hash_token};
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use uuid::Uuid;

/// A running server and direct access to its store, for keys and for seeding trust levels that a
/// write through the API cannot reach.
pub struct Harness {
    pub url: String,
    pub store: Arc<dyn MemoryStore>,
    pub tenant: String,
    http: reqwest::Client,
}

impl Harness {
    pub fn new(url: String, store: Arc<dyn MemoryStore>, tenant: &str) -> Self {
        Self {
            url,
            store,
            tenant: tenant.to_owned(),
            http: reqwest::Client::new(),
        }
    }

    pub async fn key(&self, scopes: &[&str], max_trust: Trust, admin: bool) -> String {
        let token = generate_token().expect("token");
        let scopes: Vec<String> = scopes.iter().map(|s| (*s).to_owned()).collect();
        self.store
            .create_api_key(
                &self.tenant,
                &format!("pages-{}", Uuid::now_v7()),
                &hash_token(&token),
                &scopes,
                max_trust,
                admin,
            )
            .await
            .expect("create key");
        token
    }

    pub async fn call(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let method = Method::from_bytes(method.as_bytes()).expect("method");
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.url))
            .bearer_auth(token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.expect("send request");
        let status = response.status();
        let bytes = response.bytes().await.expect("read body");
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// `PUT /v1/pages/{scope}/{slug}`.
    pub async fn put(
        &self,
        token: &str,
        scope: &str,
        slug: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        self.call(
            "PUT",
            &format!("/v1/pages/{scope}/{slug}"),
            token,
            Some(body),
        )
        .await
    }

    pub async fn get(&self, token: &str, path: &str) -> (StatusCode, Value) {
        self.call("GET", path, token, None).await
    }

    /// Calls an MCP tool and returns whether it failed and its parsed text.
    pub async fn tool(&self, token: &str, name: &str, arguments: Value) -> (bool, Value) {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        });
        let (status, reply) = self.call("POST", "/mcp", token, Some(request)).await;
        assert_eq!(status, StatusCode::OK, "{reply}");
        let result = &reply["result"];
        let text = result["content"][0]["text"].as_str().expect("text");
        let parsed = serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned()));
        (result["isError"].as_bool().expect("isError"), parsed)
    }

    /// Stores a version directly at `trust`, active unless the store's own rules quarantine it.
    pub async fn seed(
        &self,
        scope: &str,
        slug: &str,
        trust: Trust,
        expected_version: Option<i64>,
        body: &str,
    ) -> PageVersion {
        let ctx = RequestContext {
            tenant: self.tenant.clone(),
            actor: "seed".to_owned(),
        };
        let mut embedding = vec![0.1f32; EMBEDDING_DIMS];
        embedding[0] = 1.0;
        self.store
            .put_page(
                &ctx,
                NewPageVersion {
                    scope: scope.parse::<Scope>().expect("scope"),
                    slug: slug.to_owned(),
                    title: "Seeded".to_owned(),
                    body: body.to_owned(),
                    summary: None,
                    trust,
                    expected_version,
                    approve_up_to: None,
                    sections: vec![NewPageSection {
                        ordinal: 0,
                        heading: "Seeded".to_owned(),
                        text: body.to_owned(),
                        embedding,
                    }],
                },
            )
            .await
            .expect("seed page")
    }
}

#[track_caller]
fn assert_error(status: StatusCode, body: &Value, expected: u16, code: &str) {
    assert_eq!(status.as_u16(), expected, "{body}");
    assert_eq!(body["error"]["code"], code, "{body}");
    assert!(body["error"]["message"].is_string(), "{body}");
}

fn page(title: &str, body: &str) -> Value {
    json!({"title": title, "body": body})
}

fn edit(title: &str, body: &str, expected_version: i64) -> Value {
    json!({"title": title, "body": body, "expected_version": expected_version})
}

fn version_of(body: &Value) -> i64 {
    body["version"]
        .as_i64()
        .unwrap_or_else(|| panic!("no version in {body}"))
}

fn page_id_of(body: &Value) -> Uuid {
    body["page_id"]
        .as_str()
        .unwrap_or_else(|| panic!("no page_id in {body}"))
        .parse()
        .expect("uuid")
}

/// Create, read, edit, list, history, conflicts, input checks and delete in an agent's own scope.
pub async fn crud_over_rest(h: &Harness) {
    let agent = h.key(&["agent:a1"], Trust::Agent, false).await;

    let (status, created) = h
        .put(
            &agent,
            "agent:a1",
            "runbook",
            page("Runbook", "# Start\nRun it."),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    assert_eq!(created["version"], 1);
    assert_eq!(created["status"], "active");
    assert_eq!(created["trust"], "agent");
    assert_eq!(created["scope"], "agent:a1");
    assert_eq!(created["slug"], "runbook");
    assert_eq!(created["base_version"], Value::Null);
    assert_eq!(created["redacted"], false);

    // Creating again while a current version exists is a conflict.
    let (status, body) = h
        .put(&agent, "agent:a1", "runbook", page("Runbook", "other"))
        .await;
    assert_error(status, &body, 409, "version_conflict");

    let (status, read) = h.get(&agent, "/v1/pages/agent:a1/runbook").await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(read["body"], "# Start\nRun it.");
    assert_eq!(read["version"], 1);
    assert_eq!(read["status"], "active");

    let (status, edited) = h
        .put(
            &agent,
            "agent:a1",
            "runbook",
            edit("Runbook", "# Start\nRun it twice.", 1),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{edited}");
    assert_eq!(edited["version"], 2);
    assert_eq!(edited["base_version"], 1);
    assert_eq!(edited["status"], "active");

    // An edit of a version that is no longer current.
    let (status, body) = h
        .put(&agent, "agent:a1", "runbook", edit("Runbook", "stale", 1))
        .await;
    assert_error(status, &body, 409, "version_conflict");

    let (status, old) = h.get(&agent, "/v1/pages/agent:a1/runbook?version=1").await;
    assert_eq!(status, StatusCode::OK, "{old}");
    assert_eq!(old["status"], "superseded");
    assert_eq!(old["body"], "# Start\nRun it.");
    let (status, body) = h.get(&agent, "/v1/pages/agent:a1/runbook?version=0").await;
    assert_error(status, &body, 400, "invalid_input");
    let (status, body) = h.get(&agent, "/v1/pages/agent:a1/runbook?version=9").await;
    assert_error(status, &body, 404, "not_found");

    let (status, history) = h.get(&agent, "/v1/pages/agent:a1/runbook/versions").await;
    assert_eq!(status, StatusCode::OK, "{history}");
    let versions: Vec<i64> = history["versions"]
        .as_array()
        .expect("versions")
        .iter()
        .map(version_of)
        .collect();
    assert_eq!(versions, [2, 1]);
    assert!(history["versions"][0].get("body").is_none());

    let (status, _) = h
        .put(&agent, "agent:a1", "readme", page("Readme", "Hello."))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, listed) = h.get(&agent, "/v1/pages?scope=agent:a1").await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let slugs: Vec<&str> = listed["pages"]
        .as_array()
        .expect("pages")
        .iter()
        .map(|p| p["slug"].as_str().expect("slug"))
        .collect();
    assert_eq!(slugs, ["readme", "runbook"]);
    assert_eq!(listed["pages"][1]["version"], 2);
    assert!(listed["pages"][0].get("body").is_none());
    let (_, prefixed) = h.get(&agent, "/v1/pages?scope=agent:a1&prefix=run").await;
    assert_eq!(prefixed["pages"].as_array().expect("pages").len(), 1);
    let (_, limited) = h.get(&agent, "/v1/pages?scope=agent:a1&limit=1").await;
    assert_eq!(limited["pages"].as_array().expect("pages").len(), 1);
    for bad in ["limit=0", "limit=501"] {
        let (status, body) = h
            .get(&agent, &format!("/v1/pages?scope=agent:a1&{bad}"))
            .await;
        assert_error(status, &body, 400, "invalid_input");
    }
    let (status, body) = h.get(&agent, "/v1/pages").await;
    assert_error(status, &body, 400, "invalid_input");

    let (status, scopes) = h.get(&agent, "/v1/scopes").await;
    assert_eq!(status, StatusCode::OK, "{scopes}");
    let summary = scopes["scopes"]
        .as_array()
        .expect("scopes")
        .iter()
        .find(|s| s["scope"] == "agent:a1")
        .expect("agent:a1 summary");
    assert_eq!(summary["pages"], 2);

    // Malformed input is refused before the store.
    for (scope, slug) in [
        ("team:x", "ok"),
        ("agent:a1", "Bad.Slug"),
        ("agent:a1", "-x"),
    ] {
        let (status, body) = h.put(&agent, scope, slug, page("T", "b")).await;
        assert_error(status, &body, 400, "invalid_input");
    }
    let (status, body) = h
        .put(
            &agent,
            "agent:a1",
            "runbook",
            json!({"title": "T", "body": "b", "trust": "system"}),
        )
        .await;
    assert_error(status, &body, 400, "invalid_input");
    let (status, body) = h.put(&agent, "agent:a1", "empty", page("", "b")).await;
    assert_error(status, &body, 400, "invalid_input");
    // Editing a page that does not exist.
    let (status, body) = h
        .put(&agent, "agent:a1", "missing", edit("T", "b", 1))
        .await;
    assert_error(status, &body, 404, "not_found");

    let (status, body) = h
        .call("DELETE", "/v1/pages/agent:a1/runbook", &agent, None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, body) = h.get(&agent, "/v1/pages/agent:a1/runbook").await;
    assert_error(status, &body, 404, "not_found");
    let (status, body) = h
        .call("DELETE", "/v1/pages/agent:a1/runbook", &agent, None)
        .await;
    assert_error(status, &body, 404, "not_found");
}

/// Secrets are masked before anything is stored, and the response says so.
pub async fn redaction_is_reported(h: &Harness) {
    let agent = h.key(&["agent:a1"], Trust::Agent, false).await;
    let (status, written) = h
        .put(
            &agent,
            "agent:a1",
            "deploy",
            json!({
                "title": "Deploy",
                "body": "# Login\npassword=hunter2 then deploy.",
                "summary": "token: abc123",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{written}");
    assert_eq!(written["redacted"], true);
    assert_eq!(written["summary"], "token: [redacted]");
    let (_, read) = h.get(&agent, "/v1/pages/agent:a1/deploy").await;
    let body = read["body"].as_str().expect("body");
    assert!(body.contains("password=[redacted]"), "{body}");
    assert!(!body.contains("hunter2"), "{body}");
}

/// A key reaches only its own scopes: a named scope is 403, a version loaded by id is 404.
pub async fn scopes_are_enforced(h: &Harness) {
    let root = h.key(&["*"], Trust::System, true).await;
    let other_admin = h.key(&["shared:other"], Trust::System, true).await;
    let agent = h.key(&["agent:a1"], Trust::Agent, false).await;

    let (status, _) = h
        .put(
            &root,
            "shared:team",
            "guide",
            json!({"title": "Guide", "body": "v1", "approve": true}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, pending) = h
        .put(
            &agent_for(h, "shared:team").await,
            "shared:team",
            "guide",
            edit("Guide", "v2", 1),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{pending}");
    assert_eq!(pending["status"], "quarantined");
    let page_id = page_id_of(&pending);

    for (method, path) in [
        ("GET", "/v1/pages/shared:team/guide"),
        ("GET", "/v1/pages/shared:team/guide/versions"),
        ("GET", "/v1/pages?scope=shared:team"),
        ("DELETE", "/v1/pages/shared:team/guide"),
    ] {
        let (status, body) = h.call(method, path, &agent, None).await;
        assert_error(status, &body, 403, "forbidden");
    }
    let (status, body) = h
        .put(&agent, "shared:team", "guide", edit("G", "x", 1))
        .await;
    assert_error(status, &body, 403, "forbidden");

    // Review by id: another scope looks exactly like an id that does not exist.
    for action in ["approve", "reject"] {
        let (status, body) = h
            .call(
                "POST",
                &format!("/v1/pages/quarantine/{page_id}/2/{action}"),
                &other_admin,
                None,
            )
            .await;
        assert_error(status, &body, 404, "not_found");
        let (status, body) = h
            .call(
                "POST",
                &format!("/v1/pages/quarantine/{}/2/{action}", Uuid::now_v7()),
                &root,
                None,
            )
            .await;
        assert_error(status, &body, 404, "not_found");
        let (status, body) = h
            .call(
                "POST",
                &format!("/v1/pages/quarantine/{page_id}/x/{action}"),
                &root,
                None,
            )
            .await;
        assert_error(status, &body, 400, "invalid_input");
    }

    // Listing: an exact scope the key lacks is 403, and so is a pattern with no overlap.
    for scopes in ["shared:team", "user:*"] {
        let (status, body) = h
            .get(
                &other_admin,
                &format!("/v1/pages/quarantine?scopes={scopes}"),
            )
            .await;
        assert_error(status, &body, 403, "forbidden");
    }
    // A pattern narrows to what the key allows.
    let (status, listed) = h
        .get(&other_admin, "/v1/pages/quarantine?scopes=shared:*")
        .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(listed["versions"], json!([]));
}

async fn agent_for(h: &Harness, scope: &str) -> String {
    h.key(&[scope], Trust::Agent, false).await
}

/// An agent's edit of a shared page waits for review while readers keep the old version, and an
/// admin approves or rejects it. Approving over a base that moved is a conflict.
pub async fn review_flow(h: &Harness) {
    let admin = h.key(&["*"], Trust::System, true).await;
    let agent = h.key(&["shared:team"], Trust::Agent, false).await;

    // An agent cannot approve its own write.
    let (status, body) = h
        .put(
            &agent,
            "shared:team",
            "guide",
            json!({"title": "Guide", "body": "v1", "approve": true}),
        )
        .await;
    assert_error(status, &body, 403, "forbidden");
    // An admin can, and the version is active at once.
    let (status, first) = h
        .put(
            &admin,
            "shared:team",
            "guide",
            json!({"title": "Guide", "body": "v1", "approve": true}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["status"], "active");
    let page_id = page_id_of(&first);

    let (status, pending) = h
        .put(
            &agent,
            "shared:team",
            "guide",
            edit("Guide", "v2 by agent", 1),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{pending}");
    assert_eq!(pending["status"], "quarantined");
    assert_eq!(pending["version"], 2);
    let (_, read) = h.get(&agent, "/v1/pages/shared:team/guide").await;
    assert_eq!(read["version"], 1);
    assert_eq!(read["body"], "v1");
    let (status, body) = h.get(&agent, "/v1/pages/shared:team/guide?version=2").await;
    assert_error(status, &body, 404, "not_found");

    // Only an admin lists and reviews.
    let (status, body) = h
        .get(&agent, "/v1/pages/quarantine?scopes=shared:team")
        .await;
    assert_error(status, &body, 403, "forbidden");
    let (status, body) = h
        .call(
            "POST",
            &format!("/v1/pages/quarantine/{page_id}/2/approve"),
            &agent,
            None,
        )
        .await;
    assert_error(status, &body, 403, "forbidden");

    for scopes in ["shared:team", "shared:*", "*"] {
        let (status, listed) = h
            .get(&admin, &format!("/v1/pages/quarantine?scopes={scopes}"))
            .await;
        assert_eq!(status, StatusCode::OK, "{listed}");
        let versions = listed["versions"].as_array().expect("versions");
        assert_eq!(versions.len(), 1, "{scopes}: {listed}");
        assert_eq!(versions[0]["version"], 2);
        assert_eq!(versions[0]["body"], "v2 by agent");
        assert_eq!(versions[0]["base_body"], "v1");
        assert_eq!(versions[0]["status"], "quarantined");
    }
    for bad in ["limit=0", "limit=201"] {
        let (status, body) = h
            .get(
                &admin,
                &format!("/v1/pages/quarantine?scopes=shared:team&{bad}"),
            )
            .await;
        assert_error(status, &body, 400, "invalid_input");
    }

    let (status, approved) = h
        .call(
            "POST",
            &format!("/v1/pages/quarantine/{page_id}/2/approve"),
            &admin,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{approved}");
    assert_eq!(approved["version"], 2);
    assert_eq!(approved["status"], "active");
    assert_eq!(approved["body"], "v2 by agent");
    let (_, read) = h.get(&agent, "/v1/pages/shared:team/guide").await;
    assert_eq!(read["version"], 2);

    // Reject: the version is kept as rejected and readers never see it.
    let (_, third) = h
        .put(&agent, "shared:team", "guide", edit("Guide", "v3 bad", 2))
        .await;
    assert_eq!(third["status"], "quarantined");
    let (status, rejected) = h
        .call(
            "POST",
            &format!("/v1/pages/quarantine/{page_id}/3/reject"),
            &admin,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{rejected}");
    assert_eq!(rejected["status"], "rejected");
    assert_eq!(rejected["version"], 3);
    let (_, read) = h.get(&agent, "/v1/pages/shared:team/guide").await;
    assert_eq!(read["version"], 2);
    let (status, body) = h
        .call(
            "POST",
            &format!("/v1/pages/quarantine/{page_id}/3/approve"),
            &admin,
            None,
        )
        .await;
    assert_error(status, &body, 400, "invalid_input");

    // Approving an edit whose base is no longer current would overwrite what it never saw.
    let (_, fourth) = h
        .put(
            &agent,
            "shared:team",
            "guide",
            edit("Guide", "v4 from v2", 2),
        )
        .await;
    assert_eq!(fourth["status"], "quarantined");
    assert_eq!(fourth["version"], 4);
    let (status, fifth) = h
        .put(
            &admin,
            "shared:team",
            "guide",
            json!({"title": "Guide", "body": "v5 by admin", "expected_version": 2, "approve": true}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{fifth}");
    assert_eq!(fifth["status"], "active");
    let (status, body) = h
        .call(
            "POST",
            &format!("/v1/pages/quarantine/{page_id}/4/approve"),
            &admin,
            None,
        )
        .await;
    assert_error(status, &body, 409, "version_conflict");
    let (_, read) = h.get(&agent, "/v1/pages/shared:team/guide").await;
    assert_eq!(read["body"], "v5 by admin");

    let (_, history) = h.get(&admin, "/v1/pages/shared:team/guide/versions").await;
    let statuses: Vec<(i64, &str)> = history["versions"]
        .as_array()
        .expect("versions")
        .iter()
        .map(|v| (version_of(v), v["status"].as_str().expect("status")))
        .collect();
    assert_eq!(
        statuses,
        [
            (5, "active"),
            (4, "quarantined"),
            (3, "rejected"),
            (2, "superseded"),
            (1, "superseded"),
        ]
    );
}

/// Admin rights stop at `max_trust`, for the version reviewed and for the version it replaces.
pub async fn trust_ceilings(h: &Harness) {
    let low_admin = h.key(&["*"], Trust::Agent, true).await;
    let admin = h.key(&["*"], Trust::System, true).await;
    let agent = h.key(&["shared:*"], Trust::Agent, false).await;

    // A user-trust current version: an agent-trust admin cannot overwrite it with approve.
    let seeded = h
        .seed("shared:hi", "policy", Trust::User, None, "user text")
        .await;
    assert_eq!(seeded.status, PageVersionStatus::Active);
    let (status, body) = h
        .put(
            &low_admin,
            "shared:hi",
            "policy",
            json!({"title": "P", "body": "x", "expected_version": 1, "approve": true}),
        )
        .await;
    assert_error(status, &body, 403, "forbidden");

    // An agent edit waits, and the low admin cannot approve it over the user version.
    let (_, pending) = h
        .put(&agent, "shared:hi", "policy", edit("P", "agent text", 1))
        .await;
    assert_eq!(pending["status"], "quarantined", "{pending}");
    let page_id = page_id_of(&pending);
    let (status, body) = h
        .call(
            "POST",
            &format!("/v1/pages/quarantine/{page_id}/2/approve"),
            &low_admin,
            None,
        )
        .await;
    assert_error(status, &body, 403, "forbidden");
    // Rejecting only needs the version's own trust.
    let (status, rejected) = h
        .call(
            "POST",
            &format!("/v1/pages/quarantine/{page_id}/2/reject"),
            &low_admin,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{rejected}");

    // A version of higher trust than the key: neither approve nor reject.
    h.seed("shared:top", "rules", Trust::System, None, "system text")
        .await;
    let waiting = h
        .seed("shared:top", "rules", Trust::User, Some(1), "user edit")
        .await;
    assert_eq!(waiting.status, PageVersionStatus::Quarantined);
    for action in ["approve", "reject"] {
        let (status, body) = h
            .call(
                "POST",
                &format!("/v1/pages/quarantine/{}/2/{action}", waiting.page_id),
                &low_admin,
                None,
            )
            .await;
        assert_error(status, &body, 403, "forbidden");
    }
    let (status, approved) = h
        .call(
            "POST",
            &format!("/v1/pages/quarantine/{}/2/approve", waiting.page_id),
            &admin,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{approved}");
    assert_eq!(approved["body"], "user edit");
}

/// Writing needs agent trust. Deleting needs user trust in user and shared scopes, agent in agent.
pub async fn write_and_delete_trust(h: &Harness) {
    let external = h.key(&["*"], Trust::External, true).await;
    let agent = h.key(&["*"], Trust::Agent, false).await;
    let user = h.key(&["*"], Trust::User, false).await;

    let (status, body) = h
        .put(&external, "agent:x", "notes", page("Notes", "n"))
        .await;
    assert_error(status, &body, 403, "forbidden");

    let (status, own) = h.put(&agent, "agent:x", "notes", page("Notes", "n")).await;
    assert_eq!(status, StatusCode::OK, "{own}");
    assert_eq!(own["status"], "active");
    // A user-trust key writes at agent trust too, so a user page still waits for review.
    let (status, mine) = h.put(&user, "user:u1", "prefs", page("Prefs", "p")).await;
    assert_eq!(status, StatusCode::OK, "{mine}");
    assert_eq!(mine["trust"], "agent");
    assert_eq!(mine["status"], "quarantined");
    h.seed("shared:team", "faq", Trust::User, None, "faq").await;

    let (status, body) = h
        .call("DELETE", "/v1/pages/agent:x/notes", &external, None)
        .await;
    assert_error(status, &body, 403, "forbidden");
    for path in ["/v1/pages/shared:team/faq", "/v1/pages/user:u1/prefs"] {
        let (status, body) = h.call("DELETE", path, &agent, None).await;
        assert_error(status, &body, 403, "forbidden");
    }
    let (status, _) = h
        .call("DELETE", "/v1/pages/agent:x/notes", &agent, None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    for path in ["/v1/pages/shared:team/faq", "/v1/pages/user:u1/prefs"] {
        let (status, body) = h.call("DELETE", path, &user, None).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{path}: {body}");
    }
    let (status, body) = h.get(&user, "/v1/pages/shared:team/faq").await;
    assert_error(status, &body, 404, "not_found");
}

/// Recall returns page hits beside facts, over REST and MCP, and none when it travels in time.
pub async fn recall_returns_pages(h: &Harness) {
    let agent = h.key(&["agent:a1"], Trust::Agent, false).await;
    let (status, _) = h
        .put(
            &agent,
            "agent:a1",
            "zebra-ops",
            page(
                "Zebra operations",
                "# Restart\nStop the zebra feeder service before restarting the enclosure.\n\
                 # Feeding\nGive hay twice a day.",
            ),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let query = "restart the zebra feeder service";
    let (status, recalled) = h
        .call(
            "POST",
            "/v1/recall",
            &agent,
            Some(json!({"scopes": ["agent:a1"], "query": query})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{recalled}");
    let hit = &recalled["pages"][0];
    assert_eq!(hit["slug"], "zebra-ops", "{recalled}");
    assert_eq!(hit["scope"], "agent:a1");
    assert_eq!(hit["title"], "Zebra operations");
    assert_eq!(hit["version"], 1);
    assert_eq!(hit["section"], "Zebra operations > Restart");
    assert!(hit["snippet"]
        .as_str()
        .expect("snippet")
        .contains("zebra feeder"));
    assert!(hit["score"].is_number());
    assert_eq!(recalled["pages_skipped"], Value::Null);
    assert_eq!(recalled["pages"].as_array().expect("pages").len(), 1);

    let (_, off) = h
        .call(
            "POST",
            "/v1/recall",
            &agent,
            Some(json!({"scopes": ["agent:a1"], "query": query, "page_limit": 0})),
        )
        .await;
    assert_eq!(off["pages"], json!([]), "{off}");
    let (status, body) = h
        .call(
            "POST",
            "/v1/recall",
            &agent,
            Some(json!({"scopes": ["agent:a1"], "query": query, "page_limit": 11})),
        )
        .await;
    assert_error(status, &body, 400, "invalid_input");

    let (_, past) = h
        .call(
            "POST",
            "/v1/recall",
            &agent,
            Some(json!({"scopes": ["agent:a1"], "query": query, "as_of": "2020-01-01T00:00:00Z"})),
        )
        .await;
    assert_eq!(past["pages"], json!([]), "{past}");
    assert_eq!(past["pages_skipped"], "time_travel");

    let (failed, result) = h
        .tool(
            &agent,
            "recall",
            json!({"scopes": ["agent:a1"], "query": query, "page_limit": 2}),
        )
        .await;
    assert!(!failed, "{result}");
    assert_eq!(result["pages"][0]["slug"], "zebra-ops", "{result}");
    assert_eq!(result["pages_skipped"], Value::Null);
}

/// The MCP page tools work and apply the same rules as REST.
pub async fn mcp_page_tools(h: &Harness) {
    let agent = h
        .key(&["agent:a1", "shared:team"], Trust::Agent, false)
        .await;
    let external = h.key(&["*"], Trust::External, false).await;

    let (status, listed) = h
        .call(
            "POST",
            "/mcp",
            &agent,
            Some(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|t| t["name"].as_str().expect("name"))
        .collect();
    for name in ["page_read", "page_write", "page_list"] {
        assert!(names.contains(&name), "{names:?}");
    }

    let (failed, written) = h
        .tool(
            &agent,
            "page_write",
            json!({"scope": "agent:a1", "slug": "notes", "title": "Notes", "body": "# A\nFirst."}),
        )
        .await;
    assert!(!failed, "{written}");
    assert_eq!(written["status"], "active");
    assert_eq!(written["version"], 1);
    assert_eq!(written["redacted"], false);

    let (failed, read) = h
        .tool(
            &agent,
            "page_read",
            json!({"scope": "agent:a1", "slug": "notes"}),
        )
        .await;
    assert!(!failed, "{read}");
    assert_eq!(read["body"], "# A\nFirst.");

    let (failed, edited) = h
        .tool(
            &agent,
            "page_write",
            json!({
                "scope": "agent:a1", "slug": "notes", "title": "Notes", "body": "# A\nSecond.",
                "summary": "reworded", "expected_version": 1,
            }),
        )
        .await;
    assert!(!failed, "{edited}");
    assert_eq!(edited["version"], 2);
    let (failed, old) = h
        .tool(
            &agent,
            "page_read",
            json!({"scope": "agent:a1", "slug": "notes", "version": 1}),
        )
        .await;
    assert!(!failed, "{old}");
    assert_eq!(old["status"], "superseded");

    let (failed, error) = h
        .tool(
            &agent,
            "page_write",
            json!({"scope": "agent:a1", "slug": "notes", "title": "N", "body": "x", "expected_version": 1}),
        )
        .await;
    assert!(failed);
    assert_eq!(error["error"]["code"], "version_conflict", "{error}");

    let (failed, pages) = h
        .tool(
            &agent,
            "page_list",
            json!({"scope": "agent:a1", "prefix": "no"}),
        )
        .await;
    assert!(!failed, "{pages}");
    assert_eq!(pages["pages"][0]["slug"], "notes");
    let (failed, error) = h.tool(&agent, "page_list", json!({})).await;
    assert!(failed);
    assert_eq!(error["error"]["code"], "invalid_input", "{error}");
    assert!(error["error"]["message"]
        .as_str()
        .expect("message")
        .contains("scope"));

    // A shared write waits for review, and MCP has no way to approve it.
    let (failed, shared) = h
        .tool(
            &agent,
            "page_write",
            json!({"scope": "shared:team", "slug": "faq", "title": "FAQ", "body": "Q and A"}),
        )
        .await;
    assert!(!failed, "{shared}");
    assert_eq!(shared["status"], "quarantined");
    let (failed, error) = h
        .tool(
            &agent,
            "page_write",
            json!({"scope": "agent:a1", "slug": "x", "title": "X", "body": "x", "approve": true}),
        )
        .await;
    assert!(failed);
    assert_eq!(error["error"]["code"], "invalid_input", "{error}");
    let (failed, error) = h
        .tool(
            &agent,
            "page_read",
            json!({"scope": "shared:team", "slug": "faq"}),
        )
        .await;
    assert!(failed);
    assert_eq!(error["error"]["code"], "not_found", "{error}");

    // Scope and trust, as over REST.
    for (name, arguments) in [
        ("page_read", json!({"scope": "agent:b1", "slug": "notes"})),
        ("page_list", json!({"scope": "agent:b1"})),
        (
            "page_write",
            json!({"scope": "agent:b1", "slug": "n", "title": "N", "body": "b"}),
        ),
    ] {
        let (failed, error) = h.tool(&agent, name, arguments).await;
        assert!(failed, "{name}");
        assert_eq!(error["error"]["code"], "forbidden", "{name}: {error}");
    }
    let (failed, error) = h
        .tool(
            &external,
            "page_write",
            json!({"scope": "agent:a1", "slug": "n", "title": "N", "body": "b"}),
        )
        .await;
    assert!(failed);
    assert_eq!(error["error"]["code"], "forbidden", "{error}");
    let (failed, error) = h.tool(&agent, "page_read", json!({"slug": "notes"})).await;
    assert!(failed);
    assert_eq!(error["error"]["code"], "invalid_input", "{error}");
}

/// Every page call of the SDK against the real server.
pub async fn sdk_page_calls(h: &Harness) {
    let admin = h.key(&["*"], Trust::System, true).await;
    let agent = h.key(&["shared:team"], Trust::Agent, false).await;
    let admin = MemoryOs::new(&h.url, admin, Options::default()).expect("sdk");
    let agent = MemoryOs::new(&h.url, agent, Options::default()).expect("sdk");
    let team = Scope::shared("team");

    let first = admin
        .put_page(
            &team,
            "guide",
            &PageWrite::create("Guide", "# Use\nv1").approve(),
        )
        .await
        .expect("create");
    assert_eq!(first.version.status, PageVersionStatus::Active);
    assert!(!first.redacted);
    let pending = agent
        .put_page(
            &team,
            "guide",
            &PageWrite::update("Guide", "# Use\nv2", 1).summary("clearer"),
        )
        .await
        .expect("edit");
    assert_eq!(pending.version.status, PageVersionStatus::Quarantined);
    let conflict = agent
        .put_page(&team, "guide", &PageWrite::create("Guide", "again"))
        .await
        .expect_err("conflict");
    assert!(
        matches!(conflict, memoryos_sdk::Error::VersionConflict { .. }),
        "{conflict:?}"
    );

    let current = agent.get_page(&team, "guide", None).await.expect("read");
    assert_eq!(current.version, 1);
    let waiting = admin
        .page_quarantine(&["shared:*"], Some(10))
        .await
        .expect("list");
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0].base_body.as_deref(), Some("# Use\nv1"));
    let approved = admin
        .approve_page(pending.version.page_id, 2)
        .await
        .expect("approve");
    assert_eq!(approved.body, "# Use\nv2");

    let third = agent
        .put_page(&team, "guide", &PageWrite::update("Guide", "v3", 2))
        .await
        .expect("third");
    let rejected = admin
        .reject_page(third.version.page_id, third.version.version)
        .await
        .expect("reject");
    assert_eq!(rejected.status, PageVersionStatus::Rejected);

    let listed = agent
        .list_pages(&team, Some("gu"), None)
        .await
        .expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].version, 2);
    let history = agent.page_versions(&team, "guide").await.expect("history");
    assert_eq!(history.len(), 3);
    let old = agent.get_page(&team, "guide", Some(1)).await.expect("old");
    assert_eq!(old.body, "# Use\nv1");

    let recalled = agent
        .recall(team.clone(), "how to use the guide")
        .page_limit(2)
        .await
        .expect("recall");
    assert_eq!(
        recalled.pages.first().map(|hit| hit.slug.as_str()),
        Some("guide")
    );
    let none = agent
        .recall(team.clone(), "how to use the guide")
        .page_limit(0)
        .await
        .expect("recall");
    assert!(none.pages.is_empty());

    let forbidden = agent.delete_page(&team, "guide").await.expect_err("agent");
    assert!(
        matches!(forbidden, memoryos_sdk::Error::Forbidden { .. }),
        "{forbidden:?}"
    );
    admin.delete_page(&team, "guide").await.expect("delete");
    assert!(matches!(
        agent.get_page(&team, "guide", None).await,
        Err(memoryos_sdk::Error::NotFound { .. })
    ));
}

/// Deleting a user's memory deletes their pages and counts them.
pub async fn user_purge_counts_pages(h: &Harness) {
    let admin = h.key(&["*"], Trust::System, true).await;
    for slug in ["a", "b"] {
        let (status, _) = h
            .put(
                &admin,
                "user:u9",
                slug,
                json!({"title": "T", "body": "b", "approve": true}),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
    }
    let (status, report) = h.call("DELETE", "/v1/users/u9/memory", &admin, None).await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["pages_deleted"], 2, "{report}");
    let (_, listed) = h.get(&admin, "/v1/pages?scope=user:u9").await;
    assert_eq!(listed["pages"], json!([]));
}

/// Every scenario, so each suite can run them one test each.
macro_rules! page_scenarios {
    ($harness:path) => {
        page_scenarios!(@one $harness, crud_over_rest);
        page_scenarios!(@one $harness, redaction_is_reported);
        page_scenarios!(@one $harness, scopes_are_enforced);
        page_scenarios!(@one $harness, review_flow);
        page_scenarios!(@one $harness, trust_ceilings);
        page_scenarios!(@one $harness, write_and_delete_trust);
        page_scenarios!(@one $harness, recall_returns_pages);
        page_scenarios!(@one $harness, mcp_page_tools);
        page_scenarios!(@one $harness, sdk_page_calls);
        page_scenarios!(@one $harness, user_purge_counts_pages);
    };
    (@one $harness:path, $name:ident) => {
        #[tokio::test(flavor = "multi_thread")]
        async fn $name() {
            let (harness, _guard) = $harness().await;
            $crate::pages_shared::$name(&harness).await;
        }
    };
}
