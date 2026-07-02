//! Remote targets (push-remote / upstream) and the push/pull/fetch operations
//! against them — mirroring magit's pushRemote-vs-upstream distinction.
//!
//! git itself distinguishes the two: `branch.<b>.pushRemote` / `remote.pushDefault`
//! (where `git push` sends) versus `branch.<b>.remote`+`merge` (the upstream you
//! track). We resolve both so the menus can label them, and run explicit
//! commands against a chosen remote rather than leaning on bare `git push`.

use crate::error::Result;
use crate::repo::{git_args, Repo};
use crate::status::HeadInfo;

/// A branch's upstream, split into its remote and remote-branch parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstream {
    pub remote: String,
    pub branch: String,
}

impl Upstream {
    /// The `remote/branch` display form (e.g. `origin/main`).
    pub fn display(&self) -> String {
        format!("{}/{}", self.remote, self.branch)
    }
}

/// The current branch's resolved push/pull/fetch targets, for labeling menus.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteTargets {
    /// Current branch name; `None` when HEAD is detached.
    pub branch: Option<String>,
    /// Resolved push-remote name (e.g. `origin`); `None` when unconfigured.
    pub push_remote: Option<String>,
    /// Upstream branch; `None` when unconfigured.
    pub upstream: Option<Upstream>,
}

impl RemoteTargets {
    /// Build transient/menu labels from the already-parsed status header. This
    /// avoids re-running branch/upstream/push resolution after a refresh has
    /// populated the status screen.
    pub fn from_head(head: &HeadInfo) -> Self {
        let upstream = head.upstream.as_deref().and_then(parse_upstream);
        RemoteTargets {
            branch: head.branch.clone(),
            push_remote: head
                .push_remote
                .clone()
                .or_else(|| upstream.as_ref().map(|u| u.remote.clone())),
            upstream,
        }
    }
}

fn parse_upstream(s: &str) -> Option<Upstream> {
    s.split_once('/').map(|(remote, branch)| Upstream {
        remote: remote.to_string(),
        branch: branch.to_string(),
    })
}

impl Repo {
    /// Configured remote names (`git remote`).
    pub fn remotes(&self) -> Result<Vec<String>> {
        Ok(self.run(["remote"])?.lines())
    }

    /// `git remote add [args] <name> <url>`.
    pub fn add_remote(&self, name: &str, url: &str, args: &[String]) -> Result<String> {
        Ok(self
            .run(git_args(&["remote", "add"], args, &[name, url]))?
            .status_line())
    }

    /// `git remote rename <old> <new>`.
    pub fn rename_remote(&self, old: &str, new: &str) -> Result<String> {
        Ok(self.run(["remote", "rename", old, new])?.status_line())
    }

    /// `git remote remove <name>`.
    pub fn remove_remote(&self, name: &str) -> Result<String> {
        Ok(self.run(["remote", "remove", name])?.status_line())
    }

    /// Remote-tracking branches as `remote/branch` (e.g. `origin/main`), for the
    /// push/pull "elsewhere" target picker. Skips the symbolic `*/HEAD` refs —
    /// note `%(refname:short)` collapses `origin/HEAD` to just `origin`, so we
    /// also drop entries without a `/`.
    pub fn remote_branches(&self) -> Result<Vec<String>> {
        let out = self.run(["for-each-ref", "--format=%(refname:short)", "refs/remotes/"])?;
        let mut branches = out.lines();
        branches.retain(|l| l.contains('/') && !l.ends_with("/HEAD"));
        Ok(branches)
    }

    /// Resolve the current branch's push-remote and upstream.
    pub fn remote_targets(&self) -> Result<RemoteTargets> {
        let branch = self.current_branch()?;
        let Some(b) = branch.clone() else {
            return Ok(RemoteTargets::default());
        };
        let push_remote = self
            .config_get(&format!("branch.{b}.pushRemote"))?
            .or(self.config_get("remote.pushDefault")?);
        let upstream = self
            .run_optional([
                "rev-parse",
                "--abbrev-ref",
                "--symbolic-full-name",
                &format!("{b}@{{upstream}}"),
            ])?
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
            .and_then(|s| parse_upstream(&s));
        Ok(RemoteTargets {
            branch,
            push_remote,
            upstream,
        })
    }

    /// Persist a branch's push-remote (`branch.<b>.pushRemote`) so future pushes
    /// default there — matches magit setting it on first push to a push-remote.
    pub fn set_push_remote(&self, branch: &str, remote: &str) -> Result<()> {
        self.run(["config", &format!("branch.{branch}.pushRemote"), remote])?;
        Ok(())
    }

    /// `git push [--set-upstream] [switches] <remote> <branch>` (pushes the
    /// local branch to the same-named branch on `remote`).
    pub fn push_to(
        &self,
        remote: &str,
        branch: &str,
        set_upstream: bool,
        switches: &[String],
    ) -> Result<String> {
        let lead: &[&str] = if set_upstream {
            &["push", "--set-upstream"]
        } else {
            &["push"]
        };
        Ok(self
            .run(git_args(lead, switches, &[remote, branch]))?
            .report())
    }

    /// `git push [switches] <remote> <local>:<target>` — push the local branch
    /// to a specific (possibly differently-named or new) remote branch.
    pub fn push_ref(
        &self,
        remote: &str,
        local: &str,
        target: &str,
        switches: &[String],
    ) -> Result<String> {
        let refspec = format!("{local}:{target}");
        Ok(self
            .run(git_args(&["push"], switches, &[remote, &refspec]))?
            .report())
    }

    /// `git pull [switches] <remote> <branch>`.
    pub fn pull_from(&self, remote: &str, branch: &str, switches: &[String]) -> Result<String> {
        Ok(self
            .run(git_args(&["pull"], switches, &[remote, branch]))?
            .report())
    }

    /// `git fetch [switches] <remote>`.
    pub fn fetch_from(&self, remote: &str, switches: &[String]) -> Result<String> {
        Ok(self
            .run(git_args(&["fetch"], switches, &[remote]))?
            .report())
    }

    /// `git fetch [switches]` — the current branch's configured remote, with no
    /// explicit remote or `--all`. Used for the lightweight background
    /// auto-fetch (keeps unpushed/unpulled current without touching every remote).
    pub fn fetch_default(&self, switches: &[String]) -> Result<String> {
        Ok(self.run(git_args(&["fetch"], switches, &[]))?.report())
    }

    /// `git fetch --all [switches]`.
    pub fn fetch_all(&self, switches: &[String]) -> Result<String> {
        Ok(self
            .run(git_args(&["fetch", "--all"], switches, &[]))?
            .report())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_targets_reuse_status_head_metadata() {
        let head = HeadInfo {
            branch: Some("main".to_string()),
            upstream: Some("origin/main".to_string()),
            push_remote: Some("fork".to_string()),
            ..HeadInfo::default()
        };

        assert_eq!(
            RemoteTargets::from_head(&head),
            RemoteTargets {
                branch: Some("main".to_string()),
                push_remote: Some("fork".to_string()),
                upstream: Some(Upstream {
                    remote: "origin".to_string(),
                    branch: "main".to_string(),
                }),
            }
        );
    }
}
