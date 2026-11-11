// Ports platform/default/include/mln/gfx/headless_frontend.hpp and
// platform/default/src/mln/gfx/headless_frontend.cpp.
//
// `HeadlessFrontend` is a handle: the state lives in an `Rc`, and `Clone`
// shares it. The Map keeps a clone as its `RendererFrontend` (C++: a
// reference), so `frontend.render(&mut map)` works while the map holds it.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::Arc;

use mln_actor::{AsyncTask, RunLoop};
use mln_gfx::headless_backend::HeadlessBackend as GfxHeadlessBackend;
use mln_gfx::{BackendScope, ContextMode, RendererBackend, ScopeType, SwapBehaviour};
use mln_map::{CameraOptions, Map, TransformState};
use mln_renderer::{Renderer, RendererFrontend, RendererObserver, UpdateParameters};
use mln_util::image::PremultipliedImage;
use mln_util::size::Size;
use mln_webgpu::HeadlessBackend;

/// `HeadlessFrontend::RenderResult`.
pub struct RenderResult {
    pub image: PremultipliedImage,
    // TODO(port): gfx::RenderingStats stats.
}

struct Shared {
    size: Cell<Size>,
    pixel_ratio: f32,
    frame_time: Cell<f64>,
    backend: Rc<HeadlessBackend>,
    async_invalidate: RefCell<Option<AsyncTask>>,
    invalidate_on_update: bool,
    renderer: RefCell<Option<Renderer>>,
    update_parameters: RefCell<Option<Arc<UpdateParameters>>>,
}

#[derive(Clone)]
pub struct HeadlessFrontend {
    shared: Rc<Shared>,
}

/// `{static_cast<uint32_t>(size.width * pixelRatio), static_cast<uint32_t>(size.height * pixelRatio)}`.
fn physical_size(size: Size, pixel_ratio: f32) -> Size {
    Size::new((size.width as f32 * pixel_ratio) as u32, (size.height as f32 * pixel_ratio) as u32)
}

impl HeadlessFrontend {
    /// `HeadlessFrontend(Size, float pixelRatio)` with the default swap
    /// behaviour (NoFlush), context mode (Unique), no local font family and
    /// `invalidateOnUpdate = true`.
    pub fn new(size: Size, pixel_ratio: f32) -> Self {
        Self::with_options(size, pixel_ratio, SwapBehaviour::NoFlush, ContextMode::Unique, true)
    }

    /// `HeadlessFrontend(Size, float pixelRatio, SwapBehaviour, ContextMode,
    /// localFontFamily, invalidateOnUpdate)`.
    // TODO(port): localFontFamily.
    pub fn with_options(
        size: Size,
        pixel_ratio: f32,
        swap_behavior: SwapBehaviour,
        context_mode: ContextMode,
        invalidate_on_update: bool,
    ) -> Self {
        let backend = Rc::new(HeadlessBackend::new(physical_size(size, pixel_ratio), swap_behavior, context_mode));
        let renderer_backend: Rc<dyn RendererBackend> = backend.clone();
        let shared = Rc::new(Shared {
            size: Cell::new(size),
            pixel_ratio,
            frame_time: Cell::new(0.0),
            backend,
            async_invalidate: RefCell::new(None),
            invalidate_on_update,
            renderer: RefCell::new(Some(Renderer::new(renderer_backend, pixel_ratio, None))),
            update_parameters: RefCell::new(None),
        });
        let weak: Weak<Shared> = Rc::downgrade(&shared);
        *shared.async_invalidate.borrow_mut() = Some(AsyncTask::new(move || {
            if let Some(shared) = weak.upgrade() {
                Shared::render_frame(&shared);
            }
        }));
        HeadlessFrontend { shared }
    }

    /// `HeadlessFrontend(float pixelRatio, ...)`: a 256x256 frontend.
    pub fn with_pixel_ratio(pixel_ratio: f32) -> Self {
        Self::new(Size::new(256, 256), pixel_ratio)
    }

    pub fn get_frame_time(&self) -> f64 {
        self.shared.frame_time.get()
    }

    pub fn get_size(&self) -> Size {
        self.shared.size.get()
    }

    pub fn set_size(&mut self, size: Size) {
        if self.shared.size.get() != size {
            self.shared.size.set(size);
            self.shared.backend.set_size(physical_size(size, self.shared.pixel_ratio));
        }
    }

    /// `getRenderer()`: runs `f` with the renderer (C++ returns a pointer).
    pub fn with_renderer<R>(&self, f: impl FnOnce(&mut Renderer) -> R) -> R {
        let mut renderer = self.shared.renderer.borrow_mut();
        f(renderer.as_mut().expect("renderer"))
    }

    /// `getBackend()`.
    pub fn get_backend(&self) -> &dyn RendererBackend {
        self.shared.backend.get_renderer_backend()
    }

    /// `getCameraOptions()`.
    pub fn get_camera_options(&self) -> CameraOptions {
        match self.shared.update_parameters.borrow().as_ref() {
            // RendererState::getCameraOptions(updateParameters, padding = {})
            Some(parameters) => {
                parameters.transform_state.get_camera_options(Some(mln_util::geo::EdgeInsets::default()))
            }
            None => CameraOptions::default(),
        }
    }

    /// `hasLayer(id)` (RendererState::hasLayer).
    pub fn has_layer(&self, id: &str) -> bool {
        match self.shared.update_parameters.borrow().as_ref() {
            Some(parameters) => parameters.layers.iter().any(|layer| layer.id == id),
            None => false,
        }
    }

    /// `hasSource(id)` (RendererState::hasSource).
    pub fn has_source(&self, id: &str) -> bool {
        match self.shared.update_parameters.borrow().as_ref() {
            Some(parameters) => parameters.sources.iter().any(|source| source.base().id == id),
            None => false,
        }
    }

    /// `hasImage(id)` (RendererState::hasImage).
    pub fn has_image(&self, id: &str) -> bool {
        match self.shared.update_parameters.borrow().as_ref() {
            Some(parameters) => parameters.images.iter().any(|image| image.id == id),
            None => false,
        }
    }

    // gtests-core: pixelForLatLng/latLngForPixel (test/map Map.RendererState),
    // with RendererState::pixelForLatLng/latLngForPixel inlined.
    /// `pixelForLatLng(coordinate)`.
    pub fn pixel_for_lat_lng(&self, coordinate: &mln_util::geo::LatLng) -> mln_util::geo::ScreenCoordinate {
        match self.shared.update_parameters.borrow().as_ref() {
            Some(parameters) => {
                let state = &parameters.transform_state;
                // C++ computes the unwrapped LatLng but projects `latLng`
                // (renderer_state.cpp); kept.
                let mut unwrapped_lat_lng = coordinate.wrapped();
                unwrapped_lat_lng.unwrap_for_shortest_path(&state.get_lat_lng(mln_util::geo::WrapMode::Wrapped));
                let _ = unwrapped_lat_lng;
                let point = state.lat_lng_to_screen_coordinate(coordinate);
                mln_util::geo::ScreenCoordinate::new(point.x, f64::from(state.get_size().height) - point.y)
            }
            None => mln_util::geo::ScreenCoordinate::default(),
        }
    }

    /// `latLngForPixel(point)`.
    /// `Err`: the std::domain_error of the C++ LatLng constructor.
    pub fn lat_lng_for_pixel(&self, point: &mln_util::geo::ScreenCoordinate) -> Result<mln_util::geo::LatLng, String> {
        match self.shared.update_parameters.borrow().as_ref() {
            Some(parameters) => {
                let state = &parameters.transform_state;
                let mut flipped_point = *point;
                flipped_point.y = f64::from(state.get_size().height) - flipped_point.y;
                state.screen_coordinate_to_lat_lng(&flipped_point, mln_util::geo::WrapMode::Wrapped)
            }
            None => Ok(mln_util::geo::LatLng::default()),
        }
    }

    pub fn read_still_image(&self) -> PremultipliedImage {
        self.shared.backend.read_still_image()
    }

    /// `render(Map&)`. C++ rethrows the still image error; the port returns
    /// it (its `what()` message).
    pub fn render(&mut self, map: &mut Map) -> Result<RenderResult, String> {
        let backend = self.shared.backend.clone();
        let _guard = BackendScope::new(backend.get_renderer_backend(), ScopeType::Explicit);

        let outcome: Rc<RefCell<Option<Result<(), String>>>> = Rc::new(RefCell::new(None));
        let o = outcome.clone();
        map.render_still(Box::new(move |error| {
            *o.borrow_mut() = Some(match error {
                None => Ok(()),
                Some(e) => Err(e.to_string()),
            });
        }));

        // C++ spins on runOnce, which costs a core per renderer while the
        // workers parse tiles. Sleeping as soon as the loop is idle delays
        // the short waits of a single renderer; spinning briefly keeps them.
        const SPIN: std::time::Duration = std::time::Duration::from_millis(2);
        let run_loop = RunLoop::get();
        let mut idle_since = mln_util::time::Instant::now();
        let mut image = PremultipliedImage::new(Size::default());
        loop {
            // C++ reads the image inside the callback, which runs at the end
            // of the frame; nothing renders in between.
            match outcome.borrow_mut().take() {
                Some(Ok(())) => image = backend.read_still_image(),
                Some(Err(e)) => return Err(e),
                None => {}
            }
            if image.valid() {
                break;
            }
            if run_loop.run_once() {
                idle_since = mln_util::time::Instant::now();
            } else if idle_since.elapsed() >= SPIN {
                run_loop.wait_for_work();
            }
        }

        Ok(RenderResult { image })
    }

    /// `renderOnce(Map&)`.
    pub fn render_once(&mut self, _map: &mut Map) {
        RunLoop::get().run_once();
    }

    pub fn render_frame(&self) {
        Shared::render_frame(&self.shared);
    }

    pub fn get_transform_state(&self) -> Option<TransformState> {
        self.shared.update_parameters.borrow().as_ref().map(|p| p.transform_state.clone())
    }
}

impl Shared {
    fn render_frame(shared: &Rc<Shared>) {
        let update_parameters = shared.update_parameters.borrow().clone();
        if let Some(update_parameters) = update_parameters {
            let mut renderer = shared.renderer.borrow_mut();
            if let Some(renderer) = renderer.as_mut() {
                let start_time = mln_util::time::Instant::now();
                let backend = shared.backend.clone();
                let _guard = BackendScope::new(backend.get_renderer_backend(), ScopeType::Explicit);

                // onStyleImageMissing might be called during a render. The
                // user implemented method could trigger a call to
                // MLNRenderFrontend#update which overwrites
                // `updateParameters`. Keep our own reference so that the
                // parameters aren't destroyed while `render(...)` is still
                // using them.
                renderer.render(&update_parameters);

                shared.frame_time.set(start_time.elapsed().as_secs_f64());
            }
        }
    }
}

impl RendererFrontend for HeadlessFrontend {
    fn reset(&self) {
        let renderer = self.shared.renderer.borrow_mut().take();
        drop(renderer);
    }

    fn set_observer(&self, observer: Weak<dyn RendererObserver>) {
        if let Some(renderer) = self.shared.renderer.borrow_mut().as_mut() {
            renderer.set_observer(Some(observer));
        }
    }

    fn update(&self, parameters: Arc<UpdateParameters>) {
        *self.shared.update_parameters.borrow_mut() = Some(parameters);
        if self.shared.invalidate_on_update {
            if let Some(task) = self.shared.async_invalidate.borrow().as_ref() {
                task.send();
            }
        }
    }
}

#[cfg(test)]
#[path = "headless_frontend_test.rs"]
mod headless_frontend_test;
