//! Topology snapshot endpoints. `/api/topology` returns the
//! bootstrap site (legacy), `/api/mg/{id}/topology` the per-
//! microgrid view.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Serialize;

use crate::lisp::Config;
use crate::sim::Category;

use super::resolve_site;

#[derive(Serialize)]
pub(in crate::ui) struct TopologySnapshot {
    components: Vec<ComponentSummary>,
    /// Visible parent → child edges, matching the gRPC
    /// `ListConnections` semantic.
    connections: Vec<(u64, u64)>,
    /// Parent → child edges where the child is hidden. Surfaced
    /// separately so the UI can render them dashed without
    /// polluting the gRPC graph. Aggregator components
    /// (Meter, BatteryInverter) cache their hidden children for
    /// aggregation; we read those here.
    hidden_connections: Vec<(u64, u64)>,
    /// Latest graph-validator outcome. `None` = the graph crate
    /// accepted the topology; `Some(msg)` = it rejected with the
    /// human-readable error string. The pulse-bar graph pill
    /// flips between ✓ and ⚠ on this field.
    graph_status: Option<String>,
    /// Id of the meter flagged `:main t` in the topology, if any.
    /// The SPA's Grid-frequency tile pulls history from this id.
    main_meter_id: Option<u64>,
}

#[derive(Serialize)]
struct ComponentSummary {
    id: u64,
    name: String,
    /// Lowercase string form of [`Category`] (e.g. "grid", "battery").
    /// Stable wire shape — the UI keys icon / colour selection off it.
    category: &'static str,
    /// Subtype label like "battery" / "pv" for inverters; `None` for
    /// component categories that don't subdivide further.
    subtype: Option<&'static str>,
    hidden: bool,
    /// Current runtime knob settings — Display impls on the enums map
    /// to the same lowercase tokens the corresponding setter defuns
    /// accept, so the UI's dropdowns can round-trip via /api/eval
    /// without a string table.
    health: String,
    telemetry_mode: String,
    command_mode: String,
}

pub(in crate::ui) async fn topology(State(config): State<Config>) -> Json<TopologySnapshot> {
    Json(topology_snapshot(&config, &config.site()))
}

pub(in crate::ui) async fn topology_for_mg(
    State(config): State<Config>,
    Path(mg_id): Path<u64>,
) -> Result<Json<TopologySnapshot>, (StatusCode, String)> {
    let site = resolve_site(&config, mg_id)?;
    Ok(Json(topology_snapshot(&config, &site)))
}

fn topology_snapshot(config: &Config, site: &crate::sim::MicrogridSite) -> TopologySnapshot {
    let components = site
        .components()
        .iter()
        .map(|c| {
            let runtime = site.runtime_of(c.id());
            ComponentSummary {
                id: c.id(),
                name: site
                    .display_name(c.id())
                    .unwrap_or_else(|| c.name().to_string()),
                category: category_label(c.category()),
                subtype: c.subtype(),
                hidden: c.is_hidden(),
                health: runtime.health.to_string(),
                telemetry_mode: runtime.telemetry.to_string(),
                command_mode: runtime.command.to_string(),
            }
        })
        .collect();
    TopologySnapshot {
        components,
        connections: site.connections(),
        hidden_connections: site.hidden_connections(),
        graph_status: config.graph_status(),
        main_meter_id: site.main_meter_id(),
    }
}

fn category_label(c: Category) -> &'static str {
    match c {
        Category::Grid => "grid",
        Category::Meter => "meter",
        Category::Inverter => "inverter",
        Category::Battery => "battery",
        Category::EvCharger => "ev-charger",
        Category::Chp => "chp",
    }
}
