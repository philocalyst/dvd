//! The source-driven terminal timeline.
//!
//! Every persisted or live recording eventually has the same job: advance a
//! terminal emulator at event times, sample it on a deterministic clock, and
//! fan distinct pictures into the output sinks. Keeping that reduction here
//! means a new recording format only implements [`EventSource`]; it does not
//! grow another replay loop with subtly different timing or resize behaviour.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};

use crate::model::Snapshot;
use crate::rio_vt;
use crate::rio_vt::crosswords::{Crosswords, CrosswordsSize};
use crate::rio_vt::event::{EventListener, RioEvent, WindowId};
use crate::source::{EventSource, TerminalEvent, TerminalSize, TimedTerminalEvent};
use crate::stream::{Context as SinkContext, Deduplicator, Frame, Metadata, Rasterizer, Sink};
use crate::{Level, Pixmap};

/// What a fixed-size output should do when a source changes its terminal size.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResizePolicy {
	/// Keep the output canvas fixed, cropping or padding the terminal picture.
	Clip,
	/// Fail before emitting a resized picture.
	Reject,
}

/// Controls sampling and the safe interpretation of source resize events.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimelineOptions {
	pub frames_per_second: u32,
	pub resize_policy: ResizePolicy,
}

impl Default for TimelineOptions {
	fn default() -> Self {
		Self {
			frames_per_second: 30,
			resize_policy: ResizePolicy::Clip,
		}
	}
}

impl TimelineOptions {
	fn validate(self) -> Result<u8> {
		ensure!(
			self.frames_per_second > 0,
			"frames per second must be positive"
		);
		u8::try_from(self.frames_per_second)
			.context("frames per second must fit the output metadata (maximum 255)")
	}
}

/// Replay a source through the terminal model and existing sink fanout.
pub fn render_source<S: EventSource + ?Sized>(
	source: &mut S,
	rasterizer: &mut dyn Rasterizer,
	mut sinks: Vec<Box<dyn Sink>>,
	options: TimelineOptions,
) -> Result<()> {
	let frames_per_second = options.validate()?;
	let (width, height) = rasterizer.dimensions();
	let metadata = Metadata {
		width,
		height,
		frames_per_second,
	};
	for sink in &mut sinks {
		sink.begin(&metadata)?;
	}

	let initial = source.metadata().size;
	let mut terminal = Terminal::new(initial);
	let mut deduplicator = Deduplicator::new(Level::new());
	let mut pixels = Pixmap::new(width, height);
	let needs_pixels = sinks.iter().any(|sink| sink.requires_pixels());
	let mut buffered = None;
	let mut source_done = false;
	let mut previous_time = Duration::ZERO;
	let mut held: Option<Arc<Snapshot>> = None;
	let mut hold_ticks = 0;

	for tick in 0u64.. {
		let time = sample_time(tick, options.frames_per_second);
		while !source_done {
			if buffered.is_none() {
				buffered = source.next_event()?;
				if buffered.is_none() {
					source_done = true;
					break;
				}
			}
			let event = buffered.as_ref().expect("buffered event was just checked");
			ensure!(
				event.time >= previous_time,
				"event source returned an event before the previous timestamp"
			);
			if event.time > time {
				break;
			}
			let event = buffered.take().expect("buffered event was just checked");
			previous_time = event.time;
			terminal.apply(event, initial, options.resize_policy)?;
		}

		let snapshot = Arc::new(fit_snapshot(terminal.snapshot(), initial));
		if deduplicator.admit(&snapshot) {
			if let Some(previous) = held.replace(snapshot) {
				draw(
					rasterizer,
					&mut sinks,
					&mut pixels,
					Frame::new(previous, hold_ticks),
					needs_pixels,
				)?;
			}
			hold_ticks = 1;
		} else {
			hold_ticks = hold_ticks.saturating_add(1);
		}

		if source_done && buffered.is_none() {
			break;
		}
	}

	if let Some(snapshot) = held {
		draw(
			rasterizer,
			&mut sinks,
			&mut pixels,
			Frame::new(snapshot, hold_ticks),
			needs_pixels,
		)?;
	}
	finish_sinks(sinks)
}

fn finish_sinks(sinks: Vec<Box<dyn Sink>>) -> Result<()> {
	for sink in sinks {
		sink.finish()?;
	}
	Ok(())
}

fn draw(
	rasterizer: &mut dyn Rasterizer,
	sinks: &mut [Box<dyn Sink>],
	pixels: &mut Pixmap,
	frame: Frame,
	needs_pixels: bool,
) -> Result<()> {
	if needs_pixels {
		rasterizer.render(&frame.snapshot, pixels);
	}
	for sink in sinks.iter_mut() {
		sink.accept(SinkContext {
			frame: &frame,
			pixels: sink.requires_pixels().then_some(&*pixels),
		})?;
	}
	Ok(())
}

fn fit_snapshot(snapshot: Snapshot, target: TerminalSize) -> Snapshot {
	if snapshot.columns == target.columns && snapshot.screen_rows == target.rows {
		return snapshot;
	}

	let mut fitted = Snapshot::new(target.columns, target.rows);
	fitted.styles = snapshot.styles;
	fitted.extras = snapshot.extras;
	fitted.cursor = snapshot.cursor;
	fitted.cursor_visible = snapshot.cursor_visible;
	fitted.graphics = snapshot.graphics;
	let columns = usize::from(snapshot.columns.min(target.columns));
	let rows = usize::from(snapshot.screen_rows.min(target.rows));
	for row in 0..rows {
		let from = row * usize::from(snapshot.columns);
		let to = row * usize::from(target.columns);
		fitted.cells[to..to + columns].copy_from_slice(&snapshot.cells[from..from + columns]);
	}
	fitted
}

/// Exact floor sampling from the integer timeline, avoiding accumulated drift.
pub fn sample_time(tick: u64, frames_per_second: u32) -> Duration {
	let nanos = (u128::from(tick) * 1_000_000_000u128) / u128::from(frames_per_second);
	Duration::from_nanos(nanos.min(u128::from(u64::MAX)) as u64)
}

#[derive(Clone, Default)]
struct Events;

impl EventListener for Events {
	fn event(&self) -> (Option<RioEvent>, bool) {
		(None, false)
	}

	fn send_event(&self, _: RioEvent, _: WindowId) {}
}

struct Terminal {
	vt: Crosswords<Events>,
	size: TerminalSize,
}

impl Terminal {
	fn new(size: TerminalSize) -> Self {
		Self {
			vt: Crosswords::new(
				CrosswordsSize::new(size.columns as usize, size.rows as usize),
				rio_vt::ansi::CursorShape::Block,
				Events,
				WindowId::from(0),
				0,
				1_000,
			),
			size,
		}
	}

	fn apply(
		&mut self,
		event: TimedTerminalEvent,
		initial: TerminalSize,
		resize_policy: ResizePolicy,
	) -> Result<()> {
		match event.event {
			TerminalEvent::Output(bytes) => {
				rio_vt::performer::handler::Processor::default().advance(&mut self.vt, &bytes);
			}
			TerminalEvent::Resize(size) if size != self.size => {
				if resize_policy == ResizePolicy::Reject && size != initial {
					bail!(
						"recording changes terminal size from {}x{} to {}x{}",
						initial.columns,
						initial.rows,
						size.columns,
						size.rows
					);
				}
				self.vt.resize(CrosswordsSize::new(
					size.columns as usize,
					size.rows as usize,
				));
				self.size = size;
			}
			TerminalEvent::Input(_)
			| TerminalEvent::Resize(_)
			| TerminalEvent::Marker(_)
			| TerminalEvent::Exit(_)
			| TerminalEvent::Unknown { .. } => {}
		}
		Ok(())
	}

	fn snapshot(&mut self) -> Snapshot {
		let mut snapshot = Snapshot::new(self.size.columns, self.size.rows);
		self.vt.snapshot_visible(
			&rio_vt::event::TerminalDamage::Full,
			self.size.columns as usize,
			&mut snapshot.rows,
			&mut snapshot.styles,
			&mut snapshot.extras,
		);
		snapshot.cursor = self.vt.cursor();
		snapshot.cursor_visible =
			!matches!(snapshot.cursor.content, rio_vt::ansi::CursorShape::Hidden);
		snapshot.flatten();
		snapshot
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn sample_clock_does_not_accumulate_fractional_frames() {
		assert_eq!(sample_time(3, 7), Duration::from_nanos(428_571_428));
		assert_eq!(sample_time(7, 7), Duration::from_secs(1));
	}

	#[test]
	fn options_reject_zero_or_unrepresentable_frame_rates() {
		assert!(
			TimelineOptions {
				frames_per_second: 0,
				..Default::default()
			}
			.validate()
			.is_err()
		);
		assert!(
			TimelineOptions {
				frames_per_second: 256,
				..Default::default()
			}
			.validate()
			.is_err()
		);
	}

	#[test]
	fn resize_policy_clip_accepts_a_dynamic_terminal() {
		let initial = TerminalSize::new(2, 1);
		let resized = TimedTerminalEvent {
			time: Duration::ZERO,
			event: TerminalEvent::Resize(TerminalSize::new(4, 2)),
		};
		let mut terminal = Terminal::new(initial);
		terminal
			.apply(resized, initial, ResizePolicy::Clip)
			.expect("clip policy should resize the emulator");
		let snapshot = fit_snapshot(terminal.snapshot(), initial);
		assert_eq!((snapshot.columns, snapshot.screen_rows), (2, 1));
	}

	#[test]
	fn resize_policy_reject_reports_non_initial_size() {
		let initial = TerminalSize::new(2, 1);
		let event = TimedTerminalEvent {
			time: Duration::ZERO,
			event: TerminalEvent::Resize(TerminalSize::new(4, 2)),
		};
		let mut terminal = Terminal::new(initial);
		let error = terminal
			.apply(event, initial, ResizePolicy::Reject)
			.unwrap_err();
		assert!(error.to_string().contains("changes terminal size"));
	}
}
