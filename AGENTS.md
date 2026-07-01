# AGENTS.md

This file provides guidance to coding agents when working with code in this repository.

Magritte is a standalone macOS git client in the spirit of magit (fast, keyboard-driven, mouse-friendly) built on GPUI, without Emacs. See `README.md` for the user-facing overview and `docs/config.md` for configuration; `PLAN.md` has the long-form goals/architecture.

## Commands

Toolchain is pinned to Rust 1.96 in `.mise.toml` (`mise install`). Magritte shells out to `git` on `PATH` (it does not link libgit2).

```sh
cargo run --release -- [path-to-repo]   # run on a repo (defaults to cwd); omit --release for a faster dev build
cargo build
cargo test                              # core integration tests (crates/magritte-core/tests/*) + app unit tests
cargo test <name_substring>             # run a single test by name filter
cargo test -p magritte-core --test status   # run one integration-test file
cargo clippy --all-targets              # keep this warning-clean
```

`cargo run` detaches into the background like a GUI app; pass `--foreground` (or set `MAGRITTE_FOREGROUND`) to keep it attached for logs/debugging.

**Do NOT blindly run `cargo fmt`.** This repo uses compact hand-formatting and has no `rustfmt.toml`; default rustfmt reflows fine code and produces large unwanted churn across the whole crate. Match the surrounding style by hand instead.

### Live UI debugging (`scripts/dbg.sh`)

The fastest way to verify UI behavior is the debug control channel (compiled in only under `--features debug-capture`, which `up` enables):

```sh
scripts/dbg.sh up [repo]      # build with debug-capture + launch on a repo
scripts/dbg.sh key j          # inject a keystroke (e.g. key j, key down, key tab, key escape)
scripts/dbg.sh type ":"       # type literal text into the focused input
scripts/dbg.sh shot out.png   # screenshot (logical px == click coords) — then Read the png
scripts/dbg.sh down           # quit the running app
```

The bare `down` subcommand **quits** the app — to move the cursor down, inject the keystroke (`key j` or `key down`). Override the control dir with `MAGRITTE_DEBUG_DIR` to run an isolated instance (useful for testing on scratch repos without disturbing a dev instance). Screenshots can be flaky; retry if blank.

## Architecture

Two crates split at a **synchronous/async seam**:

### `magritte-core` — UI-free, synchronous
Drives the `git` CLI and returns plain data, so it's unit-testable against throwaway repos with no graphics stack. One module per git area (`status`, `diff`, `stage`, `branch`, `commit`, `merge`, `rebase`, `stash`, `remote`, …). Everything centers on `Repo` (`repo.rs`):

- The `run` / `run_optional` / `run_with_env` / `run_with_input` / `run_with_sequence_editor` family shell out to git through one `collect_output` path that honors an optional cancel flag and timeout.
- `cancellable()` / `with_cancel(flag)` / `with_timeout(d)` return tagged clones; the UI uses these so a refresh or `Ctrl-g` can kill an in-flight subprocess.
- Every invocation is recorded into a shared ring buffer (the `$` command log). `is_query()` classifies read-only commands so they can be hidden from that log by default.
- `transient.rs` defines the `Command`/`Transient` model (the popup command menus) shared with the UI layer.

When implementing or fixing git behavior, **match magit's source** in `.reference/magit/lisp/` (a gitignored, GPL behavior reference — not vendored, not distributed, and must be removed before any public release) rather than reaching for a simpler git command.

### `magritte` — the GPUI app
A **single `Entity<StatusView>` god-object** owns all UI state for every screen. This is deliberate (a GPUI view owns its state + behavior together; multi-entity message-passing buys nothing for a single-pane modal app — see the FB5 disposition in `FEEDBACK.md`). The lesson for working here: **split the file, not the entity.** `main.rs` (~5k lines) holds the `StatusView` struct and core view logic; cohesive slices live in sibling modules but stay `impl StatusView` blocks with `pub(crate)` methods over the same private fields:

- `render.rs` — all rendering (every screen layout, popups, title bar, the `uniform_list` row renderer) and the `Render` impl.
- `controller.rs` — command dispatch (`fire_action`), the `run_job*` / `run_command_job` background-job runners, picker orchestration, and the status-bar/report plumbing.
- `commands.rs` — the `commands()` **registry** (the single source of truth for what commands exist), default keymap, and `?`-menu / `:`-palette metadata.
- `input.rs` — `on_key`, the prefix-sequence state machine, dispatch, and the `:` palette.
- `navigation.rs` — cursor motion, selection, fold toggling, selection-anchor preservation.
- `settings.rs`, `picker.rs`, `theme.rs`, `kbd.rs`, `config.rs`, etc.

Key cross-cutting models:

- **Row model.** The status view is a flat, virtualized `uniform_list` of `Row`s (`RowKind`: Section/File/Hunk/Diff/Commit/Stash) rebuilt by `rebuild_rows` from the parsed `Status`, the fold state (`expanded` set + `collapsed_hunks`), and lazily loaded diffs. `SectionId` enumerates the status sections; `Target` is what an action acts on at point. Only on-screen rows become elements, so a huge diff stays cheap.
- **Async + staleness.** Anything slow (status, diffs, ref/branch/stash listings, transfers) is dispatched to `cx.background_executor()`. A monotonic `Generation` (`generation.rs`) stamps each async request; results that don't match the current stamp on completion are dropped, so a newer request can't be clobbered by a stale one. There are several such counters (status reads, screen loads, the prefix-timeout, the auto-fetch loop, the activity spinner). `read_cancel` / `job_cancel` are `Arc<AtomicBool>` flags that actually kill superseded/cancelled subprocesses.
- **Command flow.** Keymap, the `?` dispatch menu, and the `:` command palette all resolve through the one `commands()` registry — add a command there, not in `on_key`.
- **Config layering.** Global `~/.config/magritte/config.toml` is deep-merged with a per-repo `.git/magritte/config.toml` overlay (scalars: repo wins; `[keymap]`/`[transient.*]`: per-entry; `[[command]]`: concatenated by id). Repo-wide settings key off the *common* git dir (shared across worktrees); per-checkout UI state (fold state, `folds.toml`) keys off the *per-worktree* git dir. State files are written via `config::atomic_write_toml` (temp + rename). The native file watcher live-reloads config without a restart.

## Conventions

- **Comments** carry only what a future reader needs — don't narrate alternatives considered or justify a choice against one. Match the surrounding comment density.
- **Commits:** committing with `git commit --no-gpg-sign` is fine here (1Password signing is often locked). Do not include AI/tool attribution or thread-reference trailers in commit messages (no Claude/Codex/Amp co-author lines, generated-by lines, or Amp thread IDs). Keep `clippy --all-targets` warning-clean. Commit `TODO.md` updates alongside the work; `FEEDBACK.md`, `PLAN.md`, and the `scripts/` dev helpers stay out of commits unless asked.
- **Verify UI changes live** with `scripts/dbg.sh` + a screenshot before considering them done; verify core changes with `cargo test`.
