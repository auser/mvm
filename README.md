# mvm

Rust CLI that manages [Firecracker](https://firecracker-microvm.github.io/) microVMs via [Lima](https://lima-vm.io/) virtualization.

```
macOS / Linux Host  -->  Lima VM (Ubuntu)  -->  Firecracker microVM (172.16.0.2)
      this CLI              limactl                  /dev/kvm
```

## Quick Start

```bash
# From zero to SSH session in one command:
mvm bootstrap   # installs Lima via Homebrew, creates VM, downloads Firecracker + kernel + rootfs
mvm dev          # launches the microVM and drops you into SSH

# Or if you already have Lima installed:
mvm setup        # creates VM, installs Firecracker, downloads assets
mvm start        # starts the microVM and drops you into SSH
```

## Commands

| Command | Description |
|---------|-------------|
| `mvm bootstrap` | Full setup from scratch -- installs Lima via Homebrew, then runs all setup steps |
| `mvm setup` | Create Lima VM, install Firecracker, download kernel/rootfs (requires `limactl`) |
| `mvm dev` | Launch into the microVM, auto-bootstrapping if anything is missing |
| `mvm start` | Start the microVM and drop into interactive SSH |
| `mvm stop` | Stop the running microVM and clean up |
| `mvm ssh` | SSH into a running microVM |
| `mvm status` | Show status of Lima VM and microVM |
| `mvm destroy` | Tear down the Lima VM and all resources |

### `mvm dev` vs `mvm start`

`dev` is the smart entry point. It detects whatever state you're in and does the right thing:

- Lima not installed? Runs full bootstrap.
- Lima VM doesn't exist? Runs setup.
- Lima VM stopped? Starts it.
- Firecracker not installed? Installs it.
- MicroVM already running? Reconnects via SSH.
- MicroVM stuck? Stops and restarts it.

`start` assumes everything is set up and fails if it isn't.

## Prerequisites

- macOS or Linux host
- [Homebrew](https://brew.sh/) (for `mvm bootstrap` to auto-install Lima)
- Or install Lima manually: `brew install lima`

## Build

```bash
cargo build
cargo run -- --help
```

## Network Layout

```
MicroVM (172.16.0.2, eth0)
    |
    | TAP interface
    |
Lima VM (172.16.0.1, tap0) -- iptables NAT -- internet
    |
    | Lima virtualization
    |
Host (macOS / Linux)
```

## Architecture

The CLI runs on the host. All Linux operations happen inside the Lima VM via `limactl shell mvm bash -c "..."`. The Firecracker microVM runs inside Lima using nested virtualization (`/dev/kvm`).

### Modules

| Module | Responsibility |
|--------|----------------|
| `main.rs` | CLI dispatch and command orchestration |
| `bootstrap.rs` | Host-level prerequisites (Homebrew, Lima installation) |
| `config.rs` | Constants, state struct, Lima template rendering |
| `shell.rs` | Command helpers: `run_host`, `run_in_vm`, `replace_process` |
| `lima.rs` | Lima VM lifecycle: create, start, stop, destroy |
| `firecracker.rs` | Firecracker binary install, kernel/rootfs download, rootfs prep |
| `network.rs` | TAP device setup, IP forwarding, iptables NAT |
| `microvm.rs` | MicroVM lifecycle: start, stop, SSH, Firecracker API calls |

### Lima Config Template

The Lima VM configuration lives in `resources/lima.yaml.tera`, a [Tera](https://keats.github.io/tera/) template rendered at runtime with config values. Lima's own Go template syntax (`{{.User}}`) is preserved via `{% raw %}` blocks.

## License

MIT
