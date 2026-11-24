//! Headless error type replacing cc-switch's `crate::error::AppError` for the
//! ported modules. Only the variants/constructors used by the ported code exist.
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum AppError {
    Io(PathBuf, std::io::Error),
    Message(String),
}

impl AppError {
    pub fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io(path.to_path_buf(), source)
    }

    pub fn message(msg: impl Into<String>) -> Self {
        Self::Message(msg.into())
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(path, source) => write!(f, "{}: {source}", path.display()),
            Self::Message(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for AppError {}
