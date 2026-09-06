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

/// Converts source timestamps into replay timestamps while applying the
/// asciicast idle-time policy uniformly to every consumer.
#[derive(Clone, Debug)]
pub struct ReplayClock {
	idle_time_limit: Option<Duration>,
	source_time: Duration,
	replay_time: Duration,
}

impl ReplayClock {
	pub fn new(idle_time_limit: Option<Duration>) -> Self {
		Self {
			idle_time_limit,
			source_time: Duration::ZERO,
			replay_time: Duration::ZERO,
		}
	}

	pub fn map_event(&mut self, mut event: TimedTerminalEvent) -> Result<TimedTerminalEvent> {
		ensure!(
			event.time >= self.source_time,
			"event source returned an event before the previous timestamp"
		);
		let gap = event.time - self.source_time;
		let gap = self.idle_time_limit.map_or(gap, |limit| gap.min(limit));
		self.replay_time = self
			.replay_time
			.checked_add(gap)
			.context("replay timeline exceeds duration limits")?;
		self.source_time = event.time;
		event.time = self.replay_time;
		Ok(event)
	}

	/// Return the end of the replay, including a header's final idle period.
	pub fn finish(&mut self, source_duration: Option<Duration>) -> Result<Duration> {
		let Some(source_duration) = source_duration else {
			return Ok(self.replay_time);
		};
		ensure!(
			source_duration >= self.source_time,
			"recording duration precedes its final event"
		);
		let gap = source_duration - self.source_time;
		let gap = self.idle_time_limit.map_or(gap, |limit| gap.min(limit));
		self.replay_time = self
			.replay_time
			.checked_add(gap)
			.context("replay timeline exceeds duration limits")?;
		self.source_time = source_duration;
		Ok(self.replay_time)
	}

	pub fn replay_time(&self) -> Duration {
		self.replay_time
	}
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
	let result = (|| -> Result<()> {
		for sink in &mut sinks {
			sink.begin(&metadata)?;
		}

		let initial = source.metadata().size;
		let source_metadata = source.metadata().clone();
		let mut clock = ReplayClock::new(source_metadata.idle_time_limit);
		let mut terminal = Terminal::new(initial);
		let mut deduplicator = Deduplicator::new(Level::new());
		let mut pixels = Pixmap::new(width, height);
		let needs_pixels = sinks.iter().any(|sink| sink.requires_pixels());
		let mut buffered = None;
		let mut source_done = false;
		let mut replay_end = None;
		let mut held: Option<Arc<Snapshot>> = None;
		let mut hold_ticks = 0u64;
		let mut tick = 0u64;

		loop {
			while !source_done {
				if buffered.is_none() {
					buffered = source
						.next_event()?
						.map(|event| clock.map_event(event))
						.transpose()?;
					if buffered.is_none() {
						source_done = true;
						replay_end = Some(clock.finish(source_metadata.duration)?);
						break;
					}
				}
				let event = buffered.as_ref().expect("buffered event was just checked");
				let event_tick = tick_for_time(event.time, options.frames_per_second);
				if event_tick > tick {
					break;
				}
				let event = buffered.take().expect("buffered event was just checked");
				terminal.apply(event, initial, options.resize_policy)?;
			}

			observe(
				rasterizer,
				&mut sinks,
				&mut pixels,
				needs_pixels,
				&mut held,
				&mut hold_ticks,
				&mut deduplicator,
				&mut terminal,
				initial,
			)?;

			if let Some(next_tick) = buffered
				.as_ref()
				.map(|event| tick_for_time(event.time, options.frames_per_second))
				.or_else(|| replay_end.map(|time| tick_for_time(time, options.frames_per_second)))
				.filter(|next_tick| *next_tick > tick)
			{
				let skipped = next_tick - tick - 1;
				add_hold(&mut hold_ticks, skipped)?;
				tick = next_tick;
				continue;
			}

			if source_done
				&& buffered.is_none()
				&& replay_end
					.is_some_and(|end| tick >= tick_for_time(end, options.frames_per_second))
			{
				break;
			}
			tick = tick
				.checked_add(1)
				.context("replay timeline has too many frames")?;
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
		Ok(())
	})();

	let finish_result = finish_sinks(sinks);
	result.and(finish_result)
}

fn observe(
	rasterizer: &mut dyn Rasterizer,
	sinks: &mut [Box<dyn Sink>],
	pixels: &mut Pixmap,
	needs_pixels: bool,
	held: &mut Option<Arc<Snapshot>>,
	hold_ticks: &mut u64,
	deduplicator: &mut Deduplicator,
	terminal: &mut Terminal,
	initial: TerminalSize,
) -> Result<()> {
	let snapshot = Arc::new(fit_snapshot(terminal.snapshot(), initial));
	if deduplicator.admit(&snapshot) {
		if let Some(previous) = held.replace(snapshot) {
			draw(
				rasterizer,
				sinks,
				pixels,
				Frame::new(previous, *hold_ticks),
				needs_pixels,
			)?;
		}
		*hold_ticks = 1;
	} else {
		add_hold(hold_ticks, 1)?;
	}
	Ok(())
}

fn add_hold(hold_ticks: &mut u64, additional: u64) -> Result<()> {
	*hold_ticks = hold_ticks
		.checked_add(additional)
		.context("replay frame hold exceeds limits")?;
	Ok(())
}

fn tick_for_time(time: Duration, frames_per_second: u32) -> u64 {
	let nanos = u128::from(time.as_secs()) * 1_000_000_000u128 + u128::from(time.subsec_nanos());
	let ticks = (nanos * u128::from(frames_per_second)).div_ceil(1_000_000_000);
	ticks.min(u128::from(u64::MAX)) as u64
}

fn finish_sinks(sinks: Vec<Box<dyn Sink>>) -> Result<()> {
	let mut failure = None;
	for sink in sinks {
		if let Err(error) = sink.finish() {
			failure.get_or_insert(error);
		}
	}
	failure.map_or(Ok(()), Err)
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
	use std::sync::atomic::{AtomicUsize, Ordering};

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

	#[test]
	fn replay_clock_caps_each_idle_gap_and_retains_final_inactivity() {
		let mut clock = ReplayClock::new(Some(Duration::from_secs(1)));
		let first = clock
			.map_event(TimedTerminalEvent {
				time: Duration::from_secs(2),
				event: TerminalEvent::Output(Vec::new()),
			})
			.unwrap();
		assert_eq!(first.time, Duration::from_secs(1));
		let second = clock
			.map_event(TimedTerminalEvent {
				time: Duration::from_secs(10),
				event: TerminalEvent::Output(Vec::new()),
			})
			.unwrap();
		assert_eq!(second.time, Duration::from_secs(2));
		assert_eq!(
			clock.finish(Some(Duration::from_secs(20))).unwrap(),
			Duration::from_secs(3)
		);
	}

	#[test]
	fn replay_clock_rejects_a_duration_before_the_last_event() {
		let mut clock = ReplayClock::new(None);
		clock
			.map_event(TimedTerminalEvent {
				time: Duration::from_secs(2),
				event: TerminalEvent::Output(Vec::new()),
			})
			.unwrap();
		assert!(clock.finish(Some(Duration::from_secs(1))).is_err());
	}

	#[test]
	fn tick_for_time_rounds_up_without_iterating_the_idle_interval() {
		assert_eq!(tick_for_time(Duration::from_millis(1), 30), 1);
		assert_eq!(tick_for_time(Duration::from_secs(1), 30), 30);
	}

	struct FailingSource {
		metadata: crate::source::TerminalMetadata,
	}

	impl crate::source::EventSource for FailingSource {
		fn metadata(&self) -> &crate::source::TerminalMetadata {
			&self.metadata
		}

		fn next_event(&mut self) -> Result<Option<TimedTerminalEvent>> {
			Err(anyhow::anyhow!("source failed"))
		}
	}

	struct FinishingSink {
		finished: Arc<AtomicUsize>,
		error: bool,
	}

	impl Sink for FinishingSink {
		fn requires_pixels(&self) -> bool {
			false
		}

		fn begin(&mut self, _metadata: &Metadata) -> Result<()> {
			Ok(())
		}

		fn accept(&mut self, _context: SinkContext<'_>) -> Result<()> {
			Ok(())
		}

		fn finish(self: Box<Self>) -> Result<()> {
			self.finished.fetch_add(1, Ordering::Relaxed);
			if self.error {
				Err(anyhow::anyhow!("sink failed"))
			} else {
				Ok(())
			}
		}
	}

	#[test]
	fn source_errors_still_finish_every_sink_and_preserve_source_error() {
		let finished = Arc::new(AtomicUsize::new(0));
		let mut source = FailingSource {
			metadata: crate::source::TerminalMetadata::new(TerminalSize::new(2, 1)),
		};
		let mut rasterizer = TestRasterizer;
		let result = render_source(
			&mut source,
			&mut rasterizer,
			vec![
				Box::new(FinishingSink {
					finished: Arc::clone(&finished),
					error: false,
				}),
				Box::new(FinishingSink {
					finished: Arc::clone(&finished),
					error: true,
				}),
			],
			TimelineOptions::default(),
		);

		assert_eq!(result.unwrap_err().to_string(), "source failed");
		assert_eq!(finished.load(Ordering::Relaxed), 2);
	}

	struct TestRasterizer;

	impl Rasterizer for TestRasterizer {
		fn dimensions(&self) -> (u16, u16) {
			(2, 2)
		}

		fn render(&mut self, _snapshot: &Snapshot, _target: &mut Pixmap) {}
	}
}
