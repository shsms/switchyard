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

use crate::sim::{MicrogridSite, events::SiteEvent};

use crate::{
    lisp::Config,
    sim::{
        Category,
        history::Metric,
        setpoints::SetpointEvent,
        microgrid_site::{ScenarioReport, ScenarioSummary},
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
/// next WS tick. Mirrors the [`SiteEvent::MicrogridSample`]
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
    /// The microgrid client, built once on the first
    /// `build_microgrid` call via `MicrogridClientHandle::try_new`
    /// and reused for every rebuild — only the `LogicalMeterHandle`
    /// (which embeds the graph snapshot) gets replaced when the
    /// topology changes. A new client per rebuild would close the
    /// previous one's instructions channel, and
    /// `MicrogridClientActor` in frequenz-microgrid 0.4.1
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

/// Enterprise map from microgrid id to its loopback state. Each
/// `MicrogridServer` registered in `Config::microgrids` gets one
/// entry — the supervisor for each entry pulls samples through
/// the matching microgrid's gRPC server and feeds the entry's
/// per-stream cache.
///
/// `BTreeMap` keeps the entries ordered by id so the UI's
/// Microgrids list and `/api/mg/{id}/microgrid/latest` lookups
/// stay deterministic. Behind an `Arc<RwLock>` so handlers can
/// take a read lock for lookups without blocking new-microgrid
/// inserts coming from the create-microgrid endpoint.
pub type MicrogridLoopbacks =
    std::sync::Arc<parking_lot::RwLock<std::collections::BTreeMap<u64, SharedMicrogrid>>>;

pub fn new_microgrid_loopbacks() -> MicrogridLoopbacks {
    std::sync::Arc::new(parking_lot::RwLock::new(std::collections::BTreeMap::new()))
}

/// Callback the create-microgrid HTTP endpoint invokes once the
/// registry insertion is complete: spawn the physics tick +
/// history sampler + Microgrid gRPC server + loopback client for
/// the freshly-added microgrid. Concrete implementations live in
/// `src/bin/switchyard.rs` (production boot) and the integration
/// tests (a no-op closure when the test fixture doesn't drive
/// runtime microgrid creation).
///
/// Args: `(id, name, grpc_port, site)`. Implementations decide
/// how to react — e.g. test fixtures may want to skip the gRPC
/// listener spawn.
pub type MicrogridSpawner =
    std::sync::Arc<dyn Fn(u64, &str, u16, crate::sim::MicrogridSite) + Send + Sync>;

/// No-op spawner. Used in integration-test fixtures + the
/// snapshot-only tests that don't exercise the runtime create
/// path.
pub fn noop_microgrid_spawner() -> MicrogridSpawner {
    std::sync::Arc::new(|_id, _name, _port, _site| {})
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
/// [`MicrogridSite::broadcast_microgrid_sample`]; the existing `/ws/events`
/// stream then carries the samples to the SPA without any extra
/// wiring — they ride the same `SiteEvent` discriminator the
/// per-component samples already use.
pub fn spawn_microgrid_loopback(grpc_url: String, slot: SharedMicrogrid, world: MicrogridSite) {
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

/// Build a fresh `Microgrid` and wire up its forwarders. Same
/// code path for the initial boot and every subsequent rebuild:
/// `slot.client` is lazily initialised on first call via
/// `MicrogridClientHandle::try_new(grpc_url)`, then reused
/// forever. Each call builds a fresh `LogicalMeterHandle` against
/// the current topology and assembles the `Microgrid` via
/// `new_from_handles`. The old `Microgrid` (replaced in `slot`)
/// drops normally; its `LogicalMeterActor` exits cleanly because
/// it handles a closed instructions channel by breaking out.
///
/// Forwarder subscriptions are awaited synchronously **before** the
/// slot swap. The shared `MicrogridClientActor` caches a
/// `broadcast::Sender` per component and its backing tonic stream
/// task exits the moment it sees `receiver_count == 0` between
/// upstream samples (see
/// <https://github.com/frequenz-floss/frequenz-microgrid-rs/issues/…>).
/// Subscribing the new LM first keeps that count ≥ 1 across the
/// handoff, so the stream task survives and samples reach the new
/// forwarders without a multi-second silence.
///
/// Returns false if the gRPC connect or graph build fails outright
/// (which the crate normally retries through; a hard failure means
/// something like a malformed URL).
async fn build_microgrid(grpc_url: &str, slot: &SharedMicrogrid, world: &MicrogridSite) -> bool {
    // Lazy client init. `MicrogridClientHandle::try_new` doesn't
    // contact the server — the connection is established lazily on
    // the first RPC — so this is cheap to call. It does validate
    // the URL though, hence the Result.
    let client = match slot
        .client
        .get_or_try_init(|| MicrogridClientHandle::try_new(grpc_url.to_owned()))
        .await
    {
        Ok(c) => c.clone(),
        Err(e) => {
            log::error!("microgrid loopback: client try_new failed: {e}");
            return false;
        }
    };
    // 1 Hz sample cadence matches the existing history sampler;
    // dashboard tiles refresh at this rate. LogicalMeterHandle's
    // try_new internally loops on the graph build until it
    // succeeds, so a topology mid-mutation just delays this call
    // rather than returning Err.
    let config = LogicalMeterConfig::new(chrono::TimeDelta::seconds(1));
    let lm = match LogicalMeterHandle::try_new(client.clone(), config).await {
        Ok(lm) => lm,
        Err(e) => {
            log::error!("microgrid loopback: logical-meter setup failed: {e}");
            return false;
        }
    };
    let mut mg = Microgrid::new_from_handles(client, lm);
    let handles = subscribe_power_forwarders(&mut mg, world, slot.clone()).await;
    // Atomic swap. Aborting the old forwarders + dropping the old
    // Microgrid happens AFTER the new LM has subscribed to every
    // component it cares about (above), so the shared client's
    // per-component broadcast Senders never see receiver_count drop
    // to zero between LM generations.
    for h in slot.forwarders.lock().drain(..) {
        h.abort();
    }
    slot.latest.write().clear();
    *slot.forwarders.lock() = handles;
    *slot.microgrid.write() = Some(mg);
    true
}

/// Subscribe to MicrogridSite events and rebuild the Microgrid handle on
/// every TopologyChanged. Lagged-receiver and dropped-sender
/// events also trigger a rebuild (defensive — a missed event
/// might have been a topology change).
async fn run_supervisor(grpc_url: String, slot: SharedMicrogrid, world: MicrogridSite) {
    let mut events = world.subscribe_events();
    loop {
        match events.recv().await {
            Ok(SiteEvent::TopologyChanged { .. }) => {
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
async fn debounce_topology_burst(events: &mut tokio::sync::broadcast::Receiver<SiteEvent>) {
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

/// Rebuild the `LogicalMeterHandle` so its graph snapshot reflects
/// the new topology. `build_microgrid` does the work — it
/// subscribes the new forwarders first, then atomically aborts the
/// old ones and swaps the slot. The old `Microgrid` stays in the
/// slot until then so the shared client's per-component broadcast
/// Senders keep at least one live receiver across the handoff.
///
/// Only the `LogicalMeterHandle` inside the new Microgrid is
/// rebuilt; the `MicrogridClientHandle` cached in `slot.client` is
/// reused. See the field doc for why the client is long-lived.
async fn rebuild(grpc_url: &str, slot: &SharedMicrogrid, world: &MicrogridSite) {
    log::info!("microgrid loopback: topology changed — rebuilding handle");
    build_microgrid(grpc_url, slot, world).await;
}

/// Build subscriptions for the active-power streams the Dashboard
/// tier-1 (grid), tier-2 (battery pool), tier-3 (PV), and tier-4
/// (consumer + producer aggregates) read from, and spawn one tokio
/// task per surviving subscription to forward samples onto the
/// MicrogridSite event bus.
///
/// Each `formula.subscribe().await` is run on the caller's task so
/// that, when this function returns, the new LM has already
/// subscribed all its required components through the shared
/// client. That keeps `build_microgrid`'s swap step safe: the old
/// `Microgrid` can drop without ever taking the shared client's
/// per-component broadcast receiver count to zero.
///
/// Streams whose underlying category is absent (no PV in the
/// topology, etc.) emit a single `log::info!` and are silently
/// dropped — the Dashboard's matching tile renders as "data
/// unavailable" until that category appears.
async fn subscribe_power_forwarders(
    microgrid: &mut Microgrid,
    world: &MicrogridSite,
    state: SharedMicrogrid,
) -> Vec<JoinHandle<()>> {
    let mut handles = Vec::new();
    let lm = microgrid.logical_meter();
    let metered: [(&'static str, _); 4] = [
        ("grid_power", lm.grid::<metric::AcPowerActive>()),
        ("consumer_power", lm.consumer::<metric::AcPowerActive>()),
        ("producer_power", lm.producer::<metric::AcPowerActive>()),
        ("pv_power", lm.pv::<metric::AcPowerActive>(None)),
    ];
    for (stream, formula) in metered {
        if let Some(h) = subscribe_power_forwarder(stream, formula, world, state.clone()).await {
            handles.push(h);
        }
    }
    // Grid frequency via `lm.grid::<metric::AcFrequency>()` would
    // be the natural way to feed a "Grid frequency" tile, but
    // frequenz-microgrid 0.4.1's LogicalMeterActor's
    // `TypedFormulaResponseSender` branches only on Power /
    // Voltage / ReactivePower / Current — calling `.subscribe()`
    // on the Frequency formula returns `Internal: Can't create
    // TypedFormulaResponseSender for ...Frequency`. See
    // /vagrant/upstream-frequency-formula.md. Until that lands
    // upstream, frequency stays on the per-component
    // /api/history?metric=frequency_hz path.
    // BatteryPool takes &mut self for power() / power_bounds() (it
    // caches subscriber refs); build it once and let it go out of
    // scope after both subscriptions resolve.
    match microgrid.battery_pool(None) {
        Ok(mut pool) => {
            if let Some(h) =
                subscribe_power_forwarder("battery_pool_power", pool.power(), world, state.clone())
                    .await
            {
                handles.push(h);
            }
            // power_bounds returns a Vec<Bounds<Power>>; the
            // forwarder flattens the first envelope into two
            // separate streams so the existing point-sample
            // infrastructure (cache + sparkline) renders both
            // halves without an envelope-shaped payload variant.
            handles.push(spawn_bounds_forwarder(pool.power_bounds(), world, state));
        }
        Err(e) => log::info!("microgrid loopback: battery pool absent — skipping: {e}"),
    }
    handles
}

/// Forward a `Vec<Bounds<Power>>` stream as two point streams
/// `battery_pool_bounds_lower` + `battery_pool_bounds_upper`. The
/// upstream tracker emits a fresh Vec on every telemetry snapshot,
/// so the cadence matches the power forwarders' 1 Hz; sparklines
/// alongside the pool power tile track the same time axis.
///
/// When the Vec is empty (no batteries in the pool) both halves
/// publish `None`. When it has multiple disjoint regions we keep
/// only the outermost envelope — single-region is by far the
/// common case and a multi-region split is a niche signal that the
/// developer-facing dashboard isn't designed around.
fn spawn_bounds_forwarder(
    mut rx: tokio::sync::broadcast::Receiver<Vec<frequenz_microgrid::Bounds<Power>>>,
    world: &MicrogridSite,
    state: SharedMicrogrid,
) -> JoinHandle<()> {
    let world = world.clone();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(envelopes) => {
                    let lower = outer_bound(&envelopes, |b| b.lower(), f32::min);
                    let upper = outer_bound(&envelopes, |b| b.upper(), f32::max);
                    let ts_ms = chrono::Utc::now().timestamp_millis();
                    publish_scalar(
                        "battery_pool_bounds_lower",
                        "Power",
                        "W",
                        lower,
                        ts_ms,
                        &world,
                        &state,
                    );
                    publish_scalar(
                        "battery_pool_bounds_upper",
                        "Power",
                        "W",
                        upper,
                        ts_ms,
                        &world,
                        &state,
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    log::warn!("microgrid loopback: battery_pool_bounds lagged {n} samples");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    log::info!(
                        "microgrid loopback: battery_pool_bounds closed; forwarder exiting"
                    );
                    return;
                }
            }
        }
    })
}

fn outer_bound(
    envelopes: &[frequenz_microgrid::Bounds<Power>],
    pick: impl Fn(&frequenz_microgrid::Bounds<Power>) -> Option<Power>,
    fold: fn(f32, f32) -> f32,
) -> Option<f32> {
    envelopes
        .iter()
        .filter_map(|b| pick(b).map(|p| p.as_watts()))
        .reduce(fold)
}

/// Subscribe to one Power-valued formula and spawn a forwarder that
/// pushes each `Sample<Power>` onto the MicrogridSite event bus as a
/// `MicrogridSample { stream, quantity: "Power", unit: "W", ... }`
/// event. The `formula.subscribe().await` runs on the caller's task
/// so the LM has actually registered for the component samples by
/// the time we return — see `build_microgrid` for why that ordering
/// matters across rebuilds. Returns `None` (no spawn) if the formula
/// errored at construction (typical for absent categories) or the
/// initial subscribe failed.
async fn subscribe_power_forwarder(
    stream: &'static str,
    formula: Result<frequenz_microgrid::Formula<Power>, frequenz_microgrid::Error>,
    world: &MicrogridSite,
    state: SharedMicrogrid,
) -> Option<JoinHandle<()>> {
    let formula = match formula {
        Ok(f) => f,
        Err(e) => {
            log::info!("microgrid loopback: skip {stream} ({e})");
            return None;
        }
    };
    let mut rx = match formula.subscribe().await {
        Ok(rx) => rx,
        Err(e) => {
            log::warn!("microgrid loopback: subscribe {stream} failed: {e}");
            return None;
        }
    };
    let world = world.clone();
    Some(tokio::spawn(async move {
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

fn publish_power(stream: &'static str, sample: Sample<Power>, world: &MicrogridSite, state: &SharedMicrogrid) {
    let value = sample.value().map(|p| p.as_watts());
    let ts_ms = sample.timestamp().timestamp_millis();
    publish_scalar(stream, "Power", "W", value, ts_ms, world, state);
}

/// Push a typed scalar onto both the per-stream `latest` cache and
/// the WS event bus. The `quantity` + `unit` pair travels with the
/// sample so the SPA picks the right autoscale family (Power
/// W→kW→MW, Frequency Hz, etc.) without pattern-matching on the
/// stream name.
fn publish_scalar(
    stream: &'static str,
    quantity: &'static str,
    unit: &'static str,
    value: Option<f32>,
    ts_ms: i64,
    world: &MicrogridSite,
    state: &SharedMicrogrid,
) {
    let snapshot = MicrogridSampleSnapshot {
        quantity,
        unit,
        ts_ms,
        value,
    };
    state.latest.write().insert(stream, snapshot);
    world.broadcast_microgrid_sample(stream, quantity, unit, ts_ms, value);
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
    loopbacks: MicrogridLoopbacks,
    spawner: MicrogridSpawner,
) -> Result<(), std::io::Error> {
    let app = router(config, microgrid, loopbacks, spawner);
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
    loopbacks: MicrogridLoopbacks,
    spawner: MicrogridSpawner,
) -> Result<(), std::io::Error> {
    axum::serve(listener, router(config, microgrid, loopbacks, spawner)).await
}

fn router(
    config: Config,
    microgrid: SharedMicrogrid,
    loopbacks: MicrogridLoopbacks,
    spawner: MicrogridSpawner,
) -> Router {
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
        .route("/api/snapshots", get(snapshots_list))
        .route("/api/snapshots/save", post(snapshots_save))
        .route("/api/snapshots/load", post(snapshots_load))
        .route("/api/scenarios", get(scenarios_list))
        .route("/api/scenarios/{name}/start", post(scenarios_start))
        .route("/api/scenarios/{name}/stop", post(scenarios_stop))
        .route("/api/scenarios/{name}/next", post(scenarios_next))
        .route("/api/scenarios/{name}/prev", post(scenarios_prev))
        .route("/api/scenarios/{name}/jump/{idx}", post(scenarios_jump))
        .route("/api/microgrids", get(microgrids_list))
        .route("/api/microgrids/create", post(microgrids_create))
        .route("/api/mg/{mg_id}/topology", get(topology_for_mg))
        .route("/api/mg/{mg_id}/eval", post(eval_for_mg))
        .route("/api/mg/{mg_id}/history", get(history_for_mg))
        .route("/api/mg/{mg_id}/microgrid/status", get(microgrid_status_for_mg))
        .route("/api/mg/{mg_id}/microgrid/latest", get(microgrid_latest_for_mg))
        .route("/api/mg/{mg_id}/microgrid/formulas", get(microgrid_formulas_for_mg))
        .route("/ws/events", get(events_ws))
        .layer(Extension(microgrid))
        .layer(Extension(loopbacks))
        .layer(Extension(spawner))
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

async fn microgrids_list(
    State(config): State<Config>,
) -> Json<Vec<crate::sim::microgrids::MicrogridView>> {
    Json(crate::sim::microgrids::snapshot(&config.microgrids()))
}

/// Look up the site for `mg_id` in the registry. Per-microgrid
/// handlers call this at the start; a miss returns 404 verbatim
/// so the SPA can highlight a stale microgrid card and reload
/// without retrying every per-mg fetch on the page.
fn resolve_site(
    config: &Config,
    mg_id: u64,
) -> Result<crate::sim::MicrogridSite, (StatusCode, String)> {
    config
        .microgrids()
        .lock()
        .get(&mg_id)
        .map(|e| e.site.clone())
        .ok_or((
            StatusCode::NOT_FOUND,
            format!("microgrid {mg_id} not registered"),
        ))
}

/// Mirror of [`resolve_site`] for the loopback Microgrid client
/// slot a microgrid owns. Used by the per-mg
/// `/microgrid/{status,latest,formulas}` endpoints.
fn resolve_loopback(
    loopbacks: &MicrogridLoopbacks,
    mg_id: u64,
) -> Result<SharedMicrogrid, (StatusCode, String)> {
    loopbacks.read().get(&mg_id).cloned().ok_or((
        StatusCode::NOT_FOUND,
        format!("microgrid {mg_id} not registered"),
    ))
}

#[derive(Deserialize)]
struct CreateMicrogridBody {
    name: String,
    #[serde(default)]
    tso: Option<String>,
}

#[derive(Serialize)]
struct CreateMicrogridResp {
    id: u64,
    name: String,
    grpc_port: u16,
    tso: Option<String>,
}

/// POST /api/microgrids/create — auto-allocates id + grpc_port,
/// inserts a fresh entry in the registry, and invokes the
/// configured `MicrogridSpawner` so the new microgrid actually
/// boots its physics + history + Microgrid gRPC server +
/// loopback client without a process restart.
///
/// Empty-name requests are rejected. The new microgrid's site is
/// constructed with the shared enterprise id allocator so its
/// auto-allocated component ids stay globally unique.
async fn microgrids_create(
    State(config): State<Config>,
    Extension(spawner): Extension<MicrogridSpawner>,
    Json(body): Json<CreateMicrogridBody>,
) -> Result<Json<CreateMicrogridResp>, (StatusCode, String)> {
    use crate::sim::microgrids::{
        MicrogridDef, MicrogridEntry, next_free_id, next_free_port,
    };
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "name must be non-empty".into()));
    }
    let registry = config.microgrids();
    // next_free_id / next_free_port both lock the registry
    // internally, so they have to run *outside* the insert-lock
    // critical section to avoid a reentrant deadlock
    // (parking_lot::Mutex is non-reentrant). Concurrent creates
    // can in principle race on the allocator return values; the
    // duplicate-key insert below picks a sliding window starting
    // at the probed id to back off if we lost the race.
    let id = next_free_id(&registry);
    let grpc_port = next_free_port(&registry);
    let def = MicrogridDef {
        id,
        name: name.clone(),
        grpc_port,
        tso: body.tso.clone(),
    };
    let site = crate::sim::MicrogridSite::with_id_allocator(config.enterprise_id_allocator());
    {
        let mut r = registry.lock();
        if r.contains_key(&id) {
            return Err((
                StatusCode::CONFLICT,
                format!("microgrid id {id} unexpectedly taken; retry"),
            ));
        }
        r.insert(
            id,
            MicrogridEntry {
                def: def.clone(),
                site: site.clone(),
            },
        );
    }
    // Boot the new microgrid's runtime (physics + history +
    // gRPC server + loopback) via the binary-supplied spawner.
    // Tests pass a no-op spawner.
    spawner(id, &name, grpc_port, site);
    // Notify enterprise-wide subscribers (the WS event pump) so live
    // UI sessions start receiving topology_changed / sample events
    // from the new microgrid without a page reload.
    config.notify_microgrid_registered(id);
    let def_clone = def;
    Ok(Json(CreateMicrogridResp {
        id,
        name: def_clone.name,
        grpc_port,
        tso: def_clone.tso,
    }))
}

async fn scenarios_list(
    State(config): State<Config>,
) -> Json<Vec<crate::sim::scenarios::ScenarioView>> {
    Json(crate::sim::scenarios::snapshot(&config.scenarios()))
}

/// Common shim for the mutate endpoints. Runs the closure on a
/// blocking thread so the tulisp funcall path (which holds the
/// interpreter lock) doesn't pin a tokio worker. Maps the
/// `Result<(), String>` from the helpers into an HTTP 4xx with the
/// helper's error string verbatim.
async fn run_scenario_op(
    config: Config,
    op: impl FnOnce(Config, chrono::DateTime<chrono::Utc>) -> Result<(), String>
        + Send
        + 'static,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let now = chrono::Utc::now();
    let res = tokio::task::spawn_blocking(move || op(config, now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("join: {e}")))?;
    res.map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn scenarios_start(
    State(config): State<Config>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    run_scenario_op(config, move |cfg, now| {
        let clock = cfg.clock_handle().read().clone();
        crate::sim::scenarios::start(
            &cfg.scenarios(),
            &cfg.interpreter(),
            &cfg.microgrids(),
            &cfg.current_microgrid_handle(),
            &clock,
            &name,
            now,
        )
    })
    .await
}

async fn scenarios_stop(
    State(config): State<Config>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    run_scenario_op(config, move |cfg, now| {
        crate::sim::scenarios::stop(&cfg.scenarios(), &cfg.microgrids(), &name, now)
    })
    .await
}

async fn scenarios_next(
    State(config): State<Config>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    run_scenario_op(config, move |cfg, now| {
        crate::sim::scenarios::step(
            &cfg.scenarios(),
            &cfg.interpreter(),
            &cfg.microgrids(),
            &cfg.current_microgrid_handle(),
            &name,
            1,
            now,
        )
    })
    .await
}

async fn scenarios_prev(
    State(config): State<Config>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    run_scenario_op(config, move |cfg, now| {
        crate::sim::scenarios::step(
            &cfg.scenarios(),
            &cfg.interpreter(),
            &cfg.microgrids(),
            &cfg.current_microgrid_handle(),
            &name,
            -1,
            now,
        )
    })
    .await
}

async fn scenarios_jump(
    State(config): State<Config>,
    Path((name, idx)): Path<(String, usize)>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    run_scenario_op(config, move |cfg, now| {
        crate::sim::scenarios::jump(
            &cfg.scenarios(),
            &cfg.interpreter(),
            &cfg.microgrids(),
            &cfg.current_microgrid_handle(),
            &name,
            idx,
            now,
        )
    })
    .await
}

#[derive(Serialize)]
struct SnapshotsListResp {
    snapshots: Vec<String>,
}

async fn snapshots_list(State(config): State<Config>) -> Json<SnapshotsListResp> {
    Json(SnapshotsListResp {
        snapshots: config.list_snapshots(),
    })
}

#[derive(Deserialize)]
struct SnapshotsBody {
    name: String,
}

async fn snapshots_save(
    State(config): State<Config>,
    Json(body): Json<SnapshotsBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let path = tokio::task::spawn_blocking(move || config.save_snapshot(&body.name))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("join: {e}")))?
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "path": path.display().to_string(),
    })))
}

async fn snapshots_load(
    State(config): State<Config>,
    Json(body): Json<SnapshotsBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    tokio::task::spawn_blocking(move || config.load_snapshot(&body.name))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("join: {e}")))?
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn microgrid_status_for_mg(
    Extension(loopbacks): Extension<MicrogridLoopbacks>,
    Path(mg_id): Path<u64>,
) -> Result<(StatusCode, Json<MicrogridStatusResp>), (StatusCode, String)> {
    let slot = resolve_loopback(&loopbacks, mg_id)?;
    Ok(microgrid_status_body(&slot))
}

async fn microgrid_latest_for_mg(
    Extension(loopbacks): Extension<MicrogridLoopbacks>,
    Path(mg_id): Path<u64>,
) -> Result<Json<HashMap<&'static str, MicrogridSampleSnapshot>>, (StatusCode, String)> {
    let slot = resolve_loopback(&loopbacks, mg_id)?;
    Ok(Json(slot.latest.read().clone()))
}

async fn microgrid_formulas_for_mg(
    Extension(loopbacks): Extension<MicrogridLoopbacks>,
    Path(mg_id): Path<u64>,
) -> Result<(StatusCode, Json<HashMap<&'static str, String>>), (StatusCode, String)> {
    let slot = resolve_loopback(&loopbacks, mg_id)?;
    Ok(microgrid_formulas_body(&slot))
}

fn microgrid_status_body(state: &SharedMicrogrid) -> (StatusCode, Json<MicrogridStatusResp>) {
    let lm = state.microgrid.read().as_ref().map(|mg| mg.logical_meter());
    if let Some(lm) = lm {
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

async fn microgrid_status(
    Extension(state): Extension<SharedMicrogrid>,
) -> (StatusCode, Json<MicrogridStatusResp>) {
    microgrid_status_body(&state)
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
    microgrid_formulas_body(&state)
}

fn microgrid_formulas_body(
    state: &SharedMicrogrid,
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
    /// Id of the meter flagged `:main t` in the topology, if any.
    /// The SPA's Grid-frequency tile pulls history from this id.
    main_meter_id: Option<u64>,
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
    Json(topology_snapshot(&config, &config.site()))
}

async fn topology_for_mg(
    State(config): State<Config>,
    Path(mg_id): Path<u64>,
) -> Result<Json<TopologySnapshot>, (StatusCode, String)> {
    let site = resolve_site(&config, mg_id)?;
    Ok(Json(topology_snapshot(&config, &site)))
}

fn topology_snapshot(config: &Config, world: &crate::sim::MicrogridSite) -> TopologySnapshot {
    let components = world
        .components()
        .iter()
        .map(|c| {
            let runtime = world.runtime_of(c.id());
            ComponentSummary {
                id: c.id(),
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
    TopologySnapshot {
        components,
        connections: world.connections(),
        hidden_connections: world.hidden_connections(),
        graph_status: config.graph_status(),
        main_meter_id: world.main_meter_id(),
    }
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
    eval_response(tokio::task::spawn_blocking(move || config.eval(&body)).await)
}

async fn eval_for_mg(
    State(config): State<Config>,
    Path(mg_id): Path<u64>,
    body: String,
) -> impl IntoResponse {
    if !config.microgrids().lock().contains_key(&mg_id) {
        return (
            StatusCode::NOT_FOUND,
            Json(EvalResponse {
                ok: false,
                value: None,
                error: Some(format!("microgrid {mg_id} not registered")),
            }),
        );
    }
    let result = tokio::task::spawn_blocking(move || {
        crate::sim::microgrids::with_microgrid(
            &config.current_microgrid_handle(),
            mg_id,
            || config.eval(&body),
        )
    })
    .await;
    eval_response(result)
}

fn eval_response(
    result: Result<Result<String, String>, tokio::task::JoinError>,
) -> (StatusCode, Json<EvalResponse>) {
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
        Err(join_err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(EvalResponse {
                ok: false,
                value: None,
                error: Some(format!("eval task panicked: {join_err}")),
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
    /// Typed quantity (`"Power"`, `"ReactivePower"`, `"Frequency"`,
    /// `"Percentage"`) — mirrors the frequenz-microgrid `Sample<Q>`
    /// `Q` parameter so the SPA picks a scale family from this
    /// instead of pattern-matching on the metric name.
    quantity: &'static str,
    /// Base unit the samples are recorded in (`"W"`, `"var"`,
    /// `"Hz"`, `"%"`).
    unit: &'static str,
    /// Pairs of (timestamp_ms_since_epoch, value). The time format is
    /// JS-ready (Date.now() shape) so chart libs can plot directly.
    samples: Vec<(i64, f32)>,
}

async fn history(
    State(config): State<Config>,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<HistoryResponse>, (StatusCode, String)> {
    history_body(&config.site(), q)
}

async fn history_for_mg(
    State(config): State<Config>,
    Path(mg_id): Path<u64>,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<HistoryResponse>, (StatusCode, String)> {
    let site = resolve_site(&config, mg_id)?;
    history_body(&site, q)
}

fn history_body(
    site: &crate::sim::MicrogridSite,
    q: HistoryQuery,
) -> Result<Json<HistoryResponse>, (StatusCode, String)> {
    let metric: Metric = q.metric.parse().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            format!("unknown metric '{}'", q.metric),
        )
    })?;
    let window = ChronoDuration::seconds(q.window_s.unwrap_or(600));
    let since: DateTime<Utc> = Utc::now() - window;
    let samples = site
        .history_window(q.id, metric, since)
        .unwrap_or_default()
        .into_iter()
        .map(|s| (s.ts.timestamp_millis(), s.value))
        .collect();
    Ok(Json(HistoryResponse {
        id: q.id,
        metric: q.metric,
        quantity: metric.quantity(),
        unit: metric.unit(),
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
    let events = config.site().setpoints_window(q.id, since);
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
    Json(config.site().scenario_summary(Utc::now()))
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
    let events = config.site().scenario_events_since(since, limit);
    let summary = config.site().scenario_summary(Utc::now());
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
    Json(config.site().scenario_report(Utc::now()))
}

/// WebSocket event push. Subscribers receive SiteEvent JSON for
/// every TopologyChanged + Sample broadcast. Client-sent frames are
/// drained but ignored — the channel is server-push only for v1; an
/// upcoming change adds a /api/eval-style RPC over the same socket
/// if it turns out latency-sensitive client actions benefit from it.
async fn events_ws(ws: WebSocketUpgrade, State(config): State<Config>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| event_pump(socket, config))
}

/// Wrap a SiteEvent with the originating microgrid id so the SPA
/// can filter samples / topology bumps / setpoint events by the
/// currently-active microgrid. `mg_id` is `None` for enterprise-
/// scoped events (terminal log lines, ws lag notices).
#[derive(Serialize)]
struct WireEvent<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    mg_id: Option<u64>,
    #[serde(flatten)]
    event: &'a crate::sim::events::SiteEvent,
}

async fn event_pump(mut socket: WebSocket, config: Config) {
    use tokio::sync::broadcast::error::RecvError as BroadcastRecv;
    let mut log_rx = crate::ui_log::LOG_TAP.get().map(|t| t.subscribe());
    // Subscribe to every microgrid's per-site event bus + the
    // enterprise-wide `microgrid_registered` channel. The initial
    // snapshot covers every entry present at connect time; the
    // registered-channel branch in the select! below spawns a fresh
    // forwarder when /api/microgrids/create or (make-microgrid)
    // adds an entry mid-session. One forwarder task per site tags
    // events with the originating mg_id and pushes onto a shared
    // mpsc that the select! drains into the WebSocket.
    let (fwd_tx, mut fwd_rx) =
        tokio::sync::mpsc::channel::<(u64, crate::sim::events::SiteEvent)>(512);
    fn spawn_forwarder(
        mg_id: u64,
        mut rx: tokio::sync::broadcast::Receiver<crate::sim::events::SiteEvent>,
        tx: tokio::sync::mpsc::Sender<(u64, crate::sim::events::SiteEvent)>,
    ) {
        use tokio::sync::broadcast::error::RecvError;
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        if tx.send((mg_id, ev)).await.is_err() {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        log::warn!("ws: microgrid {mg_id} event bus lagged {n} samples");
                        continue;
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        });
    }
    {
        let reg = config.microgrids();
        let r = reg.lock();
        for (id, entry) in r.iter() {
            spawn_forwarder(*id, entry.site.subscribe_events(), fwd_tx.clone());
        }
    }
    let mut registered_rx = config.subscribe_microgrid_registered();
    // Keep one clone of the fwd_tx alive on this task so fwd_rx
    // stays open across registration bursts — the per-mg forwarders
    // each hold their own clone, but a window of "no microgrids yet"
    // would otherwise close the mpsc and kill the loop.
    let fwd_tx_keepalive = fwd_tx.clone();
    drop(fwd_tx);
    loop {
        tokio::select! {
            ev = fwd_rx.recv() => match ev {
                Some((mg_id, event)) => {
                    let wire = WireEvent { mg_id: Some(mg_id), event: &event };
                    let json = match serde_json::to_string(&wire) {
                        Ok(j) => j,
                        Err(e) => {
                            log::error!("ws: serde error: {e}");
                            continue;
                        }
                    };
                    if socket.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
                None => break, // every forwarder dropped
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
                    let event = crate::sim::events::SiteEvent::Log {
                        ts_ms: line.ts_ms,
                        level: line.level,
                        target: line.target,
                        message: line.message,
                    };
                    let wire = WireEvent { mg_id: None, event: &event };
                    if let Ok(json) = serde_json::to_string(&wire)
                        && socket.send(Message::Text(json.into())).await.is_err()
                    {
                        break;
                    }
                }
                Err(BroadcastRecv::Lagged(_)) => continue,
                Err(BroadcastRecv::Closed) => log_rx = None,
            },
            msg = socket.recv() => match msg {
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
            // A new microgrid landed in the registry (from
            // `(make-microgrid)` or /api/microgrids/create). Spawn
            // a forwarder for its site so this WS session starts
            // receiving its sample / topology_changed events.
            // Subscribers can lag if registrations burst past the
            // 64-slot channel — continue past Lagged because the
            // SPA can recover via reconnect, and Closed never
            // fires since Config keeps the Sender alive for the
            // process lifetime.
            new_id = registered_rx.recv() => match new_id {
                Ok(id) => {
                    let entry = config.microgrids().lock().get(&id).cloned();
                    if let Some(e) = entry {
                        spawn_forwarder(id, e.site.subscribe_events(), fwd_tx_keepalive.clone());
                    } else {
                        log::warn!("ws: microgrid_registered({id}) but registry has no entry");
                    }
                }
                Err(BroadcastRecv::Lagged(n)) => {
                    log::warn!("ws: microgrid_registered channel lagged {n} ids");
                    continue;
                }
                Err(BroadcastRecv::Closed) => {
                    // Config dropped its Sender; nothing more will arrive.
                    // Don't break — the existing forwarders keep working.
                    std::future::pending::<()>().await;
                }
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
