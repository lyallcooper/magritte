# Magritte Code Review Feedback

This is an adversarial review of the current work-in-progress codebase. The
goal here is not to criticize the direction: the foundation is promising. The
important part is that several current behaviors are too risky for an app that
aspires to Magit-level trust around source-control operations.

## High-Priority Findings

1. `crates/magritte-core/src/stage.rs:151` has a serious data-loss bug.
   `discard_staged_file` runs `git checkout HEAD -- path`. On an `MM` file,
   that discards both the staged change and the unrelated unstaged worktree
   edit. I verified this in a scratch repo: content went from `unstaged` back
   to `base`. It also fails entirely for staged new files and staged renames
   because `HEAD` does not contain the destination path. This directly violates
   the "no data-loss footguns" goal.

2. `crates/magritte/src/debug.rs:36` exposes a file-command control channel
   whenever `MAGRITTE_DEBUG_DIR` is set. With the default script path at
   `scripts/dbg.sh:19`, any local process that can write that directory can
   inject keys/clicks, take screenshots, and trigger destructive Git actions.
   Gate it behind debug builds or a feature, create a private 0700 nonce
   directory, and refuse world-writable control dirs.

3. Conflicts are not modeled safely. `crates/magritte-core/src/status.rs:83`
   treats unmerged entries as normal unstaged entries, and
   `crates/magritte/src/main.rs:431` allows ordinary stage/discard actions on
   them. Magit has conflict-specific discard behavior; this app currently risks
   marking conflicts resolved or failing in confusing ways.

4. The async story is incomplete. `crates/magritte-core/src/repo.rs:79` uses
   blocking `Command::output`; `crates/magritte/src/main.rs:806` drops stale
   results with a generation counter, but it does not cancel or kill the
   subprocess. A huge diff, hung credential prompt, or slow remote command can
   keep running after the UI moved on.

5. `crates/magritte/src/main.rs:1615` calls `repo.head_message()`
   synchronously from the UI path for amend/reword. That violates the stated
   "UI thread never blocks on git" rule.

6. Large-diff performance is still fragile. `crates/magritte/src/main.rs:851`
   runs whole-repo `git diff --numstat` after refresh, then can spawn 16
   prefetch diffs. `crates/magritte/src/main.rs:937` computes full syntax
   highlighting inside the UI update. Expanding a large file still builds the
   whole row model and highlight cache up front.

7. Patch application error handling is weak.
   `crates/magritte-core/src/repo.rs:127` returns early on stdin write failure,
   so if `git apply` exits early you can lose Git's stderr and may not wait on
   the child. Always close stdin, wait, and report the real Git error.

8. Multi-file region actions are not atomic.
   `crates/magritte/src/main.rs:406` applies batch actions sequentially. If
   file 3 fails, files 1 and 2 are already mutated. That is especially bad for
   destructive discards after a single confirmation.

9. Rename/copy metadata is parsed but mostly discarded.
   `crates/magritte-core/src/status.rs:66` stores `orig_path`, but
   `crates/magritte/src/main.rs:287` reduces action identity to just `path`.
   That is why staged rename discard and other path-sensitive operations are
   wrong.

10. The Magit/evil fidelity is currently aspirational. The reference maps `j`
    in status to a jump transient and evil remaps many motions/actions in
    `.reference/evil-collection/modes/magit/evil-collection-magit.el:299`; the
    app maps `j/k` to line movement in `crates/magritte/src/main.rs:2027`.
    That may be intentional, but the project needs a compatibility matrix
    instead of ad hoc key additions.

11. Error surfacing is too quiet. `crates/magritte/src/main.rs:1701` turns
    commit-diff load failure into an empty preview.
    `crates/magritte/src/config.rs:62` silently defaults unreadable config.
    Theme load failures go to stderr. GUI users need visible, actionable
    errors.

12. `crates/magritte/src/main.rs` is too large and too mixed: rendering, state
    transitions, Git action resolution, settings, commit editing, transient
    dispatch, and debug targeting all live in one 3376-line file. This will
    become a maintenance wall. Split state/action logic into testable modules
    before adding branch/log/stash/rebase.

## Hygiene

- `cargo test` passes.
- `cargo clippy --all-targets --all-features` passes with warnings.
- `cargo fmt --check` fails broadly, mostly mechanical formatting.
- `.mise.toml` uses `rust = "latest"`, which hurts reproducibility for a
  desktop app with pinned Git dependencies.

## Overall Assessment

The foundation is pointed in the right direction: porcelain v2 `-z`, a UI-free
core, and integration tests against throwaway repos are the right instincts.
But the staged-discard behavior, conflict handling, and real cancellation story
need attention before this should touch important repositories.
