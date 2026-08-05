//! A live terminal: a PTY, a child process, and the VT state machine reading it.
//!
//! `rio-vt` supplies all three. What this module adds is the shape the rest of
//! the pipeline wants — a screen that can be *captured* into a reusable
//! [`Snapshot`] buffer, and a cheap way to ask whether capturing is even worth
//! it.
//!
//! That question is the first and most valuable tier of frame dedup.
//! [`Crosswords::peek_damage_event`] answers it with an enum compare: a tape
//! sitting at an idle prompt for four seconds produces two hundred ticks and
//! zero damage, so two hundred captures collapse into one held frame without a
//! single cell ever being copied, hashed, or drawn. Everything downstream —
//! shaping, rasterizing, encoding — is spared work it would otherwise do and
//! then throw away.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use rio_vt::ansi::graphics::{OverlayViewport, kitty_overlay_geometry};
use rio_vt::crosswords::{Crosswords, CrosswordsSize};
use rio_vt::event::sync::FairMutex;
use rio_vt::event::{EventListener, Msg, RioEvent, WindowId};
use rio_vt::performer::Machine;
use rustc_hash::FxHashMap;

use crate::model::{Image, Placement, Snapshot};

/// Scrollback is deliberately small. A recording only ever shows the viewport,
/// and the one thing history buys — a placement anchored above the screen
/// staying correctly positioned as the grid scrolls — needs a handful of lines,
/// not a session's worth.
const SCROLLBACK_LINES: usize = 1_000;

const ROUTE: usize = 0;

/// A recording drives exactly one terminal, so the window and route ids that
/// multiplex Rio's panes are constant here.
fn window() -> WindowId {
	WindowId::from(0u64)
}

/// What a capture attempt found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capture {
	/// Nothing changed since the last capture. The snapshot was not touched.
	Unchanged,
	/// The snapshot was refilled and is ready to hash.
	Changed,
}

/// Events the VT core pushes out that the recorder actually cares about.
///
/// Almost everything `rio-vt` emits is for an interactive frontend — clipboard
/// requests, title changes, bell, cursor blink. A recording answers none of
/// them, so the listener keeps only the two that change what gets drawn.
#[derive(Default)]
struct Signals {
	/// The child process is gone; no further output will arrive.
	exited: AtomicBool,
	/// New images have been decoded and are waiting to be collected.
	graphics_pending: AtomicBool,
}

/// The `EventListener` handed to both `Crosswords` and `Machine`.
///
/// Shared by `Arc` rather than by channel because it is called from the reader
/// thread while the main thread may be reading it: `send_event` takes `&self`,
/// so the listener has to be `Sync`, and a plain `mpsc::Sender` is not.
#[derive(Clone, Default)]
struct Listener {
	signals: Arc<Signals>,
}

impl EventListener for Listener {
	fn event(&self) -> (Option<RioEvent>, bool) {
		(None, false)
	}

	fn send_event(&self, event: RioEvent, _window: WindowId) {
		match event {
			RioEvent::Exit | RioEvent::CloseTerminal(_) => {
				self.signals.exited.store(true, Ordering::Release);
			}
			RioEvent::UpdateGraphics { .. } => {
				self.signals.graphics_pending.store(true, Ordering::Release);
			}
			_ => {}
		}
	}
}

/// A running terminal session.
pub struct Session {
	terminal: Arc<FairMutex<Crosswords<Listener>>>,
	to_pty: corcovado::channel::Sender<Msg>,
	signals: Arc<Signals>,
	columns: u16,
	screen_rows: u16,
	cell_pixels: (u32, u32),

	/// Decoded Sixel/iTerm2 images, keyed by atlas texture key.
	atlas_images: FxHashMap<u64, Arc<Image>>,
	/// Decoded Kitty images, keyed by the protocol's own `i=` image id.
	kitty_images: FxHashMap<u32, Arc<Image>>,
}

impl Session {
	/// Spawn `shell` under a new PTY sized to `columns` x `screen_rows`.
	///
	/// `cell_pixels` is the cell size in device pixels. It is not cosmetic: it
	/// goes into the winsize the child sees, and the Sixel and Kitty protocols
	/// both size images against it, so getting it wrong makes every inline
	/// image the wrong scale.
	pub fn open(
		shell: &str,
		args: Vec<String>,
		working_directory: Option<String>,
		columns: u16,
		screen_rows: u16,
		cell_pixels: (u32, u32),
		environment: &[(String, String)],
	) -> Result<Self> {
		let listener = Listener::default();
		let signals = Arc::clone(&listener.signals);

		let size = CrosswordsSize::new(columns as usize, screen_rows as usize);
		let terminal = Crosswords::new(
			size,
			rio_vt::ansi::CursorShape::Block,
			listener.clone(),
			window(),
			ROUTE,
			SCROLLBACK_LINES,
		);
		let terminal = Arc::new(FairMutex::new(terminal));

		let (program, args) = wrap_with_environment(shell, args, environment);

		let (cell_width, cell_height) = cell_pixels;
		let pty = teletypewriter::create_pty_with_spawn(
			&program,
			args,
			&working_directory,
			columns,
			screen_rows,
			(columns as u32 * cell_width) as u16,
			(screen_rows as u32 * cell_height) as u16,
		)
		.with_context(|| format!("opening a pty for {shell}"))?;

		let machine = Machine::new(Arc::clone(&terminal), pty, listener, window(), ROUTE)
			.map_err(|error| anyhow::anyhow!("starting the pty reader: {error}"))?;
		let to_pty = machine.channel();
		machine.spawn();

		Ok(Self {
			terminal,
			to_pty,
			signals,
			columns,
			screen_rows,
			cell_pixels,
			atlas_images: FxHashMap::default(),
			kitty_images: FxHashMap::default(),
		})
	}

	/// Send bytes to the child as if they had been typed.
	pub fn write(&self, bytes: impl Into<std::borrow::Cow<'static, [u8]>>) -> Result<()> {
		self.to_pty
			.send(Msg::Input(bytes.into()))
			.map_err(|_| anyhow::anyhow!("the pty reader has stopped"))
	}

	/// Whether the child process has exited.
	pub fn exited(&self) -> bool {
		self.signals.exited.load(Ordering::Acquire)
	}

	pub fn columns(&self) -> u16 {
		self.columns
	}

	pub fn screen_rows(&self) -> u16 {
		self.screen_rows
	}

	/// Refill `into` with the current screen, unless nothing has changed.
	///
	/// This is dedup tier T0 and T1 in one call. `peek_damage_event` decides
	/// whether anything happened at all; when it did, `snapshot_visible` copies
	/// only the rows carrying a dirty bit into the buffer that is already
	/// there, leaving untouched rows as they were. A keystroke therefore costs
	/// one row copy, not a screen copy — and an idle tick costs neither.
	pub fn capture(&mut self, into: &mut Snapshot) -> Capture {
		let mut terminal = self.terminal.lock();

		let Some(damage) = terminal.peek_damage_event() else {
			return Capture::Unchanged;
		};

		// A cursor-only move still repaints: the cell under the cursor is drawn
		// differently. It is cheap to let through — one row is dirty at most —
		// and suppressing it here would drop the blink and the caret's travel.
		terminal.snapshot_visible(
			&damage,
			self.columns as usize,
			&mut into.rows,
			&mut into.styles,
			&mut into.extras,
		);

		let cursor = terminal.cursor();
		into.cursor_visible = !matches!(cursor.content, rio_vt::ansi::CursorShape::Hidden);
		into.cursor = cursor;
		into.columns = self.columns;
		into.screen_rows = self.screen_rows;

		// `reset_damage` clears the full flag and the per-line dirty bits, but it
		// does *not* record where the cursor ended up — only `damage()` does
		// that, via the `mem::replace` into `last_cursor`. Skipping it leaves
		// `last_cursor` pinned at its default forever, so `peek_damage_event`
		// keeps answering `CursorOnly` on a terminal that has been still for
		// minutes, and tier T0 never fires on a real recording.
		//
		// The returned iterator is the set of damaged lines, which the snapshot
		// above has already consumed through the dirty bits; it is dropped here
		// for the cursor bookkeeping alone.
		let _ = terminal.damage();
		terminal.reset_damage();

		// `snapshot_visible` leaves `rows` as `rio-vt` row structures; flatten
		// them into the packed cell vector the dedup kernels and the renderer
		// both read.
		into.flatten();

		if self.signals.graphics_pending.swap(false, Ordering::AcqRel) {
			absorb_new_images(
				&mut terminal,
				&mut self.atlas_images,
				&mut self.kitty_images,
			);
		}

		let cells = Cells {
			screen_rows: self.screen_rows,
			cell_pixels: self.cell_pixels,
		};
		collect_placements(
			&terminal,
			cells,
			&self.atlas_images,
			&self.kitty_images,
			into,
		);

		Capture::Changed
	}

	pub fn cell_pixels(&self) -> (u32, u32) {
		self.cell_pixels
	}
}

/// Grid geometry, passed to the placement resolver so it does not need to
/// borrow the whole session while the terminal lock is held.
#[derive(Clone, Copy)]
struct Cells {
	screen_rows: u16,
	cell_pixels: (u32, u32),
}

/// Move newly decoded images out of the VT core and into the session cache.
///
/// Decoding happens once per transmission; placements then reference the cached
/// `Arc<Image>`. That is what lets a full-screen Sixel held across a thousand
/// frames stay one allocation, and what makes [`Placement::same_paint`]'s
/// pointer comparison meaningful rather than a lucky guess.
fn absorb_new_images(
	terminal: &mut Crosswords<Listener>,
	atlas_images: &mut FxHashMap<u64, Arc<Image>>,
	kitty_images: &mut FxHashMap<u32, Arc<Image>>,
) {
	let Some(queues) = terminal.graphics_take_queues() else {
		return;
	};

	for graphic in &queues.pending {
		atlas_images.insert(
			rio_graphics::atlas_image_key(graphic.id.0),
			to_rgba(graphic),
		);
	}
	for (image_id, graphic) in &queues.pending_images {
		kitty_images.insert(*image_id, to_rgba(graphic));
	}
	for key in &queues.remove_queue {
		atlas_images.remove(key);
	}
}

/// Resolve every placement currently on screen into pixel geometry.
///
/// Both protocols anchor to absolute rows in the scrollback, so a placement has
/// to be re-resolved against the viewport every frame — an image two lines above
/// the top of the screen is a different rectangle after a scroll, even though
/// nothing about the placement itself changed. The geometry helpers that get
/// this right already live in `rio-vt` and are used here rather than
/// reimplemented.
fn collect_placements(
	terminal: &Crosswords<Listener>,
	cells: Cells,
	atlas_images: &FxHashMap<u64, Arc<Image>>,
	kitty_images: &FxHashMap<u32, Arc<Image>>,
	into: &mut Snapshot,
) {
	into.graphics.clear();

	let graphics = &terminal.graphics;
	let (cell_width, cell_height) = cells.cell_pixels;
	let history = terminal.history_size() as i64;
	let display_offset = terminal.display_offset() as i64;

	// Sixel and iTerm2: cell-anchored regions, already split into visible
	// pieces by the VT core as the grid scrolls under them.
	for placement in &graphics.atlas_placements {
		let Some(image) = atlas_images.get(&placement.image_key) else {
			continue;
		};
		let screen_row = placement.abs_row - (history - display_offset);

		into.graphics.push(Placement {
			x: (placement.col as u32 * cell_width) as i32,
			y: (screen_row * cell_height as i64) as i32,
			width: placement.columns as u32 * cell_width,
			height: placement.rows as u32 * cell_height,
			source_x: placement.src_x,
			source_y: placement.src_y,
			source_width: placement.src_width,
			source_height: placement.src_height,
			// Sixel and iTerm2 have no z parameter; they sit under the text
			// the way a terminal paints them.
			z: -1,
			image: Arc::clone(image),
		});
	}

	// Kitty: pixel-precise placements with a sub-cell offset and an explicit
	// z, resolved against the live image size so a retransmit re-crops.
	let viewport = OverlayViewport {
		cell_width: cell_width as f32,
		cell_height: cell_height as f32,
		origin_x: 0.0,
		origin_y: 0.0,
		history_size: history,
		display_offset,
		screen_lines: cells.screen_rows as i64,
	};

	for placement in graphics.kitty_placements.values() {
		let Some(image) = kitty_images.get(&placement.image_id) else {
			continue;
		};
		let Some(geometry) = kitty_overlay_geometry(
			placement,
			image.width as usize,
			image.height as usize,
			&viewport,
		) else {
			continue;
		};

		// `source_rect` is normalized; the renderer wants source pixels.
		let [u0, v0, u1, v1] = geometry.source_rect;
		let to_x = |u: f32| (u * image.width as f32).round() as u32;
		let to_y = |v: f32| (v * image.height as f32).round() as u32;

		into.graphics.push(Placement {
			x: geometry.x.round() as i32,
			y: geometry.y.round() as i32,
			width: geometry.width.round().max(0.0) as u32,
			height: geometry.height.round().max(0.0) as u32,
			source_x: to_x(u0),
			source_y: to_y(v0),
			source_width: to_x(u1).saturating_sub(to_x(u0)),
			source_height: to_y(v1).saturating_sub(to_y(v0)),
			z: placement.z_index,
			image: Arc::clone(image),
		});
	}

	// Stable sort on `z` alone, so placements at equal depth keep the order
	// they were collected in — which is the order they were transmitted.
	into.graphics.sort_by_key(|placement| placement.z);
}

/// Route the shell through `env` when the tape sets variables.
///
/// `teletypewriter` offers no way to pass an environment to the child, and the
/// alternative — mutating this process's own environment before the spawn — is
/// both racy against the threads already running and `unsafe` as of the 2024
/// edition. `env` is POSIX, does exactly this job, and leaves the recorder's
/// own environment alone.
fn wrap_with_environment(
	shell: &str,
	args: Vec<String>,
	environment: &[(String, String)],
) -> (String, Vec<String>) {
	if environment.is_empty() {
		return (shell.to_string(), args);
	}

	let mut wrapped = Vec::with_capacity(environment.len() + args.len() + 1);
	for (name, value) in environment {
		wrapped.push(format!("{name}={value}"));
	}
	wrapped.push(shell.to_string());
	wrapped.extend(args);

	("/usr/bin/env".to_string(), wrapped)
}

/// Turn a decoded graphic into straight RGBA8.
///
/// Sixel and iTerm2 hand back three bytes per pixel; Kitty may send either.
/// Normalising once here means the renderer and every encoder see one layout.
fn to_rgba(data: &rio_graphics::GraphicData) -> Arc<Image> {
	let pixel_count = data.width * data.height;
	let rgba = match data.color_type {
		rio_graphics::ColorType::Rgba => data.pixels.clone(),
		rio_graphics::ColorType::Rgb => {
			let mut out = Vec::with_capacity(pixel_count * 4);
			for pixel in data.pixels.chunks_exact(3) {
				out.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 0xff]);
			}
			out
		}
	};

	Arc::new(Image {
		width: data.width as u32,
		height: data.height as u32,
		rgba,
	})
}

/// Errors from the PTY side, kept separate so a caller can tell "the child
/// exited" apart from "the recorder broke".
#[derive(Debug)]
pub struct PtyGone;

impl std::fmt::Display for PtyGone {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter.write_str("the child process has exited")
	}
}

impl std::error::Error for PtyGone {}

impl From<PtyGone> for io::Error {
	fn from(_: PtyGone) -> Self {
		io::Error::new(io::ErrorKind::BrokenPipe, "the child process has exited")
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Read the visible screen as text, for assertions.
	fn screen_text(snapshot: &Snapshot) -> String {
		(0..snapshot.screen_rows)
			.map(|row| {
				(0..snapshot.columns)
					.map(|column| snapshot.cell(column, row).c())
					.collect::<String>()
			})
			.collect::<Vec<_>>()
			.join("\n")
	}

	/// Drive a real shell under a real PTY until `needle` shows up, or give up.
	///
	/// This is the test that proves the whole chain is connected — PTY, reader
	/// thread, VT parser, damage tracking, incremental snapshot, flatten. A unit
	/// test with a hand-fed byte stream would exercise none of the wiring that
	/// actually breaks.
	fn run_until(input: &'static str, needle: &str) -> Option<Snapshot> {
		let mut session = Session::open(
			"/bin/sh",
			Vec::new(),
			std::env::temp_dir().to_str().map(str::to_string),
			40,
			10,
			(8, 16),
			&[],
		)
		.expect("opening a pty");

		session.write(input.as_bytes()).expect("writing to the pty");

		let mut snapshot = Snapshot::new(40, 10);
		let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);

		while std::time::Instant::now() < deadline {
			if session.capture(&mut snapshot) == Capture::Changed
				&& screen_text(&snapshot).contains(needle)
			{
				return Some(snapshot);
			}
			std::thread::sleep(std::time::Duration::from_millis(20));
		}
		None
	}

	#[test]
	fn a_real_shell_writes_output_into_the_snapshot() {
		let snapshot =
			run_until("echo dvd-works\n", "dvd-works").expect("the shell never produced output");

		assert_eq!(snapshot.columns, 40);
		assert_eq!(snapshot.screen_rows, 10);
		assert_eq!(snapshot.cells.len(), 400, "cells must be columns * rows");
		assert!(
			!snapshot.styles.is_empty(),
			"a style table should be present"
		);
	}

	/// Tier T0: once the shell goes quiet, capture must report `Unchanged`
	/// without touching the snapshot. This is the property the whole dedup
	/// design rests on — an idle recording costs nothing per tick.
	#[test]
	fn an_idle_terminal_reports_no_change() {
		let mut session = Session::open(
			"/bin/sh",
			Vec::new(),
			std::env::temp_dir().to_str().map(str::to_string),
			40,
			10,
			(8, 16),
			&[],
		)
		.expect("opening a pty");

		let mut snapshot = Snapshot::new(40, 10);

		// Let the shell start up and settle.
		let settle = std::time::Instant::now() + std::time::Duration::from_secs(2);
		while std::time::Instant::now() < settle {
			session.capture(&mut snapshot);
			std::thread::sleep(std::time::Duration::from_millis(20));
		}

		// Nothing is driving the terminal now, so every further tick is idle.
		let idle = (0..20)
			.map(|_| {
				std::thread::sleep(std::time::Duration::from_millis(10));
				session.capture(&mut snapshot)
			})
			.filter(|outcome| *outcome == Capture::Unchanged)
			.count();

		assert_eq!(idle, 20, "a quiet terminal must not report changes");
	}

	#[test]
	fn no_environment_leaves_the_command_alone() {
		let (program, args) = wrap_with_environment("/bin/sh", vec!["-l".into()], &[]);
		assert_eq!(program, "/bin/sh");
		assert_eq!(args, vec!["-l".to_string()]);
	}

	#[test]
	fn environment_is_passed_through_env_rather_than_the_parent_process() {
		let (program, args) = wrap_with_environment(
			"/bin/sh",
			vec!["-l".into()],
			&[("TERM".into(), "xterm".into())],
		);

		assert_eq!(program, "/usr/bin/env");
		assert_eq!(args, vec!["TERM=xterm", "/bin/sh", "-l"]);
		// The recorder's own environment must be untouched.
		assert!(std::env::var("TERM").unwrap_or_default() != "xterm-set-by-test");
	}
}
