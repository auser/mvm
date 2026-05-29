---
title: Desktop automation
description: Model desktop-style automation as a controlled sandbox workload.
---

Desktop automation can involve credentials, session files, screenshots, browser profiles, and local documents. Treat it as sensitive by default.

## Pattern

1. Build a Nix image with the automation runtime and tools.
2. Copy only the files needed for the task into the guest.
3. Use explicit network allowlists.
4. Keep credentials as references or short-lived grants.
5. Snapshot only when you intend to preserve session state.

## Security boundaries

- Do not mount broad host directories.
- Do not persist browser profiles unless retention is intentional.
- Treat screenshots and recordings as sensitive artifacts.
- Use audit IDs to connect automation runs to policy decisions.

## Status

This is a product pattern. The SDK helpers for high-level desktop sessions are planned; use CLI/runtime primitives and explicit images today.
