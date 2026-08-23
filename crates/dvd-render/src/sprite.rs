//! Glyphs the grid draws itself, because no font can draw them correctly.
//!
//! Box drawing, block elements and braille are the one part of a terminal's
//! character set where taking the font's glyph is the *wrong* answer. Every
//! other character is ink that happens to sit inside a cell; these are ink
//! that has to reach exactly to the cell's edge, because the cell next door
//! is drawing the other half of the same line. A horizontal rule is eighty
//! separate glyphs that have to read as one unbroken stroke.
//!
//! A font cannot promise that. Its box glyphs are drawn for its own em box,
//! and this grid's cell is not that box: [`crate::fonts::measure`] takes the
//! width from the advance of `m` rounded up to a whole pixel and the height
//! from ascent-descent-leading scaled by the tape's line height. Nothing ties
//! those two numbers to the proportions the type designer drew `─` and `│`
//! for. The result at recording sizes is the classic ladder of hairline gaps
//! between the segments of a box border, and vertical rules that step
//! sideways by a pixel from one row to the next.
//!
//! So these are drawn rather than shaped, which is what Ghostty does and for
//! the same reason. Geometry is specified against the *unit cell* — see
//! [`PixelRect::fraction`] — and every stroke is snapped to whole pixels on
//! the way out, so an arm that ends at the cell boundary ends exactly there
//! at every font size and joins its neighbour with no seam and no overlap.
//!
//! Underlines are here for the same reason and one more. A font carries at
//! most one underline — a position and a thickness — and SGR asks for five
//! (single, double, curly, dotted, dashed), which no font can supply and
//! which a terminal has always drawn itself.
//!
//! What is deliberately not here: anything a font draws *well*. Powerline
//! separators, Nerd Font icons and every ordinary letter go through shaping
//! as before — they are ink inside a cell, and the bundled symbol face is
//! already the right answer for them.

use rio_vt::crosswords::style::StyleFlags;
use vello_cpu::kurbo::BezPath;

use crate::fonts::Metrics;
use crate::geom::PixelRect;

/// How wide the strokes of a drawn glyph are.
///
/// Derived from the font's own underline thickness rather than from the cell,
/// so a drawn `─` has the same visual weight as the `_` beside it — the two
/// sit next to each other constantly in a table border and disagreeing about
/// weight is immediately visible.
#[derive(Clone, Copy, Debug)]
pub struct Weights {
	pub light: f32,
	pub heavy: f32,
}

impl Weights {
	/// Round to whole pixels here, once, rather than at each stroke. A light
	/// rule that is 1px on one row and 2px on the next — which is what
	/// rounding per stroke gives when the ideal thickness lands near .5 — is
	/// the exact stepping this module exists to remove.
	pub fn from_thickness(underline_thickness: f32) -> Self {
		let light = underline_thickness.round().max(1.0);
		Self {
			light,
			// Heavy is a doubled light rather than a separately rounded
			// multiple, so the two always differ by a visible whole pixel
			// even when the light stroke is already only one.
			heavy: light * 2.0,
		}
	}
}

/// One piece of a drawn glyph.
///
/// `alpha` is a coverage weight, not a colour: the shade blocks are the only
/// producer that uses anything but full, and they need to come out as a tint
/// of the cell's own foreground rather than of some fixed grey.
#[derive(Clone, Debug)]
pub enum Stroke {
	Rect { rect: PixelRect, alpha: u8 },
	Path { path: BezPath, alpha: u8 },
}

impl Stroke {
	fn rect(rect: PixelRect) -> Self {
		Self::Rect { rect, alpha: 0xff }
	}

	fn shaded(rect: PixelRect, alpha: u8) -> Self {
		Self::Rect { rect, alpha }
	}
}

/// Whether this character is one the grid draws for itself.
///
/// Checked before shaping so a drawn character never reaches Parley at all —
/// otherwise the row's text would carry a glyph whose advance the layout
/// accounts for and whose ink the sprite pass draws a second time.
#[inline]
pub fn covers(character: char) -> bool {
	matches!(character, '\u{2500}'..='\u{259f}' | '\u{2800}'..='\u{28ff}')
}

/// Draw `character` into `cell`, appending its strokes to `out`.
///
/// Returns `false` when the character is not one this module draws, leaving
/// `out` untouched — the caller falls back to shaping it.
pub fn draw(character: char, cell: PixelRect, weights: Weights, out: &mut Vec<Stroke>) -> bool {
	match character {
		'\u{2500}'..='\u{257f}' => draw_box(character, cell, weights, out),
		'\u{2580}'..='\u{259f}' => {
			draw_block(character, cell, out);
			true
		}
		'\u{2800}'..='\u{28ff}' => {
			draw_braille(character, cell, out);
			true
		}
		_ => false,
	}
}

// -----------------------------------------------------------------------------
// Underlines — SGR 4, 4:2, 4:3, 4:4, 4:5
// -----------------------------------------------------------------------------

/// Which of the five underlines a cell asks for.
///
/// The VT core tracks all five as distinct flags, and the terminals that
/// matter emit all five: a language server marks a warning with `4:3` and an
/// error with `4:3` in a different colour, and a diff tool marks moved text
/// with `4:5`. Collapsing them all to a single rule — which is what a plain
/// `intersects(ALL_UNDERLINES)` test does — throws away the distinction the
/// program was making.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Underline {
	Single,
	Double,
	Curly,
	Dotted,
	Dashed,
}

impl Underline {
	/// The underline a set of style flags asks for, if any.
	///
	/// Checked most specific first. The flags are not exclusive in the bit
	/// pattern — `UNDERLINE` may be set alongside a more specific kind — so
	/// testing plain single first would swallow every other style.
	pub fn from_flags(flags: StyleFlags) -> Option<Self> {
		if flags.contains(StyleFlags::UNDERCURL) {
			Some(Self::Curly)
		} else if flags.contains(StyleFlags::DOUBLE_UNDERLINE) {
			Some(Self::Double)
		} else if flags.contains(StyleFlags::DOTTED_UNDERLINE) {
			Some(Self::Dotted)
		} else if flags.contains(StyleFlags::DASHED_UNDERLINE) {
			Some(Self::Dashed)
		} else if flags.contains(StyleFlags::UNDERLINE) {
			Some(Self::Single)
		} else {
			None
		}
	}
}

/// Draw one cell's underline.
///
/// The rule is clamped to stay inside the cell. A font whose underline sits
/// far below the baseline — many do, expecting the leading a text engine
/// would add — otherwise puts the rule in the *next* row's cell, where the
/// row below promptly paints its own background over it and the underline
/// disappears on every line but the last.
pub fn underline(
	kind: Underline,
	cell: PixelRect,
	metrics: &Metrics,
	out: &mut Vec<Stroke>,
) {
	let thickness = metrics.underline_thickness.round().max(1.0);
	let ideal_top = cell.y + metrics.underline_offset;
	let top = ideal_top.min(cell.bottom() - thickness).max(cell.y);

	let rule = |top: f32| PixelRect::new(cell.x, top, cell.width, thickness).snapped();

	match kind {
		Underline::Single => out.push(Stroke::rect(rule(top))),
		Underline::Double => {
			// Both rails have to fit between the ideal position and the
			// cell's floor, so the gap shrinks before the strokes do.
			let gap = thickness.max(1.0);
			let lower = (top + thickness + gap).min(cell.bottom() - thickness);
			out.push(Stroke::rect(rule(top)));
			out.push(Stroke::rect(rule(lower)));
		}
		Underline::Dotted | Underline::Dashed => {
			// A dot is as long as the rule is thick; a dash is three times
			// that. Both repeat on twice their own length, which keeps the
			// pattern legible without it reading as a solid line.
			let dot = thickness;
			let (length, pitch) = match kind {
				Underline::Dotted => (dot, dot * 2.0),
				_ => (dot * 3.0, dot * 5.0),
			};

			let mut x = cell.x;
			while x < cell.right() {
				let width = length.min(cell.right() - x);
				out.push(Stroke::rect(
					PixelRect::new(x, top, width, thickness).snapped(),
				));
				x += pitch;
			}
		}
		Underline::Curly => out.push(curl(cell, top, thickness)),
	}
}

/// An undercurl, as a filled ribbon following one full sine period per cell.
///
/// One period per cell rather than per some fixed pixel pitch, so the wave
/// stays continuous across a run of cells: adjacent cells start and end at the
/// same phase, and the ribbon joins with no visible kink at the boundary.
fn curl(cell: PixelRect, top: f32, thickness: f32) -> Stroke {
	// The wave needs vertical room of its own. It is given the space between
	// the rule's position and the cell floor, capped so a generous font
	// metric cannot turn a curl into a zigzag taller than the text above it.
	let room = (cell.bottom() - top - thickness).max(0.0);
	let amplitude = room.min(thickness * 1.5) / 2.0;
	let middle = (top + amplitude) as f64;
	let half = (thickness / 2.0) as f64;
	let amplitude = amplitude as f64;

	let left = cell.x as f64;
	let width = cell.width as f64;
	let middle_x = left + width / 2.0;
	let right = left + width;
	// A cubic per half period. Control points reaching a third of the way
	// across each half approximate a sine closely enough that the error is
	// invisible at a stroke this thin.
	let reach = width / 6.0;

	// The six points of the centre line, lifted by `offset`. The ribbon's two
	// edges are this same curve at `-half` and `+half`: offsetting vertically
	// rather than perpendicular to the tangent makes the stroke very slightly
	// thinner at the steep parts, which at one or two pixels is not a
	// difference anything can see, and it keeps the two edges from ever
	// crossing.
	let curve = |offset: f64| {
		[
			(left, middle + offset),
			(left + reach, middle - amplitude + offset),
			(middle_x - reach, middle - amplitude + offset),
			(middle_x, middle + offset),
			(middle_x + reach, middle + amplitude + offset),
			(right - reach, middle + amplitude + offset),
			(right, middle + offset),
		]
	};

	let upper = curve(-half);
	let lower = curve(half);

	let mut path = BezPath::new();
	path.move_to(upper[0]);
	path.curve_to(upper[1], upper[2], upper[3]);
	path.curve_to(upper[4], upper[5], upper[6]);
	// Down across the stroke at the right-hand end, then back along the lower
	// edge. A reversed cubic swaps its two control points, which is what
	// walking the same curve backwards means.
	path.line_to(lower[6]);
	path.curve_to(lower[5], lower[4], lower[3]);
	path.curve_to(lower[2], lower[1], lower[0]);
	path.close_path();

	Stroke::Path { path, alpha: 0xff }
}

// -----------------------------------------------------------------------------
// Box drawing — U+2500..U+257F
// -----------------------------------------------------------------------------

/// The weight of the stroke leaving a cell by one edge.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Arm {
	None,
	Light,
	Heavy,
	Double,
}

impl Arm {
	fn width(self, weights: Weights) -> f32 {
		match self {
			Arm::None => 0.0,
			Arm::Light => weights.light,
			Arm::Heavy => weights.heavy,
			// A double rule's two rails are each a light stroke, and its
			// overall footprint is three of them — rail, gap, rail.
			Arm::Double => weights.light * 3.0,
		}
	}

	fn from_code(code: u8) -> Self {
		match code {
			b'l' => Arm::Light,
			b'h' => Arm::Heavy,
			b'd' => Arm::Double,
			_ => Arm::None,
		}
	}
}

/// Which edges each box-drawing character reaches, as `up down left right`.
///
/// `.` none, `l` light, `h` heavy, `d` double. `?` marks the characters that
/// are not four arms at all — dashes, arcs and diagonals — which
/// [`draw_box`] routes elsewhere before ever reading this table.
///
/// A table rather than seventy-six match arms because that is what these
/// characters *are*: Unicode lays the block out as every combination of arm
/// weights, and writing the combinations as data makes a missing one visible
/// as a gap in a column rather than invisible as an absent branch.
#[rustfmt::skip]
const ARMS: [&[u8; 4]; 0x80] = [
	b"..ll", // 2500 ─
	b"..hh", // 2501 ━
	b"ll..", // 2502 │
	b"hh..", // 2503 ┃
	b"????", // 2504 ┄ triple dash
	b"????", // 2505 ┅
	b"????", // 2506 ┆
	b"????", // 2507 ┇
	b"????", // 2508 ┈ quadruple dash
	b"????", // 2509 ┉
	b"????", // 250A ┊
	b"????", // 250B ┋
	b".l.l", // 250C ┌
	b".l.h", // 250D ┍
	b".h.l", // 250E ┎
	b".h.h", // 250F ┏
	b".ll.", // 2510 ┐
	b".lh.", // 2511 ┑
	b".hl.", // 2512 ┒
	b".hh.", // 2513 ┓
	b"l..l", // 2514 └
	b"l..h", // 2515 ┕
	b"h..l", // 2516 ┖
	b"h..h", // 2517 ┗
	b"l.l.", // 2518 ┘
	b"l.h.", // 2519 ┙
	b"h.l.", // 251A ┚
	b"h.h.", // 251B ┛
	b"ll.l", // 251C ├
	b"ll.h", // 251D ┝
	b"hl.l", // 251E ┞
	b"lh.l", // 251F ┟
	b"hh.l", // 2520 ┠
	b"hl.h", // 2521 ┡
	b"lh.h", // 2522 ┢
	b"hh.h", // 2523 ┣
	b"lll.", // 2524 ┤
	b"llh.", // 2525 ┥
	b"hll.", // 2526 ┦
	b"lhl.", // 2527 ┧
	b"hhl.", // 2528 ┨
	b"hlh.", // 2529 ┩
	b"lhh.", // 252A ┪
	b"hhh.", // 252B ┫
	b".lll", // 252C ┬
	b".lhl", // 252D ┭
	b".llh", // 252E ┮
	b".lhh", // 252F ┯
	b".hll", // 2530 ┰
	b".hhl", // 2531 ┱
	b".hlh", // 2532 ┲
	b".hhh", // 2533 ┳
	b"l.ll", // 2534 ┴
	b"l.hl", // 2535 ┵
	b"l.lh", // 2536 ┶
	b"l.hh", // 2537 ┷
	b"h.ll", // 2538 ┸
	b"h.hl", // 2539 ┹
	b"h.lh", // 253A ┺
	b"h.hh", // 253B ┻
	b"llll", // 253C ┼
	b"llhl", // 253D ┽
	b"lllh", // 253E ┾
	b"llhh", // 253F ┿
	b"hlll", // 2540 ╀
	b"lhll", // 2541 ╁
	b"hhll", // 2542 ╂
	b"hlhl", // 2543 ╃
	b"hllh", // 2544 ╄
	b"lhhl", // 2545 ╅
	b"lhlh", // 2546 ╆
	b"hlhh", // 2547 ╇
	b"lhhh", // 2548 ╈
	b"hhhl", // 2549 ╉
	b"hhlh", // 254A ╊
	b"hhhh", // 254B ╋
	b"????", // 254C ╌ double dash
	b"????", // 254D ╍
	b"????", // 254E ╎
	b"????", // 254F ╏
	b"..dd", // 2550 ═
	b"dd..", // 2551 ║
	b".l.d", // 2552 ╒
	b".d.l", // 2553 ╓
	b".d.d", // 2554 ╔
	b".ld.", // 2555 ╕
	b".dl.", // 2556 ╖
	b".dd.", // 2557 ╗
	b"l..d", // 2558 ╘
	b"d..l", // 2559 ╙
	b"d..d", // 255A ╚
	b"l.d.", // 255B ╛
	b"d.l.", // 255C ╜
	b"d.d.", // 255D ╝
	b"ll.d", // 255E ╞
	b"dd.l", // 255F ╟
	b"dd.d", // 2560 ╠
	b"lld.", // 2561 ╡
	b"ddl.", // 2562 ╢
	b"ddd.", // 2563 ╣
	b".ldd", // 2564 ╤
	b".dll", // 2565 ╥
	b".ddd", // 2566 ╦
	b"l.dd", // 2567 ╧
	b"d.ll", // 2568 ╨
	b"d.dd", // 2569 ╩
	b"lldd", // 256A ╪
	b"ddll", // 256B ╫
	b"dddd", // 256C ╬
	b"????", // 256D ╭ arc
	b"????", // 256E ╮
	b"????", // 256F ╯
	b"????", // 2570 ╰
	b"????", // 2571 ╱ diagonal
	b"????", // 2572 ╲
	b"????", // 2573 ╳
	b"..l.", // 2574 ╴
	b"l...", // 2575 ╵
	b"...l", // 2576 ╶
	b".l..", // 2577 ╷
	b"..h.", // 2578 ╸
	b"h...", // 2579 ╹
	b"...h", // 257A ╺
	b".h..", // 257B ╻
	b"..lh", // 257C ╼
	b"lh..", // 257D ╽
	b"..hl", // 257E ╾
	b"hl..", // 257F ╿
];

fn draw_box(character: char, cell: PixelRect, weights: Weights, out: &mut Vec<Stroke>) -> bool {
	match character {
		// Dashes: the same rule as the solid character, cut into pieces. The
		// count is in the character's own name — triple, quadruple, double.
		'\u{2504}' | '\u{2505}' => return dashes(cell, weights, character, 3, true, out),
		'\u{2506}' | '\u{2507}' => return dashes(cell, weights, character, 3, false, out),
		'\u{2508}' | '\u{2509}' => return dashes(cell, weights, character, 4, true, out),
		'\u{250a}' | '\u{250b}' => return dashes(cell, weights, character, 4, false, out),
		'\u{254c}' | '\u{254d}' => return dashes(cell, weights, character, 2, true, out),
		'\u{254e}' | '\u{254f}' => return dashes(cell, weights, character, 2, false, out),
		'\u{256d}'..='\u{2570}' => {
			out.push(arc(character, cell, weights));
			return true;
		}
		'\u{2571}' => {
			out.push(diagonal(cell, weights, true));
			return true;
		}
		'\u{2572}' => {
			out.push(diagonal(cell, weights, false));
			return true;
		}
		'\u{2573}' => {
			out.push(diagonal(cell, weights, true));
			out.push(diagonal(cell, weights, false));
			return true;
		}
		_ => {}
	}

	let arms = ARMS[character as usize - 0x2500];
	let [up, down, left, right] = std::array::from_fn(|index| Arm::from_code(arms[index]));

	// The junction is as wide as the widest arm meeting here, so every arm
	// runs into the middle far enough to fill the corner completely. Stopping
	// each arm at the exact centre instead leaves an unpainted notch in the
	// quadrant between two perpendicular strokes.
	let joint_x = [up, down].map(|arm| arm.width(weights)).into_iter().fold(0.0, f32::max);
	let joint_y = [left, right]
		.map(|arm| arm.width(weights))
		.into_iter()
		.fold(0.0, f32::max);

	let centre_x = cell.x + cell.width / 2.0;
	let centre_y = cell.y + cell.height / 2.0;

	// A rule that leaves by both opposite edges at the same weight is one
	// stroke, not two arms that happen to abut. Emitting it as two is not
	// merely wasteful: each is snapped to whole pixels independently, and on
	// an odd-sized cell the two halves round to overlapping or — worse —
	// disjoint rectangles, which is a seam in the middle of every `─`.
	let mut horizontal_arms: &[(Arm, bool)] = &[(left, false), (right, true)];
	let mut vertical_arms: &[(Arm, bool)] = &[(up, false), (down, true)];
	let through_horizontal = [(left, false)];
	let through_vertical = [(up, false)];

	let horizontal_through = left != Arm::None && left == right;
	let vertical_through = up != Arm::None && up == down;
	if horizontal_through {
		horizontal_arms = &through_horizontal;
	}
	if vertical_through {
		vertical_arms = &through_vertical;
	}

	for &(arm, towards_end) in horizontal_arms {
		let (offsets, count) = rails(arm, weights);
		for &offset in &offsets[..count] {
			let y = centre_y + offset - weights.light / 2.0;
			let (x0, x1) = if horizontal_through {
				(cell.x, cell.right())
			} else if towards_end {
				(centre_x - joint_x / 2.0, cell.right())
			} else {
				(cell.x, centre_x + joint_x / 2.0)
			};
			out.push(Stroke::rect(
				PixelRect::new(x0, y, x1 - x0, weights.light).snapped(),
			));
		}
	}

	for &(arm, towards_end) in vertical_arms {
		let (offsets, count) = rails(arm, weights);
		for &offset in &offsets[..count] {
			let x = centre_x + offset - weights.light / 2.0;
			let (y0, y1) = if vertical_through {
				(cell.y, cell.bottom())
			} else if towards_end {
				(centre_y - joint_y / 2.0, cell.bottom())
			} else {
				(cell.y, centre_y + joint_y / 2.0)
			};
			out.push(Stroke::rect(
				PixelRect::new(x, y0, weights.light, y1 - y0).snapped(),
			));
		}
	}

	// A double-ruled junction paints its own rails from all four sides, which
	// leaves the square between them filled by whichever rails overlap — the
	// correct open-centre look for ╬ and the correct closed one for ╔ both
	// fall out of that without a special case.
	true
}

/// The offsets, from the centre line, of the rails a single arm is drawn as,
/// and how many of them are live.
///
/// One rail for a light or heavy rule, two for a double. A fixed array rather
/// than a `Vec` because this is called four times per drawn cell per frame,
/// and a box-heavy TUI is thousands of drawn cells a second.
fn rails(arm: Arm, weights: Weights) -> ([f32; 2], usize) {
	match arm {
		Arm::None => ([0.0; 2], 0),
		Arm::Light => ([0.0, 0.0], 1),
		// A heavy stroke is drawn as two adjacent light rails rather than one
		// thick rect, so it shares the same snapping and lands on the same
		// pixel boundaries as the light strokes it meets.
		Arm::Heavy => ([-weights.light / 2.0, weights.light / 2.0], 2),
		Arm::Double => ([-weights.light, weights.light], 2),
	}
}

fn dashes(
	cell: PixelRect,
	weights: Weights,
	character: char,
	count: u32,
	horizontal: bool,
	out: &mut Vec<Stroke>,
) -> bool {
	// The heavy variant of each dashed character is the odd codepoint of the
	// pair, without exception across all three dash families.
	let width = if character as u32 % 2 == 1 {
		weights.heavy
	} else {
		weights.light
	};

	// Gaps are a third of a dash, which is the proportion the reference
	// glyphs use and the one that still reads as dashed at small sizes.
	let span = if horizontal { cell.width } else { cell.height };
	let pitch = span / count as f32;
	let dash = pitch * 0.75;

	for index in 0..count {
		let start = index as f32 * pitch + (pitch - dash) / 2.0;
		let rect = if horizontal {
			PixelRect::new(
				cell.x + start,
				cell.y + (cell.height - width) / 2.0,
				dash,
				width,
			)
		} else {
			PixelRect::new(
				cell.x + (cell.width - width) / 2.0,
				cell.y + start,
				width,
				dash,
			)
		};
		out.push(Stroke::rect(rect.snapped()));
	}
	true
}

/// One of the four rounded corners, as a quarter ellipse.
///
/// The geometry is easiest to see from the centre of curvature rather than
/// from the corner: `╭` — "arc down and right" — is the top-left corner of a
/// rounded box, so its stroke enters at the middle of the right edge and
/// leaves at the middle of the bottom edge, and the only ellipse through both
/// of those points that meets each edge square-on is the one centred on the
/// cell's *bottom-right* corner with radii of half a cell. The other three
/// characters are the same construction reflected, which is why the whole
/// family reduces to a pair of signs.
///
/// Emitted as a filled outline — inner arc, cap, outer arc, cap — rather than
/// as a stroked centre line, because a fill is the one shape both backends
/// draw identically. `vello_cpu` fills; an SVG `<path>` with no `stroke`
/// fills; neither has to agree about how a stroke is expanded, how it is
/// capped, or whether `vector-effect` applies.
fn arc(character: char, cell: PixelRect, weights: Weights) -> Stroke {
	let centre_x = (cell.x + cell.width / 2.0) as f64;
	let centre_y = (cell.y + cell.height / 2.0) as f64;
	let half = (weights.light / 2.0) as f64;

	// Which edges the two arms leave by, from the character's own name.
	let (to_right, to_bottom) = match character {
		'\u{256d}' => (true, true),   // ╭ arc down and right
		'\u{256e}' => (false, true),  // ╮ arc down and left
		'\u{256f}' => (false, false), // ╯ arc up and left
		_ => (true, false),           // ╰ arc up and right
	};

	// The centre of curvature is the cell corner the two exits share.
	let pivot_x = if to_right { cell.right() } else { cell.x } as f64;
	let pivot_y = if to_bottom { cell.bottom() } else { cell.y } as f64;

	// Unit direction from the pivot back towards the cell centre.
	let toward_x = if to_right { -1.0 } else { 1.0 };
	let toward_y = if to_bottom { -1.0 } else { 1.0 };
	let radius_x = (pivot_x - centre_x).abs();
	let radius_y = (pivot_y - centre_y).abs();

	// The standard cubic approximation of a quarter ellipse. The radial error
	// peaks at roughly a five-thousandth of the radius — far inside a pixel at
	// any size a terminal is recorded at.
	const KAPPA: f64 = 0.552_284_75;

	// A quarter from the point on the vertical exit edge to the point on the
	// horizontal exit edge, at radius `(rx, ry)` about the pivot.
	let quarter = |path: &mut BezPath, rx: f64, ry: f64, reverse: bool| {
		let on_vertical = (pivot_x + toward_x * rx, pivot_y);
		let on_horizontal = (pivot_x, pivot_y + toward_y * ry);
		let handle_v = (on_vertical.0, pivot_y + toward_y * ry * KAPPA);
		let handle_h = (pivot_x + toward_x * rx * KAPPA, on_horizontal.1);

		if reverse {
			path.curve_to(handle_h, handle_v, on_vertical);
		} else {
			path.curve_to(handle_v, handle_h, on_horizontal);
		}
	};

	let mut path = BezPath::new();
	// Start on the inner edge where the stroke crosses the vertical exit.
	path.move_to((pivot_x + toward_x * (radius_x - half), pivot_y));
	quarter(&mut path, radius_x - half, radius_y - half, false);
	// Cap across the stroke at the horizontal exit.
	path.line_to((pivot_x, pivot_y + toward_y * (radius_y + half)));
	quarter(&mut path, radius_x + half, radius_y + half, true);
	path.close_path();

	Stroke::Path { path, alpha: 0xff }
}

fn diagonal(cell: PixelRect, weights: Weights, rising: bool) -> Stroke {
	let half = weights.light as f64 / 2.0;
	// The stroke is widened horizontally rather than perpendicular to the
	// line. At a terminal cell's aspect ratio the difference is under half a
	// pixel, and doing it this way keeps the two ends flush with the cell's
	// top and bottom edges so `╳` and stacked `╱` join cleanly.
	let (top_x, bottom_x) = if rising {
		(cell.right() as f64, cell.x as f64)
	} else {
		(cell.x as f64, cell.right() as f64)
	};

	let mut path = BezPath::new();
	path.move_to((top_x - half, cell.y as f64));
	path.line_to((top_x + half, cell.y as f64));
	path.line_to((bottom_x + half, cell.bottom() as f64));
	path.line_to((bottom_x - half, cell.bottom() as f64));
	path.close_path();

	Stroke::Path { path, alpha: 0xff }
}

// -----------------------------------------------------------------------------
// Block elements — U+2580..U+259F
// -----------------------------------------------------------------------------

/// Coverage of the shade characters, as an alpha rather than as a stipple.
///
/// Ghostty draws these as a real dot pattern. At the sizes a terminal is
/// recorded at a one-pixel stipple aliases badly under any resampling — and
/// an MP4's chroma subsampling is exactly such a resampling — so a flat tint
/// is both closer to what the viewer ends up seeing and stable frame to
/// frame. The three levels are the conventional quarter steps.
const SHADES: [u8; 3] = [0x40, 0x80, 0xc0];

fn draw_block(character: char, cell: PixelRect, out: &mut Vec<Stroke>) {
	const EIGHTH: f32 = 1.0 / 8.0;

	let rect = match character {
		// Upper half, then the lower blocks in eighths.
		'\u{2580}' => cell.fraction(0.0, 0.0, 1.0, 0.5),
		'\u{2581}'..='\u{2587}' => {
			let eighths = (character as u32 - 0x2580) as f32;
			cell.fraction(0.0, 1.0 - eighths * EIGHTH, 1.0, 1.0)
		}
		'\u{2588}' => cell,
		// Left blocks, seven eighths down to one.
		'\u{2589}'..='\u{258f}' => {
			let eighths = 8.0 - (character as u32 - 0x2588) as f32;
			cell.fraction(0.0, 0.0, eighths * EIGHTH, 1.0)
		}
		'\u{2590}' => cell.fraction(0.5, 0.0, 1.0, 1.0),
		'\u{2591}'..='\u{2593}' => {
			out.push(Stroke::shaded(
				cell,
				SHADES[character as usize - 0x2591],
			));
			return;
		}
		'\u{2594}' => cell.fraction(0.0, 0.0, 1.0, EIGHTH),
		'\u{2595}' => cell.fraction(1.0 - EIGHTH, 0.0, 1.0, 1.0),
		// Quadrants. The four corners are a bitmask in the character's
		// position within the block, which is what makes the seven
		// multi-quadrant characters fall out of the same four rectangles.
		'\u{2596}'..='\u{259f}' => {
			const QUADRANTS: [u8; 10] = [
				0b0010, // 2596 ▖ lower left
				0b0001, // 2597 ▗ lower right
				0b1000, // 2598 ▘ upper left
				0b1011, // 2599 ▙ upper left, lower left, lower right
				0b1001, // 259A ▚ upper left, lower right
				0b1110, // 259B ▛ upper left, upper right, lower left
				0b1101, // 259C ▜ upper left, upper right, lower right
				0b0100, // 259D ▝ upper right
				0b0110, // 259E ▞ upper right, lower left
				0b0111, // 259F ▟ upper right, lower left, lower right
			];
			let mask = QUADRANTS[character as usize - 0x2596];
			for (bit, (left, top)) in [
				(0b1000, (0.0, 0.0)),
				(0b0100, (0.5, 0.0)),
				(0b0010, (0.0, 0.5)),
				(0b0001, (0.5, 0.5)),
			] {
				if mask & bit != 0 {
					out.push(Stroke::rect(
						cell.fraction(left, top, left + 0.5, top + 0.5).snapped(),
					));
				}
			}
			return;
		}
		_ => return,
	};

	out.push(Stroke::rect(rect.snapped()));
}

// -----------------------------------------------------------------------------
// Braille — U+2800..U+28FF
// -----------------------------------------------------------------------------

/// Braille as a 2x4 dot matrix.
///
/// Included because plotting TUIs — `btop`, `gping`, `gtop` — draw their
/// graphs entirely out of these, and a font's braille is designed as readable
/// tactile-style dots rather than as plot pixels: the dots are small, widely
/// spaced, and a filled column does not read as a filled column. Drawn here,
/// a fully-set cell is a solid block and a line graph looks like a line.
fn draw_braille(character: char, cell: PixelRect, out: &mut Vec<Stroke>) {
	// The low six bits are the historical 2x3 cell, read down the left column
	// then down the right; the top two bits are the 8-dot extension's fourth
	// row. That discontinuity is the encoding's, not ours.
	const DOTS: [(u8, u8, u8); 8] = [
		(0b0000_0001, 0, 0),
		(0b0000_0010, 0, 1),
		(0b0000_0100, 0, 2),
		(0b0000_1000, 1, 0),
		(0b0001_0000, 1, 1),
		(0b0010_0000, 1, 2),
		(0b0100_0000, 0, 3),
		(0b1000_0000, 1, 3),
	];

	let pattern = (character as u32 - 0x2800) as u8;

	for (bit, column, row) in DOTS {
		if pattern & bit == 0 {
			continue;
		}
		let left = column as f32 / 2.0;
		let top = row as f32 / 4.0;
		out.push(Stroke::rect(
			cell.fraction(left, top, left + 0.5, top + 0.25).snapped(),
		));
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn weights() -> Weights {
		Weights::from_thickness(1.0)
	}

	fn rects(character: char, cell: PixelRect) -> Vec<PixelRect> {
		let mut out = Vec::new();
		assert!(
			draw(character, cell, weights(), &mut out),
			"U+{:04X} should be drawn",
			character as u32
		);
		out.into_iter()
			.filter_map(|stroke| match stroke {
				Stroke::Rect { rect, .. } => Some(rect),
				Stroke::Path { .. } => None,
			})
			.collect()
	}

	/// The property the whole module exists for: two cells side by side must
	/// produce one unbroken rule, with no gap and no double-painted overlap
	/// where they meet.
	#[test]
	fn a_horizontal_rule_joins_across_the_cell_boundary() {
		let left_cell = PixelRect::new(0.0, 0.0, 9.0, 19.0);
		let right_cell = PixelRect::new(9.0, 0.0, 9.0, 19.0);

		let left = rects('─', left_cell);
		let right = rects('─', right_cell);

		assert_eq!(left.len(), 1);
		assert_eq!(right.len(), 1);
		assert_eq!(
			left[0].right(),
			right[0].x,
			"the two segments must meet exactly"
		);
		assert_eq!(left[0].y, right[0].y, "and sit on the same scan line");
		assert_eq!(left[0].height, right[0].height);
	}

	/// The vertical equivalent, across rows. An odd cell height is the case
	/// that catches centring that rounds inconsistently.
	#[test]
	fn a_vertical_rule_joins_across_rows_at_an_odd_cell_height() {
		let top = rects('│', PixelRect::new(0.0, 0.0, 9.0, 19.0));
		let bottom = rects('│', PixelRect::new(0.0, 19.0, 9.0, 19.0));

		assert_eq!(top[0].bottom(), bottom[0].y);
		assert_eq!(top[0].x, bottom[0].x, "and stay in the same column");
		assert_eq!(top[0].width, bottom[0].width);
	}

	/// A corner's arms have to reach the edges it names and no others, or a
	/// border box grows a stub poking out of its own corner.
	#[test]
	fn a_corner_reaches_exactly_the_two_edges_it_names() {
		let cell = PixelRect::new(0.0, 0.0, 10.0, 20.0);
		let corner = rects('┌', cell);

		let touches_right = corner.iter().any(|rect| rect.right() >= cell.right());
		let touches_bottom = corner.iter().any(|rect| rect.bottom() >= cell.bottom());
		let touches_left = corner.iter().any(|rect| rect.x <= cell.x);
		let touches_top = corner.iter().any(|rect| rect.y <= cell.y);

		assert!(touches_right && touches_bottom, "┌ opens down and right");
		assert!(
			!touches_left && !touches_top,
			"┌ must not reach the left or top edge"
		);
	}

	/// The corner quadrant between two perpendicular arms must be painted, or
	/// every junction in a table shows a pinhole.
	#[test]
	fn a_cross_leaves_no_hole_at_its_centre() {
		let cell = PixelRect::new(0.0, 0.0, 10.0, 20.0);
		let arms = rects('┼', cell);

		let centre = (5.0f32, 10.0f32);
		let covered = arms.iter().any(|rect| {
			centre.0 >= rect.x
				&& centre.0 < rect.right()
				&& centre.1 >= rect.y
				&& centre.1 < rect.bottom()
		});
		assert!(covered, "the junction itself must be painted");
	}

	#[test]
	fn a_heavy_rule_is_thicker_than_a_light_one() {
		let cell = PixelRect::new(0.0, 0.0, 10.0, 20.0);
		let light: f32 = rects('─', cell).iter().map(|rect| rect.height).sum();
		let heavy: f32 = rects('━', cell).iter().map(|rect| rect.height).sum();

		assert!(
			heavy > light,
			"heavy {heavy} should outweigh light {light}"
		);
	}

	/// A full block must cover its cell exactly — this is the character a
	/// progress bar is made of, and one unpainted pixel column shows up as a
	/// stripe running the whole length of the bar.
	#[test]
	fn a_full_block_covers_its_whole_cell() {
		let cell = PixelRect::new(3.0, 7.0, 10.0, 20.0);
		let block = rects('█', cell);

		assert_eq!(block.len(), 1);
		assert_eq!(block[0], cell);
	}

	/// The eighth-blocks are how a bar chart shows a fraction of a column, so
	/// the fraction has to be the one the character names, measured from the
	/// bottom.
	#[test]
	fn the_lower_eighth_blocks_grow_upward_by_one_eighth_each() {
		let cell = PixelRect::new(0.0, 0.0, 8.0, 16.0);

		let one_eighth = rects('▁', cell);
		assert_eq!(one_eighth[0].bottom(), cell.bottom());
		assert_eq!(one_eighth[0].height, 2.0);

		let half = rects('▄', cell);
		assert_eq!(half[0].height, 8.0);
		assert_eq!(half[0].bottom(), cell.bottom());

		let seven_eighths = rects('▇', cell);
		assert_eq!(seven_eighths[0].height, 14.0);
	}

	/// Left blocks are the horizontal counterpart and grow from the left.
	#[test]
	fn the_left_blocks_grow_rightward_from_the_left_edge() {
		let cell = PixelRect::new(0.0, 0.0, 16.0, 8.0);

		let one_eighth = rects('▏', cell);
		assert_eq!(one_eighth[0].x, cell.x);
		assert_eq!(one_eighth[0].width, 2.0);

		let half = rects('▌', cell);
		assert_eq!(half[0].width, 8.0);
		assert_eq!(half[0].x, cell.x);
	}

	/// Four quadrants must tile the cell exactly, which is what makes ▙▚▛▜▞▟
	/// line up with the half blocks beside them.
	#[test]
	fn the_quadrants_tile_the_cell_without_gaps() {
		let cell = PixelRect::new(0.0, 0.0, 10.0, 20.0);

		let upper_left = rects('▘', cell);
		assert_eq!(upper_left.len(), 1);
		assert_eq!(upper_left[0], PixelRect::new(0.0, 0.0, 5.0, 10.0));

		let three = rects('▟', cell);
		assert_eq!(three.len(), 3, "▟ is three quadrants");
		let area: f32 = three.iter().map(|rect| rect.width * rect.height).sum();
		assert_eq!(area, cell.width * cell.height * 0.75);
	}

	/// The shades are the one producer that is not fully opaque, and they
	/// have to be ordered — a "dark shade" that came out lighter than the
	/// light one would invert every gradient drawn with them.
	#[test]
	fn the_shades_are_ordered_from_light_to_dark() {
		let cell = PixelRect::new(0.0, 0.0, 10.0, 20.0);
		let alpha = |character| {
			let mut out = Vec::new();
			draw(character, cell, weights(), &mut out);
			match out[0] {
				Stroke::Rect { alpha, .. } => alpha,
				_ => panic!("a shade is a rectangle"),
			}
		};

		assert!(alpha('░') < alpha('▒'));
		assert!(alpha('▒') < alpha('▓'));
		assert!(alpha('▓') < 0xff, "even dark shade is not solid");
	}

	/// Braille is a dot matrix, so an empty pattern draws nothing and a full
	/// one covers the cell — the two ends a plotting TUI relies on.
	#[test]
	fn braille_spans_from_no_dots_to_a_solid_cell() {
		let cell = PixelRect::new(0.0, 0.0, 10.0, 20.0);

		assert!(rects('\u{2800}', cell).is_empty(), "no dots set");

		let full = rects('\u{28ff}', cell);
		assert_eq!(full.len(), 8, "all eight dots");
		let area: f32 = full.iter().map(|rect| rect.width * rect.height).sum();
		assert_eq!(
			area,
			cell.width * cell.height,
			"a fully set cell must be solid"
		);
	}

	/// The dot at each bit position has to land where braille says it does,
	/// or a plotted line comes out mirrored or upside down.
	#[test]
	fn braille_dot_one_is_the_top_left_and_dot_eight_the_bottom_right() {
		let cell = PixelRect::new(0.0, 0.0, 10.0, 20.0);

		let dot_one = rects('\u{2801}', cell);
		assert_eq!(dot_one[0], PixelRect::new(0.0, 0.0, 5.0, 5.0));

		let dot_eight = rects('\u{2880}', cell);
		assert_eq!(dot_eight[0], PixelRect::new(5.0, 15.0, 5.0, 5.0));
	}

	/// Coverage has to agree with what `draw` will actually accept: a
	/// character claimed here but refused there would be shaped as nothing
	/// and disappear from the recording.
	#[test]
	fn everything_covers_claims_is_something_draw_accepts() {
		let cell = PixelRect::new(0.0, 0.0, 10.0, 20.0);
		for code in 0x2500u32..=0x28ff {
			let Some(character) = char::from_u32(code) else {
				continue;
			};
			if !covers(character) {
				continue;
			}
			let mut out = Vec::new();
			assert!(
				draw(character, cell, weights(), &mut out),
				"covers() claims U+{code:04X} but draw() refused it"
			);
			assert!(
				!out.is_empty() || character == '\u{2800}',
				"U+{code:04X} drew nothing at all"
			);
		}
	}

	fn metrics() -> Metrics {
		Metrics {
			cell_width: 10,
			cell_height: 20,
			baseline: 15.0,
			underline_offset: 17.0,
			underline_thickness: 1.0,
			strikeout_offset: 10.0,
			strikeout_thickness: 1.0,
		}
	}

	fn underline_strokes(kind: Underline, cell: PixelRect, metrics: &Metrics) -> Vec<Stroke> {
		let mut out = Vec::new();
		underline(kind, cell, metrics, &mut out);
		out
	}

	/// The five SGR underlines have to be five different pictures. Before
	/// this module they were all a single rule, so a curly "there is an error
	/// here" and a plain "this is a link" came out identical.
	#[test]
	fn each_underline_style_is_distinguishable_from_the_others() {
		let cell = PixelRect::new(0.0, 0.0, 20.0, 20.0);
		let metrics = metrics();

		let single = underline_strokes(Underline::Single, cell, &metrics);
		let double = underline_strokes(Underline::Double, cell, &metrics);
		let dotted = underline_strokes(Underline::Dotted, cell, &metrics);
		let dashed = underline_strokes(Underline::Dashed, cell, &metrics);
		let curly = underline_strokes(Underline::Curly, cell, &metrics);

		assert_eq!(single.len(), 1, "one continuous rule");
		assert_eq!(double.len(), 2, "two stacked rules");
		assert!(dotted.len() > dashed.len(), "dots repeat more often than dashes");
		assert!(dashed.len() > 1, "a dash pattern is more than one piece");
		assert!(
			matches!(curly.as_slice(), [Stroke::Path { .. }]),
			"a curl is a single path, not a rectangle"
		);
	}

	/// A font whose underline sits below its own descender would otherwise
	/// put the rule in the next row's cell, where that row paints its
	/// background straight over it — and every underline but the last one on
	/// screen silently disappears.
	#[test]
	fn an_underline_below_the_cell_is_pulled_back_inside_it() {
		let cell = PixelRect::new(0.0, 40.0, 10.0, 20.0);
		let generous = Metrics {
			underline_offset: 26.0, // six pixels past the bottom of the cell
			..metrics()
		};

		let strokes = underline_strokes(Underline::Single, cell, &generous);
		let Stroke::Rect { rect, .. } = strokes[0] else {
			panic!("a single underline is a rectangle");
		};

		assert!(
			rect.bottom() <= cell.bottom(),
			"the rule must stay inside its own cell, got {rect:?} in {cell:?}"
		);
		assert!(rect.y >= cell.y);
	}

	/// Both rails of a double underline have to fit too — the lower one is
	/// the one that falls out of the cell first.
	#[test]
	fn both_rails_of_a_double_underline_stay_inside_the_cell() {
		let cell = PixelRect::new(0.0, 0.0, 10.0, 20.0);
		let tight = Metrics {
			underline_offset: 18.0,
			..metrics()
		};

		for stroke in underline_strokes(Underline::Double, cell, &tight) {
			let Stroke::Rect { rect, .. } = stroke else {
				panic!("a double underline is two rectangles");
			};
			assert!(
				rect.bottom() <= cell.bottom(),
				"rail {rect:?} escaped {cell:?}"
			);
		}
	}

	/// The flags are not mutually exclusive in the bit pattern, so the most
	/// specific style has to win — testing plain single first would swallow
	/// every other kind.
	#[test]
	fn a_specific_underline_style_wins_over_the_plain_flag() {
		let curly = StyleFlags::UNDERLINE | StyleFlags::UNDERCURL;
		assert_eq!(Underline::from_flags(curly), Some(Underline::Curly));

		let dotted = StyleFlags::UNDERLINE | StyleFlags::DOTTED_UNDERLINE;
		assert_eq!(Underline::from_flags(dotted), Some(Underline::Dotted));

		assert_eq!(
			Underline::from_flags(StyleFlags::UNDERLINE),
			Some(Underline::Single)
		);
		assert_eq!(Underline::from_flags(StyleFlags::BOLD), None);
	}

	/// A dotted rule must actually cover the cell it belongs to, or a run of
	/// underlined text comes out with a bare gap at every cell boundary.
	#[test]
	fn a_dotted_underline_spans_the_whole_cell() {
		let cell = PixelRect::new(5.0, 0.0, 20.0, 20.0);
		let strokes = underline_strokes(Underline::Dotted, cell, &metrics());

		let first = match strokes.first() {
			Some(Stroke::Rect { rect, .. }) => *rect,
			_ => panic!("dots are rectangles"),
		};
		let last = match strokes.last() {
			Some(Stroke::Rect { rect, .. }) => *rect,
			_ => panic!("dots are rectangles"),
		};

		assert_eq!(first.x, cell.x, "the pattern starts at the cell's edge");
		assert!(
			last.right() <= cell.right(),
			"and no dot overruns into the next cell"
		);
	}

	/// Nothing outside the two blocks may be claimed — an ordinary letter
	/// routed into the sprite pass would vanish.
	#[test]
	fn ordinary_characters_are_left_to_the_font() {
		for character in ['a', 'Z', '0', ' ', '│' as char, '\u{e0b0}', '\u{4e00}'] {
			if ('\u{2500}'..='\u{259f}').contains(&character) {
				continue;
			}
			assert!(
				!covers(character),
				"U+{:04X} should be shaped, not drawn",
				character as u32
			);
		}
	}
}
