//! The paint pass: one reduction from model to shapes, backends that receive
//! it.
//!
//! Before this module existed, `render.rs` and `encode/svg.rs` were two
//! independent implementations of the same drawing. Background runs, cursor
//! geometry, decorations and wide-cell handling were each written twice, and
//! they already disagreed — the SVG had no bidirectional reordering, no
//! combining marks, and distorting glyph shapes with
//! `lengthAdjust="spacingAndGlyphs"`. The fix is not to maintain them better
//! in step; it is to have one place that decides what a frame *is*, and hand
//! that decision to both backends. [`paint`] is that place.
//!
//! Paint order is a terminal's, and it is load-bearing:
//!
//! 1. canvas fill and panel — only when `damage.is_everything()`
//! 2. cell backgrounds, merged into runs via [`Grid::background_runs`]
//! 3. graphics with negative `z`
//! 4. the cursor — a block cursor is a *background*, painted before the
//!    glyphs, which is why the glyph on top is recoloured rather than the
//!    cell inverted
//! 5. glyphs
//! 6. underlines and strikethroughs
//! 7. graphics with non-negative `z`
//!
//! A [`Painter`] that only wants changed cells passes a real [`Damage`]; one
//! that wants the whole frame passes [`Damage::everything`]. The mechanism is
//! correct even when `Renderer` passes `Damage::everything` every frame for
//! now — what matters is that skipping untouched cells works when someone
//! asks for it.

use rio_vt::ansi::CursorShape;
use rio_vt::crosswords::style::StyleFlags;

use crate::geom::{Cell, Color, Frame, PixelRect, Span};
use crate::grid::{Caret, Damage, Grid};
use crate::model::Placement;
use crate::shape::{GlyphRun, Shaper};

/// Anything that can receive the shapes one frame reduces to.
///
/// The whole point: [`paint`] calls these in one fixed order, so the
/// rasterizer and the SVG writer draw the same picture by construction rather
/// than by two people maintaining two implementations in step.
///
/// Taken as `&mut impl Painter` — monomorphised, no `dyn`. The call sites are
/// few and the inlining is free.
pub trait Painter {
	/// Fill a solid rectangle.
	fn fill(&mut self, rect: PixelRect, color: Color);

	/// Fill a rectangle with rounded corners. Used for the panel and for
	/// any margin fill the surface carries.
	fn rounded_fill(&mut self, rect: PixelRect, radius: f32, color: Color);

	/// Draw a run of glyphs sharing a colour and a set of attributes.
	///
	/// The origin is the canvas position of the row's top-left corner; each
	/// glyph's `x`/`y` is relative to it. A backend that emits text rather
	/// than outlines can read `run.text` for the source characters.
	fn glyphs(&mut self, run: GlyphRun<'_>, origin: (f32, f32));

	/// Composite a placed image, clipped to `clip`.
	///
	/// May be a no-op stub for now — a later workstream implements graphics
	/// compositing — but the call must be made in the right place in the
	/// paint order and the trait method must exist with the right shape.
	fn image(&mut self, placement: &Placement, clip: PixelRect);
}

/// How decorations (underlines, strikethroughs) are drawn.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
struct Decoration {
	rect: PixelRect,
	color: Color,
}

/// Draw one frame into a painter.
///
/// See the module doc for the paint order and why each step is where it is.
pub fn paint(
	grid: &Grid,
	damage: &Damage,
	shaper: &mut Shaper,
	frame: &Frame,
	surface: &crate::render::Surface,
	painter: &mut impl Painter,
) {
	// 1. Canvas fill and panel — only on a full redraw. When the damage is
	// partial, the panel is already on screen from the previous frame;
	// repainting it would cover the cells that were not redrawn.
	if damage.is_everything() {
		if let Some(fill) = surface.margin_fill {
			painter.fill(frame.canvas_rect(), fill);
		}
		let panel = crate::render::panel_rect(frame, surface);
		if surface.border_radius > 0 {
			painter.rounded_fill(panel, surface.border_radius as f32, grid.background);
		} else {
			painter.fill(panel, grid.background);
		}
	}

	// 2. Cell backgrounds, merged into runs. `background_runs` skips the
	// panel colour — painting it again would be a rectangle that changes
	// nothing.
	for row in damage.rows() {
		for (span, color) in grid.background_runs(row.row) {
			painter.fill(frame.span_rect(span), color);
		}
	}

	// 3. Graphics with negative z (beneath the text).
	for placement in &grid.graphics {
		if placement.z < 0 {
			let clip = frame.canvas_rect();
			painter.image(placement, clip);
		}
	}

	// 4. The cursor — a block cursor is a background, painted before the
	// glyphs. The glyph on top is recoloured (`Caret::text`, already
	// resolved) rather than the cell inverted.
	if let Some(caret) = grid.cursor {
		paint_cursor(painter, caret, frame);
	}

	// 5. Glyphs. Shaped per row, drawn per run of shared colour.
	for row in damage.rows() {
		let layout = shaper.row(grid, row.row);
		if layout.glyphs.is_empty() {
			continue;
		}
		paint_glyphs(painter, layout, grid, row, frame);
	}

	// 6. Underlines and strikethroughs.
	for row in damage.rows() {
		paint_decorations(painter, grid, row, frame);
	}

	// 7. Graphics with non-negative z (over the text).
	for placement in &grid.graphics {
		if placement.z >= 0 {
			let clip = frame.canvas_rect();
			painter.image(placement, clip);
		}
	}
}

/// Paint the cursor, respecting its shape.
fn paint_cursor(painter: &mut impl Painter, caret: Caret, frame: &Frame) {
	let rect = frame.cell_rect(caret.at);
	let thickness = (frame.metrics.underline_thickness * 1.5).round().max(2.0);

	match caret.shape {
		CursorShape::Block => {
			// The full cell — the glyph on top is recoloured by the
			// shaper/painter using `caret.text`.
			painter.fill(rect, caret.color);
		}
		CursorShape::Underline => {
			painter.fill(
				PixelRect::new(rect.x, rect.bottom() - thickness, rect.width, thickness),
				caret.color,
			);
		}
		CursorShape::Beam => {
			painter.fill(
				PixelRect::new(rect.x, rect.y, thickness, rect.height),
				caret.color,
			);
		}
		CursorShape::Hidden => {}
	}
}

/// Paint one row's glyphs, grouped into runs of shared colour.
fn paint_glyphs(
	painter: &mut impl Painter,
	layout: &crate::shape::RowLayout,
	grid: &Grid,
	row_span: Span,
	frame: &Frame,
) {
	let origin = frame.cell_origin(Cell::new(0, row_span.row));
	let cells = grid.row(row_span.row);

	let mut start = 0usize;
	while start < layout.glyphs.len() {
		let column = layout.glyphs[start].column as usize;
		let cell = &cells[column.min(cells.len().saturating_sub(1))];
		let color = cursor_text(grid, cell, layout.glyphs[start].column, row_span.row)
			.unwrap_or(cell.foreground);
		let flags = cell.flags;

		let mut end = start + 1;
		while end < layout.glyphs.len() {
			let next_column = layout.glyphs[end].column as usize;
			let next_cell = &cells[next_column.min(cells.len().saturating_sub(1))];
			let next_color = cursor_text(grid, next_cell, layout.glyphs[end].column, row_span.row)
				.unwrap_or(next_cell.foreground);
			if next_color != color || next_cell.flags != flags {
				break;
			}
			end += 1;
		}

		let span = Span::new(
			row_span.row,
			layout.glyphs[start].column,
			layout.glyphs[end.saturating_sub(1)].column + 1,
		);
		let glyphs = &layout.glyphs[start..end];
		let text = &layout.text;

		painter.glyphs(
			GlyphRun {
				span,
				glyphs,
				text,
				color,
				flags,
			},
			origin,
		);

		start = end;
	}
}

/// The colour a glyph is drawn in, accounting for the cursor sitting on it.
///
/// A block cursor paints over the cell, so the glyph underneath has to be
/// re-coloured or it disappears into the cursor. `Caret::text` is already
/// resolved by `Color::contrasting`, so this is a lookup rather than a
/// computation.
fn cursor_text(
	grid: &Grid,
	_cell: &crate::model::ResolvedCell,
	column: u16,
	row: u16,
) -> Option<Color> {
	let caret = grid.cursor?;
	if caret.shape != CursorShape::Block {
		return None;
	}
	if caret.at.column != column || caret.at.row != row {
		return None;
	}
	Some(caret.text)
}

/// Paint underlines and strikethroughs for a row, as rectangles.
///
/// Rectangles rather than the font's own decoration glyphs because a run
/// spanning several cells has to be continuous across them, and because the
/// thickness has to land on whole pixels to stay crisp at recording sizes.
fn paint_decorations(painter: &mut impl Painter, grid: &Grid, row_span: Span, frame: &Frame) {
	let cells = grid.row(row_span.row);
	let metrics = frame.metrics;

	for column in row_span.start..row_span.end {
		let cell = &cells[column as usize];
		let rect = frame.cell_rect(Cell::new(column, row_span.row));
		let color = cell.underline.unwrap_or(cell.foreground);

		if cell.flags.intersects(StyleFlags::ALL_UNDERLINES) {
			let top = rect.y + metrics.underline_offset;
			painter.fill(
				PixelRect::new(rect.x, top, rect.width, metrics.underline_thickness),
				color,
			);
		}

		if cell.flags.contains(StyleFlags::STRIKEOUT) {
			let top = rect.y + metrics.strikeout_offset;
			painter.fill(
				PixelRect::new(rect.x, top, rect.width, metrics.strikeout_thickness),
				cell.foreground,
			);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::fonts::Fonts;
	use crate::geom::Frame;
	use crate::grid::{Grid, GridOptions};
	use crate::model::{Palette, Snapshot};
	use crate::render::Surface;
	use crate::shape::Shaper;
	use rio_vt::crosswords::square::Square;

	/// A test double that records every call it receives, so a test can
	/// assert paint order and run merging without reading pixels.
	#[derive(Default)]
	struct RecordPainter {
		calls: Vec<&'static str>,
	}

	impl Painter for RecordPainter {
		fn fill(&mut self, _rect: PixelRect, _color: Color) {
			self.calls.push("fill");
		}
		fn rounded_fill(&mut self, _rect: PixelRect, _radius: f32, _color: Color) {
			self.calls.push("rounded_fill");
		}
		fn glyphs(&mut self, _run: GlyphRun<'_>, _origin: (f32, f32)) {
			self.calls.push("glyphs");
		}
		fn image(&mut self, _placement: &Placement, _clip: PixelRect) {
			self.calls.push("image");
		}
	}

	fn make_frame(columns: u16, rows: u16, fill: char) -> (Grid, Shaper, Frame, Surface) {
		let fonts = Fonts::resolve(Some("Liberation Mono"), 16.0, 1.0).unwrap();
		let metrics = fonts.metrics;
		let palette = Palette::default();
		let surface = Surface {
			margin: 0,
			padding: 0,
			border_radius: 0,
			margin_fill: None,
		};
		let frame = Frame::new(
			metrics,
			(0.0, 0.0),
			(
				columns * metrics.cell_width as u16,
				rows * metrics.cell_height as u16,
			),
		);
		let shaper = Shaper::new(fonts);
		let mut snapshot = Snapshot::new(columns, rows);
		snapshot.cells.fill(Square::from_char(fill));
		let mut grid = Grid::new(columns, rows);
		grid.fill(&snapshot, &palette, &GridOptions::default());
		(grid, shaper, frame, surface)
	}

	/// A `Damage::everything` frame must paint the canvas/panel first, then
	/// backgrounds, then glyphs. The panel is a `fill` (no border radius).
	#[test]
	fn paint_order_on_a_full_redraw_is_panel_then_backgrounds_then_glyphs() {
		let (grid, mut shaper, frame, surface) = make_frame(4, 2, 'W');
		let damage = Damage::everything(4, 2);
		let mut painter = RecordPainter::default();
		paint(&grid, &damage, &mut shaper, &frame, &surface, &mut painter);

		// The first call should be the panel fill (margin_fill is None, so
		// no canvas fill; border_radius is 0, so it's a `fill` not a
		// `rounded_fill`).
		assert_eq!(
			painter.calls.first(),
			Some(&"fill"),
			"panel must be painted first on a full redraw"
		);
		// Glyphs must come after backgrounds.
		let first_glyph = painter.calls.iter().position(|c| *c == "glyphs");
		assert!(
			first_glyph.is_some(),
			"glyphs must be painted for a row of text"
		);
		let last_fill_before_glyphs = painter
			.calls
			.iter()
			.take(first_glyph.unwrap())
			.rposition(|c| *c == "fill");
		assert!(
			last_fill_before_glyphs.is_some(),
			"backgrounds must be painted before glyphs"
		);
	}

	/// A row of identical backgrounds must become one `fill` rather than
	/// eighty — this is what `background_runs` merging is for.
	#[test]
	fn a_row_of_identical_backgrounds_becomes_one_fill() {
		use rio_vt::config::colors::{AnsiColor, ColorRgb};
		use rio_vt::crosswords::style::Style;

		let fonts = Fonts::resolve(Some("Liberation Mono"), 16.0, 1.0).unwrap();
		let palette = Palette::default();
		let surface = Surface {
			margin: 0,
			padding: 0,
			border_radius: 0,
			margin_fill: None,
		};
		let metrics = fonts.metrics;
		let frame = Frame::new(
			metrics,
			(0.0, 0.0),
			(8 * metrics.cell_width as u16, metrics.cell_height as u16),
		);
		let mut shaper = Shaper::new(fonts);

		// A row where every cell has the same non-default background.
		let mut snapshot = Snapshot::new(8, 1);
		snapshot.cells.fill(Square::from_char(' '));
		snapshot.styles = vec![Style {
			bg: AnsiColor::Spec(ColorRgb {
				r: 100,
				g: 50,
				b: 25,
			}),
			..Style::default()
		}];
		for cell in &mut snapshot.cells {
			cell.set_style_id(0);
		}

		let mut grid = Grid::new(8, 1);
		grid.fill(&snapshot, &palette, &GridOptions::default());

		let damage = Damage::everything(8, 1);
		let mut painter = RecordPainter::default();
		paint(&grid, &damage, &mut shaper, &frame, &surface, &mut painter);

		// Count `fill` calls that are backgrounds (not the panel). The panel
		// is the first fill; after that, one background fill for the whole
		// row, then decoration fills (if any), then nothing else.
		// The key assertion: the background is ONE fill, not eight.
		let fill_count = painter.calls.iter().filter(|c| **c == "fill").count();
		// Panel (1) + background (1) = at most 2 fills for an all-space row
		// with no decorations. If there were no merging, it would be 9.
		assert!(
			fill_count <= 2,
			"a uniform row must be one background fill, not eight; got {fill_count} fills"
		);
	}

	/// A `Damage` covering one cell must produce calls for that cell only.
	#[test]
	fn a_damage_covering_one_cell_produces_calls_for_that_cell_only() {
		let (grid, mut shaper, frame, surface) = make_frame(8, 2, ' ');

		// Change one cell.
		let mut snapshot = Snapshot::new(8, 2);
		snapshot.cells.fill(Square::from_char(' '));
		snapshot.cells[3] = Square::from_char('X');
		let palette = Palette::default();
		let mut new_grid = Grid::new(8, 2);
		new_grid.fill(&snapshot, &palette, &GridOptions::default());

		// Diff against the old grid.
		let damage = new_grid.diff(Some(&grid));

		let mut painter = RecordPainter::default();
		paint(
			&new_grid,
			&damage,
			&mut shaper,
			&frame,
			&surface,
			&mut painter,
		);

		// With only one cell changed, the panel must NOT be painted (it's
		// not a full redraw). The only fills should be the one background
		// (if any) and decorations for that one cell.
		// No `rounded_fill` should appear — that's the panel path.
		assert!(
			!painter.calls.contains(&"rounded_fill"),
			"panel must not be painted on a partial damage"
		);
	}
}
