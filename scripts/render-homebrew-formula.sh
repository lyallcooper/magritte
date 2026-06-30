#!/usr/bin/env bash
# Render the binary Homebrew formula from release artifacts in target/dist/.
#
# Usage:
#   scripts/render-homebrew-formula.sh [version] [github-owner/repo] [output]
#
# Set MAGRITTE_DOWNLOAD_REPOSITORY=owner/repo when the public binary artifacts
# live somewhere other than the source/homepage repository (for example a public
# Homebrew tap repo while the source repo remains private).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="${1:-$(awk -F '"' '/^version =/ { print $2; exit }' "$ROOT/crates/magritte/Cargo.toml")}"
REPOSITORY="${2:-${GITHUB_REPOSITORY:-}}"
OUT="${3:-$ROOT/target/dist/magritte.rb}"
MACOS_TARGET="${MAGRITTE_MACOS_TARGET:-aarch64-apple-darwin}"
LINUX_TARGET="${MAGRITTE_LINUX_TARGET:-x86_64-unknown-linux-gnu}"

if [ -z "$REPOSITORY" ]; then
  origin="$(git -C "$ROOT" config --get remote.origin.url || true)"
  REPOSITORY="$(printf '%s' "$origin" \
    | sed -E 's#^git@github.com:#github.com/#; s#^https://github.com/##; s#^github.com/##; s#\.git$##')"
fi

if [ -z "$REPOSITORY" ]; then
  echo "render-homebrew-formula: pass github-owner/repo or set GITHUB_REPOSITORY" >&2
  exit 1
fi
DOWNLOAD_REPOSITORY="${MAGRITTE_DOWNLOAD_REPOSITORY:-$REPOSITORY}"

macos_archive="magritte-v$VERSION-$MACOS_TARGET.tar.gz"
macos_sha_file="$ROOT/target/dist/$macos_archive.sha256"
if [ ! -f "$macos_sha_file" ]; then
  echo "render-homebrew-formula: missing $macos_sha_file" >&2
  exit 1
fi
macos_sha="$(awk '{ print $1; exit }' "$macos_sha_file")"
macos_url="https://github.com/$DOWNLOAD_REPOSITORY/releases/download/v$VERSION/$macos_archive"

linux_archive="magritte-v$VERSION-$LINUX_TARGET.tar.gz"
linux_sha_file="$ROOT/target/dist/$linux_archive.sha256"
linux_sha=""
linux_url=""
if [ -f "$linux_sha_file" ]; then
  linux_sha="$(awk '{ print $1; exit }' "$linux_sha_file")"
  linux_url="https://github.com/$DOWNLOAD_REPOSITORY/releases/download/v$VERSION/$linux_archive"
fi

mkdir -p "$(dirname "$OUT")"
python3 - "$ROOT/packaging/homebrew/magritte.rb.template" "$OUT" <<PY
from pathlib import Path
import sys

template = Path(sys.argv[1]).read_text()
text = (template
    .replace("@GITHUB_REPOSITORY@", "$REPOSITORY")
    .replace("@VERSION@", "$VERSION")
    .replace("@MACOS_ARM64_URL@", "$macos_url")
    .replace("@MACOS_ARM64_SHA256@", "$macos_sha"))

if "$linux_sha":
    text = (text
        .replace("@LINUX_X86_64_URL@", "$linux_url")
        .replace("@LINUX_X86_64_SHA256@", "$linux_sha"))
else:
    start = text.index("  on_linux do\n")
    end = text.index("\n  end\n\n  def install", start) + len("\n  end")
    text = text[:start] + "  on_linux do\n    odie \"Magritte does not yet ship a Linux artifact for this release\"\n  end" + text[end:]

Path(sys.argv[2]).write_text(text)
PY

echo "Rendered $OUT"
