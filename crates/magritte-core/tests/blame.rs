mod common;

use common::TestRepo;
use magritte_core::Repo;

#[test]
fn blames_each_line_with_author_and_date() {
    let t = TestRepo::new();
    t.write("f", "one\ntwo\n");
    t.commit_all("first");
    t.write("f", "one\ntwo\nthree\n");
    t.commit_all("second");
    let repo = Repo::discover(t.path()).unwrap();

    let lines = repo.blame("f").unwrap();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0].line_no, 1);
    assert_eq!(lines[0].text, "one");
    assert_eq!(lines[2].text, "three");
    // Every line is attributed to the test author with a plausible date.
    assert!(lines.iter().all(|l| l.author == "Test"));
    assert!(lines
        .iter()
        .all(|l| l.date.len() == 10 && l.date.contains('-')));
    // Lines 1-2 come from the first commit, line 3 from the second.
    assert_eq!(lines[0].short, lines[1].short);
    assert_ne!(lines[0].short, lines[2].short);
    // Group starts: the first line of each commit run is marked.
    assert!(lines[0].group_start);
    assert!(lines[2].group_start);
}
