//! The browser build's font-face shaper: mln_text's HBShaper in a wasm
//! module of its own. The map's Web Worker (examples/web) imports it the
//! first time a style declares font faces, a second instance beside its own,
//! and calls it through JavaScript as its map's `FontFaceEngine`
//! (examples/web/src/shaper.rs). Maps without font faces never download
//! it.

#![cfg(target_arch = "wasm32")]

use mln_text::glyph::{GlyphID, GlyphIDType};
use mln_text::{FreeTypeLibrary, HBShapeAdjust, HBShaper};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console)]
    fn error(message: &str);
}

/// A panic traps the instance, and the caller's binding turns the trap into
/// an error; the message goes to the console first.
#[wasm_bindgen(start)]
fn start() {
    std::panic::set_hook(Box::new(|info| error(&info.to_string())));
}

/// `HBShaper(type, fontFileData)`: undefined unless `data` is a font. The
/// face keeps `data` instead of copying it.
#[wasm_bindgen(js_name = openFace)]
pub fn open_face(type_: u16, data: Vec<u8>) -> Option<Face> {
    let shaper = HBShaper::new(GlyphIDType(type_), data, &FreeTypeLibrary::new());
    shaper.valid().then_some(Face { shaper, type_: GlyphIDType(type_) })
}

/// A font file `openFace` opened.
#[wasm_bindgen]
pub struct Face {
    shaper: HBShaper,
    type_: GlyphIDType,
}

#[wasm_bindgen]
impl Face {
    /// `createComplexGlyphIDs(text, rtl)`: per glyph, its index, then its
    /// `HBShapeAdjust` (x offset, y offset, advance).
    pub fn shape(&self, text: &[u16], rtl: bool) -> Vec<f32> {
        let mut glyph_ids: Vec<GlyphID> = Vec::new();
        let mut adjusts: Vec<HBShapeAdjust> = Vec::new();
        self.shaper.create_complex_glyph_ids(text, rtl, &mut glyph_ids, &mut adjusts);
        glyph_ids
            .iter()
            .zip(&adjusts)
            .flat_map(|(id, adjust)| [f32::from(id.code), adjust.x_offset, adjust.y_offset, adjust.advance])
            .collect()
    }

    /// `rasterizeGlyph`: `[metrics, bitmap]`, the metrics a Float64Array of
    /// width, height, left, top, advance, double resolution (0 or 1), and
    /// the bitmap's width and height, the bitmap a Uint8Array.
    pub fn rasterize(&self, code: u16) -> js_sys::Array {
        let glyph = self.shaper.rasterize_glyph(&GlyphID::with_type(code, self.type_));
        let m = &glyph.metrics;
        let size = glyph.bitmap.size;
        let metrics = [
            f64::from(m.width),
            f64::from(m.height),
            f64::from(m.left),
            f64::from(m.top),
            f64::from(m.advance),
            f64::from(u8::from(m.is_double_resolution)),
            f64::from(size.width),
            f64::from(size.height),
        ];
        let metrics = js_sys::Float64Array::from(&metrics[..]);
        let bitmap = js_sys::Uint8Array::from(&glyph.bitmap.data[..]);
        js_sys::Array::of2(&metrics, &bitmap)
    }
}
