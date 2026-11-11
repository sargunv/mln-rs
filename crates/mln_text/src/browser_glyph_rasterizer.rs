//! The browser's local glyph rasterizer: draws a glyph with an
//! `OffscreenCanvas` 2D context in a CSS font family, as MapLibre GL JS
//! draws CJK glyphs in its `localIdeographFontFamily` (with TinySDF, which
//! `GlyphManager` applies here).
//!
//! The bitmap and metrics are those of C++'s Darwin and Qt rasterizers: a
//! 24px em drawn at 2x into a 60 × 60 bitmap, a 3px border, and the glyph's
//! top measured per character. The weight and style come from the font
//! stack's first font name, as GL JS sniffs them.

use std::cell::RefCell;

use js_sys::Reflect;
use mln_util::constants::ONE_EM;
use mln_util::font_stack::FontStack;
use mln_util::image::AlphaImage;
use mln_util::logging::{Event, Log};
use mln_util::size::Size;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{OffscreenCanvas, OffscreenCanvasRenderingContext2d};

use crate::glyph::{Glyph, GlyphID, GlyphMetrics};

// 2x bitmap with 1x logical metrics.
const TEXTURE_SCALE: u32 = 2;
const FONT_SIZE: f64 = ONE_EM as f64 * TEXTURE_SCALE as f64;
const LOGICAL_SIZE: u32 = 30;
const BITMAP_SIZE: u32 = LOGICAL_SIZE * TEXTURE_SCALE;
const RASTER_BUFFER: f64 = Glyph::BORDER_SIZE as f64 * TEXTURE_SCALE as f64;
const TOP_ADJUSTMENT: f64 = 26.5;

#[derive(Debug)]
pub(crate) struct BrowserGlyphRasterizer {
    font_family: String,
    /// The canvas, created at the first glyph; `None` inside if it can't be.
    canvas: RefCell<Option<Option<Canvas>>>,
}

#[derive(Debug)]
struct Canvas {
    // Kept alive with its context.
    _canvas: OffscreenCanvas,
    context: OffscreenCanvasRenderingContext2d,
    /// The context's `font`, as last set.
    font: String,
}

impl BrowserGlyphRasterizer {
    pub(crate) fn new(font_family: String) -> Self {
        BrowserGlyphRasterizer { font_family, canvas: RefCell::new(None) }
    }

    pub(crate) fn rasterize_glyph(&self, font_stack: &FontStack, glyph_id: GlyphID) -> Glyph {
        let mut canvas = self.canvas.borrow_mut();
        let canvas = canvas.get_or_insert_with(|| match Canvas::new() {
            Ok(canvas) => Some(canvas),
            Err(error) => {
                Log::warning(Event::Glyph, format!("Unable to create a canvas for local glyphs: {error:?}"));
                None
            }
        });
        let Some(canvas) = canvas else {
            return Glyph::default();
        };
        let Some(text) = char::from_u32(u32::from(glyph_id.code)) else {
            return Glyph::default();
        };
        let first = font_stack.first().map_or("", String::as_str);
        let font = format!("{} {} {FONT_SIZE}px {}", font_style(first), font_weight(first), self.font_family);
        match canvas.draw(&font, &text.to_string()) {
            Ok((bitmap, metrics)) => Glyph { id: glyph_id, bitmap, metrics },
            Err(error) => {
                Log::warning(Event::Glyph, format!("Unable to draw glyph {}: {error:?}", glyph_id.code));
                Glyph::default()
            }
        }
    }
}

impl Canvas {
    fn new() -> Result<Self, JsValue> {
        let canvas = OffscreenCanvas::new(BITMAP_SIZE, BITMAP_SIZE)?;
        let options = js_sys::Object::new();
        Reflect::set(&options, &"willReadFrequently".into(), &true.into())?;
        let context: OffscreenCanvasRenderingContext2d = canvas
            .get_context_with_context_options("2d", &options)?
            .ok_or_else(|| JsValue::from_str("no 2d context"))?
            .dyn_into()?;
        context.set_text_baseline("alphabetic");
        context.set_text_align("left");
        context.set_fill_style_str("black");
        Ok(Canvas { _canvas: canvas, context, font: String::new() })
    }

    /// Draws `text` in `font`, its baseline `RASTER_BUFFER` below the top of
    /// its ink, and returns the bitmap's alpha and the glyph's metrics.
    fn draw(&mut self, font: &str, text: &str) -> Result<(AlphaImage, GlyphMetrics), JsValue> {
        let context = &self.context;
        if self.font != font {
            context.set_font(font);
            self.font = font.to_owned();
        }
        let measured = context.measure_text(text)?;
        let glyph_top = measured.actual_bounding_box_ascent().ceil().max(0.0);
        let size = f64::from(BITMAP_SIZE);
        context.clear_rect(0.0, 0.0, size, size);
        context.fill_text(text, 0.0, RASTER_BUFFER + glyph_top)?;
        let rgba = context.get_image_data(0.0, 0.0, size, size)?.data().0;
        let alpha = rgba.chunks_exact(4).map(|texel| texel[3]).collect();

        let scale = f64::from(TEXTURE_SCALE);
        let metrics = GlyphMetrics {
            width: LOGICAL_SIZE,
            height: LOGICAL_SIZE,
            left: f32::from(Glyph::BORDER_SIZE) + 0.5,
            top: (glyph_top / scale - TOP_ADJUSTMENT) as f32,
            advance: (measured.width() / scale).round() as u32,
            is_double_resolution: true,
        };
        Ok((AlphaImage::from_vec(Size::new(BITMAP_SIZE, BITMAP_SIZE), alpha), metrics))
    }
}

/// GL JS's `_fontStyle`: the font style a font name implies.
fn font_style(font_name: &str) -> &'static str {
    let name = font_name.to_ascii_lowercase();
    if name.contains("italic") {
        "italic"
    } else if name.contains("oblique") {
        "oblique"
    } else {
        "normal"
    }
}

/// GL JS's `_fontWeight`: the weight of the last of the OpenType weight
/// names (`usWeightClass`) that a font name has as whole words.
fn font_weight(font_name: &str) -> &'static str {
    const WEIGHTS: [(&str, &str); 17] = [
        ("thin", "100"),
        ("hairline", "100"),
        ("extra light", "200"),
        ("ultra light", "200"),
        ("light", "300"),
        ("normal", "400"),
        ("regular", "400"),
        ("medium", "500"),
        ("semibold", "600"),
        ("demibold", "600"),
        ("bold", "700"),
        ("extra bold", "800"),
        ("ultra bold", "800"),
        ("black", "900"),
        ("heavy", "900"),
        ("extra black", "950"),
        ("ultra black", "950"),
    ];
    let name = font_name.to_ascii_lowercase();
    WEIGHTS.iter().rev().find(|(word, _)| has_word(&name, word)).map_or("normal", |(_, weight)| weight)
}

/// Whether `word` is in `text` between word boundaries (`\b` in a regular
/// expression).
fn has_word(text: &str, word: &str) -> bool {
    let is_word = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
    text.match_indices(word)
        .any(|(at, _)| !is_word(text[..at].chars().next_back()) && !is_word(text[at + word.len()..].chars().next()))
}
