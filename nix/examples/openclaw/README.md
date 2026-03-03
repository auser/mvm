# OpenClaw MicroVM Example

This example demonstrates running [OpenClaw](https://github.com/OpenClaw/openclaw) in a Firecracker microVM using the native npx installer approach.

## Approach

This example uses a **runtime installer** approach rather than packaging OpenClaw with Nix:

- **Base OS**: Built with Nix, includes only Node.js 22
- **OpenClaw**: Downloaded and installed at runtime via `npx openclaw@latest`
- **Dynamic Mounts**: Configuration, secrets, and data mounted from host directories

This approach is simpler than the bundled approach (`openclaw-bundled`) and better demonstrates the mvm pattern of dynamic mounts for configuration and secrets.

## Quick Start

```bash
./start.sh [vm-name] [port]
```

Default: `vm-name=openclaw`, `port=3000`

## Dynamic Mounts

The VM demonstrates mvm's dynamic mount capabilities:

### Configuration (`/mnt/config`)

Mounted from `./config/`:
- `openclaw.json` - OpenClaw configuration (supports environment variable substitution)
- `env.sh` - Optional environment variables

The setup script copies `openclaw.json` to `/var/lib/openclaw/config.json` and applies `envsubst` for variable expansion.

### Secrets (`/mnt/secrets`)

Mounted from `./secrets/`:
- `api-keys.env` - API keys and secrets (sourced at runtime)

### Data (optional, `/mnt/data`)

If you pass `--data-dir` to `mvmctl run`, persistent data will be stored here:
- `skills/` - Custom skills
- `workspace/` - User workspace files
- `sessions/` - Session history

## First Run

The first run takes 5-10 minutes as OpenClaw downloads and caches dependencies via npx. Subsequent starts are much faster.

## Architecture

```
Host (macOS/Linux)
  └─ Lima VM (Ubuntu)
      └─ Firecracker microVM
          ├─ Node.js 22 (Nix-built)
          ├─ /mnt/config → ./config/
          ├─ /mnt/secrets → ./secrets/
          ├─ /var/lib/openclaw (tmpfs workspace)
          └─ npx openclaw@latest gateway
```

## Files

- `flake.nix` - Nix configuration for the base OS and OpenClaw service
- `start.sh` - Convenience script to build and run
- `config/openclaw.json` - Sample OpenClaw configuration
- `secrets/api-keys.env` - Example API key template

## Customization

Edit `config/openclaw.json` to configure:
- Gateway mode (local, remote, etc.)
- Port binding
- Instance settings

Add API keys to `secrets/api-keys.env`:
```bash
OPENAI_API_KEY=sk-...
ANTHROPIC_API_KEY=sk-ant-...
```

## Why Not Use the Bundled Approach?

The `openclaw-bundled` example packages OpenClaw with Nix and uses esbuild to bundle the application. While this works, it:
- Requires complex bundling configuration to work around OpenClaw's native modules
- Needs to be rebuilt for every OpenClaw update
- Obscures the dynamic mounts pattern

The runtime installer approach:
- ✅ Always uses the latest OpenClaw version
- ✅ Simpler flake configuration
- ✅ Better demonstrates mvm's dynamic mount capabilities
- ✅ Matches how users would actually deploy applications
