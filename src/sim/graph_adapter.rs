//! Adapter between switchyard's component / connection types and the
//! [`frequenz_microgrid_component_graph`] crate's [`Node`] / [`Edge`]
//! traits.
//!
//! Implementing the traits directly on `Arc<dyn SimulatedComponent>`
//! would make `Node` reach across an Arc boundary on every call —
//! cheap, but every iterator the graph crate hands out then yields
//! an `Arc<dyn _>` which is awkward for callers that just want
//! `(id, category)` pairs. Instead, this module copies the minimal
//! identity fields into plain `GraphNode` / `GraphEdge` structs at
//! snapshot time. The snapshot is what gets handed to
//! [`ComponentGraph::try_new`]; we hold the resulting graph next to
//! the MicrogridSite so its `predecessors` / `successors` iterators and its
//! `*_formula` methods are reachable from the UI server without
//! re-snapshotting on every request.
//!
//! Category mapping notes:
//! - Switchyard's `Category::Inverter` carries the AC/DC/storage
//!   distinction via `subtype()` returning `"battery"` / `"solar"`.
//!   Map those to [`InverterType::Battery`] / [`InverterType::Pv`].
//! - Switchyard's `Category::Battery` doesn't carry chemistry yet —
//!   it falls through to [`BatteryType::Unspecified`].
//! - Switchyard's `Category::EvCharger` similarly falls through to
//!   [`EvChargerType::Unspecified`] unless `subtype()` returns
//!   `"ac"` / `"dc"` / `"hybrid"`.
//!
//! Hidden components (e.g. the consumer meter at id 100 in the
//! sample config) are *excluded* from the validation graph because
//! switchyard models them as aggregating off-graph — a parent
//! meter sums their power even though no explicit connection edge
//! ties them in. To the graph crate that looks like an orphan
//! node. `MicrogridSite::connections()` already filters hidden endpoints
//! out of the edge list; the node-list filter here matches.
//! Net effect: the validator sees the gRPC-visible topology, which
//! is what a downstream control app sees too.

use frequenz_microgrid_component_graph::{
    BatteryType, ComponentCategory, ComponentGraph, ComponentGraphConfig, Edge, EvChargerType,
    InverterType, Node,
};

use crate::sim::{component::Category, microgrid_site::MicrogridSite};

/// Plain-data view of one switchyard component, sized for the graph
/// crate's `Node` trait. Cloned cheaply (two scalars).
#[derive(Debug, Clone, Copy)]
pub struct GraphNode {
    pub id: u64,
    pub category: ComponentCategory,
}

impl Node for GraphNode {
    fn component_id(&self) -> u64 {
        self.id
    }

    fn category(&self) -> ComponentCategory {
        self.category
    }
}

/// Plain-data view of one switchyard connection. The graph crate
/// owns the storage; switchyard's `MicrogridSite` keeps its
/// `Vec<(u64, u64)>` parent/child shape for the rest of the codebase.
#[derive(Debug, Clone, Copy)]
pub struct GraphEdge {
    pub source: u64,
    pub destination: u64,
}

impl Edge for GraphEdge {
    fn source(&self) -> u64 {
        self.source
    }

    fn destination(&self) -> u64 {
        self.destination
    }
}

/// Lift a switchyard `(Category, subtype)` pair into the graph crate's
/// nested `ComponentCategory` enum. Unrecognised subtypes fall through
/// to the `Unspecified` variant of the nested enum — graph validation
/// is permissive for unspecified types, which matches the
/// "we haven't classified this yet, but it's a battery" case.
fn lift_category(category: Category, subtype: Option<&str>) -> ComponentCategory {
    match category {
        Category::Grid => ComponentCategory::GridConnectionPoint,
        Category::Meter => ComponentCategory::Meter,
        Category::Inverter => ComponentCategory::Inverter(match subtype {
            Some("battery") => InverterType::Battery,
            Some("solar") | Some("pv") => InverterType::Pv,
            Some("hybrid") => InverterType::Hybrid,
            _ => InverterType::Unspecified,
        }),
        Category::Battery => ComponentCategory::Battery(match subtype {
            Some("li-ion") | Some("liion") => BatteryType::LiIon,
            Some("na-ion") | Some("naion") => BatteryType::NaIon,
            _ => BatteryType::Unspecified,
        }),
        Category::EvCharger => ComponentCategory::EvCharger(match subtype {
            Some("ac") => EvChargerType::Ac,
            Some("dc") => EvChargerType::Dc,
            Some("hybrid") => EvChargerType::Hybrid,
            _ => EvChargerType::Unspecified,
        }),
        Category::Chp => ComponentCategory::Chp,
    }
}

/// Build the lists of nodes + edges the graph crate's
/// `ComponentGraph::try_new` consumes. Hidden components are
/// excluded so the validation graph mirrors what
/// `MicrogridSite::connections()` (the gRPC + UI surface) shows.
pub fn snapshot(site: &MicrogridSite) -> (Vec<GraphNode>, Vec<GraphEdge>) {
    let nodes = site
        .components()
        .iter()
        .filter(|c| !c.is_hidden())
        .map(|c| GraphNode {
            id: c.id(),
            category: lift_category(c.category(), c.subtype()),
        })
        .collect();
    let edges = site
        .connections()
        .into_iter()
        .map(|(source, destination)| GraphEdge {
            source,
            destination,
        })
        .collect();
    (nodes, edges)
}

/// Build a validated [`ComponentGraph`] from the live `MicrogridSite`.
/// Returns the graph crate's `Error` if any category-rule, root, or
/// connectivity check fails. The caller decides how to surface the
/// failure (log + keep running for a hot-reload; abort for boot).
pub fn build(
    site: &MicrogridSite,
) -> Result<ComponentGraph<GraphNode, GraphEdge>, frequenz_microgrid_component_graph::Error> {
    let (nodes, edges) = snapshot(site);
    build_from(nodes, edges)
}

/// Validation core, split out from [`build`] so tests can drive the
/// graph crate against hand-rolled node + edge lists without going
/// through `MicrogridSite`'s real component constructors.
pub fn build_from(
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
) -> Result<ComponentGraph<GraphNode, GraphEdge>, frequenz_microgrid_component_graph::Error> {
    ComponentGraph::try_new(nodes, edges, ComponentGraphConfig::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: u64, category: ComponentCategory) -> GraphNode {
        GraphNode { id, category }
    }

    fn edge(source: u64, destination: u64) -> GraphEdge {
        GraphEdge {
            source,
            destination,
        }
    }

    /// Pure-mapping coverage for each `Category` variant — exercises
    /// the subtype string discriminator for the two categories where
    /// it matters (Inverter, Battery, EvCharger).
    #[test]
    fn lift_category_maps_every_variant() {
        assert_eq!(
            lift_category(Category::Grid, None),
            ComponentCategory::GridConnectionPoint
        );
        assert_eq!(
            lift_category(Category::Meter, None),
            ComponentCategory::Meter
        );
        assert_eq!(
            lift_category(Category::Inverter, Some("battery")),
            ComponentCategory::Inverter(InverterType::Battery)
        );
        assert_eq!(
            lift_category(Category::Inverter, Some("solar")),
            ComponentCategory::Inverter(InverterType::Pv)
        );
        assert_eq!(
            lift_category(Category::Inverter, Some("pv")),
            ComponentCategory::Inverter(InverterType::Pv)
        );
        assert_eq!(
            lift_category(Category::Inverter, Some("hybrid")),
            ComponentCategory::Inverter(InverterType::Hybrid)
        );
        assert_eq!(
            lift_category(Category::Inverter, None),
            ComponentCategory::Inverter(InverterType::Unspecified)
        );
        assert_eq!(
            lift_category(Category::Battery, Some("li-ion")),
            ComponentCategory::Battery(BatteryType::LiIon)
        );
        assert_eq!(
            lift_category(Category::Battery, Some("liion")),
            ComponentCategory::Battery(BatteryType::LiIon)
        );
        assert_eq!(
            lift_category(Category::Battery, None),
            ComponentCategory::Battery(BatteryType::Unspecified)
        );
        assert_eq!(
            lift_category(Category::EvCharger, Some("ac")),
            ComponentCategory::EvCharger(EvChargerType::Ac)
        );
        assert_eq!(
            lift_category(Category::EvCharger, Some("dc")),
            ComponentCategory::EvCharger(EvChargerType::Dc)
        );
        assert_eq!(lift_category(Category::Chp, None), ComponentCategory::Chp);
    }

    /// A minimal valid topology: grid → meter → battery_inverter → battery.
    /// Verifies the graph crate accepts the resulting graph and that
    /// the battery formula references the battery-inverter's id.
    #[test]
    fn minimal_topology_validates_and_emits_formula() {
        let nodes = vec![
            node(1, ComponentCategory::GridConnectionPoint),
            node(2, ComponentCategory::Meter),
            node(3, ComponentCategory::Inverter(InverterType::Battery)),
            node(4, ComponentCategory::Battery(BatteryType::Unspecified)),
        ];
        let edges = vec![edge(1, 2), edge(2, 3), edge(3, 4)];
        let graph = build_from(nodes, edges).expect("valid topology should accept");
        let f = graph
            .battery_formula(None)
            .expect("battery formula should resolve");
        let s = format!("{f}");
        // The graph crate emits the battery-inverter id (#3) because
        // prefer_meters_in_component_formulas is on by default but
        // there's no inverter-fronting meter here.
        assert!(
            s.contains("#3"),
            "expected battery formula to reference #3, got {s}"
        );
    }

    /// Battery formula falls back to the fronting meter when one
    /// exists upstream of the battery-inverter — exercises the
    /// `prefer_meters_in_component_formulas` path.
    #[test]
    fn battery_formula_prefers_fronting_meter() {
        let nodes = vec![
            node(1, ComponentCategory::GridConnectionPoint),
            node(2, ComponentCategory::Meter), // main meter
            node(3, ComponentCategory::Meter), // battery-fronting meter
            node(4, ComponentCategory::Inverter(InverterType::Battery)),
            node(5, ComponentCategory::Battery(BatteryType::Unspecified)),
        ];
        let edges = vec![edge(1, 2), edge(2, 3), edge(3, 4), edge(4, 5)];
        let graph = build_from(nodes, edges).expect("valid topology should accept");
        let f = graph.battery_formula(None).expect("battery formula");
        let s = format!("{f}");
        assert!(
            s.contains("#3"),
            "expected formula to use the fronting meter (#3), got {s}"
        );
    }

    /// Missing root: no `GridConnectionPoint` → try_new errors out.
    #[test]
    fn missing_grid_fails_validation() {
        let nodes = vec![node(2, ComponentCategory::Meter)];
        let edges: Vec<GraphEdge> = vec![];
        let err = build_from(nodes, edges).err().expect("no grid should not validate");
        assert!(!format!("{err}").is_empty());
    }

    /// Disconnected branch: grid is alone, second cluster has no path
    /// to the root → try_new errors on connectivity.
    #[test]
    fn disconnected_branch_fails_validation() {
        let nodes = vec![
            node(1, ComponentCategory::GridConnectionPoint),
            node(2, ComponentCategory::Meter),
            node(3, ComponentCategory::Meter),
            node(4, ComponentCategory::Inverter(InverterType::Battery)),
            node(5, ComponentCategory::Battery(BatteryType::Unspecified)),
        ];
        // 1 → 2 (one branch), 3 → 4 → 5 (orphan branch — no link to grid).
        let edges = vec![edge(1, 2), edge(3, 4), edge(4, 5)];
        let err = build_from(nodes, edges).err().expect("disconnected branch");
        assert!(!format!("{err}").is_empty());
    }
}
