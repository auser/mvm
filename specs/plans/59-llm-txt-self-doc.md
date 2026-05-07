# Plan 59 — `/.mvm/llm.txt` self-doc convention

Status: **Proposed.**

## Background

Sandbox-as-a-service products like sprites.dev ship a self-doc
convention inside every sandbox (`/.sprite/llm.txt`) so an LLM-driven
agent can self-orient when dropped into the box: discover checkpoint
semantics, mount layout, RPC entry points, and substrate identity
without needing host-side context.

mvm/mvmd is explicitly aimed at agent workloads (function-call
entrypoints per ADR-007, `RunEntrypoint`, plan 41). Cheap to ship
the equivalent now while the API shape is still wet; hard to
retrofit later because every existing template would need rebuild.

## Goal

Bake `/.mvm/llm.txt` into every guest rootfs by default. Verity-sealed,
owned `root:root`, mode `0644`. Caller-supplied `extraFiles` overrides
via attrset merge.

## Implementation

### Where it lands

`mvm/nix/flake.nix` — `mkGuest`'s `extraFiles` argument is the seam.
Around line 241 it accepts a caller map; around line 346–360 it
renders the populate-block. Add a default entry to the merged
`extraFiles` so every built rootfs gets `/.mvm/llm.txt` for free.

### Content

A small markdown document with these sections, templated with build-
time substitutions:

- Header: substrate name, agent version, variant (prod/dev), verity
  flag, build timestamp.
- "What is this?" — one-paragraph identification of the rootfs as a
  mvm guest.
- "RPC entry points" — vsock CID/port for the guest agent
  (`GUEST_AGENT_PORT=52`), the verb families (fs/proc/share), the
  control-socket location.
- "Checkpoint semantics" — pause/resume, how `PostRestore` works,
  where state lives in `~/.mvm/instances/`.
- "Where to find more" — `/etc/mvm/`, `mvmctl --help`, links to
  ADR-002 (security posture) and ADR-007 (function-call entrypoints)
  in canonical doc-site URLs.

### Override semantics

Workload flakes that supply their own `/.mvm/llm.txt` entry in
`extraFiles` override mvmd's default. The merge happens inside
mvmd's per-workload helper (caller-wins: workload `extraFiles`
override mvmd defaults), and the merged map is passed verbatim to
upstream `mkGuest`. The substrate `mkGuest` does not perform any
default-vs-caller merge of its own.

### Test

mvmd-side: an integration test that exercises the per-workload
helper end-to-end (build a guest rootfs, mount/inspect it) and
asserts:

- `/.mvm/llm.txt` exists on the rootfs.
- File mode is `0644`.
- Content begins with the expected header (e.g. starts with
  `"# mvm guest"`).
- Content contains the agent version string.

mvm-side: nothing to test here — the `mkGuest` `extraFiles` plumbing
is exercised by existing fixtures (e.g. `nix/images/examples/echo-fn`
already bakes a wrapper through `extraFiles`).

## Critical files (mvmd-side)

- New (in mvmd): a per-workload helper that renders the `llm.txt`
  content from agent context (workload id, agent version, variant)
  and threads it into the `mkGuest` call as
  `extraFiles."/.mvm/llm.txt"`.
- mvm-side: nothing changes. The `mkGuest` `extraFiles` arg already
  accepts the map; the substrate stays workload-agnostic.
- Reference precedent: `mvm/nix/images/examples/echo-fn/flake.nix:70-79`
  shows the `extraFiles` shape (used today to bake a wrapper + marker).

## Verification

- mvmd integration test that drives a function-workload through to a
  rootfs build, then inspects the derivation output to confirm
  `files/.mvm/llm.txt` is present with mode 0644.
- Plain `mvmctl build` of a non-agent fixture (e.g.
  `nix/images/examples/hello-node/`) produces a rootfs with **no**
  `/.mvm/llm.txt` — substrate stays clean.
- Override test: a workload flake that supplies its own
  `/.mvm/llm.txt` in `extraFiles` wins over mvmd's default —
  content matches the caller's, not mvmd's.

## Effort

~half-day. File mode + content templating + one test assertion.

## Out of scope

- Runtime mutability — `/.mvm/llm.txt` is verity-sealed and read-only
  in production. Agent-written runtime self-doc is a separate plan.
- Per-template content customization beyond what `mkGuest`'s build-
  time vars expose — a caller who wants richer custom content
  overrides via `extraFiles`.
