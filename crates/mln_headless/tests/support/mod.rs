// Test support for the ported test/map and test/api gtests (gtests-core).
//
// Ports, for the tests in this directory:
// - test/src/mln/test/util.cpp: `checkImage` / `getImageDiff` (pixelmatch
//   against `<base>/expected.png`; actual and diff images go to
//   target/tmp/<base>/, never next to the fixture);
// - test/include/mln/test/stub_map_observer.hpp (`StubMapObserver`);
// - test/include/mln/test/fake_file_source.hpp (`FakeFileSource`);
// - test/include/mln/test/fixture_log_observer.hpp (`FixtureLog`, per
//   thread: Rust runs tests on parallel threads of one process);
// - the `MapTest` fixture of test/map/map.test.cpp (C++ `MapAdapter` is
//   `Map::with_file_source`).
//
// Fixtures are read from the copy of the C++ repository's files in
// testdata/; paths are written as in C++, relative to the maplibre-native
// root ("test/fixtures/api/empty.json").

#![allow(dead_code)]

use std::cell::{Cell, RefCell, RefMut};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex, OnceLock};

use mln_actor::{LocalObject, RunLoop, Timer};
use mln_headless::{HeadlessFrontend, RenderResult};
use mln_map::{Map, MapLoadError, MapMode, MapObserver, MapOptions, RenderFrameStatus};
use mln_renderer::{ExceptionPtr, FontStack, GlyphRange, OverscaledTileID, TileOperation};
use mln_storage::stub_file_source::{ResponseType, StubFileSource};
use mln_storage::{AsyncRequest, Callback, FileSource, Kind, Resource, ResourceOptions, Response};
use mln_style::custom_tile_loader::TileFunction;
use mln_style::sources::custom_geometry_source::CustomGeometrySourceHandle;
use mln_style::sprite::Sprite;
use mln_tile_id::CanonicalTileID;
use mln_util::image::PremultipliedImage;
use mln_util::logging::{Event, EventSeverity, Log};

// MARK: - Fixtures

/// testdata/, the copy of the C++ repository's files these tests read.
pub fn cpp_root() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata")
}

/// `util::read_file(path)` with `path` relative to the C++ root.
pub fn read_bytes(path: &str) -> Vec<u8> {
    let full = cpp_root().join(path);
    std::fs::read(&full).unwrap_or_else(|e| panic!("{}: {e}", full.display()))
}

pub fn read_file(path: &str) -> String {
    String::from_utf8(read_bytes(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// `decodeImage(util::read_file(path))`.
pub fn decode_image(path: &str) -> PremultipliedImage {
    mln_image::decode_image(&read_bytes(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// A `Response` with `data` = the fixture's bytes.
pub fn file_response(path: &str) -> Response {
    let mut response = Response::new();
    response.data = Some(Arc::new(read_bytes(path)));
    response
}

// MARK: - checkImage (test/src/mln/test/util.cpp)

fn write_output(base: &str, name: &str, image: &PremultipliedImage) {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(base);
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::write(dir.join(name), mln_image::encode_png(image));
    }
}

/// `test::getImageDiff(base, actual, pixelThreshold)`.
pub fn get_image_diff(base: &str, actual: &PremultipliedImage, pixel_threshold: f64) -> f64 {
    let expected = decode_image(&format!("{base}/expected.png"));
    let mut diff = PremultipliedImage::new(expected.size);

    write_output(base, "actual.png", actual);

    if expected.size != actual.size {
        return 1.0;
    }

    let pixels = pixelmatch::pixelmatch(
        &actual.data,
        &expected.data,
        expected.size.width as usize,
        expected.size.height as usize,
        Some(&mut diff.data),
        pixel_threshold,
        false,
    );

    write_output(base, "diff.png", &diff);

    pixels as f64 / (f64::from(expected.size.width) * f64::from(expected.size.height))
}

/// `test::checkImages(possibleExpected, actual, imageThreshold,
/// pixelThreshold)`: the image matches one of the bases.
#[track_caller]
pub fn check_images(bases: &[&str], actual: &PremultipliedImage, image_threshold: f64, pixel_threshold: f64) {
    let diffs: Vec<f64> = bases.iter().map(|base| get_image_diff(base, actual, pixel_threshold)).collect();
    assert!(
        diffs.iter().any(|diff| *diff <= image_threshold),
        "{bases:?}: image differences {diffs:?}, none <= {image_threshold} (pixel threshold {pixel_threshold})"
    );
}

/// `test::checkImage(base, actual, imageThreshold, pixelThreshold)`: the
/// C++ defaults are 0 and 0.
#[track_caller]
pub fn check_image(base: &str, actual: &PremultipliedImage, image_threshold: f64, pixel_threshold: f64) {
    let diff = get_image_diff(base, actual, pixel_threshold);
    assert!(
        diff <= image_threshold,
        "{base}: image difference {diff} > {image_threshold} (pixel threshold {pixel_threshold})"
    );
}

/// The channel values of the centre pixel (C++ `image.data.get() +
/// (height / 2) * stride + (width / 2) * channels`).
pub fn center_pixel(image: &PremultipliedImage) -> [u8; 4] {
    let offset = (image.size.height as usize / 2) * image.stride() + (image.size.width as usize / 2) * 4;
    [image.data[offset], image.data[offset + 1], image.data[offset + 2], image.data[offset + 3]]
}

// MARK: - Custom geometry sources

/// A custom geometry source's fetch function that answers with `features`
/// through the handle in `handle`, once it's set, as the C++ tests answer
/// through `getSource("custom")`.
pub fn answer_with(
    handle: &Arc<OnceLock<CustomGeometrySourceHandle>>,
    features: impl Fn(&CanonicalTileID) -> Vec<geojson::Feature> + Send + Sync + 'static,
) -> TileFunction {
    let handle = handle.clone();
    Arc::new(move |tile_id: &CanonicalTileID| {
        if let Some(handle) = handle.get() {
            handle.set_tile_data(*tile_id, geojson::GeoJSON::FeatureCollection(features(tile_id)));
        }
    })
}

// MARK: - ASSERT_DOUBLE_EQ

/// gtest's `DoubleNear`-free `ASSERT_DOUBLE_EQ`: within 4 ULPs.
#[track_caller]
pub fn assert_double_eq(actual: f64, expected: f64) {
    fn biased(x: f64) -> u64 {
        let bits = x.to_bits();
        const SIGN: u64 = 1 << 63;
        if bits & SIGN != 0 { (!bits).wrapping_add(1) } else { bits | SIGN }
    }
    let ok = if actual.is_nan() || expected.is_nan() {
        false
    } else {
        let (a, b) = (biased(actual), biased(expected));
        a.abs_diff(b) <= 4
    };
    assert!(ok, "expected {expected} (4 ULPs), got {actual}");
}

/// `EXPECT_NEAR(a, b, tolerance)`.
#[track_caller]
pub fn assert_near(actual: f64, expected: f64, tolerance: f64) {
    assert!(
        (actual - expected).abs() <= tolerance,
        "expected {expected} +- {tolerance}, got {actual} (difference {})",
        (actual - expected).abs()
    );
}

// MARK: - StubMapObserver (test/include/mln/test/stub_map_observer.hpp)

/// A `std::function` member of StubMapObserver. A callback may replace or
/// clear any slot, including its own, while it runs.
pub struct CallbackSlot<A> {
    callback: RefCell<Option<Box<dyn FnMut(A)>>>,
    generation: Cell<u64>,
}

impl<A> Default for CallbackSlot<A> {
    fn default() -> Self {
        CallbackSlot { callback: RefCell::new(None), generation: Cell::new(0) }
    }
}

impl<A> CallbackSlot<A> {
    pub fn set(&self, callback: impl FnMut(A) + 'static) {
        *self.callback.borrow_mut() = Some(Box::new(callback));
        self.generation.set(self.generation.get() + 1);
    }

    pub fn clear(&self) {
        *self.callback.borrow_mut() = None;
        self.generation.set(self.generation.get() + 1);
    }

    pub fn call(&self, arg: A) {
        let generation = self.generation.get();
        let callback = self.callback.borrow_mut().take();
        if let Some(mut callback) = callback {
            callback(arg);
            if self.generation.get() == generation {
                *self.callback.borrow_mut() = Some(callback);
            }
        }
    }
}

/// `StubMapObserver`, with callbacks for the source and sprite events too.
// TODO(port): onRegisterShaders, onPre/PostCompileShader,
// onShaderCompileFailed callbacks (MapObserver has none of them yet).
#[derive(Default)]
pub struct StubMapObserver {
    pub will_start_loading_map_callback: CallbackSlot<()>,
    pub did_finish_loading_map_callback: CallbackSlot<()>,
    pub did_fail_loading_map_callback: CallbackSlot<()>,
    pub did_finish_loading_style_callback: CallbackSlot<()>,
    pub did_finish_rendering_frame_callback: CallbackSlot<RenderFrameStatus>,
    pub did_become_idle_callback: CallbackSlot<()>,
    pub on_style_image_missing_callback: CallbackSlot<String>,
    pub on_glyphs_loaded_callback: CallbackSlot<(FontStack, GlyphRange)>,
    pub on_glyphs_error_callback: CallbackSlot<(FontStack, GlyphRange, ExceptionPtr)>,
    pub on_glyphs_requested_callback: CallbackSlot<(FontStack, GlyphRange)>,
    pub on_tile_action_callback: CallbackSlot<(TileOperation, OverscaledTileID, String)>,
    pub on_source_changed_callback: CallbackSlot<String>,
    pub on_sprite_loaded_callback: CallbackSlot<Option<Sprite>>,
    pub on_sprite_error_callback: CallbackSlot<(Option<Sprite>, ExceptionPtr)>,
    pub on_sprite_requested_callback: CallbackSlot<Option<Sprite>>,
}

impl MapObserver for StubMapObserver {
    fn on_will_start_loading_map(&self) {
        self.will_start_loading_map_callback.call(());
    }
    fn on_did_finish_loading_map(&self) {
        self.did_finish_loading_map_callback.call(());
    }
    fn on_did_fail_loading_map(&self, _error: MapLoadError, _what: &str) {
        self.did_fail_loading_map_callback.call(());
    }
    fn on_did_finish_loading_style(&self) {
        self.did_finish_loading_style_callback.call(());
    }
    fn on_did_finish_rendering_frame(&self, status: &RenderFrameStatus) {
        self.did_finish_rendering_frame_callback.call(status.clone());
    }
    fn on_did_become_idle(&self) {
        self.did_become_idle_callback.call(());
    }
    fn on_style_image_missing(&self, id: &str) {
        self.on_style_image_missing_callback.call(id.to_owned());
    }
    fn on_glyphs_loaded(&self, font_stack: &FontStack, range: &GlyphRange) {
        self.on_glyphs_loaded_callback.call((font_stack.clone(), *range));
    }
    fn on_glyphs_error(&self, font_stack: &FontStack, range: &GlyphRange, error: ExceptionPtr) {
        self.on_glyphs_error_callback.call((font_stack.clone(), *range, error));
    }
    fn on_glyphs_requested(&self, font_stack: &FontStack, range: &GlyphRange) {
        self.on_glyphs_requested_callback.call((font_stack.clone(), *range));
    }
    fn on_tile_action(&self, op: TileOperation, id: &OverscaledTileID, source_id: &str) {
        self.on_tile_action_callback.call((op, *id, source_id.to_owned()));
    }
    fn on_source_changed(&self, source_id: &str) {
        self.on_source_changed_callback.call(source_id.to_owned());
    }
    fn on_sprite_loaded(&self, sprite: Option<&Sprite>) {
        self.on_sprite_loaded_callback.call(sprite.cloned());
    }
    fn on_sprite_error(&self, sprite: Option<&Sprite>, error: ExceptionPtr) {
        self.on_sprite_error_callback.call((sprite.cloned(), error));
    }
    fn on_sprite_requested(&self, sprite: Option<&Sprite>) {
        self.on_sprite_requested_callback.call(sprite.cloned());
    }
}

// MARK: - FakeFileSource (test/include/mln/test/fake_file_source.hpp)

struct FakeRequestEntry {
    id: u64,
    resource: Resource,
    callback: Rc<RefCell<Callback>>,
}

type FakeRequests = RefCell<Vec<FakeRequestEntry>>;

/// `FakeFileSource::FakeFileRequest`: removes itself from the list when
/// dropped.
struct FakeFileRequest {
    id: u64,
    list: Weak<FakeRequests>,
}

impl AsyncRequest for FakeFileRequest {}

impl Drop for FakeFileRequest {
    fn drop(&mut self) {
        if let Some(list) = self.list.upgrade() {
            list.borrow_mut().retain(|entry| entry.id != self.id);
        }
    }
}

/// `FakeFileSource`: records the requests made; the test responds to them.
pub struct FakeFileSource {
    requests: LocalObject<FakeRequests>,
    next_id: Mutex<u64>,
    resource_options: Mutex<ResourceOptions>,
}

impl FakeFileSource {
    /// `FakeFileSource()`: `ResourceOptions::Default()`.
    pub fn new() -> Self {
        FakeFileSource {
            requests: LocalObject::new(RefCell::new(Vec::new())),
            next_id: Mutex::new(0),
            resource_options: Mutex::new(ResourceOptions::default()),
        }
    }

    fn list(&self) -> Rc<FakeRequests> {
        self.requests.get().expect("FakeFileSource used from another thread")
    }

    /// `requests.size()`.
    pub fn requests_len(&self) -> usize {
        self.list().borrow().len()
    }

    /// `respond(kind, response)`: answers the first pending request of that
    /// kind; returns whether there was one.
    pub fn respond(&self, kind: Kind, response: &Response) -> bool {
        let callback = {
            let list = self.list();
            let list = list.borrow();
            list.iter().find(|entry| entry.resource.kind == kind).map(|entry| entry.callback.clone())
        };
        match callback {
            // Copy the callback, in case calling it deallocates the AsyncRequest.
            Some(callback) => {
                (callback.borrow_mut())(response.clone());
                true
            }
            None => false,
        }
    }
}

impl Default for FakeFileSource {
    fn default() -> Self {
        Self::new()
    }
}

impl FileSource for FakeFileSource {
    fn request(&self, resource: &Resource, callback: Callback) -> Box<dyn AsyncRequest> {
        let id = {
            let mut next = self.next_id.lock().unwrap();
            *next += 1;
            *next
        };
        let list = self.list();
        list.borrow_mut().push(FakeRequestEntry {
            id,
            resource: resource.clone(),
            callback: Rc::new(RefCell::new(callback)),
        });
        Box::new(FakeFileRequest { id, list: Rc::downgrade(&list) })
    }

    fn can_request(&self, _resource: &Resource) -> bool {
        true
    }

    fn set_resource_options(&self, options: ResourceOptions) {
        *self.resource_options.lock().unwrap() = options;
    }

    fn get_resource_options(&self) -> ResourceOptions {
        self.resource_options.lock().unwrap().clone()
    }
}

// MARK: - FixtureLog (test/include/mln/test/fixture_log_observer.hpp)

type LogRecord = (EventSeverity, Event, i64, String);
type LogRecords = Arc<Mutex<Vec<(LogRecord, bool)>>>;

struct Recorder(LogRecords);

impl mln_util::logging::Observer for Recorder {
    fn on_record(&self, severity: EventSeverity, event: Event, code: i64, msg: &str) -> bool {
        self.0.lock().unwrap().push(((severity, event, code, msg.to_owned()), false));
        true
    }
}

/// `FixtureLog`: captures the log records of the calling thread while it
/// lives (C++ installs a process-wide observer).
pub struct FixtureLog {
    messages: LogRecords,
}

impl FixtureLog {
    pub fn new() -> Self {
        let messages: LogRecords = Arc::new(Mutex::new(Vec::new()));
        Log::set_thread_observer(Some(Box::new(Recorder(messages.clone()))));
        FixtureLog { messages }
    }

    /// `count(message)`: marks the matching records checked.
    pub fn count(&self, severity: EventSeverity, event: Event, code: i64, msg: &str) -> usize {
        let mut count = 0;
        for ((s, e, c, m), checked) in self.messages.lock().unwrap().iter_mut() {
            if *s == severity && *e == event && *c == code && m == msg {
                *checked = true;
                count += 1;
            }
        }
        count
    }

    /// `unchecked()`: the records nobody counted.
    pub fn unchecked(&self) -> Vec<LogRecord> {
        self.messages.lock().unwrap().iter().filter(|(_, checked)| !checked).map(|(record, _)| record.clone()).collect()
    }
}

impl Default for FixtureLog {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for FixtureLog {
    fn drop(&mut self) {
        Log::set_thread_observer(None);
    }
}

// MARK: - RunLoop with an emergency stop

/// `runLoop.run()`, stopped (and failed) after `seconds` if nothing stopped
/// it: a hang fails the test instead of hanging the test run.
#[track_caller]
pub fn run_with_timeout(run_loop: &RunLoop, seconds: i64) {
    let timed_out = Rc::new(Cell::new(false));
    let timer = Timer::new();
    {
        let timed_out = timed_out.clone();
        let run_loop = run_loop.clone();
        timer.start(mln_util::chrono::Duration::from_secs(seconds), mln_util::chrono::Duration::zero(), move || {
            timed_out.set(true);
            run_loop.stop();
        });
    }
    run_loop.run();
    timer.stop();
    assert!(!timed_out.get(), "the run loop was not stopped within {seconds} s");
}

// MARK: - MapTest (test/map/map.test.cpp)

/// `MapTest<FileSource>`. Fields drop in declaration order, which is the
/// C++ destruction order (reverse of declaration there). The map is shared
/// (`Rc<RefCell<..>>`) so observer callbacks can reach it through a `Weak`
/// as C++ lambdas capture `test`.
pub struct MapTest<FS: FileSource + 'static = StubFileSource> {
    pub map: Rc<RefCell<Map>>,
    pub frontend: HeadlessFrontend,
    pub observer: Rc<StubMapObserver>,
    pub file_source: Arc<FS>,
    pub run_loop: RunLoop,
}

impl MapTest<StubFileSource> {
    /// `MapTest<>()`: pixel ratio 1, `MapMode::Static`.
    pub fn new() -> Self {
        Self::with_mode(1.0, MapMode::Static)
    }

    /// `MapTest<>(pixelRatio, mode)`.
    pub fn with_mode(pixel_ratio: f32, mode: MapMode) -> Self {
        let run_loop = RunLoop::new();
        let file_source = Arc::new(StubFileSource::new(ResponseType::Asynchronous));
        Self::create(run_loop, file_source, pixel_ratio, mode)
    }
}

impl MapTest<FakeFileSource> {
    /// `MapTest<FakeFileSource>()`.
    pub fn fake() -> Self {
        let run_loop = RunLoop::new();
        let file_source = Arc::new(FakeFileSource::new());
        Self::create(run_loop, file_source, 1.0, MapMode::Static)
    }
}

impl<FS: FileSource + 'static> MapTest<FS> {
    /// The shared constructor: `fileSource`, `frontend(pixelRatio)`,
    /// `map(frontend, observer, fileSource, MapOptions().withMapMode(mode)
    /// .withSize(frontend.getSize()).withPixelRatio(pixelRatio))`.
    pub fn create(run_loop: RunLoop, file_source: Arc<FS>, pixel_ratio: f32, mode: MapMode) -> Self {
        let observer = Rc::new(StubMapObserver::default());
        let frontend = HeadlessFrontend::with_pixel_ratio(pixel_ratio);
        let options =
            MapOptions::new().with_map_mode(mode).with_size(frontend.get_size()).with_pixel_ratio(pixel_ratio);
        let map = Map::with_file_source(&frontend, observer.clone(), file_source.clone(), options).unwrap();
        MapTest { map: Rc::new(RefCell::new(map)), frontend, observer, file_source, run_loop }
    }

    /// `test.map`.
    pub fn map(&self) -> RefMut<'_, Map> {
        self.map.borrow_mut()
    }

    /// A handle for callbacks (no reference cycle through the observer).
    pub fn weak_map(&self) -> Weak<RefCell<Map>> {
        Rc::downgrade(&self.map)
    }

    /// `test.frontend.render(test.map)`; C++ rethrows a still image error.
    pub fn render(&self) -> RenderResult {
        self.try_render().unwrap_or_else(|e| panic!("render: {e}"))
    }

    pub fn try_render(&self) -> Result<RenderResult, String> {
        let mut frontend = self.frontend.clone();
        let mut map = self.map.borrow_mut();
        frontend.render(&mut map)
    }

    /// `test.map.getStyle().loadJSON(json)`.
    pub fn load_json(&self, json: &str) {
        self.map().get_style_mut().load_json(json);
    }
}
