//! Lisp glue: load the config DSL, register the `make-*` functions
//! against a `World`, and act as the runtime entry point for the gRPC
//! server (which calls into us for `set_active_setpoint` and friends).
//!
//! The `Config` struct is intentionally thin — the simulation state
//! lives in `World`, the lisp interpreter is just the configuration
//! frontend.

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
        tulisp_async::register(
            &mut ctx,
            Arc::new(tulisp_async::TokioExecutor::new()),
        );

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
            .watch(Path::new(&self.filename), notify::RecursiveMode::NonRecursive)
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
    ctx.defun("set-microgrid-name", move |name: String| -> Result<bool, Error> {
        m.write().name = name;
        Ok(true)
    });
    ctx.defun("set-socket-addr", move |addr: String| -> Result<bool, Error> {
        metadata.write().socket_addr = addr;
        Ok(true)
    });
}

fn add_log_functions(ctx: &mut TulispContext) {
    ctx.defun("log.info", |msg: String| log::info!("{msg}"))
        .defun("log.warn", |msg: String| log::warn!("{msg}"))
        .defun("log.error", |msg: String| log::error!("{msg}"))
        .defun("log.debug", |msg: String| log::debug!("{msg}"))
        .defun("log.trace", |msg: String| log::trace!("{msg}"));
}

fn register_reset(ctx: &mut TulispContext, world: World) {
    ctx.defun("reset-state", move || -> Result<bool, Error> {
        world.reset();
        Ok(true)
    });
}

fn register_grid_state(ctx: &mut TulispContext, world: World) {
    let w = world.clone();
    ctx.defun(
        "set-frequency",
        move |hz: f64| -> Result<bool, Error> {
            let mut state = w.grid_state();
            state.frequency_hz = hz as f32;
            w.set_grid_state(state);
            Ok(true)
        },
    );

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
