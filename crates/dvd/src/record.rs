//! `dvd record`: persist an interactive PTY before displaying its output.
//!
//! The recorder has one durability invariant: every PTY-output chunk reaches
//! the append-only journal before it reaches the user's terminal.  Rendering
//! and playback consume that journal later; neither belongs on this path.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use anyhow::{Context, Result};
use dvd_render::recording::{Event, Geometry, RecordingHeader, RecordingWriter, TimedEvent};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use termwiz::terminal::Terminal as _;

/// Run `shell` interactively and save the source stream to `recording`.
///
/// Typed input is intentionally omitted unless `capture_input` is set: a
/// child can disable echo while receiving a password, but its output remains
/// necessary to replay the session either way.
pub fn record(shell: &str, recording: PathBuf, capture_input: bool) -> Result<()> {
	let mut terminal = RawTerminal::open()?;
	let geometry = terminal.geometry()?;
	let journal = Arc::new(Mutex::new(Journal::create(
		&recording,
		RecordingHeader {
			geometry,
			terminal: std::env::var("TERM").ok(),
			title: None,
		},
	)?));

	let pair = native_pty_system()
		.openpty(pty_size(geometry))
		.with_context(|| format!("opening a PTY for {shell}"))?;
	let mut command = CommandBuilder::new(crate::burn::absolute_shell(shell));
	command.cwd(std::env::current_dir().context("reading the current directory")?);
	let mut child = pair
		.slave
		.spawn_command(command)
		.with_context(|| format!("starting {shell}"))?;
	drop(pair.slave);

	let reader = pair
		.master
		.try_clone_reader()
		.context("cloning the PTY reader")?;
	let writer = Arc::new(Mutex::new(
		pair.master
			.take_writer()
			.context("opening the PTY writer")?,
	));
	let stopped = Arc::new(AtomicBool::new(false));
	forward_input(
		writer,
		Arc::clone(&journal),
		Arc::clone(&stopped),
		capture_input,
	)?;

	let output = copy_output(reader, Arc::clone(&journal));
	stopped.store(true, Ordering::Release);
	if output.is_err() {
		let _ = child.kill();
	}
	let status = child.wait().context("waiting for the shell")?;
	if output.is_ok() {
		journal
			.lock()
			.unwrap_or_else(|error| error.into_inner())
			.append(Event::Exit(i32::try_from(status.exit_code()).ok()))?;
	}
	journal
		.lock()
		.unwrap_or_else(|error| error.into_inner())
		.finish()?;
	drop(terminal);
	output
}

/// termwiz owns the saved terminal mode and restores it when this guard falls
/// out of scope; the explicit call keeps the ordinary return path obvious.
struct RawTerminal {
	inner: termwiz::terminal::SystemTerminal,
}

impl RawTerminal {
	fn open() -> Result<Self> {
		let capabilities =
			termwiz::caps::Capabilities::new_from_env().context("reading terminal capabilities")?;
		let mut inner = termwiz::terminal::SystemTerminal::new(capabilities)
			.context("opening the controlling terminal")?;
		inner.set_raw_mode().context("enabling raw terminal mode")?;
		Ok(Self { inner })
	}

	fn geometry(&mut self) -> Result<Geometry> {
		let size = self
			.inner
			.get_screen_size()
			.context("reading terminal size")?;
		Ok(Geometry {
			columns: u16::try_from(if size.cols == 0 { 80 } else { size.cols })
				.context("terminal has too many columns")?,
			rows: u16::try_from(if size.rows == 0 { 24 } else { size.rows })
				.context("terminal has too many rows")?,
			pixel_width: u32::try_from(size.xpixel).unwrap_or(u32::MAX),
			pixel_height: u32::try_from(size.ypixel).unwrap_or(u32::MAX),
		})
	}
}

impl Drop for RawTerminal {
	fn drop(&mut self) {
		let _ = self.inner.set_cooked_mode();
	}
}

struct Journal {
	writer: RecordingWriter,
	started: Instant,
}

impl Journal {
	fn create(path: &PathBuf, header: RecordingHeader) -> Result<Self> {
		Ok(Self {
			writer: RecordingWriter::create(path, header)
				.with_context(|| format!("creating recording {}", path.display()))?,
			started: Instant::now(),
		})
	}

	fn append(&mut self, event: Event) -> Result<()> {
		let timestamp_ns = self
			.started
			.elapsed()
			.as_nanos()
			.try_into()
			.unwrap_or(u64::MAX);
		self.writer
			.append(TimedEvent {
				timestamp_ns,
				event,
			})
			.context("appending recording event")
	}

	fn finish(&mut self) -> Result<()> {
		self.writer.finish().context("finalising recording")
	}
}

fn forward_input(
	writer: Arc<Mutex<Box<dyn Write + Send>>>,
	journal: Arc<Mutex<Journal>>,
	stopped: Arc<AtomicBool>,
	capture_input: bool,
) -> Result<()> {
	thread::Builder::new()
		.name("dvd-record-input".to_string())
		.spawn(move || {
			let mut input = match OpenOptions::new().read(true).open("/dev/tty") {
				Ok(input) => input,
				Err(_) => return,
			};
			let mut bytes = [0; 8192];
			while !stopped.load(Ordering::Acquire) {
				let count = match input.read(&mut bytes) {
					Ok(count) => count,
					Err(_) => return,
				};
				if count == 0 || stopped.load(Ordering::Acquire) {
					return;
				}
				if writer
					.lock()
					.unwrap_or_else(|error| error.into_inner())
					.write_all(&bytes[..count])
					.is_err()
				{
					return;
				}
				if capture_input
					&& journal
						.lock()
						.unwrap_or_else(|error| error.into_inner())
						.append(Event::Input(bytes[..count].to_vec()))
						.is_err()
				{
					return;
				}
			}
		})
		.context("starting the terminal input reader")?;
	Ok(())
}

fn copy_output(mut reader: Box<dyn Read + Send>, journal: Arc<Mutex<Journal>>) -> Result<()> {
	let mut terminal = OpenOptions::new()
		.write(true)
		.open("/dev/tty")
		.context("opening the controlling terminal for output")?;
	let mut bytes = [0; 8192];
	loop {
		let count = reader.read(&mut bytes).context("reading PTY output")?;
		if count == 0 {
			return terminal.flush().context("flushing terminal output");
		}
		journal
			.lock()
			.unwrap_or_else(|error| error.into_inner())
			.append(Event::Output(bytes[..count].to_vec()))?;
		terminal
			.write_all(&bytes[..count])
			.context("writing PTY output to the terminal")?;
	}
}

fn pty_size(geometry: Geometry) -> PtySize {
	PtySize {
		rows: geometry.rows,
		cols: geometry.columns,
		pixel_width: u16::try_from(geometry.pixel_width).unwrap_or(u16::MAX),
		pixel_height: u16::try_from(geometry.pixel_height).unwrap_or(u16::MAX),
	}
}
