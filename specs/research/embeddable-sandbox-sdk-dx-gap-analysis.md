# Embeddable-sandbox SDK — feature-parity gap analysis

Compares mvm against a leading adjacent product: an Apache-2.0, local-first, **libkrun-based embeddable microVM sandbox library**, Python-first (async **and** sync) with Node/Go/Rust/C SDKs and a REST server. It boots a real microVM ("a box") per workload, runs an OCI image inside, and exposes a one-call API to exec against it. (Named obliquely per repo policy — "the reference SDK" throughout. Captured at their v0.9.5.)

The point of this doc: know exactly where we have feature parity, where we trail, and where we lead — so the Stage C SDK/CLI/storage/secrets plans close the right gaps and the positioning leads with the right strengths.

## TL;DR

The reference SDK wins on **developer ergonomics**. mvm wins decisively on the **security spine**. The two products barely overlap: their parity gap against us is almost entirely DX, and our differentiation against them is almost entirely "we sign, audit, verify, and default-deny; they don't."

- **They have, we don't (yet):** a dead-simple imperative live-exec API (`box(image).start()` → `box.exec(...) -> stdout`), a handful of typed convenience box classes (code-runner, browser, desktop, interactive), first-class async **and** sync surfaces, multi-language SDKs, git-like disk snapshot/branch/clone UX, a `serve` REST frontend, a published "<50ms" boot number.
- **We have, they don't:** signed + audited execution (claim 8), content-addressed signed bundles (9), **default-deny** egress (10) vs their default-allow opt-in allow-list, verified boot / dm-verity (3), app-deps SBOM+CVE+attestation audit (11), OCI image provenance in the audit chain (14), always-on seccomp `standard` + `setpriv` (their seccomp and Linux jailer ship **off** by default).
- **We both have, different mechanism:** placeholder secret substitution on egress (their MITM-CA swap vs our decided signed/destination-bound/audited substitution — 129) and per-box OCI images.

## What the reference SDK offers

- **Runtime:** libkrun VMM (one supervisor subprocess per box via `krun_start_enter` takeover — the same gotcha we hit), gvproxy user-mode networking (libslirp alt), OCI client + blob cache, ext4 via e2fsprogs, per-box QCOW2 disk with COW shared base layers, virtiofs host mounts, gRPC-over-vsock host↔guest control. 13 crates, 4 `deps/*-sys` FFI crates (the same shape we adopted).
- **DX:** `pip install`, one-call typed sandbox classes, async + greenlet sync, `exec(cmd,*args,env,user,timeout,cwd)->{exit_code,stdout,stderr}` with streaming, `copy_in`/`copy_out`, port forwarding, OCI image refs with configurable registries. Typed helpers: a code-runner (`run(code)`, `install_package`), a browser-automation class (exposes a CDP/Playwright endpoint over a forwarded port), a desktop-automation class, an interactive-terminal class.
- **State:** disk-state snapshots (quiesce → point-in-time QCOW2), marketed as checkpoint/rollback/fork/clone "branch like git." **No live-memory pause/resume/fork** — clone is disk-clone, and restore refuses while running.
- **Secrets:** `secret(name, value, hosts=[], placeholder)`; guest sees only a placeholder env var; a host MITM proxy (host-generated CA the guest trusts) swaps the placeholder for the real value on outbound HTTPS to allow-listed hosts.
- **Egress:** opt-in allow-list enforced by a DNS sinkhole + TCP filter; **empty list = unrestricted** (default-allow).
- **Posture:** hardware virt + jailer (namespaces/chroot/pivot_root/cgroups/bubblewrap) + seccomp — but **jailer off by default on Linux, seccomp off by default**, "prioritizing compatibility." Threat model explicitly excludes malicious host, host-kernel vulns, side channels, supply chain.
- **Model:** open source, no hosted tier advertised; a REST `serve` mode with api-key/bearer auth hints at an intended remote/multi-client path.

## Feature gap table

| Capability | reference SDK | mvm today | gap → action |
|---|---|---|---|
| One-call live sandbox + `exec` | yes; async+sync; `stdout`/`exit` | declarative SDK (decorator/record) + `mvmctl run`/`invoke`; builder VM is the live env, workload VMs are headless | **build the imperative live-exec surface** (dev-tier) → 125, demoed in 120 |
| Typed convenience classes | code/browser/desktop/interactive | none | thin helpers over the base SDK (code-runner; browser/desktop = image+port presets) → 125 |
| Async **and** sync SDK | both first-class | Python `mvm` + TS | add sync surface; keep async → 125 |
| Multi-language SDKs | Py/Node/Go/Rust/C | Py + TS (Rust internal) | Node parity; Go/C are scope calls → 125 |
| OCI image per box | own client + cache | claim 14 (mvm-oci) + Nix images | parity; **we add signed provenance** |
| Copy files in/out | `copy_in`/`copy_out` | guest-agent fs RPC | expose ergonomically → 125 |
| Port forwarding | host:guest TCP/UDP | `MVM_PORTS` via vsock | parity |
| Disk snapshot / branch / clone | yes (QCOW2 COW) | warm-start substrate planned | match the UX **and exceed** (live memory) → 123 |
| Live pause/resume (memory) | **no** | planned (123): **Firecracker** live-memory (UFFD); **Vz** save/restore (macOS 26+); **libkrun** disk-only | differentiator where the backend supports it |
| Secrets (no value in guest) | placeholder + MITM-CA swap | `SecretRef` model; egress substitution decided | parity in spirit; **we exceed**: signed, destination-bound, audited → 129 |
| Egress policy | opt-in allow-list, **default-allow** | **default-deny** (claim 10) | **we lead** |
| Signed + audited execution | **no** | claim 8 | **we lead** |
| Verified boot (dm-verity) | **no** | claim 3 | **we lead** |
| App-deps audit (SBOM/CVE) | **no** | claim 11 | **we lead** |
| OCI provenance in audit | **no** | claim 14 | **we lead** |
| Default isolation | seccomp/jailer **off** by default | seccomp `standard` + `setpriv` always on | **we lead** |
| REST `serve` frontend | yes | the daemon/serve surface is mvmd's | parity via mvmd |
| Embeddable, no daemon/root | yes | `mvm` = library + CLI; `mvmd` = the daemon | parity |
| Boot time | "<50ms" claimed | sub-150ms target | measure + publish → 127 |
| Metrics / health checks | atomics + Docker-style healthcheck | metering (ADR-040) + `doctor` | parity-ish → 127 |

## Where mvm already leads — keep it, lead with it

The reference SDK's own threat model excludes the malicious host and stops at "fast sandbox + basic isolation." Our entire security spine is absent from it: no signed/audited execution, no signed content-addressed bundles, default-allow egress, no verified boot, no app-deps audit, no OCI provenance, and isolation hardening that ships off by default. This is the moat. The product story leads here; DX parity is table stakes underneath it.

## Where mvm trails — build for parity

All DX, and it concentrates in one place: **the imperative "boot a box and exec against it" experience.** Our surface is build/derive-oriented (decorator → IR → Nix build → headless workload). Theirs is "give me a running box right now and let me run commands in it." These aren't in conflict — the imperative surface is a thin ergonomic layer over the same substrate, scoped to the **dev tier**:

1. **Live-exec API** (125, demoed in 120): the one-call **`Sandbox`** ergonomic. mvm already has the class (`sdks/python/mvm/_sandbox.py`) and it's already dual-mode — **live = dev, record = prod**, with `SandboxDevOnly` guarding the boundary — so this is polish (`Sandbox.create(image=…)`, a one-shot `exec(cmd) -> {stdout, stderr, exit}`, `copy_in/out`, ports, async+sync), not a new class. The dual-mode `Sandbox` is itself a differentiator: the reference SDK's boxes are live-only, so it has no prod-lowering path.
2. **Typed helpers** (125): a code-runner (`run(code)`, `install_package`) and image+port presets for browser/desktop automation. Small wrappers, big perceived surface.
3. **Snapshot/branch/clone UX** (123): expose the warm-start substrate as checkpoint/branch/clone — past disk-only to live-memory resume on Firecracker + Vz (macOS 26+); libkrun stays disk-only (capability check 2026-05-31).
4. **Boot number** (127): measure cold/warm per backend and publish it; don't cede the "fastest" framing by silence.

## The one real design call: live exec vs the security spine

Their `box.exec(arbitrary command)` is live arbitrary execution. Our prod guest agent has **no `do_exec`** (claim 4) by design. So the ergonomic live-exec surface is a **dev-tier capability** (the builder VM / dev-mode microVM with the `dev-shell`-featured agent); production stays on the signed-`ExecutionPlan` path with no interactive exec. That is the right resolution and arguably a stronger story than theirs: **the same easy DX in dev, a locked and audited path in prod** — the thing a security-conscious buyer actually wants, and the thing the reference SDK cannot offer because it has no prod tier to lock.

## Recommendations (mapped to Stage C)

- **125 (CLI + SDK):** the live-exec imperative surface + typed helpers + async/sync + Node parity. Largest parity gap; highest DX leverage.
- **120 (core demo):** show the one-call ergonomic so the "look how easy" moment exists from day one.
- **123 (storage + warm-start):** snapshot/branch/clone UX, and live-memory resume (Firecracker + Vz) as the thing they don't have.
- **129 (secrets):** match the placeholder-on-egress model, exceed it with signed/destination-bound/audited substitution instead of a trusted MITM CA.
- **127 (bench):** publish the boot number.
- **Positioning (docs, 116-era):** lead with the security spine; present DX parity as the floor, not the pitch.
