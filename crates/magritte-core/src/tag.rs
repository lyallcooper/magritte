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

    /// Existing release tags, highest version first. A tag qualifies as a
    /// release when it matches [`parse_release_tag`] (an optional `v`/`version`/
    /// `r`/`release` prefix + a dotted-number version + optional prerelease
    /// suffix); `git tag -n` supplies each tag's message. Mirrors magit's
    /// `magit--list-releases`.
    pub fn list_releases(&self) -> Result<Vec<Release>> {
        let mut releases: Vec<Release> = self
            .run(["tag", "-n"])?
            .lines()
            .iter()
            .filter_map(|line| {
                // A lightweight tag with no message is just the bare name.
                let (tag, message) = match line.find(char::is_whitespace) {
                    Some(idx) => (&line[..idx], line[idx..].trim()),
                    None => (line.as_str(), ""),
                };
                let (prefix, version) = parse_release_tag(tag)?;
                Some(Release {
                    key: version_key(&version),
                    tag: tag.to_string(),
                    prefix,
                    version,
                    message: message.to_string(),
                })
            })
            .collect();
        releases.sort_by(|a, b| version_cmp(&b.key, &a.key));
        Ok(releases)
    }

    /// The tag name to propose for the next release on HEAD, mirroring
    /// `magit-tag-release`: if HEAD's subject is a `Release version X` commit,
    /// reuse that version (carrying the previous release's prefix, e.g. `v`);
    /// otherwise seed with the highest existing tag for the user to bump.
    pub fn next_release_seed(&self) -> Result<ReleaseSeed> {
        let releases = self.list_releases()?;
        let prev = releases.first();
        let subject = self
            .head_message()
            .unwrap_or_default()
            .lines()
            .next()
            .unwrap_or_default()
            .to_string();
        let ver = release_commit_version(&subject);
        let (tag, first) = match (&ver, prev) {
            // A "Release version X" commit: reapply the previous release's prefix.
            (Some(v), Some(p)) => (format!("{}{v}", p.prefix), false),
            // First release from such a commit: prepend `v` to a bare number.
            (Some(v), None) => {
                let tag = if v.starts_with(|c: char| c.is_ascii_digit()) {
                    format!("v{v}")
                } else {
                    v.clone()
                };
                (tag, true)
            }
            // Otherwise seed with the highest existing tag; the user bumps it.
            (None, Some(p)) => (p.tag.clone(), false),
            (None, None) => (String::new(), true),
        };
        Ok(ReleaseSeed { tag, first })
    }

    /// The annotation message to propose for release `tag`, mirroring
    /// `magit-tag-release`: reuse the previous release's message with the new
    /// version (or tag) substituted in; failing that, `"<Repo> <version>"`.
    pub fn release_message(&self, tag: &str) -> Result<String> {
        let releases = self.list_releases()?;
        let version = parse_release_tag(tag).map(|(_, v)| v).unwrap_or_default();
        if let Some(prev) = releases.first() {
            if !prev.version.is_empty() && prev.message.contains(&prev.version) {
                return Ok(prev.message.replacen(&prev.version, &version, 1));
            }
            if prev.message.contains(&prev.tag) {
                return Ok(prev.message.replacen(&prev.tag, tag, 1));
            }
        }
        let repo = self
            .workdir()
            .file_name()
            .map(|n| capitalize(&n.to_string_lossy()))
            .unwrap_or_default();
        Ok(format!("{repo} {version}").trim().to_string())
    }
}

/// A parsed release tag (see [`Repo::list_releases`]).
#[derive(Debug, Clone)]
pub struct Release {
    /// The full tag name, e.g. `v1.4.0`.
    pub tag: String,
    /// The tag's prefix, e.g. `v` or `release-` (empty for a bare version).
    pub prefix: String,
    /// The version portion, e.g. `1.4.0`.
    pub version: String,
    /// The tag's message (first line of an annotation, or the commit subject).
    pub message: String,
    /// Comparison key for version ordering.
    key: Vec<i64>,
}

/// The proposed next release tag (see [`Repo::next_release_seed`]).
#[derive(Debug, Clone)]
pub struct ReleaseSeed {
    /// The proposed tag name to seed the prompt with (may be empty).
    pub tag: String,
    /// Whether this is the first release tag (no prior release exists).
    pub first: bool,
}

/// Split a release tag into `(prefix, version)`, or `None` if it isn't a
/// release tag. Mirrors magit's `magit-release-tag-regexp`: an optional
/// `v`/`version`/`r`/`release` prefix (with an optional `-_/` separator)
/// followed by a dotted-number version with an optional `-suffix`.
fn parse_release_tag(tag: &str) -> Option<(String, String)> {
    let mut prefix = String::new();
    let mut rest = tag;
    for p in ["version", "release", "v", "r"] {
        let Some(after) = tag.strip_prefix(p) else {
            continue;
        };
        let (sep, body) = match after.chars().next() {
            Some(c @ ('-' | '_' | '/')) => (Some(c), &after[c.len_utf8()..]),
            _ => (None, after),
        };
        if body.starts_with(|c: char| c.is_ascii_digit()) {
            prefix = match sep {
                Some(c) => format!("{p}{c}"),
                None => p.to_string(),
            };
            rest = body;
            break;
        }
    }
    if prefix.is_empty() {
        if !tag.starts_with(|c: char| c.is_ascii_digit()) {
            return None;
        }
        rest = tag;
    }
    is_valid_version(rest).then(|| (prefix, rest.to_string()))
}

/// Whether `s` is a `1.2.3`-style version with an optional `-prerelease` suffix.
fn is_valid_version(s: &str) -> bool {
    let (main, pre) = match s.split_once('-') {
        Some((m, p)) => (m, Some(p)),
        None => (s, None),
    };
    let main_ok = !main.is_empty()
        && main
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
    let pre_ok = match pre {
        Some(p) => {
            !p.is_empty()
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
        }
        None => true,
    };
    main_ok && pre_ok
}

/// A version's comparison key: the dotted numbers, then (for a prerelease) a
/// negative marker per magit's `magit-tag-version-regexp-alist` so a prerelease
/// sorts below its release. Compared with [`version_cmp`] (zero-padded).
fn version_key(version: &str) -> Vec<i64> {
    let (main, pre) = match version.split_once('-') {
        Some((m, p)) => (m, Some(p)),
        None => (version, None),
    };
    let mut key: Vec<i64> = main.split('.').filter_map(|p| p.parse().ok()).collect();
    if let Some(pre) = pre {
        for token in pre.split(['.', '-']).filter(|t| !t.is_empty()) {
            key.push(
                token
                    .parse()
                    .unwrap_or_else(|_| match token.to_ascii_lowercase().as_str() {
                        "snapshot" | "cvs" | "git" | "bzr" | "svn" | "hg" | "darcs" | "unknown" => {
                            -4
                        }
                        "alpha" => -3,
                        "beta" => -2,
                        "pre" | "rc" => -1,
                        _ => -5,
                    }),
            );
        }
    }
    key
}

/// Compare two version keys, zero-padding the shorter so a release (`1.0.0`)
/// outranks its prerelease (`1.0.0-rc.1`, whose key carries a trailing negative).
fn version_cmp(a: &[i64], b: &[i64]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        match x.cmp(&y) {
            Ordering::Equal => continue,
            other => return other,
        }
    }
    Ordering::Equal
}

/// The version from a `Release version X` commit subject, if it is one.
/// Mirrors magit's `magit-release-commit-regexp`.
fn release_commit_version(subject: &str) -> Option<String> {
    subject
        .strip_prefix("Release version ")
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// Capitalize the first character (for the default release message's repo name).
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_release_tags_with_and_without_prefixes() {
        assert_eq!(
            parse_release_tag("v1.2.3"),
            Some(("v".into(), "1.2.3".into()))
        );
        assert_eq!(parse_release_tag("1.0"), Some(("".into(), "1.0".into())));
        assert_eq!(
            parse_release_tag("release-2.0"),
            Some(("release-".into(), "2.0".into()))
        );
        assert_eq!(
            parse_release_tag("version_3.4.5"),
            Some(("version_".into(), "3.4.5".into()))
        );
        assert_eq!(
            parse_release_tag("v1.0.0-rc.1"),
            Some(("v".into(), "1.0.0-rc.1".into()))
        );
    }

    #[test]
    fn rejects_non_release_tags() {
        assert_eq!(parse_release_tag("nightly"), None);
        assert_eq!(parse_release_tag("verify-1.0"), None);
        assert_eq!(parse_release_tag("v"), None);
        assert_eq!(parse_release_tag("vX.Y"), None);
    }

    #[test]
    fn orders_versions_with_prereleases_below_releases() {
        use std::cmp::Ordering;
        let key = |t: &str| version_key(&parse_release_tag(t).unwrap().1);
        assert_eq!(
            version_cmp(&key("v2.0.0"), &key("v1.9.9")),
            Ordering::Greater
        );
        assert_eq!(
            version_cmp(&key("v1.10.0"), &key("v1.9.0")),
            Ordering::Greater
        );
        // A release outranks its own prerelease.
        assert_eq!(
            version_cmp(&key("v1.0.0"), &key("v1.0.0-rc.1")),
            Ordering::Greater
        );
        // rc outranks beta outranks alpha.
        assert_eq!(
            version_cmp(&key("1.0-rc.1"), &key("1.0-beta.1")),
            Ordering::Greater
        );
        assert_eq!(
            version_cmp(&key("1.0-beta.1"), &key("1.0-alpha.1")),
            Ordering::Greater
        );
    }

    #[test]
    fn reads_the_version_from_a_release_commit() {
        assert_eq!(
            release_commit_version("Release version 1.4.0"),
            Some("1.4.0".into())
        );
        assert_eq!(release_commit_version("Fix the parser"), None);
        assert_eq!(release_commit_version("Release version   "), None);
    }
}
