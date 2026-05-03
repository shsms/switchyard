use super::*;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use std::io::Write;
use tower::ServiceExt;

/// Boots a `Config` against a freshly-written tiny config file
/// holding `body`, so the live tulisp ctx + World are wired up the
/// same way the binary wires them. Returns the Config; caller
/// composes a router with it.
///
/// Each call gets its own unique subdirectory under `temp_dir()`
/// so concurrent test runs don't stomp each other's config.lisp
/// (cargo runs the lib test suite multi-threaded by default).
async fn config_with(body: &str) -> Config {
    // tulisp-async's executor needs a tokio runtime in scope; we
    // already have one via #[tokio::test], so Config::new works.
    let mut p = std::env::temp_dir();
    p.push(format!(
        "switchyard-ui-{}-{}",
        std::process::id(),
        // Counter — even if SystemTime resolves the same nanos for
        // two near-simultaneous tests, the AtomicU64 disambiguates.
        UNIQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    ));
    std::fs::create_dir_all(&p).unwrap();
    let path = p.join("config.lisp");
    write!(std::fs::File::create(&path).unwrap(), "{body}").unwrap();
    Config::new(path.to_str().unwrap())
}

static UNIQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// One-shot a request and return (status, body). axum's `oneshot`
/// avoids binding a real port.
async fn call(config: Config, req: Request<Body>) -> (StatusCode, Vec<u8>) {
    let resp = router(config).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, bytes.to_vec())
}

fn get(path: &str) -> Request<Body> {
    Request::builder().uri(path).body(Body::empty()).unwrap()
}

fn post(path: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(path)
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn index_serves_embedded_shell() {
    let cfg = config_with("").await;
    let (status, body) = call(cfg, get("/")).await;
    assert_eq!(status, StatusCode::OK);
    let s = String::from_utf8_lossy(&body);
    assert!(s.contains("<title>switchyard</title>"));
    assert!(s.contains("/assets/app.js"));
}

#[tokio::test]
async fn asset_route_serves_embedded_files() {
    let cfg = config_with("").await;
    let (status, body) = call(cfg, get("/assets/app.js")).await;
    assert_eq!(status, StatusCode::OK);
    // Phrase from app.js — anchors the test against actually
    // serving the right file rather than just any 200.
    assert!(String::from_utf8_lossy(&body).contains("vis-network"));
}

#[tokio::test]
async fn asset_route_serves_vendored_lib() {
    let cfg = config_with("").await;
    let (status, body) = call(cfg, get("/assets/vendor/vis-network.min.js")).await;
    assert_eq!(status, StatusCode::OK);
    // vis-network's UMD bundle exports a global `vis` namespace.
    assert!(String::from_utf8_lossy(&body).contains("vis"));
}

#[tokio::test]
async fn asset_route_404s_unknown_path() {
    let cfg = config_with("").await;
    let (status, _) = call(cfg, get("/assets/does-not-exist.js")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn topology_endpoint_emits_components_and_connections() {
    let cfg = config_with(
        r#"(%make-grid :id 1
             :successors
             (list (%make-meter :id 2
                     :successors
                     (list (%make-battery :id 3)))))"#,
    )
    .await;

    let (status, body) = call(cfg, get("/api/topology")).await;
    assert_eq!(status, StatusCode::OK);
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(parsed["components"].as_array().unwrap().len(), 3);
    assert_eq!(parsed["connections"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn eval_endpoint_runs_lisp_and_returns_value() {
    let cfg = config_with("").await;
    let (status, body) = call(cfg, post("/api/eval", "(+ 2 3)")).await;
    assert_eq!(status, StatusCode::OK);
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["value"], "5");
    assert!(parsed["error"].is_null());
}

#[tokio::test]
async fn eval_endpoint_reports_lisp_errors() {
    let cfg = config_with("").await;
    let (status, body) = call(cfg, post("/api/eval", "(undefined-fn 1)")).await;
    assert_eq!(status, StatusCode::OK);
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["ok"], false);
    assert!(parsed["value"].is_null());
    assert!(parsed["error"].as_str().unwrap().len() > 0);
}

#[tokio::test]
async fn history_endpoint_returns_recent_samples() {
    // Build a world with a battery, then drive the sampler twice
    // synchronously so the rings have content to query. Battery
    // publishes soc_pct in its telemetry; that's what we query.
    let cfg = config_with("(%make-battery :id 1000)").await;
    let world = cfg.world();
    let now = chrono::Utc::now();
    world.record_history_snapshot(now - chrono::Duration::seconds(2));
    world.record_history_snapshot(now - chrono::Duration::seconds(1));

    let (status, body) = call(
        cfg,
        get("/api/history?id=1000&metric=soc_pct&window_s=10"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["id"], 1000);
    assert_eq!(parsed["metric"], "soc_pct");
    let samples = parsed["samples"].as_array().unwrap();
    assert_eq!(samples.len(), 2);
    // Each sample is [ts_ms, value]
    assert!(samples[0][0].as_i64().unwrap() < samples[1][0].as_i64().unwrap());
}

#[tokio::test]
async fn history_endpoint_rejects_unknown_metric() {
    let cfg = config_with("").await;
    let (status, body) = call(cfg, get("/api/history?id=1&metric=foo")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains("unknown metric"));
}

#[tokio::test]
async fn history_endpoint_returns_empty_for_unknown_component() {
    let cfg = config_with("").await;
    let (status, body) = call(cfg, get("/api/history?id=999&metric=active_power_w")).await;
    assert_eq!(status, StatusCode::OK);
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(parsed["samples"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn overrides_endpoint_lists_appended_evals() {
    let cfg = config_with("(set-microgrid-id 7) (%make-grid :id 1)").await;
    // Two successful evals + one error. Errors don't append.
    call(cfg.clone(), post("/api/eval", "(world-rename-component 1 \"a\")")).await;
    call(cfg.clone(), post("/api/eval", "(undefined-fn 1)")).await;
    call(cfg.clone(), post("/api/eval", "(set-microgrid-name \"foo\")")).await;
    let (status, body) = call(cfg, get("/api/overrides")).await;
    assert_eq!(status, StatusCode::OK);
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let entries = parsed["persisted"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .any(|e| e["source"].as_str().unwrap().contains("rename"))
    );
    assert!(
        entries
            .iter()
            .any(|e| e["source"].as_str().unwrap().contains("set-microgrid-name"))
    );
    assert_eq!(parsed["count"], 2);
}

/// Minimal local `load-overrides` defun for tests — real configs
/// get this from `sim/common.lisp`, but `config_with` writes a
/// bare-bones config that doesn't pull in the helper file.
const LOAD_OVERRIDES_HELPER: &str =
    "(defun load-overrides ()
       (when (file-exists-p \"config.ui-overrides.7.lisp\")
         (load \"config.ui-overrides.7.lisp\")))
     (load-overrides)";

#[tokio::test]
async fn persisted_remove_drops_form_immediately() {
    // Two evals append two forms to the override file. DELETE
    // /api/persisted/0 rewrites the file without that form and
    // reloads; the world reflects only the second rename, and
    // the file no longer contains the first.
    let body = format!(
        "(set-microgrid-id 7) (%make-grid :id 1) {LOAD_OVERRIDES_HELPER}",
    );
    let cfg = config_with(&body).await;
    call(cfg.clone(), post("/api/eval", "(world-rename-component 1 \"a\")")).await;
    call(cfg.clone(), post("/api/eval", "(world-rename-component 1 \"b\")")).await;

    let (_, body) = call(cfg.clone(), get("/api/overrides")).await;
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["persisted"].as_array().unwrap().len(), 2);

    let req = axum::http::Request::builder()
        .method(axum::http::Method::DELETE)
        .uri("/api/persisted/0")
        .body(axum::body::Body::empty())
        .unwrap();
    let (status, _) = call(cfg.clone(), req).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, body) = call(cfg.clone(), get("/api/overrides")).await;
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let persisted = parsed["persisted"].as_array().unwrap();
    assert_eq!(persisted.len(), 1);
    assert!(persisted[0]["source"].as_str().unwrap().contains("\"b\""));

    let (_, body) = call(cfg.clone(), get("/api/topology")).await;
    let topo: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let grid = topo["components"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == 1)
        .unwrap();
    assert_eq!(grid["name"], "b");

    // 404 on out-of-range idx.
    let req = axum::http::Request::builder()
        .method(axum::http::Method::DELETE)
        .uri("/api/persisted/99")
        .body(axum::body::Body::empty())
        .unwrap();
    let (status, _) = call(cfg, req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn persisted_bulk_remove_drops_indices_in_one_reload() {
    let body = format!(
        "(set-microgrid-id 7) (%make-grid :id 1) {LOAD_OVERRIDES_HELPER}",
    );
    let cfg = config_with(&body).await;
    call(cfg.clone(), post("/api/eval", "(world-rename-component 1 \"a\")")).await;
    call(cfg.clone(), post("/api/eval", "(world-rename-component 1 \"b\")")).await;
    call(cfg.clone(), post("/api/eval", "(world-rename-component 1 \"c\")")).await;

    // Drop idx 0 + 2 → only "b" survives, world reflects "b".
    let req = axum::http::Request::builder()
        .method(axum::http::Method::POST)
        .uri("/api/persisted/delete")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(r#"{"indices":[0,2]}"#))
        .unwrap();
    let (status, body) = call(cfg.clone(), req).await;
    assert_eq!(status, StatusCode::OK);
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["removed"], 2);

    let (_, body) = call(cfg.clone(), get("/api/overrides")).await;
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let persisted = parsed["persisted"].as_array().unwrap();
    assert_eq!(persisted.len(), 1);
    assert!(persisted[0]["source"].as_str().unwrap().contains("\"b\""));

    let (_, body) = call(cfg, get("/api/topology")).await;
    let topo: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let grid = topo["components"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == 1)
        .unwrap();
    assert_eq!(grid["name"], "b");
}

#[tokio::test]
async fn scenarios_round_trip_save_list_load() {
    // Two evals → save scenario "foo" snapshots the override file
    // → list shows "foo" → load appends the snapshot back into the
    // override file (so the file ends up with all four entries:
    // the original two + the two replays).
    let body = format!(
        "(set-microgrid-id 7) (%make-grid :id 1) {LOAD_OVERRIDES_HELPER}",
    );
    let cfg = config_with(&body).await;
    call(cfg.clone(), post("/api/eval", "(world-rename-component 1 \"a\")")).await;
    call(cfg.clone(), post("/api/eval", "(world-rename-component 1 \"b\")")).await;

    let (status, body) = call(
        cfg.clone(),
        post("/api/scenarios/save?name=foo", ""),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["persisted"], 2);
    let path = parsed["path"].as_str().unwrap();
    assert!(path.ends_with("scenarios/foo.lisp"));
    let written = std::fs::read_to_string(path).unwrap();
    assert!(written.contains("\"a\""));
    assert!(written.contains("\"b\""));

    let (status, body) = call(cfg.clone(), get("/api/scenarios")).await;
    assert_eq!(status, StatusCode::OK);
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let names = parsed["names"].as_array().unwrap();
    assert!(names.iter().any(|n| n == "foo"));

    let (status, body) = call(
        cfg.clone(),
        post("/api/scenarios/load?name=foo", ""),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["entries_added"], 2);

    // Override file now holds the original two appends + two replays.
    let (_, body) = call(cfg, get("/api/overrides")).await;
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["persisted"].as_array().unwrap().len(), 4);
}

#[tokio::test]
async fn scenarios_save_rejects_bad_names() {
    // axum's Query parser strips `name=` to an empty string, which
    // sanitize rejects. The other cases hit the explicit char list.
    let cfg = config_with("").await;
    for bad in ["", "../etc", "a/b", "foo.bar", "-flag"] {
        let uri = format!("/api/scenarios/save?name={bad}");
        let (status, _) = call(cfg.clone(), post(&uri, "")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "expected 400 for {bad:?}");
    }
}

#[tokio::test]
async fn scenarios_load_missing_file_is_400() {
    let cfg = config_with("").await;
    let (status, _) = call(cfg, post("/api/scenarios/load?name=nope", "")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn eval_endpoint_mutates_world() {
    // Confirm an /api/eval call that registers a component shows
    // up in the topology endpoint immediately afterwards. This is
    // the load-bearing claim of the "Lisp eval as the unifying
    // mutation API" design.
    let cfg = config_with("").await;
    let (status, _) = call(cfg.clone(), post("/api/eval", "(%make-grid :id 42)")).await;
    assert_eq!(status, StatusCode::OK);

    let (_, body) = call(cfg, get("/api/topology")).await;
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let components = parsed["components"].as_array().unwrap();
    assert!(components.iter().any(|c| c["id"] == 42));
}

#[tokio::test]
async fn scenario_endpoints_round_trip_lifecycle_and_events() {
    let cfg = config_with("").await;

    // Pre-start: name is null, count is 0.
    let (_, body) = call(cfg.clone(), get("/api/scenario")).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v["name"].is_null());
    assert_eq!(v["event_count"], 0);

    // Start + record two events.
    call(cfg.clone(), post("/api/eval", "(scenario-start \"warmup\")")).await;
    call(cfg.clone(), post("/api/eval", "(scenario-event 'outage \"bat-1003\")")).await;
    call(cfg.clone(), post("/api/eval", "(scenario-event \"note\" \"hi\")")).await;

    // Summary reflects the events.
    let (status, body) = call(cfg.clone(), get("/api/scenario")).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["name"], "warmup");
    assert_eq!(v["event_count"], 2);
    assert_eq!(v["next_event_id"], 2);

    // /api/scenario/events with default since=0 returns both.
    let (status, body) = call(cfg.clone(), get("/api/scenario/events")).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let events = v["events"].as_array().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["kind"], "outage");
    assert_eq!(events[1]["kind"], "note");

    // since=1 cursor returns only id 1 onward.
    let (_, body) = call(cfg.clone(), get("/api/scenario/events?since=1")).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let events = v["events"].as_array().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["id"], 1);
}


#[tokio::test]
async fn scenario_report_endpoint_returns_main_meter_peak() {
    let cfg = config_with(
        "(set-microgrid-id 9)
         (%make-meter :id 1 :main t)
         (scenario-start \"smoke\")
         (set-meter-power 1 4500.0)",
    )
    .await;
    // Drive the sampler so the reporter sees a peak.
    cfg.world().record_history_snapshot(Utc::now());

    let (status, body) = call(cfg, get("/api/scenario/report")).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["main_meter_id"], 1);
    let peak = v["peak_main_meter_w"].as_f64().unwrap();
    assert!((peak - 4500.0).abs() < 1e-3, "got peak {peak}");
}
