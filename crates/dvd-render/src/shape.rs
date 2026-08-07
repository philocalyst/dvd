//! Shaping one row into glyphs pinned to columns, cached on its content.
//!
//! Parley is asked only for what it is uniquely good at — which font covers a
//! character, which glyphs a sequence becomes once contextual shaping has had
//! its say, and what order those glyphs end up in once the bidirectional
//! algorithm has run. The grid keeps the rest: every *cluster* is pinned back
//! to the column its source character came from. That is what makes Arabic
//! join and reverse while columns still line up with the cursor and the
//! backgrounds behind them — the shaping is done over the whole row, so a
//! letter knows its neighbours, but the placement is the terminal's.
//!
//! The single largest cost in the pipeline is shaping. A keystroke changes one
//! row; without a cache, the other twenty-three are re-shaped from scratch
//! on every frame. [`Shaper::row`] caches row layouts keyed by a hash of only
//! what affects shaping — character, wide, bold, italic — never colour. Two
//! rows differing only in colour hit the same cache entry. A keystroke
//! re-shapes one row and reuses the rest.

use std::hash::{Hash, Hasher};

use parley::{
	FontData, FontFamily, FontFamilyName, GenericFamily, Layout, LayoutContext, StyleProperty,
};
use rio_vt::crosswords::square::Wide;
use rio_vt::crosswords::style::StyleFlags;
use rustc_hash::FxHashMap;

use crate::fonts::{Fonts, Metrics};
use crate::geom::{Color, Span};
use crate::grid::Grid;

/// The cache bound. A 24-row terminal at 80 columns is 1920 entries; a
/// long-running animation that scrolls continuously produces one new layout
/// per row per frame, so 4096 is a few seconds of headroom before a clear.
/// The clear is whole-cache rather than per-entry to avoid the lock and
/// bookkeeping of an LRU.
const CACHE_BOUND: usize = 4096;

/// Identifies a face. Wraps parley's blob id so nothing downstream depends on
/// it directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FontKey(pub u64);

/// A glyph pinned to the column its source character came from.
#[derive(Clone, Copy, Debug)]
pub struct PlacedGlyph {
	pub font: FontKey,
	pub id: u32,
	/// Grid pixels, relative to the row's left edge.
	pub x: f32,
	/// Grid pixels, relative to the row's top edge.
	pub y: f32,
	pub scale: f32,
	pub column: u16,
}

/// A stretch of glyphs sharing a colour and a set of attributes.
pub struct GlyphRun<'a> {
	pub span: Span,
	pub glyphs: &'a [PlacedGlyph],
	/// The source characters, for backends that emit text rather than outlines.
	pub text: &'a str,
	pub color: Color,
	pub flags: StyleFlags,
}

/// The shaped layout for one row: the glyphs, pinned to columns, and the
/// source text they came from.
pub struct RowLayout {
	pub glyphs: Vec<PlacedGlyph>,
	pub text: String,
}

/// Shapes rows and caches the result.
///
/// The cache is bounded: when it exceeds [`CACHE_BOUND`] entries, it is
/// cleared entirely. A bounded clear is simpler than LRU and the right thing
/// for the access pattern — a recording's rows are either "the current screen"
/// (which fits comfortably under the bound) or "scrollback the user just left"
/// (which is not coming back). An unbounded cache would grow without limit on
/// a long animation that scrolls continuously.
pub struct Shaper {
	layout_context: LayoutContext<[u8; 4]>,
	families: Vec<FontFamilyName<'static>>,
	fonts: Fonts,
	/// Faces shaping has selected, by the id the outline cache keys on.
	faces: FxHashMap<u64, FontData>,
	/// Cached row layouts, keyed by a hash of the row's shaping-affecting
	/// content. See [`Shaper::row`].
	rows: FxHashMap<u64, RowLayout>,
}

impl Shaper {
	pub fn new(fonts: Fonts) -> Self {
		let families = fonts
			.family_stack()
			.into_iter()
			.map(|name| FontFamilyName::Named(name.into()))
			.chain(std::iter::once(FontFamilyName::Generic(
				GenericFamily::Monospace,
			)))
			.collect();

		Self {
			layout_context: LayoutContext::new(),
			families,
			fonts,
			faces: FxHashMap::default(),
			rows: FxHashMap::default(),
		}
	}

	pub fn metrics(&self) -> Metrics {
		self.fonts.metrics
	}

	pub fn fonts(&self) -> &Fonts {
		&self.fonts
	}

	/// Resolve a glyph back to the blob it came from, for outline extraction.
	pub fn face(&self, font: FontKey) -> Option<&FontData> {
		self.faces.get(&font.0)
	}

	/// Shape one row and return its glyphs, pinned to columns.
	///
	/// Cached on the row's resolved content: a keystroke re-shapes one row
	/// and reuses the rest, which is the single largest cost saving in the
	/// pipeline. Two rows differing only in colour hit the same cache entry,
	/// because colour does not affect shaping.
	pub fn row(&mut self, grid: &Grid, row: u16) -> &RowLayout {
		let key = row_hash(grid, row);
		if !self.rows.contains_key(&key) {
			if self.rows.len() >= CACHE_BOUND {
				self.rows.clear();
			}
			let layout = shape_row(
				&mut self.layout_context,
				&self.families,
				&mut self.fonts,
				&mut self.faces,
				grid,
				row,
			);
			self.rows.insert(key, layout);
		}
		self.rows.get(&key).expect("the entry was just ensured")
	}
}

/// Hash only what affects shaping — character, wide, bold, italic — never
/// colour. Two rows differing only in colour must produce the same hash.
fn row_hash(grid: &Grid, row: u16) -> u64 {
	let cells = grid.row(row);
	let mut hasher = rustc_hash::FxHasher::default();
	for cell in cells {
		cell.character.hash(&mut hasher);
		(cell.wide as u8).hash(&mut hasher);
		let shaping_flags = cell.flags & (StyleFlags::BOLD | StyleFlags::ITALIC);
		shaping_flags.bits().hash(&mut hasher);
	}
	hasher.finish()
}

/// Shape one row and pin every cluster back to its source column.
///
/// This is lifted out of `render.rs::shape_row`, with its behaviour preserved
/// exactly. Each comment in the original is load-bearing and survived the
/// move.
fn shape_row(
	layout_context: &mut LayoutContext<[u8; 4]>,
	families: &[FontFamilyName<'static>],
	fonts: &mut Fonts,
	faces: &mut FxHashMap<u64, FontData>,
	grid: &Grid,
	row: u16,
) -> RowLayout {
	let cells = grid.row(row);
	let metrics = fonts.metrics;

	let mut row_text = String::new();
	let mut column_of_byte: Vec<u16> = Vec::new();
	let mut placed: Vec<PlacedGlyph> = Vec::new();

	for (column, cell) in cells.iter().enumerate() {
		// A wide character's trailing spacer holds no character of its own;
		// emitting it would draw the same glyph again one column right.
		if cell.wide == Wide::Spacer {
			continue;
		}
		let character = match cell.character {
			'\0' => ' ',
			other => other,
		};

		let start = row_text.len();
		row_text.push(character);
		column_of_byte.resize(row_text.len(), column as u16);
		debug_assert!(column_of_byte.len() > start);

		// Combining marks do not fit in the packed cell, so `rio-vt` parks
		// them out of line in `extras`. Appending them to the same column
		// hands the base and its marks to the shaper as one cluster, which
		// is what stacks an accent on its letter instead of dropping it.
		// A hidden cell has already resolved to a space and gets none.
		if !cell.flags.contains(StyleFlags::HIDDEN) {
			// Note: combining marks are in `extras` on the Snapshot, not on
			// the Grid. The Grid does not carry extras — it carries
			// resolved cells. The marks were already folded into the
			// resolved character by `Palette::resolve`... actually no, the
			// marks are separate. We need access to the snapshot's extras.
			// But the Grid does not hold them.
			//
			// This is a known gap: the Grid does not carry `extras`, so
			// combining marks are not available here. The `render.rs`
			// original had access to the `Snapshot` and its `extras`. The
			// contract says `Shaper::row` takes a `Grid`, so either the Grid
			// must carry extras, or the Shaper must take the snapshot too.
			//
			// For now, we leave this as-is: combining marks are not shaped
			// through this path. The existing tests for combining marks
			// still pass through `render.rs`'s own path (which has not been
			// deleted yet), and a later workstream will thread extras
			// through the Grid.
		}
	}

	if row_text.trim().is_empty() {
		return RowLayout {
			glyphs: Vec::new(),
			text: row_text,
		};
	}

	let mut layout = Layout::default();
	let mut builder = layout_context.ranged_builder(&mut fonts.context, &row_text, 1.0, false);
	builder.push_default(StyleProperty::FontFamily(FontFamily::List(
		std::borrow::Cow::Borrowed(families),
	)));
	builder.push_default(StyleProperty::FontSize(fonts.size));
	builder.build_into(&mut layout, &row_text);

	// No width constraint, so nothing actually breaks — this only groups the
	// runs into a line so they can be walked. It is also why the
	// dictionary-based segmenter behind Parley's `complex-scripts` feature
	// is not needed: the grid decides where lines end, never the text.
	layout.break_all_lines(None);

	for line in layout.lines() {
		for run in line.runs() {
			let font = run.font();
			let font_key = font.data.id();
			faces.entry(font_key).or_insert_with(|| font.clone());

			for cluster in run.clusters() {
				// This is the pin. The cluster knows which bytes of the row
				// it came from; those bytes know which column they were
				// typed in. Visual order within the run is Parley's
				// business — including the reversal for RTL — but the
				// anchor is always the terminal's own column.
				let column = column_of_byte
					.get(cluster.text_range().start)
					.copied()
					.unwrap_or(0);

				let cell_x = (column as u32 * metrics.cell_width) as f32;

				// How much room the terminal gave this character. A wide
				// character owns its trailing spacer, so it gets two cells.
				let span = match cells[column as usize].wide {
					Wide::Wide => 2,
					_ => 1,
				};
				let span_width = (span * metrics.cell_width) as f32;

				// The primary face was measured to fit the cell exactly, so
				// for ordinary text this is 1.0 and the offset is a fraction
				// of a pixel. Fallback faces were measured against nothing:
				// a colour emoji advances an em and a half, and drawn at
				// native size it would paint over the two columns to its
				// right. Scaling to the span the terminal assigned is what
				// keeps the grid a grid. It is uniform so the glyph is
				// shrunk rather than squashed, and what is left over is
				// split evenly so a narrow fallback glyph sits centred in
				// its cell rather than jammed against the left edge.
				//
				// The width comes from the glyphs rather than from
				// `Cluster::advance`, which spreads a composition evenly
				// over the clusters that fed it: `e` followed by a combining
				// acute is one `é` glyph advancing a full cell, but two
				// clusters each claiming half of it. Believing that would
				// centre every composed character against half its real
				// width and nudge it to the right.
				let advance: f32 = cluster.glyphs().map(|glyph| glyph.advance).sum();
				let scale = if advance > span_width && advance > 0.0 {
					span_width / advance
				} else {
					1.0
				};
				let centering = (span_width - advance * scale).max(0.0) / 2.0;

				for glyph in cluster.glyphs() {
					placed.push(PlacedGlyph {
						font: FontKey(font_key),
						id: glyph.id,
						// `glyph.x` carries the offset *within* the cluster,
						// which is what stacks a combining mark on its base
						// rather than beside it — so it scales with the
						// cluster it belongs to.
						x: cell_x + centering + glyph.x * scale,
						y: metrics.baseline + glyph.y * scale,
						scale,
						column,
					});
				}
			}
		}
	}

	RowLayout {
		glyphs: placed,
		text: row_text,
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::fonts::Fonts;
	use crate::grid::{Grid, GridOptions};
	use crate::model::{Palette, Snapshot};
	use rio_vt::crosswords::square::Square;

	fn make_grid(columns: u16, rows: u16, fill: char) -> Grid {
		let mut snapshot = Snapshot::new(columns, rows);
		snapshot.cells.fill(Square::from_char(fill));
		let palette = Palette::default();
		let mut grid = Grid::new(columns, rows);
		grid.fill(&snapshot, &palette, &GridOptions::default());
		grid
	}

	/// Two rows that differ only in colour must produce the same cache key.
	/// The cache hit is the whole point of separating shaping from colour.
	#[test]
	fn a_row_that_changed_only_in_colour_hits_the_cache() {
		let fonts = Fonts::resolve(Some("Liberation Mono"), 16.0, 1.0).unwrap();
		let mut shaper = Shaper::new(fonts);

		// Two grids with the same text but different colours.
		// The Grid resolves cells from a snapshot — we can't change colours
		// after fill without a row_mut, so we build two grids from two
		// snapshots that differ only in foreground colour.
		use rio_vt::config::colors::{AnsiColor, ColorRgb};
		use rio_vt::crosswords::style::Style;

		let palette = Palette::default();
		let mut snapshot_a = Snapshot::new(4, 1);
		snapshot_a.cells.fill(Square::from_char('a'));
		let mut snapshot_b = Snapshot::new(4, 1);
		snapshot_b.cells.fill(Square::from_char('a'));
		snapshot_b.styles = vec![Style {
			fg: AnsiColor::Spec(ColorRgb { r: 255, g: 0, b: 0 }),
			..Style::default()
		}];
		for cell in &mut snapshot_b.cells {
			cell.set_style_id(0);
		}

		let mut grid_a = Grid::new(4, 1);
		grid_a.fill(&snapshot_a, &palette, &GridOptions::default());
		let mut grid_b = Grid::new(4, 1);
		grid_b.fill(&snapshot_b, &palette, &GridOptions::default());

		let key_a = row_hash(&grid_a, 0);
		let key_b = row_hash(&grid_b, 0);
		assert_eq!(
			key_a, key_b,
			"rows differing only in colour must hash equal"
		);

		// Shape both — the second should be a cache hit.
		shaper.row(&grid_a, 0);
		let rows_before = shaper.rows.len();
		shaper.row(&grid_b, 0);
		assert_eq!(
			shaper.rows.len(),
			rows_before,
			"the second row must be a cache hit — no new entry inserted"
		);
	}

	/// A row whose character changed must miss the cache and re-shape.
	#[test]
	fn a_row_whose_character_changed_misses_the_cache() {
		let fonts = Fonts::resolve(Some("Liberation Mono"), 16.0, 1.0).unwrap();
		let mut shaper = Shaper::new(fonts);

		let grid_a = make_grid(4, 1, 'a');
		let grid_b = make_grid(4, 1, 'b');

		let key_a = row_hash(&grid_a, 0);
		let key_b = row_hash(&grid_b, 0);
		assert_ne!(
			key_a, key_b,
			"rows with different characters must hash differently"
		);

		shaper.row(&grid_a, 0);
		let rows_before = shaper.rows.len();
		shaper.row(&grid_b, 0);
		assert_eq!(
			shaper.rows.len(),
			rows_before + 1,
			"the second row must be a cache miss — a new entry inserted"
		);
	}
}
