//! Insight and reflect scenarios (SPEC sections 6 and 8, Insights) over REST, MCP and the SDK.
//! Both suites run them: `integration` on Postgres and `integration-sqlite` on a SQLite file. Each
//! scenario uses its own tenant through the pages harness, so it needs nothing from any other.

use memoryos_core::Trust;
use memoryos_sdk::{
    InsightLevel, InsightSourceKind, InsightSourceRef, InsightStatus, InsightWrite, MemoryOs,
    Options, Scope,
};
use reqwest::StatusCode;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::pages_shared::Harness;

#[track_caller]
fn assert_error(status: StatusCode, body: &Value, expected: u16, code: &str) {
    assert_eq!(status.as_u16(), expected, "{body}");
    assert_eq!(body["error"]["code"], code, "{body}");
    assert!(body["error"]["message"].is_string(), "{body}");
}

fn id_of(body: &Value) -> Uuid {
    body["id"]
        .as_str()
        .unwrap_or_else(|| panic!("no id in {body}"))
        .parse()
        .expect("uuid")
}

fn fact(id: Uuid) -> Value {
    json!({"kind": "fact", "id": id})
}

fn facts(ids: &[Uuid]) -> Value {
    Value::Array(ids.iter().copied().map(fact).collect())
}

fn insight(scope: &str, level: &str, statement: &str, sources: Value) -> Value {
    json!({"scope": scope, "level": level, "statement": statement, "sources": sources})
}

/// Commits one fact per statement about `subject`, each with its own predicate so no
/// single-valued rule replaces another, approves any that wait for review, and returns their ids
/// in order.
async fn commit(h: &Harness, token: &str, scope: &str, subject: &str, texts: &[&str]) -> Vec<Uuid> {
    let facts: Vec<Value> = texts
        .iter()
        .enumerate()
        .map(|(i, text)| {
            json!({
                "statement": text,
                "subject": {"name": subject, "kind": "person"},
                "predicate": format!("noted_{i}"),
            })
        })
        .collect();
    let (status, body) = h
        .call(
            "POST",
            "/v1/commits",
            token,
            Some(json!({
                "scope": scope,
                "source": "chat",
                "content": texts.join(" "),
                "facts": facts,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let mut ids = Vec::new();
    for committed in body["facts"].as_array().expect("facts") {
        let id = id_of(committed);
        // Commits to user and shared scopes wait for review, and an insight cites active facts.
        if committed["status"] == "quarantined" {
            let admin = h.key(&["*"], Trust::System, true).await;
            let (status, approved) = h
                .call(
                    "POST",
                    &format!("/v1/quarantine/{id}/approve"),
                    &admin,
                    None,
                )
                .await;
            assert_eq!(status, StatusCode::OK, "{approved}");
        } else {
            assert_eq!(committed["status"], "active", "{body}");
        }
        ids.push(id);
    }
    ids
}

async fn write(h: &Harness, token: &str, body: Value) -> (StatusCode, Value) {
    h.call("POST", "/v1/insights", token, Some(body)).await
}

/// Writes and returns the response body, asserting success.
async fn written(h: &Harness, token: &str, body: Value) -> Value {
    let (status, reply) = write(h, token, body).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    reply
}

/// Write, read, list, supersede, masking and input checks in an agent's own scope.
pub async fn write_read_list_over_rest(h: &Harness) {
    let agent = h.key(&["agent:a1"], Trust::Agent, false).await;
    let ids = commit(
        h,
        &agent,
        "agent:a1",
        "Ada",
        &[
            "Ada rejected the plugin system",
            "Ada asked for fewer moving parts",
            "Ada chose one binary",
        ],
    )
    .await;

    let reply = written(
        h,
        &agent,
        insight(
            "agent:a1",
            "context",
            "Ada builds small, simple systems",
            facts(&ids[..2]),
        ),
    )
    .await;
    assert_eq!(reply["redacted"], false);
    let first = &reply["insight"];
    assert_eq!(first["status"], "active", "{reply}");
    assert_eq!(first["trust"], "agent");
    assert_eq!(first["level"], "context");
    assert_eq!(first["scope"], "agent:a1");
    assert!(first["written_by"]
        .as_str()
        .expect("written_by")
        .starts_with("key:"));
    let first_id = id_of(first);

    let (status, read) = h.get(&agent, &format!("/v1/insights/{first_id}")).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(read["statement"], "Ada builds small, simple systems");
    let views = read["source_views"].as_array().expect("source_views");
    assert_eq!(views.len(), 2);
    assert!(views
        .iter()
        .all(|v| v["current"] == true && v["kind"] == "fact"));
    assert_eq!(read["history"], json!([]));

    // A planted secret is masked before it is stored or embedded.
    let reply = written(
        h,
        &agent,
        insight(
            "agent:a1",
            "context",
            "Ada deploys with password=hunter2 from one binary",
            facts(&ids[1..]),
        ),
    )
    .await;
    assert_eq!(reply["redacted"], true);
    let masked = reply["insight"]["statement"].as_str().expect("statement");
    assert!(
        masked.contains("[redacted]") && !masked.contains("hunter2"),
        "{masked}"
    );
    let (_, read) = h
        .get(
            &agent,
            &format!("/v1/insights/{}", id_of(&reply["insight"])),
        )
        .await;
    assert!(!read.to_string().contains("hunter2"), "{read}");

    // A superseding write closes the old insight and lists it in its history.
    let reply = written(
        h,
        &agent,
        json!({
            "scope": "agent:a1",
            "level": "context",
            "statement": "Ada builds small systems from one binary",
            "sources": facts(&ids),
            "supersedes": first_id,
        }),
    )
    .await;
    let second_id = id_of(&reply["insight"]);
    assert_eq!(reply["insight"]["status"], "active");
    let (_, old) = h.get(&agent, &format!("/v1/insights/{first_id}")).await;
    assert_eq!(old["status"], "superseded", "{old}");
    assert_eq!(old["superseded_by"], second_id.to_string());
    let (_, new) = h.get(&agent, &format!("/v1/insights/{second_id}")).await;
    assert_eq!(new["history"][0]["id"], first_id.to_string(), "{new}");

    let (status, listed) = h.get(&agent, "/v1/insights?scopes=agent:a1").await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(listed["insights"].as_array().expect("insights").len(), 3);
    let (_, active) = h
        .get(&agent, "/v1/insights?scopes=agent:a1&status=active")
        .await;
    assert_eq!(active["insights"].as_array().expect("insights").len(), 2);
    let (_, guidance) = h
        .get(&agent, "/v1/insights?scopes=agent:a1&level=guidance")
        .await;
    assert_eq!(guidance["insights"], json!([]));
    let (_, limited) = h.get(&agent, "/v1/insights?scopes=agent:a1&limit=1").await;
    assert_eq!(limited["insights"].as_array().expect("insights").len(), 1);
    for bad in [
        "/v1/insights",
        "/v1/insights?scopes=agent:a1&limit=0",
        "/v1/insights?scopes=agent:a1&limit=501",
        "/v1/insights?scopes=agent:a1&level=wisdom",
        "/v1/insights?scopes=agent:a1&status=gone",
        "/v1/insights?scopes=team:x",
        "/v1/insights/not-a-uuid",
    ] {
        let (status, body) = h.get(&agent, bad).await;
        assert_error(status, &body, 400, "invalid_input");
    }

    // Malformed writes are refused before the store.
    let long = "x".repeat(1001);
    for body in [
        insight("agent:a1", "wisdom", "s", facts(&ids[..2])),
        insight("agent:a1", "context", "", facts(&ids[..2])),
        insight("agent:a1", "context", "   ", facts(&ids[..2])),
        insight("agent:a1", "context", &long, facts(&ids[..2])),
        insight("agent:a1", "context", "one source", facts(&ids[..1])),
        insight("agent:a1", "context", "a repeat", facts(&[ids[0], ids[0]])),
        insight(
            "agent:a1",
            "context",
            "s",
            json!([{"kind": "page", "id": ids[0]}]),
        ),
        insight("team:x", "context", "s", facts(&ids[..2])),
        insight(
            "agent:a1",
            "context",
            "unknown",
            facts(&[ids[0], Uuid::now_v7()]),
        ),
        json!({"scope": "agent:a1", "level": "context", "statement": "s",
               "sources": facts(&ids[..2]), "trust": "system"}),
        json!({"scope": "agent:a1", "level": "context", "statement": "s"}),
    ] {
        let (status, reply) = write(h, &agent, body.clone()).await;
        assert_error(status, &reply, 400, "invalid_input");
    }
    let (status, reply) = h
        .get(&agent, &format!("/v1/insights/{}", Uuid::now_v7()))
        .await;
    assert_error(status, &reply, 404, "not_found");
}

/// Scope checks on every route, and admin-only review.
pub async fn scopes_and_review_are_enforced(h: &Harness) {
    let agent = h.key(&["agent:a1"], Trust::Agent, false).await;
    let other = h.key(&["agent:b"], Trust::System, false).await;
    let ids = commit(
        h,
        &agent,
        "agent:a1",
        "Ada",
        &["Ada likes tea", "Ada likes green tea"],
    )
    .await;
    let reply = written(
        h,
        &agent,
        insight("agent:a1", "context", "Ada is a tea drinker", facts(&ids)),
    )
    .await;
    let id = id_of(&reply["insight"]);

    let (status, body) = write(
        h,
        &agent,
        insight("agent:b", "context", "elsewhere", facts(&ids)),
    )
    .await;
    assert_error(status, &body, 403, "forbidden");
    let (status, body) = h.get(&other, "/v1/insights?scopes=agent:a1").await;
    assert_error(status, &body, 403, "forbidden");
    // A row in a scope the key cannot read looks like one that does not exist.
    for (method, path, body) in [
        ("GET", format!("/v1/insights/{id}"), None),
        (
            "POST",
            format!("/v1/insights/{id}/confirm"),
            Some(json!({"sources": facts(&ids)})),
        ),
        (
            "POST",
            format!("/v1/insights/{id}/retire"),
            Some(json!({"reason": "wrong"})),
        ),
    ] {
        let (status, reply) = h.call(method, &path, &other, body).await;
        assert_error(status, &reply, 404, "not_found");
    }
    let (status, body) = h
        .call(
            "POST",
            "/v1/reflect/batch",
            &other,
            Some(json!({"scope": "agent:a1"})),
        )
        .await;
    assert_error(status, &body, 403, "forbidden");
    let (status, body) = h
        .call(
            "POST",
            "/v1/reflect/decline",
            &other,
            Some(json!({"scope": "agent:a1", "topic_key": Uuid::now_v7(), "fact_set": "a".repeat(64)})),
        )
        .await;
    assert_error(status, &body, 403, "forbidden");

    // Review is admin only.
    for (method, path) in [
        ("GET", "/v1/insights/quarantine?scopes=agent:a1".to_owned()),
        ("POST", format!("/v1/insights/quarantine/{id}/approve")),
        ("POST", format!("/v1/insights/quarantine/{id}/reject")),
    ] {
        let (status, body) = h.call(method, &path, &agent, None).await;
        assert_error(status, &body, 403, "forbidden");
    }
    let admin = h.key(&["*"], Trust::System, true).await;
    for bad in [
        "/v1/insights/quarantine",
        "/v1/insights/quarantine?scopes=agent:a1&limit=0",
        "/v1/insights/quarantine?scopes=agent:a1&limit=201",
        "/v1/insights/quarantine?scopes=team:*",
    ] {
        let (status, body) = h.get(&admin, bad).await;
        assert_error(status, &body, 400, "invalid_input");
    }
    // Approving an insight that is not quarantined.
    let (status, body) = h
        .call(
            "POST",
            &format!("/v1/insights/quarantine/{id}/approve"),
            &admin,
            None,
        )
        .await;
    assert_error(status, &body, 400, "invalid_input");
}

/// The writer's trust is the key's `max_trust`, capped at `agent`.
pub async fn trust_comes_from_the_key(h: &Harness) {
    let agent = h.key(&["agent:a1"], Trust::Agent, false).await;
    let system = h.key(&["agent:a1"], Trust::System, true).await;
    let external = h.key(&["agent:a1"], Trust::External, false).await;
    let ids = commit(
        h,
        &agent,
        "agent:a1",
        "Ada",
        &["Ada runs Linux", "Ada runs NixOS"],
    )
    .await;

    let reply = written(
        h,
        &system,
        insight("agent:a1", "context", "Ada uses Linux at home", facts(&ids)),
    )
    .await;
    assert_eq!(reply["insight"]["trust"], "agent", "{reply}");
    assert_eq!(reply["insight"]["status"], "active");

    let reply = written(
        h,
        &external,
        insight(
            "agent:a1",
            "context",
            "Ada uses Linux everywhere",
            facts(&ids),
        ),
    )
    .await;
    assert_eq!(reply["insight"]["trust"], "external", "{reply}");
    assert_eq!(reply["insight"]["status"], "quarantined");
    let pending = id_of(&reply["insight"]);

    // An admin key below the insight's trust cannot review it.
    let low_admin = h.key(&["agent:a1"], Trust::External, true).await;
    let agent_trust = id_of(&written(
        h,
        &agent,
        json!({"scope": "agent:a1", "level": "guidance", "statement": "Suggest Linux tools to Ada",
               "sources": facts(&ids)}),
    )
    .await["insight"]);
    let (status, body) = h
        .call(
            "POST",
            &format!("/v1/insights/{agent_trust}/retire"),
            &low_admin,
            Some(json!({"reason": "no"})),
        )
        .await;
    assert_error(status, &body, 403, "forbidden");
    let (status, rejected) = h
        .call(
            "POST",
            &format!("/v1/insights/quarantine/{pending}/reject"),
            &low_admin,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{rejected}");
    assert_eq!(rejected["status"], "rejected");
}

/// Guidance in a user scope waits for an admin; approval activates it, rejection closes it.
pub async fn guidance_in_a_user_scope_waits_for_review(h: &Harness) {
    let user = h.key(&["user:u1"], Trust::User, false).await;
    let agent = h.key(&["user:u1"], Trust::Agent, false).await;
    let admin = h.key(&["*"], Trust::System, true).await;
    let ids = commit(
        h,
        &user,
        "user:u1",
        "Ada",
        &[
            "Ada asked for failure modes first",
            "Ada asked who owns the service",
            "Ada asked how it is observed",
        ],
    )
    .await;

    let reply = written(
        h,
        &user,
        insight(
            "user:u1",
            "guidance",
            "Before proposing an abstraction to Ada, cover failure modes first",
            facts(&ids),
        ),
    )
    .await;
    let guidance = &reply["insight"];
    assert_eq!(guidance["status"], "quarantined", "{reply}");
    assert_eq!(guidance["trust"], "agent");
    let guidance_id = id_of(guidance);
    let reply = written(
        h,
        &agent,
        insight(
            "user:u1",
            "context",
            "Ada reviews designs carefully",
            facts(&ids[..2]),
        ),
    )
    .await;
    assert_eq!(reply["insight"]["status"], "quarantined", "{reply}");
    let context_id = id_of(&reply["insight"]);

    let (status, listed) = h.get(&admin, "/v1/insights/quarantine?scopes=user:*").await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let pending: Vec<Uuid> = listed["insights"]
        .as_array()
        .expect("insights")
        .iter()
        .map(id_of)
        .collect();
    assert_eq!(pending, [guidance_id, context_id], "oldest first");
    let (_, one) = h
        .get(&admin, "/v1/insights/quarantine?scopes=user:u1&limit=1")
        .await;
    assert_eq!(one["insights"].as_array().expect("insights").len(), 1);

    let (status, approved) = h
        .call(
            "POST",
            &format!("/v1/insights/quarantine/{guidance_id}/approve"),
            &admin,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{approved}");
    assert_eq!(approved["status"], "active");
    let (status, rejected) = h
        .call(
            "POST",
            &format!("/v1/insights/quarantine/{context_id}/reject"),
            &admin,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{rejected}");
    assert_eq!(rejected["status"], "rejected");
    let (_, listed) = h.get(&admin, "/v1/insights/quarantine?scopes=user:*").await;
    assert_eq!(listed["insights"], json!([]));

    // An agent cannot withdraw what it could not have added without review.
    let (status, body) = h
        .call(
            "POST",
            &format!("/v1/insights/{guidance_id}/retire"),
            &agent,
            Some(json!({"reason": "outdated"})),
        )
        .await;
    assert_error(status, &body, 403, "forbidden");
    let (status, retired) = h
        .call(
            "POST",
            &format!("/v1/insights/{guidance_id}/retire"),
            &user,
            Some(json!({"reason": "outdated"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{retired}");
    assert_eq!(retired["status"], "retired");
}

/// A stale insight is confirmed with current sources; retiring takes a reason.
pub async fn confirm_and_retire(h: &Harness) {
    let agent = h.key(&["agent:a1"], Trust::Agent, false).await;
    let agent = agent.as_str();
    let ids = commit(
        h,
        agent,
        "agent:a1",
        "Ada",
        &[
            "Ada uses Postgres",
            "Ada uses pgvector",
            "Ada uses SQLite for tests",
        ],
    )
    .await;
    let reply = written(
        h,
        agent,
        insight(
            "agent:a1",
            "context",
            "Ada stores data in SQL",
            facts(&ids[..2]),
        ),
    )
    .await;
    let id = id_of(&reply["insight"]);
    let confirm = |sources: Value| {
        let path = format!("/v1/insights/{id}/confirm");
        async move {
            h.call("POST", &path, agent, Some(json!({"sources": sources})))
                .await
        }
    };

    let (status, body) = confirm(facts(&[ids[1], ids[2]])).await;
    assert_error(status, &body, 400, "invalid_input");

    let (status, body) = h
        .call(
            "POST",
            &format!("/v1/facts/{}/invalidate", ids[0]),
            agent,
            Some(json!({"reason": "moved off Postgres"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, stale) = h.get(agent, &format!("/v1/insights/{id}")).await;
    assert_eq!(stale["status"], "stale", "{stale}");
    assert_eq!(stale["stale_reason"][0]["change"], "invalidated");
    assert_eq!(stale["stale_reason"][0]["id"], ids[0].to_string());

    for bad in [
        facts(&[ids[0], ids[2]]),
        facts(&ids[2..]),
        json!([{"kind": "fact"}]),
    ] {
        let (status, body) = confirm(bad).await;
        assert_error(status, &body, 400, "invalid_input");
    }
    let (status, confirmed) = confirm(facts(&[ids[1], ids[2]])).await;
    assert_eq!(status, StatusCode::OK, "{confirmed}");
    assert_eq!(confirmed["status"], "active");
    assert_eq!(confirmed["stale_reason"], Value::Null);
    assert_eq!(confirmed["sources"], facts(&[ids[1], ids[2]]));

    let path = format!("/v1/insights/{id}/retire");
    for bad in [
        json!({"reason": ""}),
        json!({"reason": "  "}),
        json!({"reason": "x".repeat(501)}),
        json!({}),
    ] {
        let (status, body) = h.call("POST", &path, agent, Some(bad)).await;
        assert_error(status, &body, 400, "invalid_input");
    }
    let (status, retired) = h
        .call(
            "POST",
            &path,
            agent,
            Some(json!({"reason": "no longer true"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{retired}");
    assert_eq!(retired["status"], "retired");
    assert!(retired["valid_to"].is_string());
    let (status, body) = h
        .call("POST", &path, agent, Some(json!({"reason": "again"})))
        .await;
    assert_error(status, &body, 400, "invalid_input");
}

/// Recall returns active insights apart from facts, and skips them for time travel.
pub async fn recall_returns_insights(h: &Harness) {
    let agent = h.key(&["agent:a1"], Trust::Agent, false).await;
    let agent = agent.as_str();
    let ids = commit(
        h,
        agent,
        "agent:a1",
        "Ada",
        &["Ada bikes to work", "Ada bikes on weekends"],
    )
    .await;
    let reply = written(
        h,
        agent,
        insight(
            "agent:a1",
            "understanding",
            "Ada loves cycling",
            facts(&ids),
        ),
    )
    .await;
    let id = id_of(&reply["insight"]);
    let recall = |extra: Value| {
        let mut body = json!({"scopes": ["agent:a1"], "query": "Ada loves cycling"});
        for (key, value) in extra.as_object().expect("object") {
            body[key] = value.clone();
        }
        async move { h.call("POST", "/v1/recall", agent, Some(body)).await }
    };

    let (status, found) = recall(json!({})).await;
    assert_eq!(status, StatusCode::OK, "{found}");
    assert_eq!(
        found["insights"][0]["insight"]["id"],
        id.to_string(),
        "{found}"
    );
    assert!(found["insights"][0]["score"].is_number());
    assert_eq!(found["insights_skipped"], Value::Null);
    let (_, off) = recall(json!({"insight_limit": 0})).await;
    assert_eq!(off["insights"], json!([]));
    let (_, past) = recall(json!({"as_of": "2020-01-01T00:00:00Z"})).await;
    assert_eq!(past["insights"], json!([]));
    assert_eq!(past["insights_skipped"], "time_travel");
    let (status, body) = recall(json!({"insight_limit": 11})).await;
    assert_error(status, &body, 400, "invalid_input");
}

/// Stale insights first, then topics; a decline hides a topic until its facts change.
pub async fn reflect_batch_and_decline(h: &Harness) {
    let agent = h.key(&["agent:a1"], Trust::Agent, false).await;
    let agent = agent.as_str();
    let bob = commit(
        h,
        agent,
        "agent:a1",
        "Bob",
        &["Bob writes Go", "Bob writes Rust"],
    )
    .await;
    let reply = written(
        h,
        agent,
        insight(
            "agent:a1",
            "context",
            "Bob is a systems programmer",
            facts(&bob),
        ),
    )
    .await;
    let stale_id = id_of(&reply["insight"]);
    let (status, body) = h
        .call(
            "POST",
            &format!("/v1/facts/{}/invalidate", bob[0]),
            agent,
            Some(json!({"reason": "wrong"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ada = commit(
        h,
        agent,
        "agent:a1",
        "Ada",
        &["Ada drinks tea", "Ada reads novels", "Ada plays chess"],
    )
    .await;

    let batch =
        |body: Value| async move { h.call("POST", "/v1/reflect/batch", agent, Some(body)).await };
    let (status, found) = batch(json!({"scope": "agent:a1"})).await;
    assert_eq!(status, StatusCode::OK, "{found}");
    assert_eq!(found["stale"][0]["id"], stale_id.to_string(), "{found}");
    assert!(found["stale"][0]["source_views"].is_array());
    let topics = found["topics"].as_array().expect("topics");
    assert_eq!(topics.len(), 1, "{found}");
    let topic = &topics[0];
    assert_eq!(topic["subject"], "Ada");
    let mut offered: Vec<Uuid> = topic["facts"]
        .as_array()
        .expect("facts")
        .iter()
        .map(id_of)
        .collect();
    offered.sort();
    let mut expected = ada.clone();
    expected.sort();
    assert_eq!(offered, expected);
    let fact_set = topic["fact_set"].as_str().expect("fact_set").to_owned();
    let topic_key = topic["topic_key"].clone();

    let (_, one) = batch(json!({"scope": "agent:a1", "limit": 1})).await;
    assert_eq!(one["stale"].as_array().expect("stale").len(), 1);
    assert_eq!(one["topics"], json!([]));
    for bad in [
        json!({"scope": "agent:a1", "limit": 0}),
        json!({"scope": "agent:a1", "limit": 21}),
        json!({"scope": "team:x"}),
        json!({}),
        json!({"scope": "agent:a1", "min_facts": 1}),
    ] {
        let (status, body) = batch(bad).await;
        assert_error(status, &body, 400, "invalid_input");
    }

    let decline = |body: Value| async move {
        h.call("POST", "/v1/reflect/decline", agent, Some(body))
            .await
    };
    for bad in [
        json!({"scope": "agent:a1", "topic_key": topic_key, "fact_set": "abc"}),
        json!({"scope": "agent:a1", "topic_key": topic_key, "fact_set": fact_set.to_uppercase()}),
        json!({"scope": "agent:a1", "topic_key": "nope", "fact_set": fact_set}),
        json!({"scope": "agent:a1", "fact_set": fact_set}),
    ] {
        let (status, body) = decline(bad).await;
        assert_error(status, &body, 400, "invalid_input");
    }
    let (status, body) =
        decline(json!({"scope": "agent:a1", "topic_key": topic_key, "fact_set": fact_set})).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (_, after) = batch(json!({"scope": "agent:a1"})).await;
    assert_eq!(after["topics"], json!([]), "{after}");
    assert_eq!(after["stale"].as_array().expect("stale").len(), 1);

    // A new fact changes the fact set, so the topic comes back.
    commit(h, agent, "agent:a1", "Ada", &["Ada knits scarves"]).await;
    let (_, again) = batch(json!({"scope": "agent:a1"})).await;
    assert_eq!(again["topics"][0]["subject"], "Ada", "{again}");
    assert_ne!(again["topics"][0]["fact_set"], fact_set.as_str());
}

/// The six insight tools and recall's insights over MCP, under the same checks as REST.
pub async fn mcp_insight_tools(h: &Harness) {
    let agent = h.key(&["agent:a1", "user:u1"], Trust::Agent, false).await;
    let ids = commit(
        h,
        &agent,
        "agent:a1",
        "Ada",
        &[
            "Ada writes tests first",
            "Ada keeps tests fast",
            "Ada runs tests in CI",
        ],
    )
    .await;

    let (failed, written) = h
        .tool(
            &agent,
            "insight_write",
            json!({"scope": "agent:a1", "level": "understanding",
                   "statement": "Ada values fast tests; token=abc123secret",
                   "sources": facts(&ids[..2])}),
        )
        .await;
    assert!(!failed, "{written}");
    assert_eq!(written["redacted"], true);
    assert_eq!(written["insight"]["status"], "active");
    assert!(!written.to_string().contains("abc123secret"));
    let id = id_of(&written["insight"]);

    let (failed, read) = h.tool(&agent, "insight_read", json!({"id": id})).await;
    assert!(!failed, "{read}");
    assert_eq!(read["source_views"].as_array().expect("views").len(), 2);

    let (failed, recalled) = h
        .tool(
            &agent,
            "recall",
            json!({"scopes": ["agent:a1"], "query": "Ada values fast tests", "insight_limit": 2}),
        )
        .await;
    assert!(!failed, "{recalled}");
    assert_eq!(recalled["insights"][0]["insight"]["id"], id.to_string());

    let (failed, error) = h
        .tool(
            &agent,
            "insight_confirm",
            json!({"id": id, "sources": facts(&ids[1..])}),
        )
        .await;
    assert!(failed);
    assert_eq!(error["error"]["code"], "invalid_input", "{error}");

    let (failed, error) = h
        .tool(
            &agent,
            "insight_write",
            json!({"scope": "agent:b", "level": "context", "statement": "s",
                   "sources": facts(&ids[..2])}),
        )
        .await;
    assert!(failed);
    assert_eq!(error["error"]["code"], "forbidden", "{error}");
    let (failed, error) = h
        .tool(
            &agent,
            "insight_write",
            json!({"scope": "agent:a1", "level": "context", "statement": "s",
                   "sources": facts(&ids[..2]), "approve": true}),
        )
        .await;
    assert!(failed);
    assert_eq!(error["error"]["code"], "invalid_input", "{error}");

    let (failed, batch) = h
        .tool(
            &agent,
            "reflect_batch",
            json!({"scope": "agent:a1", "limit": 3}),
        )
        .await;
    assert!(!failed, "{batch}");
    assert!(
        batch["stale"].is_array() && batch["topics"].is_array(),
        "{batch}"
    );
    let (failed, error) = h
        .tool(
            &agent,
            "reflect_batch",
            json!({"scope": "agent:a1", "limit": 50}),
        )
        .await;
    assert!(failed);
    assert_eq!(error["error"]["code"], "invalid_input", "{error}");
    let (failed, declined) = h
        .tool(
            &agent,
            "reflect_decline",
            json!({"scope": "agent:a1", "topic_key": Uuid::now_v7(), "fact_set": "0".repeat(64)}),
        )
        .await;
    assert!(!failed, "{declined}");

    let (failed, retired) = h
        .tool(
            &agent,
            "insight_retire",
            json!({"id": id, "reason": "changed"}),
        )
        .await;
    assert!(!failed, "{retired}");
    assert_eq!(retired["status"], "retired");

    // An agent writing into a user scope waits for review.
    let user = h.key(&["user:u1"], Trust::User, false).await;
    let user_ids = commit(
        h,
        &user,
        "user:u1",
        "Ada",
        &["Ada likes maps", "Ada likes atlases"],
    )
    .await;
    let (failed, pending) = h
        .tool(
            &agent,
            "insight_write",
            json!({"scope": "user:u1", "level": "context", "statement": "Ada likes geography",
                   "sources": facts(&user_ids)}),
        )
        .await;
    assert!(!failed, "{pending}");
    assert_eq!(pending["insight"]["status"], "quarantined");
}

/// The SDK's insight calls against a real server.
pub async fn sdk_insight_calls(h: &Harness) {
    let agent_token = h
        .key(&["agent:a1", "shared:team"], Trust::Agent, false)
        .await;
    let admin = h.key(&["*"], Trust::System, true).await;
    let user = h.key(&["shared:team"], Trust::User, false).await;
    let ids = commit(
        h,
        &agent_token,
        "agent:a1",
        "Ada",
        &["Ada ships on Fridays", "Ada ships small changes"],
    )
    .await;
    let team_ids = commit(
        h,
        &user,
        "shared:team",
        "Team",
        &["Team pairs daily", "Team rotates on call"],
    )
    .await;
    let agent = MemoryOs::new(&h.url, agent_token, Options::default()).expect("sdk");
    let admin = MemoryOs::new(&h.url, admin, Options::default()).expect("sdk");
    let refs = |ids: &[Uuid]| -> Vec<InsightSourceRef> {
        ids.iter()
            .map(|&id| InsightSourceRef {
                kind: InsightSourceKind::Fact,
                id,
            })
            .collect()
    };
    let mine = Scope::agent("a1");
    let team = Scope::shared("team");

    let written = agent
        .write_insight(&InsightWrite::new(
            mine.clone(),
            InsightLevel::Context,
            "Ada ships often in small steps",
            refs(&ids),
        ))
        .await
        .expect("write");
    assert_eq!(written.insight.status, InsightStatus::Active);
    assert_eq!(written.insight.trust, Trust::Agent);
    assert!(!written.redacted);
    let id = written.insight.id;

    let detail = agent.get_insight(id).await.expect("read");
    assert_eq!(detail.insight.statement, "Ada ships often in small steps");
    assert_eq!(detail.source_views.len(), 2);
    let listed = agent
        .list_insights(
            std::slice::from_ref(&mine),
            Some(InsightLevel::Context),
            None,
            None,
        )
        .await
        .expect("list");
    assert_eq!(listed.len(), 1);
    let recalled = agent
        .recall(mine.clone(), "Ada ships often")
        .insight_limit(1)
        .await
        .expect("recall");
    assert_eq!(recalled.insights[0].insight.id, id);
    assert!(recalled.insights_skipped.is_none());

    let refused = agent.confirm_insight(id, &refs(&ids)).await;
    assert!(
        matches!(refused, Err(memoryos_sdk::Error::InvalidInput { .. })),
        "{refused:?}"
    );

    let pending = agent
        .write_insight(&InsightWrite::new(
            team.clone(),
            InsightLevel::Context,
            "The team works closely together",
            refs(&team_ids),
        ))
        .await
        .expect("write team");
    assert_eq!(pending.insight.status, InsightStatus::Quarantined);
    let queue = admin
        .insight_quarantine(&["shared:*"], None)
        .await
        .expect("quarantine");
    assert_eq!(queue.len(), 1);
    assert!(agent.insight_quarantine(&["shared:*"], None).await.is_err());
    let approved = admin
        .approve_insight(pending.insight.id)
        .await
        .expect("approve");
    assert_eq!(approved.status, InsightStatus::Active);
    assert!(admin.reject_insight(pending.insight.id).await.is_err());

    let batch = agent.reflect_batch(&mine, Some(5)).await.expect("batch");
    assert!(
        batch.stale.is_empty() && batch.topics.is_empty(),
        "{batch:?}"
    );
    agent
        .reflect_decline(&mine, Uuid::now_v7(), &"f".repeat(64))
        .await
        .expect("decline");

    let retired = agent.retire_insight(id, "stopped").await.expect("retire");
    assert_eq!(retired.status, InsightStatus::Retired);
}

macro_rules! insight_scenarios {
    ($harness:path) => {
        insight_scenarios!(@one $harness, write_read_list_over_rest);
        insight_scenarios!(@one $harness, scopes_and_review_are_enforced);
        insight_scenarios!(@one $harness, trust_comes_from_the_key);
        insight_scenarios!(@one $harness, guidance_in_a_user_scope_waits_for_review);
        insight_scenarios!(@one $harness, confirm_and_retire);
        insight_scenarios!(@one $harness, recall_returns_insights);
        insight_scenarios!(@one $harness, reflect_batch_and_decline);
        insight_scenarios!(@one $harness, mcp_insight_tools);
        insight_scenarios!(@one $harness, sdk_insight_calls);
    };
    (@one $harness:path, $name:ident) => {
        #[tokio::test(flavor = "multi_thread")]
        async fn $name() {
            let (harness, _guard) = $harness().await;
            $crate::insights_shared::$name(&harness).await;
        }
    };
}
