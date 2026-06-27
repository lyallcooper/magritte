mod common;

use common::TestRepo;
use magritte_core::{IgnoreDest, Repo};

fn open(t: &TestRepo) -> Repo {
    Repo::discover(t.path()).expect("discover repo")
}

#[test]
fn toplevel_writes_gitignore_and_stages_it() {
    let t = TestRepo::new();
    t.write("f", "x\n");
    t.commit_all("init");

    open(&t)
        .add_ignore_rule("build/", IgnoreDest::Toplevel)
        .unwrap();

    let gitignore = std::fs::read_to_string(t.path().join(".gitignore")).unwrap();
    assert_eq!(gitignore, "build/\n");
    // Tracked → staged, so it shows in the index (added).
    let staged = t.git(["diff", "--cached", "--name-only"]);
    assert!(staged.contains(".gitignore"), "staged: {staged:?}");
}

#[test]
fn private_writes_info_exclude_unstaged() {
    let t = TestRepo::new();
    t.write("f", "x\n");
    t.commit_all("init");

    open(&t)
        .add_ignore_rule("*.log", IgnoreDest::Private)
        .unwrap();

    let exclude = std::fs::read_to_string(t.path().join(".git/info/exclude")).unwrap();
    assert!(exclude.ends_with("*.log\n"), "exclude: {exclude:?}");
    // info/exclude isn't a tracked path, so nothing is staged.
    assert!(t.git(["diff", "--cached", "--name-only"]).is_empty());
}

#[test]
fn appends_with_a_separating_newline() {
    let t = TestRepo::new();
    t.write(".gitignore", "first"); // no trailing newline
    t.commit_all("init");

    open(&t)
        .add_ignore_rule("second", IgnoreDest::Toplevel)
        .unwrap();

    let gitignore = std::fs::read_to_string(t.path().join(".gitignore")).unwrap();
    assert_eq!(gitignore, "first\nsecond\n");
}

#[test]
fn global_writes_core_excludesfile() {
    let t = TestRepo::new();
    t.write("f", "x\n");
    t.commit_all("init");
    let excludes = t.path().join("my-global-ignore");
    t.git(["config", "core.excludesFile", excludes.to_str().unwrap()]);

    open(&t)
        .add_ignore_rule("*.tmp", IgnoreDest::Global)
        .unwrap();

    let written = std::fs::read_to_string(&excludes).unwrap();
    assert_eq!(written, "*.tmp\n");
}
