//! Embedded web UI server.
//!
//! Runs alongside the gRPC server on the same tokio runtime, separate
//! port (default 8801). The SPA shell + vendored assets are bundled
//! via rust-embed.

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
    lisp::Config,
    sim::{
        Category,
        history::Metric,
        setpoints::SetpointEvent,
        world::{ScenarioReport, ScenarioSummary},
    },
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

/// Like [`serve`] but with a caller-supplied listener — for tests
/// that need to pre-bind on a free OS-assigned port and read the
/// resulting addr before the server starts accepting connections.
pub async fn serve_with_listener(
    listener: tokio::net::TcpListener,
    config: Config,
) -> Result<(), std::io::Error> {
    axum::serve(listener, router(config)).await
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
        .route("/api/overrides", get(overrides_list))
        .route(
            "/api/persisted/{idx}",
            axum::routing::delete(persisted_remove),
        )
        .route("/api/persisted/delete", post(persisted_bulk_remove))
        .route("/api/logs", get(logs_backfill))
        .route("/api/scenario", get(scenario_summary))
        .route("/api/scenario/events", get(scenario_events))
        .route("/api/scenario/report", get(scenario_report))
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
    /// Visible parent → child edges, matching the gRPC
    /// `ListConnections` semantic.
    connections: Vec<(u64, u64)>,
    /// Parent → child edges where the child is hidden. Surfaced
    /// separately so the UI can render them dashed without
    /// polluting the gRPC graph. Aggregator components
    /// (Meter, BatteryInverter) cache their hidden children for
    /// aggregation; we read those here.
    hidden_connections: Vec<(u64, u64)>,
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
    let hidden_connections: Vec<(u64, u64)> = world
        .components()
        .iter()
        .flat_map(|c| {
            let pid = c.id();
            c.hidden_successors().into_iter().map(move |cid| (pid, cid))
        })
        .collect();
    Json(TopologySnapshot {
        components,
        connections: world.connections(),
        hidden_connections,
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
async fn eval(State(config): State<Config>, body: String) -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(move || config.eval(&body))
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

#[derive(Serialize)]
struct PersistedOverrideView {
    idx: usize,
    source: String,
}

#[derive(Serialize)]
struct OverridesResponse {
    /// Top-level forms in the on-disk override file, one per
    /// entry. Each `idx` is stable until the next bulk-delete
    /// rewrites the file.
    persisted: Vec<PersistedOverrideView>,
    /// Convenience for the chrome's "N overrides" pill — equals
    /// `persisted.len()`.
    count: usize,
}

async fn overrides_list(State(config): State<Config>) -> Json<OverridesResponse> {
    // Format each form via tulisp-fmt so the dialog shows tidy
    // Lisp (multi-line for nested forms) instead of one-liner
    // source. .trim_end() drops the formatter's file-style
    // trailing newline so adjacent <pre>s don't accumulate blank
    // lines.
    let persisted: Vec<PersistedOverrideView> = config
        .persisted_overrides()
        .into_iter()
        .map(|o| PersistedOverrideView {
            idx: o.idx,
            source: tulisp_fmt::format_with_width(&o.source, 60)
                .map(|f| f.trim_end().to_string())
                .unwrap_or(o.source),
        })
        .collect();
    let count = persisted.len();
    Json(OverridesResponse { persisted, count })
}

/// Drop a single persisted-override entry by its file-position
/// idx. Rewrites the override file without that form and reloads
/// — see `Config::remove_persisted_overrides`. The bulk-delete
/// endpoint below is the more common path; this one stays for
/// parity / single-shot scripted use.
async fn persisted_remove(
    State(config): State<Config>,
    axum::extract::Path(idx): axum::extract::Path<usize>,
) -> Result<StatusCode, (StatusCode, String)> {
    let result = tokio::task::spawn_blocking(move || config.remove_persisted_overrides(&[idx]))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("task panicked: {e}")))?;
    match result {
        Ok(0) => Err((
            StatusCode::NOT_FOUND,
            format!("no persisted override at idx {idx}"),
        )),
        Ok(_) => Ok(StatusCode::NO_CONTENT),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, format!("write failed: {e}"))),
    }
}

#[derive(Deserialize)]
struct BulkRemoveBody {
    indices: Vec<usize>,
}

#[derive(Serialize)]
struct BulkRemoveResponse {
    removed: usize,
}

/// Drop several persisted-override entries in one shot. Rewrites
/// the override file once and reloads once — the chrome's
/// checkbox-toolbar Delete button hits this, so a 5-item delete
/// is one round trip and one re-render rather than five.
async fn persisted_bulk_remove(
    State(config): State<Config>,
    Json(body): Json<BulkRemoveBody>,
) -> Result<Json<BulkRemoveResponse>, (StatusCode, String)> {
    let result =
        tokio::task::spawn_blocking(move || config.remove_persisted_overrides(&body.indices))
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("task panicked: {e}")))?;
    match result {
        Ok(removed) => Ok(Json(BulkRemoveResponse { removed })),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, format!("write failed: {e}"))),
    }
}

/// Backfill recent log lines from the LogTap ring buffer. Returns
/// an empty list when the binary didn't initialise a tap (test path).
async fn logs_backfill() -> Json<Vec<crate::ui_log::LogEvent>> {
    Json(
        crate::ui_log::LOG_TAP
            .get()
            .map(|t| t.snapshot())
            .unwrap_or_default(),
    )
}

/// Snapshot of the running scenario's lifecycle. Empty (`name:
/// null`, zero counts) before any `(scenario-start)`; freezes
/// `elapsed_s` once `(scenario-stop)` fires.
async fn scenario_summary(State(config): State<Config>) -> Json<ScenarioSummary> {
    Json(config.world().scenario_summary(Utc::now()))
}

#[derive(Deserialize)]
struct ScenarioEventsQuery {
    /// Return events with id strictly greater than this. Default 0
    /// means "everything in the ring".
    since: Option<u64>,
    /// Cap on returned entries. Default 200.
    limit: Option<usize>,
}

#[derive(Serialize)]
struct ScenarioEventsResponse {
    events: Vec<crate::sim::scenario::ScenarioEvent>,
    /// `next_event_id` lets a polling client advance its `since=`
    /// cursor even when this batch was empty (because no events
    /// have arrived since last poll, but new ones might before the
    /// next).
    next_event_id: u64,
}

async fn scenario_events(
    State(config): State<Config>,
    Query(q): Query<ScenarioEventsQuery>,
) -> Json<ScenarioEventsResponse> {
    let since = q.since.unwrap_or(0);
    let limit = q.limit.unwrap_or(200).min(1000);
    let events = config.world().scenario_events_since(since, limit);
    let next_event_id = config.world().scenario_summary(Utc::now()).next_event_id;
    Json(ScenarioEventsResponse {
        events,
        next_event_id,
    })
}

/// Aggregate metrics for the running scenario (peak main-meter
/// power so far, plus future B3/B4 fields). Independent of
/// `/api/scenario/events` so a dashboard can poll metrics
/// frequently without scanning the whole event log.
async fn scenario_report(State(config): State<Config>) -> Json<ScenarioReport> {
    Json(config.world().scenario_report(Utc::now()))
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
    // Optional: log tap. Only set when running through the binary;
    // tests hit this path with no tap initialised, so subscribe via
    // `Option<broadcast::Receiver>` and skip the branch when absent.
    let mut log_rx = crate::ui_log::LOG_TAP.get().map(|t| t.subscribe());

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
                        break; // client closed
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    log::warn!("ws: subscriber lagged by {n} events");
                    let msg = serde_json::json!({"kind": "lagged", "skipped": n}).to_string();
                    if socket.send(Message::Text(msg.into())).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Closed) => break,
            },
            // Log tap branch — only fires when LOG_TAP was initialised
            // (i.e. running under the binary, not in a unit test).
            log = async {
                match log_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => match log {
                Ok(line) => {
                    let event = crate::sim::events::WorldEvent::Log {
                        ts_ms: line.ts_ms,
                        level: line.level,
                        target: line.target,
                        message: line.message,
                    };
                    if let Ok(json) = serde_json::to_string(&event) {
                        if socket.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }
                    }
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => log_rx = None,
            },
            msg = socket.recv() => match msg {
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
mod tests;
