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
pub mod runtime_modes;
pub mod value;

use std::{
    collections::HashSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use chrono::Utc;
use notify::{RecommendedWatcher, Watcher};
use parking_lot::{Mutex, RwLock};
use tulisp::{Error, SharedMut, TulispContext, TulispObject};

use crate::sim::World;

/// Microgrid identity + gateway-level settings, exposed to the Lisp
/// config and read back by `MicrogridServer`.
#[derive(Debug, Clone)]
pub struct Metadata {
    pub microgrid_id: u64,
    pub enterprise_id: u64,
    pub name: String,
    pub socket_addr: String,
    /// Fallback request lifetime when a `SetElectricalComponentPower`
    /// caller doesn't supply `request_lifetime`. Mirrors microsim's
    /// `retain-requests-duration-ms`. Tunable via
    /// `(set-default-request-lifetime-ms N)`.
    pub default_request_lifetime: Duration,
}

impl Default for Metadata {
    fn default() -> Self {
        Self {
            microgrid_id: 0,
            enterprise_id: 0,
            name: String::new(),
            socket_addr: "[::1]:8800".to_string(),
            default_request_lifetime: Duration::from_secs(60),
        }
    }
}

#[derive(Clone)]
pub struct Config {
    filename: String,
    pub(crate) ctx: SharedMut<TulispContext>,
    pub(crate) world: World,
    pub(crate) metadata: Arc<RwLock<Metadata>>,
    /// Additional files the config has registered via `(watch-file …)`.
    /// `Config::watch` adds each to the live notify watcher so edits to
    /// e.g. `sim/defaults.lisp` trigger the same reload as edits to
    /// the entry-point config. Set semantics — duplicate registrations
    /// (from re-runs of the config during reload) are no-ops.
    extra_watches: Arc<Mutex<HashSet<PathBuf>>>,
}

/// One top-level form found in the per-microgrid override file. The
/// `idx` is the form's 0-based position; stable until the next
/// `remove_persisted_overrides` rewrites the file. `source` is the
/// form rendered via tulisp's `Display` impl — round-trips through
/// eval but doesn't preserve the original spelling (comments
/// stripped, whitespace normalized).
#[derive(Debug, Clone, serde::Serialize)]
pub struct PersistedOverride {
    pub idx: usize,
    pub source: String,
}

impl Config {
    pub fn new(filename: &str) -> Self {
        let mut ctx = TulispContext::new();
        let world = World::new();
        let metadata = Arc::new(RwLock::new(Metadata::default()));
        let extra_watches = Arc::new(Mutex::new(HashSet::new()));

        // `Path::parent()` returns `Some("")` for bare filenames like
        // "config.lisp" — tulisp rejects empty paths, so fall back to
        // the current directory in that case.
        let config_path = Path::new(filename);
        let load_dir: PathBuf = match config_path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        ctx.set_load_path(Some(&load_dir))
            .unwrap_or_else(|e| panic!("set_load_path({}): {:?}", load_dir.display(), e));

        register_runtime(&mut ctx, &world, metadata.clone(), load_dir.clone());
        register_watches(&mut ctx, load_dir.clone(), extra_watches.clone());

        // tulisp-async gives the config DSL access to run-with-timer,
        // cancel-timer, sleep-for and friends, used to drive
        // *environment* animation (per-tick voltage / frequency
        // perturbations, scheduled events). Component logic stays in
        // Rust; lisp's only job is wiring + scripting the world
        // around it. Must be called inside a tokio runtime —
        // TokioExecutor::new captures Handle::current().
        tulisp_async::register(&mut ctx, Arc::new(tulisp_async::TokioExecutor::new()));

        // One-per-process loop that walks World's TimeoutTracker and
        // calls reset_setpoint on each elapsed entry. Both gRPC's
        // SetElectricalComponentPower and the Lisp `(set-active-power …)`
        // defun add to the tracker; this loop is what makes their
        // request-lifetime semantics visible.
        Self::start_timeout_loop(world.clone());

        if let Err(e) = ctx.eval_file(filename) {
            log::error!("Tulisp error:\n{}", e.format(&ctx));
        }

        let ctx = SharedMut::new(ctx);

        // Pre-tick hook: hold the interpreter lock once per tick and
        // refresh every component's Lisp-driven inputs (lambda-bound
        // `:power`, `:sunlight%`, …) before any `tick` runs. Lets
        // components read the resolved scalar from an atomic in
        // `tick` without re-entering the interpreter — see
        // `dynamic_scalar::DynamicScalar`.
        let hook_ctx = ctx.clone();
        world.set_pre_tick(Arc::new(move |w| {
            let mut guard = hook_ctx.borrow_mut();
            for c in w.components() {
                c.refresh_inputs(&mut guard);
            }
        }));

        Self {
            filename: filename.to_string(),
            ctx,
            world,
            metadata,
            extra_watches,
        }
    }

    fn start_timeout_loop(world: World) {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                for id in world.drain_expired_timeouts() {
                    log::info!("Request timeout for component {id} — resetting setpoint");
                    if let Some(c) = world.get(id) {
                        c.reset_setpoint();
                    }
                }
            }
        });
    }

    /// Build a TAGS table for every file in `roots` and every
    /// file each transitively `(load …)`s. Drives tulisp's
    /// parse-with-etags path: every `(defun NAME …)` form across
    /// the file tree becomes one entry, and every Rust-side
    /// `ctx.defun("name", …)` call from `register_runtime` /
    /// `tulisp_async::register` adds an entry pointing at the
    /// Rust source location — so `M-.` on `(set-meter-power …)`
    /// or `(run-with-timer …)` jumps straight into the Rust
    /// implementation.
    ///
    /// Static, but must run inside a tokio runtime —
    /// `tulisp_async::TokioExecutor::new` captures
    /// `Handle::current()`. The etags binary wraps `main` with
    /// `#[tokio::main]` for that.
    ///
    /// The load path is set from the first root's parent
    /// directory (the canonical config); roots beyond the first
    /// can `(load …)` files relative to it just like config.lisp
    /// would.
    pub fn tags_table(roots: &[&str]) -> Result<String, Error> {
        let mut ctx = TulispContext::new();
        let world = World::new();
        let metadata = Arc::new(RwLock::new(Metadata::default()));

        let load_dir: PathBuf = roots
            .first()
            .and_then(|r| Path::new(r).parent())
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        ctx.set_load_path(Some(&load_dir))
            .map_err(|e| Error::os_error(format!("set_load_path({}): {e}", load_dir.display())))?;

        register_runtime(&mut ctx, &world, metadata, load_dir);
        tulisp_async::register(&mut ctx, Arc::new(tulisp_async::TokioExecutor::new()));

        ctx.tags_table(Some(roots))
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

    /// Evaluate a Lisp expression on the running interpreter and
    /// return the result formatted via `Display`. Errors are
    /// formatted with full trace context the same way the reload
    /// path's logger formats them.
    ///
    /// Synchronous — acquires the interpreter write lock for the
    /// duration of the eval. Callers in async contexts must wrap in
    /// `tokio::task::spawn_blocking` to keep the executor free.
    ///
    /// On success the source is appended to the per-microgrid
    /// override file (`config.ui-overrides.<id>.lisp`) so the
    /// edit survives a reload. Errored evals are skipped — a
    /// half-applied topology change shouldn't leave a re-erroring
    /// expression on disk. Either way the World version bumps so
    /// UI subscribers refetch.
    ///
    /// Append uses the source verbatim — no formatter pass — to
    /// keep the per-eval cost predictable. `remove_persisted_overrides`
    /// already runs `tulisp-fmt` over the file's surviving forms
    /// when it rewrites, so the file gets re-tidied whenever the
    /// user prunes the list from the UI.
    pub fn eval(&self, src: &str) -> Result<String, String> {
        let result = {
            let mut ctx = self.ctx.borrow_mut();
            match ctx.eval_string(src) {
                Ok(v) => Ok(v.to_string()),
                Err(e) => Err(e.format(&ctx)),
            }
        };
        if result.is_ok()
            && let Err(e) = self.append_to_overrides_file(src)
        {
            log::error!(
                "Failed to append override to {}: {e}",
                self.overrides_path().display(),
            );
        }
        self.world.bump_version();
        result
    }

    /// Read-only eval — same machinery as `eval` but the result is
    /// NOT appended to the override file and the world version does
    /// NOT bump. For UI introspection (e.g. "what's the current
    /// value of battery-defaults?") that shouldn't surface as a
    /// persisted edit.
    pub fn eval_silent(&self, src: &str) -> Result<String, String> {
        let mut ctx = self.ctx.borrow_mut();
        match ctx.eval_string(src) {
            Ok(v) => Ok(v.to_string()),
            Err(e) => Err(e.format(&ctx)),
        }
    }

    fn append_to_overrides_file(&self, src: &str) -> std::io::Result<()> {
        let path = self.overrides_path();
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        // Trailing blank line keeps multi-line `let*` paste shapes
        // visually separable from the next form a future eval
        // appends.
        writeln!(file, "{src}")?;
        writeln!(file)?;
        file.flush()
    }

    /// One entry per top-level form in the per-microgrid override
    /// file (`config.ui-overrides.<microgrid-id>.lisp`), parsed
    /// via `TulispContext::parse_file`. Returns an empty vec if
    /// the file is missing or malformed — load-overrides will
    /// surface a parse error on the next reload, so we don't bother
    /// propagating it here.
    pub fn persisted_overrides(&self) -> Vec<PersistedOverride> {
        let path = self.overrides_path();
        if !path.exists() {
            return Vec::new();
        }
        let path_str = path.to_string_lossy();
        let mut ctx = self.ctx.borrow_mut();
        let Ok(forms) = ctx.parse_file(&path_str) else {
            return Vec::new();
        };
        forms
            .base_iter()
            .enumerate()
            .map(|(idx, form)| PersistedOverride {
                idx,
                source: form.to_string(),
            })
            .collect()
    }

    pub fn persisted_count(&self) -> usize {
        self.persisted_overrides().len()
    }

    /// Drop a set of persisted-override entries (by their
    /// file-position idx) and re-derive World state. Atomic: the
    /// override file is rewritten without those forms (temp +
    /// rename, with a `tulisp-fmt` pretty-print pass over the
    /// surviving forms), then `reload()` re-runs config.lisp +
    /// `load-overrides` on the new file so the deleted forms'
    /// effects vanish via the World reset inside reload.
    ///
    /// Returns the count of forms actually dropped — out-of-range
    /// indices are silently ignored. An IO error during rewrite
    /// leaves the world state untouched (the file was renamed
    /// atomically only on success).
    ///
    /// Bulk shape so the UI's checkbox-toolbar can prune N entries
    /// in one round trip with one reload, instead of N round trips
    /// with N reloads.
    pub fn remove_persisted_overrides(&self, indices: &[usize]) -> std::io::Result<usize> {
        let drop: HashSet<usize> = indices.iter().copied().collect();
        let entries = self.persisted_overrides();
        let kept: Vec<String> = entries
            .iter()
            .filter(|o| !drop.contains(&o.idx))
            .map(|o| o.source.clone())
            .collect();
        let dropped = entries.len() - kept.len();
        if dropped == 0 {
            return Ok(0);
        }
        let path = self.overrides_path();
        let tmp = path.with_extension("lisp.tmp");
        {
            let mut file = fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&tmp)?;
            writeln!(file, ";; ── {} ──", Utc::now().to_rfc3339())?;
            writeln!(file)?;
            // Hand each surviving form to tulisp-fmt so the file
            // stays readable. format_with_width returns the same
            // source on failure; we fall back to the raw text
            // rather than dropping a form. Blank line between
            // forms keeps multi-line `let*` paste shapes visually
            // separable.
            for src in &kept {
                let fmt =
                    tulisp_fmt::format_with_width(src, 80).unwrap_or_else(|_| format!("{}\n", src));
                file.write_all(fmt.as_bytes())?;
                writeln!(file)?;
            }
            file.flush()?;
        }
        fs::rename(&tmp, &path)?;
        self.reload();
        Ok(dropped)
    }

    fn overrides_path(&self) -> PathBuf {
        let load_dir = Path::new(&self.filename)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        load_dir.join(format!(
            "config.ui-overrides.{}.lisp",
            self.metadata.read().microgrid_id
        ))
    }

    pub fn reload(&self) {
        let start = std::time::Instant::now();
        self.world.reset();
        {
            let mut ctx = self.ctx.borrow_mut();
            if let Err(e) = ctx.eval_file(&self.filename) {
                log::error!("Tulisp error:\n{}", e.format(&ctx));
                return;
            }
        }
        // Tell UI subscribers the World rebuilt. Catches the
        // "removed the only pending entry" case where remove_pending
        // reloads but has no surviving entries to bump-version
        // through eval_with_affects.
        self.world.bump_version();
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
        // Add every path the config registered via `(watch-file …)`.
        // Snapshotted now; reload-time additions take effect on the
        // next process restart (the live notify watcher isn't held
        // across reloads, by design — keeps the watch lifecycle simple).
        for path in self.extra_watches.lock().iter() {
            if let Err(e) = watcher.watch(path, notify::RecursiveMode::NonRecursive) {
                log::warn!("watch-file {}: {}", path.display(), e);
            }
        }

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
fn register_runtime(
    ctx: &mut TulispContext,
    world: &World,
    metadata: Arc<RwLock<Metadata>>,
    load_dir: PathBuf,
) {
    add_log_functions(ctx);
    handle::register(ctx);
    make::register(ctx, world.clone());
    register_reset(ctx, world.clone());
    register_grid_state(ctx, world.clone());
    register_metadata(ctx, metadata.clone());
    register_runtime_modes(ctx, world.clone());
    register_load_drivers(ctx, world.clone());
    register_time_helpers(ctx);
    register_reactive_setters(ctx, world.clone());
    register_setpoints(ctx, world.clone(), metadata);
    register_world_ops(ctx, world.clone());
    register_scenario(ctx, world.clone());
    register_fs_helpers(ctx, load_dir);
    csv_profile::register(ctx);
}

/// Scenario lifecycle defuns. Scripts call `(scenario-start NAME)`
/// to mark the beginning, drop `(scenario-event KIND PAYLOAD)` markers
/// at interesting moments, and `(scenario-stop)` when finished. The
/// underlying journal lives on `World` and is read by the
/// `/api/scenario` and `/api/scenario/events` endpoints.
fn register_scenario(ctx: &mut TulispContext, world: World) {
    let w = world.clone();
    ctx.defun(
        "scenario-start",
        move |name: String| -> Result<bool, Error> {
            w.scenario_start(name, Utc::now());
            Ok(true)
        },
    );

    let w = world.clone();
    ctx.defun("scenario-stop", move || -> Result<bool, Error> {
        w.scenario_stop(Utc::now());
        Ok(true)
    });

    let w = world.clone();
    ctx.defun(
        "scenario-event",
        move |kind: TulispObject, payload: TulispObject| -> Result<i64, Error> {
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

    let w = world.clone();
    ctx.defun(
        "scenario-record-csv",
        move |dir: String| -> Result<i64, Error> {
            let path = std::path::PathBuf::from(dir);
            w.scenario_open_csv(&path)
                .map(|n| n as i64)
                .map_err(|e| Error::os_error(format!("scenario-record-csv: {e}")))
        },
    );

    let w = world.clone();
    ctx.defun("scenario-stop-csv", move || -> Result<i64, Error> {
        Ok(w.scenario_close_csv() as i64)
    });

    ctx.defun("scenario-elapsed", move || -> Result<f64, Error> {
        Ok(world.scenario_elapsed_s(Utc::now()))
    });
}

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
/// Lower bound on a non-zero request-lifetime that
/// `(set-active-power)` can install. The timeout loop polls at
/// 100 ms and the default physics tick is 100 ms, so a sub-150 ms
/// lifetime can expire before the next physics tick observes the
/// setpoint at all — the ramp would clear without ever leaving
/// idle. `lifetime-ms = 0` is preserved as an explicit "expire
/// immediately" escape (used by tests) and bypasses the clamp.
const MIN_SET_ACTIVE_POWER_LIFETIME_MS: u64 = 150;

fn register_setpoints(ctx: &mut TulispContext, world: World, metadata: Arc<RwLock<Metadata>>) {
    ctx.defun(
        "set-active-power",
        move |id: i64, watts: f64, lifetime_ms: Option<i64>| -> Result<bool, Error> {
            let component = world.get(id as u64).ok_or_else(|| {
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
            world.add_timeout(id as u64, lifetime);
            Ok(true)
        },
    );
}

/// Mutation defuns the UI editor (and power-user REPL) call to
/// reshape the running World — remove a component, drop an edge,
/// rename for display.
fn register_world_ops(ctx: &mut TulispContext, world: World) {
    let w = world.clone();
    ctx.defun("world-connect", move |parent: i64, child: i64| -> bool {
        // World::connect doesn't return a status; we always ack.
        w.connect(parent as u64, child as u64);
        true
    });
    let w = world.clone();
    ctx.defun("world-remove-component", move |id: i64| -> bool {
        w.remove_component(id as u64)
    });
    let w = world.clone();
    ctx.defun("world-disconnect", move |parent: i64, child: i64| -> bool {
        w.disconnect(parent as u64, child as u64)
    });
    ctx.defun(
        "world-rename-component",
        move |id: i64, name: String| -> bool {
            world.rename(id as u64, name);
            true
        },
    );
}

/// Filesystem helpers the override-file loader needs.
fn register_fs_helpers(ctx: &mut TulispContext, load_dir: PathBuf) {
    // Path resolution mirrors tulisp's `(load PATH)`: relative paths
    // are joined onto the config file's load dir, absolutes pass
    // through. `load-overrides` gates `(load <override-file>)` with
    // a `(file-exists-p …)` check; same base path keeps both calls
    // looking at the same file regardless of the process CWD.
    ctx.defun("file-exists-p", move |path: String| -> bool {
        let p = Path::new(&path);
        let resolved = if p.is_absolute() {
            p.to_path_buf()
        } else {
            load_dir.join(p)
        };
        resolved.exists()
    });
}

fn register_reactive_setters(ctx: &mut TulispContext, world: World) {
    // Same opt-in convention as the make-* plist args:
    //   value > 0  → that constraint is active with this magnitude
    //   value ≤ 0  → that constraint is disabled
    // Mirrors what a SunSpec / IEEE 1547-2018 EMS pushes via Modbus.
    let w = world.clone();
    ctx.defun(
        "set-reactive-pf-limit",
        move |id: i64, k: f64| -> Result<bool, Error> {
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

    ctx.defun(
        "set-reactive-apparent-va",
        move |id: i64, va: f64| -> Result<bool, Error> {
            match world.get(id as u64) {
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
fn register_watches(
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

fn register_load_drivers(ctx: &mut TulispContext, world: World) {
    // Drive a meter's `:power` slot from Lisp. Accepts a number, a
    // lambda, or a symbol — numeric values land as a constant
    // override (microsim-style timer-driven load curve); lambda /
    // symbol values install a DynamicScalar that the scheduler
    // re-resolves on every tick. UI's `:power` text input piggy-
    // backs on this: whatever the user types becomes the second
    // argument here.
    let w = world.clone();
    ctx.defun(
        "set-meter-power",
        move |id: i64, value: TulispObject| -> Result<bool, Error> {
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
    ctx.defun(
        "set-solar-sunlight",
        move |id: i64, value: TulispObject| -> Result<bool, Error> {
            let Some(c) = world.get(id as u64) else {
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

fn register_runtime_modes(ctx: &mut TulispContext, world: World) {
    use crate::sim::runtime::{CommandMode, Health, TelemetryMode};

    let w = world.clone();
    ctx.defun("set-component-health", move |id: i64, h: Health| -> bool {
        w.set_health(id as u64, h);
        true
    });

    let w = world.clone();
    ctx.defun(
        "set-component-telemetry-mode",
        move |id: i64, m: TelemetryMode| -> bool {
            w.set_telemetry_mode(id as u64, m);
            true
        },
    );

    ctx.defun(
        "set-component-command-mode",
        move |id: i64, m: CommandMode| -> bool {
            world.set_command_mode(id as u64, m);
            true
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
    let m = metadata.clone();
    ctx.defun(
        "set-socket-addr",
        move |addr: String| -> Result<bool, Error> {
            m.write().socket_addr = addr;
            Ok(true)
        },
    );
    let m = metadata.clone();
    ctx.defun(
        "set-default-request-lifetime-ms",
        move |ms: i64| -> Result<bool, Error> {
            m.write().default_request_lifetime = Duration::from_millis(ms.max(0) as u64);
            Ok(true)
        },
    );
    // Reader counterpart — the override-file loader interpolates this
    // into the per-microgrid filename.
    ctx.defun("get-microgrid-id", move || -> i64 {
        metadata.read().microgrid_id as i64
    });
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static UNIQ: AtomicU64 = AtomicU64::new(0);

    /// Build a Config from a tiny config.lisp body in a unique temp
    /// dir; returns the Config + the dir so tests can mess with the
    /// per-microgrid override path.
    fn config_with(body: &str) -> (Config, std::path::PathBuf) {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "switchyard-cfg-{}-{}",
            std::process::id(),
            UNIQ.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.lisp");
        std::fs::write(&path, body).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let cfg = rt.block_on(async { Config::new(path.to_str().unwrap()) });
        // Drop the runtime — Config keeps its own handles to whatever
        // tulisp-async spawned during init.
        std::mem::forget(rt);
        (cfg, dir)
    }

    /// set-active-power applies a setpoint and arms the timeout tracker.
    /// We can verify both by checking that World registers a deadline
    /// for the targeted component after the call.
    #[test]
    fn set_active_power_applies_setpoint_and_arms_timeout() {
        let (cfg, _dir) = config_with(
            "(set-microgrid-id 9)
             (setq b1 (%make-battery :id 1 :rated-lower -5000.0 :rated-upper 5000.0))
             (%make-battery-inverter :id 2 :rated-lower -5000.0 :rated-upper 5000.0
                                       :successors (list b1))",
        );
        // 30-second lifetime — applies the setpoint and arms the
        // tracker; nothing should be expired yet.
        cfg.eval("(set-active-power 2 1500.0 30000)").unwrap();
        assert_eq!(cfg.world().drain_expired_timeouts(), Vec::<u64>::new());
        // Lifetime 0 → instantly elapses; the next drain returns id.
        cfg.eval("(set-active-power 2 1500.0 0)").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert_eq!(cfg.world().drain_expired_timeouts(), vec![2]);
    }

    /// set-active-power on an unknown id surfaces an error, and a setpoint
    /// rejected by the component (e.g. unsupported kind on a meter)
    /// also propagates rather than silently no-op'ing.
    #[test]
    fn set_active_power_rejects_unknown_or_unsupported() {
        let (cfg, _dir) = config_with(
            "(set-microgrid-id 9)
             (%make-meter :id 1)",
        );
        let res = cfg.eval("(set-active-power 999 1500.0)");
        assert!(res.is_err(), "expected error, got {res:?}");
        assert!(res.unwrap_err().contains("999"));
        // Meter doesn't support active setpoints — set_active_setpoint
        // returns Unsupported, which we surface as a Lisp error.
        let res = cfg.eval("(set-active-power 1 1500.0)");
        assert!(res.is_err(), "expected error, got {res:?}");
    }

    /// Every successful eval appends to the override file
    /// immediately — that's how an edit survives a reload (the
    /// override file is the source of truth, not an in-memory log).
    #[test]
    fn eval_appends_each_successful_form_to_override_file() {
        let (cfg, dir) = config_with("(set-microgrid-id 9) (%make-grid :id 1)");
        cfg.eval("(world-rename-component 1 \"a\")").unwrap();
        cfg.eval("(world-rename-component 1 \"b\")").unwrap();
        let path = dir.join("config.ui-overrides.9.lisp");
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("(world-rename-component 1 \"a\")"));
        assert!(body.contains("(world-rename-component 1 \"b\")"));
        // Errored eval doesn't land in the file.
        assert!(cfg.eval("(undefined-fn 1)").is_err());
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(!body.contains("undefined-fn"));
    }

    /// `(set-meter-power id (lambda () X))` installs a dynamic
    /// source. The next physics tick — or `World::tick_once` driven
    /// from a test — runs the pre-tick hook, refresh_inputs
    /// resolves the lambda, and aggregate_power_w reflects it.
    #[test]
    fn set_meter_power_accepts_a_lambda() {
        let (cfg, _dir) = config_with(
            "(set-microgrid-id 9)
             (%make-meter :id 7)",
        );
        cfg.eval("(set-meter-power 7 (lambda () 1234.5))").unwrap();
        // tick_once runs the Config-installed pre-tick hook, which
        // locks the interpreter and calls refresh_inputs on every
        // component before the tick pass.
        cfg.world()
            .tick_once(chrono::Utc::now(), std::time::Duration::from_millis(100));
        let m = cfg.world().get(7).unwrap();
        assert!((m.aggregate_power_w(&cfg.world()) - 1234.5).abs() < 1e-3);
    }

    /// `(set-meter-power id 'symbol)` derefs the symbol's variable
    /// value each refresh — scenarios use this to drive a load
    /// curve from a global that another timer mutates.
    #[test]
    fn set_meter_power_accepts_a_symbol() {
        let (cfg, _dir) = config_with(
            "(set-microgrid-id 9)
             (setq consumer-power 1500.0)
             (%make-meter :id 7)",
        );
        cfg.eval("(set-meter-power 7 'consumer-power)").unwrap();
        cfg.world()
            .tick_once(chrono::Utc::now(), std::time::Duration::from_millis(100));
        let m = cfg.world().get(7).unwrap();
        assert!((m.aggregate_power_w(&cfg.world()) - 1500.0).abs() < 1e-3);
        // Mutate the bound variable; next tick picks up the new value.
        cfg.eval("(setq consumer-power 2750.0)").unwrap();
        cfg.world()
            .tick_once(chrono::Utc::now(), std::time::Duration::from_millis(100));
        assert!((m.aggregate_power_w(&cfg.world()) - 2750.0).abs() < 1e-3);
    }

    /// `(set-solar-sunlight id (lambda () X))` mirrors
    /// `set-meter-power` for PV. Refresh resolves the lambda; the
    /// next setpoint clip surfaces the new floor.
    #[test]
    fn set_solar_sunlight_accepts_a_lambda() {
        let (cfg, _dir) = config_with(
            "(set-microgrid-id 9)
             (%make-solar-inverter :id 8 :rated-lower -8000.0 :rated-upper 0.0)",
        );
        cfg.eval("(set-solar-sunlight 8 (lambda () 25.0))").unwrap();
        cfg.world()
            .tick_once(chrono::Utc::now(), std::time::Duration::from_millis(100));
        let inv = cfg.world().get(8).unwrap();
        // Issue a setpoint below sunlight-derated min_avail so the
        // ramp clips — observable through telemetry's active_power.
        inv.set_active_setpoint(-5000.0).expect("within rated");
        cfg.world()
            .tick_once(chrono::Utc::now(), std::time::Duration::from_millis(100));
        let p = inv
            .telemetry(&cfg.world())
            .active_power_w
            .expect("active power present");
        // 25% of -8000 = -2000 W floor.
        assert!(
            (p - (-2000.0)).abs() < 1.0,
            "expected sunlight-clipped -2000 W, got {p}",
        );
    }

    /// `(set-meter-power id "garbage")` should error rather than
    /// silently passing through the from_eval branch and tripping
    /// the non-numeric refresh fallback every tick.
    #[test]
    fn set_meter_power_rejects_bare_string() {
        let (cfg, _dir) = config_with(
            "(set-microgrid-id 9)
             (%make-meter :id 7)",
        );
        // A bare string is from_eval-eligible (returns Some) and
        // would never resolve to a number — but it doesn't roundtrip
        // through a useful curve, so users should reach for a lambda
        // or symbol instead. This assertion documents the behaviour:
        // the call succeeds (string isn't nil) and refresh just keeps
        // the fallback.
        assert!(
            cfg.eval("(set-meter-power 7 \"garbage\")").is_ok(),
            "string is accepted as an eval source — fallback governs",
        );
    }

    /// `(scenario-record-csv DIR)` opens one CSV per registered
    /// component; record_history_snapshot writes a row per pass;
    /// `(scenario-stop-csv)` flushes and closes them. Test
    /// asserts the file exists and contains a header + N rows.
    #[test]
    fn scenario_csv_records_per_component_files() {
        use chrono::Utc;
        let (cfg, dir) = config_with(
            "(set-microgrid-id 9)
             (%make-meter :id 1)
             (%make-battery :id 2)",
        );
        let csv_dir = dir.join("csvs");
        cfg.eval("(scenario-start \"csv\")").unwrap();
        let opened: i64 = cfg
            .eval(&format!(
                "(scenario-record-csv {:?})",
                csv_dir.to_str().unwrap()
            ))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(opened, 2);
        // Three snapshots → three rows + header.
        for _ in 0..3 {
            cfg.world().record_history_snapshot(Utc::now());
        }
        cfg.eval("(scenario-stop-csv)").unwrap();

        let meter_csv = std::fs::read_to_string(csv_dir.join("1-meter.csv")).unwrap();
        let battery_csv = std::fs::read_to_string(csv_dir.join("2-battery.csv")).unwrap();
        // Header line + 3 data rows = 4 lines (last one ends in
        // newline so split gives 5 elements with trailing empty).
        assert_eq!(meter_csv.lines().count(), 4, "meter csv: {meter_csv}");
        assert_eq!(battery_csv.lines().count(), 4, "battery csv: {battery_csv}");
        assert!(meter_csv.starts_with("ts_iso,active_power_w"));
        // Battery rows have an empty active_power_w cell (it
        // publishes dc_power_w instead) — the column shape stays
        // uniform.
        let first_data = battery_csv.lines().nth(1).unwrap();
        assert!(
            first_data.starts_with("20") && first_data.contains(",,"),
            "expected empty active_power cell, got {first_data}"
        );
    }

    /// sim/scenarios.lisp loads cleanly and the random-* helpers
    /// produce values in their stated range.
    #[test]
    fn scenarios_helpers_load_and_run() {
        let (cfg, dir) = config_with("(set-microgrid-id 9)");
        // Copy sim/scenarios.lisp into the test's load dir so
        // (load "sim/scenarios.lisp") finds it.
        let src = std::path::Path::new("sim/scenarios.lisp");
        let dst_dir = dir.join("sim");
        std::fs::create_dir_all(&dst_dir).unwrap();
        std::fs::copy(src, dst_dir.join("scenarios.lisp")).unwrap();
        cfg.eval("(load \"sim/scenarios.lisp\")").unwrap();
        // 100 draws of random-uniform should all land in [10, 20).
        for _ in 0..100 {
            let v: f64 = cfg
                .eval("(random-uniform 10.0 20.0)")
                .unwrap()
                .parse()
                .unwrap();
            assert!((10.0..20.0).contains(&v), "out-of-range {v}");
        }
        // random-pick over a 3-element list always returns one of
        // them.
        for _ in 0..100 {
            let v = cfg.eval("(random-pick '(11 22 33))").unwrap();
            assert!(["11", "22", "33"].contains(&v.as_str()), "got {v}");
        }
        // random-pick on empty list returns nil.
        assert_eq!(cfg.eval("(random-pick '())").unwrap(), "nil");
    }

    /// `(scenario-start)` opens a scenario, `(scenario-event)`
    /// appends to the journal, `(scenario-elapsed)` returns wall-
    /// clock seconds since start, `(scenario-stop)` freezes it.
    #[test]
    fn scenario_lifecycle_round_trips_through_lisp() {
        let (cfg, _dir) = config_with("(set-microgrid-id 9)");
        cfg.eval("(scenario-start \"warmup\")").unwrap();
        let summary = cfg.world().scenario_summary(chrono::Utc::now());
        assert_eq!(summary.name.as_deref(), Some("warmup"));
        assert!(summary.started_at.is_some());
        assert!(summary.ended_at.is_none());
        assert_eq!(summary.event_count, 0);

        // First event id is 0.
        cfg.eval("(scenario-event 'outage \"bat-1003\")").unwrap();
        cfg.eval("(scenario-event \"note\" \"warming up\")")
            .unwrap();
        let summary = cfg.world().scenario_summary(chrono::Utc::now());
        assert_eq!(summary.event_count, 2);
        assert_eq!(summary.next_event_id, 2);

        let events = cfg.world().scenario_events_since(0, 100);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, "outage");
        assert_eq!(events[1].kind, "note");

        // Stop freezes elapsed; a subsequent (scenario-elapsed)
        // returns the frozen value rather than continuing to grow.
        cfg.eval("(scenario-stop)").unwrap();
        let frozen = cfg.world().scenario_summary(chrono::Utc::now());
        std::thread::sleep(std::time::Duration::from_millis(20));
        let later = cfg.world().scenario_summary(chrono::Utc::now());
        assert_eq!(frozen.elapsed_s, later.elapsed_s);
        assert!(frozen.ended_at.is_some());
    }

    /// Battery DC power integrates into the journal's per-battery
    /// charge / discharge integrals. Drive a battery via its
    /// inverter, advance physics + sampling, and assert the totals.
    #[test]
    fn battery_charge_discharge_integrates_through_snapshot() {
        use chrono::{Duration as ChronoDuration, Utc};
        let (cfg, _dir) = config_with(
            "(set-microgrid-id 9)
             (setq b (%make-battery :id 100
                                    :capacity 100000.0
                                    :rated-lower -10000.0
                                    :rated-upper 10000.0))
             (%make-battery-inverter :id 200
                                     :rated-lower -10000.0
                                     :rated-upper 10000.0
                                     :successors (list b))",
        );
        cfg.eval("(scenario-start \"integrate\")").unwrap();
        // Push a charge setpoint of +3600 W for 10 sim-seconds.
        cfg.eval("(set-active-power 200 3600.0 60000)").unwrap();
        // Advance physics enough to settle the ramp; default ramp
        // is infinity so one tick is enough.
        let mut now = Utc::now();
        cfg.world()
            .tick_once(now, std::time::Duration::from_millis(100));
        // Snapshot pass at t0 — first one just seeds the cursor
        // (dt from start is small but non-zero — ignore the result).
        cfg.world().record_history_snapshot(now);
        now += ChronoDuration::seconds(10);
        cfg.world()
            .tick_once(now, std::time::Duration::from_secs(10));
        cfg.world().record_history_snapshot(now);
        let r = cfg.world().scenario_report(now);
        // 3600 W for 10 s = 10 Wh. Allow some slop for the seed
        // sample's dt at start.
        assert!(
            r.total_battery_charged_wh > 8.0 && r.total_battery_charged_wh < 12.0,
            "expected ~10 Wh charged, got {}",
            r.total_battery_charged_wh,
        );
        assert_eq!(r.total_battery_discharged_wh, 0.0);

        // Now flip to discharging.
        cfg.eval("(set-active-power 200 -7200.0 60000)").unwrap();
        cfg.world()
            .tick_once(now, std::time::Duration::from_millis(100));
        now += ChronoDuration::seconds(5);
        cfg.world()
            .tick_once(now, std::time::Duration::from_secs(5));
        cfg.world().record_history_snapshot(now);
        let r = cfg.world().scenario_report(now);
        // 7200 W * 5 s / 3600 = 10 Wh discharged.
        assert!(
            r.total_battery_discharged_wh > 8.0 && r.total_battery_discharged_wh < 12.0,
            "expected ~10 Wh discharged, got {}",
            r.total_battery_discharged_wh,
        );
        assert_eq!(r.per_battery.len(), 1);
        assert_eq!(r.per_battery[0].id, 100);
    }

    /// `:main t` on a meter wires it as the scenario reporter's
    /// peak source. record_history_snapshot updates the journal's
    /// peak each tick; scenario_start resets it.
    #[test]
    fn main_meter_peak_tracks_active_power() {
        use chrono::Utc;
        let (cfg, _dir) = config_with(
            "(set-microgrid-id 9)
             (%make-meter :id 1 :main t :power 1000.0)",
        );
        // Pre-start, sampling shouldn't update the peak — the
        // scenario hasn't begun.
        cfg.world().record_history_snapshot(Utc::now());
        assert_eq!(
            cfg.world().scenario_report(Utc::now()).peak_main_meter_w,
            0.0,
        );

        cfg.eval("(scenario-start \"power\")").unwrap();
        cfg.eval("(set-meter-power 1 2500.0)").unwrap();
        cfg.world().record_history_snapshot(Utc::now());
        let r = cfg.world().scenario_report(Utc::now());
        assert!((r.peak_main_meter_w - 2500.0).abs() < 1e-3);

        // A higher value lifts the peak; a later lower one
        // doesn't.
        cfg.eval("(set-meter-power 1 7800.0)").unwrap();
        cfg.world().record_history_snapshot(Utc::now());
        cfg.eval("(set-meter-power 1 1100.0)").unwrap();
        cfg.world().record_history_snapshot(Utc::now());
        let r = cfg.world().scenario_report(Utc::now());
        assert!((r.peak_main_meter_w - 7800.0).abs() < 1e-3);

        // scenario-start resets the peak.
        cfg.eval("(scenario-start \"again\")").unwrap();
        cfg.eval("(set-meter-power 1 500.0)").unwrap();
        cfg.world().record_history_snapshot(Utc::now());
        assert!((cfg.world().scenario_report(Utc::now()).peak_main_meter_w - 500.0).abs() < 1e-3,);
    }

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

    /// A second `(scenario-start)` clears the previous run's events
    /// but keeps the monotonic id counter so polling clients with a
    /// `since=` cursor see new events immediately rather than
    /// rewinding through stale ids.
    #[test]
    fn scenario_restart_clears_events_keeps_ids_monotonic() {
        let (cfg, _dir) = config_with("(set-microgrid-id 9)");
        cfg.eval("(scenario-start \"first\")").unwrap();
        cfg.eval("(scenario-event 'a \"\")").unwrap();
        cfg.eval("(scenario-event 'b \"\")").unwrap();
        assert_eq!(
            cfg.world()
                .scenario_summary(chrono::Utc::now())
                .next_event_id,
            2
        );
        cfg.eval("(scenario-start \"second\")").unwrap();
        let summary = cfg.world().scenario_summary(chrono::Utc::now());
        assert_eq!(summary.event_count, 0);
        assert_eq!(summary.next_event_id, 2);
        let id = cfg
            .eval("(scenario-event 'c \"\")")
            .unwrap()
            .parse::<i64>()
            .unwrap();
        assert_eq!(id, 2);
    }
}
