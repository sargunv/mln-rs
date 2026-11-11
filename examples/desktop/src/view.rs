// The render thread: the RunLoop, the map, its renderer, the window's surface
// and the controls over the map (ui.rs).
//
// The thread sleeps in the RunLoop until it has work: a message from the
// window (host.rs), a task (a tile or style response) or a timer. A frame
// waits for the surface's next texture first, then takes the messages that
// came meanwhile, so that it draws the latest input. Frames come at most once
// per refresh of the display the window opened on, or as fast as they can in
// benchmark mode.
//
// Mouse, touch, wheel, trackpad and the arrow, `+` and `-` keys go to a
// `GestureRecognizer` (mln_gestures), which moves the camera and continues
// releases with the map's momentum.
//
// The controls lay out in egui on this thread, in the frames that have their
// input, a repaint they asked for, or a new frame of the map. egui paints
// them over the map's frame before it is presented
// (`SurfaceBackend::set_overlay`); when only the controls change, the map
// draws its last frame again under them.
//
// For checking frames without screenshots: `--readback <file.png>` writes the
// presented frame once the map is idle (or at exit), `--quit-after <seconds>`
// closes the window; the style report prints once which layers and sources
// the renderer skips.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, mpsc};
use std::time::Instant;

use mln_actor::{RunLoop, Timer};
use mln_gestures::{Button, GestureRecognizer, Input, Key, Modifiers, PinchPhase, PointerKind, WheelDelta};
use mln_gfx::{BackendScope, RendererBackend, ScopeType};
use mln_map::{
    AnimationOptions, BoundOptions, CameraOptions, CameraUpdate, CameraUpdateMode, GesturePhase, Map, MapDebugOptions,
    MapObserver, MapOptions,
};
use mln_renderer::Renderer;
use mln_storage::{DatabaseFileSource, FileSource, FileSourceManager, FileSourceType, ResourceOptions};
use mln_util::chrono::{Clock, Duration};
use mln_util::geo::{LatLng, ScreenCoordinate};
use mln_util::logging::{Event, Log};
use mln_util::size::Size;
use mln_util::tile_server_options::TileServerOptions;
use mln_webgpu::SurfaceBackend;
use mln_webgpu::surface_backend::OverlayTarget;
use winit::dpi::{LogicalSize, PhysicalSize};
use winit::event_loop::EventLoopProxy;
use winit::keyboard::KeyCode;

use crate::frontend::Frontend;
use crate::host::{Inbox, Message, UserEvent};
use crate::settings::Settings;
use crate::styles::{self, Style};
use crate::ui::{Action, Controls, Shown};
use crate::{Args, system_fonts};

/// What the render thread starts with.
pub(crate) struct Launch {
    pub args: Args,
    pub settings: Settings,
    /// The controls' egui context, which the window's thread feeds.
    pub ui: egui::Context,
    /// The window's scale factor when it opened: the map's pixel ratio.
    pub pixel_ratio: f64,
    /// The window's size in physical pixels.
    pub size: PhysicalSize<u32>,
    /// The least time between frames, outside benchmark mode.
    pub frame_interval: std::time::Duration,
    /// The epoch of the gesture recognizer's clock.
    pub start: Instant,
    pub inbox: Arc<Inbox>,
    pub proxy: EventLoopProxy<UserEvent>,
}

/// What the frontend and the map observer reach while the view is busy.
pub struct ViewShared {
    dirty: Cell<bool>,
    style_loaded: Cell<bool>,
    idle: Cell<bool>,
}

impl ViewShared {
    /// `invalidate()`: the map has a new frame to draw.
    pub fn invalidate(&self) {
        self.dirty.set(true);
    }
}

/// The view's `MapObserver`.
struct Observer {
    shared: Rc<ViewShared>,
}

impl MapObserver for Observer {
    fn on_did_finish_loading_style(&self) {
        self.shared.style_loaded.set(true);
    }

    fn on_did_become_idle(&self) {
        self.shared.idle.set(true);
    }
}

/// Where the app opens without `--style` or MLN_STYLE_URL.
const DEFAULT_STYLE: &str = "https://tiles.openfreemap.org/styles/liberty";

/// The controls' last frame, which the overlay paints over each frame of the
/// map until the next.
#[derive(Default)]
struct Paint {
    primitives: Vec<egui::ClippedPrimitive>,
    /// The textures to update before painting, and to free after.
    textures: egui::TexturesDelta,
    pixels_per_point: f32,
}

impl Drop for Paint {
    fn drop(&mut self) {
        // The window closes with these unpainted, which is fine.
        self.textures.clear();
    }
}

/// The overlay that paints the controls: egui's renderer on the map's device,
/// made with the first frame.
fn controls_overlay(paint: Rc<RefCell<Paint>>) -> mln_webgpu::surface_backend::Overlay {
    let mut renderer: Option<egui_wgpu::Renderer> = None;
    Box::new(move |target: &OverlayTarget<'_>| {
        let renderer = renderer.get_or_insert_with(|| {
            egui_wgpu::Renderer::new(target.device, target.format, egui_wgpu::RendererOptions::default())
        });
        let mut paint = paint.borrow_mut();
        let mut textures = std::mem::take(&mut paint.textures);
        for (id, deltas) in &textures.set {
            for delta in deltas {
                renderer.update_texture(target.device, target.queue, *id, delta);
            }
        }
        if !paint.primitives.is_empty() {
            let screen = egui_wgpu::ScreenDescriptor {
                size_in_pixels: [target.size.width, target.size.height],
                pixels_per_point: paint.pixels_per_point,
            };
            let mut encoder =
                target.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("controls") });
            let mut commands =
                renderer.update_buffers(target.device, target.queue, &mut encoder, &paint.primitives, &screen);
            {
                let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("controls"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: target.view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                    })],
                    ..Default::default()
                });
                renderer.render(&mut pass.forget_lifetime(), &paint.primitives, &screen);
            }
            commands.push(encoder.finish());
            target.queue.submit(commands);
        }
        for id in &textures.free {
            renderer.free_texture(id);
        }
        textures.clear();
    })
}

/// Opens the map in the window and runs it until the window closes.
pub(crate) fn run(launch: Launch) {
    let Launch { args, settings, ui, pixel_ratio, size, frame_interval, start, inbox, proxy } = launch;
    let run_loop = RunLoop::new();
    inbox.set_waker(run_loop.handle());

    // Finds the system fonts while the surface and the map get ready.
    let font_discovery = (!args.no_system_fonts)
        .then(|| mln_util::thread::builder("font discovery").spawn(system_fonts::fallback_font_faces).ok())
        .flatten();

    let framebuffer_size = Size::new(size.width, size.height);
    let surface_proxy = proxy.clone();
    let make_surface = move |instance: &wgpu::Instance| {
        let closed = || mln_util::Error::cancelled("The window closed before it had a surface");
        let (reply, surface) = mpsc::channel();
        surface_proxy.send_event(UserEvent::MakeSurface(instance.clone(), reply)).map_err(|_| closed())?;
        surface.recv().map_err(|_| closed())?.map_err(mln_util::Error::native)
    };
    // Vsync unless benchmarking.
    let backend = match SurfaceBackend::new(make_surface, framebuffer_size, !args.benchmark) {
        Ok(backend) => Rc::new(backend),
        Err(e) => {
            Log::error(Event::GraphicsBackend, e.message);
            return;
        }
    };
    let info = backend.get_adapter_info();
    let adapter = format!("{} ({:?})", info.name, info.backend);
    Log::info(
        Event::Render,
        format!(
            "Rendering with {} ({:?}, driver {} {}), surface format {:?}",
            info.name,
            info.backend,
            info.driver,
            info.driver_info,
            backend.get_swap_chain_format()
        ),
    );
    let paint = Rc::new(RefCell::new(Paint::default()));
    backend.set_overlay(Some(controls_overlay(paint.clone())));

    if args.benchmark {
        Log::info(Event::General, "BENCHMARK MODE: Some optimizations are disabled.");
    }

    // Set access token if present
    let apikey = args.apikey.clone().or_else(|| std::env::var("MLN_API_KEY").ok()).unwrap_or_default();
    let cache_db = args.cache.clone().unwrap_or_else(|| "/tmp/mbgl-cache.db".to_owned());
    let resource_options = ResourceOptions::default()
        .with_cache_path(cache_db)
        .with_api_key(apikey)
        .with_tile_server_options(TileServerOptions::map_tiler_configuration());

    let file_sources = FileSourceManager::new();
    let database_file_source = file_sources.get_file_source(FileSourceType::Database, &resource_options);
    if !settings.online {
        match file_sources.get_file_source(FileSourceType::Network, &resource_options) {
            Some(online) => {
                online.set_property("online-status", &feature::Value::Bool(false));
                Log::warning(Event::Setup, "Application is offline.");
            }
            None => Log::warning(
                Event::Setup,
                "Network resource provider is not available, only local requests are supported.",
            ),
        }
    }

    let shared =
        Rc::new(ViewShared { dirty: Cell::new(false), style_loaded: Cell::new(false), idle: Cell::new(false) });
    let renderer_backend: Rc<dyn RendererBackend> = backend.clone();
    let renderer = Renderer::new(renderer_backend.clone(), pixel_ratio as f32, None);
    let frontend = Frontend::new(renderer, renderer_backend, shared.clone());
    let map_size = logical_size(size, pixel_ratio);

    let mut map = match Map::with_resource_options(
        &frontend,
        Rc::new(Observer { shared: shared.clone() }),
        MapOptions::new().with_size(map_size).with_pixel_ratio(pixel_ratio as f32),
        &file_sources,
        &resource_options,
    ) {
        Ok(map) => map,
        Err(e) => {
            Log::error(Event::Setup, e.message);
            return;
        }
    };

    if let Err(e) = map.set_bounds(&BoundOptions::default().with_max_pitch(settings.max_pitch)) {
        Log::warning(Event::Setup, e.message);
    }
    map.jump_to(
        &CameraOptions::default()
            .with_center(LatLng::new(settings.latitude, settings.longitude).ok())
            .with_center_altitude(Some(settings.altitude))
            .with_zoom(Some(settings.zoom))
            .with_bearing(Some(settings.bearing))
            .with_pitch(Some(settings.pitch))
            .with_roll(Some(settings.roll))
            .with_fov(Some(settings.fov)),
    );
    map.set_debug(MapDebugOptions(settings.debug));

    let style_url = match &args.style {
        Some(style) if !style.contains("://") => format!("file://{style}"),
        Some(style) => style.clone(),
        None => std::env::var("MLN_STYLE_URL").unwrap_or_else(|_| DEFAULT_STYLE.to_owned()),
    };

    let mut view = View {
        run_loop,
        wake: Timer::new(),
        inbox,
        proxy,
        start,
        frame_interval,
        next_frame_at: Instant::now(),
        backend,
        frontend,
        map,
        shared,
        gestures: GestureRecognizer::default(),
        gestures_animating: false,
        ui,
        controls: Controls::default(),
        ui_input: None,
        ui_window: egui::RawInput::default(),
        ui_repaint_at: None,
        ui_shapes: Vec::new(),
        paint,
        database_file_source,
        styles: styles::styles(),
        style: styles::style_for(&style_url),
        adapter,
        frame_report: String::new(),
        benchmark: args.benchmark,
        pixel_ratio,
        size: map_size,
        quit: false,
        frames: 0,
        frame_time: 0.0,
        last_reported: Instant::now(),
        gesture_test: args.gesture_test.then_some(GestureTest { step: 0, next_at: None }),
        readback_path: args.readback.as_ref().map(PathBuf::from),
        readback_requested: false,
        quit_at: args.quit_after.map(|s| Instant::now() + std::time::Duration::from_secs_f64(s)),
        style_reported: false,
    };

    if let Some(font_discovery) = font_discovery {
        let faces = font_discovery.join().unwrap_or_default();
        if faces.is_empty() {
            Log::info(Event::Setup, "No system fonts found for complex scripts or CJK");
        } else {
            let families: Vec<&str> = faces.iter().map(|face| face.family.as_str()).collect();
            Log::info(Event::Setup, format!("Falling back to system fonts: {}", families.join(", ")));
        }
        view.map.get_style_mut().set_fallback_font_face_list(system_fonts::into_font_faces(faces));
    }
    let style = view.style.clone();
    view.load_style(style);

    view.run();

    let camera = view.map.get_camera_options(None);
    let center = camera.center.unwrap_or_default();
    Log::info(
        Event::General,
        format!(
            "Exit location: --lat={:.6} --lon={:.6} --zoom={:.6} --bearing={:.6} --pitch={:.6}",
            center.latitude(),
            center.longitude(),
            camera.zoom.unwrap_or_default(),
            camera.bearing.unwrap_or_default(),
            camera.pitch.unwrap_or_default(),
        ),
    );
    // Drop order: the map resets the renderer, then the frontend, then the
    // backend's surface, while the window is still open.
    let View { map, frontend, backend, .. } = view;
    drop(map);
    drop(frontend);
    drop(backend);
}

/// A physical size in the map's logical pixels.
fn logical_size(size: PhysicalSize<u32>, pixel_ratio: f64) -> Size {
    let size: LogicalSize<u32> = size.to_logical(pixel_ratio);
    Size::new(size.width, size.height)
}

/// Logs a camera change's error (a NaN pan from the input).
fn log_error(result: Result<(), mln_util::Error>) {
    if let Err(e) = result {
        Log::error(Event::General, e.message);
    }
}

struct View {
    run_loop: RunLoop,
    /// Wakes the RunLoop when the next frame or a deadline is due.
    wake: Timer,
    inbox: Arc<Inbox>,
    proxy: EventLoopProxy<UserEvent>,
    start: Instant,
    frame_interval: std::time::Duration,
    next_frame_at: Instant,

    backend: Rc<SurfaceBackend>,
    frontend: Frontend,
    map: Map,
    shared: Rc<ViewShared>,
    gestures: GestureRecognizer,
    /// The recognizer eases a scroll, and wants frames.
    gestures_animating: bool,

    ui: egui::Context,
    controls: Controls,
    /// The controls' input since their last frame.
    ui_input: Option<egui::RawInput>,
    /// The window's state as the controls' last input had it, without its
    /// events: a frame without input starts from it, since a default
    /// `RawInput` has no viewport info, and egui would lay the controls out
    /// at one pixel per point, and unfocused.
    ui_window: egui::RawInput,
    /// When the controls asked for their next frame (a tooltip's delay).
    ui_repaint_at: Option<Instant>,
    /// The controls' last frame, to tell whether a frame changed them.
    ui_shapes: Vec<egui::epaint::ClippedShape>,
    paint: Rc<RefCell<Paint>>,
    database_file_source: Option<Arc<dyn FileSource>>,
    styles: Vec<Style>,
    style: Style,
    adapter: String,
    frame_report: String,

    benchmark: bool,
    pixel_ratio: f64,
    /// The map's size, in logical pixels.
    size: Size,
    quit: bool,

    // Frame timer
    frames: i32,
    frame_time: f32,
    last_reported: Instant,

    gesture_test: Option<GestureTest>,
    readback_path: Option<PathBuf>,
    readback_requested: bool,
    quit_at: Option<Instant>,
    style_reported: bool,
}

impl View {
    /// Milliseconds on the gesture recognizer's clock.
    fn ms(&self, at: Instant) -> f64 {
        at.saturating_duration_since(self.start).as_secs_f64() * 1000.0
    }

    /// Loads `style` and names the window after it.
    fn load_style(&mut self, style: Style) {
        self.map.get_style_mut().load_url(&style.url);
        let name = if style.name.is_empty() { style.url.clone() } else { style.name.clone() };
        let _ = self.proxy.send_event(UserEvent::SetTitle(format!("mln-rs: {name}")));
        self.style = style;
        self.style_reported = false;
        self.shared.style_loaded.set(false);
    }

    /// Frames and work until the window closes. A readback that never
    /// happened (the map never became idle) is taken from one last frame.
    fn run(&mut self) {
        while !self.quit {
            self.run_loop.run_once();
            if self.quit {
                break;
            }
            let now = Instant::now();
            if !self.frame_due(now) {
                self.wait_until(self.next_deadline());
                continue;
            }
            if !self.benchmark && now < self.next_frame_at {
                self.wait_until(Some(self.next_frame_at));
                continue;
            }
            self.next_frame_at = now + self.frame_interval;
            self.frame();
        }
        if self.readback_path.is_some() {
            self.backend.request_readback();
            self.shared.dirty.set(true);
            self.render();
            self.save_readback();
        }
    }

    /// Whether a frame has something to do.
    fn frame_due(&self, now: Instant) -> bool {
        self.shared.dirty.get()
            || self.gestures_animating
            || self.benchmark
            || !self.inbox.is_empty()
            || self.ui_input.is_some()
            || self.next_deadline().is_some_and(|at| at <= now)
            || (self.readback_path.is_some() && (self.readback_requested || self.shared.idle.get()))
    }

    /// When a frame is due without input: the controls' repaint, the gesture
    /// test's next step, or the quit timer.
    fn next_deadline(&self) -> Option<Instant> {
        let test = self.gesture_test.as_ref().and_then(|t| t.next_at.or(self.style_reported.then_some(self.start)));
        [self.ui_repaint_at, test, self.quit_at].into_iter().flatten().min()
    }

    /// Sleeps in the RunLoop until it has work, or until `deadline`.
    fn wait_until(&self, deadline: Option<Instant>) {
        match deadline {
            Some(at) => {
                let nanos = at.saturating_duration_since(Instant::now()).as_nanos();
                self.wake.start(
                    Duration::from_nanos(i64::try_from(nanos).unwrap_or(i64::MAX)),
                    Duration::zero(),
                    || {},
                );
            }
            None => self.wake.stop(),
        }
        self.run_loop.wait_for_work();
    }

    /// One frame: the surface's next texture, the messages that came while
    /// waiting for it, the recognizer's eases, the controls, and the map's
    /// frame.
    fn frame(&mut self) {
        // Wait for the display first, so that the frame takes the input that
        // came meanwhile.
        let drawable = (self.shared.dirty.get() || !self.inbox.is_empty()) && self.backend.acquire_next_texture();

        // The map samples its animations at this frame's time.
        self.map.set_frame_time(Some(Clock::now()));
        for message in self.inbox.take() {
            self.handle(message);
        }
        let now = Instant::now();
        let ms = self.ms(now);
        self.gestures_animating = self.gestures.frame(&mut self.map, ms);

        // A perf probe: `MLN_PAN_PX=<px>` pans the map by that many pixels
        // every frame, reversing every 2 s (so it stays over the same area),
        // so the "Frame time" log measures continuous camera movement.
        if let Some(dx) = std::env::var("MLN_PAN_PX").ok().and_then(|v| v.parse::<f64>().ok()) {
            let dx = if (ms / 2000.0) as i64 % 2 == 0 { dx } else { -dx };
            log_error(self.map.move_by(&ScreenCoordinate::new(dx, 0.0), &AnimationOptions::default()));
            self.shared.invalidate();
        }

        if self.ui_input.is_some() || self.shared.dirty.get() || self.ui_repaint_at.is_some_and(|at| at <= now) {
            self.controls_frame(now);
        }

        // No texture right now (e.g. a timeout while the compositor is busy,
        // or a hidden window): try again on the next frame.
        if drawable || (self.shared.dirty.get() && self.backend.acquire_next_texture()) {
            self.render();
        }
        self.after_frame();
    }

    /// Applies one message from the window.
    fn handle(&mut self, message: Message) {
        match message {
            Message::Input { input, at } => self.gesture(input, at),
            Message::Key { code, pressed, modifiers, at } => self.on_key(code, pressed, modifiers, at),
            Message::Ui(input) => match &mut self.ui_input {
                Some(pending) => pending.append(input),
                None => self.ui_input = Some(input),
            },
            Message::Resized(size) => self.on_resize(size),
            Message::Focused { focused, at } => {
                if !focused {
                    // A drag the window can't see the end of ends here.
                    self.gesture(Input::Blur, at);
                    self.frontend.with_renderer(|renderer| renderer.reduce_memory_use());
                }
            }
            Message::Close => self.quit = true,
        }
    }

    /// Feeds one input to the gesture recognizer.
    fn gesture(&mut self, input: Input, at: Instant) {
        let at = self.ms(at);
        self.gestures.handle(&mut self.map, input, at);
        // The app has no use for clicks.
        self.gestures.take_events();
    }

    fn on_resize(&mut self, size: PhysicalSize<u32>) {
        self.backend.set_framebuffer_size(Size::new(size.width, size.height));
        self.size = logical_size(size, self.pixel_ratio);
        // An empty size (a minimized window) fails.
        if let Err(e) = self.map.set_size(self.size) {
            Log::warning(Event::General, e.message);
        }
        self.shared.invalidate();
    }

    /// The keys the map takes, which the controls didn't: the arrows pan
    /// (Shift: rotate and tilt), `+` and `-` zoom, each repeating while held,
    /// and Escape ends a box zoom.
    fn on_key(&mut self, code: KeyCode, pressed: bool, modifiers: Modifiers, at: Instant) {
        let key = match code {
            KeyCode::ArrowLeft => Key::Left,
            KeyCode::ArrowRight => Key::Right,
            KeyCode::ArrowUp => Key::Up,
            KeyCode::ArrowDown => Key::Down,
            KeyCode::Equal | KeyCode::NumpadAdd => Key::ZoomIn,
            KeyCode::Minus | KeyCode::NumpadSubtract => Key::ZoomOut,
            KeyCode::Escape => Key::Escape,
            _ => return,
        };
        if pressed {
            self.gesture(Input::Key { key, modifiers }, at);
        }
    }

    /// The controls' frame: lays them out with their input, carries out what
    /// they ask, and draws the map again under them when they changed.
    fn controls_frame(&mut self, now: Instant) {
        let mut input = self.ui_window.clone();
        if let Some(newer) = self.ui_input.take() {
            input.append(newer);
            self.ui_window = input.clone();
            self.ui_window.events.clear();
            self.ui_window.hovered_files.clear();
            self.ui_window.dropped_files.clear();
            for viewport in self.ui_window.viewports.values_mut() {
                viewport.events.clear();
            }
        }
        input.time = Some(now.saturating_duration_since(self.start).as_secs_f64());

        let camera = self.map.get_camera_options(None);
        let style = self.map.get_style();
        let attributions: Vec<String> = style
            .get_sources()
            .iter()
            .filter_map(|source| source.get_attribution())
            .filter(|attribution| !attribution.is_empty())
            .collect();
        let style_name = style.get_name();
        drop(style);
        let status = [self.adapter.clone(), self.frame_report.clone()];
        let shown = Shown {
            styles: &self.styles,
            style: &self.style,
            style_name: &style_name,
            loading: !self.map.is_fully_loaded(),
            bearing: camera.bearing.unwrap_or_default(),
            pitch: camera.pitch.unwrap_or_default(),
            attributions: &attributions,
            debug: self.map.get_debug(),
            status: &status,
        };
        let mut actions = Vec::new();
        let controls = &mut self.controls;
        let output = self.ui.run_pass(input, |ctx| actions = controls.show(ctx, &shown));

        let _ = self.proxy.send_event(UserEvent::Ui(Box::new(output.platform_output)));
        self.ui_repaint_at = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|viewport| viewport.repaint_delay)
            .filter(|delay| *delay < std::time::Duration::from_secs(3600))
            .map(|delay| now + delay);
        if output.shapes != self.ui_shapes || !output.textures_delta.is_empty() {
            let primitives = self.ui.tessellate(output.shapes.clone(), output.pixels_per_point);
            let mut paint = self.paint.borrow_mut();
            paint.primitives = primitives;
            paint.pixels_per_point = output.pixels_per_point;
            paint.textures.append(output.textures_delta);
            self.ui_shapes = output.shapes;
            self.shared.invalidate();
        }
        for action in actions {
            self.apply(action, now);
        }
    }

    /// Carries out what the controls ask.
    fn apply(&mut self, action: Action, now: Instant) {
        match action {
            Action::ZoomIn => self.gesture(Input::Key { key: Key::ZoomIn, modifiers: Modifiers::default() }, now),
            Action::ZoomOut => self.gesture(Input::Key { key: Key::ZoomOut, modifiers: Modifiers::default() }, now),
            Action::ResetNorth => {
                let north = CameraUpdate {
                    mode: CameraUpdateMode::Ease,
                    camera: CameraOptions::default().with_bearing(Some(0.0)).with_pitch(Some(0.0)),
                    animation: AnimationOptions::new(Duration::from_millis(500)),
                    phase: GesturePhase::None,
                };
                self.gestures.step_to(&mut self.map, north);
            }
            Action::PickStyle(index) => {
                if let Some(style) = self.styles.get(index).cloned() {
                    self.load_style(style);
                }
            }
            Action::SetDebug(debug) => self.map.set_debug(debug),
            Action::ResetCache => self.reset_cache(),
        }
    }

    /// Empties the cache database.
    fn reset_cache(&self) {
        let database = self.database_file_source.as_deref().and_then(|source| source.as_any());
        let Some(database) = database.and_then(|source| source.downcast_ref::<DatabaseFileSource>()) else {
            Log::warning(Event::Database, "There is no cache database to reset");
            return;
        };
        database.reset_database(|result| match result {
            Ok(()) => Log::info(Event::Database, "Reset the cache"),
            Err(e) => Log::error(Event::Database, format!("Failed to reset cache: {e}")),
        });
    }

    /// `render()`
    fn render(&mut self) {
        if !self.shared.dirty.get() {
            return;
        }
        self.shared.dirty.set(false);
        let started = Instant::now();
        {
            let backend: Rc<dyn RendererBackend> = self.backend.clone();
            let _scope = BackendScope::new(&*backend, ScopeType::Explicit);
            self.frontend.render();
        }
        self.report((1000.0 * started.elapsed().as_secs_f64()) as f32);
        if self.benchmark {
            self.shared.invalidate();
        }
    }

    /// `report(float duration)`: the frame time, logged and shown in the
    /// controls' panel once a second.
    fn report(&mut self, duration: f32) {
        self.frames += 1;
        self.frame_time += duration;
        if self.last_reported.elapsed().as_secs_f64() >= 1.0 {
            self.frame_time /= self.frames as f32;
            self.frame_report = format!("Frame time: {:.2} ms ({:.0} fps)", self.frame_time, 1000.0 / self.frame_time);
            Log::info(Event::Render, &self.frame_report);
            self.frames = 0;
            self.frame_time = 0.0;
            self.last_reported = Instant::now();
        }
    }

    /// The checks after each frame: the style report, the gesture test, the
    /// readback and the quit timer.
    fn after_frame(&mut self) {
        if self.style_reported {
            self.run_gesture_test();
        }

        if self.shared.style_loaded.get() && !self.style_reported {
            self.style_reported = true;
            report_style(&self.map);
        }

        if self.readback_path.is_some() {
            if self.readback_requested {
                self.save_readback();
            } else if self.shared.idle.get() && self.style_reported {
                self.backend.request_readback();
                self.readback_requested = true;
                self.shared.invalidate();
            }
        }

        if let Some(quit_at) = self.quit_at
            && Instant::now() >= quit_at
        {
            Log::info(Event::General, "--quit-after: closing the window");
            self.quit = true;
        }
    }

    fn save_readback(&mut self) {
        let Some(image) = self.backend.take_readback() else {
            return;
        };
        let Some(path) = self.readback_path.take() else {
            return;
        };
        let png = mln_image::encode_png(&image);
        match std::fs::write(&path, png) {
            Ok(()) => Log::info(
                Event::General,
                format!(
                    "Read back the presented frame ({}x{}) to {}",
                    image.size.width,
                    image.size.height,
                    path.display()
                ),
            ),
            Err(e) => Log::error(Event::General, format!("Failed to write {}: {e}", path.display())),
        }
    }
}

/// Prints once which layers the renderer draws and which it
/// skips (unported layer types, or sources of unported types).
pub fn report_style(map: &Map) {
    let style = map.get_style();
    let json: serde_json::Value = serde_json::from_str(&style.get_json()).unwrap_or_default();
    let declared_sources: BTreeMap<String, String> = json
        .get("sources")
        .and_then(|s| s.as_object())
        .map(|sources| {
            sources
                .iter()
                .map(|(id, s)| (id.clone(), s.get("type").and_then(|t| t.as_str()).unwrap_or("?").to_owned()))
                .collect()
        })
        .unwrap_or_default();
    let loaded_sources: Vec<String> = style.get_sources().iter().map(|s| s.get_id()).collect();

    let mut drawn: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut skipped_type: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut skipped_source: BTreeMap<String, usize> = BTreeMap::new();
    let impls = style.impl_.get_layer_impls();
    for layer in impls.iter() {
        let type_ = layer.get_type_info().type_;
        // Ask the renderer's layer manager itself, so this summary can't go
        // stale as layer types get ported.
        if mln_renderer::layermanager::create_render_layer(layer).is_none() {
            *skipped_type.entry(type_).or_default() += 1;
        } else if !layer.source.is_empty() && !loaded_sources.contains(&layer.source) {
            let source_type = declared_sources.get(&layer.source).map_or("missing", String::as_str);
            *skipped_source.entry(format!("{type_} on {source_type} source '{}'", layer.source)).or_default() += 1;
        } else {
            *drawn.entry(type_).or_default() += 1;
        }
    }
    let unported_sources: Vec<String> = declared_sources
        .iter()
        .filter(|(id, _)| !loaded_sources.contains(id))
        .map(|(id, t)| format!("{id} ({t})"))
        .collect();
    let list = |m: Vec<(String, usize)>| m.iter().map(|(k, v)| format!("{k} x{v}")).collect::<Vec<_>>().join(", ");
    Log::info(
        Event::General,
        format!(
            "Style \"{}\": {} layers. Rendered: {}. Skipped, layer type not ported: {}. Skipped, source not ported: {}. Unported sources: {}.",
            style.get_name(),
            impls.len(),
            list(drawn.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()),
            list(skipped_type.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()),
            list(skipped_source.into_iter().collect()),
            if unported_sources.is_empty() { "none".to_owned() } else { unported_sources.join(", ") },
        ),
    );
}

/// Scripted input (`--gesture-test`).
struct GestureTest {
    step: usize,
    next_at: Option<Instant>,
}

impl View {
    /// Once the style is loaded, feeds a scripted sequence of messages (drag,
    /// scroll, double click, rotate, pitch, trackpad pinch, keys) through the
    /// same handlers as the window's, and the controls' actions, logs the
    /// camera after each, then closes.
    fn run_gesture_test(&mut self) {
        let Some(test) = self.gesture_test.as_mut() else {
            return;
        };
        let now = Instant::now();
        if test.next_at.is_some_and(|t| now < t) {
            return;
        }
        test.next_at = Some(now + std::time::Duration::from_millis(1500));
        let step = test.step;
        test.step += 1;

        let center = ScreenCoordinate::new(f64::from(self.size.width) / 2.0, f64::from(self.size.height) / 2.0);
        let (cx, cy) = (center.x, center.y);
        let none = Modifiers::default();
        let ctrl = Modifiers { ctrl: true, ..none };
        let shift = Modifiers { shift: true, ..none };
        let at = |x: f64, y: f64| ScreenCoordinate::new(x, y);
        let down = |button: Button, modifiers: Modifiers| Input::PointerDown {
            id: 0,
            kind: PointerKind::Mouse,
            button,
            position: center,
            modifiers,
        };
        let drag = |button: Button, modifiers: Modifiers, dx: f64, dy: f64| {
            let mut inputs = vec![Input::PointerMove { id: 0, position: center }, down(button, modifiers)];
            for i in 1..=10 {
                let t = f64::from(i) / 10.0;
                inputs.push(Input::PointerMove { id: 0, position: at(cx + dx * t, cy + dy * t) });
            }
            inputs.push(Input::PointerUp { id: 0, position: at(cx + dx, cy + dy) });
            inputs
        };
        let wheel = |delta: WheelDelta| Input::Wheel { position: center, delta, modifiers: none };
        let pinch = |scale: f64, rotation: f64, phase: PinchPhase| Input::Pinch { center, scale, rotation, phase };
        let (name, inputs, keys, actions): (&str, Vec<Input>, Vec<KeyCode>, Vec<Action>) = match step {
            0 => ("start", vec![], vec![], vec![]),
            1 => ("drag (pan, then glide) by (200, 100)", drag(Button::Primary, none, 200.0, 100.0), vec![], vec![]),
            2 => ("scroll up 5 wheel notches (eased)", vec![wheel(WheelDelta::Notches(-1.0)); 5], vec![], vec![]),
            3 => ("scroll down 2 wheel notches (eased)", vec![wheel(WheelDelta::Notches(1.0)); 2], vec![], vec![]),
            4 => ("scroll a trackpad up 240 px", vec![wheel(WheelDelta::Pixels(-30.0)); 8], vec![], vec![]),
            5 => (
                "double click (zoom in, animated)",
                vec![
                    Input::PointerMove { id: 0, position: center },
                    down(Button::Primary, none),
                    Input::PointerUp { id: 0, position: center },
                    down(Button::Primary, none),
                    Input::PointerUp { id: 0, position: center },
                ],
                vec![],
                vec![],
            ),
            6 => ("ctrl+drag (rotate)", drag(Button::Primary, ctrl, 150.0, 0.0), vec![], vec![]),
            7 => ("right drag up (tilt)", drag(Button::Secondary, none, 0.0, -120.0), vec![], vec![]),
            8 => ("shift+drag (box zoom)", drag(Button::Primary, shift, 200.0, 150.0), vec![], vec![]),
            9 => (
                "trackpad pinch (zoom out half a level, turn 20 degrees counterclockwise, then glide)",
                std::iter::once(pinch(1.0, 0.0, PinchPhase::Begin))
                    .chain(std::iter::repeat_n(pinch(2f64.powf(-0.05), -2.0, PinchPhase::Update), 10))
                    .chain(std::iter::once(pinch(1.0, 0.0, PinchPhase::End)))
                    .collect(),
                vec![],
                vec![],
            ),
            10 => ("key Right (pan, animated)", vec![], vec![KeyCode::ArrowRight], vec![]),
            11 => ("compass (reset north and tilt, animated)", vec![], vec![], vec![Action::ResetNorth]),
            12 => ("key - (zoom out, animated)", vec![], vec![KeyCode::Minus], vec![]),
            13 => ("zoom in button (animated)", vec![], vec![], vec![Action::ZoomIn]),
            14 => (
                "debug overlays: tile borders and parse status",
                vec![],
                vec![],
                vec![Action::SetDebug(MapDebugOptions::TILE_BORDERS | MapDebugOptions::PARSE_STATUS)],
            ),
            15 => {
                let _ = self.proxy.send_event(UserEvent::RequestSize(LogicalSize::new(800.0, 600.0)));
                ("window resize to 800x600", vec![], vec![], vec![])
            }
            _ => {
                Log::info(Event::General, "Gesture test: done");
                self.gesture_test = None;
                self.quit = true;
                return;
            }
        };
        // Each step's inputs, 16 ms apart (the recognizer measures release
        // velocities and double clicks with their times).
        let mut time = now;
        for input in inputs {
            self.handle(Message::Input { input, at: time });
            time += std::time::Duration::from_millis(16);
        }
        for code in keys {
            self.handle(Message::Key { code, pressed: true, modifiers: none, at: time });
        }
        for action in actions {
            self.apply(action, time);
        }
        let camera = self.map.get_camera_options(None);
        let center = camera.center.unwrap_or_default();
        Log::info(
            Event::General,
            format!(
                "Gesture test step {step} ({name}): center {:.5},{:.5} zoom {:.3} bearing {:.1} pitch {:.1}",
                center.latitude(),
                center.longitude(),
                camera.zoom.unwrap_or(f64::NAN),
                camera.bearing.unwrap_or(f64::NAN),
                camera.pitch.unwrap_or(f64::NAN),
            ),
        );
    }
}
