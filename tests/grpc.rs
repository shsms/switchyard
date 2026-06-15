//! gRPC integration tests. Each test spawns a fresh `TestServer`,
//! connects via the generated `MicrogridClient`, and exercises the
//! RPC surface end-to-end.

mod common;

use common::TestServer;
use switchyard::proto::common::metrics::{Bounds, Metric};
use switchyard::proto::microgrid::microgrid_client::MicrogridClient;
use switchyard::proto::microgrid::{
    AugmentElectricalComponentBoundsRequest, ListElectricalComponentConnectionsRequest,
    ListElectricalComponentsRequest, PowerType, ReceiveElectricalComponentTelemetryStreamRequest,
    ReceiveElectricalComponentTelemetryStreamResponse, SetElectricalComponentPowerRequest,
    SetElectricalComponentPowerRequestStatus,
};

/// Pull the AC active-power value (W) out of a telemetry response, if present.
fn active_power_w(resp: &ReceiveElectricalComponentTelemetryStreamResponse) -> Option<f32> {
    use switchyard::proto::common::metrics::{Metric, metric_value_variant::MetricValueVariant};
    let t = resp.telemetry.as_ref()?;
    t.metric_samples.iter().find_map(|s| {
        if s.metric != Metric::AcPowerActive as i32 {
            return None;
        }
        match s.value.as_ref()?.metric_value_variant.as_ref()? {
            MetricValueVariant::SimpleMetric(v) => Some(v.value),
            _ => None,
        }
    })
}

const TINY_TOPOLOGY: &str = r#"
(set-microgrid-id 7)
(%make-grid-connection-point :id 1
            :successors
            (list (%make-meter :id 2 :main t
                               :successors
                               (list (%make-battery-inverter
                                      :id 4
                                      :rated-lower -5000.0
                                      :rated-upper  5000.0
                                      :successors
                                      (list (%make-battery
                                             :id 3
                                             :rated-lower -5000.0
                                             :rated-upper  5000.0)))))))
"#;

async fn connect(s: &TestServer) -> MicrogridClient<tonic::transport::Channel> {
    MicrogridClient::connect(s.grpc_url.clone())
        .await
        .expect("grpc connect")
}

#[tokio::test(flavor = "multi_thread")]
async fn list_components_returns_topology() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let mut c = connect(&s).await;
    let resp = c
        .list_electrical_components(ListElectricalComponentsRequest::default())
        .await
        .expect("list ok")
        .into_inner();
    let ids: Vec<u64> = resp.electrical_components.iter().map(|c| c.id).collect();
    assert!(ids.contains(&1));
    assert!(ids.contains(&2));
    assert!(ids.contains(&3));
    assert!(ids.contains(&4));
}

#[tokio::test(flavor = "multi_thread")]
async fn list_connections_returns_edges() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let mut c = connect(&s).await;
    let resp = c
        .list_electrical_component_connections(ListElectricalComponentConnectionsRequest::default())
        .await
        .expect("list ok")
        .into_inner();
    // grid → meter, meter → inverter, inverter → battery.
    assert_eq!(resp.electrical_component_connections.len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn set_power_happy_path_returns_success() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let mut c = connect(&s).await;
    let resp = c
        .set_electrical_component_power(SetElectricalComponentPowerRequest {
            electrical_component_id: 4, // the battery inverter
            power: 1000.0,
            power_type: PowerType::Active as i32,
            request_lifetime: Some(30),
        })
        .await
        .expect("set-power ok");
    let mut stream = resp.into_inner();
    let first = stream
        .message()
        .await
        .expect("stream poll")
        .expect("at least one status");
    assert_eq!(
        first.status,
        SetElectricalComponentPowerRequestStatus::Success as i32,
    );
}

const ERRORED_INVERTER_TOPOLOGY: &str = r#"
(set-microgrid-id 7)
(%make-grid-connection-point :id 1
            :successors
            (list (%make-meter :id 2 :main t
                               :successors
                               (list (%make-battery-inverter
                                      :id 4 :health 'error
                                      :rated-lower -5000.0
                                      :rated-upper  5000.0
                                      :successors
                                      (list (%make-battery
                                             :id 3
                                             :rated-lower -5000.0
                                             :rated-upper  5000.0)))))))
"#;

/// Inverter rated ±5 kW but its battery only ±1 kW, so the combined
/// envelope the gateway must enforce is ±1 kW — narrower than the
/// inverter's own bounds.
const NARROW_BATTERY_TOPOLOGY: &str = r#"
(set-microgrid-id 7)
(%make-grid-connection-point :id 1
            :successors
            (list (%make-meter :id 2 :main t
                               :successors
                               (list (%make-battery-inverter
                                      :id 4
                                      :rated-lower -5000.0
                                      :rated-upper  5000.0
                                      :successors
                                      (list (%make-battery
                                             :id 3
                                             :rated-lower -1000.0
                                             :rated-upper  1000.0)))))))
"#;

/// An errored inverter refuses every command — both setpoints and bounds
/// augmentations (the latter were previously accepted unconditionally).
#[tokio::test(flavor = "multi_thread")]
async fn errored_component_rejects_power_and_bounds() {
    let s = TestServer::start(ERRORED_INVERTER_TOPOLOGY).await;
    let mut c = connect(&s).await;
    let power_err = c
        .set_electrical_component_power(SetElectricalComponentPowerRequest {
            electrical_component_id: 4,
            power: 0.0,
            power_type: PowerType::Active as i32,
            request_lifetime: Some(30),
        })
        .await
        .expect_err("setpoint to an errored device should be rejected");
    // Erroring the device couples its command mode to Error → Unavailable.
    assert_eq!(power_err.code(), tonic::Code::Unavailable);

    let bounds_err = c
        .augment_electrical_component_bounds(AugmentElectricalComponentBoundsRequest {
            electrical_component_id: 4,
            target_metric: Metric::AcPowerActive as i32,
            bounds: vec![Bounds {
                lower: Some(-1000.0),
                upper: Some(1000.0),
            }],
            request_lifetime: Some(30),
        })
        .await
        .expect_err("bounds augmentation to an errored device should be rejected");
    assert_eq!(bounds_err.code(), tonic::Code::Unavailable);
}

/// An out-of-range `request_lifetime` is a protocol error that must be
/// rejected *before* the setpoint is applied — otherwise the component
/// runs at the commanded power with no expiry timer while the client
/// sees an error.
#[tokio::test(flavor = "multi_thread")]
async fn out_of_range_lifetime_rejects_without_actuating() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let mut c = connect(&s).await;
    let err = c
        .set_electrical_component_power(SetElectricalComponentPowerRequest {
            electrical_component_id: 4,
            power: 3000.0,
            power_type: PowerType::Active as i32,
            request_lifetime: Some(5), // below the 10 s minimum
        })
        .await
        .expect_err("sub-minimum lifetime should be rejected");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    // The inverter must not have actuated: stream it and confirm it stays
    // at 0 W rather than ramping to the rejected 3 kW (its ramp is instant).
    let resp = c
        .receive_electrical_component_telemetry_stream(
            ReceiveElectricalComponentTelemetryStreamRequest {
                electrical_component_id: 4,
                filter: None,
            },
        )
        .await
        .expect("subscribe");
    let mut stream = resp.into_inner();
    let checked = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut seen = 0;
        while let Ok(Some(msg)) = stream.message().await {
            if let Some(p) = active_power_w(&msg) {
                assert!(
                    p.abs() < 1.0,
                    "inverter actuated despite a rejected request: {p} W"
                );
                seen += 1;
                if seen >= 3 {
                    break;
                }
            }
        }
        seen
    })
    .await
    .expect("telemetry stream timed out");
    assert!(checked >= 3, "expected ≥3 power samples, got {checked}");
}

#[tokio::test(flavor = "multi_thread")]
async fn set_power_outside_envelope_is_rejected() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let mut c = connect(&s).await;
    // Inverter rated bounds are ±5 kW; +10 kW is outside.
    let err = c
        .set_electrical_component_power(SetElectricalComponentPowerRequest {
            electrical_component_id: 4,
            power: 10_000.0,
            power_type: PowerType::Active as i32,
            request_lifetime: Some(30),
        })
        .await
        .expect_err("expected rejection");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(
        err.message().contains("envelope") || err.message().contains("bounds"),
        "expected envelope/bounds in message, got {:?}",
        err.message()
    );
}

/// A setpoint inside the inverter's own bounds but outside its battery's
/// (narrower) bounds is rejected against the *intersection* — not
/// silently saturated. The complement of the inverter-only-bounds test.
#[tokio::test(flavor = "multi_thread")]
async fn set_power_outside_battery_inverter_intersection_is_rejected() {
    let s = TestServer::start(NARROW_BATTERY_TOPOLOGY).await;
    let mut c = connect(&s).await;
    // +3 kW: within the inverter's ±5 kW, outside the battery's ±1 kW.
    let err = c
        .set_electrical_component_power(SetElectricalComponentPowerRequest {
            electrical_component_id: 4,
            power: 3_000.0,
            power_type: PowerType::Active as i32,
            request_lifetime: Some(30),
        })
        .await
        .expect_err("expected rejection against the ±1 kW intersection");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(
        err.message().contains("envelope"),
        "expected 'envelope' in message, got {:?}",
        err.message()
    );
    // Within the ±1 kW intersection is accepted.
    let resp = c
        .set_electrical_component_power(SetElectricalComponentPowerRequest {
            electrical_component_id: 4,
            power: 800.0,
            power_type: PowerType::Active as i32,
            request_lifetime: Some(30),
        })
        .await
        .expect("800 W is within the intersection");
    assert_eq!(
        resp.into_inner().message().await.unwrap().unwrap().status,
        SetElectricalComponentPowerRequestStatus::Success as i32,
    );
}

/// 0 W (the fail-safe park) must always be accepted, even when an
/// augmentation has narrowed the envelope to exclude it.
#[tokio::test(flavor = "multi_thread")]
async fn zero_power_is_always_allowed() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let mut c = connect(&s).await;
    // Narrow inverter 4 to discharge-only [-5 kW, -1 kW], excluding 0 W.
    c.augment_electrical_component_bounds(AugmentElectricalComponentBoundsRequest {
        electrical_component_id: 4,
        target_metric: Metric::AcPowerActive as i32,
        bounds: vec![Bounds {
            lower: Some(-5000.0),
            upper: Some(-1000.0),
        }],
        request_lifetime: Some(30),
    })
    .await
    .expect("augment ok");

    // A non-zero setpoint outside the augmented band is still rejected...
    let err = c
        .set_electrical_component_power(SetElectricalComponentPowerRequest {
            electrical_component_id: 4,
            power: 500.0,
            power_type: PowerType::Active as i32,
            request_lifetime: Some(30),
        })
        .await
        .expect_err("500 W is outside the augmented envelope");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);

    // ...but 0 W is accepted regardless.
    let resp = c
        .set_electrical_component_power(SetElectricalComponentPowerRequest {
            electrical_component_id: 4,
            power: 0.0,
            power_type: PowerType::Active as i32,
            request_lifetime: Some(30),
        })
        .await
        .expect("0 W must be accepted");
    let first = resp
        .into_inner()
        .message()
        .await
        .expect("stream poll")
        .expect("a status");
    assert_eq!(
        first.status,
        SetElectricalComponentPowerRequestStatus::Success as i32,
    );
}

/// A malformed augmentation — inverted, or disjoint from the component's
/// bounds — must be rejected, not silently brick the component (every
/// setpoint then rejected while the running output goes unconstrained).
#[tokio::test(flavor = "multi_thread")]
async fn malformed_augmentation_is_rejected() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let mut c = connect(&s).await;

    let inverted = c
        .augment_electrical_component_bounds(AugmentElectricalComponentBoundsRequest {
            electrical_component_id: 4,
            target_metric: Metric::AcPowerActive as i32,
            bounds: vec![Bounds {
                lower: Some(1000.0),
                upper: Some(-1000.0),
            }],
            request_lifetime: Some(30),
        })
        .await
        .expect_err("inverted bounds must be rejected");
    assert_eq!(inverted.code(), tonic::Code::InvalidArgument);

    // Disjoint from the inverter's rated ±5 kW band.
    let disjoint = c
        .augment_electrical_component_bounds(AugmentElectricalComponentBoundsRequest {
            electrical_component_id: 4,
            target_metric: Metric::AcPowerActive as i32,
            bounds: vec![Bounds {
                lower: Some(50_000.0),
                upper: Some(60_000.0),
            }],
            request_lifetime: Some(30),
        })
        .await
        .expect_err("disjoint bounds must be rejected");
    assert_eq!(disjoint.code(), tonic::Code::InvalidArgument);

    // A valid tightening still succeeds.
    c.augment_electrical_component_bounds(AugmentElectricalComponentBoundsRequest {
        electrical_component_id: 4,
        target_metric: Metric::AcPowerActive as i32,
        bounds: vec![Bounds {
            lower: Some(-2000.0),
            upper: Some(2000.0),
        }],
        request_lifetime: Some(30),
    })
    .await
    .expect("valid augmentation must be accepted");
}

#[tokio::test(flavor = "multi_thread")]
async fn telemetry_stream_emits_samples_for_a_component() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let mut c = connect(&s).await;
    let resp = c
        .receive_electrical_component_telemetry_stream(
            ReceiveElectricalComponentTelemetryStreamRequest {
                electrical_component_id: 2, // main meter
                filter: None,
            },
        )
        .await
        .expect("subscribe");
    let mut stream = resp.into_inner();
    let mut got = 0usize;
    let take = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Ok(Some(msg)) = stream.message().await {
            if msg.telemetry.is_some() {
                got += 1;
                if got >= 2 {
                    break;
                }
            }
        }
    })
    .await;
    assert!(take.is_ok(), "stream timed out before 2 samples");
    assert!(got >= 2, "expected ≥2 samples, got {got}");
}
