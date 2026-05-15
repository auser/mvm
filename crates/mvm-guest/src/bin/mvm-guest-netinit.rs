//! `mvm-guest-netinit` — guest-side network defense (Plan 74 W2).
//!
//! Run as PID >1, uid 0 inside every microVM at boot, before the
//! main `mvm-guest-agent` is forked under setpriv. Installs kernel
//! blackhole routes for `mvm_core::network_policy::MANDATORY_DENY_RANGES`
//! via rtnetlink — the defense layer that catches:
//!
//! - The macOS Apple Container path where `mvm` has no host firewall.
//! - Any backend where the host iptables/nftables rules don't apply.
//! - Legitimate uid-0 dev workloads that don't actively try to
//!   defeat the routes.
//!
//! ## Exit codes
//!
//! - 0 — every IPv4 entry installed successfully (or the only
//!   failures were on entries the kernel doesn't support, which is
//!   surfaced in the report's `failed` array with `reason` carrying
//!   the kernel message).
//! - 1 — one or more routes failed to install. `/init` should
//!   fail-closed and refuse to fork the workload.
//! - 2 — could not connect to rtnetlink (kernel built without
//!   `CONFIG_RTNETLINK`, or some other systemic failure). Same
//!   fail-closed behaviour at `/init`.
//!
//! ## Output
//!
//! Single JSON line to stdout containing the [`Report`] from
//! [`mvm_guest::netinit::install_mandatory_deny`]. The kernel
//! console captures stdout; the host scrape (`firecracker.log`,
//! libkrun console output) forwards the line so an operator can
//! see what was installed without an in-VM tool. A future slice
//! wires this into the agent's vsock-audit path.
//!
//! ## Platform
//!
//! Linux-only: the module gates on `#[cfg(target_os = "linux")]`.
//! On macOS the binary compiles to a stub that prints
//! "not supported on this host" and exits non-zero — the macOS
//! CLI build doesn't ship the bin, but cargo still builds the
//! workspace and we don't want a compilation break.
//!
//! [`Report`]: mvm_guest::netinit::Report

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() {
    let report = match mvm_guest::netinit::install_mandatory_deny_via_rtnetlink().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("mvm-guest-netinit: rtnetlink connect failed: {e}");
            // Exit 2 distinguishes systemic netlink failure from
            // per-route failures so `/init` can branch (the
            // systemic case usually means a kernel feature is
            // missing, not a transient install error).
            std::process::exit(2);
        }
    };

    // Write the report as a single JSON line to stdout. Kept on
    // one line so console scrape can read it without parsing
    // multi-line JSON; the field set is stable per the type's
    // serde shape.
    match serde_json::to_string(&report) {
        Ok(json) => println!("{json}"),
        Err(e) => {
            // serializing a `Report` from our own types shouldn't
            // fail in practice; if it does the binary still needs
            // to exit with a clear code so /init can react.
            eprintln!("mvm-guest-netinit: serialize report failed: {e}");
            std::process::exit(1);
        }
    }

    if report.has_failures() {
        // Exit 1: per-route failures recorded in the report. /init
        // reads the JSON to surface which entries failed.
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!(
        "mvm-guest-netinit: not supported on this host \
         (rtnetlink is Linux-only; this binary ships in the runtime \
         overlay for Linux microVM guests only)"
    );
    std::process::exit(2);
}
