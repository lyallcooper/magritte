//! Gitignore rules — magit's `i` transient. Append a pattern to one of the four
//! ignore files, creating it if needed and staging the tracked ones (so the new
//! rule joins the next commit, as magit does).

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::repo::Repo;

/// Where to write a gitignore rule (mirrors magit's gitignore transient).
#[derive(Debug, Clone)]
pub enum IgnoreDest {
    /// Shared, tracked `<toplevel>/.gitignore`.
    Toplevel,
    /// Shared, tracked `.gitignore` in a toplevel-relative subdirectory.
    Subdir(PathBuf),
    /// Private to this clone: `$GIT_DIR/info/exclude`.
    Private,
    /// All of the user's repos: the file named by `core.excludesFile`.
    Global,
}

impl Repo {
    /// The path `core.excludesFile` points at (a leading `~` expanded), or
    /// `None` if it isn't set — global ignore is unavailable without it.
    pub fn global_excludes_file(&self) -> Result<Option<PathBuf>> {
        Ok(self
            .config_get("core.excludesFile")?
            .map(|raw| expand_tilde(raw.trim())))
    }

    /// Append `rule` to the gitignore file for `dest`, creating it (and any
    /// parent directories) if absent. The tracked files (toplevel/subdir) are
    /// staged afterward so the new rule is part of the next commit.
    pub fn add_ignore_rule(&self, rule: &str, dest: IgnoreDest) -> Result<()> {
        let rule = rule.trim();
        if rule.is_empty() {
            return Err(Error::Message("nothing to ignore".into()));
        }
        let (path, stage) = match dest {
            IgnoreDest::Toplevel => (self.workdir().join(".gitignore"), true),
            IgnoreDest::Subdir(dir) => (self.workdir().join(dir).join(".gitignore"), true),
            IgnoreDest::Private => (self.git_dir()?.join("info").join("exclude"), false),
            IgnoreDest::Global => match self.global_excludes_file()? {
                Some(path) => (path, false),
                None => return Err(Error::Message("core.excludesFile is not set".into())),
            },
        };
        append_line(&path, rule)?;
        if stage {
            let path = path.to_string_lossy().into_owned();
            self.run(["add", path.as_str()])?;
        }
        Ok(())
    }
}

/// Append `rule` as its own line, first giving the existing content a trailing
/// newline if it lacks one, and creating the file (and parent dirs) if absent.
fn append_line(path: &Path, rule: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| io_error(path, e))?;
    }
    let mut content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(io_error(path, e)),
    };
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(rule);
    content.push('\n');
    fs::write(path, content).map_err(|e| io_error(path, e))
}

fn io_error(path: &Path, e: std::io::Error) -> Error {
    Error::Message(format!("{}: {e}", path.display()))
}

/// Expand a leading `~/` against `$HOME`; leave any other path untouched.
fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(s)
}
