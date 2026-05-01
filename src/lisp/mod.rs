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
    /// Indices into the on-disk override file that the user has
    /// marked × on but hasn't persisted yet. On the next persist
    /// the file is rewritten without those forms; on discard the
    /// set is cleared. Indices are file-position-stable until the
    /// next persist (which renumbers everything).
    pending_removals: Arc<Mutex<HashSet<usize>>>,
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
/// One top-level form found in the per-microgrid override file. The
/// `idx` is the form's 0-based position; stable until the next
/// `persist_pending` rewrites the file. `source` is the form
/// rendered via tulisp's `Display` impl — round-trips through eval
/// but doesn't preserve the original spelling (comments stripped,
/// whitespace normalized).
#[derive(Debug, Clone, serde::Serialize)]
pub struct PersistedOverride {
    pub idx: usize,
    pub source: String,
}

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

        // One-per-process loop that walks World's TimeoutTracker and
        // calls reset_setpoint on each elapsed entry. Both gRPC's
        // SetElectricalComponentPower and the Lisp `(set-active-power …)`
        // defun add to the tracker; this loop is what makes their
        // request-lifetime semantics visible.
        Self::start_timeout_loop(world.clone());

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
            pending_removals: Arc::new(Mutex::new(HashSet::new())),
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

    /// Snapshot of indices the user has marked × on but hasn't
    /// persisted yet. Indices match `persisted_overrides()` until
    /// the next persist rewrites the file.
    pub fn pending_removals(&self) -> HashSet<usize> {
        self.pending_removals.lock().clone()
    }

    /// Mark a persisted-override index for removal on the next
    /// persist. Idempotent — re-marking the same idx is a no-op.
    /// Bumps World::version so the chrome's pill/dialog refresh.
    pub fn mark_persisted_for_removal(&self, idx: usize) {
        self.pending_removals.lock().insert(idx);
        self.world.bump_version();
    }

    /// Undo a pending × on a persisted override. Idempotent.
    pub fn unmark_persisted_for_removal(&self, idx: usize) {
        self.pending_removals.lock().remove(&idx);
        self.world.bump_version();
    }

    /// Drop one pending entry by id and re-derive World state by
    /// reloading config.lisp + the override file, then re-evalling
    /// every remaining pending entry in order. Side effect: per-tick
    /// physics state (SoC integration, ramp positions) reset on
    /// reload — same trade-off `Discard` has. Returns true if an
    /// entry with that id was found and removed.
    ///
    /// Surviving entries keep their original ids and timestamps.
    /// Earlier passes re-used `eval_with_affects` and got fresh ids
    /// each time, which broke "user opens modal → sees ids → clicks
    /// × on id N" because by the time a click landed, id N had been
    /// recycled to a different entry.
    pub fn remove_pending(&self, id: u64) -> bool {
        let surviving: Vec<PendingEntry> = {
            let mut log = self.pending_log.lock();
            let len_before = log.len();
            let kept: Vec<PendingEntry> = log.drain(..).filter(|e| e.id != id).collect();
            if kept.len() == len_before {
                // Nothing matched — restore the log untouched.
                *log = kept;
                return false;
            }
            kept
        };
        // Reload re-runs config.lisp + the override file from scratch
        // — that's how we "remove" the deleted entry's effect.
        self.reload();
        // Re-apply each surviving entry directly: eval the source on
        // the interpreter (no eval_with_affects so the ids don't get
        // bumped), then push the original PendingEntry back into the
        // log. This preserves both id and timestamp.
        for entry in &surviving {
            let _ = {
                let mut ctx = self.ctx.borrow_mut();
                ctx.eval_string(&entry.source)
            };
        }
        *self.pending_log.lock() = surviving;
        // reload() already bumped the version, but the pending log
        // changed shape on top — bump again so subscribers refetch
        // /api/pending and see the new (smaller) list.
        self.world.bump_version();
        true
    }

    /// Rewrite `config.ui-overrides.<microgrid-id>.lisp` to reflect
    /// the desired post-persist state: the on-disk forms minus any
    /// indices the user × marked, plus the in-memory pending log.
    /// Both queues clear on success. No-op when both are empty.
    ///
    /// Atomicity: writes to a sibling `.tmp` then renames over the
    /// target. An IO failure (permission denied, disk full, parent
    /// dir gone) returns the error with the pending log + removal
    /// markers still intact — the user can retry. A concurrent push
    /// to the pending log during the write survives because we
    /// remove by id afterwards rather than clearing.
    ///
    /// `persisted` in the result counts the rewritten file's form
    /// total (kept on-disk + newly-flushed pending), which is what
    /// the UI's "N overrides" pill wants to land on.
    pub fn persist_pending(&self) -> std::io::Result<PersistResult> {
        let path = self.overrides_path();
        let entries: Vec<PendingEntry> = self.pending_log.lock().clone();
        let removals = self.pending_removals.lock().clone();
        if entries.is_empty() && removals.is_empty() {
            return Ok(PersistResult {
                persisted: self.persisted_count(),
                path: path.to_string_lossy().into_owned(),
            });
        }
        let kept: Vec<String> = self
            .persisted_overrides()
            .into_iter()
            .filter(|o| !removals.contains(&o.idx))
            .map(|o| o.source)
            .collect();
        let total = kept.len() + entries.len();
        let tmp = path.with_extension("lisp.tmp");
        {
            let mut file = fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&tmp)?;
            writeln!(file, ";; ── {} ──", Utc::now().to_rfc3339())?;
            // Hand each form to tulisp-fmt before writing so the file
            // stays readable to a human eyeballing it. format_with_width
            // returns the same source on failure; we fall back to the
            // raw text rather than dropping a form.
            for src in &kept {
                let fmt = tulisp_fmt::format_with_width(src, 80)
                    .unwrap_or_else(|_| format!("{}\n", src));
                file.write_all(fmt.as_bytes())?;
            }
            for entry in &entries {
                let fmt = tulisp_fmt::format_with_width(&entry.source, 80)
                    .unwrap_or_else(|_| format!("{}\n", entry.source));
                file.write_all(fmt.as_bytes())?;
            }
            file.flush()?;
        }
        fs::rename(&tmp, &path)?;
        // Only now is it safe to drop the pending state — file is
        // committed. Remove pending entries by id so a concurrent
        // push under a different id survives.
        let persisted_ids: std::collections::HashSet<u64> =
            entries.iter().map(|e| e.id).collect();
        self.pending_log
            .lock()
            .retain(|e| !persisted_ids.contains(&e.id));
        self.pending_removals.lock().clear();
        Ok(PersistResult {
            persisted: total,
            path: path.to_string_lossy().into_owned(),
        })
    }

    /// Drop the pending log + any × marks on persisted entries and
    /// trigger a reload — World state goes back to whatever the
    /// on-disk config + override file describe.
    pub fn discard_pending(&self) {
        self.pending_log.lock().clear();
        self.pending_removals.lock().clear();
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
    ///
    /// Snapshots the entries first, writes + flushes, only then
    /// removes them from the log. An IO error leaves the pending log
    /// intact — same data-loss safeguard as `persist_pending`.
    pub fn save_scenario(&self, name: &str) -> std::io::Result<PersistResult> {
        let entries: Vec<PendingEntry> = self.pending_log.lock().clone();
        let dir = self.scenarios_dir();
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{name}.lisp"));
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(file, "\n;; ── {} ──", Utc::now().to_rfc3339())?;
        for entry in &entries {
            let fmt = tulisp_fmt::format_with_width(&entry.source, 80)
                .unwrap_or_else(|_| format!("{}\n", entry.source));
            file.write_all(fmt.as_bytes())?;
        }
        file.flush()?;
        let persisted_ids: HashSet<u64> = entries.iter().map(|e| e.id).collect();
        self.pending_log
            .lock()
            .retain(|e| !persisted_ids.contains(&e.id));
        Ok(PersistResult {
            persisted: entries.len(),
            path: path.to_string_lossy().into_owned(),
        })
    }

    /// Load `scenarios/<name>.lisp` into the pending log. Atomic
    /// (parse failures and eval failures both leave the pending log
    /// untouched), and round-trips with `save_scenario`: a save of N
    /// pending entries → file with N forms → load → N new pending
    /// entries (one per top-level form), so a subsequent Persist
    /// rewrites them individually.
    pub fn load_scenario(&self, name: &str) -> Result<usize, String> {
        let path = self.scenarios_dir().join(format!("{name}.lisp"));
        if !path.exists() {
            return Err(format!("scenario {} not found", path.display()));
        }
        let path_str = path.to_string_lossy().into_owned();

        let sources: Vec<String> = {
            let mut ctx = self.ctx.borrow_mut();
            let forms = ctx
                .parse_file(&path_str)
                .map_err(|e| format!("parse {}: {}", path.display(), e.format(&ctx)))?;
            forms.base_iter().map(|f| f.to_string()).collect()
        };
        if sources.is_empty() {
            return Ok(0);
        }

        // Eval as one progn. tulisp doesn't roll world state back on
        // a mid-progn error (the first form's mutation sticks), so we
        // cover that ourselves: on error, reload the base config +
        // override file and replay the pre-load pending entries.
        let combined = sources.join("\n");
        let eval_err = {
            let mut ctx = self.ctx.borrow_mut();
            ctx.eval_string(&combined)
                .err()
                .map(|e| format!("eval {}: {}", path.display(), e.format(&ctx)))
        };
        if let Some(err) = eval_err {
            let surviving = self.pending_log.lock().clone();
            self.reload();
            for entry in &surviving {
                let _ = self.ctx.borrow_mut().eval_string(&entry.source);
            }
            return Err(err);
        }

        let now = Utc::now();
        let mut log = self.pending_log.lock();
        for src in &sources {
            let id = self
                .next_pending_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            log.push(PendingEntry {
                id,
                ts: now,
                source: src.clone(),
                affects: None,
            });
        }
        drop(log);
        self.world.bump_version();
        Ok(sources.len())
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
    register_metadata(ctx, metadata.clone());
    register_runtime_modes(ctx, world.clone());
    register_load_drivers(ctx, world.clone());
    register_time_helpers(ctx);
    register_reactive_setters(ctx, world.clone());
    register_setpoints(ctx, world.clone(), metadata);
    register_world_ops(ctx, world.clone());
    register_fs_helpers(ctx);
    csv_profile::register(ctx);
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
fn register_setpoints(
    ctx: &mut TulispContext,
    world: World,
    metadata: Arc<RwLock<Metadata>>,
) {
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
                .map(|ms| Duration::from_millis(ms.max(0) as u64))
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

    /// remove_pending must keep the surviving entries' ids stable.
    /// Earlier code re-evalled survivors via eval_with_affects which
    /// assigned fresh ids — the UI tracks entries by id, so a stale
    /// click on a recycled id would error or hit the wrong row.
    #[test]
    fn remove_pending_preserves_surviving_ids() {
        let (cfg, _dir) = config_with("(set-microgrid-id 9) (%make-grid :id 1)");
        cfg.eval("(world-rename-component 1 \"a\")").unwrap();
        cfg.eval("(world-rename-component 1 \"b\")").unwrap();
        cfg.eval("(world-rename-component 1 \"c\")").unwrap();
        let before: Vec<u64> = cfg.pending().iter().map(|e| e.id).collect();
        assert_eq!(before.len(), 3);

        // Remove the middle entry.
        assert!(cfg.remove_pending(before[1]));
        let after: Vec<u64> = cfg.pending().iter().map(|e| e.id).collect();
        assert_eq!(after.len(), 2);
        assert_eq!(after, vec![before[0], before[2]]);
    }

    /// Regression: deleting a setpoint pending entry (eg. a
    /// `(set-active-power …)` call from the REPL) shouldn't take
    /// out the unrelated component the user created right before
    /// it. Replay re-evals the surviving `(make-…)` entry, so the
    /// new component should reappear with its id intact.
    #[test]
    fn remove_pending_setpoint_does_not_drop_make_entry() {
        let (cfg, _dir) = config_with(
            "(set-microgrid-id 9)
             (setq b1 (%make-battery :id 1 :rated-lower -5000.0 :rated-upper 5000.0))
             (%make-battery-inverter :id 2 :rated-lower -5000.0 :rated-upper 5000.0
                                       :successors (list b1))",
        );
        // Add a battery via /api/eval-style flow, then arm a setpoint
        // on the inverter that already exists.
        cfg.eval("(%make-battery)").unwrap();
        let new_battery_ids: Vec<u64> = cfg
            .world()
            .components()
            .iter()
            .map(|c| c.id())
            .filter(|id| *id >= 1000)
            .collect();
        assert_eq!(new_battery_ids.len(), 1, "expected one new battery");
        let new_id = new_battery_ids[0];
        cfg.eval("(set-active-power 2 1500.0 30000)").unwrap();

        // Pull the setpoint pending-entry id and × it.
        let pending = cfg.pending();
        assert_eq!(pending.len(), 2);
        let setpoint_id = pending
            .iter()
            .find(|e| e.source.contains("set-active-power"))
            .unwrap()
            .id;
        assert!(cfg.remove_pending(setpoint_id));

        // The pending log should now hold just the make-battery
        // entry; the new battery should still be in the world with
        // the same id (deterministic id allocation across reload).
        let pending = cfg.pending();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].source.contains("%make-battery"));
        assert!(
            cfg.world().get(new_id).is_some(),
            "new battery (id {new_id}) should have survived setpoint removal",
        );
    }

    /// load_scenario must propagate Lisp errors. The previous
    /// `let _ = self.eval(&src)` quietly dropped them, so a scenario
    /// with bad syntax silently no-op'd.
    #[test]
    fn load_scenario_propagates_lisp_errors() {
        let (cfg, dir) = config_with("(set-microgrid-id 9)");
        let scenarios = dir.join("scenarios");
        std::fs::create_dir_all(&scenarios).unwrap();
        std::fs::write(scenarios.join("bad.lisp"), "(this-defun-doesnt-exist 1)").unwrap();

        let res = cfg.load_scenario("bad");
        assert!(res.is_err(), "expected error, got {res:?}");
        assert!(
            res.unwrap_err().contains("this-defun-doesnt-exist"),
            "error should name the bad symbol",
        );
    }

    /// A scenario with a good form followed by a bad form must roll
    /// back atomically — neither the good form's pending entry nor
    /// its world-state mutation can stick after the load fails.
    #[test]
    fn load_scenario_partial_failure_is_atomic() {
        let (cfg, dir) = config_with("(set-microgrid-id 9)");
        let scenarios = dir.join("scenarios");
        std::fs::create_dir_all(&scenarios).unwrap();
        std::fs::write(
            scenarios.join("mixed.lisp"),
            "(%make-grid :id 99) (this-defun-doesnt-exist 1)",
        )
        .unwrap();

        let res = cfg.load_scenario("mixed");
        assert!(res.is_err(), "expected error, got {res:?}");
        assert!(cfg.pending().is_empty(), "pending log should be untouched");
        assert!(
            cfg.world().get(99).is_none(),
            "first form must not have been applied",
        );
    }

    /// persist_pending must NOT clear the in-memory log on IO error,
    /// or pending edits vanish without ever reaching disk. Force the
    /// failure by pre-creating the override path as a directory:
    /// open(append) on a directory returns EISDIR.
    #[test]
    fn persist_io_error_keeps_pending_log_intact() {
        let (cfg, dir) = config_with("(set-microgrid-id 9)");
        let override_path = dir.join("config.ui-overrides.9.lisp");
        std::fs::create_dir_all(&override_path).unwrap();

        // Push an entry into the log.
        cfg.eval("(set-microgrid-name \"x\")").unwrap();
        assert_eq!(cfg.pending().len(), 1);

        // Persist must error and the entry must still be there.
        let res = cfg.persist_pending();
        assert!(res.is_err(), "expected IO error, got {res:?}");
        assert_eq!(
            cfg.pending().len(),
            1,
            "pending entry vanished on IO failure"
        );
    }
}
