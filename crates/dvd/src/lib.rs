//! `dvd`: the tape language, the CLI, and the pipeline that drives one against
//! a terminal. Persisted session capture belongs to asciinema; DVD consumes
//! asciicast streams for playback and rendering.
//!
//! Everything that touches a PTY, a glyph or an encoder lives in `dvd-render`.
//! This crate reads a tape, turns it into commands, and feeds them to that
//! engine.

pub mod cli;
pub mod tape;

mod burn;
mod replay;
mod theme;

use std::process::ExitCode;

pub fn run(cli: cli::Cli) -> ExitCode {
	let result = match &cli.command {
		cli::Commands::Check { files } => check(files),
		cli::Commands::Burn(args) => burn::burn(args),
		cli::Commands::Themes { markdown } => themes(*markdown),
		cli::Commands::Play { files } => replay::play(files),
		cli::Commands::Render { recording, output } => replay::render(recording, output),
		cli::Commands::New { .. } => Err(anyhow::anyhow!("new is not yet implemented")),
	};

	match result {
		Ok(()) => ExitCode::SUCCESS,
		Err(error) => {
			eprintln!("error: {error:#}");
			ExitCode::FAILURE
		}
	}
}

/// `dvd check <files...>`: parse-only validation.
///
/// Every file is checked even after one fails, so a single run reports every
/// broken tape rather than stopping at the first.
fn check(files: &[std::path::PathBuf]) -> anyhow::Result<()> {
	let mut failed = 0usize;

	for file in files {
		let source = std::fs::read_to_string(file)
			.map_err(|error| anyhow::anyhow!("reading {}: {error}", file.display()))?;
		let (_commands, errors) = tape::parse(&source);

		if errors.is_empty() {
			println!("ok    {}", file.display());
		} else {
			failed += 1;
			for error in &errors {
				eprintln!("error {}:{}", file.display(), error);
			}
		}
	}

	if failed > 0 {
		anyhow::bail!("{failed} file(s) failed validation");
	}
	Ok(())
}

/// `dvd themes`: list the built-in colour schemes, one per line.
fn themes(markdown: bool) -> anyhow::Result<()> {
	if markdown {
		println!("| name |");
		println!("| --- |");
		for name in theme::names() {
			println!("| {name} |");
		}
	} else {
		for name in theme::names() {
			println!("{name}");
		}
	}
	Ok(())
}
