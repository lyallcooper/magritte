//! Tag operations — the `t` tag transient's create/delete commands.

use crate::error::Result;
use crate::repo::{git_args, Repo};

impl Repo {
    /// Tag names in version order, highest first (so `v0.4.0` leads `v0.3.0`).
    /// `version:refname` sorts embedded numbers naturally rather than
    /// lexically, and falls back to a sensible order for non-version tags —
    /// unlike `taggerdate`, which leaves lightweight tags (no tagger date)
    /// unordered.
    pub fn tags(&self) -> Result<Vec<String>> {
        Ok(self
            .run([
                "for-each-ref",
                "--sort=-version:refname",
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

    /// `git tag -a [-f] -F - <name> <target>` — create an annotated tag with
    /// `message` as its annotation, read from stdin so a multi-line message
    /// needs no escaping.
    pub fn create_annotated_tag(
        &self,
        name: &str,
        target: &str,
        force: bool,
        message: &str,
    ) -> Result<String> {
        let lead: &[&str] = if force {
            &["tag", "--annotate", "--force"]
        } else {
            &["tag", "--annotate"]
        };
        Ok(self
            .run_with_input(
                git_args(lead, &[], &["--file", "-", name, target]),
                message.as_bytes(),
            )?
            .status_line())
    }

    /// `git tag -a [-f] <name> <target>` with `GIT_EDITOR` pointed at the user's
    /// editor — the interactive path for writing the annotation externally
    /// (git opens the editor, blocking until it's closed).
    pub fn create_annotated_tag_with_editor(
        &self,
        name: &str,
        target: &str,
        force: bool,
        git_editor: &str,
    ) -> Result<String> {
        let lead: &[&str] = if force {
            &["tag", "--annotate", "--force"]
        } else {
            &["tag", "--annotate"]
        };
        Ok(self
            .run_with_env(
                git_args(lead, &[], &[name, target]),
                "GIT_EDITOR",
                git_editor,
            )?
            .status_line())
    }

    /// `git tag -d <name>` — delete a local tag.
    pub fn delete_tag(&self, name: &str) -> Result<String> {
        Ok(self.run(["tag", "--delete", name])?.status_line())
    }
}
