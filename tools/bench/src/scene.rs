//! A map and its headless frontend over the benchmarks' data (README.md):
//! the shared render suite's real-world styles, read from its cache
//! database, and maplibre-native-ffi's basemap, read from the files
//! `fixtures.py` prepared. Nothing uses the network.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use feature::{Value, ValueObject};
use mln_actor::RunLoop;
use mln_actor::timer::Timer;
use mln_gfx::{ContextMode, SwapBehaviour};
use mln_headless::HeadlessFrontend;
use mln_map::{CameraOptions, Map, MapLoadError, MapMode, MapObserver, MapOptions, RenderFrameStatus, RenderMode};
use mln_renderer::query::SourceQueryOptions;
use mln_storage::{FileSource, FileSourceManager, FileSourceType, ResourceOptions};
use mln_style_types::VisibilityType;
use mln_text::GlyphCache;
use mln_util::feature::feature_id_to_string;
use mln_util::geo::LatLng;
use mln_util::image::PremultipliedImage;
use mln_util::size::Size;

/// A display's refresh period at 60 Hz, the clock continuous cases draw on.
pub const REFRESH: Duration = Duration::from_nanos(16_666_667);

/// How long a scene may take to show a complete frame.
const WAIT_LIMIT: Duration = Duration::from_secs(60);

/// A `render-tests/real-world/*` style whose tiles the cache database
/// holds, at its test's camera and size.
#[derive(Clone, Copy, Debug)]
pub struct RealWorld {
    pub name: &'static str,
    pub latitude: f64,
    pub longitude: f64,
    pub zoom: f64,
}

pub const REAL_WORLD: &[RealWorld] = &[
    RealWorld { name: "chicago", latitude: 41.8703965, longitude: -87.6942445, zoom: 13.0 },
    RealWorld { name: "sanfrancisco", latitude: 37.7669995, longitude: -122.448635, zoom: 15.0 },
    RealWorld { name: "norway", latitude: 64.863174, longitude: 10.8181, zoom: 12.0 },
];

/// The render tests' view, whose 1x sprites the cache database holds.
pub const REAL_WORLD_SIZE: Size = Size { width: 512, height: 512 };

impl RealWorld {
    pub fn style_url(&self) -> String {
        format!("local://styles/{}.json", self.name)
    }

    pub fn center(&self) -> LatLng {
        LatLng::new(self.latitude, self.longitude).expect("a scene's center")
    }
}

/// The render suite's `text-font/chinese` style, without its unused
/// sprite: `data/chinese.geojson`'s 500 labels in Noto Sans CJK, about 2,500
/// characters over 80 glyph ranges, all in the cache database.
pub const CJK_STYLE: &str = r#"{
  "version": 8,
  "sources": { "geojson": { "type": "geojson", "data": "local://data/chinese.geojson" } },
  "glyphs": "local://glyphs/{fontstack}/{range}.pbf",
  "layers": [
    { "id": "background", "type": "background", "paint": { "background-color": "white" } },
    {
      "id": "labels",
      "type": "symbol",
      "source": "geojson",
      "layout": { "text-field": "{name}", "text-font": ["NotoCJK"], "symbol-placement": "point" }
    }
  ]
}"#;

/// The `text-font/chinese` test's camera, at its size.
pub fn cjk_center() -> LatLng {
    LatLng::new(43.3900489867738, -70.752954126).expect("the CJK view's center")
}
pub const CJK_ZOOM: f64 = 11.0;
pub const CJK_SIZE: Size = Size { width: 512, height: 512 };

/// maplibre-native-ffi's view of the basemap.
pub const BASEMAP_SIZE: Size = Size { width: 1024, height: 768 };
/// The zoom of the basemap's tiles, and of its views.
pub const BASEMAP_ZOOM: f64 = 14.0;

/// A point of the basemap's 5 x 5 block of zoom-14 tiles, in tile
/// coordinates: x from 2618 to 2623, y from 6330 to 6335.
pub fn basemap_point(x: f64, y: f64) -> LatLng {
    let tiles = 16384.0;
    let longitude = x / tiles * 360.0 - 180.0;
    let latitude = (std::f64::consts::PI * (1.0 - 2.0 * y / tiles)).sinh().atan().to_degrees();
    LatLng::new(latitude, longitude).expect("a point of the block")
}

/// The block's center, and the corners of the views that share no tile.
pub fn basemap_center() -> LatLng {
    basemap_point(2620.5, 6332.5)
}
pub fn basemap_north_west() -> LatLng {
    basemap_point(2619.0, 6331.0)
}
pub fn basemap_south_east() -> LatLng {
    basemap_point(2622.0, 6334.0)
}

/// The basemap's style, its URLs pointing into `fixtures`.
pub fn basemap_style(fixtures: &Path) -> String {
    let style = include_str!("../style.json");
    style.replace("mln-bench-fixtures:/", &format!("file://{}/", fixtures.display()))
}

/// When the map draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `MapMode::Static`: [`Scene::render_still`] draws a complete frame.
    Still,
    /// `MapMode::Continuous`, drawn only when the case calls
    /// [`Scene::render_frame`], as a host on a display clock does.
    OnDemand,
    /// `MapMode::Continuous`, drawn whenever the map updates, as the run
    /// loop delivers the update.
    Driven,
}

/// The frames the map reports, and why it failed, if it did.
#[derive(Default)]
struct Observer {
    frames: Cell<u64>,
    last_frame: RefCell<Option<RenderFrameStatus>>,
    failure: RefCell<Option<String>>,
}

impl MapObserver for Observer {
    fn on_did_finish_rendering_frame(&self, status: &RenderFrameStatus) {
        self.frames.set(self.frames.get() + 1);
        *self.last_frame.borrow_mut() = Some(status.clone());
    }

    fn on_did_fail_loading_map(&self, _error: MapLoadError, what: &str) {
        *self.failure.borrow_mut() = Some(what.to_string());
    }
}

/// The file sources every scene shares: the render suite's proxy over its
/// cache database, with the tile latency of [`crate::latency`].
fn file_sources() -> FileSourceManager {
    static FILE_SOURCES: OnceLock<FileSourceManager> = OnceLock::new();
    FILE_SOURCES
        .get_or_init(|| {
            let file_sources = render_test::file_source::proxy_file_sources();
            crate::latency::register(&file_sources);
            file_sources
        })
        .clone()
}

pub struct Scene {
    frontend: HeadlessFrontend,
    map: Map,
    observer: Rc<Observer>,
    _file_source: Option<Arc<dyn FileSource>>,
}

impl Scene {
    /// A map without a style; its GPU device is ready. Its renderer has a
    /// glyph cache of its own.
    pub fn new(size: Size, mode: Mode) -> Scene {
        Scene::with_glyph_cache(size, mode, Arc::default())
    }

    /// A map whose renderer shares `glyph_cache`.
    pub fn with_glyph_cache(size: Size, mode: Mode, glyph_cache: Arc<GlyphCache>) -> Scene {
        let file_sources = file_sources();
        let cache = Path::new(env!("CARGO_MANIFEST_DIR")).join("../render-test/testdata/metrics/cache-style.db");
        let resource_options = ResourceOptions::default().with_cache_path(cache.display().to_string());
        let frontend = HeadlessFrontend::with_glyph_cache(
            size,
            1.0,
            SwapBehaviour::NoFlush,
            ContextMode::Unique,
            mode != Mode::OnDemand,
            glyph_cache,
        );
        let file_source = file_sources.get_file_source(FileSourceType::ResourceLoader, &resource_options);
        let observer = Rc::new(Observer::default());
        let map_mode = if mode == Mode::Still { MapMode::Static } else { MapMode::Continuous };
        let map = Map::with_resource_options(
            &frontend,
            observer.clone(),
            MapOptions::new().with_map_mode(map_mode).with_size(size).with_pixel_ratio(1.0),
            &file_sources,
            &resource_options,
        )
        .expect("the map");
        Scene { frontend, map, observer, _file_source: file_source }
    }

    /// Starts loading a style; it loads while the scene renders.
    pub fn load_url(&mut self, url: &str) {
        self.map.get_style_mut().load_url(url);
    }

    pub fn load_json(&mut self, json: &str) {
        self.map.get_style_mut().load_json(json);
    }

    /// Adds an image to the style, as a sprite's would be.
    pub fn add_image(&mut self, id: &str, image: PremultipliedImage) {
        let image =
            mln_style::image::Impl::new(id.to_owned(), image, 1.0, false, Vec::new(), Vec::new(), None, None, None)
                .expect("a style image");
        self.map.get_style_mut().add_image(Arc::new(image));
    }

    pub fn jump(&mut self, center: LatLng, zoom: f64) {
        self.jump_rotated(center, zoom, 0.0);
    }

    /// Jumps to a camera with `bearing` in degrees.
    pub fn jump_rotated(&mut self, center: LatLng, zoom: f64, bearing: f64) {
        self.map.jump_to(
            &CameraOptions::default().with_center(Some(center)).with_zoom(Some(zoom)).with_bearing(Some(bearing)),
        );
    }

    /// A complete still frame, read back. Waits on the run loop, which
    /// sleeps until there is work, rather than polling it, so the wait
    /// costs no CPU time.
    pub fn render_still(&mut self) -> PremultipliedImage {
        let outcome: Rc<RefCell<Option<Option<String>>>> = Rc::default();
        let done = outcome.clone();
        self.map.render_still(Box::new(move |error| {
            *done.borrow_mut() = Some(error.map(|error| error.to_string()));
            RunLoop::get().stop();
        }));
        // The callback can run before the loop starts; `run` would then
        // wait for a stop that already happened.
        if outcome.borrow().is_none() {
            RunLoop::get().run();
        }
        if let Some(Some(error)) = outcome.take() {
            panic!("still: {error}");
        }
        self.frontend.read_still_image()
    }

    /// A complete still frame through `HeadlessFrontend::render`, as the
    /// render tests draw one.
    pub fn render(&mut self) -> PremultipliedImage {
        match self.frontend.render(&mut self.map) {
            Ok(result) => result.image,
            Err(error) => panic!("still: {error}"),
        }
    }

    /// Delivers what the map's workers have finished, without waiting.
    pub fn pump(&self) {
        RunLoop::get().run_once();
    }

    /// Draws a frame of what has loaded and waits for the GPU to finish
    /// it, as a host does before presenting it.
    pub fn render_frame(&mut self) -> RenderFrameStatus {
        self.frontend.render_frame();
        self.wait_for_gpu();
        self.observer.last_frame.borrow().clone().expect("the renderer reports each frame")
    }

    fn wait_for_gpu(&self) {
        if let Some(device) = self.webgpu().get_device() {
            device.poll(wgpu::PollType::wait_indefinitely()).expect("the GPU finishes the frame");
        }
    }

    /// The last frame [`Scene::render_frame`] drew, read back.
    pub fn read_frame(&self) -> PremultipliedImage {
        self.frontend.read_still_image()
    }

    /// Draws frames on the display's clock until one shows the view
    /// complete, and returns how many it drew.
    pub fn present_until_complete(&mut self) -> usize {
        self.present_until(|status| status.mode == RenderMode::Full)
    }

    /// Draws frames on the display's clock until the view is complete and
    /// no transition or fade needs another frame.
    pub fn settle(&mut self) -> usize {
        self.present_until(|status| status.mode == RenderMode::Full && !status.needs_repaint)
    }

    fn present_until(&mut self, done: impl Fn(&RenderFrameStatus) -> bool) -> usize {
        let start = Instant::now();
        let mut frames = 0;
        loop {
            let deadline = start + REFRESH * frames as u32;
            std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
            self.pump();
            frames += 1;
            if done(&self.render_frame()) {
                return frames;
            }
            self.check(start);
        }
    }

    /// Panics when the map failed, or took longer than its limit.
    fn check(&self, start: Instant) {
        if let Some(failure) = self.observer.failure.borrow().as_ref() {
            panic!("the map failed to load: {failure}");
        }
        assert!(start.elapsed() < WAIT_LIMIT, "the view isn't complete after {WAIT_LIMIT:?}");
    }

    /// Lets the run loop work for `duration`, drawing as the map asks
    /// (`Mode::Driven`).
    pub fn run_for(&self, duration: Duration) {
        let timer = Timer::new();
        let timeout = mln_util::chrono::Duration::from_nanos(duration.as_nanos() as i64);
        timer.start(timeout, mln_util::chrono::Duration::zero(), || RunLoop::get().stop());
        RunLoop::get().run();
    }

    /// The frames the renderer has drawn.
    pub fn frames(&self) -> u64 {
        self.observer.frames.get()
    }

    /// Whether the last frame showed the view complete with nothing left to
    /// animate.
    pub fn is_settled(&self) -> bool {
        self.observer
            .last_frame
            .borrow()
            .as_ref()
            .is_some_and(|status| status.mode == RenderMode::Full && !status.needs_repaint)
    }

    /// Shows or hides every layer that draws `source`.
    pub fn set_source_visible(&mut self, source: &str, visible: bool) {
        let ids: Vec<String> = self
            .map
            .get_style()
            .get_layers()
            .iter()
            .filter(|layer| layer.get_source_id() == source)
            .map(|layer| layer.get_id())
            .collect();
        assert!(!ids.is_empty(), "no layer draws {source}");
        let visibility = if visible { VisibilityType::Visible } else { VisibilityType::None };
        for id in ids {
            self.map.get_style_mut().get_layer_mut(&id).expect("the layer").set_visibility(visibility);
        }
    }

    /// The vertex and index buffers the GPU held after the last frame, in
    /// MiB.
    pub fn gpu_buffers_mib(&self) -> f64 {
        let last_frame = self.observer.last_frame.borrow();
        let stats = &last_frame.as_ref().expect("a frame").rendering_stats;
        f64::from(stats.mem_vertex_buffers + stats.mem_index_buffers) / (1024.0 * 1024.0)
    }

    /// Drops every loaded tile and the renderer's other data, as after a
    /// memory warning: the next frames load and lay out again.
    pub fn clear_data(&self) {
        self.frontend.with_renderer(|renderer| renderer.clear_data());
    }

    /// Sets `selected` on every other feature of `source_layer` in the
    /// loaded tiles, and `false` on the rest.
    pub fn select_every_feature(&self, source: &str, source_layer: &str) {
        self.frontend.with_renderer(|renderer| {
            let options = SourceQueryOptions::new(Some(vec![source_layer.to_string()]), None);
            let features = renderer.query_source_features(source, &options).expect("the source's features");
            let ids: BTreeSet<String> =
                features.iter().filter_map(|feature| feature_id_to_string(&feature.id)).collect();
            assert!(!ids.is_empty(), "{source_layer} has no features with ids");
            for (index, id) in ids.iter().enumerate() {
                let mut state = ValueObject::new();
                state.insert("selected".to_string(), Value::Bool(index % 2 == 0));
                renderer.set_feature_state(source, Some(source_layer), id, &state);
            }
        });
    }

    /// Sets the `selected` state of one feature.
    pub fn select_feature(&self, source: &str, source_layer: &str, id: &str, selected: bool) {
        self.frontend.with_renderer(|renderer| {
            let mut state = ValueObject::new();
            state.insert("selected".to_string(), Value::Bool(selected));
            renderer.set_feature_state(source, Some(source_layer), id, &state);
        });
    }

    /// The ids of the loaded features of a source layer.
    pub fn feature_ids(&self, source: &str, source_layer: &str) -> Vec<String> {
        self.frontend.with_renderer(|renderer| {
            let options = SourceQueryOptions::new(Some(vec![source_layer.to_string()]), None);
            let features = renderer.query_source_features(source, &options).expect("the source's features");
            let ids: BTreeSet<String> =
                features.iter().filter_map(|feature| feature_id_to_string(&feature.id)).collect();
            ids.into_iter().collect()
        })
    }

    fn webgpu(&self) -> &mln_webgpu::HeadlessBackend {
        self.frontend
            .get_backend()
            .as_any()
            .downcast_ref::<mln_webgpu::HeadlessBackend>()
            .expect("the headless frontend renders through WebGPU")
    }

    /// The adapter's description, and whether it is a software rasterizer.
    pub fn gpu(&self) -> (String, bool) {
        let Some(info) = self.webgpu().get_adapter_info() else {
            return ("no adapter".to_string(), true);
        };
        let name = info.name.to_lowercase();
        let software = info.device_type == wgpu::DeviceType::Cpu
            || ["llvmpipe", "lavapipe", "swiftshader", "software", "warp"].iter().any(|s| name.contains(s));
        (format!("{} ({:?}, {} {})", info.name, info.backend, info.driver, info.driver_info), software)
    }
}

/// Writes `image` as `<dir>/<name>.png`, the name's slashes as dashes.
pub fn write_frame(dir: &Path, name: &str, image: &PremultipliedImage) {
    std::fs::create_dir_all(dir).expect("the frames directory");
    let path = dir.join(format!("{}.png", name.replace('/', "-")));
    std::fs::write(&path, mln_image::png_writer::encode_png(image))
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
}
