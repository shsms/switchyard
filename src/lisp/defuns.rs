//! Defun installers — every `ctx.defun("name", …)` the Lisp
//! interface exposes is registered here.
//!
//! `Config::new` and `Config::reload` both walk this set; each
//! installer captures its slice of the runtime state by clone and
//! installs as many defuns as it owns. Pure plumbing, no policy.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use chrono::Utc;
use parking_lot::{Mutex, RwLock};
use tokio::sync::broadcast;
use tulisp::{Error, TulispContext, TulispObject};

use crate::sim::MicrogridSite;
use crate::sim::microgrids::SharedSiteRouter;

use super::{Metadata, csv_profile, handle, make};

pub(super) fn register_clock(ctx: &mut TulispContext, clock: crate::sim::clock::SharedClock) {
    ctx.defun(
        "set-timezone",
        move |name: String| -> Result<String, tulisp::Error> {
            let tz: chrono_tz::Tz = name
                .parse()
                .map_err(|_| tulisp::Error::os_error(format!("unknown timezone: {name:?}")))?;
            clock.write().tz = tz;
            Ok(name)
        },
    );
}

/// Newtype around `TulispObject` so `Vec<RawStage>` satisfies the
/// AsPlist field bound (which needs `TryFrom<TulispObject, Error =
/// tulisp::Error>`; the blanket impl on `TulispObject` is `Error =
/// Infallible`). Mirrors tradingsim's same-named helper.
pub struct RawStage(tulisp::TulispObject);

impl TryFrom<tulisp::TulispObject> for RawStage {
    type Error = tulisp::Error;
    fn try_from(v: tulisp::TulispObject) -> Result<Self, tulisp::Error> {
        Ok(RawStage(v))
    }
}

impl From<RawStage> for tulisp::TulispObject {
    fn from(v: RawStage) -> tulisp::TulispObject {
        v.0
    }
}

tulisp::AsPlist! {
    pub struct DefineScenarioArgs {
        name: String,
        description: Option<String> {= None},
        /// Calendar date the scenario is treated as taking place
        /// on, ISO `YYYY-MM-DD`. Optional — `None` falls back to
        /// wallclock-today.
        date: Option<String> {= None},
        stages: Vec<RawStage>,
    }
}

tulisp::AsPlist! {
    pub struct StageArgs {
        name: String,
        hour_from<":hour-from">: f64,
        hour_to<":hour-to">: f64,
        /// Optional tulisp lambda funcalled on stage entry by the
        /// auto-advance task. Receives no args; side-effects via
        /// the existing setter defuns (`set-active-power`,
        /// `set-meter-power`, `(every …)`, …) drive whatever the
        /// stage represents. Wrapped via `LispValue` so the raw
        /// lambda rides through `AsPlist!` (the bare `TulispObject`
        /// has `TryFrom::Error = Infallible`, which doesn't fit the
        /// macro's expected error shape).
        on: Option<crate::lisp::value::LispValue> {= None},
    }
}

pub(super) fn register_scenarios(
    ctx: &mut TulispContext,
    scenarios: crate::sim::scenarios::SharedScenarios,
) {
    use crate::sim::scenarios::{ScenarioDef, ScenarioEntry, ScenarioRuntime, Stage};
    use tulisp::Plistable as _;
    ctx.defun(
        "define-scenario",
        move |ctx: &mut TulispContext,
              args: tulisp::Plist<DefineScenarioArgs>|
              -> Result<String, tulisp::Error> {
            let a = args.into_inner();
            let mut stages = Vec::new();
            for raw in a.stages {
                let s = StageArgs::from_plist(ctx, &raw.0)?;
                let on =
                    s.on.map(crate::lisp::value::LispValue::into_inner)
                        .filter(|o| !o.null());
                stages.push(Stage {
                    name: s.name,
                    hour_from: s.hour_from,
                    hour_to: s.hour_to,
                    on,
                });
            }
            let date = match a.date.as_deref() {
                None => None,
                Some(s) => Some(
                    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|e| {
                        tulisp::Error::os_error(format!(
                            "define-scenario: :date must be YYYY-MM-DD; got {s:?} ({e})"
                        ))
                    })?,
                ),
            };
            let def = ScenarioDef {
                name: a.name.clone(),
                description: a.description.unwrap_or_default(),
                date,
                stages,
            };
            scenarios.lock().insert(
                a.name.clone(),
                ScenarioEntry {
                    def,
                    runtime: ScenarioRuntime::default(),
                },
            );
            Ok(a.name)
        },
    );
}

tulisp::AsPlist! {
    pub struct MakeMicrogridArgs {
        name: Option<String> {= None},
        id: Option<i64> {= None},
        grpc_port<":grpc-port">: Option<i64> {= None},
        /// Optional TSO zone label (informational; see
        /// `crate::sim::microgrids::MicrogridDef::tso`).
        tso: Option<String> {= None},
        /// Zero-arg lambda whose body builds the microgrid's
        /// topology — typically a single nested
        /// `(make-grid-connection-point …)` call. The lambda
        /// form is required (not a plain expression) because the
        /// body must evaluate *after* make-microgrid has set the
        /// current-microgrid pointer, so the nested make-* calls
        /// register into the new site instead of the previously-
        /// active one. Optional so a config can register an empty
        /// microgrid that the UI fills in component-by-component.
        topology: Option<crate::lisp::value::LispValue> {= None},
    }
}

/// Register `(make-microgrid …)`. Each call creates a fresh
/// `MicrogridSite`, inserts a registry entry for it, sets the
/// `CurrentMicrogrid` pointer, funcalls the `:topology` lambda
/// (whose body's make-* calls then register into the new site
/// via the router's per-call dispatch), and finally restores the
/// previous pointer.
//
// Seven args trips clippy's `too_many_arguments` threshold; the
// review item A6 plans to bundle the shared state into a single
// `RuntimeHandles` struct that drops the count cleanly. Until
// then, the explicit list is more readable than a one-off tuple.
#[allow(clippy::too_many_arguments)]
pub(super) fn register_microgrids(
    ctx: &mut TulispContext,
    registry: crate::sim::microgrids::SharedMicrogrids,
    router: SharedSiteRouter,
    current: crate::sim::microgrids::CurrentMicrogrid,
    id_allocator: Arc<std::sync::atomic::AtomicU64>,
    registered_tx: Arc<broadcast::Sender<u64>>,
    grid_frequency: crate::sim::frequency::SharedFrequency,
) {
    // Read-only accessors scripts use to dispatch on the active
    // microgrid (E2 of gridpool-support.org). Outside a per-mg
    // context (e.g. boot before any (make-microgrid) form, or a
    // legacy /api/eval call without an mg scope) they fall back
    // to the first registry entry so single-microgrid configs
    // keep returning sensible values.
    {
        let cur = current.clone();
        let reg = registry.clone();
        ctx.defun(
            "current-microgrid-id",
            move || -> Result<i64, tulisp::Error> {
                if let Some(id) = *cur.read() {
                    return Ok(id as i64);
                }
                let r = reg.lock();
                Ok(r.keys().next().copied().unwrap_or(0) as i64)
            },
        );
    }
    {
        let cur = current.clone();
        let reg = registry.clone();
        ctx.defun(
            "microgrid-name",
            move || -> Result<String, tulisp::Error> {
                let id_opt = *cur.read();
                let r = reg.lock();
                let entry = id_opt
                    .and_then(|id| r.get(&id))
                    .or_else(|| r.values().next());
                Ok(entry.map(|e| e.def.name.clone()).unwrap_or_default())
            },
        );
    }
    use crate::sim::microgrids::{
        DEFAULT_MICROGRID_ID, DEFAULT_MICROGRID_NAME, MicrogridDef, MicrogridEntry, next_free_id,
        next_free_port, with_microgrid,
    };
    let _ = router;
    ctx.defun(
        "make-microgrid",
        move |ctx: &mut TulispContext,
              args: tulisp::Plist<MakeMicrogridArgs>|
              -> Result<i64, tulisp::Error> {
            let a = args.into_inner();
            let id = match a.id {
                Some(v) if v > 0 => v as u64,
                _ => {
                    let probed = next_free_id(&registry);
                    if probed == DEFAULT_MICROGRID_ID
                        && registry.lock().contains_key(&DEFAULT_MICROGRID_ID)
                    {
                        DEFAULT_MICROGRID_ID + 1
                    } else {
                        probed
                    }
                }
            };
            let grpc_port = match a.grpc_port {
                Some(p) if p > 0 => p as u16,
                _ => next_free_port(&registry),
            };
            let name = a
                .name
                .clone()
                .unwrap_or_else(|| DEFAULT_MICROGRID_NAME.to_string());
            let def = MicrogridDef {
                id,
                name,
                grpc_port,
                tso: a.tso.clone(),
            };
            // Fresh site per microgrid that shares the enterprise's
            // id allocator with the bootstrap site + every other
            // microgrid — component ids stay globally unique across
            // the registry without per-site coordination.
            let site = MicrogridSite::with_id_allocator(id_allocator.clone());
            // Same grid frequency source as every other site, so
            // their `frequency_hz` reads all return the same OU
            // value (one AC grid → one frequency).
            site.set_grid_frequency(grid_frequency.clone());
            registry.lock().insert(
                id,
                MicrogridEntry {
                    def,
                    site: site.clone(),
                },
            );
            // Notify enterprise-wide subscribers (currently the WS
            // event pump) that a new microgrid landed. send() returns
            // Err when there are no live receivers — fine to ignore;
            // it just means no UI session is open.
            let _ = registered_tx.send(id);
            // Funcall the :topology lambda (if any) with the
            // current-microgrid pointer flipped to this new
            // entry, so the nested make-* calls register into
            // the new site.
            if let Some(topology) = a.topology {
                let lambda = topology.into_inner();
                if !lambda.null() {
                    let nil = TulispObject::nil();
                    let result = with_microgrid(&current, id, || ctx.funcall(&lambda, &nil));
                    result?;
                }
            }
            Ok(id as i64)
        },
    );
}

/// Register every Rust function the config DSL needs.
pub(super) fn register_runtime(
    ctx: &mut TulispContext,
    router: SharedSiteRouter,
    metadata: Arc<RwLock<Metadata>>,
    load_dir: PathBuf,
) {
    add_log_functions(ctx);
    handle::register(ctx);
    make::register(ctx, router.clone());
    register_reset(ctx, router.clone());
    register_grid_state(ctx, router.clone());
    register_metadata(ctx, metadata.clone());
    register_runtime_modes(ctx, router.clone());
    register_load_drivers(ctx, router.clone());
    register_time_helpers(ctx);
    register_reactive_setters(ctx, router.clone());
    register_setpoints(ctx, router.clone(), metadata);
    register_world_ops(ctx, router.clone());
    register_scenario(ctx, router);
    register_fs_helpers(ctx, load_dir);
    csv_profile::register(ctx);
}

/// Scenario lifecycle defuns. Scripts call `(scenario-start NAME)`
/// to mark the beginning, drop `(scenario-event KIND PAYLOAD)` markers
/// at interesting moments, and `(scenario-stop)` when finished. The
/// underlying journal lives on `MicrogridSite` and is read by the
/// `/api/scenario` and `/api/scenario/events` endpoints.
fn register_scenario(ctx: &mut TulispContext, router: SharedSiteRouter) {
    let r = router.clone();
    ctx.defun(
        "scenario-start",
        move |name: String| -> Result<bool, Error> {
            let w = r.site();
            w.scenario_start(name, Utc::now());
            Ok(true)
        },
    );

    let r = router.clone();
    ctx.defun("scenario-stop", move || -> Result<bool, Error> {
        let w = r.site();
        w.scenario_stop(Utc::now());
        Ok(true)
    });

    let r = router.clone();
    ctx.defun(
        "scenario-event",
        move |kind: TulispObject, payload: TulispObject| -> Result<i64, Error> {
            let w = r.site();
            // Accept either a string or a symbol for `kind` so
            // scripts can write `(scenario-event 'outage "bat-1003")`
            // alongside `(scenario-event "note" "warming up")`.
            // Payload renders via Display so any Lisp value works.
            let kind_str = if kind.symbolp() {
                kind.to_string()
            } else {
                String::try_from(kind)?
            };
            let payload_str = payload.to_string();
            let id = w.scenario_record(kind_str, payload_str, Utc::now());
            Ok(id as i64)
        },
    );

    let r = router.clone();
    ctx.defun(
        "scenario-record-csv",
        move |dir: String| -> Result<i64, Error> {
            let w = r.site();
            let path = std::path::PathBuf::from(dir);
            w.scenario_open_csv(&path)
                .map(|n| n as i64)
                .map_err(|e| Error::os_error(format!("scenario-record-csv: {e}")))
        },
    );

    let r = router.clone();
    ctx.defun("scenario-stop-csv", move || -> Result<i64, Error> {
        let w = r.site();
        Ok(w.scenario_close_csv() as i64)
    });

    let r = router;
    ctx.defun("scenario-elapsed", move || -> Result<f64, Error> {
        let w = r.site();
        Ok(w.scenario_elapsed_s(Utc::now()))
    });
}

/// Lower bound on a non-zero request-lifetime that
/// `(set-active-power)` can install. The timeout loop polls at
/// 100 ms and the default physics tick is 100 ms, so a sub-150 ms
/// lifetime can expire before the next physics tick observes the
/// setpoint at all — the ramp would clear without ever leaving
/// idle. `lifetime-ms = 0` is preserved as an explicit "expire
/// immediately" escape (used by tests) and bypasses the clamp.
const MIN_SET_ACTIVE_POWER_LIFETIME_MS: u64 = 150;

/// `(set-active-power ID WATTS &OPTIONAL LIFETIME-MS)` — apply an
/// active-power setpoint and arm a request-lifetime timeout, mirroring
/// what gRPC's `SetElectricalComponentPower` does. Returns `t` on
/// success; signals an error if the component doesn't exist or
/// rejects the setpoint (e.g. out-of-bounds, unsupported kind).
///
/// `LIFETIME-MS` is the duration after which the setpoint snaps back
/// to idle. Omitting it falls back to `default-request-lifetime-ms`,
/// matching the gRPC behaviour. The reset fires from the loop in
/// `Config::start_timeout_loop`.
fn register_setpoints(
    ctx: &mut TulispContext,
    router: SharedSiteRouter,
    metadata: Arc<RwLock<Metadata>>,
) {
    let r = router;
    ctx.defun(
        "set-active-power",
        move |id: i64, watts: f64, lifetime_ms: Option<i64>| -> Result<bool, Error> {
            let w = r.site();
            let component = w.get(id as u64).ok_or_else(|| {
                Error::invalid_argument(format!("set-active-power: component {id} not found"))
            })?;
            component
                .set_active_setpoint(watts as f32)
                .map_err(|e| Error::invalid_argument(format!("set-active-power: {e}")))?;
            let lifetime = lifetime_ms
                .map(|ms| {
                    let raw = ms.max(0) as u64;
                    let clamped = if raw == 0 {
                        0
                    } else {
                        raw.max(MIN_SET_ACTIVE_POWER_LIFETIME_MS)
                    };
                    Duration::from_millis(clamped)
                })
                .unwrap_or_else(|| metadata.read().default_request_lifetime);
            w.add_timeout(id as u64, lifetime);
            Ok(true)
        },
    );
}

/// Mutation defuns the UI editor (and power-user REPL) call to
/// reshape the running MicrogridSite — remove a component, drop an edge,
/// rename for display.
///
/// Component arguments accept either a raw integer id or a
/// `ComponentHandle` (as returned by `make-*` calls), so paste
/// templates can pass bindings directly without an outer
/// `(component-id …)` wrapper.
fn register_world_ops(ctx: &mut TulispContext, router: SharedSiteRouter) {
    let r = router.clone();
    ctx.defun(
        "connect",
        move |parent: TulispObject, child: TulispObject| -> Result<bool, Error> {
            let parent = arg_to_component_id(&parent)?;
            let child = arg_to_component_id(&child)?;
            let w = r.site();
            // MicrogridSite::connect doesn't return a status; we always ack.
            w.connect(parent, child);
            Ok(true)
        },
    );
    let r = router.clone();
    ctx.defun(
        "remove-component",
        move |id: TulispObject| -> Result<bool, Error> {
            let id = arg_to_component_id(&id)?;
            Ok(r.site().remove_component(id))
        },
    );
    let r = router.clone();
    ctx.defun(
        "disconnect",
        move |parent: TulispObject, child: TulispObject| -> Result<bool, Error> {
            let parent = arg_to_component_id(&parent)?;
            let child = arg_to_component_id(&child)?;
            Ok(r.site().disconnect(parent, child))
        },
    );
    let r = router;
    ctx.defun(
        "rename-component",
        move |id: TulispObject, name: String| -> Result<bool, Error> {
            let id = arg_to_component_id(&id)?;
            r.site().rename(id, name);
            Ok(true)
        },
    );
}

/// Resolve a `connect` / `disconnect` / `remove-component` /
/// `rename-component` argument to a component id. Accepts a raw
/// integer (for REPL convenience) or a `ComponentHandle` (so pasted
/// `(let* ((m1 (make-…))) (connect m1 m2))` bodies don't need to
/// wrap each binding in `(component-id …)`).
fn arg_to_component_id(v: &TulispObject) -> Result<u64, Error> {
    use crate::sim::ComponentHandle;
    if let Ok(h) = ComponentHandle::try_from(v) {
        return Ok(h.id());
    }
    if let Ok(n) = v.as_int() {
        return Ok(n as u64);
    }
    Err(Error::type_mismatch(format!(
        "expected component id (integer) or handle, got {v}"
    )))
}

/// Filesystem helpers the override-file loader needs.
fn register_fs_helpers(ctx: &mut TulispContext, load_dir: PathBuf) {
    // Path resolution mirrors tulisp's `(load PATH)`: relative paths
    // are joined onto the config file's load dir, absolutes pass
    // through. `load-overrides` gates `(load <override-file>)` with
    // a `(file-exists-p …)` check; same base path keeps both calls
    // looking at the same file regardless of the process CWD.
    let exists_dir = load_dir.clone();
    ctx.defun("file-exists-p", move |path: String| -> bool {
        let p = Path::new(&path);
        let resolved = if p.is_absolute() {
            p.to_path_buf()
        } else {
            exists_dir.join(p)
        };
        resolved.exists()
    });
    // Iterate `microgrids/config.*.lisp` under the config's load
    // dir (excluding `*.overrides.lisp`) and `(load …)` each in
    // sorted order. The top-level config.lisp calls this after
    // setting enterprise-wide defaults; each per-mg file carries
    // its own `(make-microgrid …)` form so runtime-created
    // microgrids land in their own file rather than the entry
    // config.
    ctx.defun(
        "load-microgrid-configs",
        move |ctx: &mut TulispContext| -> Result<bool, Error> {
            let dir = load_dir.join("microgrids");
            let entries = match std::fs::read_dir(&dir) {
                Ok(it) => it,
                // Missing dir is a clean no-op — single-mg setups that
                // haven't migrated yet just stay on the old shape.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
                Err(e) => {
                    return Err(Error::os_error(format!(
                        "load-microgrid-configs: reading {}: {e}",
                        dir.display()
                    )));
                }
            };
            let mut paths: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                        return false;
                    };
                    name.starts_with("config.")
                        && name.ends_with(".lisp")
                        && !name.ends_with(".overrides.lisp")
                })
                .collect();
            paths.sort();
            for path in paths {
                let escaped = path
                    .to_string_lossy()
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"");
                ctx.eval_string(&format!("(load \"{escaped}\")"))?;
            }
            Ok(true)
        },
    );
}

fn register_reactive_setters(ctx: &mut TulispContext, router: SharedSiteRouter) {
    // Same opt-in convention as the make-* plist args:
    //   value > 0  → that constraint is active with this magnitude
    //   value ≤ 0  → that constraint is disabled
    // Mirrors what a SunSpec / IEEE 1547-2018 EMS pushes via Modbus.
    let r = router.clone();
    ctx.defun(
        "set-reactive-pf-limit",
        move |id: i64, k: f64| -> Result<bool, Error> {
            let w = r.site();
            match w.get(id as u64) {
                Some(c) => {
                    c.set_reactive_pf_limit(if k > 0.0 { Some(k as f32) } else { None });
                    Ok(true)
                }
                None => Err(Error::invalid_argument(format!(
                    "set-reactive-pf-limit: component {id} not found"
                ))),
            }
        },
    );

    let r = router;
    ctx.defun(
        "set-reactive-apparent-va",
        move |id: i64, va: f64| -> Result<bool, Error> {
            let w = r.site();
            match w.get(id as u64) {
                Some(c) => {
                    c.set_reactive_apparent_va(if va > 0.0 { Some(va as f32) } else { None });
                    Ok(true)
                }
                None => Err(Error::invalid_argument(format!(
                    "set-reactive-apparent-va: component {id} not found"
                ))),
            }
        },
    );
}

/// Register `(watch-file PATH)`. Adds PATH (resolved relative to
/// the entry-point config's directory) to the set of files notify
/// watches; edits to any of them trigger the same reload as edits to
/// the entry-point config.
///
/// One-shot semantics: paths are collected during the initial config
/// eval and handed to the notify watcher in `Config::watch`. New
/// `(watch-file …)` calls during a hot-reload accumulate but won't
/// be honoured until the process restarts.
pub(super) fn register_watches(
    ctx: &mut TulispContext,
    load_dir: PathBuf,
    extra_watches: Arc<Mutex<HashSet<PathBuf>>>,
) {
    ctx.defun("watch-file", move |path: String| -> Result<bool, Error> {
        let p = Path::new(&path);
        let resolved = if p.is_absolute() {
            p.to_path_buf()
        } else {
            load_dir.join(p)
        };
        // Canonicalize so dedup works regardless of how the user
        // wrote the path. Failing canonicalize == file doesn't
        // exist; surface that as an error so a typo doesn't
        // silently no-op.
        let canonical = resolved.canonicalize().map_err(|e| {
            Error::invalid_argument(format!(
                "watch-file {}: {} ({})",
                resolved.display(),
                e,
                "file does not exist or is unreadable"
            ))
        })?;
        extra_watches.lock().insert(canonical);
        Ok(true)
    });
}

fn register_load_drivers(ctx: &mut TulispContext, router: SharedSiteRouter) {
    // Drive a meter's `:power` slot from Lisp. Accepts a number, a
    // lambda, or a symbol — numeric values land as a constant
    // override (microsim-style timer-driven load curve); lambda /
    // symbol values install a DynamicScalar that the scheduler
    // re-resolves on every tick. UI's `:power` text input piggy-
    // backs on this: whatever the user types becomes the second
    // argument here.
    let r = router.clone();
    ctx.defun(
        "set-meter-power",
        move |id: i64, value: TulispObject| -> Result<bool, Error> {
            let w = r.site();
            let Some(c) = w.get(id as u64) else {
                return Err(Error::invalid_argument(format!(
                    "set-meter-power: component {id} not found"
                )));
            };
            if value.numberp() {
                let watts = f64::try_from(&value)?;
                c.set_active_power_override(watts as f32);
            } else if let Some(scalar) =
                crate::sim::dynamic_scalar::DynamicScalar::from_lisp(&value, 0.0)
            {
                c.set_active_power_source(scalar);
            } else {
                return Err(Error::invalid_argument(format!(
                    "set-meter-power: expected a number, lambda, or symbol — got {value}"
                )));
            }
            Ok(true)
        },
    );

    // PV analogue of set-meter-power. Same numeric / dynamic
    // dispatch — drives `(set-solar-sunlight id (lambda () …))` and
    // friends from scenarios or the UI. Per-tick `min-avail =
    // rated-lower × sunlight%/100` clamp picks up the new value on
    // the next refresh + tick pair.
    let r = router;
    ctx.defun(
        "set-solar-sunlight",
        move |id: i64, value: TulispObject| -> Result<bool, Error> {
            let w = r.site();
            let Some(c) = w.get(id as u64) else {
                return Err(Error::invalid_argument(format!(
                    "set-solar-sunlight: component {id} not found"
                )));
            };
            if value.numberp() {
                let pct = f64::try_from(&value)?;
                c.set_sunlight_pct(pct as f32);
            } else if let Some(scalar) =
                crate::sim::dynamic_scalar::DynamicScalar::from_lisp(&value, 100.0)
            {
                c.set_sunlight_source(scalar);
            } else {
                return Err(Error::invalid_argument(format!(
                    "set-solar-sunlight: expected a number, lambda, or symbol — got {value}"
                )));
            }
            Ok(true)
        },
    );
}

fn register_time_helpers(ctx: &mut TulispContext) {
    // chrono::Utc::now goes through the same clock_gettime(CLOCK_REALTIME)
    // syscall as std::time::SystemTime::now (both elide leap seconds the
    // same way the kernel does), but using chrono keeps these helpers
    // consistent with the rest of switchyard's time handling and lets us
    // extend with calendar-aware variants (seconds-since-midnight, etc.)
    // without swapping API later.

    // Wall-clock seconds since the Unix epoch as a float. Free-running
    // clock for time-driven load profiles.
    ctx.defun("now-seconds", || -> f64 {
        let now = chrono::Utc::now();
        now.timestamp() as f64 + now.timestamp_subsec_nanos() as f64 * 1e-9
    });

    // Seconds since the start of the most recent `window-secs`-aligned
    // window (anchored to the Unix epoch). For window-secs = 900,
    // returns 0..900 — equivalent to (mod (now-seconds) 900) but
    // expresses intent at the call site.
    ctx.defun("window-elapsed", |window_secs: f64| -> f64 {
        if window_secs <= 0.0 {
            return 0.0;
        }
        let now = chrono::Utc::now();
        let t = now.timestamp() as f64 + now.timestamp_subsec_nanos() as f64 * 1e-9;
        t.rem_euclid(window_secs)
    });
}

fn register_runtime_modes(ctx: &mut TulispContext, router: SharedSiteRouter) {
    use crate::sim::runtime::{CommandMode, Health, TelemetryMode};

    let r = router.clone();
    ctx.defun("set-component-health", move |id: i64, h: Health| -> bool {
        let w = r.site();
        w.set_health(id as u64, h);
        true
    });

    let r = router.clone();
    ctx.defun(
        "set-component-telemetry-mode",
        move |id: i64, m: TelemetryMode| -> bool {
            let w = r.site();
            w.set_telemetry_mode(id as u64, m);
            true
        },
    );

    let r = router.clone();
    ctx.defun(
        "set-component-command-mode",
        move |id: i64, m: CommandMode| -> bool {
            let w = r.site();
            w.set_command_mode(id as u64, m);
            true
        },
    );

    let r = router.clone();
    ctx.defun("cancel-all-streams", move || -> bool {
        // Server-side graceful cancel of every active stream. Each
        // streaming task sees the epoch bump on its next iteration and
        // exits, sending the client an EOF/CANCELLED. Clients reconnect
        // and resume on fresh streams.
        r.site().cancel_all_streams();
        true
    });

    let r = router;
    ctx.defun("set-sample-lag-ms", move |ms: i64| -> bool {
        // Shift every outgoing telemetry sample's timestamp into the
        // past by MS milliseconds. Models a server that delivers
        // samples with a fixed timestamp lag, e.g. to test how a
        // downstream resampler copes with stale data.
        r.site().set_sample_lag_ms(ms.max(0) as u64);
        true
    });
}

fn register_metadata(ctx: &mut TulispContext, metadata: Arc<RwLock<Metadata>>) {
    let m = metadata.clone();
    ctx.defun("set-enterprise-id", move |id: i64| -> Result<bool, Error> {
        m.write().enterprise_id = id as u64;
        Ok(true)
    });
    let m = metadata.clone();
    ctx.defun(
        "set-assets-socket-addr",
        move |addr: String| -> Result<bool, Error> {
            m.write().assets_socket_addr = addr;
            Ok(true)
        },
    );
    let m = metadata.clone();
    ctx.defun(
        "set-dispatch-socket-addr",
        move |addr: String| -> Result<bool, Error> {
            m.write().dispatch_socket_addr = addr;
            Ok(true)
        },
    );
    ctx.defun(
        "set-default-request-lifetime-ms",
        move |ms: i64| -> Result<bool, Error> {
            metadata.write().default_request_lifetime = Duration::from_millis(ms.max(0) as u64);
            Ok(true)
        },
    );
}

fn add_log_functions(ctx: &mut TulispContext) {
    use rand::Rng;
    ctx.defun("log.info", |msg: String| log::info!("{msg}"))
        .defun("log.warn", |msg: String| log::warn!("{msg}"))
        .defun("log.error", |msg: String| log::error!("{msg}"))
        .defun("log.debug", |msg: String| log::debug!("{msg}"))
        .defun("log.trace", |msg: String| log::trace!("{msg}"))
        // Math + RNG helpers used by ported microsim configs.
        .defun("ceiling", |n: f64| n.ceil() as i64)
        .defun("floor", |n: f64| n.floor() as i64)
        .defun("sin", |n: f64| n.sin())
        .defun("cos", |n: f64| n.cos())
        .defun("random", |limit: Option<i64>| {
            if let Some(limit) = limit {
                // `gen_range(0..n)` panics on an empty/inverted range, so a
                // non-positive limit (e.g. `(random (length '()))`) would
                // abort the eval; clamp so `(random n<=0)` yields 0.
                rand::thread_rng().gen_range(0..limit.max(1))
            } else {
                rand::thread_rng().r#gen()
            }
        });
}

tulisp::AsPlist! {
    /// Plist payload for `(set-frequency-model …)`. Every field
    /// optional — only the keys the caller passes are touched.
    pub struct FrequencyModelArgs {
        /// Mean the OU process pulls toward (Hz).
        nominal: Option<f64> {= None},
        /// Mean reversion rate (1/s). Correlation time of the
        /// noisy fluctuations is roughly `1 / mean-rev-rate`.
        mean_rev_rate<":mean-rev-rate">: Option<f64> {= None},
        /// Noise intensity (Hz/sqrt(s)). Equilibrium standard
        /// deviation is `sigma / sqrt(2 * mean-rev-rate)`.
        sigma: Option<f64> {= None},
    }
}

/// Defuns the config + scenarios use to drive the shared grid
/// frequency:
///
/// - `(set-frequency F)` — one-shot write of the current value.
///   The OU driver overwrites on the next step (every 200 ms), so
///   this is useful for test fixtures or for setting an initial
///   condition the OU then evolves away from.
/// - `(set-frequency-model :nominal :mean-rev-rate :sigma)` —
///   tune the *base* driver parameters. Each key optional;
///   unspecified keys keep their current base values. Defaults
///   pick a noise floor (~47 mHz std dev) and correlation time
///   (~20 s) that look like a healthy synchronous grid.
/// - `(override-frequency-model :nominal :mean-rev-rate :sigma)`
///   — install an override on the OU dynamics. Driver keeps
///   integrating, but uses the override's params in place of the
///   base while it's set. Unspecified keys inherit from the
///   current active model (override if already set, else base) —
///   so `(override-frequency-model :nominal 49.5)` pulls toward
///   49.5 with the base dynamics, and a later
///   `(override-frequency-model :sigma 0.05)` widens noise
///   without disturbing the override nominal.
/// - `(clear-frequency-override)` — drop the override; the
///   driver returns to base dynamics from the current value.
/// - `(current-frequency)` — read the live value.
pub(super) fn register_frequency(
    ctx: &mut TulispContext,
    state: crate::sim::frequency::SharedFrequency,
) {
    use crate::sim::frequency::FrequencyModel;
    fn apply_overrides(model: &mut FrequencyModel, a: &FrequencyModelArgs) {
        if let Some(v) = a.nominal {
            model.nominal_hz = v as f32;
        }
        if let Some(v) = a.mean_rev_rate {
            model.mean_rev_rate = v.max(0.0) as f32;
        }
        if let Some(v) = a.sigma {
            model.sigma = v.max(0.0) as f32;
        }
    }

    let s = state.clone();
    ctx.defun("set-frequency", move |hz: f64| -> Result<bool, Error> {
        s.write().current_hz = hz as f32;
        Ok(true)
    });

    let s = state.clone();
    ctx.defun(
        "set-frequency-model",
        move |args: tulisp::Plist<FrequencyModelArgs>| -> Result<bool, Error> {
            let a = args.into_inner();
            apply_overrides(&mut s.write().base, &a);
            Ok(true)
        },
    );

    let s = state.clone();
    ctx.defun(
        "override-frequency-model",
        move |args: tulisp::Plist<FrequencyModelArgs>| -> Result<bool, Error> {
            let a = args.into_inner();
            let mut g = s.write();
            // Missing keys inherit from the currently-active model:
            // the existing override if there is one (so repeated
            // calls layer), else the base (so the first call after a
            // clear picks up sensible defaults).
            let mut next = g.active_model();
            apply_overrides(&mut next, &a);
            g.override_model = Some(next);
            Ok(true)
        },
    );

    let s = state.clone();
    ctx.defun(
        "clear-frequency-override",
        move || -> Result<bool, Error> {
            s.write().override_model = None;
            Ok(true)
        },
    );

    let s = state;
    ctx.defun("current-frequency", move || -> Result<f64, Error> {
        Ok(s.read().read_hz() as f64)
    });
}

fn register_reset(ctx: &mut TulispContext, router: SharedSiteRouter) {
    // Rust-side: clear the active MicrogridSite's components. The
    // Lisp-side `reset-state` (in sim/common.lisp) wraps this and
    // also cancels any outstanding tulisp-async timers so the next
    // config load doesn't double-fire `every` callbacks.
    ctx.defun("reset-microgrid", move || -> Result<bool, Error> {
        router.site().reset();
        Ok(true)
    });
}

fn register_grid_state(ctx: &mut TulispContext, router: SharedSiteRouter) {
    let r = router.clone();
    ctx.defun(
        "set-voltage-per-phase",
        move |p1: f64, p2: f64, p3: f64| -> Result<bool, Error> {
            let w = r.site();
            let mut state = w.grid_state();
            state.voltage_per_phase = (p1 as f32, p2 as f32, p3 as f32);
            w.set_grid_state(state);
            Ok(true)
        },
    );

    let r = router;
    ctx.defun(
        "set-physics-tick-ms",
        move |ms: i64| -> Result<bool, Error> {
            let w = r.site();
            w.set_physics_tick_ms(ms.max(1) as u64);
            Ok(true)
        },
    );
}
