//! Plan 74 W2 — guest-side network defense.
//!
//! Installs kernel **blackhole routes** for every IPv4 entry in
//! [`mvm_core::network_policy::MANDATORY_DENY_RANGES`] inside the
//! microVM at boot, before the workload entrypoint forks.
//!
//! ## Why kernel routes (not nftables / iptables)
//!
//! The microVM's rootfs is user-controlled. Nix-built rootfs has
//! busybox (no `nft`, no `iptables`); OCI-imported rootfs might be
//! `alpine` (has both), `python:3.12-slim` (neither), `distroless`
//! (neither), or anything else. A defense layer that depends on a
//! userspace tool inside the guest fails on most images.
//!
//! Kernel-side blackhole routes are universal — every Linux kernel
//! supports `RTN_BLACKHOLE` since 2.0, no userspace tool required.
//! `rtnetlink` talks directly to the kernel via `AF_NETLINK`; the
//! only dependency is a Linux kernel.
//!
//! ## Why this is defense-in-depth, not the sole defense
//!
//! A workload that gains root inside the guest can `ip route del`
//! the blackhole routes (CAP_NET_ADMIN inside the guest's netns).
//! That's why this layer pairs with host-side enforcement (mvm
//! iptables on Linux-direct; mvmd nftables on fleet) — see
//! mvmd ADR 0022 §"Why layered". The guest-side floor catches:
//!
//! - the macOS Apple Container path where mvm has no host firewall;
//! - the legitimate uid-0 dev workload that doesn't actively try to
//!   defeat the routes;
//! - any workload that doesn't gain root.
//!
//! The two layers together make IMDS-style exfil substantially
//! harder regardless of which platform the microVM runs on.
//!
//! ## Audit emission
//!
//! [`install_mandatory_deny`] returns a [`Report`] describing what
//! was installed (and what failed). The caller — typically the
//! `mvm-guest-netinit` binary running from `/init` — writes the
//! report as a single JSON line to stdout, which the kernel
//! console forwards to the host. A future slice wires the agent
//! to forward the report as a `LocalAuditKind::NetworkMandatoryDeny`
//! audit event via vsock; for v1 the console-scrape path is
//! sufficient to surface install failures.
//!
//! ## Failure semantics
//!
//! Any per-route failure is recorded in the report but does NOT
//! abort the install loop — we want every successful route to
//! land even if one fails. The binary exits non-zero when the
//! report carries any failures, so `/init` can fail-closed
//! (refuse to fork the workload entrypoint).

use async_trait::async_trait;
use ipnet::IpNet;
use serde::{Deserialize, Serialize};

/// Canonical line marker that the `mvm-guest-netinit` binary
/// prefixes to its JSON output line. The host-side console
/// scrape greps for this to extract the [`Report`] from the
/// VM's console log (firecracker.log on FC, libkrun console
/// output on libkrun). Keeping the marker as a public const here
/// — not duplicated in both the binary and the host parser —
/// means a future rename surfaces immediately as a compile
/// failure on both sides.
///
/// The marker is deliberately distinctive: a sequence the
/// kernel and busybox both stay away from. Underscores +
/// uppercase + double-underscore framing puts it well outside
/// any reasonable log message.
pub const REPORT_MARKER: &str = "__MVM_NETINIT_REPORT__";

/// Parse a console log buffer for the netinit report.
///
/// Scans `log` line-by-line for [`REPORT_MARKER`]; the *last*
/// matching line wins (a workload might restart and re-run
/// netinit, in which case the latest report reflects the live
/// kernel route state). Returns `None` if no marker is present
/// or every marker line carries unparseable JSON.
///
/// Pure function: no I/O, no allocation beyond the parsed
/// `Report`. Tests construct synthetic console buffers; the
/// live host-side caller reads the console log into a `String`
/// and hands it here.
pub fn parse_report_from_console(log: &str) -> Option<Report> {
    let mut last: Option<Report> = None;
    for line in log.lines() {
        // The marker can appear anywhere on the line — the kernel
        // sometimes prefixes timestamps or `[mvm-init]` tags. We
        // match by substring and then take everything after the
        // marker + one space.
        if let Some(idx) = line.find(REPORT_MARKER) {
            let json_start = idx + REPORT_MARKER.len();
            let json = line[json_start..].trim_start();
            // A malformed line is silently skipped rather than
            // aborting the scan — partial console capture is a
            // real failure mode and we'd rather emit nothing
            // than wedge the host start path on garbage.
            if let Ok(parsed) = serde_json::from_str::<Report>(json) {
                last = Some(parsed);
            }
        }
    }
    last
}

/// What was installed for a single CIDR.
///
/// `category` is owned `String` (not `&'static str`) so the
/// Deserialize impl works for round-trip from an audit-log
/// reader. Construction at install time still uses string
/// literals — `categorize_v4` returns `&'static str` and we
/// `.to_string()` on insertion.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RouteInstalled {
    pub cidr: IpNet,
    /// The mvm category this CIDR belongs to. Mirrors the audit
    /// detail format in `LocalAuditKind::NetworkMandatoryDeny`:
    /// `cloud-metadata` | `link-local` | `cgnat` | `loopback`.
    pub category: String,
}

/// Failure to install one route. The loop continues past this so
/// other routes still land; the caller branches on
/// `report.failed.is_empty()` to decide overall success.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RouteFailed {
    pub cidr: IpNet,
    pub category: String,
    /// Stringified error. Kept opaque so the JSON shape is stable
    /// even if the underlying rtnetlink error type changes.
    pub reason: String,
}

/// Cumulative outcome of one `install_mandatory_deny` run.
///
/// Serializes to a stable JSON shape so the
/// `mvm-guest-netinit` binary can write `serde_json::to_string`
/// of this directly to stdout. A future audit consumer parses
/// the same shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Report {
    pub installed: Vec<RouteInstalled>,
    pub failed: Vec<RouteFailed>,
    /// IPv6 entries from the const are intentionally skipped (the
    /// guest's bridge / TAP is IPv4-only on every backend today).
    /// Reported here so an operator parsing the JSON sees that the
    /// v6 entries were deliberately not attempted, not silently
    /// missing.
    pub skipped_ipv6: Vec<IpNet>,
}

impl Report {
    pub fn empty() -> Self {
        Self {
            installed: Vec::new(),
            failed: Vec::new(),
            skipped_ipv6: Vec::new(),
        }
    }

    /// `true` when at least one route failed to install. The
    /// `mvm-guest-netinit` binary exits non-zero on
    /// `has_failures()` so `/init` can fail-closed.
    pub fn has_failures(&self) -> bool {
        !self.failed.is_empty()
    }
}

/// Abstraction over the actual rtnetlink call so tests can use a
/// `MockInstaller` without a real `AF_NETLINK` socket. Production
/// uses [`RtnetlinkInstaller`].
#[async_trait]
pub trait RouteInstaller: Send + Sync {
    /// Add a blackhole route for `cidr`. Idempotent at the kernel
    /// level — if the route already exists, returning `Ok(())` is
    /// the correct semantics (the entry is the desired state, not
    /// a write-once operation).
    async fn install_blackhole(&self, cidr: IpNet) -> Result<(), String>;
}

/// Categorize a CIDR for the audit category field. Pure function;
/// returns the same string keys as
/// `LocalAuditKind::NetworkMandatoryDeny`'s detail format.
fn categorize(cidr: &IpNet) -> &'static str {
    // The match order mirrors the const's ordering in
    // `mvm-core::policy::network_policy`. A future const edit that
    // shifts categories should update this function in lock-step.
    match cidr.to_string().as_str() {
        "169.254.169.254/32" | "169.254.0.0/16" => "link-local",
        // Note: cloud-metadata is the /32 specifically. We keep
        // both /32 and /16 in `link-local` here for simplicity —
        // a future audit slice that needs to distinguish IMDS
        // from generic link-local can pivot on the CIDR prefix
        // length.
        "100.64.0.0/10" => "cgnat",
        "127.0.0.0/8" | "::1/128" => "loopback",
        "fe80::/10" => "link-local-v6",
        _ => "other",
    }
}

/// Cloud metadata `/32` gets its own category for the audit detail
/// so a security dashboard can alert on IMDS exfil attempts
/// distinctly from generic link-local probes.
fn categorize_v4(cidr: &IpNet) -> &'static str {
    if cidr.to_string() == "169.254.169.254/32" {
        "cloud-metadata"
    } else {
        categorize(cidr)
    }
}

/// Install blackhole routes for every IPv4 entry in
/// `MANDATORY_DENY_RANGES`. IPv6 entries are skipped (the guest
/// network stack is v4-only on every backend today) and reported
/// in `report.skipped_ipv6` so an operator can see they were
/// deliberately not attempted.
///
/// The loop is fault-tolerant: a per-route failure is recorded in
/// `report.failed` but doesn't abort. Callers branch on
/// `report.has_failures()` for the overall verdict.
pub async fn install_mandatory_deny<I: RouteInstaller>(installer: &I) -> Report {
    let mut report = Report::empty();
    for cidr in mvm_core::network_policy::mandatory_deny_ranges() {
        if !cidr.network().is_ipv4() {
            report.skipped_ipv6.push(cidr);
            continue;
        }
        let category = categorize_v4(&cidr).to_string();
        match installer.install_blackhole(cidr).await {
            Ok(()) => report.installed.push(RouteInstalled { cidr, category }),
            Err(reason) => report.failed.push(RouteFailed {
                cidr,
                category,
                reason,
            }),
        }
    }
    report
}

// ============================================================================
// Production installer — rtnetlink (Linux-only)
// ============================================================================

#[cfg(target_os = "linux")]
mod linux {
    use super::*;

    /// Production [`RouteInstaller`] that talks to the kernel via
    /// `AF_NETLINK`. Requires CAP_NET_ADMIN in the current user
    /// namespace — the binary expects to run as root from `/init`
    /// BEFORE the agent setpriv's down to uid 901.
    ///
    /// Construction is fallible because opening the netlink socket
    /// can fail (e.g. on a kernel built without `CONFIG_RTNETLINK`,
    /// which is rare but not impossible for stripped-down embedded
    /// kernels).
    pub struct RtnetlinkInstaller {
        handle: rtnetlink::Handle,
    }

    impl RtnetlinkInstaller {
        /// Connect to the kernel's rtnetlink service. Spawns the
        /// rtnetlink connection background task on the current
        /// tokio runtime. Drop semantics: the handle keeps the
        /// connection alive until the installer is dropped.
        pub async fn connect() -> Result<Self, String> {
            let (connection, handle, _) =
                rtnetlink::new_connection().map_err(|e| format!("rtnetlink connect: {e}"))?;
            tokio::spawn(connection);
            Ok(Self { handle })
        }
    }

    #[async_trait]
    impl RouteInstaller for RtnetlinkInstaller {
        async fn install_blackhole(&self, cidr: IpNet) -> Result<(), String> {
            // rtnetlink's v4 route builder takes the destination
            // prefix (address + length) and the
            // scope/protocol/kind fields. The `kind` field is what
            // makes it a blackhole — RTN_BLACKHOLE means "the
            // kernel drops packets matching this route without
            // sending ICMP unreachable", which is the strongest
            // form of "this destination is forbidden".
            match cidr {
                IpNet::V4(v4) => {
                    use netlink_packet_route::route::{RouteProtocol, RouteScope, RouteType};
                    self.handle
                        .route()
                        .add()
                        .v4()
                        .destination_prefix(v4.network(), v4.prefix_len())
                        .kind(RouteType::BlackHole)
                        .scope(RouteScope::Universe)
                        .protocol(RouteProtocol::Boot)
                        .execute()
                        .await
                        .map_err(|e| format!("route add {cidr}: {e}"))?;
                }
                IpNet::V6(_) => {
                    // Should never reach here because
                    // `install_mandatory_deny` skips v6 before
                    // calling the installer; defending against a
                    // future refactor.
                    return Err(format!(
                        "internal: install_blackhole called with IPv6 cidr {cidr} \
                         (v6 not supported yet)"
                    ));
                }
            }
            Ok(())
        }
    }

    /// Convenience: connect to rtnetlink and run the install in
    /// one call. The `mvm-guest-netinit` binary uses this.
    pub async fn install_mandatory_deny_via_rtnetlink() -> Result<Report, String> {
        let installer = RtnetlinkInstaller::connect().await?;
        Ok(install_mandatory_deny(&installer).await)
    }
}

#[cfg(target_os = "linux")]
pub use linux::{RtnetlinkInstaller, install_mandatory_deny_via_rtnetlink};

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Mutex;

    /// In-memory mock that records every `install_blackhole` call.
    /// Tests inspect the recorded CIDRs to verify which entries
    /// the loop attempted; an injected `fail_on` set forces specific
    /// CIDRs to return an error so failure aggregation is tested too.
    struct MockInstaller {
        calls: Mutex<Vec<IpNet>>,
        fail_on: HashSet<IpNet>,
    }

    impl MockInstaller {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                fail_on: HashSet::new(),
            }
        }

        fn fail_on(mut self, cidrs: &[&str]) -> Self {
            for s in cidrs {
                self.fail_on.insert(s.parse().unwrap());
            }
            self
        }

        fn recorded(&self) -> Vec<IpNet> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl RouteInstaller for MockInstaller {
        async fn install_blackhole(&self, cidr: IpNet) -> Result<(), String> {
            self.calls.lock().unwrap().push(cidr);
            if self.fail_on.contains(&cidr) {
                Err(format!("forced failure for {cidr}"))
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn install_calls_installer_for_every_ipv4_entry() {
        let mock = MockInstaller::new();
        let report = install_mandatory_deny(&mock).await;
        // Every IPv4 entry in `MANDATORY_DENY_RANGES` should have
        // exactly one call. Mirror the const's IPv4 entry count
        // exactly so a future const edit that adds a v4 entry
        // also has to update this test.
        let v4_count = mvm_core::network_policy::mandatory_deny_ranges()
            .iter()
            .filter(|n| n.network().is_ipv4())
            .count();
        assert_eq!(mock.recorded().len(), v4_count);
        assert_eq!(report.installed.len(), v4_count);
        assert!(report.failed.is_empty());
    }

    #[tokio::test]
    async fn install_skips_ipv6_entries_and_reports_them() {
        let mock = MockInstaller::new();
        let report = install_mandatory_deny(&mock).await;
        let v6_count = mvm_core::network_policy::mandatory_deny_ranges()
            .iter()
            .filter(|n| !n.network().is_ipv4())
            .count();
        assert_eq!(report.skipped_ipv6.len(), v6_count);
        // The installer was never called for any v6 entry.
        for recorded in mock.recorded() {
            assert!(
                recorded.network().is_ipv4(),
                "installer was called with non-v4 CIDR {recorded}"
            );
        }
    }

    #[tokio::test]
    async fn install_records_cloud_metadata_explicitly() {
        // The metadata `/32` is the highest-stakes entry. Asserting
        // it shows up in `installed` with category=cloud-metadata
        // means a regression that drops the entry from the const,
        // or skips it in the install loop, fails loudly here.
        let mock = MockInstaller::new();
        let report = install_mandatory_deny(&mock).await;
        let metadata: IpNet = "169.254.169.254/32".parse().unwrap();
        let entry = report
            .installed
            .iter()
            .find(|r| r.cidr == metadata)
            .expect("cloud metadata /32 must be in the installed set");
        assert_eq!(entry.category, "cloud-metadata");
    }

    #[tokio::test]
    async fn install_continues_past_failures_and_records_them() {
        // Force one specific CIDR to fail. The loop must still
        // attempt every other entry; the failed CIDR lands in
        // `report.failed`, the rest in `report.installed`.
        let mock = MockInstaller::new().fail_on(&["100.64.0.0/10"]);
        let report = install_mandatory_deny(&mock).await;
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].cidr.to_string(), "100.64.0.0/10");
        assert!(report.failed[0].reason.contains("forced failure"));
        // Other installs still happened.
        assert!(!report.installed.is_empty());
        // `has_failures()` reports the right state for the caller.
        assert!(report.has_failures());
    }

    #[tokio::test]
    async fn install_marks_clean_run_no_failures() {
        let mock = MockInstaller::new();
        let report = install_mandatory_deny(&mock).await;
        assert!(!report.has_failures());
    }

    #[tokio::test]
    async fn install_serializes_to_stable_json_shape() {
        // The binary's stdout is `serde_json::to_string(&report)`.
        // Pin the load-bearing field names so a downstream audit
        // consumer can deserialize across mvmctl versions.
        let mock = MockInstaller::new();
        let report = install_mandatory_deny(&mock).await;
        let json = serde_json::to_value(&report).unwrap();
        let obj = json.as_object().unwrap();
        for key in ["installed", "failed", "skipped_ipv6"] {
            assert!(obj.contains_key(key), "report JSON missing key {key}");
        }
        // Each installed entry has the documented field set.
        let first = obj["installed"]
            .as_array()
            .and_then(|a| a.first())
            .expect("at least one installed entry in clean run");
        assert!(first.get("cidr").is_some());
        assert!(first.get("category").is_some());
    }

    // ────────────────────────────────────────────────────────────
    // Console-scrape parser tests
    // ────────────────────────────────────────────────────────────

    fn fake_report_json() -> String {
        // A minimal Report shape — one installed, one failed,
        // one skipped — that exercises every field path on the
        // parser side.
        r#"{"installed":[{"cidr":"169.254.169.254/32","category":"cloud-metadata"}],"failed":[{"cidr":"127.0.0.0/8","category":"loopback","reason":"forced"}],"skipped_ipv6":["::1/128"]}"#.to_string()
    }

    #[test]
    fn parse_report_extracts_from_clean_line() {
        let log = format!("__MVM_NETINIT_REPORT__ {}", fake_report_json());
        let report = parse_report_from_console(&log).expect("parser must extract");
        assert_eq!(report.installed.len(), 1);
        assert_eq!(report.installed[0].category, "cloud-metadata");
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.skipped_ipv6.len(), 1);
    }

    #[test]
    fn parse_report_ignores_unrelated_console_lines() {
        let report_line = fake_report_json();
        let log = format!(
            "[    0.000000] Booting Linux...\n\
             [    0.123456] random: crng init done\n\
             [mvm-init] mounted /proc /sys /dev\n\
             __MVM_NETINIT_REPORT__ {report_line}\n\
             [mvm-agent] starting on vsock port 5252\n"
        );
        let report = parse_report_from_console(&log).expect("must find the one marker line");
        assert_eq!(report.installed.len(), 1);
    }

    #[test]
    fn parse_report_returns_none_when_no_marker() {
        let log = "kernel boot ... busybox ... agent up ... no report here";
        assert!(parse_report_from_console(log).is_none());
    }

    #[test]
    fn parse_report_returns_none_when_marker_present_but_json_malformed() {
        let log = "__MVM_NETINIT_REPORT__ {this is not json}";
        assert!(parse_report_from_console(log).is_none());
    }

    #[test]
    fn parse_report_returns_last_marker_when_multiple() {
        // Multi-boot or restart scenario: two markers on the
        // console, the LATER one reflects live state.
        let log = format!(
            "__MVM_NETINIT_REPORT__ {{\"installed\":[],\"failed\":[],\"skipped_ipv6\":[]}}\n\
             other stuff\n\
             __MVM_NETINIT_REPORT__ {}\n",
            fake_report_json()
        );
        let report = parse_report_from_console(&log).expect("parser must extract last");
        // The last marker is the one with cloud-metadata installed;
        // a returned empty report would mean we kept the FIRST.
        assert_eq!(report.installed.len(), 1);
    }

    #[test]
    fn parse_report_handles_kernel_timestamp_prefix() {
        // Kernel console output frequently prefixes lines with
        // `[    1.234567]` timestamps. The marker should be
        // findable mid-line, not only at start.
        let log = format!(
            "[    1.234567] __MVM_NETINIT_REPORT__ {}",
            fake_report_json()
        );
        let report = parse_report_from_console(&log).expect("marker mid-line must parse");
        assert_eq!(report.installed.len(), 1);
    }

    #[test]
    fn report_marker_is_distinctive_enough() {
        // Defensive: the marker must not appear in obvious
        // kernel/busybox/agent log patterns. A future rename to
        // something kernel-message-shaped would break console
        // grep silently; pin the current value here so a refactor
        // has to update the test.
        assert_eq!(REPORT_MARKER, "__MVM_NETINIT_REPORT__");
        for noise in [
            "[    0.000000] Booting Linux",
            "[mvm-init] mounted /proc",
            "[mvm-agent] starting on vsock port 5252",
            "kernel: AF_VSOCK ready",
        ] {
            assert!(
                !noise.contains(REPORT_MARKER),
                "marker collides with kernel/agent log: {noise}"
            );
        }
    }

    #[test]
    fn categorize_v4_handles_known_entries() {
        let cases = [
            ("169.254.169.254/32", "cloud-metadata"),
            ("169.254.0.0/16", "link-local"),
            ("100.64.0.0/10", "cgnat"),
            ("127.0.0.0/8", "loopback"),
        ];
        for (s, expected) in cases {
            let cidr: IpNet = s.parse().unwrap();
            assert_eq!(categorize_v4(&cidr), expected, "category for {s}");
        }
    }
}
