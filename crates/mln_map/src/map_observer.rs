// Ports include/mln/map/map_observer.hpp
//
// Methods take `&self` (the observer is shared with the map as
// `Rc<dyn MapObserver>`); implementations use `Cell`/`RefCell` for state.

use mln_renderer::{ExceptionPtr, FontStack, GlyphRange};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapLoadError {
    StyleParseError,
    StyleLoadError,
    NotFoundError,
    UnknownError,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum CameraChangeMode {
    Immediate,
    Animated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum RenderMode {
    Partial,
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderFrameStatus {
    pub mode: RenderMode,
    /// In continuous mode, shows that there are ongoing transitions.
    pub needs_repaint: bool,
    pub placement_changed: bool,
    // TODO(port): rendering_stats: Arc<gfx::RenderingStats>.
}

/// `MapObserver`. All methods have the C++ default (empty) bodies.
// TODO(port): onSourceChanged(style::Source&), onRegisterShaders, shader
// compile callbacks, tile/sprite request callbacks.
pub trait MapObserver {
    fn on_camera_will_change(&self, _mode: CameraChangeMode) {}
    fn on_camera_is_changing(&self) {}
    fn on_camera_did_change(&self, _mode: CameraChangeMode) {}
    fn on_will_start_loading_map(&self) {}
    fn on_did_finish_loading_map(&self) {}
    fn on_did_fail_loading_map(&self, _error: MapLoadError, _what: &str) {}
    fn on_will_start_rendering_frame(&self) {}
    fn on_did_finish_rendering_frame(&self, _status: &RenderFrameStatus) {}
    fn on_will_start_rendering_map(&self) {}
    fn on_did_finish_rendering_map(&self, _mode: RenderMode) {}
    fn on_did_finish_loading_style(&self) {}
    fn on_did_become_idle(&self) {}
    fn on_style_image_missing(&self, _id: &str) {}
    /// This method should return true if unused image can be removed, false
    /// otherwise. By default, unused image will be removed.
    fn on_can_remove_unused_style_image(&self, _id: &str) -> bool {
        true
    }
    fn on_render_error(&self, _error: ExceptionPtr) {}
    fn on_glyphs_loaded(&self, _font_stack: &FontStack, _range: &GlyphRange) {}
    /// A glyph range of a font stack failed to load or parse.
    fn on_glyphs_error(&self, _font_stack: &FontStack, _range: &GlyphRange, _error: ExceptionPtr) {}
    fn on_glyphs_requested(&self, _font_stack: &FontStack, _range: &GlyphRange) {}
    /// Called with a description of a corrupted symbol detected by the
    /// symbol guard checks.
    fn on_symbol_error(&self, _message: &str) {}
}

/// `MapObserver::nullObserver()`.
pub struct NullMapObserver;

impl MapObserver for NullMapObserver {}
