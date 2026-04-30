use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use crate::sim::{Category, SimulatedComponent, Telemetry, World, bounds::VecBounds};

/// Tunables exposed via `(make-battery :soc-charge-protect t :soc-charge-protect-margin 10.0 …)`.
#[derive(Clone, Debug)]
pub struct BatteryConfig {
    pub capacity_wh: f32,
    pub initial_soc_pct: f32,
    pub soc_lower_pct: f32,
    pub soc_upper_pct: f32,
    pub voltage_v: f32,
    pub rated_lower_w: f32,
    pub rated_upper_w: f32,
}

impl Default for BatteryConfig {
    fn default() -> Self {
        Self {
            capacity_wh: 92_000.0,
            initial_soc_pct: 50.0,
            soc_lower_pct: 10.0,
            soc_upper_pct: 90.0,
            voltage_v: 800.0,
            rated_lower_w: -30_000.0,
            rated_upper_w: 30_000.0,
        }
    }
}

pub struct Battery {
    id: u64,
    name: String,
    interval: Duration,
    cfg: BatteryConfig,
    state: Mutex<BatteryState>,
}

#[derive(Debug, Clone)]
struct BatteryState {
    power_w: f32,
    energy_wh: f32,
    soc_pct: f32,
}

impl Battery {
    pub fn new(id: u64, interval: Duration, cfg: BatteryConfig) -> Self {
        let init_soc = cfg.initial_soc_pct;
        Self {
            id,
            name: format!("bat-{id}"),
            interval,
            cfg,
            state: Mutex::new(BatteryState {
                power_w: 0.0,
                energy_wh: 0.0,
                soc_pct: init_soc,
            }),
        }
    }

    pub fn power_w(&self) -> f32 {
        self.state.lock().power_w
    }

    /// Set DC power (positive = charging from inverter, negative =
    /// discharging into inverter). Bounds are enforced by the parent
    /// inverter; this method is unconditional so the inverter can
    /// distribute its share verbatim.
    pub fn set_power_w(&self, p: f32) {
        self.state.lock().power_w = p;
    }
}

impl fmt::Display for Battery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl SimulatedComponent for Battery {
    fn id(&self) -> u64 {
        self.id
    }
    fn category(&self) -> Category {
        Category::Battery
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn stream_interval(&self) -> Duration {
        self.interval
    }

    fn tick(&self, _world: &World, _now: DateTime<Utc>, dt: Duration) {
        let mut s = self.state.lock();
        s.energy_wh += s.power_w * dt.as_secs_f32() / 3600.0;
        // SoC% derived from energy with one decimal place
        s.soc_pct = (self.cfg.initial_soc_pct
            + (s.energy_wh / self.cfg.capacity_wh) * 100.0)
            .clamp(0.0, 100.0);
    }

    fn telemetry(&self, _world: &World) -> Telemetry {
        let s = self.state.lock().clone();
        Telemetry {
            id: self.id,
            category: Some(Category::Battery),
            soc_pct: Some(s.soc_pct),
            soc_lower_pct: Some(self.cfg.soc_lower_pct),
            soc_upper_pct: Some(self.cfg.soc_upper_pct),
            capacity_wh: Some(self.cfg.capacity_wh),
            dc_voltage_v: Some(self.cfg.voltage_v),
            dc_power_w: Some(s.power_w),
            dc_current_a: Some(if self.cfg.voltage_v != 0.0 {
                s.power_w / self.cfg.voltage_v
            } else {
                0.0
            }),
            active_power_bounds: Some(VecBounds::single(
                self.cfg.rated_lower_w,
                self.cfg.rated_upper_w,
            )),
            component_state: Some(power_to_state(s.power_w)),
            relay_state: Some("relay-closed"),
            ..Default::default()
        }
    }

    fn aggregate_power_w(&self) -> f32 {
        self.state.lock().power_w
    }

    fn set_dc_power(&self, p: f32) {
        self.set_power_w(p);
    }

    fn rated_active_bounds(&self) -> Option<(f32, f32)> {
        Some((self.cfg.rated_lower_w, self.cfg.rated_upper_w))
    }
}

fn power_to_state(p: f32) -> &'static str {
    if p > 0.0 {
        "charging"
    } else if p < 0.0 {
        "discharging"
    } else {
        "ready"
    }
}
