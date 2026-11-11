// Ports src/mln/text/local_glyph_rasterizer.hpp and
// platform/default/src/mln/text/local_glyph_rasterizer.cpp.

use mln_util::font_stack::FontStack;

use crate::glyph::{Glyph, GlyphID};

/// Given a font stack and a glyph ID, platform-specific implementations of
/// LocalGlyphRasterizer will decide which, if any, local fonts to use and
/// then generate a matching glyph object with a greyscale rasterization of
/// the glyph and appropriate metrics. GlyphManager will then use TinySDF to
/// transform the rasterized bitmap into an SDF.
///
/// The shared default codepoint filter is
/// `util::i18n::allowsFixedWidthGlyphGeneration`. Following
/// maplibre-gl-js, platform implementations use 2x bitmaps with 1x metrics
/// via `GlyphMetrics::isDoubleResolution`.
///
/// It is left to platform-specific implementation to decide how best to
/// map a FontStack to a particular rasterization (font, weight, style).
///
/// The default implementation simply refuses to rasterize any glyphs.
pub trait LocalGlyphRasterizer {
    // virtual so that test harness can override platform-specific behavior
    fn can_rasterize_glyph(&self, _font_stack: &FontStack, _glyph_id: GlyphID) -> bool {
        false
    }

    fn rasterize_glyph(&self, _font_stack: &FontStack, _glyph_id: GlyphID) -> Glyph {
        Glyph::default()
    }
}

/// The platform's `LocalGlyphRasterizer(const std::optional<std::string>&
/// fontFamily)`.
///
/// Natively it is platform/default's, which refuses every glyph. In the
/// browser (wasm) it draws the glyphs that
/// `allowsFixedWidthGlyphGeneration` accepts (CJK ideographs, kana, hangul
/// and the like) with a canvas in `font_family`, a CSS font-family list, as
/// MapLibre GL JS draws them in its `localIdeographFontFamily`; without a
/// font family it refuses every glyph there too.
#[derive(Debug, Default)]
pub struct DefaultLocalGlyphRasterizer {
    #[cfg(not(target_family = "wasm"))]
    _font_family: Option<String>,
    #[cfg(target_family = "wasm")]
    browser: Option<crate::browser_glyph_rasterizer::BrowserGlyphRasterizer>,
}

impl DefaultLocalGlyphRasterizer {
    #[cfg(not(target_family = "wasm"))]
    pub fn new(font_family: Option<String>) -> Self {
        DefaultLocalGlyphRasterizer { _font_family: font_family }
    }

    #[cfg(target_family = "wasm")]
    pub fn new(font_family: Option<String>) -> Self {
        DefaultLocalGlyphRasterizer {
            browser: font_family.map(crate::browser_glyph_rasterizer::BrowserGlyphRasterizer::new),
        }
    }
}

#[cfg(not(target_family = "wasm"))]
impl LocalGlyphRasterizer for DefaultLocalGlyphRasterizer {}

#[cfg(target_family = "wasm")]
impl LocalGlyphRasterizer for DefaultLocalGlyphRasterizer {
    fn can_rasterize_glyph(&self, _font_stack: &FontStack, glyph_id: GlyphID) -> bool {
        // A font face's glyph ID is a glyph index in its font, not a
        // codepoint.
        self.browser.is_some()
            && glyph_id.type_ == crate::glyph::GlyphIDType::FontPBF
            && mln_util::i18n::allows_fixed_width_glyph_generation(glyph_id.code)
    }

    fn rasterize_glyph(&self, font_stack: &FontStack, glyph_id: GlyphID) -> Glyph {
        match &self.browser {
            Some(browser) => browser.rasterize_glyph(font_stack, glyph_id),
            None => Glyph::default(),
        }
    }
}
