# mvm ur-seed (Plan 86 / ADR-054)

Stage –1 bootstrap rootfs + kernel for the libkrun builder VM. Exists
only to close the Plan 77 W5 catch-22 on contributor hosts that have
no prior contract-compliant dev image.

## What it is

`flake.nix` produces a self-contained tarball per arch
(`ur-seed-<arch>-linux.tar.gz`) containing:

- `rootfs.ext4` — ext4 image holding:
  - `busybox-static` (POSIX userland) symlinked into `/bin` + `/sbin`.
  - `mvm-builder-init` (musl-static) at `/sbin/mvm-builder-init` —
    PID 1, satisfies the Plan 77 W5 seed contract.
  - The same runtime closure as the steady-state builder VM
    (`bash`, `coreutils`, `nix`, `e2fsprogs`, `iptables`, `util-linux`,
    …), staged under `/nix/store` with bin/sbin symlinks into
    `/usr/local/bin` + `/sbin`.
  - Kernel module tree at `/lib/modules/<kver>/` (virtio-fs, fuse,
    vsock — required for libkrun shares + agent transport).
- `vmlinux` — TSI-patched kernel from
  `../../nix/images/builder-vm/kernel/`. Stock nixpkgs `linuxPackages.kernel`
  lacks the TSI patches libkrun's AF_INET routing requires
  (Plan 72 W5.D bullet 10), so the ur-seed ships its own.
- `manifest.json` — seed-contract metadata (`image_kind=ur-seed`,
  `contract_version=2`, declared `init_paths`).
- `cmdline.txt` — informational; Stage 0 uses its own pinned cmdline.

## When it's built

- **Release time** (CI, Linux Nix runner) — published to GitHub
  releases alongside `mvmctl`. The `release.yml` workflow attaches
  `ur-seed-<arch>-linux.tar.gz` + `.sha256` to each release.
- **Manual bootstrap** — when a release isn't available yet, a
  contributor with a Linux+Nix env (native, remote, or Docker)
  builds the tarball and installs it via
  `mvmctl dev import-ur-seed --from <path>`.

`mvmctl dev up` **never** builds this flake. The contributor's
hot path on `dev up` is the cached ur-seed in
`~/.cache/mvm/ur-seed/<arch>/`, populated once by `fetch-ur-seed` or
`import-ur-seed`. See ADR-054 §"Acquisition policy" for why.

## Building manually (Docker bootstrap)

```sh
# From the workspace root, with Docker available:
docker run --rm -v "$PWD:/work" -v "$PWD/.ur-seed-out:/out" \
  -w /work nixos/nix:latest sh -c '
    MVM_WORKSPACE_PATH=/work nix \
      --extra-experimental-features "nix-command flakes" \
      build --impure \
      "path:/work/nix/ur-seed#packages.aarch64-linux.default" \
      --out-link /tmp/result
    cp -L /tmp/result/ur-seed-aarch64-linux.tar.gz       /out/
    cp -L /tmp/result/ur-seed-aarch64-linux.tar.gz.sha256 /out/
  '

# Install:
mvmctl dev import-ur-seed --from ./.ur-seed-out/ur-seed-aarch64-linux.tar.gz
```

For `x86_64-linux` swap the system attribute. Cross-arch builds work
on either-arch Linux runners.

First build cost: ~20 min (the TSI-patched kernel rebuild dominates).
Subsequent builds reuse the cached kernel store path.

## What the rootfs ships

The `urSeedPackages` list in `flake.nix` defines the runtime closure.
It mirrors `nix/images/builder-vm/flake.nix`'s `builderPackages` so
the ur-seed's runtime shape matches the steady-state builder VM —
the same `nix`, `bash`, `iptables`, etc. that `mvm-builder-init`
expects to find when it dispatches the in-VM job.

Bumping a package version is a one-line change to that list. The
ur-seed rebuild after a bump produces a different tarball SHA-256
deterministically (modulo nixpkgs revisions); cut a fresh release to
re-publish.

## Why a separate flake vs. extending the builder/builder-vm flakes

The ur-seed is upstream of both. Mixing its derivation into either
of those flakes risks circular imports (the builder-vm flake's
output is what the ur-seed produces *next time*, and the dev image
flake uses the builder VM as its build env). Keeping the ur-seed in
its own flake makes the dependency direction explicit and avoids
accidentally couplings to either downstream flake.

## See also

- ADR-054 (`specs/adrs/054-ur-seed-stage0-bootstrap.md`) — design
  rationale, alternatives considered, security model addendum.
- Plan 86 (`specs/plans/86-ur-seed-stage0-bootstrap.md`) — execution
  plan + workstreams.
- Plan 77 (`specs/plans/77-stage0-bootstrap-via-dev-image.md`) — the
  W5 seed contract the ur-seed satisfies.
