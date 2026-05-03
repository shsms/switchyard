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
//! - In-process tests can poke at `cfg.world()` directly when the
//!   black-box gRPC / HTTP surface isn't enough.
//! - LOG_TAP and other process-level globals stay un-initialised in
//!   tests, so the `/api/logs` endpoint just returns empty —
//!   acceptable for a fixture.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use switchyard::{
    lisp::Config, proto::microgrid::microgrid_server::MicrogridServer as MicrogridGrpcServer,
    server::MicrogridServer, sim::World, ui,
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
        std::fs::write(&path, config_body).expect("write config");

        let config = Config::new(path.to_str().unwrap());
        // Physics + history sampler match the prod boot sequence.
        World::clone(&config.world()).spawn_physics();
        World::clone(&config.world()).spawn_history_sampler();

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
        handles.push(tokio::spawn(async move {
            let _ = ui::serve_with_listener(ui_listener, ui_config).await;
        }));

        let server = MicrogridServer::new(config.clone());
        handles.push(tokio::spawn(async move {
            let _ = Server::builder()
                .add_service(MicrogridGrpcServer::new(server))
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

    /// Convenience — same as `TestServer::start` but with the
    /// minimum config (microgrid id only) for tests that don't
    /// care about the topology.
    #[allow(dead_code)]
    pub async fn empty() -> Self {
        Self::start("(set-microgrid-id 1)").await
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
