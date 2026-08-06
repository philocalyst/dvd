//! The sequence and picture parameter sets, and the choices baked into them.
//!
//! An SPS/PPS pair is negotiated once and lives in the MP4 `avcC` box, not in
//! the sample stream — every frame after the first has to agree with exactly
//! these bytes. That is why this module has so few knobs: baseline profile,
//! 4:2:0, 8-bit, one slice group, one reference frame. Nothing here needs to
//! vary per frame, because the whole point of a P slice is that it *doesn't*
//! carry a fresh parameter set.
//!
//! A handful of fields are set to values that are not the obvious default, and
//! each is called out at the point it is written rather than here, because the
//! reasoning is local to that field. The two that shape everything else:
//!
//! - `pic_order_cnt_type = 2` derives picture order directly from `frame_num`
//!   and needs no `pic_order_cnt_lsb` in any slice header. It requires that
//!   the sequence never uses B slices — true here by construction, since
//!   baseline profile does not have B slices at all — so it costs nothing to
//!   choose and removes a field this encoder would otherwise have to get
//!   right in every slice header.
//! - `deblocking_filter_control_present_flag = 1`, paired with
//!   `disable_deblocking_filter_idc = 1` in every slice, turns the in-loop
//!   deblocking filter off. Left enabled, it is not a no-op on I_PCM data: an
//!   edge between an intra-coded macroblock and anything else gets the
//!   strongest boundary strength the filter defines (clause 8.7.2.1, bS = 4),
//!   which is every macroblock edge in an all-I_PCM slice. A filter that
//!   smooths pixel values across every macroblock boundary is exactly the
//!   thing "lossless" cannot survive, so it has to be off, not merely unused.

use super::LOG2_MAX_FRAME_NUM;
use super::bitstream::BitWriter;

/// One reference frame, always the immediately preceding coded picture. A P
/// slice here never reaches further back than that, so the sequence never
/// needs more than one picture held as a reference at a time.
const MAX_NUM_REF_FRAMES: u32 = 1;

/// Sequence parameter set.
pub struct Sps {
	/// Macroblock columns, i.e. `stride / 16`.
	pub mb_width: u32,
	/// Macroblock rows, i.e. `padded_height / 16`.
	pub mb_height: u32,
	/// `(left, right, top, bottom)` crop, in the 4:2:0 crop unit of 2 luma
	/// samples. `None` when the visible size already fills every macroblock.
	pub crop: Option<(u32, u32, u32, u32)>,
}

impl Sps {
	/// Derive the SPS from the padded macroblock grid and the visible size
	/// cropped out of it — the same padding/cropping contract `mp4.rs` keeps
	/// for the plane buffers themselves, so the two can never disagree about
	/// where the real picture ends.
	pub fn new(width: u32, height: u32, stride: u32, padded_height: u32) -> Self {
		let mb_width = stride / 16;
		let mb_height = padded_height / 16;

		let crop = (stride != width || padded_height != height).then(|| {
			// 4:2:0 with frame_mbs_only_flag = 1 gives a crop unit of
			// (SubWidthC, SubHeightC) = (2, 2) luma samples (clause 7.4.2.1.1),
			// and this encoder only ever pads on the right and bottom edges —
			// same as `mp4.rs`'s `padded()` — so the left and top offsets are
			// always zero.
			(0, (stride - width) / 2, 0, (padded_height - height) / 2)
		});

		Self {
			mb_width,
			mb_height,
			crop,
		}
	}

	pub fn to_rbsp(&self) -> Vec<u8> {
		let mut w = BitWriter::new();

		// profile_idc = 66 (Baseline). Baseline never emits the high-profile
		// chroma/bit-depth block below (clause 7.3.2.1.1's condition on
		// profile_idc) — 4:2:0 8-bit is implied by the profile alone, which is
		// exactly what this encoder produces.
		w.u(8, 66);
		// constraint_set0..5_flag + reserved_zero_2bits: all zero. Nothing
		// downstream reads these; they exist for encoder/decoder capability
		// negotiation this crate has no part in.
		w.u(8, 0);
		// level_idc = 10, matching `less-avc`'s SPS. Levels bound macroblock
		// throughput and reference memory for hardware decoders; a level that
		// low is nominally too small for a 1200x600 canvas, but it is pure
		// metadata; every software decoder this encoder is verified against
		// decodes the bitstream on its actual syntax, not this advisory number.
		w.u(8, 10);

		w.ue(0); // seq_parameter_set_id

		w.ue(LOG2_MAX_FRAME_NUM - 4);
		w.ue(2); // pic_order_cnt_type — see module doc
		w.ue(MAX_NUM_REF_FRAMES - 1);
		w.u(1, 0); // gaps_in_frame_num_value_allowed_flag

		w.ue(self.mb_width - 1);
		w.ue(self.mb_height - 1);

		w.u(1, 1); // frame_mbs_only_flag — no field pictures
		w.u(1, 0); // direct_8x8_inference_flag — meaningless without B slices

		match self.crop {
			Some((left, right, top, bottom)) => {
				w.u(1, 1);
				w.ue(left);
				w.ue(right);
				w.ue(top);
				w.ue(bottom);
			}
			None => w.u(1, 0),
		}

		// vui_parameters_present_flag = 0. VUI's only effect this encoder
		// would care about is signalling full- vs limited-range colour for
		// YUV<->RGB conversion, and the round-trip this module is verified
		// against stays in YUV the whole way (`ffmpeg -f rawvideo -pix_fmt
		// yuv420p`), where that flag plays no part at all — the samples this
		// encoder writes are the samples a decoder reads back, unconditionally.
		w.u(1, 0);

		w.rbsp_trailing_bits();
		w.finish()
	}
}

/// Picture parameter set.
pub struct Pps;

impl Pps {
	pub fn to_rbsp(&self) -> Vec<u8> {
		let mut w = BitWriter::new();

		w.ue(0); // pic_parameter_set_id
		w.ue(0); // seq_parameter_set_id

		// entropy_coding_mode_flag = 0: CAVLC. CABAC would mean every ue(v)/
		// se(v) in this module turns into an arithmetic-coded bin, for a
		// stream that never spends the bits CABAC exists to save (a skip run
		// or an I_PCM header is already about as small as it gets); CAVLC
		// keeps macroblock headers as the same Exp-Golomb codes SPS/PPS
		// already use.
		w.u(1, 0);
		w.u(1, 0); // bottom_field_pic_order_in_frame_present_flag — no fields
		w.ue(0); // num_slice_groups_minus1 — one slice group, no FMO

		// num_ref_idx_l0/l1_default_active_minus1 = 0: one active reference
		// for L0, which is what `MAX_NUM_REF_FRAMES` promises the sliding
		// window will keep around. l1 is unconditionally present in the
		// syntax even though nothing here ever codes a B slice.
		w.ue(0);
		w.ue(0);

		w.u(1, 0); // weighted_pred_flag
		w.u(2, 0); // weighted_bipred_idc

		w.se(0); // pic_init_qp_minus26
		w.se(0); // pic_init_qs_minus26
		w.se(0); // chroma_qp_index_offset

		// deblocking_filter_control_present_flag = 1 — see module doc. This
		// is the one field this encoder sets differently from `less-avc`'s
		// PPS, and the reason is the same reason the rest of this module
		// exists: `less-avc` never mixes I_PCM with anything else in a slice,
		// so a filtered edge there is a filtered edge between two identical
		// encodings of "no compression happened". Here, an I_PCM macroblock
		// sits directly against skipped ones, and the boundary is real.
		w.u(1, 1);

		w.u(1, 0); // constrained_intra_pred_flag
		w.u(1, 0); // redundant_pic_cnt_present_flag

		w.rbsp_trailing_bits();
		w.finish()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// `log2_max_frame_num` has to stay wide enough that this encoder's own
	/// frame-counting scheme (see `encoder.rs`) never overflows it; this test
	/// exists so a future change to either constant fails loudly instead of
	/// producing a stream with a frame_num field too narrow for its own
	/// counter.
	#[test]
	fn log2_max_frame_num_matches_the_field_width_this_module_assumes() {
		assert_eq!(LOG2_MAX_FRAME_NUM, 4);
	}

	/// An exact multiple of sixteen needs no cropping at all — the SPS must
	/// say so plainly, or a decoder would trim rows and columns that were
	/// never padding to begin with.
	#[test]
	fn a_canvas_that_is_already_a_macroblock_multiple_needs_no_cropping() {
		let sps = Sps::new(1200, 608, 1200, 608);
		assert!(sps.crop.is_none());
	}

	/// The common case this whole padding scheme exists for: a canvas that
	/// does not divide evenly into macroblocks gets cropped back down on
	/// exactly the edges `mp4.rs` pads — right and bottom, never left or top.
	#[test]
	fn a_canvas_needing_padding_crops_only_the_right_and_bottom_edges() {
		let sps = Sps::new(1201, 602, 1216, 608);
		assert_eq!(sps.crop, Some((0, (1216 - 1201) / 2, 0, (608 - 602) / 2)));
	}

	/// The macroblock grid comes from the padded stride, not the visible
	/// size — cropping is a display-time trim, and the grid it trims out of
	/// has to be the one the planes are actually shaped as.
	#[test]
	fn the_macroblock_grid_is_derived_from_the_padded_size() {
		let sps = Sps::new(20, 20, 32, 32);
		assert_eq!(sps.mb_width, 2);
		assert_eq!(sps.mb_height, 2);
	}
}
