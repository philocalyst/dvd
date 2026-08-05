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

use rio_vt::config::colors::{AnsiColor, ColorRgb, Colors, NamedColor, term::List};
use rio_vt::crosswords::grid::row::Row;
use rio_vt::crosswords::pos::CursorState;
use rio_vt::crosswords::square::{Extras, Square, Wide};
use rio_vt::crosswords::style::{Style, StyleFlags};
use rustc_hash::FxHashMap;

/// Straight, non-premultiplied 8-bit RGBA.
pub type Rgba = [u8; 4];

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
	pub foreground: Rgba,
	pub background: Rgba,
	/// Explicit underline colour (SGR 58), when the cell sets one.
	pub underline: Option<Rgba>,
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
}

impl Default for Palette {
	fn default() -> Self {
		Self::from_colors(&Colors::default())
	}
}

impl Palette {
	pub fn new(colors: List) -> Self {
		Self { colors }
	}

	/// Build the 256-entry table from a named-colour set.
	///
	/// `List::from` is what fills the 6x6x6 cube and the 24-step grey ramp, so
	/// going through it means indexed colours land exactly where the VT core
	/// expects them rather than on a second, hand-rolled ramp.
	pub fn from_colors(colors: &Colors) -> Self {
		Self {
			colors: List::from(colors),
		}
	}

	/// The colour a bare `AnsiColor` names.
	#[inline]
	pub fn lookup(&self, color: AnsiColor) -> Rgba {
		match color {
			AnsiColor::Named(named) => float_to_rgba(self.colors[named]),
			AnsiColor::Indexed(index) => float_to_rgba(self.colors[index as usize]),
			AnsiColor::Spec(ColorRgb { r, g, b }) => [r, g, b, 0xff],
		}
	}

	#[inline]
	pub fn named(&self, named: NamedColor) -> Rgba {
		float_to_rgba(self.colors[named])
	}

	/// Fold a packed cell and its style into concrete colours.
	///
	/// Order matters and follows what a terminal actually does: dim darkens the
	/// foreground, *then* inverse swaps the two, *then* hidden blanks the
	/// glyph. Applying inverse before dim would dim the background instead,
	/// which is the classic way inverted text ends up unreadable.
	pub fn resolve(&self, cell: Square, styles: &[Style]) -> ResolvedCell {
		let style = styles
			.get(cell.style_id() as usize)
			.copied()
			.unwrap_or_default();

		let mut foreground = self.lookup(style.fg);
		let background = self.lookup(style.bg);

		if style.flags.contains(StyleFlags::DIM) {
			foreground = dim(foreground);
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

/// The conventional two-thirds intensity for SGR 2.
#[inline]
fn dim(color: Rgba) -> Rgba {
	const NUMERATOR: u16 = 2;
	const DENOMINATOR: u16 = 3;
	let scale = |channel: u8| (channel as u16 * NUMERATOR / DENOMINATOR) as u8;
	[scale(color[0]), scale(color[1]), scale(color[2]), color[3]]
}

/// `rio-vt` stores colours as `[f32; 4]` in 0..=1; the renderer and every
/// encoder want bytes.
#[inline]
fn float_to_rgba(color: [f32; 4]) -> Rgba {
	let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
	[
		channel(color[0]),
		channel(color[1]),
		channel(color[2]),
		// The table's alpha is a blend weight for the GPU path, not opacity;
		// a captured cell is always fully opaque.
		0xff,
	]
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
		let resolved = Palette::default().resolve(cell, &styles);

		assert_eq!(resolved.foreground, [10, 20, 30, 255]);
		assert_eq!(resolved.background, [200, 100, 50, 255]);
	}

	/// Dim has to land on the foreground before inverse swaps the pair.
	/// Applying it afterwards dims the background instead, which is how
	/// inverted dim text turns unreadable.
	#[test]
	fn dim_applies_before_inverse() {
		let (cell, styles) = styled(StyleFlags::DIM | StyleFlags::INVERSE);
		let resolved = Palette::default().resolve(cell, &styles);

		assert_eq!(resolved.background, [133, 66, 33, 255]);
		assert_eq!(resolved.foreground, [10, 20, 30, 255]);
	}

	#[test]
	fn hidden_blanks_the_glyph_but_keeps_the_colours() {
		let (cell, styles) = styled(StyleFlags::HIDDEN);
		let resolved = Palette::default().resolve(cell, &styles);

		assert_eq!(resolved.character, ' ');
		assert_eq!(resolved.background, [10, 20, 30, 255]);
	}

	#[test]
	fn a_cell_with_no_style_entry_falls_back_to_the_default() {
		let resolved = Palette::default().resolve(Square::from_char('q'), &[]);
		assert_eq!(resolved.character, 'q');
		assert_eq!(resolved.flags, StyleFlags::empty());
	}

	#[test]
	fn identical_snapshots_compare_equal() {
		let a = Snapshot::new(4, 2);
		let mut b = Snapshot::new(4, 2);
		assert!(a.same_picture(&b));

		b.cells[3] = Square::from_char('z');
		assert!(!a.same_picture(&b));
	}
}
