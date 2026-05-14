# Plan 68 — Hermetic live coverage for `build → TemplateBuild`

> `mvmctl build` runs `nix build` against a flake — either via host Nix
> or `LibkrunBuilderVm`. Either way the live test needs a real
> Nix install or a running builder VM, which isn't tractable on a
> generic CI runner. Plan 68 adds a `MVM_BUILD_STUB_OUTDIR` env-var
> escape hatch that bypasses the build and uses a caller-supplied
> directory as if it were the Nix-build output, then pins the
> `TemplateBuild` Emits row end-to-end.
>
> Roughly 1 day, 2 workstreams.

**Status (2026-05-12)**: not started. No substrate dependencies beyond
PR #106 (`audit_emit!`). ADR-045 architectural decision.

## Context

`crates/mvm-cli/src/commands/build/build.rs` flow:

```
mvmctl build --flake . --profile minimal --role worker
  ├── resolve --flake to a flake ref
  ├── dev_build(env, flake_ref, profile, mode)
  │     ├── (host Nix path)        → nix build … --print-out-paths
  │     └── (libkrun path)    → spawn builder VM, copy artifacts
  ├── parse build output → DevBuildResult { rootfs_path, … }
  ├── (optional) register slot in manifest registry
  └── audit_emit!(TemplateBuild, vm: revision_hash, "flake_ref={…} profile={…}")
```

Every step except the audit emit reaches external tooling. A stub
env-var that says "skip the build; use this pre-existing directory
as the output" makes the rest of the path testable.

Precedent: `MVM_DIRECT_BOOT` already takes this shape for the `up`
verb (see PR #108). The pattern is established: env-var escape
hatch, off by default, only the launchd-spawned / hermetic-test
paths set it.

## State of play

### Already in `origin/main`

- `dev_build` orchestrator + the `LibkrunBuilderVm` /
  host-Nix branches.
- `audit_emit!` in `build.rs` (verify path — confirm location).
- `AuditSandbox` fixture.

### Missing (integration)

1. No env-var or flag that skips the build for hermetic tests.
2. No live test for `TemplateBuild`.

## Workstreams

### W1 — `MVM_BUILD_STUB_OUTDIR` escape hatch (~½ day)

**Goal**: when set, `dev_build` skips invocation of Nix / the
builder VM and treats the env-var value as the build output
directory. The directory must contain at least `rootfs.ext4` and
`vmlinux` (matching what `LibkrunBuilderVm` extracts).

**Action**:

- In `crates/mvm-build/src/pipeline/dev_build.rs::dev_build`, add a
  front-of-function check:
  ```rust
  if let Ok(stub) = std::env::var("MVM_BUILD_STUB_OUTDIR")
      && !stub.trim().is_empty()
  {
      env.log_warn("MVM_BUILD_STUB_OUTDIR set; skipping nix build (test path).");
      let stub_path = std::path::PathBuf::from(stub);
      let revision_hash = stub_path
          .file_name()
          .and_then(|s| s.to_str())
          .map(String::from)
          .unwrap_or_else(|| "stub".to_string());
      return Ok(DevBuildResult {
          build_dir: stub_path.display().to_string(),
          vmlinux_path: stub_path.join("vmlinux").display().to_string(),
          rootfs_path: stub_path.join("rootfs.ext4").display().to_string(),
          initrd_path: None,
          revision_hash,
          cached: false,
          runner_dir: None,
          artifact_sizes: ArtifactSizes::default(),
      });
  }
  ```
- The stub path is *not* validated for shape; that's the caller's
  responsibility. A non-test caller setting this env-var is misuse.
- Log a `ui::warn` so the bypass is visible in stderr.
- Unit test in `dev_build.rs::tests` driving the env-var path.

**Exit tests**:

- `dev_build_with_stub_outdir_returns_caller_supplied_paths`.
- `dev_build_with_stub_outdir_logs_warning`.
- `dev_build_with_empty_stub_outdir_does_not_bypass` (defensive).

### W2 — Live test for `mvmctl build` (~½ day)

**Goal**: end-to-end exercise of `mvmctl build --flake .` against the
stub directory; pin `TemplateBuild` emit.

**Action**:

- `tests/audit_emissions_live.rs`:
  ```rust
  #[test]
  fn build_emits_template_build_audit_entry() {
      let sandbox = AuditSandbox::new();
      let stub = sandbox.home_path().join("build-output");
      std::fs::create_dir_all(&stub).expect("mkdir stub");
      std::fs::write(stub.join("vmlinux"), b"fake-kernel").expect("write kernel");
      std::fs::write(stub.join("rootfs.ext4"), b"fake-rootfs").expect("write rootfs");

      let flake_dir = sandbox.home_path().join("flake");
      std::fs::create_dir_all(&flake_dir).expect("mkdir flake");
      std::fs::write(flake_dir.join("flake.nix"), b"# stub").expect("write flake");

      let output = sandbox.mvmctl()
          .env("MVM_BUILD_STUB_OUTDIR", &stub)
          .args(["build", "--flake", flake_dir.to_str().unwrap(),
                 "--profile", "minimal"])
          .output().expect("spawn");
      assert!(output.status.success(), ...);

      let log = read_audit_log(&sandbox.audit_log_path());
      assert!(count_entries_with_kind(&log, "template_build") >= 1);
  }
  ```
- Module-doc coverage list update.

**Exit criteria**:

- Test passes.
- xtask lints clean.

## Phasing

W1 → W2 in a single PR (~150 lines total).

## Non-goals

- **Real Nix coverage.** The libkrun-builder + host-Nix paths
  are tested by `tests/smoke_e2e_boot.rs`; plan 68 only handles the
  audit emit + the orchestrator's env-var escape.
- **Multi-profile builds.** The test exercises one profile; coverage
  for `--profile worker` vs `minimal` is separate.

## Success criteria

By plan 68 close:

1. `MVM_BUILD_STUB_OUTDIR` env-var bypasses the build in
   `dev_build`.
2. `TemplateBuild` row live-pinned via the stub path.
3. xtask lints clean.

## Risk notes

- **Stub-path detection in production.** A misconfigured prod user
  setting this env-var would silently skip every build. Mitigation:
  the `ui::warn` log on bypass is unconditional; CI also asserts
  the env-var is unset on production CI runs (separate gate, not
  in plan 68 scope but worth noting in ADR-045).
