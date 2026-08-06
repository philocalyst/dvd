//! Choosing a face, and measuring the grid it implies.
//!
//! A terminal recording needs more than one font file. Text comes in four cuts,
//! and any real session mixes in characters the chosen family does not have —
//! powerline separators, box drawing, CJK, Arabic, emoji. Most of that is
//! delegated to Fontique, the font database Parley shapes against, which picks
//! the nearest cut of a family and walks per-script system fallback for
//! characters the family cannot draw.
//!
//! Per-script fallback has one hole, and it is exactly the hole a terminal
//! recording falls into. Nerd Font icons live in the Private Use Area, which
//! belongs to no script and which no system face declares coverage for, so
//! there is nothing for Fontique to fall back *to*: a themed prompt renders as
//! a row of tofu. The fix is to carry a face that has them, which is why a Nerd
//! Font is bundled alongside the plain monospace one and registered on every
//! run. It sits behind the chosen family, so it supplies the icons without
//! touching how ordinary text looks.
//!
//! What this module owns beyond that is the decision Fontique cannot make:
//! which family a recording defaults to, and what cell size that family
//! implies.
//!
//! Discovery deliberately never asks for the generic `monospace` family first.
//! On macOS that resolves to Courier New, a thin typewriter face that is the
//! single largest reason a rendered recording can look worse than the terminal
//! it was captured from.

use anyhow::{Context, Result};
use parley::FontContext;
use parley::fontique::{Blob, FamilyId};
use skrifa::{
	FontRef, MetadataProvider,
	instance::{LocationRef, Size},
	metrics::Metrics as SkrifaMetrics,
};

/// Bundled so a recording is reproducible on a machine with no fonts installed,
/// and so there is always a real face behind the last-resort fallback.
const BUNDLED: &[u8] = include_bytes!("../../../fonts/liberation_mono/LiberationMono-Regular.ttf");

const BUNDLED_FAMILY: &str = "Liberation Mono";

/// The symbol face: JetBrains Mono patched with the full Nerd Font set — the
/// icon ranges, plus complete box drawing and block elements.
///
/// The patched variant matters. Every glyph in this file advances 0.6em, the
/// same as its own `m`, so an icon occupies exactly one cell the way the
/// terminal already decided it would. A variant whose icons advance two cells
/// would need the fit-to-cell path in the renderer to shrink every one of them.
const BUNDLED_SYMBOLS: &[u8] =
	include_bytes!("../../../fonts/jetbrains_mono_nerd_font/JetBrainsMonoNerdFont-Medium.ttf");

/// Families tried in order when the tape does not name one, ordered by how well
/// each renders at recording sizes.
///
/// Nerd Font cuts come first, ahead of the plain families they patch and ahead
/// of the system faces. Installing one is a deliberate act — nobody has
/// `MesloLGS NF` by accident — and the terminal being recorded is almost
/// certainly already set to it. The plain macOS and Windows faces follow,
/// because they are what a machine with no Nerd Font is most likely showing.
const PREFERRED: &[&str] = &[
	"JetBrainsMono Nerd Font",
	"FiraCode Nerd Font",
	"Hack Nerd Font",
	"MesloLGS NF",
	"CaskaydiaCove Nerd Font",
	"SauceCodePro Nerd Font",
	"Iosevka Nerd Font",
	"JetBrains Mono",
	"SF Mono",
	"Menlo",
	"Cascadia Mono",
	"Cascadia Code",
	"Consolas",
	"Fira Code",
	"Hack",
	"Source Code Pro",
	"IBM Plex Mono",
	"Iosevka",
	"Ubuntu Mono",
	"Noto Sans Mono",
	"DejaVu Sans Mono",
	"Liberation Mono",
	"Monaco",
];

/// Everything needed to lay a grid out on whole pixels.
///
/// Integer cell sizes are not a rounding convenience — they are what keeps
/// column `n` at exactly `n * width` for every row. A fractional advance
/// accumulates across eighty columns into a visible drift between the text and
/// the background rectangles behind it.
#[derive(Clone, Copy, Debug)]
pub struct Metrics {
	pub cell_width: u32,
	pub cell_height: u32,
	/// Distance from the top of the cell down to the baseline.
	pub baseline: f32,
	/// Top edge of the underline, relative to the top of the cell.
	pub underline_offset: f32,
	pub underline_thickness: f32,
	/// Top edge of the strikethrough, relative to the top of the cell.
	pub strikeout_offset: f32,
	pub strikeout_thickness: f32,
}

/// The font database, the family resolved out of it, and the grid it measures.
pub struct Fonts {
	pub context: FontContext,
	pub family: String,
	/// The bundled Nerd Font, under whatever name Fontique filed it as. Read
	/// back rather than hardcoded: the file carries both a typographic family
	/// name and a legacy one, and which of the two ends up in the collection is
	/// Fontique's business, not ours.
	pub symbol_family: String,
	pub size: f32,
	pub metrics: Metrics,
}

impl Fonts {
	/// Resolve a family and measure the cell.
	///
	/// `requested` wins when the system has it. Otherwise the preferred list is
	/// walked, and the bundled face backs the whole thing up — which is why
	/// this returns a `Fonts` rather than an `Option`.
	pub fn resolve(requested: Option<&str>, size: f32, line_height: f32) -> Result<Self> {
		let mut context = FontContext::new();

		// Both are registered unconditionally, not just when nothing else is
		// found. For the plain face that makes `Liberation Mono` nameable from a
		// tape and guarantees the last entry of the preferred list resolves; for
		// the symbol face it is the whole point, since it has to be present to
		// be fallen back to.
		register(&mut context, BUNDLED);
		let symbol_family = register(&mut context, BUNDLED_SYMBOLS)
			.context("registering the bundled symbol font")?;

		let family = requested
			.filter(|name| has_family(&mut context, name))
			.map(str::to_string)
			.or_else(|| {
				PREFERRED
					.iter()
					.find(|name| has_family(&mut context, name))
					.map(|name| name.to_string())
			})
			.unwrap_or_else(|| BUNDLED_FAMILY.to_string());

		let metrics = measure(&mut context, &family, size, line_height)
			.with_context(|| format!("measuring the cell for {family}"))?;

		Ok(Self {
			context,
			family,
			symbol_family,
			size,
			metrics,
		})
	}

	/// The family list shaping is asked to try, most specific first.
	///
	/// Order is the entire mechanism. Fontique walks this list per cluster and
	/// takes the first family that covers the character, so the chosen face
	/// draws everything it can, the symbol face picks up the icons and box
	/// drawing it cannot, and only what neither has reaches generic monospace
	/// and the per-script system walk beyond it. Putting the symbol face last
	/// would be the same list; putting it first would silently restyle every
	/// recording that did not ask for it.
	pub fn family_stack(&self) -> Vec<String> {
		let mut stack = vec![self.family.clone()];
		if self.symbol_family != self.family {
			stack.push(self.symbol_family.clone());
		}
		stack
	}
}

/// Register a font blob and report the family Fontique filed it under.
fn register(context: &mut FontContext, data: &'static [u8]) -> Option<String> {
	let registered = context
		.collection
		.register_fonts(Blob::new(std::sync::Arc::new(data)), None);

	registered
		.first()
		.map(|(id, _)| *id)
		.and_then(|id| family_name(context, id))
}

fn family_name(context: &mut FontContext, id: FamilyId) -> Option<String> {
	context
		.collection
		.family(id)
		.map(|family| family.name().to_string())
}

fn has_family(context: &mut FontContext, name: &str) -> bool {
	context.collection.family_by_name(name).is_some()
}

/// Measure the cell from the very face shaping will select.
///
/// Measuring one file and shaping against another puts every advance off by a
/// fraction of a pixel, so the family is resolved once and measured through the
/// font Fontique actually hands back.
fn measure(
	context: &mut FontContext,
	family: &str,
	size: f32,
	line_height: f32,
) -> Result<Metrics> {
	let family_id = context
		.collection
		.family_by_name(family)
		.ok_or_else(|| anyhow::anyhow!("family {family} vanished between lookup and measure"))?
		.id();

	let font_info = context
		.collection
		.family(family_id)
		.and_then(|family| family.fonts().first().cloned())
		.ok_or_else(|| anyhow::anyhow!("family {family} has no faces"))?;

	let blob = font_info
		.load(Some(&mut context.source_cache))
		.ok_or_else(|| anyhow::anyhow!("the font data for {family} could not be loaded"))?;

	let font = FontRef::from_index(blob.as_ref(), font_info.index())
		.map_err(|error| anyhow::anyhow!("reading {family}: {error}"))?;

	let skrifa: SkrifaMetrics = font.metrics(Size::new(size), LocationRef::default());

	// The advance of a lowercase `m`, falling back to the font's own average.
	// A monospace face gives the same answer for any glyph; asking for a real
	// one rather than trusting `average_width` keeps a proportional face —
	// which a tape is allowed to ask for — from collapsing the grid.
	let advance = font
		.charmap()
		.map('m')
		.and_then(|glyph| {
			font.glyph_metrics(Size::new(size), LocationRef::default())
				.advance_width(glyph)
		})
		.or(skrifa.average_width)
		.filter(|width| *width > 0.0)
		.unwrap_or(size * 0.6);

	let cell_width = advance.ceil().max(1.0) as u32;
	let cell_height = ((skrifa.ascent - skrifa.descent + skrifa.leading) * line_height)
		.ceil()
		.max(1.0) as u32;

	// Centre the face's own line box inside the cell so that raising
	// `line_height` adds space above and below evenly, rather than dropping the
	// text against the top edge.
	let natural = skrifa.ascent - skrifa.descent + skrifa.leading;
	let half_leading = (cell_height as f32 - natural) / 2.0;

	let baseline = half_leading + skrifa.ascent;
	let default_thickness = (size * 0.05).max(1.0);

	// Font decoration offsets are measured up from the baseline, so a positive
	// offset is above it; the grid measures down from the top of the cell.
	let underline = skrifa.underline;
	let strikeout = skrifa.strikeout;

	Ok(Metrics {
		cell_width,
		cell_height,
		baseline,
		underline_offset: baseline - underline.map_or(-size * 0.1, |decoration| decoration.offset),
		underline_thickness: underline
			.map_or(default_thickness, |decoration| decoration.thickness)
			.max(1.0),
		strikeout_offset: baseline - strikeout.map_or(size * 0.25, |decoration| decoration.offset),
		strikeout_thickness: strikeout
			.map_or(default_thickness, |decoration| decoration.thickness)
			.max(1.0),
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn resolution_always_produces_a_usable_grid() {
		let fonts = Fonts::resolve(None, 16.0, 1.0).expect("a family must always resolve");

		assert!(!fonts.family.is_empty());
		assert!(fonts.metrics.cell_width > 0, "a cell needs a width");
		assert!(fonts.metrics.cell_height > 0, "a cell needs a height");
		assert!(
			fonts.metrics.baseline > 0.0
				&& fonts.metrics.baseline < fonts.metrics.cell_height as f32,
			"the baseline must sit inside the cell, got {} in {}",
			fonts.metrics.baseline,
			fonts.metrics.cell_height
		);
	}

	/// The bundled face is registered unconditionally, so naming it works even
	/// on a machine with no fonts installed at all.
	#[test]
	fn the_bundled_family_is_always_available() {
		let fonts = Fonts::resolve(Some(BUNDLED_FAMILY), 16.0, 1.0).unwrap();
		assert_eq!(fonts.family, BUNDLED_FAMILY);
	}

	/// The symbol face must be in the collection on every run, whatever the
	/// machine has installed — it is the only thing standing between a themed
	/// prompt and a row of tofu.
	#[test]
	fn the_symbol_family_is_always_registered() {
		let mut fonts = Fonts::resolve(Some(BUNDLED_FAMILY), 16.0, 1.0).unwrap();
		assert!(!fonts.symbol_family.is_empty());
		assert!(
			has_family(&mut fonts.context, &fonts.symbol_family.clone()),
			"the symbol family {} should be nameable",
			fonts.symbol_family
		);
	}

	/// Fallback order is the mechanism, so assert the order rather than just
	/// the membership: the chosen face draws what it can, and the symbol face
	/// only gets what is left.
	#[test]
	fn the_symbol_face_sits_behind_the_chosen_family() {
		let fonts = Fonts::resolve(Some(BUNDLED_FAMILY), 16.0, 1.0).unwrap();
		let stack = fonts.family_stack();

		assert_eq!(stack.first().map(String::as_str), Some(BUNDLED_FAMILY));
		assert_eq!(stack.last(), Some(&fonts.symbol_family));
	}

	/// A tape may name the symbol face itself. It must not then appear twice —
	/// a duplicated family is a second, pointless coverage probe per cluster.
	#[test]
	fn naming_the_symbol_face_does_not_duplicate_it() {
		let fonts = Fonts::resolve(None, 16.0, 1.0).unwrap();
		let symbol = fonts.symbol_family.clone();
		let chosen = Fonts::resolve(Some(&symbol), 16.0, 1.0).unwrap();

		assert_eq!(chosen.family, symbol);
		assert_eq!(chosen.family_stack(), vec![symbol]);
	}

	#[test]
	fn an_unknown_family_falls_back_rather_than_failing() {
		let fonts = Fonts::resolve(Some("No Such Font Anywhere 12345"), 16.0, 1.0).unwrap();
		assert_ne!(fonts.family, "No Such Font Anywhere 12345");
		assert!(fonts.metrics.cell_width > 0);
	}

	#[test]
	fn a_bigger_size_makes_a_bigger_cell() {
		let small = Fonts::resolve(Some(BUNDLED_FAMILY), 12.0, 1.0).unwrap();
		let large = Fonts::resolve(Some(BUNDLED_FAMILY), 24.0, 1.0).unwrap();

		assert!(large.metrics.cell_width > small.metrics.cell_width);
		assert!(large.metrics.cell_height > small.metrics.cell_height);
	}

	/// Line height stretches the cell without moving the text off centre.
	#[test]
	fn line_height_adds_space_evenly() {
		let tight = Fonts::resolve(Some(BUNDLED_FAMILY), 16.0, 1.0).unwrap();
		let loose = Fonts::resolve(Some(BUNDLED_FAMILY), 16.0, 1.6).unwrap();

		assert!(loose.metrics.cell_height > tight.metrics.cell_height);
		assert_eq!(
			loose.metrics.cell_width, tight.metrics.cell_width,
			"line height must not change the advance"
		);

		let added = (loose.metrics.cell_height - tight.metrics.cell_height) as f32;
		let moved = loose.metrics.baseline - tight.metrics.baseline;
		assert!(
			(moved - added / 2.0).abs() < 1.0,
			"the baseline should move by half the added height, moved {moved} of {added}"
		);
	}
}
