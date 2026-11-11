//! Ports `src/mln/text` (text shaping and glyphs) of the C++ core, with
//! the ICU parts its platform/default BiDi uses (`icu`).
//!
//! Shaping uses HarfRust instead of HarfBuzz, and the ported FreeType subset
//! in `ft` (charmaps, sizing, advances) instead of FreeType; outlines come
//! from skrifa. HarfRust shaped 94.2% of 59,015 cases over 149
//! fonts bit-identically to HarfBuzz; harfbuzz_impl.rs says where it differs.

pub mod bidi;
#[cfg(target_family = "wasm")]
mod browser_glyph_rasterizer;
pub mod freetype;
pub mod ft;
pub mod glyph;
pub mod glyph_cache;
pub mod glyph_pbf;
pub mod harfbuzz;
pub mod harfbuzz_impl;
pub mod hb_ft;
pub mod hb_ucd;
pub mod icu;
pub mod language_tag;
pub mod local_glyph_rasterizer;
pub mod shaping;
pub mod tagged_string;
#[rustfmt::skip]
mod hb_ucd_table;
mod rasterize;
#[cfg(test)]
mod shaping_test;

pub use bidi::{BiDi, StyledText, apply_arabic_shaping, starts_right_to_left};
pub use freetype::{FreeTypeFace, FreeTypeLibrary};
pub use glyph::{
    Glyph, GlyphID, GlyphIDType, GlyphMap, GlyphMetrics, GlyphPosition, GlyphPositions, GlyphRange, PositionedGlyph,
    PositionedLine, Shaping, WritingModeType,
};
pub use glyph_cache::GlyphCache;
pub use harfbuzz::{HBShapeAdjust, HBShaper};
pub use shaping::{AnchorAlignment, PositionedIcon, get_anchor_justification, get_shaping};
pub use tagged_string::{SectionOptions, TaggedString};
