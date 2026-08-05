//! The spine: one stream of frames, fanned out to every encoder.
//!
//! Capture and encoding run on different threads and never wait on each other
//! in the steady state. That is not a nicety — the capture loop is what keeps
//! the tape's timing honest. If it stalled behind a rasterization, the child
//! process would keep running while the recorder wasn't looking, and the
//! recording would drift away from the script that produced it.
//!
//! Three things keep the capture side free:
//!
//! 1. **Dedup runs first.** [`Dedup`] rejects frames that paint the same
//!    picture, so the encoder only ever sees distinct ones. A tape that spends
//!    most of its length at a prompt hands the encoder almost nothing.
//! 2. **The queue absorbs bursts.** Typing produces a run of distinct frames
//!    faster than they can be drawn; a deep channel lets the encoder fall
//!    behind during the burst and catch up during the next pause.
//! 3. **The encoder is parallel inside.** Rasterization uses `rayon`, so a
//!    single frame is drawn by every core rather than one.
//!
//! Because the encoder runs the whole time capture does, it is essentially
//! finished when the tape is. The channel is bounded rather than unbounded on
//! purpose: a recording of a full-screen animation produces genuinely distinct
//! frames at the frame rate, and an unbounded queue would answer that by
//! growing until it exhausted memory. [`QUEUE_DEPTH`] is the amount of slack
//! the pipeline gets before capture is asked to wait.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use fearless_simd::Level;
use vello_cpu::Pixmap;

use crate::model::Snapshot;
use crate::simd;

/// Distinct frames the encoder may fall behind by before capture waits.
///
/// Sized for the burst that matters: a `Type` command at 50 ms per keystroke
/// against a 50 fps frame rate produces a distinct frame every two or three
/// ticks, and 128 of them is several seconds of continuous typing — far longer
/// than any real burst before the tape pauses and the encoder catches up.
pub const QUEUE_DEPTH: usize = 128;

// --- Buffer pooling ---

/// A pool of reusable buffers.
///
/// The pipeline's per-frame allocations are the expensive kind: a 1200x600
/// surface is 2.8 MB, and allocating one per frame would hand the allocator
/// gigabytes of churn over a recording. Buffers taken from here return
/// themselves on drop, so steady-state allocation is zero without any caller
/// having to remember to give anything back.
pub struct Pool<T> {
	idle: Mutex<Vec<T>>,
	make: Box<dyn Fn() -> T + Send + Sync>,
}

impl<T> Pool<T> {
	pub fn new(make: impl Fn() -> T + Send + Sync + 'static) -> Arc<Self> {
		Arc::new(Self {
			idle: Mutex::new(Vec::new()),
			make: Box::new(make),
		})
	}

	/// Take a buffer, making a fresh one only if none are idle.
	pub fn take(self: &Arc<Self>) -> Pooled<T> {
		let item = self.idle.lock().unwrap_or_else(|e| e.into_inner()).pop();
		Pooled {
			item: Some(item.unwrap_or_else(|| (self.make)())),
			pool: Arc::clone(self),
		}
	}

	/// How many buffers are currently idle. Test-facing.
	pub fn idle_count(&self) -> usize {
		self.idle.lock().unwrap_or_else(|e| e.into_inner()).len()
	}
}

/// A buffer on loan from a [`Pool`], returned when dropped.
pub struct Pooled<T> {
	item: Option<T>,
	pool: Arc<Pool<T>>,
}

impl<T> std::ops::Deref for Pooled<T> {
	type Target = T;
	fn deref(&self) -> &T {
		self.item.as_ref().expect("a live loan always holds its item")
	}
}

impl<T> std::ops::DerefMut for Pooled<T> {
	fn deref_mut(&mut self) -> &mut T {
		self.item.as_mut().expect("a live loan always holds its item")
	}
}

impl<T> Drop for Pooled<T> {
	fn drop(&mut self) {
		if let Some(item) = self.item.take() {
			self.pool
				.idle
				.lock()
				.unwrap_or_else(|e| e.into_inner())
				.push(item);
		}
	}
}

impl<T: std::fmt::Debug> std::fmt::Debug for Pooled<T> {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		self.item.fmt(formatter)
	}
}

// --- Frames ---

/// One distinct picture, and how long it stays on screen.
///
/// `hold` is a count of capture ticks, not a duration. Turning it into seconds
/// needs the frame rate, which every encoder has, and keeping it integral means
/// the total duration of a recording is exact rather than the sum of a few
/// thousand rounded intervals.
pub struct Frame {
	pub snapshot: Arc<Snapshot>,
	pub hold: u32,
	/// Stills the tape asked for at this point, from `Screenshot`.
	///
	/// They ride on the frame rather than going out through a channel of their
	/// own because a still is the frame: it has to be the picture that was on
	/// screen when the command ran, and the encoder is already about to draw
	/// exactly that.
	pub stills: Vec<std::path::PathBuf>,
}

impl Frame {
	pub fn new(snapshot: Arc<Snapshot>, hold: u32) -> Self {
		Self {
			snapshot,
			hold,
			stills: Vec::new(),
		}
	}
}

/// What a sink is handed for each frame.
pub struct Ctx<'a> {
	pub frame: &'a Frame,
	/// The rasterized surface — present only when some sink asked for pixels.
	pub pixels: Option<&'a Pixmap>,
}

/// Everything an encoder needs to know before the first frame.
#[derive(Clone, Copy, Debug)]
pub struct Meta {
	pub width: u16,
	pub height: u16,
	pub frames_per_second: u8,
}

/// An output format.
///
/// `wants_pixels` is the whole reason this is a trait rather than an enum: SVG
/// draws from the model and never needs a surface, so a recording whose only
/// output is SVG skips rasterization entirely, while a recording that writes
/// both an SVG and an MP4 rasterizes once and shares the result.
pub trait Sink: Send {
	fn wants_pixels(&self) -> bool;
	fn begin(&mut self, meta: &Meta) -> Result<()>;
	fn accept(&mut self, ctx: Ctx<'_>) -> Result<()>;
	fn finish(self: Box<Self>) -> Result<()>;
}

/// Turns a snapshot into pixels.
///
/// A trait rather than a concrete renderer so the stream can be tested without
/// fonts, and so the encoder thread owns the renderer outright instead of
/// sharing it under a lock.
pub trait Rasterizer: Send {
	fn size(&self) -> (u16, u16);
	fn render(&mut self, snapshot: &Snapshot, target: &mut Pixmap);
}

// --- Dedup, tier T2 ---

/// The last dedup tier: reject frames that changed the grid without changing
/// the picture.
///
/// Tiers T0 and T1 live in [`crate::session`] and work off the VT core's damage
/// tracking. They catch "nothing happened". This catches something subtler and
/// just as common: a shell that repaints its prompt, a TUI that rewrites a row
/// with identical content, a cursor blink toggling back to where it started.
/// Those all mark rows dirty, so damage-based dedup passes them through, and
/// they would each cost a full rasterization.
///
/// The hash is the fast reject and the byte comparison is the confirmation.
/// Trusting the hash alone would drop a real frame on a collision and silently
/// corrupt the recording, which is not a trade worth making for one memcmp on
/// the rare frames that hash equal.
pub struct Dedup {
	level: Level,
	previous: Snapshot,
	previous_hash: u64,
	seen_any: bool,
}

impl Dedup {
	pub fn new(level: Level, columns: u16, screen_rows: u16) -> Self {
		Self {
			level,
			previous: Snapshot::new(columns, screen_rows),
			previous_hash: 0,
			seen_any: false,
		}
	}

	/// Whether `candidate` is a new picture. When it is, it becomes the one
	/// future candidates are compared against.
	pub fn admit(&mut self, candidate: &Snapshot) -> bool {
		let hash = self.fingerprint(candidate);

		if self.seen_any && hash == self.previous_hash && self.previous.same_picture(candidate) {
			return false;
		}

		self.remember(candidate, hash);
		true
	}

	/// Hash everything that can change the picture.
	///
	/// The cells go through the SIMD kernel because they are the bulk of it —
	/// a full screen is thousands of `u64`s. The cursor and style table are
	/// folded in afterwards: a cursor that moved without disturbing a cell, or
	/// a palette change that repainted every glyph without touching the packed
	/// codepoints, both have to break the hash.
	fn fingerprint(&self, snapshot: &Snapshot) -> u64 {
		let mut hash = simd::hash_cells(self.level, snapshot.raw_cells(), 0);

		let cursor = &snapshot.cursor;
		let cursor_bits = (cursor.pos.row.0 as u64) << 32
			| (cursor.pos.col.0 as u64) << 8
			| (cursor.content as u64) << 1
			| snapshot.cursor_visible as u64;

		hash = simd::hash_cells(self.level, &[cursor_bits], hash);
		hash = simd::hash_cells(self.level, &[snapshot.styles.len() as u64], hash);
		simd::hash_cells(self.level, &[snapshot.graphics.len() as u64], hash)
	}

	fn remember(&mut self, candidate: &Snapshot, hash: u64) {
		self.previous.columns = candidate.columns;
		self.previous.screen_rows = candidate.screen_rows;
		self.previous.cells.clear();
		self.previous.cells.extend_from_slice(&candidate.cells);
		self.previous.styles.clear();
		self.previous.styles.extend_from_slice(&candidate.styles);
		self.previous.extras.clone_from(&candidate.extras);
		self.previous.graphics.clear();
		self.previous.graphics.extend(candidate.graphics.iter().cloned());
		self.previous.cursor = candidate.cursor.clone();
		self.previous.cursor_visible = candidate.cursor_visible;
		self.previous_hash = hash;
		self.seen_any = true;
	}
}

// --- The encoder side ---

/// The receiving half of the pipeline, running on its own thread.
///
/// Owns the rasterizer and every sink, so nothing here is shared or locked. It
/// pulls frames until the channel closes, then finishes each sink in turn.
pub struct Encoder {
	rasterizer: Box<dyn Rasterizer>,
	sinks: Vec<Box<dyn Sink>>,
	surfaces: Arc<Pool<Pixmap>>,
	needs_pixels: bool,
	/// Frames that arrived while the previous one was still being drawn.
	/// Reported at the end so a recording that fell behind says so.
	pub stalls: usize,
}

impl Encoder {
	pub fn new(rasterizer: Box<dyn Rasterizer>, sinks: Vec<Box<dyn Sink>>) -> Self {
		let (width, height) = rasterizer.size();
		let needs_pixels = sinks.iter().any(|sink| sink.wants_pixels());

		Self {
			rasterizer,
			sinks,
			surfaces: Pool::new(move || Pixmap::new(width, height)),
			needs_pixels,
			stalls: 0,
		}
	}

	/// Consume frames until the sender hangs up, then close every sink.
	pub fn run(mut self, frames: std::sync::mpsc::Receiver<Frame>, meta: Meta) -> Result<()> {
		for sink in &mut self.sinks {
			sink.begin(&meta)?;
		}

		for frame in frames {
			// Rasterize once for however many sinks want pixels — and not at
			// all when none do, which is the whole SVG-only path. A `Screenshot`
			// forces the issue: it needs a surface even when every sink is happy
			// with the model.
			let wanted = self.needs_pixels || !frame.stills.is_empty();
			let mut surface = wanted.then(|| self.surfaces.take());
			if let Some(surface) = surface.as_mut() {
				self.rasterizer.render(&frame.snapshot, surface);

				for still in &frame.stills {
					crate::encode::png::write(still, surface)?;
				}
			}

			for sink in &mut self.sinks {
				let pixels = if sink.wants_pixels() {
					surface.as_deref()
				} else {
					None
				};
				sink.accept(Ctx {
					frame: &frame,
					pixels,
				})?;
			}
		}

		for sink in self.sinks {
			sink.finish()?;
		}
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use rio_vt::crosswords::square::Square;
	use std::sync::atomic::{AtomicUsize, Ordering};

	#[test]
	fn a_pooled_buffer_returns_itself_on_drop() {
		let pool = Pool::new(|| vec![0u8; 16]);
		assert_eq!(pool.idle_count(), 0);

		{
			let _loan = pool.take();
			assert_eq!(pool.idle_count(), 0, "a live loan is not idle");
		}
		assert_eq!(pool.idle_count(), 1, "dropping returns the buffer");

		// Taking again must reuse rather than allocate.
		let _reused = pool.take();
		assert_eq!(pool.idle_count(), 0);
	}

	#[test]
	fn a_pool_reuses_one_buffer_across_many_takes() {
		let made = Arc::new(AtomicUsize::new(0));
		let counter = Arc::clone(&made);
		let pool = Pool::new(move || {
			counter.fetch_add(1, Ordering::Relaxed);
			vec![0u8; 8]
		});

		for _ in 0..100 {
			let _loan = pool.take();
		}

		assert_eq!(
			made.load(Ordering::Relaxed),
			1,
			"a hundred sequential takes should allocate once"
		);
	}

	fn screen(columns: u16, rows: u16, fill: char) -> Snapshot {
		let mut snapshot = Snapshot::new(columns, rows);
		snapshot
			.cells
			.fill(Square::from_char(fill));
		snapshot
	}

	#[test]
	fn dedup_rejects_an_identical_repaint() {
		let mut dedup = Dedup::new(Level::new(), 8, 2);
		let frame = screen(8, 2, 'a');

		assert!(dedup.admit(&frame), "the first frame is always new");
		assert!(!dedup.admit(&frame), "an identical repaint is not a new picture");
		assert!(!dedup.admit(&frame));
	}

	#[test]
	fn dedup_admits_a_changed_cell() {
		let mut dedup = Dedup::new(Level::new(), 8, 2);
		let first = screen(8, 2, 'a');
		assert!(dedup.admit(&first));

		let mut second = screen(8, 2, 'a');
		second.cells[5] = Square::from_char('b');
		assert!(dedup.admit(&second), "one changed cell is a new picture");
	}

	/// A cursor that moves without disturbing a cell still repaints, so it has
	/// to break the fingerprint — otherwise the caret would freeze in place for
	/// the whole recording.
	#[test]
	fn dedup_admits_a_moved_cursor() {
		use rio_vt::crosswords::pos::{Column, Line, Pos};

		let mut dedup = Dedup::new(Level::new(), 8, 2);
		let mut first = screen(8, 2, ' ');
		first.cursor.pos = Pos::new(Line(0), Column(0));
		assert!(dedup.admit(&first));

		let mut second = screen(8, 2, ' ');
		second.cursor.pos = Pos::new(Line(0), Column(3));
		assert!(dedup.admit(&second), "a moved cursor is a new picture");
	}

	#[test]
	fn dedup_admits_a_hidden_cursor() {
		let mut dedup = Dedup::new(Level::new(), 8, 2);
		let mut visible = screen(8, 2, ' ');
		visible.cursor_visible = true;
		assert!(dedup.admit(&visible));

		let mut hidden = screen(8, 2, ' ');
		hidden.cursor_visible = false;
		assert!(dedup.admit(&hidden));
	}

	/// Counts what it was given, and whether it ever saw pixels.
	struct Counting {
		wants: bool,
		frames: Arc<AtomicUsize>,
		with_pixels: Arc<AtomicUsize>,
		total_hold: Arc<AtomicUsize>,
	}

	impl Sink for Counting {
		fn wants_pixels(&self) -> bool {
			self.wants
		}
		fn begin(&mut self, _meta: &Meta) -> Result<()> {
			Ok(())
		}
		fn accept(&mut self, ctx: Ctx<'_>) -> Result<()> {
			self.frames.fetch_add(1, Ordering::Relaxed);
			self.total_hold
				.fetch_add(ctx.frame.hold as usize, Ordering::Relaxed);
			if ctx.pixels.is_some() {
				self.with_pixels.fetch_add(1, Ordering::Relaxed);
			}
			Ok(())
		}
		fn finish(self: Box<Self>) -> Result<()> {
			Ok(())
		}
	}

	/// Counts how many times it was asked to draw.
	struct CountingRasterizer(Arc<AtomicUsize>);

	impl Rasterizer for CountingRasterizer {
		fn size(&self) -> (u16, u16) {
			(4, 4)
		}
		fn render(&mut self, _snapshot: &Snapshot, _target: &mut Pixmap) {
			self.0.fetch_add(1, Ordering::Relaxed);
		}
	}

	fn meta() -> Meta {
		Meta {
			width: 4,
			height: 4,
			frames_per_second: 50,
		}
	}

	/// The fanout property: two pixel sinks, one rasterization.
	#[test]
	fn two_pixel_sinks_share_a_single_rasterization() {
		let renders = Arc::new(AtomicUsize::new(0));
		let (a_frames, b_frames) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
		let pixels = Arc::new(AtomicUsize::new(0));
		let hold = Arc::new(AtomicUsize::new(0));

		let encoder = Encoder::new(
			Box::new(CountingRasterizer(Arc::clone(&renders))),
			vec![
				Box::new(Counting {
					wants: true,
					frames: Arc::clone(&a_frames),
					with_pixels: Arc::clone(&pixels),
					total_hold: Arc::clone(&hold),
				}),
				Box::new(Counting {
					wants: true,
					frames: Arc::clone(&b_frames),
					with_pixels: Arc::clone(&pixels),
					total_hold: Arc::clone(&hold),
				}),
			],
		);

		let (sender, receiver) = std::sync::mpsc::sync_channel(QUEUE_DEPTH);
		for _ in 0..3 {
			sender
				.send(Frame {
					snapshot: Arc::new(Snapshot::new(4, 4)),
					hold: 2,
				})
				.unwrap();
		}
		drop(sender);

		encoder.run(receiver, meta()).unwrap();

		assert_eq!(renders.load(Ordering::Relaxed), 3, "one render per frame");
		assert_eq!(a_frames.load(Ordering::Relaxed), 3);
		assert_eq!(b_frames.load(Ordering::Relaxed), 3);
		assert_eq!(pixels.load(Ordering::Relaxed), 6, "both sinks got pixels");
		assert_eq!(hold.load(Ordering::Relaxed), 12, "hold reaches every sink");
	}

	/// The SVG-only path: no sink wants pixels, so nothing is drawn at all.
	#[test]
	fn a_model_only_sink_skips_rasterization_entirely() {
		let renders = Arc::new(AtomicUsize::new(0));
		let frames = Arc::new(AtomicUsize::new(0));
		let pixels = Arc::new(AtomicUsize::new(0));

		let encoder = Encoder::new(
			Box::new(CountingRasterizer(Arc::clone(&renders))),
			vec![Box::new(Counting {
				wants: false,
				frames: Arc::clone(&frames),
				with_pixels: Arc::clone(&pixels),
				total_hold: Arc::new(AtomicUsize::new(0)),
			})],
		);

		let (sender, receiver) = std::sync::mpsc::sync_channel(QUEUE_DEPTH);
		for _ in 0..5 {
			sender
				.send(Frame {
					snapshot: Arc::new(Snapshot::new(4, 4)),
					hold: 1,
				})
				.unwrap();
		}
		drop(sender);

		encoder.run(receiver, meta()).unwrap();

		assert_eq!(renders.load(Ordering::Relaxed), 0, "nothing should be drawn");
		assert_eq!(frames.load(Ordering::Relaxed), 5, "the sink still sees them");
		assert_eq!(pixels.load(Ordering::Relaxed), 0);
	}

	/// Capture must not wait on encoding. With a queue this deep, a producer
	/// running well ahead of a slow consumer still never blocks.
	#[test]
	fn capture_does_not_block_on_a_slow_encoder() {
		let (sender, receiver) = std::sync::mpsc::sync_channel::<Frame>(QUEUE_DEPTH);

		let consumer = std::thread::spawn(move || {
			let mut seen = 0usize;
			for _frame in receiver {
				// Deliberately slower than the producer.
				std::thread::sleep(std::time::Duration::from_micros(200));
				seen += 1;
			}
			seen
		});

		let started = std::time::Instant::now();
		for _ in 0..QUEUE_DEPTH {
			sender
				.send(Frame {
					snapshot: Arc::new(Snapshot::new(4, 4)),
					hold: 1,
				})
				.unwrap();
		}
		let enqueue_time = started.elapsed();
		drop(sender);

		assert_eq!(consumer.join().unwrap(), QUEUE_DEPTH);
		assert!(
			enqueue_time < std::time::Duration::from_millis(20),
			"filling the queue should not wait on the consumer, took {enqueue_time:?}"
		);
	}
}
