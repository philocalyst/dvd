//! Commands for replaying and rendering asciicast recordings.
//!
//! Both commands use the renderer crate's source-driven timeline. Playback
//! writes output events to a terminal, while rendering sends the same event
//! stream through the normal raster and sink fanout; there is deliberately no
//! second terminal emulator or frame sampler hiding in the CLI.

use crate::burn::Outputs;
use crate::cli::Output;
use crate::theme;
use anyhow::{Context, Result, bail};
use dvd_render::Level;
use dvd_render::asciicast::AsciicastSource;
use dvd_render::fonts::Fonts;
use dvd_render::grid::GridOptions;
use dvd_render::model::Palette;
use dvd_render::render::{Renderer, Surface};
use dvd_render::source::{EventSource, TerminalEvent};
use dvd_render::timeline::{self, ReplayClock, ResizePolicy, TimelineOptions};
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

const FPS: u32 = 30;

pub fn play(paths: &[PathBuf]) -> Result<()> {
	let stdout = io::stdout();
	let mut output = stdout.lock();
	let mut stdin_used = false;
	for path in paths {
		if path.as_os_str() == "-" {
			if stdin_used {
				bail!("standard input can only be used once");
			}
			stdin_used = true;
		}
		let mut source = open_source(path)?;
		let metadata = source.metadata().clone();
		let mut clock = ReplayClock::new(metadata.idle_time_limit);
		let started = Instant::now();
		while let Some(event) = source
			.next_event()?
			.map(|event| clock.map_event(event))
			.transpose()?
		{
			if let Some(wait) = event.time.checked_sub(started.elapsed()) {
				std::thread::sleep(wait);
			}
			match event.event {
				TerminalEvent::Output(bytes) => output
					.write_all(&bytes)
					.context("writing terminal output")?,
				TerminalEvent::Resize(size) => {
					write!(output, "\x1b[8;{};{}t", size.rows, size.columns)?
				}
				TerminalEvent::Input(_)
				| TerminalEvent::Marker(_)
				| TerminalEvent::Exit(_)
				| TerminalEvent::Unknown { .. } => {}
			}
			output.flush().context("flushing terminal output")?;
		}
		let end = clock.finish(metadata.duration)?;
		if let Some(wait) = end.checked_sub(started.elapsed()) {
			std::thread::sleep(wait);
		}
	}
	Ok(())
}

/// Render an asciicast source through the same timeline used by every source.
pub fn render(recording: &Path, destinations: &[PathBuf]) -> Result<()> {
	let mut source = open_source(recording)?;
	let size = source.metadata().size;
	let outputs = destinations
		.iter()
		.map(|destination| {
			let format = destination
				.extension()
				.and_then(|part| part.to_str())
				.and_then(Output::from_extension)
				.ok_or_else(|| {
					anyhow::anyhow!("{} must be mp4, gif, png, or svg", destination.display())
				})?;
			Ok((destination.clone(), format))
		})
		.collect::<Result<Vec<_>>>()?;
	let level = Level::new();
	let fonts = Fonts::resolve(None, 18.0, 1.0)?;
	let family = fonts.family.clone();
	let font_size = fonts.size;
	let palette = source
		.metadata()
		.theme
		.clone()
		.map(|theme| Palette::from_terminal_theme(&theme))
		.transpose()?
		.unwrap_or(Palette::from_colors(&theme::resolve("dracula")?.colors()));
	let options = GridOptions::default();
	let surface = Surface::default();
	let mut renderer = Renderer::new(
		fonts,
		palette.clone(),
		options,
		surface,
		size.columns,
		size.rows,
		level,
	)?;
	let sinks = Outputs {
		font_family: Some(family),
		font_size,
		line_height: 1.0,
		palette,
		options,
		surface,
		columns: size.columns,
		rows: size.rows,
		level,
	}
	.sinks(&outputs)?;
	timeline::render_source(
		&mut *source,
		&mut renderer,
		sinks,
		TimelineOptions {
			frames_per_second: FPS,
			resize_policy: ResizePolicy::Clip,
		},
	)?;
	for (destination, _) in &outputs {
		println!("wrote {}", destination.display());
	}
	Ok(())
}

fn open_source(path: &Path) -> Result<Box<dyn EventSource>> {
	if path.as_os_str() == "-" {
		return Ok(Box::new(
			AsciicastSource::new(io::stdin())
				.context("opening asciicast stream from standard input")?,
		));
	}
	let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
	Ok(Box::new(AsciicastSource::new(file).with_context(|| {
		format!("parsing asciicast header in {}", path.display())
	})?))
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::io::Cursor;

	#[test]
	fn source_paths_report_invalid_files_with_their_path() {
		let error = match open_source(Path::new("missing.cast")) {
			Ok(_) => panic!("missing source unexpectedly opened"),
			Err(error) => error,
		};
		assert!(error.to_string().contains("missing.cast"));
	}

	#[test]
	fn asciicast_sources_are_streamed_without_path_specific_parsing() {
		let source = AsciicastSource::new(Cursor::new(
			br#"{"version":2,"width":2,"height":1}
[0,"o","ok"]
"#,
		))
		.unwrap();
		assert_eq!(source.metadata().size.columns, 2);
	}
}
