mod common;

use std::path::Path;
use std::process::Command;

use common::TestRepo;
use magritte_core::transient::{push_transient, Suffix};
use magritte_core::{Command as GitCommand, Repo};

/// Run git in an arbitrary dir with isolated config (for the bare remote).
fn git_in(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A test repo wired to a fresh bare remote named `origin`.
fn repo_with_remote() -> (TestRepo, tempfile::TempDir) {
    let t = TestRepo::new();
    t.write("README.md", "hello\n");
    t.commit_all("initial");

    let remote = tempfile::tempdir().unwrap();
    git_in(remote.path(), &["init", "--bare", "--initial-branch=main"]);
    t.git(["remote", "add", "origin", remote.path().to_str().unwrap()]);
    (t, remote)
}

#[test]
fn push_transient_defines_force_and_actions() {
    let tr = push_transient();
    assert!(tr.switches().any(|s| s.arg == "--force-with-lease"));
    assert!(tr.action_for("p").is_some());
    let action_count = tr
        .groups
        .iter()
        .flat_map(|g| &g.suffixes)
        .filter(|s| matches!(s, Suffix::Action(_)))
        .count();
    assert!(action_count >= 1);
}

#[test]
fn current_branch_is_reported() {
    let t = TestRepo::new();
    t.write("f", "x\n");
    t.commit_all("c");
    let repo = Repo::discover(t.path()).unwrap();
    assert_eq!(repo.current_branch().unwrap().as_deref(), Some("main"));
}

#[test]
fn push_set_upstream_delivers_commits() {
    let (t, remote) = repo_with_remote();
    let repo = Repo::discover(t.path()).unwrap();

    repo.execute(GitCommand::PushSetUpstream, &[]).unwrap();

    // The bare remote now has main at our HEAD.
    let local_head = t.git(["rev-parse", "HEAD"]);
    let remote_head = git_in(remote.path(), &["rev-parse", "main"]);
    assert_eq!(local_head, remote_head);

    // And upstream tracking is configured.
    let upstream = t.git(["rev-parse", "--abbrev-ref", "main@{upstream}"]);
    assert_eq!(upstream, "origin/main");
}

#[test]
fn dry_run_switch_does_not_deliver() {
    let (t, remote) = repo_with_remote();
    let repo = Repo::discover(t.path()).unwrap();
    // First establish the branch on the remote.
    repo.execute(GitCommand::PushSetUpstream, &[]).unwrap();

    // A new local commit, pushed with --dry-run, must not reach the remote.
    t.write("README.md", "hello world\n");
    t.commit_all("second");
    repo.execute(GitCommand::Push, &["--dry-run".to_string()])
        .unwrap();

    let local_head = t.git(["rev-parse", "HEAD"]);
    let remote_head = git_in(remote.path(), &["rev-parse", "main"]);
    assert_ne!(local_head, remote_head, "dry-run should not push");
}
