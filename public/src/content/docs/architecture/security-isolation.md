---
title: Security and isolation
description: Security-first architecture boundaries for mvm.
---

MVM's security posture is built from multiple layers rather than one control.

## Build boundary

Linux image construction goes through the builder VM. That keeps Nix evaluation, microVM image assembly, and Linux-only tooling out of the macOS host path.

## Launch boundary

Launches should pass through signed plan admission. A plan binds workload identity, artifact identity, resources, policy references, validity window, and nonce handling.

## Runtime boundary

Guest workloads run in microVMs. Control-plane operations should use the guest protocol and runtime supervisor instead of broad guest access.

## Policy boundary

Network, secrets, resources, and admission are policy-plane decisions. Examples should make those decisions visible.

## Audit boundary

Every high-value action should produce evidence: build, admission, launch, secret grant, network policy decision, snapshot, restore, and destroy.

## Host-executed SDK code

Runtime SDK record/live scripts execute host-side SDK code. Static decorator compilation is the safer authoring path when you need to inspect declarations without importing user modules.

## Related pages

- [Policy profiles](/guides/policy-profiles/)
- [Audit and receipts](/guides/audit-and-receipts/)
- [Threat model](/security/threat-model/)
- [Security claim ledger](/security/claim-ledger/)
