//! Plan 60 Phase 3 Slice C — host-side firewall enforcement.
//!
//! The firewall is **additive enforcement** beneath the proxy: even
//! if the L4/L7 proxies' allow-list is misconfigured, the firewall's
//! default-deny on the runtime-VM TAP keeps stray packets from
//! escaping to the host's other interfaces. Defense in depth.
//!
//! ## Platforms
//!
//! - **Linux** (`linux_nft`) — `nftables`-based rules. Available
//!   today.
//! - **macOS** (`macos_pf`) — `pfctl` shell-out. Deferred to
//!   Slice E per the plan-60 §"Phase 3 risk: pf and WFP shell-outs
//!   are fragile" note.
//! - **Windows** (`windows_wfp`) — WFP via `windivert`. Deferred
//!   to Slice E.
//!
//! Each platform ships its own rule-formatter + apply function;
//! the operator-facing surface stays uniform — pass the per-VM
//! TAP interface name + the proxy endpoint, get a fail-closed
//! firewall.

#[cfg(target_os = "linux")]
pub mod linux_nft;
