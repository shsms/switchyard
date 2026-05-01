//! Embedded web UI server.
//!
//! Runs alongside the gRPC server on the same tokio runtime, separate
//! port (default 8801, see UI.org). Phase 1 surface is intentionally
//! tiny — endpoints land one commit at a time. The SPA shell + assets
//! arrive later via rust-embed.

use std::net::SocketAddr;

use axum::{
    Json, Router,
    body::Body,
    extract::{
        Path, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use rust_embed::Embed;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;

use crate::{
    lisp::{Config, PendingEntry},
    sim::{Category, history::Metric, setpoints::SetpointEvent},
};

/// Embedded SPA assets. In debug builds rust-embed reads from the
/// `ui-assets/` folder live (so `cargo run` picks up edits without
/// rebuilding); in release builds the files are baked into the
/// binary so distribution stays single-file.
#[derive(Embed)]
#[folder = "ui-assets/"]
struct Assets;

/// Spawn the UI HTTP server on `addr`. Returns once the listener is
/// bound and accepting connections; the server itself runs to
/// completion of the returned future.
///
/// Localhost-only by default (the caller decides the bind address);
/// non-loopback is opt-in via the `--ui-bind` CLI flag.
pub async fn serve(addr: SocketAddr, config: Config) -> Result<(), std::io::Error> {
    let app = router(config);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    log::info!("Switchyard UI listening on http://{addr}");
    axum::serve(listener, app).await
}

fn router(config: Config) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/assets/{*path}", get(asset))
        .route("/api/topology", get(topology))
        .route("/api/eval", post(eval))
        .route("/api/history", get(history))
        .route("/api/defaults", get(defaults))
        .route("/api/setpoints", get(setpoints))
        .route("/api/pending", get(pending))
        .route("/api/pending/{id}", axum::routing::delete(pending_remove))
        .route("/api/persist", post(persist))
        .route("/api/discard", post(discard))
        .route("/api/scenarios", get(scenarios_list))
        .route("/api/scenarios/save", post(scenarios_save))
        .route("/api/scenarios/load", post(scenarios_load))
        .route("/ws/events", get(events_ws))
        .with_state(config)
}

async fn index() -> Response {
    serve_embedded("index.html")
}

async fn asset(Path(path): Path<String>) -> Response {
    serve_embedded(&path)
}

fn serve_embedded(path: &str) -> Response {
    match Assets::get(path) {
        Some(content) => {
            let mime = mime_for(path);
            (
                [(header::CONTENT_TYPE, HeaderValue::from_static(mime))],
                Body::from(content.data.into_owned()),
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, format!("asset not found: {path}")).into_response(),
    }
}

fn mime_for(path: &str) -> &'static str {
    let ext = path.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    match ext {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}

#[derive(Serialize)]
struct TopologySnapshot {
    components: Vec<ComponentSummary>,
    /// Parent → child edges. Hidden children are still listed in
    /// `components` (so the UI knows they exist) but their edges are
    /// excluded, matching the gRPC `ListConnections` semantic.
    connections: Vec<(u64, u64)>,
}

#[derive(Serialize)]
struct ComponentSummary {
    id: u64,
    name: String,
    /// Lowercase string form of [`Category`] (e.g. "grid", "battery").
    /// Stable wire shape — the UI keys icon / colour selection off it.
    category: &'static str,
    /// Subtype label like "battery" / "pv" for inverters; `None` for
    /// component categories that don't subdivide further.
    subtype: Option<&'static str>,
    hidden: bool,
    /// Current runtime knob settings — Display impls on the enums map
    /// to the same lowercase tokens the corresponding setter defuns
    /// accept, so the UI's dropdowns can round-trip via /api/eval
    /// without a string table.
    health: String,
    telemetry_mode: String,
    command_mode: String,
}

async fn topology(State(config): State<Config>) -> Json<TopologySnapshot> {
    let world = config.world();
    let components = world
        .components()
        .iter()
        .map(|c| {
            let runtime = world.runtime_of(c.id());
            ComponentSummary {
                id: c.id(),
                // Display-name override (set by world-rename-component)
                // wins over the component's intrinsic name.
                name: world
                    .display_name(c.id())
                    .unwrap_or_else(|| c.name().to_string()),
                category: category_label(c.category()),
                subtype: c.subtype(),
                hidden: c.is_hidden(),
                health: runtime.health.to_string(),
                telemetry_mode: runtime.telemetry.to_string(),
                command_mode: runtime.command.to_string(),
            }
        })
        .collect();
    Json(TopologySnapshot {
        components,
        connections: world.connections(),
    })
}

#[derive(Serialize)]
struct EvalResponse {
    /// Whether the expression evaluated without an error. False ==
    /// `error` populated, `value` null. True == `value` holds the
    /// Display formatted result.
    ok: bool,
    value: Option<String>,
    error: Option<String>,
}

/// Evaluate a Lisp expression on the running interpreter. Wrapped in
/// `spawn_blocking` because tulisp's `SharedMut` is std-sync-RwLock-
/// backed and grabbing the write lock from the executor thread would
/// stall every other tokio task waiting on that worker.
///
/// Always returns 200 — application-layer success/failure rides in
/// the JSON body. Reserves HTTP 4xx/5xx for transport-level problems
/// (bad UTF-8, the spawn_blocking task panicking, etc.).
#[derive(Deserialize)]
struct EvalQuery {
    /// Component id this eval targets, if known. Tagged on the
    /// resulting pending entry so the inspector can show "current
    /// overrides on component X" without parsing the source.
    affects: Option<u64>,
}

async fn eval(
    State(config): State<Config>,
    Query(q): Query<EvalQuery>,
    body: String,
) -> impl IntoResponse {
    let affects = q.affects;
    let result = tokio::task::spawn_blocking(move || config.eval_with_affects(&body, affects))
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("eval task panicked: {e}"),
            )
        });
    match result {
        Ok(Ok(value)) => (
            StatusCode::OK,
            Json(EvalResponse {
                ok: true,
                value: Some(value),
                error: None,
            }),
        ),
        Ok(Err(error)) => (
            StatusCode::OK,
            Json(EvalResponse {
                ok: false,
                value: None,
                error: Some(error),
            }),
        ),
        Err((status, msg)) => (
            status,
            Json(EvalResponse {
                ok: false,
                value: None,
                error: Some(msg),
            }),
        ),
    }
}

#[derive(Deserialize)]
struct HistoryQuery {
    /// Component id to fetch history for. Required.
    id: u64,
    /// Metric name (one of `History::Metric::as_str` strings).
    /// Required.
    metric: String,
    /// Window length in seconds. Optional; defaults to the full
    /// 10-minute capacity of the ring buffer.
    window_s: Option<i64>,
}

#[derive(Serialize)]
struct HistoryResponse {
    id: u64,
    metric: String,
    /// Pairs of (timestamp_ms_since_epoch, value). The time format is
    /// JS-ready (Date.now() shape) so chart libs can plot directly.
    samples: Vec<(i64, f32)>,
}

async fn history(
    State(config): State<Config>,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<HistoryResponse>, (StatusCode, String)> {
    let metric = parse_metric(&q.metric)
        .ok_or_else(|| (StatusCode::BAD_REQUEST, format!("unknown metric '{}'", q.metric)))?;
    let window = ChronoDuration::seconds(q.window_s.unwrap_or(600));
    let since: DateTime<Utc> = Utc::now() - window;

    let samples = config
        .world()
        .history_window(q.id, metric, since)
        .unwrap_or_default()
        .into_iter()
        .map(|s| (s.ts.timestamp_millis(), s.value))
        .collect();

    Ok(Json(HistoryResponse {
        id: q.id,
        metric: q.metric,
        samples,
    }))
}

fn parse_metric(s: &str) -> Option<Metric> {
    match s {
        "active_power_w" => Some(Metric::ActivePowerW),
        "reactive_power_var" => Some(Metric::ReactivePowerVar),
        "frequency_hz" => Some(Metric::FrequencyHz),
        "soc_pct" => Some(Metric::SocPct),
        "active_power_lower_bound_w" => Some(Metric::ActivePowerLowerBoundW),
        "active_power_upper_bound_w" => Some(Metric::ActivePowerUpperBoundW),
        "reactive_power_lower_bound_var" => Some(Metric::ReactivePowerLowerBoundVar),
        "reactive_power_upper_bound_var" => Some(Metric::ReactivePowerUpperBoundVar),
        _ => None,
    }
}

#[derive(Deserialize)]
struct SetpointsQuery {
    id: u64,
    /// Window length in seconds. Optional; defaults to the full
    /// 1000-event capacity of the ring (which at typical control-app
    /// rates covers several minutes).
    window_s: Option<i64>,
}

#[derive(Serialize)]
struct SetpointsResponse {
    id: u64,
    events: Vec<SetpointEvent>,
}

async fn setpoints(
    State(config): State<Config>,
    Query(q): Query<SetpointsQuery>,
) -> Json<SetpointsResponse> {
    let window = ChronoDuration::seconds(q.window_s.unwrap_or(600));
    let since = Utc::now() - window;
    let events = config.world().setpoints_window(q.id, since);
    Json(SetpointsResponse { id: q.id, events })
}

/// One per `*-defaults` alist defined in `sim/defaults.lisp`. The
/// `var_name` is the actual Lisp variable; `value` is its current
/// printed form (a stringified alist), readable / editable as raw
/// Lisp by the UI.
#[derive(Serialize)]
struct DefaultsEntry {
    category: &'static str,
    var_name: String,
    value: String,
}

#[derive(Serialize)]
struct DefaultsResponse {
    entries: Vec<DefaultsEntry>,
}

const DEFAULT_CATEGORIES: &[&str] = &[
    "grid",
    "meter",
    "battery",
    "battery-inverter",
    "solar-inverter",
    "ev-charger",
    "chp",
];

async fn defaults(State(config): State<Config>) -> Json<DefaultsResponse> {
    // Read each *-defaults variable via eval_silent so reading the
    // current state doesn't itself look like an edit. spawn_blocking
    // because eval acquires the std-RwLock-backed ctx.
    let entries = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        for cat in DEFAULT_CATEGORIES {
            let var = format!("{cat}-defaults");
            match config.eval_silent(&var) {
                Ok(value) => {
                    // Pretty-print via tulisp-fmt so the textarea
                    // shows one (key . value) pair per line at a
                    // narrow width — fits the side panel without
                    // horizontal scroll. Falls back to the raw
                    // Display form if the printed value isn't
                    // parseable (shouldn't happen for an alist read
                    // back from the interpreter).
                    let formatted = tulisp_fmt::format_with_width(&value, 50)
                        .map(|f| f.trim_end().to_string())
                        .unwrap_or(value);
                    out.push(DefaultsEntry {
                        category: cat,
                        var_name: var,
                        value: formatted,
                    });
                }
                Err(_) => {} // variable unset — skip
            }
        }
        out
    })
    .await
    .unwrap_or_default();
    Json(DefaultsResponse { entries })
}

/// One entry as the UI sees it: structured metadata plus the
/// formatted source (tulisp-fmt at width 60). The id is the
/// PendingEntry id from Config — used for `DELETE /api/pending/:id`.
#[derive(Serialize)]
struct PendingEntryView {
    id: u64,
    ts: chrono::DateTime<chrono::Utc>,
    affects: Option<u64>,
    source: String,
}

#[derive(Serialize)]
struct PendingResponse {
    /// Successful eval expressions accumulated since the last persist
    /// (or process start). Oldest first.
    entries: Vec<PendingEntryView>,
}

fn format_entry(e: PendingEntry) -> PendingEntryView {
    let formatted = tulisp_fmt::format_with_width(&e.source, 60)
        .map(|f| f.trim_end().to_string())
        .unwrap_or_else(|_| e.source.clone());
    PendingEntryView {
        id: e.id,
        ts: e.ts,
        affects: e.affects,
        source: formatted,
    }
}

async fn pending(State(config): State<Config>) -> Json<PendingResponse> {
    // Format each entry via tulisp-fmt so the modal shows tidy Lisp
    // (multi-line for nested forms) instead of one-liner source.
    // .trim_end() drops the formatter's file-style trailing newline
    // — the modal renders each entry in its own <pre>, an extra blank
    // line at the bottom would just look noisy.
    let entries = config.pending().into_iter().map(format_entry).collect();
    Json(PendingResponse { entries })
}

async fn pending_remove(
    State(config): State<Config>,
    axum::extract::Path(id): axum::extract::Path<u64>,
) -> Result<StatusCode, (StatusCode, String)> {
    let removed = tokio::task::spawn_blocking(move || config.remove_pending(id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("task panicked: {e}")))?;
    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err((StatusCode::NOT_FOUND, format!("no pending entry with id {id}")))
    }
}

#[derive(Serialize)]
struct PersistResponse {
    persisted: usize,
    path: String,
}

async fn persist(State(config): State<Config>) -> Result<Json<PersistResponse>, (StatusCode, String)> {
    // File I/O on the executor thread isn't quite as bad as a long
    // eval, but spawn_blocking here keeps us consistent with the
    // policy /api/eval already follows.
    tokio::task::spawn_blocking(move || config.persist_pending())
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("persist task panicked: {e}"),
            )
        })?
        .map(|r| {
            Json(PersistResponse {
                persisted: r.persisted,
                path: r.path,
            })
        })
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("write failed: {e}")))
}

#[derive(Serialize)]
struct DiscardResponse {
    discarded: usize,
}

async fn discard(State(config): State<Config>) -> Json<DiscardResponse> {
    let count = config.pending().len();
    tokio::task::spawn_blocking(move || config.discard_pending())
        .await
        .ok();
    Json(DiscardResponse { discarded: count })
}

#[derive(Serialize)]
struct ScenariosListResponse {
    names: Vec<String>,
}

async fn scenarios_list(
    State(config): State<Config>,
) -> Result<Json<ScenariosListResponse>, (StatusCode, String)> {
    config
        .list_scenarios()
        .map(|names| Json(ScenariosListResponse { names }))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("read failed: {e}")))
}

#[derive(Deserialize)]
struct ScenarioName {
    name: String,
}

async fn scenarios_save(
    State(config): State<Config>,
    Query(q): Query<ScenarioName>,
) -> Result<Json<PersistResponse>, (StatusCode, String)> {
    let name = sanitize_scenario_name(&q.name)?;
    tokio::task::spawn_blocking(move || config.save_scenario(&name))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("save panicked: {e}")))?
        .map(|r| {
            Json(PersistResponse {
                persisted: r.persisted,
                path: r.path,
            })
        })
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("write failed: {e}")))
}

#[derive(Serialize)]
struct ScenarioLoadResponse {
    loaded_lines: usize,
}

async fn scenarios_load(
    State(config): State<Config>,
    Query(q): Query<ScenarioName>,
) -> Result<Json<ScenarioLoadResponse>, (StatusCode, String)> {
    let name = sanitize_scenario_name(&q.name)?;
    tokio::task::spawn_blocking(move || config.load_scenario(&name))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("load panicked: {e}")))?
        .map(|loaded_lines| Json(ScenarioLoadResponse { loaded_lines }))
        .map_err(|e| (StatusCode::NOT_FOUND, format!("read failed: {e}")))
}

/// Reject path-traversal + obviously-bad characters in scenario
/// names. Keep it conservative — local-only single-developer means
/// anything weird is more likely a typo than an attack, but we still
/// prefer to fail cleanly than to write to `../etc/passwd.lisp`.
fn sanitize_scenario_name(name: &str) -> Result<String, (StatusCode, String)> {
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains('.')
        || name.starts_with('-')
    {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("invalid scenario name: {name:?}"),
        ));
    }
    Ok(name.to_string())
}

/// WebSocket event push. Subscribers receive WorldEvent JSON for
/// every TopologyChanged + Sample broadcast. Client-sent frames are
/// drained but ignored — the channel is server-push only for v1; an
/// upcoming change adds a /api/eval-style RPC over the same socket
/// if it turns out latency-sensitive client actions benefit from it.
async fn events_ws(ws: WebSocketUpgrade, State(config): State<Config>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| event_pump(socket, config))
}

async fn event_pump(mut socket: WebSocket, config: Config) {
    let mut rx = config.world().subscribe_events();
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(event) => {
                    let json = match serde_json::to_string(&event) {
                        Ok(j) => j,
                        Err(e) => {
                            log::error!("ws: serde error: {e}");
                            continue;
                        }
                    };
                    if socket.send(Message::Text(json.into())).await.is_err() {
                        // Client closed.
                        break;
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    // Subscriber fell behind; the receiver auto-skips
                    // ahead. Tell the client so it can re-sync.
                    log::warn!("ws: subscriber lagged by {n} events");
                    let msg = serde_json::json!({"kind": "lagged", "skipped": n}).to_string();
                    if socket.send(Message::Text(msg.into())).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Closed) => break, // shutting down
            },
            msg = socket.recv() => match msg {
                // Drain client frames so we notice a dropped socket.
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
        }
    }
}

fn category_label(c: Category) -> &'static str {
    match c {
        Category::Grid => "grid",
        Category::Meter => "meter",
        Category::Inverter => "inverter",
        Category::Battery => "battery",
        Category::EvCharger => "ev-charger",
        Category::Chp => "chp",
    }
}

#[cfg(test)]
mod tests {
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
}
