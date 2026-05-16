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
use crate::sim::history::{ComponentHistory, History, Metric, Sample};
use crate::sim::runtime::{CommandMode, ComponentRuntime, Health, TelemetryMode};
use crate::sim::scenario::{ScenarioEvent, ScenarioJournal};
use crate::sim::scenario_csv::{CsvSink, CsvSinks};
use crate::sim::setpoints::{SetpointEvent, SetpointKind, SetpointLog, SetpointOutcome};
use crate::timeout_tracker::TimeoutTracker;

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

/// Stable lowercase tokens for the setpoint-event broadcast — same
/// strings the JSON tag wire-format uses, so the UI doesn't need a
/// translation table.
fn setpoint_kind_label(k: SetpointKind) -> &'static str {
    match k {
        SetpointKind::ActivePower => "active_power",
        SetpointKind::ReactivePower => "reactive_power",
        SetpointKind::AugmentBounds => "augment_bounds",
    }
}

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
    /// Per-MicrogridSite id allocator. Reset on `reset()` so a hot-reload
    /// reuses the same id range microsim would (1000+) — clients
    /// caching component IDs across reloads see them stay stable.
    next_id: AtomicU64,
    /// Per-component runtime mode flags (health, telemetry mode,
    /// command mode). Defaulted on register, mutated via the
    /// `set-component-*` Lisp defuns or directly from server.rs.
    runtime: RwLock<HashMap<u64, ComponentRuntime>>,
    /// User-facing name overrides set via `(world-rename-component …)`.
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
    /// Optional callback invoked at the start of every `tick_once`,
    /// before any component's `tick` runs. `Config::new` installs a
    /// closure that locks the interpreter and calls
    /// `SimulatedComponent::refresh_inputs` on every registered
    /// component, so lambda-bound `:power` / `:sunlight%` / … values
    /// resolve once per tick. MicrogridSite stays interpreter-agnostic at
    /// the type level.
    pre_tick: RwLock<Option<PreTickHook>>,
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
}

/// Callback invoked at the start of every `tick_once`. Held behind an
/// `Arc<dyn Fn>` so MicrogridSite's API doesn't depend on tulisp.
pub(crate) type PreTickHook = Arc<dyn Fn(&MicrogridSite) + Send + Sync + 'static>;

/// Compute mean / median / integer-bucketed mode over a battery
/// SoC sample set. Returns `None` for an empty input.
fn compute_soc_stats(socs: &[f32]) -> Option<SocStats> {
    if socs.is_empty() {
        return None;
    }
    let mean_pct = socs.iter().map(|v| *v as f64).sum::<f64>() / socs.len() as f64;
    let mut sorted: Vec<f32> = socs.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median_pct = sorted[sorted.len() / 2 - usize::from(sorted.len().is_multiple_of(2))] as f64;
    // Mode: integer-bucketed, lowest-bucket on tie.
    let mut histogram = [0u32; 101];
    for v in socs {
        let bucket = v.clamp(0.0, 100.0).round() as usize;
        histogram[bucket] += 1;
    }
    // Pick the lowest bucket on a count tie. `max_by_key` keeps
    // the LAST max seen; iterate ascending and update only on
    // strictly greater so the lowest bucket wins.
    let mut mode_pct: u8 = 0;
    let mut best_count: u32 = 0;
    for (idx, count) in histogram.iter().enumerate() {
        if *count > best_count {
            best_count = *count;
            mode_pct = idx as u8;
        }
    }
    Some(SocStats {
        mean_pct,
        median_pct,
        mode_pct: Some(mode_pct),
    })
}

/// Snapshot of `ScenarioJournal` lifecycle state for `/api/scenario`.
/// Excludes the events themselves — those live behind a paginated
/// `/api/scenario/events` endpoint with a `since=` cursor.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ScenarioSummary {
    pub name: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub elapsed_s: f64,
    pub event_count: usize,
    /// One past the highest event id ever recorded. Stable cursor
    /// for `/api/scenario/events?since=N` — clients pass this back
    /// unchanged to mean "anything newer than what I last saw".
    pub next_event_id: u64,
    /// Lowest event id still retained in the ring. Clients compare
    /// their `since` cursor against this: if `since < earliest_event_id`
    /// they're polling into a window that has already been evicted,
    /// so some events were missed.
    pub earliest_event_id: u64,
}

/// Snapshot of scenario-scoped metrics for `/api/scenario/report`.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ScenarioReport {
    pub scenario_elapsed_s: f64,
    pub peak_main_meter_w: f64,
    pub main_meter_id: Option<u64>,
    pub total_battery_charged_wh: f64,
    pub total_battery_discharged_wh: f64,
    pub total_pv_produced_wh: f64,
    pub per_battery: Vec<PerBatteryReport>,
    pub per_pv: Vec<PerPvReport>,
    /// Stats over the *current* SoC of every registered battery.
    /// Computed lazily on each report fetch — cheap O(N) over a
    /// handful of batteries. None when no batteries are registered.
    pub soc_stats: Option<SocStats>,
    /// Per-15-minute UTC-aligned window average of main-meter
    /// active power. Sorted oldest-first.
    pub main_meter_window_averages: Vec<WindowAverageEntry>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct PerBatteryReport {
    pub id: u64,
    pub charge_wh: f64,
    pub discharge_wh: f64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct PerPvReport {
    pub id: u64,
    pub produced_wh: f64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct SocStats {
    /// Arithmetic mean of every battery's current SoC.
    pub mean_pct: f64,
    /// Median (lower of the two middle values for an even count).
    pub median_pct: f64,
    /// Mode bucketed to integer percent. If multiple buckets tie,
    /// returns the lowest. None for an empty set.
    pub mode_pct: Option<u8>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct WindowAverageEntry {
    pub window_start: DateTime<Utc>,
    pub avg_w: f64,
}

impl MicrogridSite {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(MicrogridSiteInner {
                components: RwLock::new(Vec::new()),
                by_id: RwLock::new(HashMap::new()),
                connections: RwLock::new(Vec::new()),
                grid_state: RwLock::new(GridState::default()),
                physics_tick_ms: AtomicU64::new(100),
                next_id: AtomicU64::new(FIRST_AUTO_ID),
                runtime: RwLock::new(HashMap::new()),
                name_overrides: RwLock::new(HashMap::new()),
                histories: RwLock::new(HashMap::new()),
                setpoint_logs: RwLock::new(HashMap::new()),
                version: AtomicU64::new(0),
                events: broadcast::channel(EVENT_BUS_CAPACITY).0,
                timeout_tracker: TimeoutTracker::new(),
                pre_tick: RwLock::new(None),
                scenario: RwLock::new(ScenarioJournal::default()),
                main_meter_id: RwLock::new(None),
                scenario_csv: RwLock::new(CsvSinks::new()),
            }),
        }
    }

    // ─── Scenario journal + CSV sinks ─────────────────────────────────
    //
    // Lifecycle: `scenario_start` opens a window, `scenario_record`
    // drops markers, `scenario_stop` closes it. Per-component CSV
    // sinks live alongside; opened via `scenario_open_csv` and read
    // by the physics tick. Main-meter id is the only piece of
    // scenario state outside the journal — `scenario_report` keys
    // its peak-tracking on it.

    /// Open one CSV sink per registered component under `dir`.
    /// Returns the count opened. Existing sinks are dropped first
    /// so a re-call replaces (rather than appends to) the prior
    /// recording.
    pub(crate) fn scenario_open_csv(&self, dir: &std::path::Path) -> std::io::Result<usize> {
        std::fs::create_dir_all(dir)?;
        let components = self.inner.components.read().clone();
        let mut sinks = CsvSinks::new();
        for c in &components {
            let sink = CsvSink::open(dir, c.id(), c.category())?;
            sinks.insert(c.id(), sink);
        }
        let count = sinks.len();
        *self.inner.scenario_csv.write() = sinks;
        Ok(count)
    }

    /// Drop every active CSV sink. Each underlying `BufWriter`
    /// flushes on drop.
    pub(crate) fn scenario_close_csv(&self) -> usize {
        let mut g = self.inner.scenario_csv.write();
        let count = g.len();
        g.clear();
        count
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

    /// Install the pre-tick hook. `Config::new` is the sole caller;
    /// later overwrites replace the previous closure.
    pub(crate) fn set_pre_tick(&self, hook: PreTickHook) {
        *self.inner.pre_tick.write() = Some(hook);
    }

    // ── scenario journal ────────────────────────────────────────────

    /// Begin a fresh scenario at `now`. Empties the event ring,
    /// clears the stop marker, sets the name. Used by
    /// `(scenario-start)`.
    pub(crate) fn scenario_start(&self, name: String, now: DateTime<Utc>) {
        self.inner.scenario.write().start(name, now);
    }

    /// Mark the scenario as ended at `now`. Also closes any active
    /// CSV sinks so the file flushes before a downstream loader
    /// might pick it up. Idempotent.
    pub(crate) fn scenario_stop(&self, now: DateTime<Utc>) {
        self.inner.scenario.write().stop(now);
        self.scenario_close_csv();
    }

    /// Append a journal event. Returns the assigned id.
    pub(crate) fn scenario_record(&self, kind: String, payload: String, now: DateTime<Utc>) -> u64 {
        self.inner.scenario.write().record(kind, payload, now)
    }

    /// Wall-clock seconds since the scenario started. 0 if not
    /// running. Freezes once stopped.
    pub(crate) fn scenario_elapsed_s(&self, now: DateTime<Utc>) -> f64 {
        self.inner.scenario.read().elapsed_s(now)
    }

    /// Snapshot of scenario lifecycle for `/api/scenario`.
    pub(crate) fn scenario_summary(&self, now: DateTime<Utc>) -> ScenarioSummary {
        let g = self.inner.scenario.read();
        ScenarioSummary {
            name: g.name.clone(),
            started_at: g.started_at,
            ended_at: g.ended_at,
            elapsed_s: g.elapsed_s(now),
            event_count: g.event_count(),
            next_event_id: g.next_event_id(),
            earliest_event_id: g.earliest_event_id(),
        }
    }

    /// Pull events with id > `since`, capped at `limit`. Used by
    /// `/api/scenario/events`.
    pub(crate) fn scenario_events_since(&self, since: u64, limit: usize) -> Vec<ScenarioEvent> {
        self.inner.scenario.read().events_since(since, limit)
    }

    /// Aggregate metrics for `/api/scenario/report`. Returns a
    /// snapshot. SoC stats are computed at fetch time from each
    /// battery's current telemetry — cheap, no accumulator needed.
    pub(crate) fn scenario_report(&self, now: DateTime<Utc>) -> ScenarioReport {
        use crate::sim::Category;
        let g = self.inner.scenario.read();
        let mut total_charged = 0.0;
        let mut total_discharged = 0.0;
        let per_battery: Vec<PerBatteryReport> = g
            .per_battery()
            .iter()
            .map(|(id, b)| {
                total_charged += b.charge_wh;
                total_discharged += b.discharge_wh;
                PerBatteryReport {
                    id: *id,
                    charge_wh: b.charge_wh,
                    discharge_wh: b.discharge_wh,
                }
            })
            .collect();
        let mut total_pv = 0.0;
        let per_pv: Vec<PerPvReport> = g
            .per_pv()
            .iter()
            .map(|(id, p)| {
                total_pv += p.produced_wh;
                PerPvReport {
                    id: *id,
                    produced_wh: p.produced_wh,
                }
            })
            .collect();
        let main_meter_window_averages: Vec<WindowAverageEntry> = g
            .window_avgs()
            .iter()
            .map(|(secs, (sum, count))| WindowAverageEntry {
                window_start: DateTime::<Utc>::from_timestamp(*secs, 0).unwrap_or_else(Utc::now),
                avg_w: if *count > 0 {
                    *sum / (*count as f64)
                } else {
                    0.0
                },
            })
            .collect();
        drop(g);

        // SoC stats: walk every registered battery, read its
        // current SoC. Out-of-band of the journal because it's
        // current state, not an accumulator.
        let mut socs: Vec<f32> = Vec::new();
        for c in self.inner.components.read().iter() {
            if c.category() == Category::Battery
                && let Some(s) = c.telemetry(self).soc_pct
            {
                socs.push(s);
            }
        }
        let soc_stats = compute_soc_stats(&socs);

        ScenarioReport {
            scenario_elapsed_s: self.inner.scenario.read().elapsed_s(now),
            peak_main_meter_w: self.inner.scenario.read().peak_main_meter_active_w(),
            main_meter_id: *self.inner.main_meter_id.read(),
            total_battery_charged_wh: total_charged,
            total_battery_discharged_wh: total_discharged,
            total_pv_produced_wh: total_pv,
            per_battery,
            per_pv,
            soc_stats,
            main_meter_window_averages,
        }
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

    /// Broadcast a `ConfigError` on the world event bus. Used by the
    /// watcher's reload-failure path so UI subscribers can render a
    /// "config invalid" banner instead of seeing the post-reset
    /// empty world without explanation. Fire-and-forget — a send
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
        self.inner.grid_state.read().clone()
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
    /// `world-connect` and `world-disconnect` flow through the same
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
        self.inner.next_id.store(FIRST_AUTO_ID, Ordering::Relaxed);
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
        was_present
    }

    /// Drop every `(parent, child)` edge from the graph. Returns
    /// true if at least one edge was removed. Doesn't touch either
    /// endpoint's registration.
    ///
    /// Duplicates collapse — if `(world-connect …)` was called
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
        self.inner.runtime.write().entry(id).or_default().health = health;
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
    /// If a pre-tick hook is installed it runs first — this is where
    /// `Config::new` resolves Lisp-driven inputs (lambda `:power`,
    /// symbol `:sunlight%`, …) into atomic scalars that the tick
    /// pass then reads without re-entering the interpreter.
    pub fn tick_once(&self, now: DateTime<Utc>, dt: Duration) {
        if let Some(hook) = self.inner.pre_tick.read().clone() {
            hook(self);
        }
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

    // ─── Telemetry history (charts) ──────────────────────────────────
    //
    // `spawn_history_sampler` ticks at 1 Hz and pushes per-metric
    // snapshots into ring buffers. UI's /api/history reads windows
    // via `history_window`; `history_metrics` lists what's in each
    // component's ring.

    /// Spawn the history sampler — a single task that walks every
    /// component once per second and pushes a snapshot into each
    /// component's per-metric history rings.
    ///
    /// Single-task / fixed-cadence on purpose: a per-component task
    /// at each component's own `stream_interval` would be more
    /// faithful to gRPC stream semantics, but adds task lifecycle
    /// management (cancel-on-reload, re-spawn-per-component) for
    /// little chart-side benefit. 1 Hz × 600-sample capacity = 10
    /// minutes of history per series, plenty for the v1 charts.
    pub fn spawn_history_sampler(self) {
        let cadence = Duration::from_secs(1);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(cadence);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                self.record_history_snapshot(Utc::now());
            }
        });
    }

    /// Take one snapshot pass: read every component's telemetry and
    /// push to its history rings. Extracted so tests can drive sampling
    /// deterministically without spawning the periodic task.
    ///
    /// Each pushed metric also fans out as a `SiteEvent::Sample` on
    /// the broadcast bus, after the histories lock is released — so
    /// WS subscribers see live samples but can't deadlock against
    /// each other or against /api/history readers.
    pub fn record_history_snapshot(&self, now: DateTime<Utc>) {
        use crate::sim::Category;

        let components = self.inner.components.read().clone();
        let mut emitted: Vec<(u64, Metric, f32)> = Vec::new();
        // Integrals fed to the scenario reporter. We capture them
        // off the telemetry snapshot rather than refilling from the
        // metric stream so batteries (which expose only dc_power_w,
        // not active_power_w) get integrated too.
        let mut battery_samples: Vec<(u64, f32)> = Vec::new();
        let mut pv_samples: Vec<(u64, f32)> = Vec::new();
        {
            let mut histories = self.inner.histories.write();
            let mut csv_sinks = self.inner.scenario_csv.write();
            for c in &components {
                let snap = c.telemetry(self);
                match c.category() {
                    Category::Battery => {
                        if let Some(p) = snap.dc_power_w {
                            battery_samples.push((c.id(), p));
                        }
                    }
                    Category::Inverter if c.subtype() == Some("solar") => {
                        if let Some(p) = snap.active_power_w {
                            pv_samples.push((c.id(), p));
                        }
                    }
                    _ => {}
                }
                if let Some(sink) = csv_sinks.get_mut(&c.id())
                    && let Err(e) = sink.write_row(now, &snap)
                {
                    log::warn!("CSV write failed for {}: {e}", c.id());
                }
                let entry = histories
                    .entry(c.id())
                    .or_insert_with(|| ComponentHistory::new(HISTORY_CAPACITY));
                for (m, v) in entry.push_snapshot(now, &snap) {
                    emitted.push((c.id(), m, v));
                }
            }
        }
        // Hand each new sample to the scenario reporter so the
        // metrics endpoint stays current. Only meaningful while a
        // scenario is running; the journal short-circuits for
        // unflagged ids and unwatched metrics. Integrals advance
        // the cursor at the end so the next snapshot's dt is
        // measured from now.
        let main_id = *self.inner.main_meter_id.read();
        {
            let mut journal = self.inner.scenario.write();
            for (id, metric, value) in &emitted {
                journal.record_sample(*id, *metric, *value, main_id, now);
            }
            for (id, dc_power_w) in &battery_samples {
                journal.record_battery_sample(*id, *dc_power_w, now);
            }
            for (id, active_power_w) in &pv_samples {
                journal.record_pv_sample(*id, *active_power_w, now);
            }
            journal.advance_sample_cursor(now);
        }
        let ts_ms = now.timestamp_millis();
        for (id, metric, value) in emitted {
            let _ = self.inner.events.send(SiteEvent::Sample {
                id,
                metric: metric.as_str(),
                ts_ms,
                value,
            });
        }
    }

    /// Read a windowed slice of one component's history for one
    /// metric. Returns owned samples so the caller can release the
    /// read lock immediately. `None` if the component or metric has
    /// no recorded history yet.
    pub fn history_window(
        &self,
        id: u64,
        metric: Metric,
        since: DateTime<Utc>,
    ) -> Option<Vec<Sample>> {
        let h = self.inner.histories.read();
        let c = h.get(&id)?;
        let series: &History = c.get(metric)?;
        Some(series.iter_window(since).copied().collect())
    }

    /// List the metrics for which `id` has any recorded history.
    pub fn history_metrics(&self, id: u64) -> Vec<Metric> {
        self.inner
            .histories
            .read()
            .get(&id)
            .map(|c| c.metrics().collect())
            .unwrap_or_default()
    }

    // ─── Setpoint event log ──────────────────────────────────────────
    //
    // Per-component rolling log of accepted / rejected setpoint
    // requests. Populated by the gRPC handlers; read by the UI's
    // /api/setpoints inspector. Each `log_setpoint` also broadcasts
    // on the event bus for live UI updates.

    /// Append a setpoint event to the per-component log + broadcast
    /// it on the world event bus so live UI inspectors update without
    /// a refetch. Auto-creates the ring on first push; bounded to
    /// `SETPOINT_LOG_CAPACITY` entries (oldest evict).
    pub fn log_setpoint(&self, id: u64, event: SetpointEvent) {
        let ts_ms = event.ts.timestamp_millis();
        let kind = setpoint_kind_label(event.kind);
        let value = event.value;
        let (accepted, reason) = match &event.outcome {
            SetpointOutcome::Accepted { .. } => (true, None),
            SetpointOutcome::Rejected { reason } => (false, Some(reason.clone())),
        };
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
    fn soc_stats_compute_on_typical_set() {
        let s = compute_soc_stats(&[20.0, 40.0, 60.0, 80.0]).unwrap();
        // Mean of 20, 40, 60, 80 = 50.
        assert!((s.mean_pct - 50.0).abs() < 1e-6);
        // Median (lower of middle two on even count) = 40.
        assert!((s.median_pct - 40.0).abs() < 1e-6);
        // No clear mode — all equal counts at distinct buckets;
        // returns the lowest tied bucket (20).
        assert_eq!(s.mode_pct, Some(20));
    }

    #[test]
    fn soc_stats_mode_picks_repeated_bucket() {
        let s = compute_soc_stats(&[50.0, 50.4, 50.6, 25.0, 80.0]).unwrap();
        // Three SoCs round to 50 (50, 50, 51 — actually 50.6
        // rounds to 51, so mode is 50 with 2 buckets, vs 51, 25,
        // 80 each at 1).
        assert_eq!(s.mode_pct, Some(50));
    }

    #[test]
    fn soc_stats_empty_returns_none() {
        assert!(compute_soc_stats(&[]).is_none());
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

    /// `tick_once` runs the pre-tick hook to completion before any
    /// component's `tick` fires. Components rely on this ordering so
    /// a meter can read its lambda-resolved `:power` in `tick`
    /// without re-entering the interpreter.
    #[test]
    fn pre_tick_hook_runs_before_component_tick() {
        use crate::sim::Telemetry;
        use chrono::TimeZone;
        use parking_lot::Mutex;

        struct OrderRecorder {
            order: Arc<Mutex<Vec<&'static str>>>,
        }
        impl std::fmt::Display for OrderRecorder {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "order-recorder")
            }
        }
        impl SimulatedComponent for OrderRecorder {
            fn id(&self) -> u64 {
                1
            }
            fn category(&self) -> crate::sim::Category {
                crate::sim::Category::Meter
            }
            fn name(&self) -> &str {
                "order"
            }
            fn stream_interval(&self) -> Duration {
                Duration::from_secs(1)
            }
            fn tick(&self, _: &MicrogridSite, _: DateTime<Utc>, _: Duration) {
                self.order.lock().push("tick");
            }
            fn telemetry(&self, _: &MicrogridSite) -> Telemetry {
                Telemetry::default()
            }
        }

        let w = MicrogridSite::new();
        let order: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let order_for_hook = order.clone();
        w.set_pre_tick(Arc::new(move |_| {
            order_for_hook.lock().push("pre_tick");
        }));
        w.register(OrderRecorder {
            order: order.clone(),
        });
        let now = chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        w.tick_once(now, Duration::from_millis(100));
        assert_eq!(*order.lock(), vec!["pre_tick", "tick"]);
    }

    /// Driving `record_history_snapshot` directly populates the
    /// per-component ring buffers. Verified across multiple ticks via
    /// the windowed reader and the metric-set introspection.
    #[test]
    fn history_snapshot_populates_rings() {
        use crate::sim::Telemetry;
        use chrono::TimeZone;
        struct FixedFlow {
            id: u64,
        }
        impl std::fmt::Display for FixedFlow {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "fixed-{}", self.id)
            }
        }
        impl SimulatedComponent for FixedFlow {
            fn id(&self) -> u64 {
                self.id
            }
            fn category(&self) -> crate::sim::Category {
                crate::sim::Category::Battery
            }
            fn name(&self) -> &str {
                "fixed"
            }
            fn stream_interval(&self) -> Duration {
                Duration::from_secs(1)
            }
            fn tick(&self, _: &MicrogridSite, _: DateTime<Utc>, _: Duration) {}
            fn telemetry(&self, _: &MicrogridSite) -> Telemetry {
                Telemetry {
                    active_power_w: Some(2500.0),
                    soc_pct: Some(72.5),
                    ..Default::default()
                }
            }
        }

        let w = MicrogridSite::new();
        w.register(FixedFlow { id: 7 });
        let t0 = Utc.timestamp_opt(1_000, 0).unwrap();
        let t1 = Utc.timestamp_opt(1_001, 0).unwrap();
        w.record_history_snapshot(t0);
        w.record_history_snapshot(t1);

        let metrics: std::collections::HashSet<_> = w.history_metrics(7).into_iter().collect();
        assert!(metrics.contains(&Metric::ActivePowerW));
        assert!(metrics.contains(&Metric::SocPct));

        let p = w
            .history_window(7, Metric::ActivePowerW, Utc.timestamp_opt(0, 0).unwrap())
            .unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].value, 2500.0);

        // Windowed read drops samples before `since`.
        let recent = w.history_window(7, Metric::ActivePowerW, t1).unwrap();
        assert_eq!(recent.len(), 1);
    }

    /// `record_history_snapshot` fans out a Sample event per pushed
    /// metric. We push two metrics (P + SoC) on the same tick, so
    /// expect two events at the same timestamp.
    #[tokio::test]
    async fn record_history_snapshot_emits_sample_events() {
        use crate::sim::Telemetry;
        use crate::sim::events::SiteEvent;
        use chrono::TimeZone;
        struct PVStub;
        impl std::fmt::Display for PVStub {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "stub")
            }
        }
        impl SimulatedComponent for PVStub {
            fn id(&self) -> u64 {
                7
            }
            fn category(&self) -> crate::sim::Category {
                crate::sim::Category::Inverter
            }
            fn name(&self) -> &str {
                "stub"
            }
            fn stream_interval(&self) -> Duration {
                Duration::from_secs(1)
            }
            fn tick(&self, _: &MicrogridSite, _: DateTime<Utc>, _: Duration) {}
            fn telemetry(&self, _: &MicrogridSite) -> Telemetry {
                Telemetry {
                    active_power_w: Some(-12345.0),
                    soc_pct: Some(60.0),
                    ..Default::default()
                }
            }
        }
        let w = MicrogridSite::new();
        w.register(PVStub);
        let mut rx = w.subscribe_events();
        let now = Utc.timestamp_opt(1_000, 0).unwrap();
        w.record_history_snapshot(now);

        // Drain the receiver until we've seen one event per emitted
        // metric. There's no inter-event ordering guarantee so we
        // collect into a set keyed by metric.
        let mut seen: std::collections::HashSet<&'static str> = std::collections::HashSet::new();
        for _ in 0..2 {
            match rx.recv().await.unwrap() {
                SiteEvent::Sample {
                    id,
                    metric,
                    ts_ms,
                    value: _,
                } => {
                    assert_eq!(id, 7);
                    assert_eq!(ts_ms, 1_000_000);
                    seen.insert(metric);
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert!(seen.contains("active_power_w"));
        assert!(seen.contains("soc_pct"));
    }

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
