---
title: Building from source
description: Build mvm and docs from the repository.
---

Use host cargo for normal Rust checks when they compile cleanly on macOS or Linux.

```sh
cargo check --workspace
cargo test --workspace
```

Use the builder VM for Nix builds, Firecracker operations, microVM runtime commands, and Linux-specific tests.

Docs live under `public/`:

```sh
cd public
npm install
npm run build
```

Security-sensitive build and runtime changes should update docs and tests in the same PR.
