mod common;

use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use common::TestRepo;
use magritte_core::{Error, Repo};

fn open(t: &TestRepo) -> Repo {
    Repo::discover(t.path()).expect("discover repo")
}

/// Install a `pre-commit` hook that blocks for `secs`, so `git commit` (which
/// runs through `Repo::run`) hangs deterministically until cancel/timeout.
fn install_blocking_hook(t: &TestRepo, secs: u32) {
    use std::os::unix::fs::PermissionsExt;
    let hook = t.path().join(".git/hooks/pre-commit");
    std::fs::write(&hook, format!("#!/bin/sh\nsleep {secs}\n")).unwrap();
    let mut perms = std::fs::metadata(&hook).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&hook, perms).unwrap();
}

/// A repo with one commit, a blocking pre-commit hook, and a staged change ready
/// to commit (which will hang in the hook).
fn repo_with_pending_blocked_commit() -> TestRepo {
    let t = TestRepo::new();
    t.write("f", "a\n");
    t.commit_all("init");
    install_blocking_hook(&t, 5);
    t.write("f", "b\n");
    t.git(["add", "f"]);
    t
}

#[test]
fn timeout_kills_a_blocking_command() {
    let t = repo_with_pending_blocked_commit();
    let repo = open(&t).with_timeout(Duration::from_millis(300));
    let start = Instant::now();
    let res = repo.run(["commit", "-m", "x"]);
    assert!(
        matches!(res, Err(Error::TimedOut)),
        "expected TimedOut, got {res:?}"
    );
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "should bail near the deadline, took {:?}",
        start.elapsed()
    );
}

#[test]
fn cancel_kills_a_blocking_command_promptly() {
    let t = repo_with_pending_blocked_commit();
    let (repo, cancel) = open(&t).cancellable();
    let worker = thread::spawn(move || {
        let start = Instant::now();
        (repo.run(["commit", "-m", "x"]), start.elapsed())
    });
    thread::sleep(Duration::from_millis(300));
    cancel.store(true, Ordering::Relaxed);
    let (res, elapsed) = worker.join().unwrap();
    assert!(
        matches!(res, Err(Error::Cancelled)),
        "expected Cancelled, got {res:?}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "should stop soon after cancel, took {elapsed:?}"
    );
    // The child was reaped (not holding the index lock): a later op succeeds.
    assert!(open(&t).run(["status", "--porcelain"]).is_ok());
}

#[test]
fn cancellable_path_drains_large_output_without_truncating() {
    // > 256 KB of stdout: the case that deadlocks a poll loop which doesn't
    // drain the pipe while waiting.
    let t = TestRepo::new();
    let big = "abcdefgh\n".repeat(60_000); // ~540 KB
    t.write("big.txt", &big);
    t.commit_all("big");

    let fast = open(&t).run(["show", "HEAD:big.txt"]).unwrap().stdout;
    // A generous timeout forces the killable path but lets the command finish.
    let drained = open(&t)
        .with_timeout(Duration::from_secs(30))
        .run(["show", "HEAD:big.txt"])
        .unwrap()
        .stdout;
    assert!(
        fast.len() > 256 * 1024,
        "want >256 KB to exercise the drain, got {}",
        fast.len()
    );
    assert_eq!(fast, drained, "killable path must return identical output");
}

#[test]
fn cancellable_path_matches_plain_run_for_normal_commands() {
    let t = TestRepo::new();
    t.write("f", "a\nb\n");
    t.commit_all("init");
    let plain = open(&t).run(["rev-parse", "HEAD"]).unwrap().stdout;
    let killable = open(&t)
        .with_timeout(Duration::from_secs(30))
        .run(["rev-parse", "HEAD"])
        .unwrap()
        .stdout;
    assert_eq!(plain, killable);
    // An uncancelled cancellable repo behaves like a normal one too.
    let (repo, _flag) = open(&t).cancellable();
    assert_eq!(repo.run(["rev-parse", "HEAD"]).unwrap().stdout, plain);
}
