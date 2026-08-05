//! An animated SVG, drawn from the model.
//!
//! This is the sink that never asks for pixels. It reads the same [`Snapshot`]
//! the rasterizer would have drawn and emits rectangles and text instead, which
//! is why a tape whose only output is an SVG rasterizes nothing at all.
//!
//! The animation is CSS rather than SMIL: every distinct frame becomes a group
//! that is hidden except during its own slice of the timeline, and one
//! `@keyframes` rule per group says when that slice is. SMIL would express the
//! same thing more directly and is deprecated in every browser that matters.
//!
//! Text is emitted one run at a time — a run being a stretch of cells sharing a
//! colour and a set of attributes — with `textLength` pinning each run to the
//! exact pixel width its columns occupy. Without that pin the viewer's own
//! monospace face decides the advance, and a recording made at one cell width
//! renders at another, with the text sliding out from under the background
//! rectangles behind it.

use std::fmt::Write as _;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use rio_vt::crosswords::square::Wide;
use rio_vt::crosswords::style::StyleFlags;

use crate::fonts::Metrics;
use crate::model::{Palette, Rgba, Snapshot};
use crate::render::Surface;
use crate::stream::{Ctx, Meta, Sink};

/// One frame's worth of drawing, already reduced to shapes.
struct Cell {
	body: String,
	/// Capture ticks this frame stays on screen.
	hold: u32,
}

pub struct Svg {
	path: PathBuf,
	metrics: Metrics,
	surface: Surface,
	palette: Palette,
	family: String,
	font_size: f32,
	frames: Vec<Cell>,
	width: u16,
	height: u16,
	frames_per_second: u8,
	origin: (u32, u32),
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
		Self {
			path,
			metrics,
			surface,
			palette,
			family,
			font_size,
			frames: Vec::new(),
			width: 0,
			height: 0,
			frames_per_second: 1,
			origin: (inset, inset),
		}
	}

	/// Cell backgrounds, merged into horizontal runs of one colour.
	fn backgrounds(&self, snapshot: &Snapshot, out: &mut String) {
		let default = self
			.palette
			.named(rio_vt::config::colors::NamedColor::Background);
		let (origin_x, origin_y) = self.origin;

		for row in 0..snapshot.screen_rows {
			let mut start = 0u16;
			let mut current = self.background_of(snapshot, 0, row);

			for column in 1..=snapshot.columns {
				// A sentinel that can never equal a resolved colour, to flush
				// the last run without duplicating the body of the loop.
				let color = if column == snapshot.columns {
					[0, 0, 0, 0]
				} else {
					self.background_of(snapshot, column, row)
				};

				if color != current {
					if current != default {
						let _ = write!(
							out,
							r#"<rect x="{}" y="{}" width="{}" height="{}" fill="{}"/>"#,
							origin_x + start as u32 * self.metrics.cell_width,
							origin_y + row as u32 * self.metrics.cell_height,
							(column - start) as u32 * self.metrics.cell_width,
							self.metrics.cell_height,
							hex(current)
						);
					}
					start = column;
					current = color;
				}
			}
		}
	}

	fn background_of(&self, snapshot: &Snapshot, column: u16, row: u16) -> Rgba {
		self.palette
			.resolve(snapshot.cell(column, row), &snapshot.styles)
			.background
	}

	/// Text, one run of shared styling at a time.
	fn text(&self, snapshot: &Snapshot, out: &mut String) {
		let (origin_x, origin_y) = self.origin;

		for row in 0..snapshot.screen_rows {
			let baseline = origin_y as f32
				+ (row as u32 * self.metrics.cell_height) as f32
				+ self.metrics.baseline;

			let mut run = String::new();
			let mut run_start = 0u16;
			let mut run_style: Option<(Rgba, StyleFlags)> = None;

			let flush = |run: &mut String, start: u16, end: u16, style: Option<(Rgba, StyleFlags)>, out: &mut String| {
				if run.trim().is_empty() || start == end {
					run.clear();
					return;
				}
				let (color, flags) = style.unwrap_or(([0xff; 4], StyleFlags::empty()));
				let _ = write!(
					out,
					r#"<text x="{}" y="{baseline}" fill="{}" textLength="{}" lengthAdjust="spacingAndGlyphs"{}>{}</text>"#,
					origin_x + start as u32 * self.metrics.cell_width,
					hex(color),
					(end - start) as u32 * self.metrics.cell_width,
					decorations(flags),
					escape(run),
				);
				run.clear();
			};

			for column in 0..snapshot.columns {
				let cell = snapshot.cell(column, row);
				// The trailing half of a wide character carries no glyph; it is
				// covered by the run its partner started.
				if cell.wide() == Wide::Spacer {
					continue;
				}

				let resolved = self.palette.resolve(cell, &snapshot.styles);
				let style = (resolved.foreground, resolved.flags);

				if run_style != Some(style) {
					flush(&mut run, run_start, column, run_style, out);
					run_start = column;
					run_style = Some(style);
				}

				run.push(match resolved.character {
					'\0' => ' ',
					other => other,
				});
			}

			flush(&mut run, run_start, snapshot.columns, run_style, out);
		}
	}

	fn cursor(&self, snapshot: &Snapshot, out: &mut String) {
		use rio_vt::ansi::CursorShape;

		if !snapshot.cursor_visible || snapshot.cursor.content == CursorShape::Hidden {
			return;
		}

		let column = snapshot.cursor.pos.col.0 as u32;
		let row = snapshot.cursor.pos.row.0.max(0) as u32;
		if column >= snapshot.columns as u32 || row >= snapshot.screen_rows as u32 {
			return;
		}

		let (origin_x, origin_y) = self.origin;
		let x = origin_x + column * self.metrics.cell_width;
		let y = origin_y + row * self.metrics.cell_height;
		let thickness = (self.metrics.underline_thickness * 1.5).round().max(2.0) as u32;

		let (x, y, width, height) = match snapshot.cursor.content {
			CursorShape::Block => (x, y, self.metrics.cell_width, self.metrics.cell_height),
			CursorShape::Underline => (
				x,
				y + self.metrics.cell_height - thickness,
				self.metrics.cell_width,
				thickness,
			),
			CursorShape::Beam => (x, y, thickness, self.metrics.cell_height),
			CursorShape::Hidden => return,
		};

		let _ = write!(
			out,
			r#"<rect x="{x}" y="{y}" width="{width}" height="{height}" fill="{}"/>"#,
			hex(
				self.palette
					.named(rio_vt::config::colors::NamedColor::Cursor)
			)
		);
	}

	/// The document: the panel, the per-frame groups, and the CSS that says
	/// when each group is on screen.
	fn document(&self) -> String {
		let total: u32 = self.frames.iter().map(|frame| frame.hold.max(1)).sum();
		let seconds = total as f64 / self.frames_per_second as f64;

		let mut out = String::new();
		let _ = write!(
			out,
			r#"<svg xmlns="http://www.w3.org/2000/svg" width="{}" height="{}" viewBox="0 0 {} {}" font-family="{}, ui-monospace, monospace" font-size="{}px">"#,
			self.width,
			self.height,
			self.width,
			self.height,
			escape(&self.family),
			self.font_size,
		);

		out.push_str("<style>");
		// `visibility` rather than `display`, and `steps(1)` on every rule: a
		// frame has to appear and vanish at an instant, not fade across the
		// interval to the next one.
		let _ = write!(
			out,
			".f{{visibility:hidden;animation-duration:{seconds}s;animation-iteration-count:infinite;animation-timing-function:steps(1,end)}}"
		);

		let mut elapsed = 0u32;
		for (index, frame) in self.frames.iter().enumerate() {
			let from = percent(elapsed, total);
			elapsed += frame.hold.max(1);
			let to = percent(elapsed, total);

			let _ = write!(out, ".f{index}{{animation-name:k{index}}}");
			// The final frame runs to 100%; an intermediate one hands over at
			// exactly the tick the next one starts, so there is never a gap and
			// never an overlap.
			let _ = write!(
				out,
				"@keyframes k{index}{{{from}%{{visibility:visible}}{to}%{{visibility:hidden}}}}"
			);
		}
		out.push_str("</style>");

		// The panel, drawn once and never animated — it is identical on every
		// frame, and repeating it per frame would multiply the file size by the
		// frame count for no visible difference.
		if let Some(fill) = self.surface.margin_fill {
			let _ = write!(
				out,
				r#"<rect width="{}" height="{}" fill="{}"/>"#,
				self.width,
				self.height,
				hex(fill)
			);
		}
		let _ = write!(
			out,
			r#"<rect x="{margin}" y="{margin}" width="{}" height="{}" rx="{}" fill="{}"/>"#,
			self.width as u32 - 2 * self.surface.margin,
			self.height as u32 - 2 * self.surface.margin,
			self.surface.border_radius,
			hex(
				self.palette
					.named(rio_vt::config::colors::NamedColor::Background)
			),
			margin = self.surface.margin,
		);

		for (index, frame) in self.frames.iter().enumerate() {
			let _ = write!(out, r#"<g class="f f{index}">{}</g>"#, frame.body);
		}

		out.push_str("</svg>");
		out
	}
}

impl Sink for Svg {
	/// The whole point: this sink draws from the model, so a recording that
	/// outputs only SVG never rasterizes a single frame.
	fn wants_pixels(&self) -> bool {
		false
	}

	fn begin(&mut self, meta: &Meta) -> Result<()> {
		self.width = meta.width;
		self.height = meta.height;
		self.frames_per_second = meta.frames_per_second.max(1);
		Ok(())
	}

	fn accept(&mut self, ctx: Ctx<'_>) -> Result<()> {
		let snapshot = &ctx.frame.snapshot;
		let mut body = String::new();

		self.backgrounds(snapshot, &mut body);
		// Before the text, so a block cursor sits behind its glyph rather than
		// erasing it.
		self.cursor(snapshot, &mut body);
		self.text(snapshot, &mut body);

		self.frames.push(Cell {
			body,
			hold: ctx.frame.hold,
		});
		Ok(())
	}

	fn finish(self: Box<Self>) -> Result<()> {
		anyhow::ensure!(
			!self.frames.is_empty(),
			"no frames were captured, so {} has nothing to show",
			self.path.display()
		);

		if let Some(parent) = self
			.path
			.parent()
			.filter(|parent| !parent.as_os_str().is_empty())
		{
			std::fs::create_dir_all(parent)
				.with_context(|| format!("creating {}", parent.display()))?;
		}

		let document = self.document();
		let mut file = std::io::BufWriter::new(
			std::fs::File::create(&self.path)
				.with_context(|| format!("creating {}", self.path.display()))?,
		);
		file.write_all(document.as_bytes())
			.with_context(|| format!("writing {}", self.path.display()))?;
		file.flush()
			.with_context(|| format!("flushing {}", self.path.display()))
	}
}

/// A keyframe offset, as a percentage with enough places to keep two adjacent
/// frames distinct in a long recording.
fn percent(at: u32, total: u32) -> f64 {
	if total == 0 {
		return 0.0;
	}
	(at as f64 * 100.0 / total as f64 * 10_000.0).round() / 10_000.0
}

fn hex(color: Rgba) -> String {
	format!("#{:02x}{:02x}{:02x}", color[0], color[1], color[2])
}

/// The SVG spelling of the attributes that survive the trip.
///
/// Bold and italic are real font selections; underline and strikethrough are
/// `text-decoration`. Blink has no SVG equivalent and is deliberately dropped
/// rather than approximated — a second animation on the same element would
/// fight the one that shows the frame.
fn decorations(flags: StyleFlags) -> String {
	let mut out = String::new();

	if flags.contains(StyleFlags::BOLD) {
		out.push_str(r#" font-weight="bold""#);
	}
	if flags.contains(StyleFlags::ITALIC) {
		out.push_str(r#" font-style="italic""#);
	}

	let underlined = flags.intersects(StyleFlags::ALL_UNDERLINES);
	let struck = flags.contains(StyleFlags::STRIKEOUT);
	match (underlined, struck) {
		(true, true) => out.push_str(r#" text-decoration="underline line-through""#),
		(true, false) => out.push_str(r#" text-decoration="underline""#),
		(false, true) => out.push_str(r#" text-decoration="line-through""#),
		(false, false) => {}
	}

	out
}

/// XML-escape, and drop the control characters XML has no way to carry.
///
/// A terminal grid can hold bytes that are perfectly legal in a cell and
/// illegal in an XML document; emitting one produces a file no parser will
/// open, which is a worse outcome than losing an unprintable character.
fn escape(text: &str) -> String {
	let mut out = String::with_capacity(text.len());
	for character in text.chars() {
		match character {
			'&' => out.push_str("&amp;"),
			'<' => out.push_str("&lt;"),
			'>' => out.push_str("&gt;"),
			'"' => out.push_str("&quot;"),
			'\'' => out.push_str("&apos;"),
			'\t' | '\n' | '\r' => out.push(' '),
			control if (control as u32) < 0x20 => out.push(' '),
			other => out.push(other),
		}
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn markup_characters_are_escaped() {
		assert_eq!(escape("a<b>&c"), "a&lt;b&gt;&amp;c");
		assert_eq!(escape(r#""quoted""#), "&quot;quoted&quot;");
	}

	/// A stray control byte in a cell must not produce a file no parser will
	/// open.
	#[test]
	fn control_characters_become_spaces() {
		assert_eq!(escape("a\u{1}b"), "a b");
		assert_eq!(escape("tab\there"), "tab here");
	}

	#[test]
	fn keyframe_offsets_span_the_whole_timeline() {
		assert_eq!(percent(0, 4), 0.0);
		assert_eq!(percent(1, 4), 25.0);
		assert_eq!(percent(4, 4), 100.0);
		assert_eq!(percent(0, 0), 0.0, "an empty timeline must not divide by zero");
	}

	#[test]
	fn decorations_combine_rather_than_overwrite() {
		let both = decorations(StyleFlags::UNDERLINE | StyleFlags::STRIKEOUT);
		assert!(both.contains("underline line-through"));

		let bold_italic = decorations(StyleFlags::BOLD | StyleFlags::ITALIC);
		assert!(bold_italic.contains("bold") && bold_italic.contains("italic"));
	}
}
