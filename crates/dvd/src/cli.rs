use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;
use std::str::FromStr;
use strum::VariantNames;

#[derive(Parser)]
#[command(name = "dvd")]
#[command(about = "Manage your .dvd or .tape files")]
#[command(version)]
pub struct Cli {
	/// Quiet - do not log messages
	#[arg(short, long, global = true)]
	pub quiet: bool,

	#[command(subcommand)]
	pub command: Commands,
}

/// The formats an output path can name.
///
/// Four, and each earns its place: `mp4` is the H.264 video, `gif` an animated
/// image, `png` a single still, and `svg` a self-contained animation that stays
/// sharp at any size and is selectable as text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, strum::Display, strum::VariantNames)]
#[strum(serialize_all = "lowercase")]
pub enum Output {
	#[default]
	Mp4,
	Gif,
	Png,
	Svg,
}

impl Output {
	/// The lower-cased spellings, in variant order — the single list `Display`
	/// prints, `FromStr` accepts, and error messages recite.
	pub fn allowed_extensions() -> &'static [&'static str] {
		Self::VARIANTS
	}

	pub fn from_extension(extension: &str) -> Option<Self> {
		let lower = extension.to_lowercase();
		[Self::Mp4, Self::Gif, Self::Png, Self::Svg]
			.into_iter()
			.find(|format| format.to_string() == lower)
	}
}

/// Parses an extension, not a whole path — `"mp4"`, never `"video.mp4"` or
/// `".mp4"`. That is what both `clap`'s value parser and the tape's `Output`
/// command have on hand: a `Path::extension()`, already split from the name.
/// Its `Err` names the alternatives, which is what a tape author reading `dvd
/// check`'s output needs — so this stays hand-written rather than using strum's
/// `EnumString`, whose "variant not found" carries none of that.
impl FromStr for Output {
	type Err = String;

	fn from_str(extension: &str) -> Result<Self, Self::Err> {
		Self::from_extension(extension).ok_or_else(|| {
			format!(
				"unsupported output format {extension:?}; use one of {}",
				Self::allowed_extensions().join(", ")
			)
		})
	}
}

fn validate_output_path(path_str: &str) -> Result<PathBuf, String> {
	let path = PathBuf::from(path_str);
	let extension = path
		.extension()
		.and_then(|extension| extension.to_str())
		.ok_or_else(|| {
			format!(
				"Output file '{path_str}' must have a valid extension. Allowed extensions: {}",
				Output::allowed_extensions().join(", ")
			)
		})?;
	extension.parse::<Output>()?;
	Ok(path)
}

#[derive(Subcommand)]
pub enum Commands {
	/// List all the available themes, one per line
	Themes {
		/// Output as markdown
		#[arg(long, hide = true)]
		markdown: bool,
	},

	Burn(BurnArgs),

	/// Replay asciicast files in the controlling terminal
	Play {
		/// Asciicast files to play sequentially (`-` reads standard input)
		#[arg(required = true)]
		files: Vec<PathBuf>,
	},

	/// Render an asciicast file after it has been captured
	Render {
		/// Asciicast source to render (`-` reads standard input)
		recording: PathBuf,
		/// One or more output files (mp4, gif, png, or svg)
		#[arg(required = true, value_parser = validate_output_path)]
		outputs: Vec<PathBuf>,
	},

	/// Create a new tape file with example tape file contents and documentation
	New {
		/// Name of the new tape file
		name: String,
	},

	/// Validate a glob file path and parses all the files to ensure they are valid without running them
	Check {
		/// Files to validate
		#[arg(required = true)]
		files: Vec<PathBuf>,
	},
}

#[derive(Args)]
pub struct BurnArgs {
	/// Input tape file (use "-" for stdin)
	pub input_file: PathBuf,

	/// File name(s) of video output
	#[arg(
		value_parser = validate_output_path,
		value_hint = clap::ValueHint::FilePath
	)]
	pub output_file: PathBuf,
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn output_from_str_accepts_every_allowed_extension_case_insensitively() {
		assert_eq!("mp4".parse(), Ok(Output::Mp4));
		assert_eq!("MP4".parse(), Ok(Output::Mp4));
		assert_eq!("gif".parse(), Ok(Output::Gif));
		assert_eq!("png".parse(), Ok(Output::Png));
		assert_eq!("svg".parse(), Ok(Output::Svg));
	}

	/// The error names the allowed extensions rather than just rejecting —
	/// it is what a tape author reading `dvd check`'s output sees, so it has
	/// to say what *would* have worked.
	#[test]
	fn output_from_str_names_the_allowed_extensions_when_it_rejects() {
		let error = "webm".parse::<Output>().unwrap_err();
		for extension in Output::allowed_extensions() {
			assert!(
				error.contains(extension),
				"error {error:?} should mention {extension:?}"
			);
		}
	}

	/// `FromStr` is built on `from_extension`, and `Display` is the format
	/// `from_extension` expects back — this is what keeps a round trip
	/// through the CLI (`--output` reparsed from a printed default) honest.
	#[test]
	fn every_output_variant_round_trips_through_display_and_from_str() {
		for variant in [Output::Mp4, Output::Gif, Output::Png, Output::Svg] {
			assert_eq!(variant.to_string().parse(), Ok(variant));
		}
	}

	#[test]
	fn render_requires_a_supported_output_extension() {
		let error = Cli::try_parse_from(["dvd", "render", "session.cast", "video.webm"])
			.err()
			.expect("an unsupported extension should be rejected")
			.to_string();
		assert!(error.contains("unsupported output format"));
	}

	#[test]
	fn render_accepts_multiple_output_destinations() {
		let cli = Cli::try_parse_from([
			"dvd",
			"render",
			"session.cast",
			"terminal.svg",
			"terminal.gif",
			"terminal.mp4",
		])
		.expect("render should accept one or more output paths");

		match cli.command {
			Commands::Render { recording, outputs } => {
				assert_eq!(recording, PathBuf::from("session.cast"));
				assert_eq!(outputs.len(), 3);
				assert_eq!(outputs[1], PathBuf::from("terminal.gif"));
			}
			_ => panic!("expected render command"),
		}
	}
}
