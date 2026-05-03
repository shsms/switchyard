//! HTTP-side integration tests. Each test spawns a fresh
//! `TestServer`, hits the live `/api/*` surface with reqwest, and
//! asserts on the JSON shape — covering the boot path that the
//! in-source `tests` module's axum-oneshot pattern doesn't.

mod common;

use common::TestServer;
use serde_json::Value;

const TINY_TOPOLOGY: &str = r#"
(set-microgrid-id 7)
(%make-grid :id 1
            :successors
            (list (%make-meter :id 2 :main t
                               :successors
                               (list (%make-battery :id 3)))))
"#;

async fn json(client: &reqwest::Client, url: String) -> Value {
    client
        .get(&url)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {url}: {e}"))
        .json::<Value>()
        .await
        .unwrap_or_else(|e| panic!("parse {url}: {e}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn topology_endpoint_serves_components_and_connections() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let topo = json(&reqwest::Client::new(), format!("{}/api/topology", s.ui_url)).await;
    let components = topo["components"].as_array().expect("components array");
    let ids: Vec<i64> = components
        .iter()
        .map(|c| c["id"].as_i64().unwrap())
        .collect();
    assert!(ids.contains(&1));
    assert!(ids.contains(&2));
    assert!(ids.contains(&3));
    let connections = topo["connections"].as_array().expect("connections array");
    assert_eq!(connections.len(), 2, "expected grid→meter, meter→battery");
}

#[tokio::test(flavor = "multi_thread")]
async fn eval_endpoint_round_trips_world_state() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{}/api/eval", s.ui_url))
        .body("(world-rename-component 2 \"main\")")
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);

    // Read back via /api/topology to confirm the world updated.
    let topo = json(&client, format!("{}/api/topology", s.ui_url)).await;
    let renamed = topo["components"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == 2)
        .unwrap();
    assert_eq!(renamed["name"], "main");
}

#[tokio::test(flavor = "multi_thread")]
async fn scenario_endpoints_round_trip_via_eval() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let client = reqwest::Client::new();

    // Pre-start: lifecycle is empty.
    let pre = json(&client, format!("{}/api/scenario", s.ui_url)).await;
    assert!(pre["name"].is_null());

    // Start + record an event via /api/eval.
    for body in [
        "(scenario-start \"smoke\")",
        "(scenario-event 'note \"hi\")",
    ] {
        let r = client
            .post(format!("{}/api/eval", s.ui_url))
            .body(body)
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "eval {body} failed: {:?}", r.status());
    }

    let summary = json(&client, format!("{}/api/scenario", s.ui_url)).await;
    assert_eq!(summary["name"], "smoke");
    assert_eq!(summary["event_count"], 1);

    let events = json(&client, format!("{}/api/scenario/events", s.ui_url)).await;
    let arr = events["events"].as_array().unwrap();
    assert_eq!(arr[0]["kind"], "note");

    let report = json(&client, format!("{}/api/scenario/report", s.ui_url)).await;
    assert_eq!(report["main_meter_id"], 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn overrides_endpoint_lists_each_successful_eval() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let client = reqwest::Client::new();

    for body in [
        "(world-rename-component 2 \"a\")",
        "(world-rename-component 2 \"b\")",
    ] {
        client
            .post(format!("{}/api/eval", s.ui_url))
            .body(body)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }

    let overrides = json(&client, format!("{}/api/overrides", s.ui_url)).await;
    let persisted = overrides["persisted"].as_array().unwrap();
    assert_eq!(persisted.len(), 2);
    assert!(persisted[0]["source"].as_str().unwrap().contains("\"a\""));
    assert!(persisted[1]["source"].as_str().unwrap().contains("\"b\""));
}
