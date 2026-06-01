# Sandboxes for AI — Cardoso gap analysis

**Source:** Luis Cardoso, [A field guide to sandboxes for AI](https://www.luiscardoso.dev/blog/sandboxes-for-ai), 2026-01-05.
**Analysed:** 2026-05-28.
**Companion docs:** [ADR-002 §"Cardoso minimum-viable-policy checklist"](../adrs/002-microvm-security-posture.md) (checklist + three-question summary) and [Plan 111 — Cardoso gap coordination](../plans/111-cardoso-gap-coordination.md) (workstream tracker).

This file captures the raw audit of mvm's public posture against Cardoso's
three-decision model (boundary × policy × lifecycle) and his "minimum viable
policy" checklist, so it can be cited from ADRs and plans without re-deriving
it. ADR-002 carries the canonical checklist; the plan carries the workstream
list. This research note is the why and how.

## TL;DR

mvm sits exactly where Cardoso's decision table sends "AI coding
agent + shell + don't fully trust the code": **Firecracker microVM
on Linux KVM, libkrun on macOS, Apple Container on macOS 26+ AS**,
with Cloud Hypervisor as a Tier-1 peer for VFIO / GPU workloads.
On the *boundary* axis we are aligned. On the *policy* axis we
**meet or over-deliver** vs his minimum viable policy: default-deny
egress is already claim 10; signed `ExecutionPlan` (claim 8), sealed
bundles (claim 9), sealed deps volumes (claim 11), no-raw-secret
broker (claim 13) sit above his floor. The remaining policy gap is
`timeout_seconds` + `pid_limit` on `ExecutionPlan`. On the *VM
lifecycle* axis we under-deliver: Plan 65 scaffolds snapshot/restore
trait but the cross-claim semantics are not enumerated, and there
is no warm-pool. SDK workload hooks (different axis) are fully
present. Open gaps for the AI-positioning reader: GPU implementation
(posture allowed but backend not enabled — deferred, no current
GPU dependency), published cold-start numbers, README workload-shape
taxonomy.

## Where Cardoso's framework and mvm's claims align

| Cardoso position | mvm's matching mechanism |
|---|---|
| "Shell + package managers + don't fully trust code → microVM" | Firecracker on Linux KVM; libkrun / Apple Container on macOS; Cloud Hypervisor as Tier-1 peer (ADR-002 §"Per-backend tier matrix"). |
| Firecracker's "intentionally boring" device model: net / block / vsock / console | Inherited unchanged. |
| Jailer = chroot + namespaces + cgroups + privilege drop + 24-syscall / 30-ioctl seccomp | Inherited from Firecracker on Linux. On the libkrun host side: one `mvm-libkrun-supervisor` per VM with fuzzed `SupervisorConfig` parser (claim 5 + Plan 88 W6). |
| "MicroVM still needs strong policy or it's a high-powered exfil box" | Explicit shares only (claim 1) + signed `ExecutionPlan` admission (claim 8) + sealed deps volumes (claim 11) + default-deny egress (claim 10). |
| Workspace-only filesystem; no ambient host mounts | Claim 1. |
| "Observability — sandboxes without telemetry become incident-response theater" | Chain-signed audit log; `mvmctl audit verify` exits non-zero on tampering. Service-call entries per claim 12 (Plan 104 / ADR-059). |
| Default-deny egress + allowlist | Claim 10 — `policy_default_is_deny_all` + `test_resolve_network_policy_default_is_deny_all`. |
| No long-lived credentials | Claim 8 G4 validity window + claim 13 destination-bound + time-bound credentials (Plan 104 / ADR-049 / ADR-059). |
| Policy proxy / brokered syscall pattern | Claim 12 binding-gated dispatch + claim 13 no-raw-secret broker. |
| libkrun named for "local agents on macOS/ARM64 are a first-class workflow" | Entire macOS path is libkrun (Vz on 26+ AS for builds). |
| "Embedded VMM → treat the VMM process itself as part of your TCB" | `mvm-libkrun-supervisor` is exactly this pattern. |
| Hardware-backed isolation a "different threat model" | ADR-002 explicitly puts hardware-backed key attestation out of scope. |
| "Multi-tenant changes everything" | Multi-tenant guests out of scope; mvmd handles fleet. |

## Where mvm goes beyond Cardoso's minimum-viable-policy

Properties mvm enforces that Cardoso's framework does not ask for:

- **Signed, admission-checked execution plans** (claim 8): Ed25519 host
  signer, G4 validity window, replay-store, audit chain.
- **Signed content-addressed bundles** re-verified at fetch and admit
  (claim 9).
- **Sealed dependency volumes with SBOM + CVE + attestation**
  (claim 11, ADR-047): hash-locked deps; `--prod` fails closed on
  high/critical CVEs; `mvmctl deps inspect` exposes sealed sidecars
  without a VM spawn.
- **dm-verity'd rootfs that panics on tamper** (claim 3): kernel
  panics before userspace on a flipped data block.
- **`prod-agent-no-exec` CI symbol-absence gate** (claim 4):
  production guest agent ships without `do_exec`.
- **Host-side broker with binding-gated dispatch + audit** (claim 12,
  Plan 104 / ADR-059).
- **No raw secret over broker channel** (claim 13, ADR-049).
- **Cargo deny + reproducibility double-build** (claim 7).
- **Hermetic builds — no host Nix, no host artifact dependency**
  (ADR-046 invariant): host environment never influences the artifact;
  source-checkout builds never depend on mvm-published artifacts.

## Gaps Cardoso's framework still exposes

Ordered by external-credibility impact.

1. **CLAUDE.md is out of date.** Lists 10 claims; ADR-002 has 13.
   CLAUDE.md claims 1–8 align with ADR-002 1–8; CLAUDE.md claim 9
   (app-dep audit) maps to ADR-002 **claim 11**; CLAUDE.md claim 10
   (OCI provenance) is not yet a numbered ADR-002 claim. ADR-002
   adds claims 9 (signed bundles), 10 (default-deny egress), 12
   (broker binding-gated dispatch), 13 (no-raw-secret broker channel)
   that CLAUDE.md doesn't enumerate.
   *Workstream A* in Plan 111.
2. **VM-level snapshot/restore cross-claim semantics unwritten.**
   Plan 65 scaffolds the `VmBackend::snapshot` / `restore` trait but
   the cross-claim interactions are unaddressed:
   - **Entropy reuse.** Restored VM continues from post-boot RNG
     state; repeated restores produce duplicate randomness.
     Mitigation: reseed `/dev/urandom` via virtio-rng or vsock-
     delivered entropy before vCPU unfreeze.
   - **Claim 8 admission vs continuation.** Signed `ExecutionPlan`
     was admitted once with G4 validity window + nonce; restore
     must define continuation-within-window vs re-admit semantics.
   - **Claim 3 dm-verity re-check.** Verified boot runs once;
     snapshot bypassing boot path skips re-verification unless
     snapshot file is hash-pinned and verified at resume.
   - **Claim 7 reproducibility carve-out.** Snapshots inject
     workload-runtime non-determinism the double-build can't
     reproduce; the claim must explicitly carve out restore-time
     delta.
   - **Audit chain integrity.** Two restores from one snapshot
     produce two divergent chains rooted at the same parent;
     `verify_audit_chain` must reject or explicitly support
     branching.
   *Workstream B* in Plan 111.
3. **GPU implementation gap.** ADR-002 §"Per-backend tier matrix"
   already lists Cloud Hypervisor as Tier-1 peer for VFIO / GPU
   passthrough; the posture is allowed. No backend currently ships
   VFIO. *Workstream D — deferred, no current GPU dependency.*
4. **No published cold-start number.** The cloud sandbox-runtime
   category publishes one. Plan 74 names this work. *Workstream E*
   in Plan 111.
5. **Workload-shape taxonomy missing from README positioning.**
   Cardoso's devbox / code-interpreter / tool-calling / RL split is
   becoming field vocabulary. mvm doesn't slot itself. *Workstream F*
   in Plan 111.
6. **Multi-tenant story split across two repos without README
   signal.** Cardoso routes "multi-tenant SaaS" to microVMs —
   readers will land on mvm assuming we ship that. Need to name
   mvmctl single-tenant vs mvmd multi-tenant up front. *Workstream F*
   in Plan 111.
7. **Threat-model vocabulary drift.** Cardoso uses hostile /
   semi-trusted / trusted explicitly; ADR-002 doesn't reconcile to
   those words. Third-party dependencies need a fourth category
   ("audited semi-trusted") since claim 11 treats them as pinned +
   SBOM + attested + CVE-scanned. *Workstream G* in Plan 111.
8. **TCB inventory** distributed across ADR-002 §"Trust layers
   (Matryoshka model)" + ADR-055 §"New untrusted-input surfaces" +
   ADR-061 §"Implementation choices." No single readable
   enumeration. *Workstream I* in Plan 111.
9. **Plan 104 is Cardoso's policy-proxy pattern but doesn't say
   so.** Claim 12 + claim 13 implement binding-gated dispatch and
   no-raw-secret broker — the framing matches Cardoso's policy
   proxy. One-paragraph citation tweak. *Workstream H* in Plan 111.
10. **Resource-limit completeness.** Cardoso names CPU / memory /
    disk / timeouts / PIDs. `ExecutionPlan.resources` is scaffolded;
    `timeout_seconds` and `pid_limit` are not populated.
    *Workstream C* in Plan 111.

## Cardoso's three-question model applied to mvm

| Question | mvm answer |
|---|---|
| What is shared between this code and the host? | KVM `/dev/kvm` ioctls (Linux); Hypervisor.framework calls (macOS Vz / libkrun); vsock for control plane + brokered host services (binding-gated per claim 12); one explicit virtio-fs share per declared mount. Host filesystem is never ambient. |
| What can the code touch? | Whatever the signed `ExecutionPlan` admits: declared shares, declared egress allowlist (claim 10), declared volumes, declared `host.*` brokered services (claim 12 binding). No raw devices. No host process namespace. No host network namespace. |
| What survives between runs? | Volumes the plan declares persistent (sealed deps volumes are RO and hash-locked per claim 11). Everything else is ephemeral by default. Snapshot/restore on workload microVMs not yet exposed — see Workstream B in Plan 111. SDK workload hooks (`before_build` / `before_start` / `after_start` / `before_stop` in `crates/mvm-sdk/src/compile/hooks.rs`) shape what runs at launch, not what survives across launches. |

## Disclosure trade-off

Publishing this gap analysis, the ADR-002 Cardoso checklist, and (in
Workstream I) a consolidated TCB inventory is a deliberate disclosure.
ADR-002 and the claim table are already public; this extends the same
posture. External scrutiny improves the system. The author of the
source post would call this an instance of "treat profiles as code:
version them, test them, and expect them to evolve."
