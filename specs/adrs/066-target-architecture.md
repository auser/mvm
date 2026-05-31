# ADR 066 - Target architecture for the mvm rewrite (crate graph, trait seams, process model, encryption, claim map)

**Status**: Accepted
**Date**: 2026-05-31
**Cross-refs**: ADR-002 (security posture, the 14 claims), ADR-014 (VmBackend single trait), ADR-027 (encryption layering) + ADR-042 (encryption substrate), ADR-031 (cross-platform), ADR-040 (metering), ADR-041 (signed/audited ExecutionPlan, claim 8), ADR-043 + ADR-053 (protocol versioning / readiness), ADR-046 (builder VM via libkrun — the canonical builder-VM ADR post-consolidation), ADR-047 (sealed app-deps, claim 11), ADR-051 (runtime overlay disk), ADR-062 (host services broker — the canonical broker ADR post-consolidation), ADR-063 (boundary language: lean Rust), ADR-064 (NetworkProvider trait), ADR-065 (single builder/dev image, embedded host binaries). Planning input: Plan 117 (cleanup & rearchitecture brief).

## Context

`mvm` grew through many AI-assisted sessions into **32 workspace crates / ~247k LOC** with duplicated subsystems (six vsock/framing impls, four config/secret loaders, three signer-subprocess templates), stubs, and a sparse ADR set — while holding a **14-claim, CI-enforced security posture** (ADR-002) that must survive the cleanup. Plan 117 is the planning input; this ADR is the canonical architecture decision the rewrite executes against.

The rewrite is a **complete refactor executed step-by-step**: this ADR fixes the target shape; per-workstream plans (Stage C) sequence the work; the core demo lands first. Two hard constraints frame every choice below:

1. **Every functional and security guarantee is preserved.** The 14 claims stay true and stay CI-gated. A simplification that would weaken a claim or remove its gate is rejected, not silently accepted.
2. **Runtime process isolation ≠ build-time crate count.** Crates may collapse; the separate-address-space *processes* that back claims 8/12/13 may not. "The boundary lives below the workload" is the product's #1 differentiator and is treated as a verifiable contract, not an emergent property.

This ADR settles the structural decisions taken during the Stage B brainstorm so the next contributor — or AI session — can consult the rationale without re-deriving it.

## Decision

### 1. Crate graph — name by role, front with a trait, hide impls (32 → 17 + `crates/deps/*-sys`)

Every crate is named for the **capability/role** it provides, never for a specific implementation. There is no `mvm-firecracker`: there is a generic `mvm-backend` exposing `VmBackend` with the backends as impls *inside*. Adding a backend, network, or storage impl is a new impl behind the trait, never a new architectural crate wired everywhere. **Prefer modules over crates.** A crate earns separate existence only through a trait seam an external consumer (mvmd) extends, a separate runtime process (the moat), a proc-macro boundary, or a genuinely distinct dep-closure / OS-gate; everything else is a *module* inside an existing crate (the `core::` dedups and the plan/policy/security fold are this principle applied). Lean analogs in this space carry whole subsystems — state store, disk, images, networking, jailer — as modules of a single library crate; **17 is the floor the trait seams + process isolation justify, not crate-splitting for its own sake.**

The target is **17 architectural crates** (from 32), plus a bracketed-off `crates/deps/` directory holding the unsafe FFI `-sys` binding crates (treated as plumbing, like the excluded fuzz crates). Every current crate maps to a destination:

| Current crate(s) | Destination | Note |
|---|---|---|
| `mvm-core` | **`mvm-core`** (keep) | absorbs plan + policy + security; hosts the dedups (`core::framing`, `core::config_envelope`, `core::paths`, `core::subprocess`) |
| `mvm-plan`, `mvm-policy`, `mvm-security` | → `mvm-core` | pure types + crypto; `mvm-core` already owns "signing". **No async/runtime deps may enter `mvm-core`.** |
| `mvm-ir` | → `mvm-sdk` | the IR is the SDK's lowering target |
| `mvm-sdk` | **`mvm-sdk`** (keep) | the central derivation engine; absorbs the IR |
| `mvm-sdk-macros` | **`mvm-sdk-macros`** (keep) | proc-macro crates must stand alone |
| `mvm-base` | → `mvm` | Lima-era leftover |
| `mvm` | **`mvm`** (keep) | runtime: shell, VM lifecycle, UI, templates |
| `mvm-build` | **`mvm-build`** (keep) | Nix builder pipeline; also hosts the builder-VM-only `[[bin]]`s (`mvm-host-vm-init`, `mvm-egress-proxy`), cfg-gated inert on non-Linux |
| `mvm-host-vm-init`, `mvm-egress-proxy` | → `mvm-build` (`[[bin]]`s) | builder-VM-only Linux tools |
| `mvm-runner` | → `mvm-guest` | in-guest entrypoint runner |
| `mvm-guest` | **`mvm-guest`** (keep) | vsock protocol, console, integrations, agent, runner |
| `mvm-cli` | **`mvm-cli`** (keep) | Clap CLI — a thin shell over the libraries; no logic |
| `mvm-mcp` | **`mvm-mcp`** (keep) | backs `mvmctl mcp serve` (local stdio MCP; **not** the REST sidecar — that is mvmd's) |
| `mvm-oci` | **`mvm-oci`** (keep) | OCI import/export (claim 14) |
| `mvm-backend` | **`mvm-backend`** (keep) | `VmBackend` trait + all impls; backend **selection/dispatch** lives here |
| `mvm-providers`, `mvm-libkrun`, `mvm-vz` | → `mvm-backend` (+ `crates/deps/libkrun-sys`) | FFI binding moves to `crates/deps/`; the Swift-interface (`mvm-vz`, no FFI) folds into `mvm-backend` |
| — | **`mvm-network`** (new) | `NetworkProvider`: provisioning + ingress/egress policy + DNS + audit (ADR-064 generalized to provisioning) |
| `mvm-storage` | **`mvm-storage`** (keep) | `StorageProvider` + `local` + **`encrypted`** impls in-repo |
| `mvm-addon-dns`, `mvm-addon-vsock-bridge` | → **`mvm-guest-helpers`** (`[[bin]]`s) | in-guest helper daemons |
| `mvm-supervisor`, `mvm-broker`, `mvm-host-signer`, `mvm-audit-signer`, `mvm-jailer-lite` | → **`mvm-hostd`** | one crate, **four separate `[[bin]]`s** (see §3); jailer-lite is a module |
| `mvm-libkrun-supervisor`, `mvm-vz-drainer`, `mvm-firecracker-bridge` | → **`mvm-vm-sidecar`** | one crate, cfg-gated per-backend `[[bin]]`s (one process per VM) |
| `mvm-vz-supervisor` (Swift) | **`mvm-vz-supervisor`** (keep) | non-Rust; separate build, outside the cargo workspace |
| `xtask` | **`xtask`** (keep) | workspace tooling + the claim-gate lints |

**The 17 architectural crates:** `mvm-core`, `mvm-sdk`, `mvm-sdk-macros`, `mvm`, `mvm-build`, `mvm-guest`, `mvm-cli`, `mvm-mcp`, `mvm-oci`, `mvm-backend`, `mvm-network`, `mvm-storage`, `mvm-hostd`, `mvm-vm-sidecar`, `mvm-guest-helpers`, `mvm-vz-supervisor` (Swift), `xtask`. (+ the `crates/*/fuzz` crates, excluded from the workspace as today.)

`mvm-core` is the common crate: the six vsock/framing impls collapse to one `core::framing` (`FramedMessage<T>` + pluggable auth); the four config/secret loaders to one `core::config_envelope`; scattered XDG/path helpers to one `core::paths`; the three signer-subprocess templates to one `core::subprocess` scaffold (keyless — see §3).

### 2. FFI bindings live under `crates/deps/*-sys` (current + anticipated)

All unsafe FFI bindings to named external libraries are minimal `-sys` crates grouped under `crates/deps/`. Each holds **only** the binding (bindgen/`extern` surface + a thin safe wrapper) — never selection, dispatch, or policy. The consuming role crate depends on it. This is the brief's "one honest exception" to role-based naming, kept physically demarcated so the FFI boundary (and its `bindgen`/`libclang` build cost) is isolated and auditable.

| `crates/deps/` crate | Binds | Consumed by | Status |
|---|---|---|---|
| `libkrun-sys` | libkrun C ABI (the in-process VMM) | `mvm-backend` | **exists today** (`mvm-libkrun`) |
| `vz-sys` | Apple Virtualization.framework, if bound directly rather than via the Swift supervisor | `mvm-backend` | anticipated |
| `libgvproxy-sys` | gvproxy userspace gateway, if vendored as a lib instead of shelling the binary | `mvm-network` | anticipated |
| `e2fsprogs-sys` | ext4 `mkfs`/tooling, if vendored instead of shelling `mkfs.ext4` | `mvm-build` / `mvm-storage` | anticipated |
| `libcryptsetup-sys` | LUKS2 / dm-crypt for the encrypted `StorageProvider` on Linux | `mvm-storage` | anticipated (see §5) |
| `sev-sys`, `tdx-sys` | SEV-SNP / TDX attestation + launch for the confidential-compute tier | `mvm-backend` | anticipated (deferred frontier; ADR-002 keeps malicious-host out of Phase 1 scope but the structure does not foreclose it) |

Only `libkrun-sys` exists today; the rest are anticipated homes. Adding any of them is a new `crates/deps/<name>-sys` + a trait impl in the consuming crate — nothing else moves. This reserves the structure so the FFI surface never sprawls back into the architectural crates.

### 3. Host process model — separate role binaries, one supervising process

The host side is **separate role binaries, launched and supervised by a single process.** This keeps the strongest isolation guarantee *and* a single operational entry point.

- **`mvm-hostd` is one crate with four separate `[[bin]]` targets** — `mvm-supervisor`, `mvm-broker`, `mvm-host-signer`, `mvm-audit-signer`. `cargo build` emits four distinct executables. They share a *keyless* library (framing, request routing, audit-chain **verification** which needs only public keys, config); each binary's key handling lives in a **bin-private** module, so signing/audit key code is never compiled into the broker or supervisor binary.
- **`mvm-supervisor` is the single supervising process.** The host starts one process — the supervisor. It launches and supervises the others (spawn, health-monitor, restart per a declared policy, ordered shutdown) and spawns the per-VM `mvm-vm-sidecar` process at VM launch. One thing to start; it brings up and tears down the rest.
- **The four host roles stay four separate processes** — this is the moat (four trust zones):

| process (separate address space) | holds | if a hostile workload breaks it |
|---|---|---|
| `mvm-broker` (the only daemon the guest talks to; parses untrusted vsock input) | **no keys** | nothing to steal |
| `mvm-host-signer` | the ExecutionPlan signing key only | can't touch the audit key or the guest |
| `mvm-audit-signer` | the audit key + is the **sole** log writer | can't forge plans; sole-writer = tamper-evident (claim 8) |
| `mvm-supervisor` | admission/launch + lifecycle | a tiny TCB; untrusted parsing lives in `broker`, not here |
| per-VM `mvm-vm-sidecar` | per-VM blast-radius confinement (VMM takeover / audit substrate) | one VM's compromise can't reach another's |

- **Per-role OS confinement (jailer).** Each spawned role applies `mvm-jailer-lite` confinement (`seccompiler` + `landlock` on Linux; `sandbox-exec` on macOS) **before** loading its key or touching untrusted input. The `broker` process is filesystem- and syscall-confined so it cannot `open()` the signing or audit key files even under compromise — defense-in-depth on top of the separate-binary guarantee. (ADR-064 §5 established `mvm-jailer-lite`; it folds into `mvm-hostd` as a module.)
- **Verification is a lint, not a hope.** An `xtask` check asserts the audit-signing-key and plan-signing-key symbols are **not linkable** from the supervisor or broker binaries. Because the roles are *separate binaries*, this is a clean binary-symbol check — the strongest available form of the claim-8 "sole holder" guarantee. (A multicall single binary was considered and **rejected** — see Alternatives.)

The full process-isolation map (the moat, made verifiable) is the table above plus the per-VM sidecars; it supersedes ADR-002 §"process-isolation map" framing and is the source for the claim → gate → location map in §8.

### 4. Consumption topology — library + CLI (mvmd is the sidecar)

`mvm`'s engine lives in library crates behind the trait seams. There are **two** consumption modes, not three:

- **CLI** — `mvmctl` is a thin shell over the libraries (humans + local dev: `mvmctl dev …`). No logic in the CLI layer. `mvmctl`'s `lib.rs` is the **facade**: it re-exports the top-level objects (`mvmctl::core` / `::runtime` / `::build` / `::guest`) plus the trait seams (`VmBackend`, `NetworkProvider`, `StorageProvider`, `BuildEnvironment`) and `ExecutionPlan`/policy types, so a consumer imports one ergonomic surface instead of reaching into 17 crates.
- **Library** — `mvmd` links `mvm` in-process. The contract is **traits + `mvm-core` types**, *not* the macOS/libkrun launch path (mvmd runs its own Firecracker + jailer launch on Linux).

**There is no `mvm` sidecar.** `mvmd` is the sidecar/daemon: the long-lived process, the REST/OpenAPI surface, and the config-switchable local-or-remote transport all live in `mvmd`, which consumes `mvm` as a library. The off-by-default, mTLS-authenticated, same-policy, same-audit remote control plane is `mvmd`'s concern; this repo holds no REST surface and no tenant orchestration (the `--prod` admission gate is mvmd's). Every public API in `mvm` is nonetheless designed to be ergonomic from another crate, because that library surface *is* mvmd's contract.

*(This corrects Plan 117 §A16 / §3, which floated "triple consumption" with `mvm` itself as a sidecar.)*

### 5. Encryption + key lifecycle (implements ADR-027 + ADR-042 — build it, don't re-decide it)

The accepted layering (ADR-027 table, ADR-042 substrate) is designed but largely unbuilt; this ADR maps it onto the crate graph. **All data is encrypted at rest and in flight.**

- **At rest — every mounted dir/volume + snapshots + audit + keys.** Envelope encryption owned by the `StorageProvider` `encrypted` impl in `mvm-storage`: each volume/snapshot has a data key (DEK) encrypting its bytes (AES-XTS via dm-crypt/LUKS2 on Linux — through `crates/deps/libcryptsetup-sys`; a full-blob/per-file AEAD, AES-256-GCM or ChaCha20-Poly1305, on macOS). Each DEK is wrapped by a per-tenant KEK in the OS keystore / Secure Enclave / TPM (`keyring`). The guest sees plaintext (inside the trust boundary); host-side bytes are always ciphertext. **Decided: the mvm-managed AEAD envelope on both platforms** — not host FileVault — because the DEK model is portable, mvm-controlled, rotatable, and attestable.
- **Key rotation.** KEK rotation = re-wrap DEKs (cheap) on a timer (default 90 days, configurable). DEK rotation rides the rebuild cycle — microVMs are rebuilt never mutated, so every rebuild is a free re-key point. Each per-volume DEK is bound to the content hash + signed plan + audit chain so rotation is attestable. Key material zeroizes on drop (`zeroize`).
- **In flight.** **Decided: the Noise Protocol Framework (`snow`)** for the vsock session (mutual auth + forward secrecy, no hand-rolled key exchange), upgrading today's cleartext-JSON + Ed25519-signature (authenticity only). Only vetted primitives (X25519, Ed25519, AES-256-GCM / ChaCha20-Poly1305, HKDF, HMAC-SHA256 via RustCrypto/dalek). agent↔hostd UDS = mTLS; the mvmd hop uses iroh TLS (don't double-encrypt); HTTP/TCP egress = TLS.
- **Boundary.** This defends data-at-rest and process-to-process channels. It does **not** defend a malicious host reading guest RAM (confidential computing) — out of scope per ADR-002 until a new ADR expands the threat model. The structure (§2 `sev-sys`/`tdx-sys`, the `VmBackend` trait, the extensible envelope) does not foreclose it.

One key story lives in `mvm-core` (key types, zeroizing wrappers) + the OS-keystore integration; no per-subsystem reinvention.

### 6. SDK is the central derivation engine

Every path to a microVM derives **through the SDK** (`mvm-sdk`): the four authoring surfaces and the `mvmctl dev`/`build` flow all lower to the same Workload IR and the same builder-VM Nix build. Four authoring surfaces, one IR, one build path: (1) decorator `@mvm.app()` — parsed **statically** via AST, never runs user code on the host; (2) runtime/record mode; (3) `mvm.toml` manifest; (4) custom `flake.nix` (`mkGuest` + `mvmctl build --flake .`) — a first-class power-user path. Single user-facing Python surface: `mvm` only; the IR mvm-side/mvmd-side split stays internal. Nix lives in external `.nix` template files under `nix/` (each independently `nix flake check`-able); no large embedded Nix strings in Rust. Three rootfs inputs (Nix-built / OCI-pulled / bundle) converge on one boot contract: rootfs (`/dev/vda`) + the verity-sealed runtime overlay (ADR-051) → guest agent on vsock.

### 7. Boot performance budget

Workload microVMs start as fast as possible — **target sub-150 ms** (the public number), **< 300 ms acceptable** on slower backends; Firecracker + libkrun are the focus. The #1 lever is a tiny kernel + clean external Nix templates + busybox PID-1 + squashfs; snapshot/restore (a required `VmBackend` capability, §2 trait seams) gives warm starts well under target. A benchmark harness measures cold + warm boot per-backend and **flags regressions** — tracked, not a build-failing absolute gate (hosts differ). The persistent builder/dev VM is exempt (amortized across a session). Baseline blocker to fix first: the `default-microvm` image cannot boot the admitted path today.

**macOS code-signing is the dominant cold-start cost — and a measured one.** On macOS the kernel validates an executable's code signature **per page** the first time `dyld` `mmap()`s it; when the launch path copies the supervisor + the libkrun/libkrunfw dylibs (~20+ MB) to a **fresh inode**, that validation can dominate boot — independent analysis of a libkrun-based runtime measured **~1.45 s (≈70 % of cold start)** for ~20 MB of pre-`main` dylibs, while the OS sandbox itself added only ~5 ms. mvm uses the same libkrun/libkrunfw and copies binaries, so it is exposed to the same cost. **Levers:** keep the supervisor + dylibs at **stable, warm inodes** (don't re-copy per launch); **pre-warm** the kernel code-signing cache with a first-exec warmup (measured to drop a cold copy from ~850 ms to ~34 ms); prefer `exec`ing the same on-disk binary over copying it. The benchmark harness must **attribute boot per phase** (pipeline setup → spawn-to-`main` → VMM FFI/`dlopen` → kernel boot + guest-ready), not just report a total, so this regression class stays visible.

### 8. Claim → CI-gate → code-location map

The CI claim-gates hardcode symbols/paths. Collapsing crates renames those paths, so **every rename updates its gate in the same commit.** This table is the canonical map; it is re-verified after each Stage D plan lands.

| Claim | CI gate (workflow / job / lint) | Code location after the rewrite | Rename risk |
|---|---|---|---|
| 1 no host-fs beyond shares | workspace tests; launch path | `mvm-hostd` (launch) + `mvm-vm-sidecar` | low |
| 2 no guest uid-0 elevation | tests (`setpriv --no-new-privs`, RO `/etc`) | `mvm-guest` + launch path | low |
| 3 tampered rootfs fails to boot | `security.yml::verified-boot-artifacts` + live-KVM tamper regression | `mvm-build` (dm-verity sidecar, `mvm-verity-init` initramfs) | path moves with `mvm-build` |
| 4 no `do_exec` in prod agent | `ci.yml::prod-agent-no-exec` (greps the agent's `do_exec` symbol) | `mvm-guest` (agent) | **symbol grep must track `mvm-runner` → `mvm-guest` fold** |
| 5 vsock framing + supervisor-config fuzzed | `crates/mvm-guest/fuzz`, `crates/deps/libkrun-sys/fuzz`, `mvm-vm-sidecar/fuzz`; `deny_unknown_fields` | `mvm-guest`, `crates/deps/*-sys`, `mvm-core::framing` | fuzz crate paths move |
| 6 pre-built dev image hash-verified | `download_dev_image` test (`MVM_SKIP_HASH_VERIFY`) | `mvm` (runtime) / `mvm-cli` | low |
| 7 cargo deps audited + reproducible | `deny.toml` + `deny`/`audit` jobs; reproducibility double-build | workspace + CI | low |
| 8 signed audited ExecutionPlan | workspace tests (`synthesize_plan`, `host_signer`, `admit_for_run`, `AuditEmitter`); `mvmctl audit verify`; `xtask check-no-display-on-secret-types`; **new** `xtask` key-symbol-linkage lint (§3) | `mvm-core::plan` (signing) + `mvm-hostd` (host-signer / audit-signer / supervisor bins) | **`mvm_plan::*` → `mvm_core::plan::*`; signer bins move to `mvm-hostd`** |
| 9 content-addressed bundles | bundle rejection-ladder tests (`read_and_verify_bundle`, `verify_plan_bundle`) | `mvm-core::plan::bundle` | `mvm_plan::bundle` → `mvm_core::plan::bundle` |
| 10 default-deny egress | `policy_default_is_deny_all`, `test_resolve_network_policy_default_is_deny_all`; `mvmctl up` warning (`MVM_ACK_UNRESTRICTED_NETWORK`) | `mvm-core::policy` + `mvm-network` | `mvm_policy::*` → `mvm_core::policy::*` |
| 11 sealed app-deps volume | `ci.yml::app-deps-audit` (`verify_sealed_volume`, `apply_install_gate`) | `mvm-build` + `mvm-storage` (sealed volume) + `mvm-sdk` (deps_audit) | deps_audit moves with the SDK fold |
| 12 broker binding-gated dispatch | `service_call_denied_*` tests; `xtask check-handler-adr-coverage` / `-policy-schema` / `-composition`; `mvm-hostd/fuzz` (`fuzz_service_call`) | `mvm-hostd` (broker bin) | handler-lint paths move to `mvm-hostd` |
| 13 no raw secret over broker | `host_secrets_*` / `zeroize_drop_*` / memory-hygiene tests | `mvm-hostd` (broker bin) | move with broker |
| 14 OCI provenance in audit chain | `ci.yml` `oci-*` lanes; `security.yml::fuzz` (`unpack_layer`) | `mvm-oci` + `mvm-build` (`materialize_to_ext4`) + `mvm-hostd` (`emit_oci_provenance`) | audit emit moves to `mvm-hostd` |

### 9. Cross-cutting concerns (the architecture ADR owns naming them; per-workstream plans own building them)

Version/compat matrix (host `mvmctl` ↔ guest agent ↔ runtime overlay ↔ vsock protocol; versioned envelopes, `deny_unknown_fields` fail-closed — ADR-043/053). Testing pyramid: pure unit / hermetic (`MockBackend` + `ExampleBackend`, ADR-045) / live-KVM lanes + `cargo-fuzz`; rebuild all tiers, drop none. Metering/observability (ADR-040: vCPU/RAM/disk/egress/build-minutes; aggregation is mvmd's) + a structured tracing strategy + a perf/size budget dashboard (boot, build cold/warm, image size, binary size, dep-count vs the 723-package baseline). `NetworkProvider` owns provisioning + **both ingress and egress** default-deny policy + DNS + audit. Programmable storage: sealed/encrypted/content-addressed/snapshot-upper volumes; local + encrypted impls in-repo. Error taxonomy: typed errors with stable codes + actionable messages, no silent hangs (ADR-053); `mvmctl doctor` as the one diagnostic. Builder/dev VM lifecycle: one persistent VM, idle timeout + watchdog + orphan sweep, libkrun-vs-Vz selection. On-disk state migration: pre-1.0 — never silently corrupt a chain-signed audit log; document the choice. Docs match functionality: website docs update in the same plan that changes a CLI/behavior. Resource governance / DoS: per-workload CPU / memory(balloon) / PID / timeout ceilings + pre-deserialize frame-size caps + broker/agent rate-limits; on Linux the jailer (`mvm-jailer-lite`) grows a **cgroup v2** ruleset (optionally AppArmor) alongside seccomp + Landlock so the ceilings are kernel-enforced, not advisory. Repo conventions: a per-crate `CLAUDE.md` / `AGENTS.md` context file in each crate dir (AI-navigability), and a `docs/investigations/` directory capturing multi-bug post-mortems (e.g. end-to-end boot-bringup fix chains) as committed learnings.

## Patterns borrowed and deliberate divergences

A close analog (an embeddable AI-agent compute substrate) informed several choices; the *patterns* are adopted, no external project is named in this repo.

**Borrowed:** a subprocess-per-VMM model wrapped by a per-process jailer (validates §3 and the existing `mvm-vm-sidecar` + `mvm-jailer-lite`); a single-`RwLock` runtime-state model + lock-free `AtomicU64` metrics (clean concurrency → feeds metering); lazy initialization (handle returns instantly, heavy work on first use → boot-DX); a centralized error enum + `Result` alias (→ the §9 error taxonomy); a pluggable-VMM / network-backend / volume-factory trait set with "implement + register" extensibility (validates `VmBackend` / `NetworkProvider` / `StorageProvider` + the `ExampleBackend` stub); a `-sys` `deps/` directory for FFI (§2); a shared crate for cross-cutting types (= `mvm-core`); the embeddable-library / "no daemon in the core" philosophy (validates §4: `mvm` = library + CLI, `mvmd` = the daemon). A second-pass deep read adds: a **modules-over-crates** discipline (lean analogs carry whole subsystems as modules of one crate — §1); a **per-phase boot-latency methodology** plus the measured **macOS per-page code-signing cold-start penalty** and its warm-inode / cache-prewarm levers (§7); a **richer jailer** (cgroup v2 + AppArmor on top of seccomp + Landlock → §9 resource governance); an egress **CA** for name-constrained TLS (validates ADR-006); and the per-crate context-file + `docs/investigations/` conventions (§9).

**Diverged on purpose:** host↔guest transport stays the **lean vsock framing + Noise**, not gRPC/tonic (ADR-063 drops `tokio`/`serde_json` from the agent; gRPC would re-bloat it). The rootfs stays **Nix-deterministic** with OCI as import-only (`mvm-oci`), not an OCI/libcontainer-centric guest. And runtime state stays **file-based + chain-signed JSONL**, not a SQLite-backed mutable store — a mutable DB would weaken the tamper-evident audit log that *is* the product.

**Build-layer — move off the heavy `microvm.nix` substrate.** v2's image build is layered on `microvm.nix` (ADR-013 / CHANGELOG), which produces *full NixOS* microVMs (systemd PID-1, large closure) — too heavy for the slim busybox / tiny-kernel base the boot budget (§7) demands. The rewrite **replaces it with a slim `mkGuest` build**: a minimal non-NixOS rootfs assembled with `mkfs.ext4 -d <staged-dir>` (populate-at-format, ADR-065). Worth keeping from the `microvm.nix` design: its per-hypervisor **runner** abstraction (validates `VmBackend` — "add a backend = add a runner") + the hypervisor restriction matrix (e.g. Firecracker: no 9p/virtiofs shares); **erofs** as a read-only-root option to measure against squashfs (smaller vs faster); and the read-only-root + writable-overlay model, which validates ADR-051's runtime overlay as the **transparent, image-source-agnostic agent injection** (slim base + agent-on-overlay = "every nix gets the agent" without `mkGuest` baking it in).

**Naming cleanups (kill the "sidecar" overload).** Three unrelated things were all called "sidecar": the dropped REST daemon (mvmd's), a per-VM helper process, and a build-artifact metadata file. Rewrite renames: the metadata file type `ArtifactSidecar` → **`ArtifactManifest`** (`mvm-meta.json` stays), and the per-VM crate/process `mvm-vm-sidecar` → **`mvm-vm-host`**. **The per-VM process is the VM-host, not a bolted-on sidecar** — every microVM runs in one host process (one hypervisor process per VM); it becomes *two* only for libkrun's `start_enter` takeover (Vz and Firecracker don't take over the caller) or an external gateway's audit bridge (Firecracker + passt). Aim for **one process per VM**; on macOS 26+, Vz's no-takeover model is the reason to prefer it over libkrun where available.

**Lima — a future test/dev-tier `VmBackend`** (owner-refined 2026-05-31): re-addable via the `VmBackend` trait like any backend, carrying a **test/dev-only `BackendSecurityProfile`** (admission-visible, **prod-refused** — like the Docker fallback tier) so prod admission can never silently land on it. That single impl serves the Linux/KVM **test environment** now (a virtual `/dev/kvm` for Firecracker E2E that can't run on the builder VM or GitHub-hosted runners) *and* is the clean on-ramp for the possible future broader path. Not built in this rewrite; never used for builds/evals (AGENTS.md). The live-KVM testing tier (§9) may use it.

**Adjacent-project survey (inspiration only; no external names in-repo).** A scan of adjacent secure-microVM / agent-sandbox projects validated the core direction — host-side secret proxy (= claims 12/13), warm VM pools, pause/resume-to-object-store, a control-plane/data-plane split for the fleet daemon, and per-tenant identity/mTLS *issued by a control plane* (not a generic sidecar mesh) — and surfaced concrete techniques to fold into the Stage C build / UX / CLI plans: `mkfs.ext4 -d` populate-at-format for the slim rootfs (already in ADR-065), a minimal PID-1 init-detection ladder, **named security-profile capability matrices** over the policy / `NetworkProvider` / `StorageProvider` seams, a **per-backend latency/capability tradeoff table** surfaced by `doctor`, and a terse **`--secret NAME:host`** CLI binding over the broker. Cautions reinforced: no SSH into guests, no unsigned/unaudited execution, no container fallback that dilutes the isolation claim, and every advertised claim must be CI-gate-enforced, not merely branded.

## Alternatives considered

- **Multicall single binary (re-exec into roles).** One `mvmctl` binary that re-execs itself per role — best raw DX. **Rejected:** all role code (incl. key handling) lives in one binary image, which weakens the claim-8 guarantee from "key code absent from the broker binary" to "OS-denied at runtime," and enlarges the user-facing CLI's attack surface. Separate binaries + one supervising process (§3) gives the single-entry-point DX *and* the strongest binary-symbol guarantee, so multicall is unnecessary.
- **Crate aggressiveness tiers A (~14) and C (~20–22).** A over-merges crates with different dep closures / OS gates (hurts the cross-platform green-build invariant and "one crate, one purpose"); C leaves obvious sprawl. **Tier B (~17, §1) chosen.**
- **Fresh ADR numbers for the consolidations.** Give every consolidated cluster a new number. **Rejected** in favor of preserving the canonical ADR per cluster (046/062/007/051) and archiving the rest — keeps inbound references valid (the brief's "newcomer reads it in an afternoon" goal).
- **Merging any two host-side key processes.** Would void claims 8/12/13 (a single broker-parser bug could leak both signing keys). **Rejected** — the four-process split is the moat.

## Consequences

### Positive
- One conceptual model: role-named crates, trait-fronted, FFI bracketed under `crates/deps/`. A newcomer reads the architecture in an afternoon.
- The strongest claim-8 verification (separate binaries → binary-symbol lint) *and* single-entry-point operational DX (one supervisor process).
- Adding a backend / network / storage / FFI binding is a local change (a new impl + optional `crates/deps/*-sys`), no architectural churn.
- The library surface (facade + traits + `mvm-core` types) is a clean contract for mvmd; the REST/remote surface leaves this repo entirely.

### Negative
- A large, mechanical migration: every crate rename ripples to consumers (including mvmd, a separate repo) and to the CI claim-gates. §8 is the safety net; gate updates must be same-commit.
- `mvm-network` is net-new and the biggest single lift (pulls scattered egress/DNS/audit/provisioning together).
- `mvm-core` dep-purity must be guarded (no async/runtime deps may enter while folding plan/policy/security in).
- Linux-only `[[bin]]`s now in `mvm-build` and `mvm-vm-sidecar` must stay inert cfg-stubs on macOS/Windows.

### Neutral
- Crate *count* drops 32 → 17 (+ `crates/deps/*-sys`); host *binary* count is one supervising process tree + the Swift Vz supervisor + the embedded guest/builder binaries.

## ADR consolidation (this ADR's place in the set)

ADR-066 is the architecture spine; it does not itself supersede the clusters. The Stage B consolidation (Plan 117 §6) folds each sequential-refinement cluster into one **canonical** ADR (which now carries a consolidation banner) and records the curation here; the **physical** archive — moving superseded files to `archive/adrs/`, marking their status, and writing `archive/adrs/INDEX.md` — is the mechanical **Stage E** pass (§6.5), so the live tree isn't disrupted mid-rewrite.

**Named cluster consolidations (canonical banners added 2026-05-31):**
- Builder VM → **046** — absorbs 013 (Nix pivot), 057 (symmetric builder VM), 065 (single builder/dev image).
- Broker → **062** — absorbs 049 (secret substitution), 059 (architecture), 061 (hardening).
- Entrypoints → **007** — absorbs 008 + 010 (function-service-factories duplicate pair) + 011 (control protocol).
- Images & runtime overlay → **051** — absorbs 039 (overlay composition), 050 (OCI verity).

**Dead/forbidden → archive:** 054 (ur-seed — removed; never reintroduce).

**Further consolidation candidates (recorded; executed in Stage E):** 005 (sealed builder image) → 046; 044 (audit-emit-macro) → 041; 060 (pid0-portability) → 063; 012 (provider-CLI-contract — superseded by §1/§4) → 066.

**Flagged for owner review (arguably independent — not folded unilaterally):** 003 (local MCP server), 035 (feature-flag taxonomy), 052 (user-defined base-image registry), 056 (Vz backend). Each *could* fold but encodes a distinct decision; merging them just to hit a number risks losing nuance.

**Left separate (orthogonal, never merge):** 004/006/064 (egress policy / name-constrained CA / network audit), 027/042 (encryption scope vs mechanism), 002/048 (claims table vs positioning), 014 (VmBackend trait — this ADR builds on it). ADR-064's `NetworkProvider` is generalized here from audit-observation to full provisioning + policy.

**Honest count:** 45 ADRs today (not the brief's ~65 — that counted the public-docs mirror). The four named clusters (−11) + dead (−1) land **33 canonical**; the further candidates (−4) reach **~29**; the flagged set (−4) could reach **~25**. **~25–30 is the safe landing from the real 45**; the brief's "~20" was relative to the mis-counted baseline and would require merging genuinely-distinct decisions — out of scope without owner sign-off.

## Implementation sequencing (→ Stage C plans)

Per-workstream plans (Stage C, authored via the writing-plans skill, numbered from ~120 per `check-spec-numbers`) sequence the build, **core demo first**: (1) core demo — hello-world → booting microVM via the persistent builder VM, on a clean spine; (2) crate consolidation + the `core::` dedups; (3) encryption layering + key lifecycle; (4) `NetworkProvider` + `StorageProvider` (incl. the encrypted impl); (5) guest agent (lean-Rust) + runtime overlay; (6) CLI surface (≤15 nested) + SDK derivation engine; (7) `mvm-hostd` separate-binaries + supervising process + jailer + the key-symbol lint; (8) metering/observability + the boot/size benchmark harness; (9) testing pyramid + fuzz parity + the §8 claim-gate migration. Each lands with CI green and all 14 claim gates intact; the §8 map is re-verified after each.
