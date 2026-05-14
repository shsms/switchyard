use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use crate::sim::{
    Category, SetpointError, SimulatedComponent, Telemetry, World,
    bounds::ComponentBounds,
    meter::{per_phase_apparent_current, split_per_phase},
    ramp::{CommandDelay, Ramp},
    reactive::{ReactiveCapability, ReactivePath},
    runtime::Health,
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
    /// SCADA / inverter-internal latency between accepting a Q
    /// setpoint and starting to track it. Real smart inverters take
    /// some milliseconds; default 100 ms.
    pub reactive_command_delay: Duration,
    /// Reactive slew rate (VAR/s). Sized to give an open-loop
    /// response time around 5 s when traversing a ~10 kVAR window —
    /// matches IEEE 1547-2018's Performance Category B default OLRT
    /// for Volt/VAR control. Use `f32::INFINITY` to disable.
    pub reactive_ramp_rate_var_per_s: f32,
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
            reactive_command_delay: Duration::from_millis(100),
            reactive_ramp_rate_var_per_s: 2000.0,
        }
    }
}

pub struct BatteryInverter {
    id: u64,
    name: String,
    interval: Duration,
    cfg: BatteryInverterConfig,
    bounds: Mutex<ComponentBounds>,
    delay: CommandDelay,
    ramp: Ramp,
    /// All reactive-side state — capability envelope, command-delay,
    /// slew-rate ramp, last published Q. See [`ReactivePath`].
    reactive: ReactivePath,
    /// What the children actually accepted last tick — the AC-side
    /// quantity a real inverter would publish on its telemetry bus.
    /// Differs from `ramp.actual()` whenever a battery's BMS clipped
    /// the share we pushed.
    measured_w: Mutex<f32>,
}

impl BatteryInverter {
    pub fn new(id: u64, interval: Duration, cfg: BatteryInverterConfig) -> Self {
        let bounds = ComponentBounds::rated(cfg.rated_lower_w, cfg.rated_upper_w);
        let delay = CommandDelay::new(cfg.command_delay);
        let ramp = Ramp::new(cfg.ramp_rate_w_per_s, 0.0);
        let reactive = ReactivePath::new(
            cfg.reactive,
            cfg.reactive_command_delay,
            cfg.reactive_ramp_rate_var_per_s,
        );
        Self {
            id,
            name: format!("inv-bat-{id}"),
            interval,
            cfg,
            bounds: Mutex::new(bounds),
            delay,
            ramp,
            reactive,
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

        // Active path: clamp the pending command against our OWN
        // bounds (no peeking at the children — the gateway gates
        // out-of-envelope setpoints upstream). If one slips through,
        // the children will refuse the excess via their own clamp
        // and the published battery telemetry will reveal the gap.
        if let Some(target) = self.delay.poll(now) {
            let own = self.bounds.lock().effective();
            self.ramp.set_target(own.clamp(target));
        }

        let p_live = *self.measured_w.lock();
        let commanded_p = self.ramp.advance(dt);
        // Reactive: validated when accepted, re-clamped to the live
        // envelope at p_live as the command is promoted, then slewed.
        let commanded_q = self.reactive.step(p_live, now, dt);

        // Distribute equal share among the *healthy* children. Failed
        // batteries (Health::Error / Standby) are skipped, so the
        // surviving siblings absorb the full commanded value. Each
        // child accumulates pushes additively over the tick, so an
        // MxN topology (N inverters → 1 bus → M batteries) settles to
        // the clamped sum of all parent pushes, not last-writer-wins.
        let healthy: Vec<u64> = world
            .children_of(self.id)
            .into_iter()
            .filter(|id| world.runtime_of(*id).health == Health::Ok)
            .collect();
        if healthy.is_empty() {
            // No child accepted the push → no AC output. Publishing the
            // commanded value would be a fiction: telemetry would say
            // "I delivered P W" when the bus is electrically inert.
            // The ramp state stays at commanded_p so the inverter
            // resumes delivering instantly if a child comes back
            // healthy on a later tick.
            *self.measured_w.lock() = 0.0;
            self.reactive.override_published(0.0);
        } else {
            let n = healthy.len() as f32;
            let p_share = commanded_p / n;
            let q_share = commanded_q / n;
            for id in &healthy {
                if let Some(child) = world.get(*id) {
                    child.set_dc_active_reactive(p_share, q_share);
                }
            }
            // The published value is what we *commanded* the (healthy)
            // children to take, not what any individual child clipped
            // to — battery telemetry separately exposes the accepted
            // value, so a SCADA client wanting to see saturation reads
            // both.
            *self.measured_w.lock() = commanded_p;
            self.reactive.override_published(commanded_q);
        }
    }

    fn telemetry(&self, world: &World) -> Telemetry {
        let grid = world.grid_state();
        // Report the measured AC output, not the internal ramp state —
        // those diverge when a battery clips downstream.
        let p = *self.measured_w.lock();
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
            // Reported envelope is OUR own bounds only — clients that
            // want the combined inverter+battery envelope read both
            // streams and intersect.
            active_power_bounds: Some(self.bounds.lock().effective()),
            // Reactive envelope is dynamic: tightens with |P| under
            // PF, expands toward the kVA edge when P is small.
            reactive_power_bounds: Some(self.reactive.bounds_at(p)),
            component_state: Some(power_state(p)),
            ..Default::default()
        }
    }

    fn set_active_setpoint(&self, power_w: f32) -> Result<(), SetpointError> {
        // We don't have a `&World` here (the trait method is per-component),
        // so children-summing happens in tick(). Validation here uses our
        // own (post-augmentation) bounds — anything beyond that is a hard
        // protocol error; the SoC clamp is enforced silently via tick().
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

    fn set_reactive_setpoint(&self, vars: f32) -> Result<(), SetpointError> {
        self.reactive.accept_setpoint(*self.measured_w.lock(), vars)
    }

    fn reset_setpoint(&self) {
        self.delay.reset();
        self.ramp.set_target(0.0);
        self.reactive.reset();
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

    fn aggregate_power_w(&self, _world: &World) -> f32 {
        *self.measured_w.lock()
    }

    fn aggregate_reactive_var(&self, _world: &World) -> f32 {
        self.reactive.published()
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
        Some(self.reactive.bounds_at(*self.measured_w.lock()))
    }

    fn set_reactive_pf_limit(&self, pf: Option<f32>) {
        self.reactive.set_pf_limit(pf);
    }

    fn set_reactive_apparent_va(&self, va: Option<f32>) {
        self.reactive.set_apparent_va(va);
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::common::metrics::Bounds;
    use crate::sim::bounds::VecBounds;
    use crate::sim::{Battery, battery::BatteryConfig};

    fn setup_inverter_with_battery() -> (World, u64, u64) {
        let w = World::new();
        let bat = Battery::new(
            100,
            Duration::from_secs(1),
            BatteryConfig {
                rated_lower_w: -10000.0,
                rated_upper_w: 10000.0,
                capacity_wh: 100_000.0,
                soc_protect_margin_pct: 0.0,
                ..Default::default()
            },
        );
        w.register(bat);
        let inv = BatteryInverter::new(
            200,
            Duration::from_secs(1),
            BatteryInverterConfig {
                rated_lower_w: -10000.0,
                rated_upper_w: 10000.0,
                command_delay: Duration::ZERO,
                ramp_rate_w_per_s: f32::INFINITY,
                ..Default::default()
            },
        );
        w.register(inv);
        w.connect(200, 100);
        (w, 100, 200)
    }

    /// A setpoint rejected because the live augmentation narrowed the
    /// envelope must surface the *augmented* bounds in the error, not
    /// the rated ones — a client that just installed the augmentation
    /// reads "out of [-5000, 5000]" and knows exactly which limit
    /// they're up against.
    #[test]
    fn out_of_bounds_error_reports_augmented_envelope() {
        let (_w, _bat_id, inv_id) = setup_inverter_with_battery();
        let w = World::new();
        let inv = BatteryInverter::new(
            inv_id,
            Duration::from_secs(1),
            BatteryInverterConfig {
                rated_lower_w: -10_000.0,
                rated_upper_w: 10_000.0,
                command_delay: Duration::ZERO,
                ramp_rate_w_per_s: f32::INFINITY,
                ..Default::default()
            },
        );
        w.register(inv);
        let inv = w.get(inv_id).unwrap();

        inv.augment_active_bounds(
            Utc::now(),
            VecBounds(vec![Bounds {
                lower: Some(-5_000.0),
                upper: Some(5_000.0),
            }]),
            Duration::from_secs(60),
        );

        let err = inv
            .set_active_setpoint(8_000.0)
            .expect_err("8 kW exceeds augmented envelope");
        match err {
            SetpointError::OutOfBounds { value, envelope } => {
                assert_eq!(value, 8_000.0);
                let b = envelope.0.first().expect("single-bucket envelope");
                assert_eq!(b.lower, Some(-5_000.0));
                assert_eq!(b.upper, Some(5_000.0));
            }
            other => panic!("expected OutOfBounds, got {other:?}"),
        }
    }

    /// When every downstream battery is unhealthy the inverter delivers
    /// nothing — telemetry has to mirror that, not the ramp's
    /// in-flight commanded value. Pre-fix the inverter published the
    /// commanded P even though no child accepted it.
    #[test]
    fn no_healthy_children_means_zero_published() {
        let (w, bat_id, inv_id) = setup_inverter_with_battery();
        let inv = w.get(inv_id).unwrap();

        inv.set_active_setpoint(3000.0).unwrap();
        inv.tick(&w, Utc::now(), Duration::from_millis(100));
        // Healthy children: commanded value is delivered + published.
        assert!((inv.aggregate_power_w(&w) - 3000.0).abs() < 1.0);

        w.set_health(bat_id, Health::Error);
        inv.tick(&w, Utc::now(), Duration::from_millis(100));
        assert!(
            inv.aggregate_power_w(&w).abs() < 1.0,
            "expected 0 W with no healthy children, got {}",
            inv.aggregate_power_w(&w),
        );
        // Reactive side mirrors the same rule.
        assert!(
            inv.aggregate_reactive_var(&w).abs() < 1.0,
            "expected 0 VAR with no healthy children, got {}",
            inv.aggregate_reactive_var(&w),
        );

        // Recovery: bring the battery back, the inverter resumes
        // delivering on the next tick (ramp.actual stayed at 3000).
        w.set_health(bat_id, Health::Ok);
        inv.tick(&w, Utc::now(), Duration::from_millis(100));
        assert!((inv.aggregate_power_w(&w) - 3000.0).abs() < 1.0);
    }
}
