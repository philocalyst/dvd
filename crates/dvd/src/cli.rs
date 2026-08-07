use clap::{Args, Parser, Subcommand};
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

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
/// Three, and each earns its place: `mp4` is the H.264 video, `png` a single
/// still, `svg` a self-contained animation that stays sharp at any size and is
/// selectable as text. The container formats the old list accepted (`mov`,
/// `mkv`, `webm`, and `gif`) all promised muxers or codecs this pipeline does
/// not carry, so naming one produced a file that never appeared.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Output {
	#[default]
	Mp4,
	Png,
	Svg,
}

impl Output {
	pub fn from_extension(extension: &str) -> Option<Self> {
		match extension.to_lowercase().as_str() {
			"mp4" => Some(Self::Mp4),
			"png" => Some(Self::Png),
			"svg" => Some(Self::Svg),
			_ => None,
		}
	}

	pub fn allowed_extensions() -> &'static [&'static str] {
		&["mp4", "png", "svg"]
	}
}

/// Parses an extension, not a whole path — `"mp4"`, never `"video.mp4"` or
/// `".mp4"`. That is what both `clap`'s value parser and the tape's `Output`
/// command have on hand: a `Path::extension()`, already split from the rest
/// of the name.
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

impl fmt::Display for Output {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let name = match self {
			Output::Mp4 => "mp4",
			Output::Png => "png",
			Output::Svg => "svg",
		};
		write!(f, "{name}")
	}
}

fn default_shell() -> String {
	std::env::var("SHELL")
		.unwrap_or_else(|_| "/bin/bash".to_string())
		.split('/')
		.next_back()
		.unwrap_or("bash")
		.to_string()
}

fn validate_output_path(path_str: &str) -> Result<PathBuf, String> {
	let path = PathBuf::from(path_str);

	// Get the extension of the provided path
	let extension = path
		.extension()
		.and_then(|ext| ext.to_str())
		.ok_or_else(|| {
			format!(
				"Output file '{}' must have a valid extension. Allowed extensions: {}",
				path_str,
				Output::allowed_extensions().join(", ")
			)
		})?;

	// Check that provided path extension against the allowed ones.
	Output::from_extension(extension).ok_or_else(|| {
		format!(
			"Unsupported output format '{}'. Allowed extensions: {}",
			extension,
			Output::allowed_extensions().join(", ")
		)
	})?;

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

	/// Create a new tape file by recording your actions
	Record {
		/// Shell for recording
		#[arg(short, long, default_value_t = default_shell())]
		shell: String,
		/// Output file (mp4, png, or svg)
		output: PathBuf,
	},

	/// Play a tape file
	Play {
		/// Files to play (sequentially)
		#[arg(required = true)]
		files: Vec<PathBuf>,
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
		assert_eq!("png".parse(), Ok(Output::Png));
		assert_eq!("svg".parse(), Ok(Output::Svg));
	}

	/// The error names the allowed extensions rather than just rejecting —
	/// it is what a tape author reading `dvd check`'s output sees, so it has
	/// to say what *would* have worked.
	#[test]
	fn output_from_str_names_the_allowed_extensions_when_it_rejects() {
		let error = "gif".parse::<Output>().unwrap_err();
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
		for variant in [Output::Mp4, Output::Png, Output::Svg] {
			assert_eq!(variant.to_string().parse(), Ok(variant));
		}
	}
}
