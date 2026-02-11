# Example: Ephemeral CI Runner Pool

This example creates a pool of ephemeral CI runner microVMs that process build jobs and are destroyed after each run.

## Setup

### 1. Create the Tenant

```bash
mvm tenant create ci \
  --net-id 20 \
  --subnet 10.240.20.0/24 \
  --max-vcpus 64 \
  --max-mem 131072 \
  --max-running 16 \
  --max-warm 4
```

### 2. Create the CI Runner Flake

```nix
{
  description = "CI runner image with build tools";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-24.05";

  outputs = { self, nixpkgs }: let
    system = "x86_64-linux";
    pkgs = nixpkgs.legacyPackages.${system};
  in {
    packages.${system}.ci-runner = pkgs.buildEnv {
      name = "mvm-ci-runner";
      paths = with pkgs; [
        bash
        coreutils
        git
        curl
        wget
        gnumake
        gcc
        nodejs
        python3
        docker-client
        jq
        cacert
        openssh
      ];
    };
  };
}
```

### 3. Create and Build

```bash
mvm pool create ci/runners \
  --flake ./ci-flake \
  --profile ci-runner \
  --cpus 4 \
  --mem 4096 \
  --data-disk 2048

mvm pool build ci/runners
```

### 4. Scale for CI Workload

```bash
# Keep a few warm for instant job pickup
mvm pool scale ci/runners --running 0 --warm 4 --sleeping 8
```

## Workflow

The CI controller (external system) manages the instance lifecycle:

1. **Job arrives**: Wake or start an instance
   ```bash
   mvm instance wake ci/runners/<id>
   # or remotely via coordinator
   mvm coordinator wake --node 10.0.1.5:4433 --tenant ci --pool runners --instance <id>
   ```

2. **Job runs**: SSH into the instance and execute the build
   ```bash
   mvm instance ssh ci/runners/<id>
   ```

3. **Job completes**: Sleep the instance (preserving build caches in memory)
   ```bash
   mvm instance sleep ci/runners/<id>
   ```

4. **Periodic cleanup**: Destroy and recreate instances to start fresh
   ```bash
   mvm instance destroy ci/runners/<id>
   mvm instance create ci/runners
   ```

## Desired State for Auto-Scaling

```json
{
  "schema_version": 1,
  "node_id": "ci-node-01",
  "tenants": [
    {
      "tenant_id": "ci",
      "network": {
        "tenant_net_id": 20,
        "ipv4_subnet": "10.240.20.0/24"
      },
      "quotas": {
        "max_vcpus": 64,
        "max_mem_mib": 131072,
        "max_running": 16,
        "max_warm": 4,
        "max_pools": 2,
        "max_instances_per_pool": 32,
        "max_disk_gib": 200
      },
      "pools": [
        {
          "pool_id": "runners",
          "flake_ref": "./ci-flake",
          "profile": "ci-runner",
          "instance_resources": {
            "vcpus": 4,
            "mem_mib": 4096,
            "data_disk_mib": 2048
          },
          "desired_counts": {
            "running": 0,
            "warm": 4,
            "sleeping": 8
          }
        }
      ]
    }
  ]
}
```

## Cost Optimization

- **Warm instances** consume memory but no CPU -- instant job startup (<100ms)
- **Sleeping instances** consume only disk -- ~1s wake time, good for queued jobs
- **Running count at 0** during idle periods saves all compute resources
- The sleep policy automatically transitions idle runners from warm to sleeping
