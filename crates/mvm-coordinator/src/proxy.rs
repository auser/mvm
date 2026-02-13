use std::net::SocketAddr;

use anyhow::{Context, Result};
use tokio::io;
use tokio::net::TcpStream;
use tracing::{debug, info};

/// Proxy a TCP connection bidirectionally between the client and the gateway VM.
///
/// This is a Layer 4 proxy — no application-layer inspection. Bytes flow
/// verbatim in both directions until either side closes the connection.
pub async fn proxy_connection(
    mut client: TcpStream,
    gateway_addr: SocketAddr,
    tenant_id: &str,
) -> Result<()> {
    debug!(
        gateway = %gateway_addr,
        tenant = %tenant_id,
        "Connecting to gateway"
    );

    let mut upstream = TcpStream::connect(gateway_addr)
        .await
        .with_context(|| format!("Failed to connect to gateway at {}", gateway_addr))?;

    let (bytes_client_to_gw, bytes_gw_to_client) =
        io::copy_bidirectional(&mut client, &mut upstream)
            .await
            .with_context(|| "Proxy connection error")?;

    info!(
        tenant = %tenant_id,
        sent = bytes_client_to_gw,
        received = bytes_gw_to_client,
        "Connection closed"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn test_proxy_forwards_data() {
        // Set up a mock "gateway" server
        let mock_gw = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gw_addr = mock_gw.local_addr().unwrap();

        // Mock gateway echoes back whatever it receives
        tokio::spawn(async move {
            let (mut stream, _) = mock_gw.accept().await.unwrap();
            let mut buf = vec![0u8; 1024];
            let n = stream.read(&mut buf).await.unwrap();
            stream.write_all(&buf[..n]).await.unwrap();
            stream.shutdown().await.unwrap();
        });

        // Set up a mock "client"
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();

        // Spawn the proxy
        tokio::spawn(async move {
            let (stream, _) = proxy_listener.accept().await.unwrap();
            proxy_connection(stream, gw_addr, "test-tenant")
                .await
                .unwrap();
        });

        // Connect as client and send data
        let mut client = TcpStream::connect(proxy_addr).await.unwrap();
        client.write_all(b"hello gateway").await.unwrap();
        client.shutdown().await.unwrap();

        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert_eq!(response, b"hello gateway");
    }
}
