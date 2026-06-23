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
            // Never block on an interactive credential/passphrase prompt: fail
            // fast instead of hanging a background thread with no terminal.
            .env("GIT_TERMINAL_PROMPT", "0")
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
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(&arg_vec)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| Error::Spawn { source })?;

        // Write the whole input, then drop the handle to signal EOF. Ignore a
        // write error: if git exited early (e.g. it rejected the patch) the
        // pipe breaks here, but git's real status + stderr are the authoritative
        // signal — so always wait and report those rather than a generic
        // broken-pipe error.
        {
            let mut stdin = child.stdin.take().expect("stdin was piped");
            let _ = stdin.write_all(input);
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

    /// Run `git <args>` and report whether it exited successfully, without
    /// treating a non-zero exit as an error. For predicate commands such as
    /// `git diff --quiet` (exit 1 means "there are differences").
    pub fn succeeds<I, S>(&self, args: I) -> Result<bool>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let status = Command::new("git")
            .arg("-C")
            .arg(&self.workdir)
            .args(["-c", "core.quotepath=false"])
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|source| Error::Spawn { source })?;
        Ok(status.success())
    }

    /// Like [`run`](Self::run) but a non-zero exit yields `Ok(None)` rather than
    /// an error — for queries where "no result" is expected (an unset config
    /// key, a branch with no upstream, …).
    pub fn run_optional<I, S>(&self, args: I) -> Result<Option<GitOutput>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.workdir)
            .args(["-c", "core.quotepath=false"])
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(args)
            .output()
            .map_err(|source| Error::Spawn { source })?;
        if !output.status.success() {
            return Ok(None);
        }
        Ok(Some(GitOutput {
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            stdout: output.stdout,
        }))
    }

    /// Read a single git config value (`git config --get <key>`), `None` if
    /// unset.
    pub fn config_get(&self, key: &str) -> Result<Option<String>> {
        Ok(self.run_optional(["config", "--get", key])?.and_then(|o| {
            let v = String::from_utf8_lossy(&o.stdout).trim().to_string();
            (!v.is_empty()).then_some(v)
        }))
    }
}
