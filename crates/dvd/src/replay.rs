use crate::burn::Outputs;
use crate::cli::Output;
use crate::theme;
use anyhow::{Context, Result, bail};
use dvd_render::fonts::Fonts;
use dvd_render::grid::GridOptions;
use dvd_render::model::{Palette, Snapshot};
use dvd_render::recording::{Event, Geometry, RecordingReader, TimedEvent};
use dvd_render::render::{Renderer, Surface};
use dvd_render::rio_vt;
use dvd_render::rio_vt::crosswords::{Crosswords, CrosswordsSize};
use dvd_render::rio_vt::event::{EventListener, RioEvent, WindowId};
use dvd_render::stream::{Context as SinkContext, Frame, Metadata, Rasterizer, Sink};
use dvd_render::{Level, Pixmap};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const FPS: u8 = 30;

pub fn play(paths: &[PathBuf]) -> Result<()> {
	let stdout = io::stdout();
	let mut output = stdout.lock();
	for path in paths {
		let mut reader =
			RecordingReader::open(path).with_context(|| format!("opening {}", path.display()))?;
		let started = Instant::now();
		while let Some(event) = reader.next_event()? {
			if let Some(wait) =
				Duration::from_nanos(event.timestamp_ns).checked_sub(started.elapsed())
			{
				std::thread::sleep(wait);
			}
			match event.event {
				Event::Output(bytes) => output
					.write_all(&bytes)
					.context("writing terminal output")?,
				Event::Resize(size) => write!(output, "\x1b[8;{};{}t", size.rows, size.columns)?,
				Event::Input(_) | Event::Marker(_) | Event::Exit(_) => {}
			}
			output.flush().context("flushing terminal output")?;
		}
	}
	Ok(())
}
/// Render through the existing sinks; fixed canvases reject grid resizes.
pub fn render(recording: &Path, destination: &Path) -> Result<()> {
	let mut reader = RecordingReader::open(recording)
		.with_context(|| format!("opening {}", recording.display()))?;
	let geometry = reader.header().geometry;
	let mut events = Vec::new();
	while let Some(event) = reader.next_event()? {
		if let Event::Resize(size) = &event.event
			&& (size.columns != geometry.columns || size.rows != geometry.rows)
		{
			bail!(
				"{} changes grid size; fixed-canvas rendering does not support resizes",
				recording.display()
			);
		}
		events.push(event);
	}

	let format = destination
		.extension()
		.and_then(|part| part.to_str())
		.and_then(Output::from_extension)
		.ok_or_else(|| anyhow::anyhow!("{} must be mp4, png, or svg", destination.display()))?;
	let level = Level::new();
	let fonts = Fonts::resolve(None, 18.0, 1.0)?;
	let family = fonts.family.clone();
	let size = fonts.size;
	let palette = Palette::from_colors(&theme::resolve("dracula")?.colors());
	let options = GridOptions::default();
	let surface = Surface::default();
	let mut renderer = Renderer::new(
		fonts,
		palette.clone(),
		options,
		surface,
		geometry.columns,
		geometry.rows,
		level,
	)?;
	let canvas = renderer.size();
	let mut sinks = Outputs {
		font_family: Some(family),
		font_size: size,
		line_height: 1.0,
		palette,
		options,
		surface,
		columns: geometry.columns,
		rows: geometry.rows,
		level,
	}
	.sinks(&[(destination.to_path_buf(), format)])?;
	let meta = Metadata {
		width: canvas.width,
		height: canvas.height,
		frames_per_second: FPS,
	};
	for sink in &mut sinks {
		sink.begin(&meta)?;
	}
	frames(&events, geometry, &mut renderer, &mut sinks)?;
	for sink in sinks {
		sink.finish()?;
	}
	println!("wrote {}", destination.display());
	Ok(())
}

fn frames(
	events: &[TimedEvent],
	geometry: Geometry,
	renderer: &mut Renderer,
	sinks: &mut [Box<dyn Sink>],
) -> Result<()> {
	let mut terminal = Terminal::new(geometry);
	let last = events.last().map_or(0, |event| event.timestamp_ns);
	let count = last
		.saturating_mul(FPS as u64)
		.div_ceil(1_000_000_000)
		.saturating_add(1);
	let mut next = 0;
	let mut held = None;
	let mut hold = 0;
	let canvas = renderer.size();
	let mut pixels = Pixmap::new(canvas.width, canvas.height);

	for tick in 0..count {
		let time = tick.saturating_mul(1_000_000_000) / FPS as u64;
		while events
			.get(next)
			.is_some_and(|event| event.timestamp_ns <= time)
		{
			terminal.apply(&events[next].event);
			next += 1;
		}
		if let Some((frame, ticks)) = queue(&mut held, &mut hold, Arc::new(terminal.snapshot())) {
			draw(renderer, sinks, &mut pixels, frame, ticks)?;
		}
	}
	if let Some(frame) = held {
		draw(renderer, sinks, &mut pixels, frame, hold)?;
	}
	Ok(())
}

fn queue(
	previous: &mut Option<Arc<Snapshot>>,
	hold: &mut u32,
	next: Arc<Snapshot>,
) -> Option<(Arc<Snapshot>, u32)> {
	if previous.as_ref().is_some_and(|old| old.same_picture(&next)) {
		*hold = hold.saturating_add(1);
		None
	} else {
		let result = previous.replace(next).map(|old| (old, *hold));
		*hold = 1;
		result
	}
}

fn draw(
	renderer: &mut Renderer,
	sinks: &mut [Box<dyn Sink>],
	pixels: &mut Pixmap,
	snapshot: Arc<Snapshot>,
	hold: u32,
) -> Result<()> {
	let frame = Frame::new(snapshot, hold);
	if sinks.iter().any(|sink| sink.requires_pixels()) {
		renderer.render(&frame.snapshot, pixels);
	}
	for sink in sinks {
		sink.accept(SinkContext {
			frame: &frame,
			pixels: sink.requires_pixels().then_some(&*pixels),
		})?;
	}
	Ok(())
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
	geometry: Geometry,
}
impl Terminal {
	fn new(geometry: Geometry) -> Self {
		Self {
			vt: Crosswords::new(
				CrosswordsSize::new(geometry.columns as usize, geometry.rows as usize),
				rio_vt::ansi::CursorShape::Block,
				Events,
				WindowId::from(0),
				0,
				1_000,
			),
			geometry,
		}
	}
	fn apply(&mut self, event: &Event) {
		if let Event::Output(bytes) = event {
			rio_vt::performer::handler::Processor::default().advance(&mut self.vt, bytes);
		}
	}
	fn snapshot(&mut self) -> Snapshot {
		let mut shot = Snapshot::new(self.geometry.columns, self.geometry.rows);
		self.vt.snapshot_visible(
			&rio_vt::event::TerminalDamage::Full,
			self.geometry.columns as usize,
			&mut shot.rows,
			&mut shot.styles,
			&mut shot.extras,
		);
		shot.cursor = self.vt.cursor();
		shot.cursor_visible = !matches!(shot.cursor.content, rio_vt::ansi::CursorShape::Hidden);
		shot.flatten();
		shot
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	fn shot() -> Arc<Snapshot> {
		Arc::new(Snapshot::new(2, 1))
	}
	#[test]
	fn idle_samples_become_one_held_frame() {
		let (mut previous, mut hold) = (None, 0);
		for _ in 0..301 {
			assert!(queue(&mut previous, &mut hold, shot()).is_none());
		}
		assert_eq!(hold, 301);
	}
}
