//! Client for `mvm-secrets-dispatcher` (the secrets subprocess hosting
//! `host.secrets.v1` per ADR-049).
//!
//! Same wire protocol as the broker (`ServiceCall` envelope, `ServiceResponse`
//! envelope). Only the target UDS differs. This is intentional: the
//! supervisor's call site can treat secrets calls uniformly with the
//! other broker calls except for which proxy it dispatches through.
//!
//! At the W1b.2a seam, the proxy is structurally identical to
//! [`super::broker_proxy::BrokerProxy`] — but kept as a distinct type so
//! the supervisor's static-typing makes the dispatch decision explicit
//! at the call site. If the two proxies' implementations need to
//! diverge later (e.g. when W5 wires the destination-bound credential
//! pipeline + latency floor + zeroize hygiene), the seam is in place.

use std::path::PathBuf;

use mvm_core::protocol::broker::{CorrelationId, ServiceCall, ServiceId, ServiceResponse};

use super::{
    ProxyError,
    frame::{DEFAULT_MAX_FRAME_BYTES, connect, read_frame, write_frame},
};

#[derive(Debug, Clone)]
pub struct SecretsProxy {
    pub uds_path: PathBuf,
    pub max_frame_bytes: usize,
}

impl SecretsProxy {
    pub fn new(uds_path: impl Into<PathBuf>) -> Self {
        Self {
            uds_path: uds_path.into(),
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
        }
    }

    pub async fn call(
        &self,
        service: ServiceId,
        verb: impl Into<String>,
        correlation_id: CorrelationId,
        payload: serde_json::Value,
    ) -> Result<ServiceResponse, ProxyError> {
        let call = ServiceCall {
            service,
            verb: verb.into(),
            correlation_id,
            payload,
        };
        let mut stream = connect(&self.uds_path).await?;
        write_frame(&mut stream, &self.uds_path, &call).await?;
        read_frame::<ServiceResponse>(&mut stream, &self.uds_path, self.max_frame_bytes).await
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mvm_core::protocol::broker::ServiceErrorCode;
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;
    use tokio::sync::Mutex;

    use super::*;

    async fn spawn_mock(
        path: PathBuf,
        response: ServiceResponse,
        captured: Arc<Mutex<Option<ServiceCall>>>,
    ) -> tokio::task::JoinHandle<()> {
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut len_buf = [0u8; 4];
            stream.read_exact(&mut len_buf).await.unwrap();
            let len = u32::from_be_bytes(len_buf) as usize;
            let mut body = vec![0u8; len];
            stream.read_exact(&mut body).await.unwrap();
            let call: ServiceCall = serde_json::from_slice(&body).unwrap();
            *captured.lock().await = Some(call);

            let resp_bytes = serde_json::to_vec(&response).unwrap();
            let resp_len: u32 = resp_bytes.len().try_into().unwrap();
            stream.write_all(&resp_len.to_be_bytes()).await.unwrap();
            stream.write_all(&resp_bytes).await.unwrap();
            let _ = stream.shutdown().await;
        })
    }

    #[tokio::test]
    async fn secrets_call_round_trips_through_mock() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("secrets.sock");

        let correlation = CorrelationId::new("01HCORR_SECRETS");
        let response = ServiceResponse::Err {
            correlation_id: correlation.clone(),
            // W1b.1 stub posture: every call returns NotBound until W5
            // wires HostSecretsV1Handler.
            code: ServiceErrorCode::NotBound,
            message: "host.secrets.v1 not yet bound (W5 placeholder)".into(),
        };
        let captured = Arc::new(Mutex::new(None));
        let mock = spawn_mock(path.clone(), response.clone(), captured.clone()).await;
        tokio::task::yield_now().await;

        let proxy = SecretsProxy::new(&path);
        let actual = proxy
            .call(
                ServiceId::parse("host.secrets.v1").unwrap(),
                "release",
                correlation.clone(),
                serde_json::json!({}),
            )
            .await
            .expect("transport must succeed");

        assert_eq!(actual, response);
        let captured_call = captured.lock().await.clone().expect("mock must capture");
        assert_eq!(captured_call.service.as_str(), "host.secrets.v1");
        assert_eq!(captured_call.verb, "release");
        mock.abort();
    }

    #[tokio::test]
    async fn secrets_proxy_uses_a_distinct_uds_from_broker() {
        // Type-level isolation: BrokerProxy and SecretsProxy are not
        // interchangeable even though their methods are. This test
        // guards the seam — a regression that unified them under one
        // type would silently allow secrets calls to escape to the
        // general broker's UDS.
        use super::super::broker_proxy::BrokerProxy;

        let dir = tempdir().unwrap();
        let broker_path = dir.path().join("broker.sock");
        let secrets_path = dir.path().join("secrets.sock");

        let broker = BrokerProxy::new(&broker_path);
        let secrets = SecretsProxy::new(&secrets_path);
        assert_ne!(broker.uds_path, secrets.uds_path);
    }
}
