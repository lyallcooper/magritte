# Configuration

## File location

Magritte's global configuration file lives at:

```text
$XDG_CONFIG_HOME/magritte/config.toml
```

Or when `XDG_CONFIG_HOME` is not set:

```text
~/.config/magritte/config.toml
```

Magritte automatically reloads config values when the file changes. Omitted
values automatically fall back to their default, so an empty file is still a
valid configuration.

<p class="callout">
    See <a href="/docs/config.example.toml"><code>config.example.toml</code></a> for a
    full example that you can copy and edit as you see fit.
</p>

## Settings screen

Press <kbd>,</kbd> or choose **Magritte > Settings** to access the settings
screen. Here you can change common options such as themes, fonts, editors, and
the keymap preset.

## Per-repo configuration

All configuration values can be overridden by a repo-local configuration file
located at `.git/magritte/config.toml`. E.g.:

```toml
# .git/magritte/config.toml
dark_theme = "Nord Dark"

[fetch]
auto = true
interval_minutes = 10
```

If you create the `.git/magritte` directory outside Magritte while the app is
running, restart Magritte once so it can begin watching the directory.

## Top level settings

All scalar settings are top-level TOML keys. Every setting is optional.

| Key | Values | Default | Meaning |
| --- | --- | --- | --- |
| `appearance` | `"auto"` / `"light"` / `"dark"` | `auto` | Follow the system or force light/dark mode. |
| `light_theme` | theme name | `Selenized Light` | Theme used in light mode. |
| `dark_theme` | theme name | `Selenized Dark` | Theme used in dark mode. |
| `font` | font family | platform monospace | Font for code, diffs, and aligned rows. |
| `ui_font` | font family / `"system-ui"` | value of `font` | Font for menus, headings, and labels. Use `"system-ui"` for the platform UI font. Unset falls back to monospace `font` value. |
| `font_size` | pixels | system default | Base UI size, clamped to 9–24. |
| `app_icon` | `son-of-man` / `pipe` / `golconda` / `magic` | `son-of-man` | Dock and app-switcher icon on macOS. This does not change the icon in Finder. |
| `editor` | command or app name | OS default | Editor used by Open File (<kbd>Return</kbd>). See [External file editor](#external-file-editor). |
| `commit_in_editor` | `true` / `false` | `false` | Write commit messages in `commit_editor` instead of the in-app editor. |
| `commit_editor` | command | none | Blocking command used as `GIT_EDITOR`, such as `zed --wait`, `code --wait`, or `nvim`. Only used when `commit_in_editor` is true. |
| `commit_title_ruler` | `true` / `false` | `true` | Highlight summary text after column 50. |
| `commit_body_wrap` | `true` / `false` | `true` | Wrap commit bodies at column 72. |
| `commit_vim_mode` | `true` / `false` | `false` | Enable vim emulation in the in-app commit editor. See [Vim mode keys](#vim-mode-keys). |
| `auto_refresh` | `true` / `false` | `true` | Watch the repository and refresh settled external changes automatically. |
| `refresh_on_focus` | `true` / `false` | `true` | Refresh the repository when the window regains focus. |
| `show_tags_in_title_bar` | `true` / `false` | `false` | Show the nearest reachable tag in the title bar. |
| `check_for_updates` | `true` / `false` | `true` | Check for new releases and show a quiet notification. |
| `keymap_preset` | `"evil"` / `"vanilla"` | `evil` | Base keymap applied before `[keymap]`. The legacy value `"evil-collection"` is also accepted. |
| `which_key_delay_ms` | milliseconds | `1000` | Time before possible continuations appear for a key prefix or Vim sequence. |
| `published_branches` | list of refs | `["origin/main", "origin/master"]` | Branches considered shared. Magritte warns before rewriting a commit reachable from one of them. Missing refs are ignored. Use `[]` to disable the warning. |

Theme names match the entries under **Light theme** and **Dark theme** in
Settings. Magritte includes GitHub, Solarized, Selenized, Gruvbox, Catppuccin,
Nord, Dracula, and tao variants.

### External file editor

Set `editor` to a command such as `code -w`, `zed`, or `subl -w`. On macOS, you
can also use an application name such as `"Zed"` or `"Visual Studio Code"`.
Leave it empty to use the system default application.

Magritte opens supported editors at the line under the cursor. Terminal
editors are not supported for Open File because Magritte cannot attach them to
your existing terminal session.

## Status sections

Use `[status]` to choose which sections appear and their order:

```toml
[status]
sections = [
  "untracked", "unstaged", "staged", "stashes",
  "unpulled", "unpulled-pushremote", "unpushed", "unpushed-pushremote",
  "recent",
]
recent_count = 10
```

`sections` is an ordered list. The list order becomes the display order, and an
omitted section is hidden. Omit `[status]` or use an empty list to restore the
default order shown above. Unknown section ids produce a warning.

| Section id | Contents |
| --- | --- |
| `untracked` | Untracked files |
| `unstaged` | Changes not yet staged |
| `staged` | Staged changes |
| `stashes` | Saved stashes |
| `unpulled` | Commits available from the upstream |
| `unpushed` | Local commits not on the upstream |
| `unpulled-pushremote` | Commits available from the push target |
| `unpushed-pushremote` | Local commits not on the push target |
| `recent` | The last `recent_count` commits |
| `ignored` | Ignored files. Hidden by default |

Push-target sections are hidden when the push target and upstream are the same.
All empty sections are skipped. Commit rows show labels for branches, tags, and
remotes.

Actions follow the item at the cursor. <kbd>Return</kbd> opens a commit or
stash, while <kbd>Cmd+C</kbd> copies its hash or reference. On a stash,
<kbd>a</kbd> applies it, <kbd>A</kbd> pops it, and <kbd>x</kbd> in the Evil
preset or <kbd>k</kbd> in the Vanilla preset drops it after confirmation. On a
section heading, <kbd>s</kbd> or <kbd>u</kbd> acts on the whole section.
Discarded untracked files are moved to the system Trash.

Set `show_tags_in_title_bar = true` to show the nearest reachable tag. For
example, `v1.0 (5)` means the current commit is five commits after `v1.0`.

## Auto-fetch

Use `[fetch]` to keep incoming and outgoing commit counts current. Background
fetching is off by default.

```toml
[fetch]
auto = true            # default false
interval_minutes = 30  # default 30; minimum 1
```

Magritte runs `git fetch` for the current branch's configured remote, then
refreshes the status view. It skips a fetch while another operation is running.
If a fetch fails, for example while you are offline, Magritte waits until the
next interval and tries again.

This setting can be configured per-repo like all others, as described in
[per-repo configuration](#per-repo-configuration).

## As a git mergetool

You can set git to use Magritte as your mergetool, even when you're working
outside of Magritte. Just put the following in your `.gitconfig`:

```ini
# .gitconfig
[merge]
    tool = magritte
[mergetool "magritte"]
    cmd = magritte --mergetool "$MERGED"
    trustExitCode = true
```

Run `git mergetool` to open each conflicted file in Magritte. Resolve the
conflicts and confirm the finish prompt. Magritte returns success only when the
file has no unresolved conflict markers, which lets Git stage it. Closing the
window before then reports failure for that file.

## Key mappings

Every key resolves to a command, and three tables cover the three places keys
live: `[keymap]` binds keys in the views, `[transient.<id>]` edits the keys
and options inside one command menu, and `[vim.keymap]` adds sequences to the
commit editor's Vim mode. The same [command IDs](#command-id-reference) work
in the first two, and `"unbound"` removes an entry wherever it appears.

### Global keys

Magritte offers two base keybinding presets: `evil` and `vanilla`. The `evil`
preset follows
[evil-collection](https://github.com/emacs-evil/evil-collection)'s magit
bindings, while `vanilla` follows standard Emacs and Magit bindings.

Regardless of the preset chosen, key mappings can be added, changed, or removed
via the `[keymap]` table. Each entry maps a key to a [command
ID](#command-id-reference).

```toml
keymap_preset = "evil"

[keymap]
"K" = "branch-delete"   # K now deletes a branch
"x" = "unbound"         # remove the default discard binding
"E" = "commit-extend"   # E now extends the current commit
```

Keys are case-sensitive. For example, `s` and `S` are different bindings.
Sequences are written with spaces between their keys, such as `g r` or `ctrl-x
ctrl-c`, and modifiers are written as `ctrl-`, `alt-`, or `cmd-`. Prefixes do
not need separate bindings. If you bind `". c" = "commit"`, `.` automatically
becomes a prefix.

Some keys are fixed and cannot be remapped:

- <kbd>Esc</kbd> and <kbd>Ctrl-g</kbd> cancel a job, selection, pending
  sequence, or popup.
- <kbd>?</kbd> opens help.

Transient menus, pickers, and the commit editor handle their own keys while
they are active.

### Transients menu keys

Transient menus are the command menus opened by keys such as <kbd>c</kbd>,
<kbd>b</kbd>, and <kbd>p</kbd>. Use `[transient.<id>]` to add an action or Git
option, move an existing entry, or remove one. The customizable menu ids:

| | | |
| --- | --- | --- |
| `commit` | `branch` | `tag` |
| `remote` | `stash` | `reset` |
| `rebase` | `merge` | `ignore` |
| `log` | `diff` | `push` |
| `pull` | `fetch` | `cherry-pick` |
| `revert` | `bisect` | `patch` |
| `run` | `status-jump` | |

Key mappings take the form of `"key" = "command-id"`. An extended form with
options is also available:

```toml
[transient.<id>]
# Basic form
"<key>" = "<command-id>"

# Extended form
"<key>" = {
    command = "<command-id>",   # Specify one of
    flag = "<--flag>",          # command or flag
    description = "<description>",
    before = "<other-key>",     # Specify at most
    after = "<other-key>",      # one of before,
    group = "<group name>"      # after, or group
}
```

Some concrete examples:

```toml
[transient.branch]
"X" = "branch-delete"        # b X deletes a branch

[transient.fetch]
"-d" = "--depth=1"           # add a Git option

[transient.commit]
"A" = "commit-amend"         # add an action to the Custom group
"-v" = { flag = "--verbose", description = "Show diff in message", after = "-s" }
"W" = { command = "user.wip", group = "Create" }
"f" = { after = "c" }        # move Fixup after Commit
"F" = { group = "Edit" }     # move Instant fixup into Edit
```

- A switch contains a Git option such as `"--depth=1"`. Switch keys begin with a
  dash, so <kbd>-d</kbd> appears under the menu's <kbd>-</kbd> prefix.
- Use `before` or `after` to place an entry beside another key. Use `group` to
  append it to a named group. Magritte creates a missing group at the end.
- Built-in entries can be moved by specifying only `before`, `after`, or `group`
- Set an entry to `"unbound"` to remove it. For example, `"-n" = "unbound"`
  removes commit's `--no-verify` option.

#### Config-derived switches

Some switches start with the value from your Git configuration:

| Menu option | Git configuration |
| --- | --- |
| commit `--gpg-sign` | `commit.gpgSign` |
| pull `--rebase` | `pull.rebase` or `branch.<name>.rebase` |
| fetch `--prune` | `fetch.prune` |
| rebase `--autosquash` | `rebase.autoSquash` |

Turning one of these switches off passes its negated form, such as
`--no-gpg-sign`. Magritte highlights the switch to show that it overrides your
Git configuration.

#### Saved argument defaults

Press <kbd>Ctrl-s</kbd> in a transient menu to save its current options as the
defaults for the next time you open it. Magritte asks where to save them.

Globally saved arguments are stored in `transient-arguments.toml` beside your
global configuration file. Locally saved arguments are stored in
`.git/magritte/transient-arguments.toml`.

The file stores Git arguments rather than menu keys, so key remapping does not
affect saved defaults:

```toml
# transient-arguments.toml
commit = ["--all", "--signoff"]
log = ["-n50", "--grep=fix"]
```

A config-derived switch is saved only when you change it from the Git-config
value. Leaving it untouched continues to follow Git configuration.

### Vim mode keys

Set `commit_vim_mode = true` to enable Normal, Insert, and Visual modes in the
in-app commit editor. It supports most standard vim movements and operations.

For full Vim behavior, you can use an external commit editor:

```toml
commit_in_editor = true
commit_editor = "nvim"
```

A `[vim.keymap]` table adds your own key sequences for the editor-level
commands: `commit`, `cancel`, `discard` (cancel without the confirmation),
`reflow` (the whole message), and `help`.

```toml
[vim.keymap]
"Q" = "cancel"
"; w" = "commit"
"cmd-g z" = "help"
"ctrl-x ctrl-c" = "commit"
```

Write sequences with a space between keystrokes, such as `Q x`, `Q enter`, or
`ctrl-x ctrl-c`. A modifier chord is one keystroke: use `cmd-`, `ctrl-`, `alt-`,
or `shift-`, as in `cmd-enter`. Mappings are case sensitive, e.g. `Q` means
<kbd>Shift-Q</kbd>. Named keys include `enter`, `tab`, and `escape`.

For literal character-only sequences, the compact form is also accepted: `Qx`
is equivalent to `Q x`. Spaces are recommended because they also work when a
sequence contains named keys or modifier chords.

The first key of a custom sequence shadows its normal Vim action. For example,
mapping `"d x"` makes <kbd>d</kbd> wait for <kbd>x</kbd>, so it no longer starts
the delete operator. Choose prefixes you do not otherwise need.

## Custom commands

Use `[[command]]` to make a git or shell command available in the <kbd>:</kbd>
palette and the keymap.

```toml
[[command]]
id = "user.sync"
title = "Sync"
run = "git pull --rebase && git push"

[[command]]
id = "user.wip"
title = "WIP commit"
run = "git commit -m WIP"
refresh = false           # skip the status refresh afterward
confirm = false           # never ask (unset = ask when it looks destructive)
```

`run` executes through `sh -c` in the repository root. Shell operators, pipes,
and redirection work, and the command can run any program. For example,
`run = "make test"` is valid.

### Placeholder templates

The following placeholders can be used in `run` and `title` values:

| Placeholder | Value |
| --- | --- |
| `{file}` | File at cursor |
| `{commit}` | Commit at cursor in status, log; or a commit view |
| `{branch}` | Current branch |
| `{upstream}` | Current branch's upstream, such as `origin/main` |
| `{push-remote}` | Resolved push remote, such as `origin` |
| `{default-branch}` | Branch selected by the remote's HEAD, such as `main` |
| `{default-remote}` | Remote that owns the default branch, or the push remote as a fallback |

If a required value is unavailable, the command does not run. For example, a
command containing `{file}` reports an error when no file is selected.

A title such as `"Rebase onto {default-remote}/{default-branch}"` displays as
"Rebase onto origin/main", for example. If a title placeholder cannot be
resolved, it remains visible as written.

Bind a custom command by id, for example `"X" = "user.wip"`, or run it by title
from the command palette. Bound commands also appear in the <kbd>?</kbd> menu's
Commands group.

Command output appears in a notification. Failures remain until dismissed, and
long output points to the <kbd>$</kbd> command log for the full text. Commands
containing `clean`, `--hard`, `--force`, or `--force-with-lease` ask for
confirmation. Set `confirm = false` on a command you trust to skip that
prompt, or `confirm = true` to always ask -- for a destructive command those
words can't reveal, such as a script.

## Command ID reference

Bind any id below from `[keymap]`, or reference it from a `[transient.<id>]`
action. `none` in the default-key column means the command has no direct
binding, but you can still find it in a transient menu or the <kbd>:</kbd>
command palette.

| ID | default key | command |
| --- | --- | --- |
| `commit` | <kbd>c</kbd> | Commit (transient) |
| `branch` | <kbd>b</kbd> | Branch (transient) |
| `tag` | <kbd>t</kbd> | Tag (transient) |
| `remote` | <kbd>M</kbd> | Remote (transient) |
| `stash` | <kbd>Z</kbd> | Stash (transient) |
| `reset` | <kbd>O</kbd> | Reset (transient) |
| `rebase` | <kbd>r</kbd> | Rebase (transient) |
| `merge` | <kbd>m</kbd> | Merge (transient) |
| `ignore` | <kbd>i</kbd> | Ignore (transient) |
| `log` | <kbd>l</kbd> | Log (transient) |
| `diff` | <kbd>d</kbd> | Diff (transient) |
| `worktree` | <kbd>Z</kbd> (vanilla) / <kbd>%</kbd> | Browse worktrees (visit / add / branch / move / remove) |
| `push` | <kbd>p</kbd> | Push (transient) |
| `pull` | <kbd>F</kbd> | Pull (transient) |
| `fetch` | <kbd>f</kbd> | Fetch (transient) |
| `patch` | <kbd>W</kbd> | Patch (transient: create patches, apply a diff, `git am` a mailbox) |
| `bisect` | <kbd>B</kbd> | Bisect (transient; marks good/bad/skip/reset while a bisect runs) |
| `blame` | none | Blame the file at point |
| `run` | <kbd>!</kbd> | Run a Git or shell command in the repository root or selected file's directory |
| `git-command` | <kbd>&#124;</kbd> (evil) / <kbd>:</kbd>, <kbd>Q</kbd> (vanilla) | Run a command directly (git by default) |
| `stage` | <kbd>s</kbd> | Stage the selection |
| `unstage` | <kbd>u</kbd> | Unstage the selection |
| `stage-all` | <kbd>S</kbd> | Stage all tracked changes (confirms if a file is partially staged) |
| `unstage-all` | <kbd>U</kbd> | Unstage all (confirms if a file is partially staged) |
| `discard` | <kbd>x</kbd> | Discard the selection |
| `untrack` | <kbd>K</kbd> (vanilla) / <kbd>X</kbd> (evil) | Untrack the file at point (`git rm --cached`) |
| `open-file` | <kbd>Return</kbd> | Open file at point in `editor` |
| `open-commit` / `stash-show` | <kbd>Return</kbd> | Show the commit / stash at point |
| `commit-apply` | <kbd>a</kbd> | Apply the changes of the commit at point |
| `commit-cherry-pick` | <kbd>A</kbd> | Cherry-pick transient for the commit at point |
| `revert-here` | <kbd>_</kbd> (evil) / <kbd>V</kbd> (vanilla) | Revert transient for the commit at point |
| `revert-changes` | <kbd>-</kbd> (evil) / <kbd>v</kbd> (vanilla) | Revert the commit at point's changes without committing |
| `reset-here` | <kbd>o</kbd> (evil) / <kbd>x</kbd> (vanilla) | Reset HEAD (mixed) to the commit at point (confirmed) |
| `stash-row-apply` / `stash-row-pop` | <kbd>a</kbd> / <kbd>A</kbd> | Apply / pop the stash at point |
| `stash-row-drop` | <kbd>x</kbd> (evil) / <kbd>k</kbd> (vanilla) | Drop the stash at point (confirmed) |
| `commit-details` | <kbd>=</kbd> | Toggle the details panel in a commit view |
| `fold` | <kbd>Tab</kbd> | Fold / unfold |
| `cycle-folds` | <kbd>shift-tab</kbd> | Cycle every fold through sections, everything, and folded |
| `fold-show` / `fold-hide` / `fold-show-children` / `fold-hide-children` | evil <kbd>z o</kbd> / <kbd>z c</kbd> / <kbd>z O</kbd> / <kbd>z C</kbd> | Explicit fold verbs (vim's `zo`/`zc`/`zO`/`zC`) |
| `resolve-conflicts` | <kbd>e</kbd> | Resolve the conflicted file at point in the smerge-style view |
| `diff-more-context` | <kbd>+</kbd> | More diff context lines |
| `diff-less-context` | <kbd>-</kbd> | Fewer diff context lines |
| `diff-default-context` | <kbd>0</kbd> | Default diff context (3 lines) |
| `refresh` | <kbd>g r</kbd> (evil) / <kbd>g</kbd> (vanilla) | Refresh status |
| `visual` | <kbd>v</kbd> | Toggle visual selection |
| `yank` | <kbd>y y</kbd> (evil) / <kbd>Ctrl-w</kbd>, <kbd>Cmd+C</kbd> | Copy the value at point |
| `copy-buffer-revision` | <kbd>y b</kbd> (evil) | Copy the current view's revision |
| `show-refs` | <kbd>y</kbd> (vanilla) / <kbd>y r</kbd> (evil) | Browse branches, remotes, tags (<kbd>Return</kbd> visits the tip commit; <kbd>b</kbd> checkout, <kbd>x</kbd>/<kbd>k</kbd> delete, <kbd>R</kbd> rename) |
| `settings` | <kbd>,</kbd> | Open Settings |
| `command-log` | <kbd>$</kbd> | Open the command log |
| `close` | <kbd>q</kbd> (and <kbd>Esc</kbd>) | Close the current secondary screen |
| `commit-restore-message` | none | Restore a saved message in the commit editor |
| `fsmonitor-enable` | none | Enable Git's filesystem monitor for the repository |
| `check-updates` | none | Check for updates |
| `about` | none | Show the About panel and version |
| `move-down` | <kbd>j</kbd> | Move cursor down |
| `move-up` | <kbd>k</kbd> | Move cursor up |
| `goto-top` | <kbd>g g</kbd> | Jump to top |
| `goto-bottom` | <kbd>G</kbd> | Jump to bottom |
| `next-section` | <kbd>ctrl-j</kbd> | Next file, commit, or hunk section in the status view |
| `prev-section` | <kbd>ctrl-k</kbd> | Previous section start (status view) |
| `next-sibling-section` | <kbd>g j</kbd> | Next section at the same depth |
| `prev-sibling-section` | <kbd>g k</kbd> | Previous section at the same depth |
| `section-up` | <kbd>^</kbd> | Jump to the parent section |
| `show-level-1` through `show-level-4` | <kbd>1</kbd> through <kbd>4</kbd> | Fold to sections, files, hunks, or everything |
| `status-jump` | vanilla <kbd>j</kbd> | Jump-to-section menu (magit-status-jump) |
| `jump-to-untracked` / `jump-to-unstaged` / `jump-to-staged` / `jump-to-stashes` / `jump-to-ignored` | none | Jump to a file or stash section |
| `jump-to-unpulled-upstream` / `jump-to-unpulled-pushremote` / `jump-to-unpushed-upstream` / `jump-to-unpushed-pushremote` | none | Jump to an incoming or outgoing commit section |
| `half-page-down` | <kbd>ctrl-d</kbd> | Scroll down half a page |
| `half-page-up` | <kbd>ctrl-u</kbd> | Scroll up half a page |
| `page-down` | <kbd>ctrl-f</kbd> | Scroll down a page |
| `page-up` | <kbd>ctrl-b</kbd> | Scroll up a page |
| `help` | vanilla <kbd>h</kbd> | Open the `?` help menu |
| `quit` | <kbd>ctrl-x ctrl-c</kbd> | Quit Magritte |
| `commit-create` | none | Create commit |
| `commit-amend` | none | Amend commit |
| `commit-reword` | none | Reword commit |
| `commit-extend` | none | Extend commit and keep its message |
| `branch-checkout` | none | Check out a branch or revision |
| `branch-create` | none | Create branch |
| `branch-create-checkout` | none | Create and check out a branch |
| `branch-rename` | none | Rename branch |
| `branch-delete` | none | Delete branch |
| `push-pushremote` / `push-upstream` / `push-elsewhere` / `push-other` / `push-tag` / `push-tags` | none | Push variants |
| `pull-pushremote` / `pull-upstream` / `pull-elsewhere` | none | Pull variants |
| `fetch-pushremote` / `fetch-upstream` / `fetch-all` / `fetch-elsewhere` | none | Fetch variants |
| `stash-push` / `stash-index` / `stash-keep-index` / `stash-apply` / `stash-pop` / `stash-drop` / `stash-branch` | none | Stash variants |
| `stash-snapshot` / `stash-snapshot-index` / `stash-snapshot-worktree` | none | Record the state on the stash list without resetting anything |
| `merge-editmsg` / `merge-preview` | none | Edit a merge message or preview a merge |
| `reset-branch` / `file-checkout` | none | Reset a branch or check out a file from a revision |
| `tag-create` / `tag-delete` | none | Tag variants |
| `remote-add` / `remote-rename` / `remote-remove` | none | Remote variants |
| `log-current` / `log-all` / `log-other` / `log-file` / `log-reflog` | none | Log variants |
| `diff-dwim` / `diff-range` / `diff-unstaged` / `diff-staged` / `diff-worktree` / `diff-commit` | none | Diff variants |
| `cherry-pick` / `cherry-pick-range` / `cherry-apply` | none | Cherry-pick or apply commits |
| `revert` / `revert-range` / `revert-no-commit` | none | Revert commits with or without committing |

Secondary views add scoped ids that can be remapped in the same way. These
include `refs-*`, `worktree-*`, `flat-*`, `rebase-todo-*`, `resolve-*`,
`log-open`, and `git-log-toggle-queries`. Open the <kbd>:</kbd> palette in a view to see
every command available there.
