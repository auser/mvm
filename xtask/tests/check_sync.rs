use std::process::Command;

#[test]
fn xtask_check_sync_passes_on_main() {
    // Use `cargo run -p xtask --` rather than `cargo xtask` — the
    // workspace ships no `[alias]` section in .cargo/config.toml, so
    // the `xtask` subcommand only resolves on contributor machines
    // that have set up the alias manually. CI runners don't.
    let status = Command::new("cargo")
        .args(["run", "-p", "xtask", "--", "check-mvm-host-binaries-sync"])
        .status()
        .expect("spawn cargo run -p xtask --");
    assert!(
        status.success(),
        "xtask reported a sync drift between Rust manifest and Nix attrset"
    );
}
