use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use xmlwriter::{Options, XmlWriter};

use rio_vt::ansi::CursorShape;
use rio_vt::crosswords::square::Wide;
use rio_vt::crosswords::style::StyleFlags;

use super::font_embed;
use crate::fonts::{Fonts, Metrics};
use crate::geom::{Cell, Color};
use crate::grid::{Grid, GridOptions};
use crate::model::{Palette, Snapshot};
use crate::outline::{GlyphKey, Outlines};
use crate::render::Surface;
use crate::shape::{FontKey, PlacedGlyph, Shaper};
use crate::stream::{Context as StreamContext, Metadata, Sink};

const NAMESPACE_SVG: &str = "http://www.w3.org/2000/svg";
const DEFAULT_FALLBACK_FONT: &str = "drafting-mono";
const TRANSPARENT: &str = "transparent";

/// `Glyph::scale` is stored as an integer so the artifact can be hashed and
/// compared for equality; this is the fixed-point precision it is scaled by.
const SCALE_FIXED_POINT: f32 = 1000.0;

const CURSOR_THICKNESS_MULTIPLIER: f32 = 1.5;
const MINIMUM_CURSOR_THICKNESS: f32 = 2.0;

/// Represents a distinct visual element on the screen.
/// Deriving Hash and Eq allows us to cheaply diff the screen state.
#[derive(Clone, PartialEq, Eq, Hash)]
enum VisualArtifact {
	Background {
		column: u32,
		row: u32,
		width: u32,
		height: u32,
		color: Color,
	},
	/// A real `<text>` run — the default rendering for terminal content.
	///
	/// `visible` distinguishes two very different roles the same shape ends
	/// up serving:
	///
	/// - `visible: true` is the common case: real ink *and* real,
	///   copy-pasteable content in one element. `font_id: Some(id)` embeds
	///   and forces that specific face (see `encode/font_embed.rs`);
	///   `font_id: None` means an RTL run, which deliberately renders with
	///   no forced family at all, trusting the browser's own bidi engine
	///   and the host's system Arabic fallback rather than reproducing
	///   joining ourselves (a subsetted face has no `GSUB`).
	/// - `visible: false` is the old dual-layer scheme, kept only for a
	///   glyph that is not representable as `<text>` at all (no plain cmap
	///   entry for it, or shaping substituted a different glyph than a bare
	///   cmap lookup would give — see `Shaper::cmap_glyph`): an invisible
	///   run carries the real text for selection, paired with a `Glyph`
	///   artifact drawn on top for ink, exactly as before this session's
	///   rework of this file.
	TextRun {
		column: u32,
		row: u32,
		width: u32,
		text: String,
		color: Color,
		font_id: Option<u64>,
		is_right_to_left: bool,
		visible: bool,
	},
	/// Ink for the narrow cases `TextRun` can't cover on its own — see the
	/// `visible: false` case above. Addressed by glyph ID, so it sidesteps
	/// `cmap`/`GSUB` entirely, at the cost of needing its outline baked into
	/// `<defs>` as a `<path>` (see `Outlines`).
	Glyph {
		font_id: u64,
		glyph_id: u32,
		x: u32,
		y: u32,
		color: Color,
		scale: u32, // Stored as scaled int to maintain Hash/Eq
	},
	Image {
		x: u32,
		y: u32,
		width: u32,
		height: u32,
		base64_data: String,
	},
}

/// What a contiguous stretch of one bidi run's characters decided to become,
/// while `extract_text_runs` walks it — see that method's doc for the full
/// splitting rationale.
#[derive(Clone, Copy, PartialEq)]
enum SubrunKind {
	/// Real text, forcing the embedded face `font_id` — the character's
	/// plain cmap lookup against that face matches what shaping produced.
	Embedded(u64, Color),
	/// Real text with no forced face — always an RTL run; see the
	/// `TextRun::font_id` doc for why.
	Ambient(Color),
	/// Not representable as `<text>` at all: the pre-existing invisible-text
	/// + outline-`<path>` pairing.
	Legacy,
	/// SGR-concealed (`StyleFlags::HIDDEN`) — no ink, and excluded from the
	/// copyable text entirely rather than merely hidden, so concealed input
	/// can't leak into a clipboard.
	Hidden,
}

/// Tracks the temporal lifespan of a visual artifact for SMIL animation.
#[derive(Clone, Debug)]
struct Lifespan {
	begin_milliseconds: u32,
	duration_milliseconds: u32,
}

pub struct Svg {
	path: PathBuf,
	metrics: Metrics,
	surface: Surface,
	palette: Palette,
	family: String,
	font_size: f32,

	// Delta diffing state
	active_artifacts: HashMap<VisualArtifact, Lifespan>,
	completed_artifacts: Vec<(VisualArtifact, Lifespan)>,
	current_time_milliseconds: u32,

	width: u16,
	height: u16,
	frames_per_second: u8,
	origin: (u32, u32),

	shaper: Shaper,
	outlines: Outlines,
	/// Cells resolved once per frame, reused for both shaping and the cursor
	/// so neither has to re-resolve the palette itself.
	grid: Grid,

	/// Every character drawn as real embedded-font text over the whole
	/// recording, keyed by which face drew it, mapped to the glyph shaping
	/// chose (== that face's own plain cmap lookup — see
	/// `Shaper::cmap_glyph`). Accumulated frame by frame in
	/// `extract_text_runs` so `finish()` can subset and embed each face
	/// exactly once, for exactly the glyphs the recording ended up using.
	used_chars_by_font: HashMap<u64, BTreeMap<char, u32>>,
}

impl Svg {
	pub fn new(
		path: PathBuf,
		metrics: Metrics,
		surface: Surface,
		palette: Palette,
		family: String,
		font_size: f32,
	) -> Self {
		let inset = surface.margin + surface.padding;

		// Resolve fonts, seamlessly substituting a preferred monospace fallback if needed.
		let fonts = Fonts::resolve(Some(&family), font_size, 1.0).unwrap_or_else(|_| {
			Fonts::resolve(Some(DEFAULT_FALLBACK_FONT), font_size, 1.0)
				.expect("Fallback to default monospace font must succeed")
		});

		Self {
			path,
			metrics,
			surface,
			palette,
			family,
			font_size,

			active_artifacts: HashMap::new(),
			completed_artifacts: Vec::new(),
			current_time_milliseconds: 0,

			width: 0,
			height: 0,
			frames_per_second: 1,
			origin: (inset, inset),
			shaper: Shaper::new(fonts),
			outlines: Outlines::new(),
			grid: Grid::new(0, 0),
			used_chars_by_font: HashMap::new(),
		}
	}

	/// Identifies and extracts contiguous horizontal runs of background colors.
	fn extract_backgrounds(&self, snapshot: &Snapshot, artifacts: &mut HashSet<VisualArtifact>) {
		let default_color = self
			.palette
			.named(rio_vt::config::colors::NamedColor::Background);
		let (origin_x, origin_y) = self.origin;

		for row in 0..snapshot.screen_rows {
			let mut start_column = 0u16;
			let mut current_color = self.background_of(snapshot, 0, row);

			for column in 1..=snapshot.columns {
				let next_color = if column == snapshot.columns {
					Color::TRANSPARENT
				} else {
					self.background_of(snapshot, column, row)
				};

				if next_color != current_color {
					if current_color != default_color {
						artifacts.insert(VisualArtifact::Background {
							column: origin_x + start_column as u32 * self.metrics.cell_width,
							row: origin_y + row as u32 * self.metrics.cell_height,
							width: (column - start_column) as u32 * self.metrics.cell_width,
							height: self.metrics.cell_height,
							color: current_color,
						});
					}
					start_column = column;
					current_color = next_color;
				}
			}
		}
	}

	fn background_of(&self, snapshot: &Snapshot, column: u16, row: u16) -> Color {
		self.palette
			.resolve(
				snapshot.cell(column, row),
				&snapshot.styles,
				&GridOptions::default(),
			)
			.background
	}

	/// Shapes every row of `self.grid` and inserts one real `<text>`
	/// [`VisualArtifact::TextRun`] per contiguous stretch of matching (bidi
	/// direction, embedded face, representability) — real ink *and* real
	/// selectable content in the same element, the default for everything a
	/// row draws: ASCII, CJK, box drawing, Nerd Font icons, RTL scripts.
	///
	/// A character only falls back to the pre-existing invisible-text +
	/// outline-`<path>` pairing (`SubrunKind::Legacy`) when it genuinely
	/// isn't representable as `<text>` — no plain cmap entry in the face
	/// that shaped it, or shaping landed on a different glyph than that
	/// bare cmap lookup would (a ligature or other contextual
	/// substitution); see `Shaper::cmap_glyph`. That check is skipped
	/// entirely for RTL runs by design: they always render as real text
	/// with no forced embedded face, trusting the browser's own bidi engine
	/// and the host's system Arabic fallback (which has real `GSUB`) rather
	/// than second-guessing joining ourselves.
	fn extract_text_runs(&mut self, artifacts: &mut HashSet<VisualArtifact>) {
		let (origin_x, origin_y) = self.origin;
		let metrics = self.metrics;
		let font_size = self.font_size;

		for row in 0..self.grid.rows {
			// Cloned out so the borrow on `self.shaper` ends here, before
			// the walk below needs `&mut self` for `self.shaper.cmap_glyph`
			// and `self.outlines`.
			let row_layout = self.shaper.row(&self.grid, row);
			let text = row_layout.text.clone();
			let bidirectional_runs = row_layout.bidirectional_runs.clone();
			let column_by_byte_index = row_layout.column_by_byte_index.clone();
			let glyphs = row_layout.glyphs.clone();

			if bidirectional_runs.is_empty() {
				continue;
			}

			// Every column a glyph actually landed on, keyed to every
			// `PlacedGlyph` shaping produced for it — usually one, but a
			// cluster that decomposed into more than one glyph (rather than
			// consuming more than one *column*, which the second entry
			// would have no place of its own for) leaves them all here
			// under the cluster's start column.
			let mut glyphs_by_column: HashMap<u16, Vec<PlacedGlyph>> = HashMap::new();
			for glyph in &glyphs {
				glyphs_by_column
					.entry(glyph.column)
					.or_default()
					.push(*glyph);
			}

			let row_top = origin_y as f32 + (row as u32 * metrics.cell_height) as f32;
			let baseline_y = row_top + metrics.baseline;

			for run in &bidirectional_runs {
				let (start_byte, end_byte) = run.text_range;
				if start_byte >= end_byte {
					continue;
				}

				let mut sub_start = start_byte;
				let mut sub_kind: Option<SubrunKind> = None;

				for (offset, ch) in text[start_byte..end_byte].char_indices() {
					let byte_index = start_byte + offset;
					let Some(&column) = column_by_byte_index.get(byte_index) else {
						continue;
					};

					let resolved = *self.grid.cell(Cell::new(column, row));
					let placed = glyphs_by_column.get(&column);

					let decided: Option<SubrunKind> = if resolved.flags.contains(StyleFlags::HIDDEN)
					{
						Some(SubrunKind::Hidden)
					} else if run.is_right_to_left {
						Some(SubrunKind::Ambient(self.glyph_color(
							column,
							row,
							resolved.foreground,
						)))
					} else if let Some(list) = placed {
						if let [only] = list.as_slice()
							&& self.shaper.cmap_glyph(only.font, ch) == Some(only.identifier)
						{
							self.used_chars_by_font
								.entry(only.font.0)
								.or_default()
								.insert(ch, only.identifier);
							Some(SubrunKind::Embedded(
								only.font.0,
								self.glyph_color(column, row, resolved.foreground),
							))
						} else {
							Some(SubrunKind::Legacy)
						}
					} else {
						// No glyph landed on this column at all: plain
						// whitespace, or a character a ligature to its left
						// swallowed. Either way there is nothing here that
						// constrains the surrounding run's face — extend
						// whatever is already active (or stay undecided, if
						// this is a leading gap) rather than forcing a split.
						None
					};

					if matches!(decided, Some(SubrunKind::Legacy))
						&& let Some(list) = placed
					{
						for glyph in list {
							self.emit_fallback_glyph(
								artifacts,
								*glyph,
								row,
								resolved.foreground,
								origin_x,
								row_top,
								font_size,
							);
						}
					}

					match (sub_kind, decided) {
						(None, None) => {}
						(None, Some(kind)) => sub_kind = Some(kind),
						(Some(_), None) => {}
						(Some(active), Some(kind)) if active == kind => {}
						(Some(active), Some(kind)) => {
							self.emit_text_subrun(
								artifacts,
								row,
								&text,
								&column_by_byte_index,
								sub_start,
								byte_index,
								active,
								run.is_right_to_left,
								origin_x,
								baseline_y,
							);
							sub_start = byte_index;
							sub_kind = Some(kind);
						}
					}
				}

				if let Some(kind) = sub_kind {
					self.emit_text_subrun(
						artifacts,
						row,
						&text,
						&column_by_byte_index,
						sub_start,
						end_byte,
						kind,
						run.is_right_to_left,
						origin_x,
						baseline_y,
					);
				}
			}
		}
	}

	/// Emit one finished `[sub_start, sub_end)` stretch of a bidi run as a
	/// `VisualArtifact::TextRun`. `Hidden` stretches are dropped entirely —
	/// no ink, and excluded from the copyable text rather than merely
	/// hidden, so concealed input can't leak into a clipboard.
	fn emit_text_subrun(
		&mut self,
		artifacts: &mut HashSet<VisualArtifact>,
		row: u16,
		text: &str,
		column_by_byte_index: &[u16],
		sub_start: usize,
		sub_end: usize,
		kind: SubrunKind,
		is_right_to_left: bool,
		origin_x: u32,
		baseline_y: f32,
	) {
		if sub_start >= sub_end || matches!(kind, SubrunKind::Hidden) {
			return;
		}

		let Some(&start_column) = column_by_byte_index.get(sub_start) else {
			return;
		};
		let Some(&last_column) = column_by_byte_index.get(sub_end - 1) else {
			return;
		};

		let last_span = match self.grid.cell(Cell::new(last_column, row)).wide {
			Wide::Wide => 2,
			_ => 1,
		};
		let end_column = last_column + last_span;
		let metrics = self.metrics;

		let (font_id, color, visible) = match kind {
			SubrunKind::Embedded(font_id, color) => (Some(font_id), color, true),
			SubrunKind::Ambient(color) => (None, color, true),
			SubrunKind::Legacy => {
				let start_foreground = self.grid.cell(Cell::new(start_column, row)).foreground;
				(None, start_foreground, false)
			}
			SubrunKind::Hidden => return,
		};

		artifacts.insert(VisualArtifact::TextRun {
			column: origin_x + start_column as u32 * metrics.cell_width,
			row: baseline_y.round() as u32,
			width: (end_column - start_column) as u32 * metrics.cell_width,
			text: text[sub_start..sub_end].to_string(),
			color,
			font_id,
			is_right_to_left,
			visible,
		});
	}

	/// Draw one glyph the old way — an outline `<path>` referenced by a
	/// `<use>` — for the narrow case a character isn't representable as
	/// `<text>` at all. Kept byte-for-byte equivalent to what this file's
	/// `extract_glyphs` did before real `<text>` became the default: same
	/// outline cache, same cursor-contrast colour, same "no outline means
	/// nothing to draw" skip for a genuine `.notdef`.
	fn emit_fallback_glyph(
		&mut self,
		artifacts: &mut HashSet<VisualArtifact>,
		glyph: PlacedGlyph,
		row: u16,
		foreground: Color,
		origin_x: u32,
		row_top: f32,
		font_size: f32,
	) {
		let Some(face) = self.shaper.face(glyph.font) else {
			return;
		};
		let key = GlyphKey {
			font: glyph.font.0,
			glyph: glyph.identifier,
		};
		self.outlines.ensure(key, face, font_size);
		if self.outlines.get(key).is_none() {
			// The face has no outline for this glyph (e.g. a control
			// character mapped to `.notdef`) — nothing to draw.
			return;
		}

		let x = origin_x as f32 + glyph.horizontal_position;
		let y = row_top + glyph.vertical_position;

		artifacts.insert(VisualArtifact::Glyph {
			font_id: key.font,
			glyph_id: key.glyph,
			x: x.round() as u32,
			y: y.round() as u32,
			color: self.glyph_color(glyph.column, row, foreground),
			scale: (glyph.scale * SCALE_FIXED_POINT).round() as u32,
		});
	}

	/// A glyph's ink colour, swapped for a contrasting one when a block
	/// cursor sits on top of it — otherwise the caret would paint over the
	/// character and make it unreadable, the same fix `render.rs` applies.
	fn glyph_color(&self, column: u16, row: u16, foreground: Color) -> Color {
		match self.grid.cursor {
			Some(caret)
				if caret.shape == CursorShape::Block && caret.at == Cell::new(column, row) =>
			{
				caret.text
			}
			_ => foreground,
		}
	}

	/// The cursor, as a filled rectangle sized to its shape. Reuses the
	/// `Background` artifact rather than adding a dedicated variant — a
	/// cursor is visually just another rectangle, and it needs to sort
	/// underneath the glyphs the same way a background does.
	fn extract_cursor(&self, artifacts: &mut HashSet<VisualArtifact>) {
		let Some(caret) = self.grid.cursor else {
			return;
		};
		if caret.shape == CursorShape::Hidden {
			return;
		}

		let (origin_x, origin_y) = self.origin;
		let metrics = self.metrics;
		let start_x = origin_x + caret.at.column as u32 * metrics.cell_width;
		let start_y = origin_y + caret.at.row as u32 * metrics.cell_height;
		let thickness = (metrics.underline_thickness * CURSOR_THICKNESS_MULTIPLIER)
			.round()
			.max(MINIMUM_CURSOR_THICKNESS) as u32;

		let (x, y, width, height) = match caret.shape {
			CursorShape::Block => (start_x, start_y, metrics.cell_width, metrics.cell_height),
			CursorShape::Underline => (
				start_x,
				start_y + metrics.cell_height - thickness,
				metrics.cell_width,
				thickness,
			),
			CursorShape::Beam => (start_x, start_y, thickness, metrics.cell_height),
			CursorShape::Hidden => return,
		};

		artifacts.insert(VisualArtifact::Background {
			column: x,
			row: y,
			width,
			height,
			color: caret.color,
		});
	}

	/// The core delta diffing engine. Compares the current frame's artifacts
	/// against the active ones, extending lifetimes or retiring them as needed.
	fn process_frame_delta(
		&mut self,
		current_artifacts: HashSet<VisualArtifact>,
		hold_milliseconds: u32,
	) {
		let mut next_active = HashMap::new();

		for artifact in current_artifacts {
			if let Some(mut existing_lifespan) = self.active_artifacts.remove(&artifact) {
				// Artifact persists: extend its duration
				existing_lifespan.duration_milliseconds += hold_milliseconds;
				next_active.insert(artifact, existing_lifespan);
			} else {
				// New artifact appears
				next_active.insert(
					artifact,
					Lifespan {
						begin_milliseconds: self.current_time_milliseconds,
						duration_milliseconds: hold_milliseconds,
					},
				);
			}
		}

		// Any artifacts left in `active_artifacts` are no longer on screen.
		for (dead_artifact, final_lifespan) in self.active_artifacts.drain() {
			self.completed_artifacts
				.push((dead_artifact, final_lifespan));
		}

		self.active_artifacts = next_active;
		self.current_time_milliseconds += hold_milliseconds;
	}

	/// Renders the complete Document Object Model using `xmlwriter`.
	fn write_document(&self) -> String {
		// `xmlwriter`'s own default pretty-printing indents every element —
		// including `<text>`, whose content is not decoration but the
		// exact, cell-for-cell string a `textLength`/`spacingAndGlyphs`
		// run's width was computed for. Indenting would inject a `\n` plus
		// leading spaces into that string that this file never accounted
		// for, so it is disabled outright rather than special-cased away
		// per element.
		let options = Options {
			indent: xmlwriter::Indent::None,
			attributes_indent: xmlwriter::Indent::None,
			..Options::default()
		};
		let mut xml = XmlWriter::new(options);

		xml.start_element("svg");
		xml.write_attribute("xmlns", NAMESPACE_SVG);
		xml.write_attribute("width", &self.width);
		xml.write_attribute("height", &self.height);
		xml.write_attribute("viewBox", &format!("0 0 {} {}", self.width, self.height));
		xml.write_attribute(
			"font-family",
			&format!("{}, ui-monospace, monospace", self.family),
		);
		xml.write_attribute("font-size", &format!("{}px", self.font_size));
		// A `<text>` run's content is padded with real space characters out
		// to its declared `textLength` so every character — including
		// blank cells — lands on its own grid column (see `emit_text_subrun`).
		// SVG's default whitespace handling collapses runs of whitespace
		// down to a single space before `textLength` ever sees it, which
		// then force-stretches whatever collapsed-down handful of
		// characters remain across the whole declared width — exactly the
		// grotesquely oversized glyphs this was written to fix. `preserve`
		// keeps every character, including every padding space,
		// significant, matching the 1-character-per-cell model the rest of
		// this file assumes throughout.
		xml.write_attribute("xml:space", "preserve");

		self.write_style(&mut xml);
		self.write_definitions(&mut xml);
		self.write_static_panel(&mut xml);

		// Combine completed and still-active artifacts for the final render.
		// Sorted by paint order rather than left in hash-map order, so a
		// cursor rectangle always sits underneath the glyph it covers instead
		// of wherever the hash happened to place it. `retired` distinguishes
		// the two so `write_artifact_with_smil` knows whether this exact
		// visual state ever needs to disappear again.
		let mut all_artifacts: Vec<_> = self
			.completed_artifacts
			.iter()
			.map(|(artifact, lifespan)| (artifact, lifespan, true))
			.chain(
				self.active_artifacts
					.iter()
					.map(|(artifact, lifespan)| (artifact, lifespan, false)),
			)
			.collect();
		all_artifacts.sort_by_key(|(artifact, _, _)| Self::paint_order(artifact));

		for (artifact, lifespan, retired) in all_artifacts {
			self.write_artifact_with_smil(&mut xml, artifact, lifespan, retired);
		}

		xml.end_element(); // </svg>
		xml.end_document()
	}

	/// Back-to-front paint order, then top-to-bottom, left-to-right position.
	///
	/// The layer comes first: backgrounds (including the cursor, which
	/// reuses that variant) sit under text, which sits under images. But the
	/// layer alone is not enough — `active_artifacts` is a `HashMap`, so
	/// without a position tiebreaker its iteration order (arbitrary, and
	/// different across rows) becomes the `<text>` elements' DOM order. A
	/// browser's copy/selection follows DOM order, not screen position, so
	/// that arbitrary order is exactly what made a multi-row selection come
	/// out scrambled: row 5 could easily land in the document before row 0.
	/// Sorting by (row, column) makes the DOM order match reading order.
	fn paint_order(artifact: &VisualArtifact) -> (u8, u32, u32) {
		match artifact {
			VisualArtifact::Background { column, row, .. } => (0, *row, *column),
			VisualArtifact::TextRun { column, row, .. } => (1, *row, *column),
			VisualArtifact::Glyph { x, y, .. } => (1, *y, *x),
			VisualArtifact::Image { x, y, .. } => (2, *y, *x),
		}
	}

	fn write_artifact_with_smil(
		&self,
		xml: &mut XmlWriter,
		artifact: &VisualArtifact,
		lifespan: &Lifespan,
		retired: bool,
	) {
		// An artifact that appears after the recording starts is not on
		// screen until its `begin`, so it has to start hidden — the base
		// SVG/CSS default for a shape element is visible, and there is
		// nothing else that would suppress it before then.
		let starts_hidden = lifespan.begin_milliseconds > 0;
		let hide_at_milliseconds = lifespan.begin_milliseconds + lifespan.duration_milliseconds;

		// A `<text>` element's content must be written last: once
		// `write_text` runs, `xmlwriter` has left the attributes phase and
		// any attribute written afterwards — `display` included — panics.
		let mut text_content: Option<&str> = None;

		match artifact {
			VisualArtifact::Background {
				column,
				row,
				width,
				height,
				color,
			} => {
				xml.start_element("rect");
				xml.write_attribute("x", column);
				xml.write_attribute("y", row);
				xml.write_attribute("width", width);
				xml.write_attribute("height", height);
				xml.write_attribute("fill", &color.to_string());
			}
			VisualArtifact::TextRun {
				column,
				row,
				width,
				text,
				color,
				font_id,
				is_right_to_left,
				visible,
			} => {
				xml.start_element("text");
				xml.write_attribute("x", column);
				xml.write_attribute("y", row);
				if *visible {
					xml.write_attribute("fill", &color.to_string());
					// `None` here is always an RTL run — see the
					// `TextRun::font_id` doc for why it deliberately has no
					// forced face at all.
					if let Some(font_id) = font_id {
						xml.write_attribute("font-family", &format!("f{font_id}"));
					}
				} else {
					xml.write_attribute("fill", TRANSPARENT);
				}
				xml.write_attribute("textLength", width);
				xml.write_attribute("lengthAdjust", "spacingAndGlyphs");
				if *is_right_to_left {
					xml.write_attribute("direction", "rtl");
				}
				text_content = Some(text);
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
				// Ink only — clicks must pass through to the invisible text
				// run underneath it, or dragging over it would select
				// nothing. Only ever paired with a `visible: false` text
				// run (the narrow non-representable-glyph case), so this is
				// still the exact situation it was written for.
				xml.write_attribute("pointer-events", "none");
			}
			// TODO: Image artifacts (Sixel/Kitty placements) are not yet extracted.
			VisualArtifact::Image { .. } => return,
		}

		if starts_hidden {
			xml.write_attribute("display", "none");
		}

		if let Some(text) = text_content {
			// `xmlwriter::write_text` only escapes `<`; a bare `&`, which
			// shell text is full of (`&&`, `a & b`), would otherwise land in
			// the output unescaped and make the whole document unparsable.
			xml.write_text(&text.replace('&', "&amp;"));
		}

		// Show at `begin`. Without a matching hide, a `<set>` that only ever
		// turns something on would leave it on forever once its own active
		// duration lapsed — SMIL reverts to the *underlying* value then, and
		// the underlying value here is "visible", not "hidden".
		if starts_hidden {
			xml.start_element("set");
			xml.write_attribute("attributeName", "display");
			xml.write_attribute("to", "inline");
			xml.write_attribute("begin", &format!("{}ms", lifespan.begin_milliseconds));
			xml.end_element(); // </set>
		}

		// Hide again once this exact visual state was replaced. Only
		// artifacts pulled from `completed_artifacts` need this — one still
		// in `active_artifacts` is on screen through the end of the
		// recording, so nothing ever turns it back off.
		if retired {
			xml.start_element("set");
			xml.write_attribute("attributeName", "display");
			xml.write_attribute("to", "none");
			xml.write_attribute("begin", &format!("{hide_at_milliseconds}ms"));
			xml.end_element(); // </set>
		}

		xml.end_element();
	}

	/// Embed every face this recording actually drew as real `<text>`, each
	/// subset down to exactly the glyphs it used (see `font_embed`) and
	/// named `f{font_id}` so `write_artifact_with_smil`'s `font-family:
	/// f{font_id}` finds it.
	///
	/// A face that fails to subset (a malformed font, an id that no longer
	/// resolves) is skipped with a warning rather than failing the whole
	/// recording — its `<text>` runs still rendered visibly and are still
	/// real, selectable text; they just fall through to the document's
	/// ambient `font-family` instead of the exact face shaping used.
	fn write_style(&self, xml: &mut XmlWriter) {
		// The CSS half of the whitespace contract the `xml:space="preserve"`
		// attribute in `write_document` states — see that comment for *why*
		// every space in a run has to survive. Both are needed because they
		// are read by disjoint sets of consumers: `xml:space` is the SVG 1.1
		// spelling, and is what the static rasterisers (resvg, librsvg) act
		// on; `white-space` is the SVG 2 / CSS Text spelling, and is what
		// browsers act on.
		//
		// WebKit is the one that makes this load-bearing rather than
		// belt-and-braces: it resolves SVG text whitespace purely through the
		// CSS property and ignores `xml:space` outright, so with the attribute
		// alone every run's padding collapsed away — and because each run also
		// carries a `textLength` measured for the *uncollapsed* string,
		// `lengthAdjust="spacingAndGlyphs"` then stretched whatever few glyphs
		// survived across the full declared width. A mostly-blank run holding
		// one `│` came out as a solid bar a dozen cells wide, and every value
		// in a padded column came out smeared across it. Firefox and Chromium
		// honour `xml:space` and so never showed it.
		let mut rules = String::from("text{white-space:pre}");

		for (font_id, codepoints) in &self.used_chars_by_font {
			let Some(face) = self.shaper.face(FontKey(*font_id)) else {
				continue;
			};
			match font_embed::build_font_face(
				&format!("f{font_id}"),
				face.data.as_ref(),
				face.index,
				codepoints,
			) {
				Ok(rule) => rules.push_str(&rule),
				Err(error) => {
					eprintln!(
						"dvd: warning: could not embed font f{font_id} in {}: {error:#}",
						self.path.display()
					);
				}
			}
		}

		xml.start_element("style");
		// The generated CSS is base64 plus punctuation from a fixed template
		// (see `font_embed::build_font_face`) — never `<` or `&` — so it
		// needs none of the escaping the text-bearing artifacts below do.
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

	fn write_static_panel(&self, xml: &mut XmlWriter) {
		if let Some(fill) = self.surface.margin_fill {
			xml.start_element("rect");
			xml.write_attribute("width", &self.width);
			xml.write_attribute("height", &self.height);
			xml.write_attribute("fill", &fill.to_string());
			xml.end_element();
		}

		let panel_color = self
			.palette
			.named(rio_vt::config::colors::NamedColor::Background);
		xml.start_element("rect");
		xml.write_attribute("x", &self.surface.margin);
		xml.write_attribute("y", &self.surface.margin);
		xml.write_attribute("width", &(self.width as u32 - 2 * self.surface.margin));
		xml.write_attribute("height", &(self.height as u32 - 2 * self.surface.margin));
		xml.write_attribute("rx", &self.surface.border_radius);
		xml.write_attribute("fill", &panel_color.to_string());
		xml.end_element();
	}
}

impl Sink for Svg {
	fn requires_pixels(&self) -> bool {
		false
	}

	fn begin(&mut self, meta: &Metadata) -> Result<()> {
		self.width = meta.width;
		self.height = meta.height;
		self.frames_per_second = meta.frames_per_second.max(1);
		Ok(())
	}

	fn accept(&mut self, context: StreamContext<'_>) -> Result<()> {
		let snapshot = &context.frame.snapshot;
		let mut current_frame_artifacts = HashSet::new();

		self.grid
			.fill(snapshot, &self.palette, &GridOptions::default());

		self.extract_backgrounds(snapshot, &mut current_frame_artifacts);
		self.extract_cursor(&mut current_frame_artifacts);
		self.extract_text_runs(&mut current_frame_artifacts);

		// TODO: extract_graphics() for Sixel/Kitty image placements.

		// Calculate hold in milliseconds based on FPS and ticks
		let hold_milliseconds = context.frame.hold_ticks * (1000 / self.frames_per_second as u32);

		self.process_frame_delta(current_frame_artifacts, hold_milliseconds);

		Ok(())
	}

	fn finish(self: Box<Self>) -> Result<()> {
		anyhow::ensure!(
			self.current_time_milliseconds > 0,
			"No frames were captured, so {} has nothing to show",
			self.path.display()
		);

		if let Some(parent_directory) = self.path.parent() {
			if !parent_directory.as_os_str().is_empty() {
				std::fs::create_dir_all(parent_directory).with_context(|| {
					format!("Creating directory {}", parent_directory.display())
				})?;
			}
		}

		let document_xml = self.write_document();

		let mut file_writer = BufWriter::new(
			File::create(&self.path)
				.with_context(|| format!("Creating file {}", self.path.display()))?,
		);

		file_writer
			.write_all(document_xml.as_bytes())
			.with_context(|| format!("Writing to {}", self.path.display()))?;

		file_writer
			.flush()
			.with_context(|| format!("Flushing buffer to {}", self.path.display()))
	}
}
#[cfg(test)]
mod tests {
	use super::*;
	use rio_vt::crosswords::square::Square;
	use std::sync::Arc;

	use crate::stream::Frame;

	/// Burn one screen holding `line` (padded out to `columns` with blanks, so
	/// the runs carry the interior and trailing whitespace that
	/// `write_document`'s whitespace contract exists for) and return the
	/// finished SVG document.
	fn document_for(line: &str, columns: u16) -> String {
		let mut snapshot = Snapshot::new(columns, 2);
		for (index, character) in line.chars().take(columns as usize).enumerate() {
			snapshot.cells[index] = Square::from_char(character);
		}

		let mut svg = Svg::new(
			PathBuf::from("test.svg"),
			Metrics {
				cell_width: 14,
				cell_height: 30,
				baseline: 23.0,
				underline_offset: 26.0,
				underline_thickness: 1.0,
				strikeout_offset: 14.0,
				strikeout_thickness: 1.0,
			},
			Surface::default(),
			Palette::default(),
			"monospace".to_string(),
			22.0,
		);

		svg.begin(&Metadata {
			width: 800,
			height: 200,
			frames_per_second: 50,
		})
		.expect("begin");
		svg.accept(StreamContext {
			frame: &Frame::new(Arc::new(snapshot), 1),
			pixels: None,
		})
		.expect("accept");

		svg.write_document()
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
	/// are read by disjoint consumers — `xml:space` by the SVG 1.1
	/// rasterisers, CSS `white-space` by browsers. WebKit honours only the
	/// latter, so with `xml:space` alone every run's padding collapsed and
	/// `lengthAdjust="spacingAndGlyphs"` smeared the survivors across a
	/// `textLength` measured for the uncollapsed string.
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
	/// rather than a distortion: a run's declared `textLength` is exactly its
	/// character count times the cell width, so a renderer that lays the run
	/// out on the same monospace grid has nothing to stretch or squeeze. Any
	/// run where the two disagree is one that will render smeared.
	#[test]
	fn every_text_run_declares_the_width_its_characters_occupy() {
		let document = document_for("Host   ->  [ x ]  │  ", 40);
		let runs = text_runs(&document);
		assert!(!runs.is_empty(), "the screen must have produced text runs");

		for (attributes, body) in &runs {
			let declared: u32 = attribute(attributes, "textLength")
				.expect("every run declares a textLength")
				.parse()
				.expect("textLength is an integer");
			let occupied = body.chars().count() as u32 * 14;
			assert_eq!(
				declared, occupied,
				"run {body:?} declares {declared} but its {} characters occupy {occupied}",
				body.chars().count()
			);
		}
	}

	/// Padding inside and after a run must survive into the document as real
	/// space characters. If it is ever trimmed or collapsed at emit time, the
	/// `textLength` above stops matching and every column downstream of it
	/// shifts.
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

	/// Bidi runs are created by Parley's bidi algorithm and used to split
	/// text runs in the accessible layer.
	#[test]
	fn bidi_run_splits_text_on_direction_change() {
		let ltr_run = crate::shape::BidirectionalRun {
			text_range: (0, 5),
			is_right_to_left: false,
		};
		let rtl_run = crate::shape::BidirectionalRun {
			text_range: (5, 10),
			is_right_to_left: true,
		};

		assert!(!ltr_run.is_right_to_left);
		assert!(rtl_run.is_right_to_left);
	}
}
