//! Structural guard for `nix/flake.nix` and `nix/profiles/*.nix`
//! (Phase 1 W4 — plan 60).
//!
//! These tests don't run `nix flake check` (that requires Nix on the
//! test host and adds a network dep on github:microvm-nix/microvm.nix).
//! Instead they assert the flake's *shape* — the file is present, has
//! the expected top-level inputs/outputs, and references the
//! microvm.nix module by hash-pinned input. A regression that
//! deletes the flake or removes the microvm.nix dependency trips
//! these tests on every PR's `cargo test`.
//!
//! For full evaluation, run on a host with Nix:
//!
//!   cd nix && nix flake check --no-build
//!
//! Documented in `specs/runbooks/cross-platform-install.md` (Phase 5).

use std::fs;
use std::path::PathBuf;

fn nix_dir() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR is set by cargo for integration tests");
    PathBuf::from(manifest).join("nix")
}

#[test]
fn flake_nix_exists_and_imports_microvm_nix() {
    let path = nix_dir().join("flake.nix");
    let content = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("nix/flake.nix must be present: {e}"));

    // ADR-013 invariant: the flake imports microvm.nix as the foundation.
    // Any future PR that drops this input violates the ADR.
    assert!(
        content.contains("microvm-nix/microvm.nix"),
        "nix/flake.nix must reference microvm-nix/microvm.nix as an input \
         (per ADR-013); content excerpt: {}",
        &content[..content.len().min(200)]
    );

    // The flake must declare nixosConfigurations — that's how
    // microvm.nix's NixOS module composition works. A regression
    // that drops this would silently produce a flake with no
    // buildable output.
    assert!(
        content.contains("nixosConfigurations"),
        "nix/flake.nix must declare nixosConfigurations to expose the \
         microvm.nix-built test fixtures"
    );

    // The user-facing library output. User flakes consume
    // `mvm.lib.<system>.mkGuest`; if that path stops being exposed,
    // every user project breaks at next nix evaluation. Guarding it
    // here means a refactor of the flake can't accidentally drop
    // the user contract.
    assert!(
        content.contains("lib") && content.contains("mkGuest"),
        "nix/flake.nix must expose lib.<system>.mkGuest as the \
         user-facing API (per ADR-013 + plan 60). Got: ...{}",
        &content[..content.len().min(200)]
    );

    // Internal-prefix convention: test fixtures live under
    // `internal-*` so the boundary between user-facing and mvm-
    // internal is mechanical. A regression that exposes a fixture
    // under a bare name (without the prefix) is a UX-leak waiting
    // to happen.
    assert!(
        content.contains("internal-minimal"),
        "nix/flake.nix must expose internal fixtures under the \
         internal-* namespace; bare names suggest user-facing API"
    );
}

#[test]
fn minimal_profile_exists_and_has_required_settings() {
    let path = nix_dir().join("profiles").join("minimal.nix");
    let content = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("nix/profiles/minimal.nix must be present: {e}"));

    // SSH disabled — load-bearing invariant from ADR-002 / CLAUDE.md
    // ("No SSH in microVMs, ever"). Asserted as plain string match
    // because a NixOS module's effective `services.openssh.enable`
    // can only be checked by evaluating the flake; the source line
    // is the closest we can get without booting Nix.
    assert!(
        content.contains("services.openssh.enable = false"),
        "minimal profile must explicitly disable SSH per ADR-002"
    );

    // The microvm.hypervisor must be declared — that's what
    // selects the runner. Even if it's `firecracker` (the default),
    // the explicit declaration makes the profile self-documenting.
    assert!(
        content.contains("microvm.hypervisor") || content.contains("hypervisor"),
        "minimal profile must declare a microvm.hypervisor (defaults \
         to firecracker per ADR-013)"
    );

    // system.stateVersion is mandatory for any NixOS module — a
    // missing one breaks evaluation. Guard it explicitly so the
    // failure mode is "this test fails" rather than "nix flake
    // check is the only way to find out."
    assert!(
        content.contains("system.stateVersion"),
        "minimal profile must declare system.stateVersion (NixOS \
         module evaluation requirement)"
    );
}

/// Optional: shell out to `nix eval` against `nix/tests/mk-guest-eval.nix`
/// and assert every check returns true. Skipped silently when `nix`
/// isn't on PATH (most macOS dev hosts) so this test stays cheap on
/// every PR; CI runners with Nix exercise the real eval.
///
/// This is the strongest guard we have on the user-facing
/// `lib.<system>.mkGuest` surface — it actually invokes the function
/// with each of the three entrypoint shapes (`shell` / `command` /
/// `services`) plus the explicit `dev` overrides, and asserts the
/// `passthru.mvm.{accessible, sealed, entrypointKind}` metadata is
/// inferred correctly.
#[test]
fn mk_guest_eval_assertions_all_pass_when_nix_available() {
    use std::process::Command;

    // Skip when nix isn't on PATH. Cheap precondition — a single
    // process spawn per skipped test.
    let nix_check = Command::new("nix").arg("--version").output();
    if nix_check.is_err() {
        eprintln!(
            "[nix_flake_structure::mk_guest_eval] skipped — `nix` not on PATH"
        );
        return;
    }

    let manifest = std::env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR is set by cargo for integration tests");
    let eval_file = std::path::PathBuf::from(&manifest)
        .join("nix")
        .join("tests")
        .join("mk-guest-eval.nix");

    let out = Command::new("nix")
        .arg("--extra-experimental-features")
        .arg("nix-command flakes")
        .arg("eval")
        .arg("--json")
        .arg("--file")
        .arg(&eval_file)
        .output()
        .expect("nix eval invocation");

    assert!(
        out.status.success(),
        "nix eval failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // The eval file returns an attribute set of named boolean
    // assertions. Parse the JSON and verify every value is `true`.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("nix eval output isn't JSON: {e}\nstdout: {stdout}"));
    let obj = json
        .as_object()
        .expect("mk-guest-eval.nix must return an attribute set");

    let mut failures: Vec<String> = Vec::new();
    for (name, value) in obj {
        match value.as_bool() {
            Some(true) => { /* ok */ }
            Some(false) => failures.push(format!("{name} = false")),
            None => failures.push(format!("{name} not a bool")),
        }
    }
    assert!(
        failures.is_empty(),
        "mkGuest eval assertions failed: {}\nFull output: {stdout}",
        failures.join(", ")
    );
}

#[test]
fn flake_lock_pins_microvm_input_by_hash() {
    // The flake.lock must exist and pin the microvm.nix input by
    // commit hash, not by tag or branch — that's the supply-chain
    // gate from ADR-013 §"Threat model impact" / plan 60 §"Code
    // review gate." A PR that removes flake.lock or drops the
    // microvm pin breaks this assertion.
    let path = nix_dir().join("flake.lock");
    let content = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "nix/flake.lock must be committed for hash-pinned supply \
             chain (run `cd nix && nix flake lock` to generate): {e}"
        )
    });

    // Pinned by hash means the lockfile carries a `rev` field for
    // the microvm input. We don't pin a *specific* hash here (CI's
    // `xtask audit-flake` does that on bump) — we just verify the
    // microvm input is present in the lockfile.
    assert!(
        content.contains("\"microvm\""),
        "flake.lock must contain the 'microvm' input pin"
    );
    assert!(
        content.contains("\"rev\""),
        "flake.lock must pin inputs by `rev` (commit hash)"
    );
}
