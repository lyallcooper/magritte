# Magit parity

A feature-by-feature comparison of Magritte against Magit, covering every
transient (every flag and action), the status buffer, section motions,
act-at-point behavior, and both keymaps. It exists so feature work can be
chosen deliberately: what to build, what to deliberately diverge on, and what
doesn't apply outside Emacs.

Audited against the Magit 4.x sources in the local `.reference/magit/lisp/`
checkout (plus `evil-collection-magit.el`); Magritte as of this document's
last update. Behavioral claims were verified against both sources, not just
listed from memory.

**Status legend**

| Mark | Meaning |
|---|---|
| ✓ | parity — same capability (same key unless noted) |
| ≈ | differs — present, but the key or behavior deviates (noted inline) |
| ∂ | partial — a subset exists; the missing part is noted |
| ✗ | missing |
| N/A | Emacs-specific or out of scope by design (ediff, dired, imenu, …) |

Magit hides transient suffixes above level 4 by default; rows marked
`(level N)` are those hidden-by-default suffixes, so a ✗ there is a smaller
gap than an unmarked one. `(level 0)` suffixes are also hidden by default.

Keys are written as magit's vanilla defaults; where Magritte's evil and
vanilla presets differ, both are given.

## Executive summary

**Whole areas missing:** bisect, blame, a refs browser (`y` show-refs),
worktree commands, submodules, patch create/apply (and starting a `git am` —
we can only drive one already in progress), clone/init, notes, subtree,
sparse-checkout, bundle, cherry, wip. Within existing transients, the largest
gaps are commit's whole fixup/squash column, magit's "push something other
than the current branch" group, log's limiting/formatting flags, merge
strategies, and stash's index/worktree/snapshot variants.

**Notable behavior differences in shared features:**

- Our revert defaults to `--no-edit`; magit defaults to `--edit`.
- `SPC` pages; magit's `SPC` *previews* the commit/stash at point without
  leaving the status buffer (a heavily used flow we lack entirely).
- `1`–`4` fold levels are buffer-wide; magit's digits are section-local
  (ours match magit's `M-1`..`M-4` instead). We also have no cycle commands
  (`S-TAB`, `C-TAB`).
- Magit shows *either* "Unmerged into upstream" *or* "Recent commits"; we
  always show both Unpushed and Recent.
- Our unpushed/unpulled/recent listings are fetched without a limit (no
  magit-style `(N+)` cap); a pathological divergence lists every commit.
- Core hardcodes `--untracked-files=normal`, overriding a repo's
  `status.showUntrackedFiles` config.
- Evil preset uses `Z` for stash; evil-collection's default keeps magit's
  `z` (its `Z` layout is the non-default `use-z-for-folds` option).
- One suspected difference was disproven: on stash rows both magit and
  Magritte bind `a` = apply, `A` = pop (magit via section-map remaps).

**Magritte-only surface** (no magit equivalent): the `:` command palette with
frecency ranking, which-key, `[[command]]` user commands with placeholders,
per-repo config overlay with live reload, opt-in auto-fetch and
refresh-on-focus, update checks, per-command timings in the `$` log,
clickable title-bar chrome (branch chip, ahead/behind), a native structured
rebase-todo editor, config-seeded negatable switches (e.g. `--gpg-sign` from
`commit.gpgSign`), and `Ctrl-s` transient-save with a per-repo scope.

---

## Transients

### Dispatch (magit `h`/`?` / ours `?` menu + `:` palette)

Magit's dispatch is itself a transient; ours is the `?` help menu plus the
`:` command palette, both driven by the `commands()` registry.

| Key | Command | Status |
|-----|---------|--------|
| `A` | cherry-pick | ✓ |
| `b` | branch | ✓ |
| `B` | bisect | ✗ |
| `c` | commit | ✓ |
| `C` | clone | ✗ |
| `d` | diff | ✓ |
| `D` | diff-refresh | ✗ |
| `e` / `E` | ediff-dwim / ediff | N/A |
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
| `w` | am (apply patches) | ∂ in-progress continue/skip/abort only |
| `W` | patch (format patches) | ✗ |
| `X` | reset | ✓ ours `O` (vanilla `X`) |
| `y` / `Y` | show-refs / cherry | ✗ |
| `z` | stash | ✓ ours `Z` (vanilla `z`) |
| `Z` | worktree | ✗ |
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
| `f` / `s` | fixup / squash | ✗ |
| `A` / `n` / `W` | alter / augment / revise | ✗ |
| `F` / `S` | instant-fixup / instant-squash | ✗ |
| `R` | rebase-reword-commit (level 0) | ✓ ours "Reword past", visible by default; drops commit-only switches when firing |
| `x` | autofixup (level 6) | ✗ |
| `X` | absorb-modules (level 6) | ✗ |

Sub-transients `magit-commit-absorb` (needs git-absorb) and
`magit-commit-autofixup`: ✗. The fixup/squash column is the biggest commit
gap.

### Branch (magit `b` / ours `b`)

**Arguments**: magit has one, `-r --recurse-submodules` (level 7) — ✗ (ours
has no branch args).

**Actions**

| Key | Command | Status |
|-----|---------|--------|
| — | Configure `<branch>` variables (`d` description, `u` merge/remote, `r` rebase, `p` pushRemote; `R`/`P`/`B` repo defaults) | ✗ |
| `b` | checkout branch/revision | ✓ |
| `l` | checkout local branch | ✗ |
| `o` | orphan (level 6) | ✗ |
| `c` | branch-and-checkout | ✓ |
| `s` / `S` | spinoff / spinout | ✗ |
| `w` / `W` | worktree-checkout / worktree-branch (level 5) | ✗ |
| `n` | create | ✓ |
| `C` | configure… | ✗ |
| `m` | rename | ✓ |
| `x` | branch-reset | ✗ — key conflict: our evil preset uses `x` for delete |
| `k` | delete | ✓ ours `x` evil / `k` vanilla |
| `h` / `H` | shelve / unshelve (level 7) | ✗ |

`magit-branch-configure` (per-branch variables + `a m`/`a r` auto-setup): ✗
entirely; no git-variable editing exists anywhere in Magritte.

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
| `o` | another branch | ✗ |
| `r` | explicit refspecs | ✗ |
| `m` | matching branches | ✗ |
| `T` / `t` | a tag / all tags | ✗ |
| `n` | note ref (level 6) | ✗ |
| `C` | branch-configure | ✗ |

The whole "push things other than the current branch" group is missing.

### Pull (magit `F` / ours `F`)

**Arguments**

| Key | Argument | Status |
|-----|----------|--------|
| `-f` | `--ff-only` | ✗ |
| `-r` | `--rebase` | ≈ ours negatable, seeded from `pull.rebase`, emits `--no-rebase` |
| `-A` | `--autostash` (level 7) | ✗ |
| `-F` | `--force` | ✗ |

**Actions**: `p`/`u`/`e` ✓; the optional "Fetch from"/"Fetch" groups
(`:if magit-pull-or-fetch`, off by default upstream) ✗; `r` branch.rebase
variable ✗ (our config-seeded `-r` partially substitutes); `C` configure ✗.

Magit declares `--ff-only`/`--rebase` incompatible; if we add `--ff-only`,
we need an incompatibility mechanism (see cherry-pick).

### Fetch (magit `f` / ours `f`)

**Arguments**: `-p --prune` ≈ (ours negatable, seeded from `fetch.prune`);
`-t --tags` ✗; `-u --unshallow` (level 7) ✗; `-F --force` ✗.

**Actions**: `p`/`u`/`e`/`a` ✓; `o` branch ✗; `r` refspec ✗; `m` submodules ✗
(no submodule support); `C` configure ✗. `magit-fetch-modules` sub-transient
✗. Ours only: the background `[fetch]` auto-fetch loop.

### Merge (magit `m` / ours `m`)

**Arguments**: `-f --ff-only` ✓; `-n --no-ff` ✓; `-s --strategy=` ✗;
`-X --strategy-option=` (level 5) ✗; `-b`/`-w` ignore-space (level 5) ✗;
`-A -Xdiff-algorithm=` (level 5) ✗; `-S --gpg-sign=` ✗; `+s --signoff`
(level 6) ✗. The `--ff-only`/`--no-ff` incompatibility is not enforced
(git errors at runtime).

**Actions**: `m` plain ✓; `n` no-commit ✓; `s` squash ✓; `e` edit-message ✗;
`a` absorb ✗; `p` preview ✗; `d` dissolve ✗. In progress: magit offers `m`
"Commit merge" and `a` abort; ours shows only `a` abort (committing the
resolved merge goes through the regular `c` commit transient) — ≈.

### Log (magit `l` / ours `l`)

**Arguments**

| Key | Argument | Status |
|-----|----------|--------|
| `-n` | limit count | ✓ |
| `-A` | `--author=` | ✓ |
| `=s` / `=u` | `--since=` / `--until=` (level 7) | ✗ |
| `-F` | `--grep=` | ✓ |
| `-i` / `-I` | ignore-case / invert-grep (level 7) | ✗ |
| `-G` / `-S` | search changes / occurrences | ✓ |
| `-L` | trace line range | ✗ |
| `=m` / `=p` | `--no-merges` / `--first-parent` (level 7) | ✗ |
| `-D` | `--simplify-by-decoration` | ✗ |
| `--` | limit to files | ✓ |
| `-f` | `--follow` | ✗ |
| `/s /d /a /f /m` | history simplification (levels 6–7) | ✗ |
| `-o` | commit order | ✓ |
| `-r` | `--reverse` | ✓ |
| `-g -c -d =S -h -p -s` | graph/color/decorate/signature/header/patch/stat | ✗ buffer-formatting toggles with no home in our fixed-format list |

**Actions**: `l` current ✓; `o` other ✓; `a` all references ≈ (ours labeled
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

**Arguments**: `-m --mainline=` ✓; `-e --edit` ≈ **default inverted** —
magit defaults `--edit` on; ours is off and injects `--no-edit` when no args
are toggled (and drops that fallback once any other arg is toggled);
`-E --no-edit` ✓; `=s --strategy=` ✗; `-S --gpg-sign=` ✗; `+s` (level 6) ≈
ours `-s` visible.

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

**Arguments**: magit `-u --include-untracked` ≈ (ours models untracked
inclusion as the separate `Z` action, so it can't combine with future
variants); `-a --all` (untracked + ignored) ✗.

**Actions**

| Key | Command | Status |
|-----|---------|--------|
| `z` | both | ≈ ours runs `git stash push` with **no message prompt**; magit prompts |
| `i` / `w` / `x` | index only / worktree only / keeping index | ✗ |
| `P` | push… sub-transient (level 5; `--` file limiting, keep-index) | ∂ our `z` is `git stash push` but with no file limiting or keep-index |
| `Z` / `I` / `W` | snapshots | ✗ (our `Z` key is taken by "both incl. untracked") |
| `r` | wip-commit | ✗ (no wip mode) |
| `a` / `p` / `k` | apply / pop / drop | ✓ (picker; also stash-row keys) |
| `l` | list | ≈ the Stashes status section; no dedicated buffer |
| `v` | show | ≈ Enter on a stash row; not reachable from the transient |
| `b` / `B` | branch from stash / branch here | ✗ |
| `f` | format-patch | ✗ |

### Tag (magit `t` / ours `t`)

**Arguments**: `-f` ✓; `-a` ✓; `-e --edit` ✗; `-s --sign` ✗;
`-u --local-user=` ✗.

**Actions**: `t` create ✓; `r` release (version-tag conventions) ✗;
`k` delete ≈ single tag via picker, no region multi-delete; `p` prune
(local vs remote) ✗.

### Remote (magit `M` / ours `M`)

**Arguments**: `-f` fetch-after-add ✓ (default on, both).

**Actions**: `a`/`r`/`k` add/rename/remove ✓; the variables group
(`u`/`U`/`s`/`S`/`O`/`h` for `remote.<name>.*`) ✗; `C` configure
sub-transient ✗; `p` prune ✗; `P` prune-refspecs ✗; `z` unshallow (level 7)
✗; `d u` update-default-branch ✗. (Tracked as TODO: remote variable parity.)

### Reset (magit `X` / ours `O` evil, `X` vanilla)

The six modes `m`/`s`/`h`/`k`/`i`/`w` are at parity (same keys; ours
confirms hard and worktree). Missing: `b` branch-reset (reset a *branch*,
not HEAD) ✗ and `f` file-checkout (reset one file to a revision) ✗.

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

Magit's `!` is a transient; ours is a free-text prompt prefilled with
`git ` (POSIX-quoted split, **no shell**; output to the `$` log).

| Key | Command | Status |
|-----|---------|--------|
| `!` | git command in repo root | ✓ |
| `p` / `S` | git / shell command in buffer's directory | N/A — no "current buffer directory" in a status-centric app |
| `s` | shell command in repo root | ≈ deleting the `git ` prefix runs any program, but with no shell semantics (no pipes/globs); `[[command]]` config entries do run `sh -c` |
| `k` `a` `b` `g` | gitk / git-gui launchers | N/A (Magritte is the GUI) |
| `m` | `git mergetool --gui` | ✗ (meaningful standalone) |

The one real gap is shell interpretation for ad-hoc commands. (Tracked as
TODO: full `!` run transient.)

### Missing transients

**Bisect (`B`)** ✗ — mark good/bad/skip until the culprit is found, optional
run-script; magit adds bisect sections to status while active. Args:
`--no-checkout`, `--first-parent`, term renames (level 6). Building it: an
in-progress banner like our rebase banner plus a start flow; all plain
`git bisect` subcommands.

**Blame** ✗ — annotated file view (`git blame --porcelain`), chunk motion,
re-blame at addition/removal, style cycling. The display machinery is the
bulk; the git side is one command.

**Show-refs (`y`)** ✗ — the refs browser: all branches/tags with
ahead/behind counts vs a comparison point, visit/rename/delete at point.
Args: `--contains=`, `--merged[=]`, `--no-merged[=]`, `--sort=`. A new
screen over `git for-each-ref`; substantial but high-value (it's also where
branch/tag rows as act-at-point targets would live).

**Worktree (`Z`/`%`)** ✗ — checkout/branch into a new worktree, move,
delete, visit. Magritte is already worktree-aware internally (per-worktree
UI state); "visit" means opening the other checkout's window.

**Patch (`W`)** ✗ — format-patch (sub-transient with mail args, reroll,
cover letters), apply plain patch (`--index`/`--cached`/`--3way`), save diff
as patch, request-pull. Pairs with the am gap.

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

**Ediff (`E`/`e`)** N/A as such — the standalone analog is a real
merge-conflict resolution view (today we only offer take-ours/theirs via the
context menu). **Mergetool** ✗ — launching `git mergetool --gui` per
conflicted file is meaningful standalone.

**File-dispatch** mostly N/A (buffer-centric entry point, blob navigation);
its file-scoped operations (untrack, rename, file log, blame-this-file)
are ✗ and would live as act-at-point commands on file rows.

**Margin-settings** N/A (Emacs window margins). **Insert-trailer** ∂ —
trailer insertion (Acked-by/Reviewed-by/Co-authored-by…) would be a natural
commit-editor helper; changelog insertion N/A.

Two build-once dependencies recur across these: a **git-variable infix
widget** (read/cycle/set `git config` values — branch-configure,
remote-configure, notes, mergetool, pull's `r`) and a **no-repo app state**
(clone, init).

### Non-transient magit commands

| magit key | Command | Magritte | Status |
|---|---|---|---|
| `g` | refresh | `g r` evil, `g` vanilla | ✓ |
| `G` | refresh-all | vanilla `G` → plain refresh | ≈ deliberate — single-buffer app |
| `q` | bury buffer | Esc/`q` close sub-screens | ≈ no buffer stack |
| `$` | process buffer | `$` command log | ✓ (see Other buffers) |
| `%` / `Z` | worktree | — | ✗ |
| `Q` / `:` | git-command | vanilla `:`/`Q`, evil `\|` | ✓ |
| `s`/`S`/`u`/`U` | stage/stage-modified/unstage/unstage-all | same keys | ✓ (`S` ≈, see act-at-point) |
| `k` | delete-thing | evil `x` / vanilla `k` discard | ≈ stash-row drop is hardcoded `x` in both presets |
| `K` | file-untrack | — | ✗ |
| `R` | file-rename | — | ✗ |
| `x` | reset-quickly (reset to rev at point) | — | ✗ (`x` is discard in evil; unbound in vanilla) |
| `Y` | cherry | — | ✗ |
| `I` | init | — | ✗ |
| `y` | show-refs | — | ✗ |
| `RET` | visit-thing | Enter opens file/commit/stash | ✓ (≈ semantics, see act-at-point) |
| `C-RET` | visit in other window | — | N/A |
| `SPC` / `DEL` | show-or-scroll (peek commit at point) | page down / up | ≈ no preview concept |
| `+` / `-` / `0` | more / less / default diff context | — | ✗ no context adjustment anywhere |
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
| untracked files | `Untracked` — but expanded (magit collapses the heading), uncapped (magit caps at 100 with "N not listed"), and core hardcodes `--untracked-files=normal`, overriding `status.showUntrackedFiles` | ≈ |
| unstaged / staged changes | same model (files collapsed, lazy diffs) | ✓ |
| stashes | present but **expanded** (magit hides by default) | ∂ |
| unpushed-to-pushremote | same suppression rule (only when distinct from upstream) | ✓ |
| unpushed-to-upstream **or** recent | magit shows exactly one: "Recent commits" when not ahead of upstream, else "Unmerged into upstream"; we always show both `Unpushed` and `Recent` | ∂ also heading wording differs |
| unpulled pair | ✓ | ✓ |
| child counts | `(N)` ✓; but our unpushed/unpulled fetches are unlimited — no `N+` cap marker, and a pathological divergence lists every commit | ≈ |
| file-list caps | none | ✗ |
| optional sections (tracked, skip-worktree, assume-unchanged, cherries, worktrees, modules, ignored) | only `ignored` exists (opt-in) | ∂ 1 of ~8 |

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
| `S-TAB` | global cycle | — | ✗ |
| `1`–`4` | show-level of the **surrounding** section (point-local, region-aware) | buffer-wide | ∂ ours implement magit's `M-1..4`; no local variant, and they clear the visual selection instead of honoring it |
| `M-1`–`M-4` | show-level **all** | `alt-1..4` = same buffer-wide command | ✓ |
| `SPC`/`DEL` | peek/scroll the commit at point in the other window | page down/up | ∂ no preview |
| point restoration | goto-successor | AnchorIdent rebuild anchoring | ✓ |
| visibility indicators | fringe/`…` | chevrons | ✓ |

The biggest functional holes: no cycling at all, and no SPC preview (magit
users lean on it to skim unpushed/recent commits without leaving status).

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
| `u` committed change | **reverses it in the index** (`magit-unstage-committed` t) — the "extract a change from HEAD" flow | nothing | ✗ notable |
| `k`/`x` discard untracked | delete → **system trash**, confirm | system trash, confirm (git clean fallback when unavailable) | ✓ |
| `k`/`x` discard unstaged/staged | confirm; entry-dispatched | mirrors magit exactly (incl. partial-discard `.rej` reporting) | ✓ |
| `k` conflicted hunk | smerge-keep-current + per-hunk smerge keys | keyboard verbs refused; take-ours/theirs via right-click only | ∂ |
| `v` reverse at point | reverse staged/committed hunk/file/region in worktree | no reverse verb (revert-no-commit on whole commit rows only) | ✗ |
| `a` apply at point | apply committed hunk/file to worktree; untracked file → am; prefix = 3-way | cherry-apply on commit rows only | ∂ |

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
  disproven against the source). Drop is `k` in magit vs hardcoded `x` in
  ours — in the vanilla preset that's an inconsistency (vanilla discard is
  `k`).
- **Stashes header**: magit `RET` opens a stash-list buffer, `k` clears all
  stashes (confirmed). Ours: fold only; no list buffer, no clear anywhere. ✗
- **Branch/tag/remote/worktree rows**: live in magit's refs buffer, which we
  lack; all our ref operations go through transients + pickers. ✗

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
  git-command, `$`, `?`/`h`, `j` jump, `s`/`u`/`U`, `n`/`p`/`M-n`/`M-p`/`^`,
  `TAB`, `C-w` copy.
- ≈: `G` is a refresh alias (magit: refresh-all — deliberate, single
  buffer); `S` includes untracked; `k` discards but stash-drop stays on `x`;
  `1`–`4` semantics; `SPC`/`DEL` page instead of preview; `RET` worktree-
  file semantics.
- ✗ keys with no binding at all: `x` (reset-quickly), `K`, `R`, `+`/`-`/`0`,
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
- ≈: **`z` stash** — evil-collection's default keeps magit's `z`; our `Z` is
  its non-default `use-z-for-folds` layout (without the z-fold family).
  `$` — evil-collection moves the process buffer to `` ` `` by default; we
  keep `$`. `C-w` is our yank (evil-collection: window-map) — deliberate.
  `C-u` scrolls unconditionally (evil gates it behind `want-C-u-scroll`).
  `:` opens our palette (evil-ex analog). `gh` section-up isn't bound
  (magit's `^` works).
- ✗: `gR` refresh-all, `o` reset-quickly, `X` untrack, `'`/`"`
  submodule/subtree, `=` less-context, `~` default-context, `S-SPC` preview,
  `/ n N` search, `yb` copy-buffer-revision, `yr` show-refs (`y` covers
  `yy`/`ys` roughly).
- Rebase todo editor vs evil git-rebase-mode: `p r e s f d` ✓ (+ our `w`
  reword alias); `x` ≈ collision — evil's `x` is **exec** (which we lack
  entirely), ours is a drop alias; `M-j`/`M-k` move vs our `J`/`K`;
  `ZZ`/`ZQ` vs our `Enter`/`Esc`; `u` undo ✗.

## Other buffers & screens

- **Log**: browse + act ✓ (open, cherry-pick, revert, rebase-since, copy).
  Missing: `=`/`+`/`-` limit controls (our cap is fixed at 256), `j`
  move-to-revision, `L` refresh/margins, `SPC` preview. Log-select: same
  capability, different chord (`Cmd-Enter` confirms; `Enter` inspects).
- **Revision/commit buffer**: ours shows message + flat diff + `a` details
  toggle; magit's adds notes and a diffstat section (`--stat` default),
  per-file visiting, `j` revision-jump, refine-hunk. ∂ thinner.
- **Diff buffer**: entry points ✓; the resulting view is display-only — no
  context keys, no `D` refresh transient (refine/file-filter/range-type/
  flip-revs). `C-c C-d` diff-while-committing ≈ our commit editor embeds the
  staged diff by default.
- **Refs buffer**: ✗ entirely (see Show-refs above).
- **Process buffer**: ≈ — magit has one collapsible section per subprocess
  and `k` kill-at-point; ours is a flat pager, but adds per-command timings
  with slow-command coloring and the hidden-queries toggle. Kill is global
  (`Esc`/`C-g` cancels the running job) rather than at-point.
- **Blame / bisect**: ✗ (no screens).
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
| stash drop / clear | prompt / confirm | `x` confirms; picker drop relies on the pick; no clear | ≈ |
| hard / worktree-only reset | rev prompt only, no y/n | rev pick + y/n confirm | ✓ stricter |
| amend/reword/extend published | confirms vs `magit-published-branches` (default `origin/master`) | confirms vs `published_branches` (default adds `origin/main`) | ✓ |
| rebase across published | confirms | confirms | ✓ |
| commit with nothing staged | shows diff + y/n | y/n then `--all` editor (no diff preview at prompt time) | ≈ |
| abort in-progress sequence | confirms | confirms | ✓ |
| set-upstream-and-push | the one default no-confirm | no confirm either | ✓ |
| delete unmerged branch | confirms, then `-D` | plain `-d` refuses; no force path | ≈ safe but can't force-delete |
| destructive `[[command]]` | no analog | confirms | Magritte-only |

## Recommendations

Grouped by kind, roughly ordered within each group.

**Behavior fixes in shared features (small, high value)**

2. Match magit's revert default (`--edit` on) or make the divergence a
   documented choice.
6. Honor `status.showUntrackedFiles` instead of hardcoding.
7. Cap unpushed/unpulled listings and show `(N+)`.
8. Reconsider evil `z` = stash (evil-collection's default) with `Z` for
   "include untracked", or document the deviation.
9. Rebase-todo `x`: reserve for exec (or drop the alias) before it
   entrenches.

**High-value additions to existing surfaces**

- Commit fixup/squash column (`f`/`s`, then `F`/`S` instant variants,
  rebase `f` autosquash) — the most-missed magit workflow.
- `SPC` show-or-scroll preview of the commit/stash at point.
- `u` on committed changes (reverse-in-index), `v` reverse-at-point, `a`
  apply-at-point — the second half of magit's apply engine.
- Diff context keys `+`/`-`/`0` on status hunks.
- Merge: in-progress `m` commit-merge; `e` editmsg; `p` preview; strategies.
- Push `o`/`T`/`t` (other branch, tags).
- Log: `--since`/`--until`/`--no-merges`/`--first-parent` args; `=`/`+`/`-`
  limit keys in the log view.
- Stash variants (`i`/`w`/`x`), file-limited stash push, `b` branch-from-
  stash.
- Section-local `1`–`4` and `S-TAB` global cycling.
- `x` reset-quickly, `K` untrack, `R` rename at point.
- Reset `b` (branch) and `f` (file checkout).
- The git-variable widget → branch-configure + remote-configure (existing
  TODO) + tag `-u`.

**Whole missing features, ranked for a standalone client**

1. Refs browser (`y` show-refs) — also unlocks branch/tag act-at-point.
2. Blame view.
3. Worktree commands (we're already worktree-aware internally).
4. Bisect (banner-driven, like our sequence UI).
5. Patch create/apply + starting `git am`.
6. Clone/init (needs the no-repo app state).
7. Conflict-resolution view beyond take-ours/theirs (the ediff analog),
   and/or `git mergetool` launching.
8. Submodules; then notes, cherry, subtree, sparse-checkout, bundle, wip.

**Deliberate deviations to keep (document, don't "fix")**

- Title-bar headers instead of buffer header lines; clickable chrome.
- `RET` opens the worktree file in the external editor (no blob buffers).
- `G` as a refresh alias; single-buffer model.
- The `$` log as a flat pager with timings and the queries toggle.
- Expanded-by-default sections with on-disk fold persistence.
- Always showing both Unpushed and Recent (vs magit's either/or) — arguably
  clearer; keep unless it proves noisy.
- The permissive visual-selection batching (with its stricter
  conflicted-file refusal).
- No shell in `!` (with `[[command]]` as the escape hatch).
- Stricter reset confirms and the wider `published_branches` default.
