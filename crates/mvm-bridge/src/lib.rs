//! Library surface for the per-VM gateway audit bridge sidecar
//! (Plan 113, ADR-064).
//!
//! The crate's primary product is the `mvm-bridge` binary
//! (`src/main.rs`). This `lib.rs` exposes the parser surface in
//! [`parse`] so the cargo-fuzz harness under `crates/mvm-bridge/fuzz/`
//! can drive adversarial input through the same code the binary
//! executes. The `endpoints` module carries the per-variant arm bodies
//! the binary dispatches to once the config is parsed.
//!
//! Plan 113 §Task 15 / `fuzz` CI lane —
//! `.github/workflows/security.yml` runs `cargo fuzz run` against the
//! `BridgeConfigJson` `serde_json` parser and the `PasstHashesFile`
//! `toml` parser nightly + on release-tag pushes.

pub mod endpoints;
pub mod parse;
