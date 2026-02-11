use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Default vsock guest CID (Firecracker convention).
pub const GUEST_CID: u32 = 3;

/// Port the guest vsock agent listens on.
pub const GUEST_AGENT_PORT: u32 = 52;

/// Default connect/read timeout in seconds.
pub const DEFAULT_TIMEOUT_SECS: u64 = 10;

/// Maximum response frame size (256 KiB).
const MAX_FRAME_SIZE: usize = 256 * 1024;

// ============================================================================
// Guest agent protocol (JSON over vsock)
// ============================================================================

/// Request sent from host to guest vsock agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GuestRequest {
    /// Query current worker status.
    WorkerStatus,
    /// Request sleep preparation. Guest should:
    /// 1. Finish/checkpoint in-flight OpenClaw work
    /// 2. Flush data to disk
    /// 3. Drop page cache
    /// 4. ACK with SleepPrepAck
    SleepPrep { drain_timeout_secs: u64 },
    /// Signal wake — guest should reinitialize connections and refresh secrets.
    Wake,
    /// Health probe.
    Ping,
}

/// Response from guest vsock agent to host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GuestResponse {
    /// Worker status with optional last-busy timestamp.
    WorkerStatus {
        status: String,
        last_busy_at: Option<String>,
    },
    /// Sleep preparation acknowledgement.
    SleepPrepAck {
        success: bool,
        detail: Option<String>,
    },
    /// Wake acknowledgement.
    WakeAck { success: bool },
    /// Pong.
    Pong,
    /// Error from guest agent.
    Error { message: String },
}

// ============================================================================
// Vsock UDS connection
// ============================================================================

/// Path to the Firecracker vsock UDS for an instance.
pub fn vsock_uds_path(instance_dir: &str) -> String {
    format!("{}/runtime/v.sock", instance_dir)
}

/// Connect to the guest vsock agent via Firecracker's vsock UDS proxy.
///
/// Firecracker exposes guest vsock as a Unix domain socket. The connect protocol:
/// 1. Open Unix stream to `<inst_dir>/runtime/v.sock`
/// 2. Write `CONNECT <port>\n`
/// 3. Read `OK <port>\n`
/// 4. Then use length-prefixed JSON frames
fn connect(instance_dir: &str, timeout_secs: u64) -> Result<UnixStream> {
    let uds_path = vsock_uds_path(instance_dir);
    let timeout = Duration::from_secs(timeout_secs);

    let stream = UnixStream::connect(&uds_path)
        .with_context(|| format!("Failed to connect to vsock UDS at {}", uds_path))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    // Firecracker vsock connect handshake
    let mut stream = stream;
    writeln!(stream, "CONNECT {}", GUEST_AGENT_PORT).with_context(|| "Failed to send CONNECT")?;
    stream.flush()?;

    // Read response line: "OK <port>\n"
    let mut reader = BufReader::new(&stream);
    let mut response_line = String::new();
    reader
        .read_line(&mut response_line)
        .with_context(|| "Failed to read CONNECT response")?;

    if !response_line.starts_with("OK ") {
        bail!(
            "Vsock CONNECT failed: expected 'OK {}', got '{}'",
            GUEST_AGENT_PORT,
            response_line.trim()
        );
    }

    Ok(stream)
}

/// Send a request and receive a response over a vsock connection.
///
/// Uses 4-byte big-endian length prefix + JSON body (same pattern as hostd).
fn send_request(stream: &mut UnixStream, req: &GuestRequest) -> Result<GuestResponse> {
    let data = serde_json::to_vec(req).with_context(|| "Failed to serialize request")?;

    // Write length-prefixed frame
    let len = (data.len() as u32).to_be_bytes();
    stream
        .write_all(&len)
        .with_context(|| "Failed to write frame length")?;
    stream
        .write_all(&data)
        .with_context(|| "Failed to write frame body")?;
    stream.flush()?;

    // Read response length
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .with_context(|| "Failed to read response length")?;
    let resp_len = u32::from_be_bytes(len_buf) as usize;

    if resp_len > MAX_FRAME_SIZE {
        bail!(
            "Response frame too large: {} bytes (max {})",
            resp_len,
            MAX_FRAME_SIZE
        );
    }

    // Read response body
    let mut buf = vec![0u8; resp_len];
    stream
        .read_exact(&mut buf)
        .with_context(|| "Failed to read response body")?;

    serde_json::from_slice(&buf).with_context(|| "Failed to deserialize response")
}

// ============================================================================
// High-level API
// ============================================================================

/// Query worker status from the guest vsock agent.
pub fn query_worker_status(instance_dir: &str) -> Result<GuestResponse> {
    let mut stream = connect(instance_dir, DEFAULT_TIMEOUT_SECS)?;
    send_request(&mut stream, &GuestRequest::WorkerStatus)
}

/// Request sleep preparation via vsock.
///
/// Returns Ok(true) if guest ACKed (OpenClaw idle, data flushed),
/// Ok(false) if guest NAKed or timed out.
pub fn request_sleep_prep(instance_dir: &str, drain_timeout_secs: u64) -> Result<bool> {
    let mut stream = connect(instance_dir, drain_timeout_secs)?;
    let resp = send_request(&mut stream, &GuestRequest::SleepPrep { drain_timeout_secs })?;

    match resp {
        GuestResponse::SleepPrepAck { success, .. } => Ok(success),
        GuestResponse::Error { message } => {
            bail!("Guest sleep prep error: {}", message);
        }
        _ => bail!("Unexpected response to SleepPrep"),
    }
}

/// Signal wake to the guest vsock agent.
///
/// Returns Ok(true) if guest ACKed (connections reinitialized, secrets refreshed),
/// Ok(false) if guest NAKed.
pub fn signal_wake(instance_dir: &str) -> Result<bool> {
    let mut stream = connect(instance_dir, DEFAULT_TIMEOUT_SECS)?;
    let resp = send_request(&mut stream, &GuestRequest::Wake)?;

    match resp {
        GuestResponse::WakeAck { success } => Ok(success),
        GuestResponse::Error { message } => {
            bail!("Guest wake error: {}", message);
        }
        _ => bail!("Unexpected response to Wake"),
    }
}

/// Ping the guest vsock agent (health check).
pub fn ping(instance_dir: &str) -> Result<bool> {
    let mut stream = connect(instance_dir, DEFAULT_TIMEOUT_SECS)?;
    let resp = send_request(&mut stream, &GuestRequest::Ping)?;
    Ok(matches!(resp, GuestResponse::Pong))
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_guest_request_roundtrip() {
        let variants: Vec<GuestRequest> = vec![
            GuestRequest::WorkerStatus,
            GuestRequest::SleepPrep {
                drain_timeout_secs: 30,
            },
            GuestRequest::Wake,
            GuestRequest::Ping,
        ];

        for req in &variants {
            let json = serde_json::to_string(req).unwrap();
            let parsed: GuestRequest = serde_json::from_str(&json).unwrap();
            // Verify round-trip produces valid JSON
            let json2 = serde_json::to_string(&parsed).unwrap();
            assert_eq!(json, json2);
        }
    }

    #[test]
    fn test_guest_response_roundtrip() {
        let variants: Vec<GuestResponse> = vec![
            GuestResponse::WorkerStatus {
                status: "idle".to_string(),
                last_busy_at: Some("2025-01-01T00:00:00Z".to_string()),
            },
            GuestResponse::SleepPrepAck {
                success: true,
                detail: Some("flushed".to_string()),
            },
            GuestResponse::WakeAck { success: true },
            GuestResponse::Pong,
            GuestResponse::Error {
                message: "oops".to_string(),
            },
        ];

        for resp in &variants {
            let json = serde_json::to_string(resp).unwrap();
            let parsed: GuestResponse = serde_json::from_str(&json).unwrap();
            let json2 = serde_json::to_string(&parsed).unwrap();
            assert_eq!(json, json2);
        }
    }

    #[test]
    fn test_vsock_uds_path() {
        assert_eq!(
            vsock_uds_path("/var/lib/mvm/tenants/acme/pools/workers/instances/i-abc"),
            "/var/lib/mvm/tenants/acme/pools/workers/instances/i-abc/runtime/v.sock"
        );
    }

    #[test]
    fn test_guest_request_sleep_prep_fields() {
        let req = GuestRequest::SleepPrep {
            drain_timeout_secs: 45,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("45"));
        assert!(json.contains("SleepPrep"));
    }

    #[test]
    fn test_guest_response_worker_status_fields() {
        let resp = GuestResponse::WorkerStatus {
            status: "busy".to_string(),
            last_busy_at: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"status\":\"busy\""));
    }

    #[test]
    fn test_constants() {
        assert_eq!(GUEST_CID, 3);
        assert_eq!(GUEST_AGENT_PORT, 52);
        assert_eq!(DEFAULT_TIMEOUT_SECS, 10);
    }

    #[test]
    fn test_max_frame_size() {
        assert_eq!(MAX_FRAME_SIZE, 256 * 1024);
    }
}
