use std::fmt;
use std::path::PathBuf;

/// Errors produced by the core git engine.
#[derive(Debug)]
pub enum Error {
    /// The `git` binary could not be spawned (not installed, not on PATH, etc.).
    Spawn { source: std::io::Error },
    /// A git invocation ran but exited non-zero.
    Git {
        args: Vec<String>,
        status: Option<i32>,
        stderr: String,
    },
    /// Output that should have been UTF-8 was not (e.g. a branch header).
    Encoding { context: &'static str },
    /// A porcelain record did not match the format we expect from this git version.
    Parse { context: &'static str, line: String },
    /// The given path is not inside a git working tree.
    NotARepository { path: PathBuf },
    /// A precondition for an operation was not met (e.g. detached HEAD).
    Message(String),
    /// The invocation was cancelled (superseded or user-requested) and the
    /// child process killed before it finished.
    Cancelled,
    /// The invocation exceeded its time budget and the child process was killed.
    TimedOut,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Spawn { source } => write!(f, "failed to spawn git: {source}"),
            Error::Git {
                args,
                status,
                stderr,
            } => {
                let code = status
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into());
                write!(
                    f,
                    "git {} exited with {code}: {}",
                    args.join(" "),
                    stderr.trim()
                )
            }
            Error::Encoding { context } => write!(f, "invalid utf-8 in {context}"),
            Error::Parse { context, line } => {
                write!(f, "failed to parse {context}: {line:?}")
            }
            Error::NotARepository { path } => {
                write!(f, "not a git repository: {}", path.display())
            }
            Error::Message(msg) => write!(f, "{msg}"),
            Error::Cancelled => write!(f, "cancelled"),
            Error::TimedOut => write!(f, "timed out"),
        }
    }
}

impl Error {
    /// Whether this error means the `git` binary itself is missing (not
    /// installed or not on `PATH`) — a spawn failure with `ErrorKind::NotFound`,
    /// as opposed to any other spawn or run failure.
    pub fn is_git_missing(&self) -> bool {
        matches!(self, Error::Spawn { source } if source.kind() == std::io::ErrorKind::NotFound)
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
