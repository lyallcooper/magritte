#!/usr/bin/env bash
# Drive a running Magritte in debug mode without AppleScript/screencapture.
#
# Usage:
#   scripts/dbg.sh up [repo-path]     launch magritte (debug mode) on repo-path (default: cwd)
#   scripts/dbg.sh down               quit the running magritte
#   scripts/dbg.sh send "<cmds>"      send a command batch (or pipe via stdin), print the response
#   scripts/dbg.sh key <keystroke>    e.g. key j, key shift-g, key tab, key escape, key ,
#   scripts/dbg.sh type <text>        type literal text into the focused input
#   scripts/dbg.sh shot <path>        screenshot the window (logical-sized: image px == click coords)
#   scripts/dbg.sh shot-raw <path>    screenshot at device pixels (Retina 2x; px != click coords)
#   scripts/dbg.sh targets            list clickable element ids and their center points
#   scripts/dbg.sh click-id <id>      click a clickable element by id (no coordinate guessing)
#   scripts/dbg.sh click <x> <y>      click at window-relative point (points, matches shot pixels)
#   scripts/dbg.sh shift-click-id <id>  shift-click an element by id (extends selection)
#   scripts/dbg.sh shift-click <x> <y>  shift-click at window-relative point
#   scripts/dbg.sh move <x> <y>       hover the pointer at a point (e.g. for tooltips)
#   scripts/dbg.sh drag <x1> <y1> <x2> <y2>  left-drag from one point to another (text selection)
#   scripts/dbg.sh dblclick <x> <y>   double-click at a point (open/enter)
#   scripts/dbg.sh rclick <x> <y>     right-click at a point (context menu)
#   scripts/dbg.sh sleep <ms>         pause (let a frame paint)
#   scripts/dbg.sh help               show this help
#
# Override the control dir with MAGRITTE_DEBUG_DIR (default: a per-user
# magritte-debug dir under $TMPDIR). Each dir controls its own instance, so
# several can run side by side.
set -euo pipefail

DIR="${MAGRITTE_DEBUG_DIR:-${TMPDIR:-/tmp}/magritte-debug}"
DIR="${DIR%/}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug/magritte"
# The log lives inside the control dir: no fixed world-writable /tmp path to
# symlink-hijack, and `up` on one instance can't clobber another's log.
LOG="$DIR/magritte.log"

# Kill the instance this control dir owns (recorded in $DIR/pid), if any —
# never other debug instances, which have their own dirs.
kill_instance() {
  if [ -f "$DIR/pid" ]; then
    pid="$(cat "$DIR/pid" 2>/dev/null || true)"
    [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  fi
}

# Print the header comment block (everything between the shebang and `set -…`)
# as the usage text, so the docs above are the single source of truth.
usage() { sed -n '2,/^set /p' "$0" | sed '/^set /d; s/^#\{0,1\} \{0,1\}//'; }

cmd="${1:-}"
shift || true

case "$cmd" in
  help|-h|--help)
    usage
    exit 0
    ;;

  up)
    repo="${1:-$PWD}"
    kill_instance
    sleep 0.3
    rm -rf "$DIR"; mkdir "$DIR"; chmod 700 "$DIR"
    # mkdir (not -p) fails if something reappeared at the path, so a symlink
    # planted between rm and mkdir can't redirect the control dir.
    [ -d "$DIR" ] && [ ! -L "$DIR" ] || { echo "refusing odd control dir: $DIR"; exit 1; }
    # Build with `debug-capture` so `shot` can grab the window via offscreen
    # render (works while the app is backgrounded/occluded). Dev-only feature.
    ( cd "$ROOT" && cargo build --features debug-capture ) || { echo "build failed"; exit 1; }
    # Stay in the foreground (MAGRITTE_FOREGROUND) so we keep this pid, the log
    # captures output, and the control channel is reachable — the app otherwise
    # detaches into the background.
    MAGRITTE_DEBUG_DIR="$DIR" MAGRITTE_FOREGROUND=1 "$BIN" "$repo" >"$LOG" 2>&1 &
    pid=$!
    echo "$pid" > "$DIR/pid"
    for _ in $(seq 1 60); do
      grep -q "debug channel: watching" "$LOG" 2>/dev/null && break
      sleep 0.1
    done
    echo "magritte up (pid $pid), control dir $DIR"
    ;;

  down)
    kill_instance
    rm -f "$DIR/pid"
    echo "magritte down"
    ;;

  send)
    if [ "$#" -gt 0 ]; then payload="$*"; else payload="$(cat)"; fi
    rm -f "$DIR/done"
    printf '%s' "$payload" > "$DIR/cmd.tmp" && mv "$DIR/cmd.tmp" "$DIR/cmd"
    for _ in $(seq 1 150); do
      [ -f "$DIR/done" ] && break
      sleep 0.1
    done
    if [ -f "$DIR/done" ]; then cat "$DIR/done"; rm -f "$DIR/done"; else echo "(timed out waiting for response)"; fi
    ;;

  key|type|shot|shot-raw|sleep|click|dblclick|rclick|click-id|shift-click|shift-click-id|move|drag|targets)
    exec "$0" send "$cmd $*"
    ;;

  "")
    echo "dbg.sh: no command given" >&2
    echo >&2
    usage >&2
    exit 1
    ;;

  *)
    echo "dbg.sh: unknown command: $cmd" >&2
    echo >&2
    usage >&2
    exit 1
    ;;
esac
