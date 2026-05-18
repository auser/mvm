# mvm ur-seed (Plan 86 / ADR-054)

Stage -1 bootstrap rootfs for the libkrun builder VM. Exists only to
close the Plan 77 W5 catch-22 on contributor hosts that have no prior
contract-compliant dev image.

## What it is

`flake.nix` produces a ~120 MB rootfs.ext4 containing:

- `busybox-static` — POSIX userland, single binary.
- `mvm-builder-init` (musl-static) — present at `/sbin/mvm-builder-init`
  so the Plan 77 W5 seed-contract check passes. Not used at runtime —
  the ur-seed cmdline overrides PID 1 to `/sbin/ur-seed-init`.
- `/sbin/ur-seed-init` — a POSIX shell script that mounts virtio-fs
  shares, runs `nix-portable nix build path:/work#default` against
  the in-repo `nix/images/builder-vm/` flake, copies the resulting
  `vmlinux`/`rootfs.ext4` to the output share, and halts.
- `nix-portable` (pinned upstream binary, see `pins.json`) — the
  self-extracting Nix the script invokes.

The output of `nix build .#default` is a directory containing
`rootfs.ext4`, `manifest.json`, `cmdline.txt`, plus a packed
`ur-seed-<arch>.tar.gz` + `ur-seed-<arch>.tar.gz.sha256`.

## When it's built

- **Release time** (CI, Linux Nix runner) — published to GitHub
  releases alongside `mvmctl`.
- **Manual bootstrap** — when a release isn't available yet, a
  contributor with a Linux+Nix env (native, remote, or Docker)
  builds the tarball and installs it via
  `mvmctl dev import-ur-seed --from <path>`.

`mvmctl dev up` **never** builds this flake. The contributor's
hot path on `dev up` is the cached ur-seed in
`~/.cache/mvm/ur-seed/<arch>/`, populated by `fetch-ur-seed` or
`import-ur-seed`. See ADR-054 §"Acquisition policy" for why.

## Building manually (Docker bootstrap)

```sh
# From the workspace root, with Docker available:
docker run --rm -v "$PWD:/work" -w /work/nix/ur-seed \
  nixos/nix:latest \
  nix --extra-experimental-features "nix-command flakes" \
  build .#packages.aarch64-linux.default

# Result is at ./result/ — install with:
mvmctl dev import-ur-seed --from ./result/ur-seed-aarch64-linux.tar.gz
```

For `x86_64-linux` swap the system attribute. Cross-arch builds work
on either-arch Linux runners.

## Bumping the nix-portable pin

1. Pick a new release at https://github.com/DavHau/nix-portable/releases.
2. Download both arch binaries and compute sha256.
3. Update `nixPortablePin` in `flake.nix` AND `pins.json` (keep in sync).
4. Re-build via Docker and verify the resulting `ur-seed-*.tar.gz`
   sha256 changes deterministically.
5. Cut a release so contributors can `fetch-ur-seed` the new bytes.

## Why nix-portable specifically

It's a single relocatable Nix binary that doesn't need root or a
pre-existing Nix daemon. The bounded-bridge equivalent of "we need
Nix inside an environment that has no Nix" — see the
`feedback_replace_over_workaround.md` memory: vendoring as a bounded
bridge is acceptable. Replacing nix-portable with something we own
(e.g. a curated minimal-Nix derivation) is a deliberate non-goal of
Plan 86 and not blocked by it.
