---
title: Core concepts
description: The runtime, builder VM, Workload IR, plans, policies, and cold-mode states.
---

## mvm

`mvm` is the local runtime substrate. It owns image materialization, microVM lifecycle, guest communication, signed plan admission, local audit, and backend-specific snapshot mechanics.

## Builder VM

The builder VM is the Linux execution boundary for Nix evaluation, image builds, and microVM-specific build work. Host-side cargo checks can run on macOS when they compile cleanly; Linux-only runtime work belongs inside the builder VM.

## Workload IR

Workload IR is the declarative contract between SDK authoring and runtime build/launch. Decorator SDKs and runtime recordings both lower into this contract.

## Signed plans

Execution plans bind workload identity, artifact identity, resource shape, policy references, validity windows, and nonces before launch.

## Policy plane

The policy plane decides what a workload may do: network egress, secrets, resources, and admission constraints. Runtime ergonomics should expose those decisions instead of bypassing them.

## Cold mode

Cold mode is a product lifecycle state built from backend snapshot/restore primitives. Snapshot contents are sensitive; restore should preserve integrity evidence and audit context.
