// swift-tools-version: 5.9
//
// Plan 97 — Phase A. Swift package for the `mvm-vz-supervisor` binary:
// the host-side per-VM subprocess that wraps Apple's
// Virtualization.framework (Vz) for one Linux guest, takes a JSON
// SupervisorConfig on stdin, and blocks until the guest exits.
//
// Mirrors the architectural pattern of `mvm-libkrun-supervisor`: one
// supervisor process per VM, stdin-JSON-driven, PID file under
// `vm_state_dir`. Differences live in the Vz device set (no TSI mode,
// snapshot API gated on macOS 14+, file-handle network attachments
// instead of unixstream).
//
// Phase A only. Backend wiring (Phase B) and builder-VM mode (Phase C)
// live elsewhere; this package owns the supervisor binary alone.
//
// License: Apache-2.0 OR MIT (matches the Rust workspace).

import PackageDescription

let package = Package(
    name: "mvm-vz-supervisor",
    platforms: [
        // macOS 13 (Ventura) is the floor per Plan 97 §"Minimum macOS
        // version" — full virtio surface lands here. macOS 14 unlocks
        // VZVirtualMachine.saveMachineStateTo / restoreMachineStateFrom
        // (Phase E); we feature-detect at runtime rather than raise the
        // minimum.
        .macOS(.v13),
    ],
    targets: [
        .executableTarget(
            name: "mvm-vz-supervisor",
            path: "Sources/mvm-vz-supervisor"
        ),
    ]
)
