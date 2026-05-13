//! EV charger — AC charging with command-delay + slew-rate-limited
//! ramp on the set-point, plus the same SoC-protective derate the
//! battery uses (charge taper near `soc_upper`, discharge near
//! `soc_lower` — though most chargers stay non-negative in practice).

use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use crate::sim::{
    Category, SetpointError, SimulatedComponent, Telemetry, World,
    bounds::{ComponentBounds, VecBounds},
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
    /// Rated bounds + a queue of time-limited augmentations applied
    /// via `AugmentElectricalComponentBounds`. The SoC-protective
    /// derate is composed on top at tick time — see `tick`.
    bounds: Mutex<ComponentBounds>,
}

#[derive(Debug, Clone)]
struct EvState {
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
        let delay = CommandDelay::new(cfg.command_delay);
        let ramp = Ramp::new(cfg.ramp_rate_w_per_s, 0.0);
        let bounds = ComponentBounds::rated(cfg.rated_lower_w, cfg.rated_upper_w);
        Self {
            id,
            name: format!("ev-charger-{id}"),
            interval,
            cfg,
            state: Mutex::new(EvState {
                soc_pct: init_soc,
                effective_lower_w: l,
                effective_upper_w: u,
            }),
            delay,
            ramp,
            bounds: Mutex::new(bounds),
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
        // 1. Drop any expired augmentations before recomputing
        //    bounds — otherwise a just-elapsed narrowing would clip
        //    the ramp for one extra tick.
        self.bounds.lock().drop_expired(now);

        // 2. Refresh SoC-derated bounds and snapshot them for the rest
        //    of the tick under a single lock acquisition. Splitting
        //    `(self.state.lock().lo, self.state.lock().up)` would
        //    re-enter the same parking_lot::Mutex and deadlock.
        let (soc_lo, soc_hi) = {
            let mut s = self.state.lock();
            let (l, u) = self.refresh_bounds(s.soc_pct);
            s.effective_lower_w = l;
            s.effective_upper_w = u;
            (l, u)
        };

        // 3. Compose the effective envelope: SoC-protected ∩ rated ∩
        //    augmentations. Both sides are single-bucket today, so
        //    the intersection is single-bucket too. If
        //    augmentations don't overlap SoC-protected at all (rare
        //    — a client narrowed the rated range tighter than the
        //    derate), refuse to charge or discharge.
        let aug_eff = self.bounds.lock().effective();
        let envelope = VecBounds::single(soc_lo, soc_hi).intersect(&aug_eff);
        let (lower, upper) = envelope
            .0
            .first()
            .map(|b| (b.lower.unwrap_or(soc_lo), b.upper.unwrap_or(soc_hi)))
            .unwrap_or((0.0, 0.0));

        // 4. Promote pending command + clamp into the composed
        //    envelope.
        if let Some(target) = self.delay.poll(now) {
            self.ramp.set_target(target.clamp(lower, upper));
        } else {
            // Pull existing target back if SoC or a fresh
            // augmentation just narrowed it.
            let t = self.ramp.target();
            let clamped = t.clamp(lower, upper);
            if (clamped - t).abs() > f32::EPSILON {
                self.ramp.set_target(clamped);
            }
        }

        // 5. Slew + integrate SoC. ΔSoC = P · dt / capacity, in %.
        // Clamping at the SoC boundary prevents unphysical "extra"
        // charge from accumulating when the protective taper is
        // disabled — same fix as Battery.
        let p = self.ramp.advance(dt);
        let mut s = self.state.lock();
        if self.cfg.capacity_wh > 0.0 {
            let delta_soc = p * dt.as_secs_f32() / 3600.0 / self.cfg.capacity_wh * 100.0;
            s.soc_pct = (s.soc_pct + delta_soc).clamp(0.0, 100.0);
        }
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
            active_power_bounds: self.effective_active_bounds(),
            cable_state: Some("ev-charging-cable-locked-at-ev"),
            ..Default::default()
        }
    }

    fn set_active_setpoint(&self, power_w: f32) -> Result<(), SetpointError> {
        // Validate against rated ∩ augmentations, not SoC-derated —
        // the SoC clamp stays silent (avoids bouncing accept / reject
        // as the cell tops up). Augmentations are an explicit
        // narrowing the client just asked for and expects to take
        // effect, so they belong in the validation envelope.
        let envelope = self.bounds.lock().effective();
        if !envelope.contains(power_w) {
            return Err(SetpointError::OutOfBounds {
                value: power_w,
                envelope,
            });
        }
        self.delay.set_target(Utc::now(), power_w);
        Ok(())
    }

    fn augment_active_bounds(
        &self,
        ts: DateTime<Utc>,
        bounds: VecBounds,
        lifetime: Duration,
    ) {
        self.bounds.lock().add_augmentation(ts, bounds, lifetime);
    }

    fn reset_setpoint(&self) {
        self.delay.reset();
        self.ramp.snap_to(0.0);
    }

    fn aggregate_power_w(&self, _world: &World) -> f32 {
        self.ramp.actual()
    }

    fn rated_active_bounds(&self) -> Option<(f32, f32)> {
        Some((self.cfg.rated_lower_w, self.cfg.rated_upper_w))
    }

    fn effective_active_bounds(&self) -> Option<VecBounds> {
        let s = self.state.lock();
        let soc = VecBounds::single(s.effective_lower_w, s.effective_upper_w);
        drop(s);
        let aug = self.bounds.lock().effective();
        Some(soc.intersect(&aug))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::common::metrics::Bounds;

    fn charger() -> EvCharger {
        EvCharger::new(
            300,
            Duration::from_secs(1),
            EvChargerConfig {
                rated_lower_w: 0.0,
                rated_upper_w: 22_000.0,
                soc_protect_margin_pct: 0.0,
                command_delay: Duration::ZERO,
                ramp_rate_w_per_s: f32::INFINITY,
                ..Default::default()
            },
        )
    }

    /// Augmenting the active-power bounds tightens both the
    /// validation envelope and the telemetry-reported bounds. Before
    /// the override on `augment_active_bounds` the call silently
    /// dropped — the rated bounds stayed in effect and clients saw
    /// a setpoint they thought they'd narrowed go through.
    #[test]
    fn augment_active_bounds_narrows_validation_and_telemetry() {
        let w = World::new();
        let ev = charger();
        ev.augment_active_bounds(
            Utc::now(),
            VecBounds(vec![Bounds {
                lower: Some(0.0),
                upper: Some(5_000.0),
            }]),
            Duration::from_secs(60),
        );

        // Effective bounds now reflect the augmentation.
        let eff = ev.effective_active_bounds().unwrap();
        assert_eq!(eff.0.len(), 1);
        assert_eq!(eff.0[0].lower, Some(0.0));
        assert_eq!(eff.0[0].upper, Some(5_000.0));

        // A setpoint inside the augmented envelope still works.
        assert!(ev.set_active_setpoint(3_000.0).is_ok());
        ev.tick(&w, Utc::now(), Duration::from_millis(100));
        assert!((ev.aggregate_power_w(&w) - 3_000.0).abs() < 1.0);

        // A setpoint outside the augmented envelope is rejected even
        // though it's still inside rated.
        assert!(matches!(
            ev.set_active_setpoint(10_000.0),
            Err(SetpointError::OutOfBounds { .. })
        ));
    }

    /// Once the augmentation's lifetime elapses, `tick` reaps it and
    /// the rated bounds come back in full.
    #[test]
    fn augmentation_expires_and_rated_returns() {
        let w = World::new();
        let ev = charger();
        let t0 = Utc::now();
        ev.augment_active_bounds(
            t0,
            VecBounds(vec![Bounds {
                lower: Some(0.0),
                upper: Some(5_000.0),
            }]),
            Duration::from_millis(50),
        );

        // Pre-expiry: narrowed.
        let eff = ev.effective_active_bounds().unwrap();
        assert_eq!(eff.0[0].upper, Some(5_000.0));

        // Tick past the lifetime — `drop_expired` reaps inside tick.
        ev.tick(
            &w,
            t0 + chrono::Duration::milliseconds(100),
            Duration::from_millis(50),
        );

        let eff = ev.effective_active_bounds().unwrap();
        assert_eq!(eff.0[0].upper, Some(22_000.0));
    }
}
