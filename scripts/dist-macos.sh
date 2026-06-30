#!/usr/bin/env bash
# Build a local macOS .app bundle for Magritte.
#
# Usage:
#   scripts/dist-macos.sh [version]
#
# The app is ad-hoc signed and archived under target/dist/. An icon is optional
# for now: put packaging/macos/Magritte.icns in place and the script will bundle
# it; otherwise Finder/Dock will show a generic app icon.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="${1:-$(awk -F '"' '/^version =/ { print $2; exit }' "$ROOT/crates/magritte/Cargo.toml")}"
HOST="$(rustc -vV | awk '/^host:/ { print $2 }')"
TARGET="${MAGRITTE_TARGET:-$HOST}"
OUT_DIR="$ROOT/target/dist/macos"
APP="$OUT_DIR/Magritte.app"
CONTENTS="$APP/Contents"
MACOS="$CONTENTS/MacOS"
RESOURCES="$CONTENTS/Resources"
ARCHIVE="$ROOT/target/dist/magritte-v$VERSION-$TARGET.tar.gz"

rustup target add "$TARGET" >/dev/null
cargo build --release -p magritte --target "$TARGET"

rm -rf "$APP"
mkdir -p "$MACOS" "$RESOURCES"
cp "$ROOT/target/$TARGET/release/magritte" "$MACOS/magritte"
chmod 755 "$MACOS/magritte"
sed "s/@VERSION@/$VERSION/g" \
  "$ROOT/packaging/macos/Info.plist.template" > "$CONTENTS/Info.plist"

if [ -f "$ROOT/packaging/macos/Magritte.icns" ]; then
  cp "$ROOT/packaging/macos/Magritte.icns" "$RESOURCES/Magritte.icns"
else
  echo "dist-macos: warning: packaging/macos/Magritte.icns missing; app will use a generic icon" >&2
fi

codesign --force --deep --sign - "$APP"
tar -czf "$ARCHIVE" -C "$OUT_DIR" Magritte.app
shasum -a 256 "$ARCHIVE" | tee "$ARCHIVE.sha256"
echo "Built $APP"
echo "Archived $ARCHIVE"
