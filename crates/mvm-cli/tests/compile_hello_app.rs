// Plan 120 Task 2: the decorator `.py` path lowers statically (no host exec).
// `mvmctl compile examples/python/hello-app/app.py` walks the `@mvm.app(...)`
// AST, derives the Workload IR, and renders flake.nix + launch.json — without
// importing or executing the script. This locks that wiring against regression.
use assert_cmd::cargo::CommandCargoExt;
use std::process::Command;

#[test]
fn compile_hello_app_lowers_decorator_to_flake() {
    let out = tempfile::tempdir().expect("tmp out");
    let app = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/python/hello-app/app.py"
    );

    #[allow(deprecated)]
    let result = Command::cargo_bin("mvmctl")
        .expect("locate mvmctl")
        .args(["compile", app, "--out", out.path().to_str().unwrap()])
        .output()
        .expect("spawn mvmctl compile");

    assert!(
        result.status.success(),
        "mvmctl compile <app.py> failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        out.path().join("flake.nix").exists(),
        "flake.nix not emitted into --out dir"
    );
    assert!(
        out.path().join("launch.json").exists(),
        "launch.json not emitted into --out dir"
    );
}
