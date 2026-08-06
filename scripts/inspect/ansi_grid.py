#!/usr/bin/env python3
"""Turn a real PTY transcript into a colour-accurate cell grid, then an MVG
paint script that `magick` can rasterize.

Why this exists: scripts/inspect/capture-real.sh wants a picture of what a
*genuine* terminal draws for `macchina`, independent of dvd's and vhs's own
rendering code. On this machine, driving an actual GUI terminal window
(Terminal.app via AppleScript) times out — Apple Events to it never return
in this sandbox — and `magick import` has no screen-capture delegate built
in at all (see the README for both errors verbatim). `script(1)` is the one
of the three suggested tools that works unconditionally: it wraps a real
PTY, a real shell and a real macchina process, and hands back the exact
byte stream — SGR colours, cursor moves, UTF-8 box-drawing glyphs — that a
real terminal emulator would consume to paint its screen.

This script is the "consume" half: a deliberately small VT100/xterm
subset — SGR 256-colour, cursor positioning, clear — just enough to place
each character macchina printed into a (glyph, fg, bg) grid. It does not
shape text, hint glyphs, or antialias; that would make it a fourth
independent renderer competing with dvd's and vhs's, which is not the
point. The point is ground truth for *content and colour*, not pixels: use
it to confirm dvd and vhs put the right coloured character in the right
cell, not to judge font metrics (the real still-image crops do that).
"""

import sys

CSI_FINAL = set("ABCDEFGHJKSTfmnsu")

# xterm's standard 256-colour cube: 16 named colours, a 6x6x6 cube, then a
# 24-step grey ramp. Indices 0-15 use the tape's own Dracula palette instead
# of a generic default, since that is the theme both dvd and vhs were told
# to render this comparison in — this way the transcript panel and the two
# real stills are all answering "did you draw Dracula's colours correctly?"
# rather than three different questions.
DRACULA_16 = [
	0x21222c, 0xff5555, 0x50fa7b, 0xf1fa8c, 0xbd93f9, 0xff79c6, 0x8be9fd, 0xf8f8f2,
	0x6272a4, 0xff6e6e, 0x69ff94, 0xffffa5, 0xd6acff, 0xff92df, 0xa4ffff, 0xffffff,
]


def color256(n: int) -> int:
	if n < 16:
		return DRACULA_16[n]
	if n < 232:
		n -= 16
		r, g, b = n // 36, (n // 6) % 6, n % 6
		scale = lambda v: 0 if v == 0 else 55 + v * 40
		return (scale(r) << 16) | (scale(g) << 8) | scale(b)
	v = 8 + (n - 232) * 10
	return (v << 16) | (v << 8) | v


class Cell:
	__slots__ = ("ch", "fg", "bg")

	def __init__(self, ch=" ", fg=0xF8F8F2, bg=0x282A36):
		self.ch = ch
		self.fg = fg
		self.bg = bg


def parse(data: bytes, cols: int, rows: int):
	grid = [[Cell() for _ in range(cols)] for _ in range(rows)]
	row = col = 0
	fg, bg = 0xF8F8F2, 0x282A36
	i, n = 0, len(data)
	text = data.decode("utf-8", errors="replace")
	i, n = 0, len(text)

	def clamp():
		nonlocal row, col
		row = max(0, min(rows - 1, row))
		col = max(0, min(cols - 1, col))

	while i < n:
		c = text[i]
		if c == "\x1b" and i + 1 < n and text[i + 1] == "[":
			j = i + 2
			while j < n and text[j] not in CSI_FINAL:
				j += 1
			if j >= n:
				break
			params_str, final = text[i + 2 : j], text[j]
			params = [p for p in params_str.split(";")]
			nums = [int(p) if p.isdigit() else 0 for p in params]
			if final == "m":
				k = 0
				if not nums:
					nums = [0]
				while k < len(nums):
					code = nums[k]
					if code == 0:
						fg, bg = 0xF8F8F2, 0x282A36
					elif code == 39:
						fg = 0xF8F8F2
					elif code == 49:
						bg = 0x282A36
					elif code == 38 and k + 2 < len(nums) and nums[k + 1] == 5:
						fg = color256(nums[k + 2])
						k += 2
					elif code == 48 and k + 2 < len(nums) and nums[k + 1] == 5:
						bg = color256(nums[k + 2])
						k += 2
					elif 30 <= code <= 37:
						fg = DRACULA_16[code - 30]
					elif 40 <= code <= 47:
						bg = DRACULA_16[code - 40]
					elif 90 <= code <= 97:
						fg = DRACULA_16[8 + code - 90]
					elif 100 <= code <= 107:
						bg = DRACULA_16[8 + code - 100]
					k += 1
			elif final in ("H", "f"):
				r = nums[0] - 1 if len(nums) > 0 and nums[0] else 0
				cc = nums[1] - 1 if len(nums) > 1 and nums[1] else 0
				row, col = r, cc
				clamp()
			elif final == "J":
				mode = nums[0] if nums else 0
				if mode in (2, 3):
					for rr in range(rows):
						for cc in range(cols):
							grid[rr][cc] = Cell(" ", fg, bg)
			elif final == "K":
				mode = nums[0] if nums else 0
				if mode == 0:
					for cc in range(col, cols):
						grid[row][cc] = Cell(" ", fg, bg)
				elif mode == 1:
					for cc in range(0, col + 1):
						grid[row][cc] = Cell(" ", fg, bg)
				else:
					for cc in range(cols):
						grid[row][cc] = Cell(" ", fg, bg)
			# Cursor-move (A/B/C/D), save/restore (s/u) and status reports (n)
			# do not affect the painted grid, so they are silently consumed.
			i = j + 1
			continue
		if c == "\x1b" and i + 1 < n and text[i + 1] == "]":
			# OSC (e.g. window-title) — skip to its BEL or ST terminator.
			j = i + 2
			while j < n and text[j] not in ("\x07",):
				if text[j] == "\x1b" and j + 1 < n and text[j + 1] == "\\":
					j += 1
					break
				j += 1
			i = j + 1
			continue
		if c == "\r":
			col = 0
			i += 1
			continue
		if c == "\n":
			row += 1
			col = 0
			clamp()
			i += 1
			continue
		if c in ("\x08",):
			col = max(0, col - 1)
			i += 1
			continue
		if ord(c) < 0x20:
			i += 1
			continue
		if 0 <= row < rows and 0 <= col < cols:
			grid[row][col] = Cell(c, fg, bg)
		col += 1
		if col >= cols:
			col = 0
			row = min(rows - 1, row + 1)
		i += 1
	return grid


def mvg_escape(s: str) -> str:
	return s.replace("\\", "\\\\").replace("'", "\\'")


def emit_mvg(grid, cell_w: float, cell_h: float, pad_x: float, pad_y: float, out):
	rows, cols = len(grid), len(grid[0])
	out.write("push graphic-context\n")
	# Background runs, then text runs — same two-pass order paint() uses in
	# dvd-render, so this toy renderer is at least structurally honest about
	# "fills before glyphs" even though it shares none of the real code.
	for r in range(rows):
		c = 0
		while c < cols:
			bg = grid[r][c].bg
			start = c
			while c < cols and grid[r][c].bg == bg:
				c += 1
			x1 = pad_x + start * cell_w
			x2 = pad_x + c * cell_w
			y1 = pad_y + r * cell_h
			y2 = pad_y + (r + 1) * cell_h
			out.write(f"fill '#{bg:06x}'\n")
			out.write(f"rectangle {x1:.1f},{y1:.1f} {x2:.1f},{y2:.1f}\n")
	for r in range(rows):
		c = 0
		while c < cols:
			fg = grid[r][c].fg
			start = c
			run = []
			while c < cols and grid[r][c].fg == fg and grid[r][c].ch != " ":
				run.append(grid[r][c].ch)
				c += 1
			if run:
				text = mvg_escape("".join(run))
				x = pad_x + start * cell_w
				y = pad_y + r * cell_h + cell_h * 0.78
				out.write(f"fill '#{fg:06x}'\n")
				out.write(f"text {x:.1f},{y:.1f} '{text}'\n")
			else:
				c += 1
	out.write("pop graphic-context\n")


def main():
	typescript, cols, rows, cell_w, cell_h, pad_x, pad_y, out_path = sys.argv[1:9]
	with open(typescript, "rb") as f:
		data = f.read()
	grid = parse(data, int(cols), int(rows))
	with open(out_path, "w") as out:
		emit_mvg(grid, float(cell_w), float(cell_h), float(pad_x), float(pad_y), out)


if __name__ == "__main__":
	main()
