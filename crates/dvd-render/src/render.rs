//! Snapshot in, pixels out.
//!
//! What is left here after the paint pass took over is deliberately small: a
//! canvas size, a `vello_cpu` context, and the translation from the shapes
//! [`crate::paint`] emits into the calls that context wants. Everything about
//! *what* a frame looks like — which cells changed, what colour they resolve
//! to, how a row shapes, where the caret goes, how an underline is drawn —
//! lives in [`crate::grid`], [`crate::shape`], [`crate::sprite`] and
//! [`crate::paint`], and is shared with the SVG backend.
//!
//! That split is the whole point. This file used to carry its own copy of
//! every one of those decisions: a second shaper, a second outline cache, a
//! second background-run merger, a second cursor geometry. It resolved each
//! cell's palette four separate times per row — once for the background, once
//! to build the row's text, once for the glyph colour, once for the
//! decorations — and cloned a `BezPath` per glyph per frame to escape a
//! borrow. None of that was wrong so much as unshared, and unshared is how
//! the SVG output quietly stopped matching the video beside it.
//!
//! Glyph outlines come from Skrifa and are cached by [`crate::outline`]. A
//! recording draws the same few dozen glyphs thousands of times, so the cache
//! turns outline extraction from a per-frame cost into a per-recording one.
//! Vello CPU then fills those paths, with `glifo` absent from the build
//! entirely: its `text` feature is the only edge that pulls it, and supplying
//! our own outlines means we do not need it.

use anyhow::Result;
use vello_cpu::color::{AlphaColor, Srgb};
use vello_cpu::kurbo::{Affine, BezPath, Rect, RoundedRect, Shape};
use vello_cpu::{Level, Pixmap, RenderContext, RenderSettings, Resources};

use crate::fonts::{Fonts, Metrics};
use crate::geom::{Color, Frame, PixelRect, Point, Size};
use crate::grid::{Grid, GridOptions};
use crate::model::{Palette, Placement, Snapshot};
use crate::outline::{GlyphKey, Outlines};
use crate::paint::{self, Painter, Text};
use crate::shape::{GlyphRun, Shaper};
use crate::stream::Rasterizer;

/// How finely a rounded rectangle is flattened into a path. A tenth of a pixel
/// is well below what survives the trip through chroma subsampling.
const PATH_TOLERANCE: f64 = 0.1;

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
			margin: 0,
			padding: 24,
			// Rounded by default: the corners fall outside the panel fill, so a
			// recording composites onto a page instead of sitting in a hard box.
			border_radius: 12,
			margin_fill: None,
		}
	}
}

impl Surface {
	/// Everything between the canvas edge and the first cell.
	#[inline]
	pub const fn inset(&self) -> u32 {
		self.margin + self.padding
	}

	/// The rectangle the panel occupies, given the canvas size.
	///
	/// On [`Surface`] rather than free-standing so that both backends reach
	/// the same arithmetic through the same name — the margin used to be
	/// subtracted independently in three places.
	#[inline]
	pub fn panel_rect(&self, frame: &Frame) -> PixelRect {
		let margin = self.margin as f32;
		PixelRect::new(
			margin,
			margin,
			frame.canvas.width as f32 - 2.0 * margin,
			frame.canvas.height as f32 - 2.0 * margin,
		)
	}

	/// The canvas a grid of this size needs, and where cell `(0, 0)` lands on
	/// it.
	///
	/// Rounded up to an even size. H.264 subsamples chroma 2x2 and refuses an
	/// odd width outright, so an odd canvas would mean either no video output
	/// or a video a pixel narrower than the still beside it. One column of
	/// background is a cheaper price than two outputs that disagree.
	pub fn frame_for(&self, metrics: Metrics, columns: u16, rows: u16) -> Frame {
		let chrome = 2 * self.inset();
		let even = |value: u32| ((value + 1) & !1).min(u16::MAX as u32 - 1) as u16;

		Frame::new(
			metrics,
			Point::new(self.inset() as f32, self.inset() as f32),
			Size::new(
				even(columns as u32 * metrics.cell_width + chrome),
				even(rows as u32 * metrics.cell_height + chrome),
			),
		)
	}
}

/// The CPU renderer.
///
/// Owns the state a frame is drawn *from* — the resolved grid, the row shaper,
/// the outline cache — and the `vello_cpu` context it is drawn *into*. It
/// implements [`Rasterizer`] so [`crate::stream::Encoder`] can hand it
/// snapshots without knowing any of that.
pub struct Renderer {
	context: RenderContext,
	resources: Resources,
	shaper: Shaper,
	outlines: Outlines,
	grid: Grid,
	palette: Palette,
	options: GridOptions,
	surface: Surface,
	frame: Frame,
}

impl Renderer {
	pub fn new(
		fonts: Fonts,
		palette: Palette,
		options: GridOptions,
		surface: Surface,
		columns: u16,
		rows: u16,
		level: Level,
	) -> Result<Self> {
		let frame = surface.frame_for(fonts.metrics, columns, rows);

		Ok(Self {
			context: RenderContext::new_with(
				frame.canvas.width,
				frame.canvas.height,
				RenderSettings {
					level,
					..Default::default()
				},
			),
			resources: Resources::new(),
			shaper: Shaper::new(fonts),
			outlines: Outlines::new(),
			grid: Grid::new(columns, rows),
			palette,
			options,
			surface,
			frame,
		})
	}

	pub fn metrics(&self) -> Metrics {
		self.frame.metrics
	}

	pub fn size(&self) -> Size {
		self.frame.canvas
	}
}

impl Rasterizer for Renderer {
	fn dimensions(&self) -> (u16, u16) {
		(self.frame.canvas.width, self.frame.canvas.height)
	}

	fn render(&mut self, snapshot: &Snapshot, target: &mut Pixmap) {
		self.context.reset();
		self.grid.fill(snapshot, &self.palette, &self.options);

		// Every frame is a full redraw. The pixmap handed in is drawn from a
		// pool and carries whatever the last frame that used it left behind,
		// so there is no previous picture underneath to preserve — damage
		// tracking would save work only if the surface persisted, and making
		// it persist would serialise the encoder threads on one buffer. The
		// saving that *does* apply, and does not need persistence, is the row
		// shaping cache, which is keyed on content rather than on damage.
		let damage = crate::grid::Damage::everything(self.grid.columns, self.grid.rows);

		let mut surface = VelloPainter {
			context: &mut self.context,
		};

		paint::paint(
			&self.grid,
			&damage,
			&mut self.shaper,
			&mut self.outlines,
			&self.frame,
			&self.surface,
			&mut surface,
		);

		self.context.flush();
		self.context.render(target.as_mut(), &mut self.resources);
	}
}

/// The `vello_cpu` backend.
///
/// A borrow of the render context and nothing else. Everything it needs to
/// decide is decided before it is called; its whole job is spelling
/// [`Painter`]'s vocabulary in `vello_cpu`'s.
struct VelloPainter<'a> {
	context: &'a mut RenderContext,
}

impl VelloPainter<'_> {
	fn set(&mut self, color: Color) {
		self.context.set_paint(AlphaColor::<Srgb>::from_rgba8(
			color.red(),
			color.green(),
			color.blue(),
			color.alpha(),
		));
	}

	fn rect(rect: PixelRect) -> Rect {
		Rect::new(
			rect.x as f64,
			rect.y as f64,
			rect.right() as f64,
			rect.bottom() as f64,
		)
	}
}

impl Painter for VelloPainter<'_> {
	fn fill(&mut self, rect: PixelRect, color: Color) {
		if rect.is_empty() || color.is_transparent() {
			return;
		}
		self.set(color);
		self.context.fill_rect(&Self::rect(rect));
	}

	fn rounded_fill(&mut self, rect: PixelRect, radius: f32, color: Color) {
		if rect.is_empty() || color.is_transparent() {
			return;
		}
		self.set(color);
		let rounded = RoundedRect::from_rect(Self::rect(rect), radius as f64);
		self.context.fill_path(&rounded.to_path(PATH_TOLERANCE));
	}

	fn path(&mut self, path: &BezPath, color: Color) {
		if color.is_transparent() {
			return;
		}
		self.set(color);
		self.context.fill_path(path);
	}

	fn glyphs(&mut self, run: GlyphRun<'_>, origin: Point, context: &Text<'_>) {
		if run.color.is_transparent() {
			return;
		}
		self.set(run.color);

		for glyph in run.glyphs {
			let key = GlyphKey {
				font: glyph.font.0,
				glyph: glyph.identifier,
			};
			// The outline is borrowed straight out of the cache. The previous
			// code cloned it here — a heap allocation per glyph per frame,
			// purely to escape a borrow that the two-phase `ensure`/`get`
			// split removes.
			let Some(outline) = context.outlines.get(key) else {
				continue;
			};

			self.context.set_transform(
				Affine::translate((
					(origin.x + glyph.horizontal_position) as f64,
					(origin.y + glyph.vertical_position) as f64,
				)) * Affine::scale(glyph.scale as f64),
			);
			self.context.fill_path(outline);
		}

		self.context.reset_transform();
	}

	fn image(&mut self, _placement: &Placement, _clip: PixelRect) {
		// Graphics compositing is a separate workstream; the call site exists
		// so the paint order is already correct when it lands.
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use rio_vt::ansi::CursorShape;
	use rio_vt::config::colors::NamedColor;
	use rio_vt::crosswords::square::Square;

	fn renderer(columns: u16, rows: u16) -> Renderer {
		bare_renderer(
			columns,
			rows,
			Surface {
				margin: 0,
				padding: 0,
				border_radius: 0,
				margin_fill: None,
			},
		)
	}

	fn bare_renderer(columns: u16, rows: u16, surface: Surface) -> Renderer {
		let fonts = Fonts::resolve(Some("Liberation Mono"), 16.0, 1.0).unwrap();
		Renderer::new(
			fonts,
			Palette::default(),
			GridOptions::default(),
			surface,
			columns,
			rows,
			Level::new(),
		)
		.unwrap()
	}

	fn draw(renderer: &mut Renderer, snapshot: &Snapshot) -> Pixmap {
		let size = renderer.size();
		let mut pixmap = Pixmap::new(size.width, size.height);
		renderer.render(snapshot, &mut pixmap);
		pixmap
	}

	/// Read a pixel back as straight (non-premultiplied) RGBA.
	fn pixel(pixmap: &Pixmap, x: u16, y: u16) -> [u8; 4] {
		let width = pixmap.width() as usize;
		let color = pixmap.data()[y as usize * width + x as usize].to_u8_array();
		if color[3] == 0 || color[3] == 255 {
			return color;
		}
		let straight = |channel: u8| ((channel as u32 * 255) / color[3] as u32).min(255) as u8;
		[
			straight(color[0]),
			straight(color[1]),
			straight(color[2]),
			color[3],
		]
	}

	fn background() -> [u8; 4] {
		Palette::default().named(NamedColor::Background).channels()
	}

	/// How many pixels differ from the panel background. A crude measure of
	/// "how much was drawn", which is all that is needed to tell one glyph
	/// from a glyph plus its accent.
	fn ink_count(pixmap: &Pixmap) -> usize {
		let background = background();
		(0..pixmap.height())
			.flat_map(|y| (0..pixmap.width()).map(move |x| (x, y)))
			.filter(|(x, y)| pixel(pixmap, *x, *y) != background)
			.count()
	}

	fn cell_has_ink(pixmap: &Pixmap, metrics: Metrics, column: u16, row: u16) -> bool {
		let background = background();
		let x0 = column as u32 * metrics.cell_width;
		let y0 = row as u32 * metrics.cell_height;
		(y0..y0 + metrics.cell_height).any(|y| {
			(x0..x0 + metrics.cell_width).any(|x| pixel(pixmap, x as u16, y as u16) != background)
		})
	}

	fn blank(columns: u16, rows: u16) -> Snapshot {
		let mut snapshot = Snapshot::new(columns, rows);
		snapshot.cells.fill(Square::from_char(' '));
		snapshot
	}

	#[test]
	fn the_canvas_matches_the_grid_and_chrome() {
		let renderer = renderer(10, 4);
		let metrics = renderer.metrics();
		assert_eq!(
			renderer.size(),
			Size::new(
				(10 * metrics.cell_width) as u16,
				(4 * metrics.cell_height) as u16
			)
		);
	}

	#[test]
	fn padding_and_margin_grow_the_canvas() {
		let renderer = bare_renderer(
			10,
			4,
			Surface {
				margin: 5,
				padding: 7,
				border_radius: 0,
				margin_fill: None,
			},
		);
		let metrics = renderer.metrics();
		let chrome = 2 * (5 + 7);

		assert_eq!(
			renderer.size(),
			Size::new(
				(10 * metrics.cell_width + chrome) as u16,
				(4 * metrics.cell_height + chrome) as u16
			)
		);
	}

	#[test]
	fn an_empty_screen_renders_as_the_background_colour() {
		let mut renderer = renderer(8, 2);
		let pixmap = draw(&mut renderer, &blank(8, 2));
		let size = renderer.size();

		assert_eq!(pixel(&pixmap, 0, 0), background());
		assert_eq!(
			pixel(&pixmap, size.width - 1, size.height - 1),
			background()
		);
	}

	/// Glyphs must reach the surface and land in the column they were written
	/// to rather than being packed to the left.
	#[test]
	fn a_glyph_lands_in_its_own_column() {
		let mut renderer = renderer(8, 2);
		let metrics = renderer.metrics();

		let mut snapshot = blank(8, 2);
		snapshot.cells[3] = Square::from_char('W');
		let pixmap = draw(&mut renderer, &snapshot);

		assert!(cell_has_ink(&pixmap, metrics, 3, 0), "column 3 carries ink");
		assert!(!cell_has_ink(&pixmap, metrics, 0, 0), "column 0 is blank");
		assert!(!cell_has_ink(&pixmap, metrics, 6, 0), "column 6 is blank");
	}

	#[test]
	fn a_block_cursor_paints_its_cell() {
		let mut renderer = renderer(8, 2);
		let metrics = renderer.metrics();

		let mut snapshot = blank(8, 2);
		snapshot.cursor_visible = true;
		snapshot.cursor.content = CursorShape::Block;
		let pixmap = draw(&mut renderer, &snapshot);

		assert_eq!(
			pixel(
				&pixmap,
				(metrics.cell_width / 2) as u16,
				(metrics.cell_height / 2) as u16
			),
			Palette::default().named(NamedColor::Cursor).channels(),
			"the cursor cell should be filled with the cursor colour"
		);
	}

	/// RTL: Arabic must reverse within the row while still occupying the same
	/// columns the terminal assigned.
	#[test]
	fn arabic_occupies_the_columns_it_was_written_to() {
		let mut renderer = renderer(10, 1);
		let metrics = renderer.metrics();

		let mut snapshot = blank(10, 1);
		for (index, character) in "سلام".chars().enumerate() {
			snapshot.cells[index] = Square::from_char(character);
		}
		let pixmap = draw(&mut renderer, &snapshot);

		let inked = (0..4)
			.filter(|column| cell_has_ink(&pixmap, metrics, *column, 0))
			.count();
		assert!(inked >= 2, "the Arabic run should ink its columns, got {inked}");
		assert!(
			!cell_has_ink(&pixmap, metrics, 8, 0),
			"columns past the run must stay empty"
		);
	}

	/// The point of bundling a Nerd Font: no system face declares coverage
	/// for the Private Use Area, so without the symbol face in the fallback
	/// chain every one of these cells would be blank or tofu.
	#[test]
	fn nerd_font_glyphs_reach_the_surface() {
		for (name, character) in [
			("powerline separator", '\u{e0b0}'),
			("git branch", '\u{e0a0}'),
			("devicon", '\u{e7a8}'),
			("font awesome folder", '\u{f07b}'),
			("material design", '\u{f0001}'),
		] {
			let mut renderer = renderer(4, 1);
			let metrics = renderer.metrics();

			let mut snapshot = blank(4, 1);
			snapshot.cells[1] = Square::from_char(character);
			let pixmap = draw(&mut renderer, &snapshot);

			assert!(
				cell_has_ink(&pixmap, metrics, 1, 0),
				"{name} (U+{:04X}) should draw something",
				character as u32
			);
			// An icon may overhang its cell by a pixel or two, the way an
			// italic serif does — real terminals show the same overhang. What
			// must not happen is the glyph advancing the grid.
			assert!(
				!cell_has_ink(&pixmap, metrics, 3, 0),
				"{name} (U+{:04X}) is drawing two columns from where it was written",
				character as u32
			);
		}
	}

	/// Box drawing is now drawn rather than shaped. A TUI is mostly these,
	/// and the reason they are drawn is that adjacent cells have to join.
	#[test]
	fn box_drawing_and_block_elements_render() {
		let mut renderer = renderer(8, 1);
		let metrics = renderer.metrics();

		let mut snapshot = blank(8, 1);
		for (column, character) in "─│┌┐└┘├█".chars().enumerate() {
			snapshot.cells[column] = Square::from_char(character);
		}
		let pixmap = draw(&mut renderer, &snapshot);

		for column in 0..8u16 {
			assert!(
				cell_has_ink(&pixmap, metrics, column, 0),
				"column {column} of the box drawing run should carry ink"
			);
		}
	}

	/// The property the sprite pass exists for, measured end to end: a run of
	/// horizontal rules must be one unbroken stroke across the whole row,
	/// with no gap at any cell boundary.
	#[test]
	fn a_run_of_box_drawing_has_no_seam_at_any_cell_boundary() {
		let mut renderer = renderer(8, 1);
		let metrics = renderer.metrics();

		let mut snapshot = blank(8, 1);
		for column in 0..8 {
			snapshot.cells[column] = Square::from_char('─');
		}
		let pixmap = draw(&mut renderer, &snapshot);

		// Find the scan line the rule landed on, then walk it end to end.
		let background = background();
		let row = (0..metrics.cell_height)
			.find(|y| pixel(&pixmap, 0, *y as u16) != background)
			.expect("the rule must be somewhere in the cell");

		let gaps = (0..8 * metrics.cell_width)
			.filter(|x| pixel(&pixmap, *x as u16, row as u16) == background)
			.count();

		assert_eq!(
			gaps, 0,
			"a horizontal rule must be continuous across all eight cells"
		);
	}

	/// A combining mark lives in the cell's `extras`, so it only reaches the
	/// shaper if the row builder goes looking for it — and it must not cost a
	/// column.
	#[test]
	fn a_combining_mark_is_drawn_over_its_base() {
		use rio_vt::crosswords::square::Extras;

		let base = {
			let mut renderer = renderer(4, 1);
			let mut snapshot = blank(4, 1);
			snapshot.cells[1] = Square::from_char('e');
			ink_count(&draw(&mut renderer, &snapshot))
		};

		let mut renderer = renderer(4, 1);
		let metrics = renderer.metrics();

		let mut snapshot = blank(4, 1);
		let mut cell = Square::from_char('e');
		cell.set_extras_id(Some(1));
		snapshot.cells[1] = cell;
		snapshot.extras.insert(
			1,
			Extras {
				zerowidth: vec!['\u{0301}'], // COMBINING ACUTE ACCENT
				hyperlink: None,
			},
		);
		let pixmap = draw(&mut renderer, &snapshot);

		assert!(
			ink_count(&pixmap) > base,
			"the accent should add ink on top of the bare letter"
		);
		assert!(
			!cell_has_ink(&pixmap, metrics, 2, 0),
			"a zero-width mark must not take a column of its own"
		);
	}

	#[test]
	fn a_background_colour_fills_its_cell() {
		use rio_vt::config::colors::{AnsiColor, ColorRgb};
		use rio_vt::crosswords::style::Style;

		let mut renderer = renderer(4, 1);
		let metrics = renderer.metrics();

		let mut snapshot = blank(4, 1);
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

		let pixmap = draw(&mut renderer, &snapshot);
		let middle_x = (metrics.cell_width + metrics.cell_width / 2) as u16;
		let middle_y = (metrics.cell_height / 2) as u16;

		assert_eq!(pixel(&pixmap, middle_x, middle_y), [255, 0, 0, 255]);
	}

	/// `bold_is_bright` is only reachable if the options actually travel from
	/// construction through to cell resolution. Before this, the renderer
	/// hardcoded `GridOptions::default()` in four places and the setting could
	/// not be observed at all.
	#[test]
	fn the_grid_options_a_renderer_was_built_with_reach_cell_resolution() {
		use rio_vt::config::colors::{AnsiColor, NamedColor};
		use rio_vt::crosswords::style::{Style, StyleFlags};

		let mut snapshot = blank(4, 1);
		snapshot.styles = vec![Style {
			fg: AnsiColor::Named(NamedColor::Red),
			flags: StyleFlags::BOLD,
			..Style::default()
		}];
		snapshot.cells[0].set_style_id(0);

		let palette = Palette::default();
		let surface = Surface {
			margin: 0,
			padding: 0,
			border_radius: 0,
			margin_fill: None,
		};
		let build = |options| {
			let fonts = Fonts::resolve(Some("Liberation Mono"), 16.0, 1.0).unwrap();
			Renderer::new(
				fonts,
				palette.clone(),
				options,
				surface,
				4,
				1,
				Level::new(),
			)
			.unwrap()
		};

		let mut bright = build(GridOptions {
			bold_is_bright: true,
		});
		draw(&mut bright, &snapshot);
		assert_eq!(
			bright.grid.cell(crate::geom::Cell::new(0, 0)).foreground,
			palette.named(NamedColor::LightRed),
			"bold red must land on bright red when the option is on"
		);

		let mut plain = build(GridOptions::default());
		draw(&mut plain, &snapshot);
		assert_eq!(
			plain.grid.cell(crate::geom::Cell::new(0, 0)).foreground,
			palette.named(NamedColor::Red),
			"and on plain red when it is off"
		);
	}
}
