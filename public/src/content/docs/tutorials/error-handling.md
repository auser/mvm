---
title: Error handling
description: Interpret build, admission, runtime, and restore failures.
---

Errors should tell you which boundary refused the operation.

## Common classes

| Class | Boundary | What to inspect |
| --- | --- | --- |
| Build failure | Builder VM or Nix input | build output, flake pins, package names |
| Admission failure | plan/policy | signer, policy ref, validity window, nonce |
| Runtime failure | guest process | command status, stderr, guest logs |
| File failure | guest filesystem RPC | path policy, permissions, symlinks, size |
| Network failure | egress policy | preset, allowlist, DNS/L7 policy |
| Restore failure | snapshot backend | seal/hash evidence, backend support, retention |

## SDK guidance

SDKs should return structured errors with:

- operation name;
- sandbox or workload identifier;
- audit/run identifier where available;
- sanitized stderr/stdout;
- security boundary that failed.

Secret material must not appear in exceptions, logs, or panic messages.
