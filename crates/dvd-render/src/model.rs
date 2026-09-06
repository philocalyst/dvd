//! What one captured frame is.
//!
//! A [`Snapshot`] is a whole terminal screen frozen: the packed cells, the
//! style table they index into, the out-of-line extras (combining marks and
//! hyperlinks), the cursor, and any images placed over or under the text.
//!
//! Cells are kept exactly as `rio-vt` packs them — one `u64` each — rather than
//! being expanded into a wide per-cell struct. That is what lets the dedup
//! comparison be a straight `u64` scan over the whole screen instead of a
//! field-by-field walk, and it keeps a snapshot of an 80x24 screen under 16 KiB.
//! Resolving a cell to colours and attributes is deferred to the renderer,
//! which does it once per *distinct* frame rather than once per captured tick.

use anyhow::Context;
use rio_vt::config::colors::{AnsiColor, ColorRgb, Colors, NamedColor, term::List};
use rio_vt::crosswords::grid::row::Row;
use rio_vt::crosswords::pos::CursorState;
use rio_vt::crosswords::square::{Extras, Square, Wide};
use rio_vt::crosswords::style::{Style, StyleFlags};
use rustc_hash::FxHashMap;

use crate::geom::Color;
use crate::grid::GridOptions;
use crate::source::TerminalTheme;

/// An image decoded from Sixel, iTerm2 or the Kitty graphics protocol, placed
/// somewhere on the screen.
///
/// Pixel geometry rather than cell geometry, because neither protocol is
/// cell-aligned: a Kitty placement carries a sub-cell offset and may ask to be
/// shown at native pixel size, and a Sixel region gets sliced into arbitrary
/// sub-rectangles as the grid scrolls underneath it.
#[derive(Clone, Debug)]
pub struct Placement {
	/// Destination rect in pixels, relative to the top-left of the grid.
	/// Signed, because a placement scrolled off the top is clipped by the
	/// renderer rather than dropped here.
	pub x: i32,
	pub y: i32,
	pub width: u32,
	pub height: u32,

	/// The crop within `image`, in source pixels.
	pub source_x: u32,
	pub source_y: u32,
	pub source_width: u32,
	pub source_height: u32,

	/// Paint order. Negative draws beneath the text — Kitty's `z=` semantics —
	/// and non-negative draws over it. Ties break on insertion order.
	pub z: i32,

	/// The decoded image. Shared, so an unchanged graphic held across a
	/// thousand frames stays one allocation and compares by pointer.
	pub image: std::sync::Arc<Image>,
}

/// A decoded image, RGBA8, ready to composite.
#[derive(Debug)]
pub struct Image {
	pub width: u32,
	pub height: u32,
	pub rgba: Vec<u8>,
}

impl Placement {
	/// Whether two placements would paint the same pixels in the same place.
	///
	/// The image compares by pointer, not by content. Decoded pixels are
	/// deliberately shared across frames so that an unchanged graphic stays one
	/// allocation, and memcmp-ing a full-screen Sixel every tick would cost
	/// more than the render the comparison exists to avoid. Two equal images in
	/// separate allocations merely miss the optimisation; they never compare
	/// equal when they should not.
	pub fn same_paint(&self, other: &Self) -> bool {
		self.x == other.x
			&& self.y == other.y
			&& self.width == other.width
			&& self.height == other.height
			&& self.source_x == other.source_x
			&& self.source_y == other.source_y
			&& self.source_width == other.source_width
			&& self.source_height == other.source_height
			&& self.z == other.z
			&& std::sync::Arc::ptr_eq(&self.image, &other.image)
	}
}

/// One terminal screen, frozen.
#[derive(Debug)]
pub struct Snapshot {
	pub columns: u16,
	pub screen_rows: u16,

	/// Row-major, `len == columns * screen_rows`.
	pub cells: Vec<Square>,

	/// Indexed by [`Square::style_id`].
	pub styles: Vec<Style>,

	/// Combining marks and hyperlinks, keyed by [`Square::extras_id`].
	pub extras: FxHashMap<u16, Extras>,

	pub cursor: CursorState,
	pub cursor_visible: bool,

	/// Sorted by `z`, so the renderer can split under-text from over-text with
	/// one partition point instead of two passes.
	pub graphics: Vec<Placement>,

	/// Capture scratch: the row structures `rio-vt` fills, kept between
	/// captures so an incremental snapshot has somewhere to write only the
	/// dirty rows. [`Snapshot::flatten`] folds them into `cells`.
	///
	/// Public because [`crate::session`] fills it, but it is not part of what a
	/// frame *is* — everything downstream reads `cells`.
	pub rows: Vec<Row<Square>>,
}

impl Snapshot {
	pub fn new(columns: u16, screen_rows: u16) -> Self {
		Self {
			columns,
			screen_rows,
			cells: vec![Square::default(); columns as usize * screen_rows as usize],
			styles: Vec::new(),
			extras: FxHashMap::default(),
			cursor: CursorState::new(' '),
			cursor_visible: false,
			graphics: Vec::new(),
			rows: Vec::new(),
		}
	}

	/// Fold the capture scratch into the packed cell vector.
	///
	/// Copying rather than borrowing the rows is what makes a snapshot a value:
	/// it can be shared across encoder threads by `Arc` with no lock and no
	/// lifetime tied back to the live terminal, which is still being written to
	/// by the PTY reader. The copy is a `memcpy` per row of 8-byte cells.
	pub fn flatten(&mut self) {
		let width = self.columns as usize;
		self.cells.clear();
		self.cells.reserve(width * self.rows.len());

		for row in &self.rows {
			let cells = &row.inner[..width.min(row.inner.len())];
			self.cells.extend_from_slice(cells);
			// A row narrower than the grid (mid-resize) pads with blanks so
			// `cells` stays exactly `columns * screen_rows` and indexing by
			// `row * columns + column` cannot walk into the next row.
			self.cells.resize(
				self.cells.len() + width.saturating_sub(cells.len()),
				Square::default(),
			);
		}

		self.screen_rows = self.rows.len() as u16;
	}

	#[inline]
	pub fn cell(&self, column: u16, row: u16) -> Square {
		self.cells[row as usize * self.columns as usize + column as usize]
	}

	/// The packed cells as plain `u64`s, for the dedup kernels.
	///
	/// `Square` is declared `#[repr(transparent)]` around a `u64`, so a slice of
	/// one is a slice of the other: same length, same alignment, same bytes.
	#[inline]
	pub fn raw_cells(&self) -> &[u64] {
		// SAFETY: `rio_vt::crosswords::square::Square` is `#[repr(transparent)]`
		// over `u64`, which guarantees identical size, alignment and layout. The
		// resulting slice borrows `self` and has the same element count, so it
		// cannot outlive or overrun the source.
		unsafe { std::slice::from_raw_parts(self.cells.as_ptr().cast::<u64>(), self.cells.len()) }
	}

	/// Whether this frame would paint the same picture as `other`.
	///
	/// Everything the renderer reads is compared, so equality here means the
	/// two are interchangeable on screen.
	pub fn same_picture(&self, other: &Self) -> bool {
		self.columns == other.columns
			&& self.screen_rows == other.screen_rows
			&& self.cursor_visible == other.cursor_visible
			&& self.cursor.pos == other.cursor.pos
			&& self.cursor.content == other.cursor.content
			&& self.styles == other.styles
			&& self.extras == other.extras
			&& self.graphics.len() == other.graphics.len()
			&& std::iter::zip(&self.graphics, &other.graphics).all(|(a, b)| a.same_paint(b))
			&& self.cells == other.cells
	}

	/// Empty every field without giving up the allocations, ready to be
	/// refilled by the next capture.
	pub fn clear(&mut self) {
		self.cells.clear();
		self.styles.clear();
		self.extras.clear();
		self.graphics.clear();
		self.rows.clear();
	}
}

/// How a cell resolves once its style has been applied.
///
/// Produced by [`Palette::resolve`], which is where inverse, dim, hidden and
/// the 256-colour cube all get folded down to two concrete RGBA values — so
/// nothing downstream has to know the difference between `AnsiColor::Named`,
/// `Indexed` and `Spec`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedCell {
	pub character: char,
	pub foreground: Color,
	pub background: Color,
	/// Explicit underline colour (SGR 58), when the cell sets one.
	pub underline: Option<Color>,
	pub flags: StyleFlags,
	pub wide: Wide,
}

/// The 256-colour table plus the named defaults, wrapping `rio-vt`'s own list.
///
/// Wrapped rather than reimplemented: the VT core already resolves SGR against
/// this table when it interns a style, so sharing the table is what keeps a
/// recording's colours identical to the terminal the tape describes.
#[derive(Clone, Debug)]
pub struct Palette {
	colors: List,
	/// The cursor colour. Kept separate from `colors` because `List::from`
	/// fills the ANSI cube and the named defaults but never the `Cursor` entry —
	/// it is a frontend colour, not a grid one — so reading it back through
	/// `NamedColor::Cursor` returns the transparent black the table was seeded
	/// with. The renderer and the SVG both draw the caret from here instead.
	cursor: Color,
}

impl Default for Palette {
	fn default() -> Self {
		Self::from_colors(&Colors::default())
	}
}

impl Palette {
	pub fn new(colors: List) -> Self {
		// `List` alone cannot name its own cursor — same gap as `from_colors`,
		// given the default `Colors` the entries were derived from.
		Self::from_colors(&Colors::default()).with_list(colors)
	}

	fn with_list(mut self, colors: List) -> Self {
		self.colors = colors;
		self
	}

	/// Build the 256-entry table from a named-colour set.
	///
	/// `List::from` is what fills the 6x6x6 cube and the 24-step grey ramp, so
	/// going through it means indexed colours land exactly where the VT core
	/// expects them rather than on a second, hand-rolled ramp. The cursor is
	/// taken from the same set, held out of the list for the reason above.
	pub fn from_colors(colors: &Colors) -> Self {
		Self {
			colors: List::from(colors),
			cursor: Color::from_vt(colors.cursor),
		}
	}

	/// Build the renderer palette captured in an asciicast header.
	///
	/// The terminal theme is deliberately converted at this boundary: the VT
	/// model still owns its normal defaults, while every named colour and the
	/// two explicit foreground/background colours reach both raster and SVG
	/// sinks unchanged. An eight-colour palette leaves the bright half at the
	/// VT default because the recording did not capture those entries.
	pub fn from_terminal_theme(theme: &TerminalTheme) -> anyhow::Result<Self> {
		let foreground = parse_terminal_colour(&theme.foreground, "foreground")?;
		let background = parse_terminal_colour(&theme.background, "background")?;
		let palette = theme
			.palette
			.split(':')
			.enumerate()
			.map(|(index, colour)| parse_terminal_colour(colour, &format!("palette entry {index}")))
			.collect::<anyhow::Result<Vec<_>>>()?;
		anyhow::ensure!(
			matches!(palette.len(), 8 | 16),
			"terminal palette must contain 8 or 16 colours, got {}",
			palette.len()
		);

		let mut colors = Colors::default();
		colors.foreground = foreground;
		colors.background = (background, terminal_colour(background));
		colors.cursor = foreground;
		colors.vi_cursor = foreground;
		let [black, red, green, yellow, blue, magenta, cyan, white] = palette[..8]
			.try_into()
			.expect("validated eight base colours");
		colors.black = black;
		colors.red = red;
		colors.green = green;
		colors.yellow = yellow;
		colors.blue = blue;
		colors.magenta = magenta;
		colors.cyan = cyan;
		colors.white = white;
		if palette.len() == 16 {
			let [
				light_black,
				light_red,
				light_green,
				light_yellow,
				light_blue,
				light_magenta,
				light_cyan,
				light_white,
			] = palette[8..]
				.try_into()
				.expect("validated eight bright colours");
			colors.light_black = light_black;
			colors.light_red = light_red;
			colors.light_green = light_green;
			colors.light_yellow = light_yellow;
			colors.light_blue = light_blue;
			colors.light_magenta = light_magenta;
			colors.light_cyan = light_cyan;
			colors.light_white = light_white;
		}

		Ok(Self::from_colors(&colors))
	}

	/// The colour a bare `AnsiColor` names.
	#[inline]
	pub fn lookup(&self, color: AnsiColor) -> Color {
		match color {
			AnsiColor::Named(named) => Color::from_vt(self.colors[named]),
			AnsiColor::Indexed(index) => Color::from_vt(self.colors[index as usize]),
			AnsiColor::Spec(ColorRgb { r, g, b }) => Color::rgb(r, g, b),
		}
	}

	#[inline]
	pub fn named(&self, named: NamedColor) -> Color {
		match named {
			// See the field on the struct: `List` has no entry for the cursor.
			NamedColor::Cursor => self.cursor,
			other => Color::from_vt(self.colors[other]),
		}
	}

	/// Fold a packed cell and its style into concrete colours.
	///
	/// Order matters and follows what a terminal actually does: bold-as-bright
	/// picks a different colour before any of the rest apply, dim darkens the
	/// foreground *after* that, then inverse swaps the two, then hidden blanks
	/// the glyph. Applying inverse before dim would dim the background instead,
	/// which is the classic way inverted text ends up unreadable.
	pub fn resolve(&self, cell: Square, styles: &[Style], options: &GridOptions) -> ResolvedCell {
		let style = styles
			.get(cell.style_id() as usize)
			.copied()
			.unwrap_or_default();

		let fg = if options.bold_is_bright && style.flags.contains(StyleFlags::BOLD) {
			bright(style.fg)
		} else {
			style.fg
		};

		let mut foreground = self.lookup(fg);
		let background = self.lookup(style.bg);

		if style.flags.contains(StyleFlags::DIM) {
			foreground = foreground.dimmed();
		}

		let (foreground, background) = if style.flags.contains(StyleFlags::INVERSE) {
			(background, foreground)
		} else {
			(foreground, background)
		};

		ResolvedCell {
			character: if style.flags.contains(StyleFlags::HIDDEN) {
				' '
			} else {
				cell.c()
			},
			foreground,
			background,
			underline: style.underline_color.map(|color| self.lookup(color)),
			flags: style.flags,
			wide: cell.wide(),
		}
	}
}

fn parse_terminal_colour(value: &str, name: &str) -> anyhow::Result<[f32; 4]> {
	let digits = value
		.strip_prefix('#')
		.with_context(|| format!("terminal {name} colour must be #rrggbb"))?;
	anyhow::ensure!(
		digits.len() == 6 && digits.bytes().all(|byte| byte.is_ascii_hexdigit()),
		"terminal {name} colour must be #rrggbb, got {value:?}"
	);
	let channel = |range: std::ops::Range<usize>| {
		u8::from_str_radix(&digits[range], 16)
			.with_context(|| format!("terminal {name} colour contains invalid hex"))
	};
	let [red, green, blue] = [channel(0..2)?, channel(2..4)?, channel(4..6)?];
	Ok([
		red as f32 / 255.0,
		green as f32 / 255.0,
		blue as f32 / 255.0,
		1.0,
	])
}

fn terminal_colour(colour: [f32; 4]) -> rio_graphics::Color {
	rio_graphics::Color {
		r: f64::from(colour[0]) * 255.0,
		g: f64::from(colour[1]) * 255.0,
		b: f64::from(colour[2]) * 255.0,
		a: 255.0,
	}
}

/// Map a colour onto the bright cut, the way `st` and `xterm` treat bold.
///
/// Only the eight base `NamedColor`s qualify — an already-light colour, an
/// indexed colour and a truecolour `Spec` are left exactly as they were.
#[inline]
fn bright(color: AnsiColor) -> AnsiColor {
	match color {
		AnsiColor::Named(named) if (named as u8) < 8 => AnsiColor::Named(named.to_light()),
		other => other,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn styled(flags: StyleFlags) -> (Square, Vec<Style>) {
		let style = Style {
			fg: AnsiColor::Spec(ColorRgb {
				r: 200,
				g: 100,
				b: 50,
			}),
			bg: AnsiColor::Spec(ColorRgb {
				r: 10,
				g: 20,
				b: 30,
			}),
			underline_color: None,
			flags,
		};
		let mut cell = Square::from_char('x');
		cell.set_style_id(0);
		(cell, vec![style])
	}

	#[test]
	fn raw_cells_matches_the_packed_representation() {
		let mut snapshot = Snapshot::new(2, 1);
		snapshot.cells = vec![Square::from_char('a'), Square::from_char('b')];

		let raw = snapshot.raw_cells();
		assert_eq!(raw.len(), 2);
		assert_eq!(raw[0], snapshot.cells[0].raw());
		assert_eq!(raw[1], snapshot.cells[1].raw());
	}

	#[test]
	fn inverse_swaps_foreground_and_background() {
		let (cell, styles) = styled(StyleFlags::INVERSE);
		let resolved = Palette::default().resolve(cell, &styles, &GridOptions::default());

		assert_eq!(resolved.foreground, Color::rgb(10, 20, 30));
		assert_eq!(resolved.background, Color::rgb(200, 100, 50));
	}

	/// Dim has to land on the foreground before inverse swaps the pair.
	/// Applying it afterwards dims the background instead, which is how
	/// inverted dim text turns unreadable.
	#[test]
	fn dim_applies_before_inverse() {
		let (cell, styles) = styled(StyleFlags::DIM | StyleFlags::INVERSE);
		let resolved = Palette::default().resolve(cell, &styles, &GridOptions::default());

		assert_eq!(resolved.background, Color::rgb(133, 66, 33));
		assert_eq!(resolved.foreground, Color::rgb(10, 20, 30));
	}

	#[test]
	fn hidden_blanks_the_glyph_but_keeps_the_colours() {
		let (cell, styles) = styled(StyleFlags::HIDDEN);
		let resolved = Palette::default().resolve(cell, &styles, &GridOptions::default());

		assert_eq!(resolved.character, ' ');
		assert_eq!(resolved.background, Color::rgb(10, 20, 30));
	}

	#[test]
	fn a_cell_with_no_style_entry_falls_back_to_the_default() {
		let resolved =
			Palette::default().resolve(Square::from_char('q'), &[], &GridOptions::default());
		assert_eq!(resolved.character, 'q');
		assert_eq!(resolved.flags, StyleFlags::empty());
	}

	/// `bold_is_bright` is the only thing that can change what `resolve`
	/// hands back for the same cell and palette, so it needs its own case:
	/// a bold ANSI red must land on the palette's bright red, while a
	/// truecolour `Spec` — which names no ANSI slot to promote — must come
	/// back untouched.
	#[test]
	fn bold_is_bright_promotes_named_colours_but_leaves_specs_alone() {
		let style = Style {
			fg: AnsiColor::Named(NamedColor::Red),
			bg: AnsiColor::Named(NamedColor::Background),
			underline_color: None,
			flags: StyleFlags::BOLD,
		};
		let mut cell = Square::from_char('x');
		cell.set_style_id(0);
		let styles = vec![style];
		let palette = Palette::default();
		let bright = GridOptions {
			bold_is_bright: true,
		};

		let resolved = palette.resolve(cell, &styles, &bright);
		assert_eq!(resolved.foreground, palette.named(NamedColor::LightRed));

		let spec_style = Style {
			fg: AnsiColor::Spec(ColorRgb {
				r: 10,
				g: 20,
				b: 30,
			}),
			..style
		};
		let spec_styles = vec![spec_style];
		let resolved_spec = palette.resolve(cell, &spec_styles, &bright);
		assert_eq!(resolved_spec.foreground, Color::rgb(10, 20, 30));

		// With the option off, the same bold cell resolves to plain red.
		let resolved_off = palette.resolve(cell, &styles, &GridOptions::default());
		assert_eq!(resolved_off.foreground, palette.named(NamedColor::Red));
	}

	#[test]
	fn identical_snapshots_compare_equal() {
		let a = Snapshot::new(4, 2);
		let mut b = Snapshot::new(4, 2);
		assert!(a.same_picture(&b));

		b.cells[3] = Square::from_char('z');
		assert!(!a.same_picture(&b));
	}

	#[test]
	fn asciicast_theme_reaches_named_and_background_colours_exactly() {
		let palette = Palette::from_terminal_theme(&TerminalTheme {
			foreground: "#102030".into(),
			background: "#405060".into(),
			palette: "#010203:#112233:#223344:#334455:#445566:#556677:#667788:#778899:#8899aa:#99aabb:#aabbcc:#bbccdd:#ccddee:#ddeeff:#eeffff:#ffffff".into(),
		})
		.expect("theme should parse");

		assert_eq!(
			palette.named(NamedColor::Foreground),
			Color::rgb(0x10, 0x20, 0x30)
		);
		assert_eq!(
			palette.named(NamedColor::Background),
			Color::rgb(0x40, 0x50, 0x60)
		);
		assert_eq!(palette.named(NamedColor::Red), Color::rgb(0x11, 0x22, 0x33));
		assert_eq!(
			palette.named(NamedColor::LightWhite),
			Color::rgb(0xff, 0xff, 0xff)
		);
	}

	#[test]
	fn asciicast_theme_rejects_invalid_colour_lists() {
		let error = Palette::from_terminal_theme(&TerminalTheme {
			foreground: "#ffffff".into(),
			background: "#000000".into(),
			palette: "#000000".into(),
		})
		.unwrap_err()
		.to_string();
		assert!(error.contains("8 or 16"));
	}
}
