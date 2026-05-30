---
title: Image registry configuration
description: OCI registry use as a compatibility path.
---

OCI images are compatibility inputs. Nix-built microVM artifacts remain the preferred production path.

## Production posture

- Pin image references by digest.
- Record pulled artifact identity.
- Keep mutable tags for local development only.
- Scope caches by policy and tenant where applicable.
- Verify layer digests before use.

## Security notes

Pulling an image is not the same thing as trusting it. Launch still needs policy admission, artifact evidence, and runtime audit.

Use the [Nix and OCI guide](/guides/nix-and-oci/) for the full positioning.
