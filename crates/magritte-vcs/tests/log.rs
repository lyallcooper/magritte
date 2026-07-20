//! Regression tests for the extracted command log: recording, program labels,
//! the injected query classifier, ring capacity, and the change stamp.

use std::time::Duration;

use magritte_vcs::{CommandEntry, CommandLog};

fn entry(program: Option<&str>, args: &[&str], user: bool) -> CommandEntry {
    CommandEntry {
        program: program.map(str::to_string),
        args: args.iter().map(|s| s.to_string()).collect(),
        code: Some(0),
        ok: true,
        expected: true,
        elapsed: Duration::from_millis(1),
        user,
        ..Default::default()
    }
}

/// A stand-in for an engine's classifier: internal `status` reads are queries.
fn classify(e: &CommandEntry) -> bool {
    !e.user && e.args.first().map(String::as_str) == Some("status")
}

#[test]
fn record_resolves_the_default_program_and_keeps_labels() {
    let log = CommandLog::new("git", 10, classify);
    log.record(entry(None, &["fetch", "origin"], false));
    log.record(entry(Some("ls"), &["-la"], true));
    let snap = log.snapshot();
    assert_eq!(snap[0].program.as_deref(), Some("git"));
    assert_eq!(snap[0].display(), "git fetch origin");
    assert_eq!(snap[1].program.as_deref(), Some("ls"));
    assert_eq!(snap[1].display(), "ls -la");
}

#[test]
fn record_applies_the_injected_query_classifier() {
    let log = CommandLog::new("git", 10, classify);
    log.record(entry(None, &["status", "--porcelain=v2"], false));
    log.record(entry(None, &["commit", "-m", "x"], false));
    // A user-typed command is never a query, even one the classifier would
    // otherwise hide.
    log.record(entry(None, &["status"], true));
    let snap = log.snapshot();
    assert!(snap[0].is_query());
    assert!(!snap[1].is_query());
    assert!(!snap[2].is_query());
}

#[test]
fn ring_evicts_oldest_at_capacity_and_seq_is_monotonic() {
    let log = CommandLog::new("git", 3, classify);
    assert_eq!(log.seq(), 0);
    for i in 0..5 {
        log.record(entry(None, &[&format!("cmd{i}")], false));
    }
    let snap = log.snapshot();
    assert_eq!(snap.len(), 3);
    let args: Vec<_> = snap.iter().map(|e| e.args[0].as_str()).collect();
    assert_eq!(args, ["cmd2", "cmd3", "cmd4"]);
    // The stamp counts every record, not the capped length.
    assert_eq!(log.seq(), 5);
}

#[test]
fn expected_nonzero_entries_survive_intact() {
    // An expected non-zero (a predicate like `git diff --quiet`, or a config
    // read of an unset key) must round-trip its status so the log renders it
    // neutrally rather than as a failure.
    let log = CommandLog::new("git", 10, classify);
    let mut e = entry(None, &["config", "--get", "no.such.key"], false);
    e.code = Some(1);
    e.ok = false;
    e.expected = true;
    log.record(e);
    let snap = log.snapshot();
    assert_eq!(snap[0].code, Some(1));
    assert!(!snap[0].ok);
    assert!(snap[0].expected);
}
