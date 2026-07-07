mod common;

use common::TestRepo;
use magritte_core::Repo;

/// format-patch a commit, then apply it (as a worktree diff) and `am` it (as a
/// commit) into a fresh clone-like repo built from the same base.
#[test]
fn format_apply_and_am_roundtrip() {
    let t = TestRepo::new();
    t.write("f", "base\n");
    t.commit_all("base");
    t.write("f", "base\nadded\n");
    t.commit_all("add a line");
    let repo = Repo::discover(t.path()).unwrap();

    // Create a patch for the last commit.
    let created = repo
        .format_patch(&["-1".to_string(), "HEAD".to_string()])
        .unwrap();
    assert!(created.contains(".patch"), "got: {created}");
    let patch = created.split(',').next().unwrap().trim().to_string();

    // Roll the worktree back to base and re-apply the patch as a plain diff.
    t.git(["reset", "--hard", "HEAD~1"]);
    repo.apply_patch_file(&patch).unwrap();
    assert_eq!(
        std::fs::read_to_string(t.path().join("f")).unwrap(),
        "base\nadded\n"
    );

    // Discard, then apply the patch as a commit via `am`.
    t.git(["checkout", "--", "f"]);
    repo.am_patch(&patch).unwrap();
    assert_eq!(t.git(["log", "-1", "--format=%s"]), "add a line");
    assert!(repo.sequence().is_none(), "am should have finished cleanly");
}
