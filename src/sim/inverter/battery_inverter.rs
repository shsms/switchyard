use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use crate::sim::{
    Category, SetpointError, SimulatedComponent, Telemetry, World,
    bounds::ComponentBounds,
    ramp::{CommandDelay, Ramp},
};

#[derive(Clone, Debug)]
pub struct BatteryInverterConfig {
    pub rated_lower_w: f32,
    pub rated_upper_w: f32,
    pub command_delay: Duration,
    /// W/s; use `f32::INFINITY` to disable ramping.
    pub ramp_rate_w_per_s: f32,
}

impl Default for BatteryInverterConfig {
    fn default() -> Self {
        Self {
            rated_lower_w: -30_000.0,
            rated_upper_w: 30_000.0,
            command_delay: Duration::ZERO,
            ramp_rate_w_per_s: f32::INFINITY,
        }
    }
}

pub struct BatteryInverter {
    id: u64,
    name: String,
    interval: Duration,
    cfg: BatteryInverterConfig,
    /// IDs of the underlying batteries. The inverter pushes DC power
    /// onto them via `World::get(id).set_dc_power(share)` on every tick.
    successors: Vec<u64>,
    bounds: Mutex<ComponentBounds>,
    reactive_var: Mutex<f32>,
    delay: CommandDelay,
    ramp: Ramp,
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

        if let Some(target) = self.delay.poll(now) {
            let clamped = self.bounds.lock().clamp(target);
            self.ramp.set_target(clamped);
        }

        let actual = self.ramp.advance(dt);

        if !self.successors.is_empty() {
            let share = actual / self.successors.len() as f32;
            for id in &self.successors {
                if let Some(child) = world.get(*id) {
                    child.set_dc_power(share);
                }
            }
        }
    }

    fn telemetry(&self, world: &World) -> Telemetry {
        let grid = world.grid_state();
        let p = self.ramp.actual();
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
            component_state: Some(power_state(p)),
            ..Default::default()
        }
    }

    fn set_active_setpoint(&self, power_w: f32) -> Result<(), SetpointError> {
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
        let abs_p = self.ramp.actual().abs();
        if vars < -0.35 * abs_p || vars > 0.35 * abs_p {
            return Err(SetpointError::OutOfBounds {
                value: vars,
                lower: -0.35 * abs_p,
                upper: 0.35 * abs_p,
            });
        }
        *self.reactive_var.lock() = vars;
        Ok(())
    }

    fn reset_setpoint(&self) {
        self.delay.reset();
        self.ramp.set_target(0.0);
        *self.reactive_var.lock() = 0.0;
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
        self.ramp.actual()
    }

    fn aggregate_per_phase_w(&self) -> (f32, f32, f32) {
        // Even split. The meter, on telemetry, will re-split using
        // current grid voltage if it needs more accuracy.
        let p = self.ramp.actual();
        (p / 3.0, p / 3.0, p / 3.0)
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
