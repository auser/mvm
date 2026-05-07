//! CLI-level wiring tests for the `mvmctl up --flake <archive.tar.gz>`
//! intake path (Phase 2 / `specs/contracts/mvm-archive-input.md`).
//!
//! Unit tests in `crates/mvm-cli/src/commands/vm/archive.rs` cover the
//! per-error-code contract semantics exhaustively. These integration
//! tests exist to confirm the *wiring* — that the classification step
//! runs in the `up` command before any system-level setup, and that
//! contract error codes (`E_*`) surface in `mvmctl`'s stderr stream.
//!
//! All tests here exercise error paths so they don't need Lima / KVM /
//! a working build environment — classification fails before any of
//! that runs.

use assert_cmd::Command;
use flate2::Compression;
use flate2::write::GzEncoder;
use predicates::prelude::*;
use std::io::Write;

fn mvm() -> Command {
    #[allow(deprecated)]
    Command::cargo_bin("mvmctl").unwrap()
}

fn write_tarball_with_path(name: &str, bytes: &[u8]) -> tempfile::NamedTempFile {
    let mut f = tempfile::Builder::new()
        .prefix(name)
        .suffix(".tar.gz")
        .tempfile()
        .unwrap();
    f.write_all(bytes).unwrap();
    f
}

fn append_file<W: Write>(builder: &mut tar::Builder<W>, path: &str, contents: &[u8]) {
    let mut header = tar::Header::new_gnu();
    header.set_path(path).unwrap();
    header.set_size(contents.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder.append(&header, contents).unwrap();
}

#[test]
fn unsupported_input_kind_surfaces_error_code() {
    // A path that's neither a directory nor a `.tar.gz` file should
    // yield `E_UP_INPUT_KIND_UNSUPPORTED` from the up handler.
    let dir = tempfile::tempdir().unwrap();
    let nonexistent = dir.path().join("missing.tar.gz");

    mvm()
        .args([
            "up",
            "--flake",
            nonexistent.to_str().unwrap(),
            "--name",
            "test-vm",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("E_UP_INPUT_KIND_UNSUPPORTED"));
}

#[test]
fn layout_invalid_archive_surfaces_error_code() {
    // A tarball that extracts cleanly but is missing `launch.json` /
    // `source/` should yield `E_ARCHIVE_LAYOUT_INVALID`.
    let mut tar_bytes = Vec::new();
    {
        let gz = GzEncoder::new(&mut tar_bytes, Compression::default());
        let mut builder = tar::Builder::new(gz);
        append_file(&mut builder, "flake.nix", b"# only flake, no launch.json\n");
        builder.finish().unwrap();
    }
    let archive = write_tarball_with_path("layout-invalid-", &tar_bytes);

    mvm()
        .args([
            "up",
            "--flake",
            archive.path().to_str().unwrap(),
            "--name",
            "test-vm",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("E_ARCHIVE_LAYOUT_INVALID"));
}

#[test]
fn watch_with_archive_input_is_rejected() {
    // `--watch` doesn't compose with archive inputs (the artifact is
    // immutable). The handler should bail before any extraction or
    // build occurs.
    let mut tar_bytes = Vec::new();
    {
        let gz = GzEncoder::new(&mut tar_bytes, Compression::default());
        let mut builder = tar::Builder::new(gz);
        append_file(&mut builder, "flake.nix", b"# flake\n");
        append_file(&mut builder, "launch.json", b"{}\n");
        append_file(&mut builder, "source/main.py", b"print('ok')\n");
        builder.finish().unwrap();
    }
    let archive = write_tarball_with_path("watch-reject-", &tar_bytes);

    mvm()
        .args([
            "up",
            "--flake",
            archive.path().to_str().unwrap(),
            "--name",
            "test-vm",
            "--watch",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--watch is not supported"));
}
