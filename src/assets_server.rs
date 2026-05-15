//! gRPC server: implements the Frequenz Assets API
//! (`frequenz.api.assets.v1.PlatformAssets`) on top of switchyard's
//! `World` + `Config`.
//!
//! Exposes the same topology data as `MicrogridServer`, just under the
//! assets API surface. All three RPCs validate the request's
//! `microgrid_id` against the configured one — switchyard hosts a
//! single microgrid, so anything else is `NotFound`.

use crate::lisp::Config;
use crate::proto::assets::{
    GetMicrogridRequest, GetMicrogridResponse,
    ListMicrogridElectricalComponentConnectionsRequest,
    ListMicrogridElectricalComponentConnectionsResponse, ListMicrogridElectricalComponentsRequest,
    ListMicrogridElectricalComponentsResponse, platform_assets_server,
};
use crate::proto::common::microgrid::electrical_components::ElectricalComponentConnection;
use crate::proto::common::microgrid::{Microgrid, MicrogridStatus};
use crate::proto_conv::make_component_proto;
use prost_types::Timestamp;
use std::time::SystemTime;

pub struct AssetsServer {
    pub config: Config,
}

impl AssetsServer {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    /// Rejects requests that target a different microgrid than the one
    /// switchyard is configured for.
    fn check_microgrid_id(&self, requested: u64) -> Result<(), tonic::Status> {
        let configured = self.config.metadata().microgrid_id;
        if requested != configured {
            return Err(tonic::Status::not_found(format!(
                "microgrid {requested} not found (this switchyard hosts {configured})"
            )));
        }
        Ok(())
    }
}

#[tonic::async_trait]
impl platform_assets_server::PlatformAssets for AssetsServer {
    async fn get_microgrid(
        &self,
        request: tonic::Request<GetMicrogridRequest>,
    ) -> Result<tonic::Response<GetMicrogridResponse>, tonic::Status> {
        let req = request.into_inner();
        self.check_microgrid_id(req.microgrid_id)?;

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

    async fn list_microgrid_electrical_components(
        &self,
        request: tonic::Request<ListMicrogridElectricalComponentsRequest>,
    ) -> Result<tonic::Response<ListMicrogridElectricalComponentsResponse>, tonic::Status> {
        let req = request.into_inner();
        self.check_microgrid_id(req.microgrid_id)?;

        let components: Vec<_> = self
            .config
            .world()
            .components()
            .iter()
            .filter(|c| !c.is_hidden())
            .map(|c| make_component_proto(c.as_ref()))
            .filter(|c| {
                (req.component_ids.is_empty() || req.component_ids.contains(&c.id))
                    && (req.categories.is_empty() || req.categories.contains(&c.category))
            })
            .collect();
        Ok(tonic::Response::new(
            ListMicrogridElectricalComponentsResponse {
                microgrid_id: req.microgrid_id,
                components,
            },
        ))
    }

    async fn list_microgrid_electrical_component_connections(
        &self,
        request: tonic::Request<ListMicrogridElectricalComponentConnectionsRequest>,
    ) -> Result<tonic::Response<ListMicrogridElectricalComponentConnectionsResponse>, tonic::Status>
    {
        let req = request.into_inner();
        self.check_microgrid_id(req.microgrid_id)?;

        let connections: Vec<_> = self
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
                (req.source_component_ids.is_empty()
                    || req
                        .source_component_ids
                        .contains(&c.source_electrical_component_id))
                    && (req.destination_component_ids.is_empty()
                        || req
                            .destination_component_ids
                            .contains(&c.destination_electrical_component_id))
            })
            .collect();
        Ok(tonic::Response::new(
            ListMicrogridElectricalComponentConnectionsResponse {
                microgrid_id: req.microgrid_id,
                connections,
            },
        ))
    }
}
