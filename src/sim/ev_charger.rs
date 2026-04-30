//! EV charger — AC charging with command-delay + slew-rate-limited
//! ramp on the set-point, plus the same SoC-protective derate the
//! battery uses (charge taper near `soc_upper`, discharge near
//! `soc_lower` — though most chargers stay non-negative in practice).

use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use crate::sim::{
    Category, SetpointError, SimulatedComponent, Telemetry, World,
    bounds::VecBounds,
    decay::{SocProtect, soc_protected_bounds},
    ramp::{CommandDelay, Ramp},
};

#[derive(Clone, Debug)]
pub struct EvChargerConfig {
    pub rated_lower_w: f32,
    pub rated_upper_w: f32,
    pub initial_soc_pct: f32,
    pub soc_lower_pct: f32,
    pub soc_upper_pct: f32,
    pub soc_protect_margin_pct: f32,
    pub capacity_wh: f32,
    pub command_delay: Duration,
    pub ramp_rate_w_per_s: f32,
    pub stream_jitter_pct: f32,
}

impl Default for EvChargerConfig {
    fn default() -> Self {
        Self {
            rated_lower_w: 0.0,
            rated_upper_w: 22_000.0,
            initial_soc_pct: 50.0,
            soc_lower_pct: 0.0,
            soc_upper_pct: 100.0,
            soc_protect_margin_pct: 10.0,
            capacity_wh: 30_000.0,
            command_delay: Duration::from_millis(500),
            ramp_rate_w_per_s: f32::INFINITY,
            stream_jitter_pct: 0.0,
        }
    }
}

pub struct EvCharger {
    id: u64,
    name: String,
    interval: Duration,
    cfg: EvChargerConfig,
    state: Mutex<EvState>,
    delay: CommandDelay,
    ramp: Ramp,
}

#[derive(Debug, Clone)]
struct EvState {
    energy_wh: f32,
    soc_pct: f32,
    /// SoC-protected effective bounds, refreshed every tick.
    effective_lower_w: f32,
    effective_upper_w: f32,
}

impl EvCharger {
    pub fn new(id: u64, interval: Duration, cfg: EvChargerConfig) -> Self {
        let init_soc = cfg.initial_soc_pct;
        let (l, u) = soc_protected_bounds(
            cfg.rated_lower_w,
            cfg.rated_upper_w,
            init_soc,
            SocProtect {
                soc_lower_pct: cfg.soc_lower_pct,
                soc_upper_pct: cfg.soc_upper_pct,
                margin_pct: cfg.soc_protect_margin_pct,
            },
        );
        Self {
            id,
            name: format!("ev-charger-{id}"),
            interval,
            cfg: cfg.clone(),
            state: Mutex::new(EvState {
                energy_wh: 0.0,
                soc_pct: init_soc,
                effective_lower_w: l,
                effective_upper_w: u,
            }),
            delay: CommandDelay::new(cfg.command_delay),
            ramp: Ramp::new(cfg.ramp_rate_w_per_s, 0.0),
        }
    }

    fn refresh_bounds(&self, soc: f32) -> (f32, f32) {
        soc_protected_bounds(
            self.cfg.rated_lower_w,
            self.cfg.rated_upper_w,
            soc,
            SocProtect {
                soc_lower_pct: self.cfg.soc_lower_pct,
                soc_upper_pct: self.cfg.soc_upper_pct,
                margin_pct: self.cfg.soc_protect_margin_pct,
            },
        )
    }
}

impl fmt::Display for EvCharger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl SimulatedComponent for EvCharger {
    fn id(&self) -> u64 {
        self.id
    }
    fn category(&self) -> Category {
        Category::EvCharger
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn stream_interval(&self) -> Duration {
        self.interval
    }
    fn stream_jitter_pct(&self) -> f32 {
        self.cfg.stream_jitter_pct
    }

    fn tick(&self, _world: &World, now: DateTime<Utc>, dt: Duration) {
        // 1. Refresh SoC-derated bounds and snapshot them for the rest
        //    of the tick under a single lock acquisition. Splitting
        //    `(self.state.lock().lo, self.state.lock().up)` would
        //    re-enter the same parking_lot::Mutex and deadlock.
        let (lower, upper) = {
            let mut s = self.state.lock();
            let (l, u) = self.refresh_bounds(s.soc_pct);
            s.effective_lower_w = l;
            s.effective_upper_w = u;
            (l, u)
        };

        // 2. Promote pending command + clamp into the new envelope.
        if let Some(target) = self.delay.poll(now) {
            self.ramp.set_target(target.clamp(lower, upper));
        } else {
            // Pull existing target back if SoC just narrowed it.
            let t = self.ramp.target();
            let clamped = t.clamp(lower, upper);
            if (clamped - t).abs() > f32::EPSILON {
                self.ramp.set_target(clamped);
            }
        }

        // 3. Slew + integrate energy/SoC.
        let p = self.ramp.advance(dt);
        let mut s = self.state.lock();
        s.energy_wh += p * dt.as_secs_f32() / 3600.0;
        s.soc_pct = (self.cfg.initial_soc_pct
            + (s.energy_wh / self.cfg.capacity_wh) * 100.0)
            .clamp(0.0, 100.0);
    }

    fn telemetry(&self, world: &World) -> Telemetry {
        let grid = world.grid_state();
        let p = self.ramp.actual();
        let s = self.state.lock().clone();
        Telemetry {
            id: self.id,
            category: Some(Category::EvCharger),
            active_power_w: Some(p),
            soc_pct: Some(s.soc_pct),
            soc_lower_pct: Some(self.cfg.soc_lower_pct),
            soc_upper_pct: Some(self.cfg.soc_upper_pct),
            capacity_wh: Some(self.cfg.capacity_wh),
            per_phase_voltage_v: Some(grid.voltage_per_phase),
            frequency_hz: Some(grid.frequency_hz),
            active_power_bounds: Some(VecBounds::single(
                s.effective_lower_w,
                s.effective_upper_w,
            )),
            cable_state: Some("ev-charging-cable-locked-at-ev"),
            ..Default::default()
        }
    }

    fn set_active_setpoint(&self, power_w: f32) -> Result<(), SetpointError> {
        // Validate against rated, not SoC-derated — the SoC clamp is
        // enforced silently per tick to avoid bouncing between accept/
        // reject as the cell tops up.
        if power_w < self.cfg.rated_lower_w || power_w > self.cfg.rated_upper_w {
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
        self.delay.reset();
        self.ramp.snap_to(0.0);
    }

    fn aggregate_power_w(&self) -> f32 {
        self.ramp.actual()
    }

    fn rated_active_bounds(&self) -> Option<(f32, f32)> {
        Some((self.cfg.rated_lower_w, self.cfg.rated_upper_w))
    }

    fn effective_active_bounds(&self) -> Option<VecBounds> {
        let s = self.state.lock();
        Some(VecBounds::single(s.effective_lower_w, s.effective_upper_w))
    }
}
