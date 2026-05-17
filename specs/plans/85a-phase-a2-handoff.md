# Plan 85 Phase A.2 — session handoff prompt

> This file is **not a spec** — it's a self-contained prompt to drop
> into a fresh Claude Code session (or hand to a human picking up the
> work) once PR #319 (Plan 77 W7) and PR #321 (Plan 85 spec + Phase
> A.1 unpacker) are merged. The prompt briefs the next session like
> a colleague walking in cold: state on `main`, what's deliberately
> in / out of scope for A.2, design constraints to honor, the
> starting move. Delete this file when Phase A.2 ships.

---

I'm picking up Plan 85 (mvm-oci as user-facing image-runner primitive) at **Phase A.2** — OCI whiteouts + first cargo-fuzz target.

## Context on `main`

- **PR #319** (Plan 77 W7 — Stage 0 reads vendored seed + three follow-up bugfixes: `BUILDER_INIT_PATH` byte-scan needle, PID-1 PATH, egress-lockdown gating) — **merged**.
- **PR #321** (Plan 85 spec + Phase A.1 base unpacker) — **merged**. If not, stop and merge first; A.2 stacks on A.1 directly.
- Plan 85 spec: `specs/plans/85-mvm-oci-user-image-runner.md` (load-bearing — read its Phase A.2 entry and §"Security envelope" before writing code).
- Unpacker: `crates/mvm-oci/src/unpack.rs`. A.1 handles regular files, directories, and in-root symlinks; everything else (hardlinks, device nodes, whiteouts, …) refuses with `RefusalReason::UnsupportedEntryType`.

## Phase A.2 scope

OCI Image Spec v1.1 §"Layer Filesystem Changeset" defines two whiteout shapes — both arrive as regular-file tar entries (typeflag `'0'`), the *filename* is what signals the semantic:

1. **`.wh.<name>`** — remove `<name>` from the assembled tree at this layer. The marker file itself is **not** written to the unpack output (the OCI spec requires whiteout markers not appear in the final rootfs).
2. **`.wh..wh..opq`** — clear the parent directory's prior-layer contents at this layer, but preserve the directory itself. The marker file itself is **not** written.

Both must respect the A.1 safety envelope: refuse on absolute paths, traversal segments, `JoinedPathEscape`, and — critically — `SymlinkInParent`. A whiteout targeting a path under a symlinked parent should refuse the same way regular files do.

A.2 deliverables:

- Recognize whiteout markers via filename prefix on entries that come through the `Regular | Continuous` arm of `unpack_layer`'s dispatch.
- Apply (`fs::remove_file` / `remove_dir_all`) the corresponding target in the assembled tree if present; idempotent if absent (Phase A.2 is single-layer; multi-layer is the caller's composition).
- For opaque markers: clear the parent dir's contents but keep the dir.
- New report fields: `whiteouts_applied: u64`, `opaque_markers_applied: u64`.
- New `RefusalReason` variants only if a whiteout edge case truly needs explicit rejection (e.g. `WhiteoutTargetEscape` if a `.wh.` filename combined with a parent path escapes root). Don't add variants speculatively.
- **First fuzz target**: `crates/mvm-oci/fuzz/fuzz_targets/unpack_layer.rs` feeding arbitrary bytes to `unpack_layer`, asserting (a) no panics, (b) no escape from `output_root`, (c) no leaked file descriptors. Plan 85 §R1 calls for a 30-min per-PR-on-mvm-oci gate; A.2 establishes the fuzz infrastructure even if the corpus is small at first. Wire the gate in `.github/workflows/ci.yml` to run on PRs touching `crates/mvm-oci/**`.

## Don't violate

- `tar::Header::set_path` refuses leading `/` and `..` at construction time. Adversarial fixtures use the `handrolled_tar_with_path` helper. Whiteout fixtures CAN use `tar::Builder` since `.wh.<name>` paths are legal tar entries.
- `RefusalReason::audit_tag()` returns wire-stable strings (no spaces, `=`, `,`, newlines). New variants must follow.
- `parent_chain_has_symlink` is the load-bearing CVE-class guard. Whiteouts under a symlinked parent refuse, same as regular files.
- Sync API only (`fn unpack_layer<R: Read>`); async callers wrap with `spawn_blocking`.
- Use `std::os::unix::fs::symlink` / `std::fs::symlink_metadata` (Unix-only — OCI layers are POSIX).
- One PR per sub-phase. Phase A.2 is one PR.
- Work in a git worktree, never the main checkout (memory: parallel sessions).

## Workflow

```sh
cd $REPO_ROOT
git fetch origin
git worktree add -b feat/plan85-a2-whiteouts .worktrees/plan85-a2 origin/main
cd .worktrees/plan85-a2
# write code
cargo test -p mvm-oci --lib unpack::
cargo clippy -p mvm-oci --tests --all-targets -- -D warnings
# both must be clean before pushing
```

PR base: `main`. Title: `feat(mvm-oci): plan 85 phase A.2 — OCI whiteouts + first fuzz target`.

## Out of scope (don't get pulled in)

- **DNS-in-Stage-0** — libkrun TSI UDP gap; real but separate from Plan 85.
- **Plan 77 vendored-seed work** — shipped; Plan 85 is the user-image runner; they coexist.
- **W4 microsandbox cleanup** — already done in commit `b02a5e8` (2026-05-14).
- **Hardlinks / xattrs / device nodes / setuid** — Phases A.3–A.6; do not bleed into A.2.

## Starting move

Read `crates/mvm-oci/src/unpack.rs` end-to-end (~700 lines, ~5 min). Then read the OCI Image Spec v1.1 §"Layer Filesystem Changeset" online for the canonical whiteout semantics. Then start writing.
