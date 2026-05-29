---
title: Rust quickstart
description: Declare mvm workloads from Rust and emit Workload IR.
---

The Rust SDK is the lower-level authoring surface and the ground-truth type model for Workload IR.

> **Status:** `crates/mvm-sdk` ships build-time workload builders and the runtime recording/lowering contract. A high-level Rust runtime lifecycle client is future parity work.

## Build-time declaration

```rust
use mvm_sdk::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let workload = workload("hello-rust")
        .app(
            app("hello")
                .source(local_path("."))
                .image(nix_packages(["bash", "coreutils"]))
                .entrypoint(entrypoint_command(["bash", "-lc", "echo hello from mvm"]))
                .resources(resources(1, 256, 512))
                .build()?,
        )
        .build()?;

    emit(&workload)?;
    Ok(())
}
```

Pipe the generated IR into the normal compile/build path used by the CLI.

## When to use Rust

- You need typed Workload IR construction.
- You are writing mvm-adjacent tooling.
- You need to validate generated plans before exposing them through a higher-level SDK.

Use Python or TypeScript when your application needs an ergonomic sandbox lifecycle today.
