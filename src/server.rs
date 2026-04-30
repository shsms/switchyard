//! gRPC server: implements the Frequenz Microgrid API on top of
//! switchyard's `World` + `Config`.
//!
//! Streaming telemetry is one tokio task per subscription; each task
//! samples its component at the component's own `stream_interval` and
//! forwards the protobuf message until the client disconnects.

use std::pin::Pin;
use std::time::{Duration, SystemTime};

use prost_types::Timestamp;
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;

use crate::lisp::Config;
use crate::proto::common::metrics::Metric;
use crate::proto::common::microgrid::electrical_components::ElectricalComponentConnection;
use crate::proto::common::microgrid::{Microgrid, MicrogridStatus};
use crate::proto::microgrid::{
    AckElectricalComponentErrorRequest, AugmentElectricalComponentBoundsRequest,
    AugmentElectricalComponentBoundsResponse, GetMicrogridResponse,
    ListElectricalComponentConnectionsRequest, ListElectricalComponentConnectionsResponse,
    ListElectricalComponentsRequest, ListElectricalComponentsResponse, ListSensorRequest,
    ListSensorsResponse, PowerType, PutElectricalComponentInStandbyRequest,
    ReceiveElectricalComponentTelemetryStreamRequest,
    ReceiveElectricalComponentTelemetryStreamResponse, ReceiveSensorTelemetryStreamRequest,
    ReceiveSensorTelemetryStreamResponse, SetElectricalComponentPowerRequest,
    SetElectricalComponentPowerRequestStatus, SetElectricalComponentPowerResponse,
    StartElectricalComponentRequest, StopElectricalComponentRequest, microgrid_server,
};
use crate::proto_conv::{make_component_proto, telemetry_to_proto};
use crate::sim::{SetpointError, bounds::VecBounds};
use crate::timeout_tracker::TimeoutTracker;

/// Default lifetime when a client does not supply one. Microsim uses
/// a configurable `retain-requests-duration-ms`; switchyard leaves the
/// constant for now and exposes it as a knob in a follow-up.
const DEFAULT_REQUEST_LIFETIME: Duration = Duration::from_secs(60);

pub struct MicrogridServer {
    pub config: Config,
    timeout_tracker: TimeoutTracker,
}

impl MicrogridServer {
    pub fn new(config: Config) -> Self {
        let me = Self {
            config,
            timeout_tracker: TimeoutTracker::new(),
        };
        me.start_timeout_tracker();
        me
    }

    fn start_timeout_tracker(&self) {
        let tt = self.timeout_tracker.clone();
        let world = self.config.world();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                for id in tt.remove_expired() {
                    log::info!("Request timeout for component {id} — resetting setpoint");
                    if let Some(c) = world.get(id) {
                        c.reset_setpoint();
                    }
                }
            }
        });
    }
}

#[tonic::async_trait]
impl microgrid_server::Microgrid for MicrogridServer {
    type ReceiveElectricalComponentTelemetryStreamStream = Pin<
        Box<
            dyn Stream<
                    Item = Result<ReceiveElectricalComponentTelemetryStreamResponse, tonic::Status>,
                > + Send,
        >,
    >;
    type ReceiveSensorTelemetryStreamStream = Pin<
        Box<dyn Stream<Item = Result<ReceiveSensorTelemetryStreamResponse, tonic::Status>> + Send>,
    >;
    type SetElectricalComponentPowerStream = Pin<
        Box<dyn Stream<Item = Result<SetElectricalComponentPowerResponse, tonic::Status>> + Send>,
    >;

    async fn get_microgrid(
        &self,
        _request: tonic::Request<()>,
    ) -> Result<tonic::Response<GetMicrogridResponse>, tonic::Status> {
        let m = self.config.metadata();
        let now = Some(Timestamp::from(SystemTime::now()));
        Ok(tonic::Response::new(GetMicrogridResponse {
            microgrid: Some(Microgrid {
                id: m.microgrid_id,
                enterprise_id: m.enterprise_id,
                name: if m.name.is_empty() {
                    format!("Microgrid {}", m.microgrid_id)
                } else {
                    m.name
                },
                status: MicrogridStatus::Active as i32,
                create_timestamp: now,
                ..Default::default()
            }),
        }))
    }

    async fn list_electrical_components(
        &self,
        request: tonic::Request<ListElectricalComponentsRequest>,
    ) -> Result<tonic::Response<ListElectricalComponentsResponse>, tonic::Status> {
        let req = request.into_inner();
        let comps: Vec<_> = self
            .config
            .world()
            .components()
            .iter()
            .map(|c| make_component_proto(c.as_ref()))
            .filter(|c| {
                (req.electrical_component_ids.is_empty()
                    || req.electrical_component_ids.contains(&c.id))
                    && (req.electrical_component_categories.is_empty()
                        || req.electrical_component_categories.contains(&c.category))
            })
            .collect();
        Ok(tonic::Response::new(ListElectricalComponentsResponse {
            electrical_components: comps,
        }))
    }

    async fn list_electrical_component_connections(
        &self,
        request: tonic::Request<ListElectricalComponentConnectionsRequest>,
    ) -> Result<tonic::Response<ListElectricalComponentConnectionsResponse>, tonic::Status> {
        let req = request.into_inner();
        let conns: Vec<_> = self
            .config
            .world()
            .connections()
            .into_iter()
            .map(|(from, to)| ElectricalComponentConnection {
                source_electrical_component_id: from,
                destination_electrical_component_id: to,
                ..Default::default()
            })
            .filter(|c| {
                (req.source_electrical_component_ids.is_empty()
                    || req
                        .source_electrical_component_ids
                        .contains(&c.source_electrical_component_id))
                    && (req.destination_electrical_component_ids.is_empty()
                        || req
                            .destination_electrical_component_ids
                            .contains(&c.destination_electrical_component_id))
            })
            .collect();
        Ok(tonic::Response::new(
            ListElectricalComponentConnectionsResponse {
                electrical_component_connections: conns,
            },
        ))
    }

    async fn set_electrical_component_power(
        &self,
        request: tonic::Request<SetElectricalComponentPowerRequest>,
    ) -> Result<tonic::Response<Self::SetElectricalComponentPowerStream>, tonic::Status> {
        let req = request.into_inner();
        let power_type = PowerType::try_from(req.power_type)
            .map_err(|_| tonic::Status::invalid_argument("invalid power type"))?;

        let world = self.config.world();
        let component = world.get(req.electrical_component_id).ok_or_else(|| {
            tonic::Status::not_found(format!(
                "component {} not found",
                req.electrical_component_id
            ))
        })?;

        // Gateway-level envelope check: a real microgrid API gateway
        // intersects the inverter's reported AC bounds with the sum of
        // its children's reported DC bounds and rejects setpoints that
        // exceed the result. Switchyard does the same here so client
        // code sees the production behaviour even though the inverter
        // and battery don't share a data link in our model.
        if matches!(power_type, PowerType::Active) {
            if let Some(child_env) = world.aggregate_child_bounds(req.electrical_component_id) {
                let own = component
                    .effective_active_bounds()
                    .unwrap_or_default();
                let envelope = own.intersect(&child_env);
                if !envelope.contains(req.power) {
                    return Err(tonic::Status::failed_precondition(format!(
                        "set-point {} W exceeds combined envelope {}",
                        req.power, envelope
                    )));
                }
            }
        }

        let result = match power_type {
            PowerType::Unspecified => {
                return Err(tonic::Status::invalid_argument(
                    "Power type cannot be UNSPECIFIED.",
                ));
            }
            PowerType::Active => component.set_active_setpoint(req.power),
            PowerType::Reactive => component.set_reactive_setpoint(req.power),
        };

        if let Err(e) = result {
            return Err(setpoint_error_to_status(e));
        }

        // Track lifetime so the set-point clears if the client falls
        // silent. Lifetime range matches microsim's 10s..15min window.
        let duration = if let Some(dur) = req.request_lifetime {
            if !(10..=15 * 60).contains(&dur) {
                return Err(tonic::Status::invalid_argument(
                    "Request lifetime must be between 10 seconds and 15 minutes.",
                ));
            }
            Duration::from_secs(dur)
        } else {
            DEFAULT_REQUEST_LIFETIME
        };
        self.timeout_tracker
            .add(req.electrical_component_id, duration);

        // The streaming response is one immediate Success then close.
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tokio::spawn(async move {
            let _ = tx
                .send(Ok(SetElectricalComponentPowerResponse {
                    valid_until_time: None,
                    status: SetElectricalComponentPowerRequestStatus::Success as i32,
                }))
                .await;
        });
        Ok(tonic::Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn receive_electrical_component_telemetry_stream(
        &self,
        request: tonic::Request<ReceiveElectricalComponentTelemetryStreamRequest>,
    ) -> Result<tonic::Response<Self::ReceiveElectricalComponentTelemetryStreamStream>, tonic::Status>
    {
        let id = request.into_inner().electrical_component_id;
        let world = self.config.world();

        let component = world.get(id).ok_or_else(|| {
            tonic::Status::not_found(format!("component {id} not found"))
        })?;
        let interval = component.stream_interval();
        let jitter_pct = component.stream_jitter_pct().max(0.0).min(100.0);

        let (tx, rx) = tokio::sync::mpsc::channel(128);
        tokio::spawn(async move {
            // SmallRng is `Send`; seed from thread_rng once at task
            // start (which is fine because spawning the task is sync).
            use rand::{Rng, SeedableRng, rngs::SmallRng};
            let mut rng = SmallRng::from_entropy();
            // Anchor the schedule on a moving target timestamp rather
            // than on `now` after the work finishes — otherwise the
            // tx.send / telemetry overhead accumulates each iteration
            // and the stream slowly drifts out beyond `interval` per
            // step. Pattern lifted from microsim's server loop.
            let mut next_due = SystemTime::now();
            loop {
                let snapshot = component.telemetry(&world);
                let msg = telemetry_to_proto(component.as_ref(), &snapshot);
                if tx.send(Ok(msg)).await.is_err() {
                    log::debug!("stream({id}): client disconnected");
                    break;
                }
                let factor: f32 = if jitter_pct > 0.0 {
                    let j = jitter_pct / 100.0;
                    1.0 + rng.gen_range(-j..=j)
                } else {
                    1.0
                };
                let step =
                    Duration::from_secs_f32((interval.as_secs_f32() * factor).max(0.001));
                let target = next_due + step;
                let now = SystemTime::now();
                let dur = target.duration_since(now).unwrap_or(Duration::ZERO);
                tokio::time::sleep(dur).await;
                // If we fell so far behind that `target` is already
                // past the pre-sleep `now`, re-anchor to now instead
                // of trying to fire several samples back-to-back.
                next_due = target.max(now);
            }
        });

        Ok(tonic::Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn augment_electrical_component_bounds(
        &self,
        request: tonic::Request<AugmentElectricalComponentBoundsRequest>,
    ) -> Result<tonic::Response<AugmentElectricalComponentBoundsResponse>, tonic::Status> {
        let req = request.into_inner();
        let target_metric = Metric::try_from(req.target_metric).map_err(|_| {
            tonic::Status::invalid_argument(format!(
                "invalid metric type: {}",
                req.target_metric
            ))
        })?;
        if target_metric != Metric::AcPowerActive {
            return Err(tonic::Status::invalid_argument(format!(
                "Unsupported metric type: {}. Only AC_POWER_ACTIVE is supported.",
                req.target_metric
            )));
        }

        let lifetime_s = req
            .request_lifetime
            .unwrap_or(5)
            .clamp(5, 15 * 60) as u64;
        let lifetime = Duration::from_secs(lifetime_s);
        let now = chrono::Utc::now();

        let component = self
            .config
            .world()
            .get(req.electrical_component_id)
            .ok_or_else(|| {
                tonic::Status::not_found(format!(
                    "component {} not found",
                    req.electrical_component_id
                ))
            })?;
        component.augment_active_bounds(now, VecBounds::new(req.bounds), lifetime);

        let expiry = now + chrono::Duration::seconds(lifetime_s as i64);
        Ok(tonic::Response::new(
            AugmentElectricalComponentBoundsResponse {
                valid_until_time: Some(prost_types::Timestamp {
                    seconds: expiry.timestamp(),
                    nanos: expiry.timestamp_subsec_nanos() as i32,
                }),
            },
        ))
    }

    // --- Unimplemented optional surface --------------------------------------

    async fn list_sensors(
        &self,
        _request: tonic::Request<ListSensorRequest>,
    ) -> Result<tonic::Response<ListSensorsResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("sensors are not modeled"))
    }
    async fn receive_sensor_telemetry_stream(
        &self,
        _request: tonic::Request<ReceiveSensorTelemetryStreamRequest>,
    ) -> Result<tonic::Response<Self::ReceiveSensorTelemetryStreamStream>, tonic::Status> {
        Err(tonic::Status::unimplemented("sensors are not modeled"))
    }
    async fn start_electrical_component(
        &self,
        _request: tonic::Request<StartElectricalComponentRequest>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        Err(tonic::Status::unimplemented("start not yet implemented"))
    }
    async fn put_electrical_component_in_standby(
        &self,
        _request: tonic::Request<PutElectricalComponentInStandbyRequest>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        Err(tonic::Status::unimplemented("standby not yet implemented"))
    }
    async fn stop_electrical_component(
        &self,
        _request: tonic::Request<StopElectricalComponentRequest>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        Err(tonic::Status::unimplemented("stop not yet implemented"))
    }
    async fn ack_electrical_component_error(
        &self,
        _request: tonic::Request<AckElectricalComponentErrorRequest>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        Err(tonic::Status::unimplemented("ack-error not yet implemented"))
    }
}

fn setpoint_error_to_status(err: SetpointError) -> tonic::Status {
    use SetpointError::*;
    match err {
        OutOfBounds { .. } => tonic::Status::failed_precondition(err.to_string()),
        NotHealthy => tonic::Status::failed_precondition(err.to_string()),
        Unsupported => tonic::Status::unimplemented(err.to_string()),
    }
}
