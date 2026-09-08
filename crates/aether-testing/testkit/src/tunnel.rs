use std::time::Duration;

use aether_gateway::{
    build_tunnel_runtime_router_with_state, TunnelConnConfig, TunnelControlPlaneClient,
    TunnelRuntimeState,
};
use aether_runtime_state::RuntimeSemaphore;

use crate::server::SpawnedServer;

pub const TUNNEL_HARNESS_NODE_ID: &str = "node-baseline";
pub const TUNNEL_HARNESS_GENERATION: &str = "tunnel-harness-generation-1";
pub const TUNNEL_HARNESS_MANAGEMENT_TOKEN: &str = "ae-tunnel-harness-management-token";

#[derive(Debug, Clone)]
pub struct TunnelHarnessConfig {
    pub node_id: String,
    pub max_streams: usize,
    pub ping_interval: Duration,
    pub outbound_queue_capacity: usize,
    pub max_in_flight_requests: Option<usize>,
    pub distributed_request_gate: Option<RuntimeSemaphore>,
}

impl Default for TunnelHarnessConfig {
    fn default() -> Self {
        Self {
            node_id: TUNNEL_HARNESS_NODE_ID.to_string(),
            max_streams: 128,
            ping_interval: Duration::from_secs(15),
            outbound_queue_capacity: 128,
            max_in_flight_requests: None,
            distributed_request_gate: None,
        }
    }
}

#[derive(Debug)]
pub struct TunnelHarness {
    server: SpawnedServer,
}

impl TunnelHarness {
    pub async fn start(config: TunnelHarnessConfig) -> Result<Self, String> {
        Self::start_with_server(config, None).await
    }

    pub async fn start_on_port(config: TunnelHarnessConfig, port: u16) -> Result<Self, String> {
        Self::start_with_server(config, Some(port)).await
    }

    async fn start_with_server(
        config: TunnelHarnessConfig,
        port: Option<u16>,
    ) -> Result<Self, String> {
        let state = TunnelRuntimeState::new(
            TunnelControlPlaneClient::disabled(),
            TunnelConnConfig {
                ping_interval: config.ping_interval,
                idle_timeout: Duration::from_secs(0),
                outbound_queue_capacity: config.outbound_queue_capacity,
            },
            config.max_streams,
        )
        .with_request_concurrency_limit(config.max_in_flight_requests);
        let state = if let Some(gate) = config.distributed_request_gate {
            state.with_distributed_request_gate(gate)
        } else {
            state
        };
        let state = aether_gateway::configure_test_tunnel_runtime_auth(
            state,
            &config.node_id,
            TUNNEL_HARNESS_GENERATION,
            TUNNEL_HARNESS_MANAGEMENT_TOKEN,
        )?;
        let router = build_tunnel_runtime_router_with_state(state);
        let server = match port {
            Some(port) => SpawnedServer::start_on_port(port, router)
                .await
                .map_err(|err| format!("failed to start tunnel harness: {err}"))?,
            None => SpawnedServer::start(router)
                .await
                .map_err(|err| format!("failed to start tunnel harness: {err}"))?,
        };
        Ok(Self { server })
    }

    pub fn base_url(&self) -> &str {
        self.server.base_url()
    }

    pub fn port(&self) -> u16 {
        self.server.port()
    }
}

pub fn insert_tunnel_harness_auth_headers(
    headers: &mut http::HeaderMap,
    node_id: &str,
) -> Result<(), http::header::InvalidHeaderValue> {
    headers.insert("x-node-id", http::HeaderValue::from_str(node_id)?);
    headers.insert(
        aether_contracts::tunnel_security::TUNNEL_GENERATION_HEADER,
        http::HeaderValue::from_static(TUNNEL_HARNESS_GENERATION),
    );
    headers.insert(
        http::header::AUTHORIZATION,
        http::HeaderValue::from_static(concat!("Bearer ", "ae-tunnel-harness-management-token")),
    );
    Ok(())
}
