//! Embedded web UI server.
//!
//! Runs alongside the gRPC server on the same tokio runtime, separate
//! port (default 8801, see UI.org). Phase 1 surface is intentionally
//! tiny — endpoints land one commit at a time. The SPA shell + assets
//! arrive later via rust-embed.

use std::net::SocketAddr;

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use serde::Serialize;

use crate::{lisp::Config, sim::Category};

/// Spawn the UI HTTP server on `addr`. Returns once the listener is
/// bound and accepting connections; the server itself runs to
/// completion of the returned future.
///
/// Localhost-only by default (the caller decides the bind address);
/// non-loopback is opt-in via the `--ui-bind` CLI flag.
pub async fn serve(addr: SocketAddr, config: Config) -> Result<(), std::io::Error> {
    let app = router(config);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    log::info!("Switchyard UI listening on http://{addr}");
    axum::serve(listener, app).await
}

fn router(config: Config) -> Router {
    Router::new()
        .route("/", get(placeholder_index))
        .route("/api/topology", get(topology))
        .route("/api/eval", post(eval))
        .with_state(config)
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

async fn topology(State(config): State<Config>) -> Json<TopologySnapshot> {
    let world = config.world();
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

#[derive(Serialize)]
struct EvalResponse {
    /// Whether the expression evaluated without an error. False ==
    /// `error` populated, `value` null. True == `value` holds the
    /// Display formatted result.
    ok: bool,
    value: Option<String>,
    error: Option<String>,
}

/// Evaluate a Lisp expression on the running interpreter. Wrapped in
/// `spawn_blocking` because tulisp's `SharedMut` is std-sync-RwLock-
/// backed and grabbing the write lock from the executor thread would
/// stall every other tokio task waiting on that worker.
///
/// Always returns 200 — application-layer success/failure rides in
/// the JSON body. Reserves HTTP 4xx/5xx for transport-level problems
/// (bad UTF-8, the spawn_blocking task panicking, etc.).
async fn eval(State(config): State<Config>, body: String) -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(move || config.eval(&body))
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("eval task panicked: {e}"),
            )
        });
    match result {
        Ok(Ok(value)) => (
            StatusCode::OK,
            Json(EvalResponse {
                ok: true,
                value: Some(value),
                error: None,
            }),
        ),
        Ok(Err(error)) => (
            StatusCode::OK,
            Json(EvalResponse {
                ok: false,
                value: None,
                error: Some(error),
            }),
        ),
        Err((status, msg)) => (
            status,
            Json(EvalResponse {
                ok: false,
                value: None,
                error: Some(msg),
            }),
        ),
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request, StatusCode};
    use std::io::Write;
    use tower::ServiceExt;

    /// Boots a `Config` against a freshly-written tiny config file
    /// holding `body`, so the live tulisp ctx + World are wired up the
    /// same way the binary wires them. Returns the Config; caller
    /// composes a router with it.
    ///
    /// Each call gets its own unique subdirectory under `temp_dir()`
    /// so concurrent test runs don't stomp each other's config.lisp
    /// (cargo runs the lib test suite multi-threaded by default).
    async fn config_with(body: &str) -> Config {
        // tulisp-async's executor needs a tokio runtime in scope; we
        // already have one via #[tokio::test], so Config::new works.
        let mut p = std::env::temp_dir();
        p.push(format!(
            "switchyard-ui-{}-{}",
            std::process::id(),
            // Counter — even if SystemTime resolves the same nanos for
            // two near-simultaneous tests, the AtomicU64 disambiguates.
            UNIQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        std::fs::create_dir_all(&p).unwrap();
        let path = p.join("config.lisp");
        write!(std::fs::File::create(&path).unwrap(), "{body}").unwrap();
        Config::new(path.to_str().unwrap())
    }

    static UNIQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// One-shot a request and return (status, body). axum's `oneshot`
    /// avoids binding a real port.
    async fn call(config: Config, req: Request<Body>) -> (StatusCode, Vec<u8>) {
        let resp = router(config).oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, bytes.to_vec())
    }

    fn get(path: &str) -> Request<Body> {
        Request::builder().uri(path).body(Body::empty()).unwrap()
    }

    fn post(path: &str, body: &str) -> Request<Body> {
        Request::builder()
            .method(Method::POST)
            .uri(path)
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    #[tokio::test]
    async fn placeholder_route_responds() {
        let cfg = config_with("").await;
        let (status, body) = call(cfg, get("/")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(String::from_utf8_lossy(&body).contains("switchyard UI"));
    }

    #[tokio::test]
    async fn topology_endpoint_emits_components_and_connections() {
        let cfg = config_with(
            r#"(%make-grid :id 1
                 :successors
                 (list (%make-meter :id 2
                         :successors
                         (list (%make-battery :id 3)))))"#,
        )
        .await;

        let (status, body) = call(cfg, get("/api/topology")).await;
        assert_eq!(status, StatusCode::OK);
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(parsed["components"].as_array().unwrap().len(), 3);
        assert_eq!(parsed["connections"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn eval_endpoint_runs_lisp_and_returns_value() {
        let cfg = config_with("").await;
        let (status, body) = call(cfg, post("/api/eval", "(+ 2 3)")).await;
        assert_eq!(status, StatusCode::OK);
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["value"], "5");
        assert!(parsed["error"].is_null());
    }

    #[tokio::test]
    async fn eval_endpoint_reports_lisp_errors() {
        let cfg = config_with("").await;
        let (status, body) = call(cfg, post("/api/eval", "(undefined-fn 1)")).await;
        assert_eq!(status, StatusCode::OK);
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["ok"], false);
        assert!(parsed["value"].is_null());
        assert!(parsed["error"].as_str().unwrap().len() > 0);
    }

    #[tokio::test]
    async fn eval_endpoint_mutates_world() {
        // Confirm an /api/eval call that registers a component shows
        // up in the topology endpoint immediately afterwards. This is
        // the load-bearing claim of the "Lisp eval as the unifying
        // mutation API" design.
        let cfg = config_with("").await;
        let (status, _) = call(cfg.clone(), post("/api/eval", "(%make-grid :id 42)")).await;
        assert_eq!(status, StatusCode::OK);

        let (_, body) = call(cfg, get("/api/topology")).await;
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let components = parsed["components"].as_array().unwrap();
        assert!(components.iter().any(|c| c["id"] == 42));
    }
}
