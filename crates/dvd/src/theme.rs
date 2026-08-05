//! Named colour schemes.
//!
//! A theme is the sixteen ANSI colours plus a foreground, a background and a
//! cursor — everything a tape can change about how a recording looks that is
//! not the font. They are folded into `rio-vt`'s own [`Colors`] rather than
//! into a table of our own, because the VT core resolves SGR against that
//! struct when it interns a style: sharing it is what keeps the recording's
//! colours identical to the terminal the tape describes, right down to the
//! 6x6x6 cube that `List::from` derives from these entries.

use anyhow::{Result, bail};
use dvd_render::rio_vt::config::colors::{ColorArray, Colors};

/// One scheme, as the hex strings a theme is normally published in.
pub struct Theme {
	pub name: &'static str,
	pub background: u32,
	pub foreground: u32,
	pub cursor: u32,
	/// Black, red, green, yellow, blue, magenta, cyan, white — then the eight
	/// bright cuts of the same, in the same order.
	pub ansi: [u32; 16],
}

/// The built-in schemes.
///
/// Deliberately short. A tape that wants something else names its colours
/// through the palette the terminal itself sets; this list exists so the common
/// case — "make it look like my editor" — does not require that.
pub const THEMES: &[Theme] = &[
	Theme {
		name: "dracula",
		background: 0x282a36,
		foreground: 0xf8f8f2,
		cursor: 0xf8f8f2,
		ansi: [
			0x21222c, 0xff5555, 0x50fa7b, 0xf1fa8c, 0xbd93f9, 0xff79c6, 0x8be9fd, 0xf8f8f2,
			0x6272a4, 0xff6e6e, 0x69ff94, 0xffffa5, 0xd6acff, 0xff92df, 0xa4ffff, 0xffffff,
		],
	},
	Theme {
		name: "catppuccin-mocha",
		background: 0x1e1e2e,
		foreground: 0xcdd6f4,
		cursor: 0xf5e0dc,
		ansi: [
			0x45475a, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xbac2de,
			0x585b70, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xa6adc8,
		],
	},
	Theme {
		name: "nord",
		background: 0x2e3440,
		foreground: 0xd8dee9,
		cursor: 0xd8dee9,
		ansi: [
			0x3b4252, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x88c0d0, 0xe5e9f0,
			0x4c566a, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x8fbcbb, 0xeceff4,
		],
	},
	Theme {
		name: "gruvbox-dark",
		background: 0x282828,
		foreground: 0xebdbb2,
		cursor: 0xebdbb2,
		ansi: [
			0x282828, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, 0xa89984,
			0x928374, 0xfb4934, 0xb8bb26, 0xfabd2f, 0x83a598, 0xd3869b, 0x8ec07c, 0xebdbb2,
		],
	},
	Theme {
		name: "tokyo-night",
		background: 0x1a1b26,
		foreground: 0xc0caf5,
		cursor: 0xc0caf5,
		ansi: [
			0x15161e, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xa9b1d6,
			0x414868, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xc0caf5,
		],
	},
	Theme {
		name: "solarized-dark",
		background: 0x002b36,
		foreground: 0x839496,
		cursor: 0x93a1a1,
		ansi: [
			0x073642, 0xdc322f, 0x859900, 0xb58900, 0x268bd2, 0xd33682, 0x2aa198, 0xeee8d5,
			0x002b36, 0xcb4b16, 0x586e75, 0x657b83, 0x839496, 0x6c71c4, 0x93a1a1, 0xfdf6e3,
		],
	},
	Theme {
		name: "github-light",
		background: 0xffffff,
		foreground: 0x24292f,
		cursor: 0x24292f,
		ansi: [
			0x24292f, 0xcf222e, 0x116329, 0x4d2d00, 0x0969da, 0x8250df, 0x1b7c83, 0x6e7781,
			0x57606a, 0xa40e26, 0x1a7f37, 0x633c01, 0x218bff, 0xa475f9, 0x3192aa, 0x8c959f,
		],
	},
];

pub fn names() -> impl Iterator<Item = &'static str> {
	THEMES.iter().map(|theme| theme.name)
}

/// Look a theme up by name, listing what is available when it is not.
pub fn resolve(name: &str) -> Result<&'static Theme> {
	let wanted = name.trim().to_lowercase();
	match THEMES.iter().find(|theme| theme.name == wanted) {
		Some(theme) => Ok(theme),
		None => bail!(
			"unknown theme {name:?}. Available: {}",
			names().collect::<Vec<_>>().join(", ")
		),
	}
}

impl Theme {
	/// Fold this scheme into a `Colors`.
	///
	/// Built from the default rather than from scratch so the entries a theme
	/// does not name — the dim cuts, the tab colours — keep working values
	/// instead of becoming transparent black.
	pub fn colors(&self) -> Colors {
		let mut colors = Colors::default();

		let background = channels(self.background);
		colors.background = (
			background,
			dvd_render::rio_graphics::Color {
				r: background[0] as f64,
				g: background[1] as f64,
				b: background[2] as f64,
				a: background[3] as f64,
			},
		);
		colors.foreground = channels(self.foreground);
		colors.cursor = channels(self.cursor);
		colors.vi_cursor = channels(self.cursor);

		let [black, red, green, yellow, blue, magenta, cyan, white, light_black, light_red, light_green, light_yellow, light_blue, light_magenta, light_cyan, light_white] =
			self.ansi.map(channels);

		colors.black = black;
		colors.red = red;
		colors.green = green;
		colors.yellow = yellow;
		colors.blue = blue;
		colors.magenta = magenta;
		colors.cyan = cyan;
		colors.white = white;

		colors.light_black = light_black;
		colors.light_red = light_red;
		colors.light_green = light_green;
		colors.light_yellow = light_yellow;
		colors.light_blue = light_blue;
		colors.light_magenta = light_magenta;
		colors.light_cyan = light_cyan;
		colors.light_white = light_white;

		colors
	}

	/// The background, as the bytes a margin fill wants.
	pub fn background_rgba(&self) -> [u8; 4] {
		[
			(self.background >> 16) as u8,
			(self.background >> 8) as u8,
			self.background as u8,
			0xff,
		]
	}
}

/// `0xRRGGBB` to the normalised quad `rio-vt` stores colours as.
fn channels(hex: u32) -> ColorArray {
	[
		((hex >> 16) & 0xff) as f32 / 255.0,
		((hex >> 8) & 0xff) as f32 / 255.0,
		(hex & 0xff) as f32 / 255.0,
		1.0,
	]
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn every_theme_resolves_by_its_own_name() {
		for theme in THEMES {
			assert_eq!(resolve(theme.name).unwrap().name, theme.name);
		}
	}

	#[test]
	fn lookup_ignores_case_and_surrounding_space() {
		assert_eq!(resolve("  Dracula ").unwrap().name, "dracula");
	}

	#[test]
	fn an_unknown_theme_lists_the_ones_that_exist() {
		let error = resolve("no-such-theme").unwrap_err().to_string();
		assert!(error.contains("no-such-theme"));
		assert!(error.contains("dracula"), "the error should name the options");
	}

	#[test]
	fn hex_reaches_the_colour_table_intact() {
		let colors = resolve("dracula").unwrap().colors();
		// #282a36
		assert!((colors.background.0[0] - 0x28 as f32 / 255.0).abs() < 1e-6);
		assert!((colors.background.0[1] - 0x2a as f32 / 255.0).abs() < 1e-6);
		assert!((colors.background.0[2] - 0x36 as f32 / 255.0).abs() < 1e-6);
		assert_eq!(colors.background.0[3], 1.0);
	}

	#[test]
	fn the_background_bytes_match_the_hex() {
		assert_eq!(
			resolve("dracula").unwrap().background_rgba(),
			[0x28, 0x2a, 0x36, 0xff]
		);
	}
}
