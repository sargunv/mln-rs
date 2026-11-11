// Ports src/mln/map/map_impl.hpp and src/mln/map/map_impl.cpp.
//
// C++ `Map::Impl` is at once the TransformObserver, the style::Observer and
// the RendererObserver, and holds references to the frontend and the
// MapObserver. The port keeps it in an `Rc` so the renderer can observe it
// through a `Weak` (C++: a raw pointer), with the mutable state in
// `Cell`/`RefCell`s:
// - The TransformObserver part only forwards to the MapObserver, so the
//   Transform gets a small forwarding object (`MapTransformObserver`).
// - The style::Observer part is `MapStyleObserver`, holding a `Weak` to the
//   Impl. The style calls it while the Map hands out `&mut Style` (inside
//   `load_json`, layer mutations, ...), when the Impl can't read the style.
//   Those calls are queued and replayed, in order, as soon as the mutable
//   borrow ends (`StyleMut` drop). Calls made while the style is not
//   borrowed mutably are handled immediately, as in C++.

use std::cell::{Cell, RefCell, RefMut};
use std::ops::{Deref, DerefMut};
use std::rc::{Rc, Weak};
use std::sync::Arc;

use mln_actor::{TaggedScheduler, scheduler};
use mln_map_types::{CameraOptions, MapDebugOptions, MapMode, TileLodMode};
use mln_renderer::{
    ExceptionPtr, FontStack, GlyphRange, RenderMode, RendererFrontend, RendererObserver, UpdateParameters,
};
use mln_storage::FileSource;
use mln_style::Style;
use mln_style::observer::{Observer as StyleObserver, SourceObserver};
use mln_style::source::Source;
use mln_style::sprite::Sprite;
use mln_util::chrono::{Clock, TimePoint};
use mln_util::exception::{NotFoundException, StyleLoadException, StyleParseException};
use mln_util::identity::SimpleIdentity;

use crate::map_observer::{CameraChangeMode, MapLoadError, MapObserver, RenderFrameStatus};
use crate::map_options::MapOptions;
use crate::transform::{Transform, TransformObserver};

/// `Map::StillImageCallback`: called with `None` on success, or the error.
pub type StillImageCallback = Box<dyn FnOnce(Option<ExceptionPtr>)>;

/// `StillImageRequest`.
pub struct StillImageRequest {
    pub callback: StillImageCallback,
}

/// The TransformObserver half of `Map::Impl`.
struct MapTransformObserver {
    observer: Rc<dyn MapObserver>,
}

impl TransformObserver for MapTransformObserver {
    fn on_camera_will_change(&self, mode: CameraChangeMode) {
        self.observer.on_camera_will_change(mode);
    }
    fn on_camera_is_changing(&self) {
        self.observer.on_camera_is_changing();
    }
    fn on_camera_did_change(&self, mode: CameraChangeMode) {
        self.observer.on_camera_did_change(mode);
    }
}

/// A style::Observer call, queued while the style is borrowed mutably.
enum StyleEvent {
    SourceChanged,
    Update,
    StyleLoading,
    StyleLoaded,
    StyleError(ExceptionPtr),
    ResourceError(ExceptionPtr),
    SpriteLoaded,
    SpriteError,
    SpriteRequested,
    /// Port-only: a `Style::loadURL` response is waiting (see
    /// `Observer::on_style_response`).
    StyleResponse,
}

/// The style::Observer half of `Map::Impl`.
struct MapStyleObserver {
    map: Weak<MapImpl>,
}

impl MapStyleObserver {
    fn dispatch(&self, event: StyleEvent) {
        if let Some(map) = self.map.upgrade() {
            map.on_style_event(event);
        }
    }
}

impl SourceObserver for MapStyleObserver {
    fn on_source_changed(&self, _source: &dyn Source) {
        self.dispatch(StyleEvent::SourceChanged);
    }
}

impl StyleObserver for MapStyleObserver {
    fn on_style_loading(&self) {
        self.dispatch(StyleEvent::StyleLoading);
    }
    fn on_style_loaded(&self) {
        self.dispatch(StyleEvent::StyleLoaded);
    }
    fn on_update(&self) {
        self.dispatch(StyleEvent::Update);
    }
    fn on_style_error(&self, error: ExceptionPtr) {
        self.dispatch(StyleEvent::StyleError(error));
    }
    fn on_resource_error(&self, error: ExceptionPtr) {
        self.dispatch(StyleEvent::ResourceError(error));
    }
    fn on_sprite_loaded(&self, _sprite: Option<&Sprite>) {
        self.dispatch(StyleEvent::SpriteLoaded);
    }
    fn on_sprite_error(&self, _sprite: Option<&Sprite>, _error: ExceptionPtr) {
        self.dispatch(StyleEvent::SpriteError);
    }
    fn on_sprite_requested(&self, _sprite: Option<&Sprite>) {
        self.dispatch(StyleEvent::SpriteRequested);
    }
    fn on_style_response(&self) {
        self.dispatch(StyleEvent::StyleResponse);
    }
}

pub struct MapImpl {
    pub(crate) observer: Rc<dyn MapObserver>,
    pub(crate) renderer_frontend: Box<dyn RendererFrontend>,
    // TODO(port): std::unique_ptr<util::ActionJournal> actionJournal.
    pub(crate) transform: RefCell<Transform>,

    pub(crate) mode: MapMode,
    pub(crate) pixel_ratio: f32,
    pub(crate) cross_source_collisions: bool,
    pub(crate) fast_pfor_enabled: bool,
    pub(crate) capture_rendered_features: bool,

    pub(crate) debug_options: Cell<MapDebugOptions>,
    // TODO(port): renderingStatsView, lastFrameStats, annotationManager.
    pub(crate) style: RefCell<Style>,
    pending_style_events: RefCell<Vec<StyleEvent>>,

    pub(crate) camera_mutated: Cell<bool>,

    pub(crate) prefetch_zoom_delta: Cell<u8>,

    pub(crate) loading: Cell<bool>,
    pub(crate) renderer_fully_loaded: Cell<bool>,
    pub(crate) still_image_request: RefCell<Option<StillImageRequest>>,

    pub(crate) tile_lod_min_radius: Cell<f64>,
    pub(crate) tile_lod_scale: Cell<f64>,
    pub(crate) tile_lod_pitch_threshold: Cell<f64>,
    pub(crate) tile_lod_zoom_shift: Cell<f64>,
    pub(crate) tile_lod_mode: Cell<TileLodMode>,

    /// `std::shared_ptr<FileSource> fileSource`. Declared after `style`:
    /// C++ destroys members in reverse order, so the style (and its sprite
    /// requests) goes first.
    pub(crate) file_source: Option<Arc<dyn FileSource>>,
}

impl MapImpl {
    /// `Map::Impl(RendererFrontend&, MapObserver&, std::shared_ptr<FileSource>, const MapOptions&)`.
    pub fn new(
        frontend: Box<dyn RendererFrontend>,
        observer: Rc<dyn MapObserver>,
        file_source: Option<Arc<dyn FileSource>>,
        map_options: &MapOptions,
    ) -> Result<Rc<MapImpl>, String> {
        let transform_observer: Rc<dyn TransformObserver> =
            Rc::new(MapTransformObserver { observer: observer.clone() });
        let pixel_ratio = map_options.pixel_ratio();
        // TODO(port): `frontend_.getThreadPool()` (RendererFrontend has no
        // thread pool yet); the background pool with a fresh tag instead.
        let thread_pool = TaggedScheduler::new(scheduler::get_background(), SimpleIdentity::new());
        let map = Rc::new(MapImpl {
            observer,
            renderer_frontend: frontend,
            transform: RefCell::new(Transform::new(
                transform_observer,
                map_options.constrain_mode(),
                map_options.viewport_mode(),
            )),
            mode: map_options.map_mode(),
            pixel_ratio,
            cross_source_collisions: map_options.cross_source_collisions(),
            fast_pfor_enabled: map_options.fast_pfor_enabled(),
            capture_rendered_features: map_options.rendered_feature_info(),
            debug_options: Cell::new(MapDebugOptions::NO_DEBUG),
            style: RefCell::new(Style::with_file_source(file_source.clone(), pixel_ratio, thread_pool)),
            pending_style_events: RefCell::new(Vec::new()),
            camera_mutated: Cell::new(false),
            // util::DEFAULT_PREFETCH_ZOOM_DELTA
            prefetch_zoom_delta: Cell::new(4),
            loading: Cell::new(false),
            // C++ leaves `rendererFullyLoaded` uninitialized until the first
            // frame or style load.
            renderer_fully_loaded: Cell::new(false),
            still_image_request: RefCell::new(None),
            tile_lod_min_radius: Cell::new(3.0),
            tile_lod_scale: Cell::new(1.0),
            tile_lod_pitch_threshold: Cell::new((60.0 / 180.0) * std::f64::consts::PI),
            tile_lod_zoom_shift: Cell::new(0.0),
            tile_lod_mode: Cell::new(TileLodMode::Default),
            file_source,
        });
        map.transform.borrow_mut().set_north_orientation(map_options.north_orientation());
        let style_observer: Rc<dyn StyleObserver> = Rc::new(MapStyleObserver { map: Rc::downgrade(&map) });
        map.style.borrow_mut().impl_.set_observer(Some(style_observer));
        let weak: Weak<MapImpl> = Rc::downgrade(&map);
        map.renderer_frontend.set_observer(weak);
        // C++ throws from the constructor for an empty size.
        map.transform.borrow_mut().resize(map_options.size())?;
        Ok(map)
    }

    // MARK: - style::Observer dispatch

    fn on_style_event(&self, event: StyleEvent) {
        if self.style.try_borrow().is_err() {
            // The style is being mutated through a StyleMut; replay when it
            // drops.
            self.pending_style_events.borrow_mut().push(event);
            return;
        }
        self.handle_style_event(event);
    }

    /// Replays the style::Observer calls queued while the style was
    /// borrowed mutably.
    pub(crate) fn process_pending_style_events(&self) {
        loop {
            let event = {
                let mut pending = self.pending_style_events.borrow_mut();
                if pending.is_empty() {
                    break;
                }
                pending.remove(0)
            };
            self.handle_style_event(event);
        }
    }

    fn handle_style_event(&self, event: StyleEvent) {
        match event {
            StyleEvent::SourceChanged => self.on_source_changed(),
            StyleEvent::Update => self.on_update(),
            StyleEvent::StyleLoading => self.on_style_loading(),
            StyleEvent::StyleLoaded => self.on_style_loaded(),
            StyleEvent::StyleError(error) => self.on_style_error(error),
            StyleEvent::ResourceError(error) => RendererObserver::on_resource_error(self, error),
            // TODO(port): MapObserver::onSpriteLoaded / onSpriteError /
            // onSpriteRequested (and the action journal).
            StyleEvent::SpriteLoaded | StyleEvent::SpriteError | StyleEvent::SpriteRequested => {}
            // C++ parses inside the file source callback (the lambda in
            // Style::Impl::loadURL); the observer calls the parse makes are
            // queued while the style is borrowed and replayed here.
            StyleEvent::StyleResponse => {
                self.style.borrow_mut().impl_.process_style_responses();
                self.process_pending_style_events();
            }
        }
    }

    // MARK: - Map::Impl StyleObserver

    fn on_source_changed(&self) {
        // TODO(port): observer.onSourceChanged(source).
    }

    pub fn on_update(&self) {
        // Don't load/render anything in still mode until explicitly requested.
        if self.mode != MapMode::Continuous && self.still_image_request.borrow().is_none() {
            return;
        }

        let time_point = if self.mode == MapMode::Continuous { Clock::now() } else { TimePoint::max() };

        self.transform.borrow_mut().update_transitions(time_point);

        let params = {
            let style = self.style.borrow();
            let style_impl = &style.impl_;
            UpdateParameters {
                style_loaded: style_impl.is_loaded(),
                mode: self.mode,
                pixel_ratio: self.pixel_ratio,
                debug_options: self.debug_options.get(),
                time_point,
                transform_state: self.transform.borrow().get_state().clone(),
                glyph_url: style_impl.get_glyph_url().to_owned(),
                font_faces: style_impl.get_font_faces(),
                sprite_loaded: style_impl.are_sprites_loaded(),
                transition_options: style_impl.get_transition_options(),
                light: style_impl.get_light_impl(),
                images: style_impl.get_image_impls(),
                sources: style_impl.get_source_impls(),
                layers: style_impl.get_layer_impls(),
                global_state: Some(style_impl.get_global_state_shared()),
                file_source: self.file_source.clone(),
                prefetch_zoom_delta: self.prefetch_zoom_delta.get(),
                still_image_request: self.still_image_request.borrow().is_some(),
                cross_source_collisions: self.cross_source_collisions,
                fast_pfor_enabled: self.fast_pfor_enabled,
                capture_rendered_features: self.capture_rendered_features,
                tile_lod_min_radius: self.tile_lod_min_radius.get(),
                tile_lod_scale: self.tile_lod_scale.get(),
                tile_lod_pitch_threshold: self.tile_lod_pitch_threshold.get(),
                tile_lod_zoom_shift: self.tile_lod_zoom_shift.get(),
                tile_lod_mode: self.tile_lod_mode.get(),
            }
        };

        // `update` takes C++'s `std::shared_ptr<UpdateParameters>` as an `Arc`.
        #[allow(clippy::arc_with_non_send_sync)]
        self.renderer_frontend.update(Arc::new(params));
    }

    pub fn on_style_loading(&self) {
        self.loading.set(true);
        self.renderer_fully_loaded.set(false);
        self.observer.on_will_start_loading_map();
    }

    pub fn on_style_loaded(&self) {
        if !self.camera_mutated.get() {
            let camera = self.style.borrow().get_default_camera();
            self.jump_to(&camera);
        }
        // TODO(port): if (LayerManager::annotationsEnabled)
        // annotationManager.onStyleLoaded().

        self.observer.on_did_finish_loading_style();

        // TODO(port): logStyleDependencies (debug builds only).
    }

    pub fn on_style_error(&self, error: ExceptionPtr) {
        let type_ = if error.downcast_ref::<StyleParseException>().is_some() {
            MapLoadError::StyleParseError
        } else if error.downcast_ref::<StyleLoadException>().is_some() {
            MapLoadError::StyleLoadError
        } else if error.downcast_ref::<NotFoundException>().is_some() {
            MapLoadError::NotFoundError
        } else {
            MapLoadError::UnknownError
        };
        let description = error.to_string();

        self.observer.on_did_fail_loading_map(type_, &description);
    }

    // MARK: - Map

    pub fn jump_to(&self, camera: &CameraOptions) {
        self.camera_mutated.set(true);
        self.transform.borrow_mut().jump_to(camera);
        self.on_update();
    }
}

impl Drop for MapImpl {
    fn drop(&mut self) {
        // Explicitly reset the RendererFrontend first to ensure it releases
        // all shared resources (AnnotationManager).
        self.renderer_frontend.reset();
    }
}

// MARK: - Map::Impl RendererObserver

impl RendererObserver for MapImpl {
    fn on_invalidate(&self) {
        self.on_update();
    }

    fn on_resource_error(&self, error: ExceptionPtr) {
        if self.mode != MapMode::Continuous {
            let request = self.still_image_request.borrow_mut().take();
            if let Some(request) = request {
                (request.callback)(Some(error));
            }
        }
    }

    fn on_will_start_rendering_frame(&self) {
        if self.mode == MapMode::Continuous {
            self.observer.on_will_start_rendering_frame();
        }
    }

    fn on_did_finish_rendering_frame(&self, render_mode: RenderMode, needs_repaint: bool, placement_changed: bool) {
        self.renderer_fully_loaded.set(render_mode == RenderMode::Full);

        // TODO(port): renderingStatsView->update(*style, *stats).

        if self.mode == MapMode::Continuous {
            let frame_status = RenderFrameStatus {
                mode: match render_mode {
                    RenderMode::Partial => crate::map_observer::RenderMode::Partial,
                    RenderMode::Full => crate::map_observer::RenderMode::Full,
                },
                needs_repaint,
                placement_changed,
            };
            self.observer.on_did_finish_rendering_frame(&frame_status);

            if needs_repaint || self.transform.borrow().in_transition() {
                self.on_update();
            } else if self.renderer_fully_loaded.get() {
                self.observer.on_did_become_idle();
            }
        } else if self.still_image_request.borrow().is_some() && self.renderer_fully_loaded.get() {
            let request = self.still_image_request.borrow_mut().take();
            if let Some(request) = request {
                (request.callback)(None);
            }
        }
    }

    fn on_will_start_rendering_map(&self) {
        if self.mode == MapMode::Continuous {
            self.observer.on_will_start_rendering_map();
        }
    }

    fn on_did_finish_rendering_map(&self) {
        if self.mode == MapMode::Continuous && self.loading.get() {
            self.observer.on_did_finish_rendering_map(crate::map_observer::RenderMode::Full);
            if self.loading.get() {
                self.loading.set(false);
                self.observer.on_did_finish_loading_map();
            }
        }
    }

    fn on_style_image_missing(&self, id: &str, done: &mut dyn FnMut()) {
        if self.style.borrow().get_image(id).is_none() {
            self.observer.on_style_image_missing(id);
        }

        done();
        self.on_update();
    }

    fn on_remove_unused_style_images(&self, unused_image_ids: &[String]) {
        for unused_image_id in unused_image_ids {
            if self.observer.on_can_remove_unused_style_image(unused_image_id) {
                self.style.borrow_mut().remove_image(unused_image_id);
                self.process_pending_style_events();
            }
        }
    }

    fn on_render_error(&self, error: ExceptionPtr) {
        self.observer.on_render_error(error);
    }

    fn on_glyphs_loaded(&self, font_stack: &FontStack, range: &GlyphRange) {
        self.observer.on_glyphs_loaded(font_stack, range);
    }

    fn on_glyphs_error(&self, font_stack: &FontStack, range: &GlyphRange, error: ExceptionPtr) {
        self.observer.on_glyphs_error(font_stack, range, error);
    }

    fn on_glyphs_requested(&self, font_stack: &FontStack, range: &GlyphRange) {
        self.observer.on_glyphs_requested(font_stack, range);
    }
}

/// The `style::Style&` that `Map::getStyle()` returns, for mutation: derefs
/// to `Style`; when it drops, the style::Observer calls the mutation made
/// are delivered to the map (see the comment at the top of this file).
pub struct StyleMut<'a> {
    map: &'a MapImpl,
    style: Option<RefMut<'a, Style>>,
}

impl<'a> StyleMut<'a> {
    pub(crate) fn new(map: &'a MapImpl) -> Self {
        StyleMut { map, style: Some(map.style.borrow_mut()) }
    }
}

impl Deref for StyleMut<'_> {
    type Target = Style;
    fn deref(&self) -> &Style {
        self.style.as_ref().expect("style")
    }
}

impl DerefMut for StyleMut<'_> {
    fn deref_mut(&mut self) -> &mut Style {
        self.style.as_mut().expect("style")
    }
}

impl Drop for StyleMut<'_> {
    fn drop(&mut self) {
        drop(self.style.take());
        self.map.process_pending_style_events();
    }
}
