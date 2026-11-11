// Ports src/mln/text/harfbuzz.hpp and src/mln/text/harfbuzz.cpp (the
// MLN_TEXT_SHAPING_HARFBUZZ build).

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::freetype::FreeTypeLibrary;
use crate::glyph::{Glyph, GlyphID, GlyphIDType};
use crate::harfbuzz_impl::Impl;

/// `HBShapeAdjust`: the HarfBuzz position of a glyph in pixels at 24 px.
#[derive(Clone, Copy, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct HBShapeAdjust {
    pub x_offset: f32,
    pub y_offset: f32,
    /// advanceX
    pub advance: f32,
}

impl HBShapeAdjust {
    pub fn new(x: f32, y: f32, a: f32) -> Self {
        HBShapeAdjust { x_offset: x, y_offset: y, advance: a }
    }
}

/// `HBShaper`.
pub struct HBShaper {
    impl_: Impl,
}

impl HBShaper {
    /// `HBShaper(GlyphIDType, const std::string& fontFileData, const FreeTypeLibrary&)`.
    /// The shaper keeps `font_file_data` instead of copying it.
    pub fn new(type_: GlyphIDType, font_file_data: impl Into<Arc<Vec<u8>>>, lib: &FreeTypeLibrary) -> Self {
        HBShaper { impl_: Impl::new(type_, font_file_data, lib) }
    }

    /// `createComplexGlyphIDs`: appends the glyphs of `text` (UTF-16), one
    /// run of one direction in logical order, and their positions. Glyphs
    /// come back in visual order, so a right-to-left run's glyphs come back
    /// reversed.
    pub fn create_complex_glyph_ids(
        &self,
        text: &[u16],
        rtl: bool,
        glyph_ids: &mut Vec<GlyphID>,
        adjusts: &mut Vec<HBShapeAdjust>,
    ) {
        self.impl_.create_complex_glyph_ids(text, rtl, glyph_ids, adjusts);
    }

    /// `rasterizeGlyph`.
    pub fn rasterize_glyph(&self, glyph: &GlyphID) -> Glyph {
        self.impl_.rasterize_glyph(glyph)
    }

    /// `valid()`.
    pub fn valid(&self) -> bool {
        self.impl_.valid()
    }

    /// The implementation, for tests.
    pub fn impl_(&self) -> &Impl {
        &self.impl_
    }
}
