use thiserror::Error;

// covers basically every way a package op can go wrong
#[derive(Error, Debug)]
pub enum Error {
    #[error("Package not found: {0}")]
    PackageNotFound(String),

    #[error("Package already installed: {0}")]
    AlreadyInstalled(String),

    #[error("Dependency resolution failed: {0}")]
    DependencyError(String),

    #[error("Transaction failed: {0}")]
    TransactionError(String),

    #[error("Database error: {0}")]
    DatabaseError(String),

    #[error("Network error: {0}")]
    NetworkError(String),

    #[error("Permission denied: {0}")]
    PermissionDenied(String),

    #[error("Backend not available: {0}")]
    BackendUnavailable(String),

    #[error("Operation cancelled")]
    Cancelled,

    #[error("Invalid configuration: {0}")]
    ConfigError(String),

    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
