use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

use mvm_core::protocol::{self, HOSTD_SOCKET_PATH, HostdRequest, HostdResponse};

/// Client for communicating with mvm-hostd over Unix domain socket.
///
/// Used by agentd (unprivileged) to request privileged operations from hostd.
/// Connects lazily on first request and reconnects on failure.
pub struct HostdClient {
    socket_path: String,
}

impl Default for HostdClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HostdClient {
    /// Create a new client targeting the default socket path.
    pub fn new() -> Self {
        Self {
            socket_path: HOSTD_SOCKET_PATH.to_string(),
        }
    }

    /// Create a new client targeting a custom socket path.
    pub fn with_socket(path: &str) -> Self {
        Self {
            socket_path: path.to_string(),
        }
    }

    /// Send a request to hostd and wait for the response.
    ///
    /// Opens a new connection per request (simple, reliable).
    /// Hostd handles one request per connection.
    pub async fn send(&self, req: &HostdRequest) -> Result<HostdResponse> {
        let stream = UnixStream::connect(&self.socket_path)
            .await
            .with_context(|| format!("Failed to connect to hostd at {}", self.socket_path))?;

        let (mut reader, mut writer) = stream.into_split();

        protocol::send_request(&mut writer, req).await?;

        // Shutdown write half to signal we're done sending
        writer
            .shutdown()
            .await
            .with_context(|| "Failed to shutdown write half")?;

        protocol::recv_response(&mut reader).await
    }

    /// Send a request synchronously (blocking wrapper for use in non-async code).
    pub fn send_sync(&self, req: &HostdRequest) -> Result<HostdResponse> {
        let rt = tokio::runtime::Handle::try_current();
        match rt {
            Ok(_handle) => {
                // We're inside a tokio runtime — use spawn_blocking to avoid nesting
                let req = req.clone();
                let path = self.socket_path.clone();
                std::thread::scope(|s| {
                    s.spawn(|| {
                        let rt = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .unwrap();
                        rt.block_on(async {
                            let client = HostdClient::with_socket(&path);
                            client.send(&req).await
                        })
                    })
                    .join()
                    .unwrap()
                })
            }
            Err(_) => {
                // Not inside a tokio runtime — create one
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .with_context(|| "Failed to create tokio runtime for hostd client")?;
                rt.block_on(self.send(req))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_default_socket() {
        let client = HostdClient::new();
        assert_eq!(client.socket_path, HOSTD_SOCKET_PATH);
    }

    #[test]
    fn test_client_custom_socket() {
        let client = HostdClient::with_socket("/tmp/test.sock");
        assert_eq!(client.socket_path, "/tmp/test.sock");
    }
}
