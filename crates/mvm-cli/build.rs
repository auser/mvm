use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = manifest_dir
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let bins_out = out_dir.join("mvm-host-bins");
    std::fs::create_dir_all(&bins_out).expect("create OUT_DIR/mvm-host-bins");

    let pin = read_pinned_toolchain(&workspace_root);
    println!("cargo:rustc-env=MVM_PINNED_ZIG={}", pin.zig);
    println!(
        "cargo:rustc-env=MVM_PINNED_CARGO_ZIGBUILD={}",
        pin.cargo_zigbuild
    );
    println!("cargo:rustc-env=MVM_PINNED_TARGET={}", pin.target);

    let manifest = read_rust_manifest(&workspace_root);
    let mut entries = Vec::new();

    let host_triple = std::env::var("HOST").unwrap();
    let native = host_triple.contains("linux") && host_triple.contains(strip_glibc(&pin.target));

    for (name, cargo_pkg) in manifest.iter() {
        let out_file = bins_out.join(name);
        if native {
            run_cargo_build(&workspace_root, cargo_pkg, &pin.target, &out_file);
        } else {
            run_cargo_zigbuild(&workspace_root, cargo_pkg, &pin.target, &out_file);
        }
        let sha = sha256_hex(&out_file);
        entries.push((name.clone(), out_file.clone(), sha));
        println!(
            "cargo:rerun-if-changed={}",
            workspace_root
                .join(format!("crates/{cargo_pkg}/src"))
                .display()
        );
    }

    let embedded_rs = render_embedded_rs(&entries);
    std::fs::write(out_dir.join("embedded.rs"), embedded_rs).unwrap();
    println!(
        "cargo:rerun-if-changed={}",
        workspace_root.join("Cargo.toml").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        workspace_root
            .join("crates/mvm-cli/src/host_binaries/manifest.rs")
            .display()
    );
}

struct Pin {
    zig: String,
    cargo_zigbuild: String,
    target: String,
}

fn read_pinned_toolchain(root: &Path) -> Pin {
    let toml_str = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
    let v: toml::Value = toml::from_str(&toml_str).unwrap();
    let p = &v["workspace"]["metadata"]["mvm"]["toolchain"];
    Pin {
        zig: p["zig"].as_str().unwrap().to_string(),
        cargo_zigbuild: p["cargo-zigbuild"].as_str().unwrap().to_string(),
        target: p["target"].as_str().unwrap().to_string(),
    }
}

/// Parse `name:` / `cargo_pkg:` field pairs from the Rust struct literals
/// in `crates/mvm-cli/src/host_binaries/manifest.rs`.
///
/// Returns a list of `(logical_name, cargo_package_name)` pairs in
/// declaration order. `cargo_pkg` defaults to `name` when absent so
/// that entries where the two are identical don't need the redundant field.
fn read_rust_manifest(root: &Path) -> Vec<(String, String)> {
    let src =
        std::fs::read_to_string(root.join("crates/mvm-cli/src/host_binaries/manifest.rs")).unwrap();

    let mut out = Vec::new();
    let mut current_name: Option<String> = None;
    let mut current_cargo_pkg: Option<String> = None;

    for line in src.lines() {
        // A `name:` line starts a new binary entry.
        if let Some(n) = extract_quoted_after(line, "name:") {
            // Flush any previous entry that had name but no cargo_pkg yet.
            if let Some(name) = current_name.take() {
                let pkg = current_cargo_pkg.take().unwrap_or_else(|| name.clone());
                out.push((name, pkg));
            }
            current_name = Some(n);
            current_cargo_pkg = None;
        }
        if let Some(p) = extract_quoted_after(line, "cargo_pkg:") {
            current_cargo_pkg = Some(p);
        }
    }
    // Flush the last entry.
    if let Some(name) = current_name.take() {
        let pkg = current_cargo_pkg.take().unwrap_or_else(|| name.clone());
        out.push((name, pkg));
    }

    out
}

/// Extract the first double-quoted string on `line` that appears after `key`.
fn extract_quoted_after(line: &str, key: &str) -> Option<String> {
    let i = line.find(key)? + key.len();
    let rest = &line[i..];
    let q1 = rest.find('"')? + 1;
    let q2 = rest[q1..].find('"')?;
    Some(rest[q1..q1 + q2].to_string())
}

/// Strip the glibc version suffix from a target triple.
/// e.g. `aarch64-unknown-linux-gnu.2.17` → `aarch64-unknown-linux-gnu`
fn strip_glibc(t: &str) -> &str {
    t.split('.').next().unwrap()
}

fn run_cargo_zigbuild(root: &Path, pkg: &str, target: &str, out: &Path) {
    eprintln!("[build.rs] cargo zigbuild --release --target {target} -p {pkg}");
    // We need the rustup-managed cargo, not the Homebrew one. The Homebrew
    // cargo sets RUSTC=rustc which doesn't have the cross targets, and that
    // value propagates into the nested `cargo build` that cargo-zigbuild
    // spawns. Using the rustup cargo avoids that.
    let (cargo, rustc) = rustup_cargo_and_rustc(strip_glibc(target));
    // Also clear RUSTUP_TOOLCHAIN if set, since it can override the rustc we pass.
    // And clear RUSTC_WRAPPER which could redirect to a wrong rustc.
    let status = Command::new(&cargo)
        .args(["zigbuild", "--release", "--target", target, "-p", pkg])
        .env("RUSTC", &rustc)
        .env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .current_dir(root)
        .status()
        .expect(
            "spawn `cargo zigbuild` — \
             install with: `cargo install cargo-zigbuild --version 0.20.0` \
             and `brew install zig` (or equivalent)",
        );
    assert!(status.success(), "cargo zigbuild failed for {pkg}");
    // cargo-zigbuild strips the glibc suffix from the output directory name.
    let built = root
        .join("target")
        .join(strip_glibc(target))
        .join("release")
        .join(pkg);
    std::fs::copy(&built, out)
        .unwrap_or_else(|e| panic!("copy {} → {}: {e}", built.display(), out.display()));
}

fn run_cargo_build(root: &Path, pkg: &str, target: &str, out: &Path) {
    eprintln!(
        "[build.rs] cargo build --release --target {t} -p {pkg}",
        t = strip_glibc(target)
    );
    let (cargo, rustc) = rustup_cargo_and_rustc(strip_glibc(target));
    let status = Command::new(&cargo)
        .args([
            "build",
            "--release",
            "--target",
            strip_glibc(target),
            "-p",
            pkg,
        ])
        .env("RUSTC", &rustc)
        .env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .current_dir(root)
        .status()
        .expect("spawn `cargo build`");
    assert!(status.success(), "cargo build failed for {pkg}");
    let built = root
        .join("target")
        .join(strip_glibc(target))
        .join("release")
        .join(pkg);
    std::fs::copy(&built, out)
        .unwrap_or_else(|e| panic!("copy {} → {}: {e}", built.display(), out.display()));
}

/// Find a `(cargo, rustc)` pair that has `target` installed in its sysroot.
///
/// When a host has both Homebrew Rust and rustup, Homebrew cargo sets
/// `RUSTC=rustc` (or similar) for build scripts. That rustc may not have the
/// cross-compilation stdlib targets that rustup has. This function probes
/// candidates in order and returns the first pair where `rustc` can locate
/// the target's libdir.
///
/// Priority:
/// 1. `$RUSTC` / `$CARGO` env vars if the rustc has the target.
/// 2. `rustup which rustc` / `rustup which cargo` — the active toolchain.
/// 3. Plain `"rustc"` / `"cargo"` fallback — will likely fail at cross-compile
///    time with a clear "target not installed" message.
fn rustup_cargo_and_rustc(target: &str) -> (String, String) {
    // 1. Check the env vars first.
    let env_rustc = std::env::var("RUSTC").unwrap_or_default();
    let env_cargo = std::env::var("CARGO").unwrap_or_default();
    if !env_rustc.is_empty() && rustc_has_target(&env_rustc, target) {
        return (
            if env_cargo.is_empty() {
                "cargo".to_string()
            } else {
                env_cargo
            },
            env_rustc,
        );
    }

    // 2. Ask rustup. Try PATH and the canonical ~/.cargo/bin/ location.
    let home = std::env::var("HOME").unwrap_or_default();
    let rustup_candidates = vec!["rustup".to_string(), format!("{home}/.cargo/bin/rustup")];
    for rustup in &rustup_candidates {
        let rustc_out = Command::new(rustup).args(["which", "rustc"]).output();
        let cargo_out = Command::new(rustup).args(["which", "cargo"]).output();
        if let (Ok(rc), Ok(ca)) = (rustc_out, cargo_out)
            && rc.status.success()
            && ca.status.success()
        {
            let rc_path = String::from_utf8_lossy(&rc.stdout).trim().to_string();
            let ca_path = String::from_utf8_lossy(&ca.stdout).trim().to_string();
            if !rc_path.is_empty() && !ca_path.is_empty() && rustc_has_target(&rc_path, target) {
                return (ca_path, rc_path);
            }
        }
    }

    // 3. Last resort.
    (
        if env_cargo.is_empty() {
            "cargo".to_string()
        } else {
            env_cargo
        },
        if env_rustc.is_empty() {
            "rustc".to_string()
        } else {
            env_rustc
        },
    )
}

/// Return true if the given `rustc` binary reports the target's libdir exists.
fn rustc_has_target(rustc: &str, target: &str) -> bool {
    let out = Command::new(rustc)
        .args(["--target", target, "--print", "target-libdir"])
        .output();
    if let Ok(o) = out
        && o.status.success()
    {
        let dir = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if !dir.is_empty() && std::path::Path::new(&dir).exists() {
            return true;
        }
    }
    false
}

fn sha256_hex(p: &Path) -> String {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
    let mut h = Sha256::new();
    h.update(&bytes);
    format!("{:x}", h.finalize())
}

fn render_embedded_rs(entries: &[(String, PathBuf, String)]) -> String {
    let mut s = String::new();
    s.push_str("// Generated by mvm-cli/build.rs. Do not edit.\n\n");
    s.push_str(
        "pub struct EmbeddedBinary { \
         pub name: &'static str, \
         pub bytes: &'static [u8], \
         pub sha256_hex: &'static str \
         }\n\n",
    );
    s.push_str("pub const EMBEDDED: &[EmbeddedBinary] = &[\n");
    for (name, path, sha) in entries {
        s.push_str(&format!(
            "    EmbeddedBinary {{ name: {name:?}, bytes: include_bytes!({path:?}), sha256_hex: {sha:?} }},\n"
        ));
    }
    s.push_str("];\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_rust_manifest_parses_cargo_pkg() {
        // Test the parser against an inline snippet that mirrors the real
        // manifest structure (name ≠ cargo_pkg for mvm-builder-init).
        let fake_src = r#"
            HostBinary {
                name: "mvm-builder-init",
                cargo_pkg: "mvm-host-vm-init",
                install_path: "/sbin/mvm-builder-init",
                mode: 0o755,
            },
            HostBinary {
                name: "mvm-egress-proxy",
                cargo_pkg: "mvm-egress-proxy",
                install_path: "/sbin/mvm-egress-proxy",
                mode: 0o755,
            },
        "#;

        // Inline version of the parser logic (can't call read_rust_manifest
        // from within build.rs tests without a real filesystem, so we
        // exercise extract_quoted_after directly).
        let mut out: Vec<(String, String)> = Vec::new();
        let mut current_name: Option<String> = None;
        let mut current_cargo_pkg: Option<String> = None;
        for line in fake_src.lines() {
            if let Some(n) = extract_quoted_after(line, "name:") {
                if let Some(name) = current_name.take() {
                    let pkg = current_cargo_pkg.take().unwrap_or_else(|| name.clone());
                    out.push((name, pkg));
                }
                current_name = Some(n);
                current_cargo_pkg = None;
            }
            if let Some(p) = extract_quoted_after(line, "cargo_pkg:") {
                current_cargo_pkg = Some(p);
            }
        }
        if let Some(name) = current_name.take() {
            let pkg = current_cargo_pkg.take().unwrap_or_else(|| name.clone());
            out.push((name, pkg));
        }

        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0],
            ("mvm-builder-init".into(), "mvm-host-vm-init".into())
        );
        assert_eq!(
            out[1],
            ("mvm-egress-proxy".into(), "mvm-egress-proxy".into())
        );
    }

    #[test]
    fn strip_glibc_removes_version_suffix() {
        assert_eq!(
            strip_glibc("aarch64-unknown-linux-gnu.2.17"),
            "aarch64-unknown-linux-gnu"
        );
        assert_eq!(
            strip_glibc("aarch64-unknown-linux-gnu"),
            "aarch64-unknown-linux-gnu"
        );
    }

    #[test]
    fn extract_quoted_after_basic() {
        assert_eq!(
            extract_quoted_after(r#"        name: "mvm-builder-init","#, "name:"),
            Some("mvm-builder-init".to_string())
        );
        assert_eq!(
            extract_quoted_after(r#"        cargo_pkg: "mvm-host-vm-init","#, "cargo_pkg:"),
            Some("mvm-host-vm-init".to_string())
        );
        assert_eq!(extract_quoted_after("no key here", "name:"), None);
    }
}
