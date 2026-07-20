use std::fmt;

/// Errors produced by the shared foundation: process execution and parsing.
/// Engine crates wrap these in their own richer error types (which carry the
/// failing argv, VCS-specific context, etc.).
#[derive(Debug)]
pub enum Error {
    /// The child binary could not be spawned (not installed, not on PATH, etc.).
    Spawn { source: std::io::Error },
    /// The invocation was cancelled (superseded or user-requested) and the
    /// child process killed before it finished.
    Cancelled,
    /// The invocation exceeded its time budget and the child process was killed.
    TimedOut,
    /// A machine-format record did not match the expected shape.
    Parse { context: &'static str, line: String },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Spawn { source } => write!(f, "failed to spawn: {source}"),
            Error::Cancelled => write!(f, "cancelled"),
            Error::TimedOut => write!(f, "timed out"),
            Error::Parse { context, line } => write!(f, "failed to parse {context}: {line:?}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Spawn { source } => Some(source),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
