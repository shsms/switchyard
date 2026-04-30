//! Solar (PV) inverter — placeholder implementation that produces a
//! constant negative power proportional to `sunlight_pct`. Set-point
//! curtailment, ramping, and the SoC-style derate need to land here in
//! a follow-up.

use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use crate::sim::{
    Category, SetpointError, SimulatedComponent, Telemetry, World,
    bounds::ComponentBounds,
    ramp::{CommandDelay, Ramp},
};

#[derive(Clone, Debug)]
pub struct SolarInverterConfig {
    pub rated_lower_w: f32,
    pub rated_upper_w: f32,
    pub sunlight_pct: f32,
    pub command_delay: Duration,
    pub ramp_rate_w_per_s: f32,
    pub stream_jitter_pct: f32,
}

impl Default for SolarInverterConfig {
    fn default() -> Self {
        Self {
            rated_lower_w: -30_000.0,
            rated_upper_w: 0.0,
            sunlight_pct: 100.0,
            command_delay: Duration::ZERO,
            ramp_rate_w_per_s: f32::INFINITY,
            stream_jitter_pct: 0.0,
        }
    }
}

pub struct SolarInverter {
    id: u64,
    name: String,
    interval: Duration,
    cfg: SolarInverterConfig,
    bounds: Mutex<ComponentBounds>,
    delay: CommandDelay,
    ramp: Ramp,
    reactive_var: Mutex<f32>,
}

impl SolarInverter {
    pub fn new(id: u64, interval: Duration, cfg: SolarInverterConfig) -> Self {
        let init_p = cfg.rated_lower_w * cfg.sunlight_pct / 100.0;
        let bounds = ComponentBounds::rated(cfg.rated_lower_w, cfg.rated_upper_w);
        let delay = CommandDelay::new(cfg.command_delay);
        let ramp = Ramp::new(cfg.ramp_rate_w_per_s, init_p);
        ramp.set_target(init_p);
        Self {
            id,
            name: format!("inv-pv-{id}"),
            interval,
            cfg,
            bounds: Mutex::new(bounds),
            delay,
            ramp,
            reactive_var: Mutex::new(0.0),
        }
    }
}

impl fmt::Display for SolarInverter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl SimulatedComponent for SolarInverter {
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

    fn tick(&self, _world: &World, now: DateTime<Utc>, dt: Duration) {
        self.bounds.lock().drop_expired(now);
        if let Some(target) = self.delay.poll(now) {
            let min_avail = self.cfg.rated_lower_w * self.cfg.sunlight_pct / 100.0;
            let clamped = target.max(min_avail);
            self.ramp.set_target(clamped);
        }
        self.ramp.advance(dt);
    }

    fn telemetry(&self, world: &World) -> Telemetry {
        let grid = world.grid_state();
        let p = self.ramp.actual();
        Telemetry {
            id: self.id,
            category: Some(Category::Inverter),
            active_power_w: Some(p),
            reactive_power_var: Some(*self.reactive_var.lock()),
            per_phase_voltage_v: Some(grid.voltage_per_phase),
            frequency_hz: Some(grid.frequency_hz),
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

    fn reset_setpoint(&self) {
        let init_p = self.cfg.rated_lower_w * self.cfg.sunlight_pct / 100.0;
        self.delay.reset();
        self.ramp.snap_to(init_p);
        *self.reactive_var.lock() = 0.0;
    }

    fn aggregate_power_w(&self) -> f32 {
        self.ramp.actual()
    }

    fn rated_active_bounds(&self) -> Option<(f32, f32)> {
        Some((self.cfg.rated_lower_w, self.cfg.rated_upper_w))
    }

    fn subtype(&self) -> Option<&'static str> {
        Some("solar")
    }

    fn stream_jitter_pct(&self) -> f32 {
        self.cfg.stream_jitter_pct
    }
}
