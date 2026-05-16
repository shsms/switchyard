//! gRPC server: implements the Frequenz Assets API
//! (`frequenz.api.assets.v1.PlatformAssets`) on top of switchyard's
//! `MicrogridSite` registry + `Config`.
//!
//! Exposes the same topology data as `MicrogridServer`, just under the
//! assets API surface. Each RPC looks up the requested `microgrid_id`
//! in the enterprise-scoped registry and dispatches against that
//! entry's `MicrogridSite` — anything missing comes back as
//! `NotFound`.

use crate::lisp::Config;
use crate::proto::assets::{
    GetMicrogridRequest, GetMicrogridResponse, ListMicrogridElectricalComponentConnectionsRequest,
    ListMicrogridElectricalComponentConnectionsResponse, ListMicrogridElectricalComponentsRequest,
    ListMicrogridElectricalComponentsResponse, platform_assets_server,
};
use crate::proto::common::microgrid::electrical_components::ElectricalComponentConnection;
use crate::proto::common::microgrid::{Microgrid, MicrogridStatus};
use crate::proto_conv::make_component_proto;
use crate::sim::MicrogridSite;
use crate::sim::microgrids::MicrogridDef;
use prost_types::Timestamp;
use std::time::SystemTime;

pub struct AssetsServer {
    pub config: Config,
}

impl AssetsServer {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    /// Resolve the registry entry for `requested` or return `NotFound`.
    /// Returned tuple is `(MicrogridDef, MicrogridSite)` so callers
    /// dispatch components / connections against the resolved site
    /// instead of the bootstrap one.
    fn lookup_microgrid(
        &self,
        requested: u64,
    ) -> Result<(MicrogridDef, MicrogridSite), tonic::Status> {
        let reg = self.config.microgrids();
        let r = reg.lock();
        r.get(&requested)
            .map(|e| (e.def.clone(), e.site.clone()))
            .ok_or_else(|| tonic::Status::not_found(format!("microgrid {requested} not found")))
    }
}

#[tonic::async_trait]
impl platform_assets_server::PlatformAssets for AssetsServer {
    async fn get_microgrid(
        &self,
        request: tonic::Request<GetMicrogridRequest>,
    ) -> Result<tonic::Response<GetMicrogridResponse>, tonic::Status> {
        let req = request.into_inner();
        let (def, _site) = self.lookup_microgrid(req.microgrid_id)?;
        let enterprise_id = self.config.metadata().enterprise_id;
        let now = Some(Timestamp::from(SystemTime::now()));
        Ok(tonic::Response::new(GetMicrogridResponse {
            microgrid: Some(Microgrid {
                id: def.id,
                enterprise_id,
                name: if def.name.is_empty() {
                    format!("Microgrid {}", def.id)
                } else {
                    def.name
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
        let (_def, site) = self.lookup_microgrid(req.microgrid_id)?;

        let components: Vec<_> = site
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
        let (_def, site) = self.lookup_microgrid(req.microgrid_id)?;

        let connections: Vec<_> = site
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
