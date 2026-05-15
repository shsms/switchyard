//! Embedded web UI server.
//!
//! Runs alongside the gRPC server on the same tokio runtime, separate
//! port (default 8801). The SPA shell + vendored assets are bundled
//! via rust-embed.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    Extension, Json, Router,
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
use frequenz_microgrid::{
    LogicalMeterConfig, LogicalMeterHandle, Microgrid, MicrogridClientHandle, Sample, metric,
    quantity::Power,
};
use parking_lot::{Mutex, RwLock};
use rust_embed::Embed;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;

use crate::sim::{World, events::WorldEvent};

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

/// One forwarded sample, cached so the SPA can paint immediately
/// on page load instead of waiting up to a full second for the
/// next WS tick. Mirrors the [`WorldEvent::MicrogridSample`]
/// payload minus the `kind` discriminator.
#[derive(Clone, Debug, Serialize)]
pub struct MicrogridSampleSnapshot {
    pub quantity: &'static str,
    pub unit: &'static str,
    pub ts_ms: i64,
    pub value: Option<f32>,
}

/// Shared state for the loopback Microgrid client: the handle slot
/// plus the per-stream latest-sample cache the forwarders write to,
/// plus the live forwarder JoinHandles. `Arc`'d so the constructor
/// task, the per-stream forwarders, and the HTTP handlers all hold
/// cheap clones.
///
/// `microgrid` is `RwLock<Option<…>>` rather than a `OnceCell`
/// because the supervisor task (see `spawn_microgrid_loopback`)
/// drops + rebuilds the handle whenever the topology changes —
/// the graph crate's `ComponentGraph` is snapshotted at try_new
/// time and doesn't refresh on its own, so formulas + subscriptions
/// drift if we kept the boot-time handle. HTTP handlers take a
/// brief read lock + clone the cheap `LogicalMeterHandle` out
/// before doing any async work.
pub struct MicrogridState {
    pub microgrid: RwLock<Option<Microgrid>>,
    /// The microgrid client, built once at boot via
    /// `Microgrid::try_new` and reused for every rebuild — only
    /// the `LogicalMeterHandle` (which embeds the graph snapshot)
    /// gets replaced when the topology changes. A new client per
    /// rebuild would close the previous one's instructions channel,
    /// and `MicrogridClientActor` in frequenz-microgrid 0.4.1
    /// busy-spins at 100 % CPU on a closed channel (see
    /// `microgrid-rs-busy-spin-issue.md` for the writeup). Keeping
    /// one handle clone alive forever sidesteps the bug entirely.
    ///
    /// `tokio::sync::OnceCell` rather than `RwLock<Option<_>>`
    /// because the value is set exactly once on the first
    /// successful boot.
    client: tokio::sync::OnceCell<MicrogridClientHandle>,
    /// Latest sample seen per stream name. Forwarders overwrite on
    /// each recv; the `/api/microgrid/latest` endpoint snapshots the
    /// whole map on each call. `parking_lot::RwLock` because writes
    /// are non-async (no await between lock + drop) and contention
    /// is tiny (one writer per stream at 1 Hz). Cleared on each
    /// rebuild so absent streams in the new graph don't surface
    /// stale values.
    pub latest: RwLock<HashMap<&'static str, MicrogridSampleSnapshot>>,
    /// Currently-running forwarder tasks. Rebuilds abort these +
    /// spawn fresh ones bound to the new Microgrid handle's
    /// subscriptions. Dropping the old `Microgrid` alone isn't
    /// enough — the formulas captured inside the spawned tasks
    /// hold sender clones of the underlying actor mpsc, so the
    /// actor stays alive and the forwarders keep recv'ing
    /// indefinitely without explicit abort.
    pub forwarders: Mutex<Vec<JoinHandle<()>>>,
}

pub type SharedMicrogrid = Arc<MicrogridState>;

pub fn new_microgrid_slot() -> SharedMicrogrid {
    Arc::new(MicrogridState {
        microgrid: RwLock::new(None),
        client: tokio::sync::OnceCell::new(),
        latest: RwLock::new(HashMap::new()),
        forwarders: Mutex::new(Vec::new()),
    })
}

/// Spawn a tokio task that constructs a [`Microgrid`] pointed at
/// `grpc_url`, kicks off forwarders for the aggregated streams the
/// Dashboard cares about, and stores the handle in `slot` once the
/// connection succeeds. `Microgrid::try_new` already retries lazily
/// until the gRPC server is reachable; this wrapper exists so the
/// UI's `serve` doesn't block on the gRPC server coming up — UI
/// startup proceeds, and dashboard endpoints return 503 until the
/// slot fills.
///
/// `world` is the sink the forwarders publish to via
/// [`World::broadcast_microgrid_sample`]; the existing `/ws/events`
/// stream then carries the samples to the SPA without any extra
/// wiring — they ride the same `WorldEvent` discriminator the
/// per-component samples already use.
pub fn spawn_microgrid_loopback(grpc_url: String, slot: SharedMicrogrid, world: World) {
    tokio::spawn(async move {
        if !build_microgrid(&grpc_url, &slot, &world).await {
            return;
        }
        log::info!("microgrid loopback: connected + graph built + forwarders running");
        // Watch for topology mutations and rebuild on each. The
        // graph crate's ComponentGraph is snapshotted at try_new
        // time so formulas + subscriptions go stale once the world
        // mutates; rebuilding picks up the new shape.
        run_supervisor(grpc_url, slot, world).await;
    });
}

/// Build a fresh `Microgrid` and wire up its forwarders.
///
/// First call: goes through `Microgrid::try_new` (which connects to
/// the gRPC server, builds the component graph, and spawns the
/// actor) and seeds `slot.client` with a clone of the resulting
/// `MicrogridClientHandle`. Subsequent calls: reuse the cached
/// client and only build a fresh `LogicalMeterHandle` — the new
/// LM picks up the current topology + spawns its own actor, and
/// `Microgrid::new_from_handles` stitches it together with the
/// long-lived client. The old `Microgrid` (replaced in `slot`)
/// drops normally; its `LogicalMeterActor` exits cleanly because
/// it handles a closed instructions channel by breaking out.
///
/// Returns false if the gRPC connect or graph build fails outright
/// (which the crate normally retries through; a hard failure means
/// something like a malformed URL).
async fn build_microgrid(grpc_url: &str, slot: &SharedMicrogrid, world: &World) -> bool {
    // 1 Hz sample cadence matches the existing history sampler;
    // dashboard tiles refresh at this rate.
    let config = LogicalMeterConfig::new(chrono::TimeDelta::seconds(1));
    let mut mg = if let Some(client) = slot.client.get() {
        // Rebuild path: reuse the long-lived client; only the
        // LogicalMeterHandle is topology-bound and needs replacing.
        let lm = match LogicalMeterHandle::try_new(client.clone(), config).await {
            Ok(lm) => lm,
            Err(e) => {
                log::error!("microgrid loopback: logical-meter rebuild failed: {e}");
                return false;
            }
        };
        Microgrid::new_from_handles(client.clone(), lm)
    } else {
        // Initial path: build everything via try_new and stash the
        // client clone for all future rebuilds.
        let mg = match Microgrid::try_new(grpc_url.to_owned(), config).await {
            Ok(mg) => mg,
            Err(e) => {
                log::error!("microgrid loopback: try_new failed: {e}");
                return false;
            }
        };
        let _ = slot.client.set(mg.client());
        mg
    };
    let handles = spawn_power_forwarders(&mut mg, world, slot.clone());
    // Install everything atomically from the slot's perspective.
    // Handlers reading `microgrid` under the read lock will see
    // either the old None / Some or the new Some — never a half-
    // populated state.
    *slot.forwarders.lock() = handles;
    *slot.microgrid.write() = Some(mg);
    true
}

/// Subscribe to World events and rebuild the Microgrid handle on
/// every TopologyChanged. Lagged-receiver and dropped-sender
/// events also trigger a rebuild (defensive — a missed event
/// might have been a topology change).
async fn run_supervisor(grpc_url: String, slot: SharedMicrogrid, world: World) {
    let mut events = world.subscribe_events();
    loop {
        match events.recv().await {
            Ok(WorldEvent::TopologyChanged { .. }) => {
                debounce_topology_burst(&mut events).await;
                rebuild(&grpc_url, &slot, &world).await;
            }
            Ok(_) => continue,
            Err(RecvError::Lagged(n)) => {
                log::warn!(
                    "microgrid loopback supervisor: lagged {n} events, rebuilding defensively"
                );
                debounce_topology_burst(&mut events).await;
                rebuild(&grpc_url, &slot, &world).await;
            }
            Err(RecvError::Closed) => {
                log::info!("microgrid loopback supervisor: world events closed, exiting");
                return;
            }
        }
    }
}

/// After seeing the first TopologyChanged, swallow any further
/// events that arrive within `DEBOUNCE` so a hot-reload that
/// registers 12 components in rapid succession only triggers one
/// rebuild instead of 12.
async fn debounce_topology_burst(events: &mut tokio::sync::broadcast::Receiver<WorldEvent>) {
    const DEBOUNCE: Duration = Duration::from_millis(300);
    let deadline = tokio::time::Instant::now() + DEBOUNCE;
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Ok(_)) => continue, // keep collecting
            Ok(Err(_)) => return,  // broadcast error; supervisor's main loop deals with it
            Err(_) => return,       // deadline; we're done
        }
    }
}

/// Tear down the live forwarders + drop the old Microgrid, clear
/// the latest-sample cache, then build a fresh handle so its
/// graph reflects the new topology. The cache is cleared because
/// streams whose category disappeared (e.g. someone removed the
/// PV from config.lisp) would otherwise show stale values until
/// page reload; honest "—" is better.
///
/// Only the `LogicalMeterHandle` inside the new Microgrid is
/// rebuilt; the `MicrogridClientHandle` cached in `slot.client` is
/// reused. See the field doc for why the client is long-lived.
async fn rebuild(grpc_url: &str, slot: &SharedMicrogrid, world: &World) {
    log::info!("microgrid loopback: topology changed — rebuilding handle");
    for h in slot.forwarders.lock().drain(..) {
        h.abort();
    }
    *slot.microgrid.write() = None;
    slot.latest.write().clear();
    build_microgrid(grpc_url, slot, world).await;
}

/// Build subscriptions for the active-power streams the Dashboard
/// tier-1 (grid), tier-2 (battery pool), tier-3 (PV), and tier-4
/// (consumer + producer aggregates) read from, and spawn one tokio
/// task per surviving subscription to forward samples onto the
/// World event bus.
///
/// Streams whose underlying category is absent (no PV in the
/// topology, etc.) emit a single `log::info!` and are silently
/// dropped — the Dashboard's matching tile renders as "data
/// unavailable" until that category appears.
fn spawn_power_forwarders(
    microgrid: &mut Microgrid,
    world: &World,
    state: SharedMicrogrid,
) -> Vec<JoinHandle<()>> {
    let mut handles = Vec::new();
    let lm = microgrid.logical_meter();
    let mut push = |stream, formula, state: SharedMicrogrid| {
        if let Some(h) = spawn_power_forwarder(stream, formula, world, state) {
            handles.push(h);
        }
    };
    push(
        "grid_power",
        lm.grid::<metric::AcPowerActive>(),
        state.clone(),
    );
    push(
        "consumer_power",
        lm.consumer::<metric::AcPowerActive>(),
        state.clone(),
    );
    push(
        "producer_power",
        lm.producer::<metric::AcPowerActive>(),
        state.clone(),
    );
    push(
        "pv_power",
        lm.pv::<metric::AcPowerActive>(None),
        state.clone(),
    );
    // BatteryPool takes &mut self for power() (it caches subscriber
    // refs); build it once and let it go out of scope after the
    // subscription resolves.
    match microgrid.battery_pool(None) {
        Ok(mut pool) => push("battery_pool_power", pool.power(), state),
        Err(e) => log::info!("microgrid loopback: battery pool absent — skipping: {e}"),
    }
    handles
}

/// Subscribe to one Power-valued formula and spawn a forwarder that
/// pushes each `Sample<Power>` onto the World event bus as a
/// `MicrogridSample { stream, quantity: "Power", unit: "W", ... }`
/// event. Returns `None` (no spawn) if the formula errored at
/// construction (typical for absent categories); returns the
/// spawned `JoinHandle` otherwise so the supervisor can `.abort()`
/// it on rebuild.
fn spawn_power_forwarder(
    stream: &'static str,
    formula: Result<frequenz_microgrid::Formula<Power>, frequenz_microgrid::Error>,
    world: &World,
    state: SharedMicrogrid,
) -> Option<JoinHandle<()>> {
    let formula = match formula {
        Ok(f) => f,
        Err(e) => {
            log::info!("microgrid loopback: skip {stream} ({e})");
            return None;
        }
    };
    let world = world.clone();
    Some(tokio::spawn(async move {
        let mut rx = match formula.subscribe().await {
            Ok(rx) => rx,
            Err(e) => {
                log::warn!("microgrid loopback: subscribe {stream} failed: {e}");
                return;
            }
        };
        loop {
            match rx.recv().await {
                Ok(sample) => publish_power(stream, sample, &world, &state),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    log::warn!("microgrid loopback: {stream} lagged {n} samples");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    log::info!("microgrid loopback: {stream} closed; forwarder exiting");
                    return;
                }
            }
        }
    }))
}

fn publish_power(stream: &'static str, sample: Sample<Power>, world: &World, state: &SharedMicrogrid) {
    let value = sample.value().map(|p| p.as_watts());
    let ts_ms = sample.timestamp().timestamp_millis();
    let snapshot = MicrogridSampleSnapshot {
        quantity: "Power",
        unit: "W",
        ts_ms,
        value,
    };
    state.latest.write().insert(stream, snapshot);
    world.broadcast_microgrid_sample(stream, "Power", "W", ts_ms, value);
}

/// Spawn the UI HTTP server on `addr`. Returns once the listener is
/// bound and accepting connections; the server itself runs to
/// completion of the returned future.
///
/// `microgrid` is the loopback client slot — the binary populates it
/// via [`spawn_microgrid_loopback`] before / alongside the gRPC
/// server starting. Pass an empty slot if the UI doesn't need
/// aggregated Dashboard data (tests, etc.).
///
/// Localhost-only by default (the caller decides the bind address);
/// non-loopback is opt-in via the `--ui-bind` CLI flag.
pub async fn serve(
    addr: SocketAddr,
    config: Config,
    microgrid: SharedMicrogrid,
) -> Result<(), std::io::Error> {
    let app = router(config, microgrid);
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
    microgrid: SharedMicrogrid,
) -> Result<(), std::io::Error> {
    axum::serve(listener, router(config, microgrid)).await
}

fn router(config: Config, microgrid: SharedMicrogrid) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/assets/{*path}", get(asset))
        .route("/api/topology", get(topology))
        .route("/api/eval", post(eval))
        .route("/api/format", post(format))
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
        .route("/api/clock", get(clock_info))
        .route("/api/microgrid/status", get(microgrid_status))
        .route("/api/microgrid/latest", get(microgrid_latest))
        .route("/api/microgrid/formulas", get(microgrid_formulas))
        .route("/ws/events", get(events_ws))
        .layer(Extension(microgrid))
        .with_state(config)
}

#[derive(Serialize)]
struct MicrogridStatusResp {
    /// Loopback handle is up and the component graph built.
    /// Mirrors `Microgrid::try_new`'s success guarantee — if this
    /// is true, every `LogicalMeterHandle::xxx<M>()` is reachable.
    connected: bool,
    /// Round-trip count from `list_electrical_components` —
    /// confirms switchyard's gRPC server returned what the
    /// graph crate accepted.
    component_count: Option<usize>,
}

async fn microgrid_status(
    Extension(state): Extension<SharedMicrogrid>,
) -> (StatusCode, Json<MicrogridStatusResp>) {
    // Clone the logical-meter handle under a brief read lock so
    // the supervisor task can grab the write lock for a rebuild
    // mid-request without contending. LogicalMeterHandle is Arc-
    // backed so this is cheap.
    let lm = state
        .microgrid
        .read()
        .as_ref()
        .map(|mg| mg.logical_meter());
    if let Some(lm) = lm {
        // `logical_meter().graph()` is the cached snapshot the
        // crate built at try_new. Component count there is the
        // post-pass-through view, matching what the formula
        // generators see.
        let count = lm.graph().components().count();
        (
            StatusCode::OK,
            Json(MicrogridStatusResp {
                connected: true,
                component_count: Some(count),
            }),
        )
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(MicrogridStatusResp {
                connected: false,
                component_count: None,
            }),
        )
    }
}

#[derive(Serialize)]
struct ClockInfo {
    /// IANA timezone name set via `(set-timezone …)`, default
    /// Europe/Berlin. UI passes this to `Intl.DateTimeFormat` to
    /// format the pulse-bar clock + (future) per-component
    /// timestamps in the configured civil zone.
    tz: &'static str,
}

async fn clock_info(State(config): State<Config>) -> Json<ClockInfo> {
    Json(ClockInfo {
        tz: config.tz_name(),
    })
}

/// Latest cached sample for every active aggregated stream.
/// Returns a `{ stream: snapshot }` map; absent streams (no PV in
/// the topology, no batteries, etc.) simply don't appear in the
/// map. Lets the SPA's Dashboard paint a populated tile on page
/// load instead of holding "loading…" until the next WS tick.
async fn microgrid_latest(
    Extension(state): Extension<SharedMicrogrid>,
) -> Json<HashMap<&'static str, MicrogridSampleSnapshot>> {
    Json(state.latest.read().clone())
}

/// Rendered formula strings (e.g. `"#1 + COALESCE(#2, #3, 0.0)"`)
/// per stream, lifted from the graph crate's per-category formula
/// generators. Inspection-only — these are what the dashboard
/// tooltip surfaces so a developer reading "−25 kW" can see which
/// component ids participate and how. Absent categories don't
/// appear in the response.
///
/// 503 when the loopback Microgrid handle hasn't built its
/// ComponentGraph yet — same lifecycle as `/api/microgrid/status`.
async fn microgrid_formulas(
    Extension(state): Extension<SharedMicrogrid>,
) -> (StatusCode, Json<HashMap<&'static str, String>>) {
    let lm = match state.microgrid.read().as_ref() {
        Some(mg) => mg.logical_meter(),
        None => return (StatusCode::SERVICE_UNAVAILABLE, Json(HashMap::new())),
    };
    let graph = lm.graph();
    let mut out: HashMap<&'static str, String> = HashMap::new();
    if let Ok(f) = graph.grid_formula() {
        out.insert("grid_power", format!("{f}"));
    }
    if let Ok(f) = graph.battery_formula(None) {
        out.insert("battery_pool_power", format!("{f}"));
    }
    if let Ok(f) = graph.pv_formula(None) {
        out.insert("pv_power", format!("{f}"));
    }
    if let Ok(f) = graph.consumer_formula() {
        out.insert("consumer_power", format!("{f}"));
    }
    if let Ok(f) = graph.producer_formula() {
        out.insert("producer_power", format!("{f}"));
    }
    (StatusCode::OK, Json(out))
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
    /// Latest graph-validator outcome. `None` = the graph crate
    /// accepted the topology; `Some(msg)` = it rejected with the
    /// human-readable error string. The pulse-bar graph pill
    /// flips between ✓ and ⚠ on this field.
    graph_status: Option<String>,
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
        hidden_connections: world.hidden_connections(),
        graph_status: config.graph_status(),
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
struct FormatQuery {
    /// Column budget for the formatter. Optional; defaults to 80.
    /// Clamped to a sane range so a stray client can't make
    /// `tulisp-fmt` chew through pathological inputs.
    width: Option<usize>,
}

/// Pretty-print a Lisp source string via `tulisp-fmt`. The body is
/// the raw source; the response is the formatted source as
/// text/plain. Returns 400 with the formatter's error message on
/// parse failure so the REPL can keep the user's input untouched
/// and surface the diagnostic.
async fn format(
    Query(q): Query<FormatQuery>,
    body: String,
) -> Result<String, (StatusCode, String)> {
    let width = q.width.unwrap_or(80).clamp(20, 200);
    tulisp_fmt::format_with_width(&body, width)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))
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
    let metric: Metric = q.metric.parse().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            format!("unknown metric '{}'", q.metric),
        )
    })?;
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

/// Category names the defaults endpoint walks to fetch each
/// `*-defaults` alist out of the running interpreter. Order is
/// stable so the UI's defaults editor renders the same sections
/// every time. New component categories need to be added here AND
/// to the corresponding `(setq foo-defaults '((...)))` block in
/// `sim/defaults.lisp` (otherwise the endpoint silently drops the
/// new category — `eval_silent` on an unbound symbol fails and
/// the entry is skipped).
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
            // Pretty-print via tulisp-fmt so the textarea shows
            // one (key . value) pair per line at a narrow width
            // — fits the side panel without horizontal scroll.
            // Falls back to the raw Display form if the printed
            // value isn't parseable (shouldn't happen for an alist
            // read back from the interpreter). Variables that
            // aren't bound just get skipped.
            if let Ok(value) = config.eval_silent(&var) {
                let formatted = tulisp_fmt::format_with_width(&value, 50)
                    .map(|f| f.trim_end().to_string())
                    .unwrap_or(value);
                out.push(DefaultsEntry {
                    category: cat,
                    var_name: var,
                    value: formatted,
                });
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
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("task panicked: {e}"),
            )
        })?;
    match result {
        Ok(0) => Err((
            StatusCode::NOT_FOUND,
            format!("no persisted override at idx {idx}"),
        )),
        Ok(_) => Ok(StatusCode::NO_CONTENT),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("write failed: {e}"),
        )),
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
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("task panicked: {e}"),
                )
            })?;
    match result {
        Ok(removed) => Ok(Json(BulkRemoveResponse { removed })),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("write failed: {e}"),
        )),
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
    /// Lowest event id still in the ring. Clients comparing
    /// `earliest_event_id > since` know their cursor was inside
    /// the evicted window and they missed `earliest_event_id - since`
    /// entries.
    earliest_event_id: u64,
}

async fn scenario_events(
    State(config): State<Config>,
    Query(q): Query<ScenarioEventsQuery>,
) -> Json<ScenarioEventsResponse> {
    let since = q.since.unwrap_or(0);
    let limit = q.limit.unwrap_or(200).min(1000);
    let events = config.world().scenario_events_since(since, limit);
    let summary = config.world().scenario_summary(Utc::now());
    Json(ScenarioEventsResponse {
        events,
        next_event_id: summary.next_event_id,
        earliest_event_id: summary.earliest_event_id,
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
                    if let Ok(json) = serde_json::to_string(&event)
                        && socket.send(Message::Text(json.into())).await.is_err()
                    {
                        break;
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
