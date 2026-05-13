# Runbook — `mvmctl tenant destroy`

**Audience:** hosted-cloud operators deprovisioning a tenant, plus
the auditors who verify their destruction certificates.

**Status:** Plan 60 Phase 7a Slices A + D shipped; this runbook
covers the user-facing workflow as of those commits. Slice B
(LUKS keyslot revocation) and Slice C (in-VM overlay attach) are
follow-ons that strengthen but don't replace the workflow below.

---

## What it does

`mvmctl tenant destroy --tenant <id> --confirm-deletion` walks
every overlay under `~/.mvm/overlays/<id>/`, **zero-fills each
file's bytes before unlinking** (so the bytes aren't merely
freed; they're overwritten), removes the directories, and emits
one **signed destruction certificate per workload** to stdout as
a JSON array. Human-readable progress goes to stderr so the
operator can pipe stdout directly to a file.

The certificate is signed under the host identity key at
`~/.mvm/keys/host-signer.ed25519` (the same key plan 64 W2
introduced for the audit chain). An auditor with the operator's
public key — typically `~/.mvm/keys/host-signer.pub`, base64-
encoded — can verify the certificate independently of the
operator's host.

## Operator workflow

```bash
# 1. Run the destroy. The --confirm-deletion flag is required
#    defense-in-depth: without it, the command exits non-zero
#    and the overlay is untouched.
$ mvmctl tenant destroy --tenant acme --confirm-deletion > certs.json
mvmctl tenant destroy: tenant="acme" overlay_root=/home/op/.mvm/overlays
  found 3 overlay(s)
  ✓ acme/build-runner: 42 file(s), 1048576 byte(s) wiped
  ✓ acme/code-eval: 8 file(s), 524288 byte(s) wiped
  ✓ acme/test-runner: 17 file(s), 65536 byte(s) wiped
destroyed 3 overlay(s): 67 file(s), 1638400 byte(s) total. \
    Certificate(s) printed to stdout.

# 2. Hand certs.json to the auditor (or the tenant's
#    compliance contact). Bundle with the operator's pubkey:
$ cat ~/.mvm/keys/host-signer.pub | base64 > operator-pubkey.b64

# 3. Optionally retain a sealed copy locally — the chain-signed
#    `cmd.tenant.completed` audit entry references the
#    invocation, so the file alone is a tampering risk; the
#    chain anchor is the corroboration.
```

## Auditor workflow

The auditor has three pieces of evidence:

1. The signed-certificate JSON (`certs.json`).
2. The operator's host identity pubkey (`operator-pubkey.b64`).
3. The operator's audit chain (`~/.mvm/audit/local.jsonl`).

Each piece feeds an independent verification axis. An attacker
forging a destruction certificate has to defeat **all three**:

| Axis | What it checks | What it catches |
|------|----------------|-----------------|
| Signature | Ed25519 over canonical payload | Receipt-field tampering |
| Pubkey pin | Embedded `signer_pubkey` matches operator's known key | Forgery under attacker key |
| Chain anchor | SHA-256 fingerprint in `lifecycle.tenant.destroyed` event | Forging without chain anchor; cert swap post-emission |

### One-command three-axis verification

The strongest check is the single CLI invocation that exercises
all three axes:

```bash
$ mvmctl audit verify-cert certs.json \
      --pubkey operator-pubkey.b64 \
      --chain operator-audit-chain.jsonl
mvmctl audit verify-cert: 3 certificate(s) verified
  ✓ acme/build-runner: 42 file(s), 1048576 byte(s) wiped at 2026-05-11T18:00:00Z [chain ✓]
  ✓ acme/code-eval: 8 file(s), 524288 byte(s) wiped at 2026-05-11T18:00:01Z [chain ✓]
  ✓ acme/test-runner: 17 file(s), 65536 byte(s) wiped at 2026-05-11T18:00:02Z [chain ✓]
```

Per-cert markers tell the auditor which axis fired:

- `[chain ✓]` — fingerprint matches an audit-chain entry
- `[chain ✗ MISSING ENTRY]` — cert claims a destruction the
  chain doesn't witness; suspect forged cert
- `[chain ✗ FINGERPRINT MISMATCH]` — chain has an entry for
  this tenant/workload but the fingerprint differs; operator
  swapped a cert after the chain was written

The command exits non-zero if any chain check fails (Missing
or Mismatched). Skipping the `--chain` axis (just omit the
flag) exits zero — useful when the auditor doesn't have access
to the operator's chain file.

### Other failure modes the command surfaces

- **SignatureInvalid** — any receipt field tampered after
  signing (tenant rename, `files_wiped` padding, timestamp
  shift). Exit non-zero.
- **PubkeyMismatch** — cert's embedded `signer_pubkey` doesn't
  match `--pubkey`. Exit non-zero.
- **UnsupportedVersion** — future v2+ certificate the
  auditor's verifier hasn't been updated to parse. Exit
  non-zero.
- Parse error on the cert or pubkey file — exit non-zero with
  context.

### Reading the chain manually

If the auditor wants to inspect the chain entries directly
(e.g., to investigate a `[chain ✗ MISSING ENTRY]` result),
`mvmctl audit tail --chain` walks the chain file:

```bash
$ mvmctl audit tail --chain --tenant local | \
    jq 'select(.entry.event == "lifecycle.tenant.destroyed")'
{
  "entry": {
    "event": "lifecycle.tenant.destroyed",
    "labels": {
      "tenant": "acme",
      "workload": "build-runner",
      "files_wiped": "42",
      "bytes_wiped": "1048576",
      "cert_fingerprint": "8a3f2c91..."
    },
    ...
  },
  ...
}
```

Recomputing the fingerprint from the cert file confirms a
match:

```bash
$ jq -c '.[0]' certs.json | shasum -a 256
8a3f2c91... -
```

The `mvmctl audit verify-cert --chain` flow does this
comparison automatically; the manual path is for diagnostics.

### Pipe + stdin

The cert source supports `-` so an auditor can pipe:

```bash
$ cat certs.json | mvmctl audit verify-cert - \
      --pubkey operator-pubkey.b64 \
      --chain operator-audit-chain.jsonl
```

JSON output (`--json`) emits `{ "receipts": [...],
"chain_matches": ["matched", ...] }` aligned by index for
programmatic consumers.

If the auditor prefers Rust (e.g., embedding the check into
their own audit pipeline), the same `mvm::vm::overlay` module
exports `verify_destruction_receipt`:

```rust
use mvm::vm::overlay::{verify_destruction_receipt, SignedDestructionReceipt};
use base64::Engine;
use ed25519_dalek::VerifyingKey;

let certs: Vec<SignedDestructionReceipt> =
    serde_json::from_str(&std::fs::read_to_string("certs.json")?)?;

let pubkey_b64 = std::fs::read_to_string("operator-pubkey.b64")?;
let pubkey_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
    .decode(pubkey_b64.trim())?;
let pubkey = VerifyingKey::from_bytes(&pubkey_bytes[..].try_into()?)?;

for cert in &certs {
    let receipt = verify_destruction_receipt(cert, Some(&pubkey))?;
    println!(
        "{}/{}: {} file(s) wiped, {} byte(s) wiped at {}",
        receipt.tenant,
        receipt.workload,
        receipt.files_wiped,
        receipt.bytes_wiped,
        receipt.destroyed_at,
    );
}
```

Both paths produce the same verdict — the CLI is a thin wrapper
over the same `verify_destruction_receipt` function.

### Two additional cross-reference signals

The audit chain carries two more event types the auditor can
consult independently of the per-workload
`lifecycle.tenant.destroyed` events that `--chain` matches
against:

```bash
$ mvmctl audit tail --chain --tenant local | \
    jq 'select(.entry.event | startswith("cmd.tenant"))'
{
  "entry": { "event": "cmd.tenant.invoked", ...,
             "labels": { "verb": "tenant", "pid": "12345" } }
}
{
  "entry": { "event": "cmd.tenant.completed", ...,
             "labels": { "verb": "tenant" } }
}
```

These are the `cmd.*` audit envelope from plan 60 Phase 4 —
every `mvmctl <verb>` invocation produces one `invoked` + one
`completed`/`failed` event. The auditor checks:

- That the `cmd.tenant.completed` timestamp brackets the per-
  workload `lifecycle.tenant.destroyed` timestamps.
- That `mvmctl audit verify --tenant local` succeeds — confirms
  the chain hasn't been tampered after the fact.

`mvmctl audit verify` walks the full chain and exits non-zero
on any signature or chain-link failure.

## What the certificate proves

| Property | Backed by |
|----------|-----------|
| The named tenant + workload existed on this host at the named time | Receipt fields |
| The named number of files were overwritten with zeros before unlink | `wipe_recursive`'s O_RDWR + fsync, files_wiped counter |
| The byte count is honest | bytes_wiped sums file lengths read post-zero-fill |
| The operator (not an attacker) signed it | Ed25519 over canonical payload, signer_pubkey matches operator's known pubkey |
| The certificate hasn't been tampered post-signing | Signature verification |

## What the certificate does NOT prove

These are documented limitations of Slice A's plain-filesystem
backend; Slice B (LUKS) closes them:

| Limitation | Closes in |
|------------|-----------|
| The disk's physical blocks may still hold the bytes (SSDs do block-level wear-leveling) | Slice B — LUKS keyslot revocation makes the bytes unrecoverable independent of the disk hardware |
| A concurrent process holding a file descriptor open across the destroy can still read pre-wipe bytes | Acceptable for the Slice A threat model (tenant teardown is a serialized operation); Slice E's rolling rebuild brings the pause/resume hooks that close this |
| Backups, snapshots, mirrored disks are not touched | Operator's external backup-deletion process; out of scope for `mvmctl tenant destroy` |
| Memory pages that held tenant data are not zeroed | Slice 6 (Plan 65 W6) zeros provider credentials on drop; tenant workload memory is wiped by FC's standard shutdown |

## Threat model recap

`mvmctl tenant destroy` is a **trusted-operator** operation — the
operator's host signs the certificate, so an auditor's trust in
the certificate is exactly their trust in the operator's pubkey.
This matches CLAUDE.md's existing model: "mvmctl trusts the host
with the hypervisor and private build keys." A malicious host
could fabricate a `SignedDestructionReceipt` whose `tenant` /
`workload` / `bytes_wiped` are all lies; that's an operator-
malfeasance threat, not a substrate-correctness threat.

For hosted-cloud deployments where the tenant doesn't trust the
operator, the certificate is one input among several — pair it
with attestation evidence (plan 60 Phase 6's `mvmctl attest
export`), independent backup-deletion logs, and the audit chain.

## Failure modes

| Failure | Behavior |
|---------|----------|
| `--confirm-deletion` not supplied | Exit 1 + error message; overlays untouched |
| `--tenant <id>` contains `/` or `..` | Exit 1; path-validator refuses |
| `$HOME` unset and `--overlay-root` not passed | Exit 1; clear error |
| Tenant has no overlays | Exit 0; stdout prints `[]`, stderr prints "(no overlays found)" |
| Host signer file missing or wrong mode | Exit 1; the `host_signer::load_or_init` refusal surfaces (operators chmod-fix and re-run) |
| One workload's destroy fails mid-stream | Exit 1; partial certificates printed to stdout via stderr trail (the `?` propagation aborts at the first failure — operator can retry with the remaining workloads) |

## Roadmap

- **Slice B** — LUKS-backed overlays. `bytes_wiped` becomes
  "LUKS keyslot revoked" rather than "zero-filled at the
  filesystem layer"; the disk hardware no longer matters.
- **Slice C** — overlays attached as virtio block devices on
  Firecracker / cloud-hypervisor. Today the overlay is a host-
  side directory; Slice C makes it the workload's actual
  writable layer.
- **Slice E** — rolling rebuild. `mvmctl install` swaps the
  rootfs underneath a running overlay; the workload's session
  reattaches.
- **Multi-host certificates** — if a tenant's overlay lives
  across mirrored hosts, the certificate format extends to
  carry a per-host signature so an auditor sees that every
  replica's bytes were destroyed.
