use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::{Error, Result};

/// A handle to a git working tree.
///
/// `Repo` is deliberately synchronous and cheap to clone: it holds only the
/// working-directory path. Every method shells out to the `git` binary and
/// returns plain data. The frontend is responsible for running these calls off
/// the UI thread (e.g. on a background executor) and for cancellation.
#[derive(Debug, Clone)]
pub struct Repo {
    workdir: PathBuf,
}

/// The raw result of a git invocation.
pub struct GitOutput {
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl Repo {
    /// Open the working tree that `path` belongs to, resolving to the top level.
    ///
    /// Returns [`Error::NotARepository`] if `path` is not tracked by git.
    pub fn discover(path: impl AsRef<Path>) -> Result<Repo> {
        let path = path.as_ref();
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .map_err(|source| Error::Spawn { source })?;

        if !output.status.success() {
            return Err(Error::NotARepository {
                path: path.to_path_buf(),
            });
        }

        let top = String::from_utf8(output.stdout)
            .map_err(|_| Error::Encoding {
                context: "rev-parse --show-toplevel",
            })?
            .trim_end()
            .to_string();

        Ok(Repo {
            workdir: PathBuf::from(top),
        })
    }

    /// Construct a `Repo` for an already-known working-tree root without probing.
    pub fn at(workdir: impl Into<PathBuf>) -> Repo {
        Repo {
            workdir: workdir.into(),
        }
    }

    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    /// Run `git <args>` in the working tree, returning stdout as raw bytes so
    /// that NUL-delimited (`-z`) output is preserved.
    pub fn run<I, S>(&self, args: I) -> Result<GitOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let arg_vec: Vec<String> = args
            .into_iter()
            .map(|s| s.as_ref().to_string_lossy().into_owned())
            .collect();

        let output = Command::new("git")
            .arg("-C")
            .arg(&self.workdir)
            // Keep output stable and machine-readable regardless of user config.
            .args(["-c", "core.quotepath=false"])
            .args(&arg_vec)
            .output()
            .map_err(|source| Error::Spawn { source })?;

        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

        if !output.status.success() {
            return Err(Error::Git {
                args: arg_vec,
                status: output.status.code(),
                stderr,
            });
        }

        Ok(GitOutput {
            stdout: output.stdout,
            stderr,
        })
    }

    /// Like [`run`](Self::run) but feeds `input` to git's stdin. Used to pipe
    /// patches to `git apply`.
    pub fn run_with_input<I, S>(&self, args: I, input: &[u8]) -> Result<GitOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let arg_vec: Vec<String> = args
            .into_iter()
            .map(|s| s.as_ref().to_string_lossy().into_owned())
            .collect();

        let mut child = Command::new("git")
            .arg("-C")
            .arg(&self.workdir)
            .args(["-c", "core.quotepath=false"])
            .args(&arg_vec)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| Error::Spawn { source })?;

        // Write the whole input, then drop the handle to signal EOF.
        {
            let mut stdin = child.stdin.take().expect("stdin was piped");
            stdin
                .write_all(input)
                .map_err(|source| Error::Spawn { source })?;
        }

        let output = child
            .wait_with_output()
            .map_err(|source| Error::Spawn { source })?;
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

        if !output.status.success() {
            return Err(Error::Git {
                args: arg_vec,
                status: output.status.code(),
                stderr,
            });
        }

        Ok(GitOutput {
            stdout: output.stdout,
            stderr,
        })
    }
}
