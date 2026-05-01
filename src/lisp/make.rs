//! `(make-grid)`, `(make-meter)`, `(make-battery)`, … — the lisp DSL
//! for building the microgrid topology. Each constructor takes its
//! arguments as a typed plist via tulisp's `AsPlist!` macro and
//! returns a `ComponentHandle` (an opaque `Shared<dyn TulispAny>` on
//! the lisp side).

use std::str::FromStr;
use std::time::Duration;

use tulisp::{Alistable, AsAlist, AsPlist, Error, Plist, TulispContext};

use crate::lisp::label::LispLabel;
use crate::lisp::value::LispValue;
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
        health<":health">: Option<LispLabel> {= None},
        telemetry_mode<":telemetry-mode">: Option<LispLabel> {= None},
        command_mode<":command-mode">: Option<LispLabel> {= None},
        config<":config">: Option<LispValue> {= None},
    }
}

AsAlist! {
    /// Per-category defaults for `(make-grid)`. Mirrors `GridArgs`
    /// minus per-component identity / topology (`id`, `successors`).
    #[derive(Default)]
    pub struct GridDefaults {
        rated_fuse_current<"rated-fuse-current">: Option<i64> {= None},
        stream_jitter_pct<"stream-jitter-pct">: Option<f64> {= None},
        health: Option<LispLabel> {= None},
        telemetry_mode<"telemetry-mode">: Option<LispLabel> {= None},
        command_mode<"command-mode">: Option<LispLabel> {= None},
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
        health<":health">: Option<LispLabel> {= None},
        telemetry_mode<":telemetry-mode">: Option<LispLabel> {= None},
        command_mode<":command-mode">: Option<LispLabel> {= None},
        config<":config">: Option<LispValue> {= None},
    }
}

AsAlist! {
    /// Per-category defaults for `(make-meter)`. Mirrors `MeterArgs`
    /// minus per-component identity / topology (`id`, `successors`,
    /// `hidden`).
    #[derive(Default)]
    pub struct MeterDefaults {
        interval: Option<i64> {= None},
        power: Option<f64> {= None},
        reactive_power<"reactive-power">: Option<f64> {= None},
        stream_jitter_pct<"stream-jitter-pct">: Option<f64> {= None},
        health: Option<LispLabel> {= None},
        telemetry_mode<"telemetry-mode">: Option<LispLabel> {= None},
        command_mode<"command-mode">: Option<LispLabel> {= None},
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
        health<":health">: Option<LispLabel> {= None},
        telemetry_mode<":telemetry-mode">: Option<LispLabel> {= None},
        command_mode<":command-mode">: Option<LispLabel> {= None},
        config<":config">: Option<LispValue> {= None},
    }
}

AsAlist! {
    /// Per-category defaults for `(make-battery)`. Mirrors `BatteryArgs`
    /// minus the per-component identity / topology args (`id`,
    /// `successors`). Three-layer precedence in the make-battery defun:
    /// per-component plist > this alist > Rust struct default.
    #[derive(Default)]
    pub struct BatteryDefaults {
        interval: Option<i64> {= None},
        capacity_wh<"capacity">: Option<f64> {= None},
        initial_soc<"initial-soc">: Option<f64> {= None},
        soc_lower<"soc-lower">: Option<f64> {= None},
        soc_upper<"soc-upper">: Option<f64> {= None},
        voltage: Option<f64> {= None},
        rated_lower<"rated-lower">: Option<f64> {= None},
        rated_upper<"rated-upper">: Option<f64> {= None},
        soc_protect_margin<"soc-protect-margin">: Option<f64> {= None},
        stream_jitter_pct<"stream-jitter-pct">: Option<f64> {= None},
        health: Option<LispLabel> {= None},
        telemetry_mode<"telemetry-mode">: Option<LispLabel> {= None},
        command_mode<"command-mode">: Option<LispLabel> {= None},
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
        health<":health">: Option<LispLabel> {= None},
        telemetry_mode<":telemetry-mode">: Option<LispLabel> {= None},
        command_mode<":command-mode">: Option<LispLabel> {= None},
        /// PF-style Q cap: |Q| ≤ k × |P|. Pass nil to disable.
        reactive_pf_limit<":reactive-pf-limit">: Option<f64> {= None},
        /// kVA-style Q cap: P² + Q² ≤ apparent². Pass nil to disable.
        reactive_apparent_va<":reactive-apparent-va">: Option<f64> {= None},
        /// Inverter-internal latency before a Q setpoint starts
        /// being tracked. Defaults to 100 ms.
        reactive_command_delay_ms<":reactive-command-delay-ms">: Option<i64> {= None},
        /// Reactive slew rate (VAR/s). Default 2000 ≈ IEEE 1547-2018
        /// Cat B 5 s OLRT for a 10 kVAR window.
        reactive_ramp_rate<":reactive-ramp-rate">: Option<f64> {= None},
        config<":config">: Option<LispValue> {= None},
    }
}

AsAlist! {
    /// Per-category defaults for `(make-battery-inverter)`. Mirrors
    /// `BatteryInverterArgs` minus per-component identity / topology
    /// (`id`, `successors`).
    #[derive(Default)]
    pub struct BatteryInverterDefaults {
        interval: Option<i64> {= None},
        rated_lower<"rated-lower">: Option<f64> {= None},
        rated_upper<"rated-upper">: Option<f64> {= None},
        command_delay_ms<"command-delay-ms">: Option<i64> {= None},
        ramp_rate<"ramp-rate">: Option<f64> {= None},
        stream_jitter_pct<"stream-jitter-pct">: Option<f64> {= None},
        health: Option<LispLabel> {= None},
        telemetry_mode<"telemetry-mode">: Option<LispLabel> {= None},
        command_mode<"command-mode">: Option<LispLabel> {= None},
        reactive_pf_limit<"reactive-pf-limit">: Option<f64> {= None},
        reactive_apparent_va<"reactive-apparent-va">: Option<f64> {= None},
        reactive_command_delay_ms<"reactive-command-delay-ms">: Option<i64> {= None},
        reactive_ramp_rate<"reactive-ramp-rate">: Option<f64> {= None},
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
        health<":health">: Option<LispLabel> {= None},
        telemetry_mode<":telemetry-mode">: Option<LispLabel> {= None},
        command_mode<":command-mode">: Option<LispLabel> {= None},
        /// PF-style Q cap: |Q| ≤ k × |P|. Pass 0 to disable.
        reactive_pf_limit<":reactive-pf-limit">: Option<f64> {= None},
        /// kVA-style Q cap: P² + Q² ≤ apparent². Pass 0 to disable.
        reactive_apparent_va<":reactive-apparent-va">: Option<f64> {= None},
        /// Inverter-internal latency before a Q setpoint starts being
        /// tracked. Defaults to 100 ms.
        reactive_command_delay_ms<":reactive-command-delay-ms">: Option<i64> {= None},
        /// Reactive slew rate (VAR/s). Default 2000.
        reactive_ramp_rate<":reactive-ramp-rate">: Option<f64> {= None},
        config<":config">: Option<LispValue> {= None},
    }
}

AsAlist! {
    /// Per-category defaults for `(make-solar-inverter)`. Mirrors
    /// `SolarInverterArgs` minus per-component `id`.
    #[derive(Default)]
    pub struct SolarInverterDefaults {
        interval: Option<i64> {= None},
        sunlight_pct<"sunlight%">: Option<f64> {= None},
        rated_lower<"rated-lower">: Option<f64> {= None},
        rated_upper<"rated-upper">: Option<f64> {= None},
        command_delay_ms<"command-delay-ms">: Option<i64> {= None},
        ramp_rate<"ramp-rate">: Option<f64> {= None},
        stream_jitter_pct<"stream-jitter-pct">: Option<f64> {= None},
        health: Option<LispLabel> {= None},
        telemetry_mode<"telemetry-mode">: Option<LispLabel> {= None},
        command_mode<"command-mode">: Option<LispLabel> {= None},
        reactive_pf_limit<"reactive-pf-limit">: Option<f64> {= None},
        reactive_apparent_va<"reactive-apparent-va">: Option<f64> {= None},
        reactive_command_delay_ms<"reactive-command-delay-ms">: Option<i64> {= None},
        reactive_ramp_rate<"reactive-ramp-rate">: Option<f64> {= None},
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
        health<":health">: Option<LispLabel> {= None},
        telemetry_mode<":telemetry-mode">: Option<LispLabel> {= None},
        command_mode<":command-mode">: Option<LispLabel> {= None},
    }
}

// -----------------------------------------------------------------------------
// make-chp
// -----------------------------------------------------------------------------

AsPlist! {
    pub struct ChpArgs {
        id: Option<i64> {= None},
        stream_jitter_pct<":stream-jitter-pct">: Option<f64> {= None},
        health<":health">: Option<LispLabel> {= None},
        telemetry_mode<":telemetry-mode">: Option<LispLabel> {= None},
        command_mode<":command-mode">: Option<LispLabel> {= None},
        config<":config">: Option<LispValue> {= None},
    }
}

AsAlist! {
    /// Per-category defaults for `(make-chp)`. Mirrors `ChpArgs` minus
    /// per-component `id`.
    #[derive(Default)]
    pub struct ChpDefaults {
        stream_jitter_pct<"stream-jitter-pct">: Option<f64> {= None},
        health: Option<LispLabel> {= None},
        telemetry_mode<"telemetry-mode">: Option<LispLabel> {= None},
        command_mode<"command-mode">: Option<LispLabel> {= None},
    }
}

// -----------------------------------------------------------------------------
// Registration
// -----------------------------------------------------------------------------

pub fn register(ctx: &mut TulispContext, world: World) {
    let w = world.clone();
    ctx.defun(
        "make-grid",
        move |ctx: &mut TulispContext, args: Plist<GridArgs>| {
            let a = args.into_inner();
            let d = parse_defaults::<GridDefaults>(ctx, a.config.as_ref())?;
            let id = id_or_next(&w, a.id);
            let grid = Grid::new(
                id,
                a.rated_fuse_current.or(d.rated_fuse_current).unwrap_or(0) as u32,
                a.stream_jitter_pct
                    .or(d.stream_jitter_pct)
                    .unwrap_or(0.0) as f32,
            );
            let h = register_with_modes(
                &w,
                grid,
                a.health.as_ref().or(d.health.as_ref()),
                a.telemetry_mode.as_ref().or(d.telemetry_mode.as_ref()),
                a.command_mode.as_ref().or(d.command_mode.as_ref()),
            )?;
            connect_successors(&w, id, &a.successors);
            Ok::<_, Error>(h)
        },
    );

    let w = world.clone();
    ctx.defun(
        "make-meter",
        move |ctx: &mut TulispContext, args: Plist<MeterArgs>| {
            let a = args.into_inner();
            let d = parse_defaults::<MeterDefaults>(ctx, a.config.as_ref())?;
            let id = id_or_next(&w, a.id);
            let interval = ms_to_duration(a.interval.or(d.interval), 1000);
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
                a.power.or(d.power).map(|p| p as f32),
                a.stream_jitter_pct
                    .or(d.stream_jitter_pct)
                    .unwrap_or(0.0) as f32,
                hidden,
            );
            let h = register_with_modes(
                &w,
                meter,
                a.health.as_ref().or(d.health.as_ref()),
                a.telemetry_mode.as_ref().or(d.telemetry_mode.as_ref()),
                a.command_mode.as_ref().or(d.command_mode.as_ref()),
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
        },
    );

    let w = world.clone();
    ctx.defun(
        "make-battery",
        move |ctx: &mut TulispContext, args: Plist<BatteryArgs>| {
            let a = args.into_inner();
            let d = parse_defaults::<BatteryDefaults>(ctx, a.config.as_ref())?;
            let id = id_or_next(&w, a.id);
            let interval = ms_to_duration(a.interval.or(d.interval), 1000);
            let mut cfg = BatteryConfig::default();
            if let Some(v) = a.capacity_wh.or(d.capacity_wh) {
                cfg.capacity_wh = v as f32;
            }
            if let Some(v) = a.initial_soc.or(d.initial_soc) {
                cfg.initial_soc_pct = v as f32;
            }
            if let Some(v) = a.soc_lower.or(d.soc_lower) {
                cfg.soc_lower_pct = v as f32;
            }
            if let Some(v) = a.soc_upper.or(d.soc_upper) {
                cfg.soc_upper_pct = v as f32;
            }
            if let Some(v) = a.voltage.or(d.voltage) {
                cfg.voltage_v = v as f32;
            }
            if let Some(v) = a.rated_lower.or(d.rated_lower) {
                cfg.rated_lower_w = v as f32;
            }
            if let Some(v) = a.rated_upper.or(d.rated_upper) {
                cfg.rated_upper_w = v as f32;
            }
            if let Some(v) = a.soc_protect_margin.or(d.soc_protect_margin) {
                cfg.soc_protect_margin_pct = v as f32;
            }
            if let Some(v) = a.stream_jitter_pct.or(d.stream_jitter_pct) {
                cfg.stream_jitter_pct = v as f32;
            }
            register_with_modes(
                &w,
                Battery::new(id, interval, cfg),
                a.health.as_ref().or(d.health.as_ref()),
                a.telemetry_mode.as_ref().or(d.telemetry_mode.as_ref()),
                a.command_mode.as_ref().or(d.command_mode.as_ref()),
            )
        },
    );

    let w = world.clone();
    ctx.defun(
        "make-battery-inverter",
        move |ctx: &mut TulispContext, args: Plist<BatteryInverterArgs>| {
            let a = args.into_inner();
            let d = parse_defaults::<BatteryInverterDefaults>(ctx, a.config.as_ref())?;
            let id = id_or_next(&w, a.id);
            let interval = ms_to_duration(a.interval.or(d.interval), 1000);
            let mut cfg = BatteryInverterConfig::default();
            if let Some(v) = a.rated_lower.or(d.rated_lower) {
                cfg.rated_lower_w = v as f32;
            }
            if let Some(v) = a.rated_upper.or(d.rated_upper) {
                cfg.rated_upper_w = v as f32;
            }
            if let Some(v) = a.command_delay_ms.or(d.command_delay_ms) {
                cfg.command_delay = Duration::from_millis(v.max(0) as u64);
            }
            if let Some(v) = a.ramp_rate.or(d.ramp_rate) {
                cfg.ramp_rate_w_per_s = v as f32;
            }
            if let Some(v) = a.stream_jitter_pct.or(d.stream_jitter_pct) {
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
            // without needing a third "mode" symbol. Each reactive arg
            // pulls from the per-component plist first, then the
            // category alist.
            let reactive_pf = a.reactive_pf_limit.or(d.reactive_pf_limit);
            let reactive_va = a.reactive_apparent_va.or(d.reactive_apparent_va);
            if reactive_pf.is_some() || reactive_va.is_some() {
                cfg.reactive = crate::sim::reactive::ReactiveCapability {
                    pf_limit: reactive_pf.and_then(|v| if v > 0.0 { Some(v as f32) } else { None }),
                    apparent_va: reactive_va
                        .and_then(|v| if v > 0.0 { Some(v as f32) } else { None }),
                };
            }
            if let Some(v) = a.reactive_command_delay_ms.or(d.reactive_command_delay_ms) {
                cfg.reactive_command_delay = Duration::from_millis(v.max(0) as u64);
            }
            if let Some(v) = a.reactive_ramp_rate.or(d.reactive_ramp_rate) {
                cfg.reactive_ramp_rate_var_per_s = v as f32;
            }
            let succ_ids: Vec<u64> = a
                .successors
                .as_ref()
                .map(|v| v.iter().map(|h| h.id()).collect())
                .unwrap_or_default();
            let h = register_with_modes(
                &w,
                BatteryInverter::new(id, interval, cfg, succ_ids),
                a.health.as_ref().or(d.health.as_ref()),
                a.telemetry_mode.as_ref().or(d.telemetry_mode.as_ref()),
                a.command_mode.as_ref().or(d.command_mode.as_ref()),
            )?;
            connect_successors(&w, id, &a.successors);
            Ok::<_, Error>(h)
        },
    );

    let w = world.clone();
    ctx.defun(
        "make-solar-inverter",
        move |ctx: &mut TulispContext, args: Plist<SolarInverterArgs>| {
            let a = args.into_inner();
            let d = parse_defaults::<SolarInverterDefaults>(ctx, a.config.as_ref())?;
            let id = id_or_next(&w, a.id);
            let interval = ms_to_duration(a.interval.or(d.interval), 1000);
            let mut cfg = SolarInverterConfig::default();
            if let Some(v) = a.sunlight_pct.or(d.sunlight_pct) {
                cfg.sunlight_pct = v as f32;
            }
            if let Some(v) = a.rated_lower.or(d.rated_lower) {
                cfg.rated_lower_w = v as f32;
            }
            if let Some(v) = a.rated_upper.or(d.rated_upper) {
                cfg.rated_upper_w = v as f32;
            }
            if let Some(v) = a.command_delay_ms.or(d.command_delay_ms) {
                cfg.command_delay = Duration::from_millis(v.max(0) as u64);
            }
            if let Some(v) = a.ramp_rate.or(d.ramp_rate) {
                cfg.ramp_rate_w_per_s = v as f32;
            }
            if let Some(v) = a.stream_jitter_pct.or(d.stream_jitter_pct) {
                cfg.stream_jitter_pct = v as f32;
            }
            // Same opt-in semantics as make-battery-inverter:
            // mentioning either reactive arg overrides the default
            // ReactiveCapability with both, treating 0.0 / negative as
            // "this constraint is disabled". Absent both → keep the
            // microsim-style PF=0.35 default. Each reactive arg pulls
            // from per-component plist first, then category alist.
            let reactive_pf = a.reactive_pf_limit.or(d.reactive_pf_limit);
            let reactive_va = a.reactive_apparent_va.or(d.reactive_apparent_va);
            if reactive_pf.is_some() || reactive_va.is_some() {
                cfg.reactive = crate::sim::reactive::ReactiveCapability {
                    pf_limit: reactive_pf.and_then(|v| if v > 0.0 { Some(v as f32) } else { None }),
                    apparent_va: reactive_va
                        .and_then(|v| if v > 0.0 { Some(v as f32) } else { None }),
                };
            }
            if let Some(v) = a.reactive_command_delay_ms.or(d.reactive_command_delay_ms) {
                cfg.reactive_command_delay = Duration::from_millis(v.max(0) as u64);
            }
            if let Some(v) = a.reactive_ramp_rate.or(d.reactive_ramp_rate) {
                cfg.reactive_ramp_rate_var_per_s = v as f32;
            }
            register_with_modes(
                &w,
                SolarInverter::new(id, interval, cfg),
                a.health.as_ref().or(d.health.as_ref()),
                a.telemetry_mode.as_ref().or(d.telemetry_mode.as_ref()),
                a.command_mode.as_ref().or(d.command_mode.as_ref()),
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
            a.health.as_ref(),
            a.telemetry_mode.as_ref(),
            a.command_mode.as_ref(),
        )
    });

    let w = world;
    ctx.defun(
        "make-chp",
        move |ctx: &mut TulispContext, args: Plist<ChpArgs>| {
            let a = args.into_inner();
            let d = parse_defaults::<ChpDefaults>(ctx, a.config.as_ref())?;
            let id = id_or_next(&w, a.id);
            let jitter = a
                .stream_jitter_pct
                .or(d.stream_jitter_pct)
                .unwrap_or(0.0) as f32;
            register_with_modes(
                &w,
                Chp::new(id, jitter),
                a.health.as_ref().or(d.health.as_ref()),
                a.telemetry_mode.as_ref().or(d.telemetry_mode.as_ref()),
                a.command_mode.as_ref().or(d.command_mode.as_ref()),
            )
        },
    );
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

/// Decode the optional `:config` plist arg into a per-category
/// defaults struct. Returns the struct's `Default` when no `:config`
/// was given, so callers can always do `a.field.or(d.field)`.
///
/// `D` must be a struct deriving `Alistable` + `Default`. Microsim's
/// `battery-defaults` / `inverter-defaults` etc. each get one such
/// struct in the `AsAlist!` blocks above.
fn parse_defaults<D: Alistable + Default>(
    ctx: &mut TulispContext,
    config: Option<&LispValue>,
) -> Result<D, Error> {
    match config {
        Some(v) => D::from_alist(ctx, v.as_inner()),
        None => Ok(D::default()),
    }
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
    health: Option<&LispLabel>,
    telemetry: Option<&LispLabel>,
    command: Option<&LispLabel>,
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
    health: Option<&LispLabel>,
    telemetry: Option<&LispLabel>,
    command: Option<&LispLabel>,
) -> Result<(), Error> {
    if let Some(h) = health {
        let parsed = Health::from_str(h.as_str()).map_err(|_| {
            Error::invalid_argument(format!("unknown :health '{h}'; expected ok/error/standby"))
        })?;
        world.set_health(id, parsed);
    }
    if let Some(t) = telemetry {
        let parsed = TelemetryMode::from_str(t.as_str()).map_err(|_| {
            Error::invalid_argument(format!(
                "unknown :telemetry-mode '{t}'; expected normal/silent/closed"
            ))
        })?;
        world.set_telemetry_mode(id, parsed);
    }
    if let Some(c) = command {
        let parsed = CommandMode::from_str(c.as_str()).map_err(|_| {
            Error::invalid_argument(format!(
                "unknown :command-mode '{c}'; expected normal/timeout/error"
            ))
        })?;
        world.set_command_mode(id, parsed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a context wired to a fresh World, evaluates `src`, and
    /// returns the World so the test can introspect what got registered.
    fn run(src: &str) -> World {
        let world = World::new();
        let mut ctx = TulispContext::new();
        crate::lisp::handle::register(&mut ctx);
        register(&mut ctx, world.clone());
        ctx.eval_string(src).expect("eval lisp source");
        world
    }

    #[test]
    fn battery_defaults_alone_apply() {
        let world = run(
            r#"(setq d '((capacity . 50000.0)
                         (initial-soc . 20.0)
                         (rated-lower . -8000.0)
                         (rated-upper . 8000.0)))
               (make-battery :id 100 :config d)"#,
        );
        let t = world.get(100).unwrap().telemetry(&world);
        assert_eq!(t.capacity_wh, Some(50_000.0));
        assert!((t.soc_pct.unwrap() - 20.0).abs() < 1e-3);
    }

    #[test]
    fn battery_per_component_overrides_defaults() {
        // :capacity in the plist wins over (capacity . X) in the alist;
        // initial-soc only in defaults still applies.
        let world = run(
            r#"(setq d '((capacity . 50000.0) (initial-soc . 20.0)))
               (make-battery :id 101 :config d :capacity 25000.0)"#,
        );
        let t = world.get(101).unwrap().telemetry(&world);
        assert_eq!(t.capacity_wh, Some(25_000.0));
        assert!((t.soc_pct.unwrap() - 20.0).abs() < 1e-3);
    }

    #[test]
    fn battery_defaults_accept_symbol_for_health() {
        // :health in the alist as a bare symbol, no plist override.
        // Component starts in `error` health; gRPC layer would reject
        // setpoints, but we just verify the runtime knob landed.
        let world = run(
            r#"(setq d '((health . error)))
               (make-battery :id 102 :config d)"#,
        );
        assert_eq!(
            world.runtime_of(102).health,
            crate::sim::runtime::Health::Error
        );
    }

    #[test]
    fn battery_no_config_is_unchanged() {
        // Sanity: with no :config the defaults struct is empty, so
        // the BatteryConfig::default values stand.
        let world = run("(make-battery :id 103)");
        let t = world.get(103).unwrap().telemetry(&world);
        // Default capacity from BatteryConfig::default in battery.rs.
        assert_eq!(t.capacity_wh, Some(92_000.0));
    }
}
