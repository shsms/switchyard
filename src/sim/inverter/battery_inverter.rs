use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use crate::sim::{
    Category, SetpointError, SimulatedComponent, Telemetry, World,
    bounds::ComponentBounds,
    ramp::{CommandDelay, Ramp},
    reactive::ReactiveCapability,
};

#[derive(Clone, Debug)]
pub struct BatteryInverterConfig {
    pub rated_lower_w: f32,
    pub rated_upper_w: f32,
    pub command_delay: Duration,
    /// W/s; use `f32::INFINITY` to disable ramping.
    pub ramp_rate_w_per_s: f32,
    pub stream_jitter_pct: f32,
    /// Q envelope. Default microsim-compatible PF cap of 0.35.
    pub reactive: ReactiveCapability,
}

impl Default for BatteryInverterConfig {
    fn default() -> Self {
        Self {
            rated_lower_w: -30_000.0,
            rated_upper_w: 30_000.0,
            command_delay: Duration::ZERO,
            ramp_rate_w_per_s: f32::INFINITY,
            stream_jitter_pct: 0.0,
            reactive: ReactiveCapability::microsim_default(),
        }
    }
}

pub struct BatteryInverter {
    id: u64,
    name: String,
    interval: Duration,
    cfg: BatteryInverterConfig,
    /// IDs of the underlying batteries. The inverter pushes DC power
    /// onto them via `World::get(id).set_dc_power(share)` on every tick
    /// and reads back what was accepted.
    successors: Vec<u64>,
    bounds: Mutex<ComponentBounds>,
    reactive_var: Mutex<f32>,
    delay: CommandDelay,
    ramp: Ramp,
    /// What the children actually accepted last tick — the AC-side
    /// quantity a real inverter would publish on its telemetry bus.
    /// Differs from `ramp.actual()` whenever a battery's BMS clipped
    /// the share we pushed.
    measured_w: Mutex<f32>,
}

impl BatteryInverter {
    pub fn new(
        id: u64,
        interval: Duration,
        cfg: BatteryInverterConfig,
        successors: Vec<u64>,
    ) -> Self {
        let bounds = ComponentBounds::rated(cfg.rated_lower_w, cfg.rated_upper_w);
        let delay = CommandDelay::new(cfg.command_delay);
        let ramp = Ramp::new(cfg.ramp_rate_w_per_s, 0.0);
        Self {
            id,
            name: format!("inv-bat-{id}"),
            interval,
            cfg,
            successors,
            bounds: Mutex::new(bounds),
            reactive_var: Mutex::new(0.0),
            delay,
            ramp,
            measured_w: Mutex::new(0.0),
        }
    }
}

impl fmt::Display for BatteryInverter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl SimulatedComponent for BatteryInverter {
    fn id(&self) -> u64 {
        self.id
    }
    fn category(&self) -> Category {
        Category::Inverter
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn stream_interval(&self) -> Duration {
        self.interval
    }

    fn tick(&self, world: &World, now: DateTime<Utc>, dt: Duration) {
        self.bounds.lock().drop_expired(now);

        // We only know about our OWN bounds — clamp the pending command
        // against those, no peeking at the children. The microgrid
        // gateway gates over-envelope setpoints upstream of us; if one
        // slips through the children will refuse the excess on their
        // own and the measured aggregate will simply fall short.
        if let Some(target) = self.delay.poll(now) {
            let own = self.bounds.lock().effective();
            self.ramp.set_target(own.clamp(target));
        }

        let commanded = self.ramp.advance(dt);

        // Distribute equal shares onto the DC bus and read back what
        // each battery actually accepted (each `set_dc_power` clips
        // locally to its own derated bounds). The measured aggregate
        // is what we publish to clients, not the ramp value.
        let measured = if self.successors.is_empty() {
            commanded
        } else {
            let share = commanded / self.successors.len() as f32;
            let mut sum = 0.0;
            for id in &self.successors {
                if let Some(child) = world.get(*id) {
                    child.set_dc_power(share);
                    sum += child.aggregate_power_w();
                }
            }
            sum
        };
        *self.measured_w.lock() = measured;
    }

    fn telemetry(&self, world: &World) -> Telemetry {
        let grid = world.grid_state();
        // Report the measured AC output, not the internal ramp state —
        // those diverge when a battery clips downstream.
        let p = *self.measured_w.lock();
        let pp = split_per_phase(p, grid.voltage_per_phase);
        let rp = *self.reactive_var.lock();
        let rpp = split_per_phase(rp, grid.voltage_per_phase);
        Telemetry {
            id: self.id,
            category: Some(Category::Inverter),
            active_power_w: Some(p),
            reactive_power_var: Some(rp),
            per_phase_active_w: Some(pp),
            per_phase_reactive_var: Some(rpp),
            per_phase_voltage_v: Some(grid.voltage_per_phase),
            per_phase_current_a: Some(per_phase_current(pp, rpp, grid.voltage_per_phase)),
            frequency_hz: Some(grid.frequency_hz),
            // Reported envelope is OUR own bounds only — clients that
            // want the combined inverter+battery envelope read both
            // streams and intersect.
            active_power_bounds: Some(self.bounds.lock().effective()),
            component_state: Some(power_state(p)),
            ..Default::default()
        }
    }

    fn set_active_setpoint(&self, power_w: f32) -> Result<(), SetpointError> {
        // We don't have a `&World` here (the trait method is per-component),
        // so children-summing happens in tick(). Validation here uses our
        // own (post-augmentation) bounds — anything beyond that is a hard
        // protocol error; the SoC clamp is enforced silently via tick().
        let eff = self.bounds.lock().effective();
        if !eff.contains(power_w) {
            return Err(SetpointError::OutOfBounds {
                value: power_w,
                lower: self.cfg.rated_lower_w,
                upper: self.cfg.rated_upper_w,
            });
        }
        self.delay.set_target(Utc::now(), power_w);
        Ok(())
    }

    fn set_reactive_setpoint(&self, vars: f32) -> Result<(), SetpointError> {
        // Q is validated against the live envelope at the *current*
        // active power, not the rated upper. As P drops, the
        // allowable Q drops with it (PF-style cap) or expands toward
        // the kVA edge.
        let p_now = *self.measured_w.lock();
        let (lo, hi) = self.cfg.reactive.q_bounds_at(p_now);
        if vars < lo || vars > hi {
            return Err(SetpointError::OutOfBounds {
                value: vars,
                lower: lo,
                upper: hi,
            });
        }
        *self.reactive_var.lock() = vars;
        Ok(())
    }

    fn reset_setpoint(&self) {
        self.delay.reset();
        self.ramp.set_target(0.0);
        *self.reactive_var.lock() = 0.0;
        *self.measured_w.lock() = 0.0;
    }

    fn augment_active_bounds(
        &self,
        ts: DateTime<Utc>,
        bounds: crate::sim::bounds::VecBounds,
        lifetime: Duration,
    ) {
        self.bounds.lock().add_augmentation(ts, bounds, lifetime);
    }

    fn aggregate_power_w(&self) -> f32 {
        *self.measured_w.lock()
    }

    fn aggregate_per_phase_w(&self) -> (f32, f32, f32) {
        // Even split of measured AC output. The meter re-splits using
        // current grid voltage if it needs more accuracy.
        let p = *self.measured_w.lock();
        (p / 3.0, p / 3.0, p / 3.0)
    }

    fn rated_active_bounds(&self) -> Option<(f32, f32)> {
        Some((self.cfg.rated_lower_w, self.cfg.rated_upper_w))
    }

    fn subtype(&self) -> Option<&'static str> {
        Some("battery")
    }

    fn stream_jitter_pct(&self) -> f32 {
        self.cfg.stream_jitter_pct
    }

    fn effective_active_bounds(&self) -> Option<crate::sim::bounds::VecBounds> {
        Some(self.bounds.lock().effective())
    }

    fn reactive_bounds(&self) -> Option<(f32, f32)> {
        let p = *self.measured_w.lock();
        Some(self.cfg.reactive.q_bounds_at(p))
    }
}

fn split_per_phase(total: f32, voltage: (f32, f32, f32)) -> (f32, f32, f32) {
    let sum = voltage.0 + voltage.1 + voltage.2;
    if sum == 0.0 {
        return (0.0, 0.0, 0.0);
    }
    (
        total * voltage.0 / sum,
        total * voltage.1 / sum,
        total * voltage.2 / sum,
    )
}

fn per_phase_current(
    p: (f32, f32, f32),
    r: (f32, f32, f32),
    v: (f32, f32, f32),
) -> (f32, f32, f32) {
    fn one(p: f32, r: f32, v: f32) -> f32 {
        if v == 0.0 {
            0.0
        } else {
            (p * p + r * r).sqrt() / v
        }
    }
    (one(p.0, r.0, v.0), one(p.1, r.1, v.1), one(p.2, r.2, v.2))
}

fn power_state(p: f32) -> &'static str {
    if p > 0.0 {
        "charging"
    } else if p < 0.0 {
        "discharging"
    } else {
        "ready"
    }
}
