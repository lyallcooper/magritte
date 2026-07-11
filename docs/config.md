# Configuration

Magritte's global configuration file lives at:

```text
$XDG_CONFIG_HOME/magritte/config.toml
```

Or when `XDG_CONFIG_HOME` is not set:

```text
~/.config/magritte/config.toml
```

Config values are automatically loaded by Magritte when the config file changes.

See [`config.example.toml`](config.example.toml) for a full example.

## Settings screen

Press <kbd>,</kbd> or choose **Magritte > Settings** to access the settings
screen. Here change common options such as themes, fonts, editors, and the keymap
preset. The Settings screen writes to the global configuration file.

```toml
appearance = "dark"
dark_theme = "Dracula"
editor = "zed"
keymap_preset = "evil"

[keymap]
"K" = "branch-delete"
```

For a fully annotated file with every supported section and setting type, see
[`config.example.toml`](config.example.toml). Every entry in it is commented
out, so you can copy it into place and uncomment what you want to change.

Magritte reloads the file after every save. A valid reload shows a short
confirmation in the status bar. If the file is invalid, the status bar shows
the error. At startup, Magritte falls back to its defaults. During a live
reload, it keeps the last valid configuration instead.

An invalid theme, appearance mode, or key binding produces a warning and uses
the default for that value. Fix the value and save again to clear the warning.

## Choose what to customize

- Use [`[keymap]`](#keymap) to bind, remap, or unbind keys. Bindings can be one
  key or a sequence such as <kbd>g r</kbd>.
- Use [`[status]`](#status-sections) to choose which sections appear in the
  status view and how they are ordered.
- Use [`[fetch]`](#auto-fetch) to fetch in the background on a schedule.
- Use [`[[command]]`](#commands) to add a shell command to the command palette
  and keymap.
- Use [`[transient.<id>]`](#transients) to add, move, or remove entries in a
  command menu.
- Use a [per-repository configuration](#per-repo-settings) when one repository
  needs different behavior.

Magritte does not embed a scripting language. Custom commands run through the
shell, and transient configuration changes the existing menu model.

## Per-repo settings

Place a sparse override at `.git/magritte/config.toml` when one repository
needs different settings. Set only the values that differ from your global
configuration:

```toml
# .git/magritte/config.toml
dark_theme = "Nord Dark"

[fetch]
auto = true
interval_minutes = 10
```

The Settings screen's **Open repo config** button creates and opens this file.
Magritte watches it immediately. If you create the `.git/magritte` directory
outside Magritte while the app is running, restart Magritte once so it can
begin watching the directory.

The file is inside the Git directory, so it is not committed. Repositories
with multiple worktrees share the same override.

Magritte loads the global configuration first, then applies the repository
configuration using these rules:

- A top-level value such as `dark_theme`, `font`, or `editor` replaces the
  global value.
- `[keymap]`, `[vim.keymap]`, and `[transient.*]` merge one entry at a time.
  Repository entries add or replace individual bindings.
- `[[command]]` entries combine. A repository command replaces a global
  command with the same `id`.

Saved transient arguments have their own global and repository files. See
[Saved argument defaults](#saved-argument-defaults).

## Settings

All scalar settings are top-level TOML keys. Every setting is optional.

| Key | Values | Default | Meaning |
| --- | --- | --- | --- |
| `appearance` | `"auto"` / `"light"` / `"dark"` | `auto` | Follow the system or force light or dark mode. |
| `light_theme` | theme name | `Selenized Light` | Theme used in light mode. |
| `dark_theme` | theme name | `Selenized Dark` | Theme used in dark mode. |
| `font` | font family | platform monospace | Font for code, diffs, and aligned rows. |
| `ui_font` | font family / `"system-ui"` | value of `font` | Font for menus, headings, and labels. Use `"system-ui"` for the platform font. |
| `font_size` | pixels | system default | Base UI size, clamped to 9--24. The macOS default is 13. |
| `app_icon` | `son-of-man` / `pipe` / `golconda` / `magic` | `son-of-man` | Dock and app-switcher icon on macOS. This does not change the icon in Finder. |
| `editor` | command or app name | OS default | Editor used by Open File (<kbd>Return</kbd>). See [External file editor](#external-file-editor). |
| `commit_in_editor` | `true` / `false` | `false` | Write commit messages in `commit_editor` instead of the in-app editor. |
| `commit_editor` | command | none | Blocking command used as `GIT_EDITOR`, such as `zed --wait`, `code --wait`, or `nvim`. Only used when `commit_in_editor` is true. |
| `commit_title_ruler` | `true` / `false` | `true` | Highlight summary text after column 50. |
| `commit_body_wrap` | `true` / `false` | `true` | Wrap commit bodies at column 72 while preserving indentation. Wrapping pauses in Vim Normal and Visual modes. |
| `commit_vim_mode` | `true` / `false` | `false` | Enable modal editing in the in-app commit editor. See [Vim mode keys](#vim-mode-keys-vimkeymap). |
| `refresh_on_focus` | `true` / `false` | `true` | Refresh the repository when the window regains focus. |
| `show_tags_in_title_bar` | `true` / `false` | `false` | Show the nearest reachable tag in the title bar. |
| `check_for_updates` | `true` / `false` | `true` | Check for new releases and show a quiet notification. |
| `keymap_preset` | `"evil"` / `"vanilla"` | `evil` | Base keymap applied before `[keymap]`. The legacy value `"evil-collection"` is also accepted. |
| `which_key_delay_ms` | milliseconds | `1000` | Time before possible continuations appear for a key prefix or Vim sequence. |
| `published_branches` | list of refs | `["origin/main", "origin/master"]` | Branches considered shared. Magritte warns before rewriting a commit reachable from one of them. Missing refs are ignored. Use `[]` to disable the warning. |

Magritte saves the last 10 edited commit messages for each worktree. This
includes messages discarded from the editor and messages from commits rejected
by a hook. Press <kbd>Alt-p</kbd> in the commit editor to restore the newest message.
Press it again to move through older messages. You can also run **Restore
commit message** from the <kbd>:</kbd> palette.

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

### Settings example

```toml
appearance = "dark"
light_theme = "Selenized Light"
dark_theme = "Dracula"
font = "Berkeley Mono"
editor = "zed"

commit_in_editor = false
commit_title_ruler = true
commit_body_wrap = true
```

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

Actions follow the item at the cursor. <kbd>Return</kbd> opens a commit or stash, while
<kbd>Cmd+C</kbd> copies its hash or reference. On a stash, <kbd>a</kbd> applies it, <kbd>A</kbd> pops it,
and <kbd>x</kbd> in the Evil preset or <kbd>k</kbd> in the Vanilla preset drops it after
confirmation. On a section heading, <kbd>s</kbd> or <kbd>u</kbd> acts on the whole section.
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

Put this table in `.git/magritte/config.toml` to enable fetching only for one
repository.

## Large repositories

If `git status` takes more than about half a second and no filesystem monitor
is configured, Magritte suggests enabling one. Run **Enable filesystem
monitor** from the <kbd>:</kbd> palette to set `core.fsmonitor` and
`core.untrackedCache` for the repository. Git can then avoid repeatedly
scanning the full working tree.

Magritte shows this suggestion once per repository. It does not appear when
`core.fsmonitor` already has a value, including `false`.

## As a git mergetool

Use Magritte's conflict view from terminal-driven merges and rebases by
configuring it as your Git mergetool in your `.gitconfig`:

```ini
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

## Keymap

The default `evil` preset follows evil-collection-magit. Use `vanilla` for
standard Magit and Emacs keys. The Vanilla preset includes <kbd>P</kbd> for push, <kbd>X</kbd>
for reset, <kbd>z</kbd> for stash, <kbd>k</kbd> for discard, and <kbd>n</kbd> or <kbd>p</kbd> for section motion.

Add a `[keymap]` table to change either preset. Each entry maps a key to a
[command id](#command-ids). Use `"unbound"` to remove a binding.

```toml
keymap_preset = "evil"

# TOML requires top-level settings to appear before this table.
[keymap]
"K" = "branch-delete"   # K now deletes a branch
"x" = "unbound"         # remove the default discard binding
"E" = "commit-extend"   # E now extends the current commit
```

- Keys are case-sensitive. For example, `s` and `S` are different bindings.
- Write sequences with spaces between their keys, such as `g r` or
  `ctrl-x ctrl-c`. A sequence can contain any number of keys.
- Write modifiers as `ctrl-`, `alt-`, or `cmd-`. Use an uppercase letter for
  Shift, such as `G` rather than `shift-g`.
- Prefixes do not need separate bindings. If you bind `". c" = "commit"`, `.`
  automatically becomes a prefix. Magritte shows the entered prefix and, after
  `which_key_delay_ms`, its available continuations.
- Unknown command ids produce a warning and are ignored. Pressing an unbound
  key also shows a short notice.

The same keymap controls the status view, logs, commit views, the rebase editor,
and the command log. These secondary bindings are useful when remapping:

| Keys | Action |
| --- | --- |
| arrows, <kbd>ctrl-n</kbd> / <kbd>ctrl-p</kbd> | Move the cursor |
| <kbd>space</kbd> | Page down. On a commit or stash, open it first |
| <kbd>ctrl-d</kbd> / <kbd>ctrl-u</kbd> | Move down or up half a page |
| <kbd>ctrl-f</kbd> / <kbd>ctrl-b</kbd> | Move down or up one page |
| <kbd>ctrl-j</kbd> / <kbd>ctrl-k</kbd> | Move to the next or previous section in Evil |
| <kbd>alt-j</kbd> / <kbd>alt-k</kbd> / <kbd>]</kbd> / <kbd>[</kbd> | Move to the next or previous sibling section in Evil |
| <kbd>n</kbd> / <kbd>p</kbd>, <kbd>alt-n</kbd> / <kbd>alt-p</kbd> | Section motions in Vanilla |
| <kbd>alt-1</kbd> through <kbd>alt-4</kbd> | Set fold level 1 through 4 |
| <kbd>z a</kbd>, <kbd>z o</kbd>, <kbd>z c</kbd>, <kbd>z O</kbd>, <kbd>z C</kbd>, <kbd>z 1</kbd> through <kbd>z 4</kbd>, <kbd>z r</kbd> | Vim-style folds in Evil |
| <kbd>g z</kbd>, <kbd>g n</kbd>, <kbd>g i</kbd>, <kbd>g u</kbd>, <kbd>g s</kbd>, <kbd>g f u</kbd>, <kbd>g f p</kbd>, <kbd>g p u</kbd>, <kbd>g p p</kbd> | Jump to status sections in Evil |
| <kbd>y y</kbd> / <kbd>y s</kbd>, <kbd>y b</kbd>, <kbd>y r</kbd> | Copy the current value, copy the revision, or show refs in Evil |
| <kbd>ctrl-w</kbd> | Copy the current value |
| <kbd>v</kbd> / <kbd>V</kbd> | Start a visual selection in Evil |
| <kbd>ctrl-space</kbd> | Start a visual selection in Vanilla |
| <kbd>alt-&lt;</kbd> / <kbd>alt-&gt;</kbd> | Move to the top or bottom in Vanilla |
| <kbd>h</kbd> | Open help in Vanilla. <kbd>?</kbd> works in either preset |
| <kbd>G</kbd> | Refresh in Vanilla |
| <kbd>&#124;</kbd> | Run a command in Evil |
| <kbd>ctrl-x ctrl-c</kbd> | Quit Magritte |

Some keys are fixed and cannot be remapped:

- <kbd>Esc</kbd> and <kbd>Ctrl-g</kbd> cancel a job, selection, pending sequence, or popup.
- <kbd>?</kbd> opens help.
- An unbound <kbd>:</kbd>, <kbd>Alt-x</kbd>, <kbd>Cmd-P</kbd>, or <kbd>Cmd-K</kbd> opens the command palette.

Transient menus, pickers, and the commit editor handle their own keys while
they are active.

### Vim mode keys (`[vim.keymap]`)

Set `commit_vim_mode = true` to enable Normal, Insert, and Visual modes in the
in-app commit editor. It supports counts, text objects, <kbd>d</kbd>, <kbd>c</kbd>, and <kbd>y</kbd>
operators, surround commands, indentation, repeat, regex search, substitutions,
prompt history, and undo.

Use <kbd>ZZ</kbd> or <kbd>,,</kbd> to commit. Use <kbd>ZQ</kbd> or <kbd>,k</kbd> to cancel. <kbd>gq</kbd> reformats a line,
motion, or visual selection (<kbd>gw</kbd> does the same and keeps the cursor in
place), while <kbd>,q</kbd> reformats the whole message. For full
Vim behavior, use an external editor instead:

```toml
commit_in_editor = true
commit_editor = "nvim"
```

A `[vim.keymap]` table adds your own key sequences for the editor-level
commands: `commit`, `cancel`, `discard` (cancel without the confirmation),
`reflow` (the whole message), and `help`.

```toml
[vim.keymap]
"Q" = "cancel"      # a single key
";w" = "commit"     # press ; and then w
"gz" = "help"
```

- Write Vim sequences as literal characters with no spaces. Modifier chords
  are not supported. Case matters, so `Q` means Shift-Q.
- Custom entries add to the defaults. An exact match replaces the default. For
  example, `"ZZ" = "cancel"` changes the built-in `ZZ` action.
- The first key of a custom sequence shadows its normal Vim action. Mapping
  `"dx"` makes <kbd>d</kbd> wait for <kbd>x</kbd>, so it no longer starts the delete operator.
  Choose prefixes you do not otherwise need.
- A pending sequence appears beside the mode indicator. After the configured
  delay, Magritte shows its possible continuations.
- Repository settings merge these entries one at a time. Unknown actions are
  ignored. Changes apply to an editor that is already open.

### Command ids

Bind any id below from `[keymap]`. `none` in the default-key column means the
command has no direct binding, but you can still find it in a transient menu or
the <kbd>:</kbd> command palette.

The palette shows only commands that apply to the current view and selection.
For example, `jump-to-ignored` appears only when the Ignored section is visible.
Some keys also change with context. On a commit, <kbd>a</kbd> applies that commit. On a
file, it stages the file.

Search accepts common Git terms as well as Magritte's command names. `add`
finds Stage, `restore` finds Discard, `yank` finds Copy, and `history` finds
Log.

| id | default key | command |
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

## Transients

Transient menus are the command menus opened by keys such as <kbd>c</kbd>, <kbd>b</kbd>, and <kbd>p</kbd>.
Use `[transient.<id>]` to add an action or Git option, move an existing entry,
or remove one. Valid ids include `commit`, `branch`, `tag`, `remote`, `stash`,
`reset`, `rebase`, `merge`, `ignore`, `log`, `diff`, `push`, `pull`, and
`fetch`.

```toml
[transient.branch]
"X" = "branch-delete"          # b X deletes a branch

[transient.fetch]
"-d" = "--depth=1"             # add a Git option

[transient.commit]
"A" = "commit-amend"           # add an action to the Custom group
"-v" = { flag = "--verbose", description = "Show diff in message", after = "-s" }
"W" = { command = "user.wip", group = "Create" }
"f" = { after = "c" }          # move Fixup after Commit
"F" = { group = "Edit" }       # move Instant fixup into Edit
```

- An action names a command id and runs with its default arguments.
- A switch contains a Git option such as `"--depth=1"`. Use a table when you
  also want a description or placement. Switch keys begin with a dash, so <kbd>-d</kbd>
  appears under the menu's <kbd>-</kbd> prefix.
- Use `before` or `after` to place an entry beside another key. Use `group` to
  append it to a named group. Magritte creates a missing group at the end.
- A table that contains only `before`, `after`, or `group` moves the built-in
  entry at that key.
- Set an entry to `"unbound"` to remove it. For example,
  `"-n" = "unbound"` removes commit's `--no-verify` option.

Entries apply in file order, with global entries before repository entries.
This lets a later entry refer to one added earlier. A built-in keeps its key if
a custom entry tries to reuse it. Invalid menu ids, command ids, and switch
keys produce a warning.

### Config-derived switches

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

### Saved argument defaults

Press <kbd>Ctrl-s</kbd> in a transient menu to save its current options as the defaults
for the next time you open it. Magritte asks where to save them:

- Press <kbd>g</kbd> for all repositories. Magritte writes
  `transient-arguments.toml` beside your global configuration.
- Press <kbd>l</kbd> for the current repository. Magritte writes
  `.git/magritte/transient-arguments.toml`.
- Press <kbd>Esc</kbd> to cancel.

The file stores Git arguments rather than menu keys, so key remapping does not
affect saved defaults:

```toml
commit = ["--all", "--signoff"]
log = ["-n50", "--grep=fix"]
```

A repository entry replaces the global entry for the same transient. Other
global entries still apply. Delete an entry to fall back to the global or
built-in defaults.

A config-derived switch is saved only when you change it from the Git-config
value. Leaving it untouched continues to follow Git configuration. Both files
reload automatically, and edits apply the next time you open the menu.

## Commands

Use `[[command]]` to make a shell command available in the <kbd>:</kbd> palette and the
keymap.

```toml
[[command]]
id = "user.sync"
title = "Sync"
run = "git pull --rebase && git push"
refresh = true                  # refresh afterward. This is the default

[[command]]
id = "user.wip"
title = "WIP commit"
run = "git commit -a -m WIP"
section = "My commands"         # group in the ? menu when bound
```

`run` executes through `sh -c` in the repository root. Shell operators, pipes,
and redirection work, and the command can run any program. For example,
`run = "make test"` is valid.

### Command placeholders

Magritte shell-quotes each placeholder before inserting it into `run`:

| Placeholder | Value |
| --- | --- |
| `{file}` | File at the cursor |
| `{commit}` | Commit at the cursor in status, log, or a commit view |
| `{branch}` | Current branch |
| `{upstream}` | Current branch's upstream, such as `origin/main` |
| `{push-remote}` | Resolved push remote, such as `origin` |
| `{default-branch}` | Branch selected by the remote's HEAD, such as `main` |
| `{default-remote}` | Remote that owns the default branch, or the push remote as a fallback |

If a required value is unavailable, the command does not run. For example, a
command containing `{file}` reports an error when no file is selected.

Titles can also contain placeholders. A title such as
`"Rebase onto origin/{default-branch}"` displays as **Rebase onto origin/main**.
If a title placeholder cannot be resolved, it remains visible as written.

Bind a custom command by id, for example `"X" = "user.wip"`, or run it by title
from the command palette. Bound commands also appear in the <kbd>?</kbd> menu under
their `section`, which defaults to **Commands**.

Command output appears in a notification. Failures remain until dismissed, and
long output points to the <kbd>$</kbd> command log for the full text. Commands containing
`clean`, `--hard`, `--force`, or `--force-with-lease` ask for confirmation.

An empty `run`, duplicate id, or id that matches a built-in produces a warning.
Use the <kbd>!</kbd> menu for a command you do not need to save.
