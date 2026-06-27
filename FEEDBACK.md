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

## Second-Pass Findings

These are from a re-review after the first batch of feedback was addressed.
The high-value fixes are real: staged discard is no longer the obvious
data-loss footgun it was, debug support is feature-gated, conflicts are blocked
for single-file actions, the toolchain is pinned, `git apply` stdin handling is
better, and the extracted `git_action.rs` layer is a useful separation.

1. `crates/magritte-core/src/stage.rs:172` silently swallows all failures from
   the staged-discard `git apply --reverse --reject` worktree step. The
   direction matches Magit (`.reference/magit/lisp/magit-apply.el:527`), but
   the observability does not. I reproduced an overlapping staged/unstaged hunk
   in a scratch repo: Git wrote `f.txt.rej` and exited `1`; Magritte would treat
   that as success, clear the status message, and leave the user to discover
   the reject file manually. This should return a structured "partial discard"
   result or at least surface "index reverted; worktree hunk rejected; see
   <path>.rej".

2. Real subprocess cancellation is still missing. `GIT_TERMINAL_PROMPT=0` in
   `crates/magritte-core/src/repo.rs:86` is a good fail-fast mitigation, but
   `Repo::run` and `Repo::run_with_input` still block until child exit. The UI
   generation counter drops stale status/diff results, but slow remote
   operations, hooks, SSH issues, or non-terminal blockers can still tie up
   executor work with no timeout, kill handle, or user-visible cancellation.

3. Visual-region conflict handling is inconsistent with single-file conflict
   handling. `crates/magritte/src/main.rs:1483` explains and refuses a direct
   action on a conflicted path, but `resolve_region_action` at
   `crates/magritte/src/main.rs:1444` silently skips conflicted files and
   applies the action to the rest of the selected region. For destructive
   operations, silently applying a subset of the user's selection is a bad
   trust boundary. Refuse the whole region or explicitly report the skipped
   paths before mutating anything.

4. The destructive confirmation text is now inaccurate. The comments and prompt
   around `crates/magritte/src/main.rs:1375` and
   `crates/magritte/src/git_action.rs:158` still say staged discard "reverts
   index and worktree to HEAD". The new behavior is better and more Magit-like:
   it preserves unrelated unstaged edits, may leave a staged-new file as
   untracked, and may create reject files for overlapping worktree hunks. The
   prompt should describe the real behavior, especially before a destructive
   action.

5. Git paths are still represented as lossy UTF-8 strings. `FileEntry.path` in
   `crates/magritte-core/src/status.rs:66` and parser paths produced by
   `lossy` at `crates/magritte-core/src/status.rs:347` cannot faithfully round
   trip non-UTF-8 Git paths. This is probably acceptable for many macOS repos,
   but it is below Magit-grade Git fidelity. Consider `bstr`/byte paths in the
   core and converting only at the UI edge.

6. Config persistence remains fragile. `load_reporting` in
   `crates/magritte/src/config.rs:71` silently treats an unreadable existing
   config file as defaults, and `save` at `crates/magritte/src/config.rs:102`
   writes the TOML directly instead of atomically writing a temp file and
   renaming it into place. Low severity, but easy to harden.

7. Commit-extend logic is duplicated. `Repo::commit_extend` exists in
   `crates/magritte-core/src/commit.rs:53`, but the transient executor builds
   the same `git commit --amend --no-edit` command separately in
   `crates/magritte-core/src/transient.rs:278`. This is not currently a bug,
   but it is drift-prone once commit switches or error handling become richer.

## Second-Pass Verification

- `cargo fmt --check` passes.
- `cargo test` passes.
- `cargo clippy --all-targets --all-features` passes.
- Cargo still reports a future-incompatibility warning for upstream
  `block v0.1.6`, pulled in through GPUI/cocoa. `cargo tree -i block` confirms
  this is not Magritte code, but it is worth tracking because a future Rust
  release may turn it into a hard error.

## Third-Pass Findings (2026-06-27)

Scope reviewed: the Rust workspace manifests, app crate, core crate, tests,
docs, helper script, gitignore/mise metadata, and the existing feedback trail.
I treated `.reference/` as non-owned reference material because `.gitignore`
documents it as local upstream copies, not Magritte source.

Overall: the codebase is in much better shape than a first-pass prototype. The
core has useful integration coverage, the app tests cover a meaningful slice of
UI state behavior, formatting/lints/tests are clean, and the extraction of
`git_action.rs` helped. The remaining issues are mostly product-correctness and
architecture drift rather than basic Rust hygiene.

1. **Several UI paths still run Git synchronously, despite the stated
   architecture.** `crates/magritte/src/main.rs:2911` calls
   `Repo::remote_targets()` directly from `remote_targets()`, and command
   dispatch paths synchronously enumerate stashes or branches before opening
   their UI: `stash_list()` at `main.rs:3071`, branch/ref listing at
   `main.rs:3169`, `main.rs:3199`, `main.rs:3215`, reset target listing at
   `main.rs:3254`, merge target listing at `main.rs:3300`, and remote branch
   listing at `main.rs:3801`/`main.rs:3807`. These are local Git calls, but in
   large repos, on network filesystems, or with slow config/hooks, they can
   freeze key handling and painting. The top-level design claim is that all Git
   work runs on the background executor; these call sites should open a picker
   or transient immediately with a loading/empty state, then populate choices
   from the background executor. Caching branch/remote targets after refresh
   would also reduce repeated command execution.

2. **The commit preview can omit changes that `git commit --all` will include.**
   `load_commit_diff()` at `crates/magritte/src/main.rs:4456` only falls back to
   the unstaged diff when the staged diff is empty (`main.rs:4472`). With
   `--all`, Git commits staged changes plus tracked unstaged modifications and
   deletions. If both exist, Magritte previews only the staged side, so the user
   can commit unseen tracked work. The preview should show both staged and
   tracked unstaged diffs whenever `--all` is active, with clear labels. It
   should still avoid implying that untracked files are included.

3. **The commit-all confirmation text overpromises what Git will do.**
   `start_commit()` asks "Nothing staged. Commit all uncommitted changes?" at
   `crates/magritte/src/main.rs:4247`, but the confirmation appends `--all`,
   which commits only tracked modifications/deletions. Untracked files are not
   included. The prompt should say something like "Commit all tracked unstaged
   changes?" or the implementation should offer a true stage-all workflow before
   committing.

4. **Batch prechecking is not actually atomic for whole-file actions.**
   `Action::Batch` promises to verify every part before mutating anything
   (`crates/magritte/src/git_action.rs:107`), but `check()` only dry-runs
   `ApplyRegion`; all whole-file actions return `Ok(())` at `git_action.rs:143`.
   `resolve_region_action()` can place whole-file actions in a batch when a
   visual selection includes file rows, including destructive discard/clean
   operations. That means a multi-file action can still partially mutate the
   repo after one confirmation if an early whole-file action succeeds and a
   later one fails. Either make the precheck conservative for whole-file
   operations or stop presenting mixed whole-file batches as all-or-nothing.

5. **Saving a push remote happens before the push succeeds.** In
   `run_transfer()`, `repo.set_push_remote(&branch, &chosen)` runs before
   `repo.push_to(...)` (`crates/magritte/src/main.rs:4031`). If the push is
   rejected, offline, pointed at the wrong remote, or otherwise fails, the
   branch config has already been changed, and that write error is ignored. Save
   the push remote only after a successful non-dry-run push, and surface a
   follow-up config-write failure. If the goal is upstream setup, prefer Git's
   push/upstream flags where they match the workflow instead of pre-mutating
   config.

6. **The arbitrary Git command prompt cannot handle normal shell-style
   quoting.** `run_git_command()` uses `split_whitespace()` at
   `crates/magritte/src/main.rs:3392`, while the UI advertises "type git
   command" behavior and strips an optional leading `git`. Commands involving
   paths with spaces, quoted refspecs, or escaped values will be split into the
   wrong argv. Use a shell-word parser such as `shell-words`/`shlex` without
   invoking a shell, then surface parse errors in the status area. This keeps
   the current no-shell safety property while matching user expectations.

7. **Syntax highlighting can still do too much work on the UI thread.**
   Prefetch warms up to `PREFETCH_FILE_CAP = 16` files and
   `PREFETCH_LINE_CAP = 2000` lines per file (`crates/magritte/src/main.rs:967`
   and `main.rs:970`). Each completed diff then calls
   `highlight::highlight_diff()` from inside the UI update at `main.rs:1956`.
   `highlight.rs:214` caps one file at 2000 lines, but there is no aggregate
   per-refresh budget. A refresh that warms sixteen 2000-line supported files
   can still run a large amount of tree-sitter work on the UI thread. Render
   plain text first and highlight incrementally, add a per-refresh aggregate
   budget, or move highlight generation off the UI path if GPUI's styling model
   allows it.

8. **Visual selection resolution clones whole diffs repeatedly.**
   `resolve_region_action()` walks every selected row and calls `diff_for()`
   while resolving hunk headers (`crates/magritte/src/main.rs:2598`), and
   `diff_for()` itself returns a cloned `FileDiff` (`main.rs:2400`). It clones
   again when constructing the final action at `main.rs:2642`. For large
   expanded diffs and multi-row selections this is unnecessary churn. Add a
   borrowed lookup (`diff_for_ref`), pre-index selected files, or clone exactly
   once when building the final `Action`.

9. **Stash drop and branch delete need stronger destructive affordances.**
   `run_stash_action()` executes `repo.stash_drop()` directly after picker
   selection (`crates/magritte/src/main.rs:4203`). `run_branch_action()`
   executes `repo.delete_branch(&chosen, false)` after picker selection
   (`main.rs:4136`). Git protects unmerged branches with `-d`, so branch delete
   is lower risk, but stash drop is easy to trigger and cannot be recovered
   through the app. These should get explicit yes/no confirmations, consistent
   with the existing confirmations for hard reset, discard, abort, and quit with
   an active editor.

10. **Manual conflict-resolution detection is too textual and too forgiving.**
    `has_conflict_markers()` only rejects lines starting with `<<<<<<< ` or
    `>>>>>>> ` and treats read errors as "no markers"
    (`crates/magritte/src/main.rs:2791`). It can miss malformed leftover
    `=======`/`|||||||` sections, binary/unreadable conflict paths, or files
    whose marker spacing has been edited. Because staging a conflicted path
    marks it resolved, failure should be conservative. Also, `ours`/`theirs`
    labels should become sequence-aware for rebase/cherry-pick contexts; Git's
    meanings are notoriously easy to misread there.

11. **`run_optional()`-style Git probes can hide unexpected failures.** The core
    intentionally uses optional Git commands for absent refs/config, but
    swallowing every non-zero exit as "none" makes UI decisions look clean when
    the real state is "Git failed". This is acceptable for a few tightly scoped
    probes, but it should not spread. For branch/remote titlebar and transient
    target data, consider distinguishing "missing value" from permission,
    corruption, config, and subprocess errors so the app can show a degraded
    state instead of silently dropping options.

12. **The repo still has no README.** `PLAN.md`, `TODO.md`, and
    `docs/extensibility.md` are useful engineering artifacts, but a standalone
    Git app needs a short `README.md`: what Magritte is, platform/toolchain
    expectations, how to build/run, feature flags such as debug behavior, basic
    keymap entry points, and current safety limitations. This is not a code bug,
    but it is a real adoption and maintenance gap.

## Third-Pass Verification

- `cargo fmt --check` passes.
- `cargo test` passes: app unit tests and all `magritte-core` integration tests.
- `cargo clippy --all-targets --all-features` passes.
- `cargo test --all-features` passes.
- Cargo still reports the upstream future-incompatibility warning for
  `block v0.1.6`; this appears to come through GPUI/cocoa rather than Magritte
  code, but it remains worth tracking.

## Comment Quality Notes

Overall, the comments are a net positive. They are noticeably better than the
usual "repeat the code in English" style: many explain invariants, Git behavior,
UI framework constraints, or historical choices that are not obvious from the
implementation alone.

The strongest comments are in the places where a future maintainer would
otherwise need to rediscover a subtle rule:

- `crates/magritte-core/src/stage.rs:7` explains the forward/reverse patch
  construction rules for partial staging/unstaging/discarding. This is exactly
  the right level of detail: it documents the mental model and the danger, not
  just the mechanics.
- `crates/magritte-core/src/repo.rs:142` explains the signal-mask workaround and
  `GIT_TERMINAL_PROMPT=0`. That context is operationally important and would be
  very hard to infer from the subprocess setup alone.
- `crates/magritte-core/src/diff.rs:185` explains why `str::lines()` is avoided
  for CRLF fidelity. This is a good example of a small implementation choice
  backed by a concrete correctness reason.
- `crates/magritte/src/debug.rs:65` explains why the debug control directory
  must be private. That is useful security context, not decorative prose.
- `crates/magritte/src/highlight.rs:29` explains grammar-name quirks in
  `gpui-component`, which is the kind of dependency-specific knowledge that
  belongs in a comment.

The main weakness is density, especially in `crates/magritte/src/main.rs`.
The file is nearly 9k lines and has comments acting as navigation markers,
architecture notes, UI rationale, Magit compatibility notes, and local
implementation notes all in one place. The comments help, but they also reveal
that `main.rs` is carrying too many concepts. Some comments would become
unnecessary if command dispatch, transient/picker orchestration, commit-editor
state, render helpers, and row/action resolution were split into smaller
modules with clear names.

The highest-risk comments are the ones that state guarantees the code no longer
fully provides. For example, `crates/magritte/src/git_action.rs:108` says a
multi-file region "can't half-apply", but `check()` only meaningfully dry-runs
`ApplyRegion`; whole-file actions still return `Ok(())`. This kind of stale
confidence comment is worse than no comment because it tells a maintainer a
safety property exists when it does not. Treat comments that claim safety,
atomicity, data-loss behavior, or UI-thread behavior as part of the contract and
audit them when behavior changes.

There are also comments that reference context the reader may not have. The
Magit and evil-collection references are appropriate because Magritte is
explicitly inspired by Magit, but they should usually be paired with the
observable behavior being copied. A comment like "Reset is `O`
(evil-collection-magit)" is less useful than one that also says what tradeoff
that binding makes inside Magritte's own keymap. References to
`.reference/magit/...` are even more fragile because `.reference/` is local
ignored material; future readers may not have those files. Prefer documenting
the behavior inline and using external references only as supporting provenance.

Recommended cleanup policy:

- Keep comments that explain invariants, Git edge cases, GPUI/dependency
  constraints, security assumptions, or non-obvious user-facing tradeoffs.
- Remove or shorten comments that only restate a function name, enum variant, or
  immediately obvious control flow.
- When extracting modules from `main.rs`, let module boundaries and names carry
  more of the explanation so local comments can focus on surprising details.
- Audit comments during fixes, especially any that use words like "atomic",
  "safe", "destructive", "preserve", "never", "always", or "background".
- For Magit-inspired behavior, describe the local behavior first, then cite
  Magit/evil-collection as provenance only when it adds useful context.

## Fourth-Pass Findings (2026-06-27)

Scope reviewed after the latest fixes: current manifests, README, core crate,
app crate, tests, and the specific areas touched by the recent commits
(`--all` preview, push-remote ordering, conflict handling, run-git parsing,
async picker population, and borrowed diff lookups).

Overall: the addressed feedback landed well. The core is cleaner, the README is
useful, quoted git commands now behave like users expect, push config mutation
is no longer ahead of the push, conflict staging is more conservative, and the
branch/stash/ref/reset/merge/rebase pickers no longer do their large listings
on the UI thread. The remaining findings are narrower and mostly second-order.

1. **The "UI thread never blocks on git" contract is still too strong.**
   `README.md:48` says every git call is dispatched to a background executor,
   but some synchronous git probes remain in UI command paths. Opening branch
   unnecessarily calls `remote_targets()` before rendering a transient
   (`crates/magritte/src/main.rs:592`), and push/pull/fetch/rebase still call
   `remote_targets()` synchronously (`main.rs:618`, `main.rs:640`,
   `main.rs:644`, `main.rs:648`). Transfer fallback paths also synchronously
   call `remotes()` / `remote_branches()` (`main.rs:3781`, `main.rs:3815`,
   `main.rs:3849`, `main.rs:3855`). If keeping these synchronous is a deliberate
   bounded-cost tradeoff, soften the README/PLAN wording and remove the
   unnecessary branch-transient `remote_targets()` call. If the invariant is
   meant literally, these transfer paths need the same open-then-populate
   treatment as the fixed branch/ref pickers.

2. **The new `--all` commit preview fails on unborn branches.**
   `Repo::diff_tracked_vs_head()` uses `git diff HEAD`
   (`crates/magritte-core/src/diff.rs:126`), and `load_commit_diff()` uses that
   path for `--all` (`crates/magritte/src/main.rs:4611`). In an unborn repo,
   `git diff HEAD` exits with "unknown revision", so the editor can show "diff
   unavailable" for an initial commit path. I verified this in a scratch repo.
   Use the empty tree when `HEAD` is absent, or fall back to the staged diff for
   unborn branches.

3. **Async option completions can race into the wrong option prompt.**
   `open_listed_picker()` has a generation guard, but `open_option_prompt()`
   only checks that the current popup is *some* `SetOption`
   (`crates/magritte/src/main.rs:3195`). If an authors/files completion returns
   after the user has opened another option prompt, stale candidates can populate
   the newer prompt. Reuse `picker_gen` here too, or capture the option key and
   only apply results when the still-open prompt matches that key.

4. **Mixed whole-file visual batches remain partial-mutation workflows.**
   The misleading guarantee comment is fixed, which is good, but the underlying
   behavior remains: `Action::Batch` can precheck region patches, while
   whole-file actions still report `Ok(())` from `check()` and then run
   sequentially (`crates/magritte/src/git_action.rs:107`). For destructive
   selections that include whole-file rows, a later failure can still leave
   earlier files changed after one confirmation. Either add conservative
   prechecks for whole-file destructive operations, or make the confirmation /
   error reporting explicitly say the operation is sequential and may be
   partially applied.

5. **Syntax highlighting still lacks an aggregate foreground budget.**
   Individual file diffs are capped at 2000 lines
   (`crates/magritte/src/highlight.rs:214`), but prefetch can warm several files
   and each loaded diff computes highlighting during the UI update
   (`crates/magritte/src/main.rs:1935`, `main.rs:1991`). A manual theme change
   also recomputes all loaded highlights in one pass (`main.rs:1821`). This is
   much less severe than unbounded highlighting, but an aggregate per-frame or
   per-refresh budget would make the performance story more robust.

6. **A couple of docs/comments drifted during the cleanup.**
   The push-elsewhere candidate-list doc comment is attached to `all_branches()`
   instead of `seed_push_branches()` (`crates/magritte/src/main.rs:8490`).
   `README.md:34` says `P` is push, but the app binds lowercase `p`
   (`crates/magritte/src/main.rs:639`). Small issues, but worth fixing because
   they are exactly the kind of stale-context comments that mislead later
   maintainers.

## Fourth-Pass Verification

- `cargo fmt --check` passes.
- `cargo test` passes.
- `cargo clippy --all-targets --all-features` passes.
- `cargo test --all-features` passes.
- Cargo still reports the upstream future-incompatibility warning for
  `block v0.1.6`.
- The worktree was otherwise clean except for `FEEDBACK.md`; `git status` still
  emits the existing fsmonitor IPC warning.

<!-- ───────────────────────── ADDRESSED UP TO HERE ─────────────────────────
First pass (#1–#12), second (SP1–SP7), third (TP1–TP12), and fourth (FP1–FP6)
addressed — commits up to 6fa6d47.

Fourth pass, what changed:
  - FP1  the branch transient no longer resolves remote_targets it doesn't use;
         README/PLAN "never blocks on git" softened to admit the bounded inline
         config/ref probes (e.g. @{upstream}) we keep synchronous.
  - FP2  diff_tracked_vs_head fell over on an unborn branch (no HEAD); it now
         falls back to the staged diff. Tests added.
  - FP3  option-prompt completions are guarded by picker_gen, so a late
         authors/files load can't fill a newer prompt.
  - FP4  a partially-applied whole-file Batch now reports "applied N of M; the
         rest were not" instead of only the last error.
  - FP6  fixed the doc comment that drifted onto all_branches (belongs to
         seed_push_branches); README/PLAN push key corrected to `p`.

Deferred (by decision, not oversight):
  - #4 / SP2 (real subprocess cancellation): milestone M6. GIT_TERMINAL_PROMPT=0
    is the interim fail-fast mitigation.
  - #10 (evil/magit keybinding matrix): premise is off — j/k as motion matches
    evil-collection-magit (vanilla magit's j-as-jump doesn't apply). A written
    compatibility matrix is a nice-to-have, not a bug.
  - SP5 (byte/bstr paths vs lossy UTF-8): broad core refactor, low value for a
    macOS-only v1; revisit for non-UTF-8 path fidelity.
  - TP1 / FP1 (remote-listing async): `git remote` / remote_targets read config
    and a couple of refs — bounded, not worktree/ref-count-bound — and their
    count-dependent dispatch / transient-header rendering don't fit
    open-then-populate without flicker, for negligible benefit. Kept synchronous
    (and the README/PLAN wording now reflects this).
  - TP7 / FP5 (aggregate highlight budget): the load path highlights one file per
    UI tick (per-file 2000-line cap); the only multi-file pass
    (recompute_highlights) runs on a manual theme change, still per-file capped.
    No change.
  - TP10 (match bare =======/|||||||): kept matching only the
    <<<<<<< / >>>>>>> pair; a bare ======= occurs in ordinary text and would
    false-positive.
  - FP4 (conservative whole-file prechecks): whole-file ops can't be dry-run; we
    report partial application rather than fake atomicity.
Add any new feedback BELOW this marker.
────────────────────────────────────────────────────────────────────────── -->

