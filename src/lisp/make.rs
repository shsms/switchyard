//! `(%make-grid-connection-point)`, `(%make-meter)`, `(%make-battery)`,
//! … — the Rust-side constructor primitives the lisp DSL dispatches
//! to. Each takes its arguments as a typed plist via tulisp's
//! `AsPlist!` macro and returns a `ComponentHandle` (an opaque
//! `Shared<dyn TulispAny>` on the lisp side). The user-facing names
//! (`(make-grid-connection-point)`, `(make-meter)`, …) are `defun`
//! wrappers in `sim/defaults.lisp`
//! that prepend a category-default plist to the caller's args
//! before invoking these primitives; AsPlist's last-occurrence-wins
//! key resolution lets per-component values override defaults
//! without any extra plumbing on the Rust side.

use std::time::Duration;

use tulisp::{AsPlist, Error, Plist, TulispContext};

use crate::lisp::value::LispValue;
use crate::sim::{
    Battery, BatteryInverter, Chp, ComponentHandle, EvCharger, Grid, Meter, MicrogridSite,
    SolarInverter,
    battery::BatteryConfig,
    dynamic_scalar::DynamicScalar,
    ev_charger::EvChargerConfig,
    inverter::battery_inverter::BatteryInverterConfig,
    inverter::solar_inverter::SolarInverterConfig,
    runtime::{CommandMode, Health, TelemetryMode},
};

// -----------------------------------------------------------------------------
// make-grid-connection-point
// -----------------------------------------------------------------------------

AsPlist! {
    pub struct GridArgs {
        id: Option<i64> {= None},
        name: Option<String> {= None},
        rated_fuse_current<":rated-fuse-current">: Option<i64> {= None},
        rated_lower<":rated-lower">: Option<f64> {= None},
        rated_upper<":rated-upper">: Option<f64> {= None},
        successors: Option<Vec<ComponentHandle>> {= None},
        stream_jitter_pct<":stream-jitter-pct">: Option<f64> {= None},
        health<":health">: Option<Health> {= None},
        telemetry_mode<":telemetry-mode">: Option<TelemetryMode> {= None},
        command_mode<":command-mode">: Option<CommandMode> {= None},
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
        /// PF-style Q cap: |Q| ≤ k × |P|. Pass 0 to disable (nil inherits the default).
        reactive_pf_limit<":reactive-pf-limit">: Option<f64> {= None},
        /// kVA-style Q cap: P² + Q² ≤ apparent². Pass 0 to disable (nil inherits the default).
        reactive_apparent_va<":reactive-apparent-va">: Option<f64> {= None},
        /// Inverter-internal latency before a Q setpoint starts
        /// being tracked. Defaults to 100 ms.
        reactive_command_delay_ms<":reactive-command-delay-ms">: Option<i64> {= None},
        /// Reactive slew rate (VAR/s). Default 2000 ≈ IEEE 1547-2018
        /// Cat B 5 s OLRT for a 10 kVAR window.
        reactive_ramp_rate<":reactive-ramp-rate">: Option<f64> {= None},
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
    }
}

// -----------------------------------------------------------------------------
// Registration
// -----------------------------------------------------------------------------

pub fn register(ctx: &mut TulispContext, router: crate::sim::microgrids::SharedSiteRouter) {
    let r = router.clone();
    ctx.defun(
        "%make-grid-connection-point",
        move |_ctx: &mut TulispContext, args: Plist<GridArgs>| {
            let w = r.site();
            let a = args.into_inner();
            let id = id_or_next(&w, a.id)?;
            let rated_active_bounds = match (a.rated_lower, a.rated_upper) {
                (Some(l), Some(u)) => Some((l as f32, u as f32)),
                _ => None,
            };
            let grid = Grid::new(
                id,
                a.rated_fuse_current.unwrap_or(0) as u32,
                rated_active_bounds,
                a.stream_jitter_pct.unwrap_or(0.0) as f32,
            );
            let h = register_with_modes(&w, grid, a.health, a.telemetry_mode, a.command_mode)?;
            apply_initial_name(&w, id, a.name);
            connect_successors(&w, id, &a.successors);
            Ok::<_, Error>(h)
        },
    );

    let r = router.clone();
    ctx.defun(
        "%make-meter",
        move |_ctx: &mut TulispContext, args: Plist<MeterArgs>| {
            let w = r.site();
            let a = args.into_inner();
            let id = id_or_next(&w, a.id)?;
            let interval = ms_to_duration(a.interval, 1000);
            let hidden = a.hidden.unwrap_or(false);
            // :power may be a number, a lambda, or a symbol. The
            // wrapper-expanded category default lands in `a.power`
            // when no per-component value was passed; otherwise the
            // per-component value overrides via AsPlist's last-wins.
            // `DynamicScalar::from_lisp` dispatches on shape — constant
            // for numbers, eval/funcall for the rest.
            let power_source = a
                .power
                .as_ref()
                .and_then(|v| DynamicScalar::from_lisp(v.as_inner(), 0.0));
            // Probe the main-meter slot BEFORE registering. The
            // previous shape registered first and only checked at
            // `set_main_meter` time — a `:main t` collision left a
            // half-registered meter (in the components vec + named
            // + reachable via get) without the main flag, since the
            // Err from set_main_meter propagated past the in-progress
            // construction. Failing the eval before any registry
            // mutation keeps the world consistent on rejection.
            let wants_main = a.main.unwrap_or(false);
            if wants_main
                && let Some(existing) = w.main_meter_id()
                && existing != id
            {
                return Err(Error::invalid_argument(format!(
                    "main meter already set to {existing}; can't claim {id}"
                )));
            }
            let meter = Meter::new(
                id,
                interval,
                power_source,
                a.stream_jitter_pct.unwrap_or(0.0) as f32,
                hidden,
            );
            let h = register_with_modes(&w, meter, a.health, a.telemetry_mode, a.command_mode)?;
            apply_initial_name(&w, id, a.name);
            if wants_main {
                w.set_main_meter(id).map_err(Error::invalid_argument)?;
            }
            connect_successors(&w, id, &a.successors);
            Ok::<_, Error>(h)
        },
    );

    let r = router.clone();
    ctx.defun(
        "%make-battery",
        move |_ctx: &mut TulispContext, args: Plist<BatteryArgs>| {
            let w = r.site();
            let a = args.into_inner();
            let id = id_or_next(&w, a.id)?;
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
            let h = register_with_modes(
                &w,
                Battery::new(id, interval, cfg),
                a.health,
                a.telemetry_mode,
                a.command_mode,
            )?;
            apply_initial_name(&w, id, a.name);
            Ok::<_, Error>(h)
        },
    );

    let r = router.clone();
    ctx.defun(
        "%make-battery-inverter",
        move |_ctx: &mut TulispContext, args: Plist<BatteryInverterArgs>| {
            let w = r.site();
            let a = args.into_inner();
            let id = id_or_next(&w, a.id)?;
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
            //   value ≤ 0.0  → that constraint is disabled
            //   absent       → inherit the existing field on cfg.reactive
            //                  (i.e. the BatteryInverterConfig::default
            //                  microsim_default, which sets PF=0.35)
            // `:reactive-apparent-va 50000` adds a kVA cap *without*
            // silently dropping the inherited PF limit; previously
            // that subtle interaction was the easy way to ship a
            // misconfigured inverter.
            let merge_reactive = |input: Option<f64>, fallback: Option<f32>| -> Option<f32> {
                match input {
                    Some(v) if v > 0.0 => Some(v as f32),
                    Some(_) => None,
                    None => fallback,
                }
            };
            cfg.reactive = crate::sim::reactive::ReactiveCapability {
                pf_limit: merge_reactive(a.reactive_pf_limit, cfg.reactive.pf_limit),
                apparent_va: merge_reactive(a.reactive_apparent_va, cfg.reactive.apparent_va),
            };
            if let Some(v) = a.reactive_command_delay_ms {
                cfg.reactive_command_delay = Duration::from_millis(v.max(0) as u64);
            }
            if let Some(v) = a.reactive_ramp_rate {
                cfg.reactive_ramp_rate_var_per_s = v as f32;
            }
            let h = register_with_modes(
                &w,
                BatteryInverter::new(id, interval, cfg),
                a.health,
                a.telemetry_mode,
                a.command_mode,
            )?;
            apply_initial_name(&w, id, a.name);
            connect_successors(&w, id, &a.successors);
            Ok::<_, Error>(h)
        },
    );

    let r = router.clone();
    ctx.defun(
        "%make-solar-inverter",
        move |_ctx: &mut TulispContext, args: Plist<SolarInverterArgs>| {
            let w = r.site();
            let a = args.into_inner();
            let id = id_or_next(&w, a.id)?;
            let interval = ms_to_duration(a.interval, 1000);
            let mut cfg = SolarInverterConfig::default();
            // :sunlight% accepts a number, lambda, or symbol. Number
            // seeds the initial ramp target on `cfg.sunlight_pct`;
            // lambda / symbol installs a dynamic source that takes
            // effect on the first `refresh_inputs`. The wrapper-
            // expanded category default lands in `a.sunlight_pct`
            // already; per-component plist overrides via last-wins.
            let mut dynamic_sunlight: Option<DynamicScalar> = None;
            if let Some(v) = a.sunlight_pct.as_ref() {
                let raw = v.as_inner();
                if raw.numberp() {
                    if let Ok(pct) = f64::try_from(raw) {
                        cfg.sunlight_pct = pct as f32;
                    }
                } else {
                    dynamic_sunlight = DynamicScalar::from_lisp(raw, cfg.sunlight_pct);
                }
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
            let merge_reactive = |input: Option<f64>, fallback: Option<f32>| -> Option<f32> {
                match input {
                    Some(v) if v > 0.0 => Some(v as f32),
                    Some(_) => None,
                    None => fallback,
                }
            };
            cfg.reactive = crate::sim::reactive::ReactiveCapability {
                pf_limit: merge_reactive(a.reactive_pf_limit, cfg.reactive.pf_limit),
                apparent_va: merge_reactive(a.reactive_apparent_va, cfg.reactive.apparent_va),
            };
            if let Some(v) = a.reactive_command_delay_ms {
                cfg.reactive_command_delay = Duration::from_millis(v.max(0) as u64);
            }
            if let Some(v) = a.reactive_ramp_rate {
                cfg.reactive_ramp_rate_var_per_s = v as f32;
            }
            let inverter = SolarInverter::new(id, interval, cfg);
            if let Some(scalar) = dynamic_sunlight {
                inverter.set_sunlight_source(scalar);
            }
            let h = register_with_modes(&w, inverter, a.health, a.telemetry_mode, a.command_mode)?;
            apply_initial_name(&w, id, a.name);
            Ok::<_, Error>(h)
        },
    );

    let r = router.clone();
    ctx.defun(
        "%make-ev-charger",
        move |_ctx: &mut TulispContext, args: Plist<EvChargerArgs>| {
            let w = r.site();
            let a = args.into_inner();
            let id = id_or_next(&w, a.id)?;
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
            let h = register_with_modes(
                &w,
                EvCharger::new(id, interval, cfg),
                a.health,
                a.telemetry_mode,
                a.command_mode,
            )?;
            apply_initial_name(&w, id, a.name);
            Ok::<_, Error>(h)
        },
    );

    let r = router;
    ctx.defun(
        "%make-chp",
        move |_ctx: &mut TulispContext, args: Plist<ChpArgs>| {
            let w = r.site();
            let a = args.into_inner();
            let id = id_or_next(&w, a.id)?;
            let jitter = a.stream_jitter_pct.unwrap_or(0.0) as f32;
            let h = register_with_modes(
                &w,
                Chp::new(id, jitter),
                a.health,
                a.telemetry_mode,
                a.command_mode,
            )?;
            apply_initial_name(&w, id, a.name);
            Ok::<_, Error>(h)
        },
    );
}

fn connect_successors(
    site: &MicrogridSite,
    parent: u64,
    successors: &Option<Vec<ComponentHandle>>,
) {
    if let Some(list) = successors {
        for child in list {
            // Every edge — hidden or not — lands in `MicrogridSite::connections`.
            // Visibility filtering happens at the `connections()` /
            // `hidden_connections()` boundary that drives gRPC and the
            // UI; the aggregation paths walk the unfiltered graph.
            site.connect(parent, child.id());
        }
    }
}

fn ms_to_duration(ms: Option<i64>, default_ms: u64) -> Duration {
    Duration::from_millis(ms.map(|x| x.max(0) as u64).unwrap_or(default_ms))
}

/// Resolve the component id from an `:id` plist value, falling back to
/// `MicrogridSite::next_id()` when omitted. Centralized so the
/// validation stays in one place — every make-* funnels through here.
///
/// An explicit `:id` must be positive (a negative would cast to a
/// giant u64 that registers but never matches) and must not collide
/// with a component already on this site (a collision would push a
/// second ticking component while only one stays addressable, and
/// parent meters would double-count). The auto path skips allocator
/// values an explicit `:id` already pinned for the same reason.
fn id_or_next(site: &MicrogridSite, explicit: Option<i64>) -> Result<u64, Error> {
    match explicit {
        Some(x) => {
            if x <= 0 {
                return Err(Error::invalid_argument(format!(
                    ":id must be positive, got {x}"
                )));
            }
            let id = x as u64;
            if site.get(id).is_some() {
                return Err(Error::invalid_argument(format!(
                    "component id {id} is already registered"
                )));
            }
            Ok(id)
        }
        None => loop {
            let id = site.next_id();
            if site.get(id).is_none() {
                return Ok(id);
            }
        },
    }
}

/// Register a freshly-built component, then apply any initial runtime
/// mode args. Returns the handle so the caller can also wire
/// connections / `setq` it for cross-references.
///
/// Centralising this guarantees every make-* applies modes in the
/// same order (health, then telemetry, then command) right after
/// registration — before any tick or subscriber runs.
fn register_with_modes<C: crate::sim::SimulatedComponent + 'static>(
    site: &MicrogridSite,
    component: C,
    health: Option<Health>,
    telemetry: Option<TelemetryMode>,
    command: Option<CommandMode>,
) -> Result<ComponentHandle, Error> {
    let id = component.id();
    let h = site.register(component);
    apply_initial_modes(site, id, health, telemetry, command);
    Ok(h)
}

/// Apply a `:name "…"` plist arg after registration. Stores it as a
/// display-name override so the gRPC `ListElectricalComponents`
/// response and the UI's topology endpoint both pick it up. No-op
/// when the user didn't pass `:name`.
fn apply_initial_name(site: &MicrogridSite, id: u64, name: Option<String>) {
    if let Some(n) = name {
        site.rename(id, n);
    }
}

/// Apply initial runtime mode args from a plist constructor. Each
/// `make-*` calls this immediately after `site.register(...)` so a
/// component declared with `:health 'error` is broken from the very
/// first tick. Symbol → enum parsing happens in the `TryFrom` impls
/// (`src/lisp/runtime_modes.rs`); by the time we get here the values
/// are typed.
fn apply_initial_modes(
    site: &MicrogridSite,
    id: u64,
    health: Option<Health>,
    telemetry: Option<TelemetryMode>,
    command: Option<CommandMode>,
) {
    if let Some(h) = health {
        site.set_health(id, h);
    }
    if let Some(t) = telemetry {
        site.set_telemetry_mode(id, t);
    }
    // An explicit `:command-mode` refines the default the health coupling
    // just applied — but it must not re-enable a device declared errored.
    // `set_health(Error)` couples the command mode to Error (error ⇒ no
    // commands accepted); letting `:command-mode 'normal` override that
    // would yield a broken-but-accepting component the runtime path can
    // never produce. Health wins for an errored device.
    if let Some(c) = command
        && health != Some(Health::Error)
    {
        site.set_command_mode(id, c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a context wired to a fresh MicrogridSite, evaluates `src`, and
    /// returns the MicrogridSite so the test can introspect what got registered.
    fn run(src: &str) -> MicrogridSite {
        run_with_ctx(src).0
    }

    /// Like [`run`] but also surfaces the context — needed for tests
    /// that drive `refresh_inputs` (lambda / symbol `:power` etc.)
    /// after the components have registered.
    fn run_with_ctx(src: &str) -> (MicrogridSite, TulispContext) {
        use crate::sim::microgrids::{SiteRouter, new_current_microgrid, new_registry};
        let site = MicrogridSite::new();
        let mut ctx = TulispContext::new();
        crate::lisp::handle::register(&mut ctx);
        let router = SiteRouter::new(new_registry(), new_current_microgrid(), site.clone());
        register(&mut ctx, router);
        // Load the `make-*` wrappers + `*-defaults` plists so tests
        // exercise the same wrapper → primitive path config.lisp uses.
        ctx.eval_string(include_str!("../../sim/defaults.lisp"))
            .expect("defaults.lisp");
        ctx.eval_string(src).expect("eval lisp source");
        (site, ctx)
    }

    /// `:name "..."` on any %make-* lands as a display-name override
    /// — same path as `(rename-component …)` — so the gRPC
    /// listing and the UI's topology endpoint both pick it up.
    /// Omitting `:name` falls through to the component's
    /// auto-generated default (`category-id`).
    #[test]
    fn name_arg_sets_display_name() {
        let site = run(r#"(%make-battery :id 200 :name "main-batt")"#);
        assert_eq!(site.display_name(200).as_deref(), Some("main-batt"));
        let site = run(r#"(%make-battery :id 201)"#);
        assert_eq!(site.display_name(201).as_deref(), Some("bat-201"));
    }

    #[test]
    fn primitive_plist_args_set_fields() {
        // %make-battery is the primitive — every field arrives as a
        // plist key. Defaults are applied by wrappers, not here.
        let site = run(
            r#"(%make-battery :id 100 :capacity 50000.0 :initial-soc 20.0
                              :rated-lower -8000.0 :rated-upper 8000.0)"#,
        );
        let t = site.get(100).unwrap().telemetry(&site);
        assert_eq!(t.capacity_wh, Some(50_000.0));
        assert!((t.soc_pct.unwrap() - 20.0).abs() < 1e-3);
    }

    #[test]
    fn wrapper_merges_category_defaults() {
        // `make-battery` (wrapper) prepends `battery-defaults` (plist
        // literal in sim/defaults.lisp) to the caller's args. AsPlist's
        // last-occurrence-wins resolution lets the per-component plist
        // override individual default fields while inheriting the rest.
        let site = run(r#"(make-battery :id 200 :capacity 50000.0)"#);
        let t = site.get(200).unwrap().telemetry(&site);
        // From per-component plist:
        assert_eq!(t.capacity_wh, Some(50_000.0));
        // From battery-defaults — :health ok carried through.
        assert_eq!(site.runtime_of(200).health, crate::sim::runtime::Health::Ok);
    }

    #[test]
    fn primitive_skips_wrapper_defaults() {
        // Calling %make-battery directly bypasses the wrapper's
        // default-prepending, so the BatteryConfig::default values
        // stand for every unset field.
        let site = run("(%make-battery :id 103)");
        let t = site.get(103).unwrap().telemetry(&site);
        assert_eq!(t.capacity_wh, Some(92_000.0));
    }

    #[test]
    fn defaults_accept_bare_symbol_for_health() {
        // Health is an enum carried as a bare symbol through the plist;
        // verify the wrapper-applied default (battery-defaults sets
        // :health ok) lands.
        let site = run("(make-battery :id 102)");
        assert_eq!(site.runtime_of(102).health, crate::sim::runtime::Health::Ok);
    }

    /// `:health 'error` couples command mode to Error (error ⇒ no commands
    /// accepted). An explicit `:command-mode` must not re-enable an errored
    /// device — health wins. A non-errored device still honours its mode.
    #[test]
    fn errored_health_overrides_explicit_command_mode() {
        use crate::sim::runtime::{CommandMode, Health};
        let site = run(
            r#"(%make-battery :id 300 :health 'error :command-mode 'normal)
               (%make-battery :id 301 :health 'ok :command-mode 'timeout)"#,
        );
        let errored = site.runtime_of(300);
        assert_eq!(errored.health, Health::Error);
        assert_eq!(errored.command, CommandMode::Error);

        let ok = site.runtime_of(301);
        assert_eq!(ok.health, Health::Ok);
        assert_eq!(ok.command, CommandMode::Timeout);
    }

    /// `:power N` lands as a constant DynamicScalar — aggregate_power_w
    /// reads it through immediately, no refresh required.
    #[test]
    fn meter_power_constant_reads_through() {
        let site = run("(%make-meter :id 7 :power 1875.0)");
        let m = site.get(7).unwrap();
        assert!((m.aggregate_power_w(&site) - 1875.0).abs() < 1e-3);
    }

    /// `:power (lambda () N)` produces a dynamic source that the
    /// scheduler-driven refresh path resolves on each pass. The
    /// fallback (0.0) is what aggregate_power_w sees before the
    /// first refresh; after one refresh it matches the lambda's
    /// return.
    #[test]
    fn meter_power_lambda_resolves_each_refresh() {
        let (site, mut ctx) = run_with_ctx(r#"(%make-meter :id 8 :power (lambda () 1234.5))"#);
        let m = site.get(8).unwrap();
        // Pre-refresh: cached fallback.
        assert_eq!(m.aggregate_power_w(&site), 0.0);
        // After refresh_inputs: the lambda's value is cached.
        m.refresh_inputs(&mut ctx);
        assert!((m.aggregate_power_w(&site) - 1234.5).abs() < 1e-3);
    }

    /// `:power 'symbol` derefs the variable each refresh — mutating
    /// the bound value between refreshes is what scenarios use to
    /// drive consumer load curves declaratively.
    #[test]
    fn meter_power_symbol_derefs_each_refresh() {
        let (site, mut ctx) = run_with_ctx(
            r#"(setq consumer-power 1500.0)
               (%make-meter :id 9 :power 'consumer-power)"#,
        );
        let m = site.get(9).unwrap();
        m.refresh_inputs(&mut ctx);
        assert!((m.aggregate_power_w(&site) - 1500.0).abs() < 1e-3);
        // Mutate the symbol; next refresh picks up the new value.
        ctx.eval_string("(setq consumer-power 2750.0)").unwrap();
        m.refresh_inputs(&mut ctx);
        assert!((m.aggregate_power_w(&site) - 2750.0).abs() < 1e-3);
    }

    /// `:sunlight%` accepts a lambda the same way meter `:power`
    /// does — the make-path detects the non-numeric value and wires
    /// it into the inverter's DynamicScalar. Refresh resolves it
    /// each tick; the resolved sunlight% is the floor for incoming
    /// setpoints, observable as a clip on the ramp output.
    #[test]
    fn solar_inverter_sunlight_lambda_clips_setpoint() {
        let (site, mut ctx) = run_with_ctx(
            r#"(%make-solar-inverter :id 11
                                    :sunlight% (lambda () 25.0)
                                    :rated-lower -8000.0
                                    :rated-upper 0.0)"#,
        );
        let inv = site.get(11).unwrap();
        // Refresh runs the lambda → sunlight_pct = 25 →
        // min_avail = -2000 W.
        inv.refresh_inputs(&mut ctx);
        // Issue a setpoint below min_avail; CommandDelay default is
        // zero so the next tick promotes it. Ramp default is
        // infinity so the actual jumps straight to the target,
        // floored at min_avail.
        inv.set_active_setpoint(-5000.0)
            .expect("setpoint within rated");
        let now = chrono::Utc::now();
        inv.tick(&site, now, Duration::from_millis(100));
        let p = inv
            .telemetry(&site)
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
        let (site, mut ctx) = run_with_ctx(r#"(%make-meter :id 10 :power (lambda () 1000.0))"#);
        let m = site.get(10).unwrap();
        m.refresh_inputs(&mut ctx);
        assert!((m.aggregate_power_w(&site) - 1000.0).abs() < 1e-3);

        // External setter wins; refresh becomes a no-op on the
        // collapsed constant.
        m.set_active_power_override(7777.0);
        m.refresh_inputs(&mut ctx);
        assert!((m.aggregate_power_w(&site) - 7777.0).abs() < 1e-3);
    }

    /// An explicit `:id` colliding with a registered component is a
    /// config error — pre-validation it double-registered: two ticking
    /// components, one addressable, double-counted by parent meters.
    #[test]
    fn duplicate_explicit_id_rejects() {
        let (site, mut ctx) = run_with_ctx("(%make-battery :id 100)");
        let res = ctx.eval_string("(%make-meter :id 100)");
        assert!(res.is_err(), "expected duplicate-id error, got {res:?}");
        assert_eq!(site.components().len(), 1, "no half-registration");
    }

    /// `:id 0` / negative ids are rejected — a negative cast to u64
    /// would register under a giant id that nothing ever matches.
    #[test]
    fn non_positive_explicit_id_rejects() {
        let (site, mut ctx) = run_with_ctx("(%make-battery :id 7)");
        for bad in ["(%make-meter :id 0)", "(%make-meter :id -1)"] {
            let res = ctx.eval_string(bad);
            assert!(res.is_err(), "expected error from {bad}, got {res:?}");
        }
        assert_eq!(site.components().len(), 1);
    }

    /// Auto-allocation skips over a value an explicit `:id` pinned
    /// before the allocator reached it.
    #[test]
    fn auto_id_skips_a_pinned_value() {
        let first = crate::sim::component::FIRST_AUTO_ID;
        let (site, mut ctx) = run_with_ctx(&format!("(%make-battery :id {first})"));
        ctx.eval_string("(%make-meter)").unwrap();
        assert_eq!(site.components().len(), 2);
        let auto_id = site
            .components()
            .iter()
            .map(|c| c.id())
            .find(|id| *id != first)
            .expect("the meter got its own id");
        assert!(
            auto_id > first,
            "skipped past the pinned {first}, got {auto_id}"
        );
    }
}

#[cfg(test)]
mod main_meter_tests {
    //! End-to-end tests for the `:main t` slot on `%make-meter`,
    //! driven through `Config` so the failure modes are exactly
    //! the ones a real config would see.
    use crate::lisp::test_support::config_with;

    /// Two meters with `:main t` is a config error. The first one
    /// claims the slot; the second's `(%make-meter)` returns an
    /// error rather than silently overwriting.
    #[test]
    fn duplicate_main_meter_rejects() {
        let (cfg, _dir) = config_with(
            "(set-microgrid-id 9)
             (%make-meter :id 1 :main t)",
        );
        let res = cfg.eval("(%make-meter :id 2 :main t)");
        assert!(res.is_err(), "expected duplicate-main error");
        assert!(res.unwrap_err().contains("main meter"));
    }

    /// The rejection from `duplicate_main_meter_rejects` shouldn't
    /// leave a half-registered meter behind: the failing
    /// `(%make-meter :main t)` must not land in `world.components()`
    /// or `world.get(id)`. Regressed once when the slot check fired
    /// AFTER `register_with_modes`.
    #[test]
    fn duplicate_main_meter_rejection_doesnt_register() {
        let (cfg, _dir) = config_with(
            "(set-microgrid-id 9)
             (%make-meter :id 1 :main t)",
        );
        let before = cfg.site().components().len();
        let _ = cfg.eval("(%make-meter :id 2 :main t)");
        let after = cfg.site().components().len();
        assert_eq!(
            before, after,
            "rejected :main meter leaked into the components list",
        );
        assert!(
            cfg.site().get(2).is_none(),
            "rejected :main meter is still reachable via get(2)"
        );
    }
}
