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

	/// The same colour at a fraction of its opacity.
	///
	/// Multiplies rather than replaces, so a colour that was already partly
	/// transparent cannot be made *more* opaque by asking for coverage — the
	/// shade blocks are the caller, and they mean "this much of the ink",
	/// not "this much, absolutely".
	#[inline]
	pub const fn with_alpha(self, coverage: u8) -> Self {
		let [r, g, b, a] = self.0;
		Self([r, g, b, ((a as u16 * coverage as u16) / 255) as u8])
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

/// A position on the canvas, in pixels.
///
/// A named pair rather than `(f32, f32)`. The tuple was not wrong, it was
/// unreadable: `frame.origin.0` and `frame.canvas.1` say nothing about which
/// axis they are, and the two were different *spaces* — one pixels, one whole
/// canvas extent — spelled identically.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Point {
	pub x: f32,
	pub y: f32,
}

impl Point {
	pub const ORIGIN: Self = Self { x: 0.0, y: 0.0 };

	#[inline]
	pub const fn new(x: f32, y: f32) -> Self {
		Self { x, y }
	}

	#[inline]
	pub const fn translate(self, dx: f32, dy: f32) -> Self {
		Self {
			x: self.x + dx,
			y: self.y + dy,
		}
	}
}

/// A canvas extent, in whole pixels.
///
/// `u16` because it is what every encoder downstream wants: H.264 codes width
/// and height as 16-bit values, and a `Pixmap` is constructed from a pair of
/// them. Carrying the size as `u32` and narrowing at each boundary is three
/// casts that can each disagree.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Size {
	pub width: u16,
	pub height: u16,
}

impl Size {
	#[inline]
	pub const fn new(width: u16, height: u16) -> Self {
		Self { width, height }
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

	#[inline]
	pub const fn contains(self, column: u16) -> bool {
		column >= self.start && column < self.end
	}

	/// The same run widened to cover `column`.
	#[inline]
	pub const fn including(self, column: u16) -> Self {
		if self.is_empty() {
			return Self::new(self.row, column, column + 1);
		}
		Self::new(
			self.row,
			if column < self.start {
				column
			} else {
				self.start
			},
			if column + 1 > self.end {
				column + 1
			} else {
				self.end
			},
		)
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
	pub const EMPTY: Self = Self::new(0.0, 0.0, 0.0, 0.0);

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
	pub const fn at(origin: Point, width: f32, height: f32) -> Self {
		Self::new(origin.x, origin.y, width, height)
	}

	#[inline]
	pub const fn origin(self) -> Point {
		Point::new(self.x, self.y)
	}

	#[inline]
	pub fn is_empty(self) -> bool {
		self.width <= 0.0 || self.height <= 0.0
	}

	/// The horizontal slice `[left, right)` of this rectangle, in units of its
	/// own width — the unit square mapped onto it.
	///
	/// Sprite geometry is written against a unit cell (see [`crate::sprite`]),
	/// which is the only way a box-drawing corner can be specified once and
	/// come out sharp at every font size.
	#[inline]
	pub fn fraction(self, left: f32, top: f32, right: f32, bottom: f32) -> Self {
		Self::new(
			self.x + self.width * left,
			self.y + self.height * top,
			self.width * (right - left),
			self.height * (bottom - top),
		)
	}

	/// The same rectangle with every edge rounded to the nearest whole pixel.
	///
	/// Box drawing and underlines are the callers. A one-pixel rule landing on
	/// a half-pixel boundary is anti-aliased across two rows and reads as a
	/// blurred grey smear rather than a line — the single most visible way a
	/// rendered TUI looks worse than the terminal it came from.
	///
	/// The *edges* are rounded, not the origin and the size independently.
	/// Rounding a size would let a stroke overshoot the cell it was measured
	/// against (4.5 wide starting at 4.5 becomes 5 wide starting at 5, one
	/// pixel past where it should stop); rounding both edges keeps a stroke
	/// that ended exactly at a cell boundary ending exactly there, which is
	/// the whole point. Growing to cover instead — flooring the near edge and
	/// ceiling the far one — is also wrong, and subtly: it turns every
	/// one-pixel rule that happens to straddle a boundary into a two-pixel
	/// one, so a light rule and a heavy rule come out the same weight.
	///
	/// A rule is never allowed to vanish, so the result is at least one pixel
	/// in each direction.
	#[inline]
	pub fn snapped(self) -> Self {
		let left = self.x.round();
		let top = self.y.round();
		Self::new(
			left,
			top,
			(self.right().round() - left).max(1.0),
			(self.bottom().round() - top).max(1.0),
		)
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
	pub origin: Point,
	/// Whole canvas, chrome included.
	pub canvas: Size,
}

impl Frame {
	pub const fn new(metrics: Metrics, origin: Point, canvas: Size) -> Self {
		Self {
			metrics,
			origin,
			canvas,
		}
	}

	/// The canvas rectangle a run of cells occupies.
	#[inline]
	pub fn span_rect(&self, span: Span) -> PixelRect {
		PixelRect::at(
			self.row_origin(span.row)
				.translate((span.start as u32 * self.metrics.cell_width) as f32, 0.0),
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
	pub fn cell_origin(&self, at: Cell) -> Point {
		self.cell_rect(at).origin()
	}

	/// Canvas position of a row's top-left corner — the origin every glyph in
	/// that row is placed relative to.
	#[inline]
	pub fn row_origin(&self, row: u16) -> Point {
		self.origin
			.translate(0.0, (row as u32 * self.metrics.cell_height) as f32)
	}

	/// Canvas y of the text baseline for a row.
	#[inline]
	pub fn baseline(&self, row: u16) -> f32 {
		self.row_origin(row).y + self.metrics.baseline
	}

	/// The whole canvas.
	#[inline]
	pub fn canvas_rect(&self) -> PixelRect {
		PixelRect::new(
			0.0,
			0.0,
			self.canvas.width as f32,
			self.canvas.height as f32,
		)
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
		let frame = Frame::new(metrics(), Point::ORIGIN, Size::new(100, 40));

		let left = frame.span_rect(Span::new(0, 0, 3));
		let right = frame.span_rect(Span::new(0, 3, 8));

		assert_eq!(left.right(), right.x);
		assert_eq!(left.width, 30.0);
		assert_eq!(right.width, 50.0);
	}

	#[test]
	fn the_panel_inset_moves_the_whole_grid() {
		let frame = Frame::new(metrics(), Point::new(24.0, 24.0), Size::new(148, 88));
		let rect = frame.cell_rect(Cell::new(2, 1));

		assert_eq!(rect.origin(), Point::new(44.0, 44.0));
		assert_eq!((rect.width, rect.height), (10.0, 20.0));
		assert_eq!(frame.baseline(1), 24.0 + 20.0 + 15.0);
		assert_eq!(frame.row_origin(1), Point::new(24.0, 44.0));
	}

	#[test]
	fn a_span_of_one_cell_covers_exactly_that_cell() {
		let frame = Frame::new(metrics(), Point::ORIGIN, Size::new(100, 40));
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

	/// Sprite geometry is written against the unit cell, so the mapping onto a
	/// real cell has to be exact at both edges — a box-drawing corner whose
	/// arms stop a fraction short of the boundary leaves a visible seam where
	/// it should meet its neighbour.
	#[test]
	fn a_unit_fraction_maps_onto_the_cell_it_is_taken_from() {
		let cell = PixelRect::new(10.0, 20.0, 8.0, 16.0);

		let whole = cell.fraction(0.0, 0.0, 1.0, 1.0);
		assert_eq!(whole, cell);

		let middle_bar = cell.fraction(0.0, 0.5, 1.0, 0.5);
		assert_eq!(middle_bar.x, 10.0);
		assert_eq!(middle_bar.y, 28.0);
		assert_eq!(middle_bar.width, 8.0);
		assert_eq!(middle_bar.height, 0.0);

		let right_half = cell.fraction(0.5, 0.0, 1.0, 1.0);
		assert_eq!(right_half.x, 14.0);
		assert_eq!(right_half.right(), cell.right());
	}

	/// A hairline that rounds away to nothing is one failure this prevents: a
	/// sub-pixel rule must survive as one whole pixel.
	#[test]
	fn snapping_keeps_a_hairline_as_exactly_one_pixel() {
		let hairline = PixelRect::new(4.2, 9.8, 10.0, 0.4).snapped();

		assert_eq!(hairline.y, 10.0);
		assert_eq!(
			hairline.height, 1.0,
			"a sub-pixel rule survives as one pixel, not two"
		);
		assert_eq!(hairline.x, 4.0);
		assert_eq!(hairline.right(), 14.0);
	}

	/// The other failure, and the subtler one: a one-pixel rule sitting on a
	/// half-pixel boundary must stay one pixel. Growing to cover would make
	/// it two, which is exactly the weight of a heavy rule — so a light `─`
	/// and a heavy `━` would come out identical.
	#[test]
	fn snapping_does_not_fatten_a_rule_that_straddles_a_boundary() {
		let straddling = PixelRect::new(0.0, 9.5, 10.0, 1.0).snapped();
		assert_eq!(straddling.height, 1.0);

		let heavy = PixelRect::new(0.0, 9.0, 10.0, 2.0).snapped();
		assert_eq!(heavy.height, 2.0, "and a genuinely heavy rule stays heavy");
	}

	/// A stroke measured to end at a cell boundary must still end there after
	/// snapping, or every box-drawing arm overshoots into its neighbour.
	#[test]
	fn snapping_never_pushes_a_stroke_past_the_edge_it_was_measured_to() {
		let cell = PixelRect::new(0.0, 0.0, 10.0, 20.0);
		// A corner's arm runs from just left of centre to the cell's edge.
		let arm = PixelRect::new(4.5, 0.0, cell.right() - 4.5, 20.0).snapped();

		assert_eq!(
			arm.right(),
			cell.right(),
			"the arm must stop exactly at the cell edge"
		);
	}

	/// Damage is widened cell by cell as the cursor moves, so widening has to
	/// work from an empty run as well as from an existing one.
	#[test]
	fn a_span_widens_to_include_a_column_outside_it() {
		let span = Span::new(3, 4, 6);

		assert_eq!(span.including(1), Span::new(3, 1, 6));
		assert_eq!(span.including(9), Span::new(3, 4, 10));
		assert_eq!(span.including(5), span, "an interior column changes nothing");
		assert_eq!(Span::new(3, 0, 0).including(7), Span::new(3, 7, 8));

		assert!(span.contains(4) && span.contains(5));
		assert!(!span.contains(6), "a half-open run excludes its end");
	}
}
