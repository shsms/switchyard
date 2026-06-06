//! Integration-test harness: spawn a `Config`-driven switchyard
//! server in-process on OS-assigned ports, expose its gRPC + UI
//! addresses, and tear everything down on Drop.
//!
//! Each test gets its own temp dir for `config.lisp` and a fresh
//! `Config`, so parallel tests can't stomp each other's state.
//!
//! The fixture is in-process rather than out-of-process because:
//! - cargo runs each `tests/<file>.rs` as its own binary already,
//!   so OS-level isolation is overkill.
//! - In-process tests can poke at `cfg.site()` directly when the
//!   black-box gRPC / HTTP surface isn't enough.
//! - LOG_TAP and other process-level globals stay un-initialised in
//!   tests, so the `/api/logs` endpoint just returns empty —
//!   acceptable for a fixture.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use switchyard::{
    assets_server::AssetsServer, lisp::Config,
    proto::assets::platform_assets_server::PlatformAssetsServer as AssetsGrpcServer,
    proto::microgrid::microgrid_server::MicrogridServer as MicrogridGrpcServer,
    server::MicrogridServer, sim::MicrogridSite, ui,
};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

static UNIQ: AtomicU64 = AtomicU64::new(0);

/// A live switchyard instance: gRPC + UI on OS-assigned localhost
/// ports, plus the underlying [`Config`] for direct world
/// inspection. `Drop` aborts the spawned server tasks; the temp
/// dir cleans up via the held `TempDir` handle.
///
/// Each integration-test binary picks the fields it needs; the
/// `#[allow(dead_code)]` keeps the unused-warning quiet for tests
/// that only touch one surface.
#[allow(dead_code)]
pub struct TestServer {
    pub grpc_url: String,
    pub ui_url: String,
    pub config: Config,
    handles: Vec<JoinHandle<()>>,
    _tempdir: TempDir,
}

impl TestServer {
    /// Bring up a server backed by the supplied `config.lisp` body.
    /// Caller is on a tokio runtime (provided by `#[tokio::test]`).
    pub async fn start(config_body: &str) -> Self {
        let tempdir = TempDir::with_prefix(format!(
            "switchyard-it-{}-",
            UNIQ.fetch_add(1, Ordering::Relaxed),
        ))
        .expect("create temp dir");
        let path = tempdir.path().join("config.lisp");
        let wrapped = wrap_body(config_body);
        std::fs::write(&path, wrapped).expect("write config");

        let config = Config::new(path.to_str().unwrap()).expect("config eval");
        // Physics + history sampler match the prod boot sequence.
        MicrogridSite::clone(&config.site()).spawn_physics();
        MicrogridSite::clone(&config.site()).spawn_history_sampler();

        // Bind both servers to OS-assigned ports so parallel tests
        // don't collide. local_addr() reads back the chosen port
        // before we hand the listener off to the server.
        let ui_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ui port");
        let ui_addr = ui_listener.local_addr().expect("ui addr");

        let grpc_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind grpc port");
        let grpc_addr = grpc_listener.local_addr().expect("grpc addr");

        let mut handles = Vec::new();

        let ui_config = config.clone();
        // Loopback Microgrid client: same shape the binary uses.
        // The grpc_addr we just bound is
        // the URL — try_new retries lazily until the gRPC server
        // task below comes up. Integration tests for the
        // /api/microgrid/* endpoints exercise this whole loop.
        let microgrid = ui::new_microgrid_slot();
        ui::spawn_microgrid_loopback(
            format!("http://{grpc_addr}"),
            microgrid.clone(),
            config.site(),
        );
        // Single-microgrid integration test: a one-entry loopbacks
        // map. /api/microgrid/* keeps reading the primary slot for
        // backward compat.
        let loopbacks = ui::new_microgrid_loopbacks();
        loopbacks.write().insert(0, microgrid.clone());
        handles.push(tokio::spawn(async move {
            let _ = ui::serve_with_listener(
                ui_listener,
                ui_config,
                microgrid,
                loopbacks,
                ui::noop_microgrid_spawner(),
            )
            .await;
        }));

        // Single-microgrid integration test: pin the gRPC frontend
        // to the default registry entry (the one auto-seeded by
        // Config::new when no `(make-microgrid)` form ran). The id
        // sourced from metadata mirrors the legacy `set-microgrid-id`
        // path and matches what `get_microgrid` reports.
        let default_mg_id = {
            let reg = config.microgrids();
            let r = reg.lock();
            r.keys().copied().next().expect("default microgrid entry")
        };
        let microgrid_server = MicrogridServer::new(config.clone(), default_mg_id, config.site());
        let assets_server = AssetsServer::new(config.clone());
        handles.push(tokio::spawn(async move {
            let _ = Server::builder()
                .add_service(MicrogridGrpcServer::new(microgrid_server))
                .add_service(AssetsGrpcServer::new(assets_server))
                .serve_with_incoming(TcpListenerStream::new(grpc_listener))
                .await;
        }));

        // Give axum + tonic a moment to start accepting. Without
        // this, the first reqwest occasionally races the listener
        // into a connection-refused.
        tokio::time::sleep(Duration::from_millis(50)).await;

        Self {
            grpc_url: format!("http://{grpc_addr}"),
            ui_url: format!("http://{ui_addr}"),
            config,
            handles,
            _tempdir: tempdir,
        }
    }

    /// Path of the config.lisp file backing this server. Tests
    /// that exercise the watcher (hot-reload) overwrite this file
    /// to trigger a reload.
    #[allow(dead_code)]
    pub fn config_path(&self) -> PathBuf {
        self._tempdir.path().join("config.lisp")
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        for h in &self.handles {
            h.abort();
        }
    }
}

/// Wrap a test body in `(make-microgrid …)` if the body doesn't
/// already register one. Inline `(set-microgrid-id N)` survives from
/// the pre-migration shape; pull its N into the wrapper's :id so
/// per-mg id assertions hold.
fn wrap_body(body: &str) -> String {
    if body.contains("make-microgrid") {
        return body.to_string();
    }
    let (stripped, mg_id) = strip_set_microgrid_id(body);
    let inner = if stripped.trim().is_empty() {
        "nil".to_string()
    } else {
        stripped
    };
    format!("(make-microgrid :id {mg_id} :grpc-port 8800 :topology (lambda () {inner}))")
}

fn strip_set_microgrid_id(body: &str) -> (String, u64) {
    let needle = "(set-microgrid-id ";
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    let mut mg_id: u64 = 2200;
    while let Some(idx) = rest.find(needle) {
        out.push_str(&rest[..idx]);
        let tail = &rest[idx + needle.len()..];
        if let Some(close) = tail.find(')') {
            let n_str = tail[..close].trim();
            if let Ok(v) = n_str.parse::<u64>() {
                mg_id = v;
            }
            rest = &tail[close + 1..];
        } else {
            out.push_str(&rest[idx..]);
            return (out, mg_id);
        }
    }
    out.push_str(rest);
    (out, mg_id)
}
