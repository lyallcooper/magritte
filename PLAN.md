# Magritte — Project Plan

> *Ceci n'est pas Magit.*

Magritte is a standalone macOS git client that reproduces the feel and the most
important features of [magit](https://magit.vc/), without Emacs. It is fast,
keyboard-driven, mouse-friendly, native-feeling, and — unlike most TUI git
clients — designed from the start to stay responsive in very large repositories.

---

## 1. Goals

1. **Feels like magit** — fast, keyboard-first, the status buffer as the home
   base, "act on the thing at point" semantics, transient popups for commands.
2. **Feels like a native macOS app** — native window chrome, standard shortcuts,
   trackpad/mouse friendly, in the same vein as Zed or Ghostty (not necessarily
   AppKit, but a well-behaved OS citizen).
3. **Faithfully reproduces the most important ~80% of magit** — staging,
   committing, diffs, branches, log, push/pull, stash, merge, rebase.
4. **evil-collection magit keybindings by default** — the bindings a magit +
   evil-mode user already has in muscle memory.
5. **Usable in large repos** — async by design; the UI thread never blocks on
   git. (See [gitu#374](https://github.com/altsem/gitu/issues/374) for the
   failure mode we are explicitly avoiding.)
6. **Robust and thoughtfully designed** — typed errors, a tested UI-agnostic
   core, predictable behavior, no data-loss footguns.

## 2. Non-goals (at least for v1)

- Cross-platform support. We design the core to be portable, but only macOS is a
  target initially.
- A general Emacs/text-editor experience. Magritte is a git client.
- Hosting-provider integration (PRs, issues, CI). Possible later; not core.
- Being a literal magit reimplementation. We mirror behavior and bindings, not
  Emacs internals.

---

## 3. Architecture

### 3.1 The central seam: synchronous core, async at the boundary

```
            ┌─────────────────────────────────────────┐
            │  magritte (GPUI app)                      │
            │   • views, transient popups, keymap       │
            │   • background_executor().spawn(...)       │
            │   • generation-counter cancellation        │
            └───────────────────┬───────────────────────┘
                                 │ plain data in/out
            ┌───────────────────▼───────────────────────┐
            │  magritte-core (no UI deps, synchronous)   │
            │   • Repo: git CLI runner                    │
            │   • status / diff / log / refs parsers      │
            │   • staging patch construction              │
            │   • command + transient *model*             │
            └───────────────────┬───────────────────────┘
                                 │ subprocess
                            ┌────▼────┐
                            │  git    │
                            └─────────┘
```

`magritte-core` is **synchronous and UI-free**. It invokes `git` and returns
plain data structures. This is what makes it unit-testable against throwaway
repos with no graphics stack, and it is the hedge against GPUI's API churn: the
hard logic does not depend on the frontend.

All asynchrony and cancellation live at the **GPUI boundary**. Every git call is
dispatched to a background executor; the UI thread is never allowed to block on
git. Each refresh carries a **generation counter** so results from superseded
work are dropped rather than rendered.

### 3.2 Git backend: CLI-first hybrid

We shell out to the `git` binary and parse its porcelain/`-z` output rather than
linking libgit2 as the primary engine. Rationale:

- **Behavioral fidelity** — identical semantics to what magit users expect
  (gitignore edge cases, hooks, config, sparse/partial clones).
- **Large-repo performance** — the CLI transparently benefits from git's own
  optimizations: `fsmonitor`, the untracked cache, `commit-graph`. libgit2 does
  not.
- **Stability** — porcelain v2 and `-z` formats are explicitly contracts.

libgit2 (`git2` crate) may be used later for select hot paths where in-process
access clearly wins, but it is never the source of truth for behavior.

### 3.3 Staying fast in large repos

This is requirement #5 and the main thing that differentiates Magritte.

- **Lazy diffs.** The file-level overview comes from
  `git status --porcelain=v2 -z` (fast). Per-file diffs are computed *only when a
  section is expanded*, via `git diff -- <path>` scoped to that path. Because
  magit sections start collapsed, opening a huge repo renders almost nothing.
- **Virtualized rendering.** Only on-screen lines become view nodes, so a
  50k-line diff costs the same to render as a 50-line one.
- **Cancellation.** Navigating or refreshing again cancels in-flight git work
  (generation counter / dropped results).
- **Incremental refresh.** A filesystem watcher (FSEvents via the `notify`
  crate) on the worktree and `.git`, debounced, drives targeted refreshes rather
  than re-running everything.
- **No silent caps.** If we ever bound output (e.g. log pagination), the UI says
  so rather than pretending it showed everything.

### 3.4 State model: the section tree

The status view is a tree of collapsible **sections**, mirroring magit:

```
Head:     main  <subject of HEAD commit>
Push:     origin/main
─────────────────────────────────────────
Untracked files (3)            [collapsed]
Unstaged changes (5)           [collapsed]
Staged changes (2)             [collapsed]
Stashes (1)                    [collapsed]
Unpulled from origin/main
Unpushed to origin/main
Recent commits
```

Each section node has: a kind, a fold state, a lazily-populated body, and a
"target" identity used by the act-at-point commands. The same tree abstraction
serves the status view, diff views, and log views.

### 3.5 Commands and transients

magit's signature UI is the **transient popup** (`c` → commit menu with
switches, options, and actions). We model a transient declaratively in core:

```
Transient {
  groups: [
    Group { description, suffixes: [
      Switch  { key: "-f", argument: "--force", description },
      Option  { key: "=u", argument: "--set-upstream", reader },
      Action  { key: "p", description, command },
    ] }
  ]
}
```

The *model* (which commands exist, their arguments) is UI-agnostic and lives in
core; the popup *rendering* and key dispatch live in the frontend. This keeps
the command surface testable and lets us drive the same commands from the
keyboard or the mouse.

### 3.6 Keybindings

Default keymap mirrors **evil-collection's magit** layout, so existing muscle
memory transfers. Representative bindings (to be reconciled exactly against the
evil-collection source as we implement):

| Key        | Action                              |
|------------|-------------------------------------|
| `j` / `k`  | next / previous line                |
| `gj`/`gk`  | next / previous sibling section     |
| `TAB`      | toggle section fold                 |
| `gg` / `G` | top / bottom                        |
| `gr`       | refresh                             |
| `RET`      | visit / show thing at point         |
| `v` / `V`  | visual select lines/hunk (for partial staging) |
| `s` / `u`  | stage / unstage at point            |
| `S` / `U`  | stage all / unstage all             |
| `x`        | discard at point (with confirm)     |
| `c`        | commit transient                    |
| `b`        | branch transient                    |
| `P` / `F`  | push / pull transient               |
| `f`        | fetch transient                     |
| `l`        | log transient                       |
| `d`        | diff transient                      |
| `Z`        | stash transient                     |
| `r` / `m`  | rebase / merge transient            |
| `X`        | reset transient                     |
| `?`        | dispatch / help                     |
| `q`        | quit / bury buffer                  |

Keybindings will be data-driven and remappable. Mouse equivalents (click to
fold, click affordances for stage/unstage) accompany every keyboard action.

### 3.7 Crate layout

```
magritte/
  Cargo.toml                  workspace
  crates/
    magritte-core/            git engine + models + command/transient model (sync, no UI)
    magritte/                 GPUI application (binary)        [added at M1]
```

Additional crates (e.g. `magritte-watch` for FS watching, `magritte-ui` for
reusable widgets) will be split out if and when they earn their keep.

---

## 4. Feature scope — the 80%

### Tier 1 — the daily-driver loop (v1 must-have)
- Status view: section tree, fold/unfold, act-at-point.
- Diffs: file, hunk, and line/region granularity.
- Staging / unstaging / discarding at file, hunk, and region level.
- Commit: create, amend, reword, extend, fixup, squash + message editor.
- Push / pull / fetch.

### Tier 2 — rounds out the 80%
- Branches: create, checkout, delete, rename, set-upstream.
- Log: commit list with graph; show a commit's diff.
- Stash: save, pop, apply, drop, list.
- Diff arbitrary refs / ranges.
- Remotes: list, add, remove.
- Tags: create, delete.

### Tier 3 — high value, higher effort (likely post-v1)
- Merge (with conflict surfacing).
- Rebase (non-interactive), cherry-pick, revert.
- Interactive rebase (magit's beloved UI — disproportionate effort; deferred).
- Blame, bisect, reflog, submodules, worktrees.

---

## 5. Milestones

| #  | Theme                    | Deliverable                                                                 | Status |
|----|--------------------------|-----------------------------------------------------------------------------|--------|
| M0 | Core foundation          | `magritte-core` scaffold; porcelain-v2 status parser; tests                 | ✅ done |
| M1 | First pixels             | Minimal GPUI window rendering live status via background executor           | ✅ done |
| M2 | The tree                 | Section tree with fold/unfold; lazy per-file diffs; virtualized render; evil navigation | ✅ done |
| M3 | Staging                  | Stage/unstage/discard at file → hunk → region (patch construction + `git apply --cached`) | next |
| M4 | Commit & sync            | Commit transient + message editor; push / pull / fetch transients          |        |
| M5 | Breadth                  | Log view; branch transient; stash transient                                 |        |
| M6 | Robustness               | FS watcher + debounced incremental refresh; cancellation hardening; error surfacing | |
| M7 | Tier 3                   | Merge, rebase, cherry-pick, revert (interactive rebase as a stretch)        |        |

Each milestone ends in a buildable, demoable state.

## 6. Testing strategy

- **Core:** integration tests against throwaway repos (`tempfile` + isolated git
  config), plus pure-bytes parser tests for format edge cases. This is where the
  bulk of correctness lives because the core is UI-free.
- **Staging:** round-trip tests — construct a patch from a selection, apply it,
  re-read status, assert the expected staged/unstaged split.
- **Frontend:** thin; logic pushed into core. Snapshot/interaction tests where
  GPUI supports them.
- **Large-repo smoke test:** a generated synthetic repo to guard against
  accidental "compute everything up front" regressions.

## 7. Risks & open questions

- **GPUI API churn.** Pinned to a Zed git rev; no semver. Mitigation: the core
  seam, and pinning a known-good rev. Open question: vendor `gpui-component` for
  widgets or build our own minimal set?
- **Partial (region) staging correctness.** Synthesizing valid patches for
  arbitrary line selections (esp. context lines, renames, mode changes, CRLF) is
  subtle. Needs strong round-trip tests.
- **FS watcher noise.** `.git` churns a lot; debouncing and ignoring irrelevant
  paths matters to avoid refresh storms.
- **Exact evil-collection bindings.** The table above must be reconciled
  precisely against the package, including its remaps (e.g. discard on `x`).
- **Conflict / merge UX.** Surfacing and resolving conflicts well is a design
  problem of its own; scoped to Tier 3.
- **App packaging / signing.** macOS `.app` bundling, notarization, updates —
  deferred until there's something worth shipping.
