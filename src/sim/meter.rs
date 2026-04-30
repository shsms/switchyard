use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};

use crate::sim::{Category, SimulatedComponent, Telemetry, World};

/// A power meter sums its successors' power. If the parent registered
/// with explicit `power_w`, the meter reports that value instead — the
/// "consumer" branch in microsim's config does this to model load.
pub struct Meter {
    id: u64,
    name: String,
    interval: Duration,
    successors: Vec<u64>,
    /// Override for headless meters (consumer / CHP branches).
    fixed_power_w: Option<f32>,
}

impl Meter {
    pub fn new(id: u64, interval: Duration, successors: Vec<u64>, fixed_power_w: Option<f32>) -> Self {
        Self {
            id,
            name: format!("meter-{id}"),
            interval,
            successors,
            fixed_power_w,
        }
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
    fn tick(&self, _world: &World, _now: DateTime<Utc>, _dt: Duration) {}

    fn telemetry(&self, world: &World) -> Telemetry {
        let grid = world.grid_state();

        let (p1, p2, p3) = if let Some(p) = self.fixed_power_w {
            split_per_phase(p, grid.voltage_per_phase)
        } else {
            sum_per_phase(world, &self.successors)
        };
        let total = p1 + p2 + p3;
        let (v1, v2, v3) = grid.voltage_per_phase;
        let i1 = if v1 != 0.0 { p1 / v1 } else { 0.0 };
        let i2 = if v2 != 0.0 { p2 / v2 } else { 0.0 };
        let i3 = if v3 != 0.0 { p3 / v3 } else { 0.0 };

        Telemetry {
            id: self.id,
            category: Some(Category::Meter),
            active_power_w: Some(total),
            per_phase_active_w: Some((p1, p2, p3)),
            per_phase_voltage_v: Some(grid.voltage_per_phase),
            per_phase_current_a: Some((i1, i2, i3)),
            frequency_hz: Some(grid.frequency_hz),
            component_state: Some("ready"),
            ..Default::default()
        }
    }

    fn aggregate_power_w(&self) -> f32 {
        if let Some(p) = self.fixed_power_w {
            return p;
        }
        // Meter aggregation when nested: just zero — the upstream
        // meter aggregates leaves directly. Real flow modeling here
        // would require world.get(child).
        0.0
    }
}

fn sum_per_phase(world: &World, ids: &[u64]) -> (f32, f32, f32) {
    let mut acc = (0.0, 0.0, 0.0);
    for id in ids {
        if let Some(child) = world.get(*id) {
            let (p1, p2, p3) = child.aggregate_per_phase_w();
            acc.0 += p1;
            acc.1 += p2;
            acc.2 += p3;
            // Components that only expose total power (DC-flavored
            // ones like batteries on the inverter side) get split here.
            if p1 == 0.0 && p2 == 0.0 && p3 == 0.0 {
                let total = child.aggregate_power_w();
                if total != 0.0 {
                    let grid = world.grid_state();
                    let split = split_per_phase(total, grid.voltage_per_phase);
                    acc.0 += split.0;
                    acc.1 += split.1;
                    acc.2 += split.2;
                }
            }
        }
    }
    acc
}

fn split_per_phase(total_w: f32, voltage: (f32, f32, f32)) -> (f32, f32, f32) {
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
