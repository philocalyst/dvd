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
use skrifa::{FontRef, MetadataProvider};

use crate::fonts::{Fonts, Metrics};
use crate::geom::{Color, Span};
use crate::grid::Grid;

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// The cache bound. A 24-row terminal at 80 columns is 1920 entries; a
/// long-running animation that scrolls continuously produces one new layout
/// per row per frame, so 4096 is a few seconds of headroom before a clear.
const MAXIMUM_CACHE_SIZE: usize = 4096;

const DEFAULT_GLYPH_SCALE: f32 = 1.0;
const MINIMUM_ADVANCE_FOR_SCALING: f32 = 0.0;
const ZERO_OFFSET: f32 = 0.0;
const CENTERING_DIVISOR: f32 = 2.0;

const WIDE_CHARACTER_SPAN: usize = 2;
const NORMAL_CHARACTER_SPAN: usize = 1;
const DEFAULT_COLUMN_INDEX: u16 = 0;

// -----------------------------------------------------------------------------
// Data Structures
// -----------------------------------------------------------------------------

/// Identifies a face. Wraps parley's blob identifier so nothing downstream depends on it directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FontKey(pub u64);

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

/// A stretch of glyphs sharing a colour and a set of attributes.
pub struct GlyphRun<'a> {
	pub span: Span,
	pub glyphs: &'a [PlacedGlyph],
	/// The source characters, for backends that emit text rather than outlines.
	pub text: &'a str,
	pub color: Color,
	pub flags: StyleFlags,
}

/// A run of text at a single bidirectional direction, for correct right-to-left/left-to-right markup.
#[derive(Clone, Copy, Debug)]
pub struct BidirectionalRun {
	/// Byte range into `RowLayout::text` (start, end).
	pub text_range: (usize, usize),
	/// Whether Parley's bidirectional analysis placed this run right-to-left.
	pub is_right_to_left: bool,
}

/// The shaped layout for one row: the glyphs, pinned to columns, the
/// source text they came from, and bidirectional-run boundaries for RTL support.
pub struct RowLayout {
	pub glyphs: Vec<PlacedGlyph>,
	pub text: String,
	pub bidirectional_runs: Vec<BidirectionalRun>,
	/// The column each byte of `text` came from — the same mapping shaping
	/// itself uses to pin glyphs back to columns, kept around so other
	/// consumers (the SVG accessible text layer) can turn a bidi run's byte
	/// range back into a pixel span without redoing the walk over the grid.
	pub column_by_byte_index: Vec<u16>,
}

// -----------------------------------------------------------------------------
// Shaper Engine
// -----------------------------------------------------------------------------

/// Shapes rows and caches the result.
pub struct Shaper {
	layout_context: LayoutContext<[u8; 4]>,
	families: Vec<FontFamilyName<'static>>,
	fonts: Fonts,
	/// Faces shaping has selected, by the identifier the outline cache keys on.
	faces: FxHashMap<u64, FontData>,
	/// Cached row layouts, keyed by a hash of the row's shaping-affecting content.
	rows: FxHashMap<u64, RowLayout>,
	/// `(font, character) -> that font's own plain cmap lookup`, cached — see
	/// [`Shaper::cmap_glyph`]. A frame re-derives every visual artifact from
	/// scratch (see `encode/svg.rs`), so without this cache a long recording
	/// would re-parse the same face's cmap for the same character on every
	/// single frame it appears in.
	cmap: FxHashMap<(u64, char), Option<u32>>,
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
			cmap: FxHashMap::default(),
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

	/// What `font`'s own plain `cmap` — no `GSUB`, no shaping context — maps
	/// `character` to, or `None` if the face has no entry for it at all.
	///
	/// This is deliberately *not* the glyph shaping chose for a character in
	/// context (that's `PlacedGlyph::identifier`). It is what a browser
	/// would land on rendering the bare character against this same face
	/// through a real `<text>` element, which only ever gets to consult
	/// `cmap` — no `GSUB`. `encode/svg.rs` compares the two: where they
	/// agree, a `<text>` run reproduces shaping's result exactly and the
	/// character is safe to embed as real text; where they disagree
	/// (ligatures, contextual substitution), it stays on the pre-existing
	/// outline/`<path>` route instead. See `encode/font_embed.rs` for where
	/// this same lookup is reused to build the embedded subset's own cmap.
	pub fn cmap_glyph(&mut self, font: FontKey, character: char) -> Option<u32> {
		if let Some(&cached) = self.cmap.get(&(font.0, character)) {
			return cached;
		}

		let resolved = self.faces.get(&font.0).and_then(|face| {
			FontRef::from_index(face.data.as_ref(), face.index)
				.ok()?
				.charmap()
				.map(character)
				.map(|glyph_id| glyph_id.to_u32())
		});

		self.cmap.insert((font.0, character), resolved);
		resolved
	}

	/// Shape one row and return its glyphs, pinned to columns.
	pub fn row(&mut self, grid: &Grid, row_index: u16) -> &RowLayout {
		let cache_key = compute_row_hash(grid, row_index);

		if !self.rows.contains_key(&cache_key) {
			self.ensure_cache_capacity();

			let layout = shape_terminal_row(
				&mut self.layout_context,
				&self.families,
				&mut self.fonts,
				&mut self.faces,
				grid,
				row_index,
			);

			self.rows.insert(cache_key, layout);
		}

		self.rows
			.get(&cache_key)
			.expect("The layout entry was strictly ensured")
	}

	fn ensure_cache_capacity(&mut self) {
		if self.rows.len() >= MAXIMUM_CACHE_SIZE {
			self.rows.clear();
		}
	}
}

// -----------------------------------------------------------------------------
// Core Logic & Math
// -----------------------------------------------------------------------------

/// Hash only what affects shaping — character, wide, bold, italic.
fn compute_row_hash(grid: &Grid, row_index: u16) -> u64 {
	let mut hasher = rustc_hash::FxHasher::default();

	for cell in grid.row(row_index) {
		cell.character.hash(&mut hasher);
		(cell.wide as u8).hash(&mut hasher);

		let shaping_flags = cell.flags & (StyleFlags::BOLD | StyleFlags::ITALIC);
		shaping_flags.bits().hash(&mut hasher);
	}

	hasher.finish()
}

struct ExtractedContent {
	text: String,
	column_by_byte_index: Vec<u16>,
}

/// Parses the terminal grid to extract raw text and maintain byte-to-column mappings.
fn extract_text_and_columns(grid: &Grid, row_index: u16) -> ExtractedContent {
	let cells = grid.row(row_index);

	// Low allocation setup: approximate standard row capacity
	let mut text = String::with_capacity(cells.len());
	let mut column_by_byte_index = Vec::with_capacity(cells.len() * 4); // Max UTF-8 expansion

	for (column_index, cell) in cells.iter().enumerate() {
		if cell.wide == Wide::Spacer {
			continue;
		}

		let character = match cell.character {
			'\0' => ' ',
			other => other,
		};

		text.push(character);

		// `text.len()` resolves precisely to byte length, mapping multi-byte
		// characters uniformly to their originating column index.
		column_by_byte_index.resize(text.len(), column_index as u16);
	}

	ExtractedContent {
		text,
		column_by_byte_index,
	}
}

/// Dispatches raw text to Parley and establishes the text layout block.
fn build_text_layout(
	layout_context: &mut LayoutContext<[u8; 4]>,
	families: &[FontFamilyName<'static>],
	fonts: &mut Fonts,
	text: &str,
) -> Layout<[u8; 4]> {
	let mut layout = Layout::default();
	let mut builder =
		layout_context.ranged_builder(&mut fonts.context, text, DEFAULT_GLYPH_SCALE, false);

	builder.push_default(StyleProperty::FontFamily(FontFamily::List(
		std::borrow::Cow::Borrowed(families),
	)));
	builder.push_default(StyleProperty::FontSize(fonts.size));
	builder.build_into(&mut layout, text);

	layout.break_all_lines(None);
	layout
}

fn calculate_glyph_scale(total_advance: f32, span_width: f32) -> f32 {
	if total_advance > span_width && total_advance > MINIMUM_ADVANCE_FOR_SCALING {
		span_width / total_advance
	} else {
		DEFAULT_GLYPH_SCALE
	}
}

fn calculate_centering_offset(span_width: f32, total_advance: f32, scale: f32) -> f32 {
	((span_width - (total_advance * scale)).max(ZERO_OFFSET)) / CENTERING_DIVISOR
}

/// The master orchestrator for shaping a row and mapping back terminal geometries.
fn shape_terminal_row(
	layout_context: &mut LayoutContext<[u8; 4]>,
	families: &[FontFamilyName<'static>],
	fonts: &mut Fonts,
	faces: &mut FxHashMap<u64, FontData>,
	grid: &Grid,
	row_index: u16,
) -> RowLayout {
	let content = extract_text_and_columns(grid, row_index);

	if content.text.trim().is_empty() {
		return RowLayout {
			glyphs: Vec::new(),
			text: content.text,
			bidirectional_runs: Vec::new(),
			column_by_byte_index: content.column_by_byte_index,
		};
	}

	let layout = build_text_layout(layout_context, families, fonts, &content.text);
	let cells = grid.row(row_index);
	let metrics = fonts.metrics;

	let mut placed_glyphs = Vec::new();
	let mut bidirectional_runs = Vec::new();

	for line in layout.lines() {
		for run in line.runs() {
			let font_data = run.font();
			let font_identifier = font_data.data.id();

			faces
				.entry(font_identifier)
				.or_insert_with(|| font_data.clone());

			let text_range = run.text_range();
			bidirectional_runs.push(BidirectionalRun {
				text_range: (text_range.start, text_range.end),
				is_right_to_left: run.is_rtl(),
			});

			for cluster in run.clusters() {
				let cluster_start_byte = cluster.text_range().start;
				let column_index = content
					.column_by_byte_index
					.get(cluster_start_byte)
					.copied()
					.unwrap_or(DEFAULT_COLUMN_INDEX);

				let span_size = match cells[column_index as usize].wide {
					Wide::Wide => WIDE_CHARACTER_SPAN,
					_ => NORMAL_CHARACTER_SPAN,
				};

				let span_width = (span_size as f32) * metrics.cell_width as f32;
				let column_horizontal_offset = (column_index as f32) * metrics.cell_width as f32;
				let total_advance: f32 = cluster.glyphs().map(|glyph| glyph.advance).sum();

				let scale = calculate_glyph_scale(total_advance, span_width);
				let centering_offset = calculate_centering_offset(span_width, total_advance, scale);

				for glyph in cluster.glyphs() {
					placed_glyphs.push(PlacedGlyph {
						font: FontKey(font_identifier),
						identifier: glyph.id,
						horizontal_position: column_horizontal_offset
							+ centering_offset + (glyph.x * scale),
						vertical_position: metrics.baseline + (glyph.y * scale),
						scale,
						column: column_index,
					});
				}
			}
		}
	}

	RowLayout {
		glyphs: placed_glyphs,
		text: content.text,
		bidirectional_runs,
		column_by_byte_index: content.column_by_byte_index,
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

		let key_a = compute_row_hash(&grid_a, 0);
		let key_b = compute_row_hash(&grid_b, 0);
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

		let key_a = compute_row_hash(&grid_a, 0);
		let key_b = compute_row_hash(&grid_b, 0);
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

	/// RTL text should produce bidi runs with is_rtl set correctly.
	#[test]
	fn rtl_text_produces_bidi_runs() {
		let fonts = Fonts::resolve(Some("Liberation Mono"), 16.0, 1.0).unwrap();
		let mut shaper = Shaper::new(fonts);

		// Create a snapshot with mixed LTR and RTL text
		let mut snapshot = Snapshot::new(14, 1);
		let text = "Hello مرحبا ok";
		let mut cell_index = 0;
		for ch in text.chars() {
			if cell_index < snapshot.cells.len() {
				snapshot.cells[cell_index] = Square::from_char(ch);
				cell_index += 1;
			}
		}

		let palette = Palette::default();
		let mut grid = Grid::new(14, 1);
		grid.fill(&snapshot, &palette, &GridOptions::default());

		let row_layout = shaper.row(&grid, 0);

		assert!(
			!row_layout.bidirectional_runs.is_empty(),
			"bidi_runs should not be empty"
		);
		assert!(!row_layout.text.is_empty(), "row_text should not be empty");

		// Debug output
		eprintln!("row_text: {:?}", row_layout.text);
		eprintln!("bidi_runs: {:?}", row_layout.bidirectional_runs);

		// There should be at least one RTL run for the Arabic text
		let has_rtl = row_layout
			.bidirectional_runs
			.iter()
			.any(|run| run.is_right_to_left);
		assert!(has_rtl, "should have at least one RTL bidi run");
	}
}
