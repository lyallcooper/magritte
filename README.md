# Magritte

> *Ceci n'est pas Magit.*

Magritte is a fast, keyboard-first Git client inspired by
[Magit](https://magit.vc/). It runs as a standalone app, so you get Magit's
status-centered workflow without Emacs.

Use the status view to see the whole repository, move to the file, hunk, or
commit you care about, then act on it. Commands such as commit, branch, push,
and rebase open focused menus that show their available options.

Magritte is under active development. macOS on Apple silicon is the primary
platform. A best-effort Linux x86_64 build is also available.

## Install

Install the latest release with Homebrew:

```sh
brew install lyallcooper/magritte/magritte
```

Magritte requires `git` on your `PATH`.

To build an existing checkout from source, install Rust 1.96 and run:

```sh
mise install                 # optional, uses the pinned toolchain
cargo run --release -- .
```

The first build takes longer because it compiles GPUI and the pinned
dependencies. Later builds are incremental.

## Open a repository

Pass Magritte a repository or any path inside one:

```sh
magritte ~/code/my-project
```

With no path, Magritte opens the repository that contains your current
directory:

```sh
cd ~/code/my-project
magritte
```

Magritte normally starts in the background and returns control to your shell.
Use `magritte --foreground` when you want logs to remain attached to the
terminal. `MAGRITTE_FOREGROUND=1` does the same.

## Learn the workflow

The default keymap follows evil-collection-magit. These keys cover the usual
workflow:

| Key | Action |
| --- | --- |
| `j` / `k` | Move down or up |
| `Tab` | Expand or collapse the item at the cursor |
| `s` / `u` | Stage or unstage the selection |
| `x` | Discard the selection after confirmation |
| `c` | Open the commit menu |
| `b` | Open the branch menu |
| `p` / `F` | Open the push or pull menu |
| `l` | Open the log menu |
| `g r` | Refresh the repository |
| `:` | Search all available commands |
| `?` | Show commands and their keys |
| `Esc` / `Ctrl-g` | Cancel the current action |

Commands act on the item at the cursor. For example, `s` stages the current
hunk when the cursor is on a hunk, the current file when it is on a file, and
the full section when it is on a section heading. Visual selection lets you
apply an action to several rows at once.

If you prefer standard Magit and Emacs bindings, open Settings with `,` and set
the keymap to Vanilla. You can also set `keymap_preset = "vanilla"` in the
configuration file.

## Configure Magritte

Press `,` or choose **Magritte > Settings** to change themes, fonts, editor
behavior, and the keymap preset.

For key remapping, custom commands, status sections, background fetches, and
transient menu changes, edit:

```text
~/.config/magritte/config.toml
```

Magritte reloads the file when you save it. You can also place a sparse
override at `.git/magritte/config.toml` for one repository.

See the [configuration guide](docs/config.md) for every setting and practical
examples.

## Use Magritte as a mergetool

Magritte can resolve conflicted files opened by `git mergetool`. Add this to
your Git config:

```ini
[merge]
    tool = magritte
[mergetool "magritte"]
    cmd = magritte --mergetool "$MERGED"
    trustExitCode = true
```

Then run `git mergetool` during a merge or rebase. Magritte returns success
only after the selected file no longer contains unresolved conflict markers.

## Current limitations

- Release builds are ad hoc signed. They are not notarized or signed with a
  Developer ID.
- macOS on Apple silicon is the supported release target. Linux x86_64 builds
  are best effort.
- Paths that are not valid UTF-8 may be displayed with replacement characters.
- Magritte does not watch the working tree. It refreshes after its own
  commands, when the window regains focus, during auto-fetch, or when you run
  `g r`. This avoids expensive filesystem watching in large repositories.

See [Magit parity](docs/magit-parity.md) for a detailed list of supported and
missing Magit features.

## Develop

The workspace uses Rust 1.96, pinned in [`.mise.toml`](.mise.toml). Magritte
invokes the `git` executable rather than linking to libgit2.

```sh
cargo test
cargo clippy --all-targets
cargo fmt --check
```

The workspace has two crates:

- `magritte-core` contains synchronous, UI-independent Git operations.
- `magritte` contains the GPUI app, background work, and cancellation.

Read [AGENTS.md](AGENTS.md) for repository conventions and [PLAN.md](PLAN.md)
for the longer-term architecture and roadmap.

## License

Magritte is licensed under MIT, as declared in the workspace manifest.
