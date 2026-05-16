//! Enterprise-scoped microgrid registry — what the binary knows
//! about *every* microgrid it hosts. One entry per microgrid; each
//! entry pairs the configured metadata (id, display name, gRPC
//! port, optional TSO label) with a clonable handle on that
//! microgrid's `MicrogridSite`.
//!
//! The registry lives behind a `parking_lot::Mutex` because the
//! mutators (the `(make-microgrid …)` defun, the create-microgrid
//! HTTP endpoint) need write access from any tokio task that holds
//! the interpreter lock; the readers (the UI's Microgrids landing
//! page, the gRPC servers, the loopback supervisors, the scenarios
//! auto-advance loop) are short critical sections.
//!
//! Pure data + small helpers — no async, no I/O. The HTTP routing,
//! gRPC server spawning, and scenario per-microgrid replay sit one
//! layer up.

use std::collections::BTreeMap;
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use serde::Serialize;

use crate::sim::MicrogridSite;

/// Canonical id used as the starting point when `make-microgrid` is
/// called without an explicit `:id`. The DSL's auto-allocator
/// (`next_free_id`) picks the lowest free id at or above this; tests
/// and the stock `config.lisp` pin 2200 explicitly.
pub const DEFAULT_MICROGRID_ID: u64 = 2200;
/// Default display name when `make-microgrid` is called without
/// `:name`.
pub const DEFAULT_MICROGRID_NAME: &str = "default";
/// Starting gRPC port for the auto-port allocator
/// (`next_free_port`). Multi-microgrid configs auto-allocate from
/// this base upward.
pub const DEFAULT_GRPC_PORT: u16 = 8800;

#[derive(Clone, Debug, Serialize)]
pub struct MicrogridDef {
    pub id: u64,
    pub name: String,
    pub grpc_port: u16,
    /// Optional informational TSO zone label (e.g. `"TN"`, `"AM"`,
    /// `"HZ"`, `"BW"`). No behavioural effect for v1; a chip on the
    /// Microgrids landing page surfaces it for operators that care.
    pub tso: Option<String>,
}

#[derive(Clone)]
pub struct MicrogridEntry {
    pub def: MicrogridDef,
    pub site: MicrogridSite,
}

pub type SharedMicrogrids = Arc<Mutex<BTreeMap<u64, MicrogridEntry>>>;

pub fn new_registry() -> SharedMicrogrids {
    Arc::new(Mutex::new(BTreeMap::new()))
}

/// JSON-serialisable snapshot used by `GET /api/microgrids`. Drops
/// the `MicrogridSite` handle (not serialisable) and folds in a
/// component_count derived from the underlying site so the UI's
/// card grid can render counts without a second round-trip.
#[derive(Clone, Debug, Serialize)]
pub struct MicrogridView {
    pub id: u64,
    pub name: String,
    pub grpc_port: u16,
    pub tso: Option<String>,
    pub component_count: usize,
}

impl From<&MicrogridEntry> for MicrogridView {
    fn from(e: &MicrogridEntry) -> Self {
        MicrogridView {
            id: e.def.id,
            name: e.def.name.clone(),
            grpc_port: e.def.grpc_port,
            tso: e.def.tso.clone(),
            component_count: e.site.components().len(),
        }
    }
}

/// Snapshot the registry into a list of `MicrogridView`s, sorted
/// ascending by id. Driver for `GET /api/microgrids` + `swctl
/// microgrids list`.
pub fn snapshot(registry: &SharedMicrogrids) -> Vec<MicrogridView> {
    registry.lock().values().map(MicrogridView::from).collect()
}

/// Smallest port not currently claimed by any registered microgrid,
/// starting at `DEFAULT_GRPC_PORT`. Used by the create-microgrid
/// HTTP endpoint when the caller didn't pin a port explicitly.
pub fn next_free_port(registry: &SharedMicrogrids) -> u16 {
    next_free_port_in(&registry.lock())
}

/// Like [`next_free_port`] but operates on an already-locked map.
/// Lets the create-microgrid handler pick id + port + insert under
/// one critical section so two concurrent creates can't both pick
/// the same port.
pub fn next_free_port_in(entries: &BTreeMap<u64, MicrogridEntry>) -> u16 {
    let mut ports: Vec<u16> = entries.values().map(|e| e.def.grpc_port).collect();
    ports.sort_unstable();
    let mut candidate = DEFAULT_GRPC_PORT;
    for p in ports {
        if p == candidate {
            candidate = candidate.saturating_add(10);
        } else if p > candidate {
            break;
        }
    }
    candidate
}

/// Dynamic "current microgrid" pointer + registry lookup. Every
/// lisp setter / make-* defun captures a `SharedSiteRouter` and
/// calls `.site()` at the *moment of invocation* to find the
/// `MicrogridSite` it should act on:
///
///   1. If `current_microgrid` is set (via /api/mg/{id}/eval, the
///      scenarios per-microgrid replay, or the (make-microgrid)
///      body), and that id resolves to an entry in the registry,
///      return that entry's site.
///   2. Otherwise return the first registry entry's site
///      (BTreeMap min-id ordering), so single-microgrid configs
///      keep working without ever touching `current_microgrid`.
///   3. Otherwise fall back to the bootstrap site supplied at
///      Router construction time — covers the brief window
///      between `Config::new` allocating its initial site and
///      the config eval running its first `(make-microgrid …)`
///      form.
///
/// Holding a `SharedSiteRouter` is cheap (Arc clone); the inner
/// `RwLock` only contends with the rare write path that flips
/// `current_microgrid`.
pub struct SiteRouter {
    registry: SharedMicrogrids,
    current: Arc<RwLock<Option<u64>>>,
    bootstrap: MicrogridSite,
}

pub type SharedSiteRouter = Arc<SiteRouter>;

impl SiteRouter {
    pub fn new(
        registry: SharedMicrogrids,
        current: Arc<RwLock<Option<u64>>>,
        bootstrap: MicrogridSite,
    ) -> SharedSiteRouter {
        Arc::new(Self {
            registry,
            current,
            bootstrap,
        })
    }

    /// Resolve the active site under the rules above. Cheap clone
    /// of the underlying `MicrogridSite` (`Arc<MicrogridSiteInner>`
    /// inside).
    pub fn site(&self) -> MicrogridSite {
        if let Some(id) = *self.current.read()
            && let Some(entry) = self.registry.lock().get(&id)
        {
            return entry.site.clone();
        }
        if let Some(entry) = self.registry.lock().values().next() {
            return entry.site.clone();
        }
        self.bootstrap.clone()
    }
}

/// Shared `Arc<RwLock<Option<u64>>>` carrying the active microgrid
/// id. Lifted out so callers (HTTP handlers, scenario tick) can
/// flip it via `with_microgrid`.
pub type CurrentMicrogrid = Arc<RwLock<Option<u64>>>;

pub fn new_current_microgrid() -> CurrentMicrogrid {
    Arc::new(RwLock::new(None))
}

/// Run `f` with `current_microgrid` temporarily set to `id`, then
/// restore the prior value. Single-threaded usage is assumed (the
/// interpreter ctx serialises eval anyway); `RwLock` is the
/// cheapest read-mostly primitive that fits.
pub fn with_microgrid<R>(current: &CurrentMicrogrid, id: u64, f: impl FnOnce() -> R) -> R {
    let prior = current.write().replace(id);
    let out = f();
    *current.write() = prior;
    out
}

/// Smallest microgrid id not currently registered, starting at
/// `DEFAULT_MICROGRID_ID`. Mirrors `next_free_port`'s shape.
pub fn next_free_id(registry: &SharedMicrogrids) -> u64 {
    next_free_id_in(&registry.lock())
}

/// Like [`next_free_id`] but operates on an already-locked map.
/// Pairs with [`next_free_port_in`] so the create-microgrid path
/// can pick both + insert in one critical section.
pub fn next_free_id_in(entries: &BTreeMap<u64, MicrogridEntry>) -> u64 {
    let mut ids: Vec<u64> = entries.keys().copied().collect();
    ids.sort_unstable();
    let mut candidate = DEFAULT_MICROGRID_ID;
    for id in ids {
        if id == candidate {
            candidate = candidate.saturating_add(1);
        } else if id > candidate {
            break;
        }
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::MicrogridSite;

    fn entry(id: u64, port: u16) -> MicrogridEntry {
        MicrogridEntry {
            def: MicrogridDef {
                id,
                name: format!("mg-{id}"),
                grpc_port: port,
                tso: None,
            },
            site: MicrogridSite::new(),
        }
    }

    #[test]
    fn next_free_port_increments_by_10_skipping_gaps() {
        let reg = new_registry();
        // Empty registry -> default port.
        assert_eq!(next_free_port(&reg), DEFAULT_GRPC_PORT);
        // Add 8800; next is 8810.
        reg.lock().insert(2200, entry(2200, 8800));
        assert_eq!(next_free_port(&reg), 8810);
        // Add 8810 too; next is 8820.
        reg.lock().insert(2201, entry(2201, 8810));
        assert_eq!(next_free_port(&reg), 8820);
        // Add 8830 (gap); next slot is still 8820.
        reg.lock().insert(2202, entry(2202, 8830));
        assert_eq!(next_free_port(&reg), 8820);
    }

    #[test]
    fn next_free_id_starts_at_default_and_increments() {
        let reg = new_registry();
        assert_eq!(next_free_id(&reg), DEFAULT_MICROGRID_ID);
        reg.lock()
            .insert(DEFAULT_MICROGRID_ID, entry(DEFAULT_MICROGRID_ID, 8800));
        assert_eq!(next_free_id(&reg), DEFAULT_MICROGRID_ID + 1);
    }

    #[test]
    fn snapshot_is_ascending_by_id() {
        let reg = new_registry();
        for id in [2202u64, 2200, 2201] {
            reg.lock()
                .insert(id, entry(id, 8800 + (id as u16 - 2200) * 10));
        }
        let view = snapshot(&reg);
        assert_eq!(
            view.iter().map(|v| v.id).collect::<Vec<_>>(),
            vec![2200, 2201, 2202]
        );
    }
}
