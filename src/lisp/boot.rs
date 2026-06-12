//! `Config` bootstrap and lifecycle: build the interpreter, eval
//! the config file, spawn the long-lived loops (Lisp refresh + the
//! request-timeout sweep + scenario auto-advance), and the hot-
//! reload + tags-pass entry points.
//!
//! Everything in this file is an `impl Config { ... }` (or a free
//! helper Config relies on) so that the heavy bootstrap logic
//! doesn't sit in the parent module alongside the trivial getters.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::{RecommendedWatcher, Watcher};
use parking_lot::{Mutex, RwLock};
use tokio::sync::broadcast;
use tulisp::{Error, SharedMut, TulispContext};

use crate::sim::MicrogridSite;
use crate::sim::microgrids::SiteRouter;

use super::{Config, Metadata, defuns};

impl Config {
    /// Build a config from `filename`. Returns the formatted lisp
    /// error on parse / eval failure — caller decides whether to
    /// panic (binary boot) or surface in the UI (hot reload). On
    /// error the site is left empty (no components registered)
    /// rather than partially built; the caller is expected to retry
    /// or abort.
    pub fn new(filename: &str) -> Result<Self, String> {
        use std::sync::atomic::AtomicU64;
        let mut ctx = TulispContext::new();
        let enterprise_id_allocator =
            Arc::new(AtomicU64::new(crate::sim::component::FIRST_AUTO_ID));
        let site = MicrogridSite::with_id_allocator(enterprise_id_allocator.clone());
        let metadata = Arc::new(RwLock::new(Metadata::default()));
        let extra_watches = Arc::new(Mutex::new(HashSet::new()));
        let graph_status: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(None));
        let clock = crate::sim::clock::new_clock();
        let scenarios = crate::sim::scenarios::new_registry();
        let microgrids = crate::sim::microgrids::new_registry();
        let dispatches = crate::sim::dispatch::new_store();
        let current_microgrid = crate::sim::microgrids::new_current_microgrid();
        let router = SiteRouter::new(microgrids.clone(), current_microgrid.clone(), site.clone());
        // Capacity = 1024 to absorb a mass-create burst (e.g. a
        // script POST'ing /api/microgrids/create a few hundred times
        // back-to-back) without lagging the WS event pump's
        // receiver. Even on Lagged the pump re-snapshots the
        // registry and back-fills forwarders, so capacity tuning is
        // belt-and-suspenders — but a fresh subscriber spinning up
        // mid-burst still benefits from the extra slack.
        let microgrid_registered = Arc::new(broadcast::channel(1024).0);
        // Enterprise-wide grid frequency state — one OU process drives
        // every MicrogridSite in the registry so they share the
        // physically-correct same frequency. The driver task is
        // spawned below; bootstrap site + future make-microgrid forms
        // both attach to this slot.
        let grid_frequency = crate::sim::frequency::new_shared();
        site.set_grid_frequency(grid_frequency.clone());
        crate::sim::frequency::spawn_driver(grid_frequency.clone());

        // `Path::parent()` returns `Some("")` for bare filenames like
        // "config.lisp" — tulisp rejects empty paths, so fall back to
        // the current directory in that case.
        let config_path = Path::new(filename);
        let load_dir: PathBuf = match config_path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        ctx.set_load_path(Some(&load_dir))
            .map_err(|e| format!("set_load_path({}): {e}", load_dir.display()))?;

        defuns::register_runtime(
            &mut ctx,
            router.clone(),
            metadata.clone(),
            load_dir.clone(),
            microgrids.clone(),
        );
        defuns::register_clock(&mut ctx, clock.clone());
        defuns::register_watches(&mut ctx, load_dir.clone(), extra_watches.clone());
        defuns::register_scenarios(&mut ctx, scenarios.clone());
        defuns::register_microgrids(
            &mut ctx,
            microgrids.clone(),
            router.clone(),
            current_microgrid.clone(),
            enterprise_id_allocator.clone(),
            microgrid_registered.clone(),
            grid_frequency.clone(),
        );
        defuns::register_frequency(&mut ctx, grid_frequency.clone());

        // tulisp-async gives the config DSL access to run-with-timer,
        // cancel-timer, sleep-for and friends, used to drive
        // *environment* animation (per-tick voltage / frequency
        // perturbations, scheduled events). Component logic stays in
        // Rust; lisp's only job is wiring + scripting the site
        // around it. Must be called inside a tokio runtime —
        // TokioExecutor::new captures Handle::current().
        //
        // The returned `Handle` is what the dedicated
        // `spawn_lisp_refresh_loop` task ticks at 100 ms cadence
        // to fire pending timer firings. Without it the mailbox
        // would just accumulate.
        let timer_handle =
            tulisp_async::register(&mut ctx, Arc::new(tulisp_async::TokioExecutor::new()));

        // One-per-process loop that walks every registered
        // MicrogridSite's TimeoutTracker and calls reset_setpoint on
        // each elapsed entry. Both gRPC's SetElectricalComponentPower
        // and the Lisp `(set-active-power …)` defun add to the
        // tracker; this loop is what makes their request-lifetime
        // semantics visible.
        Self::start_timeout_loop(microgrids.clone());

        if let Err(e) = ctx.eval_file(filename) {
            let formatted = e.format(&ctx);
            log::error!("Tulisp error:\n{formatted}");
            return Err(formatted);
        }

        // Every config must register at least one microgrid via
        // `(make-microgrid …)` — there's no single-microgrid
        // fallback. A bare config that forgets the form would
        // boot a binary with no gRPC servers, no loopback, and
        // an empty Microgrids UI; surface that as a hard error
        // instead.
        if microgrids.lock().is_empty() {
            return Err("config loaded but no (make-microgrid …) form ran — \
                 every config must register at least one microgrid"
                .to_string());
        }

        let initial_status = log_topology_validation(&site, "boot");
        *graph_status.write() = initial_status;

        let ctx = SharedMut::new(ctx);

        // Lisp refresh loop. One tokio task at 100 ms cadence holds
        // the interpreter lock once per pass, walks every registered
        // microgrid's components calling `refresh_inputs` (which
        // re-resolves any lambda-bound `:power` / `:sunlight%` / …
        // into `DynamicScalar`'s atomic), and drains the
        // tulisp-async timer mailbox so `(every …)` / `(run-with-
        // timer …)` callbacks fire.
        //
        // Decoupling this from the per-site physics tick means:
        //  - Physics ticks are lock-free; a long-running /api/eval
        //    no longer stalls every microgrid's per-second physics.
        //  - The refresh ticks at its own cadence (100 ms by
        //    default), so lambda-bound inputs lag at most one
        //    refresh interval behind their underlying lisp source.
        //    For a 15-min sine curve that's a 0.005% phase shift —
        //    negligible.
        //  - `Config::refresh_once` exposes the same work
        //    synchronously for tests that drive `tick_once` and
        //    expect the lambda result to be visible immediately.
        Self::spawn_lisp_refresh_loop(microgrids.clone(), ctx.clone(), timer_handle.clone());

        // Scenarios auto-advance task — polls the wallclock and
        // transitions running scenarios on stage boundaries. Lives
        // on the same runtime as the gRPC + UI servers; the
        // interpreter lock it grabs to funcall :on lambdas is the
        // same one the pre-tick hook uses, so no extra plumbing.
        crate::sim::scenarios::spawn_auto_advance(
            scenarios.clone(),
            ctx.clone(),
            microgrids.clone(),
            current_microgrid.clone(),
            clock.clone(),
        );

        Ok(Self {
            filename: filename.to_string(),
            ctx,
            site,
            metadata,
            extra_watches,
            graph_status,
            clock,
            scenarios,
            microgrids,
            dispatches,
            router,
            current_microgrid,
            enterprise_id_allocator,
            microgrid_registered,
            timer_handle,
        })
    }

    /// Trigger one refresh + timer-drain pass synchronously. Mirrors
    /// what the background loop does once per 100 ms, but on the
    /// caller's thread — tests reach for this when they need a
    /// `(run-with-timer 0 …)` fire to be visible before the next
    /// `tick_once`, or a lambda-bound `:power` value to resolve
    /// before reading `aggregate_power_w`.
    ///
    /// Acquires the interpreter lock, walks every registered
    /// microgrid's components calling `refresh_inputs`, then drains
    /// the timer mailbox once. Tests that drive `tick_once` directly
    /// call this first so lambda-bound `:power` / `:sunlight%` /
    /// `(run-with-timer 0 …)` values are visible before the synthetic
    /// physics tick.
    pub fn refresh_once(&self) {
        let mut guard = self.ctx.borrow_mut();
        let sites: Vec<MicrogridSite> = self
            .microgrids
            .lock()
            .values()
            .map(|e| e.site.clone())
            .collect();
        for site in sites {
            for c in site.components() {
                c.refresh_inputs(&mut guard);
            }
        }
        self.timer_handle.tick(&mut guard);
    }

    /// Spawn the Lisp refresh + timer-drain loop. Runs at 100 ms
    /// cadence on its own tokio task; sole acquirer of the
    /// interpreter lock for refresh purposes (eval still contends
    /// at the same lock, but only when it's not held by us). See
    /// the comment block in `Config::new` for the design rationale.
    fn spawn_lisp_refresh_loop(
        registry: crate::sim::microgrids::SharedMicrogrids,
        ctx: SharedMut<TulispContext>,
        timer_handle: tulisp_async::Handle,
    ) {
        tokio::spawn(async move {
            // First tick at +100 ms so tests that boot a Config +
            // drive `tick_once` synchronously don't race the loop.
            let start = tokio::time::Instant::now() + Duration::from_millis(100);
            let mut tick = tokio::time::interval_at(start, Duration::from_millis(100));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                // Snapshot the per-mg sites outside the ctx lock, then
                // take the lock once and run the full refresh pass.
                // A long /api/eval grabbing the same lock will delay
                // this iteration but won't block physics: each site's
                // `spawn_physics` task ticks lock-free against the
                // atomics last published by `refresh_inputs`.
                let sites: Vec<MicrogridSite> =
                    registry.lock().values().map(|e| e.site.clone()).collect();
                let mut guard = ctx.borrow_mut();
                for site in &sites {
                    for c in site.components() {
                        c.refresh_inputs(&mut guard);
                    }
                }
                timer_handle.tick(&mut guard);
            }
        });
    }

    fn start_timeout_loop(registry: crate::sim::microgrids::SharedMicrogrids) {
        tokio::spawn(async move {
            // `interval` + `Skip` keeps the cadence on the nominal
            // 100 ms grid even when one iteration overruns (a Lisp
            // reset_setpoint that grabs the interpreter lock against
            // a long /api/eval can take real time). The previous
            // `sleep(100ms)` drifted upward under load — each
            // iteration's clock started AFTER the work finished.
            //
            // `interval_at` rather than `interval` so the *first*
            // tick lands at +100 ms instead of immediately. Tests
            // that arm a deadline + check `drain_expired_timeouts`
            // synchronously rely on the BG task not racing them at
            // t=0.
            let start = tokio::time::Instant::now() + Duration::from_millis(100);
            let mut tick = tokio::time::interval_at(start, Duration::from_millis(100));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                // Snapshot the per-mg sites under the lock, then drain
                // outside the lock so a slow component callback can't
                // hold registry-wide reads.
                let sites: Vec<MicrogridSite> =
                    registry.lock().values().map(|e| e.site.clone()).collect();
                for site in sites {
                    for (id, axis) in site.drain_expired_timeouts() {
                        log::info!(
                            "Request timeout for component {id} ({axis:?}) — resetting that axis"
                        );
                        if let Some(c) = site.get(id) {
                            c.reset_setpoint_axis(axis);
                        }
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
        let site = MicrogridSite::new();
        let metadata = Arc::new(RwLock::new(Metadata::default()));
        // Throwaway router for the TAGS pass — no microgrids
        // registered, so SiteRouter::site falls through to the
        // bootstrap site and every defun captures that one.
        let microgrids = crate::sim::microgrids::new_registry();
        let current = crate::sim::microgrids::new_current_microgrid();
        let router = SiteRouter::new(microgrids.clone(), current, site.clone());

        let load_dir: PathBuf = roots
            .first()
            .and_then(|r| Path::new(r).parent())
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        ctx.set_load_path(Some(&load_dir))
            .map_err(|e| Error::os_error(format!("set_load_path({}): {e}", load_dir.display())))?;

        defuns::register_runtime(&mut ctx, router, metadata, load_dir, microgrids);
        // The Handle is unused here — tags_table is a one-shot parse
        // pass, no timers ever fire — but `register` still installs
        // the four builtins so that `(run-with-timer …)` etc. show up
        // in the generated TAGS file.
        let _ = tulisp_async::register(&mut ctx, Arc::new(tulisp_async::TokioExecutor::new()));

        ctx.tags_table(Some(roots))
    }

    /// Re-evaluate the config file, resetting MicrogridSite state first.
    /// Returns the formatted lisp error on failure — the site is
    /// left in its post-reset (empty) state in that case so the
    /// next reload starts from a known baseline.
    pub fn reload(&self) -> Result<(), String> {
        let mut ctx = self.ctx.borrow_mut();
        self.reload_locked(&mut ctx)
    }

    /// `reload` body against an already-held interpreter guard — for
    /// callers (e.g. a scoped overrides-file replace) that must hold
    /// the lock across surrounding work; re-borrowing inside would
    /// deadlock.
    pub(super) fn reload_locked(&self, ctx: &mut TulispContext) -> Result<(), String> {
        use std::sync::atomic::Ordering;
        let start = std::time::Instant::now();
        self.site.reset();
        // Reset the enterprise-wide id allocator too — every site
        // is about to be rebuilt by the re-eval of config.lisp,
        // and we want auto-allocated ids to keep starting at
        // FIRST_AUTO_ID across reloads (the comment on
        // MicrogridSite.next_id justifies why).
        self.enterprise_id_allocator
            .store(crate::sim::component::FIRST_AUTO_ID, Ordering::Relaxed);
        // Keep the registry: the per-mg runtimes (physics tick, history
        // sampler, gRPC server, loopback client) each hold their entry's
        // site handle, and the re-eval'd (make-microgrid …) forms reuse
        // those sites in place — dropping the entries would orphan every
        // runtime on a site the registry no longer hands out. Reset each
        // site up front so a microgrid the new config no longer declares
        // ends up empty (its runtimes keep running against the empty
        // site — they can't be torn down without a restart — but it
        // stops ticking stale components).
        for entry in self.microgrids.lock().values() {
            entry.site.reset();
        }
        if let Err(e) = ctx.eval_file(&self.filename) {
            let formatted = e.format(ctx);
            log::error!("Tulisp error:\n{formatted}");
            return Err(formatted);
        }
        // Belt-and-suspenders: with the keep-registry semantics above
        // this can only fire if the registry was empty before the
        // reload, which Config::new's own check rules out.
        if self.microgrids.lock().is_empty() {
            return Err("reloaded config registered no microgrids — \
                 every config must call (make-microgrid …) at least once"
                .to_string());
        }
        // Tell UI subscribers the MicrogridSite rebuilt. Catches the
        // "removed the only pending entry" case where remove_pending
        // reloads but has no surviving entries to bump-version
        // through eval_with_affects. Bump every registered microgrid
        // so per-mg UI subscribers all see the rebuild — the router-
        // resolved `cfg.site()` reads from the first registry entry,
        // not the bootstrap site we reset above.
        *self.graph_status.write() = log_topology_validation(&self.router.site(), "reload");
        for entry in self.microgrids.lock().values() {
            entry.site.bump_version();
        }
        log::info!(
            "Reloaded config in {:.1}ms",
            start.elapsed().as_secs_f64() * 1000.0
        );
        Ok(())
    }

    pub async fn watch(self) {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        // notify can fail at construction (out of inotify slots —
        // `fs.inotify.max_user_watches` exhausted by an IDE running
        // alongside) or at watch-registration time (file vanished).
        // Either case kills hot-reload but the rest of the binary
        // should keep serving; log and bail out of just this task.
        let mut watcher = match RecommendedWatcher::new(
            move |res| {
                futures::executor::block_on(async {
                    let _ = tx.send(res).await;
                });
            },
            notify::Config::default(),
        ) {
            Ok(w) => w,
            Err(e) => {
                log::error!("watch: notify init failed: {e}; hot-reload disabled");
                return;
            }
        };
        if let Err(e) = watcher.watch(
            Path::new(&self.filename),
            notify::RecursiveMode::NonRecursive,
        ) {
            log::error!(
                "watch: registering {}: {e}; hot-reload disabled",
                self.filename
            );
            return;
        }
        // Add every path the config registered via `(watch-file …)`.
        // Snapshotted now; reload-time additions take effect on the
        // next process restart (the live notify watcher isn't held
        // across reloads, by design — keeps the watch lifecycle simple).
        for path in self.extra_watches.lock().iter() {
            if let Err(e) = watcher.watch(path, notify::RecursiveMode::NonRecursive) {
                log::warn!("watch-file {}: {}", path.display(), e);
            }
        }

        // Debounce window. Editors typically fire several notify
        // events for a single save (write + close-after-write +
        // plugin reformat); we coalesce anything arriving within
        // this window into one reload. 150 ms is comfortably above
        // the inotify event-batch latency on a busy machine and
        // still feels instant to a human editing.
        const DEBOUNCE: Duration = Duration::from_millis(150);

        while let Some(res) = rx.recv().await {
            let event = match res {
                Ok(e) => e,
                Err(e) => {
                    log::error!("watch error: {:?}", e);
                    return;
                }
            };
            if !matches!(event.kind, notify::EventKind::Modify(_)) {
                continue;
            }
            // After the first Modify, drain any further events that
            // arrive within DEBOUNCE; each additional event restarts
            // the window. Once the window goes quiet, fire one reload.
            // Reload errors are logged by `reload()` and surfaced on
            // the site event bus so the UI can show a banner; the
            // loop intentionally keeps going so a typo doesn't kill
            // the live-edit feedback path.
            loop {
                match tokio::time::timeout(DEBOUNCE, rx.recv()).await {
                    Ok(Some(Ok(_))) => continue,
                    Ok(Some(Err(e))) => {
                        log::error!("watch error: {:?}", e);
                        return;
                    }
                    Ok(None) => return,
                    Err(_) => break,
                }
            }
            if let Err(msg) = self.reload() {
                self.site.broadcast_config_error(msg);
            }
        }
    }
}

/// Run the component-graph validator on the current `MicrogridSite` and
/// log the outcome. `phase` is one of "boot" / "reload" so the log
/// line tags which path triggered the check.
///
/// Log-only, not fatal. Empty worlds (no components yet) skip
/// because the graph crate requires exactly one
/// `GridConnectionPoint` and rejects empty graphs — test fixtures
/// that wire up `Config` against `""` would otherwise fail.
/// Non-empty worlds that fail validation surface as a `log::warn!`
/// in the simulator log and as a ⚠ on the pulse bar's graph pill
/// (via [`Config::graph_status`] on `/api/topology`).
///
/// On success the log line includes a one-line summary so a dev
/// reading the log can confirm switchyard parsed the topology the
/// same way `frequenz-microgrid` would.
///
/// Returns the human-readable error the validator produced. `None`
/// = the graph crate accepted the topology (or the site is empty /
/// hidden-only); `Some(msg)` = the failure message, which the
/// caller stores in [`Config::graph_status`].
fn log_topology_validation(site: &MicrogridSite, phase: &str) -> Option<String> {
    let (nodes, edges) = crate::sim::graph_adapter::snapshot(site);
    let visible_count = nodes.len();
    if visible_count == 0 {
        log::debug!("graph: {phase} skipped (no visible components)");
        return None;
    }
    match crate::sim::graph_adapter::build_from(nodes, edges) {
        Ok(graph) => {
            // `graph.components()` yields nodes that survived
            // pass-through elision. With no pass-through categories
            // in switchyard's model yet this equals visible_count;
            // we log both so the gap is visible the day we add a
            // transformer / breaker / converter.
            let logical_count = graph.components().count();
            log::info!(
                "graph: {phase} validated ({visible_count} visible, {logical_count} after pass-through elision)"
            );
            None
        }
        Err(e) => {
            let msg = format!("{e}");
            log::warn!(
                "graph: {phase} validation failed — {visible_count} visible components rejected by frequenz-microgrid-component-graph: {msg}"
            );
            Some(msg)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::Config;
    use super::super::test_support::{config_with, next_unique};

    /// `Config::new` returns Err on lisp eval failure rather than
    /// silently logging — the binary panics with a useful message
    /// and tests get a clear assertion target rather than a
    /// half-built MicrogridSite.
    #[test]
    fn config_new_returns_err_on_bad_lisp() {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "switchyard-cfg-bad-{}-{}",
            std::process::id(),
            next_unique(),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.lisp");
        std::fs::write(&path, "(this-is-not-a-defun-anywhere 42)").unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let res = rt.block_on(async { Config::new(path.to_str().unwrap()) });
        std::mem::forget(rt);
        let err = match res {
            Ok(_) => panic!("expected lisp error for undefined fn"),
            Err(e) => e,
        };
        assert!(
            err.contains("this-is-not-a-defun-anywhere"),
            "error should name the offending symbol: {err}",
        );
    }

    /// `Config::refresh_once` drains tulisp-async's pending-timer
    /// queue. Without that, run-with-timer would just accumulate
    /// PendingTasks (same-ctx model — nothing fires them
    /// asynchronously). A zero-delay one-shot timer plus one
    /// refresh is the tightest expression of the contract.
    #[test]
    fn refresh_once_drains_pending_timers() {
        let (cfg, _dir) = config_with(
            "(set-microgrid-id 9)
             (setq fired 0)
             (run-with-timer 0 nil (lambda () (setq fired 1)))",
        );
        cfg.refresh_once();
        assert_eq!(cfg.eval_silent("fired").unwrap(), "1");
    }

    /// `reload()` must rebuild each microgrid's topology on the SAME
    /// site the boot-time runtimes hold — minting fresh sites would
    /// leave physics ticking (and gRPC serving) orphaned pre-reload
    /// state while the registry's new sites never tick.
    #[test]
    fn reload_rebuilds_topology_on_the_same_site() {
        let (cfg, _dir) = config_with(
            "(set-microgrid-id 9)
             (%make-grid-connection-point :id 1)",
        );
        // The handle a boot-spawned physics task / gRPC server holds.
        let live_site = cfg.microgrids().lock().get(&9).unwrap().site.clone();
        assert!(live_site.get(1).is_some());

        cfg.reload().expect("reload succeeds");

        // The re-eval'd topology landed on the SAME site: the
        // pre-reload handle sees the rebuilt component, and the
        // registry still carries exactly one entry for id 9.
        assert!(
            live_site.get(1).is_some(),
            "pre-reload site handle must see the rebuilt topology",
        );
        let reg = cfg.microgrids();
        let r = reg.lock();
        assert_eq!(r.len(), 1);
        assert!(r.get(&9).unwrap().site.get(1).is_some());
    }
}
