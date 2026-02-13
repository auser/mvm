use std::net::SocketAddr;

use anyhow::{Context, Result};
use tracing::{debug, info};

use mvm_core::agent::{AgentRequest, AgentResponse};
use mvm_runtime::security::certs;

/// QUIC client for communicating with mvm agent nodes.
pub struct CoordinatorClient {
    endpoint: quinn::Endpoint,
}

impl CoordinatorClient {
    /// Create a new coordinator client using mTLS certificates.
    pub fn new() -> Result<Self> {
        let client_config = certs::load_client_config().with_context(
            || "Failed to load client TLS config. Run 'mvm agent certs init' first.",
        )?;

        let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)
            .with_context(|| "Failed to create QUIC client endpoint")?;
        endpoint.set_default_client_config(client_config);

        Ok(Self { endpoint })
    }

    /// Send a request to a node and receive a response.
    pub async fn send(&self, addr: SocketAddr, request: &AgentRequest) -> Result<AgentResponse> {
        let server_name = "mvm-node";

        debug!(addr = %addr, "Connecting to agent node");
        let connection = self
            .endpoint
            .connect(addr, server_name)
            .with_context(|| format!("Failed to initiate connection to {}", addr))?
            .await
            .with_context(|| format!("Failed to establish connection to {}", addr))?;

        let (mut send, mut recv) = connection
            .open_bi()
            .await
            .with_context(|| "Failed to open bi-directional stream")?;

        // Write request frame
        let request_bytes =
            serde_json::to_vec(request).with_context(|| "Failed to serialize request")?;
        let len = (request_bytes.len() as u32).to_be_bytes();
        send.write_all(&len).await?;
        send.write_all(&request_bytes).await?;
        send.finish()?;

        // Read response frame
        let mut len_buf = [0u8; 4];
        recv.read_exact(&mut len_buf)
            .await
            .with_context(|| "Failed to read response length")?;
        let resp_len = u32::from_be_bytes(len_buf) as usize;

        if resp_len > 1024 * 1024 {
            anyhow::bail!("Response too large: {} bytes", resp_len);
        }

        let mut buf = vec![0u8; resp_len];
        recv.read_exact(&mut buf)
            .await
            .with_context(|| "Failed to read response body")?;

        let response: AgentResponse =
            serde_json::from_slice(&buf).with_context(|| "Failed to parse response")?;

        connection.close(quinn::VarInt::from_u32(0), b"done");

        Ok(response)
    }

    /// Send requests to multiple nodes in parallel, collecting all responses.
    pub async fn send_multi(
        &self,
        targets: &[(SocketAddr, AgentRequest)],
    ) -> Vec<(SocketAddr, Result<AgentResponse>)> {
        let mut set = tokio::task::JoinSet::new();

        for (addr, req) in targets {
            let addr = *addr;
            let req = req.clone();
            let endpoint = self.endpoint.clone();

            set.spawn(async move {
                let client = CoordinatorClientRef { endpoint };
                let result = client.send_one(addr, &req).await;
                (addr, result)
            });
        }

        let mut results = Vec::new();
        while let Some(join_result) = set.join_next().await {
            match join_result {
                Ok((addr, result)) => results.push((addr, result)),
                Err(e) => {
                    info!(error = %e, "Multi-send task panicked");
                }
            }
        }
        results
    }
}

/// Internal helper for cloneable endpoint reference in async tasks.
struct CoordinatorClientRef {
    endpoint: quinn::Endpoint,
}

impl CoordinatorClientRef {
    async fn send_one(&self, addr: SocketAddr, request: &AgentRequest) -> Result<AgentResponse> {
        let server_name = "mvm-node";
        let connection = self
            .endpoint
            .connect(addr, server_name)?
            .await
            .with_context(|| format!("Failed to connect to {}", addr))?;

        let (mut send, mut recv) = connection.open_bi().await?;

        let request_bytes = serde_json::to_vec(request)?;
        let len = (request_bytes.len() as u32).to_be_bytes();
        send.write_all(&len).await?;
        send.write_all(&request_bytes).await?;
        send.finish()?;

        let mut len_buf = [0u8; 4];
        recv.read_exact(&mut len_buf).await?;
        let resp_len = u32::from_be_bytes(len_buf) as usize;

        if resp_len > 1024 * 1024 {
            anyhow::bail!("Response too large: {} bytes", resp_len);
        }

        let mut buf = vec![0u8; resp_len];
        recv.read_exact(&mut buf).await?;

        let response: AgentResponse = serde_json::from_slice(&buf)?;
        connection.close(quinn::VarInt::from_u32(0), b"done");

        Ok(response)
    }
}

/// Run a coordinator command using the tokio runtime.
pub fn run_coordinator_command<F, T>(f: F) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .with_context(|| "Failed to create tokio runtime")?;
    runtime.block_on(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_coordinator_client_ref_is_send() {
        // Ensure the client ref can be sent across threads
        fn assert_send<T: Send>() {}
        assert_send::<CoordinatorClientRef>();
    }

    #[test]
    fn test_agent_request_serialization_for_client() {
        let req = AgentRequest::NodeInfo;
        let bytes = serde_json::to_vec(&req).unwrap();
        assert!(!bytes.is_empty());
        let parsed: AgentRequest = serde_json::from_slice(&bytes).unwrap();
        assert!(matches!(parsed, AgentRequest::NodeInfo));
    }
}
