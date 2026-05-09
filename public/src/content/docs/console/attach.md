---
title: Attach to a microVM
description: Open an interactive shell to a running microVM and reattach across sessions
---

<!--
TODO: This page is a placeholder created in plan 62 for sidebar
parity. Intended content brief:

Document `mvmctl console <name>` end-to-end:
- accessible vs sealed images (the gate that decides if a console
  is even available — sealed production images refuse)
- one-shot exec (`--command "uname -a"`) vs interactive shell
- detach/reattach across host disconnects (long-running tmux-backed
  sessions; survive `mvmctl down`/`up`, host suspend/resume)
- scrollback semantics, signals, terminal size negotiation

Cross-references:
- console/index
- console/transparent-rebuild
- crates/mvm-guest/src/console.rs
- plan 60 §"Long-running sessions"
- plan 60 W6.2 (the accessible/sealed gate implementation)
-->

> This page is a placeholder. Content is being written — see plan 62.
