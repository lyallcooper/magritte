# Magritte

> *Ceci n'est pas Magit.*

Magritte is a standalone macOS git client in the spirit of
[magit](https://magit.vc/) — fast, keyboard-driven, mouse-friendly — without
Emacs. The status buffer is home base, you act on the thing at point, and
commands open transient popups. It's built on [GPUI](https://www.gpui.rs/) and
designed from the start to stay responsive in very large repositories: git work
runs off the UI thread, diffs are computed lazily, and rendering is virtualized.

This is a work in progress, not yet a packaged app. See [PLAN.md](PLAN.md) for
the goals, architecture, and milestones.

## Requirements

- macOS (the only target for v1)
- [Rust](https://www.rust-lang.org/) 1.96 — pinned in [`.mise.toml`](.mise.toml);
  `mise install` sets it up, or use any equivalent toolchain
- `git` on `PATH` (Magritte shells out to it rather than linking libgit2)

## Build & run

```sh
cargo run --release -- [path-to-repo]    # defaults to the current directory
```

Omit `--release` for a faster, unoptimized dev build. The first build compiles
GPUI and pulls pinned git dependencies, so it takes a while; later builds are
incremental.

## Keybindings

The default keymap mirrors **evil-collection's magit**, so existing muscle
memory transfers (`j`/`k` to move, `TAB` to fold, `s`/`u` to stage/unstage, `c`
to commit, `p`/`F` to push/pull, `l` for log, `Z` for stash, and so on). Press
`?` in the app for the dispatch/help popup. The complete key list — and how to
remap or unbind keys with a `[keymap]` table — is in
[`docs/config.md`](docs/config.md#keymap); every keyboard action has a mouse
equivalent.

## Configuration

Settings live in `~/.config/magritte/config.toml` (or `$XDG_CONFIG_HOME/…`),
loaded at startup and re-read live on change. The Settings screen (`,`) edits
appearance, fonts, and editor options; a `[keymap]` table remaps keys.
[`docs/config.md`](docs/config.md) documents every key, valid values, and the
command ids you can bind.

## Architecture

Two crates, split at a synchronous/async seam:

- **`magritte-core`** — UI-free and synchronous. Drives the `git` CLI and
  returns plain data, so it's unit-testable against throwaway repos with no
  graphics stack.
- **`magritte`** — the GPUI app. Owns all asynchrony and cancellation. Anything
  that can be slow — status, diffs, ref/branch/stash listings, transfers — is
  dispatched to a background executor, so the UI thread never blocks on it. (A
  few bounded config/ref probes, e.g. resolving `@{upstream}`, still run inline;
  they don't scan the worktree.)

Keymap remapping, transient extension, user-defined `[[command]]` commands, and
the `:` command palette are built and documented in
[`docs/config.md`](docs/config.md); [`docs/extensibility.md`](docs/extensibility.md)
tours them.

## Development

```sh
cargo test                  # core integration tests + app unit tests
cargo clippy --all-targets
cargo fmt
```

A dev-only **debug control channel** can drive a running instance (inject keys,
click elements, screenshot) for scripted testing:

```sh
scripts/dbg.sh up           # build with --features debug-capture and launch
scripts/dbg.sh key j        # inject a keystroke
scripts/dbg.sh shot out.png # screenshot the window
scripts/dbg.sh down
```

It is compiled out of normal release builds entirely.

## Current limitations

- Not yet code-signed, notarized, or packaged as a `.app`.
- macOS only; non-UTF-8 paths are handled lossily.
- Refresh is on-demand (`g r`), after our own commands, and on window focus
  (opt-out via `refresh_on_focus`); there is no filesystem watcher (intentionally
  — it's a large-repo hazard magit also avoids).

## License

Declared MIT in the workspace manifest. Licensing is not yet finalized for
public release — the local `.reference/` directory (git ignored) contains
upstream magit/evil-collection sources used only as a behavior reference; it is
not part of Magritte and is not distributed. See the licensing note in
[PLAN.md](PLAN.md#7-risks--open-questions) before going public.
