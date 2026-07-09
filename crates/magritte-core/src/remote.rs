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
    /// The single configured remote, when there's exactly one — the remote an
    /// unconfigured push/pull/fetch would use without prompting. Lets the menus
    /// name that concrete target instead of an abstract "push remote". Filled by
    /// the caller (needs a `git remote` query); `None` when unknown or when 0/2+
    /// remotes exist.
    pub sole_remote: Option<String>,
}

impl RemoteTargets {
    /// Build transient/menu labels from the already-parsed status header. This
    /// avoids re-running branch/upstream/push resolution after a refresh has
    /// populated the status screen. `sole_remote` is left unset — fill it via
    /// [`with_remotes`](Self::with_remotes) when the remote list is available.
    pub fn from_head(head: &HeadInfo) -> Self {
        let upstream = head.upstream.as_deref().and_then(parse_upstream);
        RemoteTargets {
            branch: head.branch.clone(),
            push_remote: head
                .push_remote
                .clone()
                .or_else(|| upstream.as_ref().map(|u| u.remote.clone())),
            upstream,
            sole_remote: None,
        }
    }

    /// Record the configured remotes, setting `sole_remote` when exactly one
    /// exists (what an unconfigured target resolves to without a prompt).
    pub fn with_remotes(mut self, remotes: &[String]) -> Self {
        self.sole_remote = match remotes {
            [only] => Some(only.clone()),
            _ => None,
        };
        self
    }

    /// The predicted target ref for an unconfigured push/pull on this branch:
    /// `sole_remote/branch` when there's a single remote to fall back to.
    pub fn predicted_ref(&self) -> Option<String> {
        match (&self.branch, &self.sole_remote) {
            (Some(b), Some(r)) => Some(format!("{r}/{b}")),
            _ => None,
        }
    }

    /// Whether pushing/pulling via the push-remote hits the same ref as the
    /// upstream (non-triangular: same remote *and* branch). The push/pull menus
    /// collapse `p` and `u` into one entry when so.
    pub fn push_matches_upstream(&self) -> bool {
        match (&self.branch, &self.push_remote, &self.upstream) {
            (Some(b), Some(pr), Some(u)) => *pr == u.remote && *b == u.branch,
            _ => false,
        }
    }

    /// Whether the push-remote is the upstream's remote. Fetch acts on a whole
    /// remote (no branch), so its menu collapses `p`/`u` on this alone.
    pub fn push_remote_is_upstream_remote(&self) -> bool {
        matches!(
            (&self.push_remote, &self.upstream),
            (Some(pr), Some(u)) if *pr == u.remote
        )
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

    /// The configured push remote for `branch` (`branch.<b>.pushRemote`, else
    /// `remote.pushDefault`), best-effort. The push *ref* is derived from this
    /// rather than by resolving `@{push}`, which git refuses under the default
    /// `push.default = simple` in a triangular workflow.
    pub(crate) fn push_remote_config(&self, branch: &str) -> Option<String> {
        let config = |key: &str| self.config_get(key).ok().flatten();
        config(&format!("branch.{branch}.pushRemote")).or_else(|| config("remote.pushDefault"))
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
            .map(|o| o.stdout_text())
            .filter(|s| !s.is_empty())
            .and_then(|s| parse_upstream(&s));
        let sole_remote = match self.remotes()?.as_slice() {
            [only] => Some(only.clone()),
            _ => None,
        };
        // Fall back to the upstream's remote (as `from_head` does) so the
        // push/pull menus collapse consistently on this path too.
        let push_remote = push_remote.or_else(|| upstream.as_ref().map(|u| u.remote.clone()));
        Ok(RemoteTargets {
            branch,
            push_remote,
            upstream,
            sole_remote,
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
                sole_remote: None,
            }
        );
    }

    fn targets(branch: &str, push_remote: &str, up_remote: &str, up_branch: &str) -> RemoteTargets {
        RemoteTargets {
            branch: Some(branch.to_string()),
            push_remote: Some(push_remote.to_string()),
            upstream: Some(Upstream {
                remote: up_remote.to_string(),
                branch: up_branch.to_string(),
            }),
            sole_remote: None,
        }
    }

    #[test]
    fn sole_remote_predicts_target_for_unconfigured_branch() {
        // A branch with no upstream/push-remote but exactly one remote: the
        // menus can name what a push would target (and save).
        let t = RemoteTargets {
            branch: Some("wip".to_string()),
            push_remote: None,
            upstream: None,
            sole_remote: None,
        }
        .with_remotes(&["origin".to_string()]);
        assert_eq!(t.sole_remote.as_deref(), Some("origin"));
        assert_eq!(t.predicted_ref().as_deref(), Some("origin/wip"));

        // Two remotes → ambiguous, no prediction (the push would prompt).
        let two = RemoteTargets {
            branch: Some("wip".to_string()),
            push_remote: None,
            upstream: None,
            sole_remote: None,
        }
        .with_remotes(&["origin".to_string(), "fork".to_string()]);
        assert_eq!(two.predicted_ref(), None);
    }

    #[test]
    fn push_and_upstream_coincide_when_same_remote_and_branch() {
        // Non-triangular: same remote and branch → collapses.
        let t = targets("main", "origin", "origin", "main");
        assert!(t.push_matches_upstream());
        assert!(t.push_remote_is_upstream_remote());
    }

    #[test]
    fn triangular_push_remote_does_not_collapse() {
        // Different push remote → push/pull keep both entries; fetch (remote
        // only) also stays split.
        let t = targets("main", "fork", "origin", "main");
        assert!(!t.push_matches_upstream());
        assert!(!t.push_remote_is_upstream_remote());
    }

    #[test]
    fn differing_branch_name_collapses_fetch_but_not_push() {
        // Same remote, different branch name: fetch acts on the remote alone so
        // it collapses, but the push/pull refs differ so they stay split.
        let t = targets("feature", "origin", "origin", "main");
        assert!(!t.push_matches_upstream());
        assert!(t.push_remote_is_upstream_remote());
    }

    #[test]
    fn action_dispatches_on_either_collapsed_key() {
        // The collapsed push entry is invokable by both `p` and `u`.
        let t = targets("main", "origin", "origin", "main");
        let push = crate::transient::push_transient(&t);
        assert!(push.action_for("p").is_some());
        assert!(push.action_for("u").is_some());
        assert_eq!(
            push.action_for("p").map(|a| &a.command),
            push.action_for("u").map(|a| &a.command),
        );
    }
}
