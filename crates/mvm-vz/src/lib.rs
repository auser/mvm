//! Plan 97 Phase B foundation — type-safe interface to
//! `mvm-vz-supervisor`.
//!
//! The Vz backend (`mvm-backend::VzBackend`, lands in a follow-up
//! slice) constructs a [`SupervisorConfig`], serializes it to JSON,
//! and pipes it to the Swift supervisor binary on stdin. The Swift
//! side decodes against an equivalent `Codable` schema in
//! `crates/mvm-vz-supervisor/Sources/mvm-vz-supervisor/Config.swift`
//! with strict deny-unknown-fields semantics — ADR-002 claim 5 rests
//! on those two decoders rejecting the same inputs.
//!
//! Pure data + path resolution. No FFI, no Swift toolchain dep, no
//! Vz framework binding. This crate compiles on every host the
//! workspace targets, including Linux contributors who never touch
//! the Vz code path (`has_vz()` returns `false` there). The Swift
//! supervisor's actual build is gated on macOS via
//! `crates/mvm-vz-supervisor/tools/build.sh`.

use std::path::PathBuf;

// MARK: - Config types

/// JSON payload the host pipes to `mvm-vz-supervisor` on stdin.
///
/// The schema **must** stay in lockstep with the Swift `Config.swift`
/// decoder — both sides apply deny-unknown-fields. Adding a field
/// requires landing both edits in the same PR (and the Phase A
/// equivalence fuzz corpus catches drift in CI).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupervisorConfig {
    /// Human-readable VM name, surfaced in logs.
    pub name: String,
    /// Per-VM state directory the supervisor writes the PID file into
    /// and the supervisor binary creates if absent (mode 0700).
    /// Typically `~/.mvm/vms/<name>/`.
    pub vm_state_dir: String,
    /// PID file name inside `vm_state_dir`. Defaults to `vz.pid`
    /// supervisor-side when omitted; consumers should set it
    /// explicitly so multiple backends coexist in the same state dir.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid_file_name: Option<String>,
    pub kernel: KernelConfig,
    pub resources: ResourceConfig,
    /// virtio-blk devices, in declared order. The first entry appears
    /// as `/dev/vda` in the guest, the second as `/dev/vdb`, etc.
    pub disks: Vec<DiskConfig>,
    /// virtio-fs shares. Workload microVMs default to empty; the
    /// supervisor refuses to attach a share that the admitted
    /// `ExecutionPlan` (claim 8) does not name.
    pub virtio_fs: Vec<VirtioFsShare>,
    pub vsock: VsockConfig,
    /// Capture-only console output. Workload microVMs always set this;
    /// dev-mode PTY console goes via `vsock` ports 20000+ instead
    /// (Plan 97 Security §9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub console_output_path: Option<String>,
    /// Network attachment. `None` boots a no-network guest — useful
    /// for unit tests and the very smallest smoke configurations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkConfig>,
    /// Memory balloon (Plan 97 §"Memory balloon floor"). When `None`,
    /// the supervisor omits the balloon device entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub balloon: Option<BalloonConfig>,
    /// Plan 97 Phase E — unix-domain control socket the supervisor
    /// binds (`SOCK_STREAM`, mode 0700) to accept PAUSE / RESUME /
    /// BALLOON / SAVE / STATUS commands from the host. `None` opts
    /// out — the supervisor runs without a control channel and
    /// pause/resume/balloon/snapshot verbs on `VzBackend` short-circuit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_socket_path: Option<String>,
}

impl SupervisorConfig {
    /// Absolute path the supervisor will write its PID to, computed
    /// from `vm_state_dir` and `pid_file_name`. Useful for the host
    /// to read the PID after spawn without reaching into the Swift
    /// side's default-name logic.
    pub fn resolved_pid_file(&self) -> PathBuf {
        PathBuf::from(&self.vm_state_dir).join(self.pid_file_name.as_deref().unwrap_or("vz.pid"))
    }

    /// Serialize to JSON with sorted keys for stable hashing /
    /// auditing. Defers to `serde_json` which already produces
    /// insertion-order keys; callers that need canonical
    /// hashing should layer a canonicalizer on top.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelConfig {
    /// Path to an uncompressed `vmlinux` (Vz `VZLinuxBootLoader` boots
    /// only uncompressed kernels).
    pub path: String,
    /// Kernel command line. Plan 97 Security §7 — workload mode
    /// requires the host to pre-filter this against the admitted
    /// ExecutionPlan's allow-list before this struct is constructed.
    pub cmdline: String,
    /// Optional initial ramdisk path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initrd_path: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceConfig {
    /// vCPU count. Plan 97 Security §8 — supervisor refuses values
    /// above the admitted plan's cap. Host enforces; the Swift
    /// supervisor relays without further checking (defense in depth
    /// arrives in a follow-up).
    pub cpu_count: u32,
    /// Guest memory in MiB.
    pub memory_mib: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiskConfig {
    /// Stable identifier used in logs / audit. Not user-visible inside
    /// the guest.
    pub id: String,
    /// Host path to the disk image (raw ext4, sparse-allocated, per
    /// Plan 97 §"Disk image format").
    pub path: String,
    pub read_only: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VirtioFsShare {
    /// Symbolic mount tag, used by the guest in `mount -t virtiofs
    /// <tag> <target>`.
    pub tag: String,
    /// Host directory exported into the guest.
    pub host_path: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VsockConfig {
    /// Guest ports the host wants to dial via per-port unix sockets.
    /// Each port produces a `<socket_dir>/vsock-<port>.sock` listener
    /// on the host side.
    pub ports: Vec<u32>,
    /// Per-VM directory the supervisor creates mode 0700 and binds
    /// the per-port unix sockets inside.
    pub socket_dir: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum NetworkConfig {
    /// gvproxy-backed virtio-net. ADR-055 §"Cross-platform backends".
    Gvproxy {
        /// Path to gvproxy's `--listen-vfkit` SOCK_DGRAM unix socket.
        socket_path: String,
        /// Guest's eth0 MAC, formatted `AA:BB:CC:DD:EE:FF`. Validated
        /// against the locally-administered bit in [`MacAddress`].
        mac: String,
    },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BalloonConfig {
    pub enabled: bool,
    /// Minimum memory the balloon controller is allowed to reclaim
    /// the guest down to (host-side enforcement). Plan 97
    /// §"Memory balloon floor".
    pub floor_mib: u64,
}

// MARK: - MAC address (host-side validation helper)

/// 6-byte MAC address with the locally-administered bit (`0x02`) set
/// on the first octet. Constructing via [`MacAddress::parse`] enforces
/// the bit so we never collide with a real hardware allocation —
/// mirrors the Swift `MacAddress` invariant and the libkrun
/// supervisor's contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MacAddress([u8; 6]);

impl MacAddress {
    /// Parse an `"AA:BB:CC:DD:EE:FF"` string. Returns [`Error::InvalidMac`]
    /// on any of: wrong byte count, non-hex byte, first octet missing
    /// the locally-administered bit.
    pub fn parse(s: &str) -> Result<Self, Error> {
        let mut bytes = [0u8; 6];
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() != 6 {
            return Err(Error::InvalidMac(s.to_string()));
        }
        for (i, part) in parts.iter().enumerate() {
            if part.len() != 2 {
                return Err(Error::InvalidMac(s.to_string()));
            }
            bytes[i] =
                u8::from_str_radix(part, 16).map_err(|_| Error::InvalidMac(s.to_string()))?;
        }
        if bytes[0] & 0x02 == 0 {
            return Err(Error::InvalidMac(s.to_string()));
        }
        Ok(Self(bytes))
    }

    pub fn as_bytes(&self) -> [u8; 6] {
        self.0
    }

    /// Render as `"aa:bb:cc:dd:ee:ff"` (lowercase, colon-separated)
    /// for embedding in the JSON `network.mac` field.
    pub fn to_string_lowercase(&self) -> String {
        self.0
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":")
    }
}

// MARK: - Supervisor binary path resolution

/// Subdirectory under `~/.mvm/bin/` the supervisor lives in for
/// installed (release) layouts. Source-checkout builds use
/// [`source_tree_binary_path`] instead — see
/// [`supervisor_binary_path`] for the contract.
pub const INSTALLED_BIN_DIRNAME: &str = ".mvm/bin";

/// Filename prefix of the version-pinned supervisor binary. Plan 97
/// §"Build, distribution, versioning" — the host launches
/// `~/.mvm/bin/mvm-vz-supervisor-<mvmctl_version>` and refuses to run
/// against a mismatched version.
pub const SUPERVISOR_BIN_PREFIX: &str = "mvm-vz-supervisor-";

/// Resolve the release-installed supervisor path for the given mvmctl
/// version. The host then layers fallback / source-checkout selection
/// on top — this function deliberately knows nothing about source
/// trees so its output is fully deterministic from its inputs.
pub fn supervisor_binary_path(home: &std::path::Path, mvmctl_version: &str) -> PathBuf {
    home.join(INSTALLED_BIN_DIRNAME)
        .join(format!("{SUPERVISOR_BIN_PREFIX}{mvmctl_version}"))
}

/// Source-checkout layout: the Swift Package Manager build output
/// lives under `<workspace>/crates/mvm-vz-supervisor/.build/<arch>-apple-macosx/<config>/`.
/// CLAUDE.md "Source-checkout builds never depend on mvm-published
/// artifacts" — a contributor running `cargo run` from the workspace
/// must use whatever the local `tools/build.sh` produced, not the
/// `~/.mvm/bin/` release path.
pub fn source_tree_binary_path(workspace_root: &std::path::Path) -> PathBuf {
    let arch = current_arch_triple_macos();
    workspace_root
        .join("crates/mvm-vz-supervisor/.build")
        .join(arch)
        .join("debug")
        .join("mvm-vz-supervisor")
}

fn current_arch_triple_macos() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "arm64-apple-macosx"
    } else {
        "x86_64-apple-macosx"
    }
}

// MARK: - Errors

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// MAC address string failed to parse. Carries the offending input
    /// so the caller can surface it without re-stringifying.
    InvalidMac(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidMac(s) => write!(
                f,
                "invalid MAC address (expect AA:BB:CC:DD:EE:FF with locally-administered bit): {s}"
            ),
        }
    }
}

impl std::error::Error for Error {}

// MARK: - Tests

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_config() -> SupervisorConfig {
        SupervisorConfig {
            name: "smoke".into(),
            vm_state_dir: "/tmp/vz-smoke".into(),
            pid_file_name: Some("vz.pid".into()),
            kernel: KernelConfig {
                path: "/tmp/vmlinux".into(),
                cmdline: "console=hvc0 root=/dev/vda rw init=/init".into(),
                initrd_path: None,
            },
            resources: ResourceConfig {
                cpu_count: 1,
                memory_mib: 256,
            },
            disks: vec![DiskConfig {
                id: "rootfs".into(),
                path: "/tmp/rootfs.ext4".into(),
                read_only: true,
            }],
            virtio_fs: vec![],
            vsock: VsockConfig {
                ports: vec![5252],
                socket_dir: "/tmp/vz-smoke/vsock".into(),
            },
            console_output_path: None,
            network: None,
            balloon: None,
            control_socket_path: None,
        }
    }

    #[test]
    fn config_roundtrip() {
        let cfg = minimal_config();
        let json = cfg.to_json().expect("serialize");
        let back: SupervisorConfig = serde_json::from_str(&json).expect("roundtrip parses cleanly");
        assert_eq!(back.name, cfg.name);
        assert_eq!(back.disks.len(), 1);
        assert_eq!(back.vsock.ports, vec![5252]);
    }

    #[test]
    fn unknown_field_rejected() {
        let mut value = serde_json::to_value(minimal_config()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("rogue".into(), serde_json::json!(42));
        let json = serde_json::to_string(&value).unwrap();
        let err = serde_json::from_str::<SupervisorConfig>(&json)
            .expect_err("deny_unknown_fields should reject");
        assert!(
            err.to_string().contains("rogue"),
            "error mentions the unknown field: {err}"
        );
    }

    #[test]
    fn resolved_pid_file_uses_default_when_missing() {
        let mut cfg = minimal_config();
        cfg.pid_file_name = None;
        assert!(cfg.resolved_pid_file().ends_with("vz.pid"));
    }

    #[test]
    fn mac_locally_administered_bit_enforced() {
        // 0x02 set in the first octet — accepted.
        assert!(MacAddress::parse("02:00:00:00:00:01").is_ok());
        // Bit unset — rejected.
        assert!(MacAddress::parse("00:11:22:33:44:55").is_err());
        // Wrong number of octets.
        assert!(MacAddress::parse("02:00:00:00:00").is_err());
        // Non-hex byte.
        assert!(MacAddress::parse("zz:00:00:00:00:01").is_err());
    }

    #[test]
    fn mac_string_roundtrips_lowercase() {
        let mac = MacAddress::parse("0A:BB:CC:DD:EE:FF").unwrap();
        assert_eq!(mac.to_string_lowercase(), "0a:bb:cc:dd:ee:ff");
    }

    #[test]
    fn supervisor_binary_path_is_version_pinned() {
        let home = std::path::Path::new("/Users/x");
        let path = supervisor_binary_path(home, "0.14.0");
        assert_eq!(
            path.to_str().unwrap(),
            "/Users/x/.mvm/bin/mvm-vz-supervisor-0.14.0"
        );
    }

    #[test]
    fn network_serializes_with_kind_tag() {
        let cfg = NetworkConfig::Gvproxy {
            socket_path: "/tmp/gv.sock".into(),
            mac: "02:00:00:00:00:01".into(),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        // Tagged enum format: {"kind":"gvproxy","socket_path":...,"mac":...}.
        assert!(json.contains(r#""kind":"gvproxy""#), "json: {json}");
        assert!(
            json.contains(r#""socket_path":"/tmp/gv.sock""#),
            "json: {json}"
        );
    }
}
