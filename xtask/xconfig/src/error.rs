use std::path::PathBuf;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum KconfigError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Syntax error at {file}:{line}: {message}")]
    Syntax {
        file: PathBuf,
        line: usize,
        message: String,
    },

    #[error("Circular dependency detected: {chain}")]
    CircularDependency { chain: String },

    #[error("File not found: {0}")]
    FileNotFound(PathBuf),

    #[error("Undefined symbol: {0}")]
    UndefinedSymbol(String),

    #[error("Type mismatch: expected {expected}, got {actual}")]
    TypeMismatch { expected: String, actual: String },

    #[error("Invalid expression: {0}")]
    InvalidExpression(String),

    #[error("Parse error: {0}")]
    Parse(String),

    #[error("Config error: {0}")]
    Config(String),

    #[error("Recursive source inclusion detected: {chain}")]
    RecursiveSource { chain: String },
}

pub type Result<T> = std::result::Result<T, KconfigError>;
