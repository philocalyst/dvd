//! The engine behind `dvd`: capture a terminal session, throw away the frames
//! that repeat, draw the ones that don't, and hand them to one or more
//! encoders.
//!
//! Four stages, and every one of them is on the same stream:
//!
//! ```text
//! rio-vt session ──snapshot──▶ dedup ──Arc<Snapshot>──▶ fanout ─┬─▶ rasterize ─┬─▶ h264
//!                              (T0–T2)                          │              └─▶ png
//!                                                               └────────────────▶ svg
//! ```
//!
//! The shape matters more than the parts. Dedup sits at the *source*, before
//! anything is drawn, so a recording that spends four seconds at an idle prompt
//! costs one rasterization rather than two hundred. Rasterization sits behind a
//! fanout, so it happens once no matter how many encoders are attached — and
//! not at all when the only output is SVG, which draws from the model directly.
//!
//! SIMD is not a separate code path. Kernels are `#[inline(always)]` functions
//! generic over [`fearless_simd::Simd`], and `dispatch!` picks the level at
//! runtime; [`fearless_simd::Level::Fallback`] *is* the scalar version, compiled
//! from the same source. The level is detected once and shared with `vello_cpu`,
//! which re-exports the very same `fearless_simd::Level`.
//!
//! ## The paint pass
//!
//! Before the paint pass existed, `render.rs` and `encode/svg.rs` were two
//! independent implementations of the same drawing — and they already disagreed.
//! Now there is one reduction from the model to shapes, and two backends that
//! receive it:
//!
//! ```text
//! Snapshot ──resolve──▶ Grid ──diff──▶ Damage
//!                       │
//!                       ├── shape (Parley, row-cached) ──▶ GlyphRun
//!                       │
//!                       └── paint(grid, damage, shaper, &mut painter)
//!                                   │
//!                                   ├─▶ VelloPainter  ──▶ Pixmap ──▶ mp4 / png
//!                                   ├─▶ SvgPainter    ──▶ markup
//!                                   └─▶ RecordPainter ──▶ test assertions
//! ```
//!
//! Parity is by construction: both backends receive identical `fill` /
//! `glyphs` / `image` calls in identical order. The parity test then measures
//! only what genuinely differs — rasterizer anti-aliasing versus resvg's —
//! instead of measuring two independent implementations of terminal drawing.

// Re-exported so `dvd` can name a colour, a level or a surface without taking
// its own dependency on the exact versions this crate was built against — the
// palette is `rio-vt`'s own type, and a mismatched second copy of it would not
// be the same type at all.
pub use fearless_simd::Level;
pub use rio_graphics;
pub use rio_vt;
pub use vello_cpu::Pixmap;

pub mod encode;
pub mod fonts;
pub mod geom;
pub mod grid;
pub mod model;
pub mod outline;
pub mod paint;
pub mod recording;
pub mod render;
pub mod session;
pub mod shape;
pub mod simd;
pub mod source;
pub mod sprite;
pub mod stream;
