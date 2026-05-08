use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("gen-man") => {
            let output_dir = parse_output_dir(&args).unwrap_or_else(default_man_dir);
            gen_man(&output_dir)
        }
        Some(other) => anyhow::bail!("Unknown xtask: {:?}. Available: gen-man", other),
        None => {
            eprintln!("Usage: cargo xtask <task>");
            eprintln!("Available tasks:");
            eprintln!("  gen-man [--output-dir DIR]   Generate man pages into DIR (default: man/)");
            std::process::exit(1);
        }
    }
}

fn parse_output_dir(args: &[String]) -> Option<PathBuf> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--output-dir" {
            return iter.next().map(PathBuf::from);
        }
    }
    None
}

fn default_man_dir() -> PathBuf {
    // Resolve workspace root: xtask's CARGO_MANIFEST_DIR is <workspace>/xtask/.
    let manifest = std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default());
    manifest.parent().unwrap_or(&manifest).join("man")
}

pub fn gen_man(output_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(output_dir).with_context(|| {
        format!(
            "Failed to create output directory: {}",
            output_dir.display()
        )
    })?;

    let cmd = mvm_cli::commands::cli_command();
    generate_man_pages(&cmd, output_dir)?;

    println!("Man pages written to: {}", output_dir.display());
    Ok(())
}

/// Generate man pages for `cmd` and each of its subcommands.
///
/// Top-level page: `<cmd_name>.1`
/// Subcommand pages: `<cmd_name>-<sub>.1`
fn generate_man_pages(cmd: &clap::Command, output_dir: &Path) -> Result<()> {
    let cmd_name = cmd.get_name().to_string();

    // Generate top-level man page.
    write_man_page(cmd, &output_dir.join(format!("{cmd_name}.1")))?;

    // Generate one page per direct subcommand.
    for sub in cmd.get_subcommands() {
        let sub_page_name = format!("{cmd_name}-{}", sub.get_name());
        write_man_page(sub, &output_dir.join(format!("{sub_page_name}.1")))?;
    }

    Ok(())
}

fn write_man_page(cmd: &clap::Command, path: &Path) -> Result<()> {
    let mut file = std::fs::File::create(path)
        .with_context(|| format!("Failed to create {}", path.display()))?;
    clap_mangen::Man::new(cmd.clone())
        .render(&mut file)
        .with_context(|| format!("Failed to render man page for {}", cmd.get_name()))?;
    println!("  {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gen_man_creates_main_page() {
        let tmp = tempfile::tempdir().unwrap();
        gen_man(tmp.path()).unwrap();

        let main_page = tmp.path().join("mvmctl.1");
        assert!(main_page.exists(), "mvmctl.1 should be generated");

        let content = std::fs::read_to_string(&main_page).unwrap();
        assert!(
            content.contains("mvmctl"),
            "man page should contain the command name"
        );
        assert!(content.contains(".TH"), "man page should have a .TH header");
    }

    #[test]
    fn gen_man_creates_subcommand_pages() {
        let tmp = tempfile::tempdir().unwrap();
        gen_man(tmp.path()).unwrap();

        // At least one subcommand page should be generated.
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("mvmctl-"))
            .collect();
        assert!(
            !entries.is_empty(),
            "at least one subcommand man page should be generated"
        );
    }
}
