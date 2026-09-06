//! An animated GIF sink.
//!
//! GIF is the useful middle ground between a still and a video: it is widely
//! embeddable, self-contained, and needs no browser-side player. The format's
//! centisecond clock is coarser than the capture clock, so delays are rounded
//! from the cumulative timeline rather than one frame at a time. That keeps a
//! long recording from slowly drifting away from the tape's timing.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use gif::{Encoder, Frame as GifFrame, Repeat};

use crate::stream::{Context as StreamContext, Metadata, Sink};

const GIF_CENTISECONDS_PER_SECOND: u64 = 100;

pub struct Gif {
	path: PathBuf,
	encoder: Option<Encoder<BufWriter<File>>>,
	fps: u64,
	elapsed_ticks: u64,
	written_centiseconds: u64,
	frames: usize,
	width: u16,
	height: u16,
}

impl Gif {
	pub fn new(path: PathBuf) -> Self {
		Self {
			path,
			encoder: None,
			fps: 0,
			elapsed_ticks: 0,
			written_centiseconds: 0,
			frames: 0,
			width: 0,
			height: 0,
		}
	}

	/// Convert capture ticks to GIF's centisecond clock without accumulating
	/// rounding error between adjacent frames.
	fn delay_for_ticks(&mut self, hold_ticks: u32) -> u16 {
		self.elapsed_ticks = self
			.elapsed_ticks
			.saturating_add(u64::from(hold_ticks.max(1)));
		let target = self
			.elapsed_ticks
			.saturating_mul(GIF_CENTISECONDS_PER_SECOND)
			.saturating_add(self.fps / 2)
			/ self.fps;
		let desired = target.saturating_sub(self.written_centiseconds).max(1);
		let delay = desired.min(u64::from(u16::MAX));
		self.written_centiseconds = self.written_centiseconds.saturating_add(delay);
		delay as u16
	}
}

impl Sink for Gif {
	fn requires_pixels(&self) -> bool {
		true
	}

	fn begin(&mut self, meta: &Metadata) -> Result<()> {
		if meta.frames_per_second == 0 {
			bail!("GIF output requires a non-zero frame rate");
		}

		if let Some(parent) = self
			.path
			.parent()
			.filter(|parent| !parent.as_os_str().is_empty())
		{
			std::fs::create_dir_all(parent)
				.with_context(|| format!("creating {}", parent.display()))?;
		}

		let file = File::create(&self.path)
			.with_context(|| format!("creating {}", self.path.display()))?;
		let mut encoder = Encoder::new(BufWriter::new(file), meta.width, meta.height, &[])
			.context("creating GIF encoder")?;
		encoder
			.set_repeat(Repeat::Infinite)
			.context("setting GIF to loop forever")?;

		self.encoder = Some(encoder);
		self.fps = u64::from(meta.frames_per_second);
		self.elapsed_ticks = 0;
		self.written_centiseconds = 0;
		self.frames = 0;
		self.width = meta.width;
		self.height = meta.height;
		Ok(())
	}

	fn accept(&mut self, ctx: StreamContext<'_>) -> Result<()> {
		let Some(pixels) = ctx.pixels else {
			return Ok(());
		};
		let Some(mut encoder) = self.encoder.take() else {
			bail!("GIF sink accepted a frame before begin");
		};

		let mut rgba = crate::encode::to_rgba_packed(pixels);
		let mut frame = GifFrame::from_rgba_speed(self.width, self.height, &mut rgba, 10);
		frame.delay = self.delay_for_ticks(ctx.frame.hold_ticks);
		encoder.write_frame(&frame).context("writing GIF frame")?;
		self.frames += 1;
		self.encoder = Some(encoder);
		Ok(())
	}

	fn finish(self: Box<Self>) -> Result<()> {
		if self.frames == 0 {
			bail!(
				"no frames were captured, so {} has nothing to show",
				self.path.display()
			);
		}

		let encoder = self.encoder.context("GIF sink was not started")?;
		let mut writer = encoder.into_inner().context("finishing GIF")?;
		writer.flush().context("flushing GIF output")
	}
}

#[cfg(test)]
mod tests {
	use std::io::Cursor;
	use std::sync::Arc;

	use gif::{ColorOutput, DecodeOptions, Repeat};
	use vello_cpu::Pixmap;
	use vello_cpu::color::PremulRgba8;

	use super::*;
	use crate::model::Snapshot;
	use crate::stream::{Context as StreamContext, Frame, Metadata};

	#[test]
	fn cumulative_rounding_keeps_gif_timeline_close_to_capture_timeline() {
		let mut sink = Gif::new(PathBuf::from("test.gif"));
		sink.fps = 60;

		let delays: Vec<_> = (0..60).map(|_| sink.delay_for_ticks(1)).collect();

		assert_eq!(delays.iter().map(|&delay| delay as u64).sum::<u64>(), 100);
		assert!(delays.iter().all(|&delay| delay > 0));
	}

	#[test]
	fn output_has_infinite_loop_header_and_frame_delays() {
		let path = std::env::temp_dir().join(format!(
			"dvd-gif-{}-{}.gif",
			std::process::id(),
			std::time::SystemTime::now()
				.duration_since(std::time::UNIX_EPOCH)
				.expect("clock should be after the epoch")
				.as_nanos()
		));
		let mut sink = Gif::new(path.clone());
		sink.begin(&Metadata {
			width: 2,
			height: 1,
			frames_per_second: 50,
		})
		.expect("begin");

		let snapshot = Arc::new(Snapshot::new(1, 1));
		for (hold_ticks, red) in [(1, 255), (2, 0)] {
			let mut pixmap = Pixmap::new(2, 1);
			for pixel in pixmap.data_mut() {
				*pixel = PremulRgba8 {
					r: red,
					g: 20,
					b: 30,
					a: 255,
				};
			}
			let frame = Frame::new(Arc::clone(&snapshot), hold_ticks);
			sink.accept(StreamContext {
				frame: &frame,
				pixels: Some(&pixmap),
			})
			.expect("accept");
		}
		Box::new(sink).finish().expect("finish");

		let bytes = std::fs::read(&path).expect("GIF should exist");
		assert!(bytes.starts_with(b"GIF89a"));
		assert!(
			bytes
				.windows(b"NETSCAPE2.0".len())
				.any(|window| window == b"NETSCAPE2.0")
		);

		let mut options = DecodeOptions::new();
		options.set_color_output(ColorOutput::RGBA);
		let mut reader = options
			.read_info(Cursor::new(bytes))
			.expect("GIF should decode");
		assert_eq!(reader.width(), 2);
		assert_eq!(reader.height(), 1);
		assert_eq!(reader.repeat(), Repeat::Infinite);
		let mut delays = Vec::new();
		while let Some(frame) = reader.read_next_frame().expect("frame should decode") {
			delays.push(frame.delay);
		}
		assert_eq!(delays, [2, 4]);

		std::fs::remove_file(path).expect("test output should be removable");
	}
}
