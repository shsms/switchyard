//! Embedded web UI server.
//!
//! Runs alongside the gRPC server on the same tokio runtime, separate
//! port (default 8801, see UI.org). Phase 1 surface is intentionally
//! tiny — just enough scaffolding to prove axum integrates cleanly and
//! the binary still ships as a single file. Subsequent commits add the
//! real endpoints (/api/eval, /api/topology, /ws/events, …) and the
//! embedded SPA assets.

use std::net::SocketAddr;

use axum::{Router, routing::get};

/// Spawn the UI HTTP server on `addr`. Returns once the listener is
/// bound and accepting connections; the server itself runs to
/// completion of the returned future.
///
/// Localhost-only by default (the caller decides the bind address);
/// non-loopback is opt-in via the `--ui-bind` CLI flag.
pub async fn serve(addr: SocketAddr) -> Result<(), std::io::Error> {
    let app = router();
    let listener = tokio::net::TcpListener::bind(addr).await?;
    log::info!("Switchyard UI listening on http://{addr}");
    axum::serve(listener, app).await
}

fn router() -> Router {
    Router::new().route("/", get(placeholder_index))
}

/// Phase-1 placeholder. Replaced by the embedded SPA shell when the
/// rust-embed assets land.
async fn placeholder_index() -> &'static str {
    "switchyard UI — phase 1 scaffold. SPA assets land in a later commit."
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Boot the server on an OS-picked port and hit the root once to
    /// prove the route table and listener wire up correctly. Catches
    /// surface-level axum / tokio integration regressions even before
    /// any real endpoints exist.
    #[tokio::test]
    async fn placeholder_route_responds() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router()).await.unwrap();
        });

        // Tiny TCP-level GET so we don't pull in a whole HTTP client
        // dep just for one test. axum returns the body verbatim with a
        // 200 OK on the placeholder route.
        let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        sock.write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut buf = Vec::new();
        sock.read_to_end(&mut buf).await.unwrap();
        let resp = String::from_utf8_lossy(&buf);
        assert!(resp.starts_with("HTTP/1.0 200 OK"), "got: {resp}");
        assert!(resp.contains("switchyard UI"), "got: {resp}");
    }
}
