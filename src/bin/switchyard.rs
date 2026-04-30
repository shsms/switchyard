//! Headless switchyard simulator — loads `config.lisp`, spawns the
//! physics tick, and (eventually) serves gRPC. For the scaffold pass,
//! it exercises the load + tick path without binding to a port.

use std::path::PathBuf;

use simplelog::{ColorChoice, Config as LogConfig, LevelFilter, TermLogger, TerminalMode};
use switchyard::{lisp::Config, sim::World};

fn main() {
    TermLogger::init(
        LevelFilter::Info,
        LogConfig::default(),
        TerminalMode::Mixed,
        ColorChoice::Auto,
    )
    .unwrap();

    let cfg_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.lisp".to_string());
    let cfg_path = PathBuf::from(cfg_path);
    log::info!("Loading config from {}", cfg_path.display());

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async move {
        // Construct Config inside the runtime — tulisp-async's
        // TokioExecutor captures Handle::current() at construction.
        let config = Config::new(cfg_path.to_str().unwrap());
        let world = config.world();
        log::info!(
            "Loaded {} components, {} connections",
            world.components().len(),
            world.connections().len()
        );

        World::clone(&world).spawn_physics();
        config.watch().await;
    });
}
