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
use tulisp::{Error, SharedMut, TulispContext};

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
    /// In-memory log of successful UI evals since the last persist.
    /// On `/api/persist` this gets appended to the per-microgrid
    /// override file (`config.ui-overrides.<id>.lisp`) and cleared.
    /// On `/api/discard` it's cleared without writing — discard also
    /// triggers a reload so World state matches what's on disk.
    pending_log: Arc<Mutex<Vec<PendingEntry>>>,
    /// Monotonic counter for `PendingEntry.id`. Survives clears so
    /// the UI can use the id as a stable handle for "delete entry N"
    /// without worrying about reuse.
    next_pending_id: Arc<std::sync::atomic::AtomicU64>,
}

#[derive(Debug, serde::Serialize)]
pub struct PersistResult {
    pub persisted: usize,
    pub path: String,
}

/// One successfully-evaluated UI mutation. `id` is a monotonic
/// counter scoped to the Config — stable across the entry's lifetime
/// in the pending log so the UI can address it for delete. `affects`
/// is the component id the eval targets, if known (the UI tags its
/// own evals via the /api/eval `?affects=N` query param so the
/// inspector can show "current overrides for this component" without
/// parsing the source string).
#[derive(Debug, Clone, serde::Serialize)]
pub struct PendingEntry {
    pub id: u64,
    pub ts: chrono::DateTime<Utc>,
    pub source: String,
    pub affects: Option<u64>,
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

        register_runtime(&mut ctx, &world, metadata.clone());
        register_watches(&mut ctx, load_dir.clone(), extra_watches.clone());

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
            extra_watches,
            pending_log: Arc::new(Mutex::new(Vec::new())),
            next_pending_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
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

    /// Evaluate a Lisp expression on the running interpreter and
    /// return the result formatted via `Display`. Errors are
    /// formatted with full trace context the same way the reload
    /// path's logger formats them.
    ///
    /// Synchronous — acquires the interpreter write lock for the
    /// duration of the eval. Callers in async contexts must wrap in
    /// `tokio::task::spawn_blocking` to keep the executor free.
    ///
    /// On success the source is appended to the pending log so the
    /// UI's Persist button can flush it to the per-microgrid override
    /// file. Errored evals are skipped — a half-applied topology
    /// change shouldn't leave a re-erroring expression on disk.
    /// Either way the World version bumps so UI subscribers refetch.
    pub fn eval(&self, src: &str) -> Result<String, String> {
        self.eval_with_affects(src, None)
    }

    /// Like `eval`, but tags the resulting pending entry with the
    /// component id it affects. The UI's per-component "current
    /// overrides" list filters on this. Set to `None` for
    /// non-component-specific evals (defaults edits, REPL one-offs).
    pub fn eval_with_affects(
        &self,
        src: &str,
        affects: Option<u64>,
    ) -> Result<String, String> {
        let result = {
            let mut ctx = self.ctx.borrow_mut();
            match ctx.eval_string(src) {
                Ok(v) => Ok(v.to_string()),
                Err(e) => Err(e.format(&ctx)),
            }
        };
        if result.is_ok() {
            let id = self
                .next_pending_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.pending_log.lock().push(PendingEntry {
                id,
                ts: Utc::now(),
                source: src.to_string(),
                affects,
            });
        }
        self.world.bump_version();
        result
    }

    /// Read-only eval — same machinery as `eval` but the result is
    /// NOT appended to the pending log and the world version does NOT
    /// bump. For UI introspection (e.g. "what's the current value of
    /// battery-defaults?") that shouldn't surface as a persisted edit.
    pub fn eval_silent(&self, src: &str) -> Result<String, String> {
        let mut ctx = self.ctx.borrow_mut();
        match ctx.eval_string(src) {
            Ok(v) => Ok(v.to_string()),
            Err(e) => Err(e.format(&ctx)),
        }
    }

    /// Snapshot of the in-memory pending log. Each entry is one
    /// successfully-evaluated UI expression, oldest first.
    pub fn pending(&self) -> Vec<PendingEntry> {
        self.pending_log.lock().clone()
    }

    /// Drop one pending entry by id and re-derive World state by
    /// reloading config.lisp + the override file, then re-evalling
    /// every remaining pending entry in order. Side effect: per-tick
    /// physics state (SoC integration, ramp positions) reset on
    /// reload — same trade-off `Discard` has. Returns true if an
    /// entry with that id was found and removed.
    pub fn remove_pending(&self, id: u64) -> bool {
        let removed = {
            let mut log = self.pending_log.lock();
            let before = log.len();
            log.retain(|e| e.id != id);
            log.len() < before
        };
        if !removed {
            return false;
        }
        // Clone the surviving entries before reload — reload triggers
        // any timers + config evaluation that could touch the log,
        // and we want to control re-application explicitly.
        let surviving: Vec<PendingEntry> = std::mem::take(&mut *self.pending_log.lock());
        self.reload();
        for e in surviving {
            // Re-eval, preserving original affects/id semantics. We
            // re-use `eval_with_affects` so the entries land in the
            // log again — but with fresh ids (the old id pointed to
            // the now-removed entry).
            let _ = self.eval_with_affects(&e.source, e.affects);
        }
        true
    }

    /// Append the pending log to `config.ui-overrides.<microgrid-id>
    /// .lisp` (creating the file if needed) and clear the in-memory
    /// log. Each persist batch is preceded by an ISO-8601 timestamp
    /// comment. No-op when the log is empty.
    pub fn persist_pending(&self) -> std::io::Result<PersistResult> {
        let entries = std::mem::take(&mut *self.pending_log.lock());
        let path = self.overrides_path();
        if entries.is_empty() {
            return Ok(PersistResult {
                persisted: 0,
                path: path.to_string_lossy().into_owned(),
            });
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(file, "\n;; ── {} ──", Utc::now().to_rfc3339())?;
        for entry in &entries {
            writeln!(file, "{}", entry.source)?;
        }
        Ok(PersistResult {
            persisted: entries.len(),
            path: path.to_string_lossy().into_owned(),
        })
    }

    /// Drop the pending log and trigger a reload — World state goes
    /// back to whatever the on-disk config + override file describe.
    pub fn discard_pending(&self) {
        self.pending_log.lock().clear();
        self.reload();
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

    fn scenarios_dir(&self) -> PathBuf {
        let load_dir = Path::new(&self.filename)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        load_dir.join("scenarios")
    }

    /// List scenarios (just the basenames without the `.lisp` ext)
    /// available under `<load-dir>/scenarios/`. Returns an empty Vec
    /// if the directory doesn't exist.
    pub fn list_scenarios(&self) -> std::io::Result<Vec<String>> {
        let dir = self.scenarios_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut names = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("lisp") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    names.push(stem.to_string());
                }
            }
        }
        names.sort();
        Ok(names)
    }

    /// Save the current pending log to `scenarios/<name>.lisp` +
    /// clear the log. Like `persist_pending` but the destination is
    /// a named scenario file instead of the per-microgrid override.
    /// Useful for capturing a recent series of edits as a reusable
    /// recipe ("EV-fault-during-cloud-cover", "battery-bypass", …).
    pub fn save_scenario(&self, name: &str) -> std::io::Result<PersistResult> {
        let entries = std::mem::take(&mut *self.pending_log.lock());
        let dir = self.scenarios_dir();
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{name}.lisp"));
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(file, "\n;; ── {} ──", Utc::now().to_rfc3339())?;
        for entry in &entries {
            writeln!(file, "{}", entry.source)?;
        }
        Ok(PersistResult {
            persisted: entries.len(),
            path: path.to_string_lossy().into_owned(),
        })
    }

    /// Load `scenarios/<name>.lisp` and eval its contents — every
    /// expression goes through `eval`, so each one lands in the
    /// pending log too. The user can then Persist (to apply
    /// permanently) or Discard.
    pub fn load_scenario(&self, name: &str) -> std::io::Result<usize> {
        let path = self.scenarios_dir().join(format!("{name}.lisp"));
        let src = fs::read_to_string(&path)?;
        // tulisp's eval_string handles a multi-form file by treating
        // them as a progn. We want each form to land in the pending
        // log individually, so wrap in a (progn ...) — eval logs the
        // whole string verbatim, which still re-runs cleanly.
        let _ = self.eval(&src);
        Ok(src.lines().count())
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
            if let Err(e) =
                watcher.watch(path, notify::RecursiveMode::NonRecursive)
            {
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
    register_reactive_setters(ctx, world.clone());
    register_world_ops(ctx, world.clone());
    register_fs_helpers(ctx);
    csv_profile::register(ctx);
}

/// Mutation defuns the UI editor (and power-user REPL) call to
/// reshape the running World — remove a component, drop an edge,
/// rename for display.
fn register_world_ops(ctx: &mut TulispContext, world: World) {
    let w = world.clone();
    ctx.defun(
        "world-connect",
        move |parent: i64, child: i64| -> bool {
            // World::connect doesn't return a status; we always ack.
            w.connect(parent as u64, child as u64);
            true
        },
    );
    let w = world.clone();
    ctx.defun(
        "world-remove-component",
        move |id: i64| -> bool { w.remove_component(id as u64) },
    );
    let w = world.clone();
    ctx.defun(
        "world-disconnect",
        move |parent: i64, child: i64| -> bool { w.disconnect(parent as u64, child as u64) },
    );
    ctx.defun(
        "world-rename-component",
        move |id: i64, name: String| -> bool {
            world.rename(id as u64, name);
            true
        },
    );
}

/// Filesystem helpers the override-file loader needs.
fn register_fs_helpers(ctx: &mut TulispContext) {
    // Resolves relative to the current working directory, same as
    // tulisp's (load PATH). Returns t/nil — used by load-overrides
    // to no-op on a fresh checkout where the override file doesn't
    // exist yet.
    ctx.defun("file-exists-p", |path: String| -> bool {
        Path::new(&path).exists()
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
    ctx.defun(
        "watch-file",
        move |path: String| -> Result<bool, Error> {
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
        },
    );
}

fn register_load_drivers(ctx: &mut TulispContext, world: World) {
    // Push an explicit active-power value into a meter's
    // fixed-power override. Calling this every N ms inside an
    // `(every)` callback gives Lisp a way to drive any load curve
    // (computed from a function or interpolated from a CSV) without
    // teaching Rust how to read CSV files.
    let w = world.clone();
    ctx.defun(
        "set-meter-power",
        move |id: i64, watts: f64| -> Result<bool, Error> {
            match w.get(id as u64) {
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

    // Cloud-cover schedule: drive a solar inverter's `sunlight%` from
    // a Lisp timer the same way `(set-meter-power)` drives a meter's
    // fixed-power override. Per-tick `min-avail = rated-lower ×
    // sunlight%/100` clamp picks up the new value on the next tick.
    ctx.defun(
        "set-solar-sunlight",
        move |id: i64, pct: f64| -> Result<bool, Error> {
            match world.get(id as u64) {
                Some(c) => {
                    c.set_sunlight_pct(pct as f32);
                    Ok(true)
                }
                None => Err(Error::invalid_argument(format!(
                    "set-solar-sunlight: component {id} not found"
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

    let w = world.clone();
    ctx.defun(
        "set-component-health",
        move |id: i64, h: Health| -> bool {
            w.set_health(id as u64, h);
            true
        },
    );

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
