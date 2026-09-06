//! Where a recording ends up.
//!
//! Four sinks, and the split between them is not cosmetic. [`png`], [`gif`], and [`mp4`]
//! consume the rasterized surface; [`svg`] consumes the model and never asks
//! for pixels at all, which is what lets a tape whose only output is an SVG skip
//! rasterization for the whole recording (see [`crate::stream::Encoder`]).
//!
//! What they share is this module: the one pass that turns a `vello_cpu`
//! [`Pixmap`] into the straight RGBA every encoder actually wants.

mod font_embed;
pub mod gif;
mod h264;
pub mod mp4;
pub mod png;
pub mod svg;

use vello_cpu::Pixmap;

/// Un-premultiply a rendered surface into straight RGBA8, row by row, into a
/// buffer whose stride may be wider than the image.
///
/// The stride is the whole reason this takes a destination rather than
/// returning a `Vec`. H.264 wants planes padded out to whole macroblocks, and
/// writing straight into the padded buffer avoids a second full-frame copy on
/// every single frame of the recording.
///
/// `destination` is left untouched outside the image rectangle, so a caller
/// that pre-filled the padding keeps it.
pub fn to_rgba(pixmap: &Pixmap, destination: &mut [u8], stride: usize) {
	let width = pixmap.width() as usize;
	let height = pixmap.height() as usize;
	let source = pixmap.data();

	debug_assert!(stride >= width * 4);
	debug_assert!(destination.len() >= (height - 1) * stride + width * 4);

	for y in 0..height {
		let row = &source[y * width..(y + 1) * width];
		let out = &mut destination[y * stride..y * stride + width * 4];

		for (pixel, slot) in row.iter().zip(out.chunks_exact_mut(4)) {
			let [r, g, b, a] = pixel.to_u8_array();

			// Opaque is the overwhelmingly common case — a terminal panel is
			// opaque everywhere except the rounded corners — so it is worth
			// keeping off the divide.
			if a == 255 || a == 0 {
				slot.copy_from_slice(&[r, g, b, a]);
				continue;
			}

			let straight = |channel: u8| ((channel as u32 * 255) / a as u32).min(255) as u8;
			slot.copy_from_slice(&[straight(r), straight(g), straight(b), a]);
		}
	}
}

/// The same conversion into a tightly packed buffer.
pub fn to_rgba_packed(pixmap: &Pixmap) -> Vec<u8> {
	let width = pixmap.width() as usize;
	let height = pixmap.height() as usize;
	let mut out = vec![0u8; width * height * 4];
	to_rgba(pixmap, &mut out, width * 4);
	out
}

#[cfg(test)]
mod tests {
	use super::*;
	use vello_cpu::color::PremulRgba8;

	#[test]
	fn an_opaque_surface_survives_the_round_trip() {
		let mut pixmap = Pixmap::new(2, 2);
		for (index, pixel) in pixmap.data_mut().iter_mut().enumerate() {
			*pixel = PremulRgba8 {
				r: index as u8 * 10,
				g: 20,
				b: 30,
				a: 255,
			};
		}

		let rgba = to_rgba_packed(&pixmap);
		assert_eq!(rgba.len(), 16);
		assert_eq!(&rgba[..4], &[0, 20, 30, 255]);
		assert_eq!(&rgba[4..8], &[10, 20, 30, 255]);
	}

	/// Half-transparent ink is stored premultiplied; the encoders want the
	/// colour it was before the multiply, or every semi-transparent pixel comes
	/// out too dark.
	#[test]
	fn a_translucent_pixel_is_un_premultiplied() {
		let mut pixmap = Pixmap::new(1, 1);
		pixmap.data_mut()[0] = PremulRgba8 {
			r: 128,
			g: 64,
			b: 0,
			a: 128,
		};

		let rgba = to_rgba_packed(&pixmap);
		assert_eq!(rgba[0], 255, "full red at half alpha stores as 128");
		assert_eq!(rgba[1], 127);
		assert_eq!(rgba[3], 128);
	}

	/// Padding a destination wider than the image must leave the extra columns
	/// alone — the mp4 sink pre-fills them and relies on keeping them.
	#[test]
	fn a_wider_stride_leaves_the_padding_untouched() {
		let mut pixmap = Pixmap::new(2, 2);
		for pixel in pixmap.data_mut().iter_mut() {
			*pixel = PremulRgba8 {
				r: 1,
				g: 2,
				b: 3,
				a: 255,
			};
		}

		let stride = 4 * 4;
		let mut destination = vec![0xAAu8; stride * 2];
		to_rgba(&pixmap, &mut destination, stride);

		assert_eq!(&destination[..8], &[1, 2, 3, 255, 1, 2, 3, 255]);
		assert_eq!(
			&destination[8..16],
			&[0xAA; 8],
			"columns past the image are the caller's"
		);
	}
}
