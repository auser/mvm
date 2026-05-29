//! mvm-egress-proxy — builder VM egress allowlist proxy
//! (Plan 73 Followup B.2.x, ADR-047 §"Build-time gates" →
//! "Registry allowlist").
//!
//! Library crate exposing the [`allowlist`] + [`proxy`] modules so
//! unit tests and downstream callers can drive the proxy without
//! shelling out to the binary. The binary at `src/main.rs` is a
//! thin wrapper that constructs an [`allowlist::Allowlist`],
//! binds the proxy with [`proxy::start`], and waits for SIGTERM.
//!
//! See `crates/mvm-host-vm-init/src/install.rs` for the consumer:
//! `run_install` spawns the proxy + sets `HTTPS_PROXY` /
//! `HTTP_PROXY` on the installer's env before invoking `uv` /
//! `pnpm`.

pub mod allowlist;
pub mod proxy;

pub use allowlist::{ALLOWED_PORT, Allowlist, PRODUCTION_HOSTNAMES};
pub use proxy::{DEFAULT_BIND, ProxyHandle, parse_connect_target, start};
