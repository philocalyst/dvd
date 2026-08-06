//! A from-scratch H.264 encoder whose only trick is skipping macroblocks that
//! did not change.
//!
//! `less-avc` (see `encode/mp4.rs`) codes every macroblock as I_PCM in every
//! frame, so a distinct frame costs `width * height * 1.5` bytes no matter how
//! little of the picture actually moved. For a terminal recording that is the
//! wrong trade: a keystroke changes one or two 16x16 macroblocks out of a
//! couple of thousand, and the other few thousand are the same bytes they
//! were last frame. H.264's answer to "this part of the picture didn't
//! change" is a P slice's `mb_skip_run` — a run-length of macroblocks copied
//! straight from the reference picture, no motion vector, no residual, no
//! quantizer. Pairing that with I_PCM for whatever *did* change gives a frame
//! that is still exactly lossless (skip is a byte-for-byte copy, and I_PCM
//! never compresses in the first place) and costs bytes proportional to what
//! moved rather than to the size of the canvas.
//!
//! Three submodules, one per layer of the bitstream:
//!
//! - [`bitstream`]: the bit-level writer and Exp-Golomb codes everything else
//!   is built from, plus emulation prevention and NAL unit framing.
//! - [`params`]: the SPS and PPS, negotiated once and reused for every frame.
//! - [`slice`]: the IDR and P slice bodies — the macroblock loop, the skip/
//!   I_PCM decision, and the one-macroblock-type-numbering wrinkle a P slice
//!   introduces (see that module's doc).
//!
//! This module ties them into [`Encoder`]: the thing `mp4.rs` actually calls,
//! which owns the reference picture, decides IDR vs. P per frame, and hands
//! back a finished NAL unit each time.
//!
//! One deliberate omission: there is no motion compensation, no transform, and
//! no quantizer anywhere in this encoder, and there will not be one added
//! later without changing the module's premise. Every one of those exists to
//! trade fidelity for size; this encoder's whole reason to exist is a lossless
//! trade that does not touch fidelity at all — the size reduction comes
//! entirely from not re-sending bytes that were already sent.

mod bitstream;
mod params;
mod slice;

use bitstream::{
	NAL_REF_IDC_PARAMETER_SET, NAL_REF_IDC_SLICE, NAL_UNIT_TYPE_IDR_SLICE,
	NAL_UNIT_TYPE_NON_IDR_SLICE, NAL_UNIT_TYPE_PPS, NAL_UNIT_TYPE_SPS, nal_unit,
};
use params::{Pps, Sps};
use slice::Planes;

/// Width, in bits, of every `frame_num` and (implicitly, via `pic_order_cnt_type
/// == 2`) picture-order field this encoder writes. Shared between `params.rs`
/// (which declares it in the SPS) and `slice.rs` (which writes exactly that
/// many bits in every slice header) so the two can never disagree.
pub(crate) const LOG2_MAX_FRAME_NUM: u32 = 4;

/// Distinct frames between forced IDRs.
///
/// A pure P-slice chain has no seek points past the first frame at all —
/// `stss` would list exactly one sample, and scrubbing anywhere else would
/// mean decoding from the start every time. Forcing a fresh IDR periodically
/// bounds that: 300 distinct frames is, at the frame rates this crate
/// records at (see `TICKS_PER_FRAME` in `mp4.rs`), on the order of a few
/// seconds to a minute of real activity once dedup has already thinned the
/// stream down to frames that actually changed something — close enough to a
/// typical GIF keyframe interval to make scrubbing feel instant without
/// giving back a large fraction of the size win a P slice exists for.
pub const IDR_INTERVAL: u32 = 300;

/// The two parameter-set NAL units a decoder needs before it can read any
/// slice this encoder produces. Declared once, in `avcC`, and never repeated
/// in the sample stream.
pub struct ParameterSets {
	pub sps: Vec<u8>,
	pub pps: Vec<u8>,
}

/// A copy of one coded picture's planes, kept only so the next picture has
/// something to diff against.
///
/// Owned rather than borrowed: `mp4.rs` reuses its luma/cb/cr buffers in
/// place every frame (that reuse is the entire reason those buffers exist —
/// see `stream.rs`'s `Pool`), so by the time the *next* frame is encoded the
/// bytes this frame was compared against would already be gone. The copy is
/// one `memcpy` per plane per distinct frame, which is far cheaper than the
/// I_PCM encode it lets most macroblocks skip.
struct Reference {
	luma: Vec<u8>,
	cb: Vec<u8>,
	cr: Vec<u8>,
}

/// Encodes a sequence of same-sized YUV 4:2:0 frames into H.264 NAL units,
/// coding the first frame (and periodic resyncs) as IDR and every other frame
/// as a P slice against the immediately preceding one.
pub struct Encoder {
	sps: Sps,
	pps: Pps,
	stride: u32,
	mb_width: u32,
	mb_height: u32,
	reference: Option<Reference>,
	/// `frame_num`, incrementing per non-IDR picture and wrapping at
	/// `1 << LOG2_MAX_FRAME_NUM` (clause 7.4.3's "no gaps" constraint, which
	/// holds trivially here because this encoder never drops a picture once
	/// it decides to code one).
	frame_num: u32,
	/// Distinct frames coded since the last IDR, forcing another once it
	/// reaches [`IDR_INTERVAL`].
	since_idr: u32,
}

impl Encoder {
	/// `width`/`height` are the visible canvas; `stride`/`padded_height` are
	/// that size rounded up to whole macroblocks — the same pair `mp4.rs`
	/// already computes and pads its plane buffers to.
	pub fn new(width: u32, height: u32, stride: u32, padded_height: u32) -> Self {
		Self {
			sps: Sps::new(width, height, stride, padded_height),
			pps: Pps,
			stride,
			mb_width: stride / 16,
			mb_height: padded_height / 16,
			reference: None,
			frame_num: 0,
			since_idr: 0,
		}
	}

	/// The SPS/PPS NAL units for `avcC`. Available immediately — unlike
	/// `less-avc`, nothing here is inferred from pixel data, only from the
	/// dimensions passed to [`Encoder::new`].
	pub fn parameter_sets(&self) -> ParameterSets {
		ParameterSets {
			sps: nal_unit(
				NAL_REF_IDC_PARAMETER_SET,
				NAL_UNIT_TYPE_SPS,
				&self.sps.to_rbsp(),
			),
			pps: nal_unit(
				NAL_REF_IDC_PARAMETER_SET,
				NAL_UNIT_TYPE_PPS,
				&self.pps.to_rbsp(),
			),
		}
	}

	/// Encode one frame, returning its NAL unit and whether it is a sync
	/// sample (an IDR — the only kind of frame a decoder, or a scrub, can
	/// start from).
	pub fn encode(&mut self, luma: &[u8], cb: &[u8], cr: &[u8]) -> (Vec<u8>, bool) {
		let planes = Planes {
			luma,
			cb,
			cr,
			stride: self.stride as usize,
		};

		let want_idr = self.reference.is_none() || self.since_idr >= IDR_INTERVAL;

		let rbsp = if want_idr {
			slice::idr_slice(&planes, self.mb_width, self.mb_height)
		} else {
			// `reference` is always `Some` once `want_idr` is false — the
			// only way past the check above is that it was already set.
			let reference = self
				.reference
				.as_ref()
				.expect("since_idr only advances past IDR_INTERVAL once a reference exists");
			let reference_planes = Planes {
				luma: &reference.luma,
				cb: &reference.cb,
				cr: &reference.cr,
				stride: self.stride as usize,
			};
			slice::p_slice(
				&planes,
				&reference_planes,
				self.mb_width,
				self.mb_height,
				self.frame_num,
			)
		};

		let unit_type = if want_idr {
			NAL_UNIT_TYPE_IDR_SLICE
		} else {
			NAL_UNIT_TYPE_NON_IDR_SLICE
		};
		let nal = nal_unit(NAL_REF_IDC_SLICE, unit_type, &rbsp);

		match &mut self.reference {
			Some(reference) => {
				reference.luma.copy_from_slice(luma);
				reference.cb.copy_from_slice(cb);
				reference.cr.copy_from_slice(cr);
			}
			None => {
				self.reference = Some(Reference {
					luma: luma.to_vec(),
					cb: cb.to_vec(),
					cr: cr.to_vec(),
				});
			}
		}

		if want_idr {
			self.frame_num = 0;
			self.since_idr = 0;
		} else {
			self.frame_num = (self.frame_num + 1) % (1 << LOG2_MAX_FRAME_NUM);
			self.since_idr += 1;
		}

		(nal, want_idr)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn frame(width: u32, height: u32, luma: u8, chroma: u8) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
		(
			vec![luma; (width * height) as usize],
			vec![chroma; (width / 2 * height / 2) as usize],
			vec![chroma; (width / 2 * height / 2) as usize],
		)
	}

	/// The very first frame has nothing to diff against, so it must be an
	/// IDR regardless of content — there is no reference picture yet for a P
	/// slice to skip against.
	#[test]
	fn the_first_frame_is_always_an_idr() {
		let mut encoder = Encoder::new(32, 32, 32, 32);
		let (luma, cb, cr) = frame(32, 32, 40, 128);
		let (_, is_sync) = encoder.encode(&luma, &cb, &cr);
		assert!(is_sync);
	}

	/// A second, different frame after the IDR must be a P slice — the
	/// entire point of this encoder over an all-IDR stream.
	#[test]
	fn a_second_distinct_frame_is_a_p_slice() {
		let mut encoder = Encoder::new(32, 32, 32, 32);
		let (luma_a, cb, cr) = frame(32, 32, 40, 128);
		encoder.encode(&luma_a, &cb, &cr);

		let (luma_b, _, _) = frame(32, 32, 41, 128);
		let (_, is_sync) = encoder.encode(&luma_b, &cb, &cr);
		assert!(!is_sync);
	}

	/// An unchanged macroblock costs nothing: an all-skip P slice against an
	/// identical frame must be dramatically smaller than the IDR that opened
	/// the stream, since it carries no I_PCM data at all. This is the
	/// property the whole workstream is justified by, pinned as a unit test
	/// rather than only measured end to end.
	#[test]
	fn an_unchanged_macroblock_costs_nothing() {
		let mut encoder = Encoder::new(1200, 600, 1200, 608);
		let (luma, cb, cr) = frame(1200, 608, 40, 128);
		let (idr, _) = encoder.encode(&luma, &cb, &cr);
		let (still, _) = encoder.encode(&luma, &cb, &cr);

		assert!(
			still.len() < idr.len() / 100,
			"an identical repeat of a 1200x600 frame should cost a couple of dozen bytes, not a share of {} bytes",
			idr.len()
		);
	}

	/// `since_idr` has to reset to zero at every IDR, including forced ones —
	/// otherwise the very next frame would immediately force *another* IDR,
	/// and the interval would mean nothing.
	#[test]
	fn a_forced_idr_resets_the_interval_counter() {
		let mut encoder = Encoder::new(16, 16, 16, 16);
		let (luma, cb, cr) = frame(16, 16, 40, 128);

		encoder.encode(&luma, &cb, &cr); // the opening IDR
		for _ in 0..IDR_INTERVAL {
			encoder.encode(&luma, &cb, &cr); // P slices, climbing since_idr to IDR_INTERVAL
		}

		let (_, is_sync) = encoder.encode(&luma, &cb, &cr);
		assert!(
			is_sync,
			"since_idr reaching IDR_INTERVAL must force this frame to be an IDR"
		);
		assert_eq!(
			encoder.since_idr, 0,
			"the frame that hit the interval must itself be the reset IDR"
		);

		let (_, is_sync) = encoder.encode(&luma, &cb, &cr);
		assert!(
			!is_sync,
			"the frame right after a forced IDR must not immediately force another"
		);
	}

	/// `frame_num` must wrap at `1 << LOG2_MAX_FRAME_NUM` rather than
	/// overflow the field width in the slice header — a value that no longer
	/// fits would corrupt every subsequent syntax element in that slice.
	#[test]
	fn frame_num_wraps_at_its_field_width() {
		let mut encoder = Encoder::new(16, 16, 16, 16);
		let (luma, cb, cr) = frame(16, 16, 40, 128);

		encoder.encode(&luma, &cb, &cr); // IDR, frame_num stays 0
		for _ in 0..(1 << LOG2_MAX_FRAME_NUM) {
			encoder.encode(&luma, &cb, &cr);
		}
		assert_eq!(
			encoder.frame_num, 0,
			"frame_num must have wrapped back to 0 after a full cycle"
		);
	}

	/// The parameter sets must not depend on having encoded anything — unlike
	/// `less-avc`, which infers its SPS/PPS from the first frame it is given,
	/// this encoder knows its shape from `Encoder::new` alone.
	#[test]
	fn parameter_sets_are_available_before_any_frame_is_encoded() {
		let encoder = Encoder::new(32, 32, 32, 32);
		let sets = encoder.parameter_sets();
		assert!(!sets.sps.is_empty());
		assert!(!sets.pps.is_empty());
	}
}
