// Plan 113 §Task 15 / ADR-064 — fuzz the host-side `BridgeConfigJson`
// JSON parser.
//
// `mvm-bridge`'s `main()` reads a `BridgeConfigJson` document on stdin
// before any libkrun / Firecracker / Vz / Landlock call. The bytes
// come from a pipe `mvm-backend`'s spawner writes — same-uid trust,
// but the parser is the entry point for the bridge's lifetime. A
// panic here turns into a hard process death before confinement is
// applied, before the FlowEvent pipeline is up, and before the
// parent's watchdog has a sentinel to react to.
//
// Analog of `crates/mvm-libkrun/fuzz/fuzz_targets/fuzz_supervisor_config.rs`
// and `crates/mvm-guest/fuzz/fuzz_targets/fuzz_guest_request.rs`. The
// harness goal is "never panic on any input"; `serde_json::Error` is
// the expected outcome for malformed bytes.
//
// `BridgeConfigJson` + `EndpointSpec` carry `#[serde(deny_unknown_fields)]`
// (Plan 113 — see `crates/mvm-bridge/src/parse.rs`) so unknown /
// attacker-controlled keys fail-closed during deserialization.

#![no_main]

use libfuzzer_sys::fuzz_target;
use mvm_bridge::parse::BridgeConfigJson;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = serde_json::from_str::<BridgeConfigJson>(s);
    }
});
