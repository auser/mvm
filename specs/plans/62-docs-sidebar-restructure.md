# Plan 62 — Docs sidebar restructure with placeholder pages

Status: **Ready to implement.** Self-contained: another session can pick
this up cold without prior conversation context. The work is purely in
`public/` — it does not touch crates, CLI, or any runtime code, so it
can run in parallel with the plan-60 microsandbox migration without
merge risk.

## Why

A gap analysis against the dominant sandbox-as-a-service category leader
(generic, per repo memory: do **not** name it in any committed file)
showed the product site is missing entire navigation surfaces that
prospective users — agent developers, CI users, AI-app builders —
expect to land on. Specifically the category leader's docs lead with
**Quickstart → Sandbox → Code Execution → Filesystem → Commands →
Network → Templates → Examples → SDK Reference**, while ours lead with
**Getting Started → Install → Guides → Reference**, with the Templates
section dropped entirely (note in `public/astro.config.mjs:59-61`).

This plan creates the *structural* parity — sidebar tree + placeholder
pages with frontmatter and a one-paragraph TODO body — so subsequent
content sessions can fill pages in independently. **No content writing
is in scope here.** Each placeholder has a `draft: true` frontmatter
flag and a `<!-- TODO -->` block describing the intended content.

## Constraints

- Do not name the category-leader competitor anywhere in committed
  files (text or filename). Refer generically: "sandbox-as-a-service
  products," "AI agent sandbox category," etc.
- Keep all changes inside `public/`. No crate edits.
- Do not break existing URLs. Existing pages stay where they are; new
  pages are added alongside. The sidebar is reorganized but all current
  slugs continue to resolve.
- Markdown frontmatter must validate against the Starlight schema in
  `public/src/content.config.ts` — set `draft: true` and a short
  `description` on every stub.

## Deliverables

1. **17 new placeholder pages** (paths and frontmatter listed below).
2. **Updated `public/astro.config.mjs`** with a reorganized sidebar.
3. **No content** beyond a TODO block on each new page.

## File operations

### New directories

```
public/src/content/docs/working/
public/src/content/docs/templates/
public/src/content/docs/examples/
```

`security/` already exists and gets new siblings under it.

### New pages — 17 stubs

Use this exact frontmatter shape for every stub. Fill `title` and
`description` from the table below; everything else is identical.

```markdown
---
title: <Title>
description: <One-line description from table>
draft: true
---

<!--
TODO: This page is a placeholder created in plan 62 for sidebar
parity. Intended content brief:

<copy the "Brief" cell from the table verbatim>

Cross-references:
<copy any "See" cell entries from the table>
-->

> This page is a placeholder. Content is being written — see plan 62.
```

| Slug | Title | Description | Brief | See |
|---|---|---|---|---|
| `getting-started/connect-an-llm` | Connect an LLM | Wire an LLM tool-call to an mvm sandbox | Show a minimal Python/TS snippet that hands an LLM tool-call to `mvmctl exec` and streams output back. Generic across providers. Sets the AI-agent framing on the Getting Started path. | `guides/exec.md`, ADR-007 (function-call entrypoints) |
| `working/index` | Working in the MicroVM | Overview of in-VM operations | Landing page for the new section. One-paragraph intro + cards/links to the 5 pages below. | — |
| `working/commands` | Run commands & processes | Run shell commands in a microVM, stream output, manage background processes | Document `mvmctl vm proc` workflows: one-shot exec, streaming stdout/stderr, background processes, sending stdin. Pull examples from `crates/mvm-cli/src/commands/vm/proc.rs`. | `guides/exec.md`, `crates/mvm-cli/src/commands/vm/proc.rs`, `crates/mvm-guest/src/process_rpc.rs` |
| `working/filesystem` | Filesystem operations | Upload, download, and watch files in a microVM | Host↔guest file ops via `mvmctl vm fs`. Distinct from `reference/filesystem.md` (which is drives/ext4 architecture). | `crates/mvm-cli/src/commands/vm/fs.rs`, `crates/mvm-guest/src/fs_rpc.rs`, `reference/filesystem.md` |
| `working/network` | Network & exposing ports | Internet access, port forwarding, exposing services | User-facing networking workflow: how to expose a port from the guest, how egress is controlled, when to use TAP vs vsock. Split from `guides/networking.md` (architectural detail stays there). | `guides/networking.md`, `crates/mvm-runtime/src/vm/network.rs` |
| `working/persistence` | Persistence, pause & resume | Save and restore microVM state across sessions | Pause/resume semantics, what persists, what doesn't. Reference instance snapshots. | `crates/mvm-runtime/src/vm/instance_snapshot.rs`, `crates/mvm-cli/src/commands/vm/pause.rs` |
| `working/snapshots` | Snapshots | Capture and restore microVM state | Snapshot creation, storage, restore semantics, HMAC integrity. | `crates/mvm-security/src/snapshot_hmac.rs`, plan 51 (session lifecycle) |
| `templates/index` | Templates | Reusable microVM blueprints | Section landing — what a template is, why use one, link to create/build/lifecycle pages. | `crates/mvm-runtime/src/vm/template/`, plan 38 (manifest-driven template DX) |
| `templates/create` | Create a template | Define a reusable microVM template from a Nix flake | Walk through `mvmctl template create` end-to-end. | `crates/mvm-runtime/src/vm/template/lifecycle.rs` |
| `templates/build` | Build & list templates | Build, list, and inspect templates | `mvmctl template build`, `template list`. | — |
| `templates/lifecycle` | Template lifecycle | Update, version, and remove templates | Lifecycle ops, versioning, garbage collection. | `crates/mvm-runtime/src/vm/template/lifecycle.rs` |
| `examples/index` | Examples | Real workflows built on mvm | Section landing with cards. | — |
| `examples/ai-agent-sandbox` | Sandbox for an AI agent | Run an AI agent's tool calls inside a microVM | End-to-end: agent-driven shell exec, file writes, network policy. Lean on the AI-agent workload framing in plan 60 §"Product positioning". | plan 60, ADR-007 |
| `examples/ci-cd-ephemeral-builder` | CI/CD ephemeral builder | Use mvm as a per-job clean builder in CI | Lean on `guides/verify-release.md`. | `guides/verify-release.md` |
| `examples/dev-vm-from-flake` | Reproducible dev VM from a Nix flake | Build a developer VM from a flake and use it | Showcase the Nix-flake differentiator. | `guides/nix-flakes.md`, `guides/dev-image.md` |
| `examples/code-interpreter` | Code interpreter pattern | Stateful Python/JS execution inside a microVM | Pattern for kernel-style stateful exec, streaming output. Generic — no vendor names. | plan 41 (function-call entrypoints) |
| `security/threat-model` | Threat model | Adversaries, assumptions, and out-of-scope concerns | Distill ADR-002. | `specs/adrs/002-microvm-security-posture.md` |
| `security/ci-claims` | Seven CI-enforced security claims | The seven security claims and their CI gates | Restate from `CLAUDE.md` "Security model" section. Each claim → CI job that enforces it. | CLAUDE.md, `.github/workflows/ci.yml`, `.github/workflows/security.yml` |
| `security/verified-boot` | Verified boot | dm-verity-sealed rootfs and the W3 lane | Distill `specs/runbooks/w3-verified-boot.md` and plan 27. | plan 27, `specs/runbooks/w3-verified-boot.md` |
| `reference/programmatic-use` | Programmatic use | Drive `mvmctl` from scripts and CI | Document JSON output flags, exit codes, environment variables. Cheapest substitute for a real SDK reference until one ships. | `crates/mvm-cli/` |
| `reference/limits` | Limits & resources | CPU, memory, disk, and network limits | Practical limits per backend; host requirements. | `crates/mvm-core/src/metering.rs`, `crates/mvm-supervisor/src/instance_sampler.rs` |

That's 21 stubs total (the table shows 21 rows; "17" in the section
header was a draft estimate — the implementer should produce all 21 as
listed). Three of them (`working/index`, `templates/index`,
`examples/index`) are section landings.

### Sidebar — replace the `sidebar` array in `public/astro.config.mjs`

Replace the `sidebar: [ … ]` block (currently lines 40–108) with this
exact structure. Order matters: it determines the rendered order.

```js
sidebar: [
  {
    label: "Getting Started",
    items: [
      { label: "Installation", slug: "getting-started/installation" },
      { label: "Quick Start", slug: "getting-started/quickstart" },
      { label: "Your First MicroVM", slug: "getting-started/first-microvm" },
      { label: "Connect an LLM", slug: "getting-started/connect-an-llm" },
      { label: "Nix for mvm", slug: "getting-started/nix-for-mvm" },
    ],
  },
  {
    label: "Install",
    items: [
      { label: "Linux", slug: "install/linux" },
      { label: "macOS", slug: "install/macos" },
      { label: "Windows (WSL2)", slug: "install/windows" },
    ],
  },
  {
    label: "Working in the MicroVM",
    items: [
      { label: "Overview", slug: "working" },
      { label: "Run commands & processes", slug: "working/commands" },
      { label: "Filesystem operations", slug: "working/filesystem" },
      { label: "Network & exposing ports", slug: "working/network" },
      { label: "Persistence, pause & resume", slug: "working/persistence" },
      { label: "Snapshots", slug: "working/snapshots" },
    ],
  },
  {
    label: "Templates",
    items: [
      { label: "Overview", slug: "templates" },
      { label: "Create a template", slug: "templates/create" },
      { label: "Build & list", slug: "templates/build" },
      { label: "Lifecycle", slug: "templates/lifecycle" },
    ],
  },
  {
    label: "Guides",
    items: [
      { label: "Writing Nix Flakes", slug: "guides/nix-flakes" },
      { label: "Building MicroVM Images", slug: "guides/building-microvm-images" },
      { label: "Sandboxed Exec", slug: "guides/exec" },
      { label: "Config & Secrets", slug: "guides/config-secrets" },
      { label: "Manifests", slug: "guides/manifests" },
      { label: "Networking", slug: "guides/networking" },
      { label: "Dev Image", slug: "guides/dev-image" },
      { label: "Verify Release", slug: "guides/verify-release" },
      { label: "Airgapped Bootstrap", slug: "guides/airgapped-bootstrap" },
      { label: "Troubleshooting", slug: "guides/troubleshooting" },
      { label: "Windows: WSL2 walkthrough", slug: "guides/windows-wsl2" },
      { label: "Windows: troubleshooting", slug: "guides/windows-troubleshooting" },
    ],
  },
  {
    label: "Examples",
    items: [
      { label: "Overview", slug: "examples" },
      { label: "Sandbox for an AI agent", slug: "examples/ai-agent-sandbox" },
      { label: "CI/CD ephemeral builder", slug: "examples/ci-cd-ephemeral-builder" },
      { label: "Reproducible dev VM from a flake", slug: "examples/dev-vm-from-flake" },
      { label: "Code interpreter pattern", slug: "examples/code-interpreter" },
    ],
  },
  {
    label: "Security",
    items: [
      { label: "Matryoshka Model", slug: "security/matryoshka" },
      { label: "Threat model", slug: "security/threat-model" },
      { label: "Seven CI claims", slug: "security/ci-claims" },
      { label: "Verified boot", slug: "security/verified-boot" },
    ],
  },
  {
    label: "Deploy",
    items: [
      { label: "AWS EC2", slug: "deploy/aws" },
      { label: "Ubicloud", slug: "deploy/ubicloud" },
    ],
  },
  {
    label: "Reference",
    items: [
      { label: "CLI Commands", slug: "reference/cli-commands" },
      { label: "Programmatic Use", slug: "reference/programmatic-use" },
      { label: "Architecture", slug: "reference/architecture" },
      { label: "Filesystem & Drives", slug: "reference/filesystem" },
      { label: "Guest Agent", slug: "reference/guest-agent" },
      { label: "Limits & Resources", slug: "reference/limits" },
    ],
  },
  {
    label: "Contributing",
    items: [
      { label: "Development Guide", slug: "contributing/development" },
      { label: "ADR-001: Multi-Backend VMs", slug: "contributing/adr/001-multi-backend" },
      { label: "ADR-013: microsandbox + libkrun + microvm.nix", slug: "contributing/adr/013-microsandbox-pivot" },
    ],
  },
],
```

Notes for the implementer:
- The stale comment at lines 58–62 about `guides/templates` should be
  removed since Templates is now a top-level section.
- `working` and `templates` and `examples` use bare slugs (no
  `/index`) for the section overview pages — Starlight resolves a
  bare-directory slug to the directory's `index.md` automatically. The
  files on disk should be named `working/index.md` etc.

## Acceptance criteria

The implementer is done when **all** of the following are true:

1. `cd public && pnpm build` succeeds with zero warnings about missing
   pages or broken links.
2. `cd public && just docs-dev` (or `pnpm dev`) renders the new
   sidebar in the order shown above.
3. Every new slug in the sidebar resolves to a page that renders
   without errors. Each rendered placeholder shows the
   "This page is a placeholder" notice.
4. Every existing slug from the previous sidebar still resolves —
   nothing was deleted or renamed in this plan.
5. The committed pages and this plan contain zero references to the
   competitor product name. Per the strict memory directive, that
   vendor name must not appear in any file under `public/src/` or in
   `specs/plans/62-*`. Verify with the appropriate grep before
   committing.
6. `cargo test --workspace` and `cargo clippy --workspace -- -D
   warnings` are unaffected (this plan does not touch Rust code, so a
   no-op verification is sufficient).

## Out of scope (do **not** do in this plan)

- Writing actual content for any of the 21 placeholder pages.
- Deleting, moving, or renaming any existing page. (Future plan: fold
  `guides/exec.md` into `working/commands.md` once the latter has real
  content. Not now — would break links.)
- Adding redirects.
- Touching the landing page (`index.mdx`, `Hero.astro`, `Landing.tsx`).
- Adding an SDK. The "Programmatic Use" stub is the substitute until a
  real SDK ships.
- Anything in the `crates/` tree.

## Hand-off note for the parallel migration session

This plan is intentionally orthogonal to plan 60
(microsandbox/libkrun migration). The only file overlap risk is
`public/astro.config.mjs`, which plan 60 does not touch. If plan 60
adds new ADRs to `public/src/content/docs/contributing/adr/`, the
Contributing section in this sidebar will need those entries appended —
that's a one-line append and not a structural change.
