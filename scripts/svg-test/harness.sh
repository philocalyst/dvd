#!/bin/bash
set -euo pipefail

RESVG="${1:-/nix/store/n09hr8blsad50lnpmy3jd47s9dr960hi-resvg-0.47.0/bin/resvg}"
PYTHON_VENV="/private/tmp/dvdwork/venv"
ANALYZE_SCRIPT="/private/tmp/dvdwork/analyze.py"

if [ ! -x "$RESVG" ]; then
	echo "Error: resvg not found at $RESVG"
	exit 1
fi

OUT_DIR="/tmp/svg-harness"
mkdir -p "$OUT_DIR"

# Get repo root
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.."; pwd)"
FONTS_DIR="$REPO_ROOT/fonts"
BIN="$REPO_ROOT/target/debug/dvd"

if [ ! -x "$BIN" ]; then
	echo "Building binary..."
	cargo build -p dvd >/dev/null 2>&1
fi

# Create stress tapes
create_tapes() {
	mkdir -p "$OUT_DIR/tapes"
	
	# Box drawing
	cat > "$OUT_DIR/tapes/box-drawing.dvd" << 'TAPE'
Set Width 800
Set Height 300
Set FontSize 18
Output "/tmp/svg-harness/box-drawing.svg"
Output "/tmp/svg-harness/box-drawing.png"
Type "Box: ┌─┬─┐ ├─┼─┤ └─┴─┘ ║═╬╝"
Enter
Sleep 1s
TAPE

	# CJK wide characters
	cat > "$OUT_DIR/tapes/cjk.dvd" << 'TAPE'
Set Width 800
Set Height 300
Set FontSize 18
Output "/tmp/svg-harness/cjk.svg"
Output "/tmp/svg-harness/cjk.png"
Type "CJK: 漢字 中文 日本語 한글"
Enter
Sleep 1s
TAPE

	# RTL Arabic + LTR mix
	cat > "$OUT_DIR/tapes/rtl.dvd" << 'TAPE'
Set Width 800
Set Height 300
Set FontSize 18
Output "/tmp/svg-harness/rtl.svg"
Output "/tmp/svg-harness/rtl.png"
Type "RTL: مرحبا hello بالعالم world"
Enter
Sleep 1s
TAPE

	# Bold/Italic/Underline
	cat > "$OUT_DIR/tapes/decorations.dvd" << 'TAPE'
Set Width 800
Set Height 300
Set FontSize 18
Output "/tmp/svg-harness/decorations.svg"
Output "/tmp/svg-harness/decorations.png"
Type "Normal bold italic underline"
Enter
Sleep 1s
TAPE

	# Nerd Font icons
	cat > "$OUT_DIR/tapes/nerd-icons.dvd" << 'TAPE'
Set Width 800
Set Height 300
Set FontSize 18
Output "/tmp/svg-harness/nerd-icons.svg"
Output "/tmp/svg-harness/nerd-icons.png"
Type "Icons:      "
Enter
Sleep 1s
TAPE
}

run_test() {
	local tape="$1"
	local name="$(basename "$tape" .dvd)"
	
	echo "Testing $name..."
	
	# Burn to PNG and SVG
	"$BIN" burn "$tape" "$OUT_DIR/${name}.mp4" 2>&1 >/dev/null || true
	
	if [ ! -f "$OUT_DIR/${name}.png" ] || [ ! -f "$OUT_DIR/${name}.svg" ]; then
		echo "  FAIL: PNG or SVG not generated"
		return 1
	fi
	
	# Rasterize SVG with resvg using bundled fonts
	SVG_RASTERIZED="$OUT_DIR/${name}-svg-rasterized.png"
	"$RESVG" --use-fonts-dir "$FONTS_DIR" --skip-system-fonts \
		"$OUT_DIR/${name}.svg" "$SVG_RASTERIZED" 2>&1 || true
	
	if [ ! -f "$SVG_RASTERIZED" ]; then
		echo "  FAIL: SVG rasterization failed"
		return 1
	fi
	
	# Diff using Python
	if [ -f "$ANALYZE_SCRIPT" ] && [ -d "$PYTHON_VENV" ]; then
		DIFF_IMG="$OUT_DIR/${name}-diff.png"
		source "$PYTHON_VENV/bin/activate"
		python3 "$ANALYZE_SCRIPT" "$OUT_DIR/${name}.png" "$SVG_RASTERIZED" "$DIFF_IMG" 2>&1 || true
		deactivate
		echo "  Diff image: $DIFF_IMG"
	else
		echo "  WARNING: Python harness not available, skipping pixel diff"
	fi
}

# Main
echo "Creating stress tapes..."
create_tapes

echo ""
echo "Running tests..."
for tape in "$OUT_DIR"/tapes/*.dvd; do
	run_test "$tape"
done

echo ""
echo "Results in: $OUT_DIR"
