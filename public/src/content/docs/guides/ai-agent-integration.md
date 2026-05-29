---
title: AI agent integration
description: Connect agents to mvm sandboxes with explicit policy and audit boundaries.
---

Agents should treat sandboxes as controlled tools, not ambient shell access.
For a concrete model-facing request/response contract, see [Agent tool contract](/guides/agent-tool-contract/).

## Recommended tool contract

- `create_sandbox(policy, image, resources)`
- `write_file(path, content)`
- `run_command(argv, timeout)`
- `read_file(path)`
- `snapshot_or_cold(name)`
- `stop_or_destroy()`

Every tool result should include a sandbox identifier and audit/run identifier where available.

## Security defaults

- Keep egress deny-by-default.
- Grant only the secrets needed for the current operation.
- Redact command output before returning it to a model if it may contain credentials or user data.
- Use short TTLs for transient sandboxes.
- Preserve cold state only when the workflow needs memory or filesystem continuity.
