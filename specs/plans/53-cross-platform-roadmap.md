# Plan 53 — Cross-platform release roadmap (Option B — Pragmatic)

## Summary

mvm currently ships fully on Linux + KVM (Firecracker) and macOS 26+ Apple Silicon (Apple Container, see plan 23). Older macOS, Intel Macs, and Windows hosts are second-class today. This plan turns that into a coherent multi-platform release without forking the project's security narrative.

The decision recorded here is **Option B — Pragmatic**: keep Firecracker as the unambiguous security baseline, add **libkrun** as a fourth backend (Intel Mac + macOS-no-Lima + Linux native option with Firecracker-comparable TCB), keep Docker as a clearly-marked Tier 3 with a loud `MVM_ACK_DOCKER_TIER`-suppressible warning, and treat Windows as a real first-class consumer of the WSL2 path with bootstrap automation. Cloud Hypervisor and the native-Windows microVM path (CH+WHPX) are **explicitly rejected** for now because they would fork the security narrative; the rationale lives in Plan F below so it doesn't have to be re-derived.

The roadmap is sized at roughly three sprints of work (foundation → macOS parity + Windows foundation → libkrun + Windows installer). Each numbered Plan A–K has goal, files, phases, tests, risks, and effort. Plans F, G, H are deferred-with-rationale backlog placeholders.

## Per-platform reality after this roadmap

- **Linux + KVM** — Firecracker direct. Tier 1, full ADR-002 guarantees.
- **Linux without KVM** — Docker fallback (loud warning).
- **macOS 26+ Apple Silicon** — Apple Container or libkrun. Choose Apple Container for tightest VZ integration (plan 23); libkrun for a no-Lima single-binary path.
- **macOS <26 / Intel Mac** — libkrun (new). Today these users are stuck on Lima + Firecracker; libkrun gives them a real native path.
- **WSL2** — works as Linux; Firecracker if nested KVM available, Docker otherwise.
- **Native Windows** — WSL2 setup is the supported path with first-class bootstrap automation. Users without WSL2 see a clear "install WSL2" message; the Docker backend is reachable but not promoted.

## What "microVM" means in this codebase (after roadmap)

| Backend | Hardware iso? | vsock? | Snapshots? | Verified boot? |
|---|---|---|---|---|
| Firecracker | Yes (KVM) | Yes | Yes | Yes (W3) |
| Apple Container | Yes (VZ) | Yes | No | No |
| **libkrun (planned)** | Yes (KVM on Linux, Hypervisor.framework on macOS) | Yes | No | No |
| Docker | No (shared kernel) | No (unix socket) | No | No |
| microvm.nix (QEMU) | Yes (KVM) | Yes | No | Partial |

The seven ADR-002 security claims apply *fully* to Firecracker and *partially* (no W3 verified boot) to Apple Container and libkrun. They mostly **do not apply** to Docker. Plans A and B make this legible to users.

## Security posture decision

We considered three options:

- **Strict** — Firecracker + Apple Container only. No new backends. Windows = WSL2 docs only.
- **Pragmatic (chosen)** — Firecracker + Apple Container + libkrun. Docker stays as Tier 3 fallback with loud warnings, not promoted. Windows = WSL2-first with bootstrap automation.
- **Permissive** — Add Cloud Hypervisor for DinD/GPU/Windows-guests. Promote Docker to first-class Windows path with pre-built images and `mvmctl pull`. Long-term native-Windows microVMs via CH+WHPX.

**Why we rejected Permissive**: every advantage we'd ship by adding Cloud Hypervisor is a feature Firecracker deliberately excluded for attack-surface reasons (nested KVM, GPU passthrough, larger device model, Windows-guest paths). CH is ~106K LOC vs Firecracker's ~83K. Adding it forks the security narrative — "Firecracker is the secure default, but you can opt into a larger-TCB VMM" is a different project than "we use Firecracker because microVM isolation comes from minimalism." Same logic for promoting Docker to first-class on Windows: the `mvmctl pull` + pre-built-image story would pull Tier 3 into the user's default expectations. We avoid that fork.

**Why we rejected Strict**: it leaves Intel Mac users with no path other than Lima, and ignores a strictly-additive opportunity (libkrun has comparable TCB to Firecracker, runs on Hypervisor.framework on both Mac architectures, and is library-style so the binary is self-contained). Strict was the safer narrative choice but Pragmatic is materially better DX for the same posture.

**The fork test we apply going forward**: any new backend or feature must satisfy "do the seven claims still hold (or partially hold with named exceptions) without changing Firecracker's role as the security baseline?" Cloud Hypervisor fails this test today; libkrun passes (smaller TCB, no Firecracker-excluded features); Docker passes only as an explicit Tier 3.

## Lessons from prior art (condensed)

- **SlicerVM landed on the same architecture we're heading toward**: Firecracker on Linux + Apple VZ on macOS + WSL2-only Windows + APFS CoW for macOS templates. They added Cloud Hypervisor for GPU passthrough; we're not following them there.
- **The emirb 2026 microVM blog post** (https://emirb.github.io/blog/microvm-2026/) argues containers aren't a security boundary, with seven 2024–2025 CVEs as receipts. Plan B's loud Docker-tier warning takes this seriously.
- **Windows in 2026 is a guest, not a host platform** for microVM tooling. Plan I treats Windows as a first-class consumer of the WSL2 path rather than trying to build a Windows-native microVM stack.
- **rust-vmm is a foundation, not a backend.** Improving `vm-memory` benefits Firecracker, libkrun, et al. simultaneously. Argues for thin abstraction layers and for picking VMMs that share the foundation (libkrun does).

---

# Implementation plans

Plans are organized by tier. Each plan has: **goal · files · phases · tests · risks · effort**. Plans F, G, H are deferred backlog placeholders.

## Plan A — Matryoshka ADR rewrite

**Goal**: Restructure ADR-002 around a 5-layer trust model. Map the seven CI-enforced claims onto layers. Add a per-backend tier matrix that makes the Docker-tier security collapse visible at a glance.

**Files**:
- `specs/adrs/002-microvm-security-posture.md` — restructure (in-place update)
- `public/src/content/docs/security/matryoshka.md` (new) — user-facing version
- `specs/plans/25-microvm-hardening.md` — cross-references from W1–W6 to layer numbers

**Layer model**:

| Layer | Description | What's at this layer in mvm |
|---|---|---|
| L1 | Host + hypervisor | macOS + VZ, Linux + KVM, libkrun's KVM/HVF |
| L2 | VMM (userspace) | Firecracker (~83K LOC Rust, seccomp-jailed), Containerization, libkrun |
| L3 | Guest kernel | Custom Linux from Nix; dm-verity'd rootfs (Firecracker only today) |
| L4 | Guest agent | uid 901 under setpriv, no_new_privs |
| L5 | Workload | Per-service uid, bounding-set drop, seccomp tier `standard` |

**Claim → layer mapping** (using ADR-002's existing claim numbers):
- L1: enables 3 (verified boot precondition)
- L2: 1 (no host-fs leak), 2 (no uid-0 escape via VMM), 5 (vsock framing fuzzed — host parser)
- L3: 3 (verified boot, dm-verity)
- L4: 4 (no `do_exec` in prod), 5 (guest framing too)
- L5: 1 extends here via per-service uid (W2.1)
- Cross-cutting: 6 (image hash), 7 (cargo deps audit)

**Per-backend tier matrix**:

| Backend | L1 | L2 | L3 | L4 | L5 | Notes |
|---|---|---|---|---|---|---|
| Firecracker (Linux+KVM) | ✅ | ✅ | ✅ | ✅ | ✅ | Full ADR-002 |
| Apple Container | ✅ | ✅ | ⚠️ | ✅ | ✅ | No verified boot yet |
| libkrun | ✅ | ✅ | ⚠️ | ✅ | ✅ | No verified boot yet; comparable TCB to FC |
| Docker | ❌ | ❌ | ❌ | ✅ | ✅ | L1–L3 collapse to host kernel; **Tier 3 only** |
| microvm.nix (QEMU) | ✅ | ⚠️ | ⚠️ | ✅ | ✅ | QEMU TCB much larger; partial verified boot |

**Phases**:
1. Draft layer diagram and write claim → layer mapping table.
2. Build per-backend matrix.
3. Update plan 25 to reference layers alongside W1–W6.
4. Mirror to user-facing docs.
5. Cross-link from ADR-001 (multi-backend).

**Tests / verification**:
- Every claim 1–7 appears in the mapping.
- Every backend in `AnyBackend` appears in the matrix.

**Risk**: Authoring only.

**Effort**: 1–2 days.

---

## Plan B — Doctor security-claims-by-tier output

**Goal**: `mvmctl doctor` and `mvmctl run` surface the active backend's security profile. Loud, suppressible warning when Docker tier is auto-selected.

**Files**:
- `crates/mvm-core/src/protocol/vm_backend.rs` — add `BackendSecurityProfile`, `ClaimStatus`, `LayerCoverage`
- `crates/mvm-runtime/src/vm/{firecracker,apple_container,docker,microvm_nix}.rs` — implement `security_profile()` per backend
- `crates/mvm-runtime/src/vm/backend.rs` — `AnyBackend::security_profile()` dispatch
- `crates/mvm-cli/src/doctor.rs` — render new "Security posture (active backend)" section
- `crates/mvm-cli/src/commands/vm/up.rs` — startup banner when Docker tier selected
- `crates/mvm-core/src/user_config.rs` — add `[security] ack_docker_tier = bool`

**API**:
```rust
pub struct BackendSecurityProfile {
    pub claims: [ClaimStatus; 7],
    pub layer_coverage: LayerCoverage,
    pub notes: &'static [&'static str],
}

pub enum ClaimStatus { Holds, DoesNotApply, DoesNotHold }

pub struct LayerCoverage { l1_host_hypervisor: bool, l2_vmm: bool, l3_guest_kernel: bool, l4_guest_agent: bool, l5_workload: bool }
```

**Per-backend declarations**:
- Firecracker: all `Holds`, all layers true.
- AppleContainer / libkrun: claim 3 = `DoesNotHold` (no verified boot yet), all others `Holds`, all layers true.
- Docker: claims 1/2/3 = `DoesNotHold`, claim 5 = `DoesNotApply` (unix socket), 4/6/7 = `Holds`. L1/L2/L3 = false.
- microvm.nix: claim 3 = `DoesNotHold` until verified boot lands for QEMU path.

**Doctor output sketch (Docker case)**:
```
Active backend: docker (auto-selected; KVM unavailable)

⚠️  SECURITY POSTURE: Docker tier — reduced isolation
   Container isolation, not hardware-isolated microVMs.
   Layer coverage: L1 ✗  L2 ✗  L3 ✗  L4 ✓  L5 ✓
   Claims that DO NOT hold: 1, 2, 3
   Recent container-escape CVEs (2024–2025):
     CVE-2024-21626, CVE-2024-1753, CVE-2025-9074,
     CVE-2025-23266, CVE-2025-31133, CVE-2025-52565
   Acknowledge with MVM_ACK_DOCKER_TIER=1 to silence this banner.
```

**Phases**:
1. Add types in `mvm-core`.
2. Implement `security_profile()` for each backend.
3. Wire dispatch through `AnyBackend`.
4. Doctor renderer + JSON output extension.
5. `mvmctl run` startup banner with `MVM_ACK_DOCKER_TIER` ack.
6. Tests.

**Tests**:
- `vm_backend::tests::profile_serializes_correctly`
- `firecracker::tests::profile_holds_all_claims`
- `docker::tests::profile_drops_l1_l3_claims`
- Integration: `mvmctl doctor` text + `--json` output.
- CLI: `mvmctl run --hypervisor docker` (mocked) emits banner; `MVM_ACK_DOCKER_TIER=1` suppresses it.

**Risk**: Subjective claim-status calls. Mitigation: rationale in `notes` field per backend.

**Effort**: 1 day. **Sequence**: after Plan A.

---

## Plan C — PVM FAQ entry

**Goal**: Users hitting "no `/dev/kvm`" know their options without re-discovering the landscape.

**Files**:
- `public/src/content/docs/guides/troubleshooting.md` — new section "No /dev/kvm available."
- `crates/mvm-cli/src/doctor.rs` — link from KVM-fail message.

**Content** (~200 words):
> **No `/dev/kvm` on your cloud VM?** Three options.
> 1. **Switch to a nested-virt instance.** AWS C8i/M8i/R8i (Feb 2026), GCE n2 nested, Azure Dasv5/Easv5 — these expose `/dev/kvm` and Firecracker runs natively.
> 2. **Use Tier 3 Docker fallback.** Works in any environment with Docker. **Reduced security tier** — see Matryoshka model. Use only for non-security-sensitive workloads.
> 3. **PVM (advanced, external).** [SlicerVM's PVM mode](https://docs.slicervm.com/tasks/pvm/) runs real microVMs without `/dev/kvm` via a patched Firecracker + kernel module. mvm doesn't ship this.

**Effort**: 1 hour.

---

## Plan D — APFS CoW for Apple Container templates

**Goal**: `mvmctl run --template foo` on macOS Apple Container starts in <1s by `clonefile(2)`-ing the template's `rootfs.ext4` instead of copying. Side benefit: opportunistic Linux reflinks on btrfs/xfs/overlayfs.

**Files**:
- `crates/mvm-runtime/src/vm/cow.rs` (new) — `reflink_or_err`, `reflink_or_copy`
- `crates/mvm-runtime/src/vm/template/lifecycle.rs` — add `template_clone_for_instance(template_id, instance_dir) -> Result<CloneStrategy>`
- `crates/mvm-runtime/src/vm/apple_container.rs` — call clone in `start()` before pointing VZ at the disk
- `crates/mvm-runtime/src/vm/firecracker.rs` — opportunistic clone on Linux
- `crates/mvm-runtime/src/vm/mod.rs` — export `cow` module
- `crates/mvm-runtime/Cargo.toml` — `libc.workspace = true` already present; no new deps

**API**:
```rust
pub enum CloneStrategy { Reflink, Copied }
pub fn reflink_or_err(src: &Path, dst: &Path) -> io::Result<()>;
pub fn reflink_or_copy(src: &Path, dst: &Path) -> io::Result<CloneStrategy>;
```

**Implementation notes**:
- macOS: `libc::clonefile(src.as_ptr(), dst.as_ptr(), 0)` under `#[cfg(target_os = "macos")]`.
- Linux: `ioctl(dst_fd, FICLONE, src_fd)` under `#[cfg(target_os = "linux")]`. Returns `EOPNOTSUPP` on non-reflink filesystems.
- Other OSes: unconditional `Err(EOPNOTSUPP)`.

**Boot path on Apple Container**:
1. `start()` resolves template's `rootfs.ext4` path.
2. `reflink_or_copy(template/rootfs.ext4, instance/rootfs.ext4)`.
3. Pass `instance/rootfs.ext4` to `VZDiskImageStorageDeviceAttachment(writable: true)`.
4. On instance stop, delete `instance/rootfs.ext4`. Template stays pristine.

**Phases**:
1. **Spike**: standalone POC clonefile-ing a 1GB ext4 + booting it in VZ. Confirm VZ tolerance, single-volume requirement, ext4 UUID handling.
2. Add `cow.rs` with primitives + tests.
3. Add `template_clone_for_instance()` in `lifecycle.rs`.
4. Wire into Apple Container backend.
5. Wire into Firecracker backend (opportunistic).
6. Tests + integration.

**Tests**:
- Unit: `cow::tests::clonefile_macos_creates_o1_clone` (cfg-gated; assert <100ms for 1GB).
- Unit: `cow::tests::ficlone_linux_with_supporting_fs` (skipped if FS doesn't support).
- Integration: build template, run, assert wall-clock <1s, assert template `rootfs.ext4` mtime unchanged after instance writes.
- Stress: 5 concurrent `mvmctl run --template foo` → all start, disk usage barely grows.

**Risks**:
1. **VZ tolerance** — confirm in spike.
2. **Single-volume requirement** — APFS CoW only within the same volume. Document `~/.mvm` placement.
3. **ext4 UUID collisions** — multiple instances with same UUID may confuse mount/systemd. Mitigation: `tune2fs -U random` after clone, or audit boot path.
4. **Linux reflink coverage** — ext4 needs explicit support; btrfs/xfs work. Lima's ext4 limits Linux benefit.

**Effort**: 2–4 days spike + integration; +2 days for Linux opportunistic path; +1 day for tests/docs.

---

## Plan E — libkrun backend

**Goal**: Add libkrun as a 4th backend in `AnyBackend`. Adds Intel Mac support (Apple VZ doesn't), removes Lima from the macOS runtime hot path entirely (libkrun is library-style), and provides a Linux native option with TCB comparable to Firecracker.

**Why libkrun specifically**: Only VMM in our consideration set that runs on Linux (KVM), macOS Apple Silicon (Hypervisor.framework), **and macOS Intel** (Hypervisor.framework). Library-style — links into our binary — so no separate VMM process. Used in production by Microsandbox. Apache-2.0. **Passes our fork test**: comparable TCB to Firecracker, no Firecracker-excluded features.

**Files**:
- `crates/mvm-libkrun/` (new crate)
  - `Cargo.toml` — bindgen build dep
  - `build.rs` — generate Rust bindings from `libkrun.h`
  - `src/lib.rs` — safe wrapper over the unsafe bindings
  - `src/{macos,linux}.rs` — platform-specific glue
- `crates/mvm-runtime/src/vm/libkrun.rs` (new) — `LibkrunBackend` impl
- `crates/mvm-runtime/src/vm/backend.rs` — add `Libkrun(LibkrunBackend)` variant + `auto_select` rules
- `crates/mvm-cli/src/commands/vm/up.rs` — `--hypervisor libkrun` value
- `crates/mvm-core/src/platform/platform.rs` — `has_libkrun()` detection
- `nix/` — Nix derivation for libkrun
- `crates/mvm-cli/src/bootstrap.rs` — install libkrun (Homebrew on macOS, distro pkg/binary on Linux)
- `crates/mvm-cli/src/doctor.rs` — libkrun availability check

**API surface** (safe wrapper):
```rust
pub struct KrunContext { /* opaque handle */ }

impl KrunContext {
    pub fn new(name: &str) -> Result<Self>;
    pub fn set_vm_config(&mut self, vcpus: u8, ram_mib: u32) -> Result<()>;
    pub fn set_root_disk(&mut self, path: &Path) -> Result<()>;
    pub fn set_kernel(&mut self, path: &Path, cmdline: &str) -> Result<()>;
    pub fn add_vsock_port(&mut self, port: u32, path: &Path) -> Result<()>;
    pub fn start(self) -> Result<KrunHandle>;
}
```

**Phases**:
1. **Spike** — boot a minimal Nix-built ext4 rootfs in libkrun on macOS Apple Silicon. Confirm vsock + console + Nix kernel compatibility.
2. **`mvm-libkrun` crate** — bindgen + safe wrapper. Single platform first.
3. **Cross-platform expansion** — Linux + macOS Intel boot paths.
4. **`LibkrunBackend`** — implements `VmBackend` with `VmCapabilities { snapshots: false, vsock: true, pause_resume: false, tap_networking: false }` and `BackendSecurityProfile` mirroring Apple Container (claim 3 = DoesNotHold).
5. **AnyBackend dispatch** — add variant, update `auto_select`: KVM-Linux → AppleContainer → libkrun → Docker → Firecracker-via-Lima.
6. **Bootstrap install + doctor check.**
7. **Tests** — bindings safety, lifecycle, vsock, parity with Firecracker for golden-path workloads.
8. **Docs** — when to choose libkrun vs Apple Container vs Firecracker; Intel-Mac story; macOS-no-Lima story.

**Decision points the spike must resolve**:
- Does our Nix-built kernel boot in libkrun? (Should: similar minimal config to Firecracker.)
- Does the guest agent run unchanged? (vsock should be transparent.)
- Codesigning: does libkrun need `com.apple.security.hypervisor` entitlement that mvmctl doesn't have today?

**Tests**:
- `mvm_libkrun::tests::context_lifecycle`
- `LibkrunBackend::tests::profile_holds_most_claims`
- Integration: `mvmctl run --hypervisor libkrun --flake examples/minimal` boots, vsock health check responds.
- CI lanes: macOS-arm64, macOS-x86_64, Linux-x86_64.

**Risks**:
1. **C library binding complexity** — bindgen-generated unsafe Rust. Mitigation: small surface.
2. **Hypervisor.framework lower-level than VZ** — Apple may break ABI between macOS releases. Mitigation: pin libkrun version; CI on multiple macOS versions.
3. **Codesigning entitlement** — investigate during spike; if blocking, document as macOS-Apple-Silicon-only initially.
4. **Build path still needs Linux** — libkrun solves runtime only; Lima stays for Nix builds on macOS. Tracked as future work.

**Effort**: 1–1.5 sprints. Spike (3 days) + crate + backend (1 week) + bootstrap/doctor/docs (3 days).

---

## Plan F — Cloud Hypervisor backend (rejected — backlog placeholder)

**Status**: Considered and **rejected** for security-posture reasons. The full rationale is captured here so it doesn't have to be re-derived. A separate placeholder file at `specs/plans/54-cloud-hypervisor-deferred.md` should mirror this section once Sprint 1 lands.

**Why rejected**: Cloud Hypervisor's value proposition — nested KVM inside guests, GPU passthrough, larger device model, Windows-guest support — is exactly the set of features Firecracker excluded for attack-surface reasons. Adding CH would fork our security narrative ("Firecracker for security, CH if you need the extra features"). We'd rather keep Firecracker as the unambiguous secure baseline.

**Trigger conditions to revisit**:
- A user demonstrates a need that *cannot* be satisfied by Firecracker, libkrun, or Apple Container, AND
- We're prepared to update ADR-002 to acknowledge a forked security model with explicit "use CH only when X" guidance.

Both conditions must hold. The first alone is not enough.

**Effort if pulled**: ~1 sprint to implement, plus ADR-002 update.

---

## Plan G — crosvm backend (deferred backlog placeholder)

**Status**: Considered and deferred. Crosvm is Google's KVM-based VMM, primarily targeting Chrome OS and the Android emulator. Mature but niche for our user base. libkrun (Plan E) covers the "embeddable cross-platform" niche we care about.

A separate placeholder file at `specs/plans/55-crosvm-deferred.md` should mirror this once Sprint 1 lands.

**Trigger conditions**: Real user demand for Chrome OS host or Android-workload guest support.

**Effort if pulled**: ~1 sprint.

---

## Plan H — rust-vmm internalization (deferred backlog placeholder)

**Status**: Considered and rejected for now. Composing rust-vmm crates into a working VMM is *building a VMM* — that's Firecracker's and libkrun's job, not ours. The shell-out approach has costs (process boundary, requires `firecracker` binary on PATH) but the boundary is also a feature: a Firecracker lifecycle bug doesn't crash mvmctl.

A separate placeholder file at `specs/plans/56-rust-vmm-internalization-deferred.md` should mirror this once Sprint 1 lands.

**Trigger conditions**: We need a custom VMM with an mvm-specific isolation property no upstream offers. (Plan E — libkrun — addresses the "single binary, no external dependency" pull factor.)

---

## Plan I — Windows community support (WSL2-first)

**Goal**: Windows users have a real, supported, well-documented path: WSL2 with mvm-aware bootstrap automation, native installer, dedicated docs, and a working CI lane. Docker remains reachable as Tier 3 but is *not* promoted to a first-class Windows path — users without WSL2 see "install WSL2" as the recommended answer.

**What we're explicitly NOT doing** (and why):
- **Pre-built rootfs distribution from CI for Docker tier consumption** — would elevate Docker (Tier 3, no microVM isolation) to a first-class Windows experience and pull container-tier workloads into the user's default expectations. Conflicts with our security posture.
- **Cloud Hypervisor + WHPX exploration for native-Windows microVMs** — depends on Plan F which we rejected.

**Files**:
- `crates/mvm-cli/src/bootstrap.rs` — Windows-aware bootstrap detects WSL2 availability, offers to install it, installs mvm in the distro
- `.github/workflows/windows.yml` (new) — Windows CI lane
- `installer/winget/` (new) — winget package manifest
- `public/src/content/docs/install/windows.md` (new) — top-level Windows install guide (WSL2-first)
- `public/src/content/docs/guides/windows-wsl2.md` (new) — WSL2 walkthrough
- `public/src/content/docs/guides/windows-troubleshooting.md` (new)

**Phases** (each independently shippable):

**Phase I.1 — Windows CI lane**
- Goal: `cargo build --workspace` and `cargo test -p mvm-core` green on Windows.
- Implementation: GitHub Actions Windows runner, audit `cfg(target_os = "windows")` across the codebase, fix unconditional unix-isms. Workspace gates `mvm-apple-container` via `[target.'cfg(target_os = "macos")'.dependencies]` (plan 20 specifies this; verify).
- Tests: green CI lane.
- Effort: 2–3 days.

**Phase I.2 — Windows install docs (WSL2-first)**
- Goal: One clear primary path (WSL2) with a small "alternative tiers" section.
- Content:
  - **Quickstart**: install WSL2 + Ubuntu, install mvm in the distro. Full Tier 1 isolation if WSL2 has nested KVM.
  - **Alternative**: Docker tier (with explicit "no microVM isolation" caveat).
  - **Troubleshooting**: nested-virt detection, BIOS settings, common errors.
- Effort: 1 day.

**Phase I.3 — winget manifest**
- Goal: `winget install mvm` works.
- Implementation: winget manifest pointing to GitHub release artifact (`mvmctl.exe` from Phase I.1). Initial version is unsigned standalone exe; signed MSI is a future enhancement.
- Effort: half day.

**Phase I.4 — WSL2 bootstrap automation**
- Goal: `mvmctl bootstrap` on Windows host detects whether WSL2 is configured, offers to install Ubuntu + mvm in the distro, sets up shared `~/.mvm`.
- Implementation: Windows-only branch in `bootstrap.rs` shells out to `wsl --install`, then `wsl -d Ubuntu` to install mvm inside.
- Tests: manual Windows test (CI is hard for WSL2 setup).
- Effort: 2 days.

**Risks**:
1. **CI cost** — Windows runners are slower. Mitigation: PR-only on Windows-relevant files; full matrix on main.
2. **WSL2 setup variance** — distros and Windows versions vary. Mitigation: support Ubuntu 24.04 LTS only initially.
3. **MSI signing cost** — proper Windows installer needs a code-signing cert (~$300/yr). Mitigation: ship unsigned standalone exe via winget initially.

**Effort**: 5–6 days total across all four phases.

---

## Plan J — AWS deployment guide

**Goal**: Clear instructions for running mvm on AWS EC2 with nested virt (mostly C8i/M8i/R8i families, Feb 2026+).

**Files**:
- `public/src/content/docs/deploy/aws.md` (new)

**Content** (~500 words): instance types with nested KVM (e.g. `c8i.4xlarge` ≈ $0.86/hr Frankfurt), bootstrap commands for Ubuntu 24.04 / AL2023, IAM (probably none), EBS sizing, security groups (none needed for vsock loopback), tip about AWS Bedrock AgentCore using Firecracker too.

**Effort**: half day.

---

## Plan K — Ubicloud deployment guide

**Goal**: Show how to run mvm on Ubicloud bare-metal.

**Files**:
- `public/src/content/docs/deploy/ubicloud.md` (new)

**Content** (~400 words): What Ubicloud is (open-source cloud, AGPL — note license implications), provisioning, installing mvm, trade-offs vs AWS.

**Effort**: half day.

---

# Sequencing recommendation

Roughly **3 sprints of work**, sequenced by risk × payoff:

### Sprint 1 — Foundation (~5 days)
**Theme: narrative + UX, zero arch risk, immediate marketability.**
- Plan A — Matryoshka ADR rewrite (1–2 days)
- Plan B — Doctor security-claims-by-tier (1 day)
- Plan C — PVM FAQ entry (1 hour)
- Plan J — AWS deployment guide (half day)
- Plan K — Ubicloud deployment guide (half day)
- Plan F — CH rejection placeholder file (1 hour)
- Plan G — crosvm deferred placeholder file (1 hour)
- Plan H — rust-vmm deferred placeholder file (1 hour)

**Outcome**: cohesive security narrative, honest UX for Tier 3 Docker, deployment guides, full backlog rationale paper trail.

### Sprint 2 — macOS parity + Windows foundation (~1 sprint)
**Theme: close the macOS UX gap; lay the Windows community foundation.**
- Plan D — APFS CoW for Apple Container templates (~5 days)
- Plan I.1 — Windows CI lane (parallel, ~3 days)
- Plan I.2 — Windows install docs (parallel, 1 day)

**Outcome**: macOS templates feel as fast as Linux; Windows CI green; Windows users have real install docs.

### Sprint 3 — libkrun + Windows installer (~1.5 sprints)
**Theme: Intel Mac support + macOS-no-Lima + native Windows install path.**
- Plan E — libkrun backend (full sprint + buffer)
- Plan I.3 — winget manifest (parallel, half day)
- Plan I.4 — WSL2 bootstrap automation (parallel, 2 days)

**Outcome**: Intel Mac users have a real path; Windows users `winget install mvm` and get guided WSL2 setup.

### Backlog (no scheduled work; rationale documented)
- Plan F — Cloud Hypervisor (rejected for posture reasons)
- Plan G — crosvm (deferred; trigger: Chrome OS / Android demand)
- Plan H — rust-vmm internalization (deferred; trigger: custom-VMM-required feature)

---

# Critical files referenced

| File | Plans that touch it |
|---|---|
| `crates/mvm-core/src/protocol/vm_backend.rs` | B (security profile) |
| `crates/mvm-core/src/platform/platform.rs` | E (has_libkrun), I (Windows detection) |
| `crates/mvm-runtime/src/vm/backend.rs` | E (new variant), B (profile dispatch) |
| `crates/mvm-runtime/src/vm/{firecracker,apple_container,docker,microvm_nix}.rs` | B (security profiles), D (CoW for FC + AC) |
| `crates/mvm-runtime/src/vm/cow.rs` (new) | D |
| `crates/mvm-runtime/src/vm/template/lifecycle.rs` | D |
| `crates/mvm-runtime/src/vm/libkrun.rs` (new) | E |
| `crates/mvm-libkrun/` (new crate) | E |
| `crates/mvm-cli/src/commands/vm/up.rs` | B (banner), E (CLI flag) |
| `crates/mvm-cli/src/doctor.rs` | B (security section), C (KVM-fail link), E (libkrun check) |
| `crates/mvm-cli/src/bootstrap.rs` | E (libkrun install), I.4 (WSL2 automation) |
| `specs/adrs/002-microvm-security-posture.md` | A |
| `specs/plans/{25,54,55,56}-*.md` | A (cross-refs); F, G, H placeholders |
| `public/src/content/docs/security/matryoshka.md` (new) | A |
| `public/src/content/docs/install/windows.md` (new) | I.2 |
| `public/src/content/docs/guides/windows-{wsl2,troubleshooting}.md` (new) | I.2 |
| `public/src/content/docs/deploy/{aws,ubicloud}.md` (new) | J, K |
| `public/src/content/docs/guides/troubleshooting.md` | C |
| `.github/workflows/windows.yml` (new) | I.1 |
| `installer/winget/` (new) | I.3 |

# End-to-end verification (post-Sprint-3)

- `cargo test --workspace` green on Linux + macOS-arm64 + macOS-x86_64 + Windows.
- `cargo clippy --workspace -- -D warnings` zero warnings on all four.
- `mvmctl doctor` on each platform reports correct active backend, security profile, and (Docker tier only) the warning banner.
- `mvmctl run --hypervisor firecracker` on Linux + KVM: works, full ADR-002 claims hold.
- `mvmctl run --hypervisor apple-container` on macOS 26+ Apple Silicon: works, ~700ms cold start.
- `mvmctl run --hypervisor libkrun` on Linux + KVM: works.
- `mvmctl run --hypervisor libkrun` on macOS Apple Silicon: works without Lima.
- `mvmctl run --hypervisor libkrun` on macOS Intel: works without Lima.
- `mvmctl run --template foo --hypervisor apple-container`: cold start <1s via APFS CoW.
- Windows + WSL2: `winget install mvm` → `mvmctl bootstrap` configures WSL2 distro with mvm + Firecracker; full Tier 1 isolation when nested KVM available.
- ADR-002 displays the layer model + per-backend tier matrix; user-facing security doc mirrors it.

## References

- ADR-001 (multi-backend execution): `public/src/content/docs/contributing/adr/001-multi-backend.md`
- ADR-002 (microVM security posture): `specs/adrs/002-microvm-security-posture.md`
- Plan 20 (multi-backend abstraction): `specs/plans/20-multi-backend-abstraction.md`
- Plan 23 (Apple Container dev): `specs/plans/23-apple-container-dev.md`
- Plan 25 (microVM hardening): `specs/plans/25-microvm-hardening.md`
- SlicerVM PVM mode: https://docs.slicervm.com/tasks/pvm/
- "Your Container Is Not a Sandbox" (emirb 2026): https://emirb.github.io/blog/microvm-2026/
