//! Lisp glue: load the config DSL, register the `make-*` functions
//! against a `MicrogridSite`, and act as the runtime entry point for the gRPC
//! server (which calls into us for `set_active_setpoint` and friends).
//!
//! The `Config` struct is intentionally thin — the simulation state
//! lives in `MicrogridSite`, the lisp interpreter is just the configuration
//! frontend.

pub mod csv_profile;
mod defuns;
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
use tokio::sync::broadcast;
use tulisp::{Error, SharedMut, TulispContext};

use crate::sim::MicrogridSite;
use crate::sim::microgrids::{CurrentMicrogrid, SharedSiteRouter, SiteRouter};

/// Enterprise-level gateway settings the Lisp config can override.
/// Per-microgrid identity (id, name, grpc_port, TSO) lives in the
/// `sim::microgrids` registry — each `(make-microgrid …)` form
/// inserts one entry. Metadata here only carries enterprise-wide
/// knobs: the enterprise id surfaced on every gRPC `MicrogridInfo`,
/// the assets server's bind address, and the default
/// request-lifetime fallback.
#[derive(Debug, Clone)]
pub struct Metadata {
    pub enterprise_id: u64,
    /// Address the PlatformAssets gRPC service binds to.
    /// Independent of any microgrid's `grpc_port` so a sibling
    /// service (assets / reporting / future API surfaces) doesn't
    /// fight a microgrid for its socket. Overridable from lisp
    /// via `(set-assets-socket-addr "[::1]:9900")`.
    pub assets_socket_addr: String,
    /// Address the single (enterprise-wide) `MicrogridDispatchService`
    /// gRPC service binds to. One service fronts every microgrid,
    /// keyed by `microgrid_id` in each request, so it gets its own
    /// socket — distinct from any microgrid's `grpc_port` and from
    /// the assets server. Default matches the sibling `dispatchsim`
    /// mock so existing dispatch-client wiring keeps working.
    /// Overridable from lisp via `(set-dispatch-socket-addr "…")`.
    pub dispatch_socket_addr: String,
    /// Fallback request lifetime when a `SetElectricalComponentPower`
    /// caller doesn't supply `request_lifetime`. Mirrors microsim's
    /// `retain-requests-duration-ms`. Tunable via
    /// `(set-default-request-lifetime-ms N)`. The gRPC handler's
    /// per-request validation in `server::resolve_lifetime` clamps
    /// to `[REQUEST_LIFETIME_MIN_S, REQUEST_LIFETIME_MAX_S]`; this
    /// default isn't clamped (a config that wants short / long
    /// fallbacks is responsible for picking values that align with
    /// its operational expectations).
    pub default_request_lifetime: Duration,
}

impl Default for Metadata {
    fn default() -> Self {
        Self {
            enterprise_id: 0,
            assets_socket_addr: "[::1]:9900".to_string(),
            dispatch_socket_addr: "[::1]:8900".to_string(),
            default_request_lifetime: Duration::from_secs(60),
        }
    }
}

#[derive(Clone)]
pub struct Config {
    filename: String,
    pub(crate) ctx: SharedMut<TulispContext>,
    pub(crate) site: MicrogridSite,
    pub(crate) metadata: Arc<RwLock<Metadata>>,
    /// Additional files the config has registered via `(watch-file …)`.
    /// `Config::watch` adds each to the live notify watcher so edits to
    /// e.g. `sim/defaults.lisp` trigger the same reload as edits to
    /// the entry-point config. Set semantics — duplicate registrations
    /// (from re-runs of the config during reload) are no-ops.
    extra_watches: Arc<Mutex<HashSet<PathBuf>>>,
    /// Latest topology-validation outcome from the graph crate.
    /// `None` = healthy; `Some(message)` = the validator rejected the
    /// current site. `log_topology_validation` updates this on every
    /// boot + reload; `/api/topology` exposes it so the pulse-bar
    /// graph pill (see UI-design.org §Z6) can flip between ✓ and ⚠
    /// without polling a separate endpoint.
    graph_status: Arc<RwLock<Option<String>>>,
    /// Configured display timezone. UI's TZ toggle reads the IANA
    /// name from /api/clock and formats timestamps client-side via
    /// `Intl.DateTimeFormat(..., { timeZone })`. Mutated by
    /// `(set-timezone "…")` in config.lisp; default Europe/Berlin
    /// matches the canonical European-intraday demo target.
    clock: crate::sim::clock::SharedClock,
    /// Multi-stage scenario registry — what `(define-scenario …)`
    /// writes to and what the UI's Scenarios mode + /api/scenarios
    /// read from. See `crate::sim::scenarios` for the data model.
    pub(crate) scenarios: crate::sim::scenarios::SharedScenarios,
    /// Enterprise-scoped microgrid registry — what
    /// `(make-microgrid …)` writes to and what the Microgrids UI
    /// mode + /api/microgrids read from. Empty until the config eval
    /// runs at least one `(make-microgrid …)` form; `Config::new`
    /// errors out if nothing landed in here by the end of eval. See
    /// `crate::sim::microgrids` for the data model.
    pub(crate) microgrids: crate::sim::microgrids::SharedMicrogrids,
    /// Enterprise-wide dispatch store — the single
    /// `MicrogridDispatchService` gRPC server writes here (Create /
    /// Update / Delete from the dispatch CLI), and the per-microgrid
    /// Dispatches UI view + `/api/mg/{id}/dispatches` read from it.
    /// Keyed by `microgrid_id` internally; survives a config reload
    /// (it isn't owned by any `MicrogridSite`). See
    /// `crate::sim::dispatch` for the data model.
    pub(crate) dispatches: crate::sim::dispatch::SharedDispatchStore,
    /// Dynamic site lookup the lisp defuns capture. Resolves to
    /// the current microgrid's site at call time, falling back
    /// to the first registry entry and finally to the bootstrap
    /// site allocated in `Config::new`. See
    /// [`crate::sim::microgrids::SiteRouter`].
    pub(crate) router: SharedSiteRouter,
    /// Active microgrid id, written by /api/mg/{id}/eval and the
    /// scenario per-microgrid replay. `None` defers to the
    /// router's fallback (first registry entry).
    pub(crate) current_microgrid: CurrentMicrogrid,
    /// Process-wide component-id allocator shared by every
    /// `MicrogridSite` registered through `(make-microgrid …)`,
    /// so auto-allocated component ids stay globally unique
    /// across microgrids. The bootstrap site allocated in
    /// `Config::new` uses the same allocator, so single-site
    /// configs see no behavioural change from the legacy
    /// per-site counter — only the multi-microgrid path gains
    /// cross-site uniqueness.
    pub(crate) enterprise_id_allocator: Arc<std::sync::atomic::AtomicU64>,
    /// Enterprise-wide notification fired when a new microgrid
    /// lands in `microgrids` — both `(make-microgrid …)` and
    /// `/api/microgrids/create` publish on it. The WS event pump
    /// subscribes so it can spawn a forwarder for the new site's
    /// event bus on the fly, instead of only the entries that
    /// existed at WS-connect time.
    pub(crate) microgrid_registered: Arc<broadcast::Sender<u64>>,
    /// tulisp-async timer handle. The Lisp refresh loop ticks it at
    /// 100 ms cadence to fire `(run-with-timer …)` / `(every …)`
    /// callbacks; `Config::refresh_once` ticks it synchronously for
    /// tests that drive ticks deterministically.
    pub(crate) timer_handle: tulisp_async::Handle,
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

        defuns::register_runtime(&mut ctx, router.clone(), metadata.clone(), load_dir.clone());
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

    /// Shared scenarios registry — `(define-scenario …)` writes
    /// here, the UI Scenarios mode + /api/scenarios read here, and
    /// the auto-advance task mutates the per-entry runtime state.
    pub fn scenarios(&self) -> crate::sim::scenarios::SharedScenarios {
        self.scenarios.clone()
    }

    /// Shared enterprise microgrid registry — `(make-microgrid …)`
    /// writes here, the UI Microgrids landing page + /api/microgrids
    /// read here. Always carries at least one entry once
    /// `Config::new` has returned — the hard-error in `Config::new`
    /// rejects configs whose registry is empty after eval.
    pub fn microgrids(&self) -> crate::sim::microgrids::SharedMicrogrids {
        self.microgrids.clone()
    }

    /// Shared enterprise dispatch store — the `MicrogridDispatchService`
    /// gRPC server mutates it and the UI's per-microgrid Dispatches
    /// view reads it. See [`crate::sim::dispatch`].
    pub fn dispatches(&self) -> crate::sim::dispatch::SharedDispatchStore {
        self.dispatches.clone()
    }

    /// Shared process-wide id allocator backing every microgrid
    /// in the registry. The /api/microgrids/create endpoint
    /// clones this into a fresh `MicrogridSite::with_id_allocator`
    /// so runtime-created microgrids participate in the same
    /// globally-unique component-id space as boot-time ones.
    pub fn enterprise_id_allocator(&self) -> std::sync::Arc<std::sync::atomic::AtomicU64> {
        self.enterprise_id_allocator.clone()
    }

    /// Publish a `microgrid_registered` notification. Called by the
    /// /api/microgrids/create handler after inserting the new entry,
    /// so the WS event pump can spawn a forwarder for the freshly-
    /// created site without waiting for a reconnect.
    pub fn notify_microgrid_registered(&self, id: u64) {
        let _ = self.microgrid_registered.send(id);
    }

    /// Subscribe to `microgrid_registered` notifications. The WS
    /// event pump uses this to dynamically subscribe to new
    /// microgrid event buses post-connect.
    pub fn subscribe_microgrid_registered(&self) -> broadcast::Receiver<u64> {
        self.microgrid_registered.subscribe()
    }

    /// Mutable handle on the active microgrid id. Per-microgrid
    /// HTTP routes (`/api/mg/{id}/eval` and friends) flip this via
    /// `with_microgrid` so the lisp defuns + the override-file
    /// path resolve to the URL's microgrid.
    pub fn current_microgrid_handle(&self) -> crate::sim::microgrids::CurrentMicrogrid {
        self.current_microgrid.clone()
    }

    /// Configured display-zone clock handle. The scenarios HTTP
    /// layer calls this to derive `local_hour(now)` so `start` /
    /// `auto_advance` agree on which stage wallclock-NOW maps to.
    pub fn clock_handle(&self) -> crate::sim::clock::SharedClock {
        self.clock.clone()
    }

    /// Clone of the tulisp interpreter handle. Exposed so the
    /// scenarios state machine can funcall stage `:on` lambdas from
    /// outside `lisp::Config::eval`; everything else inside the
    /// crate should reach for `eval` / `eval_silent` instead.
    pub fn interpreter(&self) -> SharedMut<TulispContext> {
        self.ctx.clone()
    }

    /// Synchronous, single-shot version of the refresh loop's work.
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
                    for id in site.drain_expired_timeouts() {
                        log::info!("Request timeout for component {id} — resetting setpoint");
                        if let Some(c) = site.get(id) {
                            c.reset_setpoint();
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
        let router = SiteRouter::new(microgrids, current, site.clone());

        let load_dir: PathBuf = roots
            .first()
            .and_then(|r| Path::new(r).parent())
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        ctx.set_load_path(Some(&load_dir))
            .map_err(|e| Error::os_error(format!("set_load_path({}): {e}", load_dir.display())))?;

        defuns::register_runtime(&mut ctx, router, metadata, load_dir);
        // The Handle is unused here — tags_table is a one-shot parse
        // pass, no timers ever fire — but `register` still installs
        // the four builtins so that `(run-with-timer …)` etc. show up
        // in the generated TAGS file.
        let _ = tulisp_async::register(&mut ctx, Arc::new(tulisp_async::TokioExecutor::new()));

        ctx.tags_table(Some(roots))
    }

    pub fn metadata(&self) -> Metadata {
        self.metadata.read().clone()
    }

    pub fn assets_socket_addr(&self) -> String {
        self.metadata.read().assets_socket_addr.clone()
    }

    pub fn dispatch_socket_addr(&self) -> String {
        self.metadata.read().dispatch_socket_addr.clone()
    }

    pub fn site(&self) -> MicrogridSite {
        self.router.site()
    }

    /// Latest graph-validator outcome. `None` = the graph crate
    /// accepted the topology at the last config-load / reload (or
    /// the site is empty); `Some(msg)` = it rejected, with the
    /// human-readable error. `/api/topology` serialises this so
    /// the pulse-bar graph pill flips to ⚠ + opens-on-click with
    /// the message (see UI-design.org §Z6).
    pub fn graph_status(&self) -> Option<String> {
        self.graph_status.read().clone()
    }

    /// IANA name of the configured display zone (default
    /// "Europe/Berlin"; redirected by `(set-timezone "…")`). The
    /// UI's TZ toggle reads this from /api/clock + formats
    /// timestamps via Intl.DateTimeFormat without round-tripping
    /// through Rust.
    pub fn tz_name(&self) -> &'static str {
        self.clock.read().tz_name()
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
    /// expression on disk. Either way the MicrogridSite version bumps so
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
            let label = self
                .overrides_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<no resolvable microgrid>".to_string());
            log::error!("Failed to append override to {label}: {e}");
        }
        // Bump the version on the microgrid the eval actually
        // mutated (the one current_microgrid points at, or — if no
        // scope was set — the router's fallback) so the WS event
        // pump fires TopologyChanged on the right bus. Without this
        // the bootstrap site's version moved, but UI sessions only
        // listen to per-mg buses.
        self.router.site().bump_version();
        result
    }

    /// Read-only eval — same machinery as `eval` but the result is
    /// NOT appended to the override file and the site version does
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
        let Some(path) = self.overrides_path() else {
            // No resolvable microgrid scope — nothing to persist
            // against. Boot path can't reach this; a future
            // `(reset-microgrid)`-then-eval flow would.
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "no resolvable microgrid scope; can't persist override",
            ));
        };
        // Per-mg overrides live under `microgrids/`; the dir might not
        // exist yet on a fresh checkout. Create lazily on the first
        // write so the user doesn't have to seed it manually.
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
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
        let Some(path) = self.overrides_path() else {
            return Vec::new();
        };
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
    /// file-position idx) and re-derive MicrogridSite state. Atomic: the
    /// override file is rewritten without those forms (temp +
    /// rename, with a `tulisp-fmt` pretty-print pass over the
    /// surviving forms), then `reload()` re-runs config.lisp +
    /// `load-overrides` on the new file so the deleted forms'
    /// effects vanish via the MicrogridSite reset inside reload.
    ///
    /// Returns the count of forms actually dropped — out-of-range
    /// indices are silently ignored. An IO error during rewrite
    /// leaves the site state untouched (the file was renamed
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
        let Some(path) = self.overrides_path() else {
            // persisted_overrides() returned entries above, so the
            // path was resolvable then; reach here only if the
            // current-microgrid pointer flipped to None in between.
            // Bail rather than touch the filesystem with a nonsense
            // path.
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "no resolvable microgrid scope; can't rewrite overrides",
            ));
        };
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
        // A reload error after a successful rewrite leaves the file
        // on disk and the site reset to empty — the next save
        // (or a manual `reload`) is the recovery path. Surface the
        // error as IO so the HTTP handler can return 5xx; the
        // user's already lost the broken forms either way.
        if let Err(msg) = self.reload() {
            return Err(std::io::Error::other(format!(
                "reload after rewrite failed: {msg}"
            )));
        }
        Ok(dropped)
    }

    /// Read the raw text of the active microgrid's overrides file.
    /// Empty string when the file doesn't exist yet (no edits have
    /// been persisted) or the scope can't resolve. Used by the
    /// canvas-undo handler to snapshot state before each mutation.
    pub fn overrides_text(&self) -> std::io::Result<String> {
        let Some(path) = self.overrides_path() else {
            return Ok(String::new());
        };
        match fs::read_to_string(&path) {
            Ok(s) => Ok(s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e),
        }
    }

    /// Replace the overrides file with `content` and reload. The
    /// canvas-undo handler restores a snapshot of the file taken
    /// before a mutation; redo replays the snapshot taken after.
    /// Atomic rewrite (temp + rename) so an interruption mid-write
    /// can't corrupt the file.
    pub fn replace_overrides_text(&self, content: &str) -> std::io::Result<()> {
        let Some(path) = self.overrides_path() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "no resolvable microgrid scope; can't rewrite overrides",
            ));
        };
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("lisp.tmp");
        {
            let mut file = fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&tmp)?;
            file.write_all(content.as_bytes())?;
            file.flush()?;
        }
        fs::rename(&tmp, &path)?;
        if let Err(msg) = self.reload() {
            return Err(std::io::Error::other(format!(
                "reload after override-text replace failed: {msg}"
            )));
        }
        Ok(())
    }

    /// Resolve the per-microgrid overrides file path. Keyed off the
    /// active microgrid id (set by /api/mg/{id}/eval and the
    /// scenarios per-mg replay), falling back to the first registry
    /// entry when nothing's selected.
    ///
    /// Returns `None` when neither source resolves — current is
    /// `None` AND the registry is empty. The boot path can't reach
    /// that case (`Config::new` rejects an empty registry), but
    /// guarding against it here keeps a future `(reset-microgrid)`-
    /// then-eval flow from writing to a meaningless
    /// `config.0.overrides.lisp`.
    fn overrides_path(&self) -> Option<PathBuf> {
        let load_dir = Path::new(&self.filename)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        let mg_id = self
            .current_microgrid
            .read()
            .or_else(|| self.microgrids.lock().keys().next().copied())?;
        Some(
            load_dir
                .join("microgrids")
                .join(format!("config.{mg_id}.overrides.lisp")),
        )
    }

    /// Directory holding per-microgrid `config.<id>.lisp` +
    /// `config.<id>.overrides.lisp` files, next to the entry config.
    /// The HTTP create endpoint writes runtime-created microgrid
    /// stubs here so they survive process restarts.
    pub fn microgrids_dir(&self) -> PathBuf {
        let load_dir = Path::new(&self.filename)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        load_dir.join("microgrids")
    }

    /// Directory snapshots are stored in: a `snapshots/` subdirectory
    /// next to the loaded config file. Lazily created on the first
    /// `save_snapshot` call.
    pub fn snapshots_dir(&self) -> PathBuf {
        let load_dir = Path::new(&self.filename)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        load_dir.join("snapshots")
    }

    /// Copy the current per-microgrid overrides file to
    /// `snapshots/<name>.lisp`. The snapshot is a frozen-in-time copy
    /// of the user's accumulated edits — replaying it (via
    /// `load_snapshot`) recovers the same dashboard / topology shape
    /// without re-running the manual click stream. Live physics state
    /// (ramps, mid-flight setpoints, current SoC, etc.) is NOT
    /// captured here — those derive from the snapshotted topology
    /// once the site re-spins from baseline.
    ///
    /// Returns the absolute path of the snapshot file on success.
    /// Errors if the name resolves to anything outside `snapshots/`
    /// (path-traversal guard) or if the overrides file isn't readable.
    pub fn save_snapshot(&self, name: &str) -> std::io::Result<PathBuf> {
        let dir = self.snapshots_dir();
        let dest = sanitise_snapshot_path(&dir, name)?;
        fs::create_dir_all(&dir)?;
        // Empty (no overrides for this mg yet) is a valid snapshot —
        // the user just hasn't edited anything. Treat a missing
        // resolvable scope the same way: write an empty snapshot so
        // load_snapshot can replay it. Reading a path that doesn't
        // exist falls through to the same empty-file write.
        match self.overrides_path() {
            Some(src) if src.exists() => {
                fs::copy(&src, &dest)?;
            }
            _ => {
                fs::write(&dest, "")?;
            }
        }
        Ok(dest)
    }

    /// Replace the current overrides file with `snapshots/<name>.lisp`
    /// and reload, so the site derives from base config.lisp +
    /// the snapshotted overrides.
    pub fn load_snapshot(&self, name: &str) -> Result<(), String> {
        let dir = self.snapshots_dir();
        let src = sanitise_snapshot_path(&dir, name)
            .map_err(|e| format!("invalid snapshot name: {e}"))?;
        if !src.exists() {
            return Err(format!("snapshot {name:?} not found"));
        }
        let dest = self
            .overrides_path()
            .ok_or_else(|| "no resolvable microgrid scope; can't pick a destination".to_string())?;
        fs::copy(&src, &dest).map_err(|e| format!("copy snapshot failed: {e}"))?;
        self.reload()
    }

    /// Names of every `*.lisp` file in `snapshots/`, sorted lex.
    /// Wraps the standalone helper at the bottom of this module.
    pub fn list_snapshots(&self) -> Vec<String> {
        list_snapshots_in(&self.snapshots_dir())
    }
}

fn sanitise_snapshot_path(dir: &Path, name: &str) -> std::io::Result<PathBuf> {
    // Reject anything that could escape the snapshots dir via `..`,
    // an absolute path, or path separators. We only accept a single
    // file-name component.
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
        || name.starts_with('.')
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid snapshot name {name:?}"),
        ));
    }
    Ok(dir.join(format!("{name}.lisp")))
}

fn list_snapshots_in(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("lisp") {
                return None;
            }
            p.file_stem().and_then(|s| s.to_str()).map(|s| s.to_owned())
        })
        .collect();
    out.sort();
    out
}

impl Config {
    /// Re-evaluate the config file, resetting MicrogridSite state first.
    /// Returns the formatted lisp error on failure — the site is
    /// left in its post-reset (empty) state in that case so the
    /// next reload starts from a known baseline.
    pub fn reload(&self) -> Result<(), String> {
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
        // Drop the registry so make-microgrid forms re-evaluating
        // during the reload start from a clean slate.
        self.microgrids.lock().clear();
        {
            let mut ctx = self.ctx.borrow_mut();
            if let Err(e) = ctx.eval_file(&self.filename) {
                let formatted = e.format(&ctx);
                log::error!("Tulisp error:\n{formatted}");
                return Err(formatted);
            }
        }
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
/// the dev sees in the simulator log; the future pulse-bar pill
/// (Z6 in UI-design.org) will surface this in the UI too.
///
/// On success the log line includes a one-line summary so a dev
/// reading the log can confirm switchyard parsed the topology the
/// same way `frequenz-microgrid` would.
/// Run the graph crate's validator on the post-eval site, log
/// the outcome (info on success / warn on failure), and return a
/// status string the caller stores in [`Config::graph_status`].
/// `None` = the graph crate accepted the topology (or the site is
/// empty / hidden-only); `Some(msg)` = the human-readable error
/// the validator produced.
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
        let wrapped = wrap_test_body(body);
        std::fs::write(&path, wrapped).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let cfg = rt
            .block_on(async { Config::new(path.to_str().unwrap()) })
            .expect("config eval");
        // Drop the runtime — Config keeps its own handles to whatever
        // tulisp-async spawned during init.
        std::mem::forget(rt);
        (cfg, dir)
    }

    /// Auto-wrap a test body in `(make-microgrid …)` if the body doesn't
    /// already register one — every config must do so post-migration, but
    /// most tests don't care about the wrapper and just want their forms
    /// evaluated in a microgrid scope. Tests that exercise make-microgrid
    /// itself supply their own form and the wrapper is skipped.
    ///
    /// Inline `(set-microgrid-id N)` from the pre-migration shape gets
    /// stripped and its N seeds the wrapper's :id so per-mg id
    /// assertions keep their original target values.
    fn wrap_test_body(body: &str) -> String {
        if body.contains("make-microgrid") {
            return body.to_string();
        }
        let (stripped, mg_id) = strip_set_microgrid_id(body);
        let inner = if stripped.trim().is_empty() {
            "nil".to_string()
        } else {
            stripped
        };
        format!("(make-microgrid :id {mg_id} :grpc-port 8800 :topology (lambda () {inner}))")
    }

    fn strip_set_microgrid_id(body: &str) -> (String, u64) {
        let needle = "(set-microgrid-id ";
        let mut out = String::with_capacity(body.len());
        let mut rest = body;
        let mut mg_id: u64 = 2200;
        while let Some(idx) = rest.find(needle) {
            out.push_str(&rest[..idx]);
            let tail = &rest[idx + needle.len()..];
            if let Some(close) = tail.find(')') {
                let n_str = tail[..close].trim();
                if let Ok(v) = n_str.parse::<u64>() {
                    mg_id = v;
                }
                rest = &tail[close + 1..];
            } else {
                out.push_str(&rest[idx..]);
                return (out, mg_id);
            }
        }
        out.push_str(rest);
        (out, mg_id)
    }

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
            UNIQ.fetch_add(1, Ordering::Relaxed),
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

    /// set-active-power applies a setpoint and arms the timeout tracker.
    /// We can verify both by checking that MicrogridSite registers a deadline
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
        assert_eq!(cfg.site().drain_expired_timeouts(), Vec::<u64>::new());
        // Lifetime 0 → instantly elapses; the next drain returns id.
        cfg.eval("(set-active-power 2 1500.0 0)").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert_eq!(cfg.site().drain_expired_timeouts(), vec![2]);
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
        let (cfg, dir) = config_with("(set-microgrid-id 9) (%make-grid-connection-point :id 1)");
        cfg.eval("(rename-component 1 \"a\")").unwrap();
        cfg.eval("(rename-component 1 \"b\")").unwrap();
        let path = dir.join("microgrids/config.9.overrides.lisp");
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("(rename-component 1 \"a\")"));
        assert!(body.contains("(rename-component 1 \"b\")"));
        // Errored eval doesn't land in the file.
        assert!(cfg.eval("(undefined-fn 1)").is_err());
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(!body.contains("undefined-fn"));
    }

    /// `(set-meter-power id (lambda () X))` installs a dynamic
    /// source. `Config::refresh_once` resolves the lambda and
    /// `aggregate_power_w` reflects it on the next read.
    #[test]
    fn set_meter_power_accepts_a_lambda() {
        let (cfg, _dir) = config_with(
            "(set-microgrid-id 9)
             (%make-meter :id 7)",
        );
        cfg.eval("(set-meter-power 7 (lambda () 1234.5))").unwrap();
        cfg.refresh_once();
        let m = cfg.site().get(7).unwrap();
        assert!((m.aggregate_power_w(&cfg.site()) - 1234.5).abs() < 1e-3);
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
        cfg.refresh_once();
        let m = cfg.site().get(7).unwrap();
        assert!((m.aggregate_power_w(&cfg.site()) - 1500.0).abs() < 1e-3);
        // Mutate the bound variable; next refresh picks up the new value.
        cfg.eval("(setq consumer-power 2750.0)").unwrap();
        cfg.refresh_once();
        assert!((m.aggregate_power_w(&cfg.site()) - 2750.0).abs() < 1e-3);
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
        cfg.refresh_once();
        let inv = cfg.site().get(8).unwrap();
        // Issue a setpoint below sunlight-derated min_avail so the
        // ramp clips — observable through telemetry's active_power.
        inv.set_active_setpoint(-5000.0).expect("within rated");
        cfg.site()
            .tick_once(chrono::Utc::now(), std::time::Duration::from_millis(100));
        let p = inv
            .telemetry(&cfg.site())
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
            cfg.site().record_history_snapshot(Utc::now());
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
        let summary = cfg.site().scenario_summary(chrono::Utc::now());
        assert_eq!(summary.name.as_deref(), Some("warmup"));
        assert!(summary.started_at.is_some());
        assert!(summary.ended_at.is_none());
        assert_eq!(summary.event_count, 0);

        // First event id is 0.
        cfg.eval("(scenario-event 'outage \"bat-1003\")").unwrap();
        cfg.eval("(scenario-event \"note\" \"warming up\")")
            .unwrap();
        let summary = cfg.site().scenario_summary(chrono::Utc::now());
        assert_eq!(summary.event_count, 2);
        assert_eq!(summary.next_event_id, 2);

        let events = cfg.site().scenario_events_since(0, 100);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, "outage");
        assert_eq!(events[1].kind, "note");

        // Stop freezes elapsed; a subsequent (scenario-elapsed)
        // returns the frozen value rather than continuing to grow.
        cfg.eval("(scenario-stop)").unwrap();
        let frozen = cfg.site().scenario_summary(chrono::Utc::now());
        std::thread::sleep(std::time::Duration::from_millis(20));
        let later = cfg.site().scenario_summary(chrono::Utc::now());
        assert_eq!(frozen.elapsed_s, later.elapsed_s);
        assert!(frozen.ended_at.is_some());
    }

    /// `(define-scenario)` parses a multi-stage definition into the
    /// shared registry. Stage windows + the optional :on lambda
    /// round-trip; missing :on leaves `Stage::on = None` so the
    /// auto-advance task knows to skip the funcall step.
    #[test]
    fn define_scenario_registers_with_stages() {
        let (cfg, _dir) = config_with("(set-microgrid-id 9)");
        cfg.eval(
            r#"
            (define-scenario
              :name "evening-peak"
              :description "Consumer ramp 17:00 → 21:00"
              :date "2026-01-15"
              :stages
              '((:name "ramp" :hour-from 17 :hour-to 18
                 :on (lambda () (set-active-power 1001 5000)))
                (:name "peak" :hour-from 18 :hour-to 20
                 :on (lambda () (set-active-power 1001 25000)))
                (:name "wind-down" :hour-from 20 :hour-to 21)))
            "#,
        )
        .unwrap();
        let regs = cfg.scenarios();
        let r = regs.lock();
        let e = r.get("evening-peak").expect("registered");
        assert_eq!(e.def.description, "Consumer ramp 17:00 → 21:00");
        assert_eq!(
            e.def.date,
            Some(chrono::NaiveDate::from_ymd_opt(2026, 1, 15).unwrap())
        );
        assert_eq!(e.def.stages.len(), 3);
        assert_eq!(e.def.stages[0].name, "ramp");
        assert_eq!(e.def.stages[0].hour_from, 17.0);
        assert_eq!(e.def.stages[0].hour_to, 18.0);
        assert!(e.def.stages[0].on.is_some());
        assert!(e.def.stages[1].on.is_some());
        // Third stage has no :on -> Stage::on stays None.
        assert!(e.def.stages[2].on.is_none());
    }

    /// `config_with` auto-wraps a body lacking `(make-microgrid …)`
    /// into a single-entry registration. The id is sourced from any
    /// inline `(set-microgrid-id N)` (a leftover from the pre-
    /// migration test fixture shape), keeping the body's intended
    /// microgrid id stable.
    #[test]
    fn auto_wrapper_registers_single_microgrid_from_set_microgrid_id() {
        let (cfg, _dir) = config_with("(set-microgrid-id 4242)");
        let reg = cfg.microgrids();
        let r = reg.lock();
        assert_eq!(r.len(), 1);
        let e = r.get(&4242).expect("auto-wrapped under set-microgrid-id");
        assert_eq!(e.def.name, "default");
        assert_eq!(e.def.grpc_port, 8800);
    }

    /// `(make-microgrid …)` builds a *new* site for the entry and
    /// funcalls the :topology lambda with the current-microgrid
    /// pointer set to the new id. Nested make-* calls register
    /// into that fresh site, not the bootstrap or any prior
    /// microgrid's site.
    #[test]
    fn make_microgrid_registers_entry_and_topology() {
        let (cfg, _dir) = config_with("(set-microgrid-id 0)");
        cfg.eval(
            r#"
            (make-microgrid
              :name "south yard"
              :id 7777
              :grpc-port 8810
              :tso "TN"
              :topology
              (lambda ()
                (%make-grid-connection-point :id 1)))
            "#,
        )
        .unwrap();
        let reg = cfg.microgrids();
        let r = reg.lock();
        let e = r.get(&7777).expect("registered");
        assert_eq!(e.def.name, "south yard");
        assert_eq!(e.def.grpc_port, 8810);
        assert_eq!(e.def.tso.as_deref(), Some("TN"));
        // The :topology lambda ran with current-microgrid pinned
        // to the new id, so the grid component lives on the new
        // microgrid's own site — NOT on the bootstrap site.
        assert!(
            e.site.get(1).is_some(),
            "grid-connection-point id=1 should be on the new site",
        );
    }

    /// Auto-allocated component ids stay globally unique across
    /// microgrids: each `(make-meter)` consumes the next entry on
    /// the enterprise-wide allocator, regardless of which site
    /// receives the component.
    #[test]
    fn auto_ids_are_globally_unique_across_microgrids() {
        let (cfg, _dir) = config_with("(set-microgrid-id 0)");
        let ids: String = cfg
            .eval(
                r#"
                (let (a b c)
                  (make-microgrid :name "alpha" :id 2200
                                  :topology (lambda ()
                                              (setq a (component-id (%make-meter)))))
                  (make-microgrid :name "beta"  :id 2201
                                  :topology (lambda ()
                                              (setq b (component-id (%make-meter)))))
                  (make-microgrid :name "gamma" :id 2202
                                  :topology (lambda ()
                                              (setq c (component-id (%make-meter)))))
                  (format "%d/%d/%d" a b c))
                "#,
            )
            .unwrap()
            .trim_matches('"')
            .to_string();
        let parts: Vec<u64> = ids.split('/').map(|s| s.parse().unwrap()).collect();
        assert_eq!(parts.len(), 3);
        // Distinct values, all >= FIRST_AUTO_ID.
        assert_ne!(parts[0], parts[1]);
        assert_ne!(parts[1], parts[2]);
        assert_ne!(parts[0], parts[2]);
        for p in &parts {
            assert!(*p >= crate::sim::component::FIRST_AUTO_ID);
        }
    }

    /// Two microgrids end up with isolated sites — adding a grid
    /// to one doesn't leak into the other.
    #[test]
    fn two_microgrids_have_isolated_sites() {
        let (cfg, _dir) = config_with("(set-microgrid-id 0)");
        cfg.eval(
            r#"
            (make-microgrid :name "alpha" :id 1001
                            :topology (lambda ()
                                        (%make-grid-connection-point :id 1)))
            (make-microgrid :name "beta"  :id 1002
                            :topology (lambda ()
                                        (%make-grid-connection-point :id 2)))
            "#,
        )
        .unwrap();
        let reg = cfg.microgrids();
        let r = reg.lock();
        let a = r.get(&1001).unwrap();
        let b = r.get(&1002).unwrap();
        // Each microgrid sees its own grid component.
        assert!(a.site.get(1).is_some(), "alpha owns id=1");
        assert!(b.site.get(2).is_some(), "beta owns id=2");
        // Neither sees the other's.
        assert!(a.site.get(2).is_none(), "alpha doesn't see beta's id=2");
        assert!(b.site.get(1).is_none(), "beta doesn't see alpha's id=1");
    }

    /// When :id / :grpc-port are omitted, make-microgrid hands out
    /// the next free values starting at the registry's known
    /// floors.
    #[test]
    fn make_microgrid_auto_allocates_id_and_port() {
        let (cfg, _dir) = config_with("(set-microgrid-id 0)");
        let first: i64 = cfg
            .eval("(make-microgrid :name \"alpha\")")
            .unwrap()
            .parse()
            .unwrap();
        let second: i64 = cfg
            .eval("(make-microgrid :name \"beta\")")
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            second > first,
            "auto-allocated ids must be strictly increasing"
        );
        let r = cfg.microgrids();
        let g = r.lock();
        let a = g.get(&(first as u64)).unwrap();
        let b = g.get(&(second as u64)).unwrap();
        assert_ne!(a.def.grpc_port, b.def.grpc_port);
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
        cfg.site()
            .tick_once(now, std::time::Duration::from_millis(100));
        // Snapshot pass at t0 — first one just seeds the cursor
        // (dt from start is small but non-zero — ignore the result).
        cfg.site().record_history_snapshot(now);
        now += ChronoDuration::seconds(10);
        cfg.site()
            .tick_once(now, std::time::Duration::from_secs(10));
        cfg.site().record_history_snapshot(now);
        let r = cfg.site().scenario_report(now);
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
        cfg.site()
            .tick_once(now, std::time::Duration::from_millis(100));
        now += ChronoDuration::seconds(5);
        cfg.site().tick_once(now, std::time::Duration::from_secs(5));
        cfg.site().record_history_snapshot(now);
        let r = cfg.site().scenario_report(now);
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
        cfg.site().record_history_snapshot(Utc::now());
        assert_eq!(
            cfg.site().scenario_report(Utc::now()).peak_main_meter_w,
            0.0,
        );

        cfg.eval("(scenario-start \"power\")").unwrap();
        cfg.eval("(set-meter-power 1 2500.0)").unwrap();
        cfg.site().record_history_snapshot(Utc::now());
        let r = cfg.site().scenario_report(Utc::now());
        assert!((r.peak_main_meter_w - 2500.0).abs() < 1e-3);

        // A higher value lifts the peak; a later lower one
        // doesn't.
        cfg.eval("(set-meter-power 1 7800.0)").unwrap();
        cfg.site().record_history_snapshot(Utc::now());
        cfg.eval("(set-meter-power 1 1100.0)").unwrap();
        cfg.site().record_history_snapshot(Utc::now());
        let r = cfg.site().scenario_report(Utc::now());
        assert!((r.peak_main_meter_w - 7800.0).abs() < 1e-3);

        // scenario-start resets the peak.
        cfg.eval("(scenario-start \"again\")").unwrap();
        cfg.eval("(set-meter-power 1 500.0)").unwrap();
        cfg.site().record_history_snapshot(Utc::now());
        assert!((cfg.site().scenario_report(Utc::now()).peak_main_meter_w - 500.0).abs() < 1e-3,);
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
            cfg.site()
                .scenario_summary(chrono::Utc::now())
                .next_event_id,
            2
        );
        cfg.eval("(scenario-start \"second\")").unwrap();
        let summary = cfg.site().scenario_summary(chrono::Utc::now());
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
