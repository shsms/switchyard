//! EV charger — placeholder. Models cable lock state, SoC, and a
//! command-delayed / ramp-limited power set-point on the AC side.

use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use crate::sim::{
    Category, SetpointError, SimulatedComponent, Telemetry, World,
    ramp::{CommandDelay, Ramp},
};

#[derive(Clone, Debug)]
pub struct EvChargerConfig {
    pub rated_lower_w: f32,
    pub rated_upper_w: f32,
    pub initial_soc_pct: f32,
    pub capacity_wh: f32,
    pub command_delay: Duration,
    pub ramp_rate_w_per_s: f32,
}

impl Default for EvChargerConfig {
    fn default() -> Self {
        Self {
            rated_lower_w: 0.0,
            rated_upper_w: 22_000.0,
            initial_soc_pct: 50.0,
            capacity_wh: 30_000.0,
            command_delay: Duration::from_millis(500),
            ramp_rate_w_per_s: f32::INFINITY,
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
}

impl EvCharger {
    pub fn new(id: u64, interval: Duration, cfg: EvChargerConfig) -> Self {
        let init_soc = cfg.initial_soc_pct;
        Self {
            id,
            name: format!("ev-charger-{id}"),
            interval,
            cfg: cfg.clone(),
            state: Mutex::new(EvState {
                energy_wh: 0.0,
                soc_pct: init_soc,
            }),
            delay: CommandDelay::new(cfg.command_delay),
            ramp: Ramp::new(cfg.ramp_rate_w_per_s, 0.0),
        }
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

    fn tick(&self, _world: &World, now: DateTime<Utc>, dt: Duration) {
        if let Some(target) = self.delay.poll(now) {
            self.ramp.set_target(target.clamp(self.cfg.rated_lower_w, self.cfg.rated_upper_w));
        }
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
        Telemetry {
            id: self.id,
            category: Some(Category::EvCharger),
            active_power_w: Some(p),
            soc_pct: Some(self.state.lock().soc_pct),
            capacity_wh: Some(self.cfg.capacity_wh),
            per_phase_voltage_v: Some(grid.voltage_per_phase),
            frequency_hz: Some(grid.frequency_hz),
            cable_state: Some("ev-charging-cable-locked-at-ev"),
            ..Default::default()
        }
    }

    fn set_active_setpoint(&self, power_w: f32) -> Result<(), SetpointError> {
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
}
