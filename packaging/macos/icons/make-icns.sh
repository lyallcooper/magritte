#!/usr/bin/env bash
# Turn the full-bleed source art in ./src into macOS app icons.
#
# For each src/<id>.png this writes crates/magritte/icons/<id>.png -- the styled
# 1024 master the app embeds for the in-app icon switcher (set as the running
# Dock icon). The default variant is also packed into packaging/macos/Magritte.icns,
# the bundle's CFBundleIconFile (the Finder icon; macOS can't switch that at
# runtime, so only the default needs an .icns). To change the default, edit
# DEFAULT below and rerun.
#
# The treatment matches Apple's macOS icon grid: the art is scaled to an 824px
# body on the 1024 canvas (~100px margin), masked to the Big Sur continuous
# rounded rectangle (straight sides, rounded corners -- not a bulging
# superellipse), and given a subtle contact shadow. Rerun after changing art:
#   packaging/macos/icons/make-icns.sh
#
# Requires ImageMagick (`magick`) and macOS `iconutil`.
set -euo pipefail

DEFAULT=pipe        # the variant used for the bundle's Finder icon
CANVAS=1024
BODY=824            # ~80% of the canvas, per Apple's grid
RADIUS=185          # corner radius for the 824 body (~0.225, the Big Sur curve)
SS=2                # supersample factor for a smooth mask edge

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
SRC_DIR="$ROOT/packaging/macos/icons/src"
ICNS_DIR="$ROOT/packaging/macos"
MASTER_DIR="$ROOT/crates/magritte/icons"
mkdir -p "$MASTER_DIR"

style_master() {  # src -> styled 1024 master
  local src="$1" out="$2" tmp
  tmp="$(mktemp -d)"
  local big=$((BODY * SS)) r=$((RADIUS * SS))
  # Art scaled to the rounded-rect body.
  magick "$src" -resize ${BODY}x${BODY}^ -gravity center -extent ${BODY}x${BODY} "$tmp/body.png"
  # Rounded-rect mask, supersampled then downscaled for an antialiased edge.
  magick -size ${big}x${big} xc:none -fill white \
    -draw "roundrectangle 0,0 $((big - 1)),$((big - 1)) $r,$r" \
    -resize ${BODY}x${BODY} "$tmp/mask.png"
  magick "$tmp/body.png" "$tmp/mask.png" -alpha off -compose CopyOpacity -composite "$tmp/rounded.png"
  # Compose on the canvas with a subtle contact shadow.
  magick -size ${CANVAS}x${CANVAS} xc:none \
    \( "$tmp/rounded.png" -background black -shadow 28x14+0+10 \) -gravity center -geometry +0+4 -composite \
    "$tmp/rounded.png" -gravity center -geometry +0+0 -composite \
    "$out"
  rm -rf "$tmp"
}

pack_icns() {  # styled 1024 master -> .icns
  local master="$1" out="$2" tmp iconset px name
  tmp="$(mktemp -d)"
  iconset="$tmp/icon.iconset"
  mkdir -p "$iconset"
  for pair in "16 16x16" "32 16x16@2x" "32 32x32" "64 32x32@2x" \
              "128 128x128" "256 128x128@2x" "256 256x256" "512 256x256@2x" \
              "512 512x512" "1024 512x512@2x"; do
    read -r px name <<<"$pair"
    magick "$master" -resize ${px}x${px} "$iconset/icon_${name}.png"
  done
  iconutil -c icns "$iconset" -o "$out"
  rm -rf "$tmp"
}

for src in "$SRC_DIR"/*.png; do
  id="$(basename "$src" .png)"
  style_master "$src" "$MASTER_DIR/$id.png"
  echo "styled $id"
done
pack_icns "$MASTER_DIR/$DEFAULT.png" "$ICNS_DIR/Magritte.icns"
echo "default ($DEFAULT) -> Magritte.icns"
