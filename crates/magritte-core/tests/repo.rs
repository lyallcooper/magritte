mod common;

use std::path::Path;

use common::TestRepo;
use magritte_core::Repo;

fn open(t: &TestRepo) -> Repo {
    Repo::discover(t.path()).expect("discover repo")
}

#[test]
fn config_get_and_bool_read_repo_config() {
    let t = TestRepo::new();
    let repo = open(&t);
    assert_eq!(repo.config_get("magritte.missing").unwrap(), None);
    let missing = repo.command_log().pop().unwrap();
    assert_eq!(missing.code, Some(1));
    assert!(!missing.ok);
    assert!(missing.expected);
    t.git(["config", "test.value", "hello"]);
    assert_eq!(
        repo.config_get("test.value").unwrap().as_deref(),
        Some("hello")
    );
    assert!(!repo.config_bool("test.flag"));
    t.git(["config", "test.flag", "yes"]); // git canonicalizes to true
    assert!(repo.config_bool("test.flag"));
}

#[test]
fn config_set_and_unset_roundtrip() {
    let t = TestRepo::new();
    let repo = open(&t);
    repo.config_set("branch.main.description", "hello").unwrap();
    assert_eq!(
        repo.config_get("branch.main.description")
            .unwrap()
            .as_deref(),
        Some("hello")
    );
    repo.config_set("branch.main.description", "changed")
        .unwrap();
    assert_eq!(
        repo.config_get("branch.main.description")
            .unwrap()
            .as_deref(),
        Some("changed")
    );
    repo.config_unset("branch.main.description").unwrap();
    assert_eq!(repo.config_get("branch.main.description").unwrap(), None);
    // Unsetting an already-absent key is a no-op, not an error.
    repo.config_unset("branch.main.description").unwrap();
}

#[test]
fn pull_rebase_default_honors_branch_then_repo_config() {
    let t = TestRepo::new();
    t.write("f", "x\n");
    t.commit_all("init");
    let repo = open(&t);
    // Set the repo-level value explicitly both ways: `Repo` runs the real git,
    // so the developer's global config would skew an "unset" assertion.
    t.git(["config", "pull.rebase", "false"]);
    assert!(!repo.pull_rebase_default(Some("main")));
    t.git(["config", "pull.rebase", "true"]);
    assert!(repo.pull_rebase_default(Some("main")));
    // A branch-scoped value overrides the repo-wide one, in either direction.
    t.git(["config", "branch.main.rebase", "false"]);
    assert!(!repo.pull_rebase_default(Some("main")));
    t.git(["config", "branch.main.rebase", "merges"]);
    assert!(repo.pull_rebase_default(Some("main")));
}

#[test]
fn git_common_dir_is_shared_across_worktrees() {
    let t = TestRepo::new();
    t.write("f", "x\n");
    t.commit_all("init");
    let main_git = t.path().join(".git").canonicalize().unwrap();
    assert_eq!(
        open(&t).git_common_dir().unwrap().canonicalize().unwrap(),
        main_git
    );

    // A linked worktree's common dir is still the main repo's .git.
    let wt = tempfile::tempdir().unwrap();
    let wt_path = wt.path().join("wt");
    t.git(["worktree", "add", wt_path.to_str().unwrap(), "-b", "wt"]);
    let linked = Repo::discover(&wt_path).unwrap();
    assert_eq!(
        linked.git_common_dir().unwrap().canonicalize().unwrap(),
        main_git
    );
    // While the per-worktree git dir is its own.
    assert_ne!(linked.git_dir().unwrap().canonicalize().unwrap(), main_git);
}

#[test]
fn run_user_and_shell_in_a_subdirectory() {
    let t = TestRepo::new();
    t.write("sub/inner.txt", "x\n");
    t.commit_all("init");
    let repo = open(&t);

    // A git subcommand in the subdir resolves relative paths there.
    let run = repo
        .run_user_in(
            None,
            &["ls-files".to_string(), ".".to_string()],
            Path::new("sub"),
        )
        .unwrap();
    assert!(run.ok);
    assert_eq!(run.stdout.trim(), "inner.txt");

    // A shell line runs with the subdir as its working directory.
    let run = repo.run_shell_in("pwd", Path::new("sub")).unwrap();
    assert!(run.ok, "{}", run.stderr);
    assert!(
        run.stdout.trim().ends_with("/sub"),
        "pwd was {}",
        run.stdout.trim()
    );
}
