# `mvm-vz-supervisor`

Plan 97 Phase A. Per-VM Swift binary that drives Apple's
[Virtualization.framework][vz] for one Linux guest.

[vz]: https://developer.apple.com/documentation/virtualization

One supervisor process per VM (matches `mvm-libkrun-supervisor`). Reads a
`SupervisorConfig` JSON document on stdin, constructs a
`VZVirtualMachineConfiguration`, starts the guest, blocks until it exits.

## Build

```sh
./tools/build.sh         # debug
./tools/build.sh release # release
```

The script wraps `swift build` with the ad-hoc codesigning step Vz
requires (the `com.apple.security.virtualization` entitlement in
`Entitlements.plist`). Without that entitlement Hypervisor.framework
refuses to start the VM.

## Quick smoke

```sh
# Reject unknown fields (ADR-002 claim 5):
echo '{"unexpected_key": 1}' | ./.build/arm64-apple-macosx/debug/mvm-vz-supervisor
# → exit 2, "unknown field: unexpected_key"

# Reject empty stdin:
printf '' | ./.build/arm64-apple-macosx/debug/mvm-vz-supervisor
# → exit 2, "empty stdin (expected SupervisorConfig JSON)"
```

## End-to-end (Phase A acceptance)

To boot the dev-shell image under Vz and dial the guest agent on vsock
5252, you need a built kernel + ext4 rootfs and an admitted
`SupervisorConfig` JSON. Phase B (the `VzBackend` Rust impl in
`crates/mvm-backend/src/vz.rs`) produces the JSON automatically; until
that lands, hand-craft one against your local
`~/.mvm/dev/current/{vmlinux,rootfs.ext4}` paths.

The expected vsock socket convention is
`~/.mvm/run/<vm_id>/vsock/vsock-<port>.sock` (mode 0700, per ADR-002
claim 1's host-fs contract).

## Files

- `Package.swift` — Swift package manifest, macOS 13+ target
- `Entitlements.plist` — `com.apple.security.virtualization`
- `tools/build.sh` — `swift build` + ad-hoc codesign
- `Sources/mvm-vz-supervisor/main.swift` — entry point (stdin → exit)
- `Sources/mvm-vz-supervisor/Config.swift` — strict JSON decoder
- `Sources/mvm-vz-supervisor/Supervisor.swift` — VZ machine lifecycle
- `Sources/mvm-vz-supervisor/VsockProxy.swift` — unix-socket ↔ vsock bridge
- `Sources/mvm-vz-supervisor/Network.swift` — gvproxy file-handle attachment

## License

Apache-2.0 OR MIT, matching the Rust workspace.

## See also

- `specs/plans/97-vz-backend.md` — the full plan
- `specs/adrs/002-microvm-security-posture.md` — security claim table
- `specs/adrs/055-passt-virtio-net.md` — gvproxy networking on macOS
- `crates/mvm-libkrun/src/bin/mvm-libkrun-supervisor.rs` — sibling
  supervisor (libkrun backend) whose architecture this mirrors
