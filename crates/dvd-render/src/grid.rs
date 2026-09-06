//! One frame, resolved once.
//!
//! Before this module existed, "what colour is this cell" was a question every
//! drawing pass answered for itself. A row of eighty columns went through
//! `Palette::resolve` in `draw_backgrounds` to learn the background, again in
//! `draw_row_text` to learn the foreground, again in `cell_foreground` to
//! decide whether the cursor was sitting on it, and a fourth time in
//! `draw_decorations` to find the underline colour — four resolutions of the
//! same `Square` against the same style table, throwing three of the four
//! answers away. [`Grid`] is the fix: [`Grid::fill`] walks the snapshot once,
//! calls [`crate::model::Palette::resolve`] exactly once per cell, and keeps
//! the answer around for every pass that follows.
//!
//! Keeping the answer around is also what makes a second frame cheap to
//! compare against a first. [`Grid::diff`] does not know or care what changed
//! — a keystroke, a repaint, a palette swap — it only compares the resolved
//! cells two grids ended up with. That comparison is [`Damage`], and it is
//! deliberately one type rather than three: the SVG sink wants it to decide
//! which frame group a run of unchanged pixels can be folded into, the
//! rasterizer wants it to skip the cells a still prompt never touches, and a
//! test wants it to assert exactly which cell a keystroke reached. Three
//! consumers maintaining three notions of "what changed" is how they drift;
//! one mechanism, computed one way, cannot.
//!
//! Resolving eagerly costs an allocation only once. [`Grid::fill`] reuses the
//! `Vec` it was given rather than building a fresh one per frame — a
//! recording is thousands of frames, and a fresh `columns * rows` allocation
//! for each one is exactly the kind of steady-state churn [`crate::stream`]
//! goes out of its way to avoid everywhere else in the pipeline.

use rio_vt::ansi::CursorShape;
use rio_vt::config::colors::NamedColor;
use rio_vt::crosswords::square::Wide;
use rio_vt::crosswords::style::StyleFlags;
use rustc_hash::FxHashMap;

use crate::geom::{Cell, Color, Span};
use crate::model::{Palette, Placement, ResolvedCell, Snapshot};

/// One frame with every cell already resolved to concrete colours.
pub struct Grid {
	pub columns: u16,
	pub rows: u16,
	/// Row-major, `len == columns * rows`. Never shrunk by [`Grid::fill`],
	/// only ever cleared and refilled, so a recording's steady-state cost is
	/// one allocation rather than one per frame.
	cells: Vec<ResolvedCell>,
	pub cursor: Option<Caret>,
	pub graphics: Vec<Placement>,
	/// Zero-width characters keyed by the ids carried in resolved cells.
	/// Keeping these out of [`ResolvedCell`] preserves its compact `Copy` shape.
	extras: FxHashMap<u16, Vec<char>>,
	/// The panel colour, read once per fill so [`Grid::background_runs`] does
	/// not have to ask the palette again for every row.
	pub background: Color,
}

impl Grid {
	/// An empty grid of the given size, every cell blank.
	pub fn new(columns: u16, rows: u16) -> Self {
		let blank = ResolvedCell {
			character: ' ',
			extras_id: None,
			foreground: Color::TRANSPARENT,
			background: Color::TRANSPARENT,
			underline: None,
			flags: StyleFlags::empty(),
			wide: Wide::Narrow,
		};

		Self {
			columns,
			rows,
			cells: vec![blank; columns as usize * rows as usize],
			cursor: None,
			graphics: Vec::new(),
			extras: FxHashMap::default(),
			background: Color::TRANSPARENT,
		}
	}

	/// Fold a snapshot into resolved cells, reusing this `Grid`'s allocations.
	///
	/// This is where [`Palette::resolve`] is called — exactly once per cell,
	/// which is the entire reason this type exists. `clear` keeps the `Vec`'s
	/// capacity, so a fill at the same size as the last one never touches the
	/// allocator.
	pub fn fill(&mut self, snapshot: &Snapshot, palette: &Palette, options: &GridOptions) {
		self.columns = snapshot.columns;
		self.rows = snapshot.screen_rows;
		self.background = palette.named(NamedColor::Background);

		let count = self.columns as usize * self.rows as usize;
		self.cells.clear();
		self.cells.reserve(count);
		for row in 0..self.rows {
			for column in 0..self.columns {
				self.cells.push(palette.resolve(
					snapshot.cell(column, row),
					&snapshot.styles,
					options,
				));
			}
		}

		self.graphics.clear();
		self.graphics.extend(snapshot.graphics.iter().cloned());

		self.extras.clear();
		for cell in &self.cells {
			let Some(id) = cell.extras_id else {
				continue;
			};
			let Some(extras) = snapshot.extras.get(&id) else {
				continue;
			};
			if !extras.zerowidth.is_empty() {
				self.extras.insert(id, extras.zerowidth.clone());
			}
		}

		// After the cells, because the caret's width is a property of the
		// character underneath it.
		self.cursor = Caret::resolve(snapshot, &self.cells, self.columns, palette);
	}

	pub fn cell(&self, at: Cell) -> &ResolvedCell {
		&self.cells[at.row as usize * self.columns as usize + at.column as usize]
	}

	pub fn row(&self, row: u16) -> &[ResolvedCell] {
		let start = row as usize * self.columns as usize;
		&self.cells[start..start + self.columns as usize]
	}

	/// The zero-width characters attached to a resolved cell.
	#[inline]
	pub fn zero_width(&self, cell: &ResolvedCell) -> &[char] {
		cell.extras_id
			.and_then(|id| self.extras.get(&id))
			.map_or(&[], Vec::as_slice)
	}

	/// Horizontal runs of one background colour, skipping the panel colour.
	///
	/// Merging runs is what turns eighty rectangles into one: a terminal row
	/// is mostly one colour, and the rasterizer and the SVG writer both pay
	/// per rectangle, not per cell. The loop keeps starting a fresh run at the
	/// first cell that has not yet been claimed by one, which is the same
	/// sentinel-free way of doing it — no comparison against an off-the-end
	/// column is needed because the loop simply stops when it runs out of
	/// cells to look at, rather than manufacturing a colour that can never
	/// occur to force the last run to flush.
	pub fn background_runs(&self, row: u16) -> impl Iterator<Item = (Span, Color)> + '_ {
		let panel = self.background;
		let cells = self.row(row);
		let mut column = 0usize;

		std::iter::from_fn(move || {
			while column < cells.len() {
				let start = column;
				let color = cells[column].background;
				while column < cells.len() && cells[column].background == color {
					column += 1;
				}
				if color != panel {
					return Some((Span::new(row, start as u16, column as u16), color));
				}
			}
			None
		})
	}

	/// Which cells differ from `previous`. `None` means "assume everything".
	///
	/// A resize is folded into "everything" rather than compared cell by
	/// cell: once the column count changes, index `n` in one grid and index
	/// `n` in the other no longer name the same screen position, so nothing
	/// short of a full redraw is a truthful answer.
	pub fn diff(&self, previous: Option<&Grid>) -> Damage {
		let Some(previous) = previous else {
			return Damage::everything(self.columns, self.rows);
		};

		if previous.columns != self.columns || previous.rows != self.rows {
			return Damage::everything(self.columns, self.rows);
		}

		let mut rows: Vec<Option<Span>> = (0..self.rows)
			.map(|row| row_span(row, self.row(row), previous.row(row)))
			.collect();

		// The cursor is not baked into any cell's `ResolvedCell` — it is an
		// overlay applied at paint time — so a caret that moved across two
		// otherwise-identical cells would be invisible to the loop above.
		// Both the cells it left and the cells it entered have to be touched
		// by hand, and "cells" is plural: a caret on a wide character covers
		// two columns and leaves two behind.
		if self.cursor != previous.cursor {
			for caret in [self.cursor, previous.cursor].into_iter().flatten() {
				touch(&mut rows, caret.span());
			}
		}

		Damage::partial(rows)
	}
}

/// The cursor, already resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caret {
	pub at: Cell,
	/// How many columns the cursor covers — two when it sits on a
	/// double-width character.
	///
	/// A caret is a highlight of *the character under it*, not of a fixed
	/// slice of the grid. On a CJK character a one-column block cursor covers
	/// its left half and leaves the right half showing through, which reads as
	/// a rendering fault rather than as a cursor. Every terminal that handles
	/// wide characters at all widens the caret to match, and it has to be
	/// decided here rather than at paint time because the damage diff needs
	/// the same answer.
	pub columns: u16,
	pub shape: CursorShape,
	pub color: Color,
	/// Ink for the glyph the cursor sits on, when the shape covers it.
	///
	/// Computed by [`Color::contrasting`] rather than by inverting the cell:
	/// inverting a mid-grey cursor produces another mid-grey, and the glyph
	/// vanishes. Only a block cursor actually paints over the glyph, but the
	/// colour costs nothing to compute for the other shapes and it means the
	/// painter never has to ask the palette itself.
	pub text: Color,
}

impl Caret {
	/// The run of cells the caret covers.
	#[inline]
	pub fn span(self) -> Span {
		Span::new(self.at.row, self.at.column, self.at.column + self.columns)
	}

	fn resolve(snapshot: &Snapshot, cells: &[ResolvedCell], columns: u16, palette: &Palette) -> Option<Self> {
		if !snapshot.cursor_visible || snapshot.cursor.content == CursorShape::Hidden {
			return None;
		}

		let column = snapshot.cursor.pos.col.0 as u32;
		let row = snapshot.cursor.pos.row.0.max(0) as u32;
		if column >= columns as u32 || row >= snapshot.screen_rows as u32 {
			return None;
		}

		let (mut column, row) = (column as u16, row as u16);
		let index = |column: u16| row as usize * columns as usize + column as usize;

		// A cursor reported on the second half of a wide character belongs to
		// the character, not to the placeholder: drawn where the terminal
		// literally said, it straddles the boundary and highlights the right
		// half of one character and the left half of the next.
		if cells
			.get(index(column))
			.is_some_and(|cell| cell.wide == Wide::Spacer)
			&& column > 0
		{
			column -= 1;
		}

		let width = match cells.get(index(column)).map(|cell| cell.wide) {
			Some(Wide::Wide) => 2,
			_ => 1,
		};

		let color = palette.named(NamedColor::Cursor);
		Some(Self {
			at: Cell::new(column, row),
			columns: width,
			shape: snapshot.cursor.content,
			color,
			text: color.contrasting(),
		})
	}
}

/// Which cells a frame changed.
///
/// One mechanism, three consumers: delta-encoded SVG frames fold an unchanged
/// stretch into the frame group before it, damage-aware rasterization skips
/// redrawing a cell nothing touched, and a test asserts exactly which column a
/// keystroke reached. Any one of those recomputing "changed" its own way is
/// how the three quietly stop agreeing; this is the one place it is decided.
pub struct Damage {
	/// Set only by [`Damage::everything`]. Cheaper than asking whether every
	/// row happens to span the full width, and it is what lets a painter skip
	/// the canvas fill and the panel on every frame but the first without
	/// re-deriving "is this actually all of it" from the spans themselves.
	full: bool,
	rows: Vec<Option<Span>>,
}

impl Damage {
	/// Every cell of a grid this size, for the first frame of a recording or
	/// for anything that followed a resize.
	pub fn everything(columns: u16, rows: u16) -> Self {
		Self {
			full: true,
			rows: (0..rows)
				.map(|row| Some(Span::new(row, 0, columns)))
				.collect(),
		}
	}

	fn partial(rows: Vec<Option<Span>>) -> Self {
		Self { full: false, rows }
	}

	pub fn is_everything(&self) -> bool {
		self.full
	}

	pub fn is_empty(&self) -> bool {
		!self.full && self.rows.iter().all(Option::is_none)
	}

	pub fn row(&self, row: u16) -> Option<Span> {
		self.rows.get(row as usize).copied().flatten()
	}

	pub fn rows(&self) -> impl Iterator<Item = Span> + '_ {
		self.rows.iter().filter_map(|span| *span)
	}
}

/// The span covering the lowest through highest column that differs between
/// two rows, or `None` when they match exactly.
fn row_span(row: u16, current: &[ResolvedCell], previous: &[ResolvedCell]) -> Option<Span> {
	let mut first = None;
	let mut last = None;

	for (column, (a, b)) in current.iter().zip(previous).enumerate() {
		if a != b {
			let column = column as u16;
			first.get_or_insert(column);
			last = Some(column);
		}
	}

	match (first, last) {
		(Some(start), Some(end)) => Some(Span::new(row, start, end + 1)),
		_ => None,
	}
}

/// Widen a row's damage to cover `covered`, starting a run if it had none.
fn touch(rows: &mut [Option<Span>], covered: Span) {
	if let Some(slot) = rows.get_mut(covered.row as usize) {
		*slot = Some(match *slot {
			Some(span) => Span::new(
				covered.row,
				span.start.min(covered.start),
				span.end.max(covered.end),
			),
			None => covered,
		});
	}
}

/// What resolution needs to know beyond the palette.
#[derive(Clone, Copy, Debug, Default)]
pub struct GridOptions {
	/// Map bold + ANSI 0..8 onto the bright cut, the way `st` and `xterm` do.
	///
	/// Off by default so wiring this type into the pipeline changes nothing a
	/// recording already looked like; a tape opts in deliberately.
	pub bold_is_bright: bool,
}

#[cfg(test)]
mod tests {
	use super::*;
	use rio_vt::config::colors::{AnsiColor, ColorRgb};
	use rio_vt::crosswords::square::Square;
	use rio_vt::crosswords::style::Style;

	fn screen(columns: u16, rows: u16, fill: char) -> Snapshot {
		let mut snapshot = Snapshot::new(columns, rows);
		snapshot.cells.fill(Square::from_char(fill));
		snapshot
	}

	#[test]
	fn fill_matches_what_palette_resolve_would_give_cell_for_cell() {
		let palette = Palette::default();
		let options = GridOptions::default();

		let mut snapshot = screen(3, 2, ' ');
		snapshot.styles = vec![Style {
			bg: AnsiColor::Spec(ColorRgb { r: 9, g: 8, b: 7 }),
			..Style::default()
		}];
		let mut styled = Square::from_char('z');
		styled.set_style_id(0);
		snapshot.cells[4] = styled;

		let mut grid = Grid::new(3, 2);
		grid.fill(&snapshot, &palette, &options);

		for row in 0..2 {
			for column in 0..3 {
				let expected =
					palette.resolve(snapshot.cell(column, row), &snapshot.styles, &options);
				assert_eq!(*grid.cell(Cell::new(column, row)), expected);
			}
		}
	}

	/// A recording is thousands of frames; a fresh `Vec` for each one would be
	/// the exact per-frame churn the rest of the pipeline goes out of its way
	/// to avoid.
	#[test]
	fn fill_reuses_the_grids_own_allocation_across_two_fills() {
		let palette = Palette::default();
		let options = GridOptions::default();
		let snapshot = screen(4, 2, 'a');

		let mut grid = Grid::new(4, 2);
		grid.fill(&snapshot, &palette, &options);
		let pointer = grid.cells.as_ptr();
		let capacity = grid.cells.capacity();

		grid.fill(&snapshot, &palette, &options);

		assert_eq!(
			grid.cells.as_ptr(),
			pointer,
			"a same-size fill must not reallocate"
		);
		assert_eq!(grid.cells.capacity(), capacity);
	}

	#[test]
	fn background_runs_merge_a_uniform_row_into_one_span() {
		let palette = Palette::default();
		let mut snapshot = screen(4, 1, ' ');
		snapshot.styles = vec![Style {
			bg: AnsiColor::Spec(ColorRgb {
				r: 10,
				g: 20,
				b: 30,
			}),
			..Style::default()
		}];
		for cell in &mut snapshot.cells {
			cell.set_style_id(0);
		}

		let mut grid = Grid::new(4, 1);
		grid.fill(&snapshot, &palette, &GridOptions::default());

		let runs: Vec<_> = grid.background_runs(0).collect();
		assert_eq!(runs, vec![(Span::new(0, 0, 4), Color::rgb(10, 20, 30))]);
	}

	#[test]
	fn background_runs_split_where_the_background_colour_changes() {
		let palette = Palette::default();
		let mut snapshot = screen(4, 1, ' ');
		snapshot.styles = vec![
			Style {
				bg: AnsiColor::Spec(ColorRgb { r: 1, g: 1, b: 1 }),
				..Style::default()
			},
			Style {
				bg: AnsiColor::Spec(ColorRgb { r: 2, g: 2, b: 2 }),
				..Style::default()
			},
		];
		for (column, cell) in snapshot.cells.iter_mut().enumerate() {
			cell.set_style_id(if column < 2 { 0 } else { 1 });
		}

		let mut grid = Grid::new(4, 1);
		grid.fill(&snapshot, &palette, &GridOptions::default());

		let runs: Vec<_> = grid.background_runs(0).collect();
		assert_eq!(
			runs,
			vec![
				(Span::new(0, 0, 2), Color::rgb(1, 1, 1)),
				(Span::new(0, 2, 4), Color::rgb(2, 2, 2)),
			]
		);
	}

	/// Painting the panel colour again as a "run" would be a rectangle that
	/// changes nothing — see `render.rs::draw_backgrounds`.
	#[test]
	fn background_runs_omit_cells_that_match_the_panel_colour() {
		let palette = Palette::default();
		let snapshot = screen(4, 1, ' ');

		let mut grid = Grid::new(4, 1);
		grid.fill(&snapshot, &palette, &GridOptions::default());

		assert_eq!(grid.background_runs(0).count(), 0);
	}

	#[test]
	fn diff_against_none_is_everything() {
		let palette = Palette::default();
		let snapshot = screen(4, 2, ' ');
		let mut grid = Grid::new(4, 2);
		grid.fill(&snapshot, &palette, &GridOptions::default());

		let damage = grid.diff(None);
		assert!(damage.is_everything());
		assert_eq!(damage.rows().count(), 2);
	}

	#[test]
	fn diff_against_an_identical_grid_is_empty() {
		let palette = Palette::default();
		let snapshot = screen(4, 2, ' ');

		let mut a = Grid::new(4, 2);
		a.fill(&snapshot, &palette, &GridOptions::default());
		let mut b = Grid::new(4, 2);
		b.fill(&snapshot, &palette, &GridOptions::default());

		let damage = a.diff(Some(&b));
		assert!(!damage.is_everything());
		assert!(damage.is_empty());
	}

	/// A single keystroke should show up as a single column on a single row —
	/// not the whole row and not the whole grid.
	#[test]
	fn one_changed_cell_produces_a_one_column_span_on_exactly_that_row() {
		let palette = Palette::default();
		let mut before = Grid::new(4, 2);
		before.fill(&screen(4, 2, ' '), &palette, &GridOptions::default());

		let mut after_snapshot = screen(4, 2, ' ');
		after_snapshot.cells[5] = Square::from_char('x'); // row 1, column 1
		let mut after = Grid::new(4, 2);
		after.fill(&after_snapshot, &palette, &GridOptions::default());

		let damage = after.diff(Some(&before));
		assert_eq!(damage.row(0), None);
		assert_eq!(damage.row(1), Some(Span::new(1, 1, 2)));
	}

	/// `bold_is_bright` is the only thing `GridOptions` currently carries, so
	/// this is the case that proves it actually reaches `Palette::resolve`
	/// through `Grid::fill` rather than being plumbed in and ignored.
	#[test]
	fn bold_is_bright_maps_bold_red_onto_bright_red_and_leaves_a_truecolour_spec_alone() {
		use rio_vt::config::colors::NamedColor;

		let palette = Palette::default();
		let mut snapshot = screen(2, 1, 'x');
		snapshot.styles = vec![
			Style {
				fg: AnsiColor::Named(NamedColor::Red),
				flags: StyleFlags::BOLD,
				..Style::default()
			},
			Style {
				fg: AnsiColor::Spec(ColorRgb {
					r: 10,
					g: 20,
					b: 30,
				}),
				flags: StyleFlags::BOLD,
				..Style::default()
			},
		];
		snapshot.cells[0].set_style_id(0);
		snapshot.cells[1].set_style_id(1);

		let mut grid = Grid::new(2, 1);
		grid.fill(
			&snapshot,
			&palette,
			&GridOptions {
				bold_is_bright: true,
			},
		);

		assert_eq!(
			grid.cell(Cell::new(0, 0)).foreground,
			palette.named(NamedColor::LightRed)
		);
		assert_eq!(
			grid.cell(Cell::new(1, 0)).foreground,
			Color::rgb(10, 20, 30)
		);
	}
}
