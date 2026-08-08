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
use crate::geom::Color;
use crate::grid::GridOptions;
use crate::model::{Palette, Snapshot};
use crate::stream::Rasterizer;

// --- Constants (Magic Numbers Extracted) ---
const DEFAULT_SURFACE_MARGIN: u32 = 0;
const DEFAULT_SURFACE_PADDING: u32 = 24;
const DEFAULT_BORDER_RADIUS: u32 = 12;

const PATH_TOLERANCE: f64 = 0.1;
const DEFAULT_SCALE: f32 = 1.0;
const CURSOR_THICKNESS_MULTIPLIER: f64 = 1.5;
const MINIMUM_CURSOR_THICKNESS: f64 = 2.0;

const WIDE_CHARACTER_SPAN: u16 = 2;
const SINGLE_CHARACTER_SPAN: u16 = 1;
const FALLBACK_CHARACTER: char = ' ';

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
	pub margin_fill: Option<Color>,
}

impl Default for Surface {
	fn default() -> Self {
		Self {
			margin: DEFAULT_SURFACE_MARGIN,
			padding: DEFAULT_SURFACE_PADDING,
			// Rounded by default: the corners fall outside the panel fill, so a
			// recording composites onto a page instead of sitting in a hard box.
			border_radius: DEFAULT_BORDER_RADIUS,
			margin_fill: None,
		}
	}
}

/// The rectangle the panel occupies on the canvas, given the surface chrome
/// and the canvas size. Shared by the paint pass so it does not duplicate the
/// margin arithmetic.
pub fn panel_rect(frame: &crate::geom::Frame, surface: &Surface) -> crate::geom::PixelRect {
	let margin = surface.margin as f32;
	crate::geom::PixelRect::new(
		margin,
		margin,
		frame.canvas.0 as f32 - 2.0 * margin,
		frame.canvas.1 as f32 - 2.0 * margin,
	)
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
	fn move_to(&mut self, point_x: f32, point_y: f32) {
		self.path.move_to((point_x as f64, -point_y as f64));
	}

	fn line_to(&mut self, point_x: f32, point_y: f32) {
		self.path.line_to((point_x as f64, -point_y as f64));
	}

	fn quad_to(&mut self, control_x: f32, control_y: f32, point_x: f32, point_y: f32) {
		self.path.quad_to(
			(control_x as f64, -control_y as f64),
			(point_x as f64, -point_y as f64),
		);
	}

	fn curve_to(
		&mut self,
		control_x0: f32,
		control_y0: f32,
		control_x1: f32,
		control_y1: f32,
		point_x: f32,
		point_y: f32,
	) {
		self.path.curve_to(
			(control_x0 as f64, -control_y0 as f64),
			(control_x1 as f64, -control_y1 as f64),
			(point_x as f64, -point_y as f64),
		);
	}

	fn close(&mut self) {
		self.path.close_path();
	}
}

/// One glyph, already pinned to its column.
struct PlacedGlyph {
	key: GlyphKey,
	/// Pen position in grid pixels.
	horizontal_offset: f32,
	vertical_offset: f32,
	/// Uniform scale for the outline.
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
	placed: Vec<PlacedGlyph>,
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
		let chrome_size = 2 * (surface.margin + surface.padding);

		// Rounded up to an even size. H.264 subsamples chroma 2x2 and refuses an
		// odd width outright, so an odd canvas would mean either no video output
		// or a video a pixel narrower than the still beside it. One column of
		// background is a cheaper price than two outputs that disagree.
		let width = Self::round_up_to_even(columns as u32 * metrics.cell_width + chrome_size)
			.min(u16::MAX as u32 - 1) as u16;

		let height = Self::round_up_to_even(screen_rows as u32 * metrics.cell_height + chrome_size)
			.min(u16::MAX as u32 - 1) as u16;

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

	#[inline]
	fn round_up_to_even(value: u32) -> u32 {
		(value + 1) & !1
	}

	pub fn metrics(&self) -> Metrics {
		self.fonts.metrics
	}

	pub fn size(&self) -> (u16, u16) {
		(self.width, self.height)
	}

	fn paint(color: Color) -> AlphaColor<Srgb> {
		AlphaColor::from_rgba8(color.red(), color.green(), color.blue(), color.alpha())
	}

	/// Draw the panel the grid sits on.
	fn draw_surface(&mut self) {
		if let Some(fill_color) = self.surface.margin_fill {
			self.context.set_paint(Self::paint(fill_color));
			self.context
				.fill_rect(&Rect::new(0.0, 0.0, self.width as f64, self.height as f64));
		}

		let margin_offset = self.surface.margin as f64;
		let panel_rectangle = Rect::new(
			margin_offset,
			margin_offset,
			self.width as f64 - margin_offset,
			self.height as f64 - margin_offset,
		);

		let background_color = self
			.palette
			.named(rio_vt::config::colors::NamedColor::Background);
		self.context.set_paint(Self::paint(background_color));

		if self.surface.border_radius > 0 {
			let rounded_panel =
				RoundedRect::from_rect(panel_rectangle, self.surface.border_radius as f64);
			self.context
				.fill_path(&rounded_panel.to_path(PATH_TOLERANCE));
		} else {
			self.context.fill_rect(&panel_rectangle);
		}
	}

	/// Fill cell backgrounds, merging horizontal runs of one colour.
	fn draw_backgrounds(&mut self, snapshot: &Snapshot) {
		let default_background = self
			.palette
			.named(rio_vt::config::colors::NamedColor::Background);
		let options = GridOptions::default();

		for row in 0..snapshot.screen_rows {
			let mut current_run_start = 0u16;
			let mut current_run_color = self.resolve_cell_background(snapshot, 0, row, &options);

			for column in 1..=snapshot.columns {
				let next_color = if column == snapshot.columns {
					Color::TRANSPARENT // Sentinel to flush the final run.
				} else {
					self.resolve_cell_background(snapshot, column, row, &options)
				};

				if next_color != current_run_color {
					if current_run_color != default_background {
						self.fill_cell_range(current_run_start, column, row, current_run_color);
					}
					current_run_start = column;
					current_run_color = next_color;
				}
			}
		}
	}

	#[inline]
	fn resolve_cell_background(
		&self,
		snapshot: &Snapshot,
		column: u16,
		row: u16,
		options: &GridOptions,
	) -> Color {
		self.palette
			.resolve(snapshot.cell(column, row), &snapshot.styles, options)
			.background
	}

	fn fill_cell_range(&mut self, start_column: u16, end_column: u16, row: u16, color: Color) {
		let (origin_x, origin_y) = self.origin;
		let metrics = self.fonts.metrics;

		let start_x = origin_x + (start_column as u32 * metrics.cell_width) as f64;
		let start_y = origin_y + (row as u32 * metrics.cell_height) as f64;
		let end_x = origin_x + (end_column as u32 * metrics.cell_width) as f64;
		let end_y = origin_y + ((row as u32 + 1) * metrics.cell_height) as f64;

		let fill_rectangle = Rect::new(start_x, start_y, end_x, end_y);

		self.context.set_paint(Self::paint(color));
		self.context.fill_rect(&fill_rectangle);
	}

	/// Shape one row and pin every cluster back to its source column.
	fn shape_row(&mut self, snapshot: &Snapshot, row: u16) {
		self.row_text.clear();
		self.column_of_byte.clear();
		self.placed.clear();

		self.build_text_string(snapshot, row);

		if self.row_text.trim().is_empty() {
			return;
		}

		self.layout_text();
		self.place_glyphs(snapshot, row);
	}

	fn build_text_string(&mut self, snapshot: &Snapshot, row: u16) {
		let options = GridOptions::default();

		for column in 0..snapshot.columns {
			let cell = snapshot.cell(column, row);

			if cell.wide() == Wide::Spacer {
				continue;
			}

			let resolved_style = self.palette.resolve(cell, &snapshot.styles, &options);
			let character = match resolved_style.character {
				'\0' => FALLBACK_CHARACTER,
				valid_character => valid_character,
			};

			self.row_text.push(character);
			self.column_of_byte.resize(self.row_text.len(), column);

			if !resolved_style.flags.contains(StyleFlags::HIDDEN) {
				if let Some(extras_id) = cell.extras_id() {
					if let Some(extras) = snapshot.extras.get(&extras_id) {
						for mark in extras.zerowidth.as_slice() {
							self.row_text.push(*mark);
							self.column_of_byte.resize(self.row_text.len(), column);
						}
					}
				}
			}
		}
	}

	fn layout_text(&mut self) {
		let mut builder = self.layout_context.ranged_builder(
			&mut self.fonts.context,
			&self.row_text,
			DEFAULT_SCALE,
			false,
		);

		builder.push_default(StyleProperty::FontFamily(FontFamily::List(
			std::borrow::Cow::Borrowed(&self.families),
		)));
		builder.push_default(StyleProperty::FontSize(self.fonts.size));
		builder.build_into(&mut self.layout, &self.row_text);

		// No width constraint, grid decides where lines end.
		self.layout.break_all_lines(None);
	}

	fn place_glyphs(&mut self, snapshot: &Snapshot, row: u16) {
		let metrics = self.fonts.metrics;

		for line in self.layout.lines() {
			for run in line.runs() {
				let font = run.font();
				let font_key = font.data.id();
				self.faces.entry(font_key).or_insert_with(|| font.clone());

				for cluster in run.clusters() {
					let column = self
						.column_of_byte
						.get(cluster.text_range().start)
						.copied()
						.unwrap_or(0);

					let cell_horizontal_offset = (column as u32 * metrics.cell_width) as f32;

					let span = match snapshot.cell(column, row).wide() {
						Wide::Wide => WIDE_CHARACTER_SPAN,
						_ => SINGLE_CHARACTER_SPAN,
					};
					let span_width = (span as u32 * metrics.cell_width) as f32;

					let advance: f32 = cluster.glyphs().map(|glyph| glyph.advance).sum();
					let scale = if advance > span_width && advance > 0.0 {
						span_width / advance
					} else {
						DEFAULT_SCALE
					};

					let centering = (span_width - advance * scale).max(0.0) / 2.0;

					for glyph in cluster.glyphs() {
						self.placed.push(PlacedGlyph {
							key: GlyphKey {
								font: font_key,
								glyph: glyph.id,
							},
							horizontal_offset: cell_horizontal_offset + centering + glyph.x * scale,
							vertical_offset: metrics.baseline + glyph.y * scale,
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
			if let Some(path) = self.extract_outline(key) {
				self.outlines.insert(key, path);
			}
		}
		self.outlines.get(&key)
	}

	fn extract_outline(&mut self, key: GlyphKey) -> Option<BezPath> {
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

	/// Draw one row's glyphs, plus the underline and strikethrough that go with them.
	fn draw_row_text(&mut self, snapshot: &Snapshot, row: u16) {
		self.shape_row(snapshot, row);

		let metrics = self.fonts.metrics;
		let (origin_x, origin_y) = self.origin;
		let row_vertical_offset = origin_y + (row as u32 * metrics.cell_height) as f64;
		let options = GridOptions::default();

		// Taken out so the borrow of `self.placed` ends before `outline` needs `&mut self`.
		let placed_glyphs = std::mem::take(&mut self.placed);

		for glyph in &placed_glyphs {
			let resolved_style =
				self.palette
					.resolve(snapshot.cell(glyph.column, row), &snapshot.styles, &options);

			if resolved_style.flags.contains(StyleFlags::HIDDEN) {
				continue;
			}

			let color =
				self.cell_foreground(snapshot, glyph.column, row, resolved_style.foreground);
			let absolute_x = origin_x + glyph.horizontal_offset as f64;
			let absolute_y = row_vertical_offset + glyph.vertical_offset as f64;

			let transform =
				Affine::translate((absolute_x, absolute_y)) * Affine::scale(glyph.scale as f64);

			if let Some(path) = self.outline(glyph.key) {
				let path = path.clone();
				self.context.set_transform(transform);
				self.context.set_paint(Self::paint(color));
				self.context.fill_path(&path);
				self.context.reset_transform();
			}
		}

		self.placed = placed_glyphs;
		self.draw_decorations(snapshot, row);
	}

	fn draw_decorations(&mut self, snapshot: &Snapshot, row: u16) {
		let metrics = self.fonts.metrics;
		let (origin_x, origin_y) = self.origin;
		let row_vertical_offset = origin_y + (row as u32 * metrics.cell_height) as f64;
		let options = GridOptions::default();

		for column in 0..snapshot.columns {
			let resolved_style =
				self.palette
					.resolve(snapshot.cell(column, row), &snapshot.styles, &options);

			if !resolved_style
				.flags
				.intersects(StyleFlags::ALL_UNDERLINES | StyleFlags::STRIKEOUT)
			{
				continue; // Skip early if no decorations are needed
			}

			let start_x = origin_x + (column as u32 * metrics.cell_width) as f64;
			let end_x = start_x + metrics.cell_width as f64;
			let decoration_color = resolved_style
				.underline
				.unwrap_or(resolved_style.foreground);

			if resolved_style.flags.intersects(StyleFlags::ALL_UNDERLINES) {
				let underline_top = row_vertical_offset + metrics.underline_offset as f64;
				self.context.set_paint(Self::paint(decoration_color));
				self.context.fill_rect(&Rect::new(
					start_x,
					underline_top,
					end_x,
					underline_top + metrics.underline_thickness as f64,
				));
			}

			if resolved_style.flags.contains(StyleFlags::STRIKEOUT) {
				let strikeout_top = row_vertical_offset + metrics.strikeout_offset as f64;
				self.context
					.set_paint(Self::paint(resolved_style.foreground));
				self.context.fill_rect(&Rect::new(
					start_x,
					strikeout_top,
					end_x,
					strikeout_top + metrics.strikeout_thickness as f64,
				));
			}
		}
	}

	fn cell_foreground(
		&self,
		snapshot: &Snapshot,
		column: u16,
		row: u16,
		foreground: Color,
	) -> Color {
		if !self.cursor_covers(snapshot, column, row) {
			return foreground;
		}

		self.palette
			.named(rio_vt::config::colors::NamedColor::Cursor)
			.contrasting()
	}

	fn cursor_covers(&self, snapshot: &Snapshot, column: u16, row: u16) -> bool {
		snapshot.cursor_visible
			&& snapshot.cursor.content == CursorShape::Block
			&& (snapshot.cursor.pos.col.0 as u16) == column
			&& (snapshot.cursor.pos.row.0 as u16) == row
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

		let start_x = origin_x + (column * metrics.cell_width) as f64;
		let start_y = origin_y + (row * metrics.cell_height) as f64;
		let width = metrics.cell_width as f64;
		let height = metrics.cell_height as f64;

		let calculated_thickness = metrics.underline_thickness as f64 * CURSOR_THICKNESS_MULTIPLIER;
		let final_thickness = calculated_thickness.round().max(MINIMUM_CURSOR_THICKNESS);

		let cursor_rectangle = match snapshot.cursor.content {
			CursorShape::Block => Rect::new(start_x, start_y, start_x + width, start_y + height),
			CursorShape::Underline => Rect::new(
				start_x,
				start_y + height - final_thickness,
				start_x + width,
				start_y + height,
			),
			CursorShape::Beam => Rect::new(
				start_x,
				start_y,
				start_x + final_thickness,
				start_y + height,
			),
			CursorShape::Hidden => return,
		};

		let cursor_color = self
			.palette
			.named(rio_vt::config::colors::NamedColor::Cursor);
		self.context.set_paint(Self::paint(cursor_color));
		self.context.fill_rect(&cursor_rectangle);
	}
}

impl Rasterizer for Renderer {
	fn dimensions(&self) -> (u16, u16) {
		(self.width, self.height)
	}

	fn render(&mut self, snapshot: &Snapshot, target: &mut Pixmap) {
		self.context.reset();

		self.draw_surface();
		self.draw_backgrounds(snapshot);
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
		let unpremultiply = |channel: u8| ((channel as u32 * 255) / color[3] as u32).min(255) as u8;
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
		let background = Palette::default()
			.named(rio_vt::config::colors::NamedColor::Background)
			.channels();
		(0..pixmap.height())
			.flat_map(|y| (0..pixmap.width()).map(move |x| (x, y)))
			.filter(|(x, y)| pixel(pixmap, *x, *y) != background)
			.count()
	}

	/// Any pixel in the cell that is not the background — i.e. ink was laid down.
	fn cell_has_ink(
		pixmap: &Pixmap,
		metrics: Metrics,
		column: u16,
		row: u16,
		background: [u8; 4],
	) -> bool {
		let x0 = column as u32 * metrics.cell_width;
		let y0 = row as u32 * metrics.cell_height;
		(y0..y0 + metrics.cell_height).any(|y| {
			(x0..x0 + metrics.cell_width).any(|x| pixel(pixmap, x as u16, y as u16) != background)
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

		let background = Palette::default()
			.named(rio_vt::config::colors::NamedColor::Background)
			.channels();
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

		let background = Palette::default()
			.named(rio_vt::config::colors::NamedColor::Background)
			.channels();
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

		let cursor = Palette::default()
			.named(rio_vt::config::colors::NamedColor::Cursor)
			.channels();
		assert_eq!(
			pixel(
				&pixmap,
				(metrics.cell_width / 2) as u16,
				(metrics.cell_height / 2) as u16
			),
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

		let background = Palette::default()
			.named(rio_vt::config::colors::NamedColor::Background)
			.channels();
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

			let background = Palette::default()
				.named(rio_vt::config::colors::NamedColor::Background)
				.channels();
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

		let background = Palette::default()
			.named(rio_vt::config::colors::NamedColor::Background)
			.channels();
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
		let background = Palette::default()
			.named(rio_vt::config::colors::NamedColor::Background)
			.channels();
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
