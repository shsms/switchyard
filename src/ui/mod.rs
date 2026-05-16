//! Embedded web UI server.
//!
//! Runs alongside the gRPC server on the same tokio runtime, separate
//! port (default 8801). The SPA shell + vendored assets are bundled
//! via rust-embed.

mod events_ws;
mod loopback;

use events_ws::events_ws;
pub use loopback::spawn_microgrid_loopback;

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use frequenz_microgrid::{Microgrid, MicrogridClientHandle};
use parking_lot::{Mutex, RwLock};
use rust_embed::Embed;
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

use crate::{
    lisp::Config,
    sim::{
        Category,
        history::Metric,
        microgrid_site::{ScenarioReport, ScenarioSummary},
        setpoints::SetpointEvent,
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
    /// Rolling history per stream (timestamp + value), ring-buffered
    /// to 1000 entries — 15 minutes at the 1 Hz forwarder cadence
    /// with a little slack. Feeds `/api/microgrid/history` so the
    /// Dashboard tile sparklines can backfill on page load instead
    /// of starting empty.
    pub history: RwLock<HashMap<&'static str, VecDeque<HistorySample>>>,
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
        history: RwLock::new(HashMap::new()),
        forwarders: Mutex::new(Vec::new()),
    })
}

/// One point on a microgrid_sample stream's rolling history ring.
/// Cap = 1000 (15 min at 1 Hz with slack); oldest entry drops on
/// insert when full.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct HistorySample {
    pub ts_ms: i64,
    pub value: Option<f32>,
}

const MICROGRID_HISTORY_CAP: usize = 1000;

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
        .route("/api/microgrid/history", get(microgrid_history))
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
        .route(
            "/api/mg/{mg_id}/microgrid/status",
            get(microgrid_status_for_mg),
        )
        .route(
            "/api/mg/{mg_id}/microgrid/latest",
            get(microgrid_latest_for_mg),
        )
        .route(
            "/api/mg/{mg_id}/microgrid/history",
            get(microgrid_history_for_mg),
        )
        .route(
            "/api/mg/{mg_id}/microgrid/formulas",
            get(microgrid_formulas_for_mg),
        )
        .route(
            "/api/mg/{mg_id}/overrides/text",
            get(overrides_text_for_mg).post(overrides_text_replace_for_mg),
        )
        .route(
            "/api/mg/{mg_id}/dispatches",
            get(dispatches_for_mg).post(dispatch_create_for_mg),
        )
        .route(
            "/api/mg/{mg_id}/dispatches/{dispatch_id}",
            delete(dispatch_delete_for_mg),
        )
        .route(
            "/api/mg/{mg_id}/dispatches/{dispatch_id}/active",
            post(dispatch_set_active_for_mg),
        )
        .route("/ws/events", get(events_ws))
        .layer(Extension(microgrid))
        .layer(Extension(loopbacks))
        .layer(Extension(spawner))
        .with_state(config)
}

/// JSON shape for one dispatch in the per-microgrid Dispatches view.
/// Timestamps are epoch-millis so the SPA formats them client-side via
/// its TZ toggle, like every other UI timestamp. `target` / `recurrence`
/// are pre-rendered human strings — the SPA only displays them.
#[derive(Serialize)]
struct DispatchView {
    id: u64,
    #[serde(rename = "type")]
    type_: String,
    active: bool,
    dry_run: bool,
    start_ms: Option<i64>,
    duration_s: Option<u32>,
    end_ms: Option<i64>,
    create_ms: Option<i64>,
    update_ms: Option<i64>,
    target: String,
    recurrence: Option<String>,
    payload: serde_json::Value,
}

/// Per-microgrid dispatch list, read straight from the shared
/// `DispatchStore` (no gRPC round-trip). Newest-created first — ids
/// are monotonic, so descending id order matches. Returns `[]` for a
/// microgrid with no dispatches; the store, not the registry, is the
/// authority here, so an unknown `mg_id` simply yields an empty list.
async fn dispatches_for_mg(
    State(config): State<Config>,
    Path(mg_id): Path<u64>,
) -> Json<Vec<DispatchView>> {
    let views = config
        .dispatches()
        .list_mg(mg_id)
        .iter()
        .rev()
        .map(dispatch_to_view)
        .collect();
    Json(views)
}

/// Body for `POST /api/mg/{id}/dispatches`. `target` is the same
/// human syntax the dispatch CLI takes (category names or numeric
/// ids); `payload` is free JSON (must be an object). With no
/// `start_ms` the dispatch starts immediately.
#[derive(Deserialize)]
struct DispatchCreateReq {
    #[serde(rename = "type")]
    type_: String,
    target: String,
    #[serde(default)]
    duration_s: Option<u32>,
    #[serde(default)]
    active: Option<bool>,
    #[serde(default)]
    dry_run: Option<bool>,
    #[serde(default)]
    payload: Option<serde_json::Value>,
    #[serde(default)]
    start_ms: Option<i64>,
}

/// Create a dispatch from the UI. Parses the human target / payload,
/// then goes through the same `DispatchStore::create` the gRPC server
/// uses, so the construction rules are identical.
async fn dispatch_create_for_mg(
    State(config): State<Config>,
    Path(mg_id): Path<u64>,
    Json(req): Json<DispatchCreateReq>,
) -> Result<(StatusCode, Json<DispatchView>), (StatusCode, String)> {
    let target = crate::sim::dispatch::parse_target(&req.target)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let payload = match req.payload {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => Some(
            crate::sim::dispatch::json_to_struct(&value)
                .map_err(|e| (StatusCode::BAD_REQUEST, e))?,
        ),
    };
    let start_immediately = req.start_ms.is_none();
    let start_time = req.start_ms.map(|ms| prost_types::Timestamp {
        seconds: ms.div_euclid(1000),
        nanos: (ms.rem_euclid(1000) * 1_000_000) as i32,
    });
    let data = crate::proto::dispatch::DispatchData {
        r#type: req.type_,
        start_time,
        duration: req.duration_s,
        target: Some(target),
        is_active: req.active.unwrap_or(true),
        is_dry_run: req.dry_run.unwrap_or(false),
        payload,
        recurrence: None,
    };
    let dispatch = config
        .dispatches()
        .create(mg_id, data, start_immediately)
        .map_err(dispatch_err_to_http)?;
    Ok((StatusCode::CREATED, Json(dispatch_to_view(&dispatch))))
}

/// Body for `POST /api/mg/{id}/dispatches/{did}/active` — pause
/// (`false`) or resume (`true`).
#[derive(Deserialize)]
struct DispatchSetActiveReq {
    active: bool,
}

async fn dispatch_set_active_for_mg(
    State(config): State<Config>,
    Path((mg_id, dispatch_id)): Path<(u64, u64)>,
    Json(req): Json<DispatchSetActiveReq>,
) -> Result<Json<DispatchView>, (StatusCode, String)> {
    let dispatch = config
        .dispatches()
        .set_active(mg_id, dispatch_id, req.active)
        .map_err(dispatch_err_to_http)?;
    Ok(Json(dispatch_to_view(&dispatch)))
}

async fn dispatch_delete_for_mg(
    State(config): State<Config>,
    Path((mg_id, dispatch_id)): Path<(u64, u64)>,
) -> Result<StatusCode, (StatusCode, String)> {
    config.dispatches().remove(mg_id, dispatch_id).ok_or((
        StatusCode::NOT_FOUND,
        format!("dispatch {dispatch_id} not found for microgrid {mg_id}"),
    ))?;
    Ok(StatusCode::NO_CONTENT)
}

fn dispatch_err_to_http(err: crate::sim::dispatch::DispatchError) -> (StatusCode, String) {
    use crate::sim::dispatch::DispatchError;
    let code = match err {
        DispatchError::MissingStartTime => StatusCode::BAD_REQUEST,
        DispatchError::NotFound => StatusCode::NOT_FOUND,
    };
    (code, err.to_string())
}

fn dispatch_to_view(d: &crate::proto::dispatch::Dispatch) -> DispatchView {
    let data = d.data.clone().unwrap_or_default();
    let meta = d.metadata.unwrap_or_default();
    DispatchView {
        id: meta.dispatch_id,
        type_: data.r#type,
        active: data.is_active,
        dry_run: data.is_dry_run,
        start_ms: data.start_time.as_ref().map(ts_to_ms),
        duration_s: data.duration,
        end_ms: meta.end_time.as_ref().map(ts_to_ms),
        create_ms: meta.create_time.as_ref().map(ts_to_ms),
        update_ms: meta.update_time.as_ref().map(ts_to_ms),
        target: crate::sim::dispatch::target_to_string(data.target.as_ref()),
        recurrence: recurrence_to_string(data.recurrence.as_ref()),
        payload: data
            .payload
            .as_ref()
            .map(crate::sim::dispatch::struct_to_json)
            .unwrap_or(serde_json::Value::Null),
    }
}

fn ts_to_ms(ts: &prost_types::Timestamp) -> i64 {
    // i128 + clamp: an extreme start_time stored via gRPC (seconds near
    // i64::MAX) must not overflow `seconds * 1000` when the UI lists it.
    let ms = ts.seconds as i128 * 1000 + (ts.nanos as i128) / 1_000_000;
    ms.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

/// Compact recurrence summary, e.g. `daily ×2`. `None` for a
/// non-recurring dispatch (no rule, or an unspecified frequency).
fn recurrence_to_string(rule: Option<&crate::proto::dispatch::RecurrenceRule>) -> Option<String> {
    use crate::proto::dispatch::recurrence_rule::Frequency;
    let rule = rule?;
    let freq = Frequency::try_from(rule.freq).ok()?;
    if freq == Frequency::Unspecified {
        return None;
    }
    let label = freq
        .as_str_name()
        .strip_prefix("FREQUENCY_")
        .unwrap_or(freq.as_str_name())
        .to_lowercase();
    let interval = rule.interval.max(1);
    Some(format!("{label} ×{interval}"))
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
        MicrogridDef, MicrogridEntry, next_free_id_in, next_free_port_in,
    };
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "name must be non-empty".into()));
    }
    let registry = config.microgrids();
    let site = crate::sim::MicrogridSite::with_id_allocator(config.enterprise_id_allocator());
    // Allocate id + port AND insert the entry under one lock so
    // concurrent creates can't pick the same port (the earlier
    // shape probed both before locking; two simultaneous calls
    // could land on the same grpc_port and the second tonic
    // listener would fail to bind silently inside its tokio task).
    let (id, grpc_port, def) = {
        let mut r = registry.lock();
        let id = next_free_id_in(&r);
        let grpc_port = next_free_port_in(&r);
        let def = MicrogridDef {
            id,
            name: name.clone(),
            grpc_port,
            tso: body.tso.clone(),
        };
        r.insert(
            id,
            MicrogridEntry {
                def: def.clone(),
                site: site.clone(),
            },
        );
        (id, grpc_port, def)
    };
    // Persist the per-mg config stub BEFORE spawning the runtime.
    // If the write fails the next boot would orphan the live tasks
    // (gRPC server, loopback, physics, history sampler) since the
    // stub is what re-creates the microgrid at load-time. Rolling
    // back the registry insert + bailing out keeps the failure
    // mode clean: nothing started, nothing leaked.
    if let Err(e) = write_microgrid_stub(&config, id, &name, grpc_port, body.tso.as_deref()) {
        registry.lock().remove(&id);
        return Err((StatusCode::INTERNAL_SERVER_ERROR, e));
    }
    // Boot the new microgrid's runtime (physics + history +
    // gRPC server + loopback) via the binary-supplied spawner.
    // Tests pass a no-op spawner.
    spawner(id, &name, grpc_port, site);
    // Notify enterprise-wide subscribers (the WS event pump) so live
    // UI sessions start receiving topology_changed / sample events
    // from the new microgrid without a page reload.
    config.notify_microgrid_registered(id);
    Ok(Json(CreateMicrogridResp {
        id,
        name: def.name,
        grpc_port,
        tso: def.tso,
    }))
}

/// Write `microgrids/config.<id>.lisp` for a runtime-created entry.
/// The stub carries a `(make-microgrid …)` form pinned to this id /
/// port / tso, plus an empty `:topology` lambda that just
/// `(load-overrides)`s — the UI populates the topology over time by
/// appending to the per-mg overrides file next to this stub. Errors
/// out instead of clobbering an existing file (concurrent creates
/// shouldn't fight over the same path, but the registry already
/// dedups by id so this is just paranoia).
fn write_microgrid_stub(
    config: &Config,
    id: u64,
    name: &str,
    grpc_port: u16,
    tso: Option<&str>,
) -> Result<(), String> {
    let dir = config.microgrids_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let path = dir.join(format!("config.{id}.lisp"));
    if path.exists() {
        return Err(format!(
            "stub file {} already exists; refusing to clobber",
            path.display()
        ));
    }
    // Escape only " and \ inside the name string. The TSO is one of
    // the four short codes ("TN" / "AM" / "HZ" / "BW") or unset, so
    // the same escape rule covers it.
    fn esc(s: &str) -> String {
        s.replace('\\', "\\\\").replace('"', "\\\"")
    }
    let tso_form = match tso {
        Some(t) if !t.is_empty() => format!(" :tso \"{}\"", esc(t)),
        _ => String::new(),
    };
    let content = format!(
        ";; Runtime-created microgrid (id {id}). Edit by hand or via\n\
         ;; the UI — UI edits land in config.{id}.overrides.lisp next\n\
         ;; to this file.\n\
         \n\
         (make-microgrid\n\
        \x20:id {id}\n\
        \x20:name \"{name_esc}\"\n\
        \x20:grpc-port {grpc_port}{tso_form}\n\
        \x20:topology\n\
        \x20(lambda ()\n\
        \x20  (load-overrides)))\n",
        name_esc = esc(name),
    );
    std::fs::write(&path, content).map_err(|e| format!("write {}: {e}", path.display()))
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
    op: impl FnOnce(Config, chrono::DateTime<chrono::Utc>) -> Result<(), String> + Send + 'static,
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

async fn microgrid_history_for_mg(
    Extension(loopbacks): Extension<MicrogridLoopbacks>,
    Path(mg_id): Path<u64>,
) -> Result<Json<HashMap<&'static str, Vec<HistorySample>>>, (StatusCode, String)> {
    let slot = resolve_loopback(&loopbacks, mg_id)?;
    Ok(Json(microgrid_history_body(&slot)))
}

async fn microgrid_history(
    Extension(state): Extension<SharedMicrogrid>,
) -> Json<HashMap<&'static str, Vec<HistorySample>>> {
    Json(microgrid_history_body(&state))
}

fn microgrid_history_body(state: &SharedMicrogrid) -> HashMap<&'static str, Vec<HistorySample>> {
    state
        .history
        .read()
        .iter()
        .map(|(k, ring)| (*k, ring.iter().copied().collect()))
        .collect()
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

fn topology_snapshot(config: &Config, site: &crate::sim::MicrogridSite) -> TopologySnapshot {
    let components = site
        .components()
        .iter()
        .map(|c| {
            let runtime = site.runtime_of(c.id());
            ComponentSummary {
                id: c.id(),
                name: site
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
        connections: site.connections(),
        hidden_connections: site.hidden_connections(),
        graph_status: config.graph_status(),
        main_meter_id: site.main_meter_id(),
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
        crate::sim::microgrids::with_microgrid(&config.current_microgrid_handle(), mg_id, || {
            config.eval(&body)
        })
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

/// Return the raw text of a microgrid's overrides file, or empty
/// string if it doesn't exist yet. The undo stack on the canvas
/// snapshots this before each mutating eval so Ctrl-Z can restore
/// the prior shape verbatim.
async fn overrides_text_for_mg(
    State(config): State<Config>,
    Path(mg_id): Path<u64>,
) -> Result<String, (StatusCode, String)> {
    if !config.microgrids().lock().contains_key(&mg_id) {
        return Err((
            StatusCode::NOT_FOUND,
            format!("microgrid {mg_id} not registered"),
        ));
    }
    tokio::task::spawn_blocking(move || {
        crate::sim::microgrids::with_microgrid(&config.current_microgrid_handle(), mg_id, || {
            config.overrides_text()
        })
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task panicked: {e}"),
        )
    })?
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("read failed: {e}"),
        )
    })
}

/// Overwrite a microgrid's overrides file with the body and reload.
/// Used by the canvas undo stack to restore a prior snapshot — the
/// body is the full file contents, not a delta.
async fn overrides_text_replace_for_mg(
    State(config): State<Config>,
    Path(mg_id): Path<u64>,
    body: String,
) -> Result<StatusCode, (StatusCode, String)> {
    if !config.microgrids().lock().contains_key(&mg_id) {
        return Err((
            StatusCode::NOT_FOUND,
            format!("microgrid {mg_id} not registered"),
        ));
    }
    tokio::task::spawn_blocking(move || {
        crate::sim::microgrids::with_microgrid(&config.current_microgrid_handle(), mg_id, || {
            config.replace_overrides_text(&body)
        })
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task panicked: {e}"),
        )
    })?
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("write failed: {e}"),
        )
    })?;
    Ok(StatusCode::NO_CONTENT)
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
