//! Headless switchyard simulator: load `config.lisp`, spawn the
//! physics tick, serve the Microgrid gRPC API.

use std::path::PathBuf;

use simplelog::{
    ColorChoice, CombinedLogger, ConfigBuilder, LevelFilter, TermLogger, TerminalMode,
};
use switchyard::{
    assets_server::AssetsServer,
    lisp::Config,
    proto::assets::platform_assets_server::PlatformAssetsServer as AssetsGrpcServer,
    proto::microgrid::microgrid_server::MicrogridServer as MicrogridGrpcServer,
    server::MicrogridServer,
    sim::MicrogridSite,
    ui, ui_log,
};
use tonic::transport::Server;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    // Suppress per-tick "channel closed" spam from frequenz-microgrid
    // 0.4.1's ComponentTelemetryTracker. When a `BatteryPool` drops
    // (which happens on every topology rebuild) the tracker tasks it
    // spawned keep ticking on a timer and log at error level when
    // they fail to send into the closed mpsc — see
    // /vagrant/upstream-tracker-leak.md. The trackers are otherwise
    // harmless (orphaned, no measurable CPU), but the log spam scales
    // linearly with rebuilds. Drop the noisy module here — same list
    // applied to both the terminal logger and the UI tap so the SPA's
    // log panel + /api/logs backfill stay clean too.
    let ignore_targets: &[&str] =
        &["frequenz_microgrid::microgrid::telemetry_tracker::component_telemetry_tracker"];

    // Combined logger: terminal output (existing UX) + a tap that
    // captures records into a ring buffer + broadcasts them on a
    // tokio channel. The UI server reads both: /api/logs returns the
    // ring for backfill on page load, /ws/events forwards the live
    // stream so the SPA's log panel updates in real time.
    let log_tap = ui_log::LogTap::new(
        500,
        LevelFilter::Info,
        ignore_targets.iter().map(|s| (*s).to_owned()).collect(),
    );
    ui_log::LOG_TAP
        .set(log_tap.clone())
        .unwrap_or_else(|_| panic!("LOG_TAP already initialised"));
    let mut log_cfg = ConfigBuilder::new();
    for t in ignore_targets {
        log_cfg.add_filter_ignore_str(t);
    }
    let log_config = log_cfg.build();
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

    // Snapshot the enterprise registry: one tuple per microgrid
    // (id, name, grpc_port, site). Each will get its own physics
    // tick + history sampler + Microgrid gRPC server.
    let entries: Vec<(u64, String, u16, switchyard::sim::MicrogridSite)> = config
        .microgrids()
        .lock()
        .values()
        .map(|e| (e.def.id, e.def.name.clone(), e.def.grpc_port, e.site.clone()))
        .collect();
    if entries.is_empty() {
        log::error!("Boot produced no microgrids in the registry — config eval bug?");
        std::process::exit(1);
    }
    log::info!(
        "Enterprise carries {} microgrid(s); spawning per-microgrid runtimes",
        entries.len()
    );
    for (id, name, port, site) in &entries {
        log::info!(
            "Microgrid #{id} {name:?} → :{port} ({} components, {} connections)",
            site.components().len(),
            site.connections().len(),
        );
        MicrogridSite::clone(site).spawn_physics();
        MicrogridSite::clone(site).spawn_history_sampler();
    }

    // Watch the config file in the background so saves trigger reload.
    tokio::spawn(config.clone().watch());

    // UI server. Localhost-only for now; --ui-bind / --ui-port land
    // in a follow-up commit.
    let ui_addr = "127.0.0.1:8801".parse().unwrap();
    let ui_config = config.clone();
    // One loopback Microgrid client per registered microgrid. The
    // map keys by id so the upcoming /api/mg/{id}/microgrid/*
    // routes can look up the right slot directly; the legacy
    // /api/microgrid/* endpoints continue to read the *first*
    // microgrid's slot for backward compat until C1 lands.
    let loopbacks = ui::new_microgrid_loopbacks();
    let (first_id, _, _first_port, _) = entries[0].clone();
    let mut primary_slot: Option<ui::SharedMicrogrid> = None;
    for (id, name, port, site) in &entries {
        let slot = ui::new_microgrid_slot();
        let grpc_url = format!("http://[::1]:{port}");
        ui::spawn_microgrid_loopback(grpc_url, slot.clone(), site.clone());
        loopbacks.write().insert(*id, slot.clone());
        if *id == first_id {
            primary_slot = Some(slot);
        }
        log::info!("Microgrid #{id} {name:?} loopback client spawned");
    }
    let microgrid = primary_slot.expect("primary loopback slot");
    tokio::spawn(async move {
        if let Err(e) = ui::serve(ui_addr, ui_config, microgrid, loopbacks).await {
            log::error!("UI server exited: {e}");
        }
    });

    // One Microgrid gRPC server per registry entry. tonic Server's
    // `serve` future drives a single listener — we spawn one task
    // per microgrid so the binary's main future is the join of all
    // listeners. AssetsServer mounts on the first microgrid's port
    // for now; B3 splits it onto its own port (9900).
    let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    for (id, name, port, site) in entries {
        let addr: std::net::SocketAddr = format!("[::1]:{port}")
            .parse()
            .unwrap_or_else(|e| panic!("invalid grpc port for microgrid {id}: {e}"));
        log::info!("Microgrid #{id} {name:?} gRPC listening on {addr}");
        let cfg_for_server = config.clone();
        let is_primary = id == first_id;
        let cfg_for_assets = config.clone();
        tasks.push(tokio::spawn(async move {
            let mg_server = MicrogridServer::new(cfg_for_server, id, site);
            let mut builder =
                Server::builder().add_service(MicrogridGrpcServer::new(mg_server));
            if is_primary {
                builder = builder
                    .add_service(AssetsGrpcServer::new(AssetsServer::new(cfg_for_assets)));
            }
            if let Err(e) = builder.serve(addr).await {
                log::error!("Microgrid #{id} gRPC server exited: {e}");
            }
        }));
    }
    for h in tasks {
        let _ = h.await;
    }
}
