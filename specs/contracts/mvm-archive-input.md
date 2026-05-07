# Contract: `mvmctl up --flake <path>` accepts a single-archive artifact

## Status

**Active — binding upstream contract** (activated 2026-05-06).

ADR-0012 (single-archive compiled artifact) defines mvmforge's
side: `mvmforge compile --out <path>.tar.gz` produces a deterministic
gzipped tar of the artifact. This document is the binding contract
that the upstream `mvmctl` implementation must satisfy on the
intake side. The paired upstream-side work is sequenced via
[`specs/upstream-mvm-prompt.md`](../upstream-mvm-prompt.md).

## Audience

mvm maintainers extending `mvmctl up`. mvmforge maintainers using
this as the source of truth for what shape `mvmforge up` hands to
`mvmctl`.

## Background

`mvmforge` produces an artifact from a Workload IR manifest. As of
ADR-0006/0007 the artifact has been a directory containing
`flake.nix`, `launch.json`, and a bundled `source/` tree. ADR-0012
adds a tarball variant: same logical contents, packaged as a single
deterministic `.tar.gz` for distribution and content-addressable
storage.

`mvmctl up` today accepts a directory: `mvmctl up --flake
/path/to/artifact-dir`. To consume the new tarball shape directly
without forcing every consumer to extract first, `mvmctl up` must
also accept a `.tar.gz` path.

## Contract

`mvmctl up --flake <path>` MUST accept either:

1. **a directory**, with the existing semantics; or
2. **a regular file ending in `.tar.gz` or `.tgz`** (case-insensitive),
   where the file is a gzipped POSIX tar of an artifact directory.

The dispatch is by the path's filesystem type:

- If `<path>` is a directory → existing behavior.
- If `<path>` is a regular file with `.tar.gz` / `.tgz` suffix →
  archive-mode behavior (below).
- Anything else → error with a stable code, e.g.
  `E_UP_INPUT_KIND_UNSUPPORTED`.

### Archive-mode behavior

When given a tarball, `mvmctl` MUST:

1. **Extract to an agent-controlled temp dir.** Pick a path
   under `mvmctl`'s own working area (e.g. `/run/mvmctl/up-<rand>`
   or `$XDG_RUNTIME_DIR/mvmctl/up-<rand>`). Create the dir 0700.
2. **Refuse path traversal.** Refuse any tar entry whose
   normalized path component contains `..` or starts with `/`.
   Refuse symlink entries (the mvmforge-side archiver doesn't emit
   them, per ADR-0012; defense in depth).
3. **Cap inflated size.** Default 1 GiB; configurable via
   `MVMCTL_MAX_ARCHIVE_INFLATED_BYTES`. Exceeding the cap aborts
   the extraction with `E_ARCHIVE_TOO_LARGE`.
4. **Verify the extracted tree shape.** After extraction, the temp
   dir must contain at least `flake.nix`, `launch.json`, and a
   `source/` directory. Missing top-level entries → fail with
   `E_ARCHIVE_LAYOUT_INVALID`.
5. **Treat the temp dir as the artifact directory** for the rest of
   the boot pipeline. Identical semantics to the directory mode
   from that point on.
6. **Clean up the temp dir** when the workload exits or the boot
   handoff fails — same lifetime as a directory artifact's temp
   resources.

### What `mvmctl` MUST NOT do

- **No on-the-fly de-duplication of identical archives across
  invocations.** Each `mvmctl up <path>.tar.gz` extracts fresh —
  determinism comes from the archive bytes, not from a cache that
  could go stale.
- **No fallback fetch.** If the path doesn't resolve to a directory
  or a `.tar.gz` file, fail loudly. Don't try to interpret it as a
  URL, an OCI ref, a Nix store path, or any other shape.
- **No mtime-derived behavior.** The archive mtime is zero by
  contract (see ADR-0012); consuming code must not branch on it.

## Error codes (stable)

- `E_UP_INPUT_KIND_UNSUPPORTED` — path is neither a directory nor a
  recognized archive file.
- `E_ARCHIVE_TOO_LARGE` — inflated size exceeds the configured cap.
- `E_ARCHIVE_LAYOUT_INVALID` — extracted tree is missing required
  top-level entries (`flake.nix`, `launch.json`, or `source/`).
- `E_ARCHIVE_PATH_TRAVERSAL` — a tar entry tried to escape the
  extraction root.

These should follow the same envelope shape `mvmctl` already uses for
boot-time errors (single-line stable code + human-readable detail).

## Validation

- `mvmforge compile <manifest> --out artifact.tar.gz` produces a
  tarball; piping that into `mvmctl up --flake artifact.tar.gz`
  succeeds with the same boot-time outcome as the equivalent
  directory artifact.
- A tarball with a `..`-traversal entry is refused with
  `E_ARCHIVE_PATH_TRAVERSAL`.
- A 2 GiB tarball with the default 1 GiB cap is refused with
  `E_ARCHIVE_TOO_LARGE`.
- A tarball missing `flake.nix` or `launch.json` is refused with
  `E_ARCHIVE_LAYOUT_INVALID`.
- The temp extraction dir is cleaned up after a successful boot and
  after a failed boot.

## Related

- ADR-0012 (mvmforge): defines the archive shape this contract
  consumes.
- ADR-0010 §3 amended: factories move to mvm in the same upstream
  session.
- [`specs/contracts/mvm-mkfunctionservice.md`](mvm-mkfunctionservice.md):
  the paired contract for the factory side of the same upstream
  release.
- [`specs/upstream-mvm-prompt.md`](../upstream-mvm-prompt.md): the
  upstream-coordination prompt that includes both contracts.
