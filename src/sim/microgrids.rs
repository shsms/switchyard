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

use parking_lot::Mutex;
use serde::Serialize;

use crate::sim::MicrogridSite;

/// Canonical id the binary boots into when `config.lisp` doesn't
/// call `(make-microgrid …)` — preserves the single-microgrid
/// shape every existing sample config + integration test relies
/// on. Matches the prior `(set-microgrid-id 2200)` default in the
/// stock `config.lisp`.
pub const DEFAULT_MICROGRID_ID: u64 = 2200;
/// Default display name for the implicit microgrid.
pub const DEFAULT_MICROGRID_NAME: &str = "default";
/// Default gRPC port for the implicit microgrid. Multi-microgrid
/// configs auto-allocate from this base upward.
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
    let mut ports: Vec<u16> = registry.lock().values().map(|e| e.def.grpc_port).collect();
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

/// Smallest microgrid id not currently registered, starting at
/// `DEFAULT_MICROGRID_ID`. Mirrors `next_free_port`'s shape.
pub fn next_free_id(registry: &SharedMicrogrids) -> u64 {
    let mut ids: Vec<u64> = registry.lock().keys().copied().collect();
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
        reg.lock().insert(DEFAULT_MICROGRID_ID, entry(DEFAULT_MICROGRID_ID, 8800));
        assert_eq!(next_free_id(&reg), DEFAULT_MICROGRID_ID + 1);
    }

    #[test]
    fn snapshot_is_ascending_by_id() {
        let reg = new_registry();
        for id in [2202u64, 2200, 2201] {
            reg.lock().insert(id, entry(id, 8800 + (id as u16 - 2200) * 10));
        }
        let view = snapshot(&reg);
        assert_eq!(view.iter().map(|v| v.id).collect::<Vec<_>>(), vec![2200, 2201, 2202]);
    }
}
