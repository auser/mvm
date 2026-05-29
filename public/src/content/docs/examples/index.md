---
title: Examples
description: Practical mvm patterns for agents, CI builders, development VMs, and code execution.
---

These examples are intentionally small. They show the product shape without
claiming behavior that is not shipped.

| Example | Use it when | Security boundary |
| --- | --- | --- |
| [AI agent sandbox](/examples/ai-agent-sandbox/) | A model or agent needs to run tools. | Deny-by-default network, narrow files, redacted output. |
| [CI/CD ephemeral builder](/examples/ci-cd-ephemeral-builder/) | CI needs a disposable Linux build environment. | Fresh runtime state, builder VM for image builds, receipts. |
| [Reproducible dev VM from a flake](/examples/dev-vm-from-flake/) | A project needs a repeatable local Linux runtime. | Pinned Nix inputs and explicit runtime sizing. |
| [Code interpreter pattern](/examples/code-interpreter/) | User code should run outside the host process. | Timeout, bounded output, no default secrets. |

For SDK-shaped workflows, see [SDK overview](/sdk/) and [Tutorials](/tutorials/).
