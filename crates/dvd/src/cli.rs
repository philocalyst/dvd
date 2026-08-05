use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Output {
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
