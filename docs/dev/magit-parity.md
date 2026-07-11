# Magit parity

This reference is for contributors deciding what Magritte should implement and
where it should differ from Magit. It compares transient menus, status
sections, navigation, actions at the cursor, and the Evil and Vanilla keymaps.

The comparison was last audited on 2026-07-07 against the Magit 4.x and
`evil-collection-magit.el` sources. Entries describe observable behavior
from those sources.

## How to read this document

| Mark | Meaning |
| --- | --- |
| ✓ | Same capability and key unless noted |
| ≈ | Available with different behavior or keys |
| ∂ | Partially available. The missing behavior is noted |
| ✗ | Missing |
| N/A | Specific to Emacs or outside Magritte's scope |

Magit hides transient entries above level 4 by default. Rows marked `(level N)`
refer to those hidden entries, so a missing one is less visible than an
unmarked gap. Level 0 entries are also hidden by default.

Keys use Magit's Vanilla defaults. When Magritte's Evil and Vanilla presets
differ, the entry lists both.

## Executive summary

Magritte does not yet cover submodules, clone and init, notes, subtree,
sparse-checkout, bundle, cherry, or WIP modes. The largest gaps in existing
menus are log filtering and formatting options plus the stash worktree
variant.

Bisect, blame, patch creation and application, `git am`, additional push
targets, tag pushes, merge message editing and preview, merge strategies,
branch reset, file checkout, and the main stash variants are available.

Important differences in shared features:

- Revert uses Git's default message with `--no-edit`. Magit defaults to
  `--edit`, which requires an interactive editor that does not fit Magritte's
  background Git process.
- `Space` on a commit or stash opens a full-screen preview. `Esc` returns to the
  same row. Elsewhere, `Space` moves down a page. Magit previews in place and
  supports reverse preview with `DEL`.
- `1`–`4` fold levels are buffer-wide. Magit's digits are section-local, while
  Magritte's behavior matches `M-1` through `M-4`. Global cycling with `S-TAB`
  is available, but local cycle commands are not.
- Magit shows either Unmerged into upstream or Recent commits. Magritte always
  shows both Unpushed and Recent.
- Evil preset adopts evil-collection's non-default `use-z-for-folds` layout:
  `Z` for stash and `z` as a vim-style fold prefix (`za`/`zo`/`zc`/`zO`/`zC`/
  `z1`-`z4`/`zr`).
- Both apps use `a` to apply a stash and `A` to pop one from a stash row.

Magritte also adds features with no direct Magit equivalent: a ranked `:`
command palette, which-key hints, custom commands with placeholders,
live-reloaded repository settings, optional auto-fetch, refresh on focus,
update checks, per-change gutter indicators on diff lines, command timings, clickable title-bar controls, a structured
rebase editor, switches seeded from Git configuration, and repository-scoped
transient defaults.

---

## Transients

### Dispatch (magit `h`/`?` / ours `?` menu + `:` palette)

Magit's dispatch is itself a transient; ours is the `?` help menu plus the
`:` command palette, both driven by the `commands()` registry.

| Key | Command | Status |
|-----|---------|--------|
| `A` | cherry-pick | ✓ |
| `b` | branch | ✓ |
| `B` | bisect | ✓ |
| `c` | commit | ✓ |
| `C` | clone | ✗ |
| `d` | diff | ✓ |
| `D` | diff-refresh | ✗ |
| `e` / `E` | ediff-dwim / ediff | ≈ `e` on a conflicted file opens the in-app resolve view (smerge-style per-conflict ours/theirs/both/base), not ediff |
| `f` / `F` | fetch / pull | ✓ |
| `h` | magit-info (manual) | ≈ ours is the `?` menu itself; no manual |
| `H` | describe-section | N/A |
| `i` | gitignore | ✓ |
| `I` | init | ✗ |
| `j` | status-jump | ✓ (vanilla `j`; evil `g`-sequences) |
| `J` | display-repository-buffer | N/A |
| `l` | log | ✓ |
| `L` | log-refresh | ✗ |
| `m` / `M` | merge / remote | ✓ |
| `o` / `O` | submodule / subtree | ✗ |
| `P` | push | ✓ ours `p` (vanilla `P`) |
| `Q` | git-command | ✓ ours `!` (evil `\|`; vanilla `:`/`Q`) |
| `r` / `t` | rebase / tag | ✓ |
| `T` | notes | ✗ |
| `V` | revert | ✓ ours `_` (vanilla `V`) |
| `w` | am (apply patches) | ✓ (via the `W` patch transient) |
| `W` | patch (format patches) | ✓ |
| `X` | reset | ✓ ours `O` (vanilla `X`) |
| `y` / `Y` | show-refs / cherry | ∂ show-refs (vanilla `y`; evil `yr`); `Y` cherry ✗ |
| `z` | stash | ✓ ours `Z` (vanilla `z`) |
| `Z` | worktree | ✓ vanilla `Z`+`%` (magit); evil `%` (its `Z` is stash, evil-collection's z-for-folds layout); full browse/visit/remove/add/branch/move |
| `!` | run | ✓ |
| `a` | apply change at point | ✗ (cherry-apply exists for commit rows; no diff-section apply) |
| `v` | reverse change at point | ≈ revert-no-commit on commit rows only; no diff-region reverse |
| `k` | discard | ✓ ours `x` (vanilla `k`) |
| `s` / `u` | stage / unstage | ✓ |
| `S` | stage-modified | ✓ (`git add -u`, confirm when something is staged) |
| `U` | unstage-all | ✓ |
| `g` | refresh | ✓ ours `g r` (vanilla `g`) |
| `q` | bury-buffer | ≈ Esc/`q` close sub-screens; quit is palette-only |
| `TAB` / `RET` | section-toggle / visit-thing | ✓ |
| `C-x m` / `C-x i` | describe-mode / magit-info | N/A |

Ours only: settings `,`, command-log `$`, check-updates, visual `v`, yank
`y`, motions.

### Commit (magit `c` / ours `c`)

**Arguments**

| Key | Argument | Status |
|-----|----------|--------|
| `-a` | `--all` | ✓ |
| `-e` | `--allow-empty` | ✓ |
| `-v` | `--verbose` (magit default: on) | ✗ mostly moot — our editor shows the staged diff itself |
| `-n` | `--no-verify` | ✓ |
| `-R` | `--reset-author` | ✓ |
| `-A` | `--author=` (author completion) | ✓ |
| `-D` | `--date=` (level 7) | ≈ ours is a fixed `--date=now` switch; magit reads an arbitrary date |
| `-S` | `--gpg-sign=` (level 5) | ≈ ours is a boolean seeded from `commit.gpgSign` (emits `--no-gpg-sign` when toggled off); magit takes a key id |
| `+s` | `--signoff` (level 6) | ≈ ours on `-s`, visible by default |
| `-C` | `--reuse-message=` | ✗ |

**Actions**

| Key | Command | Status |
|-----|---------|--------|
| `c` | create | ✓ |
| `e` | extend | ✓ |
| `a` | amend | ✓ |
| `w` | reword | ✓ |
| `d` | reshelve (level 0) | ✗ |
| `f` / `s` | fixup / squash | ✓ (target: commit at point, else log-select) |
| `A` / `n` / `W` | alter / augment / revise | ✗ |
| `F` / `S` | instant-fixup / instant-squash | ✓ (create + autosquash; warns if the target is published) |
| `R` | rebase-reword-commit (level 0) | ✓ ours "Reword past", visible by default; drops commit-only switches when firing |
| `x` | autofixup (level 6) | ✗ |
| `X` | absorb-modules (level 6) | ✗ |

Sub-transients `magit-commit-absorb` (needs git-absorb) and
`magit-commit-autofixup`: ✗.

### Branch (magit `b` / ours `b`)

**Arguments**: magit has one, `-r --recurse-submodules` (level 7) — ✗ (ours
has no branch args).

**Actions**

| Key | Command | Status |
|-----|---------|--------|
| — | Configure `<branch>` variables (`d` description, `u` merge/remote, `r` rebase, `p` pushRemote; `R`/`P`/`B` repo defaults) | ∂ description / rebase / pushRemote + repo defaults `pull.rebase`, `remote.pushDefault` via `C`; `u` merge/remote auto-setup ✗ |
| `b` | checkout branch/revision | ✓ |
| `l` | checkout local branch | ✗ |
| `o` | orphan (level 6) | ✗ |
| `c` | branch-and-checkout | ✓ |
| `s` / `S` | spinoff / spinout | ✗ |
| `w` / `W` | worktree-checkout / worktree-branch (level 5) | ✓ in the worktree browser (`b` / `c`) |
| `n` | create | ✓ |
| `C` | configure… | ✓ |
| `m` | rename | ✓ |
| `x` | branch-reset | ≈ not here (key conflict: our evil preset uses `x` for delete); available as the reset transient's `b` |
| `k` | delete | ✓ ours `x` evil / `k` vanilla |
| `h` / `H` | shelve / unshelve (level 7) | ✗ |

`magit-branch-configure`: ∂ — the `C` sub-transient edits description /
rebase / pushRemote (+ the repo defaults); the `a m`/`a r` upstream auto-setup
variables are still missing.

### Push (magit `P` / ours `p`, vanilla `P`)

**Arguments** — exact parity, magit's best-covered transient in ours:

| Key | Argument | Status |
|-----|----------|--------|
| `-f` | `--force-with-lease` | ✓ |
| `-F` | `--force` | ✓ |
| `-h` | `--no-verify` | ✓ |
| `-n` | `--dry-run` | ✓ |
| `-u` | `--set-upstream` (level 5) | ✓ visible by default |
| `-T` | `--tags` | ✓ |
| `-t` | `--follow-tags` | ✓ |

**Actions**

| Key | Command | Status |
|-----|---------|--------|
| `p` / `u` / `e` | pushremote / upstream / elsewhere | ✓ |
| `o` | another branch | ✓ pick the source (current first, or type a rev), then the target `remote/branch` (seeded like elsewhere) |
| `r` | explicit refspecs | ✗ |
| `m` | matching branches | ✗ |
| `T` / `t` | a tag / all tags | ✓ remote resolved like the other pushes (sole remote direct, else a picker) |
| `n` | note ref (level 6) | ✗ |
| `C` | branch-configure | ✗ |

Ours-only refinements to the `p`/`u`/`e` group: when the push-remote and
upstream resolve to the same ref we collapse `p` and `u` into one `p/u` entry
(magit always lists both); and, like magit, the target labels are predictive —
a configured ref is named (`main → origin/main`), an unconfigured target that
will be set names the sole remote it would use (`origin/main, setting it`) or
falls back to `push remote, setting it`.

### Pull (magit `F` / ours `F`)

**Arguments**

| Key | Argument | Status |
|-----|----------|--------|
| `-f` | `--ff-only` | ✗ |
| `-r` | `--rebase` | ≈ ours negatable, seeded from `pull.rebase`, emits `--no-rebase` |
| `-A` | `--autostash` (level 7) | ✗ |
| `-F` | `--force` | ✗ |

**Actions**: `p`/`u`/`e` ✓ (same `p`/`u` collapse as push when the targets
coincide); the optional "Fetch from"/"Fetch" groups (`:if
magit-pull-or-fetch`, off by default upstream) ✗; `r` branch.rebase variable
and `C` configure ✓ (the branch Configure sub-transient).

Magit declares `--ff-only`/`--rebase` incompatible; if we add `--ff-only`,
we need an incompatibility mechanism (see cherry-pick).

### Fetch (magit `f` / ours `f`)

**Arguments**: `-p --prune` ≈ (ours negatable, seeded from `fetch.prune`);
`-t --tags` ✗; `-u --unshallow` (level 7) ✗; `-F --force` ✗.

**Actions**: `p`/`u`/`e`/`a` ✓ (`p`/`u` collapse to one entry when the
push-remote is the upstream's remote); `o` branch ✗; `r` refspec ✗;
`m` submodules ✗ (no submodule support); `C` configure ✗. `magit-fetch-modules`
sub-transient ✗. Ours only: the background `[fetch]` auto-fetch loop.

### Merge (magit `m` / ours `m`)

**Arguments**: `-f --ff-only` ✓; `-n --no-ff` ✓ (the incompatibility is
enforced — toggling one turns the other off); `-s --strategy=` ✓ (magit's
choices plus `ort`); `-X --strategy-option=` (level 5) ✗; `-b`/`-w`
ignore-space (level 5) ✗; `-A -Xdiff-algorithm=` (level 5) ✗; `-S
--gpg-sign=` ✗; `+s --signoff` (level 6) ✗.

**Actions**: `m` plain ✓; `e` edit-message ✓ (mechanically like magit:
`merge --no-commit --no-ff`, then our commit editor opens seeded with git's
prepared MERGE_MSG and committing concludes the merge); `n` no-commit ✓;
`s` squash ✓; `p` preview ≈ (the three-dot `HEAD...<branch>` diff, not
magit's merge-tree buffer); `a` absorb ✗; `d` dissolve ✗. In progress:
magit's `m` "Commit merge" and `a` abort ✓ (ours also seeds the commit
editor from MERGE_MSG on the regular `c c` path during a merge).

### Log (magit `l` / ours `l`)

**Arguments**

| Key | Argument | Status |
|-----|----------|--------|
| `-n` | limit count | ✓ |
| `-A` | `--author=` | ✓ |
| `=s` / `=u` | `--since=` / `--until=` (level 7) | ✓ ours `-s`/`-u` |
| `-F` | `--grep=` | ✓ |
| `-i` / `-I` | ignore-case / invert-grep (level 7) | ✗ |
| `-G` / `-S` | search changes / occurrences | ✓ |
| `-L` | trace line range | ✗ |
| `=m` / `=p` | `--no-merges` / `--first-parent` (level 7) | ✓ ours `-m`/`-p` |
| `-D` | `--simplify-by-decoration` | ✗ |
| `--` | limit to files | ✓ |
| `-f` | `--follow` | ✓ ours defaults on (no prefix-arg mechanism; only sent for single-file logs, where git accepts it) |
| `/s /d /a /f /m` | history simplification (levels 6–7) | ✗ |
| `-o` | commit order | ✓ |
| `-r` | `--reverse` | ✓ |
| `-g -c -d =S -h -p -s` | graph/color/decorate/signature/header/patch/stat | ✗ buffer-formatting toggles with no home in our fixed-format list |

**Actions**: `l` current ✓; `f` file ≈ (magit-log-buffer-file lives in the
file dispatch, keyed to the visited buffer; ours logs the file at point from
the log transient, prompting for a tracked file when there is none); `o`
other ✓; `a` all references ≈ (ours labeled
"all branches" but runs `--all`, magit's `a` semantics); `b` all branches ≈
folded into ours `a`; `h` HEAD (level 0) ✗; `u` related ✗; `L` local
branches ✗; `B`/`T`/`m` (level 7) ✗; `r`/`O`/`H` reflogs ≈ ours has one
HEAD reflog (magit's `H`), no current-branch/other-ref variants, and toggled
args are dropped for reflog; `i`/`w` wiplog N/A (no wip mode); `s` shortlog
✗. Sub-transients `magit-log-refresh` and `magit-shortlog`: ✗ (our `Ctrl-s`
save-defaults covers part of log-refresh's set/save).

### Diff (magit `d` / ours `d`)

**Arguments**: `--` files ✓; `-i --ignore-submodules=` ✓;
`-b`/`-w` whitespace ✓; `-D --irreversible-delete` ✓ (visible by default,
level 5 upstream); `-U` context ✓; `-W --function-context` ✓;
`-A --diff-algorithm=` ✓; `-X --diff-merges=` ✓; `-M`/`-C` rename/copy ≈
(ours plain switches; magit options taking a similarity threshold); `-R` ✓;
`--color-moved`/`--color-moved-ws` (level 5) ✗; `--no-ext-diff` ✓;
`--stat` ✗; `--show-signature` ✗.

**Actions**: `d` dwim ✓ (shown as "smart"); `r` range ✓; `u`/`s`/`w`/`c` ✓;
`p` paths ✗ (partially covered by `--` files); `t` stash-show ✗ (Enter on a
stash row shows it, but no transient action). Sub-transients
`magit-diff-refresh` (re-arg the live buffer, refine-hunk/file-filter/
range-type/flip-revs) and `magit-revision-jump`: ✗ — our args apply to the
*next* diff, not the current buffer.

### Cherry-pick (magit `A` / ours `A`)

**Arguments**: `-m --mainline=` ✓; `=s --strategy=` ✗; `-F --ff` ✓ (default
on, both); `-x` ✓; `-e --edit` ✓; `-S --gpg-sign=` ✗; `+s --signoff`
(level 6) ≈ ours `-s`, visible.

Magit declares `--ff`/`-x` incompatible; **we don't enforce it** — with
`--ff` default-on, toggling `-x` produces an argv git rejects.

**Actions**: `A` pick ✓; `a` apply ✓ (ours strips `--ff` before adding
`--no-commit`); `h` harvest ✗; `m` squash ✗ (merge transient only);
`d`/`n`/`s` donate/spinout/spinoff ✗. In progress: `A`/`s`/`a` ✓ (plus
click-only banner buttons). Ours only: `r` range prompt (magit uses region
selection instead).

### Revert (magit `V` / ours `_` evil, `V` vanilla)

**Arguments**: `-m --mainline=` ✓; `-e --edit` / `-E --no-edit` ✗ **by
design** — revert always uses git's default message (`--no-edit` is forced at
run time regardless of the other args), because an interactive `--edit` would
hang our background-git model; the switches are dropped rather than left as a
footgun. `=s --strategy=` ✗; `-S --gpg-sign=` ✗; `+s` (level 6) ≈ ours `-s`
visible.

**Actions**: revert-commit / revert-no-commit ✓ (evil `_`/`-`, vanilla
`V`/`v`, matching evil-collection); in-progress continue/skip/abort ✓. Ours
only: `r` range.

### Am (magit `w` / ours: in-progress only)

Everything about *starting* an am is ✗ (args `-3`(on)/`-p`/`-c`/`-k`/`-b`/
`-d`/`-t`/`-S`/`+s`; actions maildir/patches/plain patch). In progress:
continue/skip/abort ✓ (`w` prefix + banner). Pairs with the missing patch
transient.

### Rebase (magit `r` / ours `r`)

**Arguments**

| Key | Argument | Status |
|-----|----------|--------|
| `-k` | `--keep-empty` | ✗ |
| `-p` | `--preserve-merges` | N/A (obsolete) |
| `-r` | `--rebase-merges=` (cousins mode value) | ≈ ours `-m`, plain switch, no mode value |
| `-u` | `--update-refs` | ✓ |
| `-s` / `-X` / `=X` / `-f` / `-x` | strategy/options/algorithm/force/exec (level 7) | ✗ |
| `-d` / `-t` | committer-date-is-author-date / ignore-date | ✗ |
| `-a` | `--autosquash` | ✓ (ours negatable, seeded from `rebase.autoSquash`) |
| `-A` | `--autostash` (default on) | ✓ (default on) |
| `-i` | `--interactive` switch | ✗ as a switch; covered by the `i` action |
| `-h` | `--no-verify` | ✗ |
| `-S` / `+s` | gpg-sign / signoff | ✗ |

**Actions**: `p`/`u`/`e` onto targets ✓; `i` interactive ✓ (native todo
editor); `w` reword-a-commit ✓; `s` subset ✗; `m` edit-commit ✗; `k`
remove-commit ✗; `f` autosquash ✗; `t` reshelve-since (level 6) ✗.
In progress: `r`/`s`/`e`/`a` ✓ exactly (same prefix swap as magit, plus
banner keycaps). Ours only: rebase-since-commit at point (`r` on a commit
row / log view).

### Stash (magit `z` / ours `Z` evil, `z` vanilla)

**Arguments**: magit `-u --include-untracked` ✓; `-a --all` (untracked +
ignored) ✓ (both apply to the push variants and the both/worktree snapshots;
the index variants ignore them, as magit's do); `--` file limiting ✓ (from
magit's `z P` push sub-transient; ours lives on the one stash menu and
applies to every push variant — snapshots ignore it).

**Actions**

| Key | Command | Status |
|-----|---------|--------|
| `z` | both | ✓ (prompts for an optional message, like magit) |
| `i` | index only | ✓ (`git stash push --staged`, git ≥ 2.35; magit reverse-applies by hand) |
| `x` | keeping index | ✓ (`--keep-index`; same message prompt) |
| `w` | worktree only | ✗ |
| `P` | push… sub-transient (level 5; `--` file limiting, keep-index) | ≈ folded into the one stash menu: `--` file limiting and `x` keep-index live here |
| `Z` / `I` / `W` | snapshots | ✓ (plumbing port of `magit-stash-save` with `keep`: records on `refs/stash` without touching the index or worktree; honors `-u`/`-a`) |
| `r` | wip-commit | ✗ (no wip mode) |
| `a` / `p` / `k` | apply / pop / drop | ✓ (picker; also stash-row keys) |
| `l` | list | ≈ the Stashes status section; no dedicated buffer |
| `v` | show | ≈ Enter on a stash row; not reachable from the transient |
| `b` | branch from stash | ✓ (pick the stash, then the new branch name) |
| `B` | branch here | ✗ |
| `f` | format-patch | ✗ |

### Tag (magit `t` / ours `t`)

**Arguments**: `-f` ✓; `-a` ✓ (opens the in-app message editor to write the
annotation — or the external editor when `commit_in_editor` is set — like
magit's annotated-tag flow); `-e --edit` ✗; `-s --sign` ✗; `-u --local-user=`
✗.

**Actions**: `t` create ✓; `r` release (version-tag conventions) ✗;
`k` delete ≈ single tag via picker (version-ordered, highest first), no region
multi-delete; `p` prune (local vs remote) ✗.

### Remote (magit `M` / ours `M`)

**Arguments**: `-f` fetch-after-add ✓ (default on, both).

**Actions**: `a`/`r`/`k` add/rename/remove ✓; the variables
(`remote.<name>.url` / `fetch` / `pushurl` / `push` / `tagOpt` /
`followRemoteHEAD`) and the `C` configure sub-transient ✓; `p` prune ✗;
`P` prune-refspecs ✗; `z` unshallow (level 7) ✗; `d u` update-default-branch
✗.

### Reset (magit `X` / ours `O` evil, `X` vanilla)

The six modes `m`/`s`/`h`/`k`/`i`/`w` are at parity (same keys; ours
confirms hard and worktree). `b` branch-reset ✓ (pick a local branch, then
the revision with its upstream offered first; the current branch hard-resets
through the usual confirmation, any other moves via `update-ref` like magit);
`f` file-checkout ✓ (pick a revision, then a file from its tree — the file at
point offered first; `git checkout <rev> -- <file>`). Magit's prefix-arg
set-upstream variant of branch-reset ✗.

### Gitignore (magit `i` / ours `i`)

The visible surface is at full parity (`t`/`s`/`p`/`g`, prompts anchored
with the file at point like magit). The level-7 skip-worktree
(`w`/`W`) and assume-unchanged (`u`/`U`) groups ✗ — only useful with the
matching status sections, which we also lack.

### Status jump (magit `j` / ours `j` vanilla, `g`-sequences evil)

`z`/`n`/`i`/`u`/`s`/`fu`/`fp`/`pu`/`pp` ✓ (same greying/hiding of absent
sections). `t` tracked ✗, `a` assume-unchanged ✗, `w` skip-worktree ✗ —
section gaps, not transient gaps. `j` imenu N/A (the `:` palette covers
fuzzy jumping).

### Run (magit `!` / ours `!`)

Magit's `!` is a transient; ours now is too, with the same keys. The git
prompts are prefilled with `git ` (POSIX-quoted split, **no shell**); the
shell prompts run the raw line via `sh -c` (pipes, `&&`). Evil's `|` (and
vanilla's `:`) stay the direct git prompt, like evil-collection / magit.

| Key | Command | Status |
|-----|---------|--------|
| `!` | git command in repo root | ✓ |
| `p` / `S` | git / shell command in working directory | ✓ — labeled with the file at point's directory, shown only when there is one (the GUI reading of the buffer's directory) |
| `s` | shell command in repo root | ✓ |
| `k` `a` `b` `g` | gitk / git-gui launchers | N/A (Magritte is the GUI) |
| `m` | `git mergetool --gui` | ✗ (meaningful standalone) |

(Shell interpretation for ad-hoc commands now ships via the transient's
`s`/`S` shell variants.)

### Missing transients

**Bisect (`B`)** ∂ — shipped: start (bad/good revision prompts),
mark good/bad/skip, reset, and a banner showing the decisions while active.
Remaining: the optional run-script, `--no-checkout`, `--first-parent`, and
term renames (level 6).

**Blame** ∂ — shipped: the annotated file view (`git blame --porcelain`,
inline commit annotations above each run, opened via `:blame`). Remaining:
chunk motion, re-blame at addition/removal, style cycling.

**Show-refs (`y`)** ∂ — the refs browser is in: branches (with an `↑ahead
↓behind` margin vs their upstream), remote-tracking refs, and tags in one
scrollable list, colored by kind, with visit (Return — `magit-visit-ref`'s
default: show the tip commit, never checkout), checkout (`b`), delete
(`k`/`x`), and rename (`R`, local branches) at point. Vanilla binds `y` (magit); evil
binds `yr` via its `y` yank family (matching evil-collection). Remaining: the
comparison args (`--contains=`, `--merged[=]`, `--no-merged[=]`, `--sort=`) and
ahead/behind vs an arbitrary comparison point (we show it vs the upstream).

**Worktree** ✓ — the worktree browser lists the linked worktrees
(branch/detached, main + current markers, path) and covers magit's full verb
set at point: visit (Return/`g`, opens or focuses that worktree's Magritte
window), remove (`k`/`x`, confirmed, non-force so git refuses a dirty worktree),
add for an existing ref (`b`), new branch + worktree (`c`), and move (`m`) —
each create/move prompts for a directory seeded with a sibling default. Vanilla
binds `Z`+`%` (magit's pair); evil binds `%` — its `Z` is stash, matching
evil-collection's `use-z-for-folds` layout. Deviation: visit opens a window
rather than a status buffer (the GUI-native equivalent).

**Patch (`W`)** ∂ — shipped: create (`format-patch` a range), apply a diff
to the worktree, and apply a mailbox as commits (`git am`, pausing into the
sequence banner on conflict). Remaining: the mail-args/reroll/cover-letter
sub-transient, apply's `--index`/`--cached`/`--3way` switches, save-diff-as-
patch, request-pull.

**Clone (`C`) / Init (`I`)** ✗ — both need a "no repo yet" app state (URL/
directory prompts, progress, open the result); the git side is simple.

**Submodule (`o`)** ✗ — full lifecycle (add/register/populate/update/sync/
unpopulate/remove/list/fetch). Commands are straightforward but only useful
with submodule awareness in the status view.

**Notes (`T`)** ✗ — edit/remove/merge/prune `git notes`; needs the
git-variable widget for its ref variables. Low demand.

**Subtree (`O`)** ✗ — `git subtree` import/export wrappers. Niche.

**Sparse-checkout (`>`)** ✗ — enable/disable/set/add/reapply.

**Bundle** ✗ — create/verify/list bundle files. Very niche.

**Cherry (`Y`)** ✗ — `git cherry -v` listing (commits not equivalent to
upstream); a variant of our log screen.

**Ediff (`E`/`e`)** ≈ — the standalone analog is the in-app resolve view:
`e` on a conflicted file lists its conflicts with ours/base/theirs blocks
tinted; `o`/`t`/`b`/`B` keep ours/theirs/both/base per conflict (smerge's
semantics, not ediff's three-window session), `n`/`p` move between conflicts,
`u` undoes, and resolving the last conflict offers to stage the file.
**Mergetool** ∂ — Magritte can serve as `git mergetool` through
`magritte --mergetool "$MERGED"`. It does not provide Magit's `! m` action for
launching `git mergetool --gui` from inside the app.

**File-dispatch** mostly N/A (buffer-centric entry point, blob navigation);
`K` untrack is done (act-at-point on a file row); the rest (rename, file log,
blame-this-file) are ✗ and would likewise live as act-at-point file commands.

**Margin-settings** N/A (Emacs window margins). **Insert-trailer** ∂ —
trailer insertion (Acked-by/Reviewed-by/Co-authored-by…) would be a natural
commit-editor helper; changelog insertion N/A.

Two build-once dependencies recur across these: a **git-variable infix
widget** (now built — `Suffix::Variable`, used by branch-configure and
remote-configure; notes can reuse it) and a **no-repo app state**
(clone, init).

### Non-transient magit commands

| magit key | Command | Magritte | Status |
|---|---|---|---|
| `g` | refresh | `g r` evil, `g` vanilla | ✓ |
| `G` | refresh-all | vanilla `G` → plain refresh | ≈ deliberate — single-buffer app |
| `q` | bury buffer | Esc/`q` close sub-screens | ≈ no buffer stack |
| `$` | process buffer | `$` command log | ✓ (see Other buffers) |
| `%` / `Z` | worktree | worktree browser: vanilla `Z`+`%`, evil `%` | ✓ (see Worktree) |
| `Q` / `:` | git-command | vanilla `:`/`Q`, evil `\|` | ✓ |
| `s`/`S`/`u`/`U` | stage/stage-modified/unstage/unstage-all | same keys | ✓ (`S` ≈, see act-at-point) |
| `k` | delete-thing | evil `x` / vanilla `k` discard | ✓ stash-row drop follows the preset too (evil `x` / vanilla `k`) |
| `K` | file-untrack | untrack the file at point (`git rm --cached`): vanilla `K`, evil `X` (evil-collection's remap) | ✓ |
| `R` | file-rename | — | ✗ |
| `x` | reset-quickly (reset to rev at point) | vanilla `x` / evil `o` (evil-collection's remap) resets HEAD (mixed) to the commit at point in the log or a status commit section, confirmed | ✓ |
| `Y` | cherry | — | ✗ |
| `I` | init | — | ✗ |
| `y` | show-refs | `y r` via the `y` yank family (evil keeps `y` a prefix) | ∂ |
| `RET` | visit-thing | Enter opens file/commit/stash | ✓ (≈ semantics, see act-at-point) |
| `C-RET` | visit in other window | — | N/A |
| `SPC` / `DEL` | show-or-scroll (peek commit at point) | Space previews the commit/stash at point (Esc returns); DEL pages up | ≈ overlay preview, not other-window; no reverse-preview |
| `+` / `-` / `0` | more / less / default diff context | more / less / default diff context (status view) | ✓ status view; diff/commit views ✗ |
| `M-TAB` | dired-jump | — | N/A |
| `M-<tab>` | cycle diff sections | `1`–`4` levels | ≈ level-set, not cycle |
| `h`/`?` | dispatch | `?` menu + `h` (vanilla) | ✓ |
| `H` / `J` | describe-section / display-repo-buffer | — | N/A |
| `C-c C-e` | edit-thing | Enter opens in external editor | ≈ |
| `C-c C-o` | browse-thing (open on forge) | — | ✗ feasible: open commit/file on the remote web UI |
| `C-w` | copy-section-value | `y`/`ctrl-w`/`cmd-c` | ✓ |
| `M-w` | copy-buffer-revision | — | ✗ |

---

## Status buffer

### Headers

Magit renders headers as buffer lines; Magritte puts the equivalents in the
native title bar.

| Header | magit | Magritte | Status |
|---|---|---|---|
| `Head:` | hash + branch + commit subject | branch chip (click → branch transient); no HEAD hash/subject anywhere | ≈ |
| `Merge:`/`Rebase:` | upstream + its subject; label per `pull.rebase`; warns on invalid upstream | upstream chunk + clickable ↑/↓ counts (click → push/pull) | ≈ no subject, no merge-vs-rebase label, no invalid-upstream warning; adds clickable counts |
| `Push:` | push target + subject; warns if unset | shown only when distinct from upstream | ≈ hidden rather than warned |
| `Tag:`/`Tags:` | current + next tag with distances, on by default | same format, **off by default** (`show_tags_in_title_bar`) | ≈ |
| Error header | `GitError!` line + "[Type $ for details]" | status-bar toast + `$` log | ≈ |
| Diff-filter header | `Filter!` when a diff filter is active | no persistent status diff filter exists | N/A |

Magritte adds a dirty-worktree dot and busy spinner (no magit analog).

### Sections

| magit sections-hook entry | Magritte | Status |
|---|---|---|
| merge-log (foldable log of the merge range) | banner heading only | ∂ |
| rebase-sequence (todo as navigable commit sections) | banner: heading + steps (cap 8) + action keycaps; steps aren't actionable rows | ∂ |
| am-sequence / sequencer-sequence | banner (click-only actions) | ∂ |
| bisect-output / -rest / -log | — | ✗ |
| untracked files | `Untracked` — but expanded (magit collapses the heading) and uncapped (magit caps at 100 with "N not listed"). Now honors `status.showUntrackedFiles` (no hardcoded `--untracked-files`) | ≈ |
| unstaged / staged changes | same model (files collapsed, lazy diffs) | ✓ |
| stashes | present but **expanded** (magit hides by default) | ∂ |
| unpushed-to-pushremote | same suppression rule (only when distinct from upstream) | ✓ |
| unpushed-to-upstream **or** recent | magit shows exactly one: "Recent commits" when not ahead of upstream, else "Unmerged into upstream"; we always show both `Unpushed` and `Recent` | ∂ also heading wording differs |
| unpulled pair | ✓ | ✓ |
| child counts | `(N)` ✓; unpushed/unpulled listings are capped at 256/side so a pathological divergence can't fetch every commit. No `(N+)` marker on the count, but the title bar shows the true ahead/behind | ≈ |
| file-list caps | none | ✗ |
| optional sections (tracked, skip-worktree, assume-unchanged, cherries, worktrees, modules, ignored) | only `ignored` exists (opt-in) | ∂ 1 of ~8 |

**Ref decorations** on commit rows (and the log view, and the commit-detail
`Refs:` line) follow magit's faces: local branches blue, remote-tracking refs
green, tags yellow, the current branch bold. Like magit we drop remote
`*/HEAD` pointers and fold the current branch with its upstream into one
`origin/main` entry when both decorate a commit. ✓

### Collapse defaults & configurability

- Magit starts most log-ish sections and stashes hidden and the untracked
  heading collapsed; Magritte starts everything expanded. Deliberate
  divergence, but worth an explicit stance.
- Visibility persistence: magit caches per-session; Magritte persists
  section folds **on disk** per checkout (stronger), while file/hunk folds
  are session-only (weaker).
- Section set/order: magit's hook takes arbitrary elisp; our
  `[status].sections` reorders/omits the 10 known ids. Custom *sections*
  aren't extensible (custom commands are).
- `recent_count` ✓ (both default 10).

## Section motions & folding

| Key | magit | Magritte | Status |
|---|---|---|---|
| `n`/`p` | next/prev visible section start | ✓ (`ctrl-j`/`ctrl-k` evil, `n`/`p` vanilla) | ✓ |
| `M-n`/`M-p` | siblings | ✓ (`g j`/`g k`, `]`/`[` evil; `alt-n`/`alt-p` vanilla) | ✓ |
| `^` | parent | ✓ | ✓ |
| `TAB` | toggle | ✓ (hunk-aware; expanding a file lazy-loads) | ✓ |
| `C-c TAB`/`C-<tab>` | 4-state section cycle | — | ✗ |
| `M-<tab>` | cycle diff sections | — | ✗ |
| `S-TAB` | global cycle | `shift-tab` | ✓ |
| `1`–`4` | show-level of the **surrounding** section (point-local, region-aware) | buffer-wide | ∂ ours implement magit's `M-1..4`; no local variant, and they clear the visual selection instead of honoring it |
| `M-1`–`M-4` | show-level **all** | `alt-1..4` = same buffer-wide command | ✓ |
| `SPC`/`DEL` | peek/scroll the commit at point in the other window | Space previews the commit/stash at point (overlay, Esc returns); DEL pages up | ∂ overlay preview; DEL page-only |
| point restoration | goto-successor | AnchorIdent rebuild anchoring | ✓ |
| visibility indicators | fringe/`…` | chevrons | ✓ |

The remaining folding gap is local cycling with `C-TAB` and `M-TAB`. `S-TAB`
global cycling and `Space` preview are available.

## Act-at-point

### Verb matrix

| Verb × target | magit | Magritte | Status |
|---|---|---|---|
| `s` untracked | `git add` (prefix → `--intent-to-add`) | `git add` | ≈ no intent-to-add |
| `s` unstaged file/hunk/region | add / apply --cached | same, line-granular | ✓ |
| `s` staged/committed | loud user-error | silent no-op | ≈ |
| `s` on section headers | stages the section (with confirm for stage-modified) | `s` on Untracked stages all untracked; `s` on Unstaged = stage-modified; `u` on Staged = unstage-all | ✓ |
| `u` staged file/hunk/region | reverse-apply / reset | same, rename-aware | ✓ |
| `u` unstaged file | drops intent-to-add entries | no-op | ✗ (no ita support) |
| `u` committed change | **reverses it in the index** (`magit-unstage-committed` t) — the "extract a change from HEAD" flow | `u` in the commit view reverse-stages the file/hunk at point into the index | ✓ (commit view; status view has no committed changes) |
| `k`/`x` discard untracked | delete → **system trash**, confirm | system trash, confirm (git clean fallback when unavailable) | ✓ |
| `k`/`x` discard unstaged/staged | confirm; entry-dispatched | mirrors magit exactly (incl. partial-discard `.rej` reporting) | ✓ |
| `k` conflicted hunk | smerge-keep-current + per-hunk smerge keys | staging verbs refused; `e` opens the per-conflict resolve view (o/t/b/B), take-ours/theirs whole-file via right-click | ≈ |
| `v` reverse at point | reverse staged/committed hunk/file/region in worktree | commit/diff view: reverse the file / hunk / region at point in the worktree (evil `-` / vanilla `v`, per preset). Status-view unstaged/staged reverse still ✗ (covered by discard/unstage) | ✓ committed (incl. region) |
| `a` apply at point | apply committed hunk/file to worktree; untracked file → am; prefix = 3-way | commit/diff view: apply the file / hunk / region at point to the worktree (`a`); no `am`/3-way | ∂ committed (incl. region) done; am/3-way ✗ |

### Row types

- **File/hunk rows**: magit `RET` visits the *blob* for the diff side at
  point (index/HEAD blob for staged), `C-RET` the worktree file. Ours opens
  the worktree file in the external editor at the diff's line — a deliberate
  ≈ (right file and line, never a historical blob). `C` commit-add-log,
  `K` untrack, `R` rename: ✗.
- **Commit rows**: show/apply/pick/revert ✓ at parity; ours adds `r`
  (rebase-since) and copy-hash. A *region* of commit rows shows the range
  diff in magit; ours only copies hashes (∂).
- **Stash rows**: `a` apply, `A` pop, `RET` show — **matches magit** (its
  section map remaps `a`→apply, `A`→pop; a suspected reversal was
  disproven against the source). Drop follows the preset like discard:
  evil `x`, vanilla `k` (magit's key).
- **Stashes header**: magit `RET` opens a stash-list buffer, `k` clears all
  stashes (confirmed). Ours: fold only; no list buffer, no clear anywhere. ✗
- **Branch/tag/remote rows**: live in magit's refs buffer — ours is the
  `y` refs browser (checkout/delete at point). Worktree rows ✗.

### Region model

Magit scopes a region to lines-within-one-hunk, else sibling hunks, else
files — and errors loudly on invalid combinations. Ours resolves per file at
the coarsest selected granularity, silently skips files whose section
doesn't match the verb, and batches the rest; a conflicted file anywhere
refuses the whole action (stricter than magit). Net: ours is more permissive
where magit is more predictable.

## Keymaps

### Vanilla preset

Covered above per area; the residual key-level notes:

- Bound and matching: all shared transient prefixes, `g` refresh, `Q`/`:`
  git-command, `$`, `?`/`h`, `j` jump, `s`/`u`/`U`, `k` discard/stash-drop,
  `n`/`p`/`M-n`/`M-p`/`^`, `TAB`, `C-w` copy.
- ≈: `G` is a refresh alias (magit: refresh-all — deliberate, single
  buffer); `S` includes untracked; `1`–`4` semantics; `DEL` pages (no
  reverse-preview); `RET` worktree-file semantics.
- ✗ keys with no binding at all:
  `M-w`, `C-c C-o` browse, and every missing-feature prefix (`B` `C` `D`
  `H` `I` `L` `o` `O` `T` `W` `y` `Y` `Z` `%` `>`; `w` works only as the
  in-progress am prefix).
- Magritte-only: `,` settings, paging cluster (`C-v`/`M-v`/`Space`/
  `Backspace`), `M-<`/`M->`, `C-SPC` visual, `C-x C-c`, palette
  (`Cmd-P`/`Cmd-K`/`M-x`), `Cmd-C`.

### Evil preset vs evil-collection

- Matching: `C-j`/`C-k`, `gj`/`gk`/`]`/`[`, `gr`, `x` discard, `p` push,
  `O` reset, `-`/`_` revert pair, `|` git-command, `j`/`k`/`gg`/`G`,
  `C-d`/`C-f`/`C-b`, `v`/`V` visual, `g`-jump family (`gz gn gu gs gfu gfp
  gpu gpp`; ours adds `g i`).
- ✓: **`z` folds / `Z` stash** — evil adopts evil-collection's non-default
  `use-z-for-folds` layout: `Z` stash, and `z` a fold prefix (`za` toggle,
  `zo`/`zc` show/hide, `zO`/`zC` show/hide children, `z1`-`z4`/`zr` levels).
  `$` — evil-collection moves the process buffer to `` ` `` by default; we
  keep `$`. We also keep `C-w` as copy (evil-collection frees it for
  window-map) — deliberate; the `y` yank family is our main copy.
  `C-u` scrolls unconditionally (evil gates it behind `want-C-u-scroll`).
  `:` opens our palette (evil-ex analog). `gh` section-up isn't bound
  (magit's `^` works).
- ✓: the `y` yank family — `y` is a prefix with `yy`/`ys` copy (we don't split
  whole-line from section-value), `yb` copy-buffer-revision, and `yr` show-refs,
  matching evil-collection. `Cmd+C` copies without the prefix.
- ✗: `gR` refresh-all, `o` reset-quickly, `X` untrack, `'`/`"`
  submodule/subtree, `=` less-context, `~` default-context, `S-SPC` preview,
  `/ n N` search.
- Rebase todo editor vs evil git-rebase-mode: `p r e s f d` ✓ (+ our `w`
  reword alias); `x` ≈ collision — evil's `x` is **exec** (which we lack
  entirely), ours is a drop alias; `M-j`/`M-k` move vs our `J`/`K`;
  `ZZ`/`ZQ` vs our `Enter`/`Esc`; `u` undo ✗.

## Other buffers & screens

- **Log**: browse + act ✓ (open, cherry-pick, revert, rebase-since, reset-to
  here via `x`, copy). `+`/`-` double/halve the commit limit ✓ (magit's `=`
  set-to-value ✗). Still missing: `j` move-to-revision, `L` refresh/margins.
  `SPC` preview ✓ (from the
  status commit rows). Log-select: same capability, different chord
  (`Cmd-Enter` confirms; `Enter` inspects).
- **Revision/commit buffer**: ours shows message + flat diff, a `=` details
  toggle, and the apply engine at point (`a` apply-to-worktree, `v`/`-`
  reverse, `u` reverse-in-index); magit's adds notes and a diffstat section
  (`--stat` default), per-file visiting, `j` revision-jump, refine-hunk. ∂
  thinner.
- **Diff buffer**: entry points ✓; the apply engine at point works here too
  (`a`/`v`/`u`, same as the commit view). Still no context keys and no `D`
  refresh transient (refine/file-filter/range-type/flip-revs). `C-c C-d`
  diff-while-committing ≈ our commit editor embeds the staged diff by default.
- **Refs buffer**: ∂ — branches/remotes/tags with checkout/delete/rename at
  point and an ahead/behind margin; no comparison args yet (see Show-refs).
- **Process buffer**: ≈ — magit has one collapsible section per subprocess
  and `k` kill-at-point; ours is a flat pager, but adds per-command timings
  with slow-command coloring and the hidden-queries toggle. Kill is global
  (`Esc`/`C-g` cancels the running job) rather than at-point.
- **Blame / bisect**: ∂ — both shipped (a blame pager and a bisect banner); see Missing transients for the remaining depth.
- **Rebase todo**: native structured editor (keycap actions, reorder,
  confirm-on-dirty-cancel) vs git-rebase-mode buffer; todo kinds beyond
  pick/reword/edit/squash/fixup/drop (exec, break, label, reset, merge) ✗;
  no undo; no show-commit-at-point.

## Safety & confirmations

| Operation | magit default | Magritte | Status |
|---|---|---|---|
| single stage/unstage | never confirms | never confirms | ✓ |
| `S` with staged present / `U` with unstaged present | confirms (blurs the staged/unstaged split) | confirms | ✓ |
| discard (any granularity) | confirms; deletions go to **trash** | confirms; deletions go to trash (fallback: delete) | ✓ |
| reverse `v` | confirms | no verb | ✗ |
| stash drop / clear | prompt / confirm | drop at point confirms (evil `x` / vanilla `k`); picker drop relies on the pick; no clear | ≈ |
| hard / worktree-only reset | rev prompt only, no y/n | rev pick + y/n confirm | ✓ stricter |
| amend/reword/extend published | confirms vs `magit-published-branches` (default `origin/master`) | confirms vs `published_branches` (default adds `origin/main`) | ✓ |
| rebase across published | confirms | confirms | ✓ |
| commit with nothing staged | shows diff + y/n | y/n then `--all` editor (no diff preview at prompt time) | ≈ |
| abort in-progress sequence | confirms | confirms | ✓ |
| set-upstream-and-push | the one default no-confirm | no confirm either | ✓ |
| delete unmerged branch | confirms, then `-D` | plain `-d` refuses; no force path | ≈ safe but can't force-delete |
| destructive `[[command]]` | no analog | confirms | Magritte-only |

## Recommendations

The highest-value parity work is in features users encounter during existing
workflows:

- Add reverse preview with `DEL` and consider an in-place preview instead of a
  full-screen overlay.
- Make apply and reverse behavior depend on the diff type, as Magit does for
  staged and unstaged changes.
- Support `+`, `-`, and `0` context changes in commit and standalone diff views.
- Add Magit's remaining log filters and direct limit entry.
- Add the stash worktree variant.
- Add section-local fold levels and local cycle commands.
- Reserve rebase-todo `x` for `exec`, or remove the conflicting alias before
  adding `exec` support.
- Decide whether Evil should keep Magritte's `z` fold prefix or return to
  evil-collection's default `z` stash binding.

Larger missing areas should start with clone and init because they establish the
no-repository application state needed by both. Submodules are the next broad
Git workflow. Notes, cherry, subtree, sparse checkout, bundle, and WIP modes are
more specialized.

The conflict view would also benefit from word-level conflict highlighting and
an in-app `! m` action, even though Magritte already works as an external Git
mergetool.

The following differences fit the standalone app and should remain intentional:

- Use title-bar controls instead of buffer header lines.
- Open worktree files in the configured editor instead of creating blob
  buffers.
- Keep `G` as a refresh alias and retain the single-buffer navigation model.
- Keep the `$` log as a flat pager with timings and a query toggle.
- Persist expanded section state on disk.
- Show both Unpushed and Recent sections.
- Collapse duplicate push, pull, and fetch targets when the push remote and
  upstream resolve to the same ref.
- Use Git's default revert message because an interactive `--edit` cannot run
  in the background Git process.
- Keep range batching, strict conflicted-file checks, additional reset
  confirmations, and the broader `published_branches` default.
