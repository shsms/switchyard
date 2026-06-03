//! In-memory dispatch store backing the `MicrogridDispatchService`
//! gRPC surface (`src/dispatch_server.rs`) and the per-microgrid
//! Dispatches view in the UI.
//!
//! Switchyard is a *store-and-serve* dispatch backend, in the spirit
//! of the sibling `dispatchsim` mock: the python dispatch CLI (or any
//! `frequenz-client-dispatch`) creates / updates / deletes dispatches
//! here, the UI lists them, and downstream control apps (e.g. the
//! edge-app) consume the stream and act on them. Switchyard itself
//! never executes a dispatch against its simulated components.
//!
//! State is enterprise-wide but keyed by `microgrid_id` (the dispatch
//! API carries the id in every request, so a single service fronts
//! all microgrids — unlike the Microgrid API's per-port servers). It
//! lives on [`crate::lisp::Config`] so the single gRPC service and the
//! UI handlers share one source of truth; it survives a config reload
//! (the store is not torn down with a microgrid's `MicrogridSite`) but
//! is not persisted across a process restart.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::RwLock;
use tokio::sync::broadcast;

use crate::proto::dispatch as pb;

/// Capacity of the per-store change broadcast ring. Each
/// `StreamMicrogridDispatches` subscription and every UI WebSocket
/// holds a receiver; a subscriber that falls this far behind gets a
/// `Lagged` and re-syncs (the UI refetches `/api/mg/{id}/dispatches`,
/// a streaming client re-lists). Dispatch mutations are rare relative
/// to telemetry, so this is generous headroom.
const CHANGE_BUS_CAPACITY: usize = 256;

/// Which lifecycle transition a [`DispatchEvent`] reports. Maps onto
/// the proto `frequenz.api.common.v1alpha8.streaming.Event` enum in
/// the stream handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DispatchChange {
    Created,
    Updated,
    Deleted,
}

/// A single dispatch lifecycle change, fanned out to the gRPC stream
/// and the UI. Carries the full `Dispatch` (not just its id) so a
/// streaming client can emit the record even for a `Deleted` event,
/// after it has already left the store.
#[derive(Clone, Debug)]
pub struct DispatchEvent {
    pub microgrid_id: u64,
    pub change: DispatchChange,
    pub dispatch: pb::Dispatch,
}

impl DispatchEvent {
    /// The affected dispatch's id (sourced from its metadata).
    pub fn dispatch_id(&self) -> u64 {
        self.dispatch
            .metadata
            .as_ref()
            .map(|m| m.dispatch_id)
            .unwrap_or(0)
    }
}

/// Thread-safe, in-memory dispatch store. Cheap to clone via the
/// `SharedDispatchStore` alias (it's all behind one `Arc`).
pub struct DispatchStore {
    /// `microgrid_id -> (dispatch_id -> Dispatch)`. An outer `BTreeMap`
    /// keeps per-microgrid listing deterministic; the inner one keeps
    /// dispatches ordered by id so an unsorted `List` is still stable.
    inner: RwLock<BTreeMap<u64, BTreeMap<u64, pb::Dispatch>>>,
    /// Monotonic dispatch-id allocator, shared across every microgrid
    /// so ids are globally unique (mirrors production, where each
    /// dispatch — even across microgrids — carries a distinct id).
    next_id: AtomicU64,
    /// Lifecycle change fan-out. `send` returning `Err` (no receivers)
    /// is ignored — a mutation with nobody listening is fine.
    changes: broadcast::Sender<DispatchEvent>,
}

/// Shared handle to the [`DispatchStore`], stored on `Config`.
pub type SharedDispatchStore = Arc<DispatchStore>;

/// Build an empty store. Ids start at 1 (id 0 reads as "unset" on the
/// wire, so the first real dispatch is 1).
pub fn new_store() -> SharedDispatchStore {
    let (changes, _) = broadcast::channel(CHANGE_BUS_CAPACITY);
    Arc::new(DispatchStore {
        inner: RwLock::new(BTreeMap::new()),
        next_id: AtomicU64::new(1),
        changes,
    })
}

impl DispatchStore {
    /// Subscribe to lifecycle changes across all microgrids. Consumers
    /// filter by `microgrid_id` themselves.
    pub fn subscribe(&self) -> broadcast::Receiver<DispatchEvent> {
        self.changes.subscribe()
    }

    /// Allocate the next globally-unique dispatch id.
    pub fn alloc_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Insert a freshly-created dispatch and broadcast `Created`.
    /// Assumes `dispatch.metadata.dispatch_id` was set from
    /// [`alloc_id`](Self::alloc_id), so it can't collide.
    pub fn insert(&self, microgrid_id: u64, dispatch: pb::Dispatch) {
        let id = dispatch_id_of(&dispatch);
        self.inner
            .write()
            .entry(microgrid_id)
            .or_default()
            .insert(id, dispatch.clone());
        self.broadcast(microgrid_id, DispatchChange::Created, dispatch);
    }

    /// Overwrite an existing dispatch in place and broadcast `Updated`.
    /// Returns `false` (and broadcasts nothing) if no dispatch with
    /// that id exists for the microgrid.
    pub fn replace(&self, microgrid_id: u64, dispatch: pb::Dispatch) -> bool {
        let id = dispatch_id_of(&dispatch);
        {
            let mut guard = self.inner.write();
            match guard.get_mut(&microgrid_id).and_then(|m| m.get_mut(&id)) {
                Some(slot) => *slot = dispatch.clone(),
                None => return false,
            }
        }
        self.broadcast(microgrid_id, DispatchChange::Updated, dispatch);
        true
    }

    /// Remove a dispatch, returning it (and broadcasting `Deleted`) if
    /// it existed.
    pub fn remove(&self, microgrid_id: u64, dispatch_id: u64) -> Option<pb::Dispatch> {
        let removed = {
            let mut guard = self.inner.write();
            let gone = guard
                .get_mut(&microgrid_id)
                .and_then(|m| m.remove(&dispatch_id));
            // Drop the now-empty per-microgrid map so an idle microgrid
            // doesn't linger in the outer map forever.
            if guard.get(&microgrid_id).is_some_and(|m| m.is_empty()) {
                guard.remove(&microgrid_id);
            }
            gone
        };
        if let Some(dispatch) = &removed {
            self.broadcast(microgrid_id, DispatchChange::Deleted, dispatch.clone());
        }
        removed
    }

    /// Fetch a single dispatch by id.
    pub fn get(&self, microgrid_id: u64, dispatch_id: u64) -> Option<pb::Dispatch> {
        self.inner
            .read()
            .get(&microgrid_id)
            .and_then(|m| m.get(&dispatch_id))
            .cloned()
    }

    /// All dispatches for a microgrid, ascending by id. Filtering and
    /// sorting for `List` happen in the gRPC handler against this.
    pub fn list_mg(&self, microgrid_id: u64) -> Vec<pb::Dispatch> {
        self.inner
            .read()
            .get(&microgrid_id)
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Total dispatches across all microgrids (test/diagnostic helper).
    pub fn total(&self) -> usize {
        self.inner.read().values().map(BTreeMap::len).sum()
    }

    fn broadcast(&self, microgrid_id: u64, change: DispatchChange, dispatch: pb::Dispatch) {
        let _ = self.changes.send(DispatchEvent {
            microgrid_id,
            change,
            dispatch,
        });
    }
}

fn dispatch_id_of(d: &pb::Dispatch) -> u64 {
    d.metadata.as_ref().map(|m| m.dispatch_id).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dispatch(id: u64, type_: &str) -> pb::Dispatch {
        pb::Dispatch {
            metadata: Some(pb::DispatchMetadata {
                dispatch_id: id,
                ..Default::default()
            }),
            data: Some(pb::DispatchData {
                r#type: type_.to_string(),
                ..Default::default()
            }),
        }
    }

    #[test]
    fn alloc_id_is_monotonic_and_starts_at_one() {
        let store = new_store();
        assert_eq!(store.alloc_id(), 1);
        assert_eq!(store.alloc_id(), 2);
        assert_eq!(store.alloc_id(), 3);
    }

    #[test]
    fn insert_get_list_remove_roundtrip() {
        let store = new_store();
        store.insert(261, dispatch(1, "SET_POWER"));
        store.insert(261, dispatch(2, "CHARGE"));
        store.insert(900, dispatch(3, "SET_POWER"));

        assert_eq!(store.total(), 3);
        // list_mg is scoped to the microgrid and ascending by id.
        let ids: Vec<u64> = store
            .list_mg(261)
            .iter()
            .map(|d| d.metadata.as_ref().unwrap().dispatch_id)
            .collect();
        assert_eq!(ids, vec![1, 2]);
        assert!(store.get(261, 2).is_some());
        // Cross-microgrid isolation: 261's id space doesn't leak into 900.
        assert!(store.get(900, 1).is_none());

        let removed = store.remove(261, 1).expect("dispatch 1 present");
        assert_eq!(removed.data.unwrap().r#type, "SET_POWER");
        assert!(store.get(261, 1).is_none());
        assert_eq!(store.total(), 2);
    }

    #[test]
    fn replace_only_updates_existing() {
        let store = new_store();
        store.insert(1, dispatch(1, "OLD"));
        assert!(store.replace(1, dispatch(1, "NEW")));
        assert_eq!(store.get(1, 1).unwrap().data.unwrap().r#type, "NEW");
        // Unknown id: no-op, reported as false.
        assert!(!store.replace(1, dispatch(99, "GHOST")));
        assert!(!store.replace(2, dispatch(1, "WRONG_MG")));
    }

    #[test]
    fn changes_broadcast_lifecycle_events() {
        let store = new_store();
        let mut rx = store.subscribe();

        store.insert(261, dispatch(1, "SET_POWER"));
        let ev = rx.try_recv().expect("created event");
        assert_eq!(ev.microgrid_id, 261);
        assert_eq!(ev.change, DispatchChange::Created);
        assert_eq!(ev.dispatch_id(), 1);

        store.replace(261, dispatch(1, "SET_POWER_V2"));
        assert_eq!(rx.try_recv().unwrap().change, DispatchChange::Updated);

        store.remove(261, 1);
        let ev = rx.try_recv().expect("deleted event");
        assert_eq!(ev.change, DispatchChange::Deleted);
        // The deleted record rides along even though it's gone from the store.
        assert_eq!(ev.dispatch.data.unwrap().r#type, "SET_POWER_V2");
    }
}
