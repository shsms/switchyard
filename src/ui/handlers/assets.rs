//! Embedded SPA assets + `/api/logs` log-tap backfill.

use super::super::state::Assets;
use axum::{
    Json,
    body::Body,
    extract::Path,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};

pub(in crate::ui) async fn index() -> Response {
    serve_embedded("index.html")
}

pub(in crate::ui) async fn asset(Path(path): Path<String>) -> Response {
    serve_embedded(&path)
}

fn serve_embedded(path: &str) -> Response {
    match Assets::get(path) {
        Some(content) => {
            let mime = mime_for(path);
            (
                [(header::CONTENT_TYPE, HeaderValue::from_static(mime))],
                Body::from(content.data.into_owned()),
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, format!("asset not found: {path}")).into_response(),
    }
}

fn mime_for(path: &str) -> &'static str {
    let ext = path.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    match ext {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}

/// Backfill recent log lines from the LogTap ring buffer. Returns
/// an empty list when the binary didn't initialise a tap (test path).
pub(in crate::ui) async fn logs_backfill() -> Json<Vec<crate::ui_log::LogEvent>> {
    Json(
        crate::ui_log::LOG_TAP
            .get()
            .map(|t| t.snapshot())
            .unwrap_or_default(),
    )
}
