// Plan 120 core-demo E2E: dev up -> compile the hello-app -> up (build+boot) ->
// guest agent answers Ping over vsock -> teardown. Gated on MVM_E2E_SMOKE=1
// (needs libkrun + the builder VM; runs for minutes). The spine's regression guard.
//
// macOS (libkrun) is forced, test-only: MVM_BUILDER_BACKEND=libkrun on every call
// (harmless on `compile`), plus `--hypervisor libkrun` on `up` — auto-select on a
// macOS-26 host picks Vz (builder) / apple-container (workload), not libkrun. This
// does NOT change `up`'s product auto-select.
//
// Run under state isolation so demo audit/nonce/key state never touches the real
// ~/.mvm and never races a parallel session:
//   MVM_DATA_DIR="$PWD/.mvm-test" MVM_E2E_SMOKE=1 \
//     cargo test -p mvm-cli --test core_demo_e2e -- --nocapture
use assert_cmd::cargo::CommandCargoExt;
use std::process::Command;

fn mvmctl(args: &[&str]) -> std::process::Output {
    #[allow(deprecated)]
    Command::cargo_bin("mvmctl")
        .expect("locate mvmctl")
        .env("MVM_BUILDER_BACKEND", "libkrun")
        .args(args)
        .output()
        .expect("spawn mvmctl")
}

#[test]
fn core_demo_dev_compile_up_ping() {
    if std::env::var("MVM_E2E_SMOKE").ok().as_deref() != Some("1") {
        eprintln!("skipping core-demo E2E; set MVM_E2E_SMOKE=1 to run");
        return;
    }
    let out = tempfile::tempdir().expect("tmp out");
    let app = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/python/hello-app/app.py"
    );

    // 1) builder VM up (idempotent), via libkrun.
    let dev = mvmctl(&["dev", "up"]);
    assert!(
        dev.status.success(),
        "dev up failed: {}",
        String::from_utf8_lossy(&dev.stderr)
    );

    // 2) lower the decorator app to flake.nix + launch.json.
    let c = mvmctl(&["compile", app, "--out", out.path().to_str().unwrap()]);
    assert!(
        c.status.success(),
        "compile failed: {}",
        String::from_utf8_lossy(&c.stderr)
    );

    // 3) build + boot the workload microVM via libkrun; `up` waits for the guest
    //    agent (wait_for_guest_agent -> vsock Ping). Exit 0 + no "not reachable"
    //    line == the agent answered. Never bypass admission (no --hypervisor mock /
    //    MVM_DIRECT_BOOT) and never flip MVM_ACK_UNRESTRICTED_NETWORK.
    let up = mvmctl(&[
        "up",
        "--hypervisor",
        "libkrun",
        "--flake",
        out.path().to_str().unwrap(),
    ]);
    let log = String::from_utf8_lossy(&up.stderr);
    assert!(up.status.success(), "up failed: {log}");
    assert!(
        !log.contains("Guest agent not reachable"),
        "agent never answered: {log}"
    );

    // 4) teardown the builder (best-effort).
    let _ = mvmctl(&["dev", "down"]);
}
