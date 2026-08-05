//! The hot loops, written once.
//!
//! Every kernel here is an `#[inline(always)]` function generic over
//! [`Simd`], invoked through [`fearless_simd::dispatch!`]. There is no second,
//! scalar copy to keep in sync: [`Level::Fallback`] instantiates the same
//! source with a one-lane backend, so the scalar path and the vector path
//! cannot drift apart. The equivalence tests at the bottom of this file assert
//! that directly — fallback and detected-level output must be byte-identical.
//!
//! The [`Level`] is detected once and threaded through everything, including
//! `vello_cpu`, which re-exports this very type from `vello_common`. One CPU
//! feature probe for the whole process.

use fearless_simd::{Level, Simd, dispatch};

/// The one-lane level, for tests that compare it against the detected one.
///
/// `dispatch!` compiles this arm away on a target that has real SIMD, so it is
/// only reachable when `fearless_simd/force_support_fallback` is on — which
/// this crate enables through its dev-dependencies, for test builds only.
#[cfg(test)]
fn scalar_level() -> Level {
	Level::Fallback(fearless_simd::Fallback::new())
}

/// BT.601 luma coefficients in Q8 fixed point, and the limited-range transform
/// that goes with them.
///
/// Fixed point rather than float because the inputs are `u8` and the output is
/// `u8`: a float round-trip would cost two conversions per lane to land on the
/// same 256 values. Q8 keeps the intermediate inside `i32` for any `u8` input,
/// so there is nothing to saturate against on the luma side.
const Y_R: i32 = 66; // 0.257 * 256
const Y_G: i32 = 129; // 0.504 * 256
const Y_B: i32 = 25; // 0.098 * 256
const Y_OFFSET: i32 = 16 << 8;

const U_R: i32 = -38; // -0.148 * 256
const U_G: i32 = -74; // -0.291 * 256
const U_B: i32 = 112; //  0.439 * 256

const V_R: i32 = 112; //  0.439 * 256
const V_G: i32 = -94; // -0.368 * 256
const V_B: i32 = -18; // -0.071 * 256

const C_OFFSET: i32 = 128 << 8;

/// Convert one RGBA row to limited-range BT.601 luma.
///
/// Chroma is deliberately not computed here. It is subsampled 2x2, so it needs
/// two rows at once; splitting luma out keeps this loop a straight lane-wise
/// pass with no cross-row addressing, which is the shape that vectorizes.
#[inline(always)]
fn luma_row<S: Simd>(_simd: S, rgba: &[u8], luma: &mut [u8]) {
	for (pixel, out) in rgba.chunks_exact(4).zip(luma.iter_mut()) {
		let r = pixel[0] as i32;
		let g = pixel[1] as i32;
		let b = pixel[2] as i32;
		*out = ((Y_R * r + Y_G * g + Y_B * b + Y_OFFSET) >> 8) as u8;
	}
}

/// Convert a 2x2 pixel block pair of rows to subsampled chroma.
///
/// Averaging in the source domain before the matrix — rather than converting
/// four pixels and averaging the results — is both cheaper and what every
/// decoder expects on the way back out.
#[inline(always)]
fn chroma_rows<S: Simd>(_simd: S, top: &[u8], bottom: &[u8], cb: &mut [u8], cr: &mut [u8]) {
	let pairs = cb.len();

	for i in 0..pairs {
		let x = i * 8; // two pixels, four bytes each

		// The final column of an odd-width frame has no right-hand partner;
		// fold it against itself so the block average stays well-defined.
		let (r, g, b) = if x + 8 <= top.len() {
			let r = top[x] as i32 + top[x + 4] as i32 + bottom[x] as i32 + bottom[x + 4] as i32;
			let g =
				top[x + 1] as i32 + top[x + 5] as i32 + bottom[x + 1] as i32 + bottom[x + 5] as i32;
			let b =
				top[x + 2] as i32 + top[x + 6] as i32 + bottom[x + 2] as i32 + bottom[x + 6] as i32;
			(r, g, b)
		} else {
			let r = 2 * (top[x] as i32 + bottom[x] as i32);
			let g = 2 * (top[x + 1] as i32 + bottom[x + 1] as i32);
			let b = 2 * (top[x + 2] as i32 + bottom[x + 2] as i32);
			(r, g, b)
		};

		let (r, g, b) = (r / 4, g / 4, b / 4);

		cb[i] = (((U_R * r + U_G * g + U_B * b + C_OFFSET) >> 8).clamp(0, 255)) as u8;
		cr[i] = (((V_R * r + V_G * g + V_B * b + C_OFFSET) >> 8).clamp(0, 255)) as u8;
	}
}

/// Hash a row of packed `Square`s.
///
/// FxHash's mixing step, applied lane-wise. The value is only ever compared
/// against another hash produced by this same function, so it needs to be fast
/// and well-distributed, not portable or stable across versions.
#[inline(always)]
fn hash_u64s<S: Simd>(_simd: S, values: &[u64], seed: u64) -> u64 {
	const K: u64 = 0x51_7c_c1_b7_27_22_0a_95;

	let mut hash = seed;
	for &value in values {
		hash = (hash.rotate_left(5) ^ value).wrapping_mul(K);
	}
	hash
}

/// Whether two rows of packed `Square`s are byte-identical.
///
/// Paired with [`hash_u64s`] rather than replacing it: the hash decides cheaply
/// that two frames *might* match, and this decides that they do. Dropping a
/// frame on a hash collision would silently corrupt a recording.
#[inline(always)]
fn u64s_eq<S: Simd>(_simd: S, a: &[u64], b: &[u64]) -> bool {
	a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x == y)
}

// --- Dispatched entry points ---
//
// Each of these is the only way the rest of the crate reaches a kernel. The
// `dispatch!` macro monomorphises the body once per SIMD level the target
// supports and selects between them on `level`, which the caller detected once
// at startup.

/// Convert a whole RGBA surface to planar YUV 4:2:0.
///
/// `luma` must be `width * height`, and each chroma plane
/// `width.div_ceil(2) * height.div_ceil(2)`.
pub fn rgba_to_yuv420(
	level: Level,
	rgba: &[u8],
	width: usize,
	height: usize,
	luma: &mut [u8],
	cb: &mut [u8],
	cr: &mut [u8],
) {
	let chroma_width = width.div_ceil(2);

	debug_assert_eq!(rgba.len(), width * height * 4);
	debug_assert_eq!(luma.len(), width * height);
	debug_assert_eq!(cb.len(), chroma_width * height.div_ceil(2));
	debug_assert_eq!(cb.len(), cr.len());

	for y in 0..height {
		let row = &rgba[y * width * 4..(y + 1) * width * 4];
		let out = &mut luma[y * width..(y + 1) * width];
		dispatch!(level, simd => luma_row(simd, row, out));
	}

	for (block, (cb_row, cr_row)) in cb
		.chunks_exact_mut(chroma_width)
		.zip(cr.chunks_exact_mut(chroma_width))
		.enumerate()
	{
		let top_index = block * 2;
		// An odd-height frame's last block has no lower row; pair the top row
		// with itself so the average is over real pixels either way.
		let bottom_index = (top_index + 1).min(height - 1);

		let top = &rgba[top_index * width * 4..(top_index + 1) * width * 4];
		let bottom = &rgba[bottom_index * width * 4..(bottom_index + 1) * width * 4];

		dispatch!(level, simd => chroma_rows(simd, top, bottom, cb_row, cr_row));
	}
}

/// Hash packed grid cells for the dedup comparison.
pub fn hash_cells(level: Level, values: &[u64], seed: u64) -> u64 {
	dispatch!(level, simd => hash_u64s(simd, values, seed))
}

/// Confirm a hash match is a real match.
pub fn cells_eq(level: Level, a: &[u64], b: &[u64]) -> bool {
	dispatch!(level, simd => u64s_eq(simd, a, b))
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A deterministic pseudo-random byte stream. Enough to exercise every
	/// lane and every rounding boundary without pulling in a rand dependency.
	fn noise(len: usize, seed: u64) -> Vec<u8> {
		let mut state = seed | 1;
		(0..len)
			.map(|_| {
				state ^= state << 13;
				state ^= state >> 7;
				state ^= state << 17;
				(state >> 24) as u8
			})
			.collect()
	}

	fn convert(
		level: Level,
		rgba: &[u8],
		width: usize,
		height: usize,
	) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
		let chroma = width.div_ceil(2) * height.div_ceil(2);
		let mut luma = vec![0u8; width * height];
		let mut cb = vec![0u8; chroma];
		let mut cr = vec![0u8; chroma];
		rgba_to_yuv420(level, rgba, width, height, &mut luma, &mut cb, &mut cr);
		(luma, cb, cr)
	}

	/// The claim the whole module rests on: one body, and the scalar
	/// instantiation agrees with the vector one exactly.
	#[test]
	fn yuv_conversion_is_identical_at_every_level() {
		for (width, height) in [(2, 2), (16, 8), (17, 9), (64, 32)] {
			let rgba = noise(width * height * 4, 0x9E37_79B9);

			let fallback = convert(scalar_level(), &rgba, width, height);
			let detected = convert(Level::new(), &rgba, width, height);

			assert_eq!(
				fallback, detected,
				"{width}x{height} diverged between levels"
			);
		}
	}

	#[test]
	fn a_flat_black_surface_converts_to_limited_range_black() {
		let (luma, cb, cr) = convert(Level::new(), &[0, 0, 0, 255].repeat(4), 2, 2);
		assert!(
			luma.iter().all(|&v| v == 16),
			"luma floor is 16, got {luma:?}"
		);
		assert!(cb.iter().all(|&v| v == 128));
		assert!(cr.iter().all(|&v| v == 128));
	}

	#[test]
	fn a_flat_white_surface_converts_to_limited_range_white() {
		let (luma, _, _) = convert(Level::new(), &[255u8; 16], 2, 2);
		assert!(
			luma.iter().all(|&v| v == 235),
			"luma ceiling is 235, got {luma:?}"
		);
	}

	#[test]
	fn hashing_agrees_between_levels_and_separates_content() {
		let cells: Vec<u64> = (0..257u64)
			.map(|i| i.wrapping_mul(0x9E37_79B9_7F4A_7C15))
			.collect();

		assert_eq!(
			hash_cells(scalar_level(), &cells, 0),
			hash_cells(Level::new(), &cells, 0),
		);

		let mut changed = cells.clone();
		changed[128] ^= 1;
		assert_ne!(
			hash_cells(Level::new(), &cells, 0),
			hash_cells(Level::new(), &changed, 0)
		);
	}

	#[test]
	fn equality_agrees_between_levels() {
		let a: Vec<u64> = (0..129).collect();
		let mut b = a.clone();

		assert!(cells_eq(scalar_level(), &a, &b));
		assert!(cells_eq(Level::new(), &a, &b));

		b[64] = u64::MAX;
		assert!(!cells_eq(scalar_level(), &a, &b));
		assert!(!cells_eq(Level::new(), &a, &b));
	}
}
