//! Overrides endpoints: list, single-form delete, bulk delete,
//! and the full-text read/write the canvas undo stack uses.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

use crate::lisp::Config;

#[derive(Serialize)]
pub(in crate::ui) struct PersistedOverrideView {
    idx: usize,
    source: String,
}

#[derive(Serialize)]
pub(in crate::ui) struct OverridesResponse {
    /// Top-level forms in the on-disk override file, one per
    /// entry. Each `idx` is stable until the next bulk-delete
    /// rewrites the file.
    persisted: Vec<PersistedOverrideView>,
    /// Convenience for the chrome's "N overrides" pill — equals
    /// `persisted.len()`.
    count: usize,
}

pub(in crate::ui) async fn overrides_list(State(config): State<Config>) -> Json<OverridesResponse> {
    // Format each form via tulisp-fmt so the dialog shows tidy
    // Lisp (multi-line for nested forms) instead of one-liner
    // source. .trim_end() drops the formatter's file-style
    // trailing newline so adjacent <pre>s don't accumulate blank
    // lines.
    let persisted: Vec<PersistedOverrideView> = config
        .persisted_overrides()
        .into_iter()
        .map(|o| PersistedOverrideView {
            idx: o.idx,
            source: tulisp_fmt::format_with_width(&o.source, 60)
                .map(|f| f.trim_end().to_string())
                .unwrap_or(o.source),
        })
        .collect();
    let count = persisted.len();
    Json(OverridesResponse { persisted, count })
}

/// Drop a single persisted-override entry by its file-position
/// idx. Rewrites the override file without that form and reloads
/// — see `Config::remove_persisted_overrides`. The bulk-delete
/// endpoint below is the more common path; this one stays for
/// parity / single-shot scripted use.
pub(in crate::ui) async fn persisted_remove(
    State(config): State<Config>,
    Path(idx): Path<usize>,
) -> Result<StatusCode, (StatusCode, String)> {
    let result = tokio::task::spawn_blocking(move || config.remove_persisted_overrides(&[idx]))
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("task panicked: {e}"),
            )
        })?;
    match result {
        Ok(0) => Err((
            StatusCode::NOT_FOUND,
            format!("no persisted override at idx {idx}"),
        )),
        Ok(_) => Ok(StatusCode::NO_CONTENT),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("write failed: {e}"),
        )),
    }
}

#[derive(Deserialize)]
pub(in crate::ui) struct BulkRemoveBody {
    indices: Vec<usize>,
}

#[derive(Serialize)]
pub(in crate::ui) struct BulkRemoveResponse {
    removed: usize,
}

/// Drop several persisted-override entries in one shot. Rewrites
/// the override file once and reloads once — the chrome's
/// checkbox-toolbar Delete button hits this, so a 5-item delete
/// is one round trip and one re-render rather than five.
pub(in crate::ui) async fn persisted_bulk_remove(
    State(config): State<Config>,
    Json(body): Json<BulkRemoveBody>,
) -> Result<Json<BulkRemoveResponse>, (StatusCode, String)> {
    let result =
        tokio::task::spawn_blocking(move || config.remove_persisted_overrides(&body.indices))
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("task panicked: {e}"),
                )
            })?;
    match result {
        Ok(removed) => Ok(Json(BulkRemoveResponse { removed })),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("write failed: {e}"),
        )),
    }
}

/// Return the raw text of a microgrid's overrides file, or empty
/// string if it doesn't exist yet. The undo stack on the canvas
/// snapshots this before each mutating eval so Ctrl-Z can restore
/// the prior shape verbatim.
pub(in crate::ui) async fn overrides_text_for_mg(
    State(config): State<Config>,
    Path(mg_id): Path<u64>,
) -> Result<String, (StatusCode, String)> {
    if !config.microgrids().lock().contains_key(&mg_id) {
        return Err((
            StatusCode::NOT_FOUND,
            format!("microgrid {mg_id} not registered"),
        ));
    }
    tokio::task::spawn_blocking(move || config.scoped(mg_id, |cfg, _ctx| cfg.overrides_text()))
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("task panicked: {e}"),
            )
        })?
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("read failed: {e}"),
            )
        })
}

/// Overwrite a microgrid's overrides file with the body and reload.
/// Used by the canvas undo stack to restore a prior snapshot — the
/// body is the full file contents, not a delta.
pub(in crate::ui) async fn overrides_text_replace_for_mg(
    State(config): State<Config>,
    Path(mg_id): Path<u64>,
    body: String,
) -> Result<StatusCode, (StatusCode, String)> {
    if !config.microgrids().lock().contains_key(&mg_id) {
        return Err((
            StatusCode::NOT_FOUND,
            format!("microgrid {mg_id} not registered"),
        ));
    }
    tokio::task::spawn_blocking(move || {
        config.scoped(mg_id, |cfg, ctx| {
            cfg.replace_overrides_text_locked(ctx, &body)
        })
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task panicked: {e}"),
        )
    })?
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("write failed: {e}"),
        )
    })?;
    Ok(StatusCode::NO_CONTENT)
}
