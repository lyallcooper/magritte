#!/usr/bin/env bash
# Scaffold a throwaway git playground for manual testing. Builds a working clone
# with real (bare, on-disk) remotes so push/pull/fetch work end-to-end, plus —
# depending on the scenario — branch topology for merge/rebase/reset/cherry-pick
# or a repo left paused mid-merge/mid-rebase to exercise the in-progress banner.
#
# Usage:
#   scripts/playground.sh [dir] [scenario]
#     dir       where to build it (default: /tmp/magritte-playground)
#     scenario  (default: diverged)
#
#   Remote transfer:
#     clean      work even with origin; only a dirty worktree to stage/commit
#     ahead      work has an unpushed commit            → push succeeds
#     behind     origin advanced elsewhere (no overlap) → fetch behind, pull FFs
#     diverged   both sides advanced, same line         → push rejected (non-ff),
#                                                          pull conflicts
#   Local history (clean tree, run the op in-app):
#     branches   main + feature (clean merge/rebase + interactive-rebase fodder)
#                + conflicting (conflicts on merge/rebase); covers merge (m),
#                rebase (r), interactive rebase, reset (O), cherry-pick/revert
#                (in the log, l), branch checkout/create/rename/delete
#   Paused sequences (open the app to resolve/continue/abort):
#     merge-conflict    left mid-merge with a conflict
#     rebase-conflict   left mid-rebase with a conflict
#
# Every scenario sets up three remotes: origin (tracked, file://), upstream (a
# second remote), and slow (an ext:: helper that hangs ~30s, for C-g cancel).
#
# Then launch:  scripts/dbg.sh up <dir>/work
#
#   scripts/playground.sh help        show this help
set -euo pipefail

# Print the header comment block (between the shebang and `set -…`) as usage.
usage() { sed -n '2,/^set /p' "$0" | sed '/^set /d; s/^#\{0,1\} \{0,1\}//'; }

case "${1:-}" in
  help | -h | --help)
    usage
    exit 0
    ;;
esac

DIR="${1:-/tmp/magritte-playground}"
SCENARIO="${2:-diverged}"
W="$DIR/work"

# Validate up front so a typo fails before we build (and wipe) anything.
case "$SCENARIO" in
  clean | ahead | behind | diverged | branches | merge-conflict | rebase-conflict) ;;
  *)
    echo "playground.sh: unknown scenario: $SCENARIO" >&2
    echo "  use: clean|ahead|behind|diverged|branches|merge-conflict|rebase-conflict" >&2
    echo "  (run 'scripts/playground.sh help' for details)" >&2
    exit 1
    ;;
esac

# Run git so a locked 1Password signing key or a global hook never blocks the
# scaffolding (the app's own commits still sign normally).
g() { git -c commit.gpgsign=false -c core.hooksPath=/dev/null -C "$W" "$@"; }
commit() { g commit --no-gpg-sign -q -m "$1"; }
# Overwrite a tracked file, stage it, and commit.
edit() { printf '%b' "$2" > "$W/$1"; g add "$1"; commit "$3"; }

echo "Building $SCENARIO playground in $DIR"
# Refuse to wipe a directory that doesn't look like a previous playground
# (`origin.git` is its sentinel) — a mistyped path must not become `rm -rf ~`.
if [ -e "$DIR" ] && [ -n "$(ls -A "$DIR" 2>/dev/null)" ] && [ ! -d "$DIR/origin.git" ]; then
  echo "playground.sh: $DIR exists, is not empty, and doesn't look like a playground; refusing to delete it" >&2
  exit 1
fi
rm -rf "$DIR"
mkdir -p "$DIR"
cd "$DIR"

git init -q --bare origin.git
git init -q --bare upstream.git

# A remote helper that just blocks, reached via the ext:: transport — a
# deterministic stand-in for a hung network transfer.
printf '#!/bin/sh\nsleep 30\n' > slow-helper.sh
chmod +x slow-helper.sh

git clone -q "file://$DIR/origin.git" work
g config user.email tester@example.com
g config user.name "Test User"
g config protocol.ext.allow always
g remote add upstream "file://$DIR/upstream.git"
g remote add slow "ext::$DIR/slow-helper.sh %G"

# Base history shared by everyone.
printf 'line 1\nline 2\nline 3\n' > "$W/shared.txt"
printf 'first\nsecond\n' > "$W/conflict.txt"
printf 'notes\n' > "$W/notes.txt"
g add .
commit "initial commit"
g push -q -u origin main
g push -q upstream main

# A throwaway "teammate" clone used to advance origin from the other side, then
# discarded — so `work` discovers the new state only via fetch/pull.
advance_origin() { # file content msg
  rm -rf "$DIR/teammate"
  git clone -q "file://$DIR/origin.git" "$DIR/teammate"
  printf '%b' "$2" > "$DIR/teammate/$1"
  git -c commit.gpgsign=false -c core.hooksPath=/dev/null -C "$DIR/teammate" \
    -c user.email=mate@example.com -c user.name="Team Mate" commit -aqm "$3"
  git -C "$DIR/teammate" push -q origin main
  rm -rf "$DIR/teammate"
}

# One unstaged edit, one staged new file, one untracked file — for status/stage/
# discard/commit testing. Only for the remote scenarios; the history scenarios
# stay clean so merge/rebase aren't blocked by a dirty tree.
dirty_worktree() {
  printf 'notes\nunstaged edit\n' > "$W/notes.txt"
  printf 'staged new file\n' > "$W/staged.txt"
  g add staged.txt
  printf 'untracked\n' > "$W/untracked.txt"
}

case "$SCENARIO" in
  clean)
    dirty_worktree ;;
  ahead)
    edit shared.txt 'line 1\nline 2\nline 3\nlocal unpushed\n' "local work (ahead of origin)"
    dirty_worktree ;;
  behind)
    advance_origin shared.txt 'line 1\nline 2\nline 3\nfrom teammate\n' "teammate change"
    dirty_worktree ;;
  diverged)
    advance_origin conflict.txt 'THEIRS one\nsecond\n' "teammate edits line 1"
    edit conflict.txt 'OURS one\nsecond\n' "our edit to line 1 (diverges from origin)"
    dirty_worktree ;;

  branches)
    # main advances past where the branches fork.
    edit shared.txt 'line 1\nMAIN line 2\nline 3\n' "main: rewrite line 2"
    edit main-only.txt 'main only\n' "main: add main-only.txt"
    # feature forks from the initial commit; touches only feature.txt, so it
    # merges/rebases onto main cleanly. The WIP/oops commits are squash/fixup
    # fodder for interactive rebase.
    g checkout -q -b feature main~2
    edit feature.txt 'feature\n' "feature: add feature.txt"
    edit feature.txt 'feature\nmore\n' "WIP typo"
    edit feature.txt 'feature\nmore\neven more\n' "feature: extend feature.txt"
    edit feature.txt 'feature\nmore\neven more\n\n' "oops trailing blank line"
    # conflicting forks from the same point but rewrites the line main changed,
    # so merging/rebasing it hits a conflict.
    g checkout -q -b conflicting main~2
    edit shared.txt 'line 1\nCONFLICT line 2\nline 3\n' "conflict: rewrite line 2"
    g checkout -q main ;;

  merge-conflict)
    edit conflict.txt 'MAIN one\nsecond\n' "main: edit conflict line 1"
    g checkout -q -b feature main~1
    edit conflict.txt 'FEATURE one\nsecond\n' "feature: edit conflict line 1"
    g checkout -q main
    g merge feature -m "merge feature" || true ;;

  rebase-conflict)
    edit conflict.txt 'MAIN one\nsecond\n' "main: edit conflict line 1"
    g checkout -q -b feature main~1
    edit conflict.txt 'FEATURE one\nsecond\n' "feature: edit conflict line 1"
    g rebase main || true ;;
esac

echo
echo "Ready. Launch with:  scripts/dbg.sh up $W"
echo
echo "Remotes:  origin (tracked), upstream (2nd), slow (hangs ~30s)"
case "$SCENARIO" in
  ahead)    echo "Try:  push (p) — succeeds; origin had no new commits." ;;
  behind)   echo "Try:  fetch (f) shows 1 behind; pull (F) fast-forwards." ;;
  diverged) echo "Try:  push (p) rejected (non-ff) → pull (F) → resolve conflict.txt." ;;
  clean)    echo "Try:  stage/commit the dirty files; push (p) to 'elsewhere'." ;;
  branches) cat <<EOF
Branches: main (current), feature (clean), conflicting (conflicts).
Try:  merge (m) feature → clean;  merge (m) conflicting → conflict.
      rebase (r) — checkout feature first, rebase onto main → clean.
      rebase (r) conflicting onto main → conflict.
      interactive rebase on feature → squash the WIP/oops commits.
      reset (O) main back a commit; cherry-pick/revert from the log (l).
EOF
;;
  merge-conflict)  echo "Open it: paused mid-merge. Resolve conflict.txt, or abort the sequence." ;;
  rebase-conflict) echo "Open it: paused mid-rebase. Resolve, continue/skip, or abort." ;;
esac
if [ "$SCENARIO" != "merge-conflict" ] && [ "$SCENARIO" != "rebase-conflict" ]; then
  echo "Cancel:  fetch/push the 'slow' remote (→ elsewhere → slow), then C-g."
fi
