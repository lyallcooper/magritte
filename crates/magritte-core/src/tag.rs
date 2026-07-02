//! Tag operations — the `t` tag transient's create/delete commands.

use crate::error::Result;
use crate::repo::{git_args, Repo};

impl Repo {
    /// Tag names, newest tagger date first where available.
    pub fn tags(&self) -> Result<Vec<String>> {
        Ok(self
            .run([
                "for-each-ref",
                "--sort=-taggerdate",
                "--format=%(refname:short)",
                "refs/tags/",
            ])?
            .lines())
    }

    /// `git tag [-f] <name> <target>` — create a lightweight tag.
    pub fn create_tag(&self, name: &str, target: &str, force: bool) -> Result<String> {
        let lead: &[&str] = if force { &["tag", "--force"] } else { &["tag"] };
        Ok(self
            .run(git_args(lead, &[], &[name, target]))?
            .status_line())
    }

    /// `git tag -a [-f] -m <name> <name> <target>` — create an annotated tag
    /// without requiring an external editor. The tag name is a good default
    /// annotation, and users can edit annotations later once that flow exists.
    pub fn create_annotated_tag(&self, name: &str, target: &str, force: bool) -> Result<String> {
        let lead: &[&str] = if force {
            &["tag", "--annotate", "--force"]
        } else {
            &["tag", "--annotate"]
        };
        Ok(self
            .run(git_args(lead, &[], &["--message", name, name, target]))?
            .status_line())
    }

    /// `git tag -d <name>` — delete a local tag.
    pub fn delete_tag(&self, name: &str) -> Result<String> {
        Ok(self.run(["tag", "--delete", name])?.status_line())
    }
}
