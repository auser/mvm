# Example: Web Application Fleet

This example deploys a fleet of Nginx + application instances across a tenant.

## Setup

### 1. Create the Tenant

```bash
mvm tenant create web-prod \
  --net-id 10 \
  --subnet 10.240.10.0/24 \
  --max-vcpus 32 \
  --max-mem 65536 \
  --max-running 8 \
  --max-warm 4
```

### 2. Create the Flake

Create a `flake.nix` for the web worker image:

```nix
{
  description = "Web fleet worker image";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-24.05";

  outputs = { self, nixpkgs }: let
    system = "x86_64-linux";
    pkgs = nixpkgs.legacyPackages.${system};
  in {
    packages.${system}.web = pkgs.buildEnv {
      name = "mvm-web";
      paths = with pkgs; [
        nginx
        bash
        coreutils
        curl
        cacert
      ];
    };
  };
}
```

### 3. Create and Build the Pool

```bash
mvm pool create web-prod/frontend \
  --flake ./web-flake \
  --profile web \
  --cpus 2 \
  --mem 2048 \
  --data-disk 512

mvm pool build web-prod/frontend
```

### 4. Scale Up

```bash
mvm pool scale web-prod/frontend --running 4 --warm 2
```

This launches 4 running instances and keeps 2 warm (instant failover).

## Monitoring

```bash
# List all instances
mvm instance list --tenant web-prod

# Check a specific instance
mvm instance stats web-prod/frontend/<instance-id>

# Verify network
mvm net verify
```

## Desired State (Agent-Managed)

For automated management via the agent, create a desired state file:

```json
{
  "schema_version": 1,
  "node_id": "web-node-01",
  "tenants": [
    {
      "tenant_id": "web-prod",
      "network": {
        "tenant_net_id": 10,
        "ipv4_subnet": "10.240.10.0/24"
      },
      "quotas": {
        "max_vcpus": 32,
        "max_mem_mib": 65536,
        "max_running": 8,
        "max_warm": 4,
        "max_pools": 4,
        "max_instances_per_pool": 16,
        "max_disk_gib": 100
      },
      "pools": [
        {
          "pool_id": "frontend",
          "flake_ref": "./web-flake",
          "profile": "web",
          "instance_resources": {
            "vcpus": 2,
            "mem_mib": 2048,
            "data_disk_mib": 512
          },
          "desired_counts": {
            "running": 4,
            "warm": 2,
            "sleeping": 0
          }
        }
      ]
    }
  ],
  "prune_unknown_tenants": false,
  "prune_unknown_pools": false
}
```

Push to the agent:

```bash
mvm agent reconcile --desired desired.json
```

Or run as a daemon:

```bash
mvm agent serve --desired desired.json --interval-secs 30
```

## Traffic Routing

Instances are on `10.240.10.0/24`. Within the Lima VM, set up a load balancer (e.g., HAProxy) on the host bridge to distribute traffic:

```
10.240.10.1 (gateway/bridge)  --> HAProxy
10.240.10.3 (instance 1)      --> Nginx
10.240.10.4 (instance 2)      --> Nginx
10.240.10.5 (instance 3)      --> Nginx
10.240.10.6 (instance 4)      --> Nginx
```

## Scaling Down

During low-traffic periods, reduce running instances and sleep the rest:

```bash
mvm pool scale web-prod/frontend --running 2 --warm 1 --sleeping 3
```

Sleeping instances save memory while preserving state for ~1s wake-up.
