# User Guide: Writing Nix Flakes for mvm

This guide explains how to create custom Nix flakes that build microVM images for mvm worker pools.

## Overview

mvm uses Nix flakes to produce reproducible microVM images. Each pool references a flake and a profile. The build process runs inside an ephemeral Firecracker VM with Nix installed, producing a root filesystem and kernel.

## Flake Structure

A minimal mvm-compatible flake:

```nix
{
  description = "My mvm worker image";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-24.05";
  };

  outputs = { self, nixpkgs }: let
    system = "x86_64-linux";
    pkgs = nixpkgs.legacyPackages.${system};
  in {
    packages.${system} = {
      minimal = pkgs.buildEnv {
        name = "mvm-minimal";
        paths = with pkgs; [ busybox curl ];
      };

      baseline = pkgs.buildEnv {
        name = "mvm-baseline";
        paths = with pkgs; [ bash coreutils curl wget openssl ];
      };

      python = pkgs.buildEnv {
        name = "mvm-python";
        paths = with pkgs; [ python3 python3Packages.pip bash coreutils ];
      };
    };
  };
}
```

## Profiles

Profiles are named outputs within your flake. When you create a pool:

```bash
mvm pool create acme/workers --flake ./my-flake --profile baseline --cpus 2 --mem 1024
```

mvm builds the `baseline` output from `./my-flake`.

### Built-in Profiles

- **minimal** -- BusyBox + curl. Smallest image, fastest boot.
- **baseline** -- Standard shell utilities. Good for general workloads.
- **python** -- Python 3 + pip. For scripting and ML workloads.

## Build Process

When you run `mvm pool build <tenant>/<pool>`:

1. An ephemeral builder microVM starts with Nix pre-installed
2. The flake is copied into the builder VM
3. `nix build .#<profile>` runs inside the builder
4. The resulting closure is packed into an ext4 root filesystem
5. A revision hash is computed and stored
6. The `current` symlink is updated atomically

## Adding Services

To run services at boot, include systemd units in your flake:

```nix
baseline = pkgs.buildEnv {
  name = "mvm-baseline";
  paths = with pkgs; [
    bash coreutils curl
    (pkgs.writeTextDir "etc/systemd/system/my-app.service" ''
      [Unit]
      Description=My Application
      After=network.target

      [Service]
      ExecStart=/usr/bin/my-app
      Restart=always

      [Install]
      WantedBy=multi-user.target
    '')
  ];
};
```

## Data Disks

Pools can provision data disks per instance:

```bash
mvm pool create acme/workers --flake . --profile baseline --cpus 2 --mem 1024 --data-disk 1024
```

This creates a 1 GiB ext4 data disk mounted at `/data` inside each instance.

## Secrets

Tenant-level secrets are injected into instances via a read-only virtio block device mounted at `/run/secrets/`:

```bash
mvm tenant secrets set acme --from-file secrets.json
```

Inside the instance, access secrets at `/run/secrets/secrets.json`.

## Scaling

After building, scale the pool:

```bash
mvm pool scale acme/workers --running 4 --warm 2 --sleeping 2
```

- **Running** -- actively serving, full CPU
- **Warm** -- vCPUs paused, instant resume
- **Sleeping** -- snapshotted to disk, ~1s wake

## Updating Images

To deploy a new version:

1. Modify your flake
2. Re-build: `mvm pool build acme/workers`
3. New instances use the updated revision automatically
4. Existing running instances continue on the old revision until restarted

## Rollback

If a new revision is broken:

```bash
mvm pool rollback acme/workers --revision <hash>
```

This updates the `current` symlink without rebuilding.
