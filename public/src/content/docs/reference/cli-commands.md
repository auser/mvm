---
title: CLI Commands
description: Complete command reference for mvmctl.
---

## Environment Management

| Command | Description |
|---------|-------------|
| `mvmctl bootstrap` | Full setup from scratch: Homebrew deps (macOS), Lima, Firecracker, kernel, rootfs |
| `mvmctl setup` | Create Lima VM and install Firecracker assets (requires limactl) |
| `mvmctl setup --recreate` | Stop microVM, rebuild rootfs from upstream squashfs |
| `mvmctl dev` | Auto-bootstrap if needed, drop into Lima dev shell |
| `mvmctl status` | Show platform, Lima VM, Firecracker, and microVM status |
| `mvmctl destroy` | Tear down Lima VM and all resources (confirmation required) |
| `mvmctl doctor` | Run system diagnostics and dependency checks |
| `mvmctl update` | Check for and install mvmctl updates |
| `mvmctl update --check` | Only check for updates, don't install |

## MicroVM Lifecycle

| Command | Description |
|---------|-------------|
| `mvmctl run --flake <ref>` | Build from flake and boot a headless Firecracker VM |
| `mvmctl run --template <name>` | Run from a pre-built template (skip build) |
| `mvmctl run -p HOST:GUEST` | Forward a port mapping into the VM (repeatable) |
| `mvmctl run -e KEY=VALUE` | Inject an environment variable (repeatable) |
| `mvmctl run -v host:guest:size` | Mount a volume into the microVM (repeatable) |
| `mvmctl run --forward` | Auto-forward declared ports after boot (blocks until Ctrl-C) |
| `mvmctl run --hypervisor <backend>` | Hypervisor backend: `firecracker` (default) or `qemu` |
| `mvmctl stop [name]` | Stop a running microVM by name |
| `mvmctl stop --all` | Stop all running VMs |
| `mvmctl up [name]` | Launch microVMs from `mvm.toml` or CLI flags |
| `mvmctl down [name]` | Stop microVMs from `mvm.toml`, by name, or all |
| `mvmctl forward <name> -p PORT` | Forward a port from a running microVM to localhost |
| `mvmctl shell` | Open a shell in the Lima VM |
| `mvmctl shell --project ~/dir` | Open shell and cd into a project directory |
| `mvmctl ssh` | Open a shell in the Lima VM (alias for `mvmctl shell`) |
| `mvmctl ssh-config` | Print an SSH config entry for the Lima VM |
| `mvmctl sync` | Build mvmctl from source inside Lima and install to `/usr/local/bin/` |
| `mvmctl sync --debug` | Debug build (faster compile, slower runtime) |

## Building

| Command | Description |
|---------|-------------|
| `mvmctl build <path>` | Build from Mvmfile.toml in the given directory |
| `mvmctl build --flake <ref>` | Build from a Nix flake (local or remote) |
| `mvmctl build --flake <ref> --watch` | Build and rebuild on flake.lock changes |
| `mvmctl build --json` | Output structured JSON events instead of human-readable output |
| `mvmctl cleanup` | Remove old dev-build artifacts and run Nix garbage collection |
| `mvmctl cleanup --all` | Remove all cached build revisions |
| `mvmctl cleanup --keep <N>` | Keep the N newest build revisions |

## Templates

| Command | Description |
|---------|-------------|
| `mvmctl template init <name> --local` | Scaffold a new template directory with flake.nix |
| `mvmctl template create <name>` | Create a single template definition |
| `mvmctl template create-multi <base>` | Create templates for multiple roles (`--roles worker,gateway`) |
| `mvmctl template build <name>` | Build a template (runs nix build in Lima) |
| `mvmctl template build <name> --force` | Rebuild even if cached |
| `mvmctl template build <name> --snapshot` | Build, boot, wait for healthy, and capture a Firecracker snapshot |
| `mvmctl template build <name> --update-hash` | Recompute the Nix fixed-output derivation hash |
| `mvmctl template push <name>` | Push to S3-compatible registry |
| `mvmctl template pull <name>` | Pull from registry |
| `mvmctl template verify <name>` | Verify template checksums |
| `mvmctl template list` | List all templates (`--json` for JSON) |
| `mvmctl template info <name>` | Show template details and revisions (`--json` for JSON) |
| `mvmctl template edit <name>` | Edit template configuration (--cpus, --mem, --flake, etc.) |
| `mvmctl template delete <name>` | Delete a template (`--force` to skip confirmation) |

## MicroVM Diagnostics

| Command | Description |
|---------|-------------|
| `mvmctl vm ping [name]` | Health-check running microVMs via vsock (all if no name given) |
| `mvmctl vm status [name]` | Query worker status (`--json` for JSON) |
| `mvmctl vm inspect <name>` | Deep-dive inspection (probes, integrations, worker status) |
| `mvmctl vm exec <name> -- <cmd>` | Run a command inside a running microVM (dev-only) |
| `mvmctl vm diagnose <name>` | Run layered diagnostics on a VM (works even when vsock is broken) |
| `mvmctl logs <name>` | View guest console logs (`-f` to follow, `-n` for line count) |
| `mvmctl logs <name> --hypervisor` | View Firecracker hypervisor logs |

## Security

| Command | Description |
|---------|-------------|
| `mvmctl security status` | Show security posture score (`--json` for JSON) |

## Utilities

| Command | Description |
|---------|-------------|
| `mvmctl completions <shell>` | Generate shell completions (bash, zsh, fish, powershell) |
| `mvmctl shell-init` | Print shell configuration (completions + dev aliases) to stdout |
| `mvmctl release` | Pre-release checks (deploy guard + cargo publish dry-run) |
| `mvmctl release --guard-only` | Run deploy guard checks only (version, tag, inter-crate deps) |

## Global Options

All commands accept these global options:

| Option | Description |
|--------|-------------|
| `--log-format <human\|json>` | Log format: human (default) or json (structured) |
| `--fc-version <VERSION>` | Override Firecracker version (e.g., v1.14.0) |

## Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `MVM_DATA_DIR` | Root data directory for templates and builds | `~/.mvm` |
| `MVM_FC_VERSION` | Firecracker version (auto-normalized to `vMAJOR.MINOR`) | Latest stable |
| `MVM_FC_ASSET_BASE` | S3 base URL for Firecracker assets | AWS default |
| `MVM_FC_ASSET_ROOTFS` | Override rootfs filename | Auto-detected |
| `MVM_FC_ASSET_KERNEL` | Override kernel filename | Auto-detected |
| `MVM_BUILDER_MODE` | Builder transport: `auto`, `vsock`, or `ssh` | `auto` |
| `MVM_TEMPLATE_REGISTRY_ENDPOINT` | S3-compatible endpoint URL for template push/pull | None |
| `MVM_TEMPLATE_REGISTRY_BUCKET` | S3 bucket name for templates | None |
| `MVM_TEMPLATE_REGISTRY_ACCESS_KEY_ID` | S3 access key ID | None |
| `MVM_TEMPLATE_REGISTRY_SECRET_ACCESS_KEY` | S3 secret access key | None |
| `MVM_TEMPLATE_REGISTRY_PREFIX` | Key prefix inside the bucket | `mvm` |
| `MVM_TEMPLATE_REGISTRY_REGION` | S3 region | `us-east-1` |
| `MVM_SSH_PORT` | Lima SSH local port | `60022` |
| `MVM_PRODUCTION` | Enable production mode checks | `false` |
| `RUST_LOG` | Logging level (e.g., `debug`, `mvm=trace`) | `info` |
