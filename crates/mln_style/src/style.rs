// Ports include/mln/style/style.hpp + src/mln/style/style.cpp.

use feature::Value;
use mln_util::exception::{ExceptionPtr, MisuseException};
use mln_util::immutable::Immutable;

use crate::expression::GlobalStateMap;
use crate::image;
use crate::layer::Layer;
use crate::light::Light;
use crate::map::camera::CameraOptions;
use crate::source::Source;
pub use crate::style_impl::{Impl, LayerRefMut, LightRefMut};
use crate::transition_options::TransitionOptions;

#[derive(Debug)]
pub struct Style {
    // Private implementation
    pub impl_: Box<Impl>,
}

impl Style {
    /// A style without a file source (see `Impl::new`).
    pub fn new(pixel_ratio: f32) -> Self {
        Style { impl_: Box::new(Impl::new(pixel_ratio)) }
    }

    /// `Style(std::shared_ptr<FileSource>, float pixelRatio, const TaggedScheduler&)`.
    pub fn with_file_source(
        file_source: Option<std::sync::Arc<dyn mln_storage::FileSource>>,
        pixel_ratio: f32,
        thread_pool: mln_actor::TaggedScheduler,
    ) -> Self {
        Style { impl_: Box::new(Impl::with_file_source(file_source, pixel_ratio, thread_pool)) }
    }

    pub fn load_json(&mut self, json: &str) {
        self.impl_.load_json(json);
    }

    pub fn load_url(&mut self, url: &str) {
        self.impl_.load_url(url);
    }

    pub fn get_json(&self) -> String {
        self.impl_.get_json()
    }

    pub fn get_url(&self) -> String {
        self.impl_.get_url()
    }

    /// False while a style passed to load_json/load_url is still being fetched or parsed.
    pub fn is_loaded(&self) -> bool {
        self.impl_.loaded
    }

    // Defaults
    pub fn get_name(&self) -> String {
        self.impl_.get_name()
    }

    pub fn get_default_camera(&self) -> CameraOptions {
        self.impl_.get_default_camera()
    }

    // TransitionOptions
    pub fn get_transition_options(&self) -> TransitionOptions {
        self.impl_.get_transition_options()
    }

    pub fn set_transition_options(&mut self, options: &TransitionOptions) {
        self.impl_.mutated = true;
        self.impl_.set_transition_options(options);
    }

    /// `setFallbackFontFaces(json)`: sets the font faces that every font
    /// stack falls back to after the style's own, for the styles that load
    /// after this call. `json` is an array of font files, each an object
    /// with a `url` and an optional `unicode-range`, as in a font face of a
    /// style's `font-faces`. C++ throws `std::runtime_error` if `json` isn't
    /// such an array. Without a font-face shaper, the map ignores them, as it
    /// ignores a style's `font-faces`.
    pub fn set_fallback_font_faces(&mut self, json: &str) -> Result<(), ExceptionPtr> {
        let faces = crate::parser::parse_fallback_font_faces(json)?;
        self.set_fallback_font_face_list(faces);
        Ok(())
    }

    /// `set_fallback_font_faces` with faces an app has made, such as
    /// `FontFace::fallback(url, ranges).with_data(bytes)` for a font file it
    /// holds in memory.
    pub fn set_fallback_font_face_list(&mut self, faces: mln_text::glyph::FontFaces) {
        self.impl_.set_fallback_font_faces(Some(std::sync::Arc::new(faces)));
    }

    // Light
    pub fn get_light_mut(&mut self) -> LightRefMut<'_> {
        self.impl_.mutated = true;
        self.impl_.get_light_mut()
    }

    pub fn get_light(&self) -> &Light {
        self.impl_.get_light()
    }

    pub fn set_light(&mut self, light: Light) {
        self.impl_.set_light(light);
    }

    // Global state, used by the "global-state" expression.
    pub fn get_global_state(&self) -> GlobalStateMap {
        self.impl_.get_global_state()
    }

    /// C++ throws `util::MisuseException` if the style has not finished loading.
    pub fn set_global_state_property(&mut self, name: &str, value: &Value) -> Result<(), ExceptionPtr> {
        if !self.impl_.loaded {
            return Err(MisuseException::ptr("Style is not done loading."));
        }

        self.impl_.mutated = true;
        self.impl_.set_global_state_property(name, value);
        Ok(())
    }

    // Images
    pub fn get_image(&self, name: &str) -> Option<Immutable<image::Impl>> {
        self.impl_.get_image(name)
    }

    pub fn add_image(&mut self, image: Immutable<image::Impl>) {
        self.impl_.mutated = true;
        self.impl_.add_image(image);
    }

    pub fn remove_image(&mut self, name: &str) {
        self.impl_.mutated = true;
        self.impl_.remove_image(name);
    }

    // Sources
    pub fn get_sources(&self) -> Vec<&dyn Source> {
        self.impl_.get_sources()
    }

    pub fn get_source(&self, id: &str) -> Option<&dyn Source> {
        self.impl_.get_source(id)
    }

    pub fn add_source(&mut self, source: Box<dyn Source>) -> Result<(), ExceptionPtr> {
        self.impl_.mutated = true;
        self.impl_.add_source(source)
    }

    pub fn remove_source(&mut self, source_id: &str) -> Option<Box<dyn Source>> {
        self.impl_.mutated = true;
        self.impl_.remove_source(source_id)
    }

    // Layers
    pub fn get_layers(&self) -> Vec<&dyn Layer> {
        self.impl_.get_layers()
    }

    pub fn get_layer(&self, layer_id: &str) -> Option<&dyn Layer> {
        self.impl_.get_layer(layer_id)
    }

    /// The non-const `getLayer` (marks the style mutated). The guard notifies
    /// the style of changes when dropped.
    pub fn get_layer_mut(&mut self, layer_id: &str) -> Option<LayerRefMut<'_>> {
        self.impl_.mutated = true;
        self.impl_.get_layer_mut(layer_id)
    }

    pub fn add_layer(&mut self, layer: Box<dyn Layer>, before_layer_id: Option<&str>) -> Result<(), ExceptionPtr> {
        self.impl_.mutated = true;
        self.impl_.add_layer(layer, before_layer_id).map(|_| ())
    }

    pub fn remove_layer(&mut self, id: &str) -> Option<Box<dyn Layer>> {
        self.impl_.mutated = true;
        self.impl_.remove_layer(id)
    }
}
