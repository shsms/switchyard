//! Lisp glue: load the config DSL, register the `make-*` functions
//! against a `World`, and act as the runtime entry point for the gRPC
//! server (which calls into us for `set_active_setpoint` and friends).
//!
//! The `Config` struct is intentionally thin — the simulation state
//! lives in `World`, the lisp interpreter is just the configuration
//! frontend.

pub mod csv_profile;
pub mod handle;
pub mod make;

use std::{path::Path, sync::Arc, time::Duration};

use notify::{RecommendedWatcher, Watcher};
use parking_lot::RwLock;
use tulisp::{Error, SharedMut, TulispContext};

use crate::sim::World;

/// Microgrid identity metadata. Kept minimal for now — the full set
/// (delivery area, EIC, location) lands when the server needs it.
#[derive(Debug, Clone, Default)]
pub struct Metadata {
    pub microgrid_id: u64,
    pub enterprise_id: u64,
    pub name: String,
    pub socket_addr: String,
}

#[derive(Clone)]
pub struct Config {
    filename: String,
    pub(crate) ctx: SharedMut<TulispContext>,
    pub(crate) world: World,
    pub(crate) metadata: Arc<RwLock<Metadata>>,
}

impl Config {
    pub fn new(filename: &str) -> Self {
        let mut ctx = TulispContext::new();
        let world = World::new();
        let metadata = Arc::new(RwLock::new(Metadata {
            socket_addr: "[::1]:8800".to_string(),
            ..Default::default()
        }));

        // `Path::parent()` returns `Some("")` for bare filenames like
        // "config.lisp" — tulisp rejects empty paths, so fall back to
        // the current directory in that case.
        let config_path = Path::new(filename);
        let load_dir: &Path = match config_path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        ctx.set_load_path(Some(load_dir))
            .unwrap_or_else(|e| panic!("set_load_path({}): {:?}", load_dir.display(), e));

        register_runtime(&mut ctx, &world, metadata.clone());

        // tulisp-async gives the config DSL access to run-with-timer,
        // cancel-timer, sleep-for and friends, used to drive
        // *environment* animation (per-tick voltage / frequency
        // perturbations, scheduled events). Component logic stays in
        // Rust; lisp's only job is wiring + scripting the world
        // around it. Must be called inside a tokio runtime —
        // TokioExecutor::new captures Handle::current().
        tulisp_async::register(&mut ctx, Arc::new(tulisp_async::TokioExecutor::new()));

        if let Err(e) = ctx.eval_file(filename) {
            log::error!("Tulisp error:\n{}", e.format(&ctx));
        }

        Self {
            filename: filename.to_string(),
            ctx: SharedMut::new(ctx),
            world,
            metadata,
        }
    }

    pub fn metadata(&self) -> Metadata {
        self.metadata.read().clone()
    }

    pub fn socket_addr(&self) -> String {
        self.metadata.read().socket_addr.clone()
    }

    pub fn world(&self) -> World {
        self.world.clone()
    }

    pub fn reload(&self) {
        let start = std::time::Instant::now();
        self.world.reset();
        let mut ctx = self.ctx.borrow_mut();
        if let Err(e) = ctx.eval_file(&self.filename) {
            log::error!("Tulisp error:\n{}", e.format(&ctx));
            return;
        }
        log::info!(
            "Reloaded config in {:.1}ms",
            start.elapsed().as_secs_f64() * 1000.0
        );
    }

    pub async fn watch(self) {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let mut watcher = RecommendedWatcher::new(
            move |res| {
                futures::executor::block_on(async {
                    let _ = tx.send(res).await;
                });
            },
            notify::Config::default(),
        )
        .unwrap();
        watcher
            .watch(
                Path::new(&self.filename),
                notify::RecursiveMode::NonRecursive,
            )
            .unwrap();

        while let Some(res) = rx.recv().await {
            match res {
                Ok(event) => {
                    if let notify::EventKind::Modify(_) = event.kind {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        self.reload();
                    }
                }
                Err(e) => {
                    log::error!("watch error: {:?}", e);
                    return;
                }
            }
        }
    }
}

/// Register every Rust function the config DSL needs.
fn register_runtime(ctx: &mut TulispContext, world: &World, metadata: Arc<RwLock<Metadata>>) {
    add_log_functions(ctx);
    handle::register(ctx);
    make::register(ctx, world.clone());
    register_reset(ctx, world.clone());
    register_grid_state(ctx, world.clone());
    register_metadata(ctx, metadata);
    register_runtime_modes(ctx, world.clone());
    register_load_drivers(ctx, world.clone());
    register_time_helpers(ctx);
    csv_profile::register(ctx);
}

fn register_load_drivers(ctx: &mut TulispContext, world: World) {
    // Push an explicit active-power value into a meter's
    // fixed-power override. Calling this every N ms inside an
    // `(every)` callback gives Lisp a way to drive any load curve
    // (computed from a function or interpolated from a CSV) without
    // teaching Rust how to read CSV files.
    ctx.defun(
        "set-meter-power",
        move |id: i64, watts: f64| -> Result<bool, Error> {
            match world.get(id as u64) {
                Some(c) => {
                    c.set_active_power_override(watts as f32);
                    Ok(true)
                }
                None => Err(Error::invalid_argument(format!(
                    "set-meter-power: component {id} not found"
                ))),
            }
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

fn register_runtime_modes(ctx: &mut TulispContext, world: World) {
    use crate::sim::runtime::{CommandMode, Health, TelemetryMode};
    use std::str::FromStr;

    let w = world.clone();
    ctx.defun(
        "set-component-health",
        move |id: i64, label: String| -> Result<bool, Error> {
            let h = Health::from_str(&label).map_err(|_| {
                Error::invalid_argument(format!(
                    "unknown health label '{label}'; expected ok/error/standby"
                ))
            })?;
            w.set_health(id as u64, h);
            Ok(true)
        },
    );

    let w = world.clone();
    ctx.defun(
        "set-component-telemetry-mode",
        move |id: i64, label: String| -> Result<bool, Error> {
            let m = TelemetryMode::from_str(&label).map_err(|_| {
                Error::invalid_argument(format!(
                    "unknown telemetry mode '{label}'; expected normal/silent/closed"
                ))
            })?;
            w.set_telemetry_mode(id as u64, m);
            Ok(true)
        },
    );

    ctx.defun(
        "set-component-command-mode",
        move |id: i64, label: String| -> Result<bool, Error> {
            let m = CommandMode::from_str(&label).map_err(|_| {
                Error::invalid_argument(format!(
                    "unknown command mode '{label}'; expected normal/timeout/error"
                ))
            })?;
            world.set_command_mode(id as u64, m);
            Ok(true)
        },
    );
}

fn register_metadata(ctx: &mut TulispContext, metadata: Arc<RwLock<Metadata>>) {
    let m = metadata.clone();
    ctx.defun("set-microgrid-id", move |id: i64| -> Result<bool, Error> {
        m.write().microgrid_id = id as u64;
        Ok(true)
    });
    let m = metadata.clone();
    ctx.defun("set-enterprise-id", move |id: i64| -> Result<bool, Error> {
        m.write().enterprise_id = id as u64;
        Ok(true)
    });
    let m = metadata.clone();
    ctx.defun(
        "set-microgrid-name",
        move |name: String| -> Result<bool, Error> {
            m.write().name = name;
            Ok(true)
        },
    );
    ctx.defun(
        "set-socket-addr",
        move |addr: String| -> Result<bool, Error> {
            metadata.write().socket_addr = addr;
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
        .defun("random", |limit: Option<i64>| {
            if let Some(limit) = limit {
                rand::thread_rng().gen_range(0..limit)
            } else {
                rand::thread_rng().r#gen()
            }
        });
}

fn register_reset(ctx: &mut TulispContext, world: World) {
    // Rust-side: clear the World registry. The Lisp-side `reset-state`
    // (in sim/common.lisp) wraps this and also cancels any
    // outstanding tulisp-async timers so the next config load doesn't
    // double-fire `every` callbacks.
    ctx.defun("world-reset", move || -> Result<bool, Error> {
        world.reset();
        Ok(true)
    });
}

fn register_grid_state(ctx: &mut TulispContext, world: World) {
    let w = world.clone();
    ctx.defun("set-frequency", move |hz: f64| -> Result<bool, Error> {
        let mut state = w.grid_state();
        state.frequency_hz = hz as f32;
        w.set_grid_state(state);
        Ok(true)
    });

    let w = world.clone();
    ctx.defun(
        "set-voltage-per-phase",
        move |p1: f64, p2: f64, p3: f64| -> Result<bool, Error> {
            let mut state = w.grid_state();
            state.voltage_per_phase = (p1 as f32, p2 as f32, p3 as f32);
            w.set_grid_state(state);
            Ok(true)
        },
    );

    ctx.defun(
        "set-physics-tick-ms",
        move |ms: i64| -> Result<bool, Error> {
            world.set_physics_tick_ms(ms.max(1) as u64);
            Ok(true)
        },
    );
}
