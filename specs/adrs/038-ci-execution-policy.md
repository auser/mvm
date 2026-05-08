---
title: "ADR-038: CI execution policy — push runs CI; release runs everything else; githooks run tests on commit"
status: Proposed
date: 2026-05-07
related: ADR-033 (code-quality enforcement), plan 60-mvm-microsandbox-migration
---

## Status

Proposed. Implementation: workflow trigger updates land in Phase 0; githook upgrades follow.

## Context

The previous iteration's CI workflows triggered on `pull_request` against `main` plus tag pushes. That was an "ecosystem-wide CI cost reduction" move — but it has two side-effects we want to fix:

1. Pushes to feature branches with no PR open get **no CI feedback at all**, which delays surfacing regressions until PR creation.
2. Heavy gates (security audit, Windows lane, reproducibility) run on every PR push, doubling effort for trivial changes.

The user already shipped a pre-commit hook (`.githooks/pre-commit`) from the previous repo. The hook is light by design (cargo fmt + nix fmt only) because Linux-specific deps don't compile cleanly on the macOS host. The intent is:

- **Local**: pre-commit runs format + (planned) test set on the dev's machine via Lima/Linux.
- **Remote on push**: a single, focused CI workflow runs on every push to give feedback on feature branches.
- **Remote on release**: heavier gates (security audit, full Windows lane, cross-platform matrix, reproducibility check, release artifact build) run when a tag is pushed.

This split mirrors what the user wants: "only run the CI github action when we push — the rest when we do a release."

## Decision

### Triggers

| Workflow | Trigger | Rationale |
|---|---|---|
| `ci.yml` | `push:` (any branch) + `workflow_dispatch:` | Fast feedback on every push; no PR gating delay |
| `security.yml` | `push: tags: ["v*"]` + nightly cron + `workflow_dispatch:` | Security audit is heavy; run at release + nightly catches new CVEs |
| `windows.yml` | `push: tags: ["v*"]` + `workflow_dispatch:` | Windows lane is expensive; release-time backstop only |
| `release.yml` | `push: tags: ["v*"]` | Builds release artifacts |
| `publish-crates.yml` | `release: types: [published]` + `workflow_dispatch:` | Publishes to crates.io on GitHub release |
| `pages.yml` | `workflow_dispatch:` | Manual docs publish |

`pull_request:` triggers are **dropped from `ci.yml`, `security.yml`, `windows.yml`** because they run on the source branch's push instead. Forks needing PR-time runs use `workflow_dispatch` (operator escape hatch).

### Concurrency

Each workflow keeps its `concurrency.group: ${{ github.workflow }}-${{ github.ref }}` with `cancel-in-progress: true` so superseded SHAs don't pile up.

### Pre-commit hook (`.githooks/pre-commit`)

Stays light at first: `cargo fmt --all` + `nix fmt` on staged files (current behaviour). **Phase 1 upgrades** to also run, when a Lima dev VM is available:

- `limactl shell mvm-builder -- cargo clippy --workspace --all-targets -- -D warnings`
- `limactl shell mvm-builder -- cargo nextest run --workspace`

If Lima isn't available, the hook prints a hint and skips silently — host-side compile fails for Linux-only deps.

### `just ci` recipe

A single `just ci` target reproduces what the on-push CI does, so devs can sanity-check before pushing. Phase 0 ships a stub; Phase 1 fills it out.

## Consequences

**Positive**:
- Feature-branch developers get CI feedback on every push.
- Release-time gates aren't paid on every PR.
- Pre-commit + on-push CI + release CI form three checkpoints with clear responsibilities.
- Forks still have a path (`workflow_dispatch`).

**Negative**:
- `push:` (any branch) means `ci.yml` runs even on draft / experimental branches. Mitigated by concurrency cancellation: only the latest SHA matters.
- Loss of pre-merge guarantee: a PR may merge without CI having run if the source branch was pushed before the PR was opened. Mitigated by branch-protection requiring `ci.yml` to be a required check.

**Neutral**:
- Existing workflows keep their structure; only triggers change.

## Alternatives considered

- **Keep `pull_request` triggers**: rejected. Doubles CI cost on PR pushes for the same SHA.
- **`push:` to main only + `pull_request:`**: rejected. Drops feature-branch feedback entirely.
- **Run everything on every push**: rejected. Cost-prohibitive (Windows runners are 2× Linux; the security audit lane is slow).

## Threat model impact

- The release gates (security audit, reproducibility, signing) all still run before any artifact ships. No regression.
- A subverted release would have to bypass the `release.yml` + `publish-crates.yml` chain, which is signed and gated.

## Compliance impact

- SOC 2: positive — separation of dev-time vs. release-time controls is a documented control distinction.
- All others: neutral.
