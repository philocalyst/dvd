# The VHS twin of ../tests/tapes/macchina.dvd. Read that file's header first
# — this one only exists so `scripts/inspect/compare.sh` can hold every
# setting fixed and let the renderer be the sole difference between the two
# recordings. `Output` is intentionally omitted: the comparison script always
# passes `-o` itself so both tools write into the same gitignored scratch
# directory, and a tape with no `Output` line is exactly what `vhs validate`
# and a bare `vhs -o … macchina.vhs.tape` both accept.

Set Shell "bash"
Set FontFamily "Menlo"
Set FontSize 18
Set Width 1200
Set Height 700
Set Padding 24
Set Theme "Dracula"
Set Framerate 30
Set TypingSpeed 40ms
# `true`, matching the dvd twin — not because VHS has the same problem, but
# so the two tapes stay textually identical settings (see the comment in
# macchina.dvd: `dvd`'s twin no longer *needs* `true`, a past bug there has
# since been fixed, but it stays `true` here too for the same reason it stays
# `true` there — textual parity between the two tapes). VHS never wrote
# terminal-mode escapes through the PTY input path, so it never had this bug.
Set CursorBlink true

Require macchina

# Same startup-race guard as the dvd twin.
Sleep 500ms

Type "macchina"
Enter

# VHS's `Wait` also blocks on the shell prompt reappearing, so this stays a
# faithful twin of `dvd`'s `Wait` above — but the two tools disagree on the
# regex and default timeout that decides "the prompt is back": `dvd` uses
# `Set WaitPattern`/`Set WaitTimeout` (default `[\$>❯#]\s*$`, 30s); VHS's
# equivalents are its own `Set WaitTimeout` (default 5s, no user-visible
# pattern override before this VHS version). Neither is set here, so each
# tool waits on its own built-in prompt heuristic — a difference in the
# harness, not in what gets drawn once the wait is satisfied.
Wait

# Same settle beat as the dvd tape, so neither still is captured mid-redraw.
Sleep 2s

# Settings `dvd` accepts but never draws (see the comment on
# `Settings::apply` in crates/dvd/src/burn.rs): LetterSpacing, LoopOffset,
# WindowBar, WindowBarSize. None are set here either, so both recordings are
# plain, chrome-free rectangles and the omission is not a source of drift.
#
# Settings VHS has that `dvd`'s tape language has no equivalent for at all:
# `Set BorderRadius`/`Set Margin`/`Set MarginFill` *do* exist in both
# languages (and are left at their shared default of "off" here), but VHS
# additionally offers a genuine window-bar renderer (`Set WindowBar Rings` et
# al.) that actually paints title-bar chrome — `dvd` parses the setting and
# silently drops it. Also unmatched: VHS's `Set LoopOffset` controls where a
# GIF's loop seam sits, which has no meaning for `dvd`'s PNG/SVG/MP4 outputs.
