//! `/api/history` + setpoint event log readers.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};

use crate::lisp::Config;
use crate::sim::history::Metric;
use crate::sim::setpoints::SetpointEvent;

use super::resolve_site;

#[derive(Deserialize)]
pub(in crate::ui) struct HistoryQuery {
    /// Component id to fetch history for. Required.
    id: u64,
    /// Metric name (one of `History::Metric::as_str` strings).
    /// Required.
    metric: String,
    /// Window length in seconds. Optional; defaults to the full
    /// 10-minute capacity of the ring buffer.
    window_s: Option<i64>,
}

#[derive(Serialize)]
pub(in crate::ui) struct HistoryResponse {
    id: u64,
    metric: String,
    /// Typed quantity (`"Power"`, `"ReactivePower"`, `"Frequency"`,
    /// `"Percentage"`) — mirrors the frequenz-microgrid `Sample<Q>`
    /// `Q` parameter so the SPA picks a scale family from this
    /// instead of pattern-matching on the metric name.
    quantity: &'static str,
    /// Base unit the samples are recorded in (`"W"`, `"var"`,
    /// `"Hz"`, `"%"`).
    unit: &'static str,
    /// Pairs of (timestamp_ms_since_epoch, value). The time format is
    /// JS-ready (Date.now() shape) so chart libs can plot directly.
    samples: Vec<(i64, f32)>,
}

pub(in crate::ui) async fn history(
    State(config): State<Config>,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<HistoryResponse>, (StatusCode, String)> {
    history_body(&config.site(), q)
}

pub(in crate::ui) async fn history_for_mg(
    State(config): State<Config>,
    Path(mg_id): Path<u64>,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<HistoryResponse>, (StatusCode, String)> {
    let site = resolve_site(&config, mg_id)?;
    history_body(&site, q)
}

fn history_body(
    site: &crate::sim::MicrogridSite,
    q: HistoryQuery,
) -> Result<Json<HistoryResponse>, (StatusCode, String)> {
    let metric: Metric = q.metric.parse().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            format!("unknown metric '{}'", q.metric),
        )
    })?;
    let window = ChronoDuration::seconds(q.window_s.unwrap_or(600));
    let since: DateTime<Utc> = Utc::now() - window;
    let samples = site
        .history_window(q.id, metric, since)
        .unwrap_or_default()
        .into_iter()
        .map(|s| (s.ts.timestamp_millis(), s.value))
        .collect();
    Ok(Json(HistoryResponse {
        id: q.id,
        metric: q.metric,
        quantity: metric.quantity(),
        unit: metric.unit(),
        samples,
    }))
}

#[derive(Deserialize)]
pub(in crate::ui) struct SetpointsQuery {
    id: u64,
    /// Window length in seconds. Optional; defaults to the full
    /// 1000-event capacity of the ring (which at typical control-app
    /// rates covers several minutes).
    window_s: Option<i64>,
}

#[derive(Serialize)]
pub(in crate::ui) struct SetpointsResponse {
    id: u64,
    events: Vec<SetpointEvent>,
}

pub(in crate::ui) async fn setpoints(
    State(config): State<Config>,
    Query(q): Query<SetpointsQuery>,
) -> Json<SetpointsResponse> {
    let window = ChronoDuration::seconds(q.window_s.unwrap_or(600));
    let since = Utc::now() - window;
    let events = config.site().setpoints_window(q.id, since);
    Json(SetpointsResponse { id: q.id, events })
}
