// What GlyphManager shapes and rasterizes a style's "font-faces" with: the
// C++ core's HBShaper (src/mln/text/harfbuzz.hpp) behind a trait, so that a
// host can load it elsewhere. Not a C++ type.
//
// Natively, and for any build with the `font-face-shaper` feature, the
// engine is mln_text's HBShaper in-process, ready at once. The browser build
// (examples/web) turns the feature off, which leaves HarfRust, skrifa and
// the FreeType port out of its module, and sets an engine that loads them as
// a second wasm module the first time a style declares font faces
// (crates/mln_web_shaper).

use std::sync::{Arc, Mutex};

use mln_text::HBShapeAdjust;
use mln_text::glyph::{Glyph, GlyphID, GlyphIDType};
use mln_util::sync::MutexExt;

/// A font file loaded for shaping: what `HBShaper` does for GlyphManager.
pub trait FontFaceShaper {
    /// `HBShaper::createComplexGlyphIDs`: appends the glyphs of `text`
    /// (UTF-16), one run of one direction in logical order, in visual order,
    /// and their positions.
    fn shape(&self, text: &[u16], rtl: bool, glyph_ids: &mut Vec<GlyphID>, adjusts: &mut Vec<HBShapeAdjust>);

    /// `HBShaper::rasterizeGlyph`: the glyph's coverage at 24 px, before the
    /// SDF transform.
    fn rasterize(&self, glyph: &GlyphID) -> Glyph;
}

/// Loads font files as `FontFaceShaper`s.
pub trait FontFaceEngine: Send + Sync {
    /// Gets the engine ready to `load`, if it isn't yet, and then calls
    /// `ready` with whether it is: before returning, or later from any
    /// thread. It may be called again, before or after `ready` was called.
    fn prepare(&self, ready: Box<dyn FnOnce(Result<(), String>) + Send>);

    /// `HBShaper(type, fontFileData)`, once `prepare` reported the engine
    /// ready: `None` when `data` isn't a font the engine can read. The
    /// shaper may keep `data` (the response's bytes) instead of copying it.
    fn load(&self, type_: GlyphIDType, data: Arc<Vec<u8>>) -> Option<Box<dyn FontFaceShaper>>;
}

static ENGINE: Mutex<Option<Arc<dyn FontFaceEngine>>> = Mutex::new(None);

/// Sets the engine of the GlyphManagers created from now on, instead of the
/// default (mln_text's HBShaper with the `font-face-shaper` feature, else
/// none): for a host that loads the shaper itself, such as the browser
/// build.
pub fn set_font_face_engine(engine: Arc<dyn FontFaceEngine>) {
    *ENGINE.lock_unpoisoned() = Some(engine);
}

/// The engine `set_font_face_engine` set, else the default.
pub fn font_face_engine() -> Arc<dyn FontFaceEngine> {
    if let Some(engine) = ENGINE.lock_unpoisoned().clone() {
        return engine;
    }
    default_engine()
}

#[cfg(feature = "font-face-shaper")]
fn default_engine() -> Arc<dyn FontFaceEngine> {
    Arc::new(in_process::HBShaperEngine)
}

/// Without the shaper, a style's font faces load nothing until a host sets
/// an engine: their text is left out, as for a font file that fails to
/// parse.
#[cfg(not(feature = "font-face-shaper"))]
fn default_engine() -> Arc<dyn FontFaceEngine> {
    struct NoEngine;
    impl FontFaceEngine for NoEngine {
        fn prepare(&self, ready: Box<dyn FnOnce(Result<(), String>) + Send>) {
            ready(Err("this build has no font-face shaper (mln_renderer's font-face-shaper feature), \
                       and no engine was set with set_font_face_engine"
                .to_owned()));
        }

        fn load(&self, _type: GlyphIDType, _data: Arc<Vec<u8>>) -> Option<Box<dyn FontFaceShaper>> {
            None
        }
    }
    Arc::new(NoEngine)
}

#[cfg(feature = "font-face-shaper")]
mod in_process {
    use std::sync::Arc;

    use mln_text::glyph::{Glyph, GlyphID, GlyphIDType};
    use mln_text::{FreeTypeLibrary, HBShapeAdjust, HBShaper};

    use super::{FontFaceEngine, FontFaceShaper};

    /// mln_text's HBShaper in this thread, as the C++ core shapes.
    pub(super) struct HBShaperEngine;

    impl FontFaceEngine for HBShaperEngine {
        fn prepare(&self, ready: Box<dyn FnOnce(Result<(), String>) + Send>) {
            ready(Ok(()));
        }

        /// `loadHBShaper`'s `HBShaper(type, data, ftLibrary)` and `valid()`.
        fn load(&self, type_: GlyphIDType, data: Arc<Vec<u8>>) -> Option<Box<dyn FontFaceShaper>> {
            let shaper = HBShaper::new(type_, data, &FreeTypeLibrary::new());
            shaper.valid().then(|| Box::new(shaper) as Box<dyn FontFaceShaper>)
        }
    }

    impl FontFaceShaper for HBShaper {
        fn shape(&self, text: &[u16], rtl: bool, glyph_ids: &mut Vec<GlyphID>, adjusts: &mut Vec<HBShapeAdjust>) {
            self.create_complex_glyph_ids(text, rtl, glyph_ids, adjusts);
        }

        fn rasterize(&self, glyph: &GlyphID) -> Glyph {
            self.rasterize_glyph(glyph)
        }
    }
}
