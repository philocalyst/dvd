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
//! on every frame. [`Shaper::ensure_row`] caches row layouts keyed by a hash of
//! only what affects shaping — character, wide, bold, italic — never colour.
//! Two rows differing only in colour hit the same cache entry. A keystroke
//! re-shapes one row and reuses the rest.
//!
//! ## Why lookup is two calls rather than one
//!
//! [`Shaper::ensure_row`] takes `&mut self` and returns a [`RowKey`];
//! [`Shaper::layout`] takes `&self` and returns the layout. Collapsing them
//! into one `&mut self -> &RowLayout` is the obvious API and it is the wrong
//! one: the returned reference borrows the shaper mutably for as long as it
//! lives, so a caller holding a layout can no longer reach
//! [`Shaper::faces`] — which it must, to resolve a glyph back to the file it
//! came from. The previous shape of this module forced `encode/svg.rs` to
//! clone the text, the column map, the bidi runs *and* the glyph vector out
//! of every row of every frame purely to end that borrow. Splitting the two
//! phases costs one extra call and deletes four allocations per row per
//! frame.

use std::hash::{Hash, Hasher};

use parley::{
	FontData, FontFamily, FontFamilyName, GenericFamily, Layout, LayoutContext, StyleProperty,
};
use rio_vt::crosswords::square::Wide;
use rio_vt::crosswords::style::StyleFlags;
use rustc_hash::FxHashMap;
use skrifa::{FontRef, MetadataProvider};

use crate::fonts::{Fonts, Metrics};
use crate::geom::{Color, Span};
use crate::grid::Grid;
use crate::sprite;

/// The cache bound. A 24-row terminal at 80 columns is 1920 entries; a
/// long-running animation that scrolls continuously produces one new layout
/// per row per frame, so 4096 is a few seconds of headroom before a clear.
const MAXIMUM_CACHE_SIZE: usize = 4096;

const UNSCALED: f32 = 1.0;

/// Identifies a face. Wraps parley's blob identifier so nothing downstream
/// depends on it directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FontKey(pub u64);

/// Identifies a cached row layout. Opaque on purpose: it is a hash of the
/// row's shaping-relevant content, and nothing outside this module has any
/// business deriving one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RowKey(u64);

/// A glyph pinned to the column its source character came from.
#[derive(Clone, Copy, Debug)]
pub struct PlacedGlyph {
	pub font: FontKey,
	pub identifier: u32,
	/// Grid pixels, relative to the row's left edge.
	pub horizontal_position: f32,
	/// Grid pixels, relative to the row's top edge.
	pub vertical_position: f32,
	pub scale: f32,
	pub column: u16,
}

/// The faces shaping has selected so far.
///
/// A named type rather than a bare map because it is handed out to painters,
/// which need to resolve a [`FontKey`] back to the blob it came from in order
/// to extract an outline, and which have no business seeing the rest of the
/// shaper.
#[derive(Default)]
pub struct Faces {
	by_key: FxHashMap<u64, FontData>,
}

impl Faces {
	#[inline]
	pub fn get(&self, font: FontKey) -> Option<&FontData> {
		self.by_key.get(&font.0)
	}

	#[inline]
	pub fn keys(&self) -> impl Iterator<Item = FontKey> + '_ {
		self.by_key.keys().copied().map(FontKey)
	}

	fn remember(&mut self, font: &FontData) -> FontKey {
		let key = font.data.id();
		self.by_key.entry(key).or_insert_with(|| font.clone());
		FontKey(key)
	}
}

/// A stretch of glyphs sharing a colour, a set of attributes and a direction.
///
/// Carries the row's whole text and byte-to-column map alongside the run's own
/// byte range, rather than a pre-sliced string: a backend that emits markup
/// needs to know where the run sits in its row to size it, and one that emits
/// outlines ignores the text entirely. Everything here is a borrow, so a run
/// costs nothing to hand over.
pub struct GlyphRun<'a> {
	/// The columns this run covers.
	pub span: Span,
	pub glyphs: &'a [PlacedGlyph],
	/// The whole row's source characters.
	pub text: &'a str,
	/// This run's byte range within `text`.
	pub range: std::ops::Range<usize>,
	/// The column each byte of `text` came from.
	pub columns: &'a [u16],
	pub color: Color,
	pub flags: StyleFlags,
	/// Whether Parley's bidirectional analysis placed this run right-to-left.
	pub right_to_left: bool,
}

/// A run of text at a single bidirectional direction.
#[derive(Clone, Copy, Debug)]
pub struct BidirectionalRun {
	/// Byte range into [`RowLayout::text`].
	pub start: usize,
	pub end: usize,
	pub right_to_left: bool,
}

/// The shaped layout for one row.
pub struct RowLayout {
	pub glyphs: Vec<PlacedGlyph>,
	pub text: String,
	pub bidirectional_runs: Vec<BidirectionalRun>,
	/// The column each byte of `text` came from — the same mapping shaping
	/// itself uses to pin glyphs back to columns, kept around so other
	/// consumers can turn a byte range back into a column span without
	/// redoing the walk over the grid.
	pub column_by_byte_index: Vec<u16>,
}

impl RowLayout {
	fn blank(text: String, column_by_byte_index: Vec<u16>) -> Self {
		Self {
			glyphs: Vec::new(),
			text,
			bidirectional_runs: Vec::new(),
			column_by_byte_index,
		}
	}

	/// The column a byte of `text` belongs to.
	#[inline]
	pub fn column_at(&self, byte: usize) -> Option<u16> {
		self.column_by_byte_index.get(byte).copied()
	}
}

/// What a face's own plain `cmap` maps a character to, cached.
///
/// Deliberately *not* the glyph shaping chose for a character in context
/// (that is [`PlacedGlyph::identifier`]). It is what a browser would land on
/// rendering the bare character against this same face through a real
/// `<text>` element, which only ever gets to consult `cmap` — no `GSUB`.
/// `encode/svg.rs` compares the two: where they agree, a `<text>` run
/// reproduces shaping's result exactly and the character is safe to embed as
/// real text; where they disagree (ligatures, contextual substitution) it
/// falls back to an outline instead.
///
/// Its own type, owned by the backend that needs it, rather than a field on
/// [`Shaper`]. As a field it forced every lookup to take `&mut Shaper`, which
/// is precisely the borrow that cannot be held at the same time as a
/// [`RowLayout`] — the reason the SVG sink used to clone every row.
#[derive(Default)]
pub struct Cmap {
	resolved: FxHashMap<(u64, char), Option<u32>>,
}

impl Cmap {
	pub fn new() -> Self {
		Self::default()
	}

	pub fn glyph(&mut self, faces: &Faces, font: FontKey, character: char) -> Option<u32> {
		if let Some(&cached) = self.resolved.get(&(font.0, character)) {
			return cached;
		}

		let resolved = faces.get(font).and_then(|face| {
			FontRef::from_index(face.data.as_ref(), face.index)
				.ok()?
				.charmap()
				.map(character)
				.map(|glyph| glyph.to_u32())
		});

		self.resolved.insert((font.0, character), resolved);
		resolved
	}
}

/// Shapes rows and caches the result.
pub struct Shaper {
	layout_context: LayoutContext<[u8; 4]>,
	families: Vec<FontFamilyName<'static>>,
	fonts: Fonts,
	faces: Faces,
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
			faces: Faces::default(),
			rows: FxHashMap::default(),
		}
	}

	pub fn metrics(&self) -> Metrics {
		self.fonts.metrics
	}

	pub fn size(&self) -> f32 {
		self.fonts.size
	}

	pub fn fonts(&self) -> &Fonts {
		&self.fonts
	}

	/// Every face shaping has selected, for resolving a glyph back to the blob
	/// it came from.
	pub fn faces(&self) -> &Faces {
		&self.faces
	}

	/// Shape `row` if it is not already cached, and return the key it is
	/// filed under. See the module doc for why this does not simply return
	/// the layout.
	pub fn ensure_row(&mut self, grid: &Grid, row: u16) -> RowKey {
		let key = row_hash(grid, row);

		if !self.rows.contains_key(&key) {
			// Dropping the whole table rather than evicting one entry: the
			// access pattern is a working set of whole screens, so an LRU
			// would evict the row about to be asked for next, and the cost of
			// re-shaping a screen once every few thousand frames is not worth
			// the bookkeeping to avoid.
			if self.rows.len() >= MAXIMUM_CACHE_SIZE {
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

		RowKey(key)
	}

	/// The layout for a key returned by [`Shaper::ensure_row`].
	pub fn layout(&self, key: RowKey) -> &RowLayout {
		self.rows
			.get(&key.0)
			.expect("a RowKey is only ever handed out by ensure_row, which inserts")
	}

	#[cfg(test)]
	fn cached_rows(&self) -> usize {
		self.rows.len()
	}
}

/// Hash only what affects shaping — character, wide, bold, italic.
///
/// Colour is deliberately absent: two rows that differ only in colour shape
/// identically, and sharing the entry is the whole point of the cache.
fn row_hash(grid: &Grid, row: u16) -> u64 {
	let mut hasher = rustc_hash::FxHasher::default();

	for cell in grid.row(row) {
		cell.character.hash(&mut hasher);
		(cell.wide as u8).hash(&mut hasher);
		(cell.flags & (StyleFlags::BOLD | StyleFlags::ITALIC))
			.bits()
			.hash(&mut hasher);
	}

	hasher.finish()
}

/// The row's characters, and the column each byte came from.
fn row_text(grid: &Grid, row: u16) -> (String, Vec<u16>) {
	let cells = grid.row(row);
	let mut text = String::with_capacity(cells.len());
	let mut column_by_byte_index = Vec::with_capacity(cells.len());

	for (column, cell) in cells.iter().enumerate() {
		// Both spacer kinds are placeholders the terminal owns, not
		// characters: `Spacer` is the second half of a double-width cell, and
		// `LeadingSpacer` is the stub a soft wrap leaves at the end of a line
		// when a wide character did not fit. Emitting either would give the
		// shaper a phantom character and push the rest of the row one column
		// to the right.
		if matches!(cell.wide, Wide::Spacer | Wide::LeadingSpacer) {
			continue;
		}

		text.push(match cell.character {
			'\0' => ' ',
			other => other,
		});
		// `text.len()` is a byte length, so this maps every byte of a
		// multi-byte character onto the one column it came from.
		column_by_byte_index.resize(text.len(), column as u16);
	}

	(text, column_by_byte_index)
}

fn shape_row(
	layout_context: &mut LayoutContext<[u8; 4]>,
	families: &[FontFamilyName<'static>],
	fonts: &mut Fonts,
	faces: &mut Faces,
	grid: &Grid,
	row: u16,
) -> RowLayout {
	let (text, column_by_byte_index) = row_text(grid, row);

	if text.trim().is_empty() {
		return RowLayout::blank(text, column_by_byte_index);
	}

	let metrics = fonts.metrics;
	let mut layout = Layout::default();
	let mut builder = layout_context.ranged_builder(&mut fonts.context, &text, UNSCALED, false);
	builder.push_default(StyleProperty::FontFamily(FontFamily::List(
		std::borrow::Cow::Borrowed(families),
	)));
	builder.push_default(StyleProperty::FontSize(fonts.size));
	builder.build_into(&mut layout, &text);
	// No width constraint: the grid, not the text engine, decides where lines
	// end.
	layout.break_all_lines(None);

	let cells = grid.row(row);
	let mut glyphs = Vec::new();
	let mut bidirectional_runs = Vec::new();

	for line in layout.lines() {
		for run in line.runs() {
			let font = run.font();
			let font_key = faces.remember(font);

			let range = run.text_range();
			bidirectional_runs.push(BidirectionalRun {
				start: range.start,
				end: range.end,
				right_to_left: run.is_rtl(),
			});

			for cluster in run.clusters() {
				let column = column_by_byte_index
					.get(cluster.text_range().start)
					.copied()
					.unwrap_or(0);

				let Some(cell) = cells.get(column as usize) else {
					continue;
				};

				// Box drawing, blocks and braille are drawn by `sprite`
				// against the real cell rectangle, not taken from the font —
				// see that module for why. The character stays in `text` so
				// it is still selectable and still maps to its column; only
				// its ink comes from somewhere else.
				if sprite::covers(cell.character) {
					continue;
				}

				let columns_spanned = if cell.wide == Wide::Wide { 2 } else { 1 };
				let span_width = (columns_spanned * metrics.cell_width) as f32;
				let advance: f32 = cluster.glyphs().map(|glyph| glyph.advance).sum();

				// An icon wider than the cells the terminal gave it is shrunk
				// to fit rather than allowed to overrun its neighbour.
				let scale = if advance > span_width && advance > 0.0 {
					span_width / advance
				} else {
					UNSCALED
				};
				let centring = (span_width - advance * scale).max(0.0) / 2.0;

				// Shrinking about the *cell's* centre rather than about the
				// baseline. Scaling about the baseline pins the glyph's feet
				// and pulls its head down, so a shrunk icon sinks towards the
				// bottom of its cell while the unscaled text beside it stays
				// put; scaling about the centre keeps it optically in place.
				let middle = metrics.cell_height as f32 / 2.0;
				let baseline = middle + (metrics.baseline - middle) * scale;
				let left = (column as u32 * metrics.cell_width) as f32;

				for glyph in cluster.glyphs() {
					glyphs.push(PlacedGlyph {
						font: font_key,
						identifier: glyph.id,
						horizontal_position: left + centring + glyph.x * scale,
						vertical_position: baseline + glyph.y * scale,
						scale,
						column,
					});
				}
			}
		}
	}

	RowLayout {
		glyphs,
		text,
		bidirectional_runs,
		column_by_byte_index,
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::grid::{Grid, GridOptions};
	use crate::model::{Palette, Snapshot};
	use rio_vt::config::colors::{AnsiColor, ColorRgb};
	use rio_vt::crosswords::square::Square;
	use rio_vt::crosswords::style::Style;
	use rstest::rstest;

	fn shaper() -> Shaper {
		Shaper::new(Fonts::resolve(Some("Liberation Mono"), 16.0, 1.0).unwrap())
	}

	fn grid_of(text: &str, columns: u16) -> Grid {
		let mut snapshot = Snapshot::new(columns, 1);
		snapshot.cells.fill(Square::from_char(' '));
		for (index, character) in text.chars().take(columns as usize).enumerate() {
			snapshot.cells[index] = Square::from_char(character);
		}
		let mut grid = Grid::new(columns, 1);
		grid.fill(&snapshot, &Palette::default(), &GridOptions::default());
		grid
	}

	/// "aaaa" painted red: same characters as `grid_of("aaaa", 4)`, differing
	/// only in appearance.
	fn coloured_row() -> Grid {
		let mut coloured = Snapshot::new(4, 1);
		coloured.cells.fill(Square::from_char('a'));
		coloured.styles = vec![Style {
			fg: AnsiColor::Spec(ColorRgb { r: 255, g: 0, b: 0 }),
			..Style::default()
		}];
		for cell in &mut coloured.cells {
			cell.set_style_id(0);
		}
		let mut grid = Grid::new(4, 1);
		grid.fill(&coloured, &Palette::default(), &GridOptions::default());
		grid
	}

	/// The cache keys on content, not appearance. A re-shape shares the key
	/// and the entry only when the characters match; any changed character
	/// forces a fresh key and a new entry.
	#[rstest]
	// Two rows that differ only in colour must share a cache entry. The hit
	// is the whole point of keying on content rather than on appearance.
	#[case(grid_of("aaaa", 4), coloured_row(), true, 0)]
	// A single changed character must miss and add its own entry.
	#[case(grid_of("aaaa", 4), grid_of("abaa", 4), false, 1)]
	fn re_shaping_hits_the_cache_only_when_the_characters_match(
		#[case] first: Grid,
		#[case] second: Grid,
		#[case] shares_key: bool,
		#[case] cache_delta: usize,
	) {
		let mut shaper = shaper();

		let key = shaper.ensure_row(&first, 0);
		let cached = shaper.cached_rows();
		let same = shaper.ensure_row(&second, 0);

		assert_eq!(
			key == same,
			shares_key,
			"key sharing must follow the characters"
		);
		assert_eq!(
			shaper.cached_rows(),
			cached + cache_delta,
			"entry count must follow the characters"
		);
	}

	/// A layout has to stay readable while the faces map is also borrowed —
	/// that pairing is the reason lookup is split in two, and a painter that
	/// cannot do it has to clone the whole row instead.
	#[test]
	fn a_layout_and_the_faces_can_be_held_at_the_same_time() {
		let mut shaper = shaper();
		let grid = grid_of("hello", 8);
		let key = shaper.ensure_row(&grid, 0);

		let layout = shaper.layout(key);
		let faces = shaper.faces();

		assert!(!layout.glyphs.is_empty());
		for glyph in &layout.glyphs {
			assert!(
				faces.get(glyph.font).is_some(),
				"every placed glyph must resolve back to the face that shaped it"
			);
		}
	}

	#[test]
	fn rtl_text_produces_a_right_to_left_run() {
		let mut shaper = shaper();
		let grid = grid_of("Hello مرحبا ok", 14);
		let key = shaper.ensure_row(&grid, 0);
		let layout = shaper.layout(key);

		assert!(!layout.bidirectional_runs.is_empty());
		assert!(
			layout
				.bidirectional_runs
				.iter()
				.any(|run| run.right_to_left),
			"the Arabic stretch must come back as a right-to-left run"
		);
	}

	/// The second half of a double-width cell is the terminal's placeholder,
	/// not a character. Emitting it would hand the shaper a phantom and shift
	/// every column after it.
	#[test]
	fn a_wide_cells_spacer_does_not_become_a_character() {
		let mut snapshot = Snapshot::new(4, 1);
		snapshot.cells.fill(Square::from_char(' '));
		let mut wide = Square::from_char('世');
		wide.set_wide(Wide::Wide);
		snapshot.cells[0] = wide;
		let mut spacer = Square::from_char(' ');
		spacer.set_wide(Wide::Spacer);
		snapshot.cells[1] = spacer;

		let mut grid = Grid::new(4, 1);
		grid.fill(&snapshot, &Palette::default(), &GridOptions::default());

		let (text, columns) = row_text(&grid, 0);

		assert_eq!(text.chars().count(), 3, "the spacer must not add a character");
		assert_eq!(text.chars().next(), Some('世'));
		assert_eq!(columns[0], 0);
		// The character after the wide one came from column 2, not column 1.
		let after = text.char_indices().nth(1).map(|(byte, _)| columns[byte]);
		assert_eq!(after, Some(2), "the next character keeps its own column");
	}

	/// Characters the grid draws itself must not also be shaped, or their ink
	/// is laid down twice — once from the font and once from the sprite.
	#[test]
	fn drawn_characters_stay_out_of_the_glyph_list_but_keep_their_text() {
		let mut shaper = shaper();
		let grid = grid_of("a─b", 4);
		let key = shaper.ensure_row(&grid, 0);
		let layout = shaper.layout(key);

		assert!(
			layout.text.contains('─'),
			"the character stays in the text so it is still selectable"
		);
		assert!(
			layout.glyphs.iter().all(|glyph| glyph.column != 1),
			"but no glyph is placed in its column"
		);
		assert!(
			layout.glyphs.iter().any(|glyph| glyph.column == 0),
			"the ordinary characters around it are still shaped"
		);
	}

	/// A face's plain cmap lookup must be stable and must not need a mutable
	/// borrow of the shaper — that independence is what lets a backend
	/// consult it while holding a layout.
	#[test]
	fn the_cmap_cache_answers_the_same_way_twice() {
		let mut shaper = shaper();
		let grid = grid_of("abc", 4);
		let key = shaper.ensure_row(&grid, 0);

		let font = shaper.layout(key).glyphs[0].font;
		let mut cmap = Cmap::new();

		let first = cmap.glyph(shaper.faces(), font, 'a');
		let second = cmap.glyph(shaper.faces(), font, 'a');

		assert!(first.is_some(), "a Latin face must map a plain 'a'");
		assert_eq!(first, second);
	}
}
