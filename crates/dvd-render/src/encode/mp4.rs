//! H.264 in an MP4 container, muxed in process.
//!
//! `less-avc` encodes; the `mp4` crate muxes. Neither pulls in libav, which is
//! the whole reason for the pairing — a recorder that shells out to `ffmpeg`
//! only works on machines that have it.
//!
//! What `less-avc` gives up in exchange is inter-frame prediction: every frame
//! is coded as lossless I_PCM, so every frame is an IDR and the file is large
//! in a way an x264 file would not be. For terminal recordings that trade is
//! less bad than it sounds — the dedup stages upstream mean the encoder only
//! ever sees frames that actually differ, and a still prompt costs one frame no
//! matter how long it is held.
//!
//! The awkward part is shape. H.264 codes whole 16x16 macroblocks, so the
//! planes have to be padded out even when the canvas is not a multiple of 16.
//! The padding is written once, at the start, and every frame is converted
//! straight into the middle of it — the alternative, converting tightly and
//! then copying row by row into a padded buffer, is a second full-frame pass
//! per frame for no gain.

use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;

use anyhow::{Context, Result};
use fearless_simd::Level;
use less_avc::ycbcr_image::{DataPlane, Planes, YCbCrImage};
use less_avc::{BitDepth, LessEncoder};
use mp4::{AvcConfig, MediaConfig, Mp4Config, Mp4Sample, Mp4Writer, TrackConfig, TrackType};

use crate::model::Rgba;
use crate::stream::{Ctx, Meta, Sink};

/// The macroblock edge every plane dimension is rounded up to.
const MACROBLOCK: u32 = 16;

/// Ticks per second on the video track.
///
/// A multiple of the frame rate rather than the conventional 1000, so a frame
/// held for `n` capture ticks lasts exactly `n` ticks of the track and the
/// recording's duration is the tape's duration rather than the sum of a few
/// thousand roundings.
const TICKS_PER_FRAME: u32 = 1000;

fn padded(value: u32) -> u32 {
	value.div_ceil(MACROBLOCK) * MACROBLOCK
}

pub struct Mp4 {
	path: PathBuf,
	level: Level,

	width: u32,
	height: u32,
	stride: u32,
	rows: u32,
	timescale: u32,

	/// Straight RGBA at the padded size, reused for every frame.
	rgba: Vec<u8>,
	luma: Vec<u8>,
	cb: Vec<u8>,
	cr: Vec<u8>,

	/// Both are created from the first frame: the encoder infers its parameter
	/// sets from the image it is handed, and the track cannot be declared until
	/// those exist.
	encoder: Option<LessEncoder>,
	writer: Option<Mp4Writer<BufWriter<File>>>,
	elapsed: u64,
	/// What the padding is filled with, so a canvas that is not a multiple of
	/// sixteen does not get a black stripe bleeding into the edge macroblocks.
	edge: Rgba,
}

impl Mp4 {
	pub fn new(path: PathBuf, level: Level, edge: Rgba) -> Self {
		Self {
			path,
			level,
			width: 0,
			height: 0,
			stride: 0,
			rows: 0,
			timescale: 0,
			rgba: Vec::new(),
			luma: Vec::new(),
			cb: Vec::new(),
			cr: Vec::new(),
			encoder: None,
			writer: None,
			elapsed: 0,
			edge,
		}
	}

}

/// Describe the planes to the encoder.
///
/// A free function rather than a method so it borrows the three plane fields
/// and nothing else — `accept` needs the encoder mutably at the same moment it
/// needs the planes to read from, and a `&self` method would put both behind
/// one borrow of the whole struct.
fn image<'a>(
	luma: &'a [u8],
	cb: &'a [u8],
	cr: &'a [u8],
	width: u32,
	height: u32,
	stride: u32,
) -> YCbCrImage<'a> {
	let plane = |data: &'a [u8], stride: usize| DataPlane {
		data,
		stride,
		bit_depth: BitDepth::Depth8,
	};

	YCbCrImage {
		// The *visible* size. Everything past it is padding, and the cropping
		// the encoder writes into the SPS is derived from exactly this pair —
		// which is why the strides are the padded ones and these two are not.
		width,
		height,
		planes: Planes::YCbCr((
			plane(luma, stride as usize),
			plane(cb, (stride / 2) as usize),
			plane(cr, (stride / 2) as usize),
		)),
	}
}

impl Sink for Mp4 {
	fn wants_pixels(&self) -> bool {
		true
	}

	fn begin(&mut self, meta: &Meta) -> Result<()> {
		// `less-avc` refuses an odd width outright, and an odd height would put
		// the chroma planes half a row out. The renderer already rounds its
		// canvas to an even size for exactly this reason; this is the check that
		// keeps the two facts tied together.
		anyhow::ensure!(
			meta.width % 2 == 0 && meta.height % 2 == 0,
			"h.264 needs an even canvas, got {}x{}",
			meta.width,
			meta.height
		);
		anyhow::ensure!(meta.frames_per_second > 0, "the frame rate must be positive");

		self.width = meta.width as u32;
		self.height = meta.height as u32;
		self.stride = padded(self.width);
		self.rows = padded(self.height);
		self.timescale = meta.frames_per_second as u32 * TICKS_PER_FRAME;

		let chroma = (self.stride / 2 * self.rows / 2) as usize;
		self.luma = vec![0u8; (self.stride * self.rows) as usize];
		self.cb = vec![128u8; chroma];
		self.cr = vec![128u8; chroma];

		// Pre-fill the whole padded surface with the edge colour, then never
		// touch the padding again — `to_rgba` only writes the image rectangle.
		self.rgba = self
			.edge
			.iter()
			.copied()
			.cycle()
			.take((self.stride * self.rows * 4) as usize)
			.collect();

		Ok(())
	}

	fn accept(&mut self, ctx: Ctx<'_>) -> Result<()> {
		let Some(pixels) = ctx.pixels else {
			return Ok(());
		};

		let stride_bytes = self.stride as usize * 4;
		super::to_rgba(pixels, &mut self.rgba, stride_bytes);

		crate::simd::rgba_to_yuv420(
			self.level,
			&self.rgba,
			self.stride as usize,
			self.rows as usize,
			&mut self.luma,
			&mut self.cb,
			&mut self.cr,
		);

		let frame = image(
			&self.luma,
			&self.cb,
			&self.cr,
			self.width,
			self.height,
			self.stride,
		);

		let nal = match self.encoder.as_mut() {
			Some(encoder) => encoder
				.encode(&frame)
				.map_err(|error| anyhow::anyhow!("encoding a frame: {error}"))?,
			None => {
				let (initial, encoder) = LessEncoder::new(&frame)
					.map_err(|error| anyhow::anyhow!("starting the h.264 encoder: {error}"))?;

				let file = File::create(&self.path)
					.with_context(|| format!("creating {}", self.path.display()))?;
				let mut writer = Mp4Writer::write_start(
					BufWriter::new(file),
					&Mp4Config {
						major_brand: str::parse("isom").expect("a four-character brand"),
						minor_version: 512,
						compatible_brands: ["isom", "iso2", "avc1", "mp41"]
							.iter()
							.map(|brand| str::parse(brand).expect("a four-character brand"))
							.collect(),
						timescale: 1000,
					},
				)
				.context("starting the mp4 container")?;

				// The parameter sets go in the sample description, not the
				// stream: an MP4 reader expects them in `avcC` and length-
				// prefixed samples that carry no start codes.
				writer
					.add_track(&TrackConfig {
						track_type: TrackType::Video,
						timescale: self.timescale,
						language: "und".to_string(),
						media_conf: MediaConfig::AvcConfig(AvcConfig {
							width: self.width as u16,
							height: self.height as u16,
							seq_param_set: initial.sps.to_nal_unit(),
							pic_param_set: initial.pps.to_nal_unit(),
						}),
					})
					.context("declaring the video track")?;

				self.writer = Some(writer);
				self.encoder = Some(encoder);
				initial.frame
			}
		};

		let unit = nal.to_nal_unit();
		let mut sample = Vec::with_capacity(unit.len() + 4);
		sample.extend_from_slice(&(unit.len() as u32).to_be_bytes());
		sample.extend_from_slice(&unit);

		let duration = ctx.frame.hold.max(1) * TICKS_PER_FRAME;
		let writer = self
			.writer
			.as_mut()
			.expect("the writer is created alongside the encoder");

		writer
			.write_sample(
				1,
				&Mp4Sample {
					start_time: self.elapsed,
					duration,
					rendering_offset: 0,
					// Every frame `less-avc` produces is a coded IDR slice, so
					// every one of them is a seek point.
					is_sync: true,
					bytes: sample.into(),
				},
			)
			.context("writing a video sample")?;

		self.elapsed += duration as u64;
		Ok(())
	}

	fn finish(self: Box<Self>) -> Result<()> {
		let Some(mut writer) = self.writer else {
			anyhow::bail!(
				"no frames were captured, so {} has nothing to show",
				self.path.display()
			);
		};

		writer.write_end().context("finalising the mp4 container")?;

		// `Mp4Writer` wraps a `BufWriter`, and dropping one swallows the error
		// from its final flush. Everything written so far is worthless if that
		// last write fails, so it is taken back and flushed explicitly.
		use std::io::Write;
		writer
			.into_writer()
			.flush()
			.with_context(|| format!("flushing {}", self.path.display()))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn padding_rounds_up_to_whole_macroblocks() {
		assert_eq!(padded(16), 16, "an exact fit is left alone");
		assert_eq!(padded(1), 16);
		assert_eq!(padded(17), 32);
		assert_eq!(padded(1200), 1200);
		assert_eq!(padded(602), 608);
	}
}
