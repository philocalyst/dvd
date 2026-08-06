#!/usr/bin/env bash
# Burn a .dvd tape with our own renderer and pull one still frame from it.
#
# Usage: burn-dvd.sh [tape-file] [output-basename]
#   tape-file         defaults to tests/tapes/macchina.dvd
#   output-basename   defaults to "dvd"; produces $OUT/<basename>.mp4 and
#                     $OUT/<basename>-still.png
#
# Needs: cargo (to build the dvd binary), ffmpeg.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./lib.sh

require cargo ffmpeg

TAPE="${1:-$TAPE_DVD}"
BASENAME="${2:-dvd}"
[[ -f "$TAPE" ]] || die "tape not found: $TAPE"

MP4="$OUT/$BASENAME.mp4"
STILL="$OUT/$BASENAME-still.png"

# The Rust core is under active rewrite elsewhere in this tree; a transient
# build failure here is that work-in-progress, not a bug in this script. We
# still fail loudly rather than silently reusing a stale binary, since a
# stale binary would quietly invalidate the whole comparison.
log "building dvd (release)…"
( cd "$ROOT" && cargo build --release --bin dvd --quiet ) \
	|| die "cargo build failed — if the Rust core is mid-rewrite, retry once it settles; this script is not the bug"

DVD_BIN="$ROOT/target/release/dvd"
[[ -x "$DVD_BIN" ]] || die "expected dvd binary at $DVD_BIN after a successful build"

# Same fix as burn-vhs.sh, for the same reason: on this machine `bash` on
# $PATH is Oils (`osh`), not Apple's real bash. dvd tolerates it — it
# doesn't pass `--noprofile`/`--norc`, so osh starts fine and the burn
# succeeds — but the still it produces then shows an "osh-0.37$" prompt
# instead of a real bash one, which is a difference in *shell identity*,
# not in rendering, and would otherwise sit in every side-by-side and heat
# map as noise unrelated to dvd-vs-vhs fidelity. Forcing real bash here
# keeps the two twins' prompts textually identical, same as burn-vhs.sh.
log "burning $TAPE -> $MP4"
# dvd's own "wrote <path>" goes to stdout; redirected to stderr here so it
# doesn't land in a caller's `STILL="$(./burn-dvd.sh)"` capture alongside
# this script's own final `echo "$STILL"`, which must be the only stdout line.
PATH="/bin:$PATH" BASH_SILENCE_DEPRECATION_WARNING=1 "$DVD_BIN" burn "$TAPE" "$MP4" 1>&2 \
	|| die "dvd burn failed on $TAPE"
[[ -s "$MP4" ]] || die "dvd burn produced an empty or missing $MP4"

# Grab the settled final frame, not a fixed timestamp. Dedup means idle time
# costs no frames at all (see dvd-render's module doc), so the mp4's *last
# encoded frame* can sit seconds before the container's declared duration —
# `ffprobe`-then-seek can land past the last real frame and pull nothing.
# `-update 1` decodes the whole (short) clip and keeps overwriting one PNG,
# so what is left on disk is whatever frame was actually last, which for
# both macchina tapes is the settled prompt during the closing `Sleep 2s`.
log "decoding $MP4 for its settled final frame -> $STILL"
ffmpeg -y -v error -i "$MP4" -update 1 -frames:v 9999 "$STILL" \
	|| die "ffmpeg failed to extract a still from $MP4"
[[ -s "$STILL" ]] || die "ffmpeg produced an empty or missing $STILL"

echo "$STILL"
