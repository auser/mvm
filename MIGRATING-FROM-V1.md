# Migrating from mvm v1 to v2

> v2 (`0.14.0`+) is a complete rewrite. It ships at the same project
> name (`tinylabscom/mvm`), the same binary name (`mvmctl`), and the
> same canonical install path, but is **not API-compatible** with v1.
> This document is the upgrade guide and the feature-parity ledger.

If you need v1 — its final tip is preserved on the same repository:

```bash
git clone https://github.com/tinylabscom/mvm
cd mvm
git checkout legacy/v1     # final v1 development branch
# or
git checkout v1-final      # immutable tag at the same commit
```

Every v1 commit URL, PR URL, and release tag URL
(`v0.7.1`–`v0.13.0`) continues to resolve under the same hostname.

## Quick decision tree

| You used v1 for… | Do this in v2 |
|---|---|
| `mvmctl up <flake>` + `mvmctl console <vm>` for a debug shell | `mvmctl up --dev <flake>` then `mvmctl console <vm>` |
| `mvmctl up <flake>` in production / CI | unchanged on the surface; image is now sealed by default — `console` will refuse without `--force` |
| `mvmctl dev` on macOS via Lima | `mvmctl dev` now uses Apple Container (macOS 26+ AS) or libkrun; **Lima is gone** |
| Hand-written `flake.nix` with custom rootfs init | Migrate to `mkGuest` (see `nix/lib/default.nix` and `nix/images/examples/`) |
| `mvmctl template create/build/info` | Image building lives at `mvmctl build`; template manifest support lives at `mvmctl up --launch-plan` |
| `mvmctl exec` for one-shot guest exec | Use `mvmctl invoke` for production; `mvmctl exec` stays dev-only |
| Integration with mvmd (sibling repo) | mvmd's `cargo build --workspace` is currently blocked on an upstream dep conflict (`libkrun 0.4.5 ⊥ iroh-base 0.96.1`); targeted package builds work; full build greens when upstream resolves |

## Behavior changes that bite the most

### `mvmctl up` produces sealed images by default

In v1, `mvmctl up <flake>` produced an image where you could open a
console and get a shell — useful for debugging, but it meant
production images shipped with `do_exec` available, undermining
security claim 4.

In v2:

```bash
mvmctl up <flake>            # Prod posture: sealed image, console refuses
mvmctl up --dev <flake>      # Dev posture: accessible image, console works
mvmctl console <vm>          # Refuses on a sealed VM with a clear error
mvmctl console <vm> --force  # Overrides the refusal if you're sure
```

The Prod default is the new normal. If you have automation that runs
`mvmctl up` and expects to attach a console, either pass `--dev` for
the dev posture or use `--force` on `mvmctl console` for one-off
debugging.

CI symbol gate (`prod-agent-no-exec`) still asserts the production
agent has no `do_exec` symbol. Now the gate matches what's enforced
at runtime, not just at build time.

### Lima is gone

v1 booted a Lima VM on macOS as the development sandbox. v2 doesn't.
The replacements:

| Host | v2 behavior |
|---|---|
| Linux + `/dev/kvm` | `mvmctl dev` runs directly on the host shell |
| macOS 26+ on Apple Silicon | `mvmctl dev` uses Apple Container |
| macOS 26+ Intel / older macOS / KVM-less Linux | `mvmctl dev` bails with a libkrun-builder pointer; `mvmctl up <flake>` falls through to `LibkrunBuilderVm` for the build half |
| Windows | first-class support pending (plan 53's WSL2 path) |

If you had a Lima `mvm` VM provisioned by v1, you can delete it with
`limactl delete mvm`. v2 will not interact with it.

### Image build substrate moved to `microvm.nix`

v1's hand-rolled rootfs init is gone. `mkGuest` (in
`nix/lib/default.nix`) is the new authoring surface. Three forms:

```nix
# Form 1 — shell entrypoint (drops you in a shell on console attach)
mkGuest {
  inherit pkgs;
  entrypoint.shell = "bash";
}

# Form 2 — command entrypoint (one-shot)
mkGuest {
  inherit pkgs;
  entrypoint.command = [ "/bin/myapp" "--serve" ];
}

# Form 3 — services entrypoint (multiple supervised services)
mkGuest {
  inherit pkgs;
  entrypoint.services = {
    myapp = { exec = "/bin/myapp --serve"; };
    sidecar = { exec = "/bin/sidecar"; };
  };
}
```

The `accessible` flag is auto-inferred from the form (`shell` →
accessible by default; `command`/`services` → sealed by default).
Override with explicit `dev = true` if needed. The flag flows through
to runtime via the `passthru.mvm` sidecar (a `mvm-meta.json` file
written next to `rootfs.ext4`).

See `nix/images/examples/` for working flakes. Migrating a v1 custom
flake is mostly: replace the init invocation with `mkGuest`, move
service definitions into `entrypoint.services`, and delete any
explicit rootfs-mounting code (the substrate handles it).

### CLI surface deltas

| v1 command | v2 command | Notes |
|---|---|---|
| `mvmctl template create` | `mvmctl up` consumes flakes directly | Templates as a separate concept retired in plan 38 |
| `mvmctl template build` | `mvmctl build` | Manifest-driven; carries `BuildMode` |
| `mvmctl template list/info` | `mvmctl manifest ls/info` | Per-manifest, not per-template |
| `mvmctl exec` | `mvmctl invoke` for prod; `mvmctl exec` dev-only | Plan 41 / Sprint 45 W3 |
| `mvmctl share *` | `mvmctl volume *` | Plan 45 — when Phase 2 lands |
| `mvmctl dev shell` on macOS via Lima | `mvmctl dev` (auto-detects backend) | See "Lima is gone" above |

The full new CLI surface is documented at `public/src/content/docs/`
(the docs site); `mvmctl --help` is authoritative.

## Feature parity status (per v1 surface)

This is the honest delta. "Shipped in v2" means the feature is in
`origin/main` today; "Deferred" means it's named in plan 60 with a
phase; "Lives in mvmforge / sibling repo" means it moved out of mvm.

| Feature | v1 location | v2 status |
|---|---|---|
| Firecracker backend | `mvm-runtime/src/vm/firecracker.rs` | **Shipped** (`mvm-backend/src/firecracker.rs`) |
| Apple Container backend | `mvm-apple-container` crate | **Shipped** (collapsed into `mvm-providers`) |
| libkrun backend | `mvm-libkrun` crate | **Shipped** (collapsed into `mvm-providers`) |
| Lima dev VM | `vm/lima.rs` | **Removed** (replaced by Apple Container / direct host / libkrun-builder) |
| `mvmctl exec` | `commands/exec.rs` | **Shipped** dev-only + `mvmctl invoke` for prod (plan 41) |
| dm-verity verified boot (claim 3) | `nix/lib/minimal-init/` | **Shipped** (`nix/packages/mvm-verity-init.nix`) |
| HMAC-signed snapshots | n/a (new in v2) | **Shipped** (`mvm-security/src/snapshot_hmac.rs`) |
| Function-service factories | n/a | **Shipped** at `nix/lib/factories/` (plans 48/49) |
| Session lifecycle (`mvmctl invoke` warm-VM) | partial in v1 | **Partial in v2** — substrate at `mvm-mcp::session`; pool management deferred |
| L4 egress allowlist (`NetworkPreset::Agent`) | `mvm-core::network::NetworkPreset` | **Carryover candidate** from `legacy/v1` PR #20 — needs port |
| L7 egress runtime (mitmdump) | foundation only (plan 34) | **Deferred** to plan 60 Phase 2/3 |
| AES-256-GCM snapshot encryption | dead code in v1 (orphaned files) | **Shipped primitives** (`mvm-security/src/snapshot_crypto.rs`); file-bound wrappers deferred to Phase 2 |
| `KeyProvider` trait | dead code in v1 | **Shipped** (`mvm-security/src/keystore.rs`) — `EnvKeyProvider` only; `FileKeyProvider` deferred |
| Volume primitive + virtiofs mount | in-flight on `legacy/v1` `feat/sprint-46-filesystem-volumes` | **Deferred** — Sprint 49 work; v2 will absorb via convergence rule |
| Audit signing + per-tenant streams | plan 37 Wave 3 on `legacy/v1` | **Deferred** to plan 60 Phase 3 |
| PII redaction in L7 proxy | plan 37 Wave 2.5 on `legacy/v1` | **Deferred** to plan 60 Phase 2 |
| Tool-call mediation (ToolGate) | plan 37 Wave 2.7 on `legacy/v1` | **Deferred** to plan 60 Phase 2 |
| Attestation (TPM2 / SEV-SNP / TDX) | plan 37 Wave 3 on `legacy/v1` | **Deferred** to plan 60 Phase 3 |
| Reproducibility double-build CI | `.github/workflows/ci.yml` | **Shipped** |
| `cargo deny` + `cargo audit` CI | shipped in v1 | **Shipped** |
| `prod-agent-no-exec` symbol gate | shipped in v1 | **Shipped + extended** to assert `handle_run_entrypoint` symbol is *present* (combined contract gate) |
| Mesh DNS / vsock bridge | in-flight on `legacy/v1` ADR-0018/0020 | **Deferred** to plan 60 Phase 3 |

## Recovering individual v1 features

If you need a specific v1 feature now and it's listed as Deferred,
two options:

1. **Use v1 for that workload.** `git checkout legacy/v1` builds the
   v1 stack against the same dep graph it always had. No automation
   support, but the binary works.
2. **Port the specific branch.** Each Deferred row above points at
   the v1 branch carrying the work. Cherry-picking onto v2 main is
   mechanical but requires hand-resolving the dep / shape changes
   (different crate layout, `microvm.nix` substrate, etc.). File a
   GitHub issue if you need this to be prioritized — the v2 sprint
   plan slots the deferrals into Phase 2/3 of plan 60.

## Things that didn't change

The following are identical (or stronger) in v2 — listed so you
don't waste time looking for breakage where there is none:

- Project name (`mvm`), binary name (`mvmctl`), repo URL
  (`tinylabscom/mvm`)
- All 7 CI-enforced security claims (CLAUDE.md "Security model" is
  the canonical statement)
- Public XDG paths (`~/.mvm/`, `~/.cache/mvm/`) and their 0700 perms
- Vsock framing protocol (still `mvm_guest::vsock`, still
  `deny_unknown_fields` everywhere)
- `cargo deny` / `cargo audit` / `prod-agent-no-exec` / fuzz-corpus
  CI gates
- Release tag download URLs for v1 (`v0.7.1`–`v0.13.0`)
- Reproducibility commitment (double-build CI gate)
- `clippy::too_many_arguments = "deny"` workspace-wide

## Reporting issues

Open an issue at <https://github.com/tinylabscom/mvm/issues> and
include:

1. Your platform (`uname -a`)
2. `mvmctl --version`
3. Whether you came from v1 (which version)
4. What you expected (often: "this v1 surface")
5. What happened

If your issue is "feature X from v1 isn't in v2," tag it
`v1-parity` and the maintainers will slot it against plan 60's
phase schedule.
