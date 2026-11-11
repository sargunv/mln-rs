// Ports platform/default/include/mln/map/map_snapshotter.hpp and
// platform/default/src/mln/map/map_snapshotter.cpp.
//
// Deviation: C++ renders on a renderer thread (`util::Thread<SnapshotterRenderer>`)
// and brings the renderer's observer calls back to the snapshotter's thread
// through an actor (`ForwardingRendererObserver`). The port's renderer and
// HeadlessFrontend are single-threaded (`Rc`), so `SnapshotterRenderer` runs
// on the snapshotter's thread and calls its observer directly. The snapshot
// callback still arrives later, from the run loop, as C++'s actor delivers
// it; the port also gathers the image, attributions and transforms there
// (C++: in the still image callback, on the same thread just before), since
// `Map::render_still` can call its callback while the map is borrowed.
// TODO(port): annotations (`addAnnotationImage`, `addAnnotation`), with
// Map::addAnnotation*; `localFontFamily`.

use std::cell::{Cell, RefCell, RefMut};
use std::rc::{Rc, Weak};
use std::sync::Arc;

use mln_actor::AsyncTask;
use mln_gfx::{ContextMode, SwapBehaviour};
use mln_map::transform::Transform;
use mln_map::{CameraOptions, Map, MapLoadError, MapMode, MapObserver, MapOptions, TransformState};
use mln_renderer::{ExceptionPtr, RenderMode, RendererFrontend, RendererObserver, RenderingStats, UpdateParameters};
use mln_storage::{FileSourceManager, ResourceOptions};
use mln_util::exception::MisuseException;
use mln_util::geo::{EdgeInsets, LatLng, LatLngBounds, ScreenCoordinate, WrapMode};
use mln_util::image::PremultipliedImage;
use mln_util::size::Size;

use crate::HeadlessFrontend;

/// `MapSnapshotterObserver`.
pub trait MapSnapshotterObserver {
    fn on_did_fail_loading_style(&self, _error: &str) {}
    fn on_did_finish_loading_style(&self) {}
    fn on_style_image_missing(&self, _id: &str) {}
}

/// `MapSnapshotterObserver::nullObserver()`.
pub struct NullMapSnapshotterObserver;

impl MapSnapshotterObserver for NullMapSnapshotterObserver {}

/// `MapSnapshotter::PointForFn`.
pub type PointForFn = Box<dyn Fn(&LatLng) -> ScreenCoordinate>;
/// `MapSnapshotter::LatLngForFn`.
/// `Err`: the std::domain_error of the C++ LatLng constructor.
pub type LatLngForFn = Box<dyn Fn(&ScreenCoordinate) -> Result<LatLng, String>>;
/// `MapSnapshotter::Attributions`.
pub type Attributions = Vec<String>;
/// `MapSnapshotter::Callback` (a `std::function`: `snapshot` may call it
/// twice, see there).
pub type Callback = Box<dyn FnMut(Option<ExceptionPtr>, PremultipliedImage, Attributions, PointForFn, LatLngForFn)>;

/// `SnapshotterRenderer`: a HeadlessFrontend that reads the still image at
/// the end of the frame that answers a still image request.
struct SnapshotterRenderer {
    still_image: RefCell<PremultipliedImage>,
    has_pending_still_image_request: Cell<bool>,
    renderer_observer: RefCell<Option<Weak<dyn RendererObserver>>>,
    frontend: HeadlessFrontend,
}

impl SnapshotterRenderer {
    fn new(size: Size, pixel_ratio: f32) -> Self {
        SnapshotterRenderer {
            still_image: RefCell::new(PremultipliedImage::new(Size::default())),
            has_pending_still_image_request: Cell::new(false),
            renderer_observer: RefCell::new(None),
            frontend: HeadlessFrontend::with_options(
                size,
                pixel_ratio,
                SwapBehaviour::NoFlush,
                ContextMode::Unique,
                true,
            ),
        }
    }

    fn reset(&self) {
        self.has_pending_still_image_request.set(false);
        self.frontend.reset();
    }

    fn set_observer(self: &Rc<Self>, observer: Weak<dyn RendererObserver>) {
        *self.renderer_observer.borrow_mut() = Some(observer);
        let this: Rc<dyn RendererObserver> = self.clone();
        self.frontend.set_observer(Rc::downgrade(&this));
    }

    fn update(&self, params: Arc<UpdateParameters>) {
        self.has_pending_still_image_request.set(params.still_image_request);
        self.frontend.update(params);
    }

    fn set_size(&self, size: Size) {
        self.frontend.clone().set_size(size);
    }

    fn take_image(&self) -> PremultipliedImage {
        std::mem::replace(&mut self.still_image.borrow_mut(), PremultipliedImage::new(Size::default()))
    }

    fn renderer_observer(&self) -> Option<Rc<dyn RendererObserver>> {
        self.renderer_observer.borrow().as_ref().and_then(Weak::upgrade)
    }
}

impl RendererObserver for SnapshotterRenderer {
    fn on_invalidate(&self) {
        if let Some(observer) = self.renderer_observer() {
            observer.on_invalidate();
        }
    }

    fn on_resource_error(&self, error: ExceptionPtr) {
        self.has_pending_still_image_request.set(false);
        if let Some(observer) = self.renderer_observer() {
            observer.on_resource_error(error);
        }
    }

    fn on_will_start_rendering_map(&self) {
        if let Some(observer) = self.renderer_observer() {
            observer.on_will_start_rendering_map();
        }
    }

    fn on_will_start_rendering_frame(&self) {
        if let Some(observer) = self.renderer_observer() {
            observer.on_will_start_rendering_frame();
        }
    }

    fn on_did_finish_rendering_frame(
        &self,
        mode: RenderMode,
        repaint: bool,
        placement_changed: bool,
        stats: Arc<RenderingStats>,
    ) {
        if mode == RenderMode::Full && self.has_pending_still_image_request.get() {
            *self.still_image.borrow_mut() = self.frontend.read_still_image();
        }
        if let Some(observer) = self.renderer_observer() {
            observer.on_did_finish_rendering_frame(mode, repaint, placement_changed, stats);
        }
    }

    fn on_did_finish_rendering_map(&self) {
        if let Some(observer) = self.renderer_observer() {
            observer.on_did_finish_rendering_map();
        }
    }

    fn on_style_image_missing(&self, id: &str, done: &mut dyn FnMut()) {
        match self.renderer_observer() {
            Some(observer) => observer.on_style_image_missing(id, done),
            None => done(),
        }
    }

    fn on_remove_unused_style_images(&self, ids: &[String]) {
        if let Some(observer) = self.renderer_observer() {
            observer.on_remove_unused_style_images(ids);
        }
    }

    fn on_render_error(&self, error: ExceptionPtr) {
        if let Some(observer) = self.renderer_observer() {
            observer.on_render_error(error);
        }
    }
}

/// `SnapshotterRendererFrontend`.
#[derive(Clone)]
struct SnapshotterRendererFrontend {
    update_parameters: Rc<RefCell<Option<Arc<UpdateParameters>>>>,
    renderer: Rc<SnapshotterRenderer>,
}

impl SnapshotterRendererFrontend {
    fn new(size: Size, pixel_ratio: f32) -> Self {
        SnapshotterRendererFrontend {
            update_parameters: Rc::new(RefCell::new(None)),
            renderer: Rc::new(SnapshotterRenderer::new(size, pixel_ratio)),
        }
    }

    fn set_size(&self, size: Size) {
        self.renderer.set_size(size);
    }

    /// `getTransformState()`: the last update's, else the default.
    fn get_transform_state(&self) -> TransformState {
        self.update_parameters.borrow().as_ref().map(|params| params.transform_state.clone()).unwrap_or_default()
    }

    fn take_image(&self) -> PremultipliedImage {
        self.renderer.take_image()
    }
}

impl RendererFrontend for SnapshotterRendererFrontend {
    fn reset(&self) {
        self.renderer.reset();
    }

    fn set_observer(&self, observer: Weak<dyn RendererObserver>) {
        self.renderer.set_observer(observer);
    }

    fn update(&self, parameters: Arc<UpdateParameters>) {
        *self.update_parameters.borrow_mut() = Some(parameters.clone());
        self.renderer.update(parameters);
    }
}

/// The MapObserver half of `MapSnapshotter::Impl`.
struct SnapshotterMapObserver {
    observer: Rc<dyn MapSnapshotterObserver>,
}

impl MapObserver for SnapshotterMapObserver {
    fn on_did_fail_loading_map(&self, _error: MapLoadError, what: &str) {
        self.observer.on_did_fail_loading_style(what);
    }

    fn on_did_finish_loading_style(&self) {
        self.observer.on_did_finish_loading_style();
    }

    fn on_style_image_missing(&self, id: &str) {
        self.observer.on_style_image_missing(id);
    }
}

/// `MapSnapshotter`.
pub struct MapSnapshotter {
    /// `renderStillCallback`: the callback of the snapshot being rendered.
    render_still_callback: Rc<RefCell<Option<Callback>>>,
    /// The outcome of the still image the delivery task hands over.
    still_image_result: Rc<RefCell<Option<Option<ExceptionPtr>>>>,
    deliver: AsyncTask,
    frontend: SnapshotterRendererFrontend,
    map: Rc<RefCell<Map>>,
    region: LatLngBounds,
    region_insets: EdgeInsets,
}

impl MapSnapshotter {
    /// `MapSnapshotter(size, pixelRatio, resourceOptions, observer)`, with
    /// file sources of its own.
    pub fn with_observer(
        size: Size,
        pixel_ratio: f32,
        resource_options: &ResourceOptions,
        observer: Rc<dyn MapSnapshotterObserver>,
    ) -> Result<Self, String> {
        let frontend = SnapshotterRendererFrontend::new(size, pixel_ratio);
        let map = Rc::new(RefCell::new(Map::with_resource_options(
            &frontend,
            Rc::new(SnapshotterMapObserver { observer }),
            MapOptions::new().with_map_mode(MapMode::Static).with_size(size).with_pixel_ratio(pixel_ratio),
            &FileSourceManager::new(),
            resource_options,
        )?));

        let render_still_callback: Rc<RefCell<Option<Callback>>> = Rc::new(RefCell::new(None));
        let still_image_result: Rc<RefCell<Option<Option<ExceptionPtr>>>> = Rc::new(RefCell::new(None));
        let deliver = {
            let render_still_callback = render_still_callback.clone();
            let still_image_result = still_image_result.clone();
            let map = Rc::downgrade(&map);
            let frontend = frontend.clone();
            AsyncTask::new(move || {
                let Some(error) = still_image_result.borrow_mut().take() else {
                    return;
                };
                let Some(map) = map.upgrade() else {
                    return;
                };
                let (point_for_fn, lat_lng_for_fn, attributions) = {
                    let map = map.borrow();
                    let center = map.get_camera_options(None).center.expect("the camera's center");

                    // Create lambda that captures the current transform state
                    // and can be used to translate for geographic to screen
                    // coordinates
                    let transform_state = frontend.get_transform_state();
                    let point_for_fn: PointForFn = Box::new(move |lat_lng| {
                        let mut unwrapped_lat_lng = lat_lng.wrapped();
                        unwrapped_lat_lng.unwrap_for_shortest_path(&center);
                        Transform::from_state(transform_state.clone()).lat_lng_to_screen_coordinate(&unwrapped_lat_lng)
                    });

                    // Create lambda that captures the current transform state
                    // and can be used to translate for geographic to screen
                    // coordinates
                    let transform_state = frontend.get_transform_state();
                    let lat_lng_for_fn: LatLngForFn = Box::new(move |screen_coordinate| {
                        Transform::from_state(transform_state.clone())
                            .screen_coordinate_to_lat_lng(screen_coordinate, WrapMode::Wrapped)
                    });

                    // Collect all source attributions
                    let attributions: Attributions =
                        map.get_style().get_sources().iter().filter_map(|source| source.get_attribution()).collect();
                    (point_for_fn, lat_lng_for_fn, attributions)
                };

                let image =
                    if error.is_some() { PremultipliedImage::new(Size::default()) } else { frontend.take_image() };

                // A canceled snapshot has no callback left.
                let callback = render_still_callback.borrow_mut().take();
                if let Some(mut callback) = callback {
                    callback(error, image, attributions, point_for_fn, lat_lng_for_fn);
                }
            })
        };

        Ok(MapSnapshotter {
            render_still_callback,
            still_image_result,
            deliver,
            frontend,
            map,
            region: LatLngBounds::empty(),
            region_insets: EdgeInsets::default(),
        })
    }

    /// `MapSnapshotter(size, pixelRatio, resourceOptions)`. `Err` where the
    /// C++ Map constructor throws (an empty size).
    pub fn new(size: Size, pixel_ratio: f32, resource_options: &ResourceOptions) -> Result<Self, String> {
        Self::with_observer(size, pixel_ratio, resource_options, Rc::new(NullMapSnapshotterObserver))
    }

    /// The map, for `getStyle()`: `snapshotter.map().get_style_mut()`.
    pub fn map(&self) -> RefMut<'_, Map> {
        self.map.borrow_mut()
    }

    pub fn set_style_url(&self, style_url: &str) {
        self.map().get_style_mut().load_url(style_url);
    }

    pub fn get_style_url(&self) -> String {
        self.map.borrow().get_style().get_url()
    }

    pub fn set_style_json(&self, style_json: &str) {
        self.map().get_style_mut().load_json(style_json);
    }

    pub fn get_style_json(&self) -> String {
        self.map.borrow().get_style().get_json()
    }

    pub fn set_size(&self, size: Size) {
        // C++ ignores Map::setSize's result here too.
        let _ = self.map().set_size(size);
        self.frontend.set_size(size);
    }

    pub fn get_size(&self) -> Size {
        self.map.borrow().get_map_options().size()
    }

    /// `setCameraOptions(options)`: the region padding applies unless the
    /// options set their own.
    pub fn set_camera_options(&self, options: &CameraOptions) {
        let mut options_with_padding = *options;
        if options.padding.is_none() {
            // Apply region padding if no padding is explicitly set in camera options
            options_with_padding = options_with_padding.with_padding(Some(self.region_insets));
        }
        self.map().jump_to(&options_with_padding);
    }

    pub fn get_camera_options(&self) -> CameraOptions {
        self.map.borrow().get_camera_options(None)
    }

    /// `Err`: as `Map::camera_for_lat_lngs`.
    pub fn set_region(&mut self, region: &LatLngBounds) -> Result<(), String> {
        self.region = *region;
        self.jump_to_region()
    }

    /// `Err`: as `Map::lat_lng_bounds_for_camera`.
    pub fn get_region(&self) -> Result<LatLngBounds, String> {
        let map = self.map.borrow();
        map.lat_lng_bounds_for_camera(&map.get_camera_options(Some(self.get_padding())))
    }

    /// `Err`: as `Map::camera_for_lat_lngs`.
    pub fn set_padding(&mut self, insets: &EdgeInsets) -> Result<(), String> {
        self.region_insets = *insets;
        if !self.region.is_empty() {
            self.jump_to_region()?;
        }
        Ok(())
    }

    pub fn get_padding(&self) -> EdgeInsets {
        self.region_insets
    }

    fn jump_to_region(&self) -> Result<(), String> {
        let lat_lngs = [self.region.southwest(), self.region.northeast()];
        let camera = self.map.borrow().camera_for_lat_lngs(&lat_lngs, &self.region_insets, None, None)?;
        self.map().jump_to(&camera);
        Ok(())
    }

    /// `snapshot(callback)`: renders a still image of the map and calls
    /// `callback` with it from the run loop.
    pub fn snapshot(&self, mut callback: Callback) {
        if self.render_still_callback.borrow().is_some() {
            callback(
                Some(MisuseException::ptr("MapSnapshotter is currently rendering an image")),
                PremultipliedImage::new(Size::default()),
                Vec::new(),
                Box::new(|_| ScreenCoordinate::default()),
                Box::new(|_| Ok(LatLng::default())),
            );
            // C++ goes on to replace the current snapshot's callback with
            // this one, which is then called again with the result.
        }
        *self.render_still_callback.borrow_mut() = Some(callback);

        let still_image_result = self.still_image_result.clone();
        let deliver = self.deliver.sender();
        self.map().render_still(Box::new(move |error| {
            *still_image_result.borrow_mut() = Some(error);
            deliver.send();
        }));
    }

    /// `cancel()`: the current snapshot's callback is not called.
    pub fn cancel(&self) {
        self.render_still_callback.borrow_mut().take();
    }
}
