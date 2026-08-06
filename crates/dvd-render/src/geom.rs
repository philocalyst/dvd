//! The vocabulary two backends have to agree on.
//!
//! A terminal frame is drawn twice — once into pixels by `vello_cpu`, once into
//! markup by the SVG sink — and the only way those two stay in step is for both
//! to be handed the same shapes in the same coordinates. This module is that
//! shared coordinate system: a colour, a cell, a run of cells, and the
//! rectangle a run of cells occupies.
//!
//! Two coordinate spaces exist and they are deliberately different types.
//! [`Cell`] and [`Span`] are grid coordinates, which is what the terminal
//! decided; [`PixelRect`] is canvas coordinates, which is where that lands once
//! the cell size and the panel inset are known. [`Grid`](crate::grid::Grid)
//! speaks the first, painters speak the second, and [`Frame`] is the one place
//! that converts — so a backend cannot accidentally do its own arithmetic and
//! drift half a pixel away from the other one.

use crate::fonts::Metrics;

/// Straight, non-premultiplied 8-bit RGBA.
///
/// A newtype rather than a bare `[u8; 4]` for one reason worth the churn: this
/// is the value both backends have to spell identically, and a type that knows
/// how to spell itself (`#rrggbb`) removes the hand-rolled hex formatter the SVG
/// sink used to carry. Everything else it gains — comparing by value, being
/// `Copy`, naming a channel — a plain array had too.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Color(pub [u8; 4]);

impl Color {
	pub const TRANSPARENT: Self = Self([0, 0, 0, 0]);
	pub const BLACK: Self = Self([0, 0, 0, 0xff]);
	pub const WHITE: Self = Self([0xff, 0xff, 0xff, 0xff]);

	#[inline]
	pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
		Self([r, g, b, 0xff])
	}

	#[inline]
	pub const fn channels(self) -> [u8; 4] {
		self.0
	}

	#[inline]
	pub const fn red(self) -> u8 {
		self.0[0]
	}

	#[inline]
	pub const fn green(self) -> u8 {
		self.0[1]
	}

	#[inline]
	pub const fn blue(self) -> u8 {
		self.0[2]
	}

	#[inline]
	pub const fn alpha(self) -> u8 {
		self.0[3]
	}

	#[inline]
	pub const fn is_transparent(self) -> bool {
		self.0[3] == 0
	}

	/// Perceived brightness, BT.709.
	///
	/// Used to pick a legible ink against an arbitrary background — a glyph
	/// under a block cursor, most often. Luma rather than a plain inversion
	/// because inverting a mid-grey cursor produces another mid-grey.
	#[inline]
	pub fn luma(self) -> f32 {
		0.2126 * self.0[0] as f32 + 0.7152 * self.0[1] as f32 + 0.0722 * self.0[2] as f32
	}

	/// Black or white, whichever stays readable on top of this colour.
	#[inline]
	pub fn contrasting(self) -> Self {
		if self.luma() > 128.0 {
			Self::BLACK
		} else {
			Self::WHITE
		}
	}

	/// The conventional two-thirds intensity for SGR 2.
	#[inline]
	pub fn dimmed(self) -> Self {
		const NUMERATOR: u16 = 2;
		const DENOMINATOR: u16 = 3;
		let scale = |channel: u8| (channel as u16 * NUMERATOR / DENOMINATOR) as u8;
		Self([
			scale(self.0[0]),
			scale(self.0[1]),
			scale(self.0[2]),
			self.0[3],
		])
	}

	/// `rio-vt` stores colours as `[f32; 4]` in 0..=1; everything downstream
	/// wants bytes.
	///
	/// The alpha is dropped on purpose. In the VT core's table it is a blend
	/// weight for a GPU frontend, not opacity, and a captured cell is always
	/// fully opaque — carrying it through would make an ordinary theme's text
	/// translucent.
	#[inline]
	pub fn from_vt(color: [f32; 4]) -> Self {
		let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
		Self([
			channel(color[0]),
			channel(color[1]),
			channel(color[2]),
			0xff,
		])
	}
}

/// `#rrggbb`, which is what both SVG and every theme file want.
///
/// The alpha is not written: SVG spells opacity as a separate attribute rather
/// than as a fourth channel, and every colour that reaches markup is opaque.
impl std::fmt::Display for Color {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(
			formatter,
			"#{:02x}{:02x}{:02x}",
			self.0[0], self.0[1], self.0[2]
		)
	}
}

impl std::fmt::Debug for Color {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		if self.0[3] == 0xff {
			write!(formatter, "{self}")
		} else {
			write!(formatter, "{self}/{:02x}", self.0[3])
		}
	}
}

impl From<[u8; 4]> for Color {
	fn from(channels: [u8; 4]) -> Self {
		Self(channels)
	}
}

impl From<Color> for [u8; 4] {
	fn from(color: Color) -> Self {
		color.0
	}
}

/// One position on the grid.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Cell {
	pub column: u16,
	pub row: u16,
}

impl Cell {
	#[inline]
	pub const fn new(column: u16, row: u16) -> Self {
		Self { column, row }
	}
}

/// A half-open run of cells within one row: `start..end`.
///
/// Half-open because that is what makes an empty run expressible and adjacent
/// runs tile without an off-by-one — a row of eighty columns is `0..80`, and the
/// run after `0..8` starts at `8`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span {
	pub row: u16,
	pub start: u16,
	pub end: u16,
}

impl Span {
	#[inline]
	pub const fn new(row: u16, start: u16, end: u16) -> Self {
		Self { row, start, end }
	}

	/// A single cell as a run of one.
	#[inline]
	pub const fn cell(at: Cell) -> Self {
		Self {
			row: at.row,
			start: at.column,
			end: at.column + 1,
		}
	}

	#[inline]
	pub const fn columns(self) -> u16 {
		self.end.saturating_sub(self.start)
	}

	#[inline]
	pub const fn is_empty(self) -> bool {
		self.end <= self.start
	}
}

/// A rectangle in canvas pixels, as an origin and a size.
///
/// Origin-and-size rather than two corners because every consumer wants the
/// size: `vello_cpu` builds a `Rect` from it, SVG writes `width`/`height`
/// attributes, and an empty rectangle is `width == 0` in both.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct PixelRect {
	pub x: f32,
	pub y: f32,
	pub width: f32,
	pub height: f32,
}

impl PixelRect {
	#[inline]
	pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
		Self {
			x,
			y,
			width,
			height,
		}
	}

	#[inline]
	pub fn is_empty(self) -> bool {
		self.width <= 0.0 || self.height <= 0.0
	}

	#[inline]
	pub fn right(self) -> f32 {
		self.x + self.width
	}

	#[inline]
	pub fn bottom(self) -> f32 {
		self.y + self.height
	}

	/// The same rectangle inset on every side, clamped at empty.
	#[inline]
	pub fn inset(self, by: f32) -> Self {
		Self {
			x: self.x + by,
			y: self.y + by,
			width: (self.width - 2.0 * by).max(0.0),
			height: (self.height - 2.0 * by).max(0.0),
		}
	}

	/// The overlap of two rectangles, or `None` when they do not touch.
	pub fn intersect(self, other: Self) -> Option<Self> {
		let x = self.x.max(other.x);
		let y = self.y.max(other.y);
		let right = self.right().min(other.right());
		let bottom = self.bottom().min(other.bottom());

		(right > x && bottom > y).then_some(Self::new(x, y, right - x, bottom - y))
	}
}

/// Where the grid sits on the canvas, and how big a cell is.
///
/// The one place grid coordinates become pixel coordinates. Both backends go
/// through it, which is what keeps a background rectangle and the glyph on top
/// of it agreeing to the pixel — the failure this type exists to prevent is two
/// callers each rounding the same product their own way.
#[derive(Clone, Copy, Debug)]
pub struct Frame {
	pub metrics: Metrics,
	/// Canvas position of the top-left corner of cell `(0, 0)`.
	pub origin: (f32, f32),
	/// Whole canvas, chrome included.
	pub canvas: (u16, u16),
}

impl Frame {
	pub fn new(metrics: Metrics, origin: (f32, f32), canvas: (u16, u16)) -> Self {
		Self {
			metrics,
			origin,
			canvas,
		}
	}

	/// The canvas rectangle a run of cells occupies.
	#[inline]
	pub fn span_rect(&self, span: Span) -> PixelRect {
		PixelRect::new(
			self.origin.0 + (span.start as u32 * self.metrics.cell_width) as f32,
			self.origin.1 + (span.row as u32 * self.metrics.cell_height) as f32,
			(span.columns() as u32 * self.metrics.cell_width) as f32,
			self.metrics.cell_height as f32,
		)
	}

	/// The canvas rectangle one cell occupies.
	#[inline]
	pub fn cell_rect(&self, at: Cell) -> PixelRect {
		self.span_rect(Span::cell(at))
	}

	/// Canvas position of a cell's top-left corner.
	#[inline]
	pub fn cell_origin(&self, at: Cell) -> (f32, f32) {
		let rect = self.cell_rect(at);
		(rect.x, rect.y)
	}

	/// Canvas y of the text baseline for a row.
	#[inline]
	pub fn baseline(&self, row: u16) -> f32 {
		self.origin.1 + (row as u32 * self.metrics.cell_height) as f32 + self.metrics.baseline
	}

	/// The whole canvas.
	#[inline]
	pub fn canvas_rect(&self) -> PixelRect {
		PixelRect::new(0.0, 0.0, self.canvas.0 as f32, self.canvas.1 as f32)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

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

	#[test]
	fn a_colour_spells_itself_the_way_svg_wants() {
		assert_eq!(Color::rgb(0x28, 0x2a, 0x36).to_string(), "#282a36");
		assert_eq!(Color::rgb(0, 0, 0).to_string(), "#000000");
		assert_eq!(Color::WHITE.to_string(), "#ffffff");
	}

	/// The VT core's alpha is a GPU blend weight, not opacity. Carrying it
	/// through would make ordinary text translucent.
	#[test]
	fn converting_from_the_vt_table_forces_opacity() {
		assert_eq!(Color::from_vt([1.0, 0.0, 0.0, 0.0]), Color::rgb(255, 0, 0));
		assert_eq!(
			Color::from_vt([2.0, -1.0, 0.5, 1.0]),
			Color::rgb(255, 0, 128)
		);
	}

	#[test]
	fn ink_is_chosen_to_stay_readable() {
		assert_eq!(Color::WHITE.contrasting(), Color::BLACK);
		assert_eq!(Color::BLACK.contrasting(), Color::WHITE);
		// Mid-grey is the case a plain inversion gets wrong.
		assert_eq!(Color::rgb(128, 128, 128).contrasting(), Color::WHITE);
	}

	/// Adjacent runs have to tile exactly, or a row of backgrounds grows a seam.
	#[test]
	fn adjacent_spans_tile_without_a_gap_or_an_overlap() {
		let frame = Frame::new(metrics(), (0.0, 0.0), (100, 40));

		let left = frame.span_rect(Span::new(0, 0, 3));
		let right = frame.span_rect(Span::new(0, 3, 8));

		assert_eq!(left.right(), right.x);
		assert_eq!(left.width, 30.0);
		assert_eq!(right.width, 50.0);
	}

	#[test]
	fn the_panel_inset_moves_the_whole_grid() {
		let frame = Frame::new(metrics(), (24.0, 24.0), (148, 88));
		let rect = frame.cell_rect(Cell::new(2, 1));

		assert_eq!((rect.x, rect.y), (44.0, 44.0));
		assert_eq!((rect.width, rect.height), (10.0, 20.0));
		assert_eq!(frame.baseline(1), 24.0 + 20.0 + 15.0);
	}

	#[test]
	fn a_span_of_one_cell_covers_exactly_that_cell() {
		let frame = Frame::new(metrics(), (0.0, 0.0), (100, 40));
		assert_eq!(
			frame.span_rect(Span::cell(Cell::new(4, 1))),
			frame.cell_rect(Cell::new(4, 1))
		);
		assert_eq!(Span::cell(Cell::new(4, 1)).columns(), 1);
	}

	#[test]
	fn rectangles_that_miss_each_other_do_not_intersect() {
		let a = PixelRect::new(0.0, 0.0, 10.0, 10.0);
		let b = PixelRect::new(20.0, 0.0, 10.0, 10.0);
		assert_eq!(a.intersect(b), None);

		let c = PixelRect::new(5.0, 5.0, 10.0, 10.0);
		assert_eq!(a.intersect(c), Some(PixelRect::new(5.0, 5.0, 5.0, 5.0)));
	}

	/// Clipping a placement against the panel must not produce a negative size.
	#[test]
	fn insetting_past_the_middle_collapses_rather_than_inverting() {
		let rect = PixelRect::new(0.0, 0.0, 4.0, 4.0).inset(3.0);
		assert!(rect.is_empty());
		assert_eq!((rect.width, rect.height), (0.0, 0.0));
	}
}
