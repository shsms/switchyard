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
    /// Excluded from gRPC component / connection listings, but still
    /// aggregated by parent meters via World::get. Used for synthetic
    /// loads / generators that present as a power flow without being
    /// a discrete addressable component.
    hidden: bool,
}

impl Meter {
    pub fn new(
        id: u64,
        interval: Duration,
        successors: Vec<u64>,
        fixed_power_w: Option<f32>,
        stream_jitter_pct: f32,
        hidden: bool,
    ) -> Self {
        Self {
            id,
            name: format!("meter-{id}"),
            interval,
            successors,
            fixed_power_w: Mutex::new(fixed_power_w),
            stream_jitter_pct,
            hidden,
        }
    }

    /// Children to aggregate over: union of successors set at make-
    /// time (which include hidden children — those don't appear in
    /// `World.connections`) and the visible-edge entries currently
    /// in the topology graph (so post-make `(world-connect …)` calls
    /// from the UI / REPL get picked up). Returned in registration
    /// order with no duplicates.
    fn child_ids(&self, world: &World) -> Vec<u64> {
        let mut seen: std::collections::HashSet<u64> =
            self.successors.iter().copied().collect();
        let mut out = self.successors.clone();
        for (p, c) in world.connections() {
            if p == self.id && seen.insert(c) {
                out.push(c);
            }
        }
        out
    }

    fn aggregate_active(&self, world: &World) -> f32 {
        if let Some(p) = *self.fixed_power_w.lock() {
            return p;
        }
        self.child_ids(world)
            .into_iter()
            .filter_map(|id| world.get(id).map(|c| (id, c)))
            .map(|(child_id, child)| {
                // Parallel-paths share: a child with N parents in the
                // connection graph contributes 1/N to each parent. So
                // 1 inverter shared by 2 parallel meters appears as
                // half of its flow under each — the top meter sums
                // them and lands on the inverter's actual power.
                // hidden children have 0 edges in the graph; clamp
                // to 1 (this meter is the sole consumer).
                let share = world.parent_count(child_id).max(1) as f32;
                child.aggregate_power_w(world) / share
            })
            .sum()
    }

    fn aggregate_reactive(&self, world: &World) -> f32 {
        // No reactive override on fixed-power meters — those model
        // pure-real loads (consumer kW, CHP). If we ever need a
        // synthetic reactive load, add a `fixed_reactive_var` knob.
        if self.fixed_power_w.lock().is_some() {
            return 0.0;
        }
        self.child_ids(world)
            .into_iter()
            .filter_map(|id| world.get(id).map(|c| (id, c)))
            .map(|(child_id, child)| {
                let share = world.parent_count(child_id).max(1) as f32;
                child.aggregate_reactive_var(world) / share
            })
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

    fn is_hidden(&self) -> bool {
        self.hidden
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::SimulatedComponent;

    /// Stub component that returns a fixed P / Q for testing how
    /// meters aggregate their children. Doesn't model any physics.
    struct FixedFlow {
        id: u64,
        p: f32,
        q: f32,
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
        fn category(&self) -> Category {
            Category::Inverter
        }
        fn name(&self) -> &str {
            "fixed"
        }
        fn stream_interval(&self) -> Duration {
            Duration::from_secs(1)
        }
        fn tick(&self, _: &World, _: chrono::DateTime<chrono::Utc>, _: Duration) {}
        fn telemetry(&self, _: &World) -> Telemetry {
            Telemetry::default()
        }
        fn aggregate_power_w(&self, _: &World) -> f32 {
            self.p
        }
        fn aggregate_reactive_var(&self, _: &World) -> f32 {
            self.q
        }
    }

    /// 1 inverter, 2 parallel meters, 1 top meter:
    ///
    ///                  top (id 2)
    ///                  ╱     ╲
    ///         meter_a (10)  meter_b (11)
    ///                  ╲     ╱
    ///                inverter (100)
    ///
    /// inverter publishes 10 kW. Each parallel meter should see half
    /// (5 kW); the top meter aggregates both halves and lands on the
    /// inverter's actual flow (10 kW), not 20 kW.
    #[test]
    fn parallel_paths_share_one_inverter() {
        let w = World::new();

        // Register an inverter that publishes 10 kW active, 0 VAR.
        let inverter = std::sync::Arc::new(FixedFlow {
            id: 100,
            p: 10_000.0,
            q: 0.0,
        });
        w.register_arc(inverter);

        // Two parallel meters that each list the inverter as their
        // only successor — connect both edges so parent_count(100) = 2.
        let meter_a = Meter::new(10, Duration::from_secs(1), vec![100], None, 0.0, false);
        let meter_b = Meter::new(11, Duration::from_secs(1), vec![100], None, 0.0, false);
        w.register(meter_a);
        w.register(meter_b);
        w.connect(10, 100);
        w.connect(11, 100);

        // Top meter aggregates both parallel meters.
        let top = Meter::new(2, Duration::from_secs(1), vec![10, 11], None, 0.0, false);
        w.register(top);
        w.connect(2, 10);
        w.connect(2, 11);

        let m_a = w.get(10).unwrap();
        let m_b = w.get(11).unwrap();
        let m_top = w.get(2).unwrap();

        assert!((m_a.aggregate_power_w(&w) - 5_000.0).abs() < 1e-3);
        assert!((m_b.aggregate_power_w(&w) - 5_000.0).abs() < 1e-3);
        assert!((m_top.aggregate_power_w(&w) - 10_000.0).abs() < 1e-3);
    }

    /// Children connected via post-make `(world-connect …)` (eg.
    /// the UI's copy / paste flow) must aggregate too — the meter's
    /// internal successor list isn't the only source of truth.
    #[test]
    fn world_connect_after_make_aggregates() {
        let w = World::new();

        // Inverter publishes 2 kW; meter starts with no successors.
        let inverter = std::sync::Arc::new(FixedFlow {
            id: 100,
            p: 2_000.0,
            q: 0.0,
        });
        w.register_arc(inverter);
        let m = Meter::new(2, Duration::from_secs(1), vec![], None, 0.0, false);
        w.register(m);

        // Pre-connect: nothing under the meter.
        let m = w.get(2).unwrap();
        assert_eq!(m.aggregate_power_w(&w), 0.0);

        // Post-connect: aggregation picks up the new edge.
        w.connect(2, 100);
        assert!((m.aggregate_power_w(&w) - 2_000.0).abs() < 1e-3);
    }

    /// Hidden children (no edges in the connections graph) get
    /// parent_count = 0; meter aggregation clamps that to 1 so a
    /// hidden consumer-load meter contributes its full power to its
    /// owning meter.
    #[test]
    fn hidden_child_with_no_edges_contributes_full() {
        let w = World::new();
        // Hidden consumer that publishes 1500 W active.
        let consumer = std::sync::Arc::new(FixedFlow {
            id: 9000,
            p: 1500.0,
            q: 0.0,
        });
        w.register_arc(consumer);
        // No w.connect — the hidden meter convention.
        assert_eq!(w.parent_count(9000), 0);

        let m = Meter::new(2, Duration::from_secs(1), vec![9000], None, 0.0, false);
        w.register(m);
        let m = w.get(2).unwrap();
        assert!((m.aggregate_power_w(&w) - 1500.0).abs() < 1e-3);
    }
}
