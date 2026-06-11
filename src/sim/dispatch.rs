//! In-memory dispatch store backing the `MicrogridDispatchService`
//! gRPC surface (`src/dispatch_server.rs`) and the per-microgrid
//! Dispatches view in the UI.
//!
//! Switchyard is a *store-and-serve* dispatch backend: the python
//! dispatch CLI (or any `frequenz-client-dispatch`) creates /
//! updates / deletes dispatches
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

use chrono::{DateTime, TimeZone, Utc};
use parking_lot::RwLock;
use prost_types::Timestamp;
use tokio::sync::broadcast;

use crate::proto::dispatch as pb;

/// Why a dispatch mutation was rejected. The transport layers map it
/// to their own error shape — the gRPC server to a `tonic::Status`,
/// the UI to an HTTP status — so the domain rule lives in one place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DispatchError {
    /// Create was asked for a dispatch with neither a `start_time` nor
    /// `start_immediately`.
    MissingStartTime,
    /// No dispatch with that id exists for the microgrid.
    NotFound,
}

impl std::fmt::Display for DispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingStartTime => {
                write!(f, "start_time is required unless start_immediately is set")
            }
            Self::NotFound => write!(f, "dispatch not found"),
        }
    }
}

impl std::error::Error for DispatchError {}

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

    /// Build and store a new dispatch from caller-supplied
    /// `DispatchData`, returning the stored `Dispatch`. This is the one
    /// construction path — the gRPC `CreateMicrogridDispatch` handler
    /// and the UI's create endpoint both go through it, so id
    /// allocation, timestamping, and `end_time` derivation stay
    /// identical regardless of who created the dispatch.
    ///
    /// `start_immediately` overrides `start_time` to server-now. A
    /// dispatch with neither is rejected ([`DispatchError::MissingStartTime`]).
    /// A past `start_time` is intentionally accepted (see the module
    /// docs): switchyard is a sim backend, and an already-started
    /// dispatch is a useful thing to create for downstream testing.
    pub fn create(
        &self,
        microgrid_id: u64,
        mut data: pb::DispatchData,
        start_immediately: bool,
    ) -> Result<pb::Dispatch, DispatchError> {
        let now = Utc::now();
        if start_immediately {
            data.start_time = Some(to_ts(now));
        }
        if data.start_time.is_none() {
            return Err(DispatchError::MissingStartTime);
        }
        let recurring = is_recurring(&data);
        let end_time = compute_end_time(data.start_time.as_ref(), data.duration, recurring);
        let dispatch = pb::Dispatch {
            metadata: Some(pb::DispatchMetadata {
                dispatch_id: self.alloc_id(),
                create_time: Some(to_ts(now)),
                update_time: Some(to_ts(now)),
                end_time,
            }),
            data: Some(data),
        };
        self.insert(microgrid_id, dispatch.clone());
        Ok(dispatch)
    }

    /// Apply `f` to a stored dispatch under ONE write-lock
    /// acquisition, re-stamp `update_time` / `end_time`, and
    /// broadcast `Updated`. This closes the read-modify-write window
    /// a get → merge → replace sequence leaves open: two concurrent
    /// updates (or an update racing a pause) would otherwise drop one
    /// of the writes wholesale, last-replace-wins.
    pub fn update_with(
        &self,
        microgrid_id: u64,
        dispatch_id: u64,
        f: impl FnOnce(&mut pb::Dispatch),
    ) -> Result<pb::Dispatch, DispatchError> {
        let updated = {
            let mut guard = self.inner.write();
            let slot = guard
                .get_mut(&microgrid_id)
                .and_then(|m| m.get_mut(&dispatch_id))
                .ok_or(DispatchError::NotFound)?;
            f(slot);
            stamp_updated(slot);
            slot.clone()
        };
        self.broadcast(microgrid_id, DispatchChange::Updated, updated.clone());
        Ok(updated)
    }

    /// Flip a dispatch's `is_active` flag (pause / resume), re-stamping
    /// `update_time` + `end_time` and broadcasting `Updated`. Returns
    /// [`DispatchError::NotFound`] if the dispatch is gone.
    pub fn set_active(
        &self,
        microgrid_id: u64,
        dispatch_id: u64,
        active: bool,
    ) -> Result<pb::Dispatch, DispatchError> {
        self.update_with(microgrid_id, dispatch_id, |dispatch| {
            if let Some(data) = dispatch.data.as_mut() {
                data.is_active = active;
            }
        })
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

// --- shared dispatch helpers ----------------------------------------------
//
// Time + recurrence helpers used by both the store (create / set_active)
// and the gRPC server's field-mask update. Kept here so the one
// definition of "how end_time is derived" serves every write path.

pub(crate) fn to_ts(dt: DateTime<Utc>) -> Timestamp {
    Timestamp {
        seconds: dt.timestamp(),
        nanos: dt.timestamp_subsec_nanos() as i32,
    }
}

pub(crate) fn ts_to_dt(ts: &Timestamp) -> Option<DateTime<Utc>> {
    Utc.timestamp_opt(ts.seconds, ts.nanos.max(0) as u32)
        .single()
}

/// A dispatch recurs if its rule carries a real (non-`UNSPECIFIED`)
/// frequency.
pub(crate) fn is_recurring(data: &pb::DispatchData) -> bool {
    data.recurrence
        .as_ref()
        .is_some_and(|r| r.freq != pb::recurrence_rule::Frequency::Unspecified as i32)
}

/// `end_time` is only calculable for a one-off (non-recurring) dispatch
/// with a finite duration. Indefinite (no duration) and recurring
/// dispatches have no single predetermined end, so it stays unset.
pub(crate) fn compute_end_time(
    start: Option<&Timestamp>,
    duration_s: Option<u32>,
    recurring: bool,
) -> Option<Timestamp> {
    if recurring {
        return None;
    }
    let start = ts_to_dt(start?)?;
    // checked_add_signed (not `+`, which `.expect()`s) so a far-future
    // start + long duration that overflows chrono's date range yields
    // "no end time" rather than panicking the create/update handler.
    let end = start.checked_add_signed(chrono::Duration::seconds(duration_s? as i64))?;
    Some(to_ts(end))
}

/// Re-stamp `update_time` to now and re-derive `end_time` from the
/// current data. Used after any in-place edit (set_active, field-mask
/// update).
pub(crate) fn stamp_updated(dispatch: &mut pb::Dispatch) {
    let now = Utc::now();
    let (start, duration, recurring) = match dispatch.data.as_ref() {
        Some(d) => (d.start_time, d.duration, is_recurring(d)),
        None => (None, None, false),
    };
    let end_time = compute_end_time(start.as_ref(), duration, recurring);
    if let Some(meta) = dispatch.metadata.as_mut() {
        meta.update_time = Some(to_ts(now));
        meta.end_time = end_time;
    }
}

// --- human-input parsing (shared by the UI create endpoint + swctl) -------

fn category_from_alias(token: &str) -> Option<i32> {
    use crate::proto::common::microgrid::electrical_components::ElectricalComponentCategory as Cat;
    let cat = match token.to_lowercase().as_str() {
        "battery" => Cat::Battery,
        "grid" => Cat::GridConnectionPoint,
        "meter" => Cat::Meter,
        "inverter" => Cat::Inverter,
        "ev_charger" | "ev-charger" | "evcharger" => Cat::EvCharger,
        "chp" => Cat::Chp,
        _ => return None,
    };
    Some(cat as i32)
}

/// Parse a target spec into proto `TargetComponents`. Accepts either a
/// comma-separated list of numeric component ids (`"1,2,3"`) or a
/// comma-separated list of category names (`"battery,grid"`); the two
/// can't be mixed. Mirrors the `frequenz-client-dispatch` CLI's target
/// argument so the UI / swctl take the same syntax.
pub fn parse_target(spec: &str) -> Result<pb::TargetComponents, String> {
    use pb::target_components::{CategoryAndType, CategoryTypeSet, Components, IdSet};
    let tokens: Vec<&str> = spec
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.is_empty() {
        return Err("target is empty".to_string());
    }
    // All tokens numeric => a set of component ids.
    if let Some(ids) = tokens
        .iter()
        .map(|t| t.parse::<u64>().ok())
        .collect::<Option<Vec<u64>>>()
    {
        return Ok(pb::TargetComponents {
            components: Some(Components::ComponentIds(IdSet { ids })),
        });
    }
    // Otherwise category names (no id/category mixing).
    let mut categories = Vec::with_capacity(tokens.len());
    for token in &tokens {
        match category_from_alias(token) {
            Some(category) => categories.push(CategoryAndType {
                category,
                r#type: None,
            }),
            None => {
                return Err(format!(
                    "unknown target {token:?}; expected component ids (e.g. \"1,2\") or \
                     categories (battery, grid, meter, inverter, ev_charger, chp)"
                ));
            }
        }
    }
    Ok(pb::TargetComponents {
        components: Some(Components::ComponentCategoriesTypes(CategoryTypeSet {
            categories,
        })),
    })
}

/// Human label for an `ElectricalComponentCategory` value, e.g.
/// `BATTERY` — strips the verbose proto enum prefix, or shows the raw
/// number for an unknown value.
pub fn electrical_category_label(cat: i32) -> String {
    use crate::proto::common::microgrid::electrical_components::ElectricalComponentCategory as Cat;
    match Cat::try_from(cat) {
        Ok(c) => c
            .as_str_name()
            .strip_prefix("ELECTRICAL_COMPONENT_CATEGORY_")
            .unwrap_or(c.as_str_name())
            .to_string(),
        Err(_) => format!("category {cat}"),
    }
}

/// Render a dispatch target as a short human string — the inverse of
/// [`parse_target`], used by the UI list + swctl. Matches on
/// component-id and category-set shapes; the deprecated bare-category
/// set is handled too so an older dispatch still renders.
#[allow(deprecated)]
pub fn target_to_string(target: Option<&pb::TargetComponents>) -> String {
    use pb::target_components::Components;
    match target.and_then(|t| t.components.as_ref()) {
        None => "—".to_string(),
        Some(Components::ComponentIds(ids)) => {
            let list = ids
                .ids
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            format!("ids: {list}")
        }
        Some(Components::ComponentCategories(c)) => c
            .categories
            .iter()
            .map(|cat| electrical_category_label(*cat))
            .collect::<Vec<_>>()
            .join(", "),
        Some(Components::ComponentCategoriesTypes(c)) => c
            .categories
            .iter()
            .map(|ct| electrical_category_label(ct.category))
            .collect::<Vec<_>>()
            .join(", "),
    }
}

// --- payload <-> JSON conversion (google.protobuf.Struct) -----------------

/// Convert a JSON value to a proto `Struct`. Errors unless the value is
/// a JSON object — a dispatch `payload` is a `google.protobuf.Struct`,
/// which is keyed at the top level.
pub fn json_to_struct(value: &serde_json::Value) -> Result<prost_types::Struct, String> {
    match value {
        serde_json::Value::Object(map) => Ok(prost_types::Struct {
            fields: map
                .iter()
                .map(|(k, v)| (k.clone(), prost_value_from_json(v)))
                .collect(),
        }),
        _ => Err("payload must be a JSON object".to_string()),
    }
}

fn prost_value_from_json(value: &serde_json::Value) -> prost_types::Value {
    use prost_types::value::Kind;
    use serde_json::Value as J;
    let kind = match value {
        J::Null => Kind::NullValue(0),
        J::Bool(b) => Kind::BoolValue(*b),
        J::Number(n) => Kind::NumberValue(n.as_f64().unwrap_or(0.0)),
        J::String(s) => Kind::StringValue(s.clone()),
        J::Array(items) => Kind::ListValue(prost_types::ListValue {
            values: items.iter().map(prost_value_from_json).collect(),
        }),
        J::Object(map) => Kind::StructValue(prost_types::Struct {
            fields: map
                .iter()
                .map(|(k, v)| (k.clone(), prost_value_from_json(v)))
                .collect(),
        }),
    };
    prost_types::Value { kind: Some(kind) }
}

/// Convert a proto `Struct` back to a JSON value (for the UI's
/// dispatch list).
pub fn struct_to_json(s: &prost_types::Struct) -> serde_json::Value {
    let mut map = serde_json::Map::with_capacity(s.fields.len());
    for (k, v) in &s.fields {
        map.insert(k.clone(), prost_value_to_json(v));
    }
    serde_json::Value::Object(map)
}

fn prost_value_to_json(v: &prost_types::Value) -> serde_json::Value {
    use prost_types::value::Kind;
    use serde_json::Value as J;
    match &v.kind {
        None | Some(Kind::NullValue(_)) => J::Null,
        Some(Kind::NumberValue(n)) => serde_json::Number::from_f64(*n).map_or(J::Null, J::Number),
        Some(Kind::StringValue(s)) => J::String(s.clone()),
        Some(Kind::BoolValue(b)) => J::Bool(*b),
        Some(Kind::StructValue(st)) => struct_to_json(st),
        Some(Kind::ListValue(l)) => J::Array(l.values.iter().map(prost_value_to_json).collect()),
    }
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
        store.insert(261, dispatch(1, "alpha"));
        store.insert(261, dispatch(2, "beta"));
        store.insert(900, dispatch(3, "alpha"));

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
        assert_eq!(removed.data.unwrap().r#type, "alpha");
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

        store.insert(261, dispatch(1, "alpha"));
        let ev = rx.try_recv().expect("created event");
        assert_eq!(ev.microgrid_id, 261);
        assert_eq!(ev.change, DispatchChange::Created);
        assert_eq!(ev.dispatch_id(), 1);

        store.replace(261, dispatch(1, "alpha-v2"));
        assert_eq!(rx.try_recv().unwrap().change, DispatchChange::Updated);

        store.remove(261, 1);
        let ev = rx.try_recv().expect("deleted event");
        assert_eq!(ev.change, DispatchChange::Deleted);
        // The deleted record rides along even though it's gone from the store.
        assert_eq!(ev.dispatch.data.unwrap().r#type, "alpha-v2");
    }

    #[test]
    fn create_stamps_id_times_and_end() {
        let store = new_store();
        let data = pb::DispatchData {
            r#type: "alpha".to_string(),
            duration: Some(3600),
            ..Default::default()
        };
        // start_immediately supplies the start time, so this succeeds
        // despite `data.start_time` being None.
        let d = store.create(261, data, true).expect("create ok");
        let meta = d.metadata.unwrap();
        assert_eq!(meta.dispatch_id, 1);
        assert!(meta.create_time.is_some() && meta.update_time.is_some());
        // Finite one-off => end_time = start + duration.
        let start = d.data.unwrap().start_time.unwrap();
        assert_eq!(meta.end_time.unwrap().seconds, start.seconds + 3600);
        assert!(store.get(261, 1).is_some());
    }

    #[test]
    fn create_requires_a_start_time() {
        let store = new_store();
        let err = store
            .create(1, pb::DispatchData::default(), false)
            .unwrap_err();
        assert_eq!(err, DispatchError::MissingStartTime);
        assert_eq!(store.total(), 0);
    }

    #[test]
    fn set_active_toggles_and_broadcasts() {
        let store = new_store();
        let data = pb::DispatchData {
            r#type: "X".to_string(),
            is_active: true,
            ..Default::default()
        };
        let created = store.create(7, data, true).unwrap();
        let id = created.metadata.unwrap().dispatch_id;
        let mut rx = store.subscribe();

        let paused = store.set_active(7, id, false).unwrap();
        assert!(!paused.data.unwrap().is_active);
        assert_eq!(rx.try_recv().unwrap().change, DispatchChange::Updated);
        // The stored copy reflects the pause.
        assert!(!store.get(7, id).unwrap().data.unwrap().is_active);

        // Unknown dispatch => NotFound, nothing broadcast.
        assert_eq!(
            store.set_active(7, 999, true).unwrap_err(),
            DispatchError::NotFound
        );
    }

    #[test]
    fn parse_target_handles_ids_categories_and_errors() {
        use crate::proto::common::microgrid::electrical_components::ElectricalComponentCategory;
        use pb::target_components::Components;

        // Numeric list => component ids.
        match parse_target("1, 2, 3").unwrap().components.unwrap() {
            Components::ComponentIds(set) => assert_eq!(set.ids, vec![1, 2, 3]),
            other => panic!("expected ids, got {other:?}"),
        }
        // Category names => category-type set (grid aliases to the
        // GRID_CONNECTION_POINT category).
        match parse_target("battery,grid").unwrap().components.unwrap() {
            Components::ComponentCategoriesTypes(set) => {
                let cats: Vec<i32> = set.categories.iter().map(|c| c.category).collect();
                assert_eq!(
                    cats,
                    vec![
                        ElectricalComponentCategory::Battery as i32,
                        ElectricalComponentCategory::GridConnectionPoint as i32,
                    ]
                );
            }
            other => panic!("expected categories, got {other:?}"),
        }
        assert!(parse_target("").is_err());
        assert!(parse_target("nonsense").is_err());
    }

    #[test]
    fn payload_json_struct_roundtrip() {
        let json = serde_json::json!({
            "target_power_w": 5000.0,
            "nested": {"on": true, "tags": ["a", "b"]},
        });
        let st = json_to_struct(&json).expect("object converts");
        // Round-trips back to the same JSON.
        assert_eq!(struct_to_json(&st), json);
        // A non-object payload is rejected.
        assert!(json_to_struct(&serde_json::json!(5)).is_err());
    }

    #[test]
    fn compute_end_time_never_panics_on_extreme_inputs() {
        // start beyond chrono's representable range => no end, no panic.
        let huge = Timestamp {
            seconds: i64::MAX,
            nanos: 0,
        };
        assert!(compute_end_time(Some(&huge), Some(3600), false).is_none());
        // A near-max start + a long duration overflows the add => still None.
        let near_max = Timestamp {
            seconds: 8_210_000_000_000,
            nanos: 0,
        };
        assert!(compute_end_time(Some(&near_max), Some(u32::MAX), false).is_none());
        // A normal finite, non-recurring dispatch still gets an end_time.
        let t = Timestamp {
            seconds: 1_700_000_000,
            nanos: 0,
        };
        assert_eq!(
            compute_end_time(Some(&t), Some(3600), false)
                .unwrap()
                .seconds,
            1_700_003_600
        );
    }
}
