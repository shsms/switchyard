//! Lisp glue: load the config DSL, register the `make-*` functions
//! against a `MicrogridSite`, and act as the runtime entry point for the gRPC
//! server (which calls into us for `set_active_setpoint` and friends).
//!
//! The `Config` struct is intentionally thin — the simulation state
//! lives in `MicrogridSite`, the lisp interpreter is just the configuration
//! frontend. Behaviour is fanned out across child modules:
//!
//! - `boot` — `Config::new`, the long-lived loops (lisp refresh,
//!   request-timeout sweep), the tags-table pass, hot-reload + watch.
//! - `overrides` — `eval` + persisted-override file plumbing.
//! - `snapshots` — `save_snapshot` / `load_snapshot` against the
//!   per-microgrid overrides file.
//! - `defuns` — every `ctx.defun(...)` installer the config DSL
//!   exposes.

mod boot;
pub mod csv_profile;
mod defuns;
pub mod handle;
pub mod make;
mod overrides;
pub mod runtime_modes;
mod snapshots;
pub mod value;

#[cfg(test)]
mod test_support;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::{Mutex, RwLock};
use tokio::sync::broadcast;
use tulisp::{SharedMut, TulispContext};

use crate::sim::MicrogridSite;
use crate::sim::microgrids::{CurrentMicrogrid, SharedSiteRouter};

pub use overrides::PersistedOverride;

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
    pub(crate) filename: String,
    pub(crate) ctx: SharedMut<TulispContext>,
    pub(crate) site: MicrogridSite,
    pub(crate) metadata: Arc<RwLock<Metadata>>,
    /// Additional files the config has registered via `(watch-file …)`.
    /// `Config::watch` adds each to the live notify watcher so edits to
    /// e.g. `sim/defaults.lisp` trigger the same reload as edits to
    /// the entry-point config. Set semantics — duplicate registrations
    /// (from re-runs of the config during reload) are no-ops.
    pub(crate) extra_watches: Arc<Mutex<HashSet<PathBuf>>>,
    /// Latest topology-validation outcome from the graph crate.
    /// `None` = healthy; `Some(message)` = the validator rejected the
    /// current site. `boot::log_topology_validation` updates this on
    /// every boot + reload; `/api/topology` exposes it so the pulse-
    /// bar graph pill (see UI-design.org §Z6) can flip between ✓ and
    /// ⚠ without polling a separate endpoint.
    pub(crate) graph_status: Arc<RwLock<Option<String>>>,
    /// Configured display timezone. UI's TZ toggle reads the IANA
    /// name from /api/clock and formats timestamps client-side via
    /// `Intl.DateTimeFormat(..., { timeZone })`. Mutated by
    /// `(set-timezone "…")` in config.lisp; default Europe/Berlin
    /// matches the canonical European-intraday demo target.
    pub(crate) clock: crate::sim::clock::SharedClock,
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

impl Config {
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
    pub fn enterprise_id_allocator(&self) -> Arc<std::sync::atomic::AtomicU64> {
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
    pub fn current_microgrid_handle(&self) -> CurrentMicrogrid {
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
}
