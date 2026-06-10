//! The simulation registry, scheduler, and shared environment.
//!
//! `MicrogridSite` owns every component, the parent → child topology, and the
//! external grid state (per-phase voltage, frequency) that components
//! query when computing AC quantities.
//!
//! On every `physics_tick_ms` interval, `tick_once` walks the
//! components in registration order (children first because Lisp
//! evaluates `:successors` before the surrounding `make-*` call) and
//! invokes `SimulatedComponent::tick` on each.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use chrono::{DateTime, Utc};
use parking_lot::RwLock;

use tokio::sync::broadcast;

use crate::sim::component::{ComponentHandle, FIRST_AUTO_ID, SimulatedComponent};
use crate::sim::events::{EVENT_BUS_CAPACITY, SiteEvent};
use crate::sim::history::ComponentHistory;
use crate::sim::runtime::{CommandMode, ComponentRuntime, Health, TelemetryMode};
use crate::sim::scenario::ScenarioJournal;
use crate::sim::scenario_csv::CsvSinks;
use crate::sim::setpoints::{SetpointEvent, SetpointLog, SetpointOutcome};
use crate::timeout_tracker::TimeoutTracker;

mod history;
mod scenarios;

pub(crate) use scenarios::{ScenarioReport, ScenarioSummary};

/// Hard cap on per-component-per-metric ring buffer length. At the
/// fixed 1 Hz history sampling cadence (see `spawn_history_sampler`)
/// this works out to a 10-minute window per series — plenty for the
/// "what was my control app doing recently" use case.
const HISTORY_CAPACITY: usize = 600;

/// Cap on per-component setpoint-log length. Setpoint requests
/// arrive at the gRPC server's pace; a busy control app might land
/// 10/sec on one component. 1000 entries ≈ 100 s of dense traffic
/// or several minutes of typical use; older events evict.
const SETPOINT_LOG_CAPACITY: usize = 1000;

/// External AC environment shared by all AC components. Mirrors
/// microsim's `voltage-per-phase` / `ac-frequency` globals.
#[derive(Debug, Clone)]
pub struct GridState {
    pub voltage_per_phase: (f32, f32, f32),
    pub frequency_hz: f32,
}

impl Default for GridState {
    fn default() -> Self {
        Self {
            voltage_per_phase: (230.0, 230.0, 230.0),
            frequency_hz: 50.0,
        }
    }
}

#[derive(Clone)]
pub struct MicrogridSite {
    inner: Arc<MicrogridSiteInner>,
}

struct MicrogridSiteInner {
    components: RwLock<Vec<Arc<dyn SimulatedComponent>>>,
    by_id: RwLock<HashMap<u64, Arc<dyn SimulatedComponent>>>,
    connections: RwLock<Vec<(u64, u64)>>,
    grid_state: RwLock<GridState>,
    physics_tick_ms: AtomicU64,
    /// *Process-wide* component-id allocator, cloned across every
    /// `MicrogridSite` in the enterprise so component ids stay
    /// globally unique across microgrids — matching the platform,
    /// where ids are enterprise-scoped. Two sites in the same registry
    /// share the same `Arc<AtomicU64>`; calling `next_id` on
    /// either advances the same counter.
    ///
    /// Single-site / legacy paths construct a fresh allocator
    /// per `MicrogridSite::new()` so they keep the prior
    /// per-site numbering behaviour without coordination — only
    /// the multi-microgrid path (`(make-microgrid …)` via the
    /// registry) wires sites to a shared allocator via
    /// `MicrogridSite::with_id_allocator`.
    next_id: Arc<AtomicU64>,
    /// Bumped by `cancel_all_streams()`. Streaming tasks in server.rs
    /// compare against the value they captured at start and break when
    /// it has changed. Models a server-initiated graceful cancel of
    /// every active stream.
    stream_cancel_epoch: AtomicU64,
    /// Server-side artificial lag added to every sample's timestamp.
    /// When > 0, the protobuf message's timestamps are shifted into
    /// the past by this many milliseconds — modelling a server that
    /// delivers samples with stale timestamps.
    sample_lag_ms: AtomicU64,
    /// Per-component runtime mode flags (health, telemetry mode,
    /// command mode). Defaulted on register, mutated via the
    /// `set-component-*` Lisp defuns or directly from server.rs.
    runtime: RwLock<HashMap<u64, ComponentRuntime>>,
    /// User-facing name overrides set via `(rename-component …)`.
    /// Reads go through `display_name`; the component's intrinsic
    /// `SimulatedComponent::name()` stays as the auto-derived default.
    name_overrides: RwLock<HashMap<u64, String>>,
    /// Per-component telemetry history rings, populated by the
    /// `spawn_history_sampler` task. Read by the UI's `/api/history`
    /// endpoint. Cleared on `reset()` so a hot-reload starts charts
    /// fresh.
    histories: RwLock<HashMap<u64, ComponentHistory>>,
    /// Per-component log of incoming setpoint requests + outcome.
    /// Populated by the gRPC server handlers for SetActivePower /
    /// SetReactivePower / AugmentBounds; read by /api/setpoints for
    /// the UI's control inspector.
    setpoint_logs: RwLock<HashMap<u64, SetpointLog>>,
    /// Monotonic version counter; bumped via `bump_version` on every
    /// accepted /api/eval (and future programmatic mutations) so UI
    /// tabs know to refetch /api/topology.
    version: AtomicU64,
    /// Broadcast bus for live UI subscribers. Senders are cheap to
    /// clone; receivers are obtained via `subscribe_events`.
    events: broadcast::Sender<SiteEvent>,
    /// Per-component setpoint expiry deadlines. Both the gRPC
    /// `SetElectricalComponentPower` handler and the `(set-power …)`
    /// Lisp defun add to this; a single tokio task in
    /// `Config::start_timeout_loop` polls for expirations and calls
    /// `reset_setpoint` on each. Living on MicrogridSite means the loop runs
    /// once per process regardless of which call sites schedule.
    timeout_tracker: TimeoutTracker,
    /// Scenario lifecycle + event journal. Scoped to the MicrogridSite
    /// rather than the Config because long-running scenarios
    /// outlive an `eval_file` call and the gRPC server reads from
    /// it via `MicrogridSite::scenario_*`.
    scenario: RwLock<ScenarioJournal>,
    /// Id of the meter flagged with `:main t` at construction. The
    /// scenario reporter tracks its active-power peak, and the
    /// `/api/scenario/report` endpoint surfaces it. At most one
    /// meter may carry the flag — `set_main_meter` returns Err if
    /// a second tries to claim it.
    main_meter_id: RwLock<Option<u64>>,
    /// Per-component CSV sinks active during the scenario.
    /// Populated by `(scenario-record-csv DIR)`; drained on
    /// `(scenario-stop-csv)` or implicitly by `scenario-stop`.
    /// Empty by default — recording is opt-in.
    scenario_csv: RwLock<CsvSinks>,
    /// Received-setpoint CSV sinks — one per envelope-bearing
    /// component, written event-driven from `log_setpoint`. Same
    /// open/close lifecycle as `scenario_csv`.
    scenario_setpoints_csv: RwLock<CsvSinks>,
    /// Effective-active-bounds CSV sinks — one per envelope-bearing
    /// component, sampled by `record_history_snapshot` at the same
    /// 1 Hz pass as telemetry. Same lifecycle as `scenario_csv`.
    scenario_bounds_csv: RwLock<CsvSinks>,
    /// Optional handle on the grid frequency state. Wired
    /// in by `Config::new` so every MicrogridSite in the registry
    /// reads the same OU-driven frequency value (one AC grid →
    /// one frequency, by physics). Bootstrap MicrogridSites built
    /// outside that path (tags pass, unit tests) leave it `None`
    /// and fall back to the per-mg `grid_state.frequency_hz`.
    grid_frequency: RwLock<Option<crate::sim::frequency::SharedFrequency>>,
}

impl MicrogridSite {
    pub fn new() -> Self {
        Self::with_id_allocator(Arc::new(AtomicU64::new(FIRST_AUTO_ID)))
    }

    /// Build a `MicrogridSite` that shares the supplied id
    /// allocator with whichever other sites already hold a clone.
    /// `(make-microgrid …)` uses this so every site in the
    /// registry draws auto-ids from one process-wide counter and
    /// the enterprise-wide id-uniqueness invariant holds without
    /// coordination on the lisp side.
    pub fn with_id_allocator(next_id: Arc<AtomicU64>) -> Self {
        Self {
            inner: Arc::new(MicrogridSiteInner {
                components: RwLock::new(Vec::new()),
                by_id: RwLock::new(HashMap::new()),
                connections: RwLock::new(Vec::new()),
                grid_state: RwLock::new(GridState::default()),
                physics_tick_ms: AtomicU64::new(100),
                next_id,
                runtime: RwLock::new(HashMap::new()),
                name_overrides: RwLock::new(HashMap::new()),
                histories: RwLock::new(HashMap::new()),
                setpoint_logs: RwLock::new(HashMap::new()),
                version: AtomicU64::new(0),
                events: broadcast::channel(EVENT_BUS_CAPACITY).0,
                timeout_tracker: TimeoutTracker::new(),
                scenario: RwLock::new(ScenarioJournal::default()),
                main_meter_id: RwLock::new(None),
                scenario_csv: RwLock::new(CsvSinks::new()),
                scenario_setpoints_csv: RwLock::new(CsvSinks::new()),
                scenario_bounds_csv: RwLock::new(CsvSinks::new()),
                grid_frequency: RwLock::new(None),
                stream_cancel_epoch: AtomicU64::new(0),
                sample_lag_ms: AtomicU64::new(0),
            }),
        }
    }

    /// Read the current sample-lag offset (ms). The server uses this
    /// to shift telemetry timestamps into the past, modelling a server
    /// that delivers samples with stale timestamps.
    pub fn sample_lag_ms(&self) -> u64 {
        self.inner.sample_lag_ms.load(Ordering::Acquire)
    }

    /// Set the sample-lag offset (ms). 0 = use the wall clock; > 0
    /// shifts every sample's timestamp into the past by that many ms.
    pub fn set_sample_lag_ms(&self, ms: u64) {
        self.inner.sample_lag_ms.store(ms, Ordering::Release);
    }

    /// Current stream-cancel epoch. Streaming tasks capture this on
    /// start; on each iteration they re-read it and break if it has
    /// changed since their start. Used by `cancel_all_streams()` to
    /// drop every active stream from the server side without killing
    /// the process.
    pub fn stream_cancel_epoch(&self) -> u64 {
        self.inner.stream_cancel_epoch.load(Ordering::Acquire)
    }

    /// Bump the stream-cancel epoch. Every currently-running stream
    /// task will see the change on its next iteration (≤ one stream
    /// interval) and exit cleanly. New clients reconnecting after will
    /// pick up the new epoch and stream normally.
    pub fn cancel_all_streams(&self) {
        self.inner
            .stream_cancel_epoch
            .fetch_add(1, Ordering::Release);
    }

    /// Wire this site to an grid frequency source. After this
    /// call, `grid_state()` reads `frequency_hz` from the shared OU
    /// state instead of the per-mg `GridState::frequency_hz` slot.
    /// Voltage stays per-mg.
    pub fn set_grid_frequency(&self, freq: crate::sim::frequency::SharedFrequency) {
        *self.inner.grid_frequency.write() = Some(freq);
    }

    /// Id of the meter currently flagged as the microgrid's main /
    /// point-of-common-coupling meter (via `:main t` on `make-meter`).
    /// `None` if no meter has the flag — pure-PV / pure-battery
    /// topologies are valid. The UI's frequency tile reads this to
    /// pick which meter's history to sample for grid frequency,
    /// since frequenz-microgrid 0.4.1's LogicalMeter can't carry a
    /// `Sample<Frequency>` formula through its actor.
    pub fn main_meter_id(&self) -> Option<u64> {
        *self.inner.main_meter_id.read()
    }

    /// Mark `id` as the main meter. Returns `Err` if a different
    /// meter already holds the flag — the make-path treats that
    /// as a config error and surfaces it as a Lisp error.
    pub(crate) fn set_main_meter(&self, id: u64) -> Result<(), String> {
        let mut g = self.inner.main_meter_id.write();
        if let Some(existing) = *g
            && existing != id
        {
            return Err(format!(
                "main meter already set to {existing}; can't claim {id}",
            ));
        }
        *g = Some(id);
        Ok(())
    }

    // ─── Setpoint timeouts ────────────────────────────────────────────
    //
    // Each accepted setpoint schedules a deadline; on expiry the gRPC
    // / Config loop pulls the id out via `drain_expired_timeouts`
    // and calls `reset_setpoint` on the component.

    /// Schedule a setpoint expiry for `id` at `now + lifetime`.
    /// Replaces any previously-scheduled deadline for that id —
    /// "latest set wins" semantics, matching microsim's behavior.
    pub fn add_timeout(&self, id: u64, lifetime: Duration) {
        self.inner.timeout_tracker.add(id, lifetime);
    }

    /// Drain any deadlines that have elapsed and return their ids.
    /// Called by `Config`'s timeout loop, which then calls
    /// `reset_setpoint` on each.
    pub fn drain_expired_timeouts(&self) -> Vec<u64> {
        self.inner.timeout_tracker.remove_expired()
    }

    // ─── Version counter + event broadcast bus ────────────────────────
    //
    // Every accepted /api/eval bumps `version`, which fires a
    // `TopologyChanged` on the broadcast bus. Live UI tabs listen
    // and refetch /api/topology on each bump.

    pub fn version(&self) -> u64 {
        self.inner.version.load(Ordering::Relaxed)
    }

    /// Increment the version counter and broadcast a
    /// `TopologyChanged` event. Returns the new version. Send errors
    /// (no live subscribers) are swallowed — the event is fire-and-
    /// forget by design.
    pub fn bump_version(&self) -> u64 {
        let v = self.inner.version.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = self
            .inner
            .events
            .send(SiteEvent::TopologyChanged { version: v });
        v
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<SiteEvent> {
        self.inner.events.subscribe()
    }

    /// Broadcast a `ConfigError` on the site event bus. Used by the
    /// watcher's reload-failure path so UI subscribers can render a
    /// "config invalid" banner instead of seeing the post-reset
    /// empty site without explanation. Fire-and-forget — a send
    /// error means there are no live subscribers, which is fine.
    pub fn broadcast_config_error(&self, message: String) {
        let _ = self.inner.events.send(SiteEvent::ConfigError {
            ts_ms: chrono::Utc::now().timestamp_millis(),
            message,
        });
    }

    /// Broadcast one aggregated-stream sample from the loopback
    /// Microgrid client. The forwarder tasks in
    /// `ui::spawn_microgrid_loopback` call this for each
    /// `Sample<Q>` they receive; the SPA's WS reads them off
    /// `/ws/events`. Fire-and-forget for the same reason
    /// [`Self::broadcast_config_error`] is.
    pub fn broadcast_microgrid_sample(
        &self,
        stream: &'static str,
        quantity: &'static str,
        unit: &'static str,
        ts_ms: i64,
        value: Option<f32>,
    ) {
        let _ = self.inner.events.send(SiteEvent::MicrogridSample {
            stream,
            quantity,
            unit,
            ts_ms,
            value,
        });
    }

    // ─── Scheduler knobs + grid state ────────────────────────────────
    //
    // `physics_tick` is the cadence at which `spawn_physics` runs
    // every component's `tick`. `grid_state` is the environmental
    // state (per-phase voltage + frequency) that components read
    // during tick.

    pub fn next_id(&self) -> u64 {
        self.inner.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn physics_tick(&self) -> Duration {
        Duration::from_millis(self.inner.physics_tick_ms.load(Ordering::Relaxed))
    }

    pub fn set_physics_tick_ms(&self, ms: u64) {
        self.inner.physics_tick_ms.store(ms, Ordering::Relaxed);
    }

    pub fn grid_state(&self) -> GridState {
        let mut state = self.inner.grid_state.read().clone();
        if let Some(freq) = self.inner.grid_frequency.read().as_ref() {
            state.frequency_hz = freq.read().read_hz();
        }
        state
    }

    pub fn set_grid_state(&self, state: GridState) {
        *self.inner.grid_state.write() = state;
    }

    // ─── Component registry + topology graph ─────────────────────────
    //
    // Components register via `register` / `register_arc` and land in
    // both `components` (registration order = tick order) and `by_id`
    // (for O(1) lookup). `connections` carries every parent→child
    // edge — `connections()` filters to visible, `hidden_connections`
    // returns the rest; `children_of` is the unfiltered walk that
    // aggregation paths use.

    pub fn register<C: SimulatedComponent + 'static>(&self, c: C) -> ComponentHandle {
        self.register_arc(Arc::new(c))
    }

    pub fn register_arc(&self, c: Arc<dyn SimulatedComponent>) -> ComponentHandle {
        let id = c.id();
        self.inner.components.write().push(c.clone());
        self.inner.by_id.write().insert(id, c.clone());
        // Default runtime mode: every flag at "Normal" — i.e. emit
        // telemetry, accept commands, report physics-derived state.
        self.inner.runtime.write().entry(id).or_default();
        ComponentHandle::from_arc(c)
    }

    pub fn connect(&self, parent: u64, child: u64) {
        self.inner.connections.write().push((parent, child));
    }

    /// Visible edges only — drops any edge whose parent or child is
    /// marked hidden. gRPC ListConnections and the UI topology graph
    /// both want this filtered view. Use [`Self::all_connections`]
    /// for aggregation paths that need the unfiltered set.
    pub fn connections(&self) -> Vec<(u64, u64)> {
        let by_id = self.inner.by_id.read();
        self.inner
            .connections
            .read()
            .iter()
            .filter(|(p, c)| {
                !by_id.get(p).map(|x| x.is_hidden()).unwrap_or(false)
                    && !by_id.get(c).map(|x| x.is_hidden()).unwrap_or(false)
            })
            .copied()
            .collect()
    }

    /// Edges where at least one endpoint is hidden — the complement
    /// of [`Self::connections`]. The UI surfaces these as a separate
    /// `hidden_connections` field so a hidden meter's outgoing edges
    /// can be drawn dashed while still leaving the gRPC graph clean.
    pub fn hidden_connections(&self) -> Vec<(u64, u64)> {
        let by_id = self.inner.by_id.read();
        self.inner
            .connections
            .read()
            .iter()
            .filter(|(p, c)| {
                by_id.get(p).map(|x| x.is_hidden()).unwrap_or(false)
                    || by_id.get(c).map(|x| x.is_hidden()).unwrap_or(false)
            })
            .copied()
            .collect()
    }

    /// Every edge from `parent`, hidden or not. Used by aggregation
    /// paths (meter / inverter / `aggregate_child_bounds`) that need
    /// to walk the *physical* graph; the visible-only filter in
    /// [`Self::connections`] is for the user-facing surface.
    /// `connect` and `disconnect` flow through the same
    /// underlying vec, so anything wired up post-make from the UI /
    /// REPL automatically lands here.
    pub fn children_of(&self, parent: u64) -> Vec<u64> {
        self.inner
            .connections
            .read()
            .iter()
            .filter_map(|(p, c)| (*p == parent).then_some(*c))
            .collect()
    }

    pub fn components(&self) -> Vec<Arc<dyn SimulatedComponent>> {
        self.inner.components.read().clone()
    }

    pub fn get(&self, id: u64) -> Option<Arc<dyn SimulatedComponent>> {
        self.inner.by_id.read().get(&id).cloned()
    }

    /// Number of `(parent, child)` edges in the connections graph
    /// where `child == id`. A meter aggregating a child that's
    /// shared with a sibling meter (parallel paths) divides the
    /// child's flow by this count so the sum at the parent of those
    /// siblings doesn't double-count. Returns 0 for hidden children
    /// whose edges were intentionally suppressed; callers should
    /// treat 0 as "this meter is the sole consumer" by clamping with
    /// `.max(1)`.
    pub fn parent_count(&self, id: u64) -> usize {
        self.inner
            .connections
            .read()
            .iter()
            .filter(|(_, c)| *c == id)
            .count()
    }

    /// Sum the `effective_active_bounds()` of every direct child of
    /// `parent`. Returns `None` when `parent` has no children that
    /// expose bounds.
    ///
    /// The microgrid API gateway uses this to gate setpoints against
    /// the downstream physical envelope — a real inverter has no data
    /// link to its battery's BMS limits, but the gateway sees both
    /// telemetry streams and intersects them on the client's behalf.
    pub fn aggregate_child_bounds(&self, parent: u64) -> Option<crate::sim::bounds::VecBounds> {
        use crate::sim::bounds::VecBounds;
        let child_ids: Vec<u64> = self
            .inner
            .connections
            .read()
            .iter()
            .filter(|(p, _)| *p == parent)
            .map(|(_, c)| *c)
            .collect();
        if child_ids.is_empty() {
            return None;
        }
        let bounds: Vec<VecBounds> = child_ids
            .iter()
            .filter_map(|id| self.get(*id))
            .filter_map(|c| c.effective_active_bounds())
            .collect();
        if bounds.is_empty() {
            None
        } else {
            Some(VecBounds::sum_single(bounds))
        }
    }

    /// Wipe every registered component. Called from `(reset-state)` in
    /// the config DSL on hot-reload. Also resets the id allocator so a
    /// reloaded config sees the same ids the previous load saw,
    /// matching microsim's `(setq comp--id--counter 1000)` behaviour.
    ///
    /// Reset *also* clears scenario-scoped state (the journal,
    /// per-component CSV sinks, the main-meter flag) so a hot-reload
    /// truly starts from scratch — leaving them in place leaked stale
    /// integrals against gone-and-reborn ids and blocked a reload
    /// from claiming a *different* meter as main.
    ///
    /// Grid state is environmental (set by the config's `every`
    /// timer); we deliberately keep it across reloads so the first
    /// tick after reload still has plausible per-phase voltage /
    /// frequency values.
    pub fn reset(&self) {
        self.inner.components.write().clear();
        self.inner.by_id.write().clear();
        self.inner.connections.write().clear();
        self.inner.runtime.write().clear();
        self.inner.name_overrides.write().clear();
        self.inner.histories.write().clear();
        self.inner.setpoint_logs.write().clear();
        *self.inner.scenario.write() = ScenarioJournal::default();
        *self.inner.main_meter_id.write() = None;
        // `clear()` drops every sink; each BufWriter flushes on drop.
        self.inner.scenario_csv.write().clear();
        self.inner.scenario_setpoints_csv.write().clear();
        self.inner.scenario_bounds_csv.write().clear();
        // Deliberately do NOT rewind `next_id`: the allocator is shared
        // across every site in an enterprise, so a per-site reset (a lone
        // `(reset-microgrid)`) must not rewind the global counter while
        // other sites still hold live components at higher ids — the next
        // auto-allocation would then collide. A full `Config::reload`
        // resets the allocator explicitly when every site is rebuilt.
    }

    /// Remove a component from the registry and drop every edge that
    /// touches it (in or out). Returns true if the component was
    /// present. The Arc held by any in-flight gRPC stream task keeps
    /// the underlying component alive until the subscriber drops —
    /// the registry just stops handing it out from `get()`.
    pub fn remove_component(&self, id: u64) -> bool {
        let was_present = self.inner.by_id.write().remove(&id).is_some();
        self.inner.components.write().retain(|c| c.id() != id);
        self.inner
            .connections
            .write()
            .retain(|(p, c)| *p != id && *c != id);
        self.inner.histories.write().remove(&id);
        self.inner.runtime.write().remove(&id);
        self.inner.setpoint_logs.write().remove(&id);
        self.inner.name_overrides.write().remove(&id);
        // If the removed component was the flagged main meter, free the
        // slot so a different meter can claim `:main` afterwards and the
        // scenario report stops tracking a component that's gone.
        {
            let mut main = self.inner.main_meter_id.write();
            if *main == Some(id) {
                *main = None;
            }
        }
        was_present
    }

    /// Drop every `(parent, child)` edge from the graph. Returns
    /// true if at least one edge was removed. Doesn't touch either
    /// endpoint's registration.
    ///
    /// Duplicates collapse — if `(connect …)` was called
    /// twice with the same pair, one disconnect removes both. The
    /// connections graph carries no positional identity, so there's
    /// no "remove only the first instance" semantics.
    pub fn disconnect(&self, parent: u64, child: u64) -> bool {
        let mut edges = self.inner.connections.write();
        let before = edges.len();
        edges.retain(|(p, c)| !(*p == parent && *c == child));
        edges.len() < before
    }

    /// Override a component's display name. Reads via `display_name`;
    /// `SimulatedComponent::name()` is unchanged so internal log
    /// lines and physics-derived state keep their stable default.
    pub fn rename(&self, id: u64, name: String) {
        self.inner.name_overrides.write().insert(id, name);
    }

    /// User-facing display name — override if present, else the
    /// component's intrinsic `name()`. Returns `None` when the id
    /// isn't registered (and no override was placed for a since-
    /// removed component).
    pub fn display_name(&self, id: u64) -> Option<String> {
        if let Some(n) = self.inner.name_overrides.read().get(&id) {
            return Some(n.clone());
        }
        self.inner
            .by_id
            .read()
            .get(&id)
            .map(|c| c.name().to_string())
    }

    // ─── Per-component runtime modes ─────────────────────────────────
    //
    // Health / telemetry mode / command mode flags carried in
    // `runtime`. Defaulted on register; mutated via the
    // `set-component-*` Lisp defuns or gRPC. `runtime_of` returns
    // the current snapshot; the per-setter methods mutate in place.

    pub fn runtime_of(&self, id: u64) -> ComponentRuntime {
        self.inner
            .runtime
            .read()
            .get(&id)
            .copied()
            .unwrap_or_default()
    }

    pub fn set_health(&self, id: u64, health: Health) {
        let mut runtime = self.inner.runtime.write();
        let entry = runtime.entry(id).or_default();
        entry.health = health;
        // Couple command handling to health: an errored device is also
        // unreachable for commands (both setpoints and bounds are
        // rejected); clearing the error restores normal command handling.
        match health {
            Health::Error => entry.command = CommandMode::Error,
            Health::Ok => entry.command = CommandMode::Normal,
            Health::Standby => {}
        }
    }

    pub fn set_telemetry_mode(&self, id: u64, mode: TelemetryMode) {
        self.inner.runtime.write().entry(id).or_default().telemetry = mode;
    }

    pub fn set_command_mode(&self, id: u64, mode: CommandMode) {
        self.inner.runtime.write().entry(id).or_default().command = mode;
    }

    // ─── Physics tick ────────────────────────────────────────────────
    //
    // `tick_once` runs one synchronous pass over every component;
    // `spawn_physics` is the long-running task that does it on a
    // `tokio::time::interval`. Pre-tick hook fires first so Lisp-
    // driven inputs resolve once per tick before any `tick()` reads
    // an atomic.

    /// Tick every registered component once. Children are stored before
    /// parents, so a single forward pass updates leaves before the
    /// meters that aggregate them.
    ///
    /// Pure Rust — does NOT enter the Lisp interpreter. Lambda-bound
    /// component inputs (`:power`, `:sunlight%`, …) are refreshed
    /// by `Config`'s dedicated lisp-refresh task on its own 100 ms
    /// cadence; this method only reads the atomic scalars those
    /// refreshes leave behind. Tests that need a synchronous refresh
    /// before driving `tick_once` should call `Config::refresh_once`.
    pub fn tick_once(&self, now: DateTime<Utc>, dt: Duration) {
        let components = self.inner.components.read().clone();
        for c in components {
            c.tick(self, now, dt);
        }
    }

    /// Spawn the physics loop. Returns immediately. The loop holds an
    /// `Arc` clone of the MicrogridSite, so the MicrogridSite cannot drop until the
    /// task exits — and right now there is no exit path. That's fine
    /// for the long-running binary but means tests that need a clean
    /// shutdown should call `tick_once` directly instead.
    pub fn spawn_physics(self) {
        tokio::spawn(async move {
            let mut last = Utc::now();
            let mut interval = tokio::time::interval(self.physics_tick());
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                let now = Utc::now();
                let dt = (now - last)
                    .to_std()
                    .unwrap_or_else(|_| Duration::from_millis(0));
                last = now;
                self.tick_once(now, dt);
                // Re-read the tick interval each iteration so config
                // changes take effect without a restart.
                let target = self.physics_tick();
                if interval.period() != target {
                    interval = tokio::time::interval(target);
                    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                }
            }
        });
    }

    // ─── Setpoint event log ──────────────────────────────────────────
    //
    // Per-component rolling log of accepted / rejected setpoint
    // requests. Populated by the gRPC handlers; read by the UI's
    // /api/setpoints inspector. Each `log_setpoint` also broadcasts
    // on the event bus for live UI updates.

    /// Append a setpoint event to the per-component log + broadcast
    /// it on the site event bus so live UI inspectors update without
    /// a refetch. Auto-creates the ring on first push; bounded to
    /// `SETPOINT_LOG_CAPACITY` entries (oldest evict).
    pub fn log_setpoint(&self, id: u64, event: SetpointEvent) {
        let ts_ms = event.ts.timestamp_millis();
        let kind = event.kind.as_str();
        let value = event.value;
        let (accepted, reason) = match &event.outcome {
            SetpointOutcome::Accepted { .. } => (true, None),
            SetpointOutcome::Rejected { reason } => (false, Some(reason.clone())),
        };
        // Scenario recording first (the ring push consumes the event).
        // Event-driven rather than sampled: a control app can issue
        // several requests between two 1 Hz passes and a replay wants
        // every one of them.
        if let Some(sink) = self.inner.scenario_setpoints_csv.write().get_mut(&id)
            && let Err(e) = sink.write_setpoint_row(&event)
        {
            log::warn!("setpoints CSV write failed for {id}: {e}");
        }
        self.inner
            .setpoint_logs
            .write()
            .entry(id)
            .or_insert_with(|| SetpointLog::new(SETPOINT_LOG_CAPACITY))
            .push(event);
        let _ = self.inner.events.send(SiteEvent::Setpoint {
            id,
            ts_ms,
            setpoint_kind: kind,
            value,
            accepted,
            reason,
        });
    }

    /// Read the recent setpoint events for one component.  Returns
    /// owned events so the caller can release the lock immediately.
    /// Empty Vec when the component has no recorded setpoints yet —
    /// either because it's new or because no client has set anything.
    pub fn setpoints_window(&self, id: u64, since: DateTime<Utc>) -> Vec<SetpointEvent> {
        self.inner
            .setpoint_logs
            .read()
            .get(&id)
            .map(|log| log.iter_window(since).cloned().collect())
            .unwrap_or_default()
    }
}

impl Default for MicrogridSite {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_health_couples_command_mode() {
        let w = MicrogridSite::new();
        // Erroring a component makes it unreachable for commands too.
        w.set_health(5, Health::Error);
        assert_eq!(w.runtime_of(5).health, Health::Error);
        assert_eq!(w.runtime_of(5).command, CommandMode::Error);
        // Clearing the error restores normal command handling.
        w.set_health(5, Health::Ok);
        assert_eq!(w.runtime_of(5).command, CommandMode::Normal);
        // Standby refuses via the health check but leaves command mode alone.
        w.set_command_mode(5, CommandMode::Timeout);
        w.set_health(5, Health::Standby);
        assert_eq!(w.runtime_of(5).command, CommandMode::Timeout);
    }

    /// Two meters can list the same inverter as a successor and both
    /// edges land in the connections graph (a parallel-meter
    /// setup). `aggregate_child_bounds` from either parent finds its
    /// own children independently — no double-counting at the bounds
    /// layer.
    #[test]
    fn shared_child_under_two_parents() {
        let w = MicrogridSite::new();
        w.connect(2, 100);
        w.connect(3, 100);
        let conns = w.connections();
        assert_eq!(conns.len(), 2);
        assert!(conns.contains(&(2, 100)));
        assert!(conns.contains(&(3, 100)));
        // No registered component for id 100 in this lightweight
        // test, so aggregate_child_bounds returns None — we're
        // checking the connection-graph shape, not the bounds math.
        assert!(w.aggregate_child_bounds(2).is_none());
        assert!(w.aggregate_child_bounds(3).is_none());
    }

    /// `children_of` is the unfiltered list of edges from a parent.
    /// Hidden-aware filtering happens at the `connections()` /
    /// `hidden_connections()` boundary using registered components'
    /// `is_hidden()`; this helper is the raw graph walk used by
    /// aggregation paths that need to include hidden children.
    #[test]
    fn children_of_returns_every_edge_from_parent() {
        let w = MicrogridSite::new();
        w.connect(2, 100);
        w.connect(2, 101);
        assert_eq!(w.children_of(2), vec![100, 101]);
        w.disconnect(2, 100);
        assert_eq!(w.children_of(2), vec![101]);
    }

    /// `parent_count` reflects how many edges in the connections
    /// graph terminate on a given child. Meter aggregation divides
    /// by this so a child shared by N parents contributes 1/N to
    /// each.
    #[test]
    fn parent_count_reports_edge_count() {
        let w = MicrogridSite::new();
        assert_eq!(w.parent_count(100), 0); // unconnected
        w.connect(2, 100);
        assert_eq!(w.parent_count(100), 1);
        w.connect(3, 100);
        assert_eq!(w.parent_count(100), 2);
        // unrelated child unaffected
        assert_eq!(w.parent_count(101), 0);
    }

    // `tick_once` used to invoke a pre-tick hook installed by
    // `Config::new` to refresh Lisp-driven component inputs. That
    // hook moved off the per-site tick to a dedicated Lisp-refresh
    // tokio task on `Config`, decoupling physics from the
    // interpreter lock. The ordering test the old shape relied on
    // no longer makes sense — physics is pure Rust now and the
    // refresh runs at its own cadence — so the test was deleted
    // along with the hook field.

    /// `bump_version` advances the counter and broadcasts a
    /// `TopologyChanged` event with the new version. Used by
    /// `Config::eval` after every eval so UI tabs refetch.
    #[tokio::test]
    async fn bump_version_broadcasts_event() {
        let w = MicrogridSite::new();
        let mut rx = w.subscribe_events();
        assert_eq!(w.version(), 0);
        let v = w.bump_version();
        assert_eq!(v, 1);
        assert_eq!(w.version(), 1);
        match rx.recv().await.unwrap() {
            crate::sim::events::SiteEvent::TopologyChanged { version } => {
                assert_eq!(version, 1);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// Components used as stubs in the mutation-method tests below.
    /// All they need to do is identify themselves; physics is irrelevant.
    struct Stub {
        id: u64,
        name: String,
    }
    impl Stub {
        fn new(id: u64) -> Self {
            Self {
                id,
                name: format!("stub-{id}"),
            }
        }
    }
    impl std::fmt::Display for Stub {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.name)
        }
    }
    impl SimulatedComponent for Stub {
        fn id(&self) -> u64 {
            self.id
        }
        fn category(&self) -> crate::sim::Category {
            crate::sim::Category::Meter
        }
        fn name(&self) -> &str {
            &self.name
        }
        fn stream_interval(&self) -> Duration {
            Duration::from_secs(1)
        }
        fn tick(&self, _: &MicrogridSite, _: DateTime<Utc>, _: Duration) {}
        fn telemetry(&self, _: &MicrogridSite) -> crate::sim::Telemetry {
            crate::sim::Telemetry::default()
        }
    }

    #[test]
    fn remove_component_drops_registry_and_edges() {
        let w = MicrogridSite::new();
        w.register(Stub::new(1));
        w.register(Stub::new(2));
        w.register(Stub::new(3));
        w.connect(1, 2);
        w.connect(2, 3);
        w.connect(1, 3);

        assert!(w.remove_component(2));
        assert!(w.get(2).is_none());
        assert!(w.get(1).is_some());
        let edges = w.connections();
        // Both edges touching id 2 went away; the 1→3 direct edge stays.
        assert_eq!(edges, vec![(1, 3)]);
        // Removing a missing id is a no-op that returns false.
        assert!(!w.remove_component(99));
    }

    #[test]
    fn disconnect_drops_one_edge_keeps_endpoints() {
        let w = MicrogridSite::new();
        w.register(Stub::new(1));
        w.register(Stub::new(2));
        w.connect(1, 2);
        w.connect(1, 2); // duplicate
        assert!(w.disconnect(1, 2));
        // First call drops both copies (retain semantics).
        assert!(w.connections().is_empty());
        assert!(w.get(1).is_some());
        assert!(w.get(2).is_some());
        // Second disconnect on the same edge returns false.
        assert!(!w.disconnect(1, 2));
    }

    #[test]
    fn rename_overrides_display_name_only() {
        let w = MicrogridSite::new();
        w.register(Stub::new(7));
        assert_eq!(w.display_name(7).as_deref(), Some("stub-7"));
        w.rename(7, "frontside-meter".into());
        assert_eq!(w.display_name(7).as_deref(), Some("frontside-meter"));
        // The component's intrinsic name() is untouched.
        assert_eq!(w.get(7).unwrap().name(), "stub-7");
    }

    /// `reset()` clears history alongside the rest of the MicrogridSite so a
    /// hot-reload starts charts fresh — old component-id histories
    /// don't linger as orphan entries.
    #[test]
    fn reset_clears_history() {
        let w = MicrogridSite::new();
        // Push directly via the public API by way of a minimal stub.
        w.inner.histories.write().insert(
            42,
            crate::sim::history::ComponentHistory::new(HISTORY_CAPACITY),
        );
        w.reset();
        assert!(w.inner.histories.read().is_empty());
    }

    /// A per-site `reset()` must not rewind the enterprise-wide id
    /// allocator shared across sites — otherwise resetting one microgrid
    /// hands out ids that collide with components still live on another.
    #[test]
    fn reset_does_not_rewind_a_shared_id_allocator() {
        let alloc = Arc::new(AtomicU64::new(FIRST_AUTO_ID));
        let site_a = MicrogridSite::with_id_allocator(alloc.clone());
        let site_b = MicrogridSite::with_id_allocator(alloc.clone());
        // Site A advances the shared counter.
        assert_eq!(site_a.next_id(), FIRST_AUTO_ID);
        assert_eq!(site_a.next_id(), FIRST_AUTO_ID + 1);
        // Resetting B must leave the shared counter where A left it.
        site_b.reset();
        assert_eq!(
            site_a.next_id(),
            FIRST_AUTO_ID + 2,
            "a per-site reset rewound the shared id allocator"
        );
    }

    /// Beyond histories, `reset()` also flushes the scenario journal,
    /// the main-meter flag, and any open CSV sinks. Leaving these
    /// across a hot-reload leaks stale integrals against ids that
    /// have since been re-registered and blocks a reload from
    /// claiming a different meter as `:main`.
    #[test]
    fn reset_clears_scenario_and_main_meter() {
        use crate::sim::setpoints::{SetpointEvent, SetpointKind, SetpointOutcome};
        let w = MicrogridSite::new();
        w.register(Stub::new(1));
        w.set_main_meter(1).unwrap();
        w.log_setpoint(
            1,
            SetpointEvent {
                ts: Utc::now(),
                kind: SetpointKind::ActivePower,
                value: 1234.0,
                ttl_s: Some(60),
                outcome: SetpointOutcome::Accepted {
                    effective_value: Some(1234.0),
                },
            },
        );
        w.scenario_start("smoke".into(), Utc::now());
        w.scenario_record("k".into(), "v".into(), Utc::now());

        w.reset();

        assert!(
            w.inner.setpoint_logs.read().is_empty(),
            "setpoint_logs must clear",
        );
        assert!(
            w.inner.scenario.read().started_at.is_none(),
            "scenario journal must reset",
        );
        assert_eq!(w.inner.scenario.read().event_count(), 0);
        assert!(
            w.inner.main_meter_id.read().is_none(),
            "main_meter_id must clear so reload can pick a different meter",
        );
    }
}
