//! Resolving remotes and branch candidates for the transfer/branch pickers:
//! the default remote, the full branch candidate set, the push "elsewhere"
//! seeding, and splitting a `remote/branch` ref. Pure functions over a `Repo`.

use magritte_core::Repo;

/// The remote a bare (unqualified) branch name targets: the conventional
/// `origin` if present, else the first configured remote, else `origin`.
pub(crate) fn default_remote(repo: &Repo) -> String {
    let remotes = repo.remotes().unwrap_or_default();
    if remotes.iter().any(|r| r == "origin") {
        "origin".to_string()
    } else {
        remotes
            .into_iter()
            .next()
            .unwrap_or_else(|| "origin".to_string())
    }
}

/// The configured remotes — the candidate set for the remote-configure picker.
pub(crate) fn remotes(repo: &Repo) -> magritte_core::Result<Vec<String>> {
    repo.remotes()
}

/// Local + remote branch names — the candidate set shared by the branch, log,
/// reset, merge, and rebase pickers.
pub(crate) fn all_branches(repo: &Repo) -> magritte_core::Result<Vec<String>> {
    let mut names = repo.local_branches()?;
    names.extend(repo.remote_branches()?);
    Ok(names)
}

/// The push "elsewhere" candidate list, magit-style: seed `<remote>/<current>`
/// for every remote (existing or not) so the same-named push target is always a
/// normal candidate, then append the existing remote branches. The preferred
/// remote (push-remote if set, else [`default_remote`]) comes first, so the most
/// likely target is the default selection.
pub(crate) fn seed_push_branches(
    repo: &Repo,
    remotes: &[String],
    current: &str,
    existing: Vec<String>,
) -> Vec<String> {
    if current.is_empty() {
        return existing;
    }
    let preferred = repo
        .remote_targets()
        .ok()
        .and_then(|t| t.push_remote)
        .unwrap_or_else(|| default_remote(repo));
    let mut ordered: Vec<&String> = remotes.iter().collect();
    ordered.sort_by_key(|r| **r != preferred);

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(remotes.len() + existing.len());
    for cand in ordered
        .into_iter()
        .map(|r| format!("{r}/{current}"))
        .chain(existing)
    {
        if seen.insert(cand.clone()) {
            out.push(cand);
        }
    }
    out
}

/// Split a chosen `remote/branch` ref into its parts. A bare value (no `/`,
/// from a freshly-typed branch) defaults to [`default_remote`].
pub(crate) fn split_ref(repo: &Repo, chosen: &str) -> (String, String) {
    match chosen.split_once('/') {
        Some((remote, branch)) => (remote.to_string(), branch.to_string()),
        None => (default_remote(repo), chosen.to_string()),
    }
}
