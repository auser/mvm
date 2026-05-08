use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use crate::{errors::MvRuntimeError, runtime::ProcessHandle};

use super::SandboxConfig;

#[derive(Clone)]
pub struct Sandbox {
    config: SandboxConfig,
    handle: Option<Arc<Mutex<ProcessHandle>>>,
}

impl Sandbox {
    pub fn builder() -> SandboxBuilder {
        SandboxBuilder::default()
    }
}

pub struct SandboxBuilder {
    config: SandboxConfig,
    build_error: Option<MvRuntimeError>,
}

impl Default for SandboxBuilder {
    fn default() -> Self {
        Self::new("default")
    }
}

impl SandboxBuilder {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            config: SandboxConfig {
                name: name.into(),
                ..Default::default()
            },
            build_error: None,
        }
    }
}
