//! Hot-reload integration test. Edit the config file on disk and
//! assert the world rebuilds with the new content. Mirrors what
//! the production binary does when the user saves config.lisp.

mod common;

use std::time::Duration;

use common::TestServer;
use serde_json::Value;
use switchyard::lisp::Config;

#[tokio::test(flavor = "multi_thread")]
async fn editing_config_lisp_rebuilds_the_world() {
    // Initial topology: just a grid.
    let initial = "(set-microgrid-id 9)\n(%make-grid :id 1)\n";
    let s = TestServer::start(initial).await;
    // Spawn the watcher — production path is `tokio::spawn(config.clone().watch())`.
    tokio::spawn(Config::clone(&s.config).watch());
    // Give the watcher a moment to install the inotify hook before
    // we rewrite. Without this the rewrite races the .watch() call.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let v0 = s.config.world().version();
    let path = s.config_path();

    // Append a meter under the grid by replacing the file. The
    // notify watcher fires on close-after-write (inotify) and the
    // reload path resets the world and re-evals.
    let updated = "(set-microgrid-id 9)\n\
                   (%make-grid :id 1\n\
                    :successors (list (%make-meter :id 2)))\n";
    std::fs::write(&path, updated).expect("rewrite config");

    // Poll for the version to bump — bounded so the test fails
    // loudly rather than hanging if the watcher misfires.
    let mut versioned = false;
    for _ in 0..50 {
        if s.config.world().version() > v0 {
            versioned = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(versioned, "world version never bumped after rewrite");

    // The new meter shows up via /api/topology.
    let topo: Value = reqwest::get(format!("{}/api/topology", s.ui_url))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<i64> = topo["components"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_i64().unwrap())
        .collect();
    assert!(ids.contains(&2), "expected id 2 after reload, got {ids:?}");
}
