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
live reload your current settings stay in place. Individual bad values (an
unknown theme, appearance mode, or key binding) are reported the same way and
fall back to their default rather than failing the whole file. A successful
live reload confirms with a brief "Settings reloaded from disk"; fixing a
flagged value and saving again clears its warning.

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
| `refresh_on_focus` | `true` / `false` | `true` | Re-run `git status` when the window regains focus, picking up out-of-app changes. |
| `which_key_delay_ms` | milliseconds | `1000` | Delay before the which-key list of continuations appears after a prefix key — see *Keymap*. |

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

- **Keystrokes** are case-sensitive (`s` vs `S`, `f` fetch vs `F` pull). Most are
  a single key; the rest are space-separated **sequences** of any length (`g g`,
  `g r`, or your own `z b c`). An unknown command id is ignored with a startup
  warning rather than silently dropped.
- **Modifiers** use prefixes on the key: `C-` (Ctrl), `M-` (Alt/Option), `D-`
  (Cmd). So `C-d` is Ctrl-d, and `C-x C-c` is a two-step sequence. A shifted
  letter is just its uppercase (`G`, not `S-g`).
- **Prefixes are implicit**: any key that begins a sequence becomes a prefix.
  Binding `". c" = "commit"` makes `.` a prefix automatically. Press the prefix
  and a lightweight strip at the bottom shows the keys typed so far with a
  trailing dash (`g-`); each further key extends the sequence until it resolves.
  After `which_key_delay_ms` (default 1000) with no follow-up, the strip expands
  into a which-key list of the available continuations.
- **Unbound keys** report themselves: pressing a key or sequence with no binding
  shows a brief "… is unbound" notice (emacs' echo-area feedback).
- **One unified keymap** — there are no hardcoded keys. Motions, paging, `Tab`,
  and `C-x C-c` are all ordinary keymap entries you can remap or unbind, in
  every view (status, log, commit, rebase-todo, and the `$` pager). The default
  *secondary* bindings — arrows and `C-n`/`C-p` (move), `Space`/`C-f`/`C-b`
  (page), `C-d`/`C-u` (half-page), `C-j`/`C-k`/`]`/`[` (section), `C-x C-c`
  (quit) — sit alongside the primary keys below and remap the same way.
- **Two genuine exceptions**, both Emacs keyboard-quit conventions: `Esc` and
  `C-g` always cancel/abort (a job, a selection, a pending sequence, a popup),
  and aren't rebindable. Keys typed inside a transient, picker, or the commit
  editor are consumed by that mode, not the keymap.

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
| `git-command` | `!` | Run a command (git by default) |
| `stage` | `s` | Stage the selection |
| `unstage` | `u` | Unstage the selection |
| `stage-all` | `S` | Stage all |
| `unstage-all` | `U` | Unstage all |
| `discard` | `x` | Discard the selection |
| `open-file` | `Return` | Open file at point in `editor` |
| `fold` | `Tab` | Fold / unfold |
| `refresh` | `g r` | Refresh status |
| `visual` | `v` | Toggle visual selection |
| `yank` | `y` | Copy the selection |
| `settings` | `,` | Open Settings |
| `command-log` | `$` | Open the command log |
| `move-down` | `j` | Move cursor down |
| `move-up` | `k` | Move cursor up |
| `goto-top` | `g g` | Jump to top |
| `goto-bottom` | `G` | Jump to bottom |
| `next-section` | `g j` | Next section (status view) |
| `prev-section` | `g k` | Previous section (status view) |
| `half-page-down` | `C-d` | Scroll down half a page |
| `half-page-up` | `C-u` | Scroll up half a page |
| `page-down` | `C-f` | Scroll down a page |
| `page-up` | `C-b` | Scroll up a page |
| `quit` | `C-x C-c` | Quit Magritte |
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

## Transients

A `[transient.<id>]` table adds extra suffixes into a transient menu — magit's
`transient-append-suffix`. The section id is the transient's command id
(`commit`, `branch`, `stash`, `reset`, `rebase`, `merge`, `ignore`, `log`,
`push`, `pull`, `fetch`); each entry maps a suffix key to either an **action**
(a command to run) or a **switch** (a toggleable git flag).

```toml
[transient.branch]
"X" = "branch-delete"          # action: a command id → `b X` deletes a branch

[transient.fetch]
"-d" = "--depth=1"             # switch: a bare `-`-prefixed flag, no label

[transient.commit]
"A" = "commit-amend"           # action
# switch with a description — use a table:
"-v" = { flag = "--verbose", description = "Show diff in message" }
```

- **Actions** — the value is a **command id** (no leading `-`); runs with
  default arguments.
- **Switches** — the value is a **git flag**: a bare string like `"--depth=1"`,
  or a table `{ flag = "…", description = "…" }` to add a label. Keyed dash-first
  (`-d`, toggled with `- d`), like the built-in switches.

Injected suffixes appear in a **Custom** group at the bottom of the menu. A key
already used by a built-in suffix is left alone (the built-in wins). A section
that isn't a real transient, an action naming an unknown command, or a switch
whose key isn't dash-prefixed warns at startup.

## Commands

A `[[command]]` table defines your own command — a shell command the `:` palette
and `[keymap]` can run by `id`.

```toml
[[command]]
id = "user.sync"                # bind in [keymap] / shown in the palette by title
title = "Sync"
run = "git pull --rebase && git push"
refresh = true                  # re-read status afterward (default true)

[[command]]
id = "user.wip"
title = "WIP commit"
run = "git commit -a -m WIP"
```

- **`run` is a shell command**, executed with `sh -c` in the repo root — so
  `&&`, pipes, and redirection all work, and it can run any program, not just
  git (`run = "make test"`).
- **Placeholders** are resolved at run time against the current selection and
  shell-quoted: `{file}` (the file at point), `{commit}` (the commit at point in
  the log), `{branch}` (the current branch). If one can't be resolved — e.g.
  `{file}` with no file selected — the command reports that and doesn't run.
- **Bind it** like any built-in: `[keymap]` entry `"X" = "user.wip"`, or run it
  from the `:` palette by its `title`. Its output shows as a toast (a failure
  stays until dismissed); long output is cut off with a pointer to the `$` log,
  which records the command and its full output.
- **Destructive commands confirm first** — one whose words include `clean`,
  `--hard`, or `--force` prompts before running, like the built-in destructive
  operations.
- An empty `run`, an `id` that shadows a built-in, or a duplicate `id` warns at
  startup. For a *one-off* command, use the `!` prompt instead.
