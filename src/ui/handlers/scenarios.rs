//! Scenarios HTTP surface: list the registry, start (the live runner),
//! plus the per-scenario summary / events / report readers. The old
//! stage-mutation endpoints (next/prev/jump) are gone with the
//! day-stage model.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::lisp::Config;
use crate::sim::microgrid_site::{ScenarioReport, ScenarioSummary};

pub(in crate::ui) async fn scenarios_list(
    State(config): State<Config>,
) -> Json<Vec<crate::sim::scenarios::ScenarioView>> {
    Json(crate::sim::scenarios::snapshot(&config.scenarios()))
}

/// Live runner: compile + start scenario NAME on the wall clock. Its
/// cue / check timers fire on the refresh loop as real time passes.
/// Runs on a blocking thread since `start` holds the interpreter lock.
pub(in crate::ui) async fn scenarios_start(
    State(config): State<Config>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let res = tokio::task::spawn_blocking(move || {
        crate::sim::scenarios::start(&config.interpreter(), &config.scenarios(), &name)
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("join: {e}")))?;
    res.map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// Snapshot of the running scenario's lifecycle. Empty (`name:
/// null`, zero counts) before any `(scenario-start)`; freezes
/// `elapsed_s` once `(scenario-stop)` fires.
pub(in crate::ui) async fn scenario_summary(State(config): State<Config>) -> Json<ScenarioSummary> {
    Json(config.site().scenario_summary(Utc::now()))
}

#[derive(Deserialize)]
pub(in crate::ui) struct ScenarioEventsQuery {
    /// Return events with id strictly greater than this. Default 0
    /// means "everything in the ring".
    since: Option<u64>,
    /// Cap on returned entries. Default 200.
    limit: Option<usize>,
}

#[derive(Serialize)]
pub(in crate::ui) struct ScenarioEventsResponse {
    events: Vec<crate::sim::scenario::ScenarioEvent>,
    /// `next_event_id` lets a polling client advance its `since=`
    /// cursor even when this batch was empty (because no events
    /// have arrived since last poll, but new ones might before the
    /// next).
    next_event_id: u64,
    /// Lowest event id still in the ring. Clients comparing
    /// `earliest_event_id > since` know their cursor was inside
    /// the evicted window and they missed `earliest_event_id - since`
    /// entries.
    earliest_event_id: u64,
}

pub(in crate::ui) async fn scenario_events(
    State(config): State<Config>,
    Query(q): Query<ScenarioEventsQuery>,
) -> Json<ScenarioEventsResponse> {
    let since = q.since.unwrap_or(0);
    let limit = q.limit.unwrap_or(200).min(1000);
    let events = config.site().scenario_events_since(since, limit);
    let summary = config.site().scenario_summary(Utc::now());
    Json(ScenarioEventsResponse {
        events,
        next_event_id: summary.next_event_id,
        earliest_event_id: summary.earliest_event_id,
    })
}

/// Aggregate metrics for the running scenario (peak main-meter
/// power so far, plus future B3/B4 fields). Independent of
/// `/api/scenario/events` so a dashboard can poll metrics
/// frequently without scanning the whole event log.
pub(in crate::ui) async fn scenario_report(State(config): State<Config>) -> Json<ScenarioReport> {
    Json(config.site().scenario_report(Utc::now()))
}
