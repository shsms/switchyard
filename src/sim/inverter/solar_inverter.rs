//! Solar (PV) inverter. Active-side: produces a negative power
//! proportional to `sunlight_pct`, slewed by the ramp + command-delay
//! pair. Reactive-side: shares [`ReactivePath`] with the battery
//! inverter — a real PV smart inverter (IEEE 1547-2018) does Volt/VAR
//! control alongside its real-power output.

use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use crate::sim::{
    Category, SetpointError, SimulatedComponent, Telemetry, World,
    bounds::ComponentBounds,
    meter::{per_phase_apparent_current, split_per_phase},
    ramp::{CommandDelay, Ramp},
    reactive::{ReactiveCapability, ReactivePath},
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
    /// SCADA / inverter-internal latency before a Q setpoint starts
    /// being tracked. 100 ms default.
    pub reactive_command_delay: Duration,
    /// Reactive slew rate (VAR/s). 2000 default ≈ 5 s OLRT for a
    /// 10 kVAR window — IEEE 1547-2018 Cat B baseline.
    pub reactive_ramp_rate_var_per_s: f32,
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
            reactive_command_delay: Duration::from_millis(100),
            reactive_ramp_rate_var_per_s: 2000.0,
        }
    }
}

pub struct SolarInverter {
    id: u64,
    name: String,
    interval: Duration,
    cfg: SolarInverterConfig,
    /// Runtime-mutable cloud-cover knob. `cfg.sunlight_pct` seeds this
    /// at construction; thereafter every read goes through the mutex
    /// so a Lisp timer can drive a cloud-cover schedule via
    /// `(set-solar-sunlight ID PCT)`.
    sunlight_pct: Mutex<f32>,
    bounds: Mutex<ComponentBounds>,
    delay: CommandDelay,
    ramp: Ramp,
    /// Reactive-side state — capability, command-delay, slew-rate
    /// ramp, last published Q. See [`ReactivePath`].
    reactive: ReactivePath,
}

impl SolarInverter {
    pub fn new(id: u64, interval: Duration, cfg: SolarInverterConfig) -> Self {
        let init_p = cfg.rated_lower_w * cfg.sunlight_pct / 100.0;
        let bounds = ComponentBounds::rated(cfg.rated_lower_w, cfg.rated_upper_w);
        let delay = CommandDelay::new(cfg.command_delay);
        let ramp = Ramp::new(cfg.ramp_rate_w_per_s, init_p);
        ramp.set_target(init_p);
        let reactive = ReactivePath::new(
            cfg.reactive,
            cfg.reactive_command_delay,
            cfg.reactive_ramp_rate_var_per_s,
        );
        let sunlight_pct = Mutex::new(cfg.sunlight_pct);
        Self {
            id,
            name: format!("inv-pv-{id}"),
            interval,
            cfg,
            sunlight_pct,
            bounds: Mutex::new(bounds),
            delay,
            ramp,
            reactive,
        }
    }

    /// Update the live cloud-cover percentage. Drives the per-tick
    /// `min_avail = rated_lower_w × sunlight_pct / 100` clamp the
    /// inverter applies to incoming setpoints. No clamp on the
    /// input — out-of-range values just produce out-of-range
    /// `min_avail`, mirroring microsim.
    pub fn set_sunlight_pct(&self, pct: f32) {
        *self.sunlight_pct.lock() = pct;
    }

    fn min_avail_w(&self) -> f32 {
        self.cfg.rated_lower_w * *self.sunlight_pct.lock() / 100.0
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
            self.ramp.set_target(target.max(self.min_avail_w()));
        }
        let p = self.ramp.advance(dt);
        // Reactive: validated when accepted, re-clamped to the live
        // envelope at p as the command is promoted, then slewed.
        // Solar has no children to clip Q so step()'s auto-publish
        // is what telemetry reads next tick.
        self.reactive.step(p, now, dt);
    }

    fn telemetry(&self, world: &World) -> Telemetry {
        let grid = world.grid_state();
        let p = self.ramp.actual();
        let pp = split_per_phase(p, grid.voltage_per_phase);
        let rp = self.reactive.published();
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
            reactive_power_bounds: Some(self.reactive.bounds_at(p)),
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
        self.reactive.accept_setpoint(self.ramp.actual(), vars)
    }

    fn reset_setpoint(&self) {
        // Snap back to the cloud-cover-determined floor — same value
        // the per-tick clamp uses, so a reset that races with a cloud
        // shift lands consistently.
        self.delay.reset();
        self.ramp.snap_to(self.min_avail_w());
        self.reactive.reset();
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
        self.reactive.published()
    }

    fn rated_active_bounds(&self) -> Option<(f32, f32)> {
        Some((self.cfg.rated_lower_w, self.cfg.rated_upper_w))
    }

    fn reactive_bounds(&self) -> Option<(f32, f32)> {
        Some(self.reactive.bounds_at(self.ramp.actual()))
    }

    fn set_reactive_pf_limit(&self, pf: Option<f32>) {
        self.reactive.set_pf_limit(pf);
    }

    fn set_reactive_apparent_va(&self, va: Option<f32>) {
        self.reactive.set_apparent_va(va);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_sun(pct: f32) -> SolarInverterConfig {
        SolarInverterConfig {
            rated_lower_w: -10_000.0,
            rated_upper_w: 0.0,
            sunlight_pct: pct,
            ramp_rate_w_per_s: f32::INFINITY,
            ..Default::default()
        }
    }

    #[test]
    fn sunlight_pct_drives_min_avail_floor() {
        let inv = SolarInverter::new(1, Duration::from_secs(1), cfg_with_sun(50.0));
        // 50% of -10 kW rated = -5 kW available.
        assert!((inv.min_avail_w() - (-5_000.0)).abs() < 1e-3);

        // Sun goes behind a cloud → less generation available.
        inv.set_sunlight_pct(20.0);
        assert!((inv.min_avail_w() - (-2_000.0)).abs() < 1e-3);

        // Out-of-range values pass through (microsim parity).
        inv.set_sunlight_pct(150.0);
        assert!((inv.min_avail_w() - (-15_000.0)).abs() < 1e-3);
    }
}
