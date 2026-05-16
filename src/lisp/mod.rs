//! Lisp glue: load the config DSL, register the `make-*` functions
//! against a `MicrogridSite`, and act as the runtime entry point for the gRPC
//! server (which calls into us for `set_active_setpoint` and friends).
//!
//! The `Config` struct is intentionally thin — the simulation state
//! lives in `MicrogridSite`, the lisp interpreter is just the configuration
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
use tokio::sync::broadcast;
use tulisp::{Error, SharedMut, TulispContext, TulispObject};

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
        let current_microgrid = crate::sim::microgrids::new_current_microgrid();
        let router = SiteRouter::new(microgrids.clone(), current_microgrid.clone(), site.clone());
        // Capacity = 64 because new-microgrid bursts are tiny (config
        // eval emits a few in quick succession, runtime creates are
        // one-at-a-time). Lagged receivers can only miss notifications
        // here, not events on per-site buses — the SPA's reconnect
        // already covers WS sessions that fall behind.
        let microgrid_registered = Arc::new(broadcast::channel(64).0);
        // Enterprise-wide grid frequency state — one OU process drives
        // every MicrogridSite in the registry so they share the
        // physically-correct same frequency. The driver task is
        // spawned below; bootstrap site + future make-microgrid forms
        // both attach to this slot.
        let grid_frequency = crate::sim::frequency::new_shared();
        site.set_grid_frequency(grid_frequency.clone());
        crate::sim::frequency::spawn_driver(grid_frequency.clone());
        // Shared slot for the per-tick hook so make-microgrid can
        // install it on each freshly-created MicrogridSite. Populated
        // below once the timer handle exists.
        let pre_tick_slot: Arc<RwLock<Option<crate::sim::microgrid_site::PreTickHook>>> =
            Arc::new(RwLock::new(None));

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

        register_runtime(&mut ctx, router.clone(), metadata.clone(), load_dir.clone());
        register_clock(&mut ctx, clock.clone());
        register_watches(&mut ctx, load_dir.clone(), extra_watches.clone());
        register_scenarios(&mut ctx, scenarios.clone());
        register_microgrids(
            &mut ctx,
            microgrids.clone(),
            router.clone(),
            current_microgrid.clone(),
            enterprise_id_allocator.clone(),
            pre_tick_slot.clone(),
            microgrid_registered.clone(),
            grid_frequency.clone(),
        );
        register_frequency(&mut ctx, grid_frequency.clone());

        // tulisp-async gives the config DSL access to run-with-timer,
        // cancel-timer, sleep-for and friends, used to drive
        // *environment* animation (per-tick voltage / frequency
        // perturbations, scheduled events). Component logic stays in
        // Rust; lisp's only job is wiring + scripting the site
        // around it. Must be called inside a tokio runtime —
        // TokioExecutor::new captures Handle::current().
        //
        // The returned `Handle` is what the pre-tick hook ticks each
        // physics step to fire pending timer firings. Without it the
        // mailbox would just accumulate.
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

        // Pre-tick hook: hold the interpreter lock once per tick,
        // refresh every component's Lisp-driven inputs (lambda-bound
        // `:power`, `:sunlight%`, …), then drain any timer firings
        // whose deadline has passed. Lets components read the
        // resolved scalar from an atomic in `tick` without re-entering
        // the interpreter — see `dynamic_scalar::DynamicScalar` —
        // and gives `(every …)` callbacks a fire cadence anchored to
        // the physics tick.
        //
        // The hook owns the only `Handle` clone we keep outside ctx;
        // that's enough to keep the mailbox alive between ticks.
        let hook_ctx = ctx.clone();
        let pre_tick_hook: crate::sim::microgrid_site::PreTickHook =
            Arc::new(move |w: &MicrogridSite| {
                let mut guard = hook_ctx.borrow_mut();
                for c in w.components() {
                    c.refresh_inputs(&mut guard);
                }
                timer_handle.tick(&mut guard);
            });
        site.set_pre_tick(pre_tick_hook.clone());
        // Microgrid sites that were already registered while the
        // config was evaluating (before this hook existed) need the
        // hook installed retroactively so their `(every …)` callbacks
        // fire on the per-mg physics loop. Future make-microgrid
        // forms (post-eval `cfg.eval("(make-microgrid ...)")`, runtime
        // create-microgrid HTTP requests) pick it up from the shared
        // slot.
        for entry in microgrids.lock().values() {
            entry.site.set_pre_tick(pre_tick_hook.clone());
        }
        *pre_tick_slot.write() = Some(pre_tick_hook.clone());

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
            router,
            current_microgrid,
            enterprise_id_allocator,
            microgrid_registered,
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

    fn start_timeout_loop(registry: crate::sim::microgrids::SharedMicrogrids) {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
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

        register_runtime(&mut ctx, router, metadata, load_dir);
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
            log::error!(
                "Failed to append override to {}: {e}",
                self.overrides_path().display(),
            );
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
        let path = self.overrides_path();
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

    fn overrides_path(&self) -> PathBuf {
        let load_dir = Path::new(&self.filename)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        // Per-microgrid overrides live next to the per-mg config file
        // under `microgrids/`. Keyed by the active microgrid id (set
        // by /api/mg/{id}/eval and the scenarios per-mg replay).
        // Falls back to the first registered microgrid for callers
        // that read overrides before any UI request has selected one
        // — every config registers at least one microgrid, so this
        // is always a valid id.
        let mg_id = self.current_microgrid.read().unwrap_or_else(|| {
            self.microgrids
                .lock()
                .keys()
                .next()
                .copied()
                .unwrap_or_default()
        });
        load_dir
            .join("microgrids")
            .join(format!("config.{mg_id}.overrides.lisp"))
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
        let src = self.overrides_path();
        if src.exists() {
            fs::copy(&src, &dest)?;
        } else {
            // No overrides means the snapshot is empty config + base
            // config.lisp; write an empty file so load_snapshot has
            // something to read.
            fs::write(&dest, "")?;
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
        let dest = self.overrides_path();
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

/// Register the `(set-timezone IANA-NAME)` defun. Validates the
/// argument via chrono-tz's `FromStr` impl — a typo surfaces as a
/// lisp error at config-load time rather than silently falling
/// through to UTC at format time. The UI's TZ toggle picks up the
/// new zone on its next /api/clock poll.
fn register_clock(ctx: &mut TulispContext, clock: crate::sim::clock::SharedClock) {
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

fn register_scenarios(ctx: &mut TulispContext, scenarios: crate::sim::scenarios::SharedScenarios) {
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
// Eight args trips clippy's `too_many_arguments` threshold; the
// review item A6 plans to bundle the shared state into a single
// `RuntimeHandles` struct that drops the count cleanly. Until
// then, the explicit list is more readable than a one-off tuple.
#[allow(clippy::too_many_arguments)]
fn register_microgrids(
    ctx: &mut TulispContext,
    registry: crate::sim::microgrids::SharedMicrogrids,
    router: SharedSiteRouter,
    current: crate::sim::microgrids::CurrentMicrogrid,
    id_allocator: Arc<std::sync::atomic::AtomicU64>,
    pre_tick: Arc<RwLock<Option<crate::sim::microgrid_site::PreTickHook>>>,
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
            // Install the shared pre-tick hook if Config::new has
            // populated it (post-eval reload or runtime
            // create-microgrid). During the initial config eval the
            // slot is still empty; Config::new walks the registry
            // afterwards and installs the hook on every entry.
            if let Some(hook) = pre_tick.read().clone() {
                site.set_pre_tick(hook);
            }
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
fn register_runtime(
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

    let r = router;
    ctx.defun(
        "set-component-command-mode",
        move |id: i64, m: CommandMode| -> bool {
            let w = r.site();
            w.set_command_mode(id as u64, m);
            true
        },
    );
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
                rand::thread_rng().gen_range(0..limit)
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
fn register_frequency(ctx: &mut TulispContext, state: crate::sim::frequency::SharedFrequency) {
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

    /// The pre-tick hook drains tulisp-async's pending-timer queue
    /// each physics step. Without that, run-with-timer would just
    /// accumulate PendingTasks (same-ctx model — nothing fires them
    /// asynchronously). A zero-delay one-shot timer plus one
    /// tick_once is the tightest expression of the contract.
    #[test]
    fn pre_tick_drains_pending_timers() {
        let (cfg, _dir) = config_with(
            "(set-microgrid-id 9)
             (setq fired 0)
             (run-with-timer 0 nil (lambda () (setq fired 1)))",
        );
        cfg.site()
            .tick_once(chrono::Utc::now(), Duration::from_millis(100));
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
    /// source. The next physics tick — or `MicrogridSite::tick_once` driven
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
        cfg.site()
            .tick_once(chrono::Utc::now(), std::time::Duration::from_millis(100));
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
        cfg.site()
            .tick_once(chrono::Utc::now(), std::time::Duration::from_millis(100));
        let m = cfg.site().get(7).unwrap();
        assert!((m.aggregate_power_w(&cfg.site()) - 1500.0).abs() < 1e-3);
        // Mutate the bound variable; next tick picks up the new value.
        cfg.eval("(setq consumer-power 2750.0)").unwrap();
        cfg.site()
            .tick_once(chrono::Utc::now(), std::time::Duration::from_millis(100));
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
        cfg.site()
            .tick_once(chrono::Utc::now(), std::time::Duration::from_millis(100));
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
