//! Snapshot save / load endpoints. Calls `Config::save_snapshot`
//! and `Config::load_snapshot`; both wrap blocking IO.

use axum::{Json, extract::State, http::StatusCode};
use serde::{Deserialize, Serialize};

use crate::lisp::Config;

#[derive(Serialize)]
pub(in crate::ui) struct SnapshotsListResp {
    snapshots: Vec<String>,
}

pub(in crate::ui) async fn snapshots_list(State(config): State<Config>) -> Json<SnapshotsListResp> {
    Json(SnapshotsListResp {
        snapshots: config.list_snapshots(),
    })
}

#[derive(Deserialize)]
pub(in crate::ui) struct SnapshotsBody {
    name: String,
}

pub(in crate::ui) async fn snapshots_save(
    State(config): State<Config>,
    Json(body): Json<SnapshotsBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let path = tokio::task::spawn_blocking(move || config.save_snapshot(&body.name))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("join: {e}")))?
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "path": path.display().to_string(),
    })))
}

pub(in crate::ui) async fn snapshots_load(
    State(config): State<Config>,
    Json(body): Json<SnapshotsBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    tokio::task::spawn_blocking(move || config.load_snapshot(&body.name))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("join: {e}")))?
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}
