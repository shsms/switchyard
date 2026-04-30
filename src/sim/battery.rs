use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use crate::sim::{
    Category, SimulatedComponent, Telemetry, World,
    bounds::VecBounds,
    decay::{SocProtect, soc_protected_bounds as decay_soc_bounds},
};

/// Tunables exposed via `(make-battery :soc-protect-margin 10.0 …)`.
#[derive(Clone, Debug)]
pub struct BatteryConfig {
    pub capacity_wh: f32,
    pub initial_soc_pct: f32,
    pub soc_lower_pct: f32,
    pub soc_upper_pct: f32,
    pub voltage_v: f32,
    pub rated_lower_w: f32,
    pub rated_upper_w: f32,
    /// Width of the SoC band (in % points) where the rated DC bound is
    /// tapered toward zero. With margin = 10 and `soc_upper_pct = 90`,
    /// the charge bound starts decaying at SoC=80% and reaches 0 at
    /// SoC=90%. Same on the discharge side near `soc_lower_pct`. Set to
    /// `0.0` to disable.
    pub soc_protect_margin_pct: f32,
    pub stream_jitter_pct: f32,
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
            soc_protect_margin_pct: 10.0,
            stream_jitter_pct: 0.0,
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
    /// Active DC power in W. Drives SoC integration — net energy
    /// transferred is set by P only, even when the inverter is also
    /// pushing reactive load through the DC bus.
    power_w: f32,
    /// Reactive component the inverter pushed onto the DC bus this
    /// tick. Doesn't change SoC, but inflates dc_current and
    /// apparent dc_power in telemetry to reflect the conductor /
    /// capacitor / IGBT loading a real DC ammeter would read.
    reactive_var: f32,
    energy_wh: f32,
    soc_pct: f32,
    /// Cached effective DC bounds — recomputed every tick from SoC,
    /// then read by `effective_active_bounds` and the inverter.
    effective_lower_w: f32,
    effective_upper_w: f32,
}

impl Battery {
    pub fn new(id: u64, interval: Duration, cfg: BatteryConfig) -> Self {
        let init_soc = cfg.initial_soc_pct;
        let (l, u) = soc_protected_bounds(&cfg, init_soc);
        Self {
            id,
            name: format!("bat-{id}"),
            interval,
            cfg,
            state: Mutex::new(BatteryState {
                power_w: 0.0,
                reactive_var: 0.0,
                energy_wh: 0.0,
                soc_pct: init_soc,
                effective_lower_w: l,
                effective_upper_w: u,
            }),
        }
    }

    pub fn power_w(&self) -> f32 {
        self.state.lock().power_w
    }
}

fn soc_protected_bounds(cfg: &BatteryConfig, soc: f32) -> (f32, f32) {
    decay_soc_bounds(
        cfg.rated_lower_w,
        cfg.rated_upper_w,
        soc,
        SocProtect {
            soc_lower_pct: cfg.soc_lower_pct,
            soc_upper_pct: cfg.soc_upper_pct,
            margin_pct: cfg.soc_protect_margin_pct,
        },
    )
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
    fn stream_jitter_pct(&self) -> f32 {
        self.cfg.stream_jitter_pct
    }

    fn tick(&self, _world: &World, _now: DateTime<Utc>, dt: Duration) {
        let mut s = self.state.lock();

        // Energy ↔ SoC update from current power.
        s.energy_wh += s.power_w * dt.as_secs_f32() / 3600.0;
        s.soc_pct = (self.cfg.initial_soc_pct + (s.energy_wh / self.cfg.capacity_wh) * 100.0)
            .clamp(0.0, 100.0);

        // Refresh SoC-derated bounds.
        let (l, u) = soc_protected_bounds(&self.cfg, s.soc_pct);
        s.effective_lower_w = l;
        s.effective_upper_w = u;

        // Self-clamp: if the inverter just pushed power that's above
        // the new derated ceiling, pull it back. The inverter will
        // re-aggregate next tick and stop overshooting.
        if s.power_w > s.effective_upper_w {
            s.power_w = s.effective_upper_w;
        }
        if s.power_w < s.effective_lower_w {
            s.power_w = s.effective_lower_w;
        }
    }

    fn telemetry(&self, _world: &World) -> Telemetry {
        let s = self.state.lock().clone();
        // Apparent DC magnitude with sign of P. Reactive load
        // doesn't move net energy (so SoC integrates on `power_w`
        // alone, see tick()) but it does flow through the conductors,
        // so dc_power and dc_current here reflect the apparent
        // loading a real instrument would read.
        let apparent = (s.power_w * s.power_w + s.reactive_var * s.reactive_var).sqrt();
        let signed_apparent = apparent * if s.power_w >= 0.0 { 1.0 } else { -1.0 };
        Telemetry {
            id: self.id,
            category: Some(Category::Battery),
            soc_pct: Some(s.soc_pct),
            soc_lower_pct: Some(self.cfg.soc_lower_pct),
            soc_upper_pct: Some(self.cfg.soc_upper_pct),
            capacity_wh: Some(self.cfg.capacity_wh),
            dc_voltage_v: Some(self.cfg.voltage_v),
            dc_power_w: Some(signed_apparent),
            dc_current_a: Some(if self.cfg.voltage_v != 0.0 {
                signed_apparent / self.cfg.voltage_v
            } else {
                0.0
            }),
            active_power_bounds: Some(VecBounds::single(s.effective_lower_w, s.effective_upper_w)),
            component_state: Some(power_to_state(s.power_w)),
            relay_state: Some("relay-closed"),
            ..Default::default()
        }
    }

    fn aggregate_power_w(&self, _world: &World) -> f32 {
        self.state.lock().power_w
    }

    /// Accept whatever the inverter pushed onto the DC bus, clipped to
    /// the SoC-protective effective bounds. The inverter has no data
    /// link that lets it know our limits — it sends a setpoint and
    /// reads back what was actually accepted.
    fn set_dc_power(&self, p: f32) {
        let mut s = self.state.lock();
        s.power_w = p.clamp(s.effective_lower_w, s.effective_upper_w);
        s.reactive_var = 0.0;
    }

    /// Active+reactive variant. Active is clamped to the SoC envelope
    /// like in `set_dc_power`; reactive is stored verbatim — the
    /// battery itself doesn't refuse Q, the inverter is what
    /// terminates reactive energy on the AC side.
    fn set_dc_active_reactive(&self, p: f32, q: f32) {
        let mut s = self.state.lock();
        s.power_w = p.clamp(s.effective_lower_w, s.effective_upper_w);
        s.reactive_var = q;
    }

    fn aggregate_reactive_var(&self, _world: &World) -> f32 {
        self.state.lock().reactive_var
    }

    fn rated_active_bounds(&self) -> Option<(f32, f32)> {
        Some((self.cfg.rated_lower_w, self.cfg.rated_upper_w))
    }

    fn effective_active_bounds(&self) -> Option<VecBounds> {
        let s = self.state.lock();
        Some(VecBounds::single(s.effective_lower_w, s.effective_upper_w))
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
