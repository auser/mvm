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
         microvm.nix-built profiles"
    );

    // microvm.declaredRunner is the top-level runner the docs point
    // users at. Its presence is a documentation contract (see
    // `public/src/content/docs/guides/building-microvm-images.md`).
    assert!(
        content.contains("declaredRunner"),
        "nix/flake.nix must expose microvm.declaredRunner so the docs' \
         build command works as documented"
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
