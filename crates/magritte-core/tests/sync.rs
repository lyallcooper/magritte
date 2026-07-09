mod common;

use std::path::Path;
use std::process::Command;

use common::TestRepo;
use magritte_core::transient::push_transient;
use magritte_core::{RemoteTargets, Repo};

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
    let tr = push_transient(&RemoteTargets::default());
    assert!(tr.switches().any(|s| s.arg == "--force-with-lease"));
    // push-remote / upstream / elsewhere.
    assert!(tr.action_for("p").is_some());
    assert!(tr.action_for("u").is_some());
    assert!(tr.action_for("e").is_some());
}

#[test]
fn push_transient_labels_resolved_targets() {
    let (t, _remote) = repo_with_remote();
    let repo = Repo::discover(t.path()).unwrap();
    repo.push_to("origin", "main", true, &[]).unwrap();

    let targets = repo.remote_targets().unwrap();
    assert_eq!(targets.branch.as_deref(), Some("main"));
    assert_eq!(
        targets.upstream.as_ref().map(|u| u.display()),
        Some("origin/main".to_string())
    );

    let tr = push_transient(&targets);
    // The upstream action names the resolved branch.
    match tr.action_for("u") {
        Some(a) => assert_eq!(a.description, "origin/main"),
        None => panic!("missing upstream action"),
    }
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

    repo.push_to("origin", "main", true, &[]).unwrap();

    // The bare remote now has main at our HEAD.
    let local_head = t.git(["rev-parse", "HEAD"]);
    let remote_head = git_in(remote.path(), &["rev-parse", "main"]);
    assert_eq!(local_head, remote_head);

    // And upstream tracking is configured.
    let upstream = t.git(["rev-parse", "--abbrev-ref", "main@{upstream}"]);
    assert_eq!(upstream, "origin/main");
}

#[test]
fn push_ref_creates_new_remote_branch() {
    let (t, remote) = repo_with_remote();
    let repo = Repo::discover(t.path()).unwrap();

    // Push the local `main` to a differently-named, not-yet-existing target.
    repo.push_ref("origin", "main", "feature", &[]).unwrap();

    // The remote gained `feature` at our HEAD, and our remote-tracking refs
    // (after a fetch) list it as `origin/feature`.
    let local_head = t.git(["rev-parse", "HEAD"]);
    let remote_head = git_in(remote.path(), &["rev-parse", "feature"]);
    assert_eq!(local_head, remote_head);

    repo.fetch_from("origin", &[]).unwrap();
    let branches = repo.remote_branches().unwrap();
    assert!(
        branches.iter().any(|b| b == "origin/feature"),
        "expected origin/feature in {branches:?}"
    );
}

#[test]
fn push_tag_delivers_one_tag() {
    let (t, remote) = repo_with_remote();
    let repo = Repo::discover(t.path()).unwrap();
    t.git(["tag", "v1.0.0"]);
    t.git(["tag", "v1.1.0"]);

    repo.push_tag("origin", "v1.0.0", &[]).unwrap();

    let tags = git_in(remote.path(), &["tag", "--list"]);
    assert_eq!(tags, "v1.0.0", "only the pushed tag arrives");
}

#[test]
fn push_all_tags_delivers_every_tag() {
    let (t, remote) = repo_with_remote();
    let repo = Repo::discover(t.path()).unwrap();
    t.git(["tag", "v1.0.0"]);
    t.git(["tag", "v1.1.0"]);

    repo.push_all_tags("origin", &[]).unwrap();

    let tags = git_in(remote.path(), &["tag", "--list"]);
    assert_eq!(tags, "v1.0.0\nv1.1.0");
}

#[test]
fn dry_run_switch_does_not_deliver() {
    let (t, remote) = repo_with_remote();
    let repo = Repo::discover(t.path()).unwrap();
    // First establish the branch on the remote.
    repo.push_to("origin", "main", true, &[]).unwrap();

    // A new local commit, pushed with --dry-run, must not reach the remote.
    t.write("README.md", "hello world\n");
    t.commit_all("second");
    repo.push_to("origin", "main", false, &["--dry-run".to_string()])
        .unwrap();

    let local_head = t.git(["rev-parse", "HEAD"]);
    let remote_head = git_in(remote.path(), &["rev-parse", "main"]);
    assert_ne!(local_head, remote_head, "dry-run should not push");
}
