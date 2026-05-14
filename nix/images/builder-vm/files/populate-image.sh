#!/usr/bin/env bash
# `populateImageCommands` body for nixpkgs's `make-ext4-fs.nix`.
# Invoked from `../flake.nix::rootfsImage`.
#
# make-ext4-fs.nix runs this snippet inside its own build env, with
# CWD set to a workdir that contains the staging tree at `./files/`.
# Our job is to drop the assembled rootfs tree into that staging
# dir; make-ext4-fs.nix then runs `mkfs.ext4` against it.
#
# Required env var:
#   rootfsTree   Path to the runCommand-built tree from
#                ./assemble-rootfs.sh. The flake's call site
#                substitutes the store path before invoking bash.

set -euo pipefail

cp -a --reflink=auto "$rootfsTree"/. ./files/
