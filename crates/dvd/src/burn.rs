//! Running a tape against a real terminal.
//!
//! Two threads, and the split is the whole design. A *director* executes the
//! tape — typing, sleeping, pressing keys — against the PTY, and a *pump*
//! captures the screen on a fixed clock and feeds distinct frames to the
//! encoder. They share the session behind a mutex and touch it for microseconds
//! at a time.
//!
//! It has to be two, because the tape's timing is real time. `Sleep 2s` means
//! the child process runs unobserved for two seconds; if the same thread that
//! sleeps were also the one capturing, those two seconds would produce no
//! frames and the recording would jump. Equally, if the capture clock had to
//! wait on a rasterization, the child would keep running while the recorder
//! was not looking and the recording would drift away from the script.
//!
//! ```text
//!   director ──writes──▶ pty ──▶ vt state ◀──captures── pump ──▶ dedup ──▶ encoder thread
//!   (tape time)                                       (frame clock)
//! ```

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use regex::Regex;

use dvd_render::encode::{mp4::Mp4, png::Png, svg::Svg};
use dvd_render::fonts::Fonts;
use dvd_render::model::{Palette, Snapshot};
use dvd_render::render::{Renderer, Surface};
use dvd_render::session::{Capture, Session};
use dvd_render::stream::{Dedup, Encoder, Frame, Meta, QUEUE_DEPTH, Sink};
use dvd_render::{Level, rio_vt};

use crate::cli::{BurnArgs, Output};
use crate::tape::{self, Commands, Setting, TokenType, WaitMode};
use crate::theme;

/// How long the pump keeps capturing after the last command, so the terminal's
/// answer to that command is actually in the recording.
///
/// Without it a tape ending in `Enter` records the keystroke and stops before
/// the shell has printed anything, which looks exactly like a broken recorder.
const TRAILING_CAPTURE: Duration = Duration::from_millis(750);

/// Everything a `Set` can change, resolved.
#[derive(Debug, Clone)]
pub struct Settings {
	pub shell: String,
	pub font_family: Option<String>,
	pub font_size: f32,
	pub line_height: f32,
	pub width: u32,
	pub height: u32,
	pub padding: u32,
	pub margin: u32,
	pub margin_fill: Option<String>,
	pub border_radius: u32,
	pub framerate: u32,
	pub playback_speed: f32,
	pub typing_speed: Duration,
	pub theme: String,
	pub wait_timeout: Duration,
	pub wait_pattern: String,
	pub cursor_blink: bool,
}

impl Default for Settings {
	fn default() -> Self {
		Self {
			shell: default_shell(),
			font_family: None,
			font_size: 22.0,
			line_height: 1.0,
			width: 1200,
			height: 600,
			padding: 60,
			margin: 0,
			margin_fill: None,
			border_radius: 0,
			framerate: 50,
			playback_speed: 1.0,
			typing_speed: Duration::from_millis(50),
			theme: "dracula".to_string(),
			wait_timeout: Duration::from_secs(30),
			// The shell prompt, roughly: a line that has stopped producing
			// output and is waiting. Matches what `Wait` means with no pattern.
			wait_pattern: r"[\$>❯#]\s*$".to_string(),
			cursor_blink: false,
		}
	}
}

fn default_shell() -> String {
	std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string())
}

impl Settings {
	fn apply(&mut self, setting: &Setting) {
		match setting {
			Setting::Shell(value) => self.shell = absolute_shell(value),
			Setting::FontFamily(value) => self.font_family = Some(value.clone()),
			Setting::FontSize(value) => self.font_size = *value as f32,
			Setting::LineHeight(value) => self.line_height = *value,
			Setting::Width(value) => self.width = *value,
			Setting::Height(value) => self.height = *value,
			Setting::Padding(value) => self.padding = *value,
			Setting::Margin(value) => self.margin = *value,
			Setting::MarginFill(value) => self.margin_fill = Some(value.clone()),
			Setting::BorderRadius(value) => self.border_radius = *value,
			Setting::Framerate(value) => self.framerate = (*value).max(1),
			Setting::PlaybackSpeed(value) => {
				if *value > 0.0 {
					self.playback_speed = *value;
				}
			}
			Setting::TypingSpeed(value) => self.typing_speed = *value,
			Setting::Theme(value) => self.theme = value.clone(),
			Setting::WaitTimeout(value) => self.wait_timeout = *value,
			Setting::WaitPattern(value) => self.wait_pattern = value.clone(),
			Setting::CursorBlink(value) => self.cursor_blink = *value,
			// Accepted by the language, with nothing behind them here: letter
			// spacing would break the fixed advance the whole grid is built on,
			// and the window bar and loop offset are chrome this renderer does
			// not draw. Silently ignoring beats refusing a tape that carries one.
			Setting::LetterSpacing(_)
			| Setting::LoopOffset(_)
			| Setting::WindowBar(_)
			| Setting::WindowBarSize(_) => {}
		}
	}
}

/// A bare shell name has to become a path, because the PTY spawns the program
/// directly rather than going through a shell that would search `PATH`.
fn absolute_shell(name: &str) -> String {
	if name.contains('/') {
		return name.to_string();
	}
	which(name).unwrap_or_else(|| format!("/bin/{name}"))
}

fn which(program: &str) -> Option<String> {
	std::env::var_os("PATH")?
		.to_str()?
		.split(':')
		.map(|directory| Path::new(directory).join(program))
		.find(|candidate| candidate.is_file())
		.and_then(|path| path.to_str().map(str::to_string))
}

/// `#rrggbb`, `#rgb`, or a bare hex triple.
fn parse_color(value: &str) -> Result<[u8; 4]> {
	let hex = value.trim().trim_start_matches('#');
	let expand = |slice: &str| u8::from_str_radix(slice, 16);

	let (r, g, b) = match hex.len() {
		3 => {
			let mut nibble = hex.chars().map(|c| {
				u8::from_str_radix(&c.to_string(), 16).map(|value| value * 17)
			});
			(
				nibble.next().transpose()?,
				nibble.next().transpose()?,
				nibble.next().transpose()?,
			)
		}
		6 => (
			expand(&hex[0..2]).ok(),
			expand(&hex[2..4]).ok(),
			expand(&hex[4..6]).ok(),
		),
		_ => (None, None, None),
	};

	match (r, g, b) {
		(Some(r), Some(g), Some(b)) => Ok([r, g, b, 0xff]),
		_ => bail!("{value:?} is not a colour; use #rgb or #rrggbb"),
	}
}

/// The commands, minus the ones that only configure the recording.
struct Plan {
	settings: Settings,
	environment: Vec<(String, String)>,
	outputs: Vec<PathBuf>,
	steps: Vec<Commands>,
}

/// Split a parsed tape into "what the recording is" and "what it does".
///
/// `Set`, `Env`, `Output` and `Require` all have to be resolved before the PTY
/// exists — the first two decide how it is opened, and the last two decide
/// whether opening it is worth doing at all.
fn plan(commands: Vec<Commands>) -> Result<Plan> {
	let mut plan = Plan {
		settings: Settings::default(),
		environment: Vec::new(),
		outputs: Vec::new(),
		steps: Vec::new(),
	};

	for command in commands {
		match command {
			Commands::Set(set) => plan.settings.apply(&set.setting),
			Commands::Env(env) => plan.environment.push((env.variable, env.value)),
			Commands::Output(output) => plan.outputs.push(output.path),
			Commands::Require(require) => {
				if which(&require.program).is_none() {
					bail!(
						"this tape requires {:?}, which is not on PATH",
						require.program
					);
				}
			}
			other => plan.steps.push(other),
		}
	}

	Ok(plan)
}

/// Build the sinks for a set of output paths.
fn sinks(
	paths: &[PathBuf],
	renderer: &Renderer,
	fonts: &Fonts,
	palette: &Palette,
	surface: Surface,
	level: Level,
) -> Result<Vec<Box<dyn Sink>>> {
	let mut sinks: Vec<Box<dyn Sink>> = Vec::new();

	for path in paths {
		let extension = path
			.extension()
			.and_then(|extension| extension.to_str())
			.unwrap_or_default();

		let format = Output::from_extension(extension).with_context(|| {
			format!(
				"{} has no output format; use one of {}",
				path.display(),
				Output::allowed_extensions().join(", ")
			)
		})?;

		sinks.push(match format {
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
				fonts.family.clone(),
				fonts.size,
			)),
		});
	}

	Ok(sinks)
}

/// `dvd burn`: run a tape and write every output it names.
pub fn burn(args: &BurnArgs) -> Result<()> {
	let source = if args.input_file.as_os_str() == "-" {
		std::io::read_to_string(std::io::stdin()).context("reading the tape from stdin")?
	} else {
		std::fs::read_to_string(&args.input_file)
			.with_context(|| format!("reading {}", args.input_file.display()))?
	};

	let (commands, errors) = tape::parse(&source);
	if !errors.is_empty() {
		for error in &errors {
			eprintln!("error {}:{}", args.input_file.display(), error);
		}
		bail!("{} did not parse", args.input_file.display());
	}

	let mut plan = plan(commands)?;

	// The path on the command line is an output like any other, and it is the
	// one the user typed most recently, so it goes last.
	if !plan.outputs.contains(&args.output_file) {
		plan.outputs.push(args.output_file.clone());
	}

	record(plan)
}

fn record(plan: Plan) -> Result<()> {
	let Plan {
		settings,
		environment,
		outputs,
		steps,
	} = plan;

	// One CPU probe for the process, shared with `vello_cpu`'s rasterizer and
	// with our own kernels.
	let level = Level::new();

	let theme = theme::resolve(&settings.theme)?;
	let palette = Palette::from_colors(&theme.colors());

	let fonts = Fonts::resolve(
		settings.font_family.as_deref(),
		settings.font_size,
		settings.line_height,
	)?;
	let metrics = fonts.metrics;

	let surface = Surface {
		margin: settings.margin,
		padding: settings.padding,
		border_radius: settings.border_radius,
		margin_fill: settings
			.margin_fill
			.as_deref()
			.map(parse_color)
			.transpose()?,
	};

	// The tape gives a canvas size in pixels; the terminal needs a grid. The
	// chrome comes off first, and what is left divides into whole cells —
	// a partial column would be a cell the terminal could write into and the
	// canvas could not show.
	let chrome = 2 * (settings.margin + settings.padding);
	let columns = settings
		.width
		.saturating_sub(chrome)
		.checked_div(metrics.cell_width)
		.unwrap_or(0)
		.clamp(1, u16::MAX as u32) as u16;
	let rows = settings
		.height
		.saturating_sub(chrome)
		.checked_div(metrics.cell_height)
		.unwrap_or(0)
		.clamp(1, u16::MAX as u32) as u16;

	let renderer = Renderer::new(fonts, palette.clone(), surface, columns, rows, level)?;
	let (width, height) = renderer.size();

	let fonts_for_svg = Fonts::resolve(
		settings.font_family.as_deref(),
		settings.font_size,
		settings.line_height,
	)?;
	let sinks = sinks(&outputs, &renderer, &fonts_for_svg, &palette, surface, level)?;

	let mut session = Session::open(
		&settings.shell,
		Vec::new(),
		std::env::current_dir()
			.ok()
			.and_then(|path| path.to_str().map(str::to_string)),
		columns,
		rows,
		(metrics.cell_width, metrics.cell_height),
		&environment,
	)?;

	// Cursor blink is a property of the emulated terminal, not of the renderer:
	// with it off the caret is a steady block and dedup collapses an idle
	// prompt to a single frame instead of one every half second.
	if !settings.cursor_blink {
		session.write(&b"\x1b[?12l"[..])?;
	}

	// Playback speed is a property of the *file*, not of the capture. Capturing
	// slower would let the child run between frames; telling the encoder the
	// frames are further apart plays the same frames back slower.
	let played = (settings.framerate as f32 / settings.playback_speed)
		.round()
		.clamp(1.0, u8::MAX as f32) as u8;

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
		screen: Mutex::new(Screen::default()),
	});

	let director = {
		let session = Arc::clone(&session);
		let stage = Arc::clone(&stage);
		let settings = settings.clone();
		std::thread::Builder::new()
			.name("dvd-direct".to_string())
			.spawn(move || direct(&steps, &settings, &session, &stage))
			.context("starting the director thread")?
	};

	let pump_result = pump(&session, &stage, &settings, columns, rows, level, sender);

	// The director is joined first: it is the thread that can fail with a
	// message worth reading (a `Wait` that timed out, a `Require` that was not
	// met), and reporting a pump error over the top of that would bury it.
	let directed = director
		.join()
		.map_err(|_| anyhow::anyhow!("the director thread panicked"))?;
	directed?;
	pump_result?;

	encoding
		.join()
		.map_err(|_| anyhow::anyhow!("the encoder thread panicked"))?
		.context("encoding")?;

	for output in &outputs {
		println!("wrote {}", output.display());
	}

	Ok(())
}

/// What the two threads say to each other.
struct Stage {
	/// `Hide`/`Show`. Capture continues while hidden — the terminal has to stay
	/// current — but nothing is emitted.
	visible: AtomicBool,
	/// The director has run the last command.
	finished: AtomicBool,
	/// Stills requested since the last frame was emitted.
	stills: Mutex<Vec<PathBuf>>,
	/// The most recent picture, as text, for `Wait` to match against.
	screen: Mutex<Screen>,
}

#[derive(Default)]
struct Screen {
	lines: Vec<String>,
	cursor_row: usize,
}

impl Screen {
	fn read(snapshot: &Snapshot) -> Self {
		Self {
			lines: (0..snapshot.screen_rows)
				.map(|row| {
					(0..snapshot.columns)
						.map(|column| match snapshot.cell(column, row).c() {
							'\0' => ' ',
							other => other,
						})
						.collect::<String>()
				})
				.collect(),
			cursor_row: snapshot.cursor.pos.row.0.max(0) as usize,
		}
	}

	fn matches(&self, pattern: &Regex, mode: &WaitMode) -> bool {
		match mode {
			WaitMode::Line => self
				.lines
				.get(self.cursor_row)
				.is_some_and(|line| pattern.is_match(line.trim_end())),
			WaitMode::Screen => pattern.is_match(&self.lines.join("\n")),
		}
	}
}

/// The capture clock.
///
/// Ticks at the frame rate, admits distinct pictures, and counts how many ticks
/// each one survives. The hold count is what makes the recording's timing
/// right: a frame that stayed on screen for two seconds is one frame with a
/// hold of a hundred, not a hundred identical frames.
fn pump(
	session: &Mutex<Session>,
	stage: &Stage,
	settings: &Settings,
	columns: u16,
	rows: u16,
	level: Level,
	sender: std::sync::mpsc::SyncSender<Frame>,
) -> Result<()> {
	let interval = Duration::from_secs_f64(1.0 / settings.framerate as f64);
	let mut dedup = Dedup::new(level, columns, rows);
	let mut scratch = Snapshot::new(columns, rows);

	// The frame currently on screen, waiting to learn how long it lasts.
	let mut pending: Option<Frame> = None;
	let mut next_tick = Instant::now();
	let mut trailing: Option<Instant> = None;

	loop {
		next_tick += interval;
		let now = Instant::now();
		if next_tick > now {
			std::thread::sleep(next_tick - now);
		} else {
			// Behind schedule — a long rasterization or a busy machine. Resync
			// rather than trying to catch up, which would only compress the
			// next few frames and make the recording jerk.
			next_tick = now;
		}

		let captured = {
			let mut session = session.lock().unwrap_or_else(|error| error.into_inner());
			session.capture(&mut scratch)
		};

		if captured == Capture::Changed {
			*stage.screen.lock().unwrap_or_else(|e| e.into_inner()) = Screen::read(&scratch);
		}

		let stills = std::mem::take(&mut *stage.stills.lock().unwrap_or_else(|e| e.into_inner()));
		let visible = stage.visible.load(Ordering::Acquire);

		// A new picture only counts if it is one we are allowed to show; while
		// hidden the frame on screen simply goes on being held.
		let fresh = visible && captured == Capture::Changed && dedup.admit(&scratch);

		if fresh || !stills.is_empty() {
			let mut snapshot = Snapshot::new(columns, rows);
			snapshot.columns = scratch.columns;
			snapshot.screen_rows = scratch.screen_rows;
			snapshot.cells.clone_from(&scratch.cells);
			snapshot.styles.clone_from(&scratch.styles);
			snapshot.extras.clone_from(&scratch.extras);
			snapshot.graphics.clone_from(&scratch.graphics);
			snapshot.cursor = scratch.cursor.clone();
			snapshot.cursor_visible = scratch.cursor_visible;

			// The frame that was on screen is now finished: its hold is however
			// many ticks it survived, and it can go.
			if let Some(previous) = pending.take() {
				let _ = sender.send(previous);
			}

			let mut frame = Frame::new(Arc::new(snapshot), 1);
			frame.stills = stills;
			pending = Some(frame);
		} else if let Some(frame) = pending.as_mut() {
			frame.hold += 1;
		}

		if stage.finished.load(Ordering::Acquire) {
			// Keep capturing briefly: the last command's *effect* — the output
			// of whatever was just run — arrives after the command does.
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

/// Execute the tape.
fn direct(
	steps: &[Commands],
	settings: &Settings,
	session: &Mutex<Session>,
	stage: &Stage,
) -> Result<()> {
	let result = run_steps(steps, settings, session, stage);
	// However it went, the pump has to be told to stop, or the recording never
	// ends and the error never surfaces.
	stage.finished.store(true, Ordering::Release);
	result
}

fn run_steps(
	steps: &[Commands],
	settings: &Settings,
	session: &Mutex<Session>,
	stage: &Stage,
) -> Result<()> {
	let write = |bytes: Vec<u8>| -> Result<()> {
		session
			.lock()
			.unwrap_or_else(|error| error.into_inner())
			.write(bytes)
	};

	let mut clipboard = String::new();

	for step in steps {
		match step {
			Commands::Type(typed) => {
				let rate = typed.rate.unwrap_or(settings.typing_speed);
				type_text(&typed.text, rate, &write)?;
			}

			Commands::Sleep(sleep) => {
				std::thread::sleep(sleep.duration.unwrap_or(Duration::from_secs(1)));
			}

			Commands::Key(key) => {
				let bytes = key_bytes(&key.key)
					.with_context(|| format!("{:?} is not a key this recorder can send", key.key))?;
				let rate = key.rate.unwrap_or(settings.typing_speed);

				// `repeat_count` is zero when the tape wrote a bare key with no
				// count, which still means press it once.
				for _ in 0..key.repeat_count.max(1) {
					write(bytes.to_vec())?;
					std::thread::sleep(rate);
				}
			}

			Commands::Ctrl(combo) => {
				write(ctrl_bytes(&combo.keys)?)?;
				std::thread::sleep(combo.rate.unwrap_or(settings.typing_speed));
			}

			Commands::Alt(combo) => {
				let mut bytes = vec![0x1b];
				bytes.extend_from_slice(combo.keys.join("").as_bytes());
				write(bytes)?;
				std::thread::sleep(combo.rate.unwrap_or(settings.typing_speed));
			}

			Commands::Shift(combo) => {
				write(shift_bytes(&combo.keys))?;
				std::thread::sleep(combo.rate.unwrap_or(settings.typing_speed));
			}

			Commands::Wait(wait) => {
				let pattern = match &wait.pattern {
					Some(pattern) => pattern.clone(),
					None => Regex::new(&settings.wait_pattern)
						.context("the default Wait pattern is not a valid regex")?,
				};
				let timeout = wait.timeout.unwrap_or(settings.wait_timeout);
				await_screen(stage, &pattern, &wait.mode, timeout)?;
			}

			Commands::Screenshot(shot) => {
				stage
					.stills
					.lock()
					.unwrap_or_else(|error| error.into_inner())
					.push(shot.path.clone());
				// Give the pump a tick to notice, so the still is of the screen
				// as it is now rather than of whatever comes next.
				std::thread::sleep(Duration::from_secs_f64(
					2.0 / settings.framerate as f64,
				));
			}

			Commands::Copy(copy) => clipboard = copy.text.clone(),
			Commands::Paste => type_text(&clipboard, Duration::ZERO, &write)?,

			Commands::Hide => stage.visible.store(false, Ordering::Release),
			Commands::Show => stage.visible.store(true, Ordering::Release),

			// Resolved before the session existed; see `plan`.
			Commands::Set(_) | Commands::Env(_) | Commands::Output(_) | Commands::Require(_) => {}
		}
	}

	Ok(())
}

fn type_text(
	text: &str,
	rate: Duration,
	write: &impl Fn(Vec<u8>) -> Result<()>,
) -> Result<()> {
	// Character by character rather than all at once: the point of a recording
	// is to look like someone typing, and a shell with readline echoes each
	// byte as it arrives.
	for character in text.chars() {
		let mut buffer = [0u8; 4];
		write(character.encode_utf8(&mut buffer).as_bytes().to_vec())?;
		if !rate.is_zero() {
			std::thread::sleep(rate);
		}
	}
	Ok(())
}

fn await_screen(
	stage: &Stage,
	pattern: &Regex,
	mode: &WaitMode,
	timeout: Duration,
) -> Result<()> {
	let deadline = Instant::now() + timeout;

	while Instant::now() < deadline {
		if stage
			.screen
			.lock()
			.unwrap_or_else(|error| error.into_inner())
			.matches(pattern, mode)
		{
			return Ok(());
		}
		std::thread::sleep(Duration::from_millis(10));
	}

	bail!("waited {timeout:?} for /{}/ and it never matched", pattern.as_str())
}

/// The bytes a terminal sends for a named key.
fn key_bytes(key: &TokenType) -> Option<&'static [u8]> {
	Some(match key {
		TokenType::Enter => b"\r",
		TokenType::Tab => b"\t",
		TokenType::Space => b" ",
		// DEL, not BS. Every modern terminal sends 0x7f for the backspace key,
		// and readline is configured to expect it.
		TokenType::Backspace => b"\x7f",
		TokenType::Escape => b"\x1b",
		TokenType::Delete => b"\x1b[3~",
		TokenType::Insert => b"\x1b[2~",
		TokenType::Up => b"\x1b[A",
		TokenType::Down => b"\x1b[B",
		TokenType::Right => b"\x1b[C",
		TokenType::Left => b"\x1b[D",
		TokenType::PageUp => b"\x1b[5~",
		TokenType::PageDown => b"\x1b[6~",
		TokenType::Home => b"\x1b[H",
		TokenType::End => b"\x1b[F",
		_ => return None,
	})
}

/// Fold a `Ctrl+...` chain down to the control byte it sends.
///
/// Control characters are the low five bits of the letter, which is why
/// `Ctrl+C` is 0x03 and `Ctrl+[` is the escape byte. Leading `Alt` turns the
/// whole thing into an escape-prefixed sequence, which is how a terminal has
/// always spelled meta.
fn ctrl_bytes(keys: &[String]) -> Result<Vec<u8>> {
	let mut prefix = Vec::new();
	let mut rest = keys;

	while let Some((first, tail)) = rest.split_first() {
		match first.as_str() {
			"Alt" => {
				prefix.push(0x1b);
				rest = tail;
			}
			// Shift on a control chord changes nothing: the control byte is
			// derived from the letter's position in the alphabet, and case has
			// already been folded out of it.
			"Shift" => rest = tail,
			_ => break,
		}
	}

	let mut out = prefix;
	for key in rest {
		let byte = match key.as_str() {
			"Space" => 0x00,
			"Enter" => 0x0d,
			"Tab" => 0x09,
			"Escape" => 0x1b,
			"Backspace" => 0x08,
			"[" => 0x1b,
			"\\" => 0x1c,
			"]" => 0x1d,
			"^" => 0x1e,
			"-" | "_" => 0x1f,
			single if single.chars().count() == 1 => {
				let character = single.chars().next().expect("one character");
				let upper = character.to_ascii_uppercase();
				if !upper.is_ascii_alphabetic() && !(0x3f..=0x5f).contains(&(upper as u32)) {
					bail!("Ctrl+{single} is not a control sequence");
				}
				(upper as u8) & 0x1f
			}
			other => bail!("Ctrl+{other} is not a control sequence"),
		};
		out.push(byte);
	}

	Ok(out)
}

fn shift_bytes(keys: &[String]) -> Vec<u8> {
	let mut out = Vec::new();
	for key in keys {
		match key.as_str() {
			// The one shifted key with a sequence of its own; everything else
			// is just the upper case of what was typed.
			"Tab" => out.extend_from_slice(b"\x1b[Z"),
			"Enter" => out.push(b'\r'),
			other => out.extend_from_slice(other.to_uppercase().as_bytes()),
		}
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn control_bytes_are_the_low_five_bits() {
		assert_eq!(ctrl_bytes(&["c".to_string()]).unwrap(), vec![0x03]);
		assert_eq!(ctrl_bytes(&["C".to_string()]).unwrap(), vec![0x03]);
		assert_eq!(ctrl_bytes(&["a".to_string()]).unwrap(), vec![0x01]);
		assert_eq!(ctrl_bytes(&["[".to_string()]).unwrap(), vec![0x1b]);
	}

	#[test]
	fn a_leading_alt_escape_prefixes_the_chord() {
		assert_eq!(
			ctrl_bytes(&["Alt".to_string(), "c".to_string()]).unwrap(),
			vec![0x1b, 0x03]
		);
	}

	#[test]
	fn shift_tab_is_a_sequence_rather_than_a_letter() {
		assert_eq!(shift_bytes(&["Tab".to_string()]), b"\x1b[Z".to_vec());
		assert_eq!(shift_bytes(&["a".to_string()]), b"A".to_vec());
	}

	#[test]
	fn backspace_sends_del_the_way_a_real_terminal_does() {
		assert_eq!(key_bytes(&TokenType::Backspace), Some(&b"\x7f"[..]));
		assert_eq!(key_bytes(&TokenType::Enter), Some(&b"\r"[..]));
		assert_eq!(key_bytes(&TokenType::Up), Some(&b"\x1b[A"[..]));
	}

	#[test]
	fn colours_parse_in_both_lengths() {
		assert_eq!(parse_color("#ff8800").unwrap(), [0xff, 0x88, 0x00, 0xff]);
		assert_eq!(parse_color("f80").unwrap(), [0xff, 0x88, 0x00, 0xff]);
		assert!(parse_color("not a colour").is_err());
	}

	#[test]
	fn settings_start_from_defaults_and_take_what_the_tape_sets() {
		let mut settings = Settings::default();
		assert_eq!(settings.width, 1200);

		settings.apply(&Setting::Width(800));
		settings.apply(&Setting::Theme("nord".to_string()));
		settings.apply(&Setting::Framerate(30));

		assert_eq!(settings.width, 800);
		assert_eq!(settings.theme, "nord");
		assert_eq!(settings.framerate, 30);
	}

	/// A zero frame rate would divide by zero in the pump's interval.
	#[test]
	fn a_zero_framerate_is_clamped() {
		let mut settings = Settings::default();
		settings.apply(&Setting::Framerate(0));
		assert!(settings.framerate >= 1);
	}

	#[test]
	fn the_plan_separates_configuration_from_steps() {
		let (commands, errors) = tape::parse(
			"Set Width 800\nSet Theme \"nord\"\nOutput out.gif\nType \"hi\"\nEnter",
		);
		assert!(errors.is_empty(), "{errors:?}");

		let plan = plan(commands).unwrap();
		assert_eq!(plan.settings.width, 800);
		assert_eq!(plan.settings.theme, "nord");
		assert_eq!(plan.outputs, vec![PathBuf::from("out.gif")]);
		assert_eq!(plan.steps.len(), 2, "Type and Enter are the only steps");
	}

	#[test]
	fn a_wait_matches_the_cursor_line_or_the_whole_screen() {
		let screen = Screen {
			lines: vec!["first".into(), "second $ ".into(), "third".into()],
			cursor_row: 1,
		};

		let prompt = Regex::new(r"\$\s*$").unwrap();
		assert!(screen.matches(&prompt, &WaitMode::Line));

		let elsewhere = Regex::new("third").unwrap();
		assert!(!elsewhere.is_match(&screen.lines[screen.cursor_row]));
		assert!(screen.matches(&elsewhere, &WaitMode::Screen));
	}
}
