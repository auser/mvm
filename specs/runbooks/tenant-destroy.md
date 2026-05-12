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
3. The chain-signed `cmd.tenant.*` audit entries in the
   operator's `~/.mvm/audit/local.jsonl` (optional but
   recommended — provides an anchor that's hard to forge
   retroactively).

### Verifying with the supplied pubkey

A Rust verifier (the operator runs `mvm` already, but a third
party can lift `mvm::vm::overlay` into their own check):

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

`verify_destruction_receipt` returns `Err(SignatureInvalid)` if
*any* field of the embedded receipt was tampered after signing
— tenant rename, files_wiped padding, timestamp shift, all
fail. It also surfaces `PubkeyMismatch` if the certificate's
embedded `signer_pubkey` doesn't match the operator's claimed
pubkey, and `UnsupportedVersion` for future v2+ certificates the
auditor's verifier hasn't been updated to parse.

### Cross-referencing the audit chain

The audit chain entry the operator emitted at the time of the
destroy gives a second anchor:

```bash
$ mvmctl audit show --tenant local | jq 'select(.event | startswith("cmd.tenant"))'
{
  "event": "cmd.tenant.invoked",
  "timestamp": "2026-05-11T18:00:00Z",
  ...
}
{
  "event": "cmd.tenant.completed",
  "timestamp": "2026-05-11T18:00:01Z",
  ...
}
```

Both entries are chain-signed; `mvmctl audit verify` walks the
full chain and exits nonzero on any tamper. Cross-referencing
`cmd.tenant.completed`'s timestamp against the certificate's
`destroyed_at` gives the auditor a check independent of the
certificate's own signature.

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
