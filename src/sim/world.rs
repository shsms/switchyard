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

use crate::sim::component::{ComponentHandle, FIRST_AUTO_ID, SimulatedComponent};
use crate::sim::runtime::{CommandMode, ComponentRuntime, Health, TelemetryMode};

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
            }),
        }
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
        self.inner.next_id.store(FIRST_AUTO_ID, Ordering::Relaxed);
        // The grid state is environmental (set by the config's `every`
        // timer); we deliberately keep it across reloads so the first
        // tick after reload still has plausible values.
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
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}
