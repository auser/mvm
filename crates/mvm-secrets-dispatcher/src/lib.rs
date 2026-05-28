//! `mvm-secrets-dispatcher` — secrets subprocess (Plan 104 §H-L1, ADR-061).
//!
//! Same wire envelope shape as `mvm-broker` (host-bound `ServiceCall` →
//! guest-bound `ServiceResponse`). The only difference is the handler
//! set: this subprocess hosts `host.secrets.v1` exclusively (ADR-049's
//! destination-bound signed-credential issuer) while `mvm-broker` hosts
//! the multi-handler general broker.
//!
//! W1b.1 ships the scaffold + dispatch-loop skeleton + the
//! [`SubprocessConfig`](crate::config::SubprocessConfig) envelope.
//! `HostSecretsV1Handler` wires in via [`Registry::register`] in W5. Until
//! then every call returns `Err(NotBound)` — the W1a/W1b acceptance
//! criterion.
//!
//! Per-subprocess isolation knobs (seccomp `standard`, setpriv
//! `--bounding-set=-all --no-new-privs`, per-workload cgroup + namespace,
//! `RLIMIT_CORE=0`, `mlock` of the secret arena) are wired by the
//! supervisor at spawn time — W1b.2. The crate itself is intentionally
//! free of those concerns so the same scaffold compiles on macOS dev
//! hosts where seccomp doesn't apply.

pub mod config;
pub mod registry;
pub mod server;
