// Ports src/mln/style/style_impl.hpp + src/mln/style/style_impl.cpp.
//
// Deviations (structural; see also layer.rs):
// - C++ `Style::Impl` is the `LayerObserver` and `LightObserver` of its
//   layers and light. Here `get_layer_mut` / `get_light_mut` return guards
//   that call `on_layer_changed` / `on_light_changed` when dropped, if the
//   layer's or light's impl was replaced.
// - The observer is an `Rc<dyn Observer>` (C++: `Observer*`, default
//   `&nullObserver`).
// - C++ `Style::Impl` is the SpriteLoaderObserver of its sprite loader,
//   whose callbacks arrive later from the RunLoop. The members those
//   callbacks change (images, sprite loading status, last error, observer)
//   live in `SpriteShared`, shared with the loader's observer
//   (`StyleSpriteObserver`) through an `Rc`; its borrow ends before the
//   style's observer is called, so the observer can read the style.
//
// - C++ `Style::Impl` is the SourceObserver of its sources. Sources notify
//   from setters and from asynchronous request callbacks (see source.rs), so
//   the observer handed to them is a `StyleSourceObserver` sharing
//   `SpriteShared` (the style's observer and last error) and the file
//   source. `sources.update(source)` happens when the impls are read
//   (`get_source_impls`), which picks up every source whose impl changed.
//
// - `loadURL` (added by glfw): the response callback can't reach the
//   `&mut Impl` its owner holds, so it queues the response in
//   `style_responses` and calls `Observer::on_style_response`; the owner
//   (the Map) then calls `process_style_responses`, which is the body of the
//   C++ lambda.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::Arc;

use feature::Value;
use mln_actor::{TaggedScheduler, scheduler};
use mln_sprite::{SpriteLoader, SpriteLoaderObserver};
use mln_storage::{AsyncRequest, FileSource, Resource, Response};
use mln_util::error::Error;
use mln_util::identity::SimpleIdentity;
use mln_util::immutable::Immutable;
use mln_util::logging::{Event, Log};

use crate::collection::{Collection, CollectionWithPersistentOrder, ImmutableVector};
use crate::expression::GlobalStateMap;
use crate::image;
use crate::layer::{self, Layer};
use crate::light::{self, Light};
use crate::map::camera::CameraOptions;
use crate::observer::{NullObserver, Observer, SourceObserver};
use crate::parser::Parser;
use crate::source::{self, Source};
use crate::sprite::Sprite;
use crate::style::StyleLoadError;
use crate::transition_options::TransitionOptions;

pub type ImageImpls = Vec<Immutable<image::Impl>>;

/// The members of `Style::Impl` that its SpriteLoaderObserver half changes.
struct SpriteShared {
    images: Immutable<ImageImpls>,
    sprites_loading_status: HashMap<String, bool>,
    last_error: Option<Error>,
    observer: Option<Rc<dyn Observer>>,
}

impl SpriteShared {
    fn observer(&self) -> Rc<dyn Observer> {
        match &self.observer {
            Some(observer) => observer.clone(),
            None => Rc::new(NullObserver),
        }
    }
}

fn shared_observer(shared: &RefCell<SpriteShared>) -> Rc<dyn Observer> {
    shared.borrow().observer()
}

/// `Style::Impl::onSpriteLoaded`.
fn on_sprite_loaded(shared: &RefCell<SpriteShared>, sprite: Option<&Sprite>, images: Vec<Immutable<image::Impl>>) {
    {
        let mut shared = shared.borrow_mut();
        let mut new_images = (*shared.images).clone();

        let mut remaining = Vec::new();
        for image in images {
            let first = new_images.partition_point(|a| a.id < image.id);
            if first < new_images.len() && image.id == new_images[first].id {
                new_images[first] = image;
            } else {
                remaining.push(image);
            }
        }

        new_images.extend(remaining);
        new_images.sort_by(|a, b| a.id.cmp(&b.id));
        shared.images = Arc::new(new_images);
        match sprite {
            Some(sprite) => shared.sprites_loading_status.insert(sprite.id.clone(), true),
            None => shared.sprites_loading_status.insert("default".to_owned(), true),
        };
    }
    let observer = shared_observer(shared);
    observer.on_update(); // For *-pattern properties.
    observer.on_sprite_loaded(sprite);
}

/// `Style::Impl::onSpriteError`.
fn on_sprite_error(shared: &RefCell<SpriteShared>, sprite: Option<&Sprite>, error: Error) {
    shared.borrow_mut().last_error = Some(error.clone());
    Log::error(Event::Style, format!("Failed to load sprite: {error}"));
    let observer = shared_observer(shared);
    observer.on_resource_error(error.clone());
    match sprite {
        Some(sprite) => shared.borrow_mut().sprites_loading_status.insert(sprite.id.clone(), true),
        None => shared.borrow_mut().sprites_loading_status.insert("default".to_owned(), false),
    };
    // Unblock rendering tiles (even though sprite request has failed).
    observer.on_update();
    observer.on_sprite_error(sprite, error);
}

/// `Style::Impl::onSpriteRequested`.
fn on_sprite_requested(shared: &RefCell<SpriteShared>, sprite: Option<&Sprite>) {
    shared_observer(shared).on_sprite_requested(sprite);
}

/// The SpriteLoaderObserver half of `Style::Impl`.
struct StyleSpriteObserver {
    shared: std::rc::Weak<RefCell<SpriteShared>>,
}

impl SpriteLoaderObserver for StyleSpriteObserver {
    fn on_sprite_loaded(&self, sprite: Option<Sprite>, images: Vec<Immutable<image::Impl>>) {
        if let Some(shared) = self.shared.upgrade() {
            on_sprite_loaded(&shared, sprite.as_ref(), images);
        }
    }

    fn on_sprite_error(&self, sprite: Option<Sprite>, error: Error) {
        if let Some(shared) = self.shared.upgrade() {
            on_sprite_error(&shared, sprite.as_ref(), error);
        }
    }

    fn on_sprite_requested(&self, sprite: &Option<Sprite>) {
        if let Some(shared) = self.shared.upgrade() {
            on_sprite_requested(&shared, sprite.as_ref());
        }
    }
}

/// The SourceObserver half of `Style::Impl`.
struct StyleSourceObserver {
    shared: std::rc::Weak<RefCell<SpriteShared>>,
    file_source: Option<Arc<dyn FileSource>>,
}

impl StyleSourceObserver {
    fn observer(&self) -> Option<Rc<dyn Observer>> {
        self.shared.upgrade().map(|shared| shared_observer(&shared))
    }
}

impl SourceObserver for StyleSourceObserver {
    /// `onSourceLoaded(Source&)`.
    fn on_source_loaded(&self, source: &dyn Source) {
        // sources.update(source): see get_source_impls.
        if let Some(observer) = self.observer() {
            observer.on_source_loaded(source);
            observer.on_update();
        }
    }

    /// `onSourceChanged(Source&)`.
    fn on_source_changed(&self, source: &dyn Source) {
        // sources.update(source): see get_source_impls.
        if let Some(observer) = self.observer() {
            observer.on_source_changed(source);
            observer.on_update();
        }
    }

    /// `onSourceError(Source&, std::exception_ptr)`.
    fn on_source_error(&self, source: &dyn Source, error: Error) {
        let Some(shared) = self.shared.upgrade() else {
            return;
        };
        shared.borrow_mut().last_error = Some(error.clone());
        Log::error(Event::Style, format!("Failed to load source {}: {error}", source.get_id()));
        let observer = shared_observer(&shared);
        observer.on_source_error(source, error.clone());
        observer.on_resource_error(error);
    }

    /// `onSourceDescriptionChanged(Source&)`.
    fn on_source_description_changed(&self, source: &dyn Source) {
        // sources.update(source): see get_source_impls.
        if let Some(observer) = self.observer() {
            observer.on_source_description_changed(source);
        }
        if !source.loaded() {
            if let Some(file_source) = &self.file_source {
                source.load_description(&**file_source);
            }
        }
    }
}

/// `Style::Impl`.
pub struct Impl {
    // C++ declares fileSource first and destroys members in reverse order,
    // so the sprite loader (and its requests) goes before the file source;
    // Rust drops fields in declaration order.
    sprite_loader: SpriteLoader,
    /// `styleRequest`
    style_request: Option<Box<dyn AsyncRequest>>,
    /// Responses to `style_request` waiting for `process_style_responses`.
    style_responses: Rc<RefCell<VecDeque<Response>>>,
    file_source: Option<Arc<dyn FileSource>>,

    url: String,
    json: String,

    glyph_url: String,
    /// symbols: `std::shared_ptr<FontFaces> fontFaces`.
    font_faces: Option<std::sync::Arc<mln_text::glyph::FontFaces>>,
    /// symbols: `std::shared_ptr<FontFaces> fallbackFontFaces`.
    fallback_font_faces: Option<std::sync::Arc<mln_text::glyph::FontFaces>>,
    sources: CollectionWithPersistentOrder<dyn Source>,
    /// The source impls with the changes made through the sources since the
    /// collection's last update (see get_source_impls), keyed by the
    /// collection's impls they refresh.
    source_impls_cache: RefCell<Option<(ImmutableVector<dyn source::Impl>, ImmutableVector<dyn source::Impl>)>>,
    source_observer: Rc<dyn SourceObserver>,
    layers: Collection<dyn Layer>,
    transition_options: TransitionOptions,
    light: Light,
    shared: Rc<RefCell<SpriteShared>>,

    // Global state for the "global-state" expression. The map itself is
    // immutable and replaced wholesale on change so that it can be safely
    // shared across threads.
    global_state: Arc<GlobalStateMap>,
    global_state_defaults: GlobalStateMap,

    // Defaults
    name: String,
    default_camera: CameraOptions,

    pixel_ratio: f32,

    pub mutated: bool,
    pub loaded: bool,
}

impl std::fmt::Debug for Impl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("style::Impl")
            .field("url", &self.url)
            .field("layers", &self.layers.get_impls())
            .field("loaded", &self.loaded)
            .finish_non_exhaustive()
    }
}

impl Impl {
    /// A style without a file source (`Impl(nullptr, pixelRatio,
    /// threadPool)`) on the background thread pool.
    pub fn new(pixel_ratio: f32) -> Self {
        Self::with_file_source(
            None,
            pixel_ratio,
            TaggedScheduler::new(scheduler::get_background(), SimpleIdentity::new()),
        )
    }

    /// `Impl(std::shared_ptr<FileSource>, float pixelRatio, const TaggedScheduler&)`.
    pub fn with_file_source(
        file_source: Option<Arc<dyn FileSource>>,
        pixel_ratio: f32,
        thread_pool: TaggedScheduler,
    ) -> Self {
        let shared = Rc::new(RefCell::new(SpriteShared {
            images: Arc::new(Vec::new()),
            sprites_loading_status: HashMap::new(),
            last_error: None,
            observer: None,
        }));
        let sprite_loader = SpriteLoader::new(pixel_ratio, thread_pool);
        sprite_loader.set_observer(Rc::new(StyleSpriteObserver { shared: Rc::downgrade(&shared) }));
        let source_observer: Rc<dyn SourceObserver> =
            Rc::new(StyleSourceObserver { shared: Rc::downgrade(&shared), file_source: file_source.clone() });
        Impl {
            sprite_loader,
            style_request: None,
            style_responses: Rc::new(RefCell::new(VecDeque::new())),
            file_source,
            url: String::new(),
            json: String::new(),
            glyph_url: String::new(),
            font_faces: None,
            fallback_font_faces: None,
            sources: CollectionWithPersistentOrder::default(),
            source_impls_cache: RefCell::new(None),
            source_observer,
            layers: Collection::default(),
            transition_options: TransitionOptions::default(),
            light: Light::new(),
            shared,
            global_state: Arc::new(GlobalStateMap::new()),
            global_state_defaults: GlobalStateMap::new(),
            name: String::new(),
            default_camera: CameraOptions::default(),
            pixel_ratio,
            mutated: false,
            loaded: false,
        }
    }

    fn observer(&self) -> Rc<dyn Observer> {
        shared_observer(&self.shared)
    }

    /// Cancels any pending style request.
    pub fn cancel_pending_request(&mut self) {
        self.style_request = None;
        self.style_responses.borrow_mut().clear();
    }

    pub fn load_json(&mut self, json: &str) {
        self.shared.borrow_mut().last_error = None;
        self.observer().on_style_loading();

        self.cancel_pending_request();
        self.url.clear();
        self.parse(json);
    }

    pub fn load_url(&mut self, url: &str) {
        let Some(file_source) = self.file_source.clone() else {
            self.observer()
                .on_style_error(StyleLoadError::Load("Unable to find resource provider for style url.".to_owned()));
            return;
        };

        self.shared.borrow_mut().last_error = None;
        self.observer().on_style_loading();

        self.loaded = false;
        self.url = url.to_owned();

        self.style_responses.borrow_mut().clear();
        let responses = Rc::downgrade(&self.style_responses);
        let shared = Rc::downgrade(&self.shared);
        self.style_request = Some(file_source.request(
            &Resource::style(url),
            Box::new(move |res: Response| {
                let (Some(responses), Some(shared)) = (responses.upgrade(), shared.upgrade()) else {
                    return;
                };
                responses.borrow_mut().push_back(res);
                shared_observer(&shared).on_style_response();
            }),
        ));
    }

    /// The body of the `loadURL` response lambda, for each response queued
    /// since the last call (see the file comment).
    pub fn process_style_responses(&mut self) {
        loop {
            let Some(res) = self.style_responses.borrow_mut().pop_front() else {
                return;
            };

            // Don't allow a loaded, mutated style to be overwritten with a new version.
            if self.mutated && self.loaded {
                continue;
            }

            if let Some(error) = &res.error {
                let message = format!("loading style failed: {}", error.message);
                Log::error(Event::Setup, &message);
                let observer = self.observer();
                observer.on_style_error(StyleLoadError::Load(message));
                observer.on_resource_error(Error::native(error.message.clone()));
            } else if res.not_modified || res.no_content {
                continue;
            } else if let Some(data) = &res.data {
                let json = String::from_utf8_lossy(data).into_owned();
                self.parse(&json);
            }
        }
    }

    fn parse(&mut self, json: &str) {
        let mut parser = Parser::default();

        if let Err(error) = parser.parse(json) {
            let message = format!("Failed to parse style: {error}");
            Log::error(Event::ParseStyle, &message);
            let observer = self.observer();
            observer.on_style_error(StyleLoadError::Parse(message));
            observer.on_resource_error(error);
            return;
        }

        self.mutated = false;
        self.loaded = false;
        self.json = json.to_owned();

        self.sources.clear();
        self.layers.clear();
        self.shared.borrow_mut().images = Arc::new(Vec::new());

        self.transition_options = parser.transition;

        self.global_state_defaults = parser.global_state_defaults.clone();
        self.global_state = Arc::new(self.global_state_defaults.clone());

        for source in std::mem::take(&mut parser.sources) {
            // C++ lets the exception propagate; a parsed style cannot
            // contain duplicate source ids (they are object keys).
            if let Err(error) = self.add_source(source) {
                Log::error(Event::ParseStyle, error.to_string());
            }
        }

        for layer in std::mem::take(&mut parser.layers) {
            // The parser drops duplicate layer ids, so this cannot fail for
            // a source-less M1 style. TODO(port): C++ throws here for a layer
            // incompatible with its source.
            if let Err(error) = self.add_layer(layer, None) {
                Log::error(Event::ParseStyle, error.to_string());
            }
        }

        self.name = parser.name.clone();
        self.default_camera.center = Some(parser.lat_lng);
        self.default_camera.center_altitude = Some(parser.center_altitude);
        self.default_camera.zoom = Some(parser.zoom);
        self.default_camera.bearing = Some(parser.bearing);
        self.default_camera.pitch = Some(parser.pitch);
        self.default_camera.roll = Some(parser.roll);

        self.set_light(parser.light.clone());

        if let Some(file_source) = self.file_source.clone() {
            if parser.sprites.is_empty() {
                // We identify no sprite with 'default' as string in the sprite loading status.
                self.shared.borrow_mut().sprites_loading_status.insert("default".to_owned(), false);
                self.sprite_loader.load(None, &*file_source);
            } else {
                for sprite in &parser.sprites {
                    self.shared.borrow_mut().sprites_loading_status.insert(sprite.id.clone(), false);
                    self.sprite_loader.load(Some(sprite.clone()), &*file_source);
                }
            }
        } else {
            // We identify no sprite with 'default' as string in the sprite loading status.
            self.shared.borrow_mut().sprites_loading_status.insert("default".to_owned(), false);
            self.on_sprite_error(None, Error::native("Unable to find resource provider for sprite url."));
        }
        self.glyph_url = parser.glyph_url.clone();
        self.font_faces = parser.font_faces.clone();
        if let Some(fallback) = self.fallback_font_faces.as_ref().filter(|faces| !faces.is_empty()) {
            // The style's own faces come first, so they win where both cover a font.
            let mut merged = self.font_faces.as_deref().cloned().unwrap_or_default();
            merged.extend(fallback.iter().cloned());
            self.font_faces = Some(std::sync::Arc::new(merged));
        }
        self.loaded = true;
        self.observer().on_style_loaded();
    }

    pub fn get_json(&self) -> String {
        self.json.clone()
    }

    pub fn get_url(&self) -> String {
        self.url.clone()
    }

    pub fn set_observer(&mut self, observer: Option<Rc<dyn Observer>>) {
        self.shared.borrow_mut().observer = observer;
    }

    pub fn get_last_error(&self) -> Option<Error> {
        self.shared.borrow().last_error.clone()
    }

    pub fn set_transition_options(&mut self, options: &TransitionOptions) {
        if self.transition_options.duration == options.duration
            && self.transition_options.delay == options.delay
            && self.transition_options.enable_placement_transitions == options.enable_placement_transitions
        {
            return;
        }
        self.transition_options = *options;
        self.observer().on_update();
    }

    pub fn get_transition_options(&self) -> TransitionOptions {
        self.transition_options
    }

    /// Set a global state property. A null value resets the property to the
    /// default defined in the style's root "state" property (or null).
    pub fn set_global_state_property(&mut self, property: &str, value: &Value) {
        let mut new_value = value;
        if matches!(value, Value::Null) {
            if let Some(default) = self.global_state_defaults.get(property) {
                new_value = default;
            }
        }

        let unchanged = match self.global_state.get(property) {
            Some(current) => state_values_equal(current, new_value),
            None => false,
        };
        if unchanged {
            return;
        }

        let mut new_state = (*self.global_state).clone();
        new_state.insert(property.to_owned(), new_value.clone());
        self.global_state = Arc::new(new_state);
        self.reevaluate_layer_visibilities();
        self.observer().on_update();
    }

    /// Re-evaluate the visibility expressions of all layers against the
    /// current global state.
    pub fn reevaluate_layer_visibilities(&mut self) {
        let global_state = self.global_state.clone();
        let mut changed = Vec::new();
        for layer in self.layers.get_wrappers_mut() {
            let before = layer.base_impl();
            layer.reevaluate_visibility(&global_state);
            if !Arc::ptr_eq(&before, &layer.base_impl()) {
                changed.push(layer.get_id());
            }
        }
        // C++: each change reaches onLayerChanged through the observer.
        for id in changed {
            self.on_layer_changed(&id);
        }
    }

    pub fn get_global_state(&self) -> GlobalStateMap {
        (*self.global_state).clone()
    }

    pub fn get_global_state_shared(&self) -> Arc<GlobalStateMap> {
        self.global_state.clone()
    }

    // Sources

    pub fn get_sources(&self) -> Vec<&dyn Source> {
        self.sources.get_wrappers()
    }

    pub fn get_source(&self, id: &str) -> Option<&dyn Source> {
        self.sources.get(id)
    }

    /// See [`Style::add_source`](crate::Style::add_source).
    pub fn add_source(&mut self, source: Box<dyn Source>) -> Result<(), Error> {
        if self.sources.get(&source.get_id()).is_some() {
            return Err(Error::already_exists(format!("Source {} already exists", source.get_id())));
        }

        source.set_observer(Some(self.source_observer.clone()));
        let item = self.sources.add(source);
        if let Some(file_source) = &self.file_source {
            item.load_description(&**file_source);
        }
        self.observer().on_update();
        Ok(())
    }

    pub fn remove_source(&mut self, id: &str) -> Option<Box<dyn Source>> {
        // Check if source is in use
        for layer in self.layers.iter() {
            if layer.get_source_id() == id {
                Log::warning(Event::General, format!("Source '{id}' is in use, cannot remove"));
                return None;
            }
        }

        let source = self.sources.remove(id);

        if let Some(source) = &source {
            source.set_observer(None);
            self.observer().on_update();
        }

        source
    }

    // Layers

    pub fn get_layers(&self) -> Vec<&dyn Layer> {
        self.layers.get_wrappers()
    }

    pub fn get_layer(&self, id: &str) -> Option<&dyn Layer> {
        self.layers.get(id)
    }

    /// `Layer* getLayer(id)` for mutation: the guard notifies this style
    /// (`onLayerChanged`) when dropped if the layer changed.
    pub fn get_layer_mut(&mut self, id: &str) -> Option<LayerRefMut<'_>> {
        let before = self.layers.get(id)?.base_impl();
        Some(LayerRefMut { style: self, id: id.to_owned(), before })
    }

    /// `addLayer(std::unique_ptr<Layer>, before)`. See
    /// [`Style::add_layer`](crate::Style::add_layer).
    pub fn add_layer(&mut self, layer: Box<dyn Layer>, before: Option<&str>) -> Result<&mut dyn Layer, Error> {
        // TODO: verify source
        if let Some(source) = self.sources.get(&layer.get_source_id()) {
            if !source.supports_layer_type(layer.base_impl().get_type_info()) {
                let message =
                    format!("Layer '{}' is not compatible with source '{}'", layer.get_id(), layer.get_source_id());
                return Err(Error::invalid_argument(message));
            }
        }

        if self.layers.get(&layer.get_id()).is_some() {
            return Err(Error::already_exists(format!("Layer {} already exists", layer.get_id())));
        }

        let id = layer.get_id();
        self.layers.add(layer, before);
        // The global state is not available while the layer itself is parsed.
        let global_state = self.global_state.clone();
        let changed = {
            let result = self.layers.get_mut(&id).expect("layer just added");
            let before_impl = result.base_impl();
            result.reevaluate_visibility(&global_state);
            !Arc::ptr_eq(&before_impl, &result.base_impl())
        };
        if changed {
            self.layers.update(&id);
        }
        self.observer().on_update();

        Ok(self.layers.get_mut(&id).expect("layer just added"))
    }

    /// Moves the layer `id` below the layer `before`, or above every layer,
    /// as one change. Returns false when the style has no layer `id`.
    pub fn move_layer(&mut self, id: &str, before: Option<&str>) -> bool {
        let Some(layer) = self.layers.remove(id) else { return false };
        self.layers.add(layer, before);
        self.observer().on_update();
        true
    }

    pub fn remove_layer(&mut self, id: &str) -> Option<Box<dyn Layer>> {
        let layer = self.layers.remove(id);

        if layer.is_some() {
            self.observer().on_update();
        }

        layer
    }

    // Light

    pub fn set_light(&mut self, light: Light) {
        self.light = light;
        self.on_light_changed();
    }

    pub fn get_light(&self) -> &Light {
        &self.light
    }

    /// `Light* getLight()` for mutation (see `get_layer_mut`).
    pub fn get_light_mut(&mut self) -> LightRefMut<'_> {
        let before = self.light.impl_.clone();
        LightRefMut { style: self, before }
    }

    pub fn get_name(&self) -> String {
        self.name.clone()
    }

    pub fn get_default_camera(&self) -> CameraOptions {
        self.default_camera
    }

    // Images

    pub fn get_image(&self, id: &str) -> Option<Immutable<image::Impl>> {
        self.shared.borrow().images.iter().find(|image| image.id == id).cloned()
    }

    /// `addImage(std::unique_ptr<style::Image>)` (by impl).
    pub fn add_image(&mut self, image: Immutable<image::Impl>) {
        let mut new_images = (*self.shared.borrow().images).clone();
        let it = new_images.partition_point(|a| a.id < image.id);
        if it < new_images.len() && new_images[it].id == image.id {
            // We permit using addImage to update.
            new_images[it] = image;
        } else {
            new_images.insert(it, image);
        }
        self.shared.borrow_mut().images = Arc::new(new_images);
        self.observer().on_update();
    }

    pub fn remove_image(&mut self, id: &str) {
        let mut new_images = (*self.shared.borrow().images).clone();
        let Some(found) = new_images.iter().position(|image| image.id == id) else {
            Log::warning(Event::General, format!("Image '{id}' is not present in style, cannot remove"));
            return;
        };
        new_images.remove(found);
        self.shared.borrow_mut().images = Arc::new(new_images);
        self.observer().on_update();
    }

    /// symbols: `getFontFaces()`.
    pub fn get_font_faces(&self) -> Option<std::sync::Arc<mln_text::glyph::FontFaces>> {
        self.font_faces.clone()
    }

    /// symbols: `setFallbackFontFaces(faces)`.
    pub fn set_fallback_font_faces(&mut self, faces: Option<std::sync::Arc<mln_text::glyph::FontFaces>>) {
        self.fallback_font_faces = faces;
    }

    pub fn get_glyph_url(&self) -> &str {
        &self.glyph_url
    }

    pub fn get_image_impls(&self) -> Immutable<ImageImpls> {
        self.shared.borrow().images.clone()
    }

    /// `getSourceImpls()`. Applies the C++ `sources.update(source)` of the
    /// SourceObserver callbacks: a source whose impl changed since the
    /// collection last stored it gets its current impl (one new vector per
    /// change, as C++ makes one per update).
    pub fn get_source_impls(&self) -> ImmutableVector<dyn source::Impl> {
        let impls = self.sources.get_impls();
        let current = |impl_: &Immutable<dyn source::Impl>| -> Immutable<dyn source::Impl> {
            match self.sources.get(&impl_.id) {
                Some(source) => source.base_impl(),
                None => impl_.clone(),
            }
        };
        if impls.iter().all(|impl_| Arc::ptr_eq(impl_, &current(impl_))) {
            return impls;
        }
        let mut cache = self.source_impls_cache.borrow_mut();
        if let Some((base, refreshed)) = &*cache {
            if Arc::ptr_eq(base, &impls) && refreshed.iter().all(|impl_| Arc::ptr_eq(impl_, &current(impl_))) {
                return refreshed.clone();
            }
        }
        let refreshed: ImmutableVector<dyn source::Impl> = Arc::new(impls.iter().map(current).collect());
        *cache = Some((impls, refreshed.clone()));
        refreshed
    }

    pub fn get_layer_impls(&self) -> ImmutableVector<dyn layer::Impl> {
        self.layers.get_impls()
    }

    /// The light's impl, for the renderer's `UpdateParameters`.
    pub fn get_light_impl(&self) -> Immutable<light::Impl> {
        self.light.impl_.clone()
    }

    pub fn dump_debug_logs(&self) {
        Log::info(Event::General, format!("styleURL: {}", self.url));
        for source in self.sources.iter() {
            source.dump_debug_logs();
        }
    }

    pub fn are_sprites_loaded(&self) -> bool {
        let shared = self.shared.borrow();
        if shared.sprites_loading_status.is_empty() {
            return false; // If nothing is stored inside, sprites are not yet loaded.
        }
        shared.sprites_loading_status.values().all(|loaded| *loaded)
    }

    pub fn is_loaded(&self) -> bool {
        if !self.loaded {
            return false;
        }

        if !self.are_sprites_loaded() {
            return false;
        }

        self.sources.iter().all(|source| source.loaded())
    }

    pub fn pixel_ratio(&self) -> f32 {
        self.pixel_ratio
    }

    // SpriteLoaderObserver implementation.

    pub fn on_sprite_loaded(&mut self, sprite: Option<&Sprite>, images: Vec<Immutable<image::Impl>>) {
        on_sprite_loaded(&self.shared, sprite, images);
    }

    pub fn on_sprite_error(&mut self, sprite: Option<&Sprite>, error: Error) {
        on_sprite_error(&self.shared, sprite, error);
    }

    pub fn on_sprite_requested(&mut self, sprite: Option<&Sprite>) {
        on_sprite_requested(&self.shared, sprite);
    }

    // The SourceObserver implementation is StyleSourceObserver.

    // LayerObserver implementation.

    /// `onLayerChanged(Layer&)`.
    pub fn on_layer_changed(&mut self, id: &str) {
        let global_state = self.global_state.clone();
        if let Some(layer) = self.layers.get_mut(id) {
            layer.reevaluate_visibility(&global_state);
        }
        self.layers.update(id);
        self.observer().on_update();
    }

    // LightObserver implementation.

    pub fn on_light_changed(&mut self) {
        self.observer().on_update();
    }
}

/// Equality on state values that, unlike the exact-type variant equality,
/// treats numerically equal numbers of different arithmetic types as equal
/// (e.g. a style default parsed as an integer vs. a runtime-set double).
fn state_values_equal(a: &Value, b: &Value) -> bool {
    fn to_double(v: &Value) -> Option<f64> {
        match v {
            Value::Uint(u) => Some(*u as f64),
            Value::Int(i) => Some(*i as f64),
            Value::Double(d) => Some(*d),
            _ => None,
        }
    }
    if let Some(a) = to_double(a) {
        return match to_double(b) {
            Some(b) => a == b,
            None => false,
        };
    }
    match (a, b) {
        (Value::Array(array_a), Value::Array(array_b)) => {
            array_a.len() == array_b.len() && array_a.iter().zip(array_b.iter()).all(|(x, y)| state_values_equal(x, y))
        }
        (Value::Object(object_a), Value::Object(object_b)) => {
            if object_a.len() != object_b.len() {
                return false;
            }
            object_a.iter().all(|(key, value)| match object_b.get(key) {
                Some(other) => state_values_equal(value, other),
                None => false,
            })
        }
        _ => a == b,
    }
}

/// A mutable layer of a style; notifies the style when dropped (see
/// `Impl::get_layer_mut`).
pub struct LayerRefMut<'a> {
    style: &'a mut Impl,
    id: String,
    before: Immutable<dyn layer::Impl>,
}

impl std::ops::Deref for LayerRefMut<'_> {
    type Target = dyn Layer;
    fn deref(&self) -> &(dyn Layer + 'static) {
        self.style.layers.get(&self.id).expect("LayerRefMut: layer removed")
    }
}

impl std::ops::DerefMut for LayerRefMut<'_> {
    fn deref_mut(&mut self) -> &mut (dyn Layer + 'static) {
        self.style.layers.get_mut(&self.id).expect("LayerRefMut: layer removed")
    }
}

impl Drop for LayerRefMut<'_> {
    fn drop(&mut self) {
        let Some(layer) = self.style.layers.get(&self.id) else {
            return;
        };
        if !Arc::ptr_eq(&self.before, &layer.base_impl()) {
            let id = std::mem::take(&mut self.id);
            self.style.on_layer_changed(&id);
        }
    }
}

/// The mutable light of a style; notifies the style when dropped (see
/// `Impl::get_light_mut`).
pub struct LightRefMut<'a> {
    style: &'a mut Impl,
    before: Immutable<light::Impl>,
}

impl std::ops::Deref for LightRefMut<'_> {
    type Target = Light;
    fn deref(&self) -> &Light {
        &self.style.light
    }
}

impl std::ops::DerefMut for LightRefMut<'_> {
    fn deref_mut(&mut self) -> &mut Light {
        &mut self.style.light
    }
}

impl Drop for LightRefMut<'_> {
    fn drop(&mut self) {
        if !Arc::ptr_eq(&self.before, &self.style.light.impl_) {
            self.style.on_light_changed();
        }
    }
}
