//! An animated SVG, drawn from the model and never from pixels.
//!
//! This sink shares the paint pass with the rasterizer: [`crate::paint::paint`]
//! decides what a frame is, and [`SvgPainter`] spells that decision in markup.
//! Everything the two backends have in common — background runs, cursor
//! geometry and width, drawn box characters, underline styles, glyph runs and
//! their colours — is decided once, upstream of both.
//!
//! What is *not* shared, and cannot be, is what a run of glyphs becomes. The
//! rasterizer fills outlines; this file would rather emit real `<text>`, so the
//! recording is selectable, copy-pasteable and a fraction of the size. That
//! choice lives in [`SvgPainter::glyphs`], and it is the only place this
//! backend decides anything about a frame's appearance on its own.
//!
//! ## The timeline
//!
//! Frames are not written out one after another. Every frame reduces to a set
//! of [`VisualArtifact`]s, and a artifact that appears in consecutive frames
//! is one element whose lifetime is extended rather than one element per
//! frame. A prompt that sits unchanged for four seconds is a handful of
//! elements with a long duration, not two hundred copies. SMIL `<set>`
//! elements turn each lifetime on and off.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use vello_cpu::kurbo::BezPath;
use xmlwriter::{Options, XmlWriter};

use super::font_embed;
use crate::fonts::Fonts;
use crate::geom::{Cell, Color, Frame, PixelRect, Point};
use crate::grid::{Damage, Grid, GridOptions};
use crate::model::Placement;
use crate::model::Palette;
use crate::outline::{GlyphKey, Outlines};
use crate::paint::{self, Painter, Text};
use crate::render::Surface;
use crate::shape::{Cmap, FontKey, GlyphRun, PlacedGlyph};
use crate::sprite;
use crate::stream::{Context as StreamContext, Metadata, Sink};

const NAMESPACE_SVG: &str = "http://www.w3.org/2000/svg";
const TRANSPARENT: &str = "transparent";

/// `Glyph::scale` is stored as an integer so the artifact can be hashed and
/// compared for equality; this is the fixed-point precision it is scaled by.
const SCALE_FIXED_POINT: f32 = 1000.0;

/// A distinct visual element on the screen.
///
/// `Hash` and `Eq` are what make the timeline work: two frames that produce an
/// equal artifact produce *the same* element, so its lifetime extends instead
/// of a second one being written.
#[derive(Clone, PartialEq, Eq, Hash)]
enum VisualArtifact {
	/// Any solid rectangle: a cell background, the panel, the cursor, an
	/// underline, a drawn box character. One variant rather than several
	/// because they are the same element with the same paint order, and
	/// splitting them would only mean four ways to write a `<rect>`.
	Rectangle {
		x: i32,
		y: i32,
		width: u32,
		height: u32,
		radius: u32,
		color: Color,
	},
	/// A filled path — a rounded box corner, a diagonal, an undercurl.
	Path { data: String, color: Color },
	/// A real `<text>` run.
	///
	/// `visible` distinguishes two very different roles the same shape serves:
	///
	/// - `visible: true` is the common case: real ink *and* real,
	///   copy-pasteable content in one element. `font_id: Some(id)` embeds and
	///   forces that specific face (see `encode/font_embed.rs`); `font_id:
	///   None` means an RTL run, which deliberately renders with no forced
	///   family at all, trusting the browser's own bidi engine and the host's
	///   system Arabic fallback rather than reproducing joining ourselves (a
	///   subsetted face has no `GSUB`).
	/// - `visible: false` carries text for selection only, for characters
	///   whose ink comes from somewhere else: a glyph with no plain cmap entry
	///   (paired with a `Glyph` artifact), or a box-drawing character the
	///   sprite pass already painted.
	TextRun {
		x: i32,
		y: i32,
		width: u32,
		text: String,
		color: Color,
		font_id: Option<u64>,
		right_to_left: bool,
		visible: bool,
	},
	/// Ink for the narrow case `TextRun` cannot cover: a glyph addressed by
	/// id, sidestepping `cmap` and `GSUB` entirely, at the cost of needing its
	/// outline baked into `<defs>`.
	Glyph {
		font_id: u64,
		glyph_id: u32,
		x: i32,
		y: i32,
		color: Color,
		scale: u32,
	},
}

impl VisualArtifact {
	/// Back-to-front paint order, then top-to-bottom, left-to-right position.
	///
	/// The layer comes first: rectangles and paths sit under text. But the
	/// layer alone is not enough — the artifact set is a `HashMap`, so without
	/// a position tiebreaker its iteration order (arbitrary, and different
	/// across rows) becomes the `<text>` elements' DOM order. A browser's
	/// copy and selection follow DOM order, not screen position, so that
	/// arbitrary order is exactly what made a multi-row selection come out
	/// scrambled: row 5 could easily land in the document before row 0.
	/// Sorting by (row, column) makes the DOM order match reading order.
	fn paint_order(&self) -> (u8, i32, i32) {
		match self {
			Self::Rectangle { x, y, .. } => (0, *y, *x),
			Self::Path { .. } => (0, i32::MAX, 0),
			Self::TextRun { x, y, .. } => (1, *y, *x),
			Self::Glyph { x, y, .. } => (1, *y, *x),
		}
	}
}

/// How long an artifact stays on screen.
#[derive(Clone, Copy, Debug)]
struct Lifespan {
	begin_milliseconds: u32,
	duration_milliseconds: u32,
}

pub struct Svg {
	path: PathBuf,
	surface: Surface,
	palette: Palette,
	options: GridOptions,
	family: String,
	font_size: f32,

	// The shared engine, exactly as the rasterizer holds it.
	shaper: crate::shape::Shaper,
	outlines: Outlines,
	grid: Grid,
	cmap: Cmap,
	frame: Frame,

	// The timeline.
	active: HashMap<VisualArtifact, Lifespan>,
	completed: Vec<(VisualArtifact, Lifespan)>,
	now_milliseconds: u32,
	frames_per_second: u32,

	/// Every character drawn as real embedded-font text over the whole
	/// recording, keyed by which face drew it, mapped to the glyph shaping
	/// chose. Accumulated frame by frame so `finish` can subset and embed each
	/// face exactly once, for exactly the glyphs the recording ended up using.
	used_by_font: HashMap<u64, BTreeMap<char, u32>>,
}

impl Svg {
	pub fn new(
		path: PathBuf,
		fonts: Fonts,
		palette: Palette,
		options: GridOptions,
		surface: Surface,
		columns: u16,
		rows: u16,
	) -> Self {
		let family = fonts.family.clone();
		let font_size = fonts.size;
		let frame = surface.frame_for(fonts.metrics, columns, rows);

		Self {
			path,
			surface,
			palette,
			options,
			family,
			font_size,
			shaper: crate::shape::Shaper::new(fonts),
			outlines: Outlines::new(),
			grid: Grid::new(columns, rows),
			cmap: Cmap::new(),
			frame,
			active: HashMap::new(),
			completed: Vec::new(),
			now_milliseconds: 0,
			frames_per_second: 1,
			used_by_font: HashMap::new(),
		}
	}

	/// Extend the lifetime of everything still on screen, retire what is not,
	/// and start the clock for whatever is new.
	fn advance(&mut self, current: HashSet<VisualArtifact>, hold_milliseconds: u32) {
		let mut next = HashMap::with_capacity(current.len());

		for artifact in current {
			match self.active.remove(&artifact) {
				Some(mut lifespan) => {
					lifespan.duration_milliseconds += hold_milliseconds;
					next.insert(artifact, lifespan);
				}
				None => {
					next.insert(
						artifact,
						Lifespan {
							begin_milliseconds: self.now_milliseconds,
							duration_milliseconds: hold_milliseconds,
						},
					);
				}
			}
		}

		// Whatever is left was on screen last frame and is not on screen now.
		self.completed.extend(self.active.drain());
		self.active = next;
		self.now_milliseconds += hold_milliseconds;
	}

	fn write_document(&self) -> String {
		// `xmlwriter`'s own default pretty-printing indents every element —
		// including `<text>`, whose content is not decoration but the exact,
		// cell-for-cell string a `textLength` run's width was computed for.
		// Indenting would inject a newline plus leading spaces into that
		// string, so it is disabled outright rather than special-cased away.
		let mut xml = XmlWriter::new(Options {
			indent: xmlwriter::Indent::None,
			attributes_indent: xmlwriter::Indent::None,
			..Options::default()
		});

		xml.start_element("svg");
		xml.write_attribute("xmlns", NAMESPACE_SVG);
		xml.write_attribute("width", &self.frame.canvas.width);
		xml.write_attribute("height", &self.frame.canvas.height);
		xml.write_attribute(
			"viewBox",
			&format!(
				"0 0 {} {}",
				self.frame.canvas.width, self.frame.canvas.height
			),
		);
		xml.write_attribute(
			"font-family",
			&format!("{}, ui-monospace, monospace", self.family),
		);
		xml.write_attribute("font-size", &format!("{}px", self.font_size));
		// A `<text>` run's content is padded with real space characters out to
		// its declared `textLength` so every character — including blank cells
		// — lands on its own grid column. SVG's default whitespace handling
		// collapses runs of whitespace down to a single space before
		// `textLength` ever sees it, which then force-stretches whatever
		// collapsed-down handful of characters remain across the whole
		// declared width — grotesquely oversized glyphs. `preserve` keeps
		// every character significant, matching the one-character-per-cell
		// model the rest of this file assumes.
		xml.write_attribute("xml:space", "preserve");

		self.write_style(&mut xml);
		self.write_definitions(&mut xml);

		// Sorted by paint order rather than left in hash-map order, so a
		// cursor rectangle always sits underneath the glyph it covers instead
		// of wherever the hash happened to place it. `retired` distinguishes
		// the two sources so `write_artifact` knows whether this exact visual
		// state ever needs to disappear again.
		let mut all: Vec<_> = self
			.completed
			.iter()
			.map(|(artifact, lifespan)| (artifact, lifespan, true))
			.chain(
				self.active
					.iter()
					.map(|(artifact, lifespan)| (artifact, lifespan, false)),
			)
			.collect();
		all.sort_by_key(|(artifact, _, _)| artifact.paint_order());

		for (artifact, lifespan, retired) in all {
			write_artifact(&mut xml, artifact, lifespan, retired);
		}

		xml.end_element();
		xml.end_document()
	}

	/// Embed every face this recording drew as real `<text>`, each subset down
	/// to exactly the glyphs it used and named `f{font_id}` so a run's
	/// `font-family` finds it.
	///
	/// A face that fails to subset is skipped with a warning rather than
	/// failing the whole recording — its runs still rendered visibly and are
	/// still real, selectable text; they just fall through to the document's
	/// ambient family instead of the exact face shaping used.
	fn write_style(&self, xml: &mut XmlWriter) {
		// The CSS half of the whitespace contract the `xml:space="preserve"`
		// attribute states. Both are needed because they are read by disjoint
		// sets of consumers: `xml:space` is the SVG 1.1 spelling, which the
		// static rasterisers (resvg, librsvg) act on; `white-space` is the
		// SVG 2 / CSS Text spelling, which browsers act on.
		//
		// WebKit is what makes this load-bearing rather than belt-and-braces:
		// it resolves SVG text whitespace purely through the CSS property and
		// ignores `xml:space` outright, so with the attribute alone every
		// run's padding collapsed — and because each run also carries a
		// `textLength` measured for the *uncollapsed* string,
		// `lengthAdjust="spacingAndGlyphs"` then stretched whatever few glyphs
		// survived across the full declared width. A mostly-blank run holding
		// one `│` came out as a solid bar a dozen cells wide.
		let mut rules = String::from("text{white-space:pre}");

		for (font_id, codepoints) in &self.used_by_font {
			let Some(face) = self.shaper.faces().get(FontKey(*font_id)) else {
				continue;
			};
			match font_embed::build_font_face(
				&format!("f{font_id}"),
				face.data.as_ref(),
				face.index,
				codepoints,
			) {
				Ok(rule) => rules.push_str(&rule),
				Err(error) => eprintln!(
					"dvd: warning: could not embed font f{font_id} in {}: {error:#}",
					self.path.display()
				),
			}
		}

		xml.start_element("style");
		// The generated CSS is base64 plus punctuation from a fixed template —
		// never `<` or `&` — so it needs none of the escaping the text-bearing
		// artifacts do.
		xml.write_text(&rules);
		xml.end_element();
	}

	fn write_definitions(&self, xml: &mut XmlWriter) {
		xml.start_element("defs");
		for (key, path) in self.outlines.cache.iter() {
			xml.start_element("path");
			xml.write_attribute("id", &format!("g{}-{}", key.font, key.glyph));
			xml.write_attribute("d", &path.to_svg());
			xml.end_element();
		}
		xml.end_element();
	}
}

impl Sink for Svg {
	fn requires_pixels(&self) -> bool {
		false
	}

	fn begin(&mut self, meta: &Metadata) -> Result<()> {
		self.frames_per_second = meta.frames_per_second.max(1) as u32;
		Ok(())
	}

	fn accept(&mut self, context: StreamContext<'_>) -> Result<()> {
		self.grid
			.fill(&context.frame.snapshot, &self.palette, &self.options);

		// Every frame is reduced in full. The timeline, not the paint pass, is
		// what makes an unchanged screen cheap: an artifact that comes out
		// equal to last frame's extends its lifetime instead of being written
		// again.
		let damage = Damage::everything(self.grid.columns, self.grid.rows);
		let mut artifacts = HashSet::new();

		let mut painter = SvgPainter {
			artifacts: &mut artifacts,
			cmap: &mut self.cmap,
			used_by_font: &mut self.used_by_font,
			grid: &self.grid,
			frame: &self.frame,
		};

		paint::paint(
			&self.grid,
			&damage,
			&mut self.shaper,
			&mut self.outlines,
			&self.frame,
			&self.surface,
			&mut painter,
		);

		// Rounded rather than truncated, and computed from the tick count in
		// one step: `ticks * (1000 / fps)` loses up to a millisecond per frame
		// to integer division at every frame rate that does not divide 1000,
		// and a long recording accumulates that into visible drift.
		let hold =
			(context.frame.hold_ticks * 1000).div_ceil(self.frames_per_second);
		self.advance(artifacts, hold);

		Ok(())
	}

	fn finish(self: Box<Self>) -> Result<()> {
		anyhow::ensure!(
			self.now_milliseconds > 0,
			"No frames were captured, so {} has nothing to show",
			self.path.display()
		);

		if let Some(parent) = self.path.parent()
			&& !parent.as_os_str().is_empty()
		{
			std::fs::create_dir_all(parent)
				.with_context(|| format!("Creating directory {}", parent.display()))?;
		}

		let document = self.write_document();
		let mut file = BufWriter::new(
			File::create(&self.path)
				.with_context(|| format!("Creating file {}", self.path.display()))?,
		);

		file.write_all(document.as_bytes())
			.with_context(|| format!("Writing to {}", self.path.display()))?;
		file.flush()
			.with_context(|| format!("Flushing buffer to {}", self.path.display()))
	}
}

/// What a contiguous stretch of one glyph run's characters becomes.
#[derive(Clone, Copy, PartialEq)]
enum Subrun {
	/// Real text, forcing the embedded face — the character's plain cmap
	/// lookup against that face matches what shaping produced.
	Embedded(u64),
	/// Real text with no forced face — always an RTL run.
	Ambient,
	/// Ink comes from an outline `<path>`; the text is carried invisibly for
	/// selection. A ligature, or anything else whose shaped glyph a bare cmap
	/// lookup would not reproduce.
	Outlined,
	/// Ink comes from the sprite pass; the text is carried invisibly for
	/// selection. Box drawing, blocks and braille.
	Drawn,
}

/// The SVG backend for the shared paint pass.
///
/// Borrows the pieces of [`Svg`] it needs rather than owning them, so the
/// paint pass can hold the grid and shaper at the same time.
struct SvgPainter<'a> {
	artifacts: &'a mut HashSet<VisualArtifact>,
	cmap: &'a mut Cmap,
	used_by_font: &'a mut HashMap<u64, BTreeMap<char, u32>>,
	grid: &'a Grid,
	frame: &'a Frame,
}

impl SvgPainter<'_> {
	fn rectangle(&mut self, rect: PixelRect, radius: f32, color: Color) {
		if rect.is_empty() || color.is_transparent() {
			return;
		}
		self.artifacts.insert(VisualArtifact::Rectangle {
			x: rect.x.round() as i32,
			y: rect.y.round() as i32,
			width: rect.width.round().max(1.0) as u32,
			height: rect.height.round().max(1.0) as u32,
			radius: radius.round() as u32,
			color,
		});
	}

	/// How many columns the character starting at `byte` occupies.
	fn columns_at(&self, run: &GlyphRun<'_>, byte: usize) -> u16 {
		run.columns
			.get(byte)
			.and_then(|column| {
				self.grid
					.row(run.span.row)
					.get(*column as usize)
					.map(|cell| {
						if cell.wide == rio_vt::crosswords::square::Wide::Wide {
							2
						} else {
							1
						}
					})
			})
			.unwrap_or(1)
	}

	/// Emit one finished stretch of a run as a `<text>` element.
	///
	/// The declared `textLength` is the width of the *columns* the stretch
	/// covers, not of its character count — a double-width character is one
	/// character in two cells, and measuring by character would squeeze every
	/// CJK run to half its width.
	#[allow(clippy::too_many_arguments)]
	fn emit_text(
		&mut self,
		run: &GlyphRun<'_>,
		start_byte: usize,
		end_byte: usize,
		kind: Subrun,
		color: Color,
		baseline: f32,
	) {
		if start_byte >= end_byte {
			return;
		}

		let Some(&first_column) = run.columns.get(start_byte) else {
			return;
		};
		let text = &run.text[start_byte..end_byte];

		// Walk the stretch to total its columns, so a mix of narrow and wide
		// characters comes out at the width the grid gave it.
		let mut columns = 0u16;
		for (offset, _) in text.char_indices() {
			columns += self.columns_at(run, start_byte + offset);
		}

		let (font_id, visible) = match kind {
			Subrun::Embedded(font_id) => (Some(font_id), true),
			Subrun::Ambient => (None, true),
			Subrun::Outlined | Subrun::Drawn => (None, false),
		};

		let origin = self.frame.cell_origin(Cell::new(first_column, run.span.row));
		self.artifacts.insert(VisualArtifact::TextRun {
			x: origin.x.round() as i32,
			y: baseline.round() as i32,
			width: columns as u32 * self.frame.metrics.cell_width,
			text: text.to_string(),
			color,
			font_id,
			right_to_left: run.right_to_left,
			visible,
		});
	}

	/// Draw one glyph as an outline `<use>`, for the narrow case a character
	/// is not representable as `<text>`.
	fn emit_outline(&mut self, glyph: &PlacedGlyph, origin: Point, color: Color, outlines: &Outlines) {
		let key = GlyphKey {
			font: glyph.font.0,
			glyph: glyph.identifier,
		};
		// No outline means the face genuinely has nothing for this glyph — a
		// control character mapped to `.notdef`. There is nothing to draw.
		if outlines.get(key).is_none() {
			return;
		}

		self.artifacts.insert(VisualArtifact::Glyph {
			font_id: key.font,
			glyph_id: key.glyph,
			x: (origin.x + glyph.horizontal_position).round() as i32,
			y: (origin.y + glyph.vertical_position).round() as i32,
			color,
			scale: (glyph.scale * SCALE_FIXED_POINT).round() as u32,
		});
	}
}

impl Painter for SvgPainter<'_> {
	fn fill(&mut self, rect: PixelRect, color: Color) {
		self.rectangle(rect, 0.0, color);
	}

	fn rounded_fill(&mut self, rect: PixelRect, radius: f32, color: Color) {
		self.rectangle(rect, radius, color);
	}

	fn path(&mut self, path: &BezPath, color: Color) {
		if color.is_transparent() {
			return;
		}
		self.artifacts.insert(VisualArtifact::Path {
			data: path.to_svg(),
			color,
		});
	}

	/// Turn a run of glyphs into markup.
	///
	/// The default is a real, visible `<text>` element: it is ink and
	/// selectable content in one, it stays crisp at any zoom, and it is a
	/// fraction of the size of the equivalent outlines. A character only falls
	/// back to something else when `<text>` genuinely cannot reproduce it:
	///
	/// - the sprite pass already drew it (box drawing, blocks, braille), so
	///   the text is carried invisibly and the ink is the sprite's;
	/// - shaping landed on a different glyph than the face's own plain cmap
	///   lookup would (a ligature, a contextual substitution), so a browser —
	///   which only gets to consult `cmap` — would draw something else.
	///
	/// The cmap check is skipped entirely for RTL runs by design: they render
	/// as real text with no forced embedded face, trusting the browser's own
	/// bidi engine and the host's system Arabic fallback (which has real
	/// `GSUB`) rather than second-guessing joining ourselves, since a
	/// subsetted face carries no `GSUB` at all.
	fn glyphs(&mut self, run: GlyphRun<'_>, origin: Point, context: &Text<'_>) {
		let baseline = origin.y + self.frame.metrics.baseline;

		// Which glyphs landed on each column. Usually one, but a cluster that
		// decomposed into several leaves them all under its start column.
		let mut by_column: HashMap<u16, Vec<&PlacedGlyph>> = HashMap::new();
		for glyph in run.glyphs {
			by_column.entry(glyph.column).or_default().push(glyph);
		}

		let mut start = run.range.start;
		let mut active: Option<Subrun> = None;

		for (offset, character) in run.text[run.range.clone()].char_indices() {
			let byte = run.range.start + offset;
			let Some(&column) = run.columns.get(byte) else {
				continue;
			};

			let decided = if sprite::covers(character) {
				Some(Subrun::Drawn)
			} else if run.right_to_left {
				Some(Subrun::Ambient)
			} else if let Some(glyphs) = by_column.get(&column) {
				match glyphs.as_slice() {
					[only]
						if self.cmap.glyph(context.faces, only.font, character)
							== Some(only.identifier) =>
					{
						self.used_by_font
							.entry(only.font.0)
							.or_default()
							.insert(character, only.identifier);
						Some(Subrun::Embedded(only.font.0))
					}
					glyphs => {
						for glyph in glyphs {
							self.emit_outline(glyph, origin, run.color, context.outlines);
						}
						Some(Subrun::Outlined)
					}
				}
			} else {
				// No glyph landed on this column at all: a character a
				// ligature to its left swallowed. Nothing here constrains the
				// surrounding run's face, so extend whatever is already
				// active rather than forcing a split.
				None
			};

			match (active, decided) {
				(Some(current), Some(next)) if current != next => {
					self.emit_text(&run, start, byte, current, run.color, baseline);
					start = byte;
					active = Some(next);
				}
				(None, Some(next)) => active = Some(next),
				_ => {}
			}
		}

		if let Some(kind) = active {
			self.emit_text(&run, start, run.range.end, kind, run.color, baseline);
		}
	}

	fn image(&mut self, _placement: &Placement, _clip: PixelRect) {
		// Graphics compositing is a separate workstream; the call site exists
		// so the paint order is already correct when it lands.
	}
}

fn write_artifact(
	xml: &mut XmlWriter,
	artifact: &VisualArtifact,
	lifespan: &Lifespan,
	retired: bool,
) {
	// An artifact that appears after the recording starts is not on screen
	// until its `begin`, so it has to start hidden — the base SVG default for
	// a shape element is visible, and nothing else would suppress it.
	let starts_hidden = lifespan.begin_milliseconds > 0;
	let hide_at = lifespan.begin_milliseconds + lifespan.duration_milliseconds;

	// A `<text>` element's content must be written last: once `write_text`
	// runs, `xmlwriter` has left the attributes phase and any attribute
	// written afterwards — `display` included — panics.
	let mut content: Option<&str> = None;

	match artifact {
		VisualArtifact::Rectangle {
			x,
			y,
			width,
			height,
			radius,
			color,
		} => {
			xml.start_element("rect");
			xml.write_attribute("x", x);
			xml.write_attribute("y", y);
			xml.write_attribute("width", width);
			xml.write_attribute("height", height);
			if *radius > 0 {
				xml.write_attribute("rx", radius);
			}
			xml.write_attribute("fill", &color.to_string());
			write_opacity(xml, *color);
		}
		VisualArtifact::Path { data, color } => {
			xml.start_element("path");
			xml.write_attribute("d", data);
			xml.write_attribute("fill", &color.to_string());
			write_opacity(xml, *color);
		}
		VisualArtifact::TextRun {
			x,
			y,
			width,
			text,
			color,
			font_id,
			right_to_left,
			visible,
		} => {
			xml.start_element("text");
			xml.write_attribute("x", x);
			xml.write_attribute("y", y);
			if *visible {
				xml.write_attribute("fill", &color.to_string());
				// `None` is always an RTL run — see `TextRun::font_id`.
				if let Some(font_id) = font_id {
					xml.write_attribute("font-family", &format!("f{font_id}"));
				}
			} else {
				xml.write_attribute("fill", TRANSPARENT);
			}
			xml.write_attribute("textLength", width);
			xml.write_attribute("lengthAdjust", "spacingAndGlyphs");
			if *right_to_left {
				xml.write_attribute("direction", "rtl");
			}
			content = Some(text);
		}
		VisualArtifact::Glyph {
			font_id,
			glyph_id,
			x,
			y,
			color,
			scale,
		} => {
			let scale = *scale as f32 / SCALE_FIXED_POINT;
			xml.start_element("use");
			xml.write_attribute("href", &format!("#g{font_id}-{glyph_id}"));
			xml.write_attribute("transform", &format!("translate({x} {y}) scale({scale})"));
			xml.write_attribute("fill", &color.to_string());
			// Ink only — clicks must pass through to the invisible text run
			// underneath, or dragging over it would select nothing.
			xml.write_attribute("pointer-events", "none");
		}
	}

	if starts_hidden {
		xml.write_attribute("display", "none");
	}

	if let Some(text) = content {
		// `xmlwriter::write_text` only escapes `<`; a bare `&`, which shell
		// text is full of (`&&`, `a & b`), would otherwise land in the output
		// unescaped and make the whole document unparsable.
		xml.write_text(&text.replace('&', "&amp;"));
	}

	// Show at `begin`. Without a matching hide, a `<set>` that only ever turns
	// something on would leave it on forever once its own active duration
	// lapsed — SMIL reverts to the *underlying* value then, and the underlying
	// value here is "visible", not "hidden".
	if starts_hidden {
		write_set(xml, "inline", lifespan.begin_milliseconds);
	}

	// Hide again once this exact visual state was replaced. Only a retired
	// artifact needs this — one still active is on screen through the end of
	// the recording, so nothing ever turns it back off.
	if retired {
		write_set(xml, "none", hide_at);
	}

	xml.end_element();
}

/// Alpha as a separate attribute, because SVG spells opacity that way rather
/// than as a fourth channel. Only the shade blocks are ever not opaque.
fn write_opacity(xml: &mut XmlWriter, color: Color) {
	if color.alpha() != 0xff {
		xml.write_attribute(
			"fill-opacity",
			&format!("{:.3}", color.alpha() as f32 / 255.0),
		);
	}
}

fn write_set(xml: &mut XmlWriter, to: &str, at_milliseconds: u32) {
	xml.start_element("set");
	xml.write_attribute("attributeName", "display");
	xml.write_attribute("to", to);
	xml.write_attribute("begin", &format!("{at_milliseconds}ms"));
	xml.end_element();
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::model::Snapshot;
	use crate::stream::Frame as StreamFrame;
	use rio_vt::crosswords::square::Square;
	use std::sync::Arc;

	fn svg_for(columns: u16) -> Svg {
		let fonts = Fonts::resolve(Some("Liberation Mono"), 22.0, 1.0).unwrap();
		Svg::new(
			PathBuf::from("test.svg"),
			fonts,
			Palette::default(),
			GridOptions::default(),
			Surface::default(),
			columns,
			2,
		)
	}

	/// Burn one screen holding `line` (padded out to `columns` with blanks, so
	/// the runs carry the interior and trailing whitespace the whitespace
	/// contract exists for) and return the finished SVG document.
	fn document_for(line: &str, columns: u16) -> String {
		let mut svg = svg_for(columns);

		let mut snapshot = Snapshot::new(columns, 2);
		snapshot.cells.fill(Square::from_char(' '));
		for (index, character) in line.chars().take(columns as usize).enumerate() {
			snapshot.cells[index] = Square::from_char(character);
		}

		svg.begin(&Metadata {
			width: 800,
			height: 200,
			frames_per_second: 50,
		})
		.expect("begin");
		svg.accept(StreamContext {
			frame: &StreamFrame::new(Arc::new(snapshot), 1),
			pixels: None,
		})
		.expect("accept");

		svg.write_document()
	}

	fn cell_width() -> u32 {
		svg_for(4).frame.metrics.cell_width
	}

	/// Every `<text>` element in `document`, as `(attributes, character data)`
	/// with the SMIL `<set>` children and entity escapes taken back out.
	fn text_runs(document: &str) -> Vec<(String, String)> {
		let mut runs = Vec::new();
		let mut rest = document;

		while let Some(start) = rest.find("<text") {
			let after = &rest[start..];
			let attributes_end = after.find('>').expect("a <text> element must be closed");
			let end = after.find("</text>").expect("a <text> element must be ended");
			let mut body = after[attributes_end + 1..end].to_string();
			while let Some(set_start) = body.find("<set") {
				let set_end = body[set_start..].find("/>").expect("<set> is self-closing");
				body.replace_range(set_start..set_start + set_end + 2, "");
			}
			runs.push((after[..attributes_end].to_string(), body.replace("&amp;", "&")));
			rest = &after[end..];
		}

		runs
	}

	fn attribute(attributes: &str, name: &str) -> Option<String> {
		let key = format!("{name}=\"");
		let start = attributes.find(&key)? + key.len();
		let end = attributes[start..].find('"')? + start;
		Some(attributes[start..end].to_string())
	}

	/// The regression test for the WebKit whitespace collapse: the document
	/// has to state "keep every space" in *both* spellings, because the two
	/// are read by disjoint consumers.
	#[test]
	fn document_preserves_whitespace_in_both_spellings() {
		let document = document_for("Host   ->  value", 40);

		assert!(
			document.contains(r#"xml:space="preserve""#),
			"the SVG 1.1 spelling must be on the root element"
		);
		assert!(
			document.contains("white-space:pre"),
			"the CSS spelling must be in the stylesheet — WebKit reads only this one"
		);
	}

	/// The invariant that makes `lengthAdjust="spacingAndGlyphs"` a no-op
	/// rather than a distortion: a run's declared `textLength` is exactly the
	/// width of the columns it covers, so a renderer laying the run out on the
	/// same monospace grid has nothing to stretch or squeeze.
	#[test]
	fn every_text_run_declares_the_width_its_columns_occupy() {
		let document = document_for("Host   ->  [ x ]  |  ", 40);
		let runs = text_runs(&document);
		let width = cell_width();
		assert!(!runs.is_empty(), "the screen must have produced text runs");

		for (attributes, body) in &runs {
			let declared: u32 = attribute(attributes, "textLength")
				.expect("every run declares a textLength")
				.parse()
				.expect("textLength is an integer");
			let occupied = body.chars().count() as u32 * width;
			assert_eq!(
				declared, occupied,
				"run {body:?} declares {declared} but its {} characters occupy {occupied}",
				body.chars().count()
			);
		}
	}

	/// Padding inside and after a run must survive into the document as real
	/// space characters. If it is trimmed or collapsed at emit time, the
	/// `textLength` above stops matching and every column downstream shifts.
	#[test]
	fn interior_and_trailing_padding_survives_into_the_document() {
		let document = document_for("a   b", 12);
		let runs = text_runs(&document);
		let joined: String = runs.iter().map(|(_, body)| body.as_str()).collect();

		assert!(
			joined.contains("a   b"),
			"the three interior spaces must survive verbatim, got {joined:?}"
		);
		assert!(
			joined.chars().filter(|c| *c == ' ').count() >= 7,
			"the trailing blanks out to column 12 must survive too, got {joined:?}"
		);
	}

	/// A box-drawing character is drawn by the sprite pass, so its `<text>`
	/// must be invisible — otherwise the browser draws the font's glyph on top
	/// of the sprite and the same ink lands twice, at two different weights.
	#[test]
	fn a_drawn_character_carries_invisible_text_over_sprite_ink() {
		let document = document_for("a─b", 8);
		let runs = text_runs(&document);

		let drawn = runs
			.iter()
			.find(|(_, body)| body.contains('─'))
			.expect("the box character must still appear as selectable text");

		assert_eq!(
			attribute(&drawn.0, "fill").as_deref(),
			Some(TRANSPARENT),
			"its text must not be painted — the sprite is the ink"
		);
		assert!(
			runs.iter().any(|(attributes, body)| body.contains('a')
				&& attribute(attributes, "fill").as_deref() != Some(TRANSPARENT)),
			"the ordinary characters around it stay visible"
		);
	}

	/// Both backends go through the same paint pass, so a frame's rectangles
	/// have to arrive here too — the panel among them.
	#[test]
	fn the_panel_reaches_the_document_as_a_rounded_rectangle() {
		let document = document_for("hi", 8);

		assert!(
			document.contains("<rect"),
			"the panel and backgrounds are rectangles"
		);
		assert!(
			document.contains("rx="),
			"the default surface has a border radius, so the panel is rounded"
		);
	}

	/// A frame that repeats must not write its artifacts twice — that is the
	/// whole point of the timeline.
	#[test]
	fn an_unchanged_frame_extends_a_lifetime_rather_than_adding_elements() {
		let mut svg = svg_for(8);
		let mut snapshot = Snapshot::new(8, 2);
		snapshot.cells.fill(Square::from_char(' '));
		snapshot.cells[0] = Square::from_char('x');
		let snapshot = Arc::new(snapshot);

		svg.begin(&Metadata {
			width: 400,
			height: 100,
			frames_per_second: 50,
		})
		.unwrap();

		svg.accept(StreamContext {
			frame: &StreamFrame::new(Arc::clone(&snapshot), 1),
			pixels: None,
		})
		.unwrap();
		let after_one = svg.active.len();
		assert_eq!(svg.completed.len(), 0);

		svg.accept(StreamContext {
			frame: &StreamFrame::new(Arc::clone(&snapshot), 1),
			pixels: None,
		})
		.unwrap();

		assert_eq!(
			svg.active.len(),
			after_one,
			"an identical frame adds no new artifacts"
		);
		assert_eq!(
			svg.completed.len(),
			0,
			"and retires none of the existing ones"
		);
		assert!(
			svg.active.values().all(|life| life.duration_milliseconds > 20),
			"their durations should have grown instead"
		);
	}

	/// The hold has to accumulate at the recording's real rate. Deriving the
	/// per-tick duration by integer division first loses up to a millisecond
	/// per frame, which a long recording turns into visible drift.
	#[test]
	fn the_timeline_advances_at_the_recorded_frame_rate() {
		let mut svg = svg_for(8);
		let mut snapshot = Snapshot::new(8, 2);
		snapshot.cells.fill(Square::from_char(' '));

		// 3 fps is the case integer division gets wrong: 1000/3 truncates to
		// 333, losing a millisecond of every single frame.
		svg.begin(&Metadata {
			width: 400,
			height: 100,
			frames_per_second: 3,
		})
		.unwrap();

		svg.accept(StreamContext {
			frame: &StreamFrame::new(Arc::new(snapshot), 3),
			pixels: None,
		})
		.unwrap();

		assert_eq!(
			svg.now_milliseconds, 1000,
			"three ticks at three frames a second is one whole second"
		);
	}
}
