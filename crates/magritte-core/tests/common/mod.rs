use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

/// A throwaway git repository in a temp dir, isolated from the user's global
/// and system git config for deterministic tests.
pub struct TestRepo {
    pub dir: TempDir,
}

impl TestRepo {
    pub fn new() -> TestRepo {
        let dir = tempfile::tempdir().expect("create temp dir");
        let repo = TestRepo { dir };
        repo.git(["init", "--initial-branch=main"]);
        repo.git(["config", "user.name", "Test"]);
        repo.git(["config", "user.email", "test@example.com"]);
        repo.git(["config", "commit.gpgsign", "false"]);
        repo
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Run a git command in the repo, asserting success. Returns trimmed stdout.
    pub fn git<I, S>(&self, args: I) -> String
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let output = Command::new("git")
            .arg("-C")
            .arg(self.path())
            // Isolate from the developer's real git configuration.
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .args(args)
            .output()
            .expect("spawn git");
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    pub fn write(&self, rel: &str, contents: &str) {
        let path = self.path().join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(path, contents).expect("write file");
    }

    pub fn commit_all(&self, message: &str) {
        self.git(["add", "-A"]);
        self.git(["commit", "-m", message]);
    }
}
