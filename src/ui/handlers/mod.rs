//! HTTP handler glue. Each topic lives in its own submodule.
//! `register` in `super::router` wires every route through one of
//! the `pub(in crate::ui)` fns exported here.

use axum::http::StatusCode;

use crate::lisp::Config;

use super::state::{MicrogridLoopbacks, SharedMicrogrid};

pub(in crate::ui) mod assets;
pub(in crate::ui) mod defaults;
pub(in crate::ui) mod dispatches;
pub(in crate::ui) mod eval;
pub(in crate::ui) mod history;
pub(in crate::ui) mod microgrid_data;
pub(in crate::ui) mod microgrids;
pub(in crate::ui) mod overrides;
pub(in crate::ui) mod scenarios;
pub(in crate::ui) mod snapshots;
pub(in crate::ui) mod topology;

/// Look up the site for `mg_id` in the registry. Per-microgrid
/// handlers call this at the start; a miss returns 404 verbatim
/// so the SPA can highlight a stale microgrid card and reload
/// without retrying every per-mg fetch on the page.
pub(in crate::ui) fn resolve_site(
    config: &Config,
    mg_id: u64,
) -> Result<crate::sim::MicrogridSite, (StatusCode, String)> {
    config
        .microgrids()
        .lock()
        .get(&mg_id)
        .map(|e| e.site.clone())
        .ok_or((
            StatusCode::NOT_FOUND,
            format!("microgrid {mg_id} not registered"),
        ))
}

/// Mirror of [`resolve_site`] for the loopback Microgrid client
/// slot a microgrid owns. Used by the per-mg
/// `/microgrid/{status,latest,formulas}` endpoints.
pub(in crate::ui) fn resolve_loopback(
    loopbacks: &MicrogridLoopbacks,
    mg_id: u64,
) -> Result<SharedMicrogrid, (StatusCode, String)> {
    loopbacks.read().get(&mg_id).cloned().ok_or((
        StatusCode::NOT_FOUND,
        format!("microgrid {mg_id} not registered"),
    ))
}
