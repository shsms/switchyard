//! `(make-grid)`, `(make-meter)`, `(make-battery)`, … — the lisp DSL
//! for building the microgrid topology. Each constructor takes its
//! arguments as a typed plist via tulisp's `AsPlist!` macro and
//! returns a `ComponentHandle` (an opaque `Shared<dyn TulispAny>` on
//! the lisp side).

use std::str::FromStr;
use std::time::Duration;

use tulisp::{AsPlist, Error, Plist, TulispContext};

use crate::sim::{
    Battery, BatteryInverter, Chp, ComponentHandle, EvCharger, Grid, Meter, SolarInverter, World,
    battery::BatteryConfig,
    ev_charger::EvChargerConfig,
    inverter::battery_inverter::BatteryInverterConfig,
    inverter::solar_inverter::SolarInverterConfig,
    runtime::{CommandMode, Health, TelemetryMode},
};

// -----------------------------------------------------------------------------
// make-grid
// -----------------------------------------------------------------------------

AsPlist! {
    pub struct GridArgs {
        id: Option<i64> {= None},
        rated_fuse_current<":rated-fuse-current">: Option<i64> {= None},
        successors: Option<Vec<ComponentHandle>> {= None},
        stream_jitter_pct<":stream-jitter-pct">: Option<f64> {= None},
        health<":health">: Option<String> {= None},
        telemetry_mode<":telemetry-mode">: Option<String> {= None},
        command_mode<":command-mode">: Option<String> {= None},
    }
}

// -----------------------------------------------------------------------------
// make-meter
// -----------------------------------------------------------------------------

AsPlist! {
    pub struct MeterArgs {
        id: Option<i64> {= None},
        interval: Option<i64> {= None},
        power: Option<f64> {= None},
        successors: Option<Vec<ComponentHandle>> {= None},
        hidden: Option<bool> {= None},
        reactive_power<":reactive-power">: Option<f64> {= None},
        stream_jitter_pct<":stream-jitter-pct">: Option<f64> {= None},
        health<":health">: Option<String> {= None},
        telemetry_mode<":telemetry-mode">: Option<String> {= None},
        command_mode<":command-mode">: Option<String> {= None},
    }
}

// -----------------------------------------------------------------------------
// make-battery
// -----------------------------------------------------------------------------

AsPlist! {
    pub struct BatteryArgs {
        id: Option<i64> {= None},
        interval: Option<i64> {= None},
        capacity_wh<":capacity">: Option<f64> {= None},
        initial_soc<":initial-soc">: Option<f64> {= None},
        soc_lower<":soc-lower">: Option<f64> {= None},
        soc_upper<":soc-upper">: Option<f64> {= None},
        voltage: Option<f64> {= None},
        rated_lower<":rated-lower">: Option<f64> {= None},
        rated_upper<":rated-upper">: Option<f64> {= None},
        soc_protect_margin<":soc-protect-margin">: Option<f64> {= None},
        stream_jitter_pct<":stream-jitter-pct">: Option<f64> {= None},
        health<":health">: Option<String> {= None},
        telemetry_mode<":telemetry-mode">: Option<String> {= None},
        command_mode<":command-mode">: Option<String> {= None},
    }
}

// -----------------------------------------------------------------------------
// make-battery-inverter
// -----------------------------------------------------------------------------

AsPlist! {
    pub struct BatteryInverterArgs {
        id: Option<i64> {= None},
        interval: Option<i64> {= None},
        successors: Option<Vec<ComponentHandle>> {= None},
        rated_lower<":rated-lower">: Option<f64> {= None},
        rated_upper<":rated-upper">: Option<f64> {= None},
        command_delay_ms<":command-delay-ms">: Option<i64> {= None},
        ramp_rate<":ramp-rate">: Option<f64> {= None},
        stream_jitter_pct<":stream-jitter-pct">: Option<f64> {= None},
        health<":health">: Option<String> {= None},
        telemetry_mode<":telemetry-mode">: Option<String> {= None},
        command_mode<":command-mode">: Option<String> {= None},
        /// PF-style Q cap: |Q| ≤ k × |P|. Pass nil to disable.
        reactive_pf_limit<":reactive-pf-limit">: Option<f64> {= None},
        /// kVA-style Q cap: P² + Q² ≤ apparent². Pass nil to disable.
        reactive_apparent_va<":reactive-apparent-va">: Option<f64> {= None},
    }
}

// -----------------------------------------------------------------------------
// make-solar-inverter
// -----------------------------------------------------------------------------

AsPlist! {
    pub struct SolarInverterArgs {
        id: Option<i64> {= None},
        interval: Option<i64> {= None},
        sunlight_pct<":sunlight%">: Option<f64> {= None},
        rated_lower<":rated-lower">: Option<f64> {= None},
        rated_upper<":rated-upper">: Option<f64> {= None},
        command_delay_ms<":command-delay-ms">: Option<i64> {= None},
        ramp_rate<":ramp-rate">: Option<f64> {= None},
        stream_jitter_pct<":stream-jitter-pct">: Option<f64> {= None},
        health<":health">: Option<String> {= None},
        telemetry_mode<":telemetry-mode">: Option<String> {= None},
        command_mode<":command-mode">: Option<String> {= None},
        /// PF-style Q cap: |Q| ≤ k × |P|. Pass 0 to disable.
        reactive_pf_limit<":reactive-pf-limit">: Option<f64> {= None},
        /// kVA-style Q cap: P² + Q² ≤ apparent². Pass 0 to disable.
        reactive_apparent_va<":reactive-apparent-va">: Option<f64> {= None},
    }
}

// -----------------------------------------------------------------------------
// make-ev-charger
// -----------------------------------------------------------------------------

AsPlist! {
    pub struct EvChargerArgs {
        id: Option<i64> {= None},
        interval: Option<i64> {= None},
        rated_lower<":rated-lower">: Option<f64> {= None},
        rated_upper<":rated-upper">: Option<f64> {= None},
        initial_soc<":initial-soc">: Option<f64> {= None},
        soc_lower<":soc-lower">: Option<f64> {= None},
        soc_upper<":soc-upper">: Option<f64> {= None},
        soc_protect_margin<":soc-protect-margin">: Option<f64> {= None},
        capacity_wh<":capacity">: Option<f64> {= None},
        command_delay_ms<":command-delay-ms">: Option<i64> {= None},
        ramp_rate<":ramp-rate">: Option<f64> {= None},
        stream_jitter_pct<":stream-jitter-pct">: Option<f64> {= None},
        health<":health">: Option<String> {= None},
        telemetry_mode<":telemetry-mode">: Option<String> {= None},
        command_mode<":command-mode">: Option<String> {= None},
    }
}

// -----------------------------------------------------------------------------
// make-chp
// -----------------------------------------------------------------------------

AsPlist! {
    pub struct ChpArgs {
        id: Option<i64> {= None},
        stream_jitter_pct<":stream-jitter-pct">: Option<f64> {= None},
        health<":health">: Option<String> {= None},
        telemetry_mode<":telemetry-mode">: Option<String> {= None},
        command_mode<":command-mode">: Option<String> {= None},
    }
}

// -----------------------------------------------------------------------------
// Registration
// -----------------------------------------------------------------------------

pub fn register(ctx: &mut TulispContext, world: World) {
    let w = world.clone();
    ctx.defun("make-grid", move |args: Plist<GridArgs>| {
        let a = args.into_inner();
        let id = id_or_next(&w, a.id);
        let grid = Grid::new(
            id,
            a.rated_fuse_current.unwrap_or(0) as u32,
            a.stream_jitter_pct.unwrap_or(0.0) as f32,
        );
        let h = register_with_modes(
            &w,
            grid,
            a.health.as_deref(),
            a.telemetry_mode.as_deref(),
            a.command_mode.as_deref(),
        )?;
        connect_successors(&w, id, &a.successors);
        Ok::<_, Error>(h)
    });

    let w = world.clone();
    ctx.defun("make-meter", move |args: Plist<MeterArgs>| {
        let a = args.into_inner();
        let id = id_or_next(&w, a.id);
        let interval = ms_to_duration(a.interval, 1000);
        let succ_ids: Vec<u64> = a
            .successors
            .as_ref()
            .map(|v| v.iter().map(|h| h.id()).collect())
            .unwrap_or_default();
        let hidden = a.hidden.unwrap_or(false);
        let meter = Meter::new(
            id,
            interval,
            succ_ids,
            a.power.map(|p| p as f32),
            a.stream_jitter_pct.unwrap_or(0.0) as f32,
            hidden,
        );
        let h = register_with_modes(
            &w,
            meter,
            a.health.as_deref(),
            a.telemetry_mode.as_deref(),
            a.command_mode.as_deref(),
        )?;
        // Hidden meters: their *outgoing* edges (to children) are
        // suppressed too, mirroring microsim. The handle-side filter
        // in connect_successors elsewhere skips edges *into* hidden
        // children — so a hidden meter is invisible in both directions
        // while still aggregating into its parent.
        if !hidden {
            connect_successors(&w, id, &a.successors);
        }
        Ok::<_, Error>(h)
    });

    let w = world.clone();
    ctx.defun("make-battery", move |args: Plist<BatteryArgs>| {
        let a = args.into_inner();
        let id = id_or_next(&w, a.id);
        let interval = ms_to_duration(a.interval, 1000);
        let mut cfg = BatteryConfig::default();
        if let Some(v) = a.capacity_wh {
            cfg.capacity_wh = v as f32;
        }
        if let Some(v) = a.initial_soc {
            cfg.initial_soc_pct = v as f32;
        }
        if let Some(v) = a.soc_lower {
            cfg.soc_lower_pct = v as f32;
        }
        if let Some(v) = a.soc_upper {
            cfg.soc_upper_pct = v as f32;
        }
        if let Some(v) = a.voltage {
            cfg.voltage_v = v as f32;
        }
        if let Some(v) = a.rated_lower {
            cfg.rated_lower_w = v as f32;
        }
        if let Some(v) = a.rated_upper {
            cfg.rated_upper_w = v as f32;
        }
        if let Some(v) = a.soc_protect_margin {
            cfg.soc_protect_margin_pct = v as f32;
        }
        if let Some(v) = a.stream_jitter_pct {
            cfg.stream_jitter_pct = v as f32;
        }
        register_with_modes(
            &w,
            Battery::new(id, interval, cfg),
            a.health.as_deref(),
            a.telemetry_mode.as_deref(),
            a.command_mode.as_deref(),
        )
    });

    let w = world.clone();
    ctx.defun(
        "make-battery-inverter",
        move |args: Plist<BatteryInverterArgs>| {
            let a = args.into_inner();
            let id = id_or_next(&w, a.id);
            let interval = ms_to_duration(a.interval, 1000);
            let mut cfg = BatteryInverterConfig::default();
            if let Some(v) = a.rated_lower {
                cfg.rated_lower_w = v as f32;
            }
            if let Some(v) = a.rated_upper {
                cfg.rated_upper_w = v as f32;
            }
            if let Some(v) = a.command_delay_ms {
                cfg.command_delay = Duration::from_millis(v.max(0) as u64);
            }
            if let Some(v) = a.ramp_rate {
                cfg.ramp_rate_w_per_s = v as f32;
            }
            if let Some(v) = a.stream_jitter_pct {
                cfg.stream_jitter_pct = v as f32;
            }
            // Reactive capability semantics:
            //   neither arg present  → microsim-compatible PF=0.35 default
            //   either arg present   → override the default with both
            //                          (the unspecified one is "disabled")
            //   value ≤ 0.0          → that constraint is disabled
            // This lets the user write
            //   :reactive-apparent-va 32000.0     ;; kVA only
            //   :reactive-pf-limit 0.0 :reactive-apparent-va 0.0 ;; unrestricted
            //   :reactive-pf-limit 0.5            ;; tighter PF, no kVA
            // without needing a third "mode" symbol.
            if a.reactive_pf_limit.is_some() || a.reactive_apparent_va.is_some() {
                cfg.reactive = crate::sim::reactive::ReactiveCapability {
                    pf_limit: a
                        .reactive_pf_limit
                        .and_then(|v| if v > 0.0 { Some(v as f32) } else { None }),
                    apparent_va: a
                        .reactive_apparent_va
                        .and_then(|v| if v > 0.0 { Some(v as f32) } else { None }),
                };
            }
            let succ_ids: Vec<u64> = a
                .successors
                .as_ref()
                .map(|v| v.iter().map(|h| h.id()).collect())
                .unwrap_or_default();
            let h = register_with_modes(
                &w,
                BatteryInverter::new(id, interval, cfg, succ_ids),
                a.health.as_deref(),
                a.telemetry_mode.as_deref(),
                a.command_mode.as_deref(),
            )?;
            connect_successors(&w, id, &a.successors);
            Ok::<_, Error>(h)
        },
    );

    let w = world.clone();
    ctx.defun(
        "make-solar-inverter",
        move |args: Plist<SolarInverterArgs>| {
            let a = args.into_inner();
            let id = id_or_next(&w, a.id);
            let interval = ms_to_duration(a.interval, 1000);
            let mut cfg = SolarInverterConfig::default();
            if let Some(v) = a.sunlight_pct {
                cfg.sunlight_pct = v as f32;
            }
            if let Some(v) = a.rated_lower {
                cfg.rated_lower_w = v as f32;
            }
            if let Some(v) = a.rated_upper {
                cfg.rated_upper_w = v as f32;
            }
            if let Some(v) = a.command_delay_ms {
                cfg.command_delay = Duration::from_millis(v.max(0) as u64);
            }
            if let Some(v) = a.ramp_rate {
                cfg.ramp_rate_w_per_s = v as f32;
            }
            if let Some(v) = a.stream_jitter_pct {
                cfg.stream_jitter_pct = v as f32;
            }
            // Same opt-in semantics as make-battery-inverter:
            // mentioning either reactive arg overrides the default
            // ReactiveCapability with both, treating 0.0 / negative as
            // "this constraint is disabled". Absent both → keep the
            // microsim-style PF=0.35 default.
            if a.reactive_pf_limit.is_some() || a.reactive_apparent_va.is_some() {
                cfg.reactive = crate::sim::reactive::ReactiveCapability {
                    pf_limit: a
                        .reactive_pf_limit
                        .and_then(|v| if v > 0.0 { Some(v as f32) } else { None }),
                    apparent_va: a
                        .reactive_apparent_va
                        .and_then(|v| if v > 0.0 { Some(v as f32) } else { None }),
                };
            }
            register_with_modes(
                &w,
                SolarInverter::new(id, interval, cfg),
                a.health.as_deref(),
                a.telemetry_mode.as_deref(),
                a.command_mode.as_deref(),
            )
        },
    );

    let w = world.clone();
    ctx.defun("make-ev-charger", move |args: Plist<EvChargerArgs>| {
        let a = args.into_inner();
        let id = id_or_next(&w, a.id);
        let interval = ms_to_duration(a.interval, 1000);
        let mut cfg = EvChargerConfig::default();
        if let Some(v) = a.rated_lower {
            cfg.rated_lower_w = v as f32;
        }
        if let Some(v) = a.rated_upper {
            cfg.rated_upper_w = v as f32;
        }
        if let Some(v) = a.initial_soc {
            cfg.initial_soc_pct = v as f32;
        }
        if let Some(v) = a.soc_lower {
            cfg.soc_lower_pct = v as f32;
        }
        if let Some(v) = a.soc_upper {
            cfg.soc_upper_pct = v as f32;
        }
        if let Some(v) = a.soc_protect_margin {
            cfg.soc_protect_margin_pct = v as f32;
        }
        if let Some(v) = a.capacity_wh {
            cfg.capacity_wh = v as f32;
        }
        if let Some(v) = a.command_delay_ms {
            cfg.command_delay = Duration::from_millis(v.max(0) as u64);
        }
        if let Some(v) = a.ramp_rate {
            cfg.ramp_rate_w_per_s = v as f32;
        }
        if let Some(v) = a.stream_jitter_pct {
            cfg.stream_jitter_pct = v as f32;
        }
        register_with_modes(
            &w,
            EvCharger::new(id, interval, cfg),
            a.health.as_deref(),
            a.telemetry_mode.as_deref(),
            a.command_mode.as_deref(),
        )
    });

    let w = world;
    ctx.defun("make-chp", move |args: Plist<ChpArgs>| {
        let a = args.into_inner();
        let id = id_or_next(&w, a.id);
        let jitter = a.stream_jitter_pct.unwrap_or(0.0) as f32;
        register_with_modes(
            &w,
            Chp::new(id, jitter),
            a.health.as_deref(),
            a.telemetry_mode.as_deref(),
            a.command_mode.as_deref(),
        )
    });
}

fn connect_successors(world: &World, parent: u64, successors: &Option<Vec<ComponentHandle>>) {
    if let Some(list) = successors {
        for child in list {
            // Hidden children: aggregated into the parent (succ_ids
            // captures every successor) but excluded from the
            // connections graph that gRPC clients see.
            if child.is_hidden() {
                continue;
            }
            world.connect(parent, child.id());
        }
    }
}

fn ms_to_duration(ms: Option<i64>, default_ms: u64) -> Duration {
    Duration::from_millis(ms.map(|x| x.max(0) as u64).unwrap_or(default_ms))
}

/// Resolve the component id from an `:id` plist value, falling back to
/// `World::next_id()` when omitted. Centralized so casts stay one
/// place — each make-* used to inline the same `as u64 / next_id()`
/// pattern.
fn id_or_next(world: &World, explicit: Option<i64>) -> u64 {
    explicit
        .map(|x| x as u64)
        .unwrap_or_else(|| world.next_id())
}

/// Register a freshly-built component, then apply any initial runtime
/// mode args. Returns the handle so the caller can also wire
/// connections / `setq` it for cross-references.
///
/// Centralising this guarantees every make-* applies modes in the
/// same order (health, then telemetry, then command) right after
/// registration — before any tick or subscriber runs.
fn register_with_modes<C: crate::sim::SimulatedComponent + 'static>(
    world: &World,
    component: C,
    health: Option<&str>,
    telemetry: Option<&str>,
    command: Option<&str>,
) -> Result<ComponentHandle, Error> {
    let id = component.id();
    let h = world.register(component);
    apply_initial_modes(world, id, health, telemetry, command)?;
    Ok(h)
}

/// Apply initial runtime mode args from a plist constructor. Each
/// `make-*` calls this immediately after `world.register(...)` so a
/// component declared with `:health 'error` is broken from the very
/// first tick.
fn apply_initial_modes(
    world: &World,
    id: u64,
    health: Option<&str>,
    telemetry: Option<&str>,
    command: Option<&str>,
) -> Result<(), Error> {
    if let Some(h) = health {
        let h = Health::from_str(h).map_err(|_| {
            Error::invalid_argument(format!("unknown :health '{h}'; expected ok/error/standby"))
        })?;
        world.set_health(id, h);
    }
    if let Some(t) = telemetry {
        let t = TelemetryMode::from_str(t).map_err(|_| {
            Error::invalid_argument(format!(
                "unknown :telemetry-mode '{t}'; expected normal/silent/closed"
            ))
        })?;
        world.set_telemetry_mode(id, t);
    }
    if let Some(c) = command {
        let c = CommandMode::from_str(c).map_err(|_| {
            Error::invalid_argument(format!(
                "unknown :command-mode '{c}'; expected normal/timeout/error"
            ))
        })?;
        world.set_command_mode(id, c);
    }
    Ok(())
}
