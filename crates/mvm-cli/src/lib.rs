// mvm-cli: Clap commands, UI, bootstrap
// Depends on mvm-core, mvm, mvm-build

pub mod bootstrap;
// plan 72 W5 — Layer-1 builder VM image acquisition (source-checkout
// cache lookup + release-download stub). `find_builder_vm_flake` is
// unconditional (useful from `mvmctl doctor` to surface "is this a
// source checkout?"); the `ensure_builder_vm_image` resolver gates
// behind `backends-builder-vm-libkrun` because it returns a
// `BuilderVmImage` from that feature's gated module.
pub mod builder_vm_image;
pub mod commands;
pub mod config_watcher;
pub mod doctor;
pub mod exec;
pub mod http;
pub mod logging;
pub mod metrics_server;
pub mod security_cmd;
pub mod shell_init;
pub mod template_cmd;
pub mod ui;
pub mod update;
pub mod watch;

pub use commands::run;
