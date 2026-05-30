//! Parser surface for `mvm-bridge` (Plan 113, ADR-064).
//!
//! Extracted from `src/main.rs` so the cargo-fuzz harness under
//! `crates/mvm-bridge/fuzz/` (Plan 113 §Task 15) can call the same
//! serde deserializers and the same hash-verify helper the binary uses
//! at startup. The binary's `main()` imports [`BridgeConfigJson`],
//! [`EndpointSpec`], [`PasstHashesFile`], and [`verify_passt_hash`]
//! from this module verbatim — there is no parser duplication.
//!
//! [`BridgeConfigJson`] carries an [`EndpointSpec`] discriminator that
//! tells the binary which backend arm to run:
//!
//! * `EndpointSpec::Passt` — Linux/passt path (Firecracker today,
//!   future Cloud Hypervisor). Carries the operator-pinned `passt`
//!   binary + hashes + the parent-inherited socketpair fds.
//! * `EndpointSpec::VzIngest` — Apple Vz NDJSON ingest path. Carries
//!   the Swift `mvm-vz-supervisor` events socket path.
//!
//! All deserializer shapes carry `#[serde(deny_unknown_fields)]` so a
//! malicious or merely sloppy producer can never inject an
//! attacker-controlled field that a future schema bump would
//! interpret. The fuzz target's contract is "no panic on any input,"
//! mirroring `fuzz_supervisor_config` / `fuzz_guest_request` (ADR-002
//! claim 5).

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Stdin JSON contract. Producer is one of `mvm-backend`'s spawners
/// (`microvm.rs::spawn_mvm_bridge` for the Firecracker path,
/// `vz.rs::spawn_mvm_bridge` for the Vz path). All paths are absolute
/// and already-canonicalised by the parent.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeConfigJson {
    /// VM name; used to label the bridge thread + the audit chain
    /// `vm` field.
    pub vm_name: String,

    /// `~/.mvm/audit/` — destination of the chain-signed JSONL log
    /// that `FileAuditSigner` appends to. Shared with sibling VMs;
    /// the cross-process flock inside `FileAuditSigner` serialises
    /// writes per tenant.
    pub audit_dir: PathBuf,

    /// `~/.mvm/audit/gateway-<vm>.sock` — subscriber socket the
    /// bridge binds at startup so `nc -U <path>` consumers see the
    /// live NDJSON flow-event tail.
    pub audit_socket: PathBuf,

    /// `~/.mvm/keys/host-signer.ed25519` — mode 0600 file owned by
    /// the calling user. The bridge re-reads it on each launch to
    /// seed the `FileAuditSigner`'s `SigningKey`.
    pub signing_key_path: PathBuf,

    /// Serialised `SignedExecutionPlan` envelope as produced by
    /// `mvm-cli::plan_admission::populate_audit_substrate`. Trust
    /// model in the module doc: the bridge parses the inner
    /// `ExecutionPlan` body directly without re-verifying the
    /// envelope.
    pub plan_json: String,

    /// Optional serialised `PolicyBundle` (the resolved bundle pin
    /// rather than the bundle archive itself; the bridge uses it
    /// to label flow-event audit entries with the bundle digest).
    #[serde(default)]
    pub bundle_json: Option<String>,

    /// Backend-specific endpoint variant. See [`EndpointSpec`].
    pub endpoints: EndpointSpec,
}

/// Discriminated union over the backend-specific tail of the bridge
/// config. Tagged on `kind`:
///
/// * `"passt"` — Linux/passt socketpair endpoints (Firecracker today;
///   Cloud Hypervisor in the future). The `mvm-jailer-lite`
///   confinement spec + `passt`-hash pin live in this arm.
/// * `"vz_ingest"` — Apple Vz NDJSON ingest socket. No confinement;
///   the Swift `mvm-vz-supervisor` owns the socket lifecycle.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EndpointSpec {
    /// Linux passt-backed leaf. Carried by Firecracker today; reusable
    /// by any future Linux backend that fronts a virtio-net socketpair
    /// with passt (e.g. Cloud Hypervisor). Runtime-Linux-only — the
    /// dispatch in `main()` bails on non-Linux targets.
    Passt {
        /// Path to the operator-installed `passt` binary the bridge
        /// relays packets to. The bridge verifies its SHA256 against
        /// `passt_hashes_path` before installing confinement and
        /// before touching the inherited fds.
        passt_path: PathBuf,

        /// `~/.mvm/passt-hashes.toml` — operator-curated allowlist of
        /// accepted `passt` binary SHA256 hashes. See
        /// [`PasstHashesFile`].
        passt_hashes_path: PathBuf,

        /// `~/.mvm/keys/` — directory containing the host signer
        /// key. Used to scope the Landlock confinement spec; the
        /// actual secret bytes are read from
        /// [`BridgeConfigJson::signing_key_path`] before
        /// confinement clamps the bridge.
        keys_dir: PathBuf,

        /// Raw fd number of the parent half of the passt socketpair.
        /// `mvm-backend`'s `pre_exec` `dup2`s the parent socketpair
        /// fd into this slot and clears `O_CLOEXEC` so the kernel
        /// preserves the fd across exec. The bridge takes ownership
        /// via `OwnedFd::from_raw_fd`.
        gateway_fd_raw: i32,

        /// Raw fd number of the supervisor half of the inner
        /// virtio-net socketpair whose other half is plumbed into
        /// the hypervisor. Same `pre_exec` contract as
        /// `gateway_fd_raw`.
        supervisor_fd_raw: i32,
    },
    /// macOS Vz NDJSON ingest leaf. Carries only the path the Swift
    /// `mvm-vz-supervisor` writes its NDJSON `FlowEventWire` stream
    /// to. The bridge binds this socket (mode 0700) and reads from
    /// each accepted connection.
    VzIngest {
        /// Path the Swift `mvm-vz-supervisor` writes its NDJSON
        /// `FlowEventWire` stream to.
        events_socket_path: PathBuf,
    },
}

/// On-disk format of `~/.mvm/passt-hashes.toml`. Operator-managed —
/// the `passt` binary is operator-installed (apt / dnf / nix /
/// source build), so the SHA256 is operator-pinned per-host. CI
/// fixtures use `tempfile` to construct a valid file; never falls
/// back to a default-trust posture.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PasstHashesFile {
    /// Lowercase hex SHA256 strings (64 hex chars each, no `0x`
    /// prefix). An empty list is a hard failure — the bridge cannot
    /// admit any `passt` binary because there is nothing to match
    /// against.
    pub sha256: Vec<String>,
}

/// Verify the `passt` binary at `passt_path` SHA256-matches one of
/// the entries in `hashes_path`. Defence in depth (Cardoso minimum-
/// viable-policy): the operator-pinned allowlist is checked *before*
/// the bridge installs Landlock + seccomp, so the error path can
/// still read the binary and produce a clean remediation hint.
///
/// Fails closed on:
///   * `hashes_path` missing or unreadable
///   * `hashes_path` parses but `sha256 = []` (empty allowlist)
///   * `passt_path` missing or unreadable
///   * computed SHA256 not present in the allowlist
///
/// All failure messages include the offending path and a concrete
/// remediation hint (the exact `sha256sum` command to pin the
/// in-use binary).
pub fn verify_passt_hash(passt_path: &Path, hashes_path: &Path) -> Result<()> {
    let raw = std::fs::read_to_string(hashes_path).with_context(|| {
        format!(
            "read passt-hashes allowlist at {} (operator must pre-populate \
             this file with `sha256 = [\"<sha256sum {}>\", ...]` before \
             starting the bridge)",
            hashes_path.display(),
            passt_path.display(),
        )
    })?;
    let parsed: PasstHashesFile = toml::from_str(&raw)
        .with_context(|| format!("parse passt-hashes TOML at {}", hashes_path.display()))?;
    if parsed.sha256.is_empty() {
        return Err(anyhow!(
            "passt-hashes allowlist at {} contains zero SHA256 entries; \
             fail-closed. Populate with `sha256 = [\"$(sha256sum {} | \
             cut -d' ' -f1)\"]`.",
            hashes_path.display(),
            passt_path.display(),
        ));
    }

    let mut hasher = Sha256::new();
    let mut f = std::fs::File::open(passt_path)
        .with_context(|| format!("read passt binary at {} for SHA256", passt_path.display()))?;
    let mut buf = [0u8; 8192];
    loop {
        let n = f
            .read(&mut buf)
            .with_context(|| format!("read passt binary at {} for SHA256", passt_path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let computed = hex_encode(&hasher.finalize());

    let mut accepted = false;
    for entry in &parsed.sha256 {
        if entry.eq_ignore_ascii_case(&computed) {
            accepted = true;
            break;
        }
    }
    if !accepted {
        return Err(anyhow!(
            "passt binary {} SHA256 mismatch: computed {}, allowlist {} \
             admits {:?}. Add the computed hash with `sha256 += [\"{}\"]` \
             if this binary is trusted (verify with `sha256sum {}`).",
            passt_path.display(),
            computed,
            hashes_path.display(),
            parsed.sha256,
            computed,
            passt_path.display(),
        ));
    }
    Ok(())
}

/// Lowercase hex encoder. Local instead of pulling `hex` as a dep
/// since this is the only consumer in this crate; matches the
/// `sha256sum` output format byte-for-byte.
pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a tiny passt-like binary fixture under a tempdir and a
    /// matching `passt-hashes.toml`. Returns both paths.
    fn fixture(passt_bytes: &[u8], hashes_toml: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let passt_path = dir.path().join("passt");
        let hashes_path = dir.path().join("passt-hashes.toml");
        {
            let mut f = std::fs::File::create(&passt_path).expect("create passt fixture");
            f.write_all(passt_bytes).expect("write passt fixture");
        }
        std::fs::write(&hashes_path, hashes_toml).expect("write hashes file");
        (dir, passt_path, hashes_path)
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        hex_encode(&h.finalize())
    }

    #[test]
    fn verify_passt_hash_accepts_pinned_hash() {
        let body = b"#!/bin/sh\necho fake-passt\n";
        let hash = sha256_hex(body);
        let toml = format!("sha256 = [\"{hash}\"]\n");
        let (_dir, passt, hashes) = fixture(body, &toml);
        verify_passt_hash(&passt, &hashes).expect("hash matches allowlist");
    }

    #[test]
    fn verify_passt_hash_accepts_uppercase_pinned_hash() {
        let body = b"#!/bin/sh\necho fake-passt\n";
        let hash = sha256_hex(body).to_uppercase();
        let toml = format!("sha256 = [\"{hash}\"]\n");
        let (_dir, passt, hashes) = fixture(body, &toml);
        verify_passt_hash(&passt, &hashes)
            .expect("hash compare is case-insensitive (sha256sum output is lowercase by convention but operators sometimes paste uppercase)");
    }

    #[test]
    fn verify_passt_hash_rejects_wrong_hash() {
        let body = b"#!/bin/sh\necho fake-passt\n";
        let wrong = "0".repeat(64);
        let toml = format!("sha256 = [\"{wrong}\"]\n");
        let (_dir, passt, hashes) = fixture(body, &toml);
        let err = verify_passt_hash(&passt, &hashes).expect_err("wrong hash must reject");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("SHA256 mismatch"),
            "error must name mismatch: {msg}"
        );
        assert!(
            msg.contains(&sha256_hex(body)),
            "error must include computed hash so operator can pin it: {msg}"
        );
        assert!(
            msg.contains(passt.to_str().unwrap()),
            "error must include passt path: {msg}"
        );
    }

    #[test]
    fn verify_passt_hash_rejects_empty_allowlist() {
        let body = b"#!/bin/sh\necho fake-passt\n";
        let (_dir, passt, hashes) = fixture(body, "sha256 = []\n");
        let err = verify_passt_hash(&passt, &hashes).expect_err("empty allowlist must fail closed");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("zero SHA256 entries"),
            "error must explain empty allowlist: {msg}"
        );
        assert!(
            msg.contains("sha256sum"),
            "error must give a remediation hint: {msg}"
        );
    }

    #[test]
    fn verify_passt_hash_rejects_missing_hashes_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let passt = dir.path().join("passt");
        std::fs::write(&passt, b"binary").expect("write passt");
        let hashes = dir.path().join("does-not-exist.toml");
        let err =
            verify_passt_hash(&passt, &hashes).expect_err("missing hashes file must fail closed");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("read passt-hashes allowlist"),
            "error must name the missing file: {msg}"
        );
        assert!(
            msg.contains("pre-populate"),
            "error must give remediation hint: {msg}"
        );
    }

    #[test]
    fn verify_passt_hash_rejects_missing_passt_binary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let passt = dir.path().join("nonexistent-passt");
        let hashes = dir.path().join("passt-hashes.toml");
        std::fs::write(&hashes, "sha256 = [\"deadbeef\"]\n").expect("write hashes");
        let err =
            verify_passt_hash(&passt, &hashes).expect_err("missing passt binary must fail closed");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("read passt binary"),
            "error must name the missing binary path: {msg}"
        );
    }

    #[test]
    fn verify_passt_hash_rejects_malformed_toml() {
        let body = b"binary";
        let (_dir, passt, hashes) = fixture(body, "this is not toml = = =\n");
        let err = verify_passt_hash(&passt, &hashes).expect_err("malformed TOML must reject");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("parse passt-hashes TOML"),
            "error must name the parse failure: {msg}"
        );
    }

    #[test]
    fn verify_passt_hash_rejects_unknown_field_in_hashes_file() {
        let body = b"binary";
        let toml = "sha256 = [\"deadbeef\"]\nattacker_injected = \"value\"\n";
        let (_dir, passt, hashes) = fixture(body, toml);
        let err = verify_passt_hash(&passt, &hashes)
            .expect_err("unknown TOML field must reject (deny_unknown_fields)");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("parse passt-hashes TOML") || msg.contains("unknown field"),
            "error must name the parse rejection: {msg}"
        );
    }

    #[test]
    fn hex_encode_matches_sha256sum_format() {
        // sha256sum emits lowercase hex, no `0x`, no separators. Our
        // encoder must match byte-for-byte.
        let h = sha256_hex(b"hello");
        assert_eq!(
            h,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    // ─── Discriminator round-trip ───────────────────────────────────

    /// Sanity that the `kind` discriminator round-trips for the
    /// passt arm — every per-arm field is required, unknown fields
    /// reject.
    #[test]
    fn endpoint_spec_passt_round_trip() {
        let json = serde_json::json!({
            "kind": "passt",
            "passt_path": "/usr/bin/passt",
            "passt_hashes_path": "/home/me/.mvm/passt-hashes.toml",
            "keys_dir": "/home/me/.mvm/keys",
            "gateway_fd_raw": 7,
            "supervisor_fd_raw": 8,
        });
        let parsed: EndpointSpec = serde_json::from_value(json).expect("valid passt variant");
        match parsed {
            EndpointSpec::Passt {
                passt_path,
                gateway_fd_raw,
                supervisor_fd_raw,
                ..
            } => {
                assert_eq!(passt_path.to_str(), Some("/usr/bin/passt"));
                assert_eq!(gateway_fd_raw, 7);
                assert_eq!(supervisor_fd_raw, 8);
            }
            EndpointSpec::VzIngest { .. } => panic!("expected passt arm"),
        }
    }

    #[test]
    fn endpoint_spec_vz_ingest_round_trip() {
        let json = serde_json::json!({
            "kind": "vz_ingest",
            "events_socket_path": "/home/me/.mvm/vms/foo/vz-events.sock",
        });
        let parsed: EndpointSpec = serde_json::from_value(json).expect("valid vz_ingest variant");
        match parsed {
            EndpointSpec::VzIngest { events_socket_path } => {
                assert_eq!(
                    events_socket_path.to_str(),
                    Some("/home/me/.mvm/vms/foo/vz-events.sock"),
                );
            }
            EndpointSpec::Passt { .. } => panic!("expected vz_ingest arm"),
        }
    }

    #[test]
    fn endpoint_spec_unknown_kind_rejects() {
        let json = serde_json::json!({
            "kind": "attacker_invented_kind",
            "events_socket_path": "/x",
        });
        let err =
            serde_json::from_value::<EndpointSpec>(json).expect_err("unknown kind must reject");
        let msg = err.to_string();
        assert!(
            msg.contains("attacker_invented_kind") || msg.contains("variant"),
            "error must name the bad variant: {msg}"
        );
    }

    #[test]
    fn endpoint_spec_unknown_field_rejects() {
        // deny_unknown_fields on a tagged enum applies per-variant.
        let json = serde_json::json!({
            "kind": "vz_ingest",
            "events_socket_path": "/x",
            "attacker_injected": "value",
        });
        let err = serde_json::from_value::<EndpointSpec>(json)
            .expect_err("unknown field must reject under deny_unknown_fields");
        let msg = err.to_string();
        assert!(
            msg.contains("attacker_injected") || msg.contains("unknown field"),
            "error must name the injected field: {msg}"
        );
    }

    /// Full envelope round-trip with the passt variant nested.
    #[test]
    fn bridge_config_json_with_passt_round_trip() {
        let json = serde_json::json!({
            "vm_name": "smoke",
            "audit_dir": "/home/me/.mvm/audit",
            "audit_socket": "/home/me/.mvm/audit/gateway-smoke.sock",
            "signing_key_path": "/home/me/.mvm/keys/host-signer.ed25519",
            "plan_json": "{}",
            "endpoints": {
                "kind": "passt",
                "passt_path": "/usr/bin/passt",
                "passt_hashes_path": "/home/me/.mvm/passt-hashes.toml",
                "keys_dir": "/home/me/.mvm/keys",
                "gateway_fd_raw": 7,
                "supervisor_fd_raw": 8,
            },
        });
        let cfg: BridgeConfigJson = serde_json::from_value(json).expect("valid envelope");
        assert_eq!(cfg.vm_name, "smoke");
        match cfg.endpoints {
            EndpointSpec::Passt { gateway_fd_raw, .. } => assert_eq!(gateway_fd_raw, 7),
            EndpointSpec::VzIngest { .. } => panic!("expected passt"),
        }
    }
}
