#!/usr/bin/env bash
# Run vhs on the twin tape and pull one comparable still.
#
# Usage: burn-vhs.sh [tape-file] [output-basename]
#   tape-file         defaults to tests/tapes/macchina.vhs.tape
#   output-basename   defaults to "vhs"; produces $OUT/<basename>.gif and
#                     $OUT/<basename>-still.png
#
# Needs: vhs, ffmpeg. vhs itself drives a headless Chromium (via ttyd) that
# it manages internally — nothing extra to install for that here, but the
# first run can be slow while vhs's own tooling warms up.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./lib.sh

require vhs ffmpeg

TAPE="${1:-$TAPE_VHS}"
BASENAME="${2:-vhs}"
[[ -f "$TAPE" ]] || die "tape not found: $TAPE"

# vhs emits GIF, never MP4 — there is no `Output foo.mp4` in the vhs tape
# language at all for this comparison's purposes (it can target mp4 too, but
# GIF is the format actually reachable without an `Output` line, and the tape
# deliberately omits one so this script controls the destination). Handling
# a GIF source is the one thing this script does that burn-dvd.sh does not.
GIF="$OUT/$BASENAME.gif"
STILL="$OUT/$BASENAME-still.png"

# On this machine `bash` on $PATH is Oils (`osh`), a from-scratch
# bash-compatible shell installed by a Nix/home-manager overlay ahead of
# Apple's real bash — `bash --version` prints "Oils 0.37.0". vhs launches
# the tape's `Set Shell "bash"` with `--noprofile --norc`, flags Oils
# doesn't recognise, so the child shell prints "osh usage error: got
# invalid flag '--noprofile'" and exits instead of ever showing a prompt —
# which is exactly why `Wait` timed out with an empty capture the first
# time this ran. That is an artifact of this machine's shell, not a vhs or
# dvd bug, so the fix belongs here (in the environment vhs's child process
# inherits), not in the tape: put Apple's real bash at /bin first on PATH
# for this one invocation.
log "running vhs on $TAPE -> $GIF"
# vhs narrates its own tape (echoing every `Set`/`Type`/`Wait`) to stdout;
# redirected to stderr so it doesn't land in a caller's
# `STILL="$(./burn-vhs.sh)"` capture alongside this script's own final
# `echo "$STILL"`, which must be the only stdout line.
( cd "$ROOT" && PATH="/bin:$PATH" BASH_SILENCE_DEPRECATION_WARNING=1 vhs -o "$GIF" "$TAPE" 1>&2 ) \
	|| die "vhs failed on $TAPE"
[[ -s "$GIF" ]] || die "vhs produced an empty or missing $GIF"

# Same settled-last-frame logic as burn-dvd.sh, and for the same reason: a
# GIF's own timing can hold its last frame on screen for a while (both
# twins close on `Sleep 2s`), so decode-and-keep-overwriting is more robust
# than seeking to a guessed timestamp.
log "decoding $GIF for its settled final frame -> $STILL"
ffmpeg -y -v error -i "$GIF" -update 1 -frames:v 9999 "$STILL" \
	|| die "ffmpeg failed to extract a still from $GIF"
[[ -s "$STILL" ]] || die "ffmpeg produced an empty or missing $STILL"

echo "$STILL"
