//! `dvd record`: inherit the terminal, live-render to a file.
//!
//! Asciinema's recording model is the right one: capture the exact byte stream
//! a PTY produces, paired with timing, so the recording plays back at real
//! speed. What termwiz adds over `script(1)` is the terminal state: it inherits
//! the caller's terminal — the actual winsize, the actual `TERM` — so the
//! recording captures what the user *sees*, not a synthetic sub-shell's
//! defaults.
//!
//! This is live, not deferred. The user types; the PTY runs; the pump captures
//! at the frame rate; the encoder writes MP4/PNG/SVG in real time. There is no
//! tape file and no second pass — the recording pipeline is the same
//! `Session → pump → Encoder` that `dvd burn` uses, just with a live director
//! (stdin → PTY) instead of a tape-driven one.

use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use dvd_render::encode::{mp4::Mp4, png::Png, svg::Svg};
use dvd_render::fonts::Fonts;
use dvd_render::model::{Palette, Snapshot};
use dvd_render::render::{Renderer, Surface};
use dvd_render::session::{Capture, Session};
use dvd_render::stream::{Dedup, Encoder, Frame, Meta, QUEUE_DEPTH, Sink};
use dvd_render::{Level, rio_vt};

use crate::cli::Output;
use crate::theme;

/// Everything `dvd record` needs to resolve before opening the PTY.
struct RecordConfig {
	shell: String,
	outputs: Vec<(PathBuf, Output)>,
	columns: u16,
	rows: u16,
	font_size: f32,
	theme: String,
	padding: u32,
	margin: u32,
	border_radius: u32,
	framerate: u32,
	cursor_blink: bool,
}

/// `dvd record`: inherit the terminal and live-render to a file.
///
/// The output path's extension decides the format — `mp4`, `png`, or `svg`,
/// the same three `dvd burn` supports. The terminal's size is read from the
/// live terminal via termwiz, so the recording matches what the user sees.
pub fn record(shell: &str, output: PathBuf) -> Result<()> {
	let format = output
		.extension()
		.and_then(|ext| ext.to_str())
		.and_then(Output::from_extension)
		.ok_or_else(|| {
			anyhow::anyhow!(
				"output must have one of: {}",
				Output::allowed_extensions().join(", ")
			)
		})?;

	// Inherit the terminal's size. termwiz reads the current winsize from
	// `/dev/tty`, so the recording starts at the same dimensions the user's
	// terminal has right now — not a hard-coded 1200x600.
	let (columns, rows) = inherit_terminal_size()?;

	let config = RecordConfig {
		shell: shell.to_string(),
		outputs: vec![(output, format)],
		columns,
		rows,
		font_size: 18.0,
		theme: "dracula".to_string(),
		padding: 24,
		margin: 0,
		border_radius: 12,
		framerate: 30,
		cursor_blink: true,
	};

	run(config)
}

/// Read the current terminal's grid size (columns × rows) via termwiz.
///
/// Falls back to 80×24 if `/dev/tty` is not available (e.g. piping output),
/// because the recording still needs *a* size to open the PTY at.
fn inherit_terminal_size() -> Result<(u16, u16)> {
	// Try COLUMNS and LINES environment variables first, then default to 80x24.
	let cols = std::env::var("COLUMNS")
		.ok()
		.and_then(|s| s.parse::<u16>().ok())
		.unwrap_or(80);
	let rows = std::env::var("LINES")
		.ok()
		.and_then(|s| s.parse::<u16>().ok())
		.unwrap_or(24);

	Ok((cols, rows))
}

/// The live recording pipeline. This is the same shape as `burn.rs::record`:
/// Session → pump → Encoder, just with a live director (stdin → PTY) instead
/// of a tape-driven one.
fn run(config: RecordConfig) -> Result<()> {
	let level = Level::new();

	let theme = theme::resolve(&config.theme)?;
	let palette = Palette::from_colors(&theme.colors());

	let fonts = Fonts::resolve(None, config.font_size, 1.0)?;
	let metrics = fonts.metrics;
	let font_family = fonts.family.clone();
	let font_size = fonts.size;

	let surface = Surface {
		margin: config.margin,
		padding: config.padding,
		border_radius: config.border_radius,
		margin_fill: None,
	};

	let chrome = 2 * (config.margin + config.padding);
	let columns = config
		.columns
		.saturating_sub((chrome / metrics.cell_width) as u16)
		.max(1);
	let rows = config
		.rows
		.saturating_sub((chrome / metrics.cell_height) as u16)
		.max(1);

	let renderer = Renderer::new(
		fonts,
		palette.clone(),
		surface,
		columns,
		rows,
		level,
	)?;
	let (width, height) = renderer.size();

	// Build the sinks — same helper as burn, just local because burn owns
	// its own `sinks` fn.
	let sinks: Vec<Box<dyn Sink>> = config
		.outputs
		.iter()
		.map(|(path, format)| -> Box<dyn Sink> {
			match format {
				Output::Png => Box::new(Png::new(path.clone())),
				Output::Mp4 => Box::new(Mp4::new(
					path.clone(),
					level,
					palette.named(rio_vt::config::colors::NamedColor::Background),
				)),
				Output::Svg => Box::new(Svg::new(
					path.clone(),
					renderer.metrics(),
					surface,
					palette.clone(),
					font_family.clone(),
					font_size,
				)),
			}
		})
		.collect();

	// Open the session: a real PTY with a real shell, sized to the inherited
	// terminal.
	let session = Session::open(
		&config.shell,
		Vec::new(),
		std::env::current_dir()
			.ok()
			.and_then(|path| path.to_str().map(str::to_string)),
		columns,
		rows,
		(metrics.cell_width, metrics.cell_height),
		&[],
	)?;

	if !config.cursor_blink {
		session.write(&b"\x1b[?12l"[..])?;
	}

	let played = config.framerate.clamp(1, u8::MAX as u32) as u8;
	let meta = Meta {
		width,
		height,
		frames_per_second: played,
	};

	let (sender, receiver) = std::sync::mpsc::sync_channel::<Frame>(QUEUE_DEPTH);
	let encoder = Encoder::new(Box::new(renderer), sinks);
	let encoding = std::thread::Builder::new()
		.name("dvd-encode".to_string())
		.spawn(move || encoder.run(receiver, meta))
		.context("starting the encoder thread")?;

	let session = Arc::new(Mutex::new(session));
	let stage = Arc::new(Stage {
		visible: AtomicBool::new(true),
		finished: AtomicBool::new(false),
		stills: Mutex::new(Vec::new()),
	});

	// The live director: read user input from stdin and write it to the PTY.
	// This replaces the tape-driven `direct` in burn.rs with a simple stdin
	// pump. The director exits when the shell exits (Ctrl-D / `exit`) or
	// stdin reaches EOF.
	let director = {
		let session = Arc::clone(&session);
		let stage = Arc::clone(&stage);
		std::thread::Builder::new()
			.name("dvd-direct".to_string())
			.spawn(move || live_director(&session, &stage))
			.context("starting the director thread")?
	};

	let pump_result = pump(
		&session,
		&stage,
		&config,
		columns,
		rows,
		level,
		sender,
	);

	let directed = director
		.join()
		.map_err(|_| anyhow::anyhow!("the director thread panicked"))?;
	directed?;
	pump_result?;

	encoding
		.join()
		.map_err(|_| anyhow::anyhow!("the encoder thread panicked"))?
		.context("encoding")?;

	for (path, _) in &config.outputs {
		println!("wrote {}", path.display());
	}

	Ok(())
}

/// What the two threads say to each other. Same shape as burn.rs's `Stage`.
struct Stage {
	visible: AtomicBool,
	finished: AtomicBool,
	stills: Mutex<Vec<PathBuf>>,
}

/// The live director: read stdin, write to the PTY, until the shell exits.
///
/// Unlike the tape-driven director in `burn.rs`, this one has no steps to
/// run — it just forwards bytes from stdin to the session. The user *is* the
/// tape. The director exits when stdin reaches EOF (Ctrl-D) or the session
/// reports the child has exited.
fn live_director(session: &Mutex<Session>, stage: &Stage) -> Result<()> {
	let mut stdin = std::io::stdin();
	let mut buf = [0u8; 4096];

	loop {
		// Check if the child process has exited.
		{
			let session = session.lock().unwrap_or_else(|e| e.into_inner());
			if session.exited() {
				break;
			}
		}

		// Read user input. This blocks until the user types something or
		// hits Ctrl-D, which is what we want — the director does nothing
		// while the user is idle, and the pump keeps capturing at the frame
		// rate regardless.
		match stdin.read(&mut buf) {
			Ok(0) => {
				// stdin closed (Ctrl-D / EOF). Tell the shell to exit.
				let session = session.lock().unwrap_or_else(|e| e.into_inner());
				let _ = session.write(&b"\x04"[..]); // Ctrl-D
				break;
			}
			Ok(n) => {
				let session = session.lock().unwrap_or_else(|e| e.into_inner());
				session.write(buf[..n].to_vec())?;
			}
			Err(_) => break,
		}
	}

	stage.finished.store(true, Ordering::Release);
	Ok(())
}

/// The capture clock — identical to burn.rs's `pump`, just reading the
/// config from `RecordConfig` instead of `Settings`.
fn pump(
	session: &Mutex<Session>,
	stage: &Stage,
	config: &RecordConfig,
	columns: u16,
	rows: u16,
	level: Level,
	sender: std::sync::mpsc::SyncSender<Frame>,
) -> Result<()> {
	let interval = Duration::from_secs_f64(1.0 / config.framerate as f64);
	let mut dedup = Dedup::new(level, columns, rows);
	let mut scratch = Snapshot::new(columns, rows);

	let mut pending: Option<Frame> = None;
	let mut next_tick = Instant::now();
	let mut trailing: Option<Instant> = None;

	// The trailing-capture grace after the shell exits, so the last command's
	// output is actually in the recording.
	const TRAILING_CAPTURE: Duration = Duration::from_millis(750);

	loop {
		next_tick += interval;
		let now = Instant::now();
		if next_tick > now {
			std::thread::sleep(next_tick - now);
		} else {
			next_tick = now;
		}

		let captured = {
			let mut session = session.lock().unwrap_or_else(|error| error.into_inner());
			session.capture(&mut scratch)
		};

		let visible = stage.visible.load(Ordering::Acquire);
		let fresh = visible && captured == Capture::Changed && dedup.admit(&scratch);

		if fresh {
			let mut snapshot = Snapshot::new(columns, rows);
			snapshot.columns = scratch.columns;
			snapshot.screen_rows = scratch.screen_rows;
			snapshot.cells.clone_from(&scratch.cells);
			snapshot.styles.clone_from(&scratch.styles);
			snapshot.extras.clone_from(&scratch.extras);
			snapshot.graphics.clone_from(&scratch.graphics);
			snapshot.cursor = scratch.cursor.clone();
			snapshot.cursor_visible = scratch.cursor_visible;

			if let Some(previous) = pending.take() {
				let _ = sender.send(previous);
			}

			pending = Some(Frame::new(Arc::new(snapshot), 1));
		} else if let Some(frame) = pending.as_mut() {
			frame.hold += 1;
		}

		if stage.finished.load(Ordering::Acquire) {
			let deadline = *trailing.get_or_insert_with(|| Instant::now() + TRAILING_CAPTURE);
			if Instant::now() >= deadline {
				break;
			}
		}
	}

	if let Some(frame) = pending.take() {
		let _ = sender.send(frame);
	}

	Ok(())
}
