//! Live checks for the Graphiti adapter's retirement contract.
//!
//! The reference server's `DELETE /episode/{uuid}` removes only the episode
//! node, so a deployment eligible for this check must expose the cleanup route
//! that calls `Graphiti.remove_episode`. The test is ignored by default
//! because it needs a running deployment, a model/embedding provider, and the
//! cleanup route; CI runs the mock-transport conformance tests instead.
//!
//! Run it with:
//!
//! ```text
//! AIVTUBER_GRAPHITI_ENDPOINT=http://127.0.0.1:8000 \
//! AIVTUBER_GRAPHITI_CLEANUP_PATH='/group/{scope}/episode/{uuid}/cleanup' \
//! cargo test -p aivtuber-adapters --test graphiti_live -- --ignored --nocapture
//! ```

use aivtuber_adapters::{
    GraphitiConfig, GraphitiSidecar, JsonHttpRequestTransport, UreqJsonTransport,
};
use aivtuber_domain::{MemorySidecar, SidecarClaim, SidecarRecallQuery};
use serde_json::{Value, json};
use std::env;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

fn endpoint() -> String {
    env::var("AIVTUBER_GRAPHITI_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:8000".to_owned())
}

fn cleanup_path() -> String {
    env::var("AIVTUBER_GRAPHITI_CLEANUP_PATH")
        .unwrap_or_else(|_| "/group/{scope}/episode/{uuid}/cleanup".to_owned())
}

fn token() -> Option<String> {
    env::var("AIVTUBER_MEMORY_SIDECAR_TOKEN")
        .ok()
        .filter(|token| !token.is_empty())
}

fn scope_token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    format!("graphiti-cleanup-{}-{nanos:x}", std::process::id())
}

fn get_json(transport: &UreqJsonTransport, url: &str, token: Option<&str>) -> Value {
    let response = transport
        .request_json("GET", url, token, &[], REQUEST_TIMEOUT)
        .expect("GET episodes");
    assert!(
        (200..300).contains(&response.status),
        "GET {url} answered {}",
        response.status
    );
    serde_json::from_slice(&response.body).expect("episode list is JSON")
}

fn search_json(
    transport: &UreqJsonTransport,
    url: &str,
    token: Option<&str>,
    scope: &str,
    query: &str,
) -> Value {
    let body = serde_json::to_vec(&json!({
        "group_ids": [scope],
        "query": query,
        "max_facts": 16,
    }))
    .expect("search body");
    let response = transport
        .request_json("POST", url, token, &body, REQUEST_TIMEOUT)
        .expect("POST search");
    assert!(
        (200..300).contains(&response.status),
        "POST {url} answered {}",
        response.status
    );
    serde_json::from_slice(&response.body).expect("search answer is JSON")
}

fn names_episode(fact: &Value, episode: &str) -> bool {
    fact.get("episodes")
        .and_then(Value::as_array)
        .is_some_and(|episodes| {
            episodes.iter().any(|value| {
                value.as_str() == Some(episode)
                    || value.get("uuid").and_then(Value::as_str) == Some(episode)
            })
        })
}

#[test]
#[ignore = "requires a running Graphiti deployment with the cleanup route and provider keys"]
fn retirement_removes_the_episode_and_its_derived_facts() {
    let endpoint = endpoint();
    let token = token();
    let sidecar = GraphitiSidecar::new(
        GraphitiConfig::new(endpoint.clone())
            .with_ingest_timeout(Duration::from_secs(300))
            .with_poll_interval(Duration::from_secs(1))
            .with_cleanup_path(cleanup_path()),
    );
    let transport = UreqJsonTransport::default();
    let scope = scope_token();
    let claim_id = format!("evt-{scope}");
    let text = format!("the retirement probe {scope} must not survive its claim");
    let claim = SidecarClaim {
        claim_id: claim_id.clone(),
        actor: None,
        topic: Some("cleanup-probe".to_owned()),
        text: text.clone(),
        recorded_at_ms: 1_000,
        event_at_ms: 1_000,
    };

    sidecar.open_scope(&scope).expect("open scope");
    let handle = sidecar.retain(&scope, &claim).expect("retain claim");
    let before = sidecar
        .recall(
            &scope,
            &SidecarRecallQuery {
                actor: None,
                query: text.clone(),
                limit: 12,
                now_ms: 2_000,
            },
        )
        .expect("recall before retirement");
    assert!(
        before.iter().any(|item| item.claim_id == claim_id),
        "the retained claim must be recallable before retirement"
    );

    sidecar
        .retire(&scope, &handle)
        .expect("cleanup-capable retirement");

    let after = sidecar
        .recall(
            &scope,
            &SidecarRecallQuery {
                actor: None,
                query: text.clone(),
                limit: 12,
                now_ms: 2_000,
            },
        )
        .expect("recall after retirement");
    assert!(
        !after.iter().any(|item| item.claim_id == claim_id),
        "a retired claim must not be recalled"
    );

    let base = endpoint.trim_end_matches('/');
    let episodes = get_json(
        &transport,
        &format!("{base}/episodes/{scope}?last_n=256"),
        token.as_deref(),
    );
    let episodes = episodes
        .get("episodes")
        .and_then(Value::as_array)
        .or_else(|| episodes.as_array())
        .expect("episode list");
    assert!(
        !episodes
            .iter()
            .any(|episode| episode.get("uuid").and_then(Value::as_str) == Some(handle.as_str())),
        "the retired episode node must be gone"
    );

    let facts = search_json(
        &transport,
        &format!("{base}/search"),
        token.as_deref(),
        &scope,
        &text,
    );
    let facts = facts.get("facts").and_then(Value::as_array).expect("facts");
    assert!(
        !facts
            .iter()
            .any(|fact| names_episode(fact, handle.as_str())),
        "no derived fact may still name the retired episode"
    );

    sidecar.close_scope(&scope).expect("close scope");
}
