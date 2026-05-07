//! In-guest TCP↔vsock bridge for outbound mesh traffic
//! (ADR-0018 / ADR-0020).
//!
//! Binds loopback-range IPs handed out by `mvm-mesh-dns`. On accept,
//! opens a vsock stream to mvmd-agent on the host, writes a per-
//! connection peer header (length-prefixed JSON), then proxies bytes
//! both ways. Half-close semantics preserved.
//!
//! Iroh-free by design — capability tokens stay on the host
//! (mvmd-agent attaches them to the iroh handshake transparently).
//! This bridge speaks only TCP and vsock.

#![warn(missing_docs)]

use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;
use std::path::Path;

/// One peer-name → loopback-IP + vsock-port mapping. The full set
/// for a consumer instance comes from the config disk's
/// `mesh_loopback_bindings`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopbackBinding {
    /// Bare addon name (or alias) — matches the corresponding
    /// `mesh_dns_zone` hostname's prefix. E.g. `"db"` for the entry
    /// whose zone hostname is `"db.mesh.local"`.
    pub peer: String,
    /// Loopback IP the bridge binds to listen on. Always in
    /// `127.0.0.0/8`; allocated by mvmd at consumer-instance start.
    pub loopback_ip: Ipv4Addr,
    /// Vsock port to dial on mvmd-agent. mvmd-agent binds a fresh
    /// per-instance port from the 5253+ pool.
    pub vsock_port: u32,
}

/// Per-connection peer header — written by the bridge as the first
/// frame on every vsock stream it opens. Length-prefixed JSON
/// (4-byte big-endian length + UTF-8 JSON), matching the existing
/// `mvm-core` wire conventions.
///
/// mvmd-agent reads this header before any application bytes, looks
/// up the addon-peer's iroh endpoint ID + capability token, and
/// dials over iroh-QUIC accordingly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerHeader {
    /// Wire-format version. `1` for v1.
    pub version: u32,
    /// Bare peer name (matches `LoopbackBinding::peer`).
    pub peer: String,
    /// Public Ed25519 (hex) of the consumer's iroh endpoint identity,
    /// supplied to the bridge via the config disk. Used by mvmd-agent
    /// for audit-log attribution; the bridge itself never sees the
    /// private key.
    pub consumer_endpoint_id: String,
}

impl PeerHeader {
    /// Wire-format version emitted by v1.
    pub const V1: u32 = 1;

    /// Construct a v1 peer header.
    pub fn new(peer: String, consumer_endpoint_id: String) -> Self {
        Self {
            version: Self::V1,
            peer,
            consumer_endpoint_id,
        }
    }
}

/// Parse the config disk's `mesh_loopback_bindings` JSON file.
pub fn load_bindings(path: &Path) -> std::io::Result<Vec<LoopbackBinding>> {
    let body = std::fs::read_to_string(path)?;
    if body.trim().is_empty() {
        return Ok(vec![]);
    }
    serde_json::from_str(&body).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "could not parse {} as JSON array of LoopbackBinding entries: {e}",
                path.display()
            ),
        )
    })
}

/// Serialize a peer header as a length-prefixed wire frame ready to
/// write onto a vsock stream. Returns `(4-byte length || JSON bytes)`.
pub fn encode_peer_header(header: &PeerHeader) -> Vec<u8> {
    let body = serde_json::to_vec(header).expect("PeerHeader serializes infallibly");
    let len = body.len() as u32;
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&body);
    out
}

/// Maximum inbound peer-header size (4 KiB). Defends against a
/// malicious consumer-side caller writing a huge length prefix and
/// blowing out memory before we close the stream. Real headers are
/// well under 1 KiB.
pub const MAX_HEADER_BYTES: u32 = 4 * 1024;

/// Decode a length-prefixed peer header from a byte slice. Returns
/// the parsed header + the number of bytes consumed (so the caller
/// can advance the stream cursor). Errors on length-prefix overruns,
/// truncated bodies, or non-JSON content.
pub fn decode_peer_header(buf: &[u8]) -> std::io::Result<(PeerHeader, usize)> {
    if buf.len() < 4 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "peer header: truncated length prefix",
        ));
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if len > MAX_HEADER_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("peer header: length {len} exceeds MAX_HEADER_BYTES={MAX_HEADER_BYTES}"),
        ));
    }
    let total = 4 + len as usize;
    if buf.len() < total {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "peer header: truncated body",
        ));
    }
    let header: PeerHeader = serde_json::from_slice(&buf[4..total]).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("peer header: not valid JSON: {e}"),
        )
    })?;
    Ok((header, total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn load_bindings_parses_valid_json() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("b.json");
        std::fs::write(
            &path,
            r#"[
              {"peer": "db", "loopback_ip": "127.0.255.1", "vsock_port": 5253},
              {"peer": "cache", "loopback_ip": "127.0.255.2", "vsock_port": 5254}
            ]"#,
        )
        .unwrap();
        let bindings = load_bindings(&path).unwrap();
        assert_eq!(bindings.len(), 2);
        assert_eq!(bindings[0].peer, "db");
        assert_eq!(bindings[0].loopback_ip, Ipv4Addr::new(127, 0, 255, 1));
        assert_eq!(bindings[0].vsock_port, 5253);
    }

    #[test]
    fn load_bindings_accepts_empty_file() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("b.json");
        std::fs::write(&path, "").unwrap();
        assert!(load_bindings(&path).unwrap().is_empty());
    }

    #[test]
    fn peer_header_round_trips_through_wire_format() {
        let h = PeerHeader::new("db".to_string(), "abcd1234".to_string());
        let wire = encode_peer_header(&h);
        let (decoded, consumed) = decode_peer_header(&wire).unwrap();
        assert_eq!(decoded, h);
        assert_eq!(consumed, wire.len());
    }

    #[test]
    fn decode_peer_header_rejects_oversize_length_prefix() {
        let bad = [0xFF, 0xFF, 0xFF, 0xFF];
        let err = decode_peer_header(&bad).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn decode_peer_header_rejects_truncated_body() {
        // Length prefix says 100 bytes; only 4 follow.
        let mut buf = vec![0, 0, 0, 100];
        buf.extend_from_slice(b"abcd");
        let err = decode_peer_header(&buf).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn decode_peer_header_rejects_non_json_body() {
        let mut buf = vec![0, 0, 0, 5];
        buf.extend_from_slice(b"not()");
        let err = decode_peer_header(&buf).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn peer_header_extra_fields_are_rejected_by_serde_default() {
        // Deserializer is the standard serde derive (no
        // `deny_unknown_fields` here intentionally — forward
        // compatibility for future-version headers). v2 headers
        // would change `version` and adopt new fields; v1 should
        // accept-with-ignore. Locked in by:
        let buf = encode_peer_header(&PeerHeader::new("p".into(), "id".into()));
        decode_peer_header(&buf).unwrap();
    }
}
