//! Regression tests for the extracted process runner: cancellation, timeouts,
//! stdin feeding, output draining, exit statuses, and process-tree kill. These
//! drive plain `sh` children so they cover the runner itself, independent of
//! any VCS binary.
#![cfg(unix)]

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use magritte_vcs::{prepare_spawn, Error, ProcessControl};

fn sh(script: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(script);
    prepare_spawn(&mut cmd);
    cmd
}

fn control(cancel: Option<Arc<AtomicBool>>, timeout: Option<Duration>) -> ProcessControl {
    ProcessControl { cancel, timeout }
}

/// Whether `pid` (from a pidfile) still exists.
fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Read the pid a test script wrote, waiting for the file to appear.
fn read_pidfile(path: &std::path::Path) -> i32 {
    let start = Instant::now();
    loop {
        if let Ok(s) = std::fs::read_to_string(path) {
            if let Ok(pid) = s.trim().parse() {
                return pid;
            }
        }
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "pidfile never appeared at {path:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// Wait (briefly) for a process to disappear; kills the assertion, not the
/// process, on timeout.
fn assert_dies(pid: i32, what: &str) {
    let start = Instant::now();
    while alive(pid) {
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "{what} (pid {pid}) survived the kill"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn plain_path_returns_output_and_status() {
    let (stdout, stderr, status) = control(None, None)
        .collect_output_with(sh("echo out; echo err >&2; exit 3"), None)
        .unwrap();
    assert_eq!(stdout, b"out\n");
    assert_eq!(stderr, "err\n");
    // The exit status is returned, not judged: expectation policy is the
    // caller's (expected-nonzero predicates run through the same path).
    assert_eq!(status.code(), Some(3));
}

#[test]
fn guarded_path_matches_plain_output() {
    let script = "echo out; echo err >&2; exit 3";
    let plain = control(None, None)
        .collect_output_with(sh(script), None)
        .unwrap();
    let guarded = control(None, Some(Duration::from_secs(30)))
        .collect_output_with(sh(script), None)
        .unwrap();
    assert_eq!(plain.0, guarded.0);
    assert_eq!(plain.1, guarded.1);
    assert_eq!(plain.2.code(), guarded.2.code());
}

#[test]
fn stdin_feeds_large_input_on_both_paths() {
    // Bigger than any pipe buffer, so an un-threaded stdin write would
    // deadlock against the child echoing it back.
    let input = "abcdefgh\n".repeat(150_000); // ~1.3 MB
    for ctl in [
        control(None, None),
        control(None, Some(Duration::from_secs(30))),
    ] {
        let (stdout, _, status) = ctl
            .collect_output_with(sh("cat"), Some(input.as_bytes()))
            .unwrap();
        assert!(status.success());
        assert_eq!(stdout.len(), input.len(), "stdin round-trip truncated");
        assert_eq!(stdout, input.as_bytes());
    }
}

#[test]
fn guarded_path_drains_large_output() {
    // > 256 KB of stdout: the case that deadlocks a poll loop which doesn't
    // drain the pipe while waiting.
    let (stdout, _, status) = control(None, Some(Duration::from_secs(30)))
        .collect_output_with(
            sh("i=0; while [ $i -lt 60000 ]; do echo abcdefgh; i=$((i+1)); done"),
            None,
        )
        .unwrap();
    assert!(status.success());
    assert_eq!(stdout.len(), 60_000 * 9);
}

#[test]
fn timeout_kills_a_blocking_child() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let start = Instant::now();
    // A generous-but-bounded timeout: the pidfile write must land well within
    // it even on a loaded machine.
    let res = control(None, Some(Duration::from_millis(500))).collect_output_with(
        sh(&format!("echo $$ > {}; sleep 30", pidfile.display())),
        None,
    );
    assert!(matches!(res, Err(Error::TimedOut)), "got {res:?}");
    assert!(start.elapsed() < Duration::from_secs(3));
    assert_dies(read_pidfile(&pidfile), "timed-out child");
}

#[test]
fn cancel_kills_a_blocking_child_promptly() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let cancel = Arc::new(AtomicBool::new(false));
    let ctl = control(Some(cancel.clone()), None);
    let cmd = sh(&format!("echo $$ > {}; sleep 30", pidfile.display()));
    let worker = thread::spawn(move || {
        let start = Instant::now();
        (ctl.collect_output_with(cmd, None), start.elapsed())
    });
    // The pidfile is the child-has-started sync point; only cancel after it.
    let pid = read_pidfile(&pidfile);
    cancel.store(true, Ordering::Relaxed);
    let (res, elapsed) = worker.join().unwrap();
    assert!(matches!(res, Err(Error::Cancelled)), "got {res:?}");
    assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
    assert_dies(pid, "cancelled child");
}

#[test]
fn cancel_interrupts_a_child_while_stdin_is_being_fed() {
    // A child that never reads stdin: the writer thread blocks on the full
    // pipe. Cancelling must still kill the child promptly and let the writer
    // thread die on EPIPE rather than wedging the invocation.
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let cancel = Arc::new(AtomicBool::new(false));
    let ctl = control(Some(cancel.clone()), None);
    let cmd = sh(&format!("echo $$ > {}; sleep 30", pidfile.display()));
    let input = vec![b'x'; 4 * 1024 * 1024]; // far beyond any pipe buffer
    let worker = thread::spawn(move || {
        let start = Instant::now();
        (ctl.collect_output_with(cmd, Some(&input)), start.elapsed())
    });
    let pid = read_pidfile(&pidfile);
    cancel.store(true, Ordering::Relaxed);
    let (res, elapsed) = worker.join().unwrap();
    assert!(matches!(res, Err(Error::Cancelled)), "got {res:?}");
    assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
    assert_dies(pid, "stdin-fed child");
}

#[test]
fn cancel_kills_the_whole_process_tree() {
    // The direct child spawns a grandchild (as git spawns transports/hooks and
    // jj spawns git). Cancelling must kill the grandchild too — a survivor
    // could hold locks or the output pipes.
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("grandchild-pid");
    let cancel = Arc::new(AtomicBool::new(false));
    let ctl = control(Some(cancel.clone()), None);
    let cmd = sh(&format!("sleep 30 & echo $! > {}; wait", pidfile.display()));
    let worker = thread::spawn(move || ctl.collect_output_with(cmd, None));
    let grandchild = read_pidfile(&pidfile);
    assert!(
        alive(grandchild),
        "grandchild should be running before cancel"
    );
    cancel.store(true, Ordering::Relaxed);
    let res = worker.join().unwrap();
    assert!(matches!(res, Err(Error::Cancelled)), "got {res:?}");
    assert_dies(grandchild, "grandchild");
}

#[test]
fn cancel_falls_back_to_sigkill_when_sigterm_is_ignored() {
    // A child that traps SIGTERM (a wedged transport) must still die — and the
    // cancel must return promptly rather than wait on it.
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let cancel = Arc::new(AtomicBool::new(false));
    let ctl = control(Some(cancel.clone()), None);
    let cmd = sh(&format!(
        "trap '' TERM; echo $$ > {}; while :; do sleep 0.05; done",
        pidfile.display()
    ));
    let worker = thread::spawn(move || {
        let start = Instant::now();
        (ctl.collect_output_with(cmd, None), start.elapsed())
    });
    let pid = read_pidfile(&pidfile);
    cancel.store(true, Ordering::Relaxed);
    let (res, elapsed) = worker.join().unwrap();
    assert!(matches!(res, Err(Error::Cancelled)), "got {res:?}");
    assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
    assert_dies(pid, "TERM-ignoring child");
}
