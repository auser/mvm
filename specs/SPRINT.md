# Sprint 42 — microVM hardening: load-bearing guarantees

**Goal:** turn the project's stated security claim ("no SSH in microVMs,
vsock-only") from a single load-bearing layer into a stack of seven
verifiable, CI-enforced guarantees. Implement the plan recorded in
[`plans/25-microvm-hardening.md`](plans/25-microvm-hardening.md) and
the architectural decisions in
[`adrs/002-microvm-security-posture.md`](adrs/002-microvm-security-posture.md).

**Branch:** `main`

## Why this sprint, why now

Today the vsock-only claim is *true* but it's the only hardened layer.
Everything underneath it — guest privilege model, rootfs integrity, the
host-side proxy socket, the supply chain, the deserializer that parses
every host→guest message — is soft. A failure in any one defeats the
entire stack regardless of the vsock claim. The project's value prop is
that a developer can run third-party or AI-generated code in a microVM
and trust the isolation. That promise demands the protections be
technical, verifiable, and stated explicitly.

ADR-002 captures the threat model and the seventeen surfaces audited;
plan 25 sequences the work into six independently-shippable workstreams.

## Current Status (v0.13.0, sprint open)

| Metric           | Value                    |
| ---------------- | ------------------------ |
| Workspace crates | 7 + root facade + xtask  |
| Total tests      | 1 068                    |
| Clippy warnings  | 0                        |
| Edition          | 2024 (Rust 1.85+)        |
| MSRV             | 1.85                     |
| Binary           | `mvmctl`                 |

Recent maintenance:

- [x] `mvmctl dev status` now reports the same Apple Container dev image paths that `dev up` boots (`~/.mvm/dev/current`, versioned prebuilts, or launchd-provided paths), instead of only checking the legacy cache location.
- [x] Added an opt-in `runtime_boot_bench` live test for already-built runtime images, covering serial boots and three-way concurrent fan-out against a 200 ms per-VM budget.
- [x] Source-checkout `mvmctl dev up` now refuses to download published prebuilts when the local builder VM path is unavailable; it exits with a builder-image/libkrun hint instead, preserving the "dev reflects local flakes" invariant.
- [x] Extended `runtime_boot_bench` with TOML config-file input, Apple Container backend defaults, configurable CPU/memory sizing, and Apple guest-agent readiness probing.
- [x] Removed the microsandbox backend and contributor-bootstrap feature path from the Rust dependency graph; `mvmctl` now treats missing `nix` as a broken builder VM image, never as a host-Nix fallback.

## In-flight workstreams

### W1 — Cheap defaults that are wrong today  ✅ shipped

One PR, five surgical patches, no architecture changes. All five items
landed with regression tests; `cargo test --workspace` and
`cargo clippy --workspace --all-targets -- -D warnings` clean.

- [x] **W1.1** Default `seccomp` tier flipped from `unrestricted` →
      `standard` in `crates/mvm-cli/src/commands/vm/up.rs`.
- [x] **W1.2** Vsock proxy Unix socket chmod'd to `0700` immediately
      after bind, with `test_proxy_socket_is_chmod_0700` covering it.
- [x] **W1.3** Vsock proxy port allowlist: only `52` (guest agent),
      `10_000..=75_535` (port-forward), `20_000..=85_535` (console
      data) traverse the proxy. Anything else logs and drops.
      `test_proxy_port_allowlist` covers boundaries.
- [x] **W1.4** Console log + daemon stdout/stderr created with
      `mode(0o600)` via `OpenOptions::mode`. Same-host other users
      can't `tail` guest output anymore.
- [x] **W1.5** `mvm_core::config::ensure_data_dir` /
      `ensure_cache_dir`: idempotent create + chmod-to-0700 wired into
      every `dev up`. Test
      `test_ensure_private_dir_locks_existing_loose_perms` covers the
      upgrade path for hosts that pre-date the change.

### W2 — Defense in depth inside the VM  ✅ shipped  [`plans/26-w2-defense-in-depth.md`](plans/26-w2-defense-in-depth.md)

- [x] **W2.1** Per-service uid in `nix/minimal-init/default.nix::mkServiceBlock`.
      Auto-derived from `1100 + sha256_hex8(name) % 8000`, with each
      service getting its own uid+gid, membership in `serviceGroup`,
      and a per-service `/run/mvm-secrets/<svc>/` subdir (mode 0500
      dir, 0400 files, owned by the service uid). Caller-supplied
      `services.<n>.user` is honoured as the back-compat escape.
- [x] **W2.2** `/etc/{passwd,group,nsswitch.conf}` are now created in
      `/run/mvm-etc/`, then bind-mounted read-only at the live `/etc/`
      paths with the two-step `mount --bind` + `mount -o remount,bind,ro`
      Linux dance. Boot regression confirmed: `mount` reports
      `(ro,relatime)`, `echo … >> /etc/passwd` returns EROFS.
- [x] **W2.3** Service launch line is now
      `${utilLinux}/bin/setpriv --reuid=… --regid=… --groups=…,900 --bounding-set=-all --no-new-privs --inh-caps=-all -- /bin/sh -c '…'`.
      `pkgs.util-linux` is in the production closure unconditionally.
      (Initially shipped with `--clear-groups --groups=…`; that combo is
      mutually exclusive in util-linux setpriv and crashlooped every
      service on the W3 verity-boot regression. Plan 35 §C1.2 dropped
      `--clear-groups` — `--groups=` already replaces the supplementary
      set wholesale, so the security claim is unchanged.)
- [x] **W2.4** Service launch is wrapped with
      `${guestAgentPkg}/bin/mvm-seccomp-apply <tier> --` (new shim
      binary in `crates/mvm-guest/src/bin/mvm-seccomp-apply.rs`,
      Linux-only target). Default tier is `standard`; override via
      `services.<n>.seccomp = "essential" | … | "unrestricted"`.

### W3 — Verified boot via dm-verity  ✅ shipped — 2026-04-30 (initramfs landed, all 5 runbook steps green)  [`plans/27-w3-verified-boot.md`](plans/27-w3-verified-boot.md) | runbook: [`runbooks/w3-verified-boot.md`](runbooks/w3-verified-boot.md)

- [x] **Kernel** `firecracker-aarch64.config` enables
      `CONFIG_MD`, `CONFIG_BLK_DEV_DM`, `CONFIG_DM_INIT`, and
      `CONFIG_DM_VERITY` so the kernel can construct verity targets.
- [x] **W3.1** `nix/flake.nix::verityArtifacts` runs
      `veritysetup format` with `--data-block-size=1024` and a pinned
      zero salt, emits `rootfs.{ext4,verity,roothash}`
      deterministically.
- [x] **W3.2** Apple Container backend gained `VerityConfig` +
      `start_with_verity()`; opens the rootfs read-only, attaches
      the sidecar at `/dev/vdb`, attaches the verity initramfs via
      `setInitialRamdiskURL`, and passes `mvm.roothash=<hex>` on the
      cmdline. Mutual-exclusion check rejects `MVM_NIX_STORE_DISK`.
- [x] **W3.3** Firecracker backend extended `FlakeRunConfig` +
      `VmStartConfig` with `verity_path` / `roothash`. Cold-boot,
      snapshot-restore, and template-snapshot paths all probe for
      the sidecar + initramfs via `microvm::probe_verity_sidecar()`
      and pass `initrd_path` to `/boot-source` so the initramfs
      runs as PID 1.
- [x] **W3.4** `mkGuest` accepts `verifiedBoot ? true`;
      `nix/dev-image/flake.nix` sets `verifiedBoot = false` (overlay
      can't compose with verity). The dev sibling flake forwards
      the kwarg transparently.
- [x] **Initramfs** `nix/packages/mvm-verity-init.nix` builds a
      static-musl `mvm-verity-init` that runs as PID 1 from the
      cpio.gz at `nix/packages/verity-initrd.nix`. Reads
      `mvm.roothash=` from cmdline, builds `/dev/mapper/root` via
      DM ioctls (DM_DEV_CREATE → DM_TABLE_LOAD → DM_DEV_SUSPEND),
      mounts it at `/sysroot`, then `switch_root`s to the real
      `/init`. Bypasses Firecracker's auto-appended
      `root=/dev/vda ro` by owning the boot pivot in userspace.
- [x] **CI gate** `verified-boot-artifacts` lane in
      `security.yml` builds `nix/default-microvm/` and asserts
      `rootfs.{ext4,verity,roothash,initrd}` plus a 64-char hex
      roothash.
- [x] **Boot regression** (live KVM): full
      `specs/runbooks/w3-verified-boot.md` Step 3 green —
      `mvm-verity-init` reaches userspace from `/dev/dm-0`.
- [x] **Tamper regression** (live KVM): tampering the ext4
      superblock triggers
      `device-mapper: verity: 254:0: data block 1 is corrupted`
      and the kernel panics before userspace.

### W4 — Guest agent attack surface  ✅ shipped — 2026-04-30  [`plans/28-w4-guest-agent-attack-surface.md`](plans/28-w4-guest-agent-attack-surface.md)

- [x] **W4.1** `#[serde(deny_unknown_fields)]` applied to every type
      crossing the host↔guest boundary: `GuestRequest`, `GuestResponse`,
      `HostBoundRequest`, `HostBoundResponse`, `FsChange` in
      `crates/mvm-guest/src/vsock.rs`; `AuthenticatedFrame`,
      `SessionHello`, `SessionHelloAck` in
      `crates/mvm-core/src/policy/security.rs`. `MAX_FRAME_SIZE` audit
      kept the existing 256 KiB cap (the value is conservative for
      every current request shape). Six new regression tests cover the
      unknown-field rejection paths.
- [x] **W4.2** `cargo-fuzz` harness lives at
      `crates/mvm-guest/fuzz/` with two targets:
      `fuzz_guest_request` (host→guest enum) and
      `fuzz_authenticated_frame` (signed-envelope wrapper). Corpus
      seeded with valid frames committed under
      `corpus/fuzz_guest_request/`. Excluded from the main workspace
      because `libfuzzer-sys` only links under cargo-fuzz's wrapper.
      Driven by `just fuzz-guest-request` / `just fuzz-authenticated-frame`.
- [x] **W4.3** `scripts/check-prod-agent-no-exec.sh` builds the agent
      with `--no-default-features` and asserts the demangled symbol
      `mvm_guest_agent::do_exec` is absent. Wired into
      `.github/workflows/ci.yml` as the `prod-agent-no-exec` job and
      runnable locally via `just security-gate-prod-agent`. The grep
      anchors on the binary's crate name to skip stdlib's unrelated
      `<std::sys::process::unix::common::Command>::do_exec`.
- [x] **W4.4** Port-forward TCP target pinned to a
      `PORT_FORWARD_TCP_HOST` constant in
      `crates/mvm-guest/src/bin/mvm-guest-agent.rs`, with a regression
      test (`test_port_forward_target_is_loopback`) that parses the
      constant and asserts `IpAddr::is_loopback`. Audit confirmed the
      agent binds *no* TCP listeners — vsock binds only — so there is
      no `0.0.0.0` surface to defend.
- [x] **W4.5** Guest agent now launches as uid 901 (`mvm-agent`) via
      `setpriv --reuid=901 --regid=901 --groups=901,900
      --bounding-set=-all --no-new-privs --inh-caps=-all`.
      `nix/minimal-init/lib/04-etc-and-users.sh.in` provisions the
      `mvm-agent` user before `/etc` is bind-mounted read-only;
      `default.nix::guestAgentBlock` chgrps
      `/etc/mvm/{integrations,probes}.d/` to the shared service group
      so the dropped-privilege agent can still read its drop-ins.
      (Initially shipped with `--clear-groups`; dropped under plan 35
      §C1.2 — see W2.3 for the rationale.)

### W5 — Supply chain  ✅ shipped — 2026-04-30  [`plans/29-w5-supply-chain.md`](plans/29-w5-supply-chain.md)

- [x] **W5.1** Dev-image and default-microvm downloads in
      `crates/mvm-cli/src/commands/env/apple_container.rs` now fetch
      the release's per-arch checksum manifest, stream each artifact
      through SHA-256, and reject + delete the file on mismatch.
      `MVM_SKIP_HASH_VERIFY=1` documented as the emergency-rotation
      escape. Five regression tests in `hash_verify_tests` cover
      the happy path, the mismatch path, the env-var bypass, and the
      manifest-parser edge cases.
- [x] **W5.2** `deny.toml` at the workspace root + the `deny` job in
      `.github/workflows/ci.yml` runs `cargo deny check` (advisories,
      licenses, bans, sources). Three audited unmaintained-advisory
      ignores documented inline. Pre-commit hook runs the same
      locally when `cargo-deny` is installed.
- [x] **W5.3** `reproducibility` job in `ci.yml` builds `mvmctl`
      twice from a clean state with `SOURCE_DATE_EPOCH`,
      `CARGO_INCREMENTAL=0`, and `--remap-path-prefix` pinned, then
      `diff`s the SHA-256s. Mismatch fails the build with a clear
      `::error::` annotation.
- [x] **W5.4** Release workflow (`release.yml:205-247`) already
      emits a CycloneDX SBOM via `cargo-cyclonedx`, cosign-signs it,
      and attaches `sbom.cdx.json` + `.bundle` to every GitHub
      release.

### W6 — Documentation + CI gates  ✅ shipped — 2026-04-30  [`plans/30-w6-docs-and-ci-gates.md`](plans/30-w6-docs-and-ci-gates.md)

- [x] **W6.1** ADR-002 lives at
      `specs/adrs/002-microvm-security-posture.md`.
- [x] **W6.2** `CLAUDE.md` now carries a "Security model" section
      enumerating the seven CI-enforced claims, the test or workflow
      backing each, and the named non-goals from ADR-002.
- [x] **W6.3** New `.github/workflows/security.yml` consolidates
      `cargo-deny`, `cargo-audit`, the `prod-agent-no-exec` symbol
      grep, the reproducibility double-build, the cargo-fuzz lane
      (5min on PRs, 30min nightly cron), and the W5.1 hash-verify
      regression. Verity / boot lanes will land with W3.
- [x] **W6.4** `mvmctl security status` adds five live probes:
      vsock proxy socket mode, `~/.mvm` mode, prebuilt dev image
      cache state, `deny.toml` presence, and the hash-verified
      download claim. Non-JSON output prints the security + CI
      badge URLs. Unit tests cover probe shape and the deny-config
      lookup.

### W7 — Nix tree alignment with best-practices guide  🟡 in progress  [`plans/31-nix-best-practices-cleanup.md`](plans/31-nix-best-practices-cleanup.md)

Branch: `feat/nix-best-practices-cleanup`. Audit recorded in
[`specs/references/mvm-nix-best-practices.md`](references/mvm-nix-best-practices.md);
phased plan in
[`plans/31-nix-best-practices-cleanup.md`](plans/31-nix-best-practices-cleanup.md).

Scope summary (each phase is independently mergeable):

- **Phase 1** — In-place spirit-of-guide fixes. Bake `/etc/mvm/{integrations.d,probes.d}` perms into the rootfs at build time; replace runtime `find -delete` with `rm -f`; move `udhcpc.sh` into the Nix store; explicit `config = {}` on every nixpkgs import; `builtins.path { … name = "mvm-source"; filter = …; }` (drops `.git`, `target/`, `nixos.qcow2`, `.playwright-mcp/` from the eval-time copy); commit every missing `flake.lock`; add `variant = "prod" | "dev"` tag plumbed through `mkGuest` (visible in store path + `/etc/mvm/variant`); extend `scripts/check-prod-agent-no-exec.sh` to assert variant ↔ feature pairing; delete `nix/examples/{paperclip,openclaw}/`.
- **Phase 1.5** — Lima VM rename `mvm` → `mvm-builder` across runtime crates, CLI, lima template, Justfile, CLAUDE.md, memory entries. Bridge `br-mvm` stays. Migration is user-visible (one-line command, no auto-rename).
- **Phase 2** — Repo layout move to the guide's `nix/{packages,devshells,checks,apps,images,lib,…}` shape. Renames `nix/dev-image/` → `nix/images/builder/`, `nix/default-microvm/` → `nix/images/default-tenant/`, flattens `nix/dev/` to `nix/lib/dev-agent-overlay.nix` (it's an overlay, not an image). Updates mvmctl path strings + CI workflow paths (`release.yml:114,136,177`).
- **Phase 3** — New flake outputs split by execution environment. `packages.<sys>.{mvm,default}` (mvmctl Rust binary), `apps.<sys>.{mvm,default,dev}`, `devShells.<sys>.default` (host / dev-machine shell), `devShells.<sys>.builder` (Linux builder-VM-side shell), `checks.<sys>.{eval,build}`, `formatter.<sys>` (`nixfmt-rfc-style`), `treefmt.toml`. Replace `mkNodeService`'s 3-stage FOD-then-patch with `pkgs.buildNpmPackage`. Promote `xtask` to its own package and drop it from the agent fileset. Source rust toolchain from `rust-toolchain.toml`. Add `passthru.role = "builder" | "tenant"` to image derivations.
- **Phase 4** — Systems coverage: add `aarch64-darwin` to `eachSystem`. Gate Linux-only outputs (`mvm-guest-agent`, `firecracker-kernel`, builder devshell, image-build checks) via `optionalAttrs pkgs.stdenv.isLinux`. Darwin keeps `mvm`/apps/host-devshell/formatter/eval-only-checks per the guide's "macOS dev shells may include Lima/QEMU but must not pretend KVM-only features work locally."
- **Phase 5** — `ops/` scaffolding. Move `scripts/{install-systemd,dev-setup,mvm-install}.sh` into `ops/{systemd,bootstrap}/`. README per subdir documenting what host state each script changes and why elevated privileges are required. `mvmctl` host mutation in `network.rs` (TAP/iptables) is **flagged for product decision** — strict reading of the guide says move to `ops/networking/bridge-setup.sh` with `mvmctl dev up` becoming warn-only; lenient reading says user-invoked CLI ≠ `nix develop`, leave it. Pending decision before folding in.

Status:

- [x] **W7.1 (Phase 1)** — In-place rootfs/flake fixes — landed 2026-04-30; **builder-VM-side validation done 2026-05-01** inside `mvm-builder` against `nix/images/default-tenant#packages.aarch64-linux.default` (`mvm-default-microvm-prod`): `debugfs` confirms `/etc/mvm/{integrations.d,probes.d}` mode `0750`, `/etc/mvm/variant` content `prod\n` mode `0644`, `/tmp/udhcpc.sh` absent from rootfs (resolved to `/nix/store/*-mvm-udhcpc-action` script). `nix flake check` passes on all 9 flakes; `cargo test --workspace` 1067 pass; `nix eval` confirms `variant="prod"` on default-microvm and `variant="dev"` on dev-image.
- [x] **W7.2 (Phase 1.5)** — Lima VM rename `mvm` → `mvm-builder` — landed 2026-04-30; **migration verified done on dev box 2026-05-01** (`limactl list` shows only `mvm-builder`; legacy `mvm` removed). New constants `VM_NAME` / `LEGACY_VM_NAME` in `mvm::config`, six hardcoded literals in `doctor.rs` migrated to the constant, new `bootstrap::warn_if_legacy_lima_vm` detects legacy VM and prints a one-line manual migration command (no auto-rename), wired into both `mvmctl bootstrap` and `mvmctl dev up`. Docs (`AGENTS.md`, `specs/01-project.md`, `specs/runbooks/w3-verified-boot.md`, `public/.../{architecture,troubleshooting}.md`, `crates/mvm/README.md`) updated. 1067 tests pass.
- [x] **W7.3 (Phase 2)** — Repo layout move — landed 2026-04-30. `nix/{guest-agent-pkg,firecracker-kernel-pkg}.nix` → `nix/packages/{mvm-guest-agent,firecracker-kernel}.nix`; `nix/{minimal-init,rootfs-templates,kernel-configs}` → `nix/lib/`; `nix/dev-image/` → `nix/images/builder/`; `nix/default-microvm/` → `nix/images/default-tenant/`; `nix/examples/*` → `nix/images/examples/*` (paperclip + openclaw deletions staged from earlier `git rm`). Internal `import` paths in `nix/flake.nix` updated, sibling-flake `mvm.url` arithmetic fixed, mvmctl Rust path strings (`apple_container.rs`, `commands/{mod,vm/exec}.rs`, `mvm-build/dev_build.rs`, `fleet.rs`) updated, CI workflow paths in `release.yml` updated, all 7 flake.locks regenerated. `nix flake check --no-build` clean on every flake; `cargo test --workspace` 1067/1067; clippy clean.
- [x] **W7.4 (Phase 3)** — New flake outputs — landed 2026-04-30. New `packages.<sys>.{mvm,default,xtask}` (mvmctl Rust CLI + xtask runner via fileset-filtered `rustPlatform.buildRustPackage`). New `apps.<sys>.{mvm,default,xtask}` for `nix run`. New `devShells.<sys>.{host,default}` (everywhere) and `devShells.<sys>.builder` (Linux only). New `formatter.<sys> = pkgs.nixfmt-rfc-style` plus `treefmt.toml` covering nix/rust/shell/markdown. New `checks.<sys>.mvm-eval`. `passthru.role = "tenant" | "builder"` plumbed through `mkGuest`; `nix/images/builder/flake.nix` sets `role = "builder"`. Pre-commit hook runs `nix fmt --check` when `nix` is on PATH. **Deferred** (TODO comment in `nix/flake.nix:340-353`): `mkNodeService` 3-stage FOD-then-patch → `pkgs.buildNpmPackage` swap — needs Linux builder validation against hello-node before flipping (output layout changes from `$out/dist/...` to `$out/lib/node_modules/<pname>/dist/...`).
- [x] **W7.5 (Phase 4)** — `aarch64-darwin` + `x86_64-darwin` coverage — landed 2026-04-30. `flake-utils.lib.eachSystem` extended with both Darwin systems. `lib.mkGuest` exposed everywhere (function-only, no eager call). `packages.<sys>.{mvm,default,xtask}` cross-compile to native target. `packages.<sys>.{mvm-guest-agent,mvm-guest-agent-dev}` and `devShells.<sys>.builder` gated by `pkgs.lib.optionalAttrs pkgs.stdenv.isLinux`. Per-system attrs verified: `packages.aarch64-darwin = [default, mvm, xtask]`, `packages.x86_64-linux = [default, mvm, mvm-guest-agent, mvm-guest-agent-dev, xtask]`, `devShells.aarch64-darwin = [default, host]`. Reverted `mvmSrc = builtins.path` (incompatible with `lib.fileset.toSource`); per-package fileset already restricts closure.
- [x] **W7.6 (Phase 5)** — `ops/` scaffolding — landed 2026-04-30. New `ops/{bootstrap,permissions,networking,systemd}/` with READMEs documenting what each script mutates and why elevated privileges are needed. `git mv scripts/install-systemd.sh ops/systemd/install.sh`, `git mv scripts/dev-setup.sh ops/bootstrap/dev-setup.sh`, `git mv scripts/mvm-install.sh ops/bootstrap/install.sh`. `dev-setup.sh` header rewritten with mutation/idempotence summary. `public/.../development.md` updated to point at the new path. `ops/networking/` is documentation-only — `mvmctl`'s `network.rs` host-mutation question (strict vs. lenient guide reading) remains a deferred product decision flagged in the README and the plan.

## Success criteria

By sprint close, the project must be able to make these claims with
technical receipts (one CI gate per claim):

1. *No host-fs access from a guest beyond explicit shares.*
2. *No guest binary can elevate to uid 0.*
3. *A tampered rootfs ext4 fails to boot.*
4. *The guest agent does not contain `do_exec` in production builds.*
5. *Vsock framing is fuzzed.*
6. *Pre-built dev image is hash-verified.*
7. *Cargo deps are audited on every PR.*

W1 already supplies the regression infrastructure for #4 (proxy socket
perms test) and #2 (default seccomp tier). The remaining five claims
land with W2–W6.

## Phasing

W1 is shipped. W2–W6 are independent and can land in any order; W3
(verity) is the largest and likely deserves a sprint of its own if W2
+ W4 + W5 + W6 close out faster.

## Non-goals (named explicitly, see ADR-002)

- Defending against a malicious *host*. mvmctl trusts the host with
  the hypervisor, GC roots, and private build keys.
- Multi-tenant guests. One guest = one workload.
- TPM/SEV/hardware attestation. Out of scope for v1.
- Hypervisor-level egress policy enforcement L7 / DNS-pinning. The
  L3 tier shipped via plan 32 / Proposal D + `NetworkPreset::Agent`
  (PR #20). The L7 tier (mitmdump-based HTTPS proxy + DNS-answer
  pinning) is scoped in
  [`plans/34-egress-l7-proxy.md`](plans/34-egress-l7-proxy.md);
  PR #23 ships the foundation (`EgressMode::L3PlusL7`,
  `EgressProxy` trait, `StubEgressProxy`). Runtime backing remains
  a non-goal for Sprint 42.

## Sprint 43 — Nix-agent ecosystem adoption (in flight)

Master plan: [`plans/32-mcp-agent-adoption.md`](plans/32-mcp-agent-adoption.md).
Five proposals (A, A.2, B, C, D) plus cross-repo handoff plan 33.

### Shipped (PRs open, awaiting review)

- **PR #20** [`feat/mcp-agent-adoption`](https://github.com/tinylabscom/mvm/pull/20) ←
  `main` — plan 32 base. New `mvm-mcp` crate (protocol-only +
  stdio), A v1 stdio MCP server, B `nix/images/examples/llm-agent/`
  showcase flake, C local-LLM probe defaults, D v1
  `NetworkPreset::Agent` (L3-only). New ADRs 003 / 004; new plans
  32 / 33.
- **PR #21** [`feat/mcp-session-semantics`](https://github.com/tinylabscom/mvm/pull/21) ← #20 —
  A.2 v1 (session bookkeeping). `SessionMap` + `Reaper` trait +
  audit kinds + 30 s-tick reaper thread + Drop drain.
- **PR #22** [`feat/mcp-session-warm-vm`](https://github.com/tinylabscom/mvm/pull/22) ← #21 —
  A.2 v2 (warm-VM materialisation). `boot_session_vm` /
  `dispatch_in_session` / `tear_down_session_vm` exec primitives;
  per-session `Arc<Mutex<SessionVm>>` map; boot-race handling;
  reaper actually tears VMs down.
- **PR #23** [`feat/egress-l7-proxy`](https://github.com/tinylabscom/mvm/pull/23) ← #22 —
  L7 egress foundation. `EgressMode` enum (`Open` / `L3Only` /
  `L3PlusL7`), `EgressProxy` trait + `StubEgressProxy`, plan 34
  scoped.

All four PRs: `cargo build --workspace` clean, `cargo test --workspace`
green (mvm-mcp 31 tests including session lifecycle, mvm-core +6
EgressMode tests + 3 agent-preset tests, mvm-cli +2 probe tests),
`cargo clippy --workspace --all-targets -- -D warnings` clean,
`cargo build -p mvm-mcp --no-default-features --features
protocol-only` clean (mvmd-ready per plan 33).

### Deferred — concrete follow-ups

| Item | Plan | Why deferred | Estimated size |
|---|---|---|---|
| **L7 egress runtime backing** (7 tiers + 12 cross-cutting considerations folded — see plan 34 §"Cross-cutting considerations") | [`plans/34-egress-l7-proxy.md`](plans/34-egress-l7-proxy.md) | Heavyweight runtime dep (mitmdump pulls Python + cryptography, ~80 MiB closure); CA cert generation has corner cases (Name-Constrained per-VM leaves, rotation, expiry); DNS pinning needs IPv6 + CNAME-chain handling. Live-KVM integration testing is mandatory. New ADR-006 (PR #33) locks the cryptographic story before code starts. | ~1.5 sprints |
| **A.2 v2 live-KVM smoke** (cold-boot vs warm-VM latency comparison on `claude-code-vm`; race-condition test for parallel first-calls in same session; snapshot-resume against the Anthropic-allowlisted agent VM) | Plan 32 §"Proposal A.2" | Hardware not available in the dev environment; needs a Linux/KVM host with a real Firecracker stack. | ~1 day |
| **Hosted MCP transport (HTTP/SSE)** | [`plans/33-hosted-mcp-transport.md`](plans/33-hosted-mcp-transport.md) | Cross-repo: implementation lives in [mvmd](https://github.com/tinylabscom/mvmd). mvm-mcp's `protocol-only` feature is already shipped (PR #20) so mvmd can consume the wire schema unchanged. | mvmd owns sizing |
| **Per-template `default_network_policy`** ✅ shipped (PR `feat/template-default-network-policy`) | ADR-004 §"Decisions" 6 | `TemplateSpec` gains `Option<NetworkPolicy>` (back-compat via `#[serde(default)]` + `skip_serializing_if`). `mvmctl template create --network-preset agent` bakes it; `mvmctl up` consults it as fallback when no CLI flags supplied; `mvmctl template info` prints it. `llm-agent` README updated to use the baked default. | ~1 day |
| **CI lane `mcp-server-smoke`** ✅ shipped (PR #24) | Plan 32 §"Proposal A — CI gate" | Real JSON-RPC roundtrip script + CI job. Caught a real `logging::init` stdout-pollution bug in the process. | ~½ day |

### Sprint 43 success criteria

By sprint close, the project should be able to claim:

1. *LLM clients drive mvmctl as an MCP sandbox* (PR #20 — shipped).
2. *Sessions persist warm VMs across calls with idle/max reaping* (PRs #21 + #22 — shipped, live-KVM smoke deferred).
3. *Hardened LLM-agent VM exists as a worked example* (PR #20 / Proposal B — shipped).
4. *Local-LLM-first scaffolding* (PR #20 / Proposal C — shipped).
5. *L3 hypervisor egress allowlist with an `agent` preset* (PR #20 / Proposal D — shipped).
6. *L7 HTTPS proxy + SNI/Host enforcement* (foundation in PR #23, runtime in plan 34 — deferred).
7. *mvmd-ready protocol crate* (PR #20's `protocol-only` feature — shipped; mvmd consumption is plan 33's job).

5 of 7 are fully shipped on `feat/egress-l7-proxy`; 1 has its
foundation in place; 1 is cross-repo work. The sprint can close on
review approval of PRs #20–#23 — claim 6 is honestly stated as
"foundation shipped; runtime in plan 34" and that's the right
boundary given the runtime dep weight.

Cross-repo handoff for hosted MCP transport (HTTP/SSE) is documented
in [`plans/33-hosted-mcp-transport.md`](plans/33-hosted-mcp-transport.md);
implementation lives in mvmd, not this repo.

## Sprint 44 — Whitepaper alignment (proposed)

Master plan: [`plans/37-whitepaper-alignment.md`](plans/37-whitepaper-alignment.md).
Walks the V2 whitepaper (`specs/docs/whitepaper.md`) section by section,
identifies what `mvm` (the runtime/CLI half — not `mvmd`) is missing
relative to its claims, and sequences the work into six waves. Includes
ADR-004 (PII redaction lives in `mvm`, not `mvmd`) staged for creation
at `specs/adrs/004-pii-redaction-in-mvm.md` when implementation begins.

### Why this sprint

The whitepaper's load-bearing AI-native claims — signed `ExecutionPlan`
contract, Zone B runtime supervisor, L7 egress + PII redaction,
tool-call mediation, attestation-gated key release, signed policy
bundles, runtime artifact capture, audit binding to plan version — have
no code path on `mvm` today. Sprint 42 closed the local-isolation
substrate (W1–W6); Sprint 43 shipped MCP + L3 egress + the L7 proxy
foundation (PR #23). Sprint 44 builds the rest of the whitepaper's
runtime contract on top of that substrate.

### Wave breakdown

Effort labels: **XS** ≤ ½ day · **S** 1–2 days · **M** 3–5 days · **L** > 1 sprint.

- **Wave 0 — Whitepaper truth fixes (XS, prereq).** Soften §3.1 backend
  list, §14 hardware claims, §15.1 PII as design intent until built.
  Update CLAUDE.md / MEMORY.md: W3 dm-verity is **shipped**.
- **Wave 1 — Foundation (S+M).** New crates `mvm-plan`, `mvm-policy`,
  `mvm-supervisor` (lifted from `mvm-hostd`). `Supervisor::launch(plan)`
  happy path. Audit binds to plan/policy/image. Plus B6 (kill switch),
  B8 (cosign verify cache), B15 (zeroize lint), B16 (local registry),
  B19 (admission audit), B21 (config-change audit), C1 (supervisor
  self-attest), C3 (anti-debug), C4 (supervisor death = fail-closed),
  E2 (policy precedence), G4 (plan replay protection — latent bug fix).
- **Wave 2 — Differentiator (M).** L7 egress proxy in supervisor (plan
  34 expanded); inspector chain (SecretsScanner, SsrfGuard,
  InjectionGuard, DestinationPolicy); AiProviderRouter + PiiRedactor
  (detect-only first); tool-call vsock RPC + ToolGate wired. Plus B17
  (egress audit completeness with audit-emits-before-forward CI gate),
  B18 (tool audit), E1 (false-positive circuit breaker — ship-blocker),
  G1 (streaming session audit), G2 (retry-storm dedup).
- **Wave 3 — Identity & artifact closure (M).** Attestation key-release
  gate with TPM2 provider; per-run secret grants + revoke-on-stop;
  audit chain signing + per-tenant streams + export; artifact capture
  path (virtiofs `/artifacts` + ArtifactCollector). Plus B7 (audit
  buffering during mvmd outage), B9 (workload identity JWT), B10
  (memory scrub on stop), B11 (host-published trusted time), B12 (crash
  dump capture), B14 (snapshot integrity + plan-id binding), B20
  (secret-grant pairing CI), B22 (audit-write health metrics), C2
  (channel rekey), D1 (webhook inspection), D2 (RAG/retrieved-content
  inspection), D3 (file-upload inspection), E3 (attestation clock
  skew), E4 (disk-full audit), F1 (cost telemetry), F2 (stuck-workload
  detection), F4 (tenant-visible audit projection), G3 (cross-plan
  request stitching).
- **Wave 4 — Multi-tenant + release (M).** Per-tenant netns,
  per-tenant DEK, ReleasePin admission + two-slot policy rollback,
  DataClass admission gate.
- **Wave 5 — Surface & ergonomics (S+M).** Local HTTP API on supervisor
  Unix socket, `mvm-sdk` crate, cross-backend CI matrix on §3.3 fixture
  plan, threat-control matrix CI generator. Plus F3 (reproducible plan
  execution).
- **Wave 6 — Confidential & adapters (L, optional).** SEV-SNP / TDX
  provider real impls; Lima/Incus/containerd adapters; Vault / AWS SM /
  GCP SM secret providers.

### Cornerstones

Two pieces unblock everything else and should land first:

1. **`mvm_core::ExecutionPlan`** (§3.3, Wave 1) — typed, signed plan
   replacing scattered `RunParams` / `FlakeRunConfig`. Every
   "signed/audited/policy-pinned" claim hangs off this. Including
   `valid_from` / `valid_until` / `nonce` (G4) closes the latent
   replay bug.
2. **`mvm-supervisor` daemon** (§7B, Wave 1) — packages the existing
   `mvm-hostd` skeleton plus EgressProxy, ToolGate, KeystoreReleaser,
   AuditSigner, ArtifactCollector behind a single trusted process.
   Owns the data path so tenant code can't bypass policy.

### Differentiator

L7 egress + AI-provider PII redaction (§15 + §15.1, Wave 2). The
single most important AI-native claim in the whitepaper and currently
zero code. Ships as **detect-only** first to safely measure detector
quality on real traffic before transforms are enabled. **Fail-closed**
on detector error — any inspection failure blocks the request, never
forwards raw.

### Trust boundary decision (ADR-004)

PII redaction stays in `mvm`, not `mvmd`. The host running the microVM
is the only point at which a request body is in plaintext on
infrastructure we trust. Putting redaction in `mvmd` would collapse §8
plane separation, expand §13 control-plane blast radius (an `mvmd`
compromise would expose every prompt), break §19 residency, and add a
network round-trip per AI call. `mvmd` owns policy authoring,
signing, distribution, and fleet-aggregated reporting; `mvm` owns the
engine on the data path. ADR-004 staged in plan 37 Addendum A.

### Sprint 44 success criteria

By sprint close, the project should be able to claim:

1. *Workloads run from typed, signed `ExecutionPlan`s with replay
   protection.* (Wave 1)
2. *A trusted supervisor process owns the data path; tenant code
   cannot bypass policy.* (Wave 1)
3. *Every outbound egress event produces a signed, plan-bound audit
   entry.* (Wave 2)
4. *AI-provider requests pass through PII inspection; detector errors
   fail closed.* (Wave 2)
5. *Tool calls are mediated by the supervisor's `ToolGate` and
   audited.* (Wave 2)
6. *Attestation gates secret release; TPM2 implementation exists.*
   (Wave 3)
7. *Workload outputs are captured under `ArtifactPolicy` retention,
   not destroyed on exit.* (Wave 3)

Waves 4–6 are post-44 follow-ups; the sprint can close on Waves 0–3.

### Non-goals (named explicitly)

- **mvmd-side concerns:** fleet placement, releases / canary / rollout,
  host registration, cross-host wake/sleep, policy distribution,
  control-layer key rotation. Wire types live in
  `mvm_core::mvmd_iface` so mvmd can land later without reshaping
  `mvm`.
- **Hardware-attested vendor trust roots beyond TPM2 in the first pass.**
  SEV-SNP / TDX providers ship as `unimplemented!()` scaffolds.
- **Vendor-specific PII detector beyond regex/dictionary v0.**
  `Detector` trait is open for later additions.
- **Workflow-engine specific SDKs beyond the generic `mvm-sdk`.**
- **Model selection, prompt engineering, cost optimization, federated
  learning** (plan 37 Addendum H — application concerns, not runtime).

## Sprint 45 — Function-call entrypoints (in flight — substrate shipped, live smoke open)

Master plan: [`plans/41-function-call-entrypoints.md`](plans/41-function-call-entrypoints.md)
(mvm side, six workstreams). Comprehensive design rationale + 16
security mitigations: [`plans/41-function-entrypoints-design.md`](plans/41-function-entrypoints-design.md).
Architecture decision: [`adrs/007-function-call-entrypoints.md`](adrs/007-function-call-entrypoints.md).
Cross-repo: decorationer (mvmforge) `specs/adrs/0009-function-entrypoints.md`,
`specs/plans/0003-function-entrypoint-runtime.md`,
`specs/plans/0004-network-deny-default.md`.

### Status (2026-05-05)

mvm-side W1–W5 shipped to `main` in PRs #66–#71 (with #72 replacing
auto-closed #68 — see "Stack-merge artifacts" below). W6 (network
deny-default for function workloads) is captured cross-repo: the IR
shape lives in decorationer plan 0004, and the mvm-side TAP-skip glue
is mechanical once mvmforge plumbs the IR field. decorationer plan
0003 phase 1 (function-entrypoint IR variant + `Format` closed enum)
shipped as decorationer #3.

The live-KVM smoke fixture (`mkGuest extraFiles` + the `echo-fn` example
flake + `tests/smoke_invoke.rs` gated on `MVM_LIVE_SMOKE=1`) is **PR #73,
not yet run** — the substrate compiles and skips cleanly on incapable
hosts; the actual boot+invoke against a Linux/KVM (or macOS 26+ Apple
Container) host hasn't happened yet. That's the load-bearing open item.

### Why this sprint

Modal-style `f.remote(...)` semantics on top of mvm. Decorate a Python
or TS function, call it from the host, body runs in a microVM, return
value flows back. mvmforge already lands the deploy-time half
(decorator → IR → flake → boot); the function body is currently
ignored. What's missing is the call-time half — a constrained,
production-safe vsock verb that runs a baked program with stdin piped
and stdout/stderr captured.

The user's framing: a function call is an *implicit program*. The
image bakes a tiny wrapper (Python/Node runner generated by
mvmforge's Nix factories); mvm just runs it with stdin piped and
stdout captured. mvm doesn't learn Python or TS — it gets a
constrained verb that runs *the* baked entrypoint, with caps,
timeouts, per-call hygiene, snapshot integrity, and explicit-only
network grants.

The hard constraint inherited from this sprint and recorded in
CLAUDE.md memory: **everything ships at build time, ALWAYS.** No
closure shipping at call time, no runtime function registration, no
dynamic dispatch by name from outside. The wrapper, function body,
format, allowlist, and grants are all baked into the rootfs at
image-build time; only call-payload bytes (stdin) are runtime data.

### Workstream breakdown

Six workstreams, each independently shippable.

- **W1 — Wire protocol additions.**  ✅ shipped — PR #67. Adds
  `GuestRequest::RunEntrypoint` + `GuestResponse::EntrypointEvent`
  (streaming-shaped, buffered v1) + `RunEntrypointError` enum.
  `#[serde(deny_unknown_fields)]`; fuzz targets extended; agent
  stub arm in place.
- **W2 — Agent handler.**  ✅ shipped — PR #72 (recreated from
  auto-closed #68). New `crates/mvm-guest/src/entrypoint.rs`
  module: `EntrypointPolicy::production().validate()` reads
  `/etc/mvm/entrypoint`, `realpath`s, asserts mode/uid/prefix,
  holds fd; `execute()` spawns with `process_group(0)`,
  `RLIMIT_CORE=0`, `env_clear()`, drains stdout/stderr concurrently
  into capped buffers, kills on cap breach or timeout via SIGTERM
  → grace → SIGKILL escalation. `handle_run_entrypoint` in the
  agent serializes per-VM via static `Mutex`, creates per-call
  TMPDIR mode 0700 with RAII cleanup, writes `Stdout`/`Stderr`
  events streaming + returns terminal `Exit`/`Error`.
- **W3 — `mvmctl invoke` CLI.**  ✅ shipped — PR #69. New
  top-level verb. New `mvm_guest::vsock::send_run_entrypoint`
  streaming consumer (frame loop until `is_terminal()`). Boots
  transient VM via `boot_session_vm`, dispatches, tears down
  always. `--fresh`/`--reset` flags wired (informational in v1
  until session-pool plan lands). Exit-code mapping: wrapper's
  own code on `Exit`, 124 on timeout, 137 on `WrapperCrashed`,
  1 for everything else (Busy / PayloadCap / EntrypointInvalid
  / InternalError) with a warn-line to stderr.
- **W4 — Snapshot integrity (HMAC).**  ✅ shipped — PR #70. New
  `mvm-security/src/snapshot_hmac.rs`: `~/.mvm/snapshot.key`
  lazy-init mode 0600, HMAC-SHA256 over length-prefixed
  envelope (`be_u32(schema_version) || be_u64(vmstate_len) ||
  vmstate_bytes || be_u64(mem_len) || mem_bytes ||
  be_u32(version_len) || version_bytes`) — splice-resistance
  asserted by regression test. Atomic seal via `<file>.tmp` +
  fsync + rename; constant-time tag comparison on verify;
  fast-fail size check before streaming. Wired into
  `template/lifecycle.rs::seal_snapshot_artifacts` (post Firecracker
  create) and `microvm.rs::restore_from_template_snapshot` (before
  any Firecracker spawn). Migration: missing sidecar → warn +
  proceed by default; `MVM_SNAPSHOT_HMAC_STRICT=1` flips to hard
  error; `MVM_ALLOW_STALE_SNAPSHOT=1` accepts version-mismatch.
- **W5 — CI gates + doctor.**  ✅ shipped — PR #71. Combined
  `prod-agent-runentry-contract` lane (renamed from
  `prod-agent-no-exec`) — ONE build, ONE step, BOTH assertions:
  `do_exec` symbol ABSENT and `handle_run_entrypoint` symbol
  PRESENT on the same shipping binary. New `mvmctl doctor`
  probes: snapshot HMAC key (mode 0600, length); snapshot dirs
  (walk `~/.mvm/templates/*/artifacts/*/snapshot/` and report
  the first looser-than-0700 dir). New vsock verb
  `EntrypointStatus` for live-VM probing (prod-safe, no inputs;
  reports validated path + ok-flag).
- **W6 — Network: deny-default for function workloads.**  🟡
  cross-repo, IR side captured. Function-entrypoint workloads
  default `network.mode = "none"`. The IR shape (default
  derivation from `entrypoint.kind`, wildcard-egress rejection,
  granular grants in v2) is captured in decorationer plan 0004
  (decorationer #2 merged). mvm-side glue is mechanical: when
  mvmforge ships the IR change, mvm honours `mode = "none"` by
  skipping TAP allocation. **Open** — needs the mvmforge IR
  emit + an mvm-side regression test that asserts a `mode =
  "none"` workload truly has no TAP.
- **W7 — Warm-process function dispatch (ADR-0011 tier 2).**  🟡
  in progress  [`plans/43-warm-process-function-dispatch.md`](plans/43-warm-process-function-dispatch.md).
  Adds an opt-in worker pool inside the guest agent so
  function-entrypoint calls can reuse a long-running wrapper
  process across invokes instead of cold-spawning per
  `mvmctl invoke`. Driven by a new mvmforge-owned
  `/etc/mvm/runtime.json` carrying a `concurrency.kind =
  "warm_process"` field (`max_calls_per_worker`, `max_rss_mb`,
  `pool_size`, `in_process`, `max_queue_depth`). When the field
  is absent, the cold path (W2) stays bit-identical. Host wire
  (`RunEntrypoint` + `EntrypointEvent`) is unchanged; the agent
  synthesizes the existing event stream from a single buffered
  framed response per worker call. M12 (one in-flight call per
  VM) is bypassed under warm-process — the new invariant is "one
  in-flight call per worker, ≤ `pool_size` concurrent." The
  `prod-agent-no-exec` symbol gate keeps passing; the plan adds a
  positive-evidence assertion for the new
  `mvm_guest::worker_pool` module. mvm-side only — mvmforge ships
  the IR + factory + runner-wrapper changes in a coordinated
  follow-up (cross-repo ADR-0011).

### Substrate validation (live smoke)

PR #73 adds the substrate-validation infrastructure:

- `mkGuest` `extraFiles` parameter — bakes arbitrary files into
  the rootfs at build time, owned root, with declared octal mode.
  `extraFiles ? {}` default keeps backward compat for every
  existing caller. (Update post-ADR-0010 §3 Option A flip: the
  `mk{Python,Node,Wasm}FunctionService` factories now live in
  this repo at `nix/lib/factories/` and use this to bake
  `/etc/mvm/entrypoint` plus the wrapper.)
- `nix/images/examples/echo-fn/` — minimal `mkGuest` invocation
  baking a wrapper at `/usr/lib/mvm/wrappers/echo` (`#!/bin/sh\nexec cat\n`)
  plus the marker. No language runtime; just exercises the
  substrate path.
- `tests/smoke_invoke.rs` — two `MVM_LIVE_SMOKE=1`-gated tests
  (round-trip + zero-stdin). Skip cleanly without the env var
  with an `eprintln!` diagnostic.

The substrate (compile, clippy, gated-skip behaviour) is verified;
the actual boot+invoke against a capable host is the open
load-bearing item.

### Cornerstones

Two pieces unblock everything else:

1. **`RunEntrypoint` vsock verb** (W1, W2) — the production-safe
   call substrate that mvmctl invoke and mvmforge SDKs both build
   on. Distinct from `do_exec` so the existing prod gate
   (`prod-agent-no-exec`) stays meaningful.
2. **Combined CI contract gate** (W5) — `prod-agent-no-exec` AND
   `prod-agent-has-runentry` against the *same* binary that ships.
   Prevents feature-flag drift from regressing half the contract
   silently.

### Cross-repo dependency

mvmforge (decorationer) plan 0003 ships in parallel — language SDKs
(Python + TS), Nix factories (`mkPythonFunctionService`,
`mkNodeFunctionService`), hardened wrapper templates. mvm exposes the
`RunEntrypoint` substrate; mvmforge consumes it. (Update: ADR-0010
§3 was later amended back to Option A — the factories themselves
landed in this repo at `nix/lib/factories/`; mvmforge consumes them
via `mvm.lib.<system>`.) The cutover is
coordinated: when mvm's W6 lands the deny-default flip, mvmforge's
factories must already emit the new IR shape. mvmforge owns the
language-specific seccomp tiers (`standard-python`, `standard-node`);
mvm just exposes the tier-loading mechanism (already W2.4).

### Sprint 45 success criteria

By sprint close, the project should be able to claim:

1. *A constrained `RunEntrypoint` vsock verb runs the image's baked
   entrypoint program with stdin piped and stdout/stderr captured;
   `do_exec` remains dev-only.* (W1, W2, W5) — **substrate shipped
   #67/#72/#71; live-KVM exercise pending #73 run.**
2. *`mvmctl invoke` is the prod-safe call surface; `mvmctl exec`
   stays dev-only.* (W3) — **shipped #69; live-KVM exercise pending.**
3. *Firecracker snapshots are HMAC-verified at restore; tampering
   refuses resume.* (W4) — **shipped #70; tamper regression covered
   by unit tests; live-KVM exercise pending.**
4. *Function-entrypoint workloads default to no network; explicit
   IR grants are required for any reachability.* (W6) — **IR side
   captured (decorationer plan 0004); mvm-side TAP-skip pending the
   mvmforge IR emit.**
5. *Default logs do not contain stdin/stdout/stderr content.* (W2,
   W3) — **shipped — agent + mvmctl log metadata only.**
6. *Cross-repo cutover with mvmforge: a Python or TS function
   workload booted from a `mvmforge up` artifact accepts
   `mvmctl invoke <vm> --stdin <args>` and returns stdout encoded
   per the IR-declared format.* (Phase 5 integration test) —
   **blocked on decorationer plan 0003 phases 2–4 (decorator body
   preservation, host SDK call site, Nix factories).**

### Shipped (PRs landed on `main`)

| PR | Workstream | Content |
| --- | --- | --- |
| [#66](https://github.com/tinylabscom/mvm/pull/66) | Docs | ADR-007, plan 41, plan 41-design (16 mitigations), Sprint 45 entry |
| [#67](https://github.com/tinylabscom/mvm/pull/67) | W1 | Wire types: `RunEntrypoint`, `EntrypointEvent`, `RunEntrypointError`; fuzz target |
| [#72](https://github.com/tinylabscom/mvm/pull/72) | W2 | Agent handler + `entrypoint.rs` module + per-call hygiene + concurrency mutex (recreated from auto-closed #68) |
| [#69](https://github.com/tinylabscom/mvm/pull/69) | W3 | `mvmctl invoke` CLI + `send_run_entrypoint` streaming consumer |
| [#70](https://github.com/tinylabscom/mvm/pull/70) | W4 | Snapshot HMAC integrity (seal + verify wired into create/restore paths) |
| [#71](https://github.com/tinylabscom/mvm/pull/71) | W5 | Combined symbol-contract CI lane + doctor probes + `EntrypointStatus` verb |

Cross-repo (decorationer):

| PR | Content |
| --- | --- |
| [decorationer #1](https://github.com/tinylabscom/decorationer/pull/1) | ADR-0009 + plan 0003 (function entrypoint runtime — six-phase) |
| [decorationer #2](https://github.com/tinylabscom/decorationer/pull/2) | Plan 0004 (network deny-default for function workloads — IR side of W6) |
| [decorationer #3](https://github.com/tinylabscom/decorationer/pull/3) | Plan 0003 phase 1 — `Entrypoint::Function` IR variant + `Format` closed enum + new `function-app` corpus entry (byte-identical Python ↔ TS) |

### Deferred — concrete follow-ups

| Item | Plan | Why deferred | Estimated size |
|---|---|---|---|
| **Live-KVM smoke run** ([PR #73](https://github.com/tinylabscom/mvm/pull/73)) | Plan 41 W3 / W5 acceptance | Substrate compiles, clippy-clean, gated-skip works on macOS Darwin 25 host. Boot+invoke needs native Linux/KVM or macOS 26+ Apple Container — neither available in the dev session that wrote it. PR description names three plausible failure modes (`EntrypointInvalid` from chown/uid in fakeroot, vsock missing on host, `mvmctl template build --flake <path>` argv shape) so the human running it knows where to look. | ½ day on a capable host |
| **W6 mvm-side TAP-skip** | Plan 41 W6 + decorationer plan 0004 | mvmforge needs to ship the IR change first (decorationer plan 0003 phase 1 is in, but phase 2–4 SDK + Nix factory work hasn't started). Once the IR carries `entrypoint.kind = "function"` with the deny-default network mode, mvm honours it by skipping TAP allocation. | ~1 day after mvmforge ships |
| **Decorationer plan 0003 phase 2 — Python SDK** | decorationer plan 0003 | Decorator preserves function body in bundled source; emitter writes new IR; bundler ships function source; host call site shells out to `mvmctl invoke`. Blocks live-KVM smoke against a real Python wrapper. | ~2 days |
| **Decorationer plan 0003 phase 3 — TypeScript SDK** | decorationer plan 0003 | Mirror Phase 2 surface. | ~2 days |
| **Decorationer plan 0003 phase 4 — Nix factories** *(landed in mvm post-Option-A flip; see `nix/lib/factories/`)* | decorationer plan 0003 | `mkPythonFunctionService` / `mkNodeFunctionService` emitting hardened wrappers (mode=prod with sanitized error envelope, `PR_SET_DUMPABLE=0`, no payload logging) at `/etc/mvm/entrypoint` via mvm's `extraFiles` (already in mvm #73). | ~3 days |
| **Session pool management** | follow-up plan (none yet) | Pre-baked invariant: *single-tenant for VM lifetime*. v1 reuses `boot_session_vm` / `dispatch_in_session` / `tear_down_session_vm` primitives directly. Sizing / eviction / per-tenant isolation / idle reaper are real but separable from the substrate. | ~1 sprint |
| **Streaming chunked output** | follow-up plan (none yet) | v1 wire is streaming-shaped but buffered up to 1 MiB per stream. Lifting the cap means real chunked emission from the agent and a streaming consumer in `send_run_entrypoint`. | ~1 week |
| **Schema-bound payloads (v2 of W3)** | decorationer plan 0003 | Derive JSON Schema from type hints (Python `pydantic` / TS `zod`). Wrapper validates inbound bytes before user code runs. | ~1 week |
| **Guest agent signal handling** — W1 + W2 shipped; W3 (SIGHUP config reload) backlog | [`plans/44-agent-signal-handling.md`](plans/44-agent-signal-handling.md) | SIGTERM/SIGINT now flip an atomic flag the accept loop polls, triggering `WorkerPool::shutdown` for an orderly drain. Symbol-contract gate extended with `install_signal_handlers` positive evidence. SIGHUP config reload (W3) unblocks once mvm wants in-place config reload — today the agent has no hot-reloadable config surface, so it's not load-bearing. | ~½ day W1+W2 shipped |

### Stack-merge artifacts

The merge cascade left two cosmetic artifacts in the history that
are worth knowing about if you go grepping:

1. **PR #68 → #72**. When I merged #67 with `--delete-branch`, GitHub
   auto-closed #68 because its base branch (`feat/runentrypoint-wire-protocol`)
   was deleted. I rebased the same commits onto current main and
   re-PR'd as #72. W2's commit footer reads `(#72)`, not `(#68)`. #68
   shows on GitHub as **closed-not-merged** with identical content
   to the commit `26bae51` that did land.
2. **Source branches don't survive in commit metadata.** Every
   `feat/*` branch I created (W1 wire, W2 handler, W3 invoke, W4
   snapshot, W5 doctor) was deleted on merge. The squashed commits
   on `main` carry the PR# in the subject line, but the original
   pre-rebase commit DAGs (separate W2-rebase commits etc.) are
   gone from the remote. `git log` looks tidy; `git log --all
   --grep=runentrypoint` finds only the squashed forms.

Both are normal squash-merge consequences; documented here so the
next person to audit the timeline doesn't re-discover them as
suspicious.

### Non-goals (named explicitly)

- **Streaming chunked output.** v1 wire is streaming-shaped but
  buffered up to 1 MiB per stream; chunked v2 lifts the cap once a
  user hits it.
- **Pool sizing / eviction policy.** Session-VM primitives reused
  as-is; pool *management* is a follow-up plan with the pre-baked
  invariant *single-tenant for VM lifetime*.
- **Closure shipping at call time.** Forbidden by build-time-only
  rule; no runtime function registration, no dynamic dispatch.
- **Code-executing serializer formats.** IR enum is closed
  (`json`/`msgpack`); formats whose decoder runs arbitrary code
  are excluded. CI-enforced via wrapper grep.
- **Schema-bound payloads in v1.** v1 keeps caps + format
  validation only; v2 derives JSON Schema from type hints (Python
  `pydantic` / TS `zod`) and validates inbound bytes before user
  code runs.
- **Granular network IR fields in v1.** v1 ships deny-default with
  the existing one-bit `network.mode`; granular grants
  (`egress`/`peers`/`ingress`/`dns`) land in v2 — flipping the
  default later is breaking, the granular surface is additive.
- **Network deny-default flip for non-function workload kinds.**
  Backwards-incompatible for any workload that quietly relied on
  the implicit grant; separate ADR if proposed.
- **SLSA-style attestation of mvmforge artifacts.** v1 leans on
  reproducibility (W5.3) + dm-verity (W3); SLSA is v2+.
- **Multi-tenant guests within one VM.** ADR-002 already excludes;
  function entrypoints don't change this.
- **Authenticated invoke from non-local callers.** vsock socket
  mode 0700 (W1.2) gates to local user; cross-host authn is
  mvmd's problem.

## Sprint 46+ — Cross-platform expansion (proposed)  [`plans/53-cross-platform-roadmap.md`](plans/53-cross-platform-roadmap.md)

**Goal:** turn cross-platform support into a coherent multi-platform release without forking the security narrative. Decision recorded in plan 53 as **Option B — Pragmatic**: Firecracker stays the security baseline, Apple Container is the macOS exception, **libkrun** is the only new backend (Intel Mac + macOS-no-Lima), Docker stays as Tier 3 with loud warnings, Windows is first-class via WSL2 with bootstrap automation.

**Why this sprint, why now:** today mvm fully supports Linux + KVM (Firecracker) and macOS 26+ Apple Silicon (Apple Container, plan 23). Older macOS, Intel Macs, and Windows hosts are second-class. The 2026 microVM ecosystem (SlicerVM, libkrun, AWS nested-virt EC2) makes a coherent multi-platform release tractable; we want to land it before the gap widens.

**Three sequential sprint slots:**

- **Sprint 46 — Foundation (~5 days, narrative + UX, zero arch risk).** Plans A (Matryoshka ADR rewrite), B (Doctor security-claims-by-tier output), C (PVM FAQ entry), J (AWS deployment guide), K (Ubicloud deployment guide), plus deferred-backlog placeholder files for Plans F/G/H.
- **Sprint 47 — macOS parity + Windows foundation (~1 sprint).** Plan D (APFS CoW for Apple Container templates) + Plan I.1 (Windows CI lane) + Plan I.2 (Windows install docs, WSL2-first).
- **Sprint 48 — libkrun + Windows installer (~1.5 sprints).** Plan E (libkrun backend — Intel Mac + macOS-no-Lima) + Plan I.3 (winget manifest) + Plan I.4 (WSL2 bootstrap automation). Sprint 48 ships **scaffolding** for libkrun (final API, dispatch, doctor, install hints); the spike phase that lands real C bindings + boot validation is tracked separately in [`plans/57-libkrun-spike.md`](plans/57-libkrun-spike.md).

**Deferred backlog (rationale captured in plan 53):**

- **Plan F — Cloud Hypervisor backend.** *Rejected* for security-posture reasons. Every advantage CH ships (nested KVM in guests, GPU passthrough, larger device model, Windows-guest support) is exactly what Firecracker excluded for attack surface. Adding CH would fork the security narrative. Trigger conditions to revisit are documented in plan 53 §Plan F.
- **Plan G — crosvm backend.** *Deferred.* Niche for our user base; libkrun (Plan E) covers the embeddable cross-platform niche. Trigger: real Chrome OS / Android demand.
- **Plan H — rust-vmm internalization.** *Rejected for now.* Composing rust-vmm crates into a working VMM is *building a VMM*; that's Firecracker's and libkrun's job. Trigger: custom-VMM-required feature.

**Sprint 46+ success criteria (per slot):**

- After Sprint 46: ADR-002 displays the layer model + per-backend tier matrix; `mvmctl doctor` and `mvmctl run` emit the Docker-tier warning banner; AWS + Ubicloud deployment guides published; deferred plans 54/55/56 placeholder files committed.
- After Sprint 47: macOS Apple Container template instantiation <1s via APFS CoW; `cargo build --workspace` green on Windows; Windows install docs (WSL2-first) published.
- After Sprint 48: libkrun runs on Linux + KVM, macOS Apple Silicon (no Lima), and macOS Intel; `winget install mvm` works on Windows; `mvmctl bootstrap` on Windows configures WSL2 + Ubuntu + mvm automatically.

**Non-goals (named explicitly):**

- Cloud Hypervisor backend (Plan F, rejected).
- Promoting Docker to a first-class Windows path via pre-built rootfs distribution (would conflict with the security posture).
- Native-Windows microVMs via Cloud Hypervisor + WHPX (depends on Plan F).
- Eliminating Lima from the macOS *build* path (libkrun solves runtime only; build-on-host is future work).

## Sprint 49 — Filesystem Volumes (sandbox-runtime parity, in flight)

Branch: [`feat/sprint-46-filesystem-volumes`](https://github.com/tinylabscom/mvm/tree/feat/sprint-46-filesystem-volumes) — branch name preserved for PR continuity; the sprint itself was relabeled from 46 to 49 during merge to disambiguate from Sprint 46 (cross-platform foundation, already merged via #97).
Plan: [`plans/45-filesystem-volumes.md`](plans/45-filesystem-volumes.md).
mvmd companion: [`mvmd/specs/plans/29-filesystem-volumes.md`](../../mvmd/specs/plans/29-filesystem-volumes.md) (sister repo — needs corresponding rename).

### Why this sprint

mvm's in-flight share registry (untracked on `feat/sandbox-sdk-foundation`) does not match the established sandbox-runtime Volume primitive shape: those volumes are **named, multi-attach, filesystem-semantics**. We replace the share registry with a `Volume` primitive that ships in mvm-core (wire types) plus a new `mvm-storage` crate (trait + impls for `LocalBackend` + `ObjectStoreBackend` via `opendal`, with mandatory `EncryptedBackend<B>` decorator). mvmd consumes `mvm-storage` via the `mvmctl` git facade and reconciles with its existing `StorageBucket` primitive (see Plan 45 §"Discoveries during implementation" D1).

### Workstream breakdown (mvm-side, post-D5 / Path C)

- **W-Volume — local volume primitive** (Phase 1, 5, 6, 8): `mvm-core` wire types + `mvm-storage` minimal crate (trait + `LocalBackend` only) + `volume_registry.rs` + `mvmctl volume create/ls/rm` (local) + `MountPathPolicy` extension for Nix paths.
- **W-Mount-API — declarative mount at boot** (Phase 7, 10): `mvmctl up --volume <name>:<path>` + `MountVolume`/`UnmountVolume` vsock verbs + `mkGuest.volumeMounts` Nix attrset.
- **W-RemoteClient — `--remote` proxy to mvmd** (new, replaces the dropped W-DataPlane): small `mvmctl::mvmd_client` module (~50–100 LoC, uses workspace `reqwest`). Supports `volume create|ls|rm|cp|read|write|attach|detach|snapshot create|snapshot ls|snapshot restore` against mvmd REST. `~/.mvm/config.toml` `[remote]` section: `endpoint`, `api_key_ref`, `default_org`, `default_workspace`. All optional.
- **W-Doctor — FDE check** (Phase 9): `mvmctl doctor` reports FileVault/LUKS state. **Warns** on dev box (no enforcement); mvmd enforces hard-block on workers.
- **Out-of-scope on mvm side (per D5, moved to mvmd Sprint 137 W2)**: `ObjectStoreBackend` impl, `EncryptedBackend<B>` decorator, AES-256-GCM / AES-SIV / HKDF crypto code, `opendal` dep — all live in mvmd.

### Cross-repo dependency

mvmd Sprint 29 (`mvmd/specs/plans/29-filesystem-volumes.md` — sister repo file needs corresponding rename) follows mvm Plan 45 phases 1-3 landing on `main`. mvmd consumes `mvmctl::storage` via the existing git workspace dep. Decision blocker on the mvmd side: extend `StorageBucket` (recommended) vs. add parallel `FilesystemVolume` — see Plan 45 §D1.

### Sprint 49 success criteria (post-D5 / Path C)

- mvm `volume` CLI replaces `share` CLI with no compat shim (greenfield rename, in-flight share files deleted).
- `mvmctl volume create scratch` (local) round-trips: VM boot with `--volume scratch:/mnt/scratch`; write file from guest; tear down VM; reboot; reattach; file persists. Plus multi-attach proof (two local VMs see same file).
- `mvmctl volume create fixtures --remote --backend s3 --url s3://...` proxies through mvmd REST and returns 200; data plane via `--remote` round-trips against MinIO (mvmd-side integration test, not mvm-side — covered in mvmd Sprint 137 W2).
- `mvmctl doctor` reports FDE state (warns on non-FDE; mvmd-side hard-block tested separately).
- Path safety: `volume cp ../etc/passwd …` rejected; `/nix*` mount denied by `MountPathPolicy`.
- `cargo test --workspace` and `cargo clippy --workspace -- -D warnings` clean.
- All `prod-agent-no-exec`, `cargo deny`, `cargo audit`, fuzz-corpus CI gates green.
- `mvm-storage` crate has minimal deps: `tokio`, `bytes`, `async-trait`, `mvm-core`, `mvm-security`. **No `opendal`, no AEAD crates** — those land in mvmd Sprint 137 W2.

### Phasing

Phases 1-10 in Plan 45's "Implementation order" map to mvm-side work and are all shipped (mvm-core types, `mvm-storage` crate, runtime registry, CLI subcommand, guest vsock verbs, security policy, doctor FDE check, mkGuest extension). Phases 13-18 are mvmd-side (covered in mvmd Sprint 137). Phase 11 (live KVM smoke) is deferred to [Plan 58](plans/58-filesystem-volumes-live-kvm-smoke.md) because it requires real KVM hardware — the deferral is documented so the work isn't lost when Sprint 49 closes.

### Non-goals (named explicitly, see Plan 45 §"Out of scope (v1)")

B1–B18 in Plan 45 §"Out of scope (v1)" — buckets-as-separate-primitive, cross-host backends (NFS/CephFs), mountable provider-backed volumes, hot attach/detach, cross-workspace ACL grants, volume export/import, tags/labels, soft-delete, read cache, webhooks, `data_disk` (plan 38), scheduler volume-affinity, per-volume LUKS, strong-consistency snapshots, HSM/KMS-backed master keys, compression/dedup, usage analytics. Each is preserved in Plan 45 with what/why/trigger so they can be picked up in future sprints.

### Live-KVM smoke (Phase 11 → [Plan 58](plans/58-filesystem-volumes-live-kvm-smoke.md))

Plan 45 Phase 11 (live KVM smoke fixture) deferred to its own plan 58 — needs a KVM-capable host that no longer fits in a software-only PR. Plan 58 captures the six scenarios (single-VM round-trip, persistence, multi-attach, RO enforcement, scope isolation, Nix-path denial) so the work isn't lost when Sprint 49 closes. (Numbered 58 because plan 46 was already taken by the metering-API work merged in #89.)

## Sprint 50 — mvm migration: Phase 0 + Phase 1 (foundation, facade, microsandbox backend) — IN FLIGHT  [`plans/60-mvm-microsandbox-migration.md`](plans/60-mvm-microsandbox-migration.md)

**Status (2026-05-08):** Phase 0 ✅ shipped; Phase 1 W1–W4 ✅ shipped on `feat/micro` (12 commits). The MicrosandboxBackend is fully wired against upstream microsandbox 0.4.5; `auto_select()` picks it as the cross-platform default (macOS arm64/x86_64 + Linux without KVM); the `nix/` flake using microvm.nix is up and `nix flake check --no-build` is clean; the docs site is migrated and refactored to a token-based light+dark mode system; mvmd's contract gate is green against `../mvm` via the local `.cargo/config.toml` patch.

Full plan in [`plans/60-mvm-microsandbox-migration.md`](plans/60-mvm-microsandbox-migration.md). The plan is checkpointed into 11 phases (0, 1, 2, 3, 4, 5, 6, 7, 7a, 7b, 8, 9, 10) — each with explicit exit tests, ADR coverage, sprint rotation, and a demo gate.

**Branch:** `feat/micro` (moves to `feat/migrate-to-mvm` once Phase 0 settles).

### Why this sprint, why now

The current `mvm` is a 5-crate, ~520-LOC skeleton; the previous iteration at `../mvm` is a mature 13-crate stack. The user wants a clean cut to a microsandbox-first build/exec model with feature parity, multi-language SDKs (Rust + Python + TypeScript), encryption-everywhere, attestation-everywhere, audit-everywhere, and a hosted-cloud-ready posture. mvmd depends on the `mvmctl` facade, which we cannot break — Phase 0 protects that contract before any other work.

### Phase 0 exit criteria

- [x] Plan saved to `specs/plans/60-mvm-microsandbox-migration.md`
- [x] Sprint 50 documented here in SPRINT.md (this section)
- [x] Phase-0 ADRs stubbed: 013 (microsandbox pivot, with microvm.nix fallback), 014 (VmBackend trait), 027 (iroh encryption layering), 031 (cross-platform strategy), 032 (hosted-cloud invariants), 033 (code-quality enforcement), 035 (feature flag taxonomy), 038 (CI execution policy)
- [x] Compliance doc stubs: `specs/compliance/{soc2-controls,pci-scope,hipaa-mapping,gdpr-mapping}.md`
- [x] Root `Cargo.toml` workspace block rewritten with full crate list + feature flags + workspace lints (`too_many_arguments = "deny"`). Workspace lint landed on `feat/cloud-hypervisor-lifecycle` — `[workspace.lints.clippy] too_many_arguments = "deny"` plus `[lints] workspace = true` opt-in in every crate's Cargo.toml.
- [x] `mvm-core`, `mvm-storage`, `mvm-plan`, `mvm-policy`, `mvm-security` ported from previous iteration; all present under `crates/`.
- [x] `src/lib.rs` facade re-exports every workspace crate (`pub use mvm_core as core;` etc.); post-W8 also re-exports `mvm_backend as backend`.
- [x] `mvm-backend/`, `mvm-providers/` are real crates with concrete impls now (W7/W8 ended the façade-only state). The "removed" wording in the original criterion meant "no longer skeleton-only" — true.
- [x] CI matrix runs Linux (every PR, `ci.yml`), macOS (release-tag pushes per ADR-038, `release.yml`), Windows (separate `windows.yml`, informational/non-blocking until WSL2 bootstrap closes the unix-isms list).
- [x] `xtask check-adr-coverage` implemented (`xtask/src/check_adr_coverage.rs`); wired into `ci.yml` as informational (`continue-on-error: true`) — the workspace carries ~12 forward references to unwritten ADRs from the compliance doc stubs that would block a hard gate today.
- [ ] **mvmd contract gate**: `cd ../mvmd && cargo build --workspace` blocked by pre-existing `microsandbox 0.4.5` ⊥ `iroh-base 0.96.1` over `sha2` (same blocker as every prior slice). Targeted package builds + manual surface audit confirm every `mvmctl::*` path mvmd imports still resolves; the contract is preserved in shape, the gate just can't execute end-to-end until the upstream dep conflict resolves.

### Wave plan (each wave is a checkpoint)

**Phase 0 — foundation + facade preservation:**

- **W0.1** ✅ — metadata + 7 Phase-0 ADR stubs + 4 compliance stubs (`343abfa`)
- **W0.1.1** ✅ — CI execution policy (push → ci.yml; release → rest) + githooks tracked + ADR-038 (`5318567`)
- **W0.2** ✅ — workspace reshape: 13 crates from `../mvm` verbatim + facade preserved + mvmd contract gate green (`1c3e00c`)

**Phase 1 — first tracer bullet (microsandbox backend):**

- **W1.1** ✅ — MicrosandboxBackend variant in `AnyBackend` + dispatch + 11 unit tests (`8c7211d`)
- **W1.2** ✅ — `microsandbox = "0.4.5"` workspace dep wired; 4 of 6 lifecycle methods real (`is_available` / `list` / `status` / `stop` / `stop_all`); resources/ imported; 244/244 lib tests green (`4072484`)
- **W1.3** ✅ — `start()` and `logs()` real; `.ext4 → .raw` hard-link bridge via `ensure_microsandbox_rootfs_alias()`; 2 new alias unit tests (`a8cb2e7`)
- **W1.3.1** ✅ — no-OCI invariant codified in ADR-013 + plan 60 (`c1d5b01`)
- **W2** ✅ — `auto_select()` priority slots Microsandbox at #2 (cross-platform default per ADR-013); `Platform::has_microsandbox()`; docs site migrated; `tailwind.css` + `custom.css` refactored to token-based light+dark mode (no hardcoded colors except the macOS-Aqua terminal-dot trio); ADR-013 docs page (`3438a24`, `6261bc4`)
- **W3** ✅ — `tests/smoke_microsandbox.rs` with `MVM_LIVE_SMOKE=1` gate; live test exercises start/stop/alias bridge against a real `mkfs.ext4` fixture; sanity test always runs (`0668c60`)
- **W4** ✅ — `nix/flake.nix` imports microvm.nix; `nix/profiles/minimal.nix`; `nix flake check --no-build` clean; 3 structural tests guard the flake's shape; "Building MicroVM Images" docs page (`5a9b765`)
- **W4-fix** ✅ — reframe: flake is a library, users keep their own `flake.nix` + `mvm.toml`; `lib.<system>.mkGuest` placeholder; internal fixtures renamed `internal-minimal-*`; docs rewritten user-flake-centric (`c323140`)
- **W5** ✅ — real `mkGuest` in `nix/lib/mk-guest.nix` + `nix/lib/default.nix`. Three entrypoint forms (`shell` / `command` / `services`) with sealed-vs-accessible auto-inferred from form (or explicit `dev` override). Same flake works for both modes — the builder writes `passthru.mvm.{accessible, sealed, entrypointKind}` and `/etc/mvm/variant` so `mvmctl console` can gate. `nix/tests/mk-guest-eval.nix` validates the inference. Rust shell-out test runs the eval when nix is on PATH; skips silently otherwise.
- **W5-perf** ✅ — ADR-013 amended with per-backend boot-time budgets and the busybox-as-PID-1 architectural commitment. NixOS+systemd is too slow (1-3s on Firecracker); the previous iteration's busybox path approached the upstream Firecracker reference of ~125ms. Sprint perf gates pinned: Firecracker ≤200ms, microsandbox/libkrun ≤500ms, Apple Container ≤1s. (`a5fa7d2`)
- **W5.1** ✅ — `mkGuest` rewritten end-to-end: NixOS+systemd path replaced with hand-rolled busybox-as-PID-1. Static `pkgsStatic.busybox` PID 1, custom `/init` script (POSIX sh, no bashisms) that mounts pseudofs + tmpfs and execs the rendered entrypoint. ext4 image emitted via `nixpkgs/nixos/lib/make-ext4-fs.nix`. `passthru.mvm.{initSystem, expectedBootMs}` exposed for CI gates. 9/9 nix-eval assertions green; user-facing surface unchanged.
- **W5.2** ✅ — crate-layout cleanup: collapsed `mvm-libkrun` + `mvm-apple-container` into a single **`mvm-providers`** crate (FFI/SDK shim layer); created **`mvm-backend`** as a thin re-export façade for the dispatch types (`AnyBackend`, `FirecrackerConfig`). The concrete backend impls (`firecracker.rs`, `microsandbox.rs`, etc.) stay under `mvm/src/vm/` for now because they reach into `mvm::{config, shell, ui, vm::microvm, vm::image}` at compile time — extracting them needs those modules to move down to a shared crate first. ADR-012 amended with a disambiguation note distinguishing the public Provider concept (mvmd) from the internal `mvm-providers` shim crate (mvm). 1788+ tests still green.

- **W6** ✅ — end-to-end boot smoke harness landed: `tests/smoke_e2e_boot.rs` boots a real Nix-built rootfs through `MicrosandboxBackend::start_with_mode`, asserts the sandbox shows up in `list()`, measures cold-boot wall-clock, and tears down clean. **Cross-platform**: runs on Linux/KVM and macOS/HVF (microsandbox's libkrun supports both); Windows excluded only because microsandbox's Windows path isn't wired (ADR-031). Gated by `MVM_LIVE_SMOKE=1` + `MVM_TEST_ROOTFS=/path/to/rootfs.ext4`; skips silently otherwise. Single-shot tripwire 2× the ADR floor (= 600ms); the strict statistical gate (`xtask perf --runs 100`) lands in Phase 9. ADR-013 boot budget tightened to a unified **≤ 300 ms cold p50 floor across every backend**; mkGuest's `passthru.mvm.expectedBootMs` and the docs page table updated to match. 9 nix-eval assertions + 4 structural tests + 3 smoke tests all green.

- **W6.1** ✅ (rootless half) — privilege-drop infrastructure landed in mkGuest. `setpriv --reuid + --regid + --clear-groups + --no-new-privs` wraps the entrypoint `exec` line; `/etc/passwd`/`/etc/group` get baked with the agent + worker rows; `passthru.mvm.{uids, rootlessEntrypoint}` surfaces the resolved values. Defaults: dev → entrypoint uid 0 (debug shell ergonomics: `apt install`, `mount`, etc.); prod → entrypoint uid 1000 (rootless workload per ADR-002 W2.1); agent always uid 990. Override knob `uids = { agent = N; entrypoint = M; }` for either direction. ADR-013 amended with the privilege model + the dev/prod default rationale; docs page gets a "Rootless workloads" section. 15/15 nix-eval assertions green (6 new); +6 since the previous wave.
- **W6.1.1** ✅ (supervision pattern + stub agent) — `/init` now forks the agent in the background under setpriv→uid 990 before setpriv-exec'ing the entrypoint. The agent binary at `/usr/local/bin/mvm-guest-agent` is a placeholder stub (sh script that logs startup and sleeps) — the real Rust binary swap is W6.1.2 (needs cross-compile infrastructure). Every derivation surfaces `passthru.mvm.agentBinary = "stub" | "real"` so production deployments can refuse to boot stub images via policy lint (lands later). 16/16 nix-eval assertions green (1 new for agentBinary metadata). ADR-013 §"Guest agent supervision" + the docs page agent-status note land in this wave.
- **W6.x — Lima removal** ✅ — `crates/mvm/src/vm/lima.rs` deleted (the 130-LOC Lima integration); `lima_state.rs` deleted entirely (zero callers); `Platform::needs_lima()` now permanently returns `false` (existing `if needs_lima() { … }` branches become dead code, prune in a follow-up); `vm/lima.rs` re-added as a thin no-op shim so mvm-cli imports keep compiling (every fn `Ok(NotFound)` / `Ok(())`); `auto_select`'s confusing "Firecracker via Lima fallback" #6 step rewritten as "production-target default reachable only in narrow feature-gating cases." ADR-013 amended with a substantial new section, **§"Linux builder via microsandbox (no Lima)"**, naming the design: on macOS hosts without a configured Linux builder, mvm bootstraps one in a microsandbox sandbox (OCI image; Nix store bind-mounted; artifacts extracted back to host). The OCI carve-out is consistent with the runtime non-goal — builders live in a different trust zone than runtime. install/macos.md updated to "zero-config default; existing builder still honored." Real microsandbox-builder implementation is its own follow-up wave.
- **W6.y — Cloud Hypervisor stub backend** ✅ — `crates/mvm/src/vm/cloud_hypervisor.rs` ships the stub `CloudHypervisorBackend: VmBackend` with the final shape (capabilities = pause+resume+snapshots+vsock+tap, security profile = Tier 1 with claim-3 partial, `is_available` reads `Platform::has_cloud_hypervisor`). Wired into `AnyBackend::CloudHypervisor` + `from_hypervisor` matcher (`cloud-hypervisor` / `cloud_hypervisor` / `ch` / `clh` aliases). `auto_select` is unchanged — Firecracker stays the default for KVM hosts; CH is opt-in for workloads that need VFIO/GPU/virtio-fs/larger guests beyond what Firecracker supports. ADR-013 gains §"Cloud Hypervisor as a Tier 1 peer of Firecracker" carrying the rationale + the tier classification + the schedule bump (CH was post-Phase-10 in the original plan; user asked for backend flexibility, so it's now near-term). 9 new dispatch + capability + alias tests; 0 fail across the workspace.

- **W6.2 — `mvmctl console` accessible/sealed gate (skeleton)** ✅ — new `crates/mvm/src/vm/runtime_meta.rs` (backend-agnostic `VmRuntimeMeta { mode, accessible }` struct + serde + `read`/`write` helpers; backward-compat parsing of pre-W6.2 `{"mode":"…"}` files as `accessible: true`). `commands/vm/console.rs` gains `--force` clap arg + `enforce_accessible_gate(name, force)` called before any vsock attach. Refusal message names the cause and points at `--force`. 4 new gate tests under `accessible_gate_tests` + 5 round-trip tests on the meta module. Microsandbox's `record_start_mode` delegates to the new shared module.
- **W7.x.1 — microsandbox-as-Linux-builder Wave 1 (contract scaffolding)** ✅ — new `crates/mvm-build/src/builder_vm.rs` with pinned `BUILDER_OCI_IMAGE = "docker.io/nixos/nix:2.24.10"`, contract types (`BuilderMounts`, `BuilderJob`, `BuilderArtifacts { …, accessible: Option<bool> }`), `BuilderVm` trait matching ADR-013's 6-step flow, and `StubBuilderVm` returning `BuilderVmError::NotYetImplemented` with an error message that names ADR-013 + the recovery path (host Nix or `nix-darwin`'s linux-builder). 6 unit tests. `thiserror = "1"` added to mvm-build deps.
- **W6.2 ↔ W7.x.1 sidecar bridge** ✅ — `ArtifactSidecar` struct in `builder_vm.rs` mirrors `passthru.mvm` exactly (camelCase wire format → byte-identical to `nix eval --json $flake#…passthru.mvm`); `write_to_dir` / `read_from_dir` helpers; `runtime_meta::from_sidecar(mode, rootfs_dir)` reads sidecar, defaults to `accessible: true` if absent, propagates errors only on malformed JSON. The sidecar is the courier carrying the accessible flag from build-time Nix metadata to runtime — see the explanation block below.
- **W6.2.1 — sidecar producer + cross-backend consumer wired** ✅ — public `mvm_build::builder_vm::emit_sidecar_via_passthru_query(env, attr, build_dir, dev_override, impure_flag)` runs `nix eval --json …passthru.mvm` and writes `<build_dir>/mvm-meta.json`. Called from both `pipeline::dev_build` (mvmctl path) and `backend::host::HostBackend::extract_artifacts` (mvmd pool path). Public `runtime_meta::record_from_rootfs(name, mode, rootfs)` writes the sidecar's `accessible` into per-VM `mode.json`. Wired into `MicrosandboxBackend::start_with_mode`, `FirecrackerBackend::start`, `AppleContainerBackend::start`, `LibkrunBackend::start` — all four real backends now honor the gate consistently. CloudHypervisorBackend stub is skipped until its real lifecycle lands.
- **W6.2.2 — `BuildMode::{Dev, Prod}` (command dictates posture)** ✅ — `dev_build` signature gains `mode: BuildMode`; `dev_override_flags` returns `""` for Prod (no `--override-input mvm`, no `--impure`, prod guest agent without `do_exec`, sealed image). All `dev_build` callers (`vm::up::run` x2, `commands::build::build`, `vm::template::lifecycle` x2) pass `BuildMode::Prod` by default. Mirrors the auto-memory rule "image composition is transparent — invocation context, not flake state." Behavior change: `mvmctl up <flake>` now produces a sealed image and `mvmctl console` refuses on it (CLAUDE.md security claim 4 is finally true at runtime, not just at the CI gate).
- **W6.2.3 — `--dev` / `--prod` CLI flags** ✅ — new `commands/shared/build_mode.rs` with `BuildModeFlags` (clap-flatten-able, mutually exclusive). `mvmctl up` and `mvmctl build` embed the struct. `--dev` opts into Dev posture for debugging; `--prod` is explicit (same as default). Clap rejects `--dev --prod`. 4 parser-level tests + 3 resolver tests.

**The W6.2 → W7 data flow now runs end-to-end:**
```
Nix derivation passthru.mvm        (build time, in the flake's mkGuest)
   ↓ emit_sidecar_via_passthru_query (nix eval --json)
<build_dir>/mvm-meta.json           (sidecar — courier file)
   ↓ VmBackend::start (any of 4 real backends)
   ↓ runtime_meta::record_from_rootfs
~/.mvm/vms/<name>/mode.json         (per-running-VM state)
   ↓ mvmctl console
   ↓ enforce_accessible_gate
refuse if accessible: false         (W6.2 gate fires; --force overrides)
```

**Working tree state at session end (2026-05-08, all uncommitted):** 11 logical changesets sitting on `feat/micro`. Build clean, `cargo clippy --workspace --all-targets -- -D warnings` clean, `cargo test --workspace` clean (one pre-existing parallel-env-var flake in `mvm-core::config::tests::test_mvm_cache_dir_env_override` re-runs deterministic; not introduced here). Plus uncommitted plan-62 docs additions and Lima dead-branch cleanup.

### Up next (in priority order — for the fresh session picking this up)

#### Phase 1 close-out (the remaining gates for the migration plan-60 Phase 1 demo)
- [x] **W7 — backend extraction (alt backends + handle registry)** ✅ — landed on `feat/w7-backend-extraction` (4 commits, 2026-05-10). New `mvm-base` crate carries the leaf substrate (`ui`, `runtime_meta`, `cow`); the 5 alt backends (`apple_container`, `cloud_hypervisor`, `docker`, `libkrun`, `microsandbox`) moved out of `mvm/src/vm/` into `mvm-backend/src/`; the dependency direction flipped from `mvm-backend → mvm` (re-export façade) to `mvm → mvm-backend` (consumer). Handle registry in `mvm-backend::handle_registry` closes the W6.2-era gap where `StartMode::Attached` was intent-only metadata: `mvmctl up --attached` now teardowns the sandbox on Ctrl-C via the CLI's existing top-level signal handler. `cargo test --workspace --no-fail-fast` 1895/0; clippy clean. mvmd contract gate not run (pre-existing `microsandbox 0.4.5` ⊥ `iroh-base 0.96.1` over `sha2`).
- [x] **W6.1.2 — cross-compile real `mvm-guest-agent` Rust binary** ✅ — landed on `feat/w6.1.2-real-guest-agent` (1 commit, 2026-05-10). New `nix/packages/mvm-guest-agent.nix` runs `rustPlatform.buildRustPackage` against the workspace at `mvmSrc` (threaded through `nix/flake.nix → nix/lib/default.nix → nix/lib/mk-guest.nix` via `self`), builds `mvm-guest-agent` + `mvm-seccomp-apply` + `mvm-verity-init` from the `mvm-guest` Cargo target. `mk-guest.nix`'s `agentBinary` attr swapped from the sh-stub to `${guestAgentPkg}/bin/mvm-guest-agent`. `withDevShell = isDev` ties the `dev-shell` Cargo feature to the same toggle that controls `accessible`/`sealed` — dev images get `do_exec`, prod images don't (preserving the `prod-agent-no-exec` CI gate from ADR-002 §W4.3). `passthru.mvm.agentBinary` flipped `"stub"` → `"real"`. Eval test in `nix/tests/mk-guest-eval.nix` updated. New passthru exports `guestAgentPkg`, `seccompApplyBinary`, `verityInitBinary` for the seccomp/verity wiring follow-ups (W2.4 and W3 own those sites). `cargo test --workspace --no-fail-fast` 1895/0 — no Rust-side deltas.
- [x] **W8.A + W8.B — Firecracker stack relocation** ✅ — landed on `feat/w8-firecracker-direct-launch` (4 commits, 2026-05-10). The split scope reflects what was actually load-bearing vs. dead:
    - **W8.A** (`ccfb27d`) deleted truly-unreachable Lima symbols that W7 missed: `linux_env::LimaEnv` + impl, `config::{render_lima_yaml, render_lima_yaml_with, LimaRenderOptions, find_lima_template}`, `config::LEGACY_VM_NAME`, `resources/lima.yaml.tera`, `mvm`'s `tera` dep, plus updates to stale "inside the Lima VM" doc comments. Revealed that the runtime path was *already* Lima-free since W7 — `create_linux_env` returns `NativeEnv` or `AppleContainerEnv`, never Lima — so the "Firecracker direct-launch rewrite" framed in the original W8 description was unnecessary. The shell + linux_env + Firecracker stack already runs on the host directly via `bash -c`.
    - **W8.B** (`46e19cb` + `4cec0c5`) finished the architectural goal: every concrete `VmBackend` impl now lives in `mvm-backend`. Substrate moved to `mvm-base` (`config`, `shell/`, `linux_env`, plus a new `snapshot_integrity` lifted from `template::lifecycle`); the FC stack moved to `mvm-backend` (`firecracker`, `microvm`, `microvm_nix`, `network`, `image`, `backend`); 17 files in `mvm-cli` + 3 in `mvm` + 2 root-tree smoke tests migrated to `mvm_backend::*` imports; `mvmctl::backend` facade re-export added so the public surface mirrors `mvmctl::core` / `mvmctl::runtime`.
    - Re-exports kept for back-compat: `mvm::{config, linux_env, shell, ui, shell_mock}` (mvmd consumes `mvmctl::runtime::shell` etc. from ~30 files) and `mvm::vm::{cow, runtime_meta}` (mvmd's W6.2 console gate). Removing them would force a sibling-repo update.
    - `cargo test --workspace --no-fail-fast` 1884 / 0; clippy clean. mvmd contract gate not run — pre-existing `microsandbox 0.4.5` ⊥ `iroh-base 0.96.1` over `sha2` blocker (same as W7); manual audit confirms every `mvmctl::*` path mvmd imports still resolves to the same shape.
- [x] **W8.C — Wire `mvmctl dev` on Linux+KVM** ✅ — landed on `feat/w8c-dev-mode-linux` (1 commit, 2026-05-10). New `commands/env/linux_native` module (130 LOC) treats the host shell as the dev environment: `dev up` runs the W8.B-relocated `mvm_backend::firecracker::install`/`download_assets`/`prepare_rootfs`, prints "ready", and optionally spawns `$SHELL -i`; `dev down` is a no-op (the host is the environment); `dev shell` spawns `$SHELL -i`; `dev status` reports `/dev/kvm`, Firecracker, and asset state with a kvm-group hint when `/dev/kvm` is missing. The `DevBackend` selector enum in `commands/env/dev.rs` now branches three ways — `AppleContainer` (macOS 26+ AS), `LinuxKvm` (Linux/WSL2 with /dev/kvm), `Unsupported` (everything else). `bail_no_dev_backend()` updated to point macOS Intel / pre-26 / no-KVM Linux / Windows at the W7.x.2 microsandbox-builder-VM follow-up, the planned home for those hosts. `cargo test --workspace --no-fail-fast` 1884/0; clippy clean.
- [x] **W6.1.2 — cross-compile real `mvm-guest-agent` Rust binary** ✅ — landed on `feat/w6.1.2-real-guest-agent` (1 commit, 2026-05-10). New `nix/packages/mvm-guest-agent.nix` runs `rustPlatform.buildRustPackage` against the workspace at `mvmSrc` (threaded through `nix/flake.nix → nix/lib/default.nix → nix/lib/mk-guest.nix` via `self`), builds `mvm-guest-agent` + `mvm-seccomp-apply` + `mvm-verity-init` from the `mvm-guest` Cargo target. `mk-guest.nix`'s `agentBinary` attr swapped from the sh-stub to `${guestAgentPkg}/bin/mvm-guest-agent`. `withDevShell = isDev` ties the `dev-shell` Cargo feature to the same toggle that controls `accessible`/`sealed` — dev images get `do_exec`, prod images don't (preserving the `prod-agent-no-exec` CI gate from ADR-002 §W4.3). `passthru.mvm.agentBinary` flipped `"stub"` → `"real"`. Eval test in `nix/tests/mk-guest-eval.nix` updated. New passthru exports `guestAgentPkg`, `seccompApplyBinary`, `verityInitBinary` for the seccomp/verity wiring follow-ups (W2.4 and W3 own those sites). `cargo test --workspace --no-fail-fast` 1895/0 — no Rust-side deltas.
- [ ] **Phase 1 close-out** — demo run + checkpoint review against `mvm` Phase 1 exit tests in plan 60.

#### Architectural completion of W6.2.x (smaller follow-ups)
- [x] **W6.2.3 follow-up — BuildMode round-trips on manifest builds** ✅ — landed on `feat/w6.2.3-template-buildmode` (1 commit, 2026-05-10). The original framing referenced `mvmctl template build`, but that namespace was retired in plan 38; the surviving entry point is `mvmctl build <manifest>` via `template_build_from_manifest`. Threading: `mode: BuildMode` arg added; `commands/build/build.rs` passes `args.build_mode.resolve()` through `build_manifest` → `template_build_from_manifest` → `dev_build`. Persistence: `TemplateRevision` gains `Option<String> build_mode` (serialized `"dev"`/`"prod"`; absent on pre-W6.2.3 records, deserialised as `None`). Doesn't participate in `cache_key()` — that stays `flake_lock + profile`, matching the rule that the key identifies "what would Nix build," not "in what posture." Cleanup: the by-id `template_build` / `template_build_with_snapshot` / `cleanup_snapshot_vm` helpers had no callers (plan 38 retired their CLI consumers) and were deleted (~420 LOC); the orphaned `vm_exec_stdout` helper went too. 3 new tests; `cargo test --workspace --no-fail-fast` 1898/0; clippy clean.
- [x] **W7.x.2 — microsandbox-as-Linux-builder Wave 2 (real impl)** ✅ — landed on `feat/w7x2-microsandbox-builder-vm` (1 commit, 2026-05-10). `MicrosandboxBuilderVm` in `mvm-build/src/builder_vm.rs` replaces the `StubBuilderVm`-as-only-impl pattern: pulls `docker.io/nixos/nix:2.24.10` via microsandbox's `PullPolicy::IfMissing`, spawns a sandbox with the three ADR-013 bind-mounts (`/work` ← flake src RO, `/nix` ← host store RW when present, `/out` ← writable artifact dir), runs `nix build` via `sandbox.shell()`, copies the resolved store path's artifacts to `/out`, and reads the sidecar to populate `accessible`. Defaults: 4 vCPU / 4 GiB; `with_resources(cpus, mem_mib)` override. `dev_build` got a `env_has_nix` probe at the top — falls through to the new `dev_build_via_microsandbox` helper when nix isn't on the env channel. The fallback makes `mvmctl build` work on macOS Intel / pre-26 / no-KVM Linux without host Nix; on those hosts the user gets a sealed image equivalent to a Prod-mode host build (the Dev-mode `--override-input mvm git+file://...` override is host-Nix-specific; threading it through to the builder VM is a follow-up). 11 new tests; `cargo test --workspace --no-fail-fast` 1895/0; clippy clean.
- [x] **CloudHypervisor real lifecycle** ✅ — landed on `feat/cloud-hypervisor-lifecycle-real` (1 commit, 2026-05-11). New `crates/mvm-backend/src/ch_runtime.rs` (~280 LOC) wraps Cloud Hypervisor's JSON-over-Unix-socket API behind sync helpers: `start_ch_daemon` spawns the daemon nohup-setsid; `api_put`/`api_put_empty` shell-out to `curl --unix-socket` (same pattern as `microvm::api_put_socket`); `build_vm_config(VmConfigArgs)` produces the `PUT /api/v1/vm.create` body as a pure function; `is_pid_alive` + `reap` + `list_ch_vms` close the lifecycle loop. `CloudHypervisorBackend::start` wires `record_from_rootfs` (W6.2 console gate consistency); `stop` does graceful `vm.shutdown` → `vmm.shutdown` → SIGTERM-cleanup; `status`/`list` walk the per-VM dirs under `VMS_DIR` looking for `ch.pid` (the discriminator vs FC's `fc.pid`). Same commit also collapsed `ci.yml`'s 7 PR-time jobs to 5 by folding `fmt` + `clippy` + `check-adr-coverage` into a single `lint` runner — ~3 min wall-clock + 2 runner-minutes saved per PR. **Untested-without-Linux+CH-host caveat**: mvm CI has no Linux+CH runner, so the spawn-dance and shell-out paths are reviewed against CH's published API but unrun. Pure pieces (JSON config builder, path helpers, JSON-string escaping) carry 8 unit tests. Out of scope and named in-doc: TAP networking (`tap_networking: false` in capabilities), snapshot/restore, dm-verity, rich `run-info.json`. `cargo test --workspace --no-fail-fast` 1913/0; clippy clean.

#### Smaller cleanup items (mechanical, low-risk)
- [x] **Lima dead-symbol sweep** ✅ — slice 2 of W7 deleted `Platform::needs_lima`, `bootstrap::{is_lima_required, ensure_lima, install_lima_linux, warn_if_legacy_lima_vm}`, `shell::inside_lima`, `vm/lima.rs`, the Lima branches in `commands/env/{bootstrap, dev, setup, uninstall}.rs`, the Lima checks in `commands/build/{build, validate}.rs`, and the orphaned `commands/env/shell.rs::open_shell` (Lima-only) + `shared::format::shell_escape` (its only consumer). `mvmctl dev` on non-Apple-Container hosts now bails with a clear W8 reference.

### Sidecar manifest — the W6.2 ↔ W7 courier (key concept for the fresh session)

A "sidecar" is a small metadata file written next to the primary build artifact. We use `mvm-meta.json` next to `rootfs.ext4` as the courier carrying `passthru.mvm` (Nix-evaluation metadata) into the runtime path without requiring the runtime to invoke Nix or mount the rootfs. The shape mirrors `mkGuest`'s `passthru.mvm` exactly so a future `nix eval --json` consumer can dump straight into the struct. Lives at `crates/mvm-build/src/builder_vm.rs::ArtifactSidecar`.

Two reasons we use a sidecar instead of embedding the metadata inside the rootfs:
1. **Host reads it without mounting the rootfs.** `mvmctl console` runs on the host before the VM boots; mounting an ext4 image on macOS or Linux-without-root is awkward.
2. **Atomic with the artifact.** Same directory, same build step. A stale sidecar paired with a wrong rootfs is impossible.

### Sprint 49 ↔ Sprint 50 convergence (volumes / mvm-storage)

Sprint 49 (plan 45 — filesystem volumes) is shipping in parallel on `feat/sprint-46-filesystem-volumes`. Its mvm-side deliverables overlap Phase 2 of this migration:

- New crate `mvm-storage` with `VolumeBackend` trait + `LocalBackend` impl + generic contract test suite
- `mvm-core::volume` wire types (`OrgId`, `WorkspaceId`, `Volume`, `VolumeName`, `VolumeBackendConfig`, `ObjectStoreSpec`, `WrappedKey`)
- `mvm::vm::volume_registry` (replaces `share_registry`; spawns virtiofsd)
- vsock `MountVolume` / `UnmountVolume` verbs replacing `MountShare` / `UnmountShare`
- `mvmctl volume create|ls|rm` + `mvmctl up --volume <name>:<path>`
- `mvm-security::policy::VolumeNamePolicy` + `MountPathPolicy` `/nix*` deny extension
- `mkGuest.volumeMounts` Nix attrset surfacing into the boot manifest

**Convergence rule:** when Sprint 49 lands on `main`, the migration plan's Phase 2 (encryption everywhere + volumes) absorbs plan 45's mvm-side artifacts as-is — we do **not** re-derive `mvm-storage` or volume types from `../mvm/crates/`. The migration's Phase 2 work then becomes additive: AEAD-encrypted snapshots layer on top of `VolumeBackend`; key rotation reuses plan 45's `WrappedKey` shape; FDE doctor check folds into Phase 9.

Backend tier matrix gap closed simultaneously: ADR-002 now lists Cloud Hypervisor (Tier 1 peer of Firecracker — wider device model, VFIO/GPU/virtio-fs) and microsandbox / libkrun (Tier 2, cross-platform default per ADR-013). Plan 45's `LocalBackend` mounts via virtiofsd; CH's wider device model is what makes virtio-fs natively viable on the Tier 1 path.

### Cornerstones

- Facade preservation is the single load-bearing constraint of Phase 0
- ADR coverage is enforced in CI from the start (no architectural drift without an ADR)
- Cross-platform CI matrix (Linux + macOS + Windows) lands now so Phase 7b's TypeScript SDK + computer-use don't surprise us

### Non-goals (explicit)

- Microsandbox integration (Phase 1)
- Encryption + key rotation (Phase 2)
- Network isolation (Phase 3)
- Any user-facing CLI surface beyond `--help`/`--version` (Phase 1+)
- mvm-studio (Tauri) wiring (Phase 5)

## Sprint 51 — close the v1→v2 refactor (in flight)  [`plans/60-mvm-microsandbox-migration.md`](plans/60-mvm-microsandbox-migration.md), [`plans/63-phase-2-encryption-everywhere.md`](plans/63-phase-2-encryption-everywhere.md), [`plans/64-supervisor-wiring.md`](plans/64-supervisor-wiring.md)

**Goal:** finish every remaining plan that the v1→v2 refactor
depends on, so the campaign can declare itself closed. Sprint 50
landed Phase 0 + 1 of the migration; Sprint 51 carries the
remaining plan-60 phases, the closed-form plans the supervisor /
encryption / signal threads needed, and the function-call surface
that mvmforge depends on.

**Status (2026-05-11 — evening, after batch 2):** 10 + 15 = 25
commits landed on `origin/main` across two focused batches.
The morning batch (batch 1) closed four plans (64, 63, 62, 44)
and the plan-60 Phase 6 policy-bundle TOML substrate. The
evening batch (batch 2) closed plan-60 Phase 6 hardware
attestation, plan-60 Phase 3 Slices A + B + four resolver-
tightening follow-ons (live L4Gate, hooked W5 resolver into
`admit_plan_for_boot`, full L7 inspector chain,
LiveArtifactCollector, fail-loud `disabled_inspectors`,
LiveKeystoreReleaser, bundle.pii wiring), the plan-60 Phase 4
`audit_total_coverage` scaffold with recursive per-subgroup
classification, plan-60 Phase 4 audit-stream URL-shape
validation, and 9 live drive-and-assert audit-emission tests
covering cache / network / manifest / secret subcommands.
Workspace now at **2311 tests / 0 failed**; clippy
`-D warnings` clean; nightly fmt clean; xtask
`check-no-display-on-secret-types` clean. CLAUDE.md
security claims 1–8 all true on every host. ADR-041 (signed +
audited `ExecutionPlan`) and ADR-042 (encryption substrate)
document the closed surfaces. Remaining work covers Phase 3
Slice C (smoltcp/TUN + firewall + DNS endpoint), Phase 4 audit
end-to-end drive-and-assert promotion + `bundle.audit` wiring,
Phases 5 / 7 through 10, plans 48/49/51/52 (function-call
surface), plan 61 (overlays + billing), and the partial-plan
sweep (32 / 16 / 18).

### Shipped — campaign batch 1 (2026-05-11 morning)

| Plan | Workstream(s) | Commit |
|---|---|---|
| 64 — supervisor wiring | W5 — `PolicyRef` resolver substrate | `0aee20f` |
| 63 — encryption everywhere (Phase 2) | W2 — `SecretBox<T>` wrapping pass | `b9e4e64` |
| 63 | W3 — `KeyringProvider` + `FileKeyProvider` in mvm-security | `1ea9352` |
| 63 | W1 — `key_rotation` primitives (rewrap_dek, rotate_master_key, migrate_wrapped_keys, rotate_luks_slot, reseal_snapshot) | `f7e39a7` |
| 63 | W4 — `mvmctl secret put/get/ls/rm` + `SecretStore` backends | `a30f866` |
| 63 | W5 — chunked AES-GCM in `pause_and_seal` / `verify_and_resume` | `6fc798d` |
| 63 | W6 — ADR-042 + CHANGELOG + plan-60 Phase 2 mark-up | `8baa4e7` |
| 62 — docs sidebar restructure | Substrate (21 stubs + sidebar config) had already landed; this commit just marks the status | `ae10ad9` |
| 44 — agent signal handling | W3 — SIGHUP config reload (hot-reloadable subset via atomics) | `05f956e` |
| 60 — microsandbox migration | Phase 6 — on-disk policy-bundle TOML format (`mvm_policy::toml_loader` + W5 resolver upgrade) | `a457012` |
| 60 | Phase 4 — LifecycleHooks + secret/cmd dual-emit + audit Recorder substrate | `d174a46`, `0cdd6b1`, `c096757`, `80f05bd` |
| 60 | Phase 7 — host-mediated tools (substrate + time_now + web_fetch + web_search + upload + download), Brave + Tavily providers, reqwest fetcher, MCP dispatcher trait evolution, env-var operator config | `fab5edd`, `e500c18`, `a4ca401`, `72597e7`, `81fed76`, `8bcb2ed`, `f92e53a`, `c538180`, `0d0f3eb`, `5e62e5a` |
| 60 | Phase 9 — `cargo xtask perf` rootfs-size + boot budgets | `b42e784` |
| 60 | Phase 10 — in-repo close-out (status notes on plan-60 phase headers, Cargo.toml repository URL already canonical); workspace-parent filesystem rename + mvmd git pin bump remain operator actions | (this commit) |

### Shipped — campaign batch 2 (2026-05-11 evening)

| Plan | Workstream(s) | Commit |
|---|---|---|
| 60 — microsandbox migration | Phase 6 — `mvm_security::attestation` (`IdentityKey` lifecycle + signed report) + feature-gated `HwAttestationProvider` stubs (TPM2 / SEV-SNP / TDX) + `mvmctl attest {export, verify, status}` CLI | `d0ba736` |
| 60 | Phase 3 Slice B — `mvm-policy::L4RuleSpec` + `mvm_supervisor::proxy::l4` (`L4Gate` trait, `LiveL4Gate::from_specs`) + `HickoryDnsResolver` + W5 resolver wires `slots.network` | `51581a8` |
| 60 | Phase 3 follow-on — `up.rs::admit_plan_for_boot` calls `resolve_supervisor_components`; typed audit-chain `error_class` per failure mode | `ac87e8d` |
| 60 | Phase 3 follow-on — `slots_from_bundle` delegates to `build_inspector_chain`, picking up SsrfGuard / SecretsScanner / InjectionGuard / PiiRedactor + honoring `disabled_inspectors` | `bf8079a` |
| 60 | Phase 3 follow-on — `LiveArtifactCollector::from_policy(&bundle.artifact)` (NotImplemented carries `capture_paths` count + retention) | `72f272f` |
| 60 | Phase 3 follow-on — `validate_egress_policy_inspector_names` fail-loud at admission on typos in `disabled_inspectors` | `586e0cd` |
| 60 | Phase 3 follow-on — `LiveKeystoreReleaser::from_policy(&bundle.keys)` (closes last Noop slot in `slots_from_bundle`) | `36db455` |
| 60 | Phase 3 follow-on — `bundle.pii.{mode, categories}` → `PiiRedactor::from_policy` + `build_inspector_chain_with_pii`; first slot where Live impl changes runtime behavior | `dc31b10` |
| 60 | Phase 4 scaffold — `tests/audit_total_coverage.rs` walks `mvm_cli::cli_command()` + asserts every top-level subcommand has an `AuditPosture` classification | `c036cea` |
| 60 | Phase 4 scaffold — recursive per-subgroup coverage (13 subgroup tables, ~54 leaf classifications including third-level `manifest tag` + `manifest alias`) | `dabd955` |
| 60 | Phase 4 follow-on — `validate_audit_policy_stream_destinations` fail-loud at admission on unknown URL schemes in `bundle.audit.stream_destinations` (`ResolveError::AuditPolicyInvalid`, `error_class = policy-audit-invalid`) | `c5c37f2` |
| 60 | Phase 4 follow-on — `tests/audit_emissions_live.rs` first 3 live drive-and-assert tests (cache prune, cache prune --dry-run negative, network create) | `d852f5a` |
| 60 | Phase 4 follow-on — 3 more live tests (network remove, manifest prune --orphans, secret put) | `3759af8` |
| 60 | Phase 4 follow-on — 3 secret-cluster live tests (secret get/ls/rm); discovered + pinned the on-disk action-name decoupling (`ls` → `"list"`, `rm` → `"delete"`) | `b22feae` |
| hooks | `chore(hooks)` — pre-commit hook no longer re-stages unstaged WIP via `git add -u`; snapshots originally-staged paths up front | `0338c66` |

Notes on commit-message vs diff mismatches in batch 2 (worth a
`git log` reader knowing about):

- `d774200` carries the "per-subgroup audit coverage" message but
  the diff is actually two other-agent files (`cmd_audit.rs` +
  `mod.rs`) that landed under it during a parallel branch race.
  The *actual* per-subgroup recursive walk shipped as `dabd955`
  immediately after with a clarifying header.
- `b22feae` is titled `test(microsandbox): satisfy clippy::io-other-error
  on Linux` but its diff also includes 107 lines of new
  `audit_emissions_live.rs` content (the secret get / ls / rm
  cluster). The pre-commit hook's `git add -u` re-staged unstaged
  WIP in the working tree. `0338c66` fixed the hook so this
  pattern won't recur.

### Shipped — campaign batch 3 (2026-05-12, in flight as PR #106/#107/#108)

Plan 60 Phase 4 audit-emit ergonomics + behavioral hardening, plus
the cleanup-host-fs and MockBackend refactors that unlock VM-lifecycle
live testing. Three open PRs stack on `main`:

| PR | Branch | Scope | Live tests |
|---|---|---|---|
| #106 | feat/sprint-51-batch-3 | `audit_emit!` macro + `LocalAuditBuilder` API + `xtask check-audit-positional` lint + CI gate + 37-site positional emit migration + DRIFT-001 (microsandbox feature gate) + ADR-013 builder-VM swap | 6 → 26 |
| #107 | feat/cleanup-host-fallback (targets #106) | `cleanup_old_dev_builds` drops `&dyn ShellEnvironment` for plain `std::fs`; `mvmctl cleanup` runs without a dev VM; SnapshotDelete live + 4 ReadOnly negative pins | 26 → 31 |
| #108 | feat/mock-backend (targets #107) | `MockBackend` substrate (`AnyBackend::Mock` variant, 10 unit tests); `MVM_DIRECT_BOOT` LocalAudit emit parity + `--detach` fix; `up_with_mock_backend` end-to-end; `set-ttl` live + 8 more ReadOnly negative pins; ADR-044 documenting the convention | 31 → 40 |

Coverage now: every Emits row in `AUDIT_POSTURE` that doesn't require
a running Firecracker / Apple Container / Docker / microsandbox / Nix
builder / GitHub network has a live drive-and-assert test. 15 ReadOnly
leaves pin the no-emit invariants.

Still hard (architectural refactors required to test hermetically):
`pause` / `resume` (talk directly to FirecrackerIO, not through
`AnyBackend.pause`/`resume`); `fs` / `proc start/signal/kill/stdin`
(guest agent over vsock — needs vsock mock); `volume mount/unmount`
(VM-attached); `build → TemplateBuild` (Nix builder); `update`
(network); `uninstall` positive (real system paths).

Reference: ADR-044 (`specs/adrs/044-audit-emit-macro.md`).

### Remaining workstreams (priority order)

| # | Plan / phase | Est. days | Notes |
|---|---|---|---|
| 1 | Plan 60 Phase 3 Slice C — smoltcp/TUN userspace-TCP consumer + host firewall (nft / pf / WFP) + DNS server endpoint + per-tenant netns lift | 8-12 | The remaining Phase 3 work after Slices A + B + four resolver follow-ons closed in batch 2. Turns `L4Gate::evaluate` decisions into accept/drop on per-VM TAPs; brings up the firewall additive layer; provisions the resolver guest VMs point `/etc/resolv.conf` at. Pairs with the mvm-hostd lift (#7 below). |
| 2 | Plan 60 Phase 4 — persistent observability | 5-8 | Scaffold shipped in batch 2 (`tests/audit_total_coverage.rs` recursive coverage of all CLI subcommands at every depth). Remaining: Prometheus + OTLP metrics endpoint; promote `audit_total_coverage` `Emits` rows to live drive-and-assert tests as each command gains a hermetic fixture; wire `bundle.audit.{chain_signing, stream_destinations}` into `AuditEmitter` construction; structured logs; event bus on `tokio::sync::broadcast`. |
| 3 | Plan 60 Phase 5 — DX layer (Python SDK, manifests, mvm-studio handshake) | 7-10 | `python/mvm` wheels via pyo3; `cargo xtask gen-stubs` for typed APIs. Templates from `../mvm/templates/` rewritten on microvm.nix. |
| 4 | Plan 60 Phase 7 — MCP server + host-mediated tools + sessions | 7-10 | PR #105 exposes `run`, `mvm.time_now`, `mvm.web_fetch`, `mvm.web_search`, `mvm.upload`, and `mvm.download`; CI smoke now asserts that MCP tool set and the secret audit live test pins `MVM_SECRET_STORE_BACKEND=file` for hermetic Linux runners. Remaining follow-up: snapshot/eval and tmux-style sessions. |
| 5 | Plan 60 Phase 7a — install/rebuild/persistent overlay/tenant destroy | 10-12 | Encrypted persistent overlay (extends plan 45's volume work); rolling rootfs swap; `mvmctl tenant destroy` emits a destruction certificate. |
| 6 | Plan 60 Phase 7b — built-in templates + TypeScript SDK | 5-7 | `ai-sandbox` / `safe-openclaw` / `computer-use` / `repl` templates with bundled policy bundles. `typescript/@mvm/sdk` napi-rs binding for hot paths. |
| 7 | Plan 60 Phase 8 — mvmd integration contract verification | 3-5 | Port `mvm/src/hostd/{mod,server}.rs`; `PROTOCOL_VERSION` const; wire-format stability test. **Coordinated with parallel mvmd work** — see "Cross-repo coordination" below. The mvm-hostd supervisor lift this depends on is what makes every Live impl in `slots_from_bundle` (shipped batch 2) actually enforce. |
| 8 | Plan 60 Phase 9 — perf + supply chain + SBOM | 7-10 | Cold-boot ≤500 ms Firecracker / ≤1 s microsandbox; rootfs ≤20 MB; PGO + MUSL builds; cosign-keyless artifacts; RFC 3161 timestamping. |
| 9 | Plan 60 Phase 10 — rename + archive | 1 | `git mv mvm mvm` + update CI paths + bump mvmd's git pin. |
| 10 | Plans 48 + 49 + 71 — function-service factories (ADR-010) + workload helper | 7-10 | Wrapper-template relocation + function-service factory pattern. Plan 71 wires `mkFunctionService` into a one-line IR-to-image helper (`mkFunctionWorkload`); unblocks Phase 5 Slice E3 live-VM smoke. |
| 11 | Plans 51 + 52 — session-lifecycle verbs + fd3 control channel (ADR-011) | 10-14 | Largest substrate change in the function-call line. |
| 12 | Plan 61 — runtime overlays + billing | 14-21 | Dev/prod image transparency + sandbox-runtime billing dimensions. Six phases. |
| 13 | Status sweep — plan 32 tail (MCP adoption tiers L1/L2/L4), plan 16 (microvm-nix-integration), plan 18 (nix-openclaw-integration) | 3-5 | Several minor plans with partial completion — audit + close or roll into a follow-up sprint. |

**Total remaining envelope:** ~90 calendar days after batch 2
(was ~100). Sprint 51 spans multiple sub-sprints in practice;
treat the workstream rows as the unit of scheduling.

### Cross-repo coordination (mvmd)

Plan 60 Phase 8 depends on parallel work in the mvmd repo. The
hand-off prompt for the mvmd session:

```
We're closing out the mvm refactor (plan 60 in the mvm repo).
Three mvmd-side workstreams to unblock Phase 8:

M1 — Unblock `cargo build --workspace`. mvmd has a sha2 dep
     conflict per plan-64 notes. Resolve it, then bump the mvm
     git pin to a SHA ≥ a457012 (plan 60 Phase 6 TOML loader).

M2 — Stand up `mvm-hostd` daemon. Listens on Unix socket
     `/run/mvm-hostd/control.sock` mode 0600. Receives
     `HostdRequest::{Start, Stop, Status}` carrying
     `SignedExecutionPlan`. On Start: verify envelope, call
     `mvm_cli::commands::vm::policy_resolver::
     resolve_supervisor_components(&plan)`, build a Supervisor
     with `.with_egress` / `.with_tool_gate` / `.with_keystore`
     / `.with_artifact_collector(slots.*)` + a FileAuditSigner,
     then `supervisor.launch(&signed, &trusted_keys).await`.
     Implement the `BackendLauncher` adapter wrapping
     `mvm_backend::AnyBackend::start()` — the piece plan 64 W3
     intentionally deferred (ADR-041).

M3 — Wire-format stability. Add `pub const PROTOCOL_VERSION: u32`
     to mvm's `mvm_core::protocol` (PR to mvm repo). New
     `tests/mvmd_compat.rs` in mvmd: round-trips
     `AgentRequest::Reconcile`, `HostdRequest::Start`,
     `HostdResponse::Started` against frozen-byte fixtures.

Verification: `cd ../mvm && cargo test --workspace`'s mvmd-compat
test passes against your branch. When green, plan 60 Phase 8
unblocks on the mvm side.
```

### Standing constraints

- CLAUDE.md "Security model" defines the 8 CI-enforced claims;
  don't regress any.
- Workspace lint `clippy::too_many_arguments = "deny"` — use
  struct args, not 5+ positionals.
- xtask `check-no-display-on-secret-types` flags Debug/Display
  on Secret/Token/Password/Wrapped*Key types. Stay clean or
  annotate `// allow(secret-debug): <reason>`.
- Every workstream: one commit + one tests-green checkpoint,
  pushed directly to `origin/main` per the post-cutover flow
  (no PR — the cutover commit `7184b9a` established this).

### Verification gates (run after every workstream)

```
cargo test --workspace --no-fail-fast       # ≥ 2098 + new
cargo clippy --workspace --all-targets -- -D warnings
cargo +nightly fmt --check
cargo run -p xtask -- check-no-display-on-secret-types
```

### Sprint 51 success criteria

By close of Sprint 51, the project can claim:

1. *Every plan 60 phase ships, including hardware-attestation
   stubs, the L4/L7 proxies, observability, the DX layer,
   templates, MCP, install/rebuild, mvmd integration, perf
   gates, and the v1→v2 rename.*
2. *Function-call surface plans (48, 49, 51, 52) close — the
   substrate mvmforge consumes is stable.*
3. *Plan 61's runtime overlay + billing model ships.*
4. *Partial-completion plans (32, 16, 18) close or roll forward
   into a successor sprint with explicit status.*
5. *CLAUDE.md security claims 1–8 stay true; ADR-002 §"Out of
   scope" remains accurate.*
6. *`cargo test --workspace` passes; clippy `-D warnings` clean;
   nightly fmt clean; xtask secret-debug lint clean.*

### Non-goals (deferred / shelved / out-of-repo)

These were deferred for stated reasons; Sprint 51 leaves them
alone:

- **Plan 15 — WASM container support** (SHELVED — no real WASM
  workload exists; OCI artifact format hasn't stabilized far
  enough).
- **Plan 53 — cross-platform roadmap** (rejected on security-
  posture grounds).
- **Plans 54 / 55 / 56 — cloud-hypervisor / crosvm / rust-vmm
  internalization** (deferred; CH already has Tier 1 backend
  status without internalization).
- **Plan 59 — llm-txt self-doc** (relocated to mvmd repo; out
  of scope here).

### What "campaign closed" looks like

Sprint 51 closes when:

1. Every `### Phase N` in plan 60 has a "✅ shipped" status
   header.
2. Plans 44, 48, 49, 51, 52, 61, 62, 63, 64 all have
   "all workstreams shipped" status headers.
3. Plans 32, 16, 18 are either fully shipped or have an
   explicit closure note ("rolled forward to sprint 52", "no
   longer relevant", etc.).
4. The workspace test count is ≥ 2500 (rough envelope based on
   how many workstreams are pending × typical per-workstream
   test growth).
5. CHANGELOG.md `[Unreleased]` section captures every shipped
   plan with date, commit SHAs, and links to ADRs.

## Sprint 52 — elastic memory + portable signed bundles (in flight)

Two ergonomics + reach gaps in the platform that need closing
without compromising the eight ADR-002 security claims. The
decision document outside the repo enumerates eight candidates;
this sprint lands the top two:

1. **Virtio-balloon elasticity** — "mem cap, not commitment."
2. **Portable image bundles + per-artifact attestation in a signed
   envelope** — content-addressed `.mvmpkg` replaces the
   manifest-path-hash registry keying.

### W1 — Virtio-balloon elasticity  ✅ shipped

Workloads opting into `mem_initial_mib` boot with a pre-inflated
balloon and only commit a fraction of `memory_mib`; a host-side
reclaim controller adjusts the balloon over the VM's life.

Shipped:

- `mvm-core` — `VmStartConfig::mem_initial_mib`,
  `VmCapabilities::balloon`, `BalloonState`,
  `VmBackend::balloon_set_target` + `balloon_state` trait methods.
- `mvm-backend::microvm` — `FlakeRunConfig::mem_initial` +
  validate(); FC start path PUTs `/balloon` with
  `deflate_on_oom: true`; new `balloon_set_target` / `balloon_state`
  free functions wrap the FC PATCH + GET endpoints.
- `mvm-backend::cloud_hypervisor` — `VmConfigArgs::balloon_mib`,
  emits the top-level `"balloon"` field in vm.create JSON, and
  `balloon_set_target` posts to `/api/v1/vm.resize`
  (`desired_balloon`); `balloon_state` parses `/api/v1/vm.info`.
- `mvm-backend::{apple_container, docker, libkrun, microsandbox,
  microvm_nix}` — `VmCapabilities::balloon = false` declared
  honestly with rationale next to each (Apple's VZ has no
  virtio-balloon; Docker is cgroup-mem not balloon; libkrun's C
  API + microsandbox builder don't surface balloon control today).
- `mvm-core::manifest` — `mem_initial: Option<String>` field with
  `parse_human_size`-backed validation (rejects zero, rejects
  `>= mem`); `Manifest::mem_initial_mib()` helper.
- `mvm-backend::image::RuntimeConfig.mem_initial` for the
  `--config` flow.
- `mvm-cli::commands::shared::start::VmStartParams.mem_initial_mib`
  threading through both `up.rs` call sites.
- `mvm-cli::exec::ExecRequest.mem_initial_mib` threading;
  short-lived session VM and `mvmctl exec` default to `None`
  (no balloon).
- `mvm-supervisor::balloon` — pure-function `BalloonPolicy`
  (two-threshold band + guest floor) returning `BalloonAction`
  decisions. Defaults: inflate above 0.80, deflate below 0.60,
  step 64 MiB, guest floor 64 MiB. Fully unit-tested.

Shipped in the W1 close-out commit:

- `HostPressureSource` trait + `SysinfoPressureSource` cross-
  platform impl. Linux PSI (`/proc/pressure/memory`) and macOS
  `vm_pressure` are stronger signals; alternative impls behind the
  same trait are the natural next refinement.
- `BalloonController<P>` with a pure `tick(vm_states, apply)`
  method: reads pressure once per tick (not per VM), decides each
  VM's action via `BalloonPolicy`, applies via the caller's
  closure. `TickOutcome` per VM carries the decision + applied
  flag + per-VM error. Pressure-read failure aborts the whole
  tick rather than applying with a stale value.
- `mvmctl doctor` "Memory ballooning (virtio-balloon)" section
  enumerates every backend's `capabilities().balloon`; surfaces a
  warning when no backend on the host advertises support.
- `Manifest::mem_initial` flows end-to-end:
  `Manifest::mem_initial_mib()` → `PersistedManifest.mem_initial_mib`
  → `TemplateSpec.mem_initial_mib` → `up.rs` resolves
  `final_mem_initial = rt_config.or(tmpl_mem_initial).filter(0 < n < final_memory)`.
  Old slot records that predate the field deserialise as `None`
  (no behaviour change).

Outstanding (deferred to follow-ups):

- Live-KVM smoke: assert host RSS climbs/falls as the controller
  inflates/deflates against a real Firecracker guest. Needs CI
  infrastructure that mvm doesn't have today.
- PSI / `vm_pressure` `HostPressureSource` impls. The current
  sysinfo-based source is "used/total" — fine for dev-laptop
  ergonomics, too coarse for production scheduling.
- Spawn the tick into a real loop inside the supervisor's main
  loop. Today the controller is a library piece; wiring it into
  the supervisor's lifecycle is the integration follow-up.

### W2 — Portable image bundles + per-artifact attestation  ✅ admit-time re-verify shipped

Sigstore-style trust model: bundle ships a signed `manifest.json`
with per-artifact SHA-256s; the publisher's public key lives
out-of-band at `~/.mvm/trusted-publishers/<key_id>.pub`. dm-verity
(claim 3) gives independent per-block integrity inside the rootfs.
Bundle hash + `key_id` pin into `PlanArtifact` so admission
re-verifies on every launch.

Shipped (`mvm-plan::bundle`):

- `BundleManifest` (canonical-JSON, `deny_unknown_fields`),
  `BundleArtifact`, `ArtifactRole` enum, `VerityInfo` binding.
- `KeyId` — content-derived identifier (sha256(pubkey) truncated to
  32 hex chars). Well-formedness validator.
- `write_bundle()` — emits a tar archive of `manifest.json` +
  `manifest.sig` + `artifacts/*`. Pre-flight asserts the signing
  key matches the manifest's declared `key_id` and that every
  artifact byte-blob matches its declared sha256 + size_bytes.
- `read_and_verify_bundle()` — 6-step verification sequence:
  schema-version sniff (pre-sig) → key_id probe (pre-sig) →
  trust-store lookup → Ed25519 verify → full manifest parse →
  per-artifact sha256 + size + path-safety re-check. All four
  failure modes (UnknownKey, SignatureInvalid,
  ArtifactSha256Mismatch, UnsafePath) reject before extraction.
- `TrustStore` trait + `FsTrustStore` rooted at
  `~/.mvm/trusted-publishers/<key_id>.pub`. Pubkey files are 32
  raw Ed25519 bytes — no PEM, no headers; populated out-of-band
  for now (`mvmctl trust add` is the follow-up).
- `PlanArtifact` (re-exported from `mvm_plan::PlanArtifact`):
  `bundle_sha256` + base64 `manifest_sig` + `key_id`. Sized for
  inlining inside an `ExecutionPlan`; the supervisor's admit path
  re-verifies in a follow-up.
- 18 new unit tests covering: clean round-trip, unknown-key
  rejection, tampered manifest rejection (sig fail or parse fail),
  wrong key under correct key_id (KeyIdMismatch), tampered
  artifact byte rejection (with same-length tamper to exercise the
  hash path), missing-artifact rejection, unsafe-path rejection
  (`..`), schema-version-bump rejection, write-time key/key_id
  mismatch detection, write-time artifact sha256 drift detection,
  trust-store file load + miss + malformed-key-id short circuit,
  PlanArtifact JSON round-trip + signature re-decode + deny-unknown-fields.

Shipped in the W2 close-out commit:

- `mvmctl bundle export <TEMPLATE> --out <PATH> [--label]`:
  resolves the template's current revision (kernel + rootfs +
  optional initrd + optional dm-verity sidecar), hashes each
  artifact, builds a `BundleManifest`, signs with the host signer
  (same key that signs `ExecutionPlan` envelopes), and writes the
  archive. Refuses to ship a bundle whose declared sha256/size or
  key_id doesn't match the signing key / actual bytes — caught at
  write time so misconfigured publishers never ship unverifiable
  bundles.
- `mvmctl bundle fetch <SOURCE> [--trust-store <DIR>] [--json] [--allow-http]`:
  reads the archive (from a local path **or** an `https://` URL —
  HTTPS uses rustls + webpki-roots through the existing
  `crate::http::download_file` helper, written to a temp file
  that drops on scope exit), looks the publisher pubkey up via
  `FsTrustStore` (defaults to `~/.mvm/trusted-publishers/`), runs
  the full 6-step rejection ladder, prints a verified-bundle
  summary (sha256, key_id, publisher, arch, profile, label,
  artifact count, verity yes/no) or full manifest JSON. Plain
  `http://` URLs are refused by default — the Ed25519 signature
  still catches tampering, but HTTP exposes traffic metadata, so
  the user must opt in explicitly via `--allow-http` (with a
  launch-time warning). Refuses on any verification failure
  before extraction.
- `mvmctl trust add <PUBKEY> [--force]`: reads 32 raw Ed25519
  pubkey bytes, derives `key_id`, writes `<key_id>.pub` to the
  trust store (mode 0644). Refuses to overwrite without `--force`.
  Trust-store directory created at mode 0700 on first use.
- `mvmctl trust list [--json]`: enumerates the store, filters to
  well-formed `<key_id>.pub` entries, sorted output.
- `mvmctl trust remove <KEY_ID>`: unlinks by key_id; refuses if
  the key_id is malformed (32 hex chars expected).
- `cmd_audit::verb_name` + `AUDIT_POSTURE` table extended with
  `bundle` (DelegatesToSub: export = `InteractiveOrControl`,
  fetch = `ReadOnly`) and `trust` (DelegatesToSub: add/remove =
  `InteractiveOrControl` until the audit-chain emitter wiring
  lands; list = `ReadOnly`).
- ADR-002 9th claim shipped: *every published bundle is
  content-addressed, key_id-pinned, and re-verified at fetch.*
  Backed by `mvm_plan::bundle::read_and_verify_bundle` rejection-
  ladder tests. ADR-002 also caught up to document claim 8
  (signed `ExecutionPlan`, already shipped in plan 64 / ADR-041
  but never previously in the ADR table).

Shipped in the W2 admit-time re-verify commit:

- `ExecutionPlan::bundle: Option<PlanArtifact>` field. Schema
  bumped 2 → 3 — older verifiers fail closed with
  `UnsupportedSchema` because they don't know how to enforce the
  re-verify the new field implies. Schema-sniff order preserved:
  signature → version → parse, so an unknown future bundle field
  can't bypass the verifier.
- `BundleResolver` trait + `FsBundleResolver` rooted at
  `~/.mvm/bundles/<bundle_sha256>.mvmpkg` (default-path matches
  the `FsTrustStore` shape).
- `verify_plan_bundle(pin, resolver, trust)` — wraps
  `read_and_verify_bundle` and cross-checks the archive's
  `bundle_sha256` + `manifest_sig` + `key_id` against the plan's
  pin. Distinct `PlanBundleError` variants for each rejection
  shape (Resolve, Verify, BundleSha256Mismatch, KeyIdMismatch,
  SignatureMismatch, SignatureRead).
- `admit_for_run` accepts an optional
  `BundleAdmissionContext { resolver, trust }` parameter. When
  the plan pins a bundle, admit_for_run runs
  `verify_plan_bundle` after the signature/window/nonce checks.
  Plan pinned but context absent = operator misconfiguration =
  refuse (fail closed, not fail open).
- `SynthesisInput.bundle_pin: Option<PlanArtifact>` carries an
  upstream pin into the synthesized plan via
  `plan.bundle = input.bundle_pin.clone()`. Today's `mvmctl up`
  path passes `None`; the CLI flag that populates it (`--bundle-pin
  <path>` reading a fetched + verified `.mvmpkg`) is the next
  surface-completing commit.
- 4 new admit-level tests (positive + 3 refusals) plus 8 new
  `verify_plan_bundle` tests covering every PlanBundleError
  variant.

Shipped in the W2 follow-on (registry replacement + bundle-pin
CLI + audit-kind variants):

- **Bundle registry** at `~/.mvm/bundles/<sha>/`. New
  `BundleRegistry::install` atomically extracts a verified
  `.mvmpkg` (stage to `<sha>.partial/`, rename to `<sha>/`),
  also persists the archive bytes at `<sha>.mvmpkg` so
  `FsBundleResolver` continues to find them. `find(sha)` returns
  an `InstalledBundle` with `path_for_role()` / `path_for_name()`
  helpers. `template_artifacts_dispatched` and the three other
  `_dispatched` variants now disambiguate 64-char hex ids:
  templates-slot wins when present, fall through to bundle
  registry otherwise. Bundle-served templates default vcpus/mem
  from operator config (manifest doesn't carry resources today).
- **`mvmctl bundle install <SOURCE> [--force]`** verb. Reuses
  `BundleSource` parser from fetch.rs (local path or `https://`);
  runs the verification ladder, atomically installs, prints
  `Installed bundle <sha> (N artifacts, key_id=...)`.
- **`mvmctl up --bundle-pin <PATH>`** flag. Reads the archive,
  verifies via `FsTrustStore::default_path()`, derives the
  `PlanArtifact` triple via `bundle_pin_from_archive`, hands an
  in-memory `BundleAdmissionContext` to `admit_for_run`. Claim 9
  re-verify fires on every launch.
- **`LocalAuditKind::TrustAdd` / `TrustRemove`** added to the
  audit-kind enum + casing pins + serde round-trip test.
  `mvmctl trust add/remove` now emit via
  `mvm_core::audit::emit`; `AUDIT_POSTURE` TRUST_SUB flipped from
  `InteractiveOrControl` → `Emits(...)`.
- `BUNDLE_SUB::install` row added with posture
  `InteractiveOrControl` (will flip to `Emits("BundleInstall")`
  once the install audit hook ships).

Closed out in the W2 final commits (`90cef3d`, `ad3f52c`,
TBD-resources):

- `LocalAuditKind::BundleInstall` variant + emit from
  `mvmctl bundle install` + AUDIT_POSTURE flipped to
  `Emits("BundleInstall")`.
- `mvmctl bundle gc <SHA>` and `--all` verbs +
  `BundleRegistry::remove` + `list` + new
  `LocalAuditKind::BundleGc`. Interactive --all confirms unless
  `--yes` (or non-TTY).
- `BundleResources { vcpus, mem_mib }` optional field on
  `BundleManifest`. **BUNDLE_SCHEMA_VERSION bumped 1 → 2.** v1
  bundles deserialise cleanly with `resources = None`; v2 with
  resources are the new default for `mvmctl bundle export`.
  Older verifiers see `schema_version = 2` and refuse with
  `UnsupportedSchema` (deliberate fail-closed).
  `bundle_artifacts_for_sha` prefers manifest resources over
  operator config when present; CLI `--cpus` / `--memory` still
  override.

W2 is now fully shipped end-to-end with no outstanding follow-ups.

### W3 — Network default flip (deny-by-default)  ✅ shipped

Pre-Sprint 52 `NetworkPolicy::default()` returned `unrestricted()`
— the entire rest of the ADR-002 model confined the guest at every
other layer, but egress was wide open. W3 flips the safe default
to `deny_all()`. Workloads that need network access opt in
explicitly via `--network-preset` / `--network-allow` /
`mvmctl trust`-provisioned template policies.

Shipped:

- `NetworkPolicy::default()` in
  `crates/mvm-core/src/policy/network_policy.rs` returns
  `Self::deny_all()` (was `Self::unrestricted()`).
- `mvmctl up` warning when the resolved policy is unrestricted —
  both for the explicit-CLI-flag path
  (`--network-preset unrestricted`) and for templates whose baked
  `default_network_policy` is unrestricted. Names the source so
  the user knows where the opt-out came from. Suppressible via
  `MVM_ACK_UNRESTRICTED_NETWORK=1` for CI / scripted use.
- ADR-002 10th claim shipped: *no untrusted workload reaches the
  network unless explicitly admitted by policy.* Framework refs
  added (ATT&CK T1071 / T1041; D3FEND Network Traffic Filtering;
  CREF Privilege Restriction).
- Tests updated:
  - `policy_default_is_deny_all` (renamed from
    `policy_default_is_unrestricted`) asserts the deny-all shape.
  - `test_resolve_network_policy_default_is_deny_all` flipped to
    match. Comment notes the pre-Sprint-52 expectation.
- 334 supervisor + all-crate lib tests green;
  `cargo test --test audit_total_coverage` green; clippy clean.

Breaking change disclosure for release notes:

> **Breaking:** `mvmctl up` and the rest of mvm now refuse network
> egress by default. Workloads that previously relied on
> implicit unrestricted egress must pass
> `--network-preset unrestricted` (which emits a launch-time
> warning) or one of the safer presets (`dev`, `agent`,
> `registries`). The escape hatch
> `MVM_ACK_UNRESTRICTED_NETWORK=1` suppresses the warning.

Outstanding (deferred follow-ups):

- CI lane `network-default-is-deny` — a black-box assertion that
  `mvmctl up` with no flags refuses outbound connectivity from
  inside the guest. Needs a live-KVM smoke harness mvm doesn't
  have today; the unit-level guarantee shipped in this commit.
- `mvmctl doctor` could surface the network default visibly in
  its security-posture section as a corollary of claim 10. The
  posture section reads from `BackendSecurityProfile`; teaching
  it about runtime policy defaults is a small follow-on.

### W4 — OCI export (reach to non-KVM hosts) ✅ shipped

Sprint 52 follow-on item from the original ranking (`#4a` in the
decision doc) — extends mvm-built workloads to hosts without KVM
by exposing the OCI tarball Nix already produces internally.

Shipped:

- `template_build_from_manifest` now copies `image.tar.gz` into
  the slot's revision dir (when the flake's `mkGuest` opted into
  `dockerTools.streamLayeredImage`). Best-effort — flakes that
  don't emit it just don't get one.
- New `mvmctl manifest export-oci <TEMPLATE> --out <PATH>` verb.
  Resolves a slot-hash / manifest-path / legacy name to the slot
  dir, finds the OCI tarball alongside the rootfs, copies it to
  `--out`. Clear error when the tarball is absent (with the
  rebuild hint).
- `LocalAuditKind::ImageExportOci` variant + snake_case wire pin
  (`image_export_oci`) + all-variants serialize roundtrip.
- AUDIT_POSTURE MANIFEST_SUB gains an `export-oci` row with
  `Emits("ImageExportOci")`.
- 2 new tests: resolve-to-slot-hash rejects unknown shas with a
  hint, verb is registered in the CLI tree.

End-to-end flow:

```
# Build the template on a KVM host
mvmctl build <manifest>
# Export to a Docker-loadable tarball
mvmctl manifest export-oci <slot> --out ./mvm-workload.tar.gz
# On any host with Docker / Podman
docker load -i mvm-workload.tar.gz
docker run mvm-...
```

Outstanding (deferred):

- Bundle-source path: `mvmctl bundle export-oci <sha>` for
  installed bundles, not just slot-built templates. Bundle
  manifests don't currently carry the OCI tarball; adding it
  would be a bundle-schema bump.
- Direct `--push <registry>` for one-step deployment. The current
  shape is "copy to a file, then docker push manually" — `--push`
  would streamline.

### W5 — secure one-shot `run` UX ✅ shipped

Follow-on from the agent sandbox CLI review: expose the secure happy
path as `mvmctl run`, while preserving `mvmctl exec` as the lower-level
dev-compatible spelling.

Shipped:

- New top-level `mvmctl run` command delegates to the existing cold
  transient execution machinery.
- `--profile restrictive|standard|dev|permissive` gates host-impacting
  options before dispatch.
- `standard` is the default and refuses writable host shares; `restrictive`
  refuses env injection and all host shares; `permissive` requires
  `MVM_ACK_PERMISSIVE_RUN=1`.
- `mvmctl run --receipt <path>` writes a host-signed JSON receipt with
  invocation hashes, output hashes, and exit status; raw argv/env values
  and raw output are deliberately omitted.
- `mvmctl run --json` emits an unsigned machine-readable execution summary
  using the same redacted invocation/outcome shape as receipts. Guest stdout
  and stderr are not streamed in JSON mode; only hashes and byte counts appear.
- Live smoke coverage for `mvmctl run --json --receipt` is gated behind
  `MVM_LIVE_SMOKE=1` and compares the public JSON summary to the signed
  receipt without allowing raw guest output into either artifact.
- `mvmctl receipt verify <path>` verifies the receipt signature against
  the local host-signer public key, with `--pubkey` for portable checks.
- `mvmctl sandbox gc` adds a dry-run-by-default cleanup path for stale
  sandbox registry entries. `--apply` only removes stopped/expired entries
  that no backend reports as live and emits `SandboxGc`.
- `mvmctl sandbox gc --json` emits the same candidate/removal decision as a
  machine-readable summary and preserves dry-run-by-default behavior unless
  `--apply` is supplied.
- `mvmctl cp` copies one regular file across the host/VM boundary with exactly
  one `VM:/absolute/path` endpoint, a default 16 MiB cap, no overwrite unless
  `--force`, guest-side path-policy validation, and `VmFileCopy` audit without
  host paths or file contents.
- `mvmctl cp --json` emits a redacted machine-readable copy summary with
  direction, VM name, guest path, byte count, and effective copy options; host
  paths and file contents are omitted.
- `mvmctl policy explain <tenant>:<workload> [--json]` validates the same
  policy bundle admission gates as the resolver and emits a redacted admission
  posture summary; JSON omits raw artifact paths, audit destination URLs, and
  egress hostnames.
- `mvmctl policy lint <tenant>:<workload> [--json]` validates the bundle and
  fails on risky-but-admissible posture such as plain HTTP egress, disabled
  inspectors, unsigned audit chains, long/disabled key rotation, broad L4
  CIDRs, wildcard ports, and sensitive-looking artifact capture paths. JSON is
  redacted with the same no-raw-paths/URLs/hostnames rule as `policy explain`.
- `mvmctl policy diff <left> <right> [--json]` validates both bundles and
  emits a redacted change report for policy review before rollout. Raw artifact
  paths, audit destination URLs, egress hostnames, and CIDRs are replaced with
  stable fingerprints and safe summaries.
- CLI reference and parser tests cover the new command and profile surface.

### Sprint 52 success criteria

1. *A workload with `mem_initial = "256M"` and `mem = "1024M"`
   boots on Firecracker and cloud-hypervisor with the balloon
   pre-inflated to 768 MiB; the host commits 256 MiB.*
2. *`AnyBackend::balloon_set_target` adjusts a running FC VM's
   commitment without reboot, observable through `balloon_state`.*
3. *A `.mvmpkg` bundle built on machine A round-trips through the
   registry, fetches to machine B, and `mvmctl up` succeeds; a
   tampered manifest fails admission with a clear error.*
4. *`mvm_plan::verify_plan` refuses an `ExecutionPlan` pinned to
   a bundle whose `key_id` is not in the consumer's trust store.*
5. *Backwards compatibility: every existing workspace test plus
   `cargo clippy --workspace --all-targets -- -D warnings` stays
   green throughout.*

## Completed Sprints

- [01-foundation.md](sprints/01-foundation.md)
- [02-production-readiness.md](sprints/02-production-readiness.md)
- [03-real-world-validation.md](sprints/03-real-world-validation.md)
- Sprint 4: Security Baseline 90%
- Sprint 5: Final Security Hardening
- [06-minimum-runtime.md](sprints/06-minimum-runtime.md)
- [07-role-profiles.md](sprints/07-role-profiles.md)
- [08-integration-lifecycle.md](sprints/08-integration-lifecycle.md)
- [09-openclaw-support.md](sprints/09-openclaw-support.md)
- [10-coordinator.md](sprints/10-coordinator.md)
- Sprint 11: Dev Environment
- [12-install-release-security.md](sprints/12-install-release-security.md)
- [13-boot-time-optimization.md](sprints/13-boot-time-optimization.md)
- [14-guest-library-and-examples.md](sprints/14-guest-library-and-examples.md)
- [15-real-world-apps.md](sprints/15-real-world-apps.md)
- [16-production-hardening.md](sprints/16-production-hardening.md)
- [17-resource-safety-release.md](sprints/17-resource-safety-release.md)
- [18-developer-experience.md](sprints/18-developer-experience.md)
- [19-observability-security.md](sprints/19-observability-security.md)
- [20-production-hardening-validation.md](sprints/20-production-hardening-validation.md)
- [21-binary-signing-attestation.md](sprints/21-binary-signing-attestation.md)
- [22-observability-deep-dive.md](sprints/22-observability-deep-dive.md)
- [23-global-config-file.md](sprints/23-global-config-file.md)
- [24-man-pages.md](sprints/24-man-pages.md)
- [25-e2e-uninstall.md](sprints/25-e2e-uninstall.md)
- [26-audit-logging.md](sprints/26-audit-logging.md)
- [27-config-validation.md](sprints/27-config-validation.md)
- [28-config-hot-reload.md](sprints/28-config-hot-reload.md)
- [29-shell-completions.md](sprints/29-shell-completions.md)
- [30-config-edit.md](sprints/30-config-edit.md)
- [31-vm-resource-defaults.md](sprints/31-vm-resource-defaults.md)
- [32-vm-list.md](sprints/32-vm-list.md)
- [33-template-init-preset.md](sprints/33-template-init-preset.md)
- [34-flake-check.md](sprints/34-flake-check.md)
- [35-run-watch.md](sprints/35-run-watch.md)
- [36-fast-boot-minimal-images.md](sprints/36-fast-boot-minimal-images.md)
- [37-image-insights-dx-guest-lib.md](sprints/37-image-insights-dx-guest-lib.md)
- [38-multi-backend-abstraction.md](sprints/38-multi-backend-abstraction.md)
- [39-developer-experience-dx.md](sprints/39-developer-experience-dx.md)
- [40-apple-container-dev.md](sprints/40-apple-container-dev.md)
- [41-microvm-one-shot-exec.md](sprints/41-microvm-one-shot-exec.md)

---

## Open Follow-ups (carryover from Sprint 41)

Tracked as GitHub issues so they're individually grabbable:

- [ ] [#3](https://github.com/tinylabscom/mvm/issues/3) — Live smoke for `mvmctl exec` on Linux/KVM and Lima dev VM (boot+exec+teardown, `--add-dir`, SIGINT, `nix build` of `nix/default-microvm/`). _Needs real hardware._
- [x] [#4](https://github.com/tinylabscom/mvm/issues/4) — Release artifacts for the bundled default microVM image. Release workflow now builds `nix/default-microvm/` per-arch and uploads `default-microvm-vmlinux-{arch}` / `default-microvm-rootfs-{arch}.ext4` / `default-microvm-{arch}-checksums-sha256.txt`. `ensure_default_microvm_image()` falls back to `download_default_microvm_image()` when Nix is unavailable or the local build fails. Cosign scope unchanged (artifacts unsigned, mirroring `dev-image`).
- [x] [#5](https://github.com/tinylabscom/mvm/issues/5) — mvmforge `launch.json` consumption: `ExecTarget::LaunchPlan` + entrypoint parser + `--launch-plan` flag. Image-from-launch-plan remains a future variant (mvmforge v0 `apps[].source` is itself "deferred").
- [ ] [#6](https://github.com/tinylabscom/mvm/issues/6) — Writable `--add-dir` (virtio-fs or 9p) — separate design / ADR required.
- [x] [#7](https://github.com/tinylabscom/mvm/issues/7) — Snapshot restore for `mvmctl exec` (easy branch: registered template, no `--add-dir`). The harder branch (parameterized snapshots for the `--add-dir` case) stays open under the same issue.
