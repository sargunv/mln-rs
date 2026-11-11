//! The map's font faces (a style's "font-faces"), shaped and rasterized by
//! the shaper module (crates/mln_web_shaper, web/pkg/mln_web_shaper.js): the
//! map's `FontFaceEngine`. This module is built without the in-process
//! shaper, so HarfRust, skrifa and the FreeType port are only in that one,
//! which the map's Web Worker loads the first time a style declares font
//! faces, as a second wasm instance beside this one. Once it is loaded,
//! calls go to it synchronously, through JavaScript.

use std::cell::RefCell;
use std::sync::Arc;

use mln_renderer::text::font_face_engine::{FontFaceEngine, FontFaceShaper};
use mln_text::HBShapeAdjust;
use mln_text::glyph::{Glyph, GlyphID, GlyphIDType, GlyphMetrics};
use mln_util::image::AlphaImage;
use mln_util::logging::{Event, Log};
use mln_util::size::Size;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

// A panic in the shaper traps its instance, and the trap is a JavaScript
// exception here. Each call catches it, as an error: thrown through this
// module, it would unwind past the RefCell borrows of the callers.
#[wasm_bindgen]
extern "C" {
    /// The shaper module, initialized.
    type ShaperModule;

    #[wasm_bindgen(method, catch, js_name = openFace)]
    fn open_face(this: &ShaperModule, type_: u16, data: &[u8]) -> Result<Option<Face>, JsValue>;

    /// A font file it opened.
    type Face;

    #[wasm_bindgen(method, catch)]
    fn shape(this: &Face, text: &[u16], rtl: bool) -> Result<Vec<f32>, JsValue>;

    #[wasm_bindgen(method, catch)]
    fn rasterize(this: &Face, code: u16) -> Result<js_sys::Array, JsValue>;

    #[wasm_bindgen(method, catch)]
    fn free(this: &Face) -> Result<(), JsValue>;
}

type Ready = Box<dyn FnOnce(Result<(), String>) + Send>;

/// Where the shaper module is in this instance (the map's Web Worker).
enum Module {
    /// `loader()` returns a promise of the module.
    NotLoaded {
        loader: js_sys::Function,
    },
    /// Loading; these `prepare`s wait for it.
    Loading {
        waiting: Vec<Ready>,
    },
    Loaded(ShaperModule),
}

thread_local! {
    static MODULE: RefCell<Option<Module>> = const { RefCell::new(None) };
}

/// Sets how this instance loads the shaper module, unless it is loading or
/// loaded it already (for an earlier map).
pub fn set_loader(loader: js_sys::Function) {
    MODULE.with(|module| {
        let mut module = module.borrow_mut();
        if matches!(*module, None | Some(Module::NotLoaded { .. })) {
            *module = Some(Module::NotLoaded { loader });
        }
    });
}

fn message(error: &JsValue) -> String {
    match error.dyn_ref::<js_sys::Error>() {
        Some(error) => String::from(error.message()),
        None => error.as_string().unwrap_or_else(|| format!("{error:?}")),
    }
}

/// The `FontFaceEngine` on the shaper module. Its state is the instance's
/// (`MODULE`): an instance has one thread.
pub struct ShaperModuleEngine;

impl FontFaceEngine for ShaperModuleEngine {
    fn prepare(&self, ready: Ready) {
        enum Step {
            NoLoader,
            Ready,
            Wait,
            Load(js_sys::Function),
        }
        let step = MODULE.with(|module| {
            let mut module = module.borrow_mut();
            match &*module {
                None => Step::NoLoader,
                Some(Module::Loaded(_)) => Step::Ready,
                Some(Module::Loading { .. }) => Step::Wait,
                Some(Module::NotLoaded { loader }) => {
                    let loader = loader.clone();
                    *module = Some(Module::Loading { waiting: Vec::new() });
                    Step::Load(loader)
                }
            }
        });
        match step {
            Step::NoLoader => return ready(Err("the map has no shaper module loader".to_owned())),
            Step::Ready => return ready(Ok(())),
            Step::Wait | Step::Load(_) => MODULE.with(|module| {
                if let Some(Module::Loading { waiting }) = &mut *module.borrow_mut() {
                    waiting.push(ready);
                }
            }),
        }
        let Step::Load(loader) = step else { return };
        let promise = loader.call0(&JsValue::NULL).and_then(|p| p.dyn_into::<js_sys::Promise>());
        wasm_bindgen_futures::spawn_local(async move {
            let loaded = match promise {
                Ok(promise) => JsFuture::from(promise).await,
                Err(error) => Err(error),
            };
            let (waiting, result) = MODULE.with(|module| {
                let mut module = module.borrow_mut();
                let waiting = match module.take() {
                    Some(Module::Loading { waiting }) => waiting,
                    _ => Vec::new(),
                };
                match loaded {
                    Ok(loaded) => {
                        *module = Some(Module::Loaded(loaded.unchecked_into()));
                        (waiting, Ok(()))
                    }
                    Err(error) => {
                        // A later prepare (another map) tries again.
                        *module = Some(Module::NotLoaded { loader });
                        (waiting, Err(format!("the shaper module failed to load: {}", message(&error))))
                    }
                }
            });
            for ready in waiting {
                ready(result.clone());
            }
        });
    }

    fn load(&self, type_: GlyphIDType, data: Arc<Vec<u8>>) -> Option<Box<dyn FontFaceShaper>> {
        let opened = MODULE.with(|module| match &*module.borrow() {
            Some(Module::Loaded(module)) => module.open_face(type_.0, &data),
            _ => Err(JsValue::from_str("the shaper module isn't loaded")),
        });
        match opened {
            Ok(face) => face.map(|face| Box::new(ShaperFace { face, type_ }) as Box<dyn FontFaceShaper>),
            Err(error) => {
                Log::error(Event::Glyph, format!("opening a font face failed: {}", message(&error)));
                None
            }
        }
    }
}

/// A font file open in the shaper module.
struct ShaperFace {
    face: Face,
    type_: GlyphIDType,
}

impl FontFaceShaper for ShaperFace {
    fn shape(&self, text: &[u16], rtl: bool, glyph_ids: &mut Vec<GlyphID>, adjusts: &mut Vec<HBShapeAdjust>) {
        match self.face.shape(text, rtl) {
            Ok(shaped) => {
                for glyph in shaped.chunks_exact(4) {
                    glyph_ids.push(GlyphID::with_type(glyph[0] as u16, self.type_));
                    adjusts.push(HBShapeAdjust::new(glyph[1], glyph[2], glyph[3]));
                }
            }
            Err(error) => Log::error(Event::Glyph, format!("shaping with a font face failed: {}", message(&error))),
        }
    }

    fn rasterize(&self, glyph: &GlyphID) -> Glyph {
        let empty = Glyph { id: *glyph, ..Glyph::default() };
        let parts = match self.face.rasterize(glyph.code) {
            Ok(parts) => parts,
            Err(error) => {
                Log::error(Event::Glyph, format!("rasterizing a font face's glyph failed: {}", message(&error)));
                return empty;
            }
        };
        let metrics = js_sys::Float64Array::new(&parts.get(0)).to_vec();
        let bitmap = js_sys::Uint8Array::new(&parts.get(1)).to_vec();
        let [width, height, left, top, advance, double_resolution, bitmap_width, bitmap_height] = metrics[..] else {
            return empty;
        };
        let Ok(bitmap) = AlphaImage::from_slice(Size::new(bitmap_width as u32, bitmap_height as u32), &bitmap) else {
            return empty;
        };
        Glyph {
            id: *glyph,
            bitmap,
            metrics: GlyphMetrics {
                width: width as u32,
                height: height as u32,
                left: left as f32,
                top: top as f32,
                advance: advance as u32,
                is_double_resolution: double_resolution != 0.0,
            },
        }
    }
}

impl Drop for ShaperFace {
    fn drop(&mut self) {
        let _ = self.face.free();
    }
}
