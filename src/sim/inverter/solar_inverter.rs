//! Solar (PV) inverter. Active-side: produces a negative power
//! proportional to `sunlight_pct`, slewed by the ramp + command-delay
//! pair. Reactive-side: same `ReactiveCapability` envelope as the
//! battery inverter — a real PV smart inverter (IEEE 1547-2018) does
//! Volt/VAR control alongside its real-power output.

use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use crate::sim::{
    Category, SetpointError, SimulatedComponent, Telemetry, World,
    bounds::ComponentBounds,
    meter::{per_phase_apparent_current, split_per_phase},
    ramp::{CommandDelay, Ramp},
    reactive::ReactiveCapability,
};

#[derive(Clone, Debug)]
pub struct SolarInverterConfig {
    pub rated_lower_w: f32,
    pub rated_upper_w: f32,
    pub sunlight_pct: f32,
    pub command_delay: Duration,
    pub ramp_rate_w_per_s: f32,
    pub stream_jitter_pct: f32,
    /// Q envelope. Default microsim-compatible PF cap of 0.35.
    pub reactive: ReactiveCapability,
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
            reactive: ReactiveCapability::microsim_default(),
        }
    }
}

pub struct SolarInverter {
    id: u64,
    name: String,
    interval: Duration,
    cfg: SolarInverterConfig,
    bounds: Mutex<ComponentBounds>,
    /// Reactive envelope. Mutex-wrapped for runtime tweaking via the
    /// (set-reactive-pf-limit) / (set-reactive-apparent-va) defuns.
    reactive: Mutex<ReactiveCapability>,
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
        let reactive = cfg.reactive;
        Self {
            id,
            name: format!("inv-pv-{id}"),
            interval,
            cfg,
            bounds: Mutex::new(bounds),
            reactive: Mutex::new(reactive),
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
            per_phase_current_a: Some(per_phase_apparent_current(pp, rpp, grid.voltage_per_phase)),
            frequency_hz: Some(grid.frequency_hz),
            active_power_bounds: Some(self.bounds.lock().effective()),
            // Dynamic envelope: tightens with |P| under PF, expands
            // toward the kVA edge when P is small.
            reactive_power_bounds: Some(self.reactive.lock().q_bounds_at(p)),
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
        // Same envelope semantics as the battery inverter: Q is
        // validated against the live (lo, hi) at the *current* P, so
        // a curtailed PV inverter with P near 0 has tight Q room
        // under a PF cap, and full ±S_rated room under a kVA cap.
        let p_now = self.ramp.actual();
        let (lo, hi) = self.reactive.lock().q_bounds_at(p_now);
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
        let init_p = self.cfg.rated_lower_w * self.cfg.sunlight_pct / 100.0;
        self.delay.reset();
        self.ramp.snap_to(init_p);
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

    fn aggregate_power_w(&self, _world: &World) -> f32 {
        self.ramp.actual()
    }

    fn aggregate_reactive_var(&self, _world: &World) -> f32 {
        *self.reactive_var.lock()
    }

    fn rated_active_bounds(&self) -> Option<(f32, f32)> {
        Some((self.cfg.rated_lower_w, self.cfg.rated_upper_w))
    }

    fn reactive_bounds(&self) -> Option<(f32, f32)> {
        Some(self.reactive.lock().q_bounds_at(self.ramp.actual()))
    }

    fn set_reactive_pf_limit(&self, pf: Option<f32>) {
        self.reactive.lock().pf_limit = pf;
    }

    fn set_reactive_apparent_va(&self, va: Option<f32>) {
        self.reactive.lock().apparent_va = va;
    }

    fn subtype(&self) -> Option<&'static str> {
        Some("solar")
    }

    fn stream_jitter_pct(&self) -> f32 {
        self.cfg.stream_jitter_pct
    }

    fn effective_active_bounds(&self) -> Option<crate::sim::bounds::VecBounds> {
        Some(self.bounds.lock().effective())
    }
}
