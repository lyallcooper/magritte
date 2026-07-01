//! Tag operations — the `t` tag transient's create/delete commands.

use crate::error::Result;
use crate::repo::Repo;

impl Repo {
    /// Tag names, newest tagger date first where available.
    pub fn tags(&self) -> Result<Vec<String>> {
        let out = self.run([
            "for-each-ref",
            "--sort=-taggerdate",
            "--format=%(refname:short)",
            "refs/tags/",
        ])?;
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// `git tag [-f] <name> <target>` — create a lightweight tag.
    pub fn create_tag(&self, name: &str, target: &str, force: bool) -> Result<String> {
        let mut argv = vec!["tag".to_string()];
        if force {
            argv.push("--force".to_string());
        }
        argv.push(name.to_string());
        argv.push(target.to_string());
        Ok(self.run(argv)?.status_line())
    }

    /// `git tag -a [-f] -m <name> <name> <target>` — create an annotated tag
    /// without requiring an external editor. The tag name is a good default
    /// annotation, and users can edit annotations later once that flow exists.
    pub fn create_annotated_tag(&self, name: &str, target: &str, force: bool) -> Result<String> {
        let mut argv = vec!["tag".to_string(), "--annotate".to_string()];
        if force {
            argv.push("--force".to_string());
        }
        argv.push("--message".to_string());
        argv.push(name.to_string());
        argv.push(name.to_string());
        argv.push(target.to_string());
        Ok(self.run(argv)?.status_line())
    }

    /// `git tag -d <name>` — delete a local tag.
    pub fn delete_tag(&self, name: &str) -> Result<String> {
        Ok(self.run(["tag", "--delete", name])?.status_line())
    }
}
