# scripts/inspect — fidelity comparison tooling

This is developer tooling, not a test target. Nothing here is wired into
`cargo test`, `build.rs`, or any `#[test]`. Its entire job is to produce
side-by-side and difference images that a human looks at, to judge whether
`dvd`'s MP4 output is as faithful to a real terminal as `vhs`'s is — and
better.

## What is installed and on PATH

| tool | what for |
| --- | --- |
| `cargo` | building the `dvd` binary |
| `vhs` | burning the VHS twin tape |
| `ffmpeg` | extracting still frames from MP4 and GIF |
| `magick`/`compare`/`montage` | ImageMagick 7 — cropping, diffing, labelling |
| `macchina` | the system-info TUI both tapes render |
| `python3` | the `ansi_grid.py` transcript renderer |
| `jq` | the justfile's `cargo metadata` lookup (not used by these scripts directly) |
| `screencapture` | macOS screen capture (used when a GUI terminal is reachable) |
| `osascript` | driving Terminal.app for the genuine-terminal reference |
| `script` | the POSIX typescript recorder (fallback when GUI capture is unavailable) |

If any are missing, the scripts fail loudly with the tool name rather than
producing an empty image.

## The scripts

### compare.sh — the single entry point

```
just inspect
# or directly:
./scripts/inspect/compare.sh
```

Runs the whole chain into `inspect-out/` (gitignored at the repo root) and
prints the absolute paths of the images to open. The `just inspect` recipe
calls exactly this path.

### burn-dvd.sh

Burns `tests/tapes/macchina.dvd` with our own renderer to an MP4, then
extracts the settled final frame as a PNG still with `ffmpeg`. The settled
last frame is used rather than a fixed timestamp because dedup means idle
time costs no frames at all — seeking to a guessed timestamp can land past
the last real encoded frame.

### burn-vhs.sh

Runs `vhs` on `tests/tapes/macchina.vhs.tape` to produce a GIF, then
extracts the settled final frame as a PNG. VHS emits GIF (never MP4 without
an `Output` line), which is the one thing this script does differently from
`burn-dvd.sh`.

### capture-real.sh

Captures a **third** reference: `macchina` running in a genuine terminal,
independent of both `dvd` and `vhs`. Three capture mechanisms were tried:

1. **`screencapture` + AppleScript** — the preferred path. Drives
   Terminal.app via AppleScript to open a window, run `macchina`, and
   screenshot it. On an ordinary interactive Mac this works. In this
   sandbox it hangs: Apple Events to Terminal.app never return (the agent's
   process lives in a WindowServer session that cannot drive the console's
   interactive Aqua session). The script still tries it first, with a
   12-second timeout, and uses it when it succeeds.

2. **`magick import`** — hard, unconditional failure. The Homebrew
   ImageMagick 7.1.2 build was compiled with no screen-capture backend
   (no X11/XQuartz, no native macOS delegate). It reports
   `delegate library support not built-in '' (X11)`. This is a build-time
   fact, not a permissions problem.

3. **`script(1)`** — the fallback that works unconditionally. It wraps a
   real PTY, a real shell and a real `macchina` process, and hands back the
   exact byte stream (SGR colours, cursor moves, UTF-8 glyphs) a real
   terminal emulator would consume. `ansi_grid.py` then turns that
   transcript into a picture.

   The fallback picture is **content-accurate** — right character, right
   colour, right cell — but it is *our* text layout code doing the drawing,
   not a real terminal's font renderer. It is clearly labelled as a
   "TRANSCRIPT RECONSTRUCTION" and must never be mistaken for a
   font-metrics reference. Only the `dvd`-vs-`vhs` stills say anything
   about font rendering.

### ansi_grid.py

Turns a `script(1)` typescript into a colour-accurate cell grid, then an
MVG paint script that `magick` can rasterize. A deliberately small
VT100/xterm subset — SGR 256-colour, cursor positioning, clear — just enough
to place each character `macchina` printed into a `(glyph, fg, bg)` grid. It
does not shape text, hint glyphs, or antialias; that would make it a fourth
independent renderer competing with `dvd`'s and `vhs`'s. The point is ground
truth for *content and colour*, not pixels.

## How to read the output

Open `montage.png` first — it is a 2x2 labelled grid with:

| tile | what |
| --- | --- |
| top-left | `dvd burn` still |
| top-right | `vhs burn` still |
| bottom-left | genuine-terminal reference (or transcript reconstruction) |
| bottom-right | AE heat map |

### The heat map (`diff-heatmap.png`)

`compare -metric AE -fuzz 8% -highlight-color '#ff5555'` produces a dimmed
copy of the `dvd` still where every pixel that differs from the `vhs` still
by more than 8% is painted red (`#ff5555`). The `AE` (Absolute Error) count
— the number of differing pixels — is printed to stderr and stored in
`diff-ae.txt`.

- A **red pixel** means: this pixel differs between `dvd` and `vhs` by more
  than the fuzz tolerance.
- The 8% fuzz absorbs antialiasing jitter inherent to two different text
  rasterizers, so the map highlights **structural** differences, not
  sub-pixel AA noise.

### The static-region heat map (`diff-heatmap-static-region.png`)

`macchina` prints live uptime, memory and process figures, so the *text*
differs between any two runs. The full-frame heat map will always show red
in those regions — that is expected and does not indicate a rendering
defect.

To separate "macchina printed different numbers" from "the renderer drew
it differently", a second heat map is computed over a **static-content
region** only: the "Kernel" and "Machine" rows, which report this Mac's
fixed kernel version and model rather than a live figure. If the
static-region heat map is mostly black, the rendering is faithful and the
red in the full-frame map is live-stat text difference, not a rendering
bug.

### The 4x zoom (`zoom4x-montage.png`)

A 4x nearest-neighbour zoom of the static region, `dvd` next to `vhs`, so
glyph shapes and box-drawing joints can be judged directly rather than
inferred from the heat map. `-filter point` is deliberate: any smoothing
filter here would show a resize algorithm's opinion of the glyphs, not the
glyphs `dvd` and `vhs` actually drew.

## Expected and acceptable differences

- **Live-stat text** (uptime, memory, processes) will always differ between
  runs. This is why the comparison targets structure, layout, colour
  fidelity, glyph shape and metrics — never byte-exact text.
- **Canvas size mismatch**: `dvd` rounds the canvas down to a whole number
  of cells (a known issue, see PLAN.md §W2), so `dvd`'s still may be a few
  pixels smaller than `vhs`'s. The scripts crop both to their shared
  top-left region for the pixel diff and print the mismatch so it stays
  visible.
- **Antialiasing**: two different rasterizers (`vello_cpu` vs VHS's
  ttyd+chromium) will always differ at the sub-pixel level. The 8% fuzz
  is there to absorb this.

What is **not** acceptable: wrong colours, missing glyphs, broken
box-drawing joints, text in the wrong column, a cursor that does not match.
Those are rendering defects the heat map will expose.
