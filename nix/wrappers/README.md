# Function-entrypoint wrapper templates

These are the per-language wrapper templates that bake into a microVM
rootfs when a workload declares `Entrypoint::Function`
(mvm ADR-007 / mvmforge ADR-0009).

Relocated from `mvmforge/nix/wrappers/` under mvm plan 49
(wrapper-templates-relocation). Canonical home is now
`mvm/nix/wrappers/`; these are the substrate-side reference
implementations.

| File | Language | Status |
| --- | --- | --- |
| `python-runner.py` | CPython 3.10+ | shipped |
| `node-runner.mjs` | Node 22+ | shipped |

## Contract

The wrapper reads `[args, kwargs]` from stdin in the IR-declared format,
dispatches `module:function`, and writes the encoded return value on
stdout. On user-code failure, it emits a single-line JSON envelope on
stderr and exits non-zero:

```json
{ "kind": "ValueError", "error_id": "abc-123", "message": "negative input" }
```

The host SDK parses this envelope and raises a structured `RemoteError`
in the caller's language.

## Build-time configuration

`mvm.lib.<system>.mkPythonFunctionService` /
`mkNodeFunctionService` (at `nix/lib/factories/`) consume the
canonical runner files in this directory via `pkgs.lib.fileContents`
and substitute `#!/usr/bin/env <runtime>` with the Nix-store path
of the runtime baked into `servicePackages`. Per-workload
configuration goes through `/etc/mvm/wrapper.json`, which the
runner reads at startup:

```json
{
  "module": "adder",
  "function": "add",
  "format": "json",
  "working_dir": "/app",
  "mode": "prod"
}
```

The wrapper reads this on startup. Both fields are baked at build —
nothing about dispatch is decided at call time except the args bytes.

## Modes

- **`prod`** (default): sets `PR_SET_DUMPABLE=0` (Linux), sanitizes error
  envelopes (no traceback, no file paths, no payload bytes in logs), no
  payload contents in operator logs. The full traceback flows through a
  separate operator-log channel (vsock secondary stream → host stderr,
  reachable via `mvmctl logs <vm>`) — never to the SDK caller.
- **`dev`**: prints the full traceback to stderr alongside the envelope.
  Never ship a `mode=dev` artifact to production.

## Decoder hardening

Both wrappers enforce ADR-0009 §Decoder hardening:

- max nesting depth 64 (cuts off recursive payloads)
- reject duplicate keys in JSON objects
- reject non-finite floats (NaN, ±Infinity)
- pinned to stdlib + a single audited msgpack library per language

## Forbidden imports (CI lane)

Phase 1 of plan-0007 wires `just wrapper-forbidden-check` into `just ci`:
the script greps wrapper templates for code-executing serializer
formats and dynamic-execution surfaces (per-language list in
[`scripts/wrapper_forbidden_tokens.json`](../../scripts/wrapper_forbidden_tokens.json),
derived from ADR-0009 §Decision). Lines containing
`# mvmforge-allow: <reason>` (Python) or `// mvmforge-allow: <reason>`
(JS/TS) are exempt.

## Threat model

The full threat model — what each defense addresses, where it lives,
and the known limits — is documented in
[`docs/src/content/docs/reference/wrapper-security.md`](../../docs/src/content/docs/reference/wrapper-security.md)
(rendered as **Reference → Wrapper Security & Threat Model** in the
Astro+Starlight docs site).

## When this gets used

The mvm-side factories at `nix/lib/factories/` consume these
canonical templates directly via `pkgs.lib.fileContents` and write
per-workload configuration to `/etc/mvm/wrapper.json` (post-Item-6
cleanup). Editing a wrapper here changes what every workload built
through `mk{Python,Node}FunctionService` runs — there is no parallel
inline copy to keep in sync.
