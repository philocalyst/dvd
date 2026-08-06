//! Bit-level writing for H.264's raw byte sequence payloads.
//!
//! Every syntax element in an H.264 slice or parameter set is either a fixed
//! number of bits (`u(n)`) or an Exponential-Golomb code (`ue(v)`, `se(v)`),
//! and none of them are required to start on a byte boundary — CAVLC macroblock
//! headers, in particular, follow directly on from whatever bit position the
//! previous element left off at. A writer built around `Vec<u8>` and a byte
//! cursor cannot express that; a writer built around individual bits can, and
//! everything above this module — SPS, PPS, slice headers, macroblock layers —
//! is built by calling `u`/`ue`/`se` in the order the spec lists the fields,
//! which is what makes those callers readable against the spec text instead of
//! against a byte-packing scheme invented for this crate.
//!
//! The one place alignment *is* required is I_PCM sample data (clause 7.3.5):
//! the encoder pads with `pcm_alignment_zero_bit` until the next byte, then the
//! raw samples themselves are whole bytes. [`BitWriter::align_zero`] is that
//! pad, and [`BitWriter::push_bytes`] is the fast path for the aligned copy
//! that follows — writing 384 PCM sample bytes one bit at a time would be a
//! needless ten-times slowdown on the single hottest loop in the encoder.
//!
//! What this module does *not* do is turn an RBSP into a NAL unit on its own.
//! Emulation prevention (clause 7.4.1.1) is a separate, stateless pass over
//! finished bytes — see [`rbsp_to_ebsp`] — kept apart from the bit writer so
//! that writer never has to reason about a byte it already flushed.

/// Accumulates syntax elements bit by bit into a byte buffer.
///
/// Bits are packed most-significant-bit first within each byte, which is what
/// every `u(n)`/`ue(v)`/`se(v)` worked example in the spec assumes. A partial
/// byte is held in `current`/`filled` until it fills up; nothing is pushed to
/// `bytes` until then, so mid-byte state never leaks into the output.
pub struct BitWriter {
	bytes: Vec<u8>,
	current: u8,
	filled: u8,
}

impl BitWriter {
	pub fn new() -> Self {
		Self {
			bytes: Vec::new(),
			current: 0,
			filled: 0,
		}
	}

	/// A single bit, MSB-first within the byte it lands in.
	fn bit(&mut self, bit: bool) {
		self.current = (self.current << 1) | (bit as u8);
		self.filled += 1;
		if self.filled == 8 {
			self.bytes.push(self.current);
			self.current = 0;
			self.filled = 0;
		}
	}

	/// `u(n)`: the low `n` bits of `value`, most-significant first.
	///
	/// `n` is capped at 32 because every fixed-width field this encoder emits
	/// — `frame_num`, `pic_order_cnt_lsb`, the two-bit `weighted_bipred_idc`,
	/// and so on — fits comfortably inside a `u32`.
	pub fn u(&mut self, n: u32, value: u32) {
		debug_assert!(
			n <= 32,
			"no fixed-width field in this encoder needs more than 32 bits"
		);
		for i in (0..n).rev() {
			self.bit((value >> i) & 1 == 1);
		}
	}

	/// `ue(v)`: Exp-Golomb coding for unsigned values (clause 9.1).
	///
	/// The code for `x` is `x + 1` written in binary, preceded by one fewer
	/// zero than that binary form has bits — e.g. `3` codes as `4` = `100`,
	/// preceded by two zeros: `00100`. That is exactly the worked example the
	/// spec gives, and the test below pins all four of them.
	pub fn ue(&mut self, value: u32) {
		let code = value
			.checked_add(1)
			.expect("ue(v) values stay well under u32::MAX in this encoder");
		let bits = 32 - code.leading_zeros();
		for _ in 0..bits - 1 {
			self.bit(false);
		}
		for i in (0..bits).rev() {
			self.bit((code >> i) & 1 == 1);
		}
	}

	/// `se(v)`: signed Exp-Golomb (clause 9.1.1), built on `ue` by the
	/// standard zig-zag: `0, 1, -1, 2, -2, ...` maps to `0, 1, 2, 3, 4, ...`.
	pub fn se(&mut self, value: i32) {
		let mapped = if value <= 0 {
			value
				.checked_neg()
				.expect("no se(v) value here is i32::MIN") as u32
				* 2
		} else {
			value as u32 * 2 - 1
		};
		self.ue(mapped);
	}

	/// Pad with zero bits up to the next byte boundary.
	///
	/// This is `pcm_alignment_zero_bit` (clause 7.3.5) when called before PCM
	/// samples, and the padding half of `rbsp_trailing_bits` when called after
	/// the stop bit. Both uses are "zero-fill to a byte", so one method serves
	/// both rather than each caller re-deriving `8 - filled`.
	pub fn align_zero(&mut self) {
		while self.filled != 0 {
			self.bit(false);
		}
	}

	/// `rbsp_trailing_bits()` (clause 7.3.2.11): a single one-bit stop marker,
	/// then zero padding to the byte boundary. Every RBSP this encoder emits —
	/// SPS, PPS, slice — ends with exactly this and nothing else.
	pub fn rbsp_trailing_bits(&mut self) {
		self.bit(true);
		self.align_zero();
	}

	/// Append already byte-aligned data directly, skipping the bit-by-bit
	/// path. Only valid once [`align_zero`](Self::align_zero) has run — the
	/// debug assertion is what catches a caller that forgot.
	pub fn push_bytes(&mut self, data: &[u8]) {
		debug_assert_eq!(
			self.filled, 0,
			"push_bytes needs a byte-aligned cursor; call align_zero first"
		);
		self.bytes.extend_from_slice(data);
	}

	/// Consume the writer, returning the accumulated RBSP bytes.
	///
	/// Panics if the last byte was never completed — every call site finishes
	/// with `rbsp_trailing_bits`, so a partial byte here means a syntax
	/// element was dropped, not a legitimate final state.
	pub fn finish(self) -> Vec<u8> {
		assert_eq!(
			self.filled, 0,
			"an RBSP must end byte-aligned; call rbsp_trailing_bits first"
		);
		self.bytes
	}
}

impl Default for BitWriter {
	fn default() -> Self {
		Self::new()
	}
}

/// Insert `emulation_prevention_three_byte` (clause 7.4.1.1) wherever three
/// bytes would otherwise read as `0x00 0x00 0x0X` for `X` in `0..=3`.
///
/// A decoder scans the bitstream for `0x00 0x00 0x01` to find the next NAL
/// unit; without this pass, genuine RBSP data containing that sequence would
/// be mistaken for a start code. Tracking a running count of consecutive zero
/// bytes and checking only the byte that would complete the forbidden
/// three-byte pattern is the standard linear-scan form of this rule — no
/// backtracking is needed because inserting `0x03` itself is never zero, so it
/// always resets the run.
pub fn rbsp_to_ebsp(rbsp: &[u8]) -> Vec<u8> {
	let mut out = Vec::with_capacity(rbsp.len() + rbsp.len() / 2 + 1);
	let mut zero_run = 0u32;

	for &byte in rbsp {
		if zero_run >= 2 && byte <= 0x03 {
			out.push(0x03);
			zero_run = 0;
		}

		out.push(byte);
		zero_run = if byte == 0 { zero_run + 1 } else { 0 };
	}

	out
}

/// The two-bit NAL reference indicator (clause 7.4.1): non-zero marks a NAL
/// unit whose picture (or parameter set) later ones may depend on.
///
/// Every slice this encoder writes is a reference — the whole delta scheme is
/// "diff against the previous coded picture", so every picture has to still be
/// around to diff against. Parameter sets always carry the highest value by
/// convention; slices here just need *some* non-zero value; `1` is that value
/// for both, kept as one constant so the choice is made once.
pub const NAL_REF_IDC_PARAMETER_SET: u8 = 3;
pub const NAL_REF_IDC_SLICE: u8 = 1;

pub const NAL_UNIT_TYPE_NON_IDR_SLICE: u8 = 1;
pub const NAL_UNIT_TYPE_IDR_SLICE: u8 = 5;
pub const NAL_UNIT_TYPE_SPS: u8 = 7;
pub const NAL_UNIT_TYPE_PPS: u8 = 8;

/// Wrap a finished RBSP into a NAL unit: the one-byte header, then the
/// emulation-prevented payload.
///
/// This is the "naked" form — no `0x00 0x00 0x00 0x01` start code — because
/// that is what an MP4 sample wants: `avcC` carries the parameter sets and
/// every sample is length-prefixed instead, so a start code would just be
/// four wasted bytes repeated on every frame.
pub fn nal_unit(ref_idc: u8, unit_type: u8, rbsp: &[u8]) -> Vec<u8> {
	debug_assert!(ref_idc <= 0b11, "nal_ref_idc is two bits");
	debug_assert!(unit_type <= 0b1_1111, "nal_unit_type is five bits");

	let header = (ref_idc << 5) | unit_type;
	let mut out = Vec::with_capacity(rbsp.len() + rbsp.len() / 2 + 2);
	out.push(header);
	out.extend(rbsp_to_ebsp(rbsp));
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	fn bits(build: impl FnOnce(&mut BitWriter)) -> Vec<u8> {
		let mut writer = BitWriter::new();
		build(&mut writer);
		writer.align_zero();
		writer.finish()
	}

	/// The spec's own worked examples (clause 9.1, Table 9-2) for `ue(v)`.
	/// Anything downstream — slice headers, macroblock types — is built on
	/// this, so a mistake here would be silent and everywhere at once.
	#[test]
	fn exp_golomb_matches_the_specs_worked_examples() {
		assert_eq!(bits(|w| w.ue(0)), [0b1000_0000]);
		assert_eq!(bits(|w| w.ue(1)), [0b0100_0000]);
		assert_eq!(bits(|w| w.ue(2)), [0b0110_0000]);
		assert_eq!(bits(|w| w.ue(3)), [0b0010_0000]);
	}

	/// Same source, the signed variant (clause 9.1.1): `se(v)` zig-zags
	/// through `ue(v)`, so this is really a test that the zig-zag mapping is
	/// the spec's and not some other equally-plausible one.
	#[test]
	fn signed_exp_golomb_matches_the_specs_worked_examples() {
		assert_eq!(bits(|w| w.se(0)), [0b1000_0000]);
		assert_eq!(bits(|w| w.se(1)), [0b0100_0000]);
		assert_eq!(bits(|w| w.se(-1)), [0b0110_0000]);
	}

	/// `u(n)` packs MSB-first and carries state across calls — two `u(4)`
	/// calls must land in the same byte, not each pad out to its own.
	#[test]
	fn fixed_width_fields_pack_msb_first_into_shared_bytes() {
		let out = bits(|w| {
			w.u(4, 0b1010);
			w.u(4, 0b0101);
		});
		assert_eq!(out, [0b1010_0101]);
	}

	/// `rbsp_trailing_bits` must fire exactly one stop bit before the zero
	/// padding — a caller that padded first, then set the stop bit, would
	/// still round-trip to the same byte count but decode as more data than
	/// was actually there.
	#[test]
	fn rbsp_trailing_bits_is_a_single_one_then_zero_padding() {
		let mut writer = BitWriter::new();
		writer.u(3, 0b101);
		writer.rbsp_trailing_bits();
		assert_eq!(writer.finish(), [0b1011_0000]);
	}

	/// `align_zero` before `push_bytes` is what lets I_PCM samples land as a
	/// raw memcpy; this pins that the padding is genuinely zero bits and that
	/// the byte position afterwards is exactly where the spec's
	/// `pcm_alignment_zero_bit` loop would have left it.
	#[test]
	fn align_zero_pads_a_partial_byte_with_zeros() {
		let mut writer = BitWriter::new();
		writer.u(3, 0b110);
		writer.align_zero();
		writer.push_bytes(&[0xAB]);
		assert_eq!(writer.finish(), [0b1100_0000, 0xAB]);
	}

	/// A cursor already on a byte boundary is untouched by `align_zero` —
	/// otherwise every whole-macroblock-aligned PCM copy would gain a spurious
	/// zero byte it never asked for.
	#[test]
	fn an_aligned_cursor_is_unaffected_by_align_zero() {
		let mut writer = BitWriter::new();
		writer.u(8, 0xFF);
		writer.align_zero();
		writer.push_bytes(&[0x01]);
		assert_eq!(writer.finish(), [0xFF, 0x01]);
	}

	/// The exact three-zero-byte case the emulation-prevention rule exists
	/// for: without the inserted `0x03`, a decoder scanning for
	/// `0x00 0x00 0x01` would read this as the start of the next NAL unit.
	#[test]
	fn a_start_code_like_run_gets_an_emulation_prevention_byte() {
		assert_eq!(rbsp_to_ebsp(&[0x00, 0x00, 0x00]), [0x00, 0x00, 0x03, 0x00]);
		assert_eq!(rbsp_to_ebsp(&[0x00, 0x00, 0x01]), [0x00, 0x00, 0x03, 0x01]);
		assert_eq!(rbsp_to_ebsp(&[0x00, 0x00, 0x02]), [0x00, 0x00, 0x03, 0x02]);
		assert_eq!(rbsp_to_ebsp(&[0x00, 0x00, 0x03]), [0x00, 0x00, 0x03, 0x03]);
	}

	/// A run of two zero bytes followed by anything above `0x03` is not
	/// ambiguous with a start code and must be left alone — over-escaping
	/// would waste a byte on every such run in a mostly-black frame.
	#[test]
	fn two_zero_bytes_followed_by_a_high_byte_need_no_escape() {
		assert_eq!(rbsp_to_ebsp(&[0x00, 0x00, 0x04]), [0x00, 0x00, 0x04]);
	}

	/// Four zero bytes in a row is two overlapping instances of the pattern,
	/// not one — the run counter must reset after the inserted `0x03` so the
	/// second pair is caught independently of the first.
	#[test]
	fn a_longer_zero_run_is_escaped_once_per_overlapping_pattern() {
		assert_eq!(
			rbsp_to_ebsp(&[0x00, 0x00, 0x00, 0x00]),
			[0x00, 0x00, 0x03, 0x00, 0x00]
		);
	}

	/// Round-tripping emulation prevention through the textbook decoder
	/// inverse (delete any `0x03` immediately after two zero bytes) must
	/// recover the original bytes exactly — that is the entire point of the
	/// scheme, and it is the one property a spot check on fixed vectors can't
	/// by itself guarantee.
	#[test]
	fn emulation_prevention_round_trips_through_its_own_inverse() {
		fn strip(ebsp: &[u8]) -> Vec<u8> {
			let mut out = Vec::new();
			let mut zero_run = 0u32;
			let mut i = 0;
			while i < ebsp.len() {
				if zero_run >= 2 && ebsp[i] == 0x03 {
					zero_run = 0;
					i += 1;
					continue;
				}
				out.push(ebsp[i]);
				zero_run = if ebsp[i] == 0 { zero_run + 1 } else { 0 };
				i += 1;
			}
			out
		}

		let samples: &[&[u8]] = &[
			&[0x67, 0x42, 0x00, 0x0a],
			&[0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04],
			&[0x00, 0x00, 0x03, 0x00, 0x00, 0x00],
			&[1, 2, 3, 0, 0, 0, 0, 0, 0, 1],
		];

		for rbsp in samples {
			let ebsp = rbsp_to_ebsp(rbsp);
			assert_eq!(&strip(&ebsp), rbsp);
		}
	}

	/// The NAL header packs `nal_ref_idc` into bits 6-5 and `nal_unit_type`
	/// into bits 4-0 (clause 7.3.1) — get the shift wrong and every decoder
	/// misreads which kind of NAL unit follows.
	#[test]
	fn the_nal_header_byte_places_ref_idc_and_type_in_their_spec_bit_positions() {
		let nal = nal_unit(NAL_REF_IDC_SLICE, NAL_UNIT_TYPE_IDR_SLICE, &[0xAB]);
		// ref_idc = 1 in bits 6-5, unit_type = 5 (0b00101) in bits 4-0, forbidden_zero_bit = 0 in bit 7:
		// 0b0_01_00101 = 0x25.
		assert_eq!(nal[0], 0x25);
		assert_eq!(
			nal[0] >> 5,
			NAL_REF_IDC_SLICE,
			"ref_idc must decode back out of bits 6-5"
		);
		assert_eq!(
			nal[0] & 0b1_1111,
			NAL_UNIT_TYPE_IDR_SLICE,
			"unit_type must decode back out of bits 4-0"
		);
		assert_eq!(&nal[1..], [0xAB]);
	}
}
