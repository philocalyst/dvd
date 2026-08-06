#!/usr/bin/env bash
# The single entry point for W7: run dvd and vhs over twin tapes, capture a
# third, tool-independent reference, and produce the images a human needs to
# judge fidelity. This is what `just inspect` calls. Nothing here is a test —
# there is no assertion in this file, only pictures — see README.md for how
# to read them.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./lib.sh

require magick compare montage python3

log "== 1/4: burning the dvd twin =========================================="
DVD_STILL="$(./burn-dvd.sh)"

log "== 2/4: burning the vhs twin =========================================="
VHS_STILL="$(./burn-vhs.sh)"

log "== 3/4: capturing the genuine-terminal reference ======================"
REAL_STILL="$(./capture-real.sh)"

log "== 4/4: composing the comparison ======================================"

# dvd and vhs were both told `Set Width 1200`/`Set Height 700`, but dvd is
# known not to honour that exactly yet (it rounds the canvas down to a whole
# number of cells — see PLAN.md, workstream W2). Rather than silently
# stretching one image to match the other, which would distort the very
# glyph metrics this tool exists to compare, the two are cropped to their
# shared top-left region for the pixel diff, and the real dimensions are
# printed so the mismatch itself stays visible.
# `-format "%w %h"` has no trailing newline, which makes `read` report EOF
# (exit 1) even after successfully filling both variables — fatal under
# `set -e`. The explicit `\n` avoids that trap.
read -r DVD_W DVD_H < <(magick identify -format "%w %h\n" "$DVD_STILL")
read -r VHS_W VHS_H < <(magick identify -format "%w %h\n" "$VHS_STILL")
if [[ "$DVD_W" != "$VHS_W" || "$DVD_H" != "$VHS_H" ]]; then
	log "NOTE: dvd's still is ${DVD_W}x${DVD_H} but vhs's is ${VHS_W}x${VHS_H}" \
		"(both tapes set Width 1200 / Height 700 — dvd does not honour it exactly yet)."
fi
ALIGN_W=$(( DVD_W < VHS_W ? DVD_W : VHS_W ))
ALIGN_H=$(( DVD_H < VHS_H ? DVD_H : VHS_H ))

DVD_ALIGNED="$OUT/dvd-aligned.png"
VHS_ALIGNED="$OUT/vhs-aligned.png"
magick "$DVD_STILL" -crop "${ALIGN_W}x${ALIGN_H}+0+0" +repage "$DVD_ALIGNED"
magick "$VHS_STILL" -crop "${ALIGN_W}x${ALIGN_H}+0+0" +repage "$VHS_ALIGNED"

# The heat map: every pixel that differs by more than the fuzz tolerance is
# painted the highlight colour over a dimmed copy of the first image. `-fuzz`
# absorbs the antialiasing jitter that is inherent to two different text
# rasterizers (see README) so the map highlights structural differences,
# not sub-pixel AA noise.
HEATMAP="$OUT/diff-heatmap.png"
AE_LOG="$OUT/diff-ae.txt"
set +e
compare -metric AE -fuzz 8% -highlight-color '#ff5555' "$DVD_ALIGNED" "$VHS_ALIGNED" "$HEATMAP" 2>"$AE_LOG"
COMPARE_STATUS=$?
set -e
# compare's exit code is 0 (identical), 1 (differences found — expected and
# normal here), or 2 (a real failure, e.g. mismatched image geometry). Only
# 2 is this script's problem.
if [[ $COMPARE_STATUS -eq 2 ]]; then
	die "compare failed outright: $(cat "$AE_LOG")"
fi
AE_COUNT="$(cat "$AE_LOG")"
log "AE (differing pixel count @ 8% fuzz): $AE_COUNT / $((ALIGN_W * ALIGN_H)) total"

# A second, narrower heat map over one static-content region only — the
# "Kernel"/"Machine" rows, which report this Mac's fixed kernel version and
# model rather than a live figure — so a reviewer can tell at a glance
# whether a red pixel means "rendering differs" or just "macchina printed a
# different uptime this run". See README for why this split matters.
STATIC_RECT="300x120+390+90"
DVD_STATIC="$OUT/dvd-static-region.png"
VHS_STATIC="$OUT/vhs-static-region.png"
magick "$DVD_ALIGNED" -crop "$STATIC_RECT" +repage "$DVD_STATIC"
magick "$VHS_ALIGNED" -crop "$STATIC_RECT" +repage "$VHS_STATIC"
STATIC_HEATMAP="$OUT/diff-heatmap-static-region.png"
STATIC_AE_LOG="$OUT/diff-ae-static-region.txt"
set +e
compare -metric AE -fuzz 8% -highlight-color '#ff5555' "$DVD_STATIC" "$VHS_STATIC" "$STATIC_HEATMAP" 2>"$STATIC_AE_LOG"
set -e
log "AE over the static (non-live-stat) region only: $(cat "$STATIC_AE_LOG")"

# A 4x nearest-neighbour zoom of that same static region, dvd next to vhs,
# so glyph shapes and box-drawing joints can be judged directly rather than
# inferred from the heat map. `-filter point` is deliberate: any smoothing
# filter here would be showing a resize algorithm's opinion of the glyphs,
# not the glyphs dvd and vhs actually drew.
DVD_ZOOM="$OUT/dvd-zoom4x.png"
VHS_ZOOM="$OUT/vhs-zoom4x.png"
magick "$DVD_STATIC" -filter point -resize 400% "$DVD_ZOOM"
magick "$VHS_STATIC" -filter point -resize 400% "$VHS_ZOOM"
ZOOM_MONTAGE="$OUT/zoom4x-montage.png"
montage -font "$MENLO_FONT" -pointsize 20 -fill white -background '#191a21' \
	-label 'dvd (4x)' "$DVD_ZOOM" -label 'vhs (4x)' "$VHS_ZOOM" \
	-tile 2x1 -geometry +6+6 "$ZOOM_MONTAGE"

# The labelled side-by-side: dvd, vhs, the genuine-terminal reference, and
# the heat map, in one image so a reviewer opens exactly one file first.
# Short, single-line labels only — montage overflows a tile's label band
# with anything longer, and the overflow is drawn in the wrong place on the
# composite rather than wrapped. The full story for each panel lives in
# README.md, not crammed in here.
MAIN_MONTAGE="$OUT/montage.png"
montage -font "$MENLO_FONT" -pointsize 16 -fill white -background '#191a21' \
	-label 'dvd burn' "$DVD_STILL" \
	-label 'vhs burn' "$VHS_STILL" \
	-label 'genuine terminal (see README)' "$REAL_STILL" \
	-label 'AE heat map, red = differs (see README)' "$HEATMAP" \
	-tile 2x2 -geometry 600x350+10+50 \
	"$MAIN_MONTAGE"

log ""
log "done. Open these:"
log "  side-by-side + heat map ......... $MAIN_MONTAGE"
log "  dvd still ........................ $DVD_STILL"
log "  vhs still ........................ $VHS_STILL"
log "  genuine-terminal reference ....... $REAL_STILL"
log "  full-frame heat map .............. $HEATMAP"
log "  static-region heat map ........... $STATIC_HEATMAP"
log "  4x glyph-level crop, dvd vs vhs ... $ZOOM_MONTAGE"
log "See scripts/inspect/README.md for how to read all of these."
