//! H.264 in an MP4 container, muxed in process.
//!
//! Two encoders live behind [`Mp4Profile`], and the `mp4` crate muxes for
//! both — neither pulls in libav, which is the whole reason for the pairing:
//! a recorder that shells out to `ffmpeg` only works on machines that have
//! it.
//!
//! [`Mp4Profile::Lossless`] is `less-avc`, unchanged: every frame is coded as
//! I_PCM macroblocks with no inter-frame prediction at all, so every frame is
//! an IDR and always decodes on its own. It is kept exactly as it was because
//! it is the fallback that always works, with no assumption about what a
//! decoder does with the frame before it.
//!
//! [`Mp4Profile::Delta`], the default, is [`crate::encode::h264`] — a second,
//! from-scratch encoder built for this crate. It still codes every changed
//! macroblock as I_PCM, so it is exactly as lossless as `less-avc`, but a
//! macroblock byte-identical to the reference picture costs a run-length
//! integer instead of 384 bytes of raw samples. For a terminal recording,
//! where dedup upstream already guarantees every frame handed to the encoder
//! changed *something*, but a keystroke still only touches one or two
//! macroblocks out of a couple of thousand, that difference is the entire
//! reason this module has two profiles instead of one.
//!
//! `Delta` cannot reuse `less-avc`'s SPS/PPS for its own frames, and doesn't
//! try to: `avcC` binds one parameter-set pair to the whole track, declared
//! once when the track is added, so mixing a `less-avc`-derived pair with
//! frames coded by a different encoder would leave the track description and
//! the samples silently describing two different bitstreams. Each profile
//! builds its own parameter sets and neither is ever handed to the other's
//! encoder.
//!
//! The awkward part shared by both is shape. H.264 codes whole 16x16
//! macroblocks, so the planes have to be padded out even when the canvas is
//! not a multiple of 16. The padding is written once, at the start, and every
//! frame is converted straight into the middle of it — the alternative,
//! converting tightly and then copying row by row into a padded buffer, is a
//! second full-frame pass per frame for no gain.

use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;

use anyhow::{Context, Result};
use fearless_simd::Level;
use less_avc::ycbcr_image::{DataPlane, Planes, YCbCrImage};
use less_avc::{BitDepth, LessEncoder};
use mp4::{AvcConfig, MediaConfig, Mp4Config, Mp4Sample, Mp4Writer, TrackConfig, TrackType};

use super::h264;
use crate::geom::Color;
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

/// How much the encoder is allowed to assume about the previous frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Mp4Profile {
	/// Every frame an independent IDR, coded by `less-avc`. Always works, and
	/// never assumes a decoder still has the previous frame around.
	Lossless,
	/// P slices that skip macroblocks byte-identical to the frame before,
	/// coded by [`crate::encode::h264`]. The size win a terminal recording
	/// exists for; see the module doc for why it is still exactly lossless.
	#[default]
	Delta,
}

/// The two encoders behind [`Mp4Profile`], boxed together so `accept` has one
/// thing to match on instead of two independently-optional fields that could,
/// in principle, disagree about which profile is running.
enum Codec {
	Lossless(LessEncoder),
	Delta(h264::Encoder),
}

pub struct Mp4 {
	path: PathBuf,
	level: Level,
	profile: Mp4Profile,

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

	/// Both are created from the first frame: `less-avc` infers its parameter
	/// sets from the image it is handed, and although `h264::Encoder` does
	/// not need a frame to build its own, the track still cannot be declared
	/// until a codec — and so a parameter set — exists, and keeping both
	/// profiles on the same "first frame creates everything" path is what
	/// keeps this struct's state machine to one shape instead of two.
	codec: Option<Codec>,
	writer: Option<Mp4Writer<BufWriter<File>>>,
	elapsed: u64,
	/// What the padding is filled with, so a canvas that is not a multiple of
	/// sixteen does not get a black stripe bleeding into the edge macroblocks.
	edge: Color,
}

impl Mp4 {
	pub fn new(path: PathBuf, level: Level, edge: Color) -> Self {
		Self {
			path,
			level,
			profile: Mp4Profile::default(),
			width: 0,
			height: 0,
			stride: 0,
			rows: 0,
			timescale: 0,
			rgba: Vec::new(),
			luma: Vec::new(),
			cb: Vec::new(),
			cr: Vec::new(),
			codec: None,
			writer: None,
			elapsed: 0,
			edge,
		}
	}

	/// Run with a different encoding profile. `Delta` is the default; this is
	/// how a caller reaches back to the always-works `Lossless` fallback.
	pub fn with_profile(mut self, profile: Mp4Profile) -> Self {
		self.profile = profile;
		self
	}
}

/// Describe the planes to `less-avc`.
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

/// Length-prefix a NAL unit the way an MP4 sample wants it: no start code,
/// just a four-byte big-endian size ahead of the bytes themselves. `avcC`
/// already carries the parameter sets, so every sample is exactly one slice
/// NAL unit framed this way.
fn length_prefixed(nal: &[u8]) -> Vec<u8> {
	let mut sample = Vec::with_capacity(nal.len() + 4);
	sample.extend_from_slice(&(nal.len() as u32).to_be_bytes());
	sample.extend_from_slice(nal);
	sample
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
			meta.width.is_multiple_of(2) && meta.height.is_multiple_of(2),
			"h.264 needs an even canvas, got {}x{}",
			meta.width,
			meta.height
		);
		anyhow::ensure!(
			meta.frames_per_second > 0,
			"the frame rate must be positive"
		);

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
			.channels()
			.into_iter()
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

		let (nal, is_sync) = match self.codec.as_mut() {
			Some(Codec::Lossless(encoder)) => {
				let frame = image(
					&self.luma,
					&self.cb,
					&self.cr,
					self.width,
					self.height,
					self.stride,
				);
				let nal = encoder
					.encode(&frame)
					.map_err(|error| anyhow::anyhow!("encoding a frame: {error}"))?;
				(nal.to_nal_unit(), true)
			}
			Some(Codec::Delta(encoder)) => encoder.encode(&self.luma, &self.cb, &self.cr),
			None => {
				// The parameter sets go in the sample description, not the
				// stream: an MP4 reader expects them in `avcC` and length-
				// prefixed samples that carry no start codes.
				let (first_nal, first_is_sync, seq_param_set, pic_param_set) = match self.profile {
					Mp4Profile::Lossless => {
						let frame = image(
							&self.luma,
							&self.cb,
							&self.cr,
							self.width,
							self.height,
							self.stride,
						);
						let (initial, encoder) = LessEncoder::new(&frame).map_err(|error| {
							anyhow::anyhow!("starting the h.264 encoder: {error}")
						})?;
						self.codec = Some(Codec::Lossless(encoder));
						(
							initial.frame.to_nal_unit(),
							true,
							initial.sps.to_nal_unit(),
							initial.pps.to_nal_unit(),
						)
					}
					Mp4Profile::Delta => {
						let mut encoder =
							h264::Encoder::new(self.width, self.height, self.stride, self.rows);
						let parameters = encoder.parameter_sets();
						let (nal, is_sync) = encoder.encode(&self.luma, &self.cb, &self.cr);
						self.codec = Some(Codec::Delta(encoder));
						(nal, is_sync, parameters.sps, parameters.pps)
					}
				};

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

				writer
					.add_track(&TrackConfig {
						track_type: TrackType::Video,
						timescale: self.timescale,
						language: "und".to_string(),
						media_conf: MediaConfig::AvcConfig(AvcConfig {
							width: self.width as u16,
							height: self.height as u16,
							seq_param_set,
							pic_param_set,
						}),
					})
					.context("declaring the video track")?;

				self.writer = Some(writer);
				(first_nal, first_is_sync)
			}
		};

		let sample = length_prefixed(&nal);

		let duration = ctx.frame.hold.max(1) * TICKS_PER_FRAME;
		let writer = self
			.writer
			.as_mut()
			.expect("the writer is created alongside the codec");

		writer
			.write_sample(
				1,
				&Mp4Sample {
					start_time: self.elapsed,
					duration,
					rendering_offset: 0,
					// Under `Lossless` every frame is an IDR, so every sample
					// is a sync point. Under `Delta` only IDRs are — `stss`
					// (built by the `mp4` crate from this flag) has to name
					// exactly those, or a seek into the middle of a P-slice
					// run would hand a decoder a picture it cannot start
					// from.
					is_sync,
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

	#[test]
	fn the_default_profile_is_delta() {
		assert_eq!(Mp4Profile::default(), Mp4Profile::Delta);
	}
}

/// End-to-end checks that shell out to `ffmpeg`/`ffprobe` to decode what this
/// module wrote and compare it against what went in.
///
/// Nothing above this point proves the bitstream this crate emits actually
/// decodes anywhere else — bit-level unit tests can be internally consistent
/// and still describe a stream no real decoder accepts. These tests are the
/// only ones in the module that would catch that, which is also exactly why
/// they cannot be hermetic: they need a real H.264 decoder, and the point is
/// to trust a *different* implementation of the spec than the one under test.
/// `#[ignore]` keeps them out of the default `cargo test` run — run them with
/// `cargo test -- --ignored`.
///
/// Every comparison here is against the padded YUV planes `Mp4` itself would
/// have computed, cropped down to the visible rectangle — not against the
/// original RGBA. Chroma subsampling makes RGB -> YUV -> RGB lossy on its own
/// terms, independently of anything this encoder does, and proving that
/// round-trip is `simd.rs`'s job, not this module's. "Lossless" here means:
/// the exact YUV bytes this encoder was handed are the exact YUV bytes a
/// decoder reads back.
#[cfg(test)]
mod verification {
	use std::path::Path;
	use std::process::Command;
	use std::sync::Arc;

	use vello_cpu::Pixmap;
	use vello_cpu::color::PremulRgba8;

	use super::*;
	use crate::model::Snapshot;
	use crate::stream::Frame;

	const FFMPEG: &str = "/opt/homebrew/bin/ffmpeg";
	const FFPROBE: &str = "/opt/homebrew/bin/ffprobe";

	fn padded16(value: u32) -> u32 {
		value.div_ceil(16) * 16
	}

	/// Drive the real `Mp4` sink exactly the way `stream::Encoder` does —
	/// `begin`, then one `accept` per frame, then `finish` — over a sequence
	/// of straight-RGBA8 frames (`width * height * 4` bytes each, alpha
	/// always 255 so premultiplication is a no-op and cannot itself introduce
	/// a discrepancy).
	fn encode(
		path: &Path,
		profile: Mp4Profile,
		width: u16,
		height: u16,
		frames: &[Vec<u8>],
	) -> Result<()> {
		let mut sink =
			Mp4::new(path.to_path_buf(), Level::new(), Color::BLACK).with_profile(profile);
		sink.begin(&Meta {
			width,
			height,
			frames_per_second: 50,
		})?;

		// The sink never reads the snapshot for a pixel frame — see `Ctx`'s
		// doc in `stream.rs` — so an empty one shared across every frame
		// costs nothing and changes nothing about what is being tested.
		let snapshot = Arc::new(Snapshot::new(1, 1));
		for rgba in frames {
			let mut pixmap = Pixmap::new(width, height);
			for (slot, chunk) in pixmap.data_mut().iter_mut().zip(rgba.chunks_exact(4)) {
				*slot = PremulRgba8 {
					r: chunk[0],
					g: chunk[1],
					b: chunk[2],
					a: chunk[3],
				};
			}

			let frame = Frame::new(Arc::clone(&snapshot), 1);
			sink.accept(Ctx {
				frame: &frame,
				pixels: Some(&pixmap),
			})?;
		}

		Box::new(sink).finish()
	}

	/// The padded luma/Cb/Cr planes `Mp4::accept` would have computed for one
	/// straight-RGBA8 frame, by calling the exact same two functions it does.
	fn padded_yuv(rgba: &[u8], width: u32, height: u32) -> (Vec<u8>, Vec<u8>, Vec<u8>, u32) {
		let stride = padded16(width);
		let rows = padded16(height);

		let mut padded_rgba = vec![0u8; (stride * rows * 4) as usize];
		for y in 0..height as usize {
			let src = &rgba[y * width as usize * 4..(y + 1) * width as usize * 4];
			let offset = y * stride as usize * 4;
			padded_rgba[offset..offset + width as usize * 4].copy_from_slice(src);
		}

		let mut luma = vec![0u8; (stride * rows) as usize];
		let mut cb = vec![128u8; (stride / 2 * rows / 2) as usize];
		let mut cr = vec![128u8; (stride / 2 * rows / 2) as usize];
		crate::simd::rgba_to_yuv420(
			Level::new(),
			&padded_rgba,
			stride as usize,
			rows as usize,
			&mut luma,
			&mut cb,
			&mut cr,
		);

		(luma, cb, cr, stride)
	}

	/// Crop a plane padded to `stride` down to a tightly packed
	/// `width x height` rectangle — what `ffmpeg`'s rawvideo output actually
	/// contains, since SPS cropping means a decoder never emits the padding
	/// at all.
	fn crop(plane: &[u8], stride: usize, width: usize, height: usize) -> Vec<u8> {
		let mut out = Vec::with_capacity(width * height);
		for y in 0..height {
			out.extend_from_slice(&plane[y * stride..y * stride + width]);
		}
		out
	}

	/// The expected bytes for one whole `ffmpeg -pix_fmt yuv420p` frame: luma
	/// then Cb then Cr, each cropped to the visible rectangle.
	fn expected_frame(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
		let (luma, cb, cr, stride) = padded_yuv(rgba, width, height);
		let mut out = crop(&luma, stride as usize, width as usize, height as usize);
		out.extend(crop(
			&cb,
			(stride / 2) as usize,
			width.div_ceil(2) as usize,
			height.div_ceil(2) as usize,
		));
		out.extend(crop(
			&cr,
			(stride / 2) as usize,
			width.div_ceil(2) as usize,
			height.div_ceil(2) as usize,
		));
		out
	}

	/// Decode `path` with `ffmpeg` into raw `yuv420p`, split into one
	/// `Vec<u8>` per frame at the container's visible dimensions.
	fn decode_frames(path: &Path, width: u32, height: u32) -> Vec<Vec<u8>> {
		let output = Command::new(FFMPEG)
			.args(["-v", "error", "-i"])
			.arg(path)
			.args(["-f", "rawvideo", "-pix_fmt", "yuv420p", "-"])
			.output()
			.expect("ffmpeg must be runnable at /opt/homebrew/bin/ffmpeg for this test");
		assert!(
			output.status.success(),
			"ffmpeg failed: {}",
			String::from_utf8_lossy(&output.stderr)
		);

		let frame_size = (width * height + 2 * width.div_ceil(2) * height.div_ceil(2)) as usize;
		assert_eq!(
			output.stdout.len() % frame_size,
			0,
			"ffmpeg's output must be a whole number of {frame_size}-byte frames, got {} bytes",
			output.stdout.len()
		);

		output
			.stdout
			.chunks_exact(frame_size)
			.map(<[u8]>::to_vec)
			.collect()
	}

	/// Encode `frames` under `Delta`, decode them back with `ffmpeg`, and
	/// assert every frame is byte-identical to what `Mp4` itself would have
	/// computed as YUV. This is the property the whole workstream rests on:
	/// not "the file plays", but the exact samples survive.
	fn assert_round_trips_losslessly(name: &str, width: u16, height: u16, frames: Vec<Vec<u8>>) {
		assert_profile_round_trips(Mp4Profile::Delta, name, width, height, frames);
	}

	fn assert_profile_round_trips(
		profile: Mp4Profile,
		name: &str,
		width: u16,
		height: u16,
		frames: Vec<Vec<u8>>,
	) {
		let path = std::env::temp_dir().join(format!("dvd-render-h264-{name}.mp4"));
		encode(&path, profile, width, height, &frames).expect("encoding must succeed");

		let decoded = decode_frames(&path, width as u32, height as u32);
		assert_eq!(
			decoded.len(),
			frames.len(),
			"ffmpeg must report exactly the frames that were written"
		);

		for (index, (rgba, decoded)) in frames.iter().zip(&decoded).enumerate() {
			let expected = expected_frame(rgba, width as u32, height as u32);
			assert_eq!(
				decoded, &expected,
				"frame {index} of {name} did not round-trip losslessly"
			);
		}

		let _ = std::fs::remove_file(&path);
	}

	/// The `Lossless` path is `less-avc`, which this crate has shipped all
	/// along and which nothing had ever decoded back to check.
	///
	/// It is worth its own test because of a real hazard the `Delta` work
	/// turned up. `less-avc` writes `deblocking_filter_control_present_flag =
	/// 0`, so its slices carry no `disable_deblocking_filter_idc` and the
	/// in-loop filter is on by default — and clause 8.7.2.1 gives an intra
	/// macroblock edge boundary strength 4, the strongest there is. What saves
	/// it is that an I_PCM macroblock decodes at `QP'Y = 0`, where the filter's
	/// own α and β thresholds are zero, so `filterSamplesFlag` can never be
	/// set and the filter touches nothing. That is a chain of three clauses
	/// deep enough that reasoning about it is not evidence; decoding is.
	///
	/// `Delta` cannot lean on the same argument, which is why it disables the
	/// filter outright: a P slice mixes skipped macroblocks carrying the
	/// slice's own QP with I_PCM macroblocks at QP 0, and the averaged QP
	/// across that boundary is not 0.
	#[test]
	#[ignore = "shells out to ffmpeg"]
	fn the_lossless_profile_also_decodes_byte_identically() {
		let frames = vec![
			solid(48, 48, [10, 10, 10, 255]),
			block(48, 48, [10, 10, 10, 255], [230, 230, 230, 255], 8, 8, 8),
			// A high-contrast edge is exactly what a deblocking filter would
			// smooth if it were running, so a flat field would not detect it.
			block(48, 48, [0, 0, 0, 255], [255, 255, 255, 255], 16, 16, 16),
		];
		assert_profile_round_trips(Mp4Profile::Lossless, "lossless-profile", 48, 48, frames);
	}

	fn solid(width: u16, height: u16, color: [u8; 4]) -> Vec<u8> {
		std::iter::repeat_n(color, width as usize * height as usize)
			.flatten()
			.collect()
	}

	/// Paint an `size x size` block of `fg` at `(x, y)` over an otherwise
	/// `bg`-filled canvas — the "a keystroke changed a handful of macroblocks"
	/// shape this whole encoder is built around.
	fn block(
		width: u16,
		height: u16,
		bg: [u8; 4],
		fg: [u8; 4],
		x: usize,
		y: usize,
		size: usize,
	) -> Vec<u8> {
		let mut frame = solid(width, height, bg);
		for row in y..(y + size).min(height as usize) {
			for col in x..(x + size).min(width as usize) {
				let offset = (row * width as usize + col) * 4;
				frame[offset..offset + 4].copy_from_slice(&fg);
			}
		}
		frame
	}

	#[test]
	#[ignore = "shells out to ffmpeg"]
	fn delta_profile_decodes_flat_colour_frames_byte_identically() {
		let frames = vec![
			solid(32, 32, [200, 30, 30, 255]),
			solid(32, 32, [30, 200, 30, 255]),
			solid(32, 32, [30, 30, 200, 255]),
		];
		assert_round_trips_losslessly("flat-colour", 32, 32, frames);
	}

	#[test]
	#[ignore = "shells out to ffmpeg"]
	fn delta_profile_decodes_a_moving_block_byte_identically() {
		let bg = [10, 10, 10, 255];
		let fg = [230, 230, 230, 255];
		let frames = (0..4)
			.map(|step| block(48, 48, bg, fg, step * 8, 8, 8))
			.collect();
		assert_round_trips_losslessly("moving-block", 48, 48, frames);
	}

	/// The scenario the whole workstream is justified by: only one pixel
	/// differs between two frames, meaning at most one macroblock is coded
	/// and every other one is a skip.
	#[test]
	#[ignore = "shells out to ffmpeg"]
	fn delta_profile_decodes_a_single_pixel_change_byte_identically() {
		let mut second = solid(32, 32, [50, 50, 50, 255]);
		second[(17 * 32 + 9) * 4..(17 * 32 + 9) * 4 + 4].copy_from_slice(&[255, 0, 0, 255]);
		assert_round_trips_losslessly(
			"single-pixel",
			32,
			32,
			vec![solid(32, 32, [50, 50, 50, 255]), second],
		);
	}

	/// A canvas that does not divide evenly into macroblocks exercises the
	/// SPS frame-cropping path this encoder derives itself, rather than
	/// borrowing `less-avc`'s.
	#[test]
	#[ignore = "shells out to ffmpeg"]
	fn delta_profile_decodes_correctly_when_the_canvas_needs_cropping() {
		let frames = vec![
			solid(20, 18, [80, 80, 80, 255]),
			block(20, 18, [80, 80, 80, 255], [255, 255, 0, 255], 2, 2, 4),
		];
		assert_round_trips_losslessly("needs-cropping", 20, 18, frames);
	}

	/// The other edge of the same path: a canvas that already fills whole
	/// macroblocks must encode a `frame_cropping_flag` of 0 and still
	/// round-trip — cropping being *absent* is as load-bearing as it being
	/// present and correct.
	#[test]
	#[ignore = "shells out to ffmpeg"]
	fn delta_profile_decodes_correctly_when_the_canvas_is_an_exact_macroblock_multiple() {
		let frames = vec![
			solid(64, 32, [5, 5, 5, 255]),
			block(64, 32, [5, 5, 5, 255], [250, 250, 250, 255], 40, 8, 16),
		];
		assert_round_trips_losslessly("exact-multiple", 64, 32, frames);
	}

	/// `ffprobe` must see the container the way `mp4.rs` described it: the
	/// visible dimensions, and a duration derived from `TICKS_PER_FRAME` and
	/// each frame's hold — not the padded size, and not a rounding artefact
	/// of the timescale conversion.
	#[test]
	#[ignore = "shells out to ffmpeg"]
	fn ffprobe_reports_the_expected_frame_count_dimensions_and_duration() {
		let frames: Vec<_> = (0..6)
			.map(|step| block(48, 32, [0, 0, 0, 255], [255, 255, 255, 255], step * 5, 4, 6))
			.collect();
		let frame_count = frames.len();
		let path = std::env::temp_dir().join("dvd-render-h264-ffprobe.mp4");
		encode(&path, Mp4Profile::Delta, 48, 32, &frames).expect("encoding must succeed");

		let output = Command::new(FFPROBE)
			.args([
				"-v",
				"error",
				"-count_frames",
				"-select_streams",
				"v:0",
				"-show_entries",
				"stream=nb_read_frames,width,height",
				"-show_entries",
				"format=duration",
				"-of",
				"default=noprint_wrappers=1",
			])
			.arg(&path)
			.output()
			.expect("ffprobe must be runnable at /opt/homebrew/bin/ffprobe for this test");
		assert!(
			output.status.success(),
			"ffprobe failed: {}",
			String::from_utf8_lossy(&output.stderr)
		);

		let report = String::from_utf8_lossy(&output.stdout);
		let field = |name: &str| {
			report
				.lines()
				.find_map(|line| line.strip_prefix(&format!("{name}=")))
				.unwrap_or_else(|| panic!("ffprobe output had no {name} field:\n{report}"))
		};

		assert_eq!(field("width"), "48");
		assert_eq!(field("height"), "32");
		assert_eq!(field("nb_read_frames"), frame_count.to_string());

		// Every frame is held one tick at 50 fps: `frame_count * 0.02` seconds.
		let expected_duration = frame_count as f64 * 0.02;
		let actual_duration: f64 = field("duration")
			.parse()
			.expect("duration must be a number");
		assert!(
			(actual_duration - expected_duration).abs() < 0.001,
			"expected a duration near {expected_duration}s, ffprobe reported {actual_duration}s"
		);

		let _ = std::fs::remove_file(&path);
	}

	/// The measurement the whole workstream is justified by: how much
	/// smaller a realistic recording is under `Delta` than under `Lossless`.
	/// Marked `#[ignore]` not because it needs `ffmpeg` — it doesn't — but
	/// because writing two hundred full-canvas IDR frames under `Lossless`
	/// is genuinely heavy (well over a hundred megabytes) and has no place
	/// slowing down the default test run; it exists to be run once, on
	/// purpose, and read.
	#[test]
	#[ignore = "encodes ~200MB of Lossless output to measure the size difference"]
	fn delta_is_dramatically_smaller_than_lossless_for_a_typical_recording() {
		const WIDTH: u16 = 1200;
		const HEIGHT: u16 = 600;
		const FRAMES: usize = 200;

		let mut base = solid(WIDTH, HEIGHT, [20, 20, 24, 255]);
		let mut frames = Vec::with_capacity(FRAMES);
		for step in 0..FRAMES {
			// One 8x16 "cell" toggles colour each frame — the shape of a
			// single character changing in an otherwise still terminal.
			let cell_x = (step * 8) % (WIDTH as usize - 8);
			let cell_y = ((step * 8) / (WIDTH as usize - 8) * 16) % (HEIGHT as usize - 16);
			let colour = if step % 2 == 0 {
				[230, 230, 230, 255]
			} else {
				[20, 20, 24, 255]
			};
			for row in cell_y..cell_y + 16 {
				for col in cell_x..cell_x + 8 {
					let offset = (row * WIDTH as usize + col) * 4;
					base[offset..offset + 4].copy_from_slice(&colour);
				}
			}
			frames.push(base.clone());
		}

		let lossless_path = std::env::temp_dir().join("dvd-render-h264-report-lossless.mp4");
		let delta_path = std::env::temp_dir().join("dvd-render-h264-report-delta.mp4");

		encode(&lossless_path, Mp4Profile::Lossless, WIDTH, HEIGHT, &frames)
			.expect("lossless encoding must succeed");
		encode(&delta_path, Mp4Profile::Delta, WIDTH, HEIGHT, &frames)
			.expect("delta encoding must succeed");

		let lossless_bytes = std::fs::metadata(&lossless_path)
			.expect("lossless file must exist")
			.len();
		let delta_bytes = std::fs::metadata(&delta_path)
			.expect("delta file must exist")
			.len();

		println!(
			"{FRAMES} frames of a {WIDTH}x{HEIGHT} canvas, one changed cell per frame:\n  \
			 Lossless: {lossless_bytes} bytes\n  \
			 Delta:    {delta_bytes} bytes ({:.1}x smaller)",
			lossless_bytes as f64 / delta_bytes as f64
		);

		assert!(
			delta_bytes * 20 < lossless_bytes,
			"Delta ({delta_bytes} bytes) should be at least 20x smaller than Lossless ({lossless_bytes} bytes) for a single-cell-per-frame recording"
		);

		let _ = std::fs::remove_file(&lossless_path);
		let _ = std::fs::remove_file(&delta_path);
	}
}
