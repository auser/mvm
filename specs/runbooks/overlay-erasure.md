# Runbook - overlay erasure certificates

**Audience:** hosted-cloud operators deprovisioning a tenant through `mvmd`,
plus auditors who verify destruction certificates emitted by that control plane.

**Status:** Plan 60 Phase 7a shipped the overlay erasure substrate and
certificate verifier in `mvm`. Public tenant lifecycle commands now belong to
`mvmd`; `mvm` keeps the local primitives and the independent audit verifier.

---

## What it does

`mvm::vm::overlay::OverlayManager::destroy_overlay` walks every overlay for a
tenant/workload pair, zero-fills each file before unlinking it, removes the
directories, and returns a destruction receipt payload. The caller signs that
payload with `sign_destruction_receipt`.

The signed certificate is rooted in the host identity key at
`~/.mvm/keys/host-signer.ed25519` when the default local host signer is used.
An auditor with the operator's public key can verify the certificate
independently of the operator's host.

`mvmctl` no longer exposes a public tenant destroy command. Tenant lifecycle,
customer-facing deprovisioning, and rollout policy live in `mvmd`; `mvm`
exports the erasure and verification substrate those flows call into.

## Operator workflow

The operator-facing destroy flow is owned by `mvmd`. A conforming destroy flow
must:

1. Resolve the tenant and workload overlays that belong to the deprovisioned
   account.
2. Call the `mvm` overlay erasure primitive for each workload.
3. Sign each returned receipt with the host identity key.
4. Persist the signed certificates and emit an audit-chain entry containing the
   certificate fingerprint.
5. Hand the signed certificate JSON, the operator public key, and the relevant
   audit chain to the auditor.

The exact control-plane command and user confirmation UX should be documented in
`mvmd`, because that repository owns tenant lifecycle semantics.

## Auditor workflow

The auditor has three pieces of evidence:

1. The signed-certificate JSON (`certs.json`).
2. The operator's host identity pubkey (`operator-pubkey.b64`).
3. The operator's audit chain (`~/.mvm/audit/local.jsonl` or an exported chain).

Each piece feeds an independent verification axis:

| Axis | What it checks | What it catches |
|------|----------------|-----------------|
| Signature | Ed25519 over canonical payload | Receipt-field tampering |
| Pubkey pin | Embedded `signer_pubkey` matches operator's known key | Forgery under attacker key |
| Chain anchor | SHA-256 fingerprint in the lifecycle destruction event | Forging without chain anchor; cert swap post-emission |

### One-command three-axis verification

`mvm` still provides the audit verifier:

```bash
$ mvmctl audit verify-cert certs.json \
      --pubkey operator-pubkey.b64 \
      --chain operator-audit-chain.jsonl
mvmctl audit verify-cert: 3 certificate(s) verified
  ✓ acme/build-runner: 42 file(s), 1048576 byte(s) wiped at 2026-05-11T18:00:00Z [chain ✓]
  ✓ acme/code-eval: 8 file(s), 524288 byte(s) wiped at 2026-05-11T18:00:01Z [chain ✓]
  ✓ acme/test-runner: 17 file(s), 65536 byte(s) wiped at 2026-05-11T18:00:02Z [chain ✓]
```

Per-certificate markers tell the auditor which axis fired:

- `[chain ✓]` - fingerprint matches an audit-chain entry.
- `[chain ✗ MISSING ENTRY]` - cert claims a destruction the chain does not
  witness; suspect forged cert.
- `[chain ✗ FINGERPRINT MISMATCH]` - chain has an entry for this tenant/workload
  but the fingerprint differs; operator swapped a cert after the chain was
  written.

The command exits non-zero if any chain check fails. Skipping `--chain` exits
zero when signature and pubkey checks pass, which is useful when the auditor
does not have access to the operator's chain file.

### Other failure modes the command surfaces

- `SignatureInvalid` - any receipt field was tampered after signing.
- `PubkeyMismatch` - the cert's embedded `signer_pubkey` does not match
  `--pubkey`.
- `UnsupportedVersion` - the verifier has not been updated for a future
  certificate version.
- Parse errors on the cert or pubkey file.

All of these exit non-zero with context.

### Reading the chain manually

If the auditor wants to inspect chain entries directly, `mvmctl audit tail
--chain` walks the chain file:

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
    }
  }
}
```

Recomputing the fingerprint from the cert file confirms a match:

```bash
$ jq -c '.[0]' certs.json | shasum -a 256
8a3f2c91... -
```

`mvmctl audit verify-cert --chain` performs this comparison automatically; the
manual path is for diagnostics.

### Pipe + stdin

The cert source supports `-` so an auditor can pipe:

```bash
$ cat certs.json | mvmctl audit verify-cert - \
      --pubkey operator-pubkey.b64 \
      --chain operator-audit-chain.jsonl
```

JSON output (`--json`) emits `{ "receipts": [...], "chain_matches": [...] }`
aligned by index for programmatic consumers.

The same verifier is available from Rust:

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

Both paths produce the same verdict; the CLI is a thin wrapper over
`verify_destruction_receipt`.

## What the certificate proves

| Property | Backed by |
|----------|-----------|
| The named tenant + workload existed on this host at the named time | Receipt fields |
| The named number of files were overwritten with zeros before unlink | `wipe_recursive`'s `O_RDWR` + fsync, files_wiped counter |
| The byte count is honest | `bytes_wiped` sums file lengths read post-zero-fill |
| The operator signed it | Ed25519 over canonical payload, signer_pubkey matches the known operator pubkey |
| The certificate has not been tampered post-signing | Signature verification |

## What the certificate does not prove

| Limitation | Closes in |
|------------|-----------|
| Physical disk blocks may still hold bytes because SSDs do block-level wear-leveling | LUKS keyslot revocation makes bytes unrecoverable independent of disk hardware |
| A concurrent process holding a file descriptor open across destroy can still read pre-wipe bytes | Tenant teardown must be serialized by the owning control plane |
| Backups, snapshots, and mirrored disks are not touched | Operator's external backup-deletion process |
| Memory pages that held tenant data are not zeroed by overlay erasure | Workload memory is wiped by the VM shutdown path |

## Threat model recap

Overlay erasure certificates are **trusted-operator** evidence. The operator's
host signs the certificate, so the auditor's trust in the certificate is their
trust in the operator's public key and audit chain. A malicious host can
fabricate a signed receipt whose tenant, workload, and byte counts are false;
that is operator malfeasance, not a substrate-correctness failure.
