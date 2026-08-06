//! Slice headers and the macroblock loop that fills a slice's data.
//!
//! This is the module the rest of the crate cares about: an IDR slice codes
//! every macroblock as I_PCM, unconditionally, and is what opens the stream
//! and what a forced periodic resync falls back to. A P slice walks the same
//! macroblock grid but, for each one, first asks whether it is byte-identical
//! to the reference picture; if it is, it becomes one macroblock added to a
//! run of `mb_skip_run`, and if it is not, it is coded as I_PCM exactly like
//! an IDR macroblock would code it. There is no motion search, no transform,
//! no quantizer — "skip" means "byte-for-byte identical to the picture already
//! decoded", which for a terminal's unchanged background is exactly the right
//! question to ask, and I_PCM for anything else means the frames this scheme
//! does encode are exactly as lossless as the all-IDR stream they replace.
//!
//! Macroblock type numbering is the one place the P slice can't just reuse
//! the IDR encoder unchanged. Table 7-13 numbers a P slice's *intra*
//! macroblock types by taking the I-slice numbering from Table 7-11 and
//! adding 5 — room for the five inter (`P_*`) prediction modes that come
//! first. I_PCM is `mb_type = 25` in an I slice and `mb_type = 30` in a P
//! slice for exactly that reason; get the offset wrong and a decoder reads
//! every "this macroblock is uncompressed" as some other, nonsensical,
//! inter-prediction mode instead.

use super::bitstream::BitWriter;

/// I_PCM in I-slice macroblock-type numbering (Table 7-11).
const I_SLICE_I_PCM_MB_TYPE: u32 = 25;

/// I_PCM in P-slice macroblock-type numbering (Table 7-13): the I-slice
/// number shifted past the five inter prediction modes that occupy
/// `mb_type` 0..=4 in a P slice.
const P_SLICE_I_PCM_MB_TYPE: u32 = I_SLICE_I_PCM_MB_TYPE + 5;

/// `slice_type` values (Table 7-6). This encoder only ever emits one slice
/// per picture, so the "all other slices in this picture share this type"
/// variants (5..=9) would be equally valid, but the plain values read more
/// directly against the syntax tables.
const SLICE_TYPE_P: u32 = 0;
const SLICE_TYPE_I: u32 = 7;

/// The planes of one padded YUV 4:2:0 frame, as `mp4.rs` already lays them
/// out: luma at `stride`, chroma at `stride / 2`, both padded to whole
/// macroblocks.
pub struct Planes<'a> {
	pub luma: &'a [u8],
	pub cb: &'a [u8],
	pub cr: &'a [u8],
	pub stride: usize,
}

impl Planes<'_> {
	fn chroma_stride(&self) -> usize {
		self.stride / 2
	}
}

/// Write one macroblock's I_PCM data (clause 7.3.5): the `mb_type`, then
/// alignment padding, then the raw samples — 256 luma bytes in raster order
/// followed by 64 Cb bytes and 64 Cr bytes, each block also in raster order.
fn write_i_pcm_macroblock(
	writer: &mut BitWriter,
	mb_type: u32,
	planes: &Planes<'_>,
	mb_row: usize,
	mb_col: usize,
) {
	writer.ue(mb_type);
	writer.align_zero(); // pcm_alignment_zero_bit

	let (y0, x0) = (mb_row * 16, mb_col * 16);
	for row in 0..16 {
		let offset = (y0 + row) * planes.stride + x0;
		writer.push_bytes(&planes.luma[offset..offset + 16]);
	}

	let chroma_stride = planes.chroma_stride();
	let (cy0, cx0) = (mb_row * 8, mb_col * 8);
	for plane in [planes.cb, planes.cr] {
		for row in 0..8 {
			let offset = (cy0 + row) * chroma_stride + cx0;
			writer.push_bytes(&plane[offset..offset + 8]);
		}
	}
}

/// Whether macroblock `(mb_row, mb_col)` is byte-identical between `current`
/// and `reference` — across luma and both chroma planes, since a skip means
/// the decoder reconstructs the whole macroblock from the reference picture
/// with no correction at all.
fn macroblock_unchanged(
	current: &Planes<'_>,
	reference: &Planes<'_>,
	mb_row: usize,
	mb_col: usize,
) -> bool {
	let (y0, x0) = (mb_row * 16, mb_col * 16);
	let luma_matches = (0..16).all(|row| {
		let offset = (y0 + row) * current.stride + x0;
		current.luma[offset..offset + 16] == reference.luma[offset..offset + 16]
	});
	if !luma_matches {
		return false;
	}

	let chroma_stride = current.chroma_stride();
	let (cy0, cx0) = (mb_row * 8, mb_col * 8);
	[(current.cb, reference.cb), (current.cr, reference.cr)]
		.iter()
		.all(|(current_plane, reference_plane)| {
			(0..8).all(|row| {
				let offset = (cy0 + row) * chroma_stride + cx0;
				current_plane[offset..offset + 8] == reference_plane[offset..offset + 8]
			})
		})
}

/// `dec_ref_pic_marking()` for an IDR picture (clause 7.3.3.3), plus the
/// `slice_qp_delta` and deblocking fields every slice ends its header with.
fn write_common_tail(writer: &mut BitWriter) {
	writer.se(0); // slice_qp_delta
	// disable_deblocking_filter_idc = 1: see params.rs's module doc. Because
	// it is 1, the two offset fields that would otherwise follow
	// (slice_alpha_c0_offset_div2, slice_beta_offset_div2) are not present.
	writer.ue(1);
}

/// Build and code one IDR (I) slice: every macroblock in raster order, all
/// I_PCM, no skip runs — `slice_type == I` drops the `mb_skip_run` branch
/// from `slice_data()` (clause 7.3.4) entirely.
pub fn idr_slice(planes: &Planes<'_>, mb_width: u32, mb_height: u32) -> Vec<u8> {
	let mut writer = BitWriter::new();

	writer.ue(0); // first_mb_in_slice
	writer.ue(SLICE_TYPE_I);
	writer.ue(0); // pic_parameter_set_id
	writer.u(super::LOG2_MAX_FRAME_NUM, 0); // frame_num — always 0 at an IDR
	writer.ue(0); // idr_pic_id
	// pic_order_cnt_type == 2: no pic_order_cnt_lsb field at all.
	writer.u(1, 0); // no_output_of_prior_pics_flag
	writer.u(1, 0); // long_term_reference_flag
	write_common_tail(&mut writer);

	for mb_row in 0..mb_height as usize {
		for mb_col in 0..mb_width as usize {
			write_i_pcm_macroblock(&mut writer, I_SLICE_I_PCM_MB_TYPE, planes, mb_row, mb_col);
		}
	}

	writer.rbsp_trailing_bits();
	writer.finish()
}

/// Build and code one P slice against `reference`: a `mb_skip_run` for every
/// stretch of unchanged macroblocks, I_PCM for the rest.
///
/// The loop mirrors `slice_data()` (clause 7.3.4) directly: read a skip
/// count, advance past it, and if that lands exactly on the last macroblock
/// stop there — a slice that ends on a skip run codes no macroblock for it at
/// all, which is why the loop checks `mb == total` before, not after, trying
/// to code one.
pub fn p_slice(
	planes: &Planes<'_>,
	reference: &Planes<'_>,
	mb_width: u32,
	mb_height: u32,
	frame_num: u32,
) -> Vec<u8> {
	let mut writer = BitWriter::new();

	writer.ue(0); // first_mb_in_slice
	writer.ue(SLICE_TYPE_P);
	writer.ue(0); // pic_parameter_set_id
	writer.u(super::LOG2_MAX_FRAME_NUM, frame_num);
	// pic_order_cnt_type == 2: no pic_order_cnt_lsb field.
	writer.u(1, 0); // num_ref_idx_active_override_flag — use the PPS default of one reference
	writer.u(1, 0); // ref_pic_list_modification_flag_l0 — the default order (most recent first) is already what we want
	writer.u(1, 0); // adaptive_ref_pic_marking_mode_flag — sliding window, which with one reference frame simply replaces it each picture
	write_common_tail(&mut writer);

	let total = (mb_width * mb_height) as usize;
	let mb_row_of = |mb: usize| mb / mb_width as usize;
	let mb_col_of = |mb: usize| mb % mb_width as usize;

	let mut mb = 0usize;
	while mb < total {
		let mut skip = 0usize;
		while mb + skip < total
			&& macroblock_unchanged(
				planes,
				reference,
				mb_row_of(mb + skip),
				mb_col_of(mb + skip),
			) {
			skip += 1;
		}

		writer.ue(skip as u32);
		mb += skip;
		if mb == total {
			break;
		}

		write_i_pcm_macroblock(
			&mut writer,
			P_SLICE_I_PCM_MB_TYPE,
			planes,
			mb_row_of(mb),
			mb_col_of(mb),
		);
		mb += 1;
	}

	writer.rbsp_trailing_bits();
	writer.finish()
}

#[cfg(test)]
mod tests {
	use super::*;

	fn solid_frame(
		width: usize,
		height: usize,
		luma: u8,
		chroma: u8,
	) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
		(
			vec![luma; width * height],
			vec![chroma; (width / 2) * (height / 2)],
			vec![chroma; (width / 2) * (height / 2)],
		)
	}

	/// The property the whole workstream exists for: a macroblock whose bytes
	/// did not change costs one `ue(v)` folded into the run, not a repeated
	/// I_PCM copy. This test only checks the predicate the P slice loop is
	/// built on; the byte-count payoff itself is measured end to end in
	/// `mod.rs`'s tests.
	#[test]
	fn an_unchanged_macroblock_is_recognised_as_a_skip_candidate() {
		let (luma, cb, cr) = solid_frame(32, 32, 10, 128);
		let planes = Planes {
			luma: &luma,
			cb: &cb,
			cr: &cr,
			stride: 32,
		};

		assert!(macroblock_unchanged(&planes, &planes, 0, 0));
		assert!(macroblock_unchanged(&planes, &planes, 1, 1));
	}

	/// One changed luma byte anywhere in the 16x16 block is enough to force
	/// that macroblock to be coded — a partial match would mean the decoder
	/// reconstructs stale pixels for the untested bytes.
	#[test]
	fn a_single_changed_luma_byte_breaks_the_match() {
		let (luma_a, cb, cr) = solid_frame(32, 32, 10, 128);
		let mut luma_b = luma_a.clone();
		luma_b[16 * 32 + 5] = 11; // inside macroblock (row 1, col 0)

		let a = Planes {
			luma: &luma_a,
			cb: &cb,
			cr: &cr,
			stride: 32,
		};
		let b = Planes {
			luma: &luma_b,
			cb: &cb,
			cr: &cr,
			stride: 32,
		};

		assert!(
			macroblock_unchanged(&a, &b, 0, 0),
			"macroblock (0,0) is untouched"
		);
		assert!(
			!macroblock_unchanged(&a, &b, 1, 0),
			"macroblock (1,0) contains the changed byte"
		);
	}

	/// A changed chroma byte must break the match too, even with luma
	/// identical — chroma carries colour, and a "skip" that ignored it would
	/// silently freeze colour while luma kept updating.
	#[test]
	fn a_changed_chroma_byte_breaks_the_match_even_with_identical_luma() {
		let (luma, cb_a, cr) = solid_frame(32, 32, 10, 128);
		let mut cb_b = cb_a.clone();
		cb_b[0] = 129;

		let a = Planes {
			luma: &luma,
			cb: &cb_a,
			cr: &cr,
			stride: 32,
		};
		let b = Planes {
			luma: &luma,
			cb: &cb_b,
			cr: &cr,
			stride: 32,
		};

		assert!(!macroblock_unchanged(&a, &b, 0, 0));
	}

	/// A P slice with nothing changed at all must still be a legal slice: one
	/// `mb_skip_run` covering every macroblock, and no macroblock_layer at
	/// all — the "break before coding" branch in `p_slice`'s loop.
	#[test]
	fn a_fully_static_frame_produces_a_single_skip_run_and_no_macroblocks() {
		let (luma, cb, cr) = solid_frame(32, 32, 10, 128);
		let planes = Planes {
			luma: &luma,
			cb: &cb,
			cr: &cr,
			stride: 32,
		};

		let rbsp = p_slice(&planes, &planes, 2, 2, 1);

		// The header is a handful of single bits plus small ue(v)/se(v)
		// codes; a run covering all four macroblocks is ue(4) = 00100, and
		// nothing else follows before the trailing bits. If a macroblock had
		// been coded, this would run to hundreds of bytes (256 luma + 128
		// chroma bytes for just one I_PCM macroblock) instead of a handful.
		assert!(
			rbsp.len() < 8,
			"a fully-skipped slice must not contain any I_PCM data, got {} bytes",
			rbsp.len()
		);
	}

	/// mb_type 30 is exactly 5 past I_PCM's I-slice number of 25 — the offset
	/// Table 7-13 defines. This test would fail loudly if that constant ever
	/// drifted from the other, since the whole point of naming both is that
	/// they must stay linked.
	#[test]
	fn p_slice_i_pcm_mb_type_is_the_i_slice_type_offset_by_five() {
		assert_eq!(P_SLICE_I_PCM_MB_TYPE, I_SLICE_I_PCM_MB_TYPE + 5);
		assert_eq!(I_SLICE_I_PCM_MB_TYPE, 25);
		assert_eq!(P_SLICE_I_PCM_MB_TYPE, 30);
	}
}
