//! gRPC integration tests. Each test spawns a fresh `TestServer`,
//! connects via the generated `MicrogridClient`, and exercises the
//! RPC surface end-to-end.

mod common;

use common::TestServer;
use switchyard::proto::microgrid::microgrid_client::MicrogridClient;
use switchyard::proto::microgrid::{
    ListElectricalComponentConnectionsRequest, ListElectricalComponentsRequest, PowerType,
    ReceiveElectricalComponentTelemetryStreamRequest, SetElectricalComponentPowerRequest,
    SetElectricalComponentPowerRequestStatus,
};

const TINY_TOPOLOGY: &str = r#"
(set-microgrid-id 7)
(%make-grid :id 1
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
