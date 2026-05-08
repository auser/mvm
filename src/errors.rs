pub type MvRuntimeResult<T> = std::result::Result<T, MvRuntimeError>;

#[derive(Debug, thiserror::Error)]
pub enum MvRuntimeError {
    #[error("runtime error: {0}")]
    Runtime(String),

    #[error("{0}")]
    Other(String),
}
