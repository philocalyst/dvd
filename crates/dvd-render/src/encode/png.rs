//! A single still.
//!
//! The frame written is the *last* one, not the first. A tape ends where the
//! author meant it to end — the command has run, the output is on screen — and
//! the first frame is almost always an empty prompt.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use vello_cpu::Pixmap;

use crate::stream::{Context as StreamContext, Metadata, Sink};

/// Write a surface out as a PNG.
///
/// Shared with the `Screenshot` command, which needs exactly this and has no
/// sink of its own.
pub fn write(path: &Path, pixmap: &Pixmap) -> Result<()> {
	if let Some(parent) = path
		.parent()
		.filter(|parent| !parent.as_os_str().is_empty())
	{
		std::fs::create_dir_all(parent)
			.with_context(|| format!("creating {}", parent.display()))?;
	}

	let rgba = super::to_rgba_packed(pixmap);
	image::RgbaImage::from_raw(pixmap.width() as u32, pixmap.height() as u32, rgba)
		.context("the surface did not fill the image buffer")?
		.save(path)
		.with_context(|| format!("writing {}", path.display()))
}

pub struct Png {
	path: PathBuf,
	/// Straight RGBA of the most recent frame. Held rather than written on
	/// arrival because "the last frame" is not known until the stream ends.
	latest: Option<Vec<u8>>,
	width: u32,
	height: u32,
}

impl Png {
	pub fn new(path: PathBuf) -> Self {
		Self {
			path,
			latest: None,
			width: 0,
			height: 0,
		}
	}
}

impl Sink for Png {
	fn requires_pixels(&self) -> bool {
		true
	}

	fn begin(&mut self, meta: &Metadata) -> Result<()> {
		self.width = meta.width as u32;
		self.height = meta.height as u32;
		Ok(())
	}

	fn accept(&mut self, ctx: StreamContext<'_>) -> Result<()> {
		let Some(pixels) = ctx.pixels else {
			return Ok(());
		};

		// Reuse the buffer: a long recording would otherwise allocate and throw
		// away a full surface for every frame, to keep only the final one.
		let stride = self.width as usize * 4;
		let latest = self
			.latest
			.get_or_insert_with(|| vec![0u8; stride * self.height as usize]);
		super::to_rgba(pixels, latest, stride);

		Ok(())
	}

	fn finish(self: Box<Self>) -> Result<()> {
		let Some(rgba) = self.latest else {
			anyhow::bail!(
				"no frames were captured, so {} has nothing to show",
				self.path.display()
			);
		};

		if let Some(parent) = self
			.path
			.parent()
			.filter(|parent| !parent.as_os_str().is_empty())
		{
			std::fs::create_dir_all(parent)
				.with_context(|| format!("creating {}", parent.display()))?;
		}

		image::RgbaImage::from_raw(self.width, self.height, rgba)
			.context("the surface did not fill the image buffer")?
			.save(&self.path)
			.with_context(|| format!("writing {}", self.path.display()))
	}
}
