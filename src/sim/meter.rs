use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use crate::sim::{Category, SimulatedComponent, Telemetry, World};

/// A power meter sums its successors' active and reactive power, then
/// voltage-splits the totals across the three phases. If the parent
/// registered with explicit `:power` — or a Lisp timer pushed a value
/// in via `(set-meter-power id W)` — that value is used verbatim
/// instead, modelling a headless consumer / CHP load.
pub struct Meter {
    id: u64,
    name: String,
    interval: Duration,
    successors: Vec<u64>,
    /// Override the aggregate-from-successors path with an explicit
    /// active-power value. Mutex-wrapped so a runtime defun can flip
    /// it without contending against the per-tick aggregation read.
    fixed_power_w: Mutex<Option<f32>>,
    stream_jitter_pct: f32,
}

impl Meter {
    pub fn new(
        id: u64,
        interval: Duration,
        successors: Vec<u64>,
        fixed_power_w: Option<f32>,
        stream_jitter_pct: f32,
    ) -> Self {
        Self {
            id,
            name: format!("meter-{id}"),
            interval,
            successors,
            fixed_power_w: Mutex::new(fixed_power_w),
            stream_jitter_pct,
        }
    }

    fn aggregate_active(&self, world: &World) -> f32 {
        if let Some(p) = *self.fixed_power_w.lock() {
            return p;
        }
        self.successors
            .iter()
            .filter_map(|id| world.get(*id))
            .map(|c| c.aggregate_power_w(world))
            .sum()
    }

    fn aggregate_reactive(&self, world: &World) -> f32 {
        // No reactive override on fixed-power meters — those model
        // pure-real loads (consumer kW, CHP). If we ever need a
        // synthetic reactive load, add a `fixed_reactive_var` knob.
        if self.fixed_power_w.lock().is_some() {
            return 0.0;
        }
        self.successors
            .iter()
            .filter_map(|id| world.get(*id))
            .map(|c| c.aggregate_reactive_var(world))
            .sum()
    }

    /// Replace the fixed-power override (creating one if there wasn't
    /// any). Used by `(set-meter-power)` to drive consumer / load
    /// curves from a Lisp timer.
    pub fn set_fixed_power(&self, watts: f32) {
        *self.fixed_power_w.lock() = Some(watts);
    }
}

impl fmt::Display for Meter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl SimulatedComponent for Meter {
    fn id(&self) -> u64 {
        self.id
    }
    fn category(&self) -> Category {
        Category::Meter
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn stream_interval(&self) -> Duration {
        self.interval
    }
    fn stream_jitter_pct(&self) -> f32 {
        self.stream_jitter_pct
    }
    fn tick(&self, _world: &World, _now: DateTime<Utc>, _dt: Duration) {}

    fn telemetry(&self, world: &World) -> Telemetry {
        let grid = world.grid_state();
        let total_p = self.aggregate_active(world);
        let total_q = self.aggregate_reactive(world);

        let pp = split_per_phase(total_p, grid.voltage_per_phase);
        let qq = split_per_phase(total_q, grid.voltage_per_phase);
        let (i1, i2, i3) = per_phase_apparent_current(pp, qq, grid.voltage_per_phase);

        Telemetry {
            id: self.id,
            category: Some(Category::Meter),
            active_power_w: Some(total_p),
            reactive_power_var: Some(total_q),
            per_phase_active_w: Some(pp),
            per_phase_reactive_var: Some(qq),
            per_phase_voltage_v: Some(grid.voltage_per_phase),
            per_phase_current_a: Some((i1, i2, i3)),
            frequency_hz: Some(grid.frequency_hz),
            component_state: Some("ready"),
            ..Default::default()
        }
    }

    fn aggregate_power_w(&self, world: &World) -> f32 {
        self.aggregate_active(world)
    }

    fn aggregate_reactive_var(&self, world: &World) -> f32 {
        self.aggregate_reactive(world)
    }

    fn set_active_power_override(&self, p: f32) {
        self.set_fixed_power(p);
    }
}

/// Voltage-weighted per-phase split of a single total. Mirrors a real
/// 3-phase meter's reading on a balanced load: phase i gets
/// `total × V_i / (V1 + V2 + V3)`. Returns zeros if all voltages are
/// zero (avoids NaN).
pub fn split_per_phase(total_w: f32, voltage: (f32, f32, f32)) -> (f32, f32, f32) {
    let sum = voltage.0 + voltage.1 + voltage.2;
    if sum == 0.0 {
        return (0.0, 0.0, 0.0);
    }
    (
        total_w * voltage.0 / sum,
        total_w * voltage.1 / sum,
        total_w * voltage.2 / sum,
    )
}

/// Per-phase apparent current = `√(P² + Q²) / V` in each phase.
pub fn per_phase_apparent_current(
    p: (f32, f32, f32),
    q: (f32, f32, f32),
    v: (f32, f32, f32),
) -> (f32, f32, f32) {
    fn one(p: f32, q: f32, v: f32) -> f32 {
        if v == 0.0 {
            0.0
        } else {
            (p * p + q * q).sqrt() / v
        }
    }
    (one(p.0, q.0, v.0), one(p.1, q.1, v.1), one(p.2, q.2, v.2))
}
