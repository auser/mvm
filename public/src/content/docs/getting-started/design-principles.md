---
title: Design principles
description: The security-first rules that shape mvm developer experience.
---

## Security first

Developer ergonomics must make the secure path short without hiding the security model. Signed plans, explicit policy, audit records, and workload identity are product features, not internal details.

## Nix first, OCI compatible

Nix-built microVM artifacts are the preferred path for reproducibility, extensibility, and auditability. OCI inputs are compatibility inputs and should be digest-pinned for production policy.

## Runtime semantics live in mvm

If a feature changes what a sandbox does, it belongs in `mvm` first. Hosted or higher-level surfaces should consume the same runtime primitive instead of redefining execution semantics.

## Static compile when possible

Decorator declarations should compile without importing user code. Runtime SDK scripts are useful, but record/live modes execute host-side SDK code and must carry that trust label.

## Explicit state

Running, paused, cold, restoring, stopped, and destroyed are distinct states. APIs should not blur them, because cleanup, retention, billing, and threat model differ.

## Explicit egress

Workloads should declare what they need. Examples should start from deny-by-default networking and add only the required destinations.

## Audit everywhere

Launches, policy decisions, secret grants, snapshot operations, and restore operations should produce evidence that callers can connect back to SDK and API actions.
