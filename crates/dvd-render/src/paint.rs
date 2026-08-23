//! One reduction from model to shapes, and the backends that receive it.
//!
//! `render.rs` and `encode/svg.rs` were two independent implementations of the
//! same drawing. Background runs, cursor geometry, decorations and wide-cell
//! handling were each written twice, and they had already drifted — the SVG
//! had no bidirectional reordering and no combining marks, the rasterizer
//! resolved every cell's palette four separate times per row, and each carried
//! its own copy of the shaping and outline caches. The fix is not to maintain
//! them better in step; it is to have one place that decides what a frame *is*
//! and hand that decision to both. [`paint`] is that place.
//!
//! Paint order is a terminal's, and it is load-bearing:
//!
//! 1. canvas fill and panel — only when `damage.is_everything()`
//! 2. cell backgrounds, merged into runs via [`Grid::background_runs`]
//! 3. graphics with negative `z`
//! 4. the cursor — a block cursor is a *background*, painted before the
//!    glyphs, which is why the glyph on top is recoloured rather than the
//!    cell inverted
//! 5. drawn characters (box drawing, blocks, braille — see [`crate::sprite`])
//! 6. glyphs
//! 7. underlines and strikethroughs
//! 8. graphics with non-negative `z`
//!
//! A [`Painter`] that only wants changed cells passes a real [`Damage`]; one
//! that wants the whole frame passes [`Damage::everything`].
//!
//! ## Why the shaper is borrowed twice
//!
//! [`paint`] takes the shaper and the outline cache separately, and asks the
//! shaper for a [`RowKey`] before asking for the layout. Both are forced by the
//! borrow checker and both are load-bearing: a painter drawing outlines needs
//! the layout *and* the faces at the same time, which a single
//! `&mut self -> &RowLayout` call makes impossible. See [`crate::shape`].

use rio_vt::ansi::CursorShape;
use rio_vt::crosswords::style::StyleFlags;
use vello_cpu::kurbo::BezPath;

use crate::geom::{Cell, Color, Frame, PixelRect, Point, Span};
use crate::grid::{Caret, Damage, Grid};
use crate::model::{Placement, ResolvedCell};
use crate::outline::{GlyphKey, Outlines};
use crate::render::Surface;
use crate::shape::{Faces, GlyphRun, RowLayout, Shaper};
use crate::sprite::{self, Stroke, Underline, Weights};

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

	/// Fill a rectangle with rounded corners. Used for the panel and for any
	/// margin fill the surface carries.
	fn rounded_fill(&mut self, rect: PixelRect, radius: f32, color: Color);

	/// Fill a path, in canvas coordinates.
	///
	/// Only the drawn shapes that are not rectangles reach this — rounded box
	/// corners, diagonals and undercurls. A *fill* rather than a stroke
	/// because it is the one operation both backends perform identically:
	/// `vello_cpu` fills, an SVG `<path>` with no `stroke` fills, and neither
	/// has to agree about how a stroke is expanded or capped.
	fn path(&mut self, path: &BezPath, color: Color);

	/// Draw a run of glyphs sharing a colour, a set of attributes and a
	/// direction. See [`GlyphRun`] for what a backend may read off it.
	fn glyphs(&mut self, run: GlyphRun<'_>, origin: Point, context: &Text<'_>);

	/// Composite a placed image, clipped to `clip`.
	fn image(&mut self, placement: &Placement, clip: PixelRect);
}

/// The read-only text machinery a painter needs to turn a [`GlyphRun`] into
/// ink.
///
/// Bundled into one borrow rather than passed as three parameters, because
/// the set is fixed and every backend that draws glyphs at all needs the
/// whole of it: the faces to resolve a glyph back to its file, the outlines
/// to get its shape, and the size the outlines were extracted at.
pub struct Text<'a> {
	pub faces: &'a Faces,
	pub outlines: &'a Outlines,
	pub size: f32,
}

/// Draw one frame into a painter.
///
/// See the module doc for the paint order and why each step is where it is.
pub fn paint(
	grid: &Grid,
	damage: &Damage,
	shaper: &mut Shaper,
	outlines: &mut Outlines,
	frame: &Frame,
	surface: &Surface,
	painter: &mut impl Painter,
) {
	// 1. Canvas fill and panel — only on a full redraw. When the damage is
	// partial the panel is already on screen from the previous frame, and
	// repainting it would cover the cells that were not redrawn.
	if damage.is_everything() {
		if let Some(fill) = surface.margin_fill {
			painter.fill(frame.canvas_rect(), fill);
		}
		let panel = surface.panel_rect(frame);
		if surface.border_radius > 0 {
			painter.rounded_fill(panel, surface.border_radius as f32, grid.background);
		} else {
			painter.fill(panel, grid.background);
		}
	}

	// 2. Cell backgrounds, merged into runs. `background_runs` skips the panel
	// colour — painting it again would be a rectangle that changes nothing.
	for row in damage.rows() {
		for (span, color) in grid.background_runs(row.row) {
			painter.fill(frame.span_rect(span), color);
		}
	}

	// 3. Graphics beneath the text.
	paint_images(painter, grid, frame, |z| z < 0);

	// 4. The cursor — a block cursor is a background, painted before the
	// glyphs. The glyph on top is recoloured (`Caret::text`, already resolved)
	// rather than the cell inverted.
	if let Some(caret) = grid.cursor {
		paint_cursor(painter, caret, frame);
	}

	// 5 and 6. Drawn characters and shaped ones, row by row. Both walk the
	// same damaged rows, and the sprite pass goes first so a drawn character
	// never lands on top of a glyph that overhangs into its cell.
	let weights = Weights::from_thickness(frame.metrics.underline_thickness);
	let mut strokes = Vec::new();

	for row in damage.rows() {
		paint_sprites(painter, grid, row, frame, weights, &mut strokes);

		let key = shaper.ensure_row(grid, row.row);
		let layout = shaper.layout(key);
		if layout.glyphs.is_empty() {
			continue;
		}

		// Outlines are extracted before the run walk rather than during it:
		// `ensure` needs `&mut outlines` and the walk hands out `&outlines`,
		// and doing it here also means a painter never has to own the
		// extraction path itself.
		for glyph in &layout.glyphs {
			if let Some(face) = shaper.faces().get(glyph.font) {
				outlines.ensure(
					GlyphKey {
						font: glyph.font.0,
						glyph: glyph.identifier,
					},
					face,
					shaper.size(),
				);
			}
		}

		let context = Text {
			faces: shaper.faces(),
			outlines,
			size: shaper.size(),
		};
		paint_glyphs(painter, layout, grid, row, frame, &context);
	}

	// 7. Underlines and strikethroughs, over the glyphs they belong to.
	for row in damage.rows() {
		paint_decorations(painter, grid, row, frame, &mut strokes);
	}

	// 8. Graphics over the text.
	paint_images(painter, grid, frame, |z| z >= 0);
}

fn paint_images(
	painter: &mut impl Painter,
	grid: &Grid,
	frame: &Frame,
	wanted: impl Fn(i32) -> bool,
) {
	let clip = frame.canvas_rect();
	for placement in grid.graphics.iter().filter(|p| wanted(p.z)) {
		painter.image(placement, clip);
	}
}

/// Paint the cursor, respecting its shape and the width of what it sits on.
fn paint_cursor(painter: &mut impl Painter, caret: Caret, frame: &Frame) {
	let rect = frame.span_rect(caret.span());
	let thickness = (frame.metrics.underline_thickness * 1.5).round().max(2.0);

	let shape = match caret.shape {
		// The full run — two columns when the caret is on a wide character.
		CursorShape::Block => rect,
		CursorShape::Underline => {
			PixelRect::new(rect.x, rect.bottom() - thickness, rect.width, thickness)
		}
		// A beam marks an insertion point, which is a position rather than a
		// character, so it stays one stroke wide over a wide cell.
		CursorShape::Beam => PixelRect::new(rect.x, rect.y, thickness, rect.height),
		CursorShape::Hidden => return,
	};

	painter.fill(shape.snapped(), caret.color);
}

/// Draw the characters the grid renders itself, cell by cell.
fn paint_sprites(
	painter: &mut impl Painter,
	grid: &Grid,
	row: Span,
	frame: &Frame,
	weights: Weights,
	strokes: &mut Vec<Stroke>,
) {
	let cells = grid.row(row.row);

	for column in row.start..row.end.min(grid.columns) {
		let cell = &cells[column as usize];
		if !sprite::covers(cell.character) {
			continue;
		}

		strokes.clear();
		let at = Cell::new(column, row.row);
		if !sprite::draw(cell.character, frame.cell_rect(at), weights, strokes) {
			continue;
		}

		let color = ink(grid, cell, at);
		emit(painter, strokes, color);
	}
}

/// Hand a batch of drawn strokes to the painter in one colour.
fn emit(painter: &mut impl Painter, strokes: &[Stroke], color: Color) {
	for stroke in strokes {
		match stroke {
			Stroke::Rect { rect, alpha } => painter.fill(*rect, color.with_alpha(*alpha)),
			Stroke::Path { path, alpha } => painter.path(path, color.with_alpha(*alpha)),
		}
	}
}

/// Paint one row's glyphs, grouped into runs that share everything a backend
/// would have to change state for.
///
/// A run breaks on colour, on attributes, and on bidirectional direction. The
/// first two are what a rasterizer cares about; the third is what a backend
/// emitting text cares about, and putting all three here is what keeps the two
/// from splitting runs differently and drawing different pictures.
fn paint_glyphs(
	painter: &mut impl Painter,
	layout: &RowLayout,
	grid: &Grid,
	row: Span,
	frame: &Frame,
	context: &Text<'_>,
) {
	let origin = frame.row_origin(row.row);
	let cells = grid.row(row.row);
	let last_column = cells.len().saturating_sub(1);

	let attributes = |column: u16| {
		let cell = &cells[(column as usize).min(last_column)];
		let at = Cell::new(column, row.row);
		(ink(grid, cell, at), cell.flags)
	};

	let mut start = 0usize;
	while start < layout.glyphs.len() {
		let (color, flags) = attributes(layout.glyphs[start].column);
		let direction = direction_at(layout, layout.glyphs[start].column);

		let mut end = start + 1;
		while end < layout.glyphs.len() {
			let column = layout.glyphs[end].column;
			if attributes(column) != (color, flags) || direction_at(layout, column) != direction {
				break;
			}
			end += 1;
		}

		let glyphs = &layout.glyphs[start..end];
		let first_column = glyphs[0].column;
		let final_column = glyphs[glyphs.len() - 1].column;
		// A run ending on a double-width character covers both of its
		// columns. Measuring the span by column count alone would make the
		// run one cell narrow, which a backend sizing markup off it turns
		// into every CJK line squeezed to the left.
		let final_width = match cells.get(final_column as usize).map(|cell| cell.wide) {
			Some(rio_vt::crosswords::square::Wide::Wide) => 2,
			_ => 1,
		};

		painter.glyphs(
			GlyphRun {
				span: Span::new(row.row, first_column, final_column + final_width),
				glyphs,
				text: &layout.text,
				range: byte_range(layout, first_column, final_column),
				columns: &layout.column_by_byte_index,
				color,
				flags,
				right_to_left: direction,
			},
			origin,
			context,
		);

		start = end;
	}
}

/// Whether the bidirectional run covering a column runs right to left.
fn direction_at(layout: &RowLayout, column: u16) -> bool {
	layout
		.column_by_byte_index
		.iter()
		.position(|&at| at == column)
		.and_then(|byte| {
			layout
				.bidirectional_runs
				.iter()
				.find(|run| byte >= run.start && byte < run.end)
		})
		.is_some_and(|run| run.right_to_left)
}

/// The byte range of `text` spanning two columns inclusive.
fn byte_range(layout: &RowLayout, first: u16, last: u16) -> std::ops::Range<usize> {
	let start = layout
		.column_by_byte_index
		.iter()
		.position(|&column| column == first)
		.unwrap_or(0);
	let end = layout
		.column_by_byte_index
		.iter()
		.rposition(|&column| column == last)
		.map_or(start, |index| index + 1);

	start..end.max(start)
}

/// The colour a cell's ink is drawn in, accounting for a block cursor sitting
/// on top of it.
///
/// A block cursor paints over the cell, so whatever is underneath has to be
/// recoloured or it disappears into the caret. `Caret::text` is already
/// resolved by `Color::contrasting`, so this is a lookup rather than a
/// computation.
fn ink(grid: &Grid, cell: &ResolvedCell, at: Cell) -> Color {
	match grid.cursor {
		Some(caret)
			if caret.shape == CursorShape::Block
				&& caret.at.row == at.row
				&& caret.span().contains(at.column) =>
		{
			caret.text
		}
		_ => cell.foreground,
	}
}

/// Paint underlines and strikethroughs for a row.
///
/// Merged into runs of one style and colour before being drawn. A rule under
/// a word is one continuous stroke in a terminal, and drawing it as one
/// rectangle per cell leaves a seam at every cell boundary once anti-aliasing
/// has had its say — the same failure the sprite pass exists to prevent for
/// box drawing.
fn paint_decorations(
	painter: &mut impl Painter,
	grid: &Grid,
	row: Span,
	frame: &Frame,
	strokes: &mut Vec<Stroke>,
) {
	let cells = grid.row(row.row);
	let metrics = frame.metrics;
	let end = row.end.min(grid.columns);

	let decoration = |column: u16| {
		let cell = &cells[column as usize];
		(
			Underline::from_flags(cell.flags),
			cell.underline.unwrap_or(cell.foreground),
			cell.flags.contains(StyleFlags::STRIKEOUT),
			cell.foreground,
		)
	};

	let mut column = row.start;
	while column < end {
		let (kind, color, struck, foreground) = decoration(column);
		if kind.is_none() && !struck {
			column += 1;
			continue;
		}

		let mut last = column;
		while last + 1 < end && decoration(last + 1) == (kind, color, struck, foreground) {
			last += 1;
		}

		let span = Span::new(row.row, column, last + 1);
		let rect = frame.span_rect(span);

		if let Some(kind) = kind {
			strokes.clear();
			sprite::underline(kind, rect, &metrics, strokes);
			emit(painter, strokes, color);
		}

		if struck {
			let top = rect.y + metrics.strikeout_offset;
			painter.fill(
				PixelRect::new(rect.x, top, rect.width, metrics.strikeout_thickness).snapped(),
				foreground,
			);
		}

		column = last + 1;
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::fonts::Fonts;
	use crate::geom::Size;
	use crate::grid::{Grid, GridOptions};
	use crate::model::{Palette, Snapshot};
	use rio_vt::config::colors::{AnsiColor, ColorRgb};
	use rio_vt::crosswords::square::{Square, Wide};
	use rio_vt::crosswords::style::Style;

	/// Records every call it receives, so a test can assert paint order and
	/// run merging without reading pixels.
	#[derive(Default)]
	struct Record {
		calls: Vec<&'static str>,
		fills: Vec<(PixelRect, Color)>,
		runs: Vec<(Span, Color, bool)>,
	}

	impl Record {
		fn first_index(&self, call: &str) -> Option<usize> {
			self.calls.iter().position(|seen| *seen == call)
		}

		fn count(&self, call: &str) -> usize {
			self.calls.iter().filter(|seen| **seen == call).count()
		}
	}

	impl Painter for Record {
		fn fill(&mut self, rect: PixelRect, color: Color) {
			self.calls.push("fill");
			self.fills.push((rect, color));
		}
		fn rounded_fill(&mut self, _rect: PixelRect, _radius: f32, _color: Color) {
			self.calls.push("rounded_fill");
		}
		fn path(&mut self, _path: &BezPath, _color: Color) {
			self.calls.push("path");
		}
		fn glyphs(&mut self, run: GlyphRun<'_>, _origin: Point, _context: &Text<'_>) {
			self.calls.push("glyphs");
			self.runs.push((run.span, run.color, run.right_to_left));
		}
		fn image(&mut self, _placement: &Placement, _clip: PixelRect) {
			self.calls.push("image");
		}
	}

	struct Harness {
		grid: Grid,
		shaper: Shaper,
		outlines: Outlines,
		frame: Frame,
		surface: Surface,
		palette: Palette,
	}

	impl Harness {
		fn new(columns: u16, rows: u16) -> Self {
			let fonts = Fonts::resolve(Some("Liberation Mono"), 16.0, 1.0).unwrap();
			let metrics = fonts.metrics;
			Self {
				grid: Grid::new(columns, rows),
				shaper: Shaper::new(fonts),
				outlines: Outlines::new(),
				frame: Frame::new(
					metrics,
					Point::ORIGIN,
					Size::new(
						columns * metrics.cell_width as u16,
						rows * metrics.cell_height as u16,
					),
				),
				surface: Surface {
					margin: 0,
					padding: 0,
					border_radius: 0,
					margin_fill: None,
				},
				palette: Palette::default(),
			}
		}

		fn load(&mut self, snapshot: &Snapshot) {
			self.grid
				.fill(snapshot, &self.palette, &GridOptions::default());
		}

		fn run(&mut self, damage: &Damage) -> Record {
			let mut painter = Record::default();
			paint(
				&self.grid,
				damage,
				&mut self.shaper,
				&mut self.outlines,
				&self.frame,
				&self.surface,
				&mut painter,
			);
			painter
		}
	}

	fn screen(columns: u16, rows: u16, line: &str) -> Snapshot {
		let mut snapshot = Snapshot::new(columns, rows);
		snapshot.cells.fill(Square::from_char(' '));
		for (index, character) in line.chars().take(columns as usize).enumerate() {
			snapshot.cells[index] = Square::from_char(character);
		}
		snapshot
	}

	/// The order is the contract. Everything else in this module is an
	/// implementation detail; this is the thing both backends rely on.
	#[test]
	fn a_full_redraw_paints_the_panel_then_backgrounds_then_glyphs() {
		let mut harness = Harness::new(8, 2);
		harness.load(&screen(8, 2, "Wide"));
		let painter = harness.run(&Damage::everything(8, 2));

		assert_eq!(
			painter.calls.first(),
			Some(&"fill"),
			"the panel is painted first on a full redraw"
		);
		let glyphs = painter
			.first_index("glyphs")
			.expect("a row of text must draw glyphs");
		assert!(glyphs > 0, "backgrounds and panel come before glyphs");
	}

	/// A partial damage must not repaint the panel — doing so would cover
	/// every cell the frame deliberately left alone.
	#[test]
	fn a_partial_damage_leaves_the_panel_alone() {
		let mut harness = Harness::new(8, 2);
		harness.load(&screen(8, 2, "        "));
		let before = harness.grid.diff(None);
		assert!(before.is_everything());

		let mut changed = screen(8, 2, "        ");
		changed.cells[3] = Square::from_char('X');

		let mut previous = Grid::new(8, 2);
		previous.fill(&screen(8, 2, "        "), &harness.palette, &GridOptions::default());
		harness.load(&changed);
		let damage = harness.grid.diff(Some(&previous));

		let painter = harness.run(&damage);
		assert_eq!(painter.count("rounded_fill"), 0);
		assert!(
			!damage.is_everything(),
			"one changed cell is not a full redraw"
		);
	}

	/// A row of one background colour must become one rectangle, not eighty.
	#[test]
	fn a_uniform_row_of_background_becomes_a_single_fill() {
		let mut harness = Harness::new(8, 1);

		let mut snapshot = screen(8, 1, "        ");
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
		harness.load(&snapshot);

		let painter = harness.run(&Damage::everything(8, 1));

		// Panel plus one merged background run. Eight would mean no merging.
		assert!(
			painter.count("fill") <= 2,
			"expected the row to merge into one fill, got {} fills",
			painter.count("fill")
		);
	}

	/// A block cursor on a double-width character has to cover both of its
	/// columns. Covering one leaves the other half of the character showing
	/// through the caret, which reads as a rendering fault.
	#[test]
	fn a_block_cursor_covers_both_columns_of_a_wide_character() {
		let mut harness = Harness::new(8, 1);

		let mut snapshot = screen(8, 1, "        ");
		let mut wide = Square::from_char('世');
		wide.set_wide(Wide::Wide);
		snapshot.cells[2] = wide;
		let mut spacer = Square::from_char(' ');
		spacer.set_wide(Wide::Spacer);
		snapshot.cells[3] = spacer;
		snapshot.cursor_visible = true;
		snapshot.cursor.content = CursorShape::Block;
		snapshot.cursor.pos.col = rio_vt::crosswords::pos::Column(2);
		harness.load(&snapshot);

		let caret = harness.grid.cursor.expect("the cursor is visible");
		assert_eq!(caret.columns, 2, "the caret widens to the character");

		let cell_width = harness.frame.metrics.cell_width as f32;
		let painter = harness.run(&Damage::everything(8, 1));
		let cursor_fill = painter
			.fills
			.iter()
			.find(|(_, color)| *color == caret.color)
			.expect("the caret must be painted");

		assert_eq!(
			cursor_fill.0.width,
			cell_width * 2.0,
			"the caret must span both columns"
		);
	}

	/// A caret reported on the trailing half of a wide character belongs to
	/// the character. Drawn where the terminal literally said, it straddles
	/// the boundary between two characters.
	#[test]
	fn a_cursor_on_a_spacer_snaps_back_to_the_character_it_belongs_to() {
		let mut harness = Harness::new(8, 1);

		let mut snapshot = screen(8, 1, "        ");
		let mut wide = Square::from_char('世');
		wide.set_wide(Wide::Wide);
		snapshot.cells[4] = wide;
		let mut spacer = Square::from_char(' ');
		spacer.set_wide(Wide::Spacer);
		snapshot.cells[5] = spacer;
		snapshot.cursor_visible = true;
		snapshot.cursor.content = CursorShape::Block;
		snapshot.cursor.pos.col = rio_vt::crosswords::pos::Column(5);
		harness.load(&snapshot);

		let caret = harness.grid.cursor.expect("the cursor is visible");
		assert_eq!(caret.at.column, 4, "the caret snaps back to the base cell");
		assert_eq!(caret.columns, 2);
	}

	/// Box drawing is drawn rather than shaped, so it must reach the painter
	/// as fills — and must not also arrive as a glyph, which would lay the
	/// same ink down twice.
	#[test]
	fn box_drawing_arrives_as_drawn_fills_and_not_as_glyphs() {
		let mut harness = Harness::new(4, 1);
		harness.load(&screen(4, 1, "────"));

		let painter = harness.run(&Damage::everything(4, 1));

		assert_eq!(
			painter.count("glyphs"),
			0,
			"a row of pure box drawing shapes nothing"
		);
		assert!(
			painter.count("fill") > 1,
			"but it still puts rectangles on the canvas"
		);
	}

	/// The underline styles have to survive the trip through the paint pass
	/// as different shapes — a curl is a path, a plain rule is a rectangle.
	#[test]
	fn an_undercurl_reaches_the_painter_as_a_path() {
		let mut harness = Harness::new(4, 1);

		let mut snapshot = screen(4, 1, "abcd");
		snapshot.styles = vec![Style {
			flags: StyleFlags::UNDERCURL,
			..Style::default()
		}];
		for cell in &mut snapshot.cells {
			cell.set_style_id(0);
		}
		harness.load(&snapshot);

		let painter = harness.run(&Damage::everything(4, 1));
		assert_eq!(
			painter.count("path"),
			1,
			"the whole underlined run is one curl"
		);
	}

	/// A rule under a word is one stroke in a terminal. Drawing it per cell
	/// leaves an anti-aliasing seam at every boundary.
	#[test]
	fn a_continuous_underline_is_merged_into_one_stroke() {
		let mut harness = Harness::new(6, 1);

		let mut snapshot = screen(6, 1, "abcdef");
		snapshot.styles = vec![Style {
			flags: StyleFlags::UNDERLINE,
			..Style::default()
		}];
		for cell in &mut snapshot.cells {
			cell.set_style_id(0);
		}
		harness.load(&snapshot);

		let painter = harness.run(&Damage::everything(6, 1));
		let cell_width = harness.frame.metrics.cell_width as f32;

		let underline = painter
			.fills
			.iter()
			.find(|(rect, _)| rect.width >= cell_width * 6.0)
			.map(|(rect, _)| *rect);

		assert!(
			underline.is_some(),
			"six underlined cells should merge into one rule, got fills {:?}",
			painter.fills.iter().map(|(rect, _)| rect.width).collect::<Vec<_>>()
		);
	}

	/// Glyph runs break where a backend would have to change state. Two
	/// differently coloured words must not arrive as one run.
	#[test]
	fn a_colour_change_breaks_a_glyph_run() {
		let mut harness = Harness::new(6, 1);

		let mut snapshot = screen(6, 1, "abcdef");
		snapshot.styles = vec![
			Style::default(),
			Style {
				fg: AnsiColor::Spec(ColorRgb { r: 255, g: 0, b: 0 }),
				..Style::default()
			},
		];
		for (index, cell) in snapshot.cells.iter_mut().enumerate() {
			cell.set_style_id(if index < 3 { 0 } else { 1 });
		}
		harness.load(&snapshot);

		let painter = harness.run(&Damage::everything(6, 1));

		assert_eq!(painter.runs.len(), 2, "one run per colour");
		assert_ne!(painter.runs[0].1, painter.runs[1].1);
		assert_eq!(painter.runs[0].0.start, 0);
		assert_eq!(painter.runs[1].0.start, 3);
	}
}
