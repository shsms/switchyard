//! Embedded web UI server.
//!
//! Runs alongside the gRPC server on the same tokio runtime, separate
//! port (default 8801). The SPA shell + vendored assets are bundled
//! via rust-embed.
//!
//! The fanout is:
//!
//! - `state` — types shared between the loopback supervisor, the WS
//!   pump, and the HTTP handlers.
//! - `loopback` — gRPC loopback supervisor.
//! - `events_ws` — WebSocket push channel.
//! - `handlers` — one submodule per HTTP topic (topology, eval,
//!   history, scenarios, microgrids, …).

mod events_ws;
mod handlers;
mod loopback;
mod state;

pub use loopback::spawn_microgrid_loopback;
pub use state::{
    HistorySample, MicrogridLoopbacks, MicrogridSampleSnapshot, MicrogridSpawner, MicrogridState,
    SharedMicrogrid, new_microgrid_loopbacks, new_microgrid_slot,
};

use std::net::SocketAddr;

use axum::{
    Extension, Router,
    routing::{delete, get, post},
};

use crate::lisp::Config;
use events_ws::events_ws;

/// Spawn the UI HTTP server on `addr`. Returns once the listener is
/// bound and accepting connections; the server itself runs to
/// completion of the returned future.
///
/// `microgrid` is the loopback client slot — the binary populates it
/// via [`spawn_microgrid_loopback`] before / alongside the gRPC
/// server starting. Pass an empty slot if the UI doesn't need
/// aggregated Dashboard data (tests, etc.).
pub async fn serve(
    addr: SocketAddr,
    config: Config,
    microgrid: SharedMicrogrid,
    loopbacks: MicrogridLoopbacks,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    log::info!("Switchyard UI listening on http://{}", addr);
    serve_with_listener(listener, config, microgrid, loopbacks).await
}

pub async fn serve_with_listener(
    listener: tokio::net::TcpListener,
    config: Config,
    microgrid: SharedMicrogrid,
    loopbacks: MicrogridLoopbacks,
) -> std::io::Result<()> {
    axum::serve(listener, router(config, microgrid, loopbacks))
        .await
        .map_err(std::io::Error::other)
}

fn router(config: Config, microgrid: SharedMicrogrid, loopbacks: MicrogridLoopbacks) -> Router {
    use handlers::{
        assets::{asset, index, logs_backfill},
        defaults::defaults,
        dispatches::{
            dispatch_create_for_mg, dispatch_delete_for_mg, dispatch_set_active_for_mg,
            dispatches_for_mg,
        },
        eval::{eval, eval_for_mg, format},
        history::{history, history_for_mg, setpoints},
        microgrid_data::{
            clock_info, microgrid_formulas, microgrid_formulas_for_mg, microgrid_history,
            microgrid_history_for_mg, microgrid_latest, microgrid_latest_for_mg, microgrid_status,
            microgrid_status_for_mg,
        },
        microgrids::{microgrids_create, microgrids_list},
        overrides::{
            overrides_list, overrides_text_for_mg, overrides_text_replace_for_mg,
            persisted_bulk_remove, persisted_remove,
        },
        scenarios::{
            scenario_events, scenario_report, scenario_summary, scenarios_list, scenarios_start,
        },
        snapshots::{snapshots_list, snapshots_load, snapshots_save},
        topology::{topology, topology_for_mg},
    };
    Router::new()
        .route("/", get(index))
        .route("/assets/{*path}", get(asset))
        .route("/api/topology", get(topology))
        .route("/api/eval", post(eval))
        .route("/api/format", post(format))
        .route("/api/history", get(history))
        .route("/api/defaults", get(defaults))
        .route("/api/setpoints", get(setpoints))
        .route("/api/overrides", get(overrides_list))
        .route(
            "/api/persisted/{idx}",
            axum::routing::delete(persisted_remove),
        )
        .route("/api/persisted/delete", post(persisted_bulk_remove))
        .route("/api/logs", get(logs_backfill))
        .route("/api/scenario", get(scenario_summary))
        .route("/api/scenario/events", get(scenario_events))
        .route("/api/scenario/report", get(scenario_report))
        .route("/api/clock", get(clock_info))
        .route("/api/microgrid/status", get(microgrid_status))
        .route("/api/microgrid/latest", get(microgrid_latest))
        .route("/api/microgrid/history", get(microgrid_history))
        .route("/api/microgrid/formulas", get(microgrid_formulas))
        .route("/api/snapshots", get(snapshots_list))
        .route("/api/snapshots/save", post(snapshots_save))
        .route("/api/snapshots/load", post(snapshots_load))
        .route("/api/scenarios", get(scenarios_list))
        .route("/api/scenarios/{name}/start", post(scenarios_start))
        .route("/api/microgrids", get(microgrids_list))
        .route("/api/microgrids/create", post(microgrids_create))
        .route("/api/mg/{mg_id}/topology", get(topology_for_mg))
        .route("/api/mg/{mg_id}/eval", post(eval_for_mg))
        .route("/api/mg/{mg_id}/history", get(history_for_mg))
        .route(
            "/api/mg/{mg_id}/microgrid/status",
            get(microgrid_status_for_mg),
        )
        .route(
            "/api/mg/{mg_id}/microgrid/latest",
            get(microgrid_latest_for_mg),
        )
        .route(
            "/api/mg/{mg_id}/microgrid/history",
            get(microgrid_history_for_mg),
        )
        .route(
            "/api/mg/{mg_id}/microgrid/formulas",
            get(microgrid_formulas_for_mg),
        )
        .route(
            "/api/mg/{mg_id}/overrides/text",
            get(overrides_text_for_mg).post(overrides_text_replace_for_mg),
        )
        .route(
            "/api/mg/{mg_id}/dispatches",
            get(dispatches_for_mg).post(dispatch_create_for_mg),
        )
        .route(
            "/api/mg/{mg_id}/dispatches/{dispatch_id}",
            delete(dispatch_delete_for_mg),
        )
        .route(
            "/api/mg/{mg_id}/dispatches/{dispatch_id}/active",
            post(dispatch_set_active_for_mg),
        )
        .route("/ws/events", get(events_ws))
        .layer(Extension(microgrid))
        .layer(Extension(loopbacks))
        .with_state(config)
}

#[cfg(test)]
mod tests;
