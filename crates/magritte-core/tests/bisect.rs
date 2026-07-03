mod common;

use common::TestRepo;
use magritte_core::{bisect::BisectMark, Repo};

/// A linear history c1..c5 on `main`, returning (repo, first, last).
fn linear() -> (TestRepo, Repo, String, String) {
    let t = TestRepo::new();
    t.write("f", "0\n");
    t.commit_all("c1");
    let first = t.git(["rev-parse", "HEAD"]);
    for i in 2..=5 {
        t.write("f", &format!("{i}\n"));
        t.commit_all(&format!("c{i}"));
    }
    let last = t.git(["rev-parse", "HEAD"]);
    let repo = Repo::discover(t.path()).unwrap();
    (t, repo, first, last)
}

#[test]
fn no_bisect_when_idle() {
    let (_t, repo, ..) = linear();
    assert!(repo.bisect().is_none());
}

#[test]
fn start_records_decisions_and_reset_clears() {
    let (_t, repo, good, bad) = linear();

    repo.bisect_start(&bad, &good).unwrap();
    let b = repo.bisect().expect("a bisect should be in progress");
    // The known bounds and the start command are summarized.
    assert!(b.decisions.iter().any(|d| d.starts_with("bad:")));
    assert!(b.decisions.iter().any(|d| d.starts_with("good:")));
    assert!(b.decisions.iter().any(|d| d.starts_with("bisect start")));

    // Marking the checked-out midpoint advances the search.
    repo.bisect_mark(BisectMark::Good).unwrap();
    assert!(repo.bisect().is_some());

    // Reset ends the session.
    repo.bisect_reset().unwrap();
    assert!(repo.bisect().is_none());
}
