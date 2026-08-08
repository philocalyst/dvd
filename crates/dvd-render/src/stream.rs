//! The spine: one stream of frames, fanned out to every encoder.
//!
//! Capture and encoding run on different threads and never wait on each other
//! in the steady state. That is not a nicety — the capture loop is what keeps
//! the tape's timing honest. If it stalled behind a rasterization, the child
//! process would keep running while the recorder wasn't looking, and the
//! recording would drift away from the script that produced it.

use anyhow::Result;
use crossbeam::queue::SegQueue; // Replaces Mutex for lock-free pooling
use fearless_simd::Level;
use std::sync::Arc;
use vello_cpu::Pixmap;

use crate::model::Snapshot;
use crate::simd;

/// Distinct frames the encoder may fall behind by before capture waits.
pub const MAXIMUM_QUEUE_DEPTH: usize = 128;

// Bit-shift constants for cursor hashing
const CURSOR_ROW_SHIFT: u8 = 32;
const CURSOR_COLUMN_SHIFT: u8 = 8;
const CURSOR_CONTENT_SHIFT: u8 = 1;

// --- Buffer pooling ---

/// A lock-free pool of reusable buffers.
///
/// Buffers taken from here return themselves on drop, ensuring zero
/// steady-state allocation without thread contention.
pub struct Pool<Type> {
	idle_items: SegQueue<Type>,
	make_item: Box<dyn Fn() -> Type + Send + Sync>,
}

impl<Type> Pool<Type> {
	pub fn new(make_item: impl Fn() -> Type + Send + Sync + 'static) -> Arc<Self> {
		Arc::new(Self {
			idle_items: SegQueue::new(),
			make_item: Box::new(make_item),
		})
	}

	/// Take a buffer, making a fresh one only if none are idle.
	pub fn take(self: &Arc<Self>) -> Pooled<Type> {
		let item = self.idle_items.pop().unwrap_or_else(|| (self.make_item)());
		Pooled {
			item: Some(item),
			pool: Arc::clone(self),
		}
	}

	/// How many buffers are currently idle.
	pub fn idle_count(&self) -> usize {
		self.idle_items.len()
	}
}

/// A buffer on loan from a [`Pool`], returned when dropped.
pub struct Pooled<Type> {
	item: Option<Type>,
	pool: Arc<Pool<Type>>,
}

impl<Type> std::ops::Deref for Pooled<Type> {
	type Target = Type;
	fn deref(&self) -> &Type {
		self.item.as_ref().expect("Live loan always holds its item")
	}
}

impl<Type> std::ops::DerefMut for Pooled<Type> {
	fn deref_mut(&mut self) -> &mut Type {
		self.item.as_mut().expect("Live loan always holds its item")
	}
}

impl<Type> Drop for Pooled<Type> {
	fn drop(&mut self) {
		if let Some(item) = self.item.take() {
			self.pool.idle_items.push(item);
		}
	}
}

impl<Type: std::fmt::Debug> std::fmt::Debug for Pooled<Type> {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		self.item.fmt(formatter)
	}
}

// --- Frames ---

/// One distinct picture, and how long it stays on screen.
pub struct Frame {
	pub snapshot: Arc<Snapshot>,
	pub hold_ticks: u32,
	pub stills: Vec<std::path::PathBuf>,
}

impl Frame {
	pub fn new(snapshot: Arc<Snapshot>, hold_ticks: u32) -> Self {
		Self {
			snapshot,
			hold_ticks,
			stills: Vec::new(),
		}
	}
}

/// What a sink is handed for each frame.
pub struct Context<'a> {
	pub frame: &'a Frame,
	pub pixels: Option<&'a Pixmap>,
}

/// Everything an encoder needs to know before the first frame.
#[derive(Clone, Copy, Debug)]
pub struct Metadata {
	pub width: u16,
	pub height: u16,
	pub frames_per_second: u8,
}

pub trait Sink: Send {
	fn requires_pixels(&self) -> bool;
	fn begin(&mut self, metadata: &Metadata) -> Result<()>;
	fn accept(&mut self, context: Context<'_>) -> Result<()>;
	fn finish(self: Box<Self>) -> Result<()>;
}

pub trait Rasterizer: Send {
	fn dimensions(&self) -> (u16, u16);
	fn render(&mut self, snapshot: &Snapshot, target: &mut Pixmap);
}

// --- Deduplication ---

/// The final deduplication tier.
pub struct Deduplicator {
	level: Level,
	/// Holding an Arc eliminates the massive per-frame memory copying
	/// required when structurally cloning the grid for caching.
	previous_snapshot: Option<Arc<Snapshot>>,
	previous_hash: u64,
}

impl Deduplicator {
	pub fn new(level: Level) -> Self {
		Self {
			level,
			previous_snapshot: None,
			previous_hash: 0,
		}
	}

	/// Checks if a candidate is a new picture. A duplicate costs only the
	/// fingerprint hash; the snapshot is copied into the Arc kept for the
	/// next comparison only when it turns out to be a new picture.
	pub fn admit(&mut self, candidate: &Snapshot) -> bool {
		let hash = self.calculate_fingerprint(candidate);

		if self.is_duplicate(candidate, hash) {
			return false;
		}

		self.previous_hash = hash;
		self.previous_snapshot = Some(Arc::new(Self::remember(candidate)));

		true
	}

	/// Copy the fields that matter for future comparisons. Deliberately
	/// skips `rows`, which is capture scratch and never part of the picture.
	fn remember(candidate: &Snapshot) -> Snapshot {
		Snapshot {
			columns: candidate.columns,
			screen_rows: candidate.screen_rows,
			cells: candidate.cells.clone(),
			styles: candidate.styles.clone(),
			extras: candidate.extras.clone(),
			cursor: candidate.cursor.clone(),
			cursor_visible: candidate.cursor_visible,
			graphics: candidate.graphics.clone(),
			rows: Vec::new(),
		}
	}

	fn is_duplicate(&self, candidate: &Snapshot, hash: u64) -> bool {
		if let Some(previous) = &self.previous_snapshot {
			hash == self.previous_hash && previous.same_picture(candidate)
		} else {
			false
		}
	}

	fn calculate_fingerprint(&self, snapshot: &Snapshot) -> u64 {
		let mut hash = simd::hash_cells(self.level, snapshot.raw_cells(), 0);

		hash = self.hash_cursor(snapshot, hash);
		hash = simd::hash_cells(self.level, &[snapshot.styles.len() as u64], hash);

		simd::hash_cells(self.level, &[snapshot.graphics.len() as u64], hash)
	}

	fn hash_cursor(&self, snapshot: &Snapshot, previous_hash: u64) -> u64 {
		let cursor = &snapshot.cursor;
		let cursor_bits = (cursor.pos.row.0 as u64) << CURSOR_ROW_SHIFT
			| (cursor.pos.col.0 as u64) << CURSOR_COLUMN_SHIFT
			| (cursor.content as u64) << CURSOR_CONTENT_SHIFT
			| snapshot.cursor_visible as u64;

		simd::hash_cells(self.level, &[cursor_bits], previous_hash)
	}
}

// --- The encoder side ---

pub struct Encoder {
	rasterizer: Box<dyn Rasterizer>,
	sinks: Vec<Box<dyn Sink>>,
	surfaces: Arc<Pool<Pixmap>>,
	requires_pixels: bool,
	pub stalled_frames: usize,
}

impl Encoder {
	pub fn new(rasterizer: Box<dyn Rasterizer>, sinks: Vec<Box<dyn Sink>>) -> Self {
		let (width, height) = rasterizer.dimensions();
		let requires_pixels = sinks.iter().any(|sink| sink.requires_pixels());

		Self {
			rasterizer,
			sinks,
			surfaces: Pool::new(move || Pixmap::new(width, height)),
			requires_pixels,
			stalled_frames: 0,
		}
	}

	/// Consume frames until the sender hangs up, then close every sink.
	pub fn run(
		mut self,
		frames: std::sync::mpsc::Receiver<Frame>,
		metadata: Metadata,
	) -> Result<()> {
		self.initialize_sinks(&metadata)?;

		for frame in frames {
			self.process_frame(&frame)?;
		}

		self.finalize_sinks()
	}

	fn initialize_sinks(&mut self, metadata: &Metadata) -> Result<()> {
		for sink in &mut self.sinks {
			sink.begin(metadata)?;
		}
		Ok(())
	}

	fn process_frame(&mut self, frame: &Frame) -> Result<()> {
		let needs_surface = self.requires_pixels || !frame.stills.is_empty();
		let mut surface_loan = needs_surface.then(|| self.surfaces.take());

		if let Some(surface) = surface_loan.as_mut() {
			self.render_and_save_stills(frame, surface)?;
		}

		self.dispatch_to_sinks(frame, surface_loan.as_deref())
	}

	fn render_and_save_stills(&mut self, frame: &Frame, surface: &mut Pixmap) -> Result<()> {
		self.rasterizer.render(&frame.snapshot, surface);

		for still_path in &frame.stills {
			crate::encode::png::write(still_path, surface)?;
		}
		Ok(())
	}

	fn dispatch_to_sinks(&mut self, frame: &Frame, pixels: Option<&Pixmap>) -> Result<()> {
		for sink in &mut self.sinks {
			// Elegant one-liner: only pass pixels if the sink specifically asks for them
			let sink_pixels = sink.requires_pixels().then_some(pixels).flatten();

			sink.accept(Context {
				frame,
				pixels: sink_pixels,
			})?;
		}
		Ok(())
	}

	fn finalize_sinks(self) -> Result<()> {
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
		snapshot.cells.fill(Square::from_char(fill));
		snapshot
	}

	#[test]
	fn dedup_rejects_an_identical_repaint() {
		let mut dedup = Deduplicator::new(Level::new());
		let frame = screen(8, 2, 'a');

		assert!(dedup.admit(&frame), "the first frame is always new");
		assert!(
			!dedup.admit(&frame),
			"an identical repaint is not a new picture"
		);
		assert!(!dedup.admit(&frame));
	}

	#[test]
	fn dedup_admits_a_changed_cell() {
		let mut dedup = Deduplicator::new(Level::new());
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

		let mut dedup = Deduplicator::new(Level::new());
		let mut first = screen(8, 2, ' ');
		first.cursor.pos = Pos::new(Line(0), Column(0));
		assert!(dedup.admit(&first));

		let mut second = screen(8, 2, ' ');
		second.cursor.pos = Pos::new(Line(0), Column(3));
		assert!(dedup.admit(&second), "a moved cursor is a new picture");
	}

	#[test]
	fn dedup_admits_a_hidden_cursor() {
		let mut dedup = Deduplicator::new(Level::new());
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
		fn requires_pixels(&self) -> bool {
			self.wants
		}
		fn begin(&mut self, _meta: &Metadata) -> Result<()> {
			Ok(())
		}
		fn accept(&mut self, ctx: Context<'_>) -> Result<()> {
			self.frames.fetch_add(1, Ordering::Relaxed);
			self.total_hold
				.fetch_add(ctx.frame.hold_ticks as usize, Ordering::Relaxed);
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
		fn dimensions(&self) -> (u16, u16) {
			(4, 4)
		}
		fn render(&mut self, _snapshot: &Snapshot, _target: &mut Pixmap) {
			self.0.fetch_add(1, Ordering::Relaxed);
		}
	}

	fn meta() -> Metadata {
		Metadata {
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

		let (sender, receiver) = std::sync::mpsc::sync_channel(MAXIMUM_QUEUE_DEPTH);
		for _ in 0..3 {
			sender
				.send(Frame {
					snapshot: Arc::new(Snapshot::new(4, 4)),
					hold_ticks: 2,
					stills: Vec::new(),
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

		let (sender, receiver) = std::sync::mpsc::sync_channel(MAXIMUM_QUEUE_DEPTH);
		for _ in 0..5 {
			sender
				.send(Frame {
					snapshot: Arc::new(Snapshot::new(4, 4)),
					hold_ticks: 1,
					stills: Vec::new(),
				})
				.unwrap();
		}
		drop(sender);

		encoder.run(receiver, meta()).unwrap();

		assert_eq!(
			renders.load(Ordering::Relaxed),
			0,
			"nothing should be drawn"
		);
		assert_eq!(
			frames.load(Ordering::Relaxed),
			5,
			"the sink still sees them"
		);
		assert_eq!(pixels.load(Ordering::Relaxed), 0);
	}

	/// Capture must not wait on encoding. With a queue this deep, a producer
	/// running well ahead of a slow consumer still never blocks.
	#[test]
	fn capture_does_not_block_on_a_slow_encoder() {
		let (sender, receiver) = std::sync::mpsc::sync_channel::<Frame>(MAXIMUM_QUEUE_DEPTH);

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
		for _ in 0..MAXIMUM_QUEUE_DEPTH {
			sender
				.send(Frame {
					snapshot: Arc::new(Snapshot::new(4, 4)),
					hold_ticks: 1,
					stills: Vec::new(),
				})
				.unwrap();
		}
		let enqueue_time = started.elapsed();
		drop(sender);

		assert_eq!(consumer.join().unwrap(), MAXIMUM_QUEUE_DEPTH);
		assert!(
			enqueue_time < std::time::Duration::from_millis(20),
			"filling the queue should not wait on the consumer, took {enqueue_time:?}"
		);
	}
}
