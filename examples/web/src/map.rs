//! The map in a Web Worker of its own: a continuous map in the page's canvas
//! (an OffscreenCanvas), its tile workers in other Web Workers, moved by the
//! page's input through a gesture recognizer (mln_gestures).

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::Arc;

use mln_actor::RunLoop;
use mln_actor::cooperative_scheduler::CooperativeScheduler;
use mln_gestures::{Button, GestureRecognizer, Input, Key, Modifiers, PointerKind, WheelDelta};
use mln_gfx::{BackendScope, RendererBackend, ScopeType};
use mln_map::{
    AnimationOptions, CameraOptions, CameraUpdate, CameraUpdateMode, GesturePhase, Map, MapLoadError, MapObserver,
    MapOptions,
};
use mln_renderer::text::font_face_engine;
use mln_renderer::tile::worker_codec::{Encoded, SerializingTransport, WorkerInstances};
use mln_renderer::{Renderer, RendererFrontend, RendererObserver, UpdateParameters};
use mln_storage::{ClientOptions, ResourceOptions};
use mln_util::LatLng;
use mln_util::chrono::Duration;
use mln_util::geo::ScreenCoordinate;
use mln_util::host_bytes::HostBytes;
use mln_util::serialization::payload::Payload;
use mln_util::size::Size;
use mln_webgpu::SurfaceBackend;
use wasm_bindgen::prelude::*;

use crate::shaper::{self, ShaperModuleEngine};
use crate::{cooperative_scheduler, deadline, js_error};

/// The Web Workers that run the map's tile workers, reached through a
/// JavaScript function `post(instance, bytes)`. Their replies come back
/// through `WebMap::receive`, with their payloads beside them.
struct WebWorkers {
    count: usize,
    post: js_sys::Function,
}

impl WorkerInstances for WebWorkers {
    fn count(&self) -> usize {
        self.count
    }

    fn post(&self, instance: usize, request: Vec<u8>) {
        let bytes = js_sys::Uint8Array::from(&request[..]);
        if let Err(error) = self.post.call2(&JsValue::NULL, &JsValue::from(instance as u32), &bytes) {
            web_sys::console::error_2(&JsValue::from_str("posting to a tile worker failed:"), &error);
        }
    }
}

thread_local! {
    /// The worker's `wake` (see `WebMap::create`).
    static WAKE: RefCell<Option<js_sys::Function>> = const { RefCell::new(None) };
}

/// The RunLoop's and the scheduler's waker: tells the worker to `tick`. An
/// instance has one thread, so this runs on the worker's.
fn wake_host() {
    WAKE.with(|wake| {
        if let Some(wake) = wake.borrow().as_ref() {
            let _ = wake.call0(&JsValue::NULL);
        }
    });
}

#[derive(Default)]
struct ConsoleMapObserver {
    loaded: Cell<bool>,
}

impl MapObserver for ConsoleMapObserver {
    fn on_did_fail_loading_map(&self, error: MapLoadError, what: &str) {
        web_sys::console::error_1(&JsValue::from_str(&format!("map failed to load ({error:?}): {what}")));
    }

    fn on_did_finish_loading_map(&self) {
        self.loaded.set(true);
    }
}

/// What `GLFWRendererFrontend` holds: the renderer and the map's latest
/// update until the next frame draws it.
struct FrontendState {
    backend: Rc<SurfaceBackend>,
    renderer: RefCell<Option<Renderer>>,
    parameters: RefCell<Option<Arc<UpdateParameters>>>,
    dirty: Cell<bool>,
}

#[derive(Clone)]
struct Frontend(Rc<FrontendState>);

impl Frontend {
    /// Draws the latest update if it is new; returns whether it drew.
    fn render(&self) -> bool {
        let state = &self.0;
        if !state.dirty.get() {
            return false;
        }
        let Some(parameters) = state.parameters.borrow().clone() else {
            return false;
        };
        if !state.backend.acquire_next_texture() {
            return false;
        }
        state.dirty.set(false);
        let backend: &dyn RendererBackend = &*state.backend;
        let _scope = BackendScope::new(backend, ScopeType::Implicit);
        if let Some(renderer) = state.renderer.borrow_mut().as_mut() {
            renderer.render(&parameters);
        }
        true
    }
}

impl RendererFrontend for Frontend {
    fn reset(&self) {
        let renderer = self.0.renderer.borrow_mut().take();
        drop(renderer);
    }

    fn set_observer(&self, observer: Weak<dyn RendererObserver>) {
        if let Some(renderer) = self.0.renderer.borrow_mut().as_mut() {
            renderer.set_observer(Some(observer));
        }
    }

    fn update(&self, parameters: Arc<UpdateParameters>) {
        *self.0.parameters.borrow_mut() = Some(parameters);
        self.0.dirty.set(true);
    }
}

fn physical(size: Size, pixel_ratio: f64) -> Size {
    Size::new(
        (f64::from(size.width) * pixel_ratio).round().max(1.0) as u32,
        (f64::from(size.height) * pixel_ratio).round().max(1.0) as u32,
    )
}

/// Hands this instance's fetches the responses its Web Worker fetched ahead
/// (web/map-worker.ts): `take(url)` returns a promise of the response
/// fetched for exactly `url`, once, or undefined (mln_storage's
/// `set_fetch_preloads`).
#[wasm_bindgen]
pub fn set_preloads(take: js_sys::Function) {
    mln_storage::set_fetch_preloads(Some(take));
}

/// A continuous map in an OffscreenCanvas, in the Web Worker that runs it
/// (web/map-worker.ts).
#[wasm_bindgen]
pub struct WebMap {
    // Dropped first: the map resets the renderer through the frontend.
    map: Map,
    frontend: Frontend,
    observer: Rc<ConsoleMapObserver>,
    run_loop: RunLoop,
    scheduler: Arc<CooperativeScheduler>,
    transport: Rc<SerializingTransport>,
    pixel_ratio: f64,
    received: Cell<u32>,
    /// The page's pointer, wheel and key input, turned into camera moves.
    gestures: GestureRecognizer,
}

#[wasm_bindgen]
impl WebMap {
    /// Creates the WebGPU device, a surface on `canvas` (the page's canvas,
    /// transferred to this worker) and the map, at
    /// `width` x `height` CSS pixels. The tiles' workers run in
    /// `workers` Web Workers (at least one), reached through
    /// `post(instance, bytes)`, whose replies go to `receive`. `wake()` is
    /// called whenever the map gets work between frames (a fetch finished, a
    /// reply arrived, a task was queued); the worker then calls `tick` soon.
    /// `load_shaper()` returns a promise of the shaper module
    /// (web/pkg/mln_web_shaper.js), initialized; the map calls it the first
    /// time a style declares font faces. With
    /// `local_ideograph_font_family` (a CSS font-family list), the map
    /// draws CJK glyphs in it rather than downloading them, as GL JS's
    /// `localIdeographFontFamily`.
    pub async fn create(
        canvas: web_sys::OffscreenCanvas,
        width: u32,
        height: u32,
        pixel_ratio: f64,
        workers: u32,
        post: js_sys::Function,
        wake: js_sys::Function,
        load_shaper: js_sys::Function,
        local_ideograph_font_family: Option<String>,
    ) -> Result<WebMap, JsValue> {
        if workers == 0 {
            return Err(js_error("the map needs at least one tile worker"));
        }
        let scheduler = cooperative_scheduler();
        shaper::set_loader(load_shaper);
        font_face_engine::set_font_face_engine(Arc::new(ShaperModuleEngine));

        let size = Size::new(width.max(1), height.max(1));
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::BROWSER_WEBGPU,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let surface = instance.create_surface(wgpu::SurfaceTarget::OffscreenCanvas(canvas)).map_err(js_error)?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                ..Default::default()
            })
            .await
            .map_err(|e| js_error(format!("WebGPU found no adapter: {e}")))?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("MapLibre WebGPU Device"),
                required_limits: wgpu::Limits::default().or_worse_values_from(&adapter.limits()),
                ..Default::default()
            })
            .await
            .map_err(|e| js_error(format!("WebGPU device: {e}")))?;
        let capabilities = surface.get_capabilities(&adapter);
        let format = capabilities.formats.first().copied().unwrap_or(wgpu::TextureFormat::Bgra8Unorm);
        // A WebGPU canvas takes any texture usage, so its frames can be read
        // back (wgpu reports only RENDER_ATTACHMENT for it).
        let usages = capabilities.usages | wgpu::TextureUsages::COPY_SRC;
        let backend = Rc::new(SurfaceBackend::from_host(
            instance,
            device,
            queue,
            surface,
            format,
            // Like GL JS's canvas: the page shows where the map draws
            // nothing. wgpu lists only Opaque for a WebGPU canvas, which
            // takes Premultiplied too.
            wgpu::CompositeAlphaMode::PreMultiplied,
            usages,
            physical(size, pixel_ratio),
        ));

        let run_loop = RunLoop::get();
        WAKE.with(|w| *w.borrow_mut() = Some(wake));
        run_loop.set_waker(Some(Arc::new(wake_host)));
        scheduler.set_waker(Some(Arc::new(wake_host)));
        let renderer_backend: Rc<dyn RendererBackend> = backend.clone();
        let mut renderer = Renderer::new(renderer_backend, pixel_ratio as f32, local_ideograph_font_family);
        let count = workers as usize;
        let transport = SerializingTransport::new(|_| Box::new(WebWorkers { count, post }));
        renderer.set_tile_worker_transport(transport.clone());
        let frontend = Frontend(Rc::new(FrontendState {
            backend,
            renderer: RefCell::new(Some(renderer)),
            parameters: RefCell::new(None),
            dirty: Cell::new(false),
        }));
        let observer = Rc::new(ConsoleMapObserver::default());
        let map = Map::with_resource_options(
            &frontend,
            observer.clone(),
            MapOptions::new().with_size(size).with_pixel_ratio(pixel_ratio as f32),
            &ResourceOptions::default(),
            &ClientOptions::default(),
        )
        .map_err(js_error)?;
        Ok(WebMap {
            map,
            frontend,
            observer,
            run_loop,
            scheduler,
            transport,
            pixel_ratio,
            received: Cell::new(0),
            gestures: GestureRecognizer::default(),
        })
    }

    /// Sets the font faces that every font stack of the styles loaded from
    /// now on falls back to: `json` is an array of font files, each an
    /// object with a `url` and an optional `unicode-range`, as in a font
    /// face of a style's `font-faces`.
    pub fn set_fallback_font_faces(&mut self, json: &str) -> Result<(), JsValue> {
        self.map.get_style_mut().set_fallback_font_faces(json).map_err(|error| JsValue::from_str(&error.to_string()))
    }

    /// Loads a style from a URL.
    pub fn set_style_url(&mut self, url: &str) {
        self.map.get_style_mut().load_url(url);
    }

    /// Moves the camera.
    pub fn jump_to(&mut self, latitude: f64, longitude: f64, zoom: f64, bearing: f64, pitch: f64) {
        let center = LatLng::new(latitude, longitude).ok();
        self.map.jump_to(
            &CameraOptions::default()
                .with_center(center)
                .with_zoom(Some(zoom))
                .with_bearing(Some(bearing))
                .with_pitch(Some(pitch)),
        );
    }

    /// The camera as `[latitude, longitude, zoom, bearing, pitch]`, in
    /// degrees but for the zoom.
    pub fn camera(&self) -> Vec<f64> {
        let camera = self.map.get_camera_options(None);
        let center = camera.center.unwrap_or_default();
        vec![
            center.latitude(),
            center.longitude(),
            camera.zoom.unwrap_or_default(),
            camera.bearing.unwrap_or_default(),
            camera.pitch.unwrap_or_default(),
        ]
    }

    /// Eases the bearing and pitch back to zero, as a step of the page's
    /// input.
    pub fn reset_north(&mut self) {
        let update = CameraUpdate {
            mode: CameraUpdateMode::Ease,
            camera: CameraOptions::default().with_bearing(Some(0.0)).with_pitch(Some(0.0)),
            animation: AnimationOptions::new(Duration::from_millis(500)),
            phase: GesturePhase::None,
        };
        self.gestures.step_to(&mut self.map, update);
    }

    /// Jumps to another map's camera, for a comparison that this map
    /// follows: its own motion stops (an ease, momentum, a wheel's eased
    /// zoom).
    pub fn follow(&mut self, latitude: f64, longitude: f64, zoom: f64, bearing: f64, pitch: f64) {
        self.map.cancel_transitions();
        let update = CameraUpdate {
            mode: CameraUpdateMode::Jump,
            camera: CameraOptions::default()
                .with_center(LatLng::new(latitude, longitude).ok())
                .with_zoom(Some(zoom))
                .with_bearing(Some(bearing))
                .with_pitch(Some(pitch)),
            animation: AnimationOptions::default(),
            phase: GesturePhase::None,
        };
        self.gestures.step_to(&mut self.map, update);
    }

    /// The attributions of the style's sources that have one, as HTML. A
    /// source that links a TileJSON has one once the TileJSON loads.
    pub fn attributions(&self) -> Vec<String> {
        let style = self.map.get_style();
        style.get_sources().iter().filter_map(|source| source.get_attribution()).filter(|a| !a.is_empty()).collect()
    }

    /// The style's `name`, or an empty string.
    pub fn style_name(&self) -> String {
        self.map.get_style().get_name()
    }

    /// Resizes the map and its surface to `width` x `height` CSS pixels.
    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), JsValue> {
        let size = Size::new(width.max(1), height.max(1));
        self.frontend.0.backend.set_framebuffer_size(physical(size, self.pixel_ratio));
        self.map.set_size(size).map_err(js_error)
    }

    /// A reply from the tile worker in Web Worker `instance`, as
    /// `TileWorker::run` returns it: its bytes, then its payloads. The bytes
    /// are copied into wasm memory; the payloads stay JS arrays, which a
    /// bucket's vertices and indices stay in until they're uploaded
    /// (mln_util::host_bytes).
    pub fn receive(&self, instance: u32, reply: js_sys::Array) {
        self.received.set(self.received.get() + 1);
        let mut parts = reply.iter().map(js_sys::Uint8Array::from);
        let bytes = parts.next().map(|bytes| bytes.to_vec()).unwrap_or_default();
        let payloads = parts.map(|part| Payload::Host(HostBytes::new(part))).collect();
        self.transport.receive(instance as usize, Encoded { bytes, payloads });
    }

    /// How many worker replies arrived so far.
    pub fn received(&self) -> u32 {
        self.received.get()
    }

    /// One animation frame at `now` (milliseconds, the clock of the input
    /// methods' `time`): samples the map's animations at this frame, steps
    /// the gestures' eased zoom and ends those that time out, runs the RunLoop's
    /// tasks, draws the map's latest update if it is new, then runs the
    /// cooperative scheduler's tasks (sprites, glyphs, raster tiles) for up
    /// to `budget_ms` and delivers their results. Returns whether it drew.
    pub fn frame(&mut self, budget_ms: f64, now: f64) -> bool {
        // The map samples its animations at this frame's time.
        self.map.set_frame_time(Some(mln_util::chrono::Clock::now()));
        self.gestures.frame(&mut self.map, now);
        self.run_loop.run_once();
        let drew = self.frontend.render();
        self.run_tasks(budget_ms);
        drew
    }

    /// Runs the map's work between frames: the RunLoop's tasks (fetches that
    /// finished, worker replies), then the cooperative scheduler's for up to
    /// `budget_ms`, and the RunLoop again for what they delivered. Returns
    /// whether tasks remain, for another `tick`. Drawing waits for `frame`.
    pub fn tick(&mut self, budget_ms: f64) -> bool {
        self.run_loop.run_once();
        self.run_tasks(budget_ms)
    }

    /// A pointer went down on the canvas, at `x`, `y` CSS pixels from its
    /// top-left corner: `kind` is the DOM `pointerType` ("mouse", "touch" or
    /// "pen"), `button` the DOM `button` (0 primary, 1 middle, 2 secondary)
    /// and `modifiers` the bits of [`modifiers`]. `time` is in milliseconds
    /// on the clock `frame` gets. Returns whether the map used it.
    pub fn pointer_down(
        &mut self,
        id: u32,
        kind: &str,
        button: i32,
        x: f64,
        y: f64,
        modifiers: u32,
        time: f64,
    ) -> bool {
        let kind = match kind {
            "touch" => PointerKind::Touch,
            "pen" => PointerKind::Pen,
            _ => PointerKind::Mouse,
        };
        let button = match button {
            1 => Button::Middle,
            2 => Button::Secondary,
            _ => Button::Primary,
        };
        let input = Input::PointerDown {
            id,
            kind,
            button,
            position: ScreenCoordinate::new(x, y),
            modifiers: modifiers_from_bits(modifiers),
        };
        self.gestures.handle(&mut self.map, input, time)
    }

    /// A pointer moved (see `pointer_down`).
    pub fn pointer_move(&mut self, id: u32, x: f64, y: f64, time: f64) -> bool {
        self.gestures.handle(&mut self.map, Input::PointerMove { id, position: ScreenCoordinate::new(x, y) }, time)
    }

    /// A pointer went up (see `pointer_down`).
    pub fn pointer_up(&mut self, id: u32, x: f64, y: f64, time: f64) -> bool {
        self.gestures.handle(&mut self.map, Input::PointerUp { id, position: ScreenCoordinate::new(x, y) }, time)
    }

    /// The browser took a pointer away (`pointercancel`).
    pub fn pointer_cancel(&mut self, id: u32, time: f64) -> bool {
        self.gestures.handle(&mut self.map, Input::PointerCancel { id }, time)
    }

    /// A DOM `wheel` event, as the page classifies it (web/main.ts): at
    /// (`x`, `y`), `amount` notches of a mouse wheel when `kind` is
    /// [`WHEEL_NOTCHES`], or `amount` pixels of continuous scrolling when it
    /// is [`WHEEL_PIXELS`]; positive scrolls down.
    pub fn wheel(&mut self, x: f64, y: f64, kind: u32, amount: f64, modifiers: u32, time: f64) -> bool {
        debug_assert!(kind == WHEEL_NOTCHES || kind == WHEEL_PIXELS, "wheel kind {kind}");
        let delta = if kind == WHEEL_NOTCHES { WheelDelta::Notches(amount) } else { WheelDelta::Pixels(amount) };
        let input =
            Input::Wheel { position: ScreenCoordinate::new(x, y), delta, modifiers: modifiers_from_bits(modifiers) };
        self.gestures.handle(&mut self.map, input, time)
    }

    /// A DOM `keydown`, by its `key`: the arrows, `+`/`=`, `-`/`_` and
    /// Escape; the map ignores the others and returns false.
    pub fn key(&mut self, key: &str, modifiers: u32, time: f64) -> bool {
        let key = match key {
            "ArrowLeft" => Key::Left,
            "ArrowRight" => Key::Right,
            "ArrowUp" => Key::Up,
            "ArrowDown" => Key::Down,
            "+" | "=" => Key::ZoomIn,
            "-" | "_" => Key::ZoomOut,
            "Escape" => Key::Escape,
            _ => return false,
        };
        self.gestures.handle(&mut self.map, Input::Key { key, modifiers: modifiers_from_bits(modifiers) }, time)
    }

    /// The canvas lost focus: every pointer is released.
    pub fn blur(&mut self, time: f64) -> bool {
        self.gestures.handle(&mut self.map, Input::Blur, time)
    }

    /// The box a Shift-drag zooms to while it is drawn, as `[x0, y0, x1,
    /// y1]` in CSS pixels, for the page to show; otherwise undefined.
    pub fn box_zoom(&self) -> Option<Vec<f64>> {
        self.gestures.box_zoom().map(|[a, b]| vec![a.x, a.y, b.x, b.y])
    }

    /// Whether the style loaded and every tile and resource the map needs
    /// is loaded.
    pub fn is_fully_loaded(&self) -> bool {
        self.observer.loaded.get() && self.map.is_fully_loaded()
    }

    /// Draws the map's latest update again with the next frame and copies
    /// it (see `take_pixels`).
    pub fn request_pixels(&self) {
        self.frontend.0.backend.request_readback();
        self.frontend.0.dirty.set(true);
    }

    /// The frame `request_pixels` asked for, as RGBA rows of the canvas's
    /// physical size, once the GPU has copied it; until then undefined.
    pub fn take_pixels(&self) -> Option<Vec<u8>> {
        self.frontend.0.backend.take_readback().map(|image| image.data.to_vec())
    }
}

/// `WebMap::wheel`'s `kind`, as web/protocol.ts's `WHEEL_PIXELS` and
/// `WHEEL_NOTCHES` send it.
const WHEEL_PIXELS: u32 = 0;
const WHEEL_NOTCHES: u32 = 1;

/// The input methods' `modifiers`: Shift 1, Ctrl 2, Alt 4, Meta 8.
fn modifiers_from_bits(bits: u32) -> Modifiers {
    Modifiers { shift: bits & 1 != 0, ctrl: bits & 2 != 0, alt: bits & 4 != 0, meta: bits & 8 != 0 }
}

impl WebMap {
    /// Runs the cooperative scheduler's tasks for up to `budget_ms`, then the
    /// RunLoop's for what they delivered. Returns whether tasks remain.
    fn run_tasks(&self, budget_ms: f64) -> bool {
        let more = self.scheduler.run_until(Some(deadline(budget_ms)));
        self.run_loop.run_once();
        more
    }
}
