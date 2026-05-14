# `specs/claims/` — public claim gating files

Each file under this directory records the lifecycle status of one
public security or capability claim. Files are consumed by
`xtask check-no-overclaim`, which scans repo text (README, public
docs, code comments, CLI help) for "guarded phrases" associated
with a claim and refuses to admit those phrases when the claim's
status is anything other than `Shipped`.

The intent is to prevent the docs/website/README from saying "we do
X" before the CI gates that prove X actually pass. Plan 74 W0 and
plan 75 W0 introduce this pattern; both plans use it to gate the
OCI ingest, network policy, and other surface that's not yet
production.

## File format

```markdown
---
claim: <kebab-case-id>
status: Planned | Preview | Shipped | Not-claimed
gated_phrases:
  - "phrase to refuse outside this file"
  - "another phrase"
exempt_paths:
  - "specs/**"
  - "CHANGELOG.md"
---

# Claim <N> — <human title>

<description of the claim, what it asserts, what CI gate ratifies it>
```

Fields:

- `claim` — stable identifier. Used in error messages.
- `status` — see below.
- `gated_phrases` — list of substrings to refuse outside this
  claim file (and any path in `exempt_paths`). Case-sensitive.
- `exempt_paths` — glob list of paths where the phrases are
  always allowed (this file, history, etc.). `specs/**` is the
  default exemption for design docs.

## Status semantics

- `Planned` — claim is on the roadmap; phrases blocked everywhere except `exempt_paths`.
- `Preview` — claim partially shipped; phrases blocked in user-facing surface (README, public docs, landing page, CLI help) but admitted in design docs and changelog entries.
- `Shipped` — claim has CI proof; phrases admitted everywhere.
- `Not-claimed` — claim is explicitly out of scope; phrases blocked everywhere.

Bumping status is a deliberate PR; it's how a claim transitions from "we plan to do this" to "we say in public that we do this."
