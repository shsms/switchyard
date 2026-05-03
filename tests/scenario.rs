//! End-to-end scenario integration. Drives a battery through a
//! known charge/discharge cycle, snapshots the report, and asserts
//! the peak / integral / SoC numbers match the analytical
//! expectation within tolerance. Exercises the same boot path the
//! sample scenarios/example.lisp uses.

mod common;

use std::time::Duration;

use common::TestServer;
use serde_json::Value;

const TOPOLOGY: &str = r#"
(set-microgrid-id 9)
(%make-grid :id 1
            :successors
            (list (%make-meter
                   :id 2
                   :main t
                   :successors
                   (list (%make-battery-inverter
                          :id 4
                          :rated-lower -10000.0
                          :rated-upper  10000.0
                          :successors
                          (list (%make-battery
                                 :id 3
                                 :capacity 100000.0
                                 :rated-lower -10000.0
                                 :rated-upper  10000.0)))))))
"#;

async fn report(client: &reqwest::Client, s: &TestServer) -> Value {
    client
        .get(format!("{}/api/scenario/report", s.ui_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn topology(client: &reqwest::Client, s: &TestServer) -> Value {
    client
        .get(format!("{}/api/topology", s.ui_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn eval_or_panic(client: &reqwest::Client, s: &TestServer, body: &str) {
    let r = client
        .post(format!("{}/api/eval", s.ui_url))
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = r.status();
    let json: Value = r.json().await.unwrap();
    assert!(
        status.is_success() && json["ok"] == true,
        "eval {body} failed: {status} {json}",
    );
}

/// `(set-meter-power id (lambda () N))` round-trips through the
/// HTTP /api/eval boundary, the polymorphic Lisp setter installs
/// a DynamicScalar source on the meter, and the next physics tick
/// exposes the resolved value via /api/topology's main-meter peak
/// once a snapshot fires. Mirrors what the dashboard does when a
/// scenario edits a curve from the side panel.
#[tokio::test(flavor = "multi_thread")]
async fn lambda_meter_power_resolves_through_http_eval() {
    let s = TestServer::start(TOPOLOGY).await;
    let client = reqwest::Client::new();

    eval_or_panic(&client, &s, "(scenario-start \"lambda\")").await;
    // Symbol form (the global mutates between snapshots — the
    // integration test asserts the deref happens each tick).
    eval_or_panic(&client, &s, "(setq curve 1234.5)").await;
    eval_or_panic(&client, &s, "(set-meter-power 2 'curve)").await;

    let mut now = chrono::Utc::now();
    s.config.world().tick_once(now, std::time::Duration::from_millis(100));
    s.config.world().record_history_snapshot(now);

    let topo = topology(&client, &s).await;
    // Telemetry isn't on /api/topology; assert via the report's
    // main-meter peak instead.
    let r = report(&client, &s).await;
    let peak = r["peak_main_meter_w"].as_f64().unwrap();
    assert!(
        (peak - 1234.5).abs() < 1.0,
        "expected ~1234.5 W peak via symbol curve, got {peak} (topo {topo})",
    );

    // Mutate the global; the next snapshot picks up the new value.
    eval_or_panic(&client, &s, "(setq curve 4321.0)").await;
    now += chrono::Duration::seconds(1);
    s.config.world().tick_once(now, std::time::Duration::from_millis(100));
    s.config.world().record_history_snapshot(now);
    let r = report(&client, &s).await;
    let peak = r["peak_main_meter_w"].as_f64().unwrap();
    assert!(
        (peak - 4321.0).abs() < 1.0,
        "expected ~4321.0 W peak after symbol mutation, got {peak}",
    );

    // Lambda form: replace the source with a thunk and confirm
    // the next snapshot resolves it.
    eval_or_panic(
        &client,
        &s,
        "(set-meter-power 2 (lambda () 9999.0))",
    )
    .await;
    now += chrono::Duration::seconds(1);
    s.config.world().tick_once(now, std::time::Duration::from_millis(100));
    s.config.world().record_history_snapshot(now);
    let r = report(&client, &s).await;
    let peak = r["peak_main_meter_w"].as_f64().unwrap();
    assert!(
        (peak - 9999.0).abs() < 1.0,
        "expected ~9999 W peak via lambda, got {peak}",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn driver_run_aggregates_peak_charge_and_soc_stats() {
    let s = TestServer::start(TOPOLOGY).await;
    let client = reqwest::Client::new();

    // Start the scenario then push a known charge setpoint.
    for body in [
        "(scenario-start \"smoke\")",
        "(set-active-power 4 3600.0 60000)",
    ] {
        client
            .post(format!("{}/api/eval", s.ui_url))
            .body(body)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }

    // Drive the simulation deterministically: tick + snapshot at
    // explicit timestamps. 10 sim-seconds at +3600 W = 10 Wh
    // charged into the battery.
    let mut now = chrono::Utc::now();
    s.config.world().tick_once(now, Duration::from_millis(100));
    s.config.world().record_history_snapshot(now);
    now += chrono::Duration::seconds(10);
    s.config.world().tick_once(now, Duration::from_secs(10));
    s.config.world().record_history_snapshot(now);

    let r = report(&client, &s).await;

    // Peak through the main meter — the inverter is publishing
    // 3600 W up the stack.
    let peak = r["peak_main_meter_w"].as_f64().unwrap();
    assert!(
        (3000.0..=4000.0).contains(&peak),
        "expected ~3600 W peak, got {peak}",
    );

    // Battery energy charged ≈ 10 Wh. Tolerance for the seed
    // sample dt at scenario_start.
    let charged = r["total_battery_charged_wh"].as_f64().unwrap();
    assert!(
        (8.0..=12.0).contains(&charged),
        "expected ~10 Wh charged, got {charged}",
    );
    let discharged = r["total_battery_discharged_wh"].as_f64().unwrap();
    assert_eq!(
        discharged, 0.0,
        "no discharge expected on a charge-only run, got {discharged}",
    );

    // SoC stats reflect the single battery's current SoC. Default
    // initial-soc on a battery is 50 % per BatteryConfig::default,
    // and 10 Wh into a 100000 Wh capacity is +0.01 % — still ≈ 50.
    let soc = &r["soc_stats"];
    assert!(!soc.is_null(), "soc_stats missing");
    let mean = soc["mean_pct"].as_f64().unwrap();
    assert!(
        (45.0..=55.0).contains(&mean),
        "expected mean SoC ≈ 50, got {mean}",
    );

    // per_battery and per_pv shapes.
    let per_battery = r["per_battery"].as_array().unwrap();
    assert_eq!(per_battery.len(), 1);
    assert_eq!(per_battery[0]["id"], 3);

    // Stop freezes elapsed.
    client
        .post(format!("{}/api/eval", s.ui_url))
        .body("(scenario-stop)")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let frozen = report(&client, &s).await;
    let frozen_elapsed = frozen["scenario_elapsed_s"].as_f64().unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let later = report(&client, &s).await;
    assert_eq!(
        frozen_elapsed,
        later["scenario_elapsed_s"].as_f64().unwrap(),
        "elapsed should freeze after scenario-stop",
    );
}
