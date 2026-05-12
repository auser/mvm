# Catalog & schema versioning — parked research notes

Status: parked research note. No commitment, no sprint item.

## Inspiration source

While studying an open-source Rust "context graph" project — a graph
database that applies git-style workflows (branch, merge, snapshot) to
typed, schema-validated data — two design choices stood out as
potentially relevant to mvm. The project's stack itself (Apache Arrow
columnar memory, Lance versioned immutable snapshots, DataFusion query,
Cedar server-side ACLs, S3-native storage) is out of scope for this
repo; what survived the filter were two narrow ideas captured below.

## What's not transferable

mvm has no data plane. Templates and catalog entries are tens of small
JSON records, not millions of typed graph nodes. Adopting
Arrow/Lance/DataFusion or vector/BM25 search would be cargo-cult. Cedar
policies *could* matter for `mvmd` (fleet, multi-tenant) but not for
this repo. Schema-as-code with strict parsing is already done —
`#[serde(deny_unknown_fields)]` is enforced on every host↔guest type
(security claim 5 in `CLAUDE.md`), and per-domain `schema_version`
fields exist on the major schemas.

Two ideas survived the filter and are recorded below as future
considerations.

## Idea A: content-addressed catalog entries

**Today.** `crates/mvm-core/src/catalog.rs` keys entries by
human-friendly name ("minimal", "postgres") — name → flake ref +
profile/role + sizing. There is no entry-level content hash.

**Idea.** Hash each catalog entry's resolved spec (locked flake ref,
profile, role, sizing) and expose it as an opaque content ID. `mvmctl
image fetch <hash>` would then reproduce a known-good entry even if the
catalog itself drifts. The idea borrows the "content hash is the
version" framing common in immutable-snapshot data formats.

**Why parked.** Nix store hashes already give artifact-level
reproducibility. Entry-level drift (someone edits the catalog, silently
changing what `image fetch minimal` produces) is real but rare today,
and `SignedManifest` records the input closure (Nix store hash, source
git SHA, lockfile hashes) which already detects most variants of this
concern at fetch time.

**Trigger to revisit.** Someone hits a bug — or a security finding —
where a catalog refresh silently changed what `image fetch <name>`
produces, and the input-closure manifest doesn't catch it.

## Idea C: migration-tested schema versioning

**Today.** `schema_version` fields exist on `Catalog`, `TemplateSpec`,
`Manifest`, `SignedManifest`, and `RuntimeConfig` (see
`crates/mvm-guest/src/runtime_config.rs` for the `deny_unknown_fields`
exemplar). Strict parsing is enforced. No fixture-based migration tests
exist — there has been no v2 of anything yet, so the question hasn't
come up.

**Idea.** Per versioned schema, add a tiny fixture test that asserts
forward/backward compatibility rules: a v1 record loads cleanly into a
v2 reader, a v2 record either loads into a v1 reader (with documented
field loss) or is rejected with a clear error. Catches a class of bugs
that strict parsing alone doesn't.

**Why parked.** Everything is v1 today. Writing migration tests now
would be a tax on hypothetical future work, not a fix for present pain.
The existing `deny_unknown_fields` posture means v1 is genuinely safe
against silent corruption.

**Trigger to revisit.** First time we bump any `schema_version` to 2 —
that's the moment to backfill fixture tests for both v1 and v2, and to
make "ship a version bump with a migration test" a workflow norm.

## Idea B: considered and skipped

A DAG-shaped template lineage (each revision records a
`parent_revision`, enabling future branch/merge UX over template
families) was considered and skipped. Templates aren't collaboratively
edited the way code is, and the existing
`~/.mvm/templates/<id>/artifacts/<revision>/` + `current` symlink model
handles point-in-time revisions adequately. Recorded here so future-us
doesn't re-derive the same idea from scratch and re-conclude the same
way.

## Critical files referenced

- `crates/mvm-core/src/catalog.rs` — catalog entry shape,
  `schema_version` field
- `crates/mvm-core/src/domain/template.rs` — `TemplateSpec`, revision
  model
- `crates/mvm-security/src/image_verify.rs` — `SignedManifest` + input
  closure tracking, the existing reference for "do versioning
  rigorously"
- `crates/mvm-guest/src/runtime_config.rs` — `deny_unknown_fields` +
  `schema_version` exemplar
