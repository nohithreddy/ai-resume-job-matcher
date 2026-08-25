use thiserror::Error;

#[derive(Debug, Error)]
pub enum DomainError {
    #[error("entity not found")]
    NotFound,
    #[error("entity already exists")]
    Conflict,
    #[error("authentication required")]
    Unauthorized,
    #[error("access forbidden")]
    Forbidden,
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("embedding dimensions do not match")]
    EmbeddingDimensionMismatch,
    #[error("embedding contains invalid values")]
    InvalidEmbedding,
    #[error("dependency unavailable: {0}")]
    DependencyUnavailable(String),
    #[error("internal error: {0}")]
    Internal(String),
}
