#[allow(dead_code)]
mod common;

use common::TestRepo;
use magritte_core::Repo;

#[test]
fn add_rename_and_remove_remote() {
    let t = TestRepo::new();
    let repo = Repo::discover(t.path()).unwrap();

    repo.add_remote("origin", "https://example.com/origin.git").unwrap();
    assert_eq!(repo.remotes().unwrap(), vec!["origin".to_string()]);

    repo.rename_remote("origin", "upstream").unwrap();
    assert_eq!(repo.remotes().unwrap(), vec!["upstream".to_string()]);

    repo.remove_remote("upstream").unwrap();
    assert!(repo.remotes().unwrap().is_empty());
}
