//! gRPC server: implements the Frequenz Dispatch API
//! (`frequenz.api.dispatch.v1.MicrogridDispatchService`) on top of
//! switchyard's in-memory [`DispatchStore`](crate::sim::dispatch).
//!
//! This is a *store-and-serve* backend: it owns the full CRUD surface
//! (Create / Get / Update / Delete / List / Stream) so the python
//! dispatch CLI — or any `frequenz-client-dispatch` — can manage
//! dispatches, the UI can list them per microgrid, and downstream
//! control apps consume the stream and act. Switchyard never executes
//! a dispatch against its own simulated components (see the
//! `crate::sim::dispatch` module docs).
//!
//! One service fronts every microgrid: each request carries its
//! `microgrid_id`, so unlike the per-port Microgrid API this binds a
//! single socket (default `[::1]:8900`, see `dispatch_socket_addr`).
//! Auth is intentionally ignored — request metadata (auth key /
//! signature) is never inspected, matching the sim context and the
//! sibling `dispatchsim` mock.

use std::pin::Pin;

use chrono::{DateTime, TimeZone, Utc};
use prost_types::Timestamp;
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;

use crate::proto::common::pagination::{PaginationInfo, PaginationParams, pagination_params};
use crate::proto::common::streaming::Event;
use crate::proto::common::types::Interval;
use crate::proto::dispatch as pb;
use crate::sim::dispatch::{DispatchChange, SharedDispatchStore};

pub struct DispatchServer {
    pub store: SharedDispatchStore,
}

impl DispatchServer {
    pub fn new(store: SharedDispatchStore) -> Self {
        Self { store }
    }
}

#[tonic::async_trait]
impl pb::microgrid_dispatch_service_server::MicrogridDispatchService for DispatchServer {
    type StreamMicrogridDispatchesStream = Pin<
        Box<dyn Stream<Item = Result<pb::StreamMicrogridDispatchesResponse, tonic::Status>> + Send>,
    >;

    async fn create_microgrid_dispatch(
        &self,
        request: tonic::Request<pb::CreateMicrogridDispatchRequest>,
    ) -> Result<tonic::Response<pb::CreateMicrogridDispatchResponse>, tonic::Status> {
        let req = request.into_inner();
        let mut data = req
            .dispatch_data
            .ok_or_else(|| tonic::Status::invalid_argument("dispatch_data is required"))?;

        let now = Utc::now();
        // `start_immediately` overrides the start time to server-now.
        if req.start_immediately == Some(true) {
            data.start_time = Some(to_ts(now));
        }
        // The production API rejects start times in the past. Switchyard
        // is a simulator backend, so it is deliberately lenient: a past
        // start_time is accepted (it's how you create an already-running
        // dispatch for downstream testing, the trick the dispatchsim mock
        // uses). We only require that *some* start time is present.
        if data.start_time.is_none() {
            return Err(tonic::Status::invalid_argument(
                "start_time is required unless start_immediately is set",
            ));
        }

        let id = self.store.alloc_id();
        let recurring = is_recurring(&data);
        let end_time = compute_end_time(data.start_time.as_ref(), data.duration, recurring);
        let dispatch = pb::Dispatch {
            metadata: Some(pb::DispatchMetadata {
                dispatch_id: id,
                create_time: Some(to_ts(now)),
                update_time: Some(to_ts(now)),
                end_time,
            }),
            data: Some(data),
        };
        self.store.insert(req.microgrid_id, dispatch.clone());
        log::info!(
            "CreateMicrogridDispatch(microgrid_id={}) -> dispatch {id}",
            req.microgrid_id
        );
        Ok(tonic::Response::new(pb::CreateMicrogridDispatchResponse {
            dispatch: Some(dispatch),
        }))
    }

    async fn get_microgrid_dispatch(
        &self,
        request: tonic::Request<pb::GetMicrogridDispatchRequest>,
    ) -> Result<tonic::Response<pb::GetMicrogridDispatchResponse>, tonic::Status> {
        let req = request.into_inner();
        let dispatch = self
            .store
            .get(req.microgrid_id, req.dispatch_id)
            .ok_or_else(|| not_found(req.microgrid_id, req.dispatch_id))?;
        Ok(tonic::Response::new(pb::GetMicrogridDispatchResponse {
            microgrid_id: req.microgrid_id,
            dispatch: Some(dispatch),
        }))
    }

    async fn delete_microgrid_dispatch(
        &self,
        request: tonic::Request<pb::DeleteMicrogridDispatchRequest>,
    ) -> Result<tonic::Response<pb::DeleteMicrogridDispatchResponse>, tonic::Status> {
        let req = request.into_inner();
        self.store
            .remove(req.microgrid_id, req.dispatch_id)
            .ok_or_else(|| not_found(req.microgrid_id, req.dispatch_id))?;
        log::info!(
            "DeleteMicrogridDispatch(microgrid_id={}, dispatch_id={})",
            req.microgrid_id,
            req.dispatch_id
        );
        Ok(tonic::Response::new(pb::DeleteMicrogridDispatchResponse {
            microgrid_id: req.microgrid_id,
            dispatch_id: req.dispatch_id,
        }))
    }

    async fn update_microgrid_dispatch(
        &self,
        request: tonic::Request<pb::UpdateMicrogridDispatchRequest>,
    ) -> Result<tonic::Response<pb::UpdateMicrogridDispatchResponse>, tonic::Status> {
        let req = request.into_inner();
        let mut dispatch = self
            .store
            .get(req.microgrid_id, req.dispatch_id)
            .ok_or_else(|| not_found(req.microgrid_id, req.dispatch_id))?;
        let update = req
            .update
            .ok_or_else(|| tonic::Status::invalid_argument("update is required"))?;

        // Honor the field mask when present and non-empty: only the
        // listed paths are touched. With no mask, apply every field the
        // caller set (the lenient interpretation the high-level client's
        // mask-less calls expect). A path's presence in the mask is what
        // distinguishes "clear duration" (-> indefinite) from "leave it".
        let masked: Option<std::collections::HashSet<&str>> = req
            .update_mask
            .as_ref()
            .filter(|m| !m.paths.is_empty())
            .map(|m| m.paths.iter().map(String::as_str).collect());
        let want = |path: &str| masked.as_ref().is_none_or(|s| s.contains(path));

        let data = dispatch.data.get_or_insert_with(Default::default);
        if want("start_time") && update.start_time.is_some() {
            data.start_time = update.start_time;
        }
        if want("duration") && (masked.is_some() || update.duration.is_some()) {
            // Under an explicit mask, a None clears the duration
            // (indefinite); mask-less, only a present value applies.
            data.duration = update.duration;
        }
        if want("target") && update.target.is_some() {
            data.target = update.target;
        }
        if want("is_active")
            && let Some(active) = update.is_active
        {
            data.is_active = active;
        }
        if want("payload") && update.payload.is_some() {
            data.payload = update.payload;
        }
        if want("recurrence")
            && let Some(rec) = &update.recurrence
        {
            data.recurrence = Some(recurrence_from_update(rec));
        }

        // Re-derive update_time + end_time after the merge.
        let now = Utc::now();
        let recurring = is_recurring(data);
        let end_time = compute_end_time(data.start_time.as_ref(), data.duration, recurring);
        if let Some(meta) = dispatch.metadata.as_mut() {
            meta.update_time = Some(to_ts(now));
            meta.end_time = end_time;
        }

        self.store.replace(req.microgrid_id, dispatch.clone());
        log::info!(
            "UpdateMicrogridDispatch(microgrid_id={}, dispatch_id={})",
            req.microgrid_id,
            req.dispatch_id
        );
        Ok(tonic::Response::new(pb::UpdateMicrogridDispatchResponse {
            dispatch: Some(dispatch),
        }))
    }

    async fn list_microgrid_dispatches(
        &self,
        request: tonic::Request<pb::ListMicrogridDispatchesRequest>,
    ) -> Result<tonic::Response<pb::ListMicrogridDispatchesResponse>, tonic::Status> {
        let req = request.into_inner();
        let mut items = self.store.list_mg(req.microgrid_id);
        if let Some(filter) = &req.filter {
            items.retain(|d| matches_filter(d, filter));
        }
        sort_dispatches(&mut items, req.sort_options.as_ref());

        let total = items.len();
        let (dispatches, next_page_token) = paginate(items, req.pagination_params.as_ref());
        log::info!(
            "ListMicrogridDispatches(microgrid_id={}) -> {} of {total} dispatch(es)",
            req.microgrid_id,
            dispatches.len(),
        );
        Ok(tonic::Response::new(pb::ListMicrogridDispatchesResponse {
            dispatches,
            pagination_info: Some(PaginationInfo {
                total_items: total as u32,
                next_page_token,
            }),
        }))
    }

    async fn stream_microgrid_dispatches(
        &self,
        request: tonic::Request<pb::StreamMicrogridDispatchesRequest>,
    ) -> Result<tonic::Response<Self::StreamMicrogridDispatchesStream>, tonic::Status> {
        let microgrid_id = request.into_inner().microgrid_id;
        let mut changes = self.store.subscribe();
        // Snapshot the current dispatches *before* spawning so the
        // replay can't miss a create that lands between subscribe and
        // the first recv (it would just be re-sent — events are
        // idempotent by id downstream).
        let snapshot = self.store.list_mg(microgrid_id);
        log::info!("StreamMicrogridDispatches(microgrid_id={microgrid_id}) subscribed");

        let (tx, rx) = tokio::sync::mpsc::channel(64);
        tokio::spawn(async move {
            // Replay existing dispatches as CREATED so a fresh
            // subscriber converges without a separate List call.
            for dispatch in snapshot {
                let msg = pb::StreamMicrogridDispatchesResponse {
                    dispatch: Some(dispatch),
                    event: Event::Created as i32,
                };
                if tx.send(Ok(msg)).await.is_err() {
                    return;
                }
            }
            loop {
                match changes.recv().await {
                    Ok(ev) if ev.microgrid_id == microgrid_id => {
                        let event = match ev.change {
                            DispatchChange::Created => Event::Created,
                            DispatchChange::Updated => Event::Updated,
                            DispatchChange::Deleted => Event::Deleted,
                        };
                        let msg = pb::StreamMicrogridDispatchesResponse {
                            dispatch: Some(ev.dispatch),
                            event: event as i32,
                        };
                        if tx.send(Ok(msg)).await.is_err() {
                            break;
                        }
                    }
                    // Event for another microgrid: ignore.
                    Ok(_) => continue,
                    // Fell behind the ring; the client re-syncs via List.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        log::warn!("dispatch stream (mg {microgrid_id}) lagged {n} events");
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        Ok(tonic::Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}

// --- helpers ---------------------------------------------------------------

fn not_found(microgrid_id: u64, dispatch_id: u64) -> tonic::Status {
    tonic::Status::not_found(format!(
        "dispatch {dispatch_id} not found for microgrid {microgrid_id}"
    ))
}

fn to_ts(dt: DateTime<Utc>) -> Timestamp {
    Timestamp {
        seconds: dt.timestamp(),
        nanos: dt.timestamp_subsec_nanos() as i32,
    }
}

fn ts_to_dt(ts: &Timestamp) -> Option<DateTime<Utc>> {
    Utc.timestamp_opt(ts.seconds, ts.nanos.max(0) as u32)
        .single()
}

/// A dispatch recurs if its rule carries a real (non-`UNSPECIFIED`)
/// frequency.
fn is_recurring(data: &pb::DispatchData) -> bool {
    data.recurrence
        .as_ref()
        .is_some_and(|r| r.freq != pb::recurrence_rule::Frequency::Unspecified as i32)
}

/// `end_time` is only calculable for a one-off (non-recurring) dispatch
/// with a finite duration. Indefinite (no duration) and recurring
/// dispatches have no single predetermined end, so it stays unset.
fn compute_end_time(
    start: Option<&Timestamp>,
    duration_s: Option<u32>,
    recurring: bool,
) -> Option<Timestamp> {
    if recurring {
        return None;
    }
    let start = ts_to_dt(start?)?;
    let end = start + chrono::Duration::seconds(duration_s? as i64);
    Some(to_ts(end))
}

fn recurrence_from_update(
    u: &pb::update_microgrid_dispatch_request::dispatch_update::RecurrenceRuleUpdate,
) -> pb::RecurrenceRule {
    pb::RecurrenceRule {
        freq: u.freq.unwrap_or_default(),
        interval: u.interval.unwrap_or_default(),
        end_criteria: u.end_criteria,
        byminutes: u.byminutes.clone(),
        byhours: u.byhours.clone(),
        byweekdays: u.byweekdays.clone(),
        bymonthdays: u.bymonthdays.clone(),
        bymonths: u.bymonths.clone(),
    }
}

fn in_interval(ts: Option<&Timestamp>, iv: &Interval) -> bool {
    let t = match ts.and_then(ts_to_dt) {
        Some(t) => t,
        // No timestamp to test against an interval filter => excluded.
        None => return false,
    };
    if let Some(start) = iv.start_time.as_ref().and_then(ts_to_dt)
        && t < start
    {
        return false;
    }
    if let Some(end) = iv.end_time.as_ref().and_then(ts_to_dt)
        && t >= end
    {
        return false;
    }
    true
}

/// Does the dispatch's target overlap the filter target? Matches on
/// component-id intersection or category intersection; a mixed
/// id-vs-category pair is treated as non-overlapping (we'd need
/// component metadata to resolve it, which this store doesn't carry).
fn targets_overlap(dispatch: Option<&pb::TargetComponents>, filter: &pb::TargetComponents) -> bool {
    use pb::target_components::Components;
    let (d, f) = match (
        dispatch.and_then(|t| t.components.as_ref()),
        filter.components.as_ref(),
    ) {
        (Some(d), Some(f)) => (d, f),
        _ => return false,
    };
    match (d, f) {
        (Components::ComponentIds(a), Components::ComponentIds(b)) => {
            a.ids.iter().any(|id| b.ids.contains(id))
        }
        (Components::ComponentCategoriesTypes(a), Components::ComponentCategoriesTypes(b)) => a
            .categories
            .iter()
            .any(|ca| b.categories.iter().any(|cb| ca.category == cb.category)),
        _ => false,
    }
}

fn matches_filter(dispatch: &pb::Dispatch, f: &pb::DispatchFilter) -> bool {
    let data = match &dispatch.data {
        Some(d) => d,
        None => return false,
    };
    let meta = dispatch.metadata.as_ref();
    let id = meta.map(|m| m.dispatch_id).unwrap_or(0);

    if let Some(active) = f.is_active
        && data.is_active != active
    {
        return false;
    }
    if let Some(dry) = f.is_dry_run
        && data.is_dry_run != dry
    {
        return false;
    }
    if !f.dispatch_ids.is_empty() && !f.dispatch_ids.contains(&id) {
        return false;
    }
    if let Some(rec) = &f.recurrence {
        use pb::dispatch_filter::Recurrence;
        match rec {
            Recurrence::IsRecurring(want) => {
                if is_recurring(data) != *want {
                    return false;
                }
            }
            // Advanced recurrence-field filtering is not implemented;
            // such a filter matches everything rather than nothing.
            Recurrence::Filter(_) => {}
        }
    }
    if let Some(iv) = &f.start_time_interval
        && !in_interval(data.start_time.as_ref(), iv)
    {
        return false;
    }
    if let Some(iv) = &f.end_time_interval
        && !in_interval(meta.and_then(|m| m.end_time.as_ref()), iv)
    {
        return false;
    }
    if let Some(iv) = &f.update_time_interval
        && !in_interval(meta.and_then(|m| m.update_time.as_ref()), iv)
    {
        return false;
    }
    if !f.targets.is_empty()
        && !f
            .targets
            .iter()
            .any(|t| targets_overlap(data.target.as_ref(), t))
    {
        return false;
    }
    // Free-text queries: `#N` matches dispatch id N exactly; any other
    // token is a case-insensitive substring match on `type`. Combined
    // with OR.
    if !f.queries.is_empty() {
        let ty = data.r#type.to_lowercase();
        let hit = f.queries.iter().any(|q| match q.strip_prefix('#') {
            Some(idstr) => idstr.parse::<u64>().map(|qid| qid == id).unwrap_or(false),
            None => ty.contains(&q.to_lowercase()),
        });
        if !hit {
            return false;
        }
    }
    true
}

/// Sort key for a dispatch under the requested field, as nanoseconds
/// since the epoch (missing timestamps sort first as `i128::MIN`).
fn sort_key(dispatch: &pb::Dispatch, field: pb::SortField) -> i128 {
    let ts = match field {
        pb::SortField::StartTime => dispatch.data.as_ref().and_then(|d| d.start_time.as_ref()),
        pb::SortField::LastUpdateTime => dispatch
            .metadata
            .as_ref()
            .and_then(|m| m.update_time.as_ref()),
        pb::SortField::CreateTime | pb::SortField::Unspecified => dispatch
            .metadata
            .as_ref()
            .and_then(|m| m.create_time.as_ref()),
    };
    ts.map(|t| t.seconds as i128 * 1_000_000_000 + t.nanos as i128)
        .unwrap_or(i128::MIN)
}

fn sort_dispatches(items: &mut [pb::Dispatch], opts: Option<&pb::SortOptions>) {
    let field = opts
        .map(|o| o.sort_field)
        .and_then(|f| pb::SortField::try_from(f).ok())
        .unwrap_or(pb::SortField::Unspecified);
    items.sort_by_key(|d| sort_key(d, field));
    // Default order is DESCENDING — the proto says an unspecified sort
    // returns newest-created first.
    let descending = !matches!(
        opts.map(|o| o.sort_order)
            .and_then(|o| pb::SortOrder::try_from(o).ok()),
        Some(pb::SortOrder::Ascending)
    );
    if descending {
        items.reverse();
    }
}

/// Offset pagination. The opaque `next_page_token` encodes
/// `"<offset>:<page_size>"` so a follow-up request that echoes it
/// continues from where the previous page ended. A `page_size` of 0
/// (the default) returns everything in one unpaged response.
fn paginate(
    items: Vec<pb::Dispatch>,
    params: Option<&PaginationParams>,
) -> (Vec<pb::Dispatch>, Option<String>) {
    let (offset, page_size) = match params.and_then(|p| p.params.as_ref()) {
        Some(pagination_params::Params::PageSize(n)) => (0usize, *n as usize),
        Some(pagination_params::Params::PageToken(token)) => parse_page_token(token),
        None => (0, 0),
    };
    let total = items.len();
    if page_size == 0 {
        return (items, None);
    }
    let start = offset.min(total);
    let end = (offset + page_size).min(total);
    let page = items[start..end].to_vec();
    let next = (end < total).then(|| format!("{end}:{page_size}"));
    (page, next)
}

fn parse_page_token(token: &str) -> (usize, usize) {
    let mut parts = token.splitn(2, ':');
    let offset = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let page_size = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (offset, page_size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::dispatch::new_store;
    use pb::microgrid_dispatch_service_server::MicrogridDispatchService;

    fn server() -> DispatchServer {
        DispatchServer::new(new_store())
    }

    fn data(type_: &str, active: bool) -> pb::DispatchData {
        pb::DispatchData {
            r#type: type_.to_string(),
            start_time: Some(to_ts(Utc::now())),
            is_active: active,
            ..Default::default()
        }
    }

    async fn create(srv: &DispatchServer, mg: u64, d: pb::DispatchData) -> pb::Dispatch {
        srv.create_microgrid_dispatch(tonic::Request::new(pb::CreateMicrogridDispatchRequest {
            microgrid_id: mg,
            dispatch_data: Some(d),
            start_immediately: None,
        }))
        .await
        .unwrap()
        .into_inner()
        .dispatch
        .unwrap()
    }

    async fn list(
        srv: &DispatchServer,
        mg: u64,
        filter: Option<pb::DispatchFilter>,
    ) -> Vec<pb::Dispatch> {
        srv.list_microgrid_dispatches(tonic::Request::new(pb::ListMicrogridDispatchesRequest {
            microgrid_id: mg,
            filter,
            sort_options: None,
            pagination_params: None,
        }))
        .await
        .unwrap()
        .into_inner()
        .dispatches
    }

    fn ids(ds: &[pb::Dispatch]) -> Vec<u64> {
        ds.iter()
            .map(|d| d.metadata.as_ref().unwrap().dispatch_id)
            .collect()
    }

    #[tokio::test]
    async fn create_assigns_id_and_stamps_times() {
        let srv = server();
        let d = create(&srv, 2200, data("SET_POWER", true)).await;
        let meta = d.metadata.unwrap();
        assert_eq!(meta.dispatch_id, 1);
        assert!(meta.create_time.is_some());
        assert!(meta.update_time.is_some());
        // Indefinite (no duration) => no calculable end_time.
        assert!(meta.end_time.is_none());
    }

    #[tokio::test]
    async fn create_computes_end_time_for_finite_oneoff() {
        let srv = server();
        let start = Utc::now();
        let mut d = data("X", true);
        d.start_time = Some(to_ts(start));
        d.duration = Some(3600);
        let res = create(&srv, 1, d).await;
        let end = res
            .metadata
            .unwrap()
            .end_time
            .expect("finite one-off has end");
        assert_eq!(end.seconds, start.timestamp() + 3600);
    }

    #[tokio::test]
    async fn create_no_end_time_for_recurring_even_with_duration() {
        let srv = server();
        let mut d = data("X", true);
        d.duration = Some(3600);
        d.recurrence = Some(pb::RecurrenceRule {
            freq: pb::recurrence_rule::Frequency::Daily as i32,
            interval: 1,
            ..Default::default()
        });
        let res = create(&srv, 1, d).await;
        assert!(res.metadata.unwrap().end_time.is_none());
    }

    #[tokio::test]
    async fn create_rejects_missing_data_and_start() {
        let srv = server();
        let err = srv
            .create_microgrid_dispatch(tonic::Request::new(pb::CreateMicrogridDispatchRequest {
                microgrid_id: 1,
                dispatch_data: None,
                start_immediately: None,
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);

        let mut d = data("X", true);
        d.start_time = None;
        let err = srv
            .create_microgrid_dispatch(tonic::Request::new(pb::CreateMicrogridDispatchRequest {
                microgrid_id: 1,
                dispatch_data: Some(d),
                start_immediately: None,
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn start_immediately_supplies_start_time() {
        let srv = server();
        let mut d = data("X", true);
        d.start_time = None; // would be rejected on its own
        let res = srv
            .create_microgrid_dispatch(tonic::Request::new(pb::CreateMicrogridDispatchRequest {
                microgrid_id: 1,
                dispatch_data: Some(d),
                start_immediately: Some(true),
            }))
            .await
            .unwrap()
            .into_inner()
            .dispatch
            .unwrap();
        assert!(res.data.unwrap().start_time.is_some());
    }

    #[tokio::test]
    async fn list_sorts_newest_first_and_filters_active() {
        let srv = server();
        create(&srv, 7, data("A", true)).await; // id 1
        create(&srv, 7, data("B", false)).await; // id 2
        create(&srv, 7, data("C", true)).await; // id 3
        // Default sort is create_time descending — newest (id 3) first.
        assert_eq!(ids(&list(&srv, 7, None).await), vec![3, 2, 1]);
        // is_active filter keeps only the two active ones.
        let active = list(
            &srv,
            7,
            Some(pb::DispatchFilter {
                is_active: Some(true),
                ..Default::default()
            }),
        )
        .await;
        assert_eq!(ids(&active), vec![3, 1]);
    }

    #[tokio::test]
    async fn list_filters_by_query_and_ids() {
        let srv = server();
        create(&srv, 1, data("peak_shave", true)).await; // id 1
        create(&srv, 1, data("set_power", true)).await; // id 2
        create(&srv, 1, data("peak_trim", true)).await; // id 3

        // Free-text token: substring match on type.
        let hits = list(
            &srv,
            1,
            Some(pb::DispatchFilter {
                queries: vec!["peak".into()],
                ..Default::default()
            }),
        )
        .await;
        assert_eq!(ids(&hits), vec![3, 1]);

        // `#N` token: exact id match.
        let hits = list(
            &srv,
            1,
            Some(pb::DispatchFilter {
                queries: vec!["#2".into()],
                ..Default::default()
            }),
        )
        .await;
        assert_eq!(ids(&hits), vec![2]);

        // dispatch_ids filter.
        let hits = list(
            &srv,
            1,
            Some(pb::DispatchFilter {
                dispatch_ids: vec![1, 3],
                ..Default::default()
            }),
        )
        .await;
        assert_eq!(ids(&hits), vec![3, 1]);
    }

    #[tokio::test]
    async fn list_paginates_with_opaque_token() {
        let srv = server();
        for _ in 0..5 {
            create(&srv, 1, data("X", true)).await;
        }
        // First page of 2.
        let resp = srv
            .list_microgrid_dispatches(tonic::Request::new(pb::ListMicrogridDispatchesRequest {
                microgrid_id: 1,
                filter: None,
                sort_options: None,
                pagination_params: Some(PaginationParams {
                    params: Some(pagination_params::Params::PageSize(2)),
                }),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.dispatches.len(), 2);
        let info = resp.pagination_info.unwrap();
        assert_eq!(info.total_items, 5);
        let token = info.next_page_token.expect("more pages remain");

        // Following the token continues from where page 1 ended.
        let resp2 = srv
            .list_microgrid_dispatches(tonic::Request::new(pb::ListMicrogridDispatchesRequest {
                microgrid_id: 1,
                filter: None,
                sort_options: None,
                pagination_params: Some(PaginationParams {
                    params: Some(pagination_params::Params::PageToken(token)),
                }),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp2.dispatches.len(), 2);
        // No overlap between the two pages.
        let page1_last = ids(&resp.dispatches)[1];
        assert!(!ids(&resp2.dispatches).contains(&page1_last));
    }

    #[tokio::test]
    async fn update_with_mask_touches_only_listed_fields() {
        let srv = server();
        let created = create(&srv, 1, data("OLD", true)).await;
        let id = created.metadata.unwrap().dispatch_id;
        let resp = srv
            .update_microgrid_dispatch(tonic::Request::new(pb::UpdateMicrogridDispatchRequest {
                microgrid_id: 1,
                dispatch_id: id,
                update_mask: Some(prost_types::FieldMask {
                    paths: vec!["is_active".into()],
                }),
                update: Some(pb::update_microgrid_dispatch_request::DispatchUpdate {
                    is_active: Some(false),
                    // type is not in the mask, so this would-be change is ignored
                    payload: None,
                    ..Default::default()
                }),
            }))
            .await
            .unwrap()
            .into_inner()
            .dispatch
            .unwrap();
        let data = resp.data.unwrap();
        assert!(!data.is_active);
        assert_eq!(data.r#type, "OLD");
    }

    #[tokio::test]
    async fn delete_removes_then_get_404s() {
        let srv = server();
        let id = create(&srv, 1, data("X", true))
            .await
            .metadata
            .unwrap()
            .dispatch_id;
        srv.delete_microgrid_dispatch(tonic::Request::new(pb::DeleteMicrogridDispatchRequest {
            microgrid_id: 1,
            dispatch_id: id,
        }))
        .await
        .unwrap();
        let err = srv
            .get_microgrid_dispatch(tonic::Request::new(pb::GetMicrogridDispatchRequest {
                microgrid_id: 1,
                dispatch_id: id,
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn dispatches_are_isolated_per_microgrid() {
        let srv = server();
        create(&srv, 100, data("A", true)).await;
        create(&srv, 200, data("B", true)).await;
        assert_eq!(ids(&list(&srv, 100, None).await), vec![1]);
        assert_eq!(ids(&list(&srv, 200, None).await), vec![2]);
        assert!(list(&srv, 300, None).await.is_empty());
    }
}
