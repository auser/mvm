pub type MvmResult<T> = std::result::Result<T, MvmError>;

#[derive(Debug, thiserror::Error)]
pub enum MvmError {
    #[error("runtime error: {0}")]
    Runtime(String),

    #[error("sandbox error: {0}")]
    Sandbox(#[from] microsandbox::MicrosandboxError),

    #[error("{0}")]
    Other(#[from] anyhow::Error),
}
