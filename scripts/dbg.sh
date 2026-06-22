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
#   scripts/dbg.sh targets            list clickable element ids and their center points
#   scripts/dbg.sh click-id <id>      click a clickable element by id (no coordinate guessing)
#   scripts/dbg.sh click <x> <y>      click at window-relative point (points, matches shot pixels)
#   scripts/dbg.sh sleep <ms>         pause (let a frame paint)
#
# Override the control dir with MAGRITTE_DEBUG_DIR (default /tmp/magritte-debug).
set -euo pipefail

DIR="${MAGRITTE_DEBUG_DIR:-/tmp/magritte-debug}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug/magritte"
LOG="/tmp/magritte-debug.log"

cmd="${1:-}"
shift || true

case "$cmd" in
  up)
    repo="${1:-$PWD}"
    pkill -f "target/debug/magritte" 2>/dev/null || true
    sleep 0.3
    rm -rf "$DIR"; mkdir -p "$DIR"
    MAGRITTE_DEBUG_DIR="$DIR" "$BIN" "$repo" >"$LOG" 2>&1 &
    pid=$!
    for _ in $(seq 1 60); do
      grep -q "debug: watching" "$LOG" 2>/dev/null && break
      sleep 0.1
    done
    echo "magritte up (pid $pid), control dir $DIR"
    ;;

  down)
    pkill -f "target/debug/magritte" 2>/dev/null || true
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

  key|type|shot|sleep|click|click-id|targets)
    exec "$0" send "$cmd $*"
    ;;

  *)
    sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'
    exit 1
    ;;
esac
