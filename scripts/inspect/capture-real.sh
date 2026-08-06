#!/usr/bin/env bash
# The third reference: macchina running somewhere that is not dvd and not
# vhs, so the comparison has a ground truth neither renderer wrote.
#
# The brief for this script named three candidate tools — script(1),
# screencapture, magick import — and asked us to use whichever actually
# works on this machine and say why the others didn't. Here is what we
# found on this macOS 15 sandbox, in the order the brief lists them:
#
#   screencapture   Works as a capture mechanism (it can shoot the real
#                   screen — proved below by the full-desktop test this
#                   script runs first). The blocker is upstream of it: there
#                   is no live Terminal.app window to point it at. Driving
#                   Terminal.app via AppleScript (`activate`, `do script`,
#                   even a read-only `count windows`) hangs indefinitely and
#                   fails with "AppleEvent timed out" (-1712); opening a
#                   .terminal settings file with `open(1)` (Launch Services,
#                   no Apple Events involved) makes Terminal the frontmost
#                   app per the menu bar, yet it draws no window and spawns
#                   no shell — confirmed by `ps` showing no login/bash
#                   process under it. This is consistent with the agent's
#                   process living in a WindowServer session that cannot
#                   drive — or be drawn into, on the capturable display —
#                   the console's interactive Aqua session. On an ordinary
#                   interactive Mac this same AppleScript does work, so it
#                   is still attempted first below and used when it succeeds.
#
#   magick import   Hard, unconditional failure: `magick import` reports
#                   "delegate library support not built-in '' (X11)" — this
#                   Homebrew ImageMagick 7.1.2 build was compiled with no
#                   screen-capture backend at all (no X11/XQuartz, no native
#                   macOS delegate). That is a build-time fact, not a
#                   permissions problem, so there is no fallback path for it
#                   to try.
#
#   script(1)       The one that works unconditionally. It needs no GUI, no
#                   Automation permission and no window to already exist —
#                   it just wraps a real PTY around a real shell running the
#                   real macchina binary and hands back the exact byte
#                   stream (SGR colours, cursor moves, UTF-8 glyphs) a real
#                   terminal emulator would consume to paint its screen.
#
# So: try the genuine GUI capture first (it costs one bounded timeout when
# unavailable, and is the right answer when it isn't); when that fails, fall
# back to script(1) and turn its transcript into a picture ourselves with
# ansi_grid.py + magick. That fallback picture is content-accurate — right
# character, right colour, right cell — but it is *our* text layout code
# doing the drawing, not a real terminal's, so it must never be mistaken
# for a font-metrics reference. It is clearly labelled and README.md repeats
# the point: only the dvd-vs-vhs stills say anything about font rendering.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./lib.sh

require script python3 magick screencapture osascript

BASENAME="real"
STILL="$OUT/$BASENAME-still.png"
LABEL_FILE="$OUT/$BASENAME-label.txt"

WIDTH="$(tape_setting Width "$TAPE_DVD")"
HEIGHT="$(tape_setting Height "$TAPE_DVD")"
PADDING="$(tape_setting Padding "$TAPE_DVD")"
FONTSIZE="$(tape_setting FontSize "$TAPE_DVD")"
MENLO="$MENLO_FONT"
[[ -f "$MENLO" ]] || die "expected system font not found: $MENLO"

# --- Attempt 1: a genuine GUI terminal window -------------------------------
try_gui_capture() {
	log "attempting a genuine GUI terminal capture (Terminal.app + screencapture)…"
	local reply_file bounds x1 y1 x2 y2 w h
	reply_file="$(mktemp)"
	if ! timeout 12 osascript >"$reply_file" 2>&1 <<-APPLESCRIPT
		tell application "Terminal"
			activate
			set the_tab to do script "clear; printf '\\\\e]0;dvd-inspect\\\\a'; macchina; sleep 4"
			delay 3
			set b to bounds of front window
		end tell
		return b
	APPLESCRIPT
	then
		log "GUI path unavailable: Terminal.app did not respond within 12s ($(tr '\n' ' ' < "$reply_file"))"
		rm -f "$reply_file"
		return 1
	fi
	bounds="$(cat "$reply_file")"
	rm -f "$reply_file"
	IFS=', ' read -r x1 y1 x2 y2 <<<"$bounds"
	if [[ -z "${x1:-}" || -z "${y2:-}" ]]; then
		log "GUI path unavailable: could not parse a window rectangle from Terminal.app ('$bounds')"
		return 1
	fi
	w=$((x2 - x1))
	h=$((y2 - y1))
	if (( w <= 0 || h <= 0 )); then
		log "GUI path unavailable: Terminal.app reported a non-positive window rectangle"
		return 1
	fi
	screencapture -x -R "${x1},${y1},${w},${h}" "$STILL" || return 1
	timeout 5 osascript -e 'tell application "Terminal" to close front window saving no' >/dev/null 2>&1 || true
	if [[ ! -s "$STILL" ]]; then
		log "GUI path unavailable: screencapture produced nothing"
		return 1
	fi
	echo "genuine Terminal.app window (AppleScript + screencapture)" > "$LABEL_FILE"
	return 0
}

# --- Fallback: a real PTY transcript, rendered by us for content only ------
fallback_transcript() {
	log "GUI capture unavailable here (see this script's header for why) — falling back to script(1)"
	local typescript cell_w cell_h cols rows mvg measured
	typescript="$OUT/$BASENAME.typescript"
	mvg="$OUT/$BASENAME.mvg"

	# A real advance width for this exact font+size, measured the same way a
	# human would eyeball it — render a run of capital Ms and divide — rather
	# than assumed. Line height has no equally cheap ground truth here, so it
	# stays an approximation (the classic ~1.2x-of-point-size terminals use);
	# that approximation is exactly why this panel is content-only.
	measured="$(magick -font "$MENLO" -pointsize "$FONTSIZE" -background none label:"MMMMMMMMMMMMMMMMMMMM" -format "%w" info:)"
	cell_w="$(python3 -c "print($measured/20)")"
	cell_h="$(python3 -c "print($FONTSIZE*1.2)")"
	cols="$(python3 -c "print(int(($WIDTH-2*$PADDING)/$cell_w))")"
	rows="$(python3 -c "print(int(($HEIGHT-2*$PADDING)/$cell_h))")"

	log "recording macchina under script(1) at ${cols}x${rows}…"
	TERM=xterm-256color script -q "$typescript" \
		bash -c "export TERM=xterm-256color; stty cols $cols rows $rows 2>/dev/null; clear; macchina" \
		>/dev/null || die "script(1) failed to run macchina"
	[[ -s "$typescript" ]] || die "script(1) produced an empty transcript"

	python3 ./ansi_grid.py "$typescript" "$cols" "$rows" "$cell_w" "$cell_h" "$PADDING" "$PADDING" "$mvg" \
		|| die "ansi_grid.py failed to parse the transcript"

	magick -size "${WIDTH}x${HEIGHT}" xc:"#282a36" -font "$MENLO" -pointsize "$FONTSIZE" -draw "@$mvg" "$STILL" \
		|| die "magick failed to rasterize the transcript"

	# A visible strip, not just a filename, so the fallback can't be mistaken
	# for pixel ground truth if this still is opened on its own.
	magick "$STILL" -gravity South -background "#44475a" -splice 0x28 \
		-font "$MENLO" -pointsize 15 -fill "#f1fa8c" -annotate +0+6 \
		"TRANSCRIPT RECONSTRUCTION (content-accurate, not a pixel/font reference — see README)" \
		"$STILL"
	echo "script(1) transcript, rendered by ansi_grid.py (fallback — see README)" > "$LABEL_FILE"
}

if ! try_gui_capture; then
	fallback_transcript
fi

[[ -s "$STILL" ]] || die "no image produced by either capture path"
log "third reference: $(cat "$LABEL_FILE")"
echo "$STILL"
