use anyhow::Result;

pub mod sandbox;

use crate::ExecutionContext;
use async_trait::async_trait;

#[async_trait]
pub trait Backend<T, E>: Send + Sync
where
    E: ExecutionContext + ?Sized,
{
    async fn prepare(&mut self, env: &E) -> Result<T>;
    async fn boot(&mut self, env: &E) -> Result<T>;
    async fn teardown(&mut self, env: &E) -> Result<T>;
}
