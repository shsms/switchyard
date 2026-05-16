//! Headless switchyard simulator: load `config.lisp`, spawn the
//! physics tick, serve the Microgrid gRPC API.

use std::path::PathBuf;

use simplelog::{
    ColorChoice, CombinedLogger, ConfigBuilder, LevelFilter, TermLogger, TerminalMode,
};
use switchyard::{
    lisp::Config, proto::microgrid::microgrid_server::MicrogridServer as MicrogridGrpcServer,
    server::MicrogridServer, sim::World, ui, ui_log,
};
use tonic::transport::Server;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    // Combined logger: terminal output (existing UX) + a tap that
    // captures records into a ring buffer + broadcasts them on a
    // tokio channel. The UI server reads both: /api/logs returns the
    // ring for backfill on page load, /ws/events forwards the live
    // stream so the SPA's log panel updates in real time.
    let log_tap = ui_log::LogTap::new(500, LevelFilter::Info);
    ui_log::LOG_TAP
        .set(log_tap.clone())
        .unwrap_or_else(|_| panic!("LOG_TAP already initialised"));
    // Suppress per-tick "channel closed" spam from frequenz-microgrid
    // 0.4.1's ComponentTelemetryTracker. When a `BatteryPool` drops
    // (which happens on every topology rebuild) the tracker tasks it
    // spawned keep ticking on a timer and log at error level when
    // they fail to send into the closed mpsc — see
    // /vagrant/upstream-component-tracker-leak.md. The trackers are
    // otherwise harmless (orphaned, no measurable CPU), but the log
    // spam scales linearly with rebuilds. Drop the noisy module here
    // until upstream lands a fix.
    let log_config = ConfigBuilder::new()
        .add_filter_ignore_str(
            "frequenz_microgrid::microgrid::telemetry_tracker::component_telemetry_tracker",
        )
        .build();
    CombinedLogger::init(vec![
        TermLogger::new(
            LevelFilter::Info,
            log_config,
            TerminalMode::Mixed,
            ColorChoice::Auto,
        ),
        Box::new(log_tap),
    ])
    .unwrap();

    let cfg_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.lisp".to_string());
    let cfg_path = PathBuf::from(cfg_path);
    log::info!("Loading config from {}", cfg_path.display());

    let config = Config::new(cfg_path.to_str().unwrap()).unwrap_or_else(|e| {
        log::error!("Failed to load config:\n{e}");
        std::process::exit(1);
    });
    let world = config.world();
    log::info!(
        "Loaded {} components, {} connections",
        world.components().len(),
        world.connections().len()
    );

    World::clone(&world).spawn_physics();
    World::clone(&world).spawn_history_sampler();

    let socket_addr_str = config.socket_addr();
    let socket_addr = socket_addr_str
        .parse()
        .unwrap_or_else(|e| panic!("invalid socket addr {socket_addr_str:?}: {e}"));
    log::info!("Microgrid gRPC server listening on {socket_addr_str}");

    // Watch the config file in the background so saves trigger reload.
    tokio::spawn(config.clone().watch());

    // UI server. Localhost-only for now; --ui-bind / --ui-port land
    // in a follow-up commit.
    let ui_addr = "127.0.0.1:8801".parse().unwrap();
    let ui_config = config.clone();
    // Loopback Microgrid client: a frequenz-microgrid `Microgrid`
    // pointed at this binary's own gRPC server. Constructed in the
    // background so the UI server starts immediately and dashboard
    // endpoints return 503 until the gRPC server is reachable + the
    // component graph is built. See UI-design.org §Z2.
    let microgrid = ui::new_microgrid_slot();
    let grpc_url = format!("http://{socket_addr_str}");
    ui::spawn_microgrid_loopback(grpc_url, microgrid.clone(), world.clone());
    tokio::spawn(async move {
        if let Err(e) = ui::serve(ui_addr, ui_config, microgrid).await {
            log::error!("UI server exited: {e}");
        }
    });

    let server = MicrogridServer::new(config);
    Server::builder()
        .add_service(MicrogridGrpcServer::new(server))
        .serve(socket_addr)
        .await
        .unwrap();
}
