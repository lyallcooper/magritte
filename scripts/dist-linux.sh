#!/usr/bin/env bash
# Build a Linux tarball for Magritte. This is intentionally simpler than the
# macOS bundle: a binary, a .desktop file, and an optional icon.
#
# Usage:
#   scripts/dist-linux.sh [version]
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="${1:-$(awk -F '"' '/^version =/ { print $2; exit }' "$ROOT/crates/magritte/Cargo.toml")}"
HOST="$(rustc -vV | awk '/^host:/ { print $2 }')"
TARGET="${MAGRITTE_TARGET:-$HOST}"
OUT_DIR="$ROOT/target/dist/linux"
ARCHIVE="$ROOT/target/dist/magritte-v$VERSION-$TARGET.tar.gz"

rustup target add "$TARGET" >/dev/null
cargo build --release -p magritte --target "$TARGET"

rm -rf "$OUT_DIR"
mkdir -p \
  "$OUT_DIR/bin" \
  "$OUT_DIR/share/applications" \
  "$OUT_DIR/share/icons/hicolor/256x256/apps"

cp "$ROOT/target/$TARGET/release/magritte" "$OUT_DIR/bin/magritte"
chmod 755 "$OUT_DIR/bin/magritte"
cp "$ROOT/packaging/linux/magritte.desktop" "$OUT_DIR/share/applications/magritte.desktop"

if [ -f "$ROOT/packaging/linux/magritte.png" ]; then
  cp "$ROOT/packaging/linux/magritte.png" \
    "$OUT_DIR/share/icons/hicolor/256x256/apps/magritte.png"
else
  echo "dist-linux: warning: packaging/linux/magritte.png missing; package will not include an icon" >&2
fi

tar -czf "$ARCHIVE" -C "$OUT_DIR" .
shasum -a 256 "$ARCHIVE" | tee "$ARCHIVE.sha256"
echo "Archived $ARCHIVE"
