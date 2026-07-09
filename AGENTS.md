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
cargo fmt                               # stock rustfmt; run before committing
```

`cargo run` detaches into the background like a GUI app; pass `--foreground` (or set `MAGRITTE_FOREGROUND`) to keep it attached for logs/debugging.

Formatting is stock `cargo fmt` (config in `rustfmt.toml`); run it (or `cargo fmt --check`) before committing.

### Live UI debugging (`scripts/dbg.sh`)

The fastest way to verify UI behavior is the debug control channel (compiled in only under the `debug`/`debug-capture` features — `up` builds with `debug-capture` so `shot` works while backgrounded):

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
- `transient/` defines the `Command`/`Transient` model (the popup command menus, `mod.rs`) and the built-in menu definitions (`menus.rs`), shared with the UI layer.

When implementing or fixing git behavior, **match magit's source** in `.reference/magit/lisp/` (a gitignored, GPL behavior reference — not vendored, not distributed, and must be removed before any public release) rather than reaching for a simpler git command.

### `magritte` — the GPUI app
A **single `Entity<StatusView>` god-object** owns all UI state for every screen. This is deliberate (a GPUI view owns its state + behavior together; multi-entity message-passing buys nothing for a single-pane modal app — see the FB5 disposition in `FEEDBACK.md`). The lesson for working here: **split the file, not the entity.** `main.rs` (~2k lines) holds the `StatusView` struct, the `Screen` enum, `main()`, and the registry/keymap invariant tests; cohesive slices live in sibling modules but stay `impl StatusView` blocks with `pub(crate)` methods over the same private fields:

- `render.rs` — the shared rendering helpers, status rows, overlays, and the `Render` impl, with per-surface renderers split alongside (`title_bar.rs`, `transient_render.rs`, `picker_render.rs`, `list_render.rs`, `diff_render.rs`).
- `controller.rs` — command dispatch (`fire_action`) and the picker orchestration those prompts share.
- `jobs.rs` — the `run_job*` / `run_command_job` background-job runners, the status-toast/report plumbing, and the auto-fetch/update-check loops; `transfer.rs` — push/pull/fetch orchestration; `rebase_flow.rs` — the interactive-rebase todo editor and the mid-rebase reword flow.
- `commands.rs` — the `commands()` **registry** (the single source of truth for what commands exist), default keymap, and `?`-menu / `:`-palette metadata.
- `input.rs` — `on_key`, the prefix-sequence state machine, dispatch, and the `:` palette.
- `navigation.rs` — cursor motion, selection, fold toggling, selection-anchor preservation.
- `row_build.rs` — the status `Row` list builder; `status_loader.rs` — the async status/diff engine; `staging.rs` — act-at-point actions and the diff cache.
- `commit_editor.rs` — the in-app commit message editor (50/72 assistance, diff preview); `vim/` — its modal Vim engine, a pure keystroke→`Action` layer (applied by `vim/apply.rs`, tested headlessly in `vim/tests.rs` — see `docs/dev/vim-mode.md`).
- `settings.rs`, `picker.rs`, `theme.rs`, `kbd.rs`, `config.rs`, `state.rs`, `watchers.rs`, etc.

Key cross-cutting models:

- **Row model.** The status view is a flat, virtualized `uniform_list` of `Row`s (`RowKind`: Plain/Header/Section/File/HunkHeader/Diff/Commit/Stash) rebuilt by `rebuild_rows` from the parsed `Status`, the fold state (`expanded` set + `collapsed_hunks`), and lazily loaded diffs. `SectionId` enumerates the status sections; `Target` is what an action acts on at point. Only on-screen rows become elements, so a huge diff stays cheap.
- **Async + staleness.** Anything slow (status, diffs, ref/branch/stash listings, transfers) is dispatched to `cx.background_executor()`. A monotonic `Generation` (`generation.rs`) stamps each async request; results that don't match the current stamp on completion are dropped, so a newer request can't be clobbered by a stale one. There are several such counters (status reads, screen loads, the prefix-timeout, the auto-fetch loop, the activity spinner). `read_cancel` / `job_cancel` are `Arc<AtomicBool>` flags that actually kill superseded/cancelled subprocesses.
- **Command flow.** Keymap, the `?` dispatch menu, and the `:` command palette all resolve through the one `commands()` registry — add a command there, not in `on_key`.
- **Config layering.** Global `~/.config/magritte/config.toml` is deep-merged with a per-repo `.git/magritte/config.toml` overlay (scalars: repo wins; `[keymap]`/`[transient.*]`: per-entry; `[[command]]`: concatenated by id). Repo-wide settings key off the *common* git dir (shared across worktrees); per-checkout UI state (fold state, `folds.toml`) keys off the *per-worktree* git dir. State files are written via `state::atomic_write_toml` (temp + rename). The native file watcher live-reloads config without a restart.

## Conventions

- **Defer to Magit** when there is any implementation ambiguity. Keybindings and features should work the same way they do as in Magit, unless we have specifically decided otherwise. The "evil" keymap preset keys should match evil-collection-magit, while the "vanilla" keymap preset keys should match vanilla emacs/magit.
- **Comments** carry only what a future reader needs—don't narrate alternatives considered or justify a choice against one. Match the surrounding comment density.
- **Commits:** Do not include AI/tool attribution or thread-reference trailers in commit messages (no Claude/Codex/Amp co-author lines, generated-by lines, or Amp thread IDs). Keep `clippy --all-targets` warning-clean. Commit `TODO.md` updates alongside the work; `FEEDBACK.md` and `PLAN.md` stay out of commits unless asked.
- **Verify UI changes live** with `scripts/dbg.sh` + a screenshot before considering them done; verify core changes with `cargo test`.
- **Refactors:** don't be afraid of big refactors. Instead of always working incrementally, constantly asking yourself if the code is in the best possible state. If there is a better architecture, tech debt you can pay down, abstractions you could improve, then you should do the work now to leave the code better than you found it. Of course you still need to do so carefully to ensure you don't break anything along the way.
- **Writing style:** When writing anything user facing (e.g. docs), we must follow some simple rules:
  - Remember the audience: information should be directly relevant to users, and not contain references to internals that they do not care about.
  - Keep it succinct but accurate: information should be presented directly, without fluff or padding, as to provide maximum utility to the user.
  - Keep it organized: sections should be laid out in a logical order, with higher relevance items coming first as appropriate
  - Use examples: whenever helpful, show via example instead of just telling via description. But be strategic so we don't make the docs overly long
  - Basic style: only use em dashes ('—') when appropriate. And don't put spaces on either sides of the dash when using them. Remember that em dashes look bad in monospace text, so prefer two hyphens ('--') instead in that context.
