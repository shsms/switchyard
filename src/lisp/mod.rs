//! Lisp glue: load the config DSL, register the `make-*` functions
//! against a `World`, and act as the runtime entry point for the gRPC
//! server (which calls into us for `set_active_setpoint` and friends).
//!
//! The `Config` struct is intentionally thin — the simulation state
//! lives in `World`, the lisp interpreter is just the configuration
//! frontend.

pub mod handle;
pub mod make;

use std::{path::Path, time::Duration};

use notify::{RecommendedWatcher, Watcher};
use tulisp::{Error, SharedMut, TulispContext};

use crate::sim::World;

#[derive(Clone)]
pub struct Config {
    filename: String,
    pub(crate) ctx: SharedMut<TulispContext>,
    pub(crate) world: World,
}

impl Config {
    pub fn new(filename: &str) -> Self {
        let mut ctx = TulispContext::new();
        let world = World::new();

        let config_path = Path::new(filename);
        if let Some(p) = config_path.parent() {
            ctx.set_load_path(Some(p))
                .unwrap_or_else(|e| panic!("set_load_path({}): {:?}", p.display(), e));
        }

        register_runtime(&mut ctx, &world);

        if let Err(e) = ctx.eval_file(filename) {
            log::error!("Tulisp error:\n{}", e.format(&ctx));
        }

        Self {
            filename: filename.to_string(),
            ctx: SharedMut::new(ctx),
            world,
        }
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
fn register_runtime(ctx: &mut TulispContext, world: &World) {
    add_log_functions(ctx);
    handle::register(ctx);
    make::register(ctx, world.clone());
    register_reset(ctx, world.clone());
    register_grid_state(ctx, world.clone());
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
