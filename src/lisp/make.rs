//! `(%make-grid)`, `(%make-meter)`, `(%make-battery)`, … — the
//! Rust-side constructor primitives the lisp DSL dispatches to. Each
//! takes its arguments as a typed plist via tulisp's `AsPlist!` macro
//! and returns a `ComponentHandle` (an opaque `Shared<dyn TulispAny>`
//! on the lisp side). The user-facing names (`(make-grid)`,
//! `(make-meter)`, …) are `defun` wrappers in `sim/defaults.lisp`
//! that prepend `:config <cat>-defaults` before calling these
//! primitives.

use std::time::Duration;

use tulisp::{Alistable, AsAlist, AsPlist, Error, Plist, TulispContext};

use crate::lisp::value::LispValue;
use crate::sim::{
    Battery, BatteryInverter, Chp, ComponentHandle, EvCharger, Grid, Meter, SolarInverter, World,
    battery::BatteryConfig,
    dynamic_scalar::DynamicScalar,
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
        name: Option<String> {= None},
        rated_fuse_current<":rated-fuse-current">: Option<i64> {= None},
        successors: Option<Vec<ComponentHandle>> {= None},
        stream_jitter_pct<":stream-jitter-pct">: Option<f64> {= None},
        health<":health">: Option<Health> {= None},
        telemetry_mode<":telemetry-mode">: Option<TelemetryMode> {= None},
        command_mode<":command-mode">: Option<CommandMode> {= None},
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
        health: Option<Health> {= None},
        telemetry_mode<"telemetry-mode">: Option<TelemetryMode> {= None},
        command_mode<"command-mode">: Option<CommandMode> {= None},
    }
}

// -----------------------------------------------------------------------------
// make-meter
// -----------------------------------------------------------------------------

AsPlist! {
    pub struct MeterArgs {
        id: Option<i64> {= None},
        name: Option<String> {= None},
        interval: Option<i64> {= None},
        /// Constant, lambda, or symbol. Resolved into a
        /// [`DynamicScalar`] in the constructor — see
        /// [`crate::sim::dynamic_scalar::DynamicScalar::from_lisp`].
        power: Option<LispValue> {= None},
        successors: Option<Vec<ComponentHandle>> {= None},
        hidden: Option<bool> {= None},
        /// Mark this meter as the microgrid's main / point-of-
        /// common-coupling meter. The scenario reporter tracks its
        /// active-power peak; at most one meter per microgrid may
        /// carry the flag.
        main: Option<bool> {= None},
        stream_jitter_pct<":stream-jitter-pct">: Option<f64> {= None},
        health<":health">: Option<Health> {= None},
        telemetry_mode<":telemetry-mode">: Option<TelemetryMode> {= None},
        command_mode<":command-mode">: Option<CommandMode> {= None},
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
        stream_jitter_pct<"stream-jitter-pct">: Option<f64> {= None},
        health: Option<Health> {= None},
        telemetry_mode<"telemetry-mode">: Option<TelemetryMode> {= None},
        command_mode<"command-mode">: Option<CommandMode> {= None},
    }
}

// -----------------------------------------------------------------------------
// make-battery
// -----------------------------------------------------------------------------

AsPlist! {
    pub struct BatteryArgs {
        id: Option<i64> {= None},
        name: Option<String> {= None},
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
        health<":health">: Option<Health> {= None},
        telemetry_mode<":telemetry-mode">: Option<TelemetryMode> {= None},
        command_mode<":command-mode">: Option<CommandMode> {= None},
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
        health: Option<Health> {= None},
        telemetry_mode<"telemetry-mode">: Option<TelemetryMode> {= None},
        command_mode<"command-mode">: Option<CommandMode> {= None},
    }
}

// -----------------------------------------------------------------------------
// make-battery-inverter
// -----------------------------------------------------------------------------

AsPlist! {
    pub struct BatteryInverterArgs {
        id: Option<i64> {= None},
        name: Option<String> {= None},
        interval: Option<i64> {= None},
        successors: Option<Vec<ComponentHandle>> {= None},
        rated_lower<":rated-lower">: Option<f64> {= None},
        rated_upper<":rated-upper">: Option<f64> {= None},
        command_delay_ms<":command-delay-ms">: Option<i64> {= None},
        ramp_rate<":ramp-rate">: Option<f64> {= None},
        stream_jitter_pct<":stream-jitter-pct">: Option<f64> {= None},
        health<":health">: Option<Health> {= None},
        telemetry_mode<":telemetry-mode">: Option<TelemetryMode> {= None},
        command_mode<":command-mode">: Option<CommandMode> {= None},
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
        health: Option<Health> {= None},
        telemetry_mode<"telemetry-mode">: Option<TelemetryMode> {= None},
        command_mode<"command-mode">: Option<CommandMode> {= None},
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
        name: Option<String> {= None},
        interval: Option<i64> {= None},
        /// Cloud-cover percentage. May be a number, a lambda, or a
        /// symbol — see [`crate::sim::dynamic_scalar::DynamicScalar::from_lisp`].
        /// Resolved each tick via the scheduler's pre-tick hook.
        sunlight_pct<":sunlight%">: Option<LispValue> {= None},
        rated_lower<":rated-lower">: Option<f64> {= None},
        rated_upper<":rated-upper">: Option<f64> {= None},
        command_delay_ms<":command-delay-ms">: Option<i64> {= None},
        ramp_rate<":ramp-rate">: Option<f64> {= None},
        stream_jitter_pct<":stream-jitter-pct">: Option<f64> {= None},
        health<":health">: Option<Health> {= None},
        telemetry_mode<":telemetry-mode">: Option<TelemetryMode> {= None},
        command_mode<":command-mode">: Option<CommandMode> {= None},
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
        health: Option<Health> {= None},
        telemetry_mode<"telemetry-mode">: Option<TelemetryMode> {= None},
        command_mode<"command-mode">: Option<CommandMode> {= None},
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
        name: Option<String> {= None},
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
        health<":health">: Option<Health> {= None},
        telemetry_mode<":telemetry-mode">: Option<TelemetryMode> {= None},
        command_mode<":command-mode">: Option<CommandMode> {= None},
        config<":config">: Option<LispValue> {= None},
    }
}

AsAlist! {
    /// Per-category defaults for `(make-ev-charger)`. Mirrors
    /// `EvChargerArgs` minus per-component `id`.
    #[derive(Default)]
    pub struct EvChargerDefaults {
        interval: Option<i64> {= None},
        rated_lower<"rated-lower">: Option<f64> {= None},
        rated_upper<"rated-upper">: Option<f64> {= None},
        initial_soc<"initial-soc">: Option<f64> {= None},
        soc_lower<"soc-lower">: Option<f64> {= None},
        soc_upper<"soc-upper">: Option<f64> {= None},
        soc_protect_margin<"soc-protect-margin">: Option<f64> {= None},
        capacity_wh<"capacity">: Option<f64> {= None},
        command_delay_ms<"command-delay-ms">: Option<i64> {= None},
        ramp_rate<"ramp-rate">: Option<f64> {= None},
        stream_jitter_pct<"stream-jitter-pct">: Option<f64> {= None},
        health: Option<Health> {= None},
        telemetry_mode<"telemetry-mode">: Option<TelemetryMode> {= None},
        command_mode<"command-mode">: Option<CommandMode> {= None},
    }
}

// -----------------------------------------------------------------------------
// make-chp
// -----------------------------------------------------------------------------

AsPlist! {
    pub struct ChpArgs {
        id: Option<i64> {= None},
        name: Option<String> {= None},
        stream_jitter_pct<":stream-jitter-pct">: Option<f64> {= None},
        health<":health">: Option<Health> {= None},
        telemetry_mode<":telemetry-mode">: Option<TelemetryMode> {= None},
        command_mode<":command-mode">: Option<CommandMode> {= None},
        config<":config">: Option<LispValue> {= None},
    }
}

AsAlist! {
    /// Per-category defaults for `(make-chp)`. Mirrors `ChpArgs` minus
    /// per-component `id`.
    #[derive(Default)]
    pub struct ChpDefaults {
        stream_jitter_pct<"stream-jitter-pct">: Option<f64> {= None},
        health: Option<Health> {= None},
        telemetry_mode<"telemetry-mode">: Option<TelemetryMode> {= None},
        command_mode<"command-mode">: Option<CommandMode> {= None},
    }
}

// -----------------------------------------------------------------------------
// Registration
// -----------------------------------------------------------------------------

pub fn register(ctx: &mut TulispContext, world: World) {
    let w = world.clone();
    ctx.defun(
        "%make-grid",
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
                a.health.or(d.health),
                a.telemetry_mode.or(d.telemetry_mode),
                a.command_mode.or(d.command_mode),
            )?;
            apply_initial_name(&w, id, a.name);
            connect_successors(&w, id, &a.successors);
            Ok::<_, Error>(h)
        },
    );

    let w = world.clone();
    ctx.defun(
        "%make-meter",
        move |ctx: &mut TulispContext, args: Plist<MeterArgs>| {
            let a = args.into_inner();
            let d = parse_defaults::<MeterDefaults>(ctx, a.config.as_ref())?;
            let id = id_or_next(&w, a.id);
            let interval = ms_to_duration(a.interval.or(d.interval), 1000);
            let hidden = a.hidden.unwrap_or(false);
            // Cache only the children that won't end up in
            // World::connections — the visible ones flow through
            // connect_successors below. For a hidden parent
            // connect_successors gets skipped entirely, so every
            // child is "hidden" from the graph's perspective.
            let cached_succ_ids: Vec<u64> = a
                .successors
                .as_ref()
                .map(|v| {
                    v.iter()
                        .filter(|h| hidden || h.is_hidden())
                        .map(|h| h.id())
                        .collect()
                })
                .unwrap_or_default();
            // :power may be a number, a lambda, or a symbol. The
            // numeric default from the per-category alist is the
            // fallback; a per-component lambda / symbol takes
            // precedence.
            let default_power = d.power.map(|p| p as f32).unwrap_or(0.0);
            let power_source = match a.power {
                Some(v) => DynamicScalar::from_lisp(v.as_inner(), default_power),
                None => d.power.map(|p| DynamicScalar::constant(p as f32)),
            };
            let meter = Meter::new(
                id,
                interval,
                cached_succ_ids,
                power_source,
                a.stream_jitter_pct
                    .or(d.stream_jitter_pct)
                    .unwrap_or(0.0) as f32,
                hidden,
            );
            let h = register_with_modes(
                &w,
                meter,
                a.health.or(d.health),
                a.telemetry_mode.or(d.telemetry_mode),
                a.command_mode.or(d.command_mode),
            )?;
            apply_initial_name(&w, id, a.name);
            if a.main.unwrap_or(false) {
                w.set_main_meter(id).map_err(Error::invalid_argument)?;
            }
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
        "%make-battery",
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
            let h = register_with_modes(
                &w,
                Battery::new(id, interval, cfg),
                a.health.or(d.health),
                a.telemetry_mode.or(d.telemetry_mode),
                a.command_mode.or(d.command_mode),
            )?;
            apply_initial_name(&w, id, a.name);
            Ok::<_, Error>(h)
        },
    );

    let w = world.clone();
    ctx.defun(
        "%make-battery-inverter",
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
            // Inverters are never hidden, so cache only the hidden
            // children (rare in practice — a battery would have to be
            // marked hidden). Visible batteries come from
            // World::connections, kept in sync by connect_successors
            // and post-make `(world-connect …)` / `(world-disconnect …)`.
            let cached_succ_ids: Vec<u64> = a
                .successors
                .as_ref()
                .map(|v| v.iter().filter(|h| h.is_hidden()).map(|h| h.id()).collect())
                .unwrap_or_default();
            let h = register_with_modes(
                &w,
                BatteryInverter::new(id, interval, cfg, cached_succ_ids),
                a.health.or(d.health),
                a.telemetry_mode.or(d.telemetry_mode),
                a.command_mode.or(d.command_mode),
            )?;
            apply_initial_name(&w, id, a.name);
            connect_successors(&w, id, &a.successors);
            Ok::<_, Error>(h)
        },
    );

    let w = world.clone();
    ctx.defun(
        "%make-solar-inverter",
        move |ctx: &mut TulispContext, args: Plist<SolarInverterArgs>| {
            let a = args.into_inner();
            let d = parse_defaults::<SolarInverterDefaults>(ctx, a.config.as_ref())?;
            let id = id_or_next(&w, a.id);
            let interval = ms_to_duration(a.interval.or(d.interval), 1000);
            let mut cfg = SolarInverterConfig::default();
            // :sunlight% accepts a number, lambda, or symbol. Pull
            // out the dynamic source (if any) before construction
            // and seed cfg.sunlight_pct from a numeric value or
            // category default. The ramp's initial target is
            // computed from the seed; a dynamic source takes effect
            // on the first refresh_inputs.
            let mut dynamic_sunlight: Option<DynamicScalar> = None;
            if let Some(v) = a.sunlight_pct.as_ref() {
                let raw = v.as_inner();
                if raw.numberp() {
                    if let Ok(pct) = f64::try_from(raw) {
                        cfg.sunlight_pct = pct as f32;
                    }
                } else {
                    dynamic_sunlight =
                        DynamicScalar::from_lisp(raw, cfg.sunlight_pct);
                    if let Some(pct) = d.sunlight_pct {
                        cfg.sunlight_pct = pct as f32;
                    }
                }
            } else if let Some(pct) = d.sunlight_pct {
                cfg.sunlight_pct = pct as f32;
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
            let inverter = SolarInverter::new(id, interval, cfg);
            if let Some(scalar) = dynamic_sunlight {
                inverter.set_sunlight_source(scalar);
            }
            let h = register_with_modes(
                &w,
                inverter,
                a.health.or(d.health),
                a.telemetry_mode.or(d.telemetry_mode),
                a.command_mode.or(d.command_mode),
            )?;
            apply_initial_name(&w, id, a.name);
            Ok::<_, Error>(h)
        },
    );

    let w = world.clone();
    ctx.defun(
        "%make-ev-charger",
        move |ctx: &mut TulispContext, args: Plist<EvChargerArgs>| {
            let a = args.into_inner();
            let d = parse_defaults::<EvChargerDefaults>(ctx, a.config.as_ref())?;
            let id = id_or_next(&w, a.id);
            let interval = ms_to_duration(a.interval.or(d.interval), 1000);
            let mut cfg = EvChargerConfig::default();
            if let Some(v) = a.rated_lower.or(d.rated_lower) {
                cfg.rated_lower_w = v as f32;
            }
            if let Some(v) = a.rated_upper.or(d.rated_upper) {
                cfg.rated_upper_w = v as f32;
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
            if let Some(v) = a.soc_protect_margin.or(d.soc_protect_margin) {
                cfg.soc_protect_margin_pct = v as f32;
            }
            if let Some(v) = a.capacity_wh.or(d.capacity_wh) {
                cfg.capacity_wh = v as f32;
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
            let h = register_with_modes(
                &w,
                EvCharger::new(id, interval, cfg),
                a.health.or(d.health),
                a.telemetry_mode.or(d.telemetry_mode),
                a.command_mode.or(d.command_mode),
            )?;
            apply_initial_name(&w, id, a.name);
            Ok::<_, Error>(h)
        },
    );

    let w = world;
    ctx.defun(
        "%make-chp",
        move |ctx: &mut TulispContext, args: Plist<ChpArgs>| {
            let a = args.into_inner();
            let d = parse_defaults::<ChpDefaults>(ctx, a.config.as_ref())?;
            let id = id_or_next(&w, a.id);
            let jitter = a
                .stream_jitter_pct
                .or(d.stream_jitter_pct)
                .unwrap_or(0.0) as f32;
            let h = register_with_modes(
                &w,
                Chp::new(id, jitter),
                a.health.or(d.health),
                a.telemetry_mode.or(d.telemetry_mode),
                a.command_mode.or(d.command_mode),
            )?;
            apply_initial_name(&w, id, a.name);
            Ok::<_, Error>(h)
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
    health: Option<Health>,
    telemetry: Option<TelemetryMode>,
    command: Option<CommandMode>,
) -> Result<ComponentHandle, Error> {
    let id = component.id();
    let h = world.register(component);
    apply_initial_modes(world, id, health, telemetry, command);
    Ok(h)
}

/// Apply a `:name "…"` plist arg after registration. Stores it as a
/// display-name override so the gRPC `ListElectricalComponents`
/// response and the UI's topology endpoint both pick it up. No-op
/// when the user didn't pass `:name`.
fn apply_initial_name(world: &World, id: u64, name: Option<String>) {
    if let Some(n) = name {
        world.rename(id, n);
    }
}

/// Apply initial runtime mode args from a plist constructor. Each
/// `make-*` calls this immediately after `world.register(...)` so a
/// component declared with `:health 'error` is broken from the very
/// first tick. Symbol → enum parsing happens in the `TryFrom` impls
/// (`src/lisp/runtime_modes.rs`); by the time we get here the values
/// are typed.
fn apply_initial_modes(
    world: &World,
    id: u64,
    health: Option<Health>,
    telemetry: Option<TelemetryMode>,
    command: Option<CommandMode>,
) {
    if let Some(h) = health {
        world.set_health(id, h);
    }
    if let Some(t) = telemetry {
        world.set_telemetry_mode(id, t);
    }
    if let Some(c) = command {
        world.set_command_mode(id, c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a context wired to a fresh World, evaluates `src`, and
    /// returns the World so the test can introspect what got registered.
    fn run(src: &str) -> World {
        run_with_ctx(src).0
    }

    /// Like [`run`] but also surfaces the context — needed for tests
    /// that drive `refresh_inputs` (lambda / symbol `:power` etc.)
    /// after the components have registered.
    fn run_with_ctx(src: &str) -> (World, TulispContext) {
        let world = World::new();
        let mut ctx = TulispContext::new();
        crate::lisp::handle::register(&mut ctx);
        register(&mut ctx, world.clone());
        ctx.eval_string(src).expect("eval lisp source");
        (world, ctx)
    }

    /// `:name "..."` on any %make-* lands as a display-name override
    /// — same path as `(world-rename-component …)` — so the gRPC
    /// listing and the UI's topology endpoint both pick it up.
    /// Omitting `:name` falls through to the component's
    /// auto-generated default (`category-id`).
    #[test]
    fn name_arg_sets_display_name() {
        let world = run(r#"(%make-battery :id 200 :name "main-batt")"#);
        assert_eq!(world.display_name(200).as_deref(), Some("main-batt"));
        let world = run(r#"(%make-battery :id 201)"#);
        assert_eq!(world.display_name(201).as_deref(), Some("bat-201"));
    }

    #[test]
    fn battery_defaults_alone_apply() {
        let world = run(
            r#"(setq d '((capacity . 50000.0)
                         (initial-soc . 20.0)
                         (rated-lower . -8000.0)
                         (rated-upper . 8000.0)))
               (%make-battery :id 100 :config d)"#,
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
               (%make-battery :id 101 :config d :capacity 25000.0)"#,
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
               (%make-battery :id 102 :config d)"#,
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
        let world = run("(%make-battery :id 103)");
        let t = world.get(103).unwrap().telemetry(&world);
        // Default capacity from BatteryConfig::default in battery.rs.
        assert_eq!(t.capacity_wh, Some(92_000.0));
    }

    /// `:power N` lands as a constant DynamicScalar — aggregate_power_w
    /// reads it through immediately, no refresh required.
    #[test]
    fn meter_power_constant_reads_through() {
        let world = run("(%make-meter :id 7 :power 1875.0)");
        let m = world.get(7).unwrap();
        assert!((m.aggregate_power_w(&world) - 1875.0).abs() < 1e-3);
    }

    /// `:power (lambda () N)` produces a dynamic source that the
    /// scheduler-driven refresh path resolves on each pass. The
    /// fallback (0.0) is what aggregate_power_w sees before the
    /// first refresh; after one refresh it matches the lambda's
    /// return.
    #[test]
    fn meter_power_lambda_resolves_each_refresh() {
        let (world, mut ctx) = run_with_ctx(
            r#"(%make-meter :id 8 :power (lambda () 1234.5))"#,
        );
        let m = world.get(8).unwrap();
        // Pre-refresh: cached fallback.
        assert_eq!(m.aggregate_power_w(&world), 0.0);
        // After refresh_inputs: the lambda's value is cached.
        m.refresh_inputs(&mut ctx);
        assert!((m.aggregate_power_w(&world) - 1234.5).abs() < 1e-3);
    }

    /// `:power 'symbol` derefs the variable each refresh — mutating
    /// the bound value between refreshes is what scenarios use to
    /// drive consumer load curves declaratively.
    #[test]
    fn meter_power_symbol_derefs_each_refresh() {
        let (world, mut ctx) = run_with_ctx(
            r#"(setq consumer-power 1500.0)
               (%make-meter :id 9 :power 'consumer-power)"#,
        );
        let m = world.get(9).unwrap();
        m.refresh_inputs(&mut ctx);
        assert!((m.aggregate_power_w(&world) - 1500.0).abs() < 1e-3);
        // Mutate the symbol; next refresh picks up the new value.
        ctx.eval_string("(setq consumer-power 2750.0)").unwrap();
        m.refresh_inputs(&mut ctx);
        assert!((m.aggregate_power_w(&world) - 2750.0).abs() < 1e-3);
    }

    /// `:sunlight%` accepts a lambda the same way meter `:power`
    /// does — the make-path detects the non-numeric value and wires
    /// it into the inverter's DynamicScalar. Refresh resolves it
    /// each tick; the resolved sunlight% is the floor for incoming
    /// setpoints, observable as a clip on the ramp output.
    #[test]
    fn solar_inverter_sunlight_lambda_clips_setpoint() {
        let (world, mut ctx) = run_with_ctx(
            r#"(%make-solar-inverter :id 11
                                    :sunlight% (lambda () 25.0)
                                    :rated-lower -8000.0
                                    :rated-upper 0.0)"#,
        );
        let inv = world.get(11).unwrap();
        // Refresh runs the lambda → sunlight_pct = 25 →
        // min_avail = -2000 W.
        inv.refresh_inputs(&mut ctx);
        // Issue a setpoint below min_avail; CommandDelay default is
        // zero so the next tick promotes it. Ramp default is
        // infinity so the actual jumps straight to the target,
        // floored at min_avail.
        inv.set_active_setpoint(-5000.0).expect("setpoint within rated");
        let now = chrono::Utc::now();
        inv.tick(&world, now, Duration::from_millis(100));
        let p = inv
            .telemetry(&world)
            .active_power_w
            .expect("active power present");
        assert!(
            (p - (-2000.0)).abs() < 1.0,
            "expected sunlight-clipped -2000 W, got {p}"
        );
    }

    /// `(set-meter-power id W)` is the existing imperative setter
    /// for driving curves from `(every)` callbacks. It collapses the
    /// source back to a constant — even if the meter was originally
    /// constructed with a lambda — so subsequent refreshes don't
    /// silently overwrite the user's intent.
    #[test]
    fn meter_set_power_collapses_to_constant() {
        let (world, mut ctx) = run_with_ctx(
            r#"(%make-meter :id 10 :power (lambda () 1000.0))"#,
        );
        let m = world.get(10).unwrap();
        m.refresh_inputs(&mut ctx);
        assert!((m.aggregate_power_w(&world) - 1000.0).abs() < 1e-3);

        // External setter wins; refresh becomes a no-op on the
        // collapsed constant.
        m.set_active_power_override(7777.0);
        m.refresh_inputs(&mut ctx);
        assert!((m.aggregate_power_w(&world) - 7777.0).abs() < 1e-3);
    }
}
