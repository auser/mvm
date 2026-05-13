# Plan 69 — Hermetic live coverage for `update → UpdateInstall`

> `mvmctl update` (and `update --check`) reaches `github.com/<org>/mvm`
> over HTTPS on every invocation. CI runners are flaky w.r.t. network
> access and rate-limited against GitHub; hermetic testing needs a
> mock HTTP server in front of the release URL. Plan 69 uses
> `httpmock` (or equivalent) to stand up a local server, redirects
> mvmctl's release-URL base via an env-var override, and pins the
> `UpdateInstall` Emits row.
>
> Roughly 1 day, 2 workstreams.

**Status (2026-05-12)**: not started. No substrate dependencies beyond
PR #106 (`audit_emit!`). ADR-045 architectural decision.

## Context

`crates/mvm-cli/src/update.rs::update` flow:

```
mvmctl update
  ├── reqwest::blocking::get(GH_RELEASES_LATEST_URL)
  │     ← JSON release metadata
  ├── parse latest version + asset URLs
  ├── (--check mode): print + exit
  ├── download mvmctl-<arch>-<version>.tar.gz
  ├── streaming SHA-256 verify
  ├── unpack + chmod + atomic rename → /usr/local/bin/mvmctl
  └── (caller emits audit_emit!(UpdateInstall) on Ok)
```

The release URL is hard-coded:

```rust
const GH_RELEASES_LATEST_URL: &str =
    "https://api.github.com/repos/tinylabscom/mvm/releases/latest";
```

For hermetic testing, the test sets up `httpmock` serving the same
JSON shape + binary asset, then overrides the URL via an env-var.

## State of play

### Already in `origin/main`

- `update.rs` with the hard-coded URLs.
- `audit_emit!(UpdateInstall)` in `commands/env/update.rs`.
- `assert_cmd` + `tempfile` already in dev-deps.

### Missing (integration)

1. No env-var override for the GitHub URL.
2. `httpmock` (or equivalent) not in dev-deps.
3. No live test for `UpdateInstall`.

## Workstreams

### W1 — `MVM_UPDATE_BASE_URL` env-var override (~½ day)

**Goal**: when set, `update.rs` uses the env-var value as the
release-metadata base URL instead of the hard-coded GitHub URL.
The path suffix (`/releases/latest`, `/releases/assets/<id>`) is
appended by `update.rs`; the env-var supplies only the base.

**Action**:

- New helper in `update.rs`:
  ```rust
  fn release_metadata_url() -> String {
      std::env::var("MVM_UPDATE_BASE_URL")
          .unwrap_or_else(|_| String::from(GH_RELEASES_LATEST_URL))
  }
  ```
- Every call site that uses `GH_RELEASES_LATEST_URL` calls
  `release_metadata_url()` instead. Same for the asset-download URL.
- A `ui::warn` line logs the override when set so the bypass is
  visible.

**Exit tests**:

- `release_metadata_url_returns_default_without_env`.
- `release_metadata_url_uses_env_when_set`.

### W2 — `httpmock` fixture + live test (~½ day)

**Goal**: end-to-end exercise of `mvmctl update` against a local
mock server; pin `UpdateInstall` emit.

**Action**:

- Add `httpmock = "0.7"` to `[dev-dependencies]` in `Cargo.toml`.
- `tests/audit_emissions_live.rs`:
  ```rust
  #[test]
  fn update_install_emits_update_install_audit_entry() {
      use httpmock::prelude::*;
      let server = MockServer::start();

      // Mock the release-metadata endpoint.
      let _m1 = server.mock(|when, then| {
          when.method(GET).path("/releases/latest");
          then.status(200).json_body(serde_json::json!({
              "tag_name": "v999.0.0",
              "assets": [{
                  "name": format!("mvmctl-{}.tar.gz", target_arch()),
                  "browser_download_url": server.url("/asset")
              }]
          }));
      });

      // Mock the asset download.
      let _m2 = server.mock(|when, then| {
          when.method(GET).path("/asset");
          then.status(200).body(stub_tarball_bytes());
      });

      let sandbox = AuditSandbox::new();
      let output = sandbox.mvmctl()
          .env("MVM_UPDATE_BASE_URL", server.base_url())
          .env("MVM_SKIP_VERIFY", "1")  // skip SHA-256 against the stub
          .args(["update", "--force"])
          .output().expect("spawn");
      // The test may fail at the rename-into-/usr/local/bin step
      // (no write perms) but the audit emit fires first.
      let log = read_audit_log(&sandbox.audit_log_path());
      assert!(count_entries_with_kind(&log, "update_install") >= 1, ...);
  }
  ```
- The test asserts the emit appeared *even if* the final
  rename step fails (the install path on a sandbox HOME doesn't
  have write access to `/usr/local/bin`). Per Plan 37 §6 the
  attempt is auditable; the emit fires regardless of post-emit
  filesystem outcome.

**Exit criteria**:

- Test passes locally and in CI.
- xtask lints clean.

## Phasing

W1 → W2 in a single PR (~200 lines including the test).

## Non-goals

- **Real cosign / Sigstore verification.** The plan-36 manifest
  signing path has its own tests; plan 69 just exercises the audit
  emit.
- **Mid-stream interruption testing.** Network failures, partial
  downloads, etc. are separate.

## Success criteria

By plan 69 close:

1. `MVM_UPDATE_BASE_URL` env-var redirects release queries.
2. `UpdateInstall` row live-pinned with httpmock.
3. `httpmock` lands in dev-deps; no production-dep growth.
4. xtask lints clean.

## Risk notes

- **Audit emit ordering vs install step.** If the audit emit fires
  *after* the rename step (which fails on a sandbox HOME), the test
  doesn't see the emit. Verify the order in `commands/env/update.rs`
  before writing the test; if needed, refactor so the emit fires on
  every attempt (per Plan 37 §6) regardless of install outcome —
  same pattern as `storage gc` in PR #107.
