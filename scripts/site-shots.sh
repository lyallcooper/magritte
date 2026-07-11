#!/usr/bin/env bash
# Retake the website's status-view screenshots (site/public/screenshots/).
#
# Stages a demo repository (a local clone of this repo with a curated set of
# unstaged/staged/untracked changes), drives a debug-capture build through
# scripts/dbg.sh, and captures the status view in light and dark at two sizes:
#
#   status-{light,dark}.png         730pt window, default font; cropped to 702pt
#   status-{light,dark}-mobile.png  640pt window, font_size 14, Recent commits
#                                   collapsed; cropped to 560pt
#
# Captures are shot-raw (2x device pixels). The crop heights and window widths
# are load-bearing: site/src/pages/index.astro hardcodes the <img> dimensions
# (730x702) and sizes the CSS traffic-light dots in cqw against each capture's
# natural width (730 desktop / 640 mobile). Change one, change the other.
#
# The visible demo edit below is written against the current tree; when its
# target snippet disappears as the code evolves, the stage step fails loudly --
# pick a new small, real-looking edit whose longest line still fits the mobile
# window (~66 columns at font 14), and whose surrounding context lines are
# short too.
#
# Requires: git, python3 with Pillow, node with site/node_modules installed
# (sharp encodes the AVIF variants), and a display (the app opens a window).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$ROOT/site/public/screenshots"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/magritte-site-shots.XXXXXX")"
DEMO="$WORK/magritte"
export MAGRITTE_DEBUG_DIR="$WORK/dbg"
DBG="$ROOT/scripts/dbg.sh"
trap '"$DBG" down >/dev/null 2>&1 || true; rm -rf "$WORK"' EXIT

echo "Staging demo repo in $DEMO"
git clone -q "$ROOT" "$DEMO"
cd "$DEMO"

# Unstaged, expanded in every shot: a small real refactor with short lines.
python3 - <<'EOF'
p = 'crates/magritte/src/staging.rs'
s = open(p).read()
old = """            if let Some(&lang) = self.langs.get(key) {
                next.insert(key.clone(), rehighlight(diff, lang));
            }"""
new = """            let Some(&lang) = self.langs.get(key) else {
                continue;
            };
            next.insert(key.clone(), rehighlight(diff, lang));"""
assert old in s, "demo edit target not found; update scripts/site-shots.sh"
open(p, 'w').write(s.replace(old, new))
EOF
# Second unstaged file and the staged file stay collapsed: any change works.
printf '\n<!-- site-shots demo edit -->\n' >> docs/config.md
printf '\n// site-shots demo edit\n' >> crates/magritte/src/theme.rs
git add crates/magritte/src/theme.rs
printf '# Notes\n\n- try the mergetool flow on the conflict from #142\n' > docs/notes.md

mkdir -p .git/magritte
place_window() { # width [height]
  printf 'mode = "windowed"\nx = 149.0\ny = 130.0\nwidth = %s.0\nheight = %s.0\n' \
    "$1" "${2:-770}" > .git/magritte/window.toml
}
# The themes are pinned: the site's CSS palette is Selenized, and the page
# blends into the screenshot's background. The font is NOT pinned -- captures
# use your global config's font, so they may differ across machines.
set_config() { # appearance [font_size]
  {
    printf 'appearance = "%s"\n' "$1"
    printf 'light_theme = "Selenized Light"\ndark_theme = "Selenized Dark"\n'
    if [ $# -gt 1 ] && [ -n "$2" ]; then printf 'font_size = %s\n' "$2"; fi
  } > .git/magritte/config.toml
}

# Fresh fold state (sections expanded, files collapsed), cursor on the first
# row; three `j` reach the staging.rs file row, Tab unfolds its diff.
open_on_staging_rs() {
  rm -f .git/magritte/folds.toml
  (cd "$ROOT" && "$DBG" up "$DEMO") >/dev/null
  "$DBG" sleep 1200 >/dev/null
  for _ in 1 2 3; do "$DBG" key j >/dev/null; done
  "$DBG" key tab >/dev/null
  "$DBG" sleep 800 >/dev/null
}
shoot_pair() { # basename  -- light is already showing; flips to dark after
  "$DBG" sleep 400 >/dev/null
  "$DBG" shot-raw "$WORK/$1-light.png" >/dev/null
  set_config dark "${FONT_ARGS[@]:-}"
  "$DBG" sleep 1500 >/dev/null
  "$DBG" shot-raw "$WORK/$1-dark.png" >/dev/null
  "$DBG" down >/dev/null
}

echo "Capturing desktop pair (730pt)"
place_window 730
set_config light
FONT_ARGS=()
open_on_staging_rs
"$DBG" move 360 745 >/dev/null # park the pointer off the rows and the ? button
shoot_pair desk

echo "Capturing mobile pair (640pt, font 14, Recent commits folded)"
place_window 640
set_config light 14
FONT_ARGS=(14)
open_on_staging_rs
# Collapse Recent commits without coordinates: bottom row, parent section, Tab;
# then back to the staging.rs row.
"$DBG" key shift-g >/dev/null
"$DBG" key ^ >/dev/null
"$DBG" key tab >/dev/null
"$DBG" key g >/dev/null && "$DBG" key g >/dev/null
for _ in 1 2 3; do "$DBG" key j >/dev/null; done
"$DBG" move 320 700 >/dev/null
shoot_pair mobile

# The tag transient, in a short window so the bottom-anchored menu sits close
# to the status content. The pointer parks on the blank row between the
# Untracked and Unstaged sections (blank rows take no hover wash).
open_tag_transient() {
  rm -f .git/magritte/folds.toml
  (cd "$ROOT" && "$DBG" up "$DEMO") >/dev/null
  "$DBG" sleep 1200 >/dev/null
  "$DBG" move 600 105 >/dev/null
  "$DBG" key t >/dev/null
  "$DBG" sleep 500 >/dev/null
}

echo "Capturing tag transient, desktop pair (730pt)"
place_window 730 500
set_config light
FONT_ARGS=()
open_tag_transient
shoot_pair tag-desk

echo "Capturing tag transient, mobile pair (640pt, font 14)"
place_window 640 500
set_config light 14
FONT_ARGS=(14)
rm -f .git/magritte/folds.toml
(cd "$ROOT" && "$DBG" up "$DEMO") >/dev/null
"$DBG" sleep 1200 >/dev/null
# The narrow window can't fit any commit rows above the menu without slicing
# one at the panel edge; fold Recent commits instead.
"$DBG" key shift-g >/dev/null
"$DBG" key ^ >/dev/null
"$DBG" key tab >/dev/null
"$DBG" key g >/dev/null && "$DBG" key g >/dev/null
"$DBG" move 600 105 >/dev/null
"$DBG" key t >/dev/null
"$DBG" sleep 500 >/dev/null
shoot_pair tag-mobile

echo "Cropping into $OUT"
python3 - "$WORK" "$OUT" <<'EOF'
import sys
from PIL import Image
work, out = sys.argv[1], sys.argv[2]
for src, dst, height in (
    ('desk-light', 'status-light', 1404),
    ('desk-dark', 'status-dark', 1404),
    ('mobile-light', 'status-light-mobile', 1120),
    ('mobile-dark', 'status-dark-mobile', 1120),
    ('tag-desk-light', 'tag-light', 1000),
    ('tag-desk-dark', 'tag-dark', 1000),
    ('tag-mobile-light', 'tag-light-mobile', 1000),
    ('tag-mobile-dark', 'tag-dark-mobile', 1000),
):
    img = Image.open(f'{work}/{src}.png').convert('RGB')
    img.crop((0, 0, img.width, height)).save(f'{out}/{dst}.png', optimize=True)
    print(f'  {dst}.png {img.width}x{height}')
EOF

# Lossy AVIF variants (~3x smaller); the PNGs stay as the <picture> fallback.
echo "Encoding AVIF variants"
(cd "$ROOT/site" && node - <<'EOF'
const sharp = require('sharp');
const fs = require('fs');
const dir = 'public/screenshots';
(async () => {
  for (const f of fs.readdirSync(dir).filter((f) => f.endsWith('.png'))) {
    const out = `${dir}/${f.replace('.png', '.avif')}`;
    await sharp(`${dir}/${f}`).avif({ quality: 60 }).toFile(out);
    console.log(`  ${out} ${(fs.statSync(out).size / 1024).toFixed(0)}KB`);
  }
})();
EOF
)

echo "Done. Eyeball the captures (composition, no hover wash, nothing clipped),"
echo "then rebuild the site: cd site && npm run build"
