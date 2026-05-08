use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::{Backend, ExecutionContext};
use microsandbox::Sandbox as MicroSandbox;

pub struct SandboxBackend<'a> {
    pub(crate) build_run_dir: &'a str,
    pub(crate) sandbox: Option<MicroSandbox>,
}

impl<'a> SandboxBackend<'a> {
    pub fn new(build_run_dir: &'a str) -> Self {
        Self {
            build_run_dir,
            sandbox: None,
        }
    }
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct SandboxContext {
    /// Name for this sandbox context
    pub name: String,
    /// Image to use for this sandbox context
    pub image: String,
    pub cpus: u8,
    pub memory: u64,
}

impl ExecutionContext for SandboxContext {}

impl SandboxContext {
    pub fn builder() -> SandboxContextBuilder {
        SandboxContextBuilder::default()
    }
}

#[derive(Clone)]
pub struct SandboxContextBuilder {
    image: String,
    name: String,
    pub(crate) cpus: u8,
    pub(crate) memory: u64,
}

impl Default for SandboxContextBuilder {
    fn default() -> Self {
        Self {
            image: String::new(),
            name: String::new(),
            cpus: 1,
            memory: 1024 * 1024 * 1024,
        }
    }
}

impl SandboxContextBuilder {
    pub fn image(mut self, image: String) -> Self {
        self.image = image;
        self
    }

    pub fn name(mut self, name: String) -> Self {
        self.name = name;
        self
    }

    pub fn cpus(mut self, cpus: u8) -> Self {
        self.cpus = cpus;
        self
    }

    pub fn memory(mut self, memory: u64) -> Self {
        self.memory = memory;
        self
    }

    pub fn build(self) -> SandboxContext {
        SandboxContext {
            name: self.name,
            image: self.image,
            cpus: self.cpus,
            memory: self.memory,
        }
    }
}

impl<'a> Backend<MicroSandbox, SandboxContext> for SandboxBackend<'a> {
    async fn prepare(&mut self, env: &SandboxContext) -> Result<MicroSandbox> {
        let sb = MicroSandbox::builder(env.name.clone())
            .image(env.image.clone())
            .create()
            .await?;
        Ok(sb)
    }

    async fn boot(&mut self, env: &SandboxContext) -> Result<MicroSandbox> {
        todo!();
    }

    async fn teardown(&mut self, env: &SandboxContext) -> Result<MicroSandbox> {
        todo!();
    }
}
