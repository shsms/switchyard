//! The simulation registry, scheduler, and shared environment.
//!
//! `World` owns every component, the parent → child topology, and the
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
use crate::sim::events::{EVENT_BUS_CAPACITY, WorldEvent};
use crate::sim::history::{ComponentHistory, History, Metric, Sample};
use crate::sim::runtime::{CommandMode, ComponentRuntime, Health, TelemetryMode};
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
pub struct World {
    inner: Arc<WorldInner>,
}

struct WorldInner {
    components: RwLock<Vec<Arc<dyn SimulatedComponent>>>,
    by_id: RwLock<HashMap<u64, Arc<dyn SimulatedComponent>>>,
    connections: RwLock<Vec<(u64, u64)>>,
    grid_state: RwLock<GridState>,
    physics_tick_ms: AtomicU64,
    /// Per-World id allocator. Reset on `reset()` so a hot-reload
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
    events: broadcast::Sender<WorldEvent>,
    /// Per-component setpoint expiry deadlines. Both the gRPC
    /// `SetElectricalComponentPower` handler and the `(set-power …)`
    /// Lisp defun add to this; a single tokio task in
    /// `Config::start_timeout_loop` polls for expirations and calls
    /// `reset_setpoint` on each. Living on World means the loop runs
    /// once per process regardless of which call sites schedule.
    timeout_tracker: TimeoutTracker,
}

impl World {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(WorldInner {
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
            }),
        }
    }

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

    pub fn version(&self) -> u64 {
        self.inner.version.load(Ordering::Relaxed)
    }

    /// Increment the version counter and broadcast a
    /// `TopologyChanged` event. Returns the new version. Send errors
    /// (no live subscribers) are swallowed — the event is fire-and-
    /// forget by design.
    pub fn bump_version(&self) -> u64 {
        let v = self.inner.version.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = self.inner.events.send(WorldEvent::TopologyChanged { version: v });
        v
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<WorldEvent> {
        self.inner.events.subscribe()
    }

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

    pub fn connections(&self) -> Vec<(u64, u64)> {
        self.inner.connections.read().clone()
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
    pub fn reset(&self) {
        self.inner.components.write().clear();
        self.inner.by_id.write().clear();
        self.inner.connections.write().clear();
        self.inner.runtime.write().clear();
        self.inner.name_overrides.write().clear();
        self.inner.histories.write().clear();
        self.inner.setpoint_logs.write().clear();
        self.inner.next_id.store(FIRST_AUTO_ID, Ordering::Relaxed);
        // The grid state is environmental (set by the config's `every`
        // timer); we deliberately keep it across reloads so the first
        // tick after reload still has plausible values.
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

    /// Drop a single parent → child edge. Returns true if the edge
    /// existed. Doesn't touch either endpoint's registration.
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

    /// Tick every registered component once. Children are stored before
    /// parents, so a single forward pass updates leaves before the
    /// meters that aggregate them.
    pub fn tick_once(&self, now: DateTime<Utc>, dt: Duration) {
        let components = self.inner.components.read().clone();
        for c in components {
            c.tick(self, now, dt);
        }
    }

    /// Spawn the physics loop. Returns immediately. The loop holds an
    /// `Arc` clone of the World, so the World cannot drop until the
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
    /// Each pushed metric also fans out as a `WorldEvent::Sample` on
    /// the broadcast bus, after the histories lock is released — so
    /// WS subscribers see live samples but can't deadlock against
    /// each other or against /api/history readers.
    pub fn record_history_snapshot(&self, now: DateTime<Utc>) {
        let components = self.inner.components.read().clone();
        let mut emitted: Vec<(u64, Metric, f32)> = Vec::new();
        {
            let mut histories = self.inner.histories.write();
            for c in &components {
                let snap = c.telemetry(self);
                let entry = histories
                    .entry(c.id())
                    .or_insert_with(|| ComponentHistory::new(HISTORY_CAPACITY));
                for (m, v) in entry.push_snapshot(now, &snap) {
                    emitted.push((c.id(), m, v));
                }
            }
        }
        let ts_ms = now.timestamp_millis();
        for (id, metric, value) in emitted {
            let _ = self.inner.events.send(WorldEvent::Sample {
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
        let _ = self.inner.events.send(WorldEvent::Setpoint {
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
    pub fn setpoints_window(
        &self,
        id: u64,
        since: DateTime<Utc>,
    ) -> Vec<SetpointEvent> {
        self.inner
            .setpoint_logs
            .read()
            .get(&id)
            .map(|log| log.iter_window(since).cloned().collect())
            .unwrap_or_default()
    }
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two meters can list the same inverter as a successor and both
    /// edges land in the connections graph (a parallel-meter
    /// setup). `aggregate_child_bounds` from either parent finds its
    /// own children independently — no double-counting at the bounds
    /// layer.
    #[test]
    fn shared_child_under_two_parents() {
        let w = World::new();
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

    /// `parent_count` reflects how many edges in the connections
    /// graph terminate on a given child. Meter aggregation divides
    /// by this so a child shared by N parents contributes 1/N to
    /// each.
    #[test]
    fn parent_count_reports_edge_count() {
        let w = World::new();
        assert_eq!(w.parent_count(100), 0); // unconnected
        w.connect(2, 100);
        assert_eq!(w.parent_count(100), 1);
        w.connect(3, 100);
        assert_eq!(w.parent_count(100), 2);
        // unrelated child unaffected
        assert_eq!(w.parent_count(101), 0);
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
            fn tick(&self, _: &World, _: DateTime<Utc>, _: Duration) {}
            fn telemetry(&self, _: &World) -> Telemetry {
                Telemetry {
                    active_power_w: Some(2500.0),
                    soc_pct: Some(72.5),
                    ..Default::default()
                }
            }
        }

        let w = World::new();
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
        use crate::sim::events::WorldEvent;
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
            fn tick(&self, _: &World, _: DateTime<Utc>, _: Duration) {}
            fn telemetry(&self, _: &World) -> Telemetry {
                Telemetry {
                    active_power_w: Some(-12345.0),
                    soc_pct: Some(60.0),
                    ..Default::default()
                }
            }
        }
        let w = World::new();
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
                WorldEvent::Sample {
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
        let w = World::new();
        let mut rx = w.subscribe_events();
        assert_eq!(w.version(), 0);
        let v = w.bump_version();
        assert_eq!(v, 1);
        assert_eq!(w.version(), 1);
        match rx.recv().await.unwrap() {
            crate::sim::events::WorldEvent::TopologyChanged { version } => {
                assert_eq!(version, 1);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// Components used as stubs in the mutation-method tests below.
    /// All they need to do is identify themselves; physics is irrelevant.
    struct Stub(u64);
    impl std::fmt::Display for Stub {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "stub-{}", self.0)
        }
    }
    impl SimulatedComponent for Stub {
        fn id(&self) -> u64 {
            self.0
        }
        fn category(&self) -> crate::sim::Category {
            crate::sim::Category::Meter
        }
        fn name(&self) -> &str {
            // 'static lifetime via leak — fine for tests
            Box::leak(format!("stub-{}", self.0).into_boxed_str())
        }
        fn stream_interval(&self) -> Duration {
            Duration::from_secs(1)
        }
        fn tick(&self, _: &World, _: DateTime<Utc>, _: Duration) {}
        fn telemetry(&self, _: &World) -> crate::sim::Telemetry {
            crate::sim::Telemetry::default()
        }
    }

    #[test]
    fn remove_component_drops_registry_and_edges() {
        let w = World::new();
        w.register(Stub(1));
        w.register(Stub(2));
        w.register(Stub(3));
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
        let w = World::new();
        w.register(Stub(1));
        w.register(Stub(2));
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
        let w = World::new();
        w.register(Stub(7));
        assert_eq!(w.display_name(7).as_deref(), Some("stub-7"));
        w.rename(7, "frontside-meter".into());
        assert_eq!(w.display_name(7).as_deref(), Some("frontside-meter"));
        // The component's intrinsic name() is untouched.
        assert_eq!(w.get(7).unwrap().name(), "stub-7");
    }

    /// `reset()` clears history alongside the rest of the World so a
    /// hot-reload starts charts fresh — old component-id histories
    /// don't linger as orphan entries.
    #[test]
    fn reset_clears_history() {
        let w = World::new();
        // Push directly via the public API by way of a minimal stub.
        w.inner.histories.write().insert(
            42,
            crate::sim::history::ComponentHistory::new(HISTORY_CAPACITY),
        );
        w.reset();
        assert!(w.inner.histories.read().is_empty());
    }
}
