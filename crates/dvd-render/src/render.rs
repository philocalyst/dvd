//! Snapshot in, pixels out.
//!
//! Terminals and text engines disagree about who owns position. Parley wants to
//! measure a paragraph and put glyphs wherever the font's advances land them; a
//! terminal has already decided, cell by cell, where every character goes. The
//! two are split down the middle here.
//!
//! Parley is asked only for what it is uniquely good at — which font covers a
//! character, which glyphs a sequence becomes once contextual shaping has had
//! its say, and what order those glyphs end up in once the bidirectional
//! algorithm has run. The grid keeps the rest: every *cluster* is pinned back to
//! the column its source character came from. That is what makes Arabic join
//! and reverse while columns still line up with the cursor and the backgrounds
//! behind them — the shaping is done over the whole row, so a letter knows its
//! neighbours, but the placement is the terminal's.
//!
//! Glyph outlines come from Skrifa and are cached as `BezPath`s keyed by font
//! and glyph id. A recording draws the same few dozen glyphs thousands of
//! times, so the cache turns outline extraction from a per-frame cost into a
//! per-recording one. Vello CPU then fills those paths, with `glifo` absent
//! from the build entirely: its `text` feature is the only edge that pulls it,
//! and supplying our own outlines means we do not need it.

use anyhow::Result;
use parley::{
	FontData, FontFamily, FontFamilyName, GenericFamily, Layout, LayoutContext, StyleProperty,
};
use rio_vt::ansi::CursorShape;
use rio_vt::crosswords::square::Wide;
use rio_vt::crosswords::style::StyleFlags;
use rustc_hash::FxHashMap;
use skrifa::instance::{LocationRef, Size};
use skrifa::outline::{DrawSettings, OutlinePen};
use skrifa::{FontRef, GlyphId, MetadataProvider};
use vello_cpu::color::{AlphaColor, Srgb};
use vello_cpu::kurbo::{Affine, BezPath, Rect, RoundedRect, Shape};
use vello_cpu::{Level, Pixmap, RenderContext, RenderSettings, Resources};

use crate::fonts::{Fonts, Metrics};
use crate::model::{Palette, Rgba, Snapshot};
use crate::stream::Rasterizer;

/// How the terminal panel sits on the canvas.
#[derive(Clone, Copy, Debug)]
pub struct Surface {
	/// Gap between the canvas edge and the panel.
	pub margin: u32,
	/// Gap between the panel edge and the first cell.
	pub padding: u32,
	pub border_radius: u32,
	/// Colour behind the panel. `None` leaves it transparent, so rounded
	/// corners composite cleanly onto whatever the image is embedded in.
	pub margin_fill: Option<Rgba>,
}

impl Default for Surface {
	fn default() -> Self {
		Self {
			margin: 0,
			padding: 24,
			// Rounded by default: the corners fall outside the panel fill, so a
			// recording composites onto a page instead of sitting in a hard box.
			border_radius: 12,
			margin_fill: None,
		}
	}
}

/// Identifies a cached outline. The render size never changes over a
/// recording, so it is not part of the key.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct GlyphKey {
	font: u64,
	glyph: u32,
}

/// Collects a glyph outline into a `BezPath`.
#[derive(Default)]
struct PathPen {
	path: BezPath,
}

impl OutlinePen for PathPen {
	fn move_to(&mut self, x: f32, y: f32) {
		self.path.move_to((x as f64, -y as f64));
	}
	fn line_to(&mut self, x: f32, y: f32) {
		self.path.line_to((x as f64, -y as f64));
	}
	fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
		self.path
			.quad_to((cx as f64, -cy as f64), (x as f64, -y as f64));
	}
	fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
		self.path.curve_to(
			(cx0 as f64, -cy0 as f64),
			(cx1 as f64, -cy1 as f64),
			(x as f64, -y as f64),
		);
	}
	fn close(&mut self) {
		self.path.close_path();
	}
}

/// One glyph, already pinned to its column.
struct Placed {
	key: GlyphKey,
	/// Pen position in grid pixels.
	x: f32,
	y: f32,
	/// Uniform scale for the outline. Almost always 1.0 — see
	/// [`Renderer::shape_row`] for the fallback glyph that is not.
	scale: f32,
	column: u16,
}

/// The CPU renderer.
pub struct Renderer {
	context: RenderContext,
	resources: Resources,
	fonts: Fonts,
	layout_context: LayoutContext<[u8; 4]>,
	outlines: FxHashMap<GlyphKey, BezPath>,
	/// Every face shaping has selected so far, by the id the outline cache keys
	/// on. Populated as rows are shaped, so resolving a glyph back to the file
	/// it came from is a hash lookup rather than a walk over the layout.
	faces: FxHashMap<u64, FontData>,
	/// The family list, built once. Cloning three names per row for a value
	/// that cannot change over a recording is work with nothing to show for it.
	families: Vec<FontFamilyName<'static>>,
	palette: Palette,
	surface: Surface,
	width: u16,
	height: u16,
	/// Where the top-left cell sits on the canvas.
	origin: (f64, f64),

	// Per-row scratch, reused across every row of every frame.
	row_text: String,
	/// Byte offset in `row_text` for the start of each column's character.
	column_of_byte: Vec<u16>,
	placed: Vec<Placed>,
	layout: Layout<[u8; 4]>,
}

impl Renderer {
	/// Build a renderer, deriving the canvas size from the grid.
	pub fn new(
		fonts: Fonts,
		palette: Palette,
		surface: Surface,
		columns: u16,
		screen_rows: u16,
		level: Level,
	) -> Result<Self> {
		let metrics = fonts.metrics;
		let chrome = 2 * (surface.margin + surface.padding);

		// Rounded up to an even size. H.264 subsamples chroma 2x2 and refuses an
		// odd width outright, so an odd canvas would mean either no video output
		// or a video a pixel narrower than the still beside it. One column of
		// background is a cheaper price than two outputs that disagree.
		let even = |value: u32| (value + 1) & !1;
		let width = even(columns as u32 * metrics.cell_width + chrome).min(u16::MAX as u32 - 1) as u16;
		let height =
			even(screen_rows as u32 * metrics.cell_height + chrome).min(u16::MAX as u32 - 1) as u16;

		let inset = (surface.margin + surface.padding) as f64;

		// The chosen family, then the bundled symbol face, then generic
		// monospace. Fontique walks per-script system fallback beyond that on
		// its own, which is what puts a glyph on screen for CJK, emoji or
		// Arabic when none of the three has one.
		let families = fonts
			.family_stack()
			.into_iter()
			.map(|name| FontFamilyName::Named(name.into()))
			.chain(std::iter::once(FontFamilyName::Generic(
				GenericFamily::Monospace,
			)))
			.collect();

		Ok(Self {
			// The level is the one detected once for the whole process and
			// shared with our own kernels — `vello_cpu` re-exports the very
			// same `fearless_simd::Level`.
			context: RenderContext::new_with(
				width,
				height,
				RenderSettings {
					level,
					..Default::default()
				},
			),
			resources: Resources::new(),
			fonts,
			layout_context: LayoutContext::new(),
			outlines: FxHashMap::default(),
			faces: FxHashMap::default(),
			families,
			palette,
			surface,
			width,
			height,
			origin: (inset, inset),
			row_text: String::new(),
			column_of_byte: Vec::new(),
			placed: Vec::new(),
			layout: Layout::default(),
		})
	}

	pub fn metrics(&self) -> Metrics {
		self.fonts.metrics
	}

	pub fn size(&self) -> (u16, u16) {
		(self.width, self.height)
	}

	fn paint(color: Rgba) -> AlphaColor<Srgb> {
		AlphaColor::from_rgba8(color[0], color[1], color[2], color[3])
	}

	/// Draw the panel the grid sits on.
	fn draw_surface(&mut self) {
		if let Some(fill) = self.surface.margin_fill {
			self.context.set_paint(Self::paint(fill));
			self.context
				.fill_rect(&Rect::new(0.0, 0.0, self.width as f64, self.height as f64));
		}

		let margin = self.surface.margin as f64;
		let panel = Rect::new(
			margin,
			margin,
			self.width as f64 - margin,
			self.height as f64 - margin,
		);

		self.context
			.set_paint(Self::paint(self.palette.named(
				rio_vt::config::colors::NamedColor::Background,
			)));

		if self.surface.border_radius > 0 {
			let rounded = RoundedRect::from_rect(panel, self.surface.border_radius as f64);
			self.context.fill_path(&rounded.to_path(0.1));
		} else {
			self.context.fill_rect(&panel);
		}
	}

	/// Fill cell backgrounds, merging horizontal runs of one colour.
	///
	/// Runs rather than per-cell rectangles because a terminal row is mostly one
	/// colour: an eighty-column row of default background is one rectangle
	/// instead of eighty, and the rasterizer sees a fraction of the edges.
	fn draw_backgrounds(&mut self, snapshot: &Snapshot) {
		let metrics = self.fonts.metrics;
		let background = self
			.palette
			.named(rio_vt::config::colors::NamedColor::Background);

		for row in 0..snapshot.screen_rows {
			let mut run_start = 0u16;
			let mut run_color = self
				.palette
				.resolve(snapshot.cell(0, row), &snapshot.styles)
				.background;

			for column in 1..=snapshot.columns {
				let color = if column == snapshot.columns {
					// Sentinel that always differs, to flush the final run.
					[0, 0, 0, 0]
				} else {
					self.palette
						.resolve(snapshot.cell(column, row), &snapshot.styles)
						.background
				};

				if color != run_color {
					// The panel is already this colour, so painting it again
					// would be a rectangle that changes nothing.
					if run_color != background {
						self.fill_cells(run_start, column, row, run_color, metrics);
					}
					run_start = column;
					run_color = color;
				}
			}
		}
	}

	fn fill_cells(&mut self, from: u16, to: u16, row: u16, color: Rgba, metrics: Metrics) {
		let (origin_x, origin_y) = self.origin;
		let rect = Rect::new(
			origin_x + (from as u32 * metrics.cell_width) as f64,
			origin_y + (row as u32 * metrics.cell_height) as f64,
			origin_x + (to as u32 * metrics.cell_width) as f64,
			origin_y + ((row as u32 + 1) * metrics.cell_height) as f64,
		);
		self.context.set_paint(Self::paint(color));
		self.context.fill_rect(&rect);
	}

	/// Shape one row and pin every cluster back to its source column.
	fn shape_row(&mut self, snapshot: &Snapshot, row: u16) {
		self.row_text.clear();
		self.column_of_byte.clear();
		self.placed.clear();

		for column in 0..snapshot.columns {
			let cell = snapshot.cell(column, row);
			// A wide character's trailing spacer holds no character of its own;
			// emitting it would draw the same glyph again one column right.
			if cell.wide() == Wide::Spacer {
				continue;
			}
			let resolved = self.palette.resolve(cell, &snapshot.styles);
			let character = match resolved.character {
				'\0' => ' ',
				other => other,
			};

			let start = self.row_text.len();
			self.row_text.push(character);
			self.column_of_byte
				.resize(self.row_text.len(), column);
			debug_assert!(self.column_of_byte.len() > start);

			// Combining marks do not fit in the packed cell, so `rio-vt` parks
			// them out of line in `extras`. Appending them to the same column
			// hands the base and its marks to the shaper as one cluster, which
			// is what stacks an accent on its letter instead of dropping it.
			// A hidden cell has already resolved to a space and gets none.
			if !resolved.flags.contains(StyleFlags::HIDDEN) {
				let marks = cell
					.extras_id()
					.and_then(|id| snapshot.extras.get(&id))
					.map(|extras| extras.zerowidth.as_slice())
					.unwrap_or_default();

				for mark in marks {
					self.row_text.push(*mark);
					self.column_of_byte.resize(self.row_text.len(), column);
				}
			}
		}

		if self.row_text.trim().is_empty() {
			return;
		}

		let mut builder = self.layout_context.ranged_builder(
			&mut self.fonts.context,
			&self.row_text,
			1.0,
			false,
		);
		builder.push_default(StyleProperty::FontFamily(FontFamily::List(
			std::borrow::Cow::Borrowed(&self.families),
		)));
		builder.push_default(StyleProperty::FontSize(self.fonts.size));
		builder.build_into(&mut self.layout, &self.row_text);

		// No width constraint, so nothing actually breaks — this only groups the
		// runs into a line so they can be walked. It is also why the
		// dictionary-based segmenter behind Parley's `complex-scripts` feature
		// is not needed: the grid decides where lines end, never the text.
		self.layout.break_all_lines(None);

		let metrics = self.fonts.metrics;
		for line in self.layout.lines() {
			for run in line.runs() {
				let font = run.font();
				let font_key = font.data.id();
				self.faces.entry(font_key).or_insert_with(|| font.clone());

				for cluster in run.clusters() {
					// This is the pin. The cluster knows which bytes of the row
					// it came from; those bytes know which column they were
					// typed in. Visual order within the run is Parley's
					// business — including the reversal for RTL — but the
					// anchor is always the terminal's own column.
					let column = self
						.column_of_byte
						.get(cluster.text_range().start)
						.copied()
						.unwrap_or(0);

					let cell_x = (column as u32 * metrics.cell_width) as f32;

					// How much room the terminal gave this character. A wide
					// character owns its trailing spacer, so it gets two cells.
					let span = match snapshot.cell(column, row).wide() {
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
						self.placed.push(Placed {
							key: GlyphKey {
								font: font_key,
								glyph: glyph.id,
							},
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
	}

	/// Fetch a glyph outline, extracting it only the first time it is seen.
	fn outline(&mut self, key: GlyphKey) -> Option<&BezPath> {
		if !self.outlines.contains_key(&key) {
			let path = self.extract_outline(key)?;
			self.outlines.insert(key, path);
		}
		self.outlines.get(&key)
	}

	fn extract_outline(&mut self, key: GlyphKey) -> Option<BezPath> {
		// Every face shaping has ever chosen was recorded as the row was shaped,
		// so the blob this glyph came from is one lookup away. Walking the
		// layout for it instead would also work, but only for glyphs belonging
		// to the row still in the layout — and it would rescan every run for
		// every glyph of every row.
		let font_data = self.faces.get(&key.font)?.clone();

		let font = FontRef::from_index(font_data.data.as_ref(), font_data.index).ok()?;
		let outlines = font.outline_glyphs();
		let glyph = outlines.get(GlyphId::from(key.glyph))?;

		let mut pen = PathPen::default();
		glyph
			.draw(
				DrawSettings::unhinted(Size::new(self.fonts.size), LocationRef::default()),
				&mut pen,
			)
			.ok()?;

		Some(pen.path)
	}

	/// Draw one row's glyphs, plus the underline and strikethrough that go with
	/// them.
	fn draw_row_text(&mut self, snapshot: &Snapshot, row: u16) {
		self.shape_row(snapshot, row);

		let metrics = self.fonts.metrics;
		let (origin_x, origin_y) = self.origin;
		let row_y = origin_y + (row as u32 * metrics.cell_height) as f64;

		// Taken out so the borrow of `self.placed` ends before `outline` needs
		// `&mut self`.
		let placed = std::mem::take(&mut self.placed);

		for glyph in &placed {
			let resolved = self
				.palette
				.resolve(snapshot.cell(glyph.column, row), &snapshot.styles);

			// A hidden cell resolves to a space, which has no outline anyway;
			// skipping early avoids the cache lookup.
			if resolved.flags.contains(StyleFlags::HIDDEN) {
				continue;
			}

			let color = self.cell_foreground(snapshot, glyph.column, row, resolved.foreground);
			// Scale first, then translate: the outline is cached at its own
			// origin, so scaling after the move would drag the glyph toward the
			// top-left of the canvas by the same factor.
			let transform = Affine::translate((origin_x + glyph.x as f64, row_y + glyph.y as f64))
				* Affine::scale(glyph.scale as f64);

			if let Some(path) = self.outline(glyph.key) {
				let path = path.clone();
				self.context.set_transform(transform);
				self.context.set_paint(Self::paint(color));
				self.context.fill_path(&path);
				self.context.reset_transform();
			}
		}

		self.placed = placed;

		self.draw_decorations(snapshot, row);
	}

	/// Underlines and strikethroughs, drawn as rectangles.
	///
	/// Rectangles rather than the font's own decoration glyphs because a run
	/// spanning several cells has to be continuous across them, and because the
	/// thickness has to land on whole pixels to stay crisp at recording sizes.
	fn draw_decorations(&mut self, snapshot: &Snapshot, row: u16) {
		let metrics = self.fonts.metrics;
		let (origin_x, origin_y) = self.origin;
		let row_y = origin_y + (row as u32 * metrics.cell_height) as f64;

		for column in 0..snapshot.columns {
			let resolved = self
				.palette
				.resolve(snapshot.cell(column, row), &snapshot.styles);

			let x0 = origin_x + (column as u32 * metrics.cell_width) as f64;
			let x1 = x0 + metrics.cell_width as f64;
			let color = resolved.underline.unwrap_or(resolved.foreground);

			if resolved.flags.intersects(StyleFlags::ALL_UNDERLINES) {
				let top = row_y + metrics.underline_offset as f64;
				self.context.set_paint(Self::paint(color));
				self.context.fill_rect(&Rect::new(
					x0,
					top,
					x1,
					top + metrics.underline_thickness as f64,
				));
			}

			if resolved.flags.contains(StyleFlags::STRIKEOUT) {
				let top = row_y + metrics.strikeout_offset as f64;
				self.context.set_paint(Self::paint(resolved.foreground));
				self.context.fill_rect(&Rect::new(
					x0,
					top,
					x1,
					top + metrics.strikeout_thickness as f64,
				));
			}
		}
	}

	/// The colour a glyph is drawn in, accounting for the cursor sitting on it.
	///
	/// A block cursor paints over the cell, so the glyph underneath has to be
	/// re-coloured or it disappears into the cursor. Picking by luma rather than
	/// inverting the cell keeps the character legible against any cursor colour.
	fn cell_foreground(
		&self,
		snapshot: &Snapshot,
		column: u16,
		row: u16,
		foreground: Rgba,
	) -> Rgba {
		if !self.cursor_covers(snapshot, column, row) {
			return foreground;
		}

		let cursor = self
			.palette
			.named(rio_vt::config::colors::NamedColor::Cursor);
		let luma = 0.2126 * cursor[0] as f32 + 0.7152 * cursor[1] as f32 + 0.0722 * cursor[2] as f32;

		if luma > 128.0 {
			[0, 0, 0, 255]
		} else {
			[255, 255, 255, 255]
		}
	}

	fn cursor_covers(&self, snapshot: &Snapshot, column: u16, row: u16) -> bool {
		snapshot.cursor_visible
			&& snapshot.cursor.content == CursorShape::Block
			&& snapshot.cursor.pos.col.0 as u16 == column
			&& snapshot.cursor.pos.row.0 as u16 == row
	}

	fn draw_cursor(&mut self, snapshot: &Snapshot) {
		if !snapshot.cursor_visible || snapshot.cursor.content == CursorShape::Hidden {
			return;
		}

		let metrics = self.fonts.metrics;
		let (origin_x, origin_y) = self.origin;
		let column = snapshot.cursor.pos.col.0 as u32;
		let row = snapshot.cursor.pos.row.0.max(0) as u32;

		if column >= snapshot.columns as u32 || row >= snapshot.screen_rows as u32 {
			return;
		}

		let x = origin_x + (column * metrics.cell_width) as f64;
		let y = origin_y + (row * metrics.cell_height) as f64;
		let width = metrics.cell_width as f64;
		let height = metrics.cell_height as f64;
		let thickness = (metrics.underline_thickness as f64 * 1.5).round().max(2.0);

		let rect = match snapshot.cursor.content {
			CursorShape::Block => Rect::new(x, y, x + width, y + height),
			CursorShape::Underline => Rect::new(x, y + height - thickness, x + width, y + height),
			CursorShape::Beam => Rect::new(x, y, x + thickness, y + height),
			CursorShape::Hidden => return,
		};

		self.context.set_paint(Self::paint(
			self.palette
				.named(rio_vt::config::colors::NamedColor::Cursor),
		));
		self.context.fill_rect(&rect);
	}
}

impl Rasterizer for Renderer {
	fn size(&self) -> (u16, u16) {
		(self.width, self.height)
	}

	fn render(&mut self, snapshot: &Snapshot, target: &mut Pixmap) {
		self.context.reset();

		self.draw_surface();
		self.draw_backgrounds(snapshot);

		// The block cursor is a background, painted before the text so the
		// glyph lands on top of it — which is why `cell_foreground` recolours
		// that glyph rather than inverting the cell.
		self.draw_cursor(snapshot);

		for row in 0..snapshot.screen_rows {
			self.draw_row_text(snapshot, row);
		}

		self.context.flush();
		self.context.render(target.as_mut(), &mut self.resources);
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use rio_vt::crosswords::square::Square;

	fn renderer(columns: u16, rows: u16) -> Renderer {
		let fonts = Fonts::resolve(Some("Liberation Mono"), 16.0, 1.0).unwrap();
		Renderer::new(
			fonts,
			Palette::default(),
			Surface {
				margin: 0,
				padding: 0,
				border_radius: 0,
				margin_fill: None,
			},
			columns,
			rows,
			Level::new(),
		)
		.unwrap()
	}

	/// Read a pixel back as straight (non-premultiplied) RGBA.
	///
	/// The pixmap stores premultiplied alpha; every colour these tests assert
	/// against is opaque, so undoing it is a no-op, but going through the
	/// conversion keeps the assertions honest if that ever stops being true.
	fn pixel(pixmap: &Pixmap, x: u16, y: u16) -> [u8; 4] {
		let width = pixmap.width() as usize;
		let premultiplied = pixmap.data()[y as usize * width + x as usize];
		let color = premultiplied.to_u8_array();
		if color[3] == 0 || color[3] == 255 {
			return color;
		}
		let unpremultiply =
			|channel: u8| ((channel as u32 * 255) / color[3] as u32).min(255) as u8;
		[
			unpremultiply(color[0]),
			unpremultiply(color[1]),
			unpremultiply(color[2]),
			color[3],
		]
	}

	/// How many pixels differ from the panel background across the whole
	/// canvas. A crude measure of "how much was drawn", which is all that is
	/// needed to tell one glyph from a glyph plus its accent.
	fn ink_count(pixmap: &Pixmap) -> usize {
		let background = Palette::default().named(rio_vt::config::colors::NamedColor::Background);
		(0..pixmap.height())
			.flat_map(|y| (0..pixmap.width()).map(move |x| (x, y)))
			.filter(|(x, y)| pixel(pixmap, *x, *y) != background)
			.count()
	}

	/// Any pixel in the cell that is not the background — i.e. ink was laid down.
	fn cell_has_ink(pixmap: &Pixmap, metrics: Metrics, column: u16, row: u16, background: [u8; 4]) -> bool {
		let x0 = column as u32 * metrics.cell_width;
		let y0 = row as u32 * metrics.cell_height;
		(y0..y0 + metrics.cell_height).any(|y| {
			(x0..x0 + metrics.cell_width)
				.any(|x| pixel(pixmap, x as u16, y as u16) != background)
		})
	}

	#[test]
	fn the_canvas_matches_the_grid_and_chrome() {
		let renderer = renderer(10, 4);
		let metrics = renderer.metrics();
		assert_eq!(
			renderer.size(),
			(
				(10 * metrics.cell_width) as u16,
				(4 * metrics.cell_height) as u16
			)
		);
	}

	#[test]
	fn padding_and_margin_grow_the_canvas() {
		let fonts = Fonts::resolve(Some("Liberation Mono"), 16.0, 1.0).unwrap();
		let metrics = fonts.metrics;
		let renderer = Renderer::new(
			fonts,
			Palette::default(),
			Surface {
				margin: 5,
				padding: 7,
				border_radius: 0,
				margin_fill: None,
			},
			10,
			4,
			Level::new(),
		)
		.unwrap();

		let chrome = 2 * (5 + 7);
		assert_eq!(
			renderer.size(),
			(
				(10 * metrics.cell_width + chrome) as u16,
				(4 * metrics.cell_height + chrome) as u16
			)
		);
	}

	#[test]
	fn an_empty_screen_renders_as_the_background_colour() {
		let mut renderer = renderer(8, 2);
		let (width, height) = renderer.size();
		let mut pixmap = Pixmap::new(width, height);

		let mut snapshot = Snapshot::new(8, 2);
		snapshot.cells.fill(Square::from_char(' '));
		renderer.render(&snapshot, &mut pixmap);

		let background = Palette::default().named(rio_vt::config::colors::NamedColor::Background);
		assert_eq!(pixel(&pixmap, 0, 0), background);
		assert_eq!(pixel(&pixmap, width - 1, height - 1), background);
	}

	/// The test that proves glyphs actually reach the surface, and land in the
	/// column they were written to rather than being packed to the left.
	#[test]
	fn a_glyph_lands_in_its_own_column() {
		let mut renderer = renderer(8, 2);
		let metrics = renderer.metrics();
		let (width, height) = renderer.size();
		let mut pixmap = Pixmap::new(width, height);

		let mut snapshot = Snapshot::new(8, 2);
		snapshot.cells.fill(Square::from_char(' '));
		// A glyph with plenty of ink, in the middle of the row.
		snapshot.cells[3] = Square::from_char('W');

		renderer.render(&snapshot, &mut pixmap);

		let background = Palette::default().named(rio_vt::config::colors::NamedColor::Background);
		assert!(
			cell_has_ink(&pixmap, metrics, 3, 0, background),
			"the glyph should be drawn in column 3"
		);
		assert!(
			!cell_has_ink(&pixmap, metrics, 0, 0, background),
			"column 0 is a space and should stay empty"
		);
		assert!(
			!cell_has_ink(&pixmap, metrics, 6, 0, background),
			"column 6 is a space and should stay empty"
		);
	}

	#[test]
	fn a_block_cursor_paints_its_cell() {
		let mut renderer = renderer(8, 2);
		let metrics = renderer.metrics();
		let (width, height) = renderer.size();
		let mut pixmap = Pixmap::new(width, height);

		let mut snapshot = Snapshot::new(8, 2);
		snapshot.cells.fill(Square::from_char(' '));
		snapshot.cursor_visible = true;
		snapshot.cursor.content = CursorShape::Block;

		renderer.render(&snapshot, &mut pixmap);

		let cursor = Palette::default().named(rio_vt::config::colors::NamedColor::Cursor);
		assert_eq!(
			pixel(&pixmap, (metrics.cell_width / 2) as u16, (metrics.cell_height / 2) as u16),
			cursor,
			"the cursor cell should be filled with the cursor colour"
		);
	}

	/// RTL: Arabic must reverse within the row while still occupying the same
	/// columns the terminal assigned. Both ends of the run carry ink, and the
	/// untouched columns beyond it stay clear.
	#[test]
	fn arabic_occupies_the_columns_it_was_written_to() {
		let mut renderer = renderer(10, 1);
		let metrics = renderer.metrics();
		let (width, height) = renderer.size();
		let mut pixmap = Pixmap::new(width, height);

		let mut snapshot = Snapshot::new(10, 1);
		snapshot.cells.fill(Square::from_char(' '));
		for (index, character) in "سلام".chars().enumerate() {
			snapshot.cells[index] = Square::from_char(character);
		}

		renderer.render(&snapshot, &mut pixmap);

		let background = Palette::default().named(rio_vt::config::colors::NamedColor::Background);
		let inked = (0..4)
			.filter(|column| cell_has_ink(&pixmap, metrics, *column, 0, background))
			.count();

		assert!(
			inked >= 2,
			"the Arabic run should put ink in the columns it was written to, found {inked}"
		);
		assert!(
			!cell_has_ink(&pixmap, metrics, 8, 0, background),
			"columns past the run must stay empty"
		);
	}

	/// The point of bundling a Nerd Font. Liberation Mono is the primary here
	/// and has nothing in the Private Use Area, and no system face declares
	/// coverage for it either — so without the symbol face in the fallback
	/// chain every one of these cells would be blank or tofu.
	#[test]
	fn nerd_font_glyphs_reach_the_surface() {
		// One from each corner of the set: a powerline separator, the git
		// branch mark a themed prompt opens with, a devicon, a Font Awesome
		// glyph, and a Material Design one from plane 15.
		for (name, character) in [
			("powerline separator", '\u{e0b0}'),
			("git branch", '\u{e0a0}'),
			("devicon", '\u{e7a8}'),
			("font awesome folder", '\u{f07b}'),
			("material design", '\u{f0001}'),
		] {
			let mut renderer = renderer(4, 1);
			let metrics = renderer.metrics();
			let (width, height) = renderer.size();
			let mut pixmap = Pixmap::new(width, height);

			let mut snapshot = Snapshot::new(4, 1);
			snapshot.cells.fill(Square::from_char(' '));
			snapshot.cells[1] = Square::from_char(character);

			renderer.render(&snapshot, &mut pixmap);

			let background = Palette::default().named(rio_vt::config::colors::NamedColor::Background);
			assert!(
				cell_has_ink(&pixmap, metrics, 1, 0, background),
				"{name} (U+{:04X}) should draw something",
				character as u32
			);
			// An icon may overhang its cell by a pixel or two, the way an
			// italic serif or a script descender does — real terminals show the
			// same overhang and clipping it would chop the glyph. What must not
			// happen is the glyph advancing the grid: two columns over is far
			// past any overhang and belongs to a space.
			assert!(
				!cell_has_ink(&pixmap, metrics, 3, 0, background),
				"{name} (U+{:04X}) is drawing two columns from where it was written",
				character as u32
			);
		}
	}

	/// Box drawing and block elements are the other half of "and other
	/// glyphs": a TUI is mostly these, and a face that lacks them turns a
	/// bordered panel into a field of tofu.
	#[test]
	fn box_drawing_and_block_elements_render() {
		let mut renderer = renderer(8, 1);
		let metrics = renderer.metrics();
		let (width, height) = renderer.size();
		let mut pixmap = Pixmap::new(width, height);

		let mut snapshot = Snapshot::new(8, 1);
		snapshot.cells.fill(Square::from_char(' '));
		for (column, character) in "─│┌┐└┘├█".chars().enumerate() {
			snapshot.cells[column] = Square::from_char(character);
		}

		renderer.render(&snapshot, &mut pixmap);

		let background = Palette::default().named(rio_vt::config::colors::NamedColor::Background);
		for column in 0..8u16 {
			assert!(
				cell_has_ink(&pixmap, metrics, column, 0, background),
				"column {column} of the box drawing run should carry ink"
			);
		}
	}

	/// A combining mark lives in the cell's `extras`, not in the packed cell,
	/// so it only reaches the shaper if the row builder goes looking for it.
	/// Without that, a bare `e` and an `e` carrying a combining acute render
	/// identically — the accent is simply dropped.
	///
	/// The mark also must not cost a column. Whether the shaper stacks it as a
	/// separate mark glyph or composes the pair into a single `é` is the font's
	/// business; either way the terminal assigned one cell and the grid has to
	/// come out the same width.
	#[test]
	fn a_combining_mark_is_drawn_over_its_base() {
		use rio_vt::crosswords::square::Extras;

		let base = {
			let mut renderer = renderer(4, 1);
			let (width, height) = renderer.size();
			let mut pixmap = Pixmap::new(width, height);
			let mut snapshot = Snapshot::new(4, 1);
			snapshot.cells.fill(Square::from_char(' '));
			snapshot.cells[1] = Square::from_char('e');
			renderer.render(&snapshot, &mut pixmap);
			ink_count(&pixmap)
		};

		let mut renderer = renderer(4, 1);
		let metrics = renderer.metrics();
		let (width, height) = renderer.size();
		let mut pixmap = Pixmap::new(width, height);

		let mut snapshot = Snapshot::new(4, 1);
		snapshot.cells.fill(Square::from_char(' '));
		let mut cell = Square::from_char('e');
		cell.set_extras_id(Some(1));
		snapshot.cells[1] = cell;
		snapshot.extras.insert(
			1,
			Extras {
				// COMBINING ACUTE ACCENT
				zerowidth: vec!['\u{0301}'],
				hyperlink: None,
			},
		);

		renderer.render(&snapshot, &mut pixmap);

		assert!(
			ink_count(&pixmap) > base,
			"the accent should add ink on top of the bare letter"
		);
		let background = Palette::default().named(rio_vt::config::colors::NamedColor::Background);
		assert!(
			!cell_has_ink(&pixmap, metrics, 2, 0, background),
			"a zero-width mark must not take a column of its own"
		);
	}

	#[test]
	fn a_background_colour_fills_its_cell() {
		use rio_vt::config::colors::{AnsiColor, ColorRgb};
		use rio_vt::crosswords::style::Style;

		let mut renderer = renderer(4, 1);
		let metrics = renderer.metrics();
		let (width, height) = renderer.size();
		let mut pixmap = Pixmap::new(width, height);

		let mut snapshot = Snapshot::new(4, 1);
		snapshot.cells.fill(Square::from_char(' '));
		snapshot.styles = vec![
			Style::default(),
			Style {
				bg: AnsiColor::Spec(ColorRgb { r: 255, g: 0, b: 0 }),
				..Style::default()
			},
		];
		let mut cell = Square::from_char(' ');
		cell.set_style_id(1);
		snapshot.cells[1] = cell;

		renderer.render(&snapshot, &mut pixmap);

		let middle_x = (metrics.cell_width + metrics.cell_width / 2) as u16;
		let middle_y = (metrics.cell_height / 2) as u16;
		assert_eq!(pixel(&pixmap, middle_x, middle_y), [255, 0, 0, 255]);
	}
}
