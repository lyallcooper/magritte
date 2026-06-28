# Configuration

Magritte reads a single TOML file:

```
$XDG_CONFIG_HOME/magritte/config.toml      # or, if XDG_CONFIG_HOME is unset:
~/.config/magritte/config.toml
```

It's loaded at startup and re-read live when the file changes — edits apply
without a restart. The Settings screen (`,`, or **Magritte → Settings** / `Cmd+,`)
writes the same file and is the easiest way to change appearance options; this
doc covers editing by hand, plus the `[keymap]` remapping the UI doesn't expose.

A missing file means defaults. A file that fails to parse is ignored, with the
error shown in the status bar — at startup that falls back to defaults; on a
live reload your current settings stay in place.

## Settings

All scalar keys are top-level. Every key is optional; omit one for its default.

| Key | Values | Default | Meaning |
|-----|--------|---------|---------|
| `appearance` | `"auto"`* / `"light"` / `"dark"` | `auto` | `auto` follows the system; otherwise force one mode. |
| `light_theme` | theme name | `Selenized White` | Theme used in light mode. |
| `dark_theme` | theme name | `Selenized Black` | Theme used in dark mode. |
| `font` | font family | platform monospace | Monospace font for code, diffs, and tabular rows. |
| `ui_font` | font family | *(uses `font`)* | Proportional font for chrome (menus, headers, labels). Empty = monospace everywhere. |
| `editor` | command or app name | OS default opener | External editor for "open file" (`Return`) — see below. |
| `commit_in_editor` | `true` / `false` | `false` | Write commit messages in `commit_editor` instead of the in-app editor. |
| `commit_editor` | command | *(none)* | Blocking editor command used as `GIT_EDITOR`, e.g. `zed --wait`, `code --wait`, `nvim`. Only used when `commit_in_editor = true`. |
| `commit_title_ruler` | `true` / `false` | `true` | Highlight commit-summary characters past column 50. |
| `commit_body_wrap` | `true` / `false` | `true` | Auto-hard-wrap the commit body at column 72. |

\* `appearance` defaults to auto whether you write `"auto"` or leave it empty.

**Theme names** are the entries in the Settings → *Light theme* / *Dark theme*
dropdowns. Bundled families: GitHub, Solarized, Selenized, Gruvbox, Catppuccin,
Nord, Dracula, tao (each with light and dark variants).

**`editor`** is either a CLI command run directly (`code -w`, `zed`, `subl -w`)
or, on macOS, an application name opened via `open -a` (`Zed`,
`Visual Studio Code`). Empty opens the file in the OS default app. The file
opens at the line under the cursor for editors with a known goto syntax.
Terminal editors are out of scope (a GUI app can't reliably launch one).

### Example

```toml
appearance = "dark"
light_theme = "Selenized White"
dark_theme = "Dracula"
font = "Berkeley Mono"
editor = "zed"

commit_in_editor = false
commit_title_ruler = true
commit_body_wrap = true
```

## Keymap

The default keymap mirrors evil-collection's magit. A `[keymap]` table overrides
it: each entry maps a **keystroke** to a **command id**, or to the sentinel
`"unbound"` to remove a default binding.

```toml
# [keymap] must come after the scalar keys above (TOML table rule).
[keymap]
"K" = "branch-delete"   # bind K to delete-branch
"x" = "unbound"         # remove the default discard binding
"E" = "commit-extend"   # leaf commands work too, not just top-level ones
```

- **Keystrokes** are case-sensitive single keys as shown in the `?` menu
  (`s` vs `S`, `f` fetch vs `F` pull). An unknown command id is ignored with a
  startup warning rather than silently dropped.
- **Reserved** (handled before the keymap, so binding them has no effect):
  the motions `j` `k` `g g` `G` `g j` `g k`, the fold key `Tab`, the refresh
  sequence `g r`, and any key inside a transient, picker, or visual mode.

### Command ids

Any id below can be bound to a key. Top-level ones have a default key; the rest
are reachable today only through their prefix's transient or the `:` palette.

| id | default key | command |
|----|-------------|---------|
| `commit` | `c` | Commit (transient) |
| `branch` | `b` | Branch (transient) |
| `stash` | `Z` | Stash (transient) |
| `reset` | `O` | Reset (transient) |
| `rebase` | `r` | Rebase (transient) |
| `merge` | `m` | Merge (transient) |
| `ignore` | `i` | Ignore (transient) |
| `log` | `l` | Log (transient) |
| `push` | `p` | Push (transient) |
| `pull` | `F` | Pull (transient) |
| `fetch` | `f` | Fetch (transient) |
| `git-command` | `!` | Run a raw git command |
| `stage` | `s` | Stage the selection |
| `unstage` | `u` | Unstage the selection |
| `stage-all` | `S` | Stage all |
| `unstage-all` | `U` | Unstage all |
| `discard` | `x` | Discard the selection |
| `open-file` | `Return` | Open file at point in `editor` |
| `fold` | `Tab` | Fold / unfold (reserved) |
| `refresh` | `g r` | Refresh status (reserved) |
| `visual` | `v` | Toggle visual selection |
| `yank` | `y` | Copy the selection |
| `settings` | `,` | Open Settings |
| `git-log` | `$` | Open the git command log |
| `commit-create` | — | Create commit |
| `commit-amend` | — | Amend commit |
| `commit-reword` | — | Reword commit |
| `commit-extend` | — | Extend commit (keep message) |
| `branch-checkout` | — | Checkout branch/revision |
| `branch-create` | — | Create branch |
| `branch-create-checkout` | — | Create and checkout branch |
| `branch-rename` | — | Rename branch |
| `branch-delete` | — | Delete branch |
| `push-pushremote` / `push-upstream` / `push-elsewhere` | — | Push variants |
| `pull-pushremote` / `pull-upstream` / `pull-elsewhere` | — | Pull variants |
| `fetch-pushremote` / `fetch-upstream` / `fetch-all` / `fetch-elsewhere` | — | Fetch variants |
| `stash-push` / `stash-push-all` / `stash-apply` / `stash-pop` / `stash-drop` | — | Stash variants |
| `log-current` / `log-all` / `log-other` / `log-reflog` | — | Log variants |
