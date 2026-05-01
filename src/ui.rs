//! Embedded web UI server.
//!
//! Runs alongside the gRPC server on the same tokio runtime, separate
//! port (default 8801, see UI.org). Phase 1 surface is intentionally
//! tiny — endpoints land one commit at a time. The SPA shell + assets
//! arrive later via rust-embed.

use std::net::SocketAddr;

use axum::{Json, Router, extract::State, routing::get};
use serde::Serialize;

use crate::sim::{Category, World};

/// Spawn the UI HTTP server on `addr`. Returns once the listener is
/// bound and accepting connections; the server itself runs to
/// completion of the returned future.
///
/// Localhost-only by default (the caller decides the bind address);
/// non-loopback is opt-in via the `--ui-bind` CLI flag.
pub async fn serve(addr: SocketAddr, world: World) -> Result<(), std::io::Error> {
    let app = router(world);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    log::info!("Switchyard UI listening on http://{addr}");
    axum::serve(listener, app).await
}

fn router(world: World) -> Router {
    Router::new()
        .route("/", get(placeholder_index))
        .route("/api/topology", get(topology))
        .with_state(world)
}

/// Phase-1 placeholder. Replaced by the embedded SPA shell when the
/// rust-embed assets land.
async fn placeholder_index() -> &'static str {
    "switchyard UI — phase 1 scaffold. SPA assets land in a later commit."
}

#[derive(Serialize)]
struct TopologySnapshot {
    components: Vec<ComponentSummary>,
    /// Parent → child edges. Hidden children are still listed in
    /// `components` (so the UI knows they exist) but their edges are
    /// excluded, matching the gRPC `ListConnections` semantic.
    connections: Vec<(u64, u64)>,
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
}

async fn topology(State(world): State<World>) -> Json<TopologySnapshot> {
    let components = world
        .components()
        .iter()
        .map(|c| ComponentSummary {
            id: c.id(),
            name: c.name().to_string(),
            category: category_label(c.category()),
            subtype: c.subtype(),
            hidden: c.is_hidden(),
        })
        .collect();
    Json(TopologySnapshot {
        components,
        connections: world.connections(),
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// One-line helper that calls a route and returns the response,
    /// using axum's `oneshot` so we don't need to bind a real port.
    async fn get_route(world: World, path: &str) -> (StatusCode, Vec<u8>) {
        let resp = router(world)
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, bytes.to_vec())
    }

    #[tokio::test]
    async fn placeholder_route_responds() {
        let (status, body) = get_route(World::new(), "/").await;
        assert_eq!(status, StatusCode::OK);
        assert!(String::from_utf8_lossy(&body).contains("switchyard UI"));
    }

    #[tokio::test]
    async fn topology_endpoint_emits_components_and_connections() {
        // Build a tiny world: grid → meter → battery, all wired.
        let world = World::new();
        let mut ctx = tulisp::TulispContext::new();
        crate::lisp::handle::register(&mut ctx);
        crate::lisp::make::register(&mut ctx, world.clone());
        ctx.eval_string(
            r#"(%make-grid :id 1
                 :successors
                 (list (%make-meter :id 2
                         :successors
                         (list (%make-battery :id 3)))))"#,
        )
        .expect("eval");

        let (status, body) = get_route(world, "/api/topology").await;
        assert_eq!(status, StatusCode::OK);
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();

        let components = parsed["components"].as_array().unwrap();
        assert_eq!(components.len(), 3);
        let categories: Vec<_> = components
            .iter()
            .map(|c| c["category"].as_str().unwrap())
            .collect();
        assert!(categories.contains(&"grid"));
        assert!(categories.contains(&"meter"));
        assert!(categories.contains(&"battery"));

        let connections = parsed["connections"].as_array().unwrap();
        assert_eq!(connections.len(), 2);
    }
}
