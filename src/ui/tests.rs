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
async fn pending_endpoint_lists_logged_evals() {
    let cfg = config_with("(set-microgrid-id 7)").await;
    // Two successful evals + one error. Errors don't log.
    call(cfg.clone(), post("/api/eval", "(+ 1 2)")).await;
    call(cfg.clone(), post("/api/eval", "(undefined-fn 1)")).await;
    call(cfg.clone(), post("/api/eval", "(set-microgrid-name \"foo\")")).await;
    let (status, body) = call(cfg, get("/api/pending")).await;
    assert_eq!(status, StatusCode::OK);
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let entries = parsed["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["source"], "(+ 1 2)");
    assert_eq!(entries[1]["source"], "(set-microgrid-name \"foo\")");
    // Each entry now carries a stable id for delete-by-id.
    assert!(entries[0]["id"].as_u64().is_some());
    assert!(entries[1]["id"].as_u64().is_some());
}

#[tokio::test]
async fn pending_remove_only_entry_bumps_world_version() {
    // Regression: removing the only pending entry used to leave
    // the WS topology event unfired — `reload` happens but no
    // surviving entries replay, so no eval_with_affects
    // bump_version call. UI consumers stuck showing stale state.
    let cfg = config_with("(set-microgrid-id 7) (%make-grid :id 1)").await;
    call(cfg.clone(), post("/api/eval", "(world-rename-component 1 \"x\")")).await;
    let v_before = cfg.world().version();
    let (_, body) = call(cfg.clone(), get("/api/pending")).await;
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let id = parsed["entries"][0]["id"].as_u64().unwrap();

    let req = axum::http::Request::builder()
        .method(axum::http::Method::DELETE)
        .uri(format!("/api/pending/{id}"))
        .body(axum::body::Body::empty())
        .unwrap();
    let (status, _) = call(cfg.clone(), req).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // World version must have advanced — that's how the WS event
    // tells the SPA to re-fetch /api/topology + /api/pending.
    assert!(cfg.world().version() > v_before);
}

#[tokio::test]
async fn pending_remove_drops_one_entry_and_replays_rest() {
    let cfg = config_with("(set-microgrid-id 7) (%make-grid :id 1)").await;
    call(cfg.clone(), post("/api/eval", "(world-rename-component 1 \"a\")")).await;
    call(cfg.clone(), post("/api/eval", "(world-rename-component 1 \"b\")")).await;
    // Pending log has two entries; current name is "b" (last write wins).
    let (_, body) = call(cfg.clone(), get("/api/pending")).await;
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let id_b = parsed["entries"][1]["id"].as_u64().unwrap();

    // Remove the second entry → reload + replay drops "b", leaves "a".
    let req = axum::http::Request::builder()
        .method(axum::http::Method::DELETE)
        .uri(format!("/api/pending/{id_b}"))
        .body(axum::body::Body::empty())
        .unwrap();
    let (status, _) = call(cfg.clone(), req).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, body) = call(cfg.clone(), get("/api/pending")).await;
    let after: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let entries = after["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert!(entries[0]["source"]
        .as_str()
        .unwrap()
        .contains("\"a\""));

    let (_, body) = call(cfg, get("/api/topology")).await;
    let topo: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let grid = topo["components"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == 1)
        .unwrap();
    assert_eq!(grid["name"], "a");
}

#[tokio::test]
async fn persist_writes_overrides_file_and_clears_log() {
    let cfg = config_with("(set-microgrid-id 7)").await;
    call(cfg.clone(), post("/api/eval", "(+ 1 2)")).await;
    call(cfg.clone(), post("/api/eval", "(set-microgrid-name \"foo\")")).await;

    let (status, body) = call(cfg.clone(), post("/api/persist", "")).await;
    assert_eq!(status, StatusCode::OK);
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["persisted"], 2);
    let path = parsed["path"].as_str().unwrap();
    let written = std::fs::read_to_string(path).unwrap();
    // Both expressions present + a header timestamp comment.
    assert!(written.contains("(+ 1 2)"));
    assert!(written.contains("(set-microgrid-name \"foo\")"));
    assert!(written.contains(";; ──"));
    // Filename parameterised by microgrid-id.
    assert!(path.ends_with("config.ui-overrides.7.lisp"));

    // Pending log cleared after persist.
    let (_, body) = call(cfg, get("/api/pending")).await;
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(parsed["entries"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn persisted_remove_drops_form_immediately() {
    // Persist two evals → file has 2 forms. DELETE /api/persisted/0
    // rewrites the file without that form, reloads, and replays the
    // pending log; the world reflects only the second rename, and
    // the file no longer contains the first.
    //
    // The config inlines a load-overrides defun so the reload
    // triggered inside `Config::remove_persisted_override` actually
    // re-reads the rewritten file. Real configs get this for free
    // via `sim/common.lisp`; tests rely on a minimal body so we
    // declare it locally.
    let cfg = config_with(
        "(set-microgrid-id 7)
         (%make-grid :id 1)
         (defun load-overrides ()
           (when (file-exists-p \"config.ui-overrides.7.lisp\")
             (load \"config.ui-overrides.7.lisp\")))
         (load-overrides)",
    )
    .await;
    call(cfg.clone(), post("/api/eval", "(world-rename-component 1 \"a\")")).await;
    call(cfg.clone(), post("/api/eval", "(world-rename-component 1 \"b\")")).await;
    let (status, _) = call(cfg.clone(), post("/api/persist", "")).await;
    assert_eq!(status, StatusCode::OK);

    // Both forms on disk; world's name reflects the last write.
    let (_, body) = call(cfg.clone(), get("/api/pending")).await;
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["persisted"].as_array().unwrap().len(), 2);

    // × idx 0 → file shrinks to 1 form, world's name is now "b".
    let req = axum::http::Request::builder()
        .method(axum::http::Method::DELETE)
        .uri("/api/persisted/0")
        .body(axum::body::Body::empty())
        .unwrap();
    let (status, _) = call(cfg.clone(), req).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, body) = call(cfg.clone(), get("/api/pending")).await;
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
async fn discard_clears_log_and_reloads() {
    // Config that boots with one component; eval adds another;
    // discard reloads → only the original survives.
    let cfg = config_with("(set-microgrid-id 7) (%make-grid :id 1)").await;
    call(cfg.clone(), post("/api/eval", "(%make-meter :id 99)")).await;
    // Verify the meter is live before discard.
    let (_, body) = call(cfg.clone(), get("/api/topology")).await;
    let pre: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(pre["components"].as_array().unwrap().len(), 2);

    let (status, body) = call(cfg.clone(), post("/api/discard", "")).await;
    assert_eq!(status, StatusCode::OK);
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["discarded"], 1);

    // Post-discard: pending empty + topology back to one component.
    let (_, body) = call(cfg.clone(), get("/api/pending")).await;
    let p: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(p["entries"].as_array().unwrap().is_empty());
    let (_, body) = call(cfg, get("/api/topology")).await;
    let post_t: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(post_t["components"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn scenarios_round_trip_save_list_load() {
    // Add two pending entries → save scenario "foo" → list shows
    // it → discard clears pending → load "foo" puts the saved
    // forms back into the pending log.
    let cfg = config_with("(set-microgrid-id 7) (%make-grid :id 1)").await;
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

    // save_scenario clears the pending log.
    let (_, body) = call(cfg.clone(), get("/api/pending")).await;
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(parsed["entries"].as_array().unwrap().is_empty());

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
    // Round-trip: 2 forms saved → 2 pending entries on load.
    assert_eq!(parsed["entries_added"], 2);

    let (_, body) = call(cfg, get("/api/pending")).await;
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["entries"].as_array().unwrap().len(), 2);
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
