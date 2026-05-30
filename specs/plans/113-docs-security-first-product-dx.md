# Plan 113: Security-first docs relaunch

**Status:** In progress
**Date:** 2026-05-29
**Goal:** reshape the public docs around secure sandbox developer workflows, imperative runtime SDKs, decorator-style workload declarations, builder VM secure builds, cold-mode recovery, Nix-first artifacts, and OCI compatibility without copying another product's website or weakening `mvm`'s security posture.

## Context

The secure sandbox runtime category has a clear expected capability shape: language SDKs, examples, API/daemon/CLI surfaces, runtime boxes, network/proxy layers, and snapshot management. `mvm` needs the same low-friction path where it fits, while preserving a different product promise:

- `mvm` is the local runtime substrate for sandboxed microVM execution.
- The builder VM is already a load-bearing developer/build boundary: developers run host-side commands while Linux Nix evaluation, builds, and image assembly happen inside the builder VM.
- The builder VM DX must expose two persistent modes:
  - `cargo run -- dev up` starts or reuses a persistent developer builder VM for interactive/debuggable build work.
  - `cargo run -- build` starts or reuses a persistent non-interactive builder VM for normal build jobs, with `--no-persistent-builder` as the explicit fallback/debug path.
- `mvm` already has cold-state and snapshot recovery primitives: Firecracker pause/resume writes sealed instance snapshots, Vz save/restore hash-pins machine-state files in the audit chain, and pool instances have Sleeping/Running lifecycle paths.
- Platform docs must be explicit: Linux execution and macOS are supported targets; Windows is eventual/future work tracked by [mvm#428](https://github.com/tinylabscom/mvm/issues/428) instead of being presented as shipped.
- OCI images are supported for compatibility; the core trust story is Nix-built microVM artifacts, signed plans, verified artifacts, and auditable launches.
- The SDK target is split: imperative runtime lifecycle APIs and declarative decorator APIs.
- Security claims must be statused and backed by implementation evidence.

## Required docs shape

| Section | Purpose | Security requirement |
| --- | --- | --- |
| Getting Started | Fast path to first sandbox | No unsupported latency, OCI, or secret claims. |
| SDK | Runtime lifecycle plus decorator compilation | Separate host-side SDK execution from guest execution. |
| Tutorials | Agent, LLM, file, browser, services, snapshots | Every tutorial names network, secret, file, and persistence boundaries. |
| Guides | Nix, OCI, manifests, builder VM | OCI by digest; Nix as preferred auditable path. |
| Security | Claim ledger, threat model, verified boot, sandbox parity | Every strong claim maps to Shipped/Preview/Planned/Not claimed. |
| Architecture | `mvm`, guest agent, builder VM, backends | Runtime semantics stay in `mvm`. |
| Reference | CLI, SDK API, config, limits | Examples must match current commands or be marked planned. |
| Cold mode | Pause/resume, snapshot save/restore, Sleeping pools | State retention, tamper evidence, and backend limitations are explicit. |

## Workstreams

### W1 — Information architecture

- [x] Restructure Starlight sidebar around product sections: Getting Started, Tutorials, SDK, Guides, Security, Architecture, Reference.
- [x] Keep existing pages reachable while introducing the new top-level grouping.
- [x] Add an LLM-friendly documentation index once the new pages exist.

### W2 — SDK developer experience

- [x] Add runtime SDK overview covering `create`, command execution, files, logs, snapshot/fork, stop, destroy, and detach.
- [x] Add decorator SDK overview covering static compile, Workload IR, Nix image selection, resources, hooks, network policy, and secret references.
- [x] Mark unshipped lifecycle APIs as Planned unless code and tests prove them.
- [x] Add Python, TypeScript, Rust, and C quickstart/status pages.

### W3 — Security claim ledger

- [x] Add a public claim-ledger page that links claims to ADRs, plan rows, tests, and status.
- [x] Run existing docs checks against the new high-risk language.
- [ ] Require every quickstart and tutorial to link to the claim ledger or the relevant status page.

### W4 — Platform support

- [x] Make Linux execution and macOS support visible in the first-viewport docs path.
- [x] Keep Windows language in the eventual/future bucket until the tracking issue is implemented.
- [x] Link the Windows issue from install and troubleshooting docs: [mvm#428](https://github.com/tinylabscom/mvm/issues/428).

### W5 — Nix + OCI positioning

- [x] Add a guide that presents Nix as the primary path for reproducibility, extensibility, and auditability.
- [x] Make builder VM secure-build positioning first-class in Getting Started and Architecture, not buried in an advanced guide.
- [x] Document the two desired persistent builder personas:
      `cargo run -- dev up` for the interactive persistent developer builder VM,
      and `cargo run -- build` for the persistent non-interactive builder VM.
- [x] Keep the existing explicit controls visible: `mvmctl persistent-builder start|submit|status|stop` and `mvmctl build --no-persistent-builder`.
- [x] Present OCI as compatibility input with digest pinning, layer verification, cache scoping, and mutable-tag policy.
- [x] Do not describe Docker as the production runtime.

### W6 — Cold mode and state recovery

- [x] Replace the previous snapshots stub with shipped Firecracker pause/resume and Vz save/restore semantics.
- [x] Add a cold-mode page explaining that a sandbox can move from running to sealed/sleeping state and recover from a snapshot.
- [x] Document backend differences: Firecracker sealed instance snapshots, Vz machine-state files, pool delta snapshots, and unsupported backends.
- [x] Carry the security implications: snapshot contents are sensitive, restore integrity is checked, and retention/deletion policy matters.

### W7 — Tutorials and examples

- [x] Add secure agent sandbox tutorial.
- [x] Add LLM tool execution tutorial with deny-by-default egress.
- [x] Add file transfer tutorial with mount/upload/download boundaries.
- [x] Add browser automation tutorial with credential, network, and persistence warnings.
- [x] Add service/port-forwarding tutorial with policy and audit notes.
- [x] Add desktop automation, interactive terminal, any-language, long-running services, and error-handling tutorials.

### W8 — Verification

- [x] Run `cargo xtask check-doc-claims`.
- [x] Run the docs build.
- [ ] Run `cargo check --workspace` if Rust code changes.
- [x] Update `specs/SPRINT.md` after each completed docs slice.

## Security implications to carry through every page

- Strong claims need status and evidence.
- Builder VM secure-build claims should link to the shipped builder VM docs and code paths.
- Persistent builder claims should distinguish current low-level controls from the desired top-level `dev up` / `build` DX until command behavior proves the top-level flow.
- Cold-mode docs should say exactly which backend path is being described instead of implying universal support.
- Platform claims should name Linux/macOS as current and Windows as eventual unless linked evidence proves otherwise.
- SDK samples must say whether code runs on host or guest.
- Secrets should be managed references by default; raw guest materialization must be explicit and unsafe.
- OCI examples should pin digests or label mutable tags as local/dev only.
- Nix examples should preserve flake pinning and builder VM isolation.
- Stateful sandbox docs must describe cleanup, snapshots, retention, and deletion semantics.
- Browser and desktop automation examples must name credential, egress, and file-transfer boundaries.
- Install examples should avoid unverified `curl | sh` as the only path.

## Exit criteria

- A reader can understand the `mvm` product model from the first page.
- A reader can understand that secure builds happen through the builder VM and runtime guests boot built artifacts.
- A reader can understand cold-mode snapshot recovery and its backend-specific limits.
- Runtime SDK and decorator SDK docs are separate, with status labels where behavior is planned.
- Security claim language is backed by the claim ledger and passes lint.
- Nix-first and OCI-compatibility positioning is explicit.
- Docs build passes from the worktree.
