---
title: Tutorials
description: Practical mvm workflows for agent sandboxes, code execution, file transfer, LLM tools, browser automation, services, and cold-mode recovery.
---

These tutorials mirror the workflows developers expect from an agent sandbox platform, but each one keeps the `mvm` trust boundary visible.

Use tutorials when you want to complete one workflow end to end. Use
[Guides](/guides/) when you need the underlying operating model, security
policy, or troubleshooting detail behind that workflow.

| Tutorial | Use it for | Security boundary |
| --- | --- | --- |
| [Agent sandbox](/tutorials/agent-sandbox/) | Run generated or third-party code in a microVM. | Guest code runs behind microVM, network, filesystem, and audit boundaries. |
| [Code execution](/tutorials/code-execution/) | Execute a command or script and capture output. | Exec is a guest operation; host files and secrets are explicit inputs only. |
| [File transfer](/tutorials/file-transfer/) | Move files into and out of the guest. | Transfer paths are narrower than broad host mounts. |
| [LLM tool integration](/tutorials/llm-tool-integration/) | Call a sandbox from an LLM tool loop. | Tool calls should use deny-by-default egress and redacted logs. |
| [Browser automation](/tutorials/browser-automation/) | Run Playwright or Puppeteer inside an isolated guest. | Browser state, credentials, downloads, and egress are sensitive. |
| [Services and ports](/tutorials/services-and-ports/) | Start a service and expose a host port. | Port exposure is explicit and audited. |
| [Cold-mode recovery](/tutorials/cold-mode-recovery/) | Pause, restore, and wake a sandbox from saved state. | Snapshot contents are sensitive and backend-specific. |

## Local path

Tutorials use `mvm` directly:

```text
local: SDK or CLI -> mvm -> microVM
```

When an example shows a target SDK shape that is not shipped yet, the tutorial marks it as planned rather than implying the behavior already exists.

## Status labels

Some SDK snippets show target APIs that are not shipped yet. Those snippets are labeled Planned. CLI examples use current `mvmctl` commands unless a page explicitly says the API is illustrative.

See [Security claim ledger](/security/claim-ledger/) and [Sandbox parity status](/security/sandbox-parity-status/) for claim status.
