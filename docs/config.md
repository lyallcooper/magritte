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

A repository can override these settings for itself — see *Per-repo settings*.

## Per-repo settings

Drop a `config.toml` (and/or `transient-switches.toml`) in **`.git/magritte/`** to
override settings for one repository. It's a *sparse overlay* on your global
config — set only the keys you want to change; everything else falls through.
The file lives in the repo's git dir, so it's private (never committed) and
shared across the repo's worktrees, and it's re-read live like the global one.

Merge rules — global first, repo on top:

- **Scalars** (theme, font, editor, commit options, …): the repo value wins.
- **`[keymap]`** and **`[transient.*]`**: merged entry by entry — the repo adds
  or overrides individual bindings/suffixes; `"x" = "unbound"` still removes one.
- **`[[command]]`**: concatenated, a repo command replacing a global one of the
  same `id` (so a repo adds commands, or overrides one by id).

Handy for a distinct theme per repo (tell work from personal at a glance),
repo-specific keybindings or commands, or — via `transient-switches.toml` —
per-repo switch defaults (see *Saved switch defaults*).

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
| `show_tags` | `true` / `false` | `false` | Show the nearest tag(s) in the title bar — see *Status sections*. |
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

## Status sections

The status view shows magit-style sections. A `[status]` table picks which
sections appear and in what order:

```toml
[status]
sections = ["untracked", "unstaged", "staged", "stashes", "unpulled", "unpushed", "recent"]
recent_count = 10
```

- `sections` is an **ordered list of ids** — order is display order, presence
  includes a section, omission hides it. Omit `[status]` (or leave `sections`
  empty) for the default order shown above. An unknown id warns at startup.
- Ids:
  - `untracked`, `unstaged`, `staged` — the file sections.
  - `stashes` — the stash list.
  - `unpushed` / `unpulled` — commits ahead of / behind the **upstream**.
  - `unpushed-pushremote` / `unpulled-pushremote` — the same vs the **push
    target** in a triangular workflow; empty (hidden) when the push target is
    the upstream. In the default order, interleaved with the upstream ones.
  - `recent` — the last `recent_count` commits.
  - `ignored` — ignored files. **Off by default**; add it to opt in.
- An empty section is skipped. `recent_count` (default 10) sizes the recent list.
- Commit rows show their ref labels (branches, tags, remotes), colored.
- Like everything else, this is per-repo overridable — drop a `[status]` in
  `.git/magritte/config.toml` to reorder sections for one repository.

**Act at point** in a section: on a commit row, `Return` opens its diff and
`y` (or `Cmd+C`) copies the hash; on a stash row, `Return` shows it, `a`
applies, `A` pops, `x` drops (confirmed), and `y` copies the reference. File
rows stage/unstage/discard as usual.

Set `show_tags = true` to show the nearest tag(s) in the title bar —
`Tag: v1.0 (5)` (commits since) or `Tags: v1.0 (5), v1.1 (2)` (also the next
tag ahead). Off by default.

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
- **Modifiers** are word prefixes on the key: `ctrl-`, `alt-`, `cmd-`. So
  `ctrl-d` is Ctrl-d, and `ctrl-x ctrl-c` is a two-step sequence. A shifted
  letter is just its uppercase (`G`, not `shift-g`).
- **Prefixes are implicit**: any key that begins a sequence becomes a prefix.
  Binding `". c" = "commit"` makes `.` a prefix automatically. Press the prefix
  and a lightweight strip at the bottom shows the keys typed so far with a
  trailing dash (`g-`); each further key extends the sequence until it resolves.
  After `which_key_delay_ms` (default 1000) with no follow-up, the strip expands
  into a which-key list of the available continuations.
- **Unbound keys** report themselves: pressing a key or sequence with no binding
  shows a brief "… is unbound" notice (emacs' echo-area feedback).
- **One unified keymap** — the motions, paging, section jumps, quit, and every
  command id below are ordinary keymap entries you can remap or unbind, in every
  view (status, log, commit, rebase-todo, and the `$` pager). The default
  *secondary* bindings remap the same way:

  | keys | does |
  |------|------|
  | arrows, `ctrl-n` / `ctrl-p` | move the cursor (alongside `j`/`k`) |
  | `space` | page down |
  | `ctrl-d` / `ctrl-u` | half-page down / up |
  | `ctrl-j` / `ctrl-k` / `]` / `[` | previous / next section |
  | `V` | visual selection (alongside `v`) |
  | `ctrl-x ctrl-c` | quit |

  (`ctrl-f` / `ctrl-b` page down / up too — they're the *primary* keys for
  `page-down` / `page-up`, listed below.)
- **Fixed keys** (always act; not rebindable):
  - `Esc` and `Ctrl-g` cancel/abort — a job, a selection, a pending sequence, a
    popup (Emacs keyboard-quit).
  - `Tab` folds/unfolds. (You can bind another key to the `fold` command, but
    `Tab` itself stays fold.)
  - The accelerators `?` (help), `:` (palette), `!` (run a command), `$`
    (command log), and `Cmd+C` (yank) always reach those, regardless of remaps.
  - On a commit or stash row, the act-at-point verbs (`Return`, `y`, and for a
    stash `a` apply / `A` pop / `x` drop) act on the item at point.

  Keys typed inside a transient, picker, or the commit editor are consumed by
  that mode, not the keymap.

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
| `half-page-down` | `ctrl-d` | Scroll down half a page |
| `half-page-up` | `ctrl-u` | Scroll up half a page |
| `page-down` | `ctrl-f` | Scroll down a page |
| `page-up` | `ctrl-b` | Scroll up a page |
| `quit` | `ctrl-x ctrl-c` | Quit Magritte |
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
"-v" = { flag = "--verbose", description = "Show diff in message" }  # switch + label
# place a suffix in an existing section by its title:
"-S" = { flag = "--gpg-sign", group = "Arguments" }
"W" = { command = "commit-amend", group = "Edit HEAD" }
```

- **Actions** — the value is a **command id** (no leading `-`); runs with
  default arguments.
- **Switches** — the value is a **git flag**: a bare string like `"--depth=1"`,
  or a table `{ flag = "…", description = "…" }` to add a label. Keyed dash-first
  (`-d`, toggled with `- d`), like the built-in switches.
- **Section** — the table form takes an optional `group` (a section title). By
  default switches land in **Arguments** and actions in a **Custom** group; name
  a `group` to place them elsewhere (a title that doesn't exist is created).
- **Remove** a built-in suffix with the sentinel `"key" = "unbound"` (like
  `[keymap]`), e.g. `"-n" = "unbound"` drops commit's `--no-verify`. Pair it with
  a new binding at the same key to *replace* a default.

A key already used by a built-in suffix is left alone (the built-in wins). A
section that isn't a real transient, an action naming an unknown command, or a
switch whose key isn't dash-prefixed warns at startup.

### Config-derived switches

Some built-in switches reflect a git config that git itself honors, so they
open already enabled when that config is set: commit `--gpg-sign`
(`commit.gpgSign`), pull `--rebase` (`pull.rebase`, including a per-branch
`branch.<name>.rebase` override), fetch `--prune` (`fetch.prune`), and rebase
`--autosquash` (`rebase.autoSquash`). Toggling such a switch *off* sends the
negation explicitly (e.g. `--no-gpg-sign`), shown highlighted so it's clear
you're overriding the configured default.

### Saved switch defaults

Inside any transient, **`Ctrl-s`** saves the current switch toggles as that
transient's defaults (magit's `transient-save`); reopening it starts from them.
`Ctrl-s` then asks for a **scope** — press **`g`** to save *globally* or **`l`**
to save *for this repo* (anything else, incl. `Esc`, cancels):

- **Global** → `transient-switches.toml` beside the config (e.g. `commit = ["-a", "-s"]`).
- **This repo** → `.git/magritte/transient-switches.toml` in the repo (shared
  across its worktrees, never committed).

When a transient opens, the repo scope wins over the global one **per transient
id**: a repo's `commit = [...]` entry fully defines commit's defaults, while the
global file still supplies the transients the repo doesn't mention. Delete an
entry (or its file) to fall back to the lower scope, then the built-in defaults.

A config-derived switch (above) is only recorded when it differs from the
configured default — as its flag (forced on) or its negation (forced off, e.g.
`commit = ["--no-gpg-sign"]`); leaving it untouched keeps following the config,
so an old or empty saved set never silently disables it.

Both files are re-read live, like the config: editing one by hand takes effect
on the next transient you open, no restart needed.

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
section = "My commands"         # which ? group to list it under when bound
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
- **Shows in the `?` menu** when bound to a key — under the `section` group
  (default "Commands"); a section title that doesn't exist is created. Unbound
  commands stay palette-only.
- **Destructive commands confirm first** — one whose words include `clean`,
  `--hard`, or `--force` prompts before running, like the built-in destructive
  operations.
- An empty `run`, an `id` that shadows a built-in, or a duplicate `id` warns at
  startup. For a *one-off* command, use the `!` prompt instead.
