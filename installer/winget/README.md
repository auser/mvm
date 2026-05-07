# Windows winget manifest for mvm

Plan 53 / Sprint 48 / Plan I.3.

## Status

The three manifest files in this directory (`Mvm.Mvmctl.installer.yaml`,
`Mvm.Mvmctl.locale.en-US.yaml`, `Mvm.Mvmctl.yaml`) are the **template**
that gets submitted to the [winget-pkgs](https://github.com/microsoft/winget-pkgs)
community repository when each release of `mvmctl` ships.

They are not used at build time — winget pulls them from the Microsoft
repository, not from this directory. We keep them in-tree so:

1. Reviewers can see exactly what the published manifest looks like.
2. The release script (`xtask release`) can stamp the new version +
   SHA-256 into them and open a PR against winget-pkgs without the
   release engineer having to recreate the YAML by hand each time.

## What's deferred

- **Code signing.** The current manifest points at the unsigned
  `mvmctl.exe` from the GitHub release. winget accepts unsigned
  binaries but flags them in the install UI. A signed MSI is a future
  enhancement (~$300/yr cert + signing automation).
- **Submission automation.** Today an mvm release-engineer manually
  opens the winget-pkgs PR. Plan 53 doesn't track automating this —
  it's noisy CI work disproportionate to the volume of mvm releases.

See [`public/.../install/windows.md`](../../public/src/content/docs/install/windows.md)
for the user-facing install flow. Once `winget install Mvm.Mvmctl` is
live (i.e., the manifest has been merged upstream), the install doc
can promote it to the primary path.
