---
title: Transparent rebuilds
description: Install packages from the console and have your session resume seamlessly on the new microVM
---

<!--
TODO: This page is a placeholder created in plan 62 for sidebar
parity. Intended content brief:

Flagship UX page. Document the user-visible flow of "install a
package and stay in your shell":

1. User runs `mvmctl install python:3.12` (or the in-VM equivalent
   that calls back to the host).
2. The console pauses with a progress indicator ("rebuilding
   rootfs…"). Tmux scrollback is preserved; cwd, env, and pending
   RPCs are checkpointed.
3. The warm builder microVM regenerates the rootfs (≤ 30 s with
   cached substituters; ≤ 5 s if everything is cached).
4. Rolling swap: pause → swap rootfs → resume. The persistent
   `/workspace` overlay stays mounted, so user files survive.
5. The console reattaches automatically. The user sees the new
   prompt with the package available. To them, it feels like a
   local install — no awareness of a VM swap.

Honest caveats: live processes inside the VM do restart on swap
(CRIU-based in-flight checkpointing is post-Phase-10). Tmux
sessions reattach intact (their state is in the overlay).

Show `--explain` (diff of base layers) and `mvmctl rebuild
--dry-run` (preview what would change) as inspection tools.

Cross-references:
- console/attach
- working/persistence
- working/snapshots
- plan 60 §"Transparent install / rebuild" (Phase 7a)
- plan 60 §"Snapshots — first-class feature"
- plan 60 §"Long-running sessions"
- ADR-013 (microsandbox/libkrun pivot — the warm builder design)
-->

> This page is a placeholder. Content is being written — see plan 62.
