// Ports render-test/runner.{hpp,cpp}, and `TestContext` / `TestOperation`
// from render-test/metadata.hpp.
//
// C++ `TestRunner::Impl` holds the observer, `HeadlessFrontend`,
// `FileSource` and `Map` of one map configuration, and the operations reach
// them through `TestContext::getFrontend()` / `getMap()`. Here that pair is
// the `TestRunnerImpl` trait: map.rs implements it over `mln_map::Map` +
// `mln_headless::HeadlessFrontend`, the tests over a mock map, and
// `--self-check` replaces rendering with the test's expected image.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::time::Duration;

use feature::Value;
use geometry::Point;
use mln_image::{decode_image, encode_png};
use mln_util::feature::Feature;
use mln_util::geo::LatLng;
use mln_util::image::PremultipliedImage;
use mln_util::size::Size;

use crate::file_source::ProxyFileSource;
use crate::filesystem;
use crate::log::{self, Event};
use crate::manifest_parser::Manifest;
use crate::metadata::{
    GfxProbe, MapMode, MemoryProbe, NetworkProbe, RenderedQueryOptions, ScreenBox, TestMetadata, check_value,
};
use crate::parser::test_operation_names::{GFX_PROBE_OP, MEMORY_PROBE_OP, NETWORK_PROBE_OP};
use crate::parser::{
    TileLodMode, parse_test_operations, read_expected_image_entries, read_expected_json_entries,
    read_expected_metric_entries, read_expected_metrics, read_json, to_json_features, to_json_value,
};
use crate::test_runner_common::{deep_equal, tokenize};

/// `mln::CameraOptions` (include/mln/map/camera.hpp): the fields the
/// operations set.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CameraOptions {
    pub center: Option<LatLng>,
    pub center_altitude: Option<f64>,
    pub zoom: Option<f64>,
    pub bearing: Option<f64>,
    pub pitch: Option<f64>,
    pub roll: Option<f64>,
}

/// How a `TestRunnerImpl` call can fail, standing in for the C++ return
/// values (`nullptr` layer, `optional<Error>` conversions, failed resource
/// requests) and for API the port does not have yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MapError {
    /// `style.getLayer(id)` returned `nullptr`.
    LayerNotFound,
    /// A style conversion failed; the message is `conversion::Error::message`.
    Conversion(String),
    /// A resource request returned no data.
    LoadFailed,
    /// TODO(port): the named C++ API is not ported yet. The test errors.
    NotPorted(String),
    /// A C++ exception the runner does not catch (the C++ runner would
    /// terminate); the port errors the test with its message.
    Exception(String),
}

fn not_ported<T>(what: &str) -> Result<T, MapError> {
    Err(MapError::NotPorted(what.to_string()))
}

/// `mln::ResourceOptions` as the runner fills it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResourceOptions {
    pub cache_path: String,
    pub api_key: String,
}

/// `TestRunner::Impl` (observer, frontend, file source and map for one
/// `mapMode`/`pixelRatio`/`crossSourceCollisions` key), seen through the
/// calls the runner and the operations make. The provided methods return
/// `MapError::NotPorted`; implementations override what they support.
pub trait TestRunnerImpl {
    /// `resetContext(metadata, ctx)`: `getRenderer()->clearData()`,
    /// `frontend.setSize(metadata.size)`, `map.setSize(metadata.size)`,
    /// `map.setProjectionMode(ProjectionMode().withAxonometric(..)
    /// .withXSkew(..).withYSkew(..))`, `map.setDebug(metadata.debug)`,
    /// `map.getStyle().loadJSON(metadata.document_json)`.
    fn reset_context(&mut self, metadata: &TestMetadata) -> Result<(), MapError>;

    /// `map.jumpTo(map.getStyle().getDefaultCamera())`.
    fn jump_to_default_camera(&mut self) -> Result<(), MapError>;

    /// `frontend.render(map).image`; `Err` carries the exception message.
    fn render(&mut self) -> Result<PremultipliedImage, String>;

    /// Port-only: `map.dumpDebugLogs()` and the renderer's, for a test that
    /// timed out (watchdog.rs): which tiles never loaded or completed.
    fn dump_debug_logs(&self) {}

    /// `map.jumpTo(camera)`.
    fn jump_to(&mut self, _camera: &CameraOptions) -> Result<(), MapError> {
        not_ported("Map::jumpTo")
    }
    /// `map.getStyle().loadJSON(json)`.
    fn load_style_json(&mut self, _json: &str) -> Result<(), MapError> {
        not_ported("Style::loadJSON (setStyle operation)")
    }
    /// `map.getStyle().loadURL(url)`.
    fn load_style_url(&mut self, _url: &str) -> Result<(), MapError> {
        not_ported("Style::loadURL")
    }
    /// Request `"mapbox://render-tests/" + image_path` from the file source,
    /// `decodeImage` it and `style.addImage(std::make_unique<style::Image>(
    /// name, image, pixel_ratio, sdf))`. `LoadFailed` when the request
    /// returned no data.
    fn add_image(&mut self, _name: &str, _image_path: &str, _pixel_ratio: f32, _sdf: bool) -> Result<(), MapError> {
        not_ported("Style::addImage + render-test file source")
    }
    /// `style.removeImage(name)`.
    fn remove_image(&mut self, _name: &str) -> Result<(), MapError> {
        not_ported("Style::removeImage")
    }
    /// `convert<Filter>(filter)` (`Conversion` on failure, checked first),
    /// then `style.getLayer(layer)` (`LayerNotFound`), then `setFilter`.
    fn set_layer_filter(&mut self, _layer: &str, _filter: &serde_json::Value) -> Result<(), MapError> {
        not_ported("Layer::setFilter")
    }
    /// `layer->setMinZoom(min)`, `layer->setMaxZoom(max)`.
    fn set_layer_zoom_range(&mut self, _layer: &str, _min: f32, _max: f32) -> Result<(), MapError> {
        not_ported("Layer::setMinZoom/setMaxZoom")
    }
    /// `convertJSON<std::unique_ptr<Layer>>(json)` then `style.addLayer`.
    fn add_layer(&mut self, _json: &str) -> Result<(), MapError> {
        not_ported("Style::addLayer")
    }
    /// `bool(style.removeLayer(id))`.
    fn remove_layer(&mut self, _id: &str) -> Result<bool, MapError> {
        not_ported("Style::removeLayer")
    }
    /// `convertJSON<std::unique_ptr<Source>>(json, error, id)` then
    /// `style.addSource`.
    fn add_source(&mut self, _id: &str, _json: &str) -> Result<(), MapError> {
        not_ported("Style::addSource")
    }
    /// conformance: `convert<style::Light>(json)` then `style.setLight`
    /// (a conversion error is `MapError::Conversion`).
    fn set_light(&mut self, _json: &str) -> Result<(), MapError> {
        not_ported("Style::setLight")
    }
    /// `bool(style.removeSource(id))`.
    fn remove_source(&mut self, _id: &str) -> Result<bool, MapError> {
        not_ported("Style::removeSource")
    }
    /// `style.getLayer(layer)` (`LayerNotFound`), then
    /// `layer->setProperty(name, value)` (its error is ignored by the runner).
    fn set_layer_property(&mut self, _layer: &str, _name: &str, _value: &serde_json::Value) -> Result<(), MapError> {
        not_ported("Layer::setProperty")
    }
    /// `style.setGlobalStateProperty(name, value)`.
    fn set_global_state_property(&mut self, _name: &str, _value: &Value) -> Result<(), MapError> {
        not_ported("Style::setGlobalStateProperty")
    }
    /// `frontend.render(map)` (an exception makes the operation return
    /// false), then `frontend.getRenderer()->setFeatureState(sourceID,
    /// sourceLayer, featureID, state)`.
    fn set_feature_state(
        &mut self,
        _source_id: &str,
        _source_layer: Option<&str>,
        _feature_id: &str,
        _state: &feature::PropertyMap,
    ) -> Result<bool, MapError> {
        not_ported("setFeatureState operation (Renderer feature state)")
    }
    /// `frontend.render(map)`, then `getRenderer()->getFeatureState(..)`.
    fn get_feature_state(
        &mut self,
        _source_id: &str,
        _source_layer: Option<&str>,
        _feature_id: &str,
    ) -> Result<bool, MapError> {
        not_ported("getFeatureState operation (Renderer feature state)")
    }
    /// `frontend.render(map)`, then `getRenderer()->removeFeatureState(..)`.
    fn remove_feature_state(
        &mut self,
        _source_id: &str,
        _source_layer: Option<&str>,
        _feature_id: Option<&str>,
        _state_key: Option<&str>,
    ) -> Result<bool, MapError> {
        not_ported("removeFeatureState operation (Renderer feature state)")
    }
    /// The `sleep` operation: run the run loop until a `util::Timer` of
    /// `duration` fires.
    fn sleep(&mut self, _duration: Duration) -> Result<(), MapError> {
        not_ported("sleep operation (util::Timer + RunLoop::runOnce)")
    }
    fn set_tile_lod_min_radius(&mut self, _value: f64) -> Result<(), MapError> {
        not_ported("Map::setTileLodMinRadius")
    }
    fn set_tile_lod_scale(&mut self, _value: f64) -> Result<(), MapError> {
        not_ported("Map::setTileLodScale")
    }
    fn set_tile_lod_pitch_threshold(&mut self, _value: f64) -> Result<(), MapError> {
        not_ported("Map::setTileLodPitchThreshold")
    }
    fn set_tile_lod_zoom_shift(&mut self, _value: f64) -> Result<(), MapError> {
        not_ported("Map::setTileLodZoomShift")
    }
    fn set_tile_lod_mode(&mut self, _mode: TileLodMode) -> Result<(), MapError> {
        not_ported("Map::setTileLodMode")
    }
    /// `map.getStyle().getDefaultCamera()` (tile mode).
    fn default_camera(&mut self) -> Result<CameraOptions, MapError> {
        not_ported("Style::getDefaultCamera")
    }
    /// `util::tileCover(map.latLngBoundsForCamera(camera), zoom)`.
    fn tile_cover_for_camera(&mut self, _camera: &CameraOptions, _zoom: u8) -> Result<Vec<TileId>, MapError> {
        not_ported("Map::latLngBoundsForCamera + util::tileCover")
    }
    /// `frontend.getRenderer()->collectPlacedSymbolData(enable)`.
    fn collect_placed_symbol_data(&mut self, _enable: bool) {}
    /// `frontend.getRenderer()->getPlacedSymbolsData()`.
    fn get_placed_symbols_data(&mut self) -> Vec<PlacedSymbol> {
        Vec::new()
    }
    /// `frontend.getRenderer()->queryRenderedFeatures(geometry, options)`
    /// for a query test's point or box.
    fn query_rendered_features(
        &mut self,
        _geometry: &QueryGeometry,
        _options: &RenderedQueryOptions,
    ) -> Result<Vec<Feature>, MapError> {
        not_ported("Renderer::queryRenderedFeatures")
    }
}

/// `mln::PlacedSymbolData` (renderer.hpp), boxes as minX, minY, maxX, maxY.
#[derive(Clone, Debug, PartialEq)]
pub struct PlacedSymbol {
    /// Contents of the label (UTF-16)
    pub key: Vec<u16>,
    pub text_collision_box: Option<[f32; 4]>,
    pub icon_collision_box: Option<[f32; 4]>,
    pub text_placed: bool,
    pub icon_placed: bool,
    pub intersects_tile_border: bool,
    pub viewport_padding: f32,
    pub layer: String,
}

/// An `UnwrappedTileID` of the tile-mode cover.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TileId {
    pub z: u8,
    pub x: u32,
    pub y: u32,
    pub wrap: i16,
}

/// A query test's `queryGeometry`: a point or a box.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum QueryGeometry {
    Point(Point<f64>),
    Box(ScreenBox),
}

/// Creates the `TestRunner::Impl` for a test: `HeadlessFrontend(size,
/// pixelRatio, swapBehavior(mapMode))`, the file source, and
/// `Map(frontend, observer, MapOptions().withMapMode(..).withSize(..)
/// .withPixelRatio(..).withCrossSourceCollisions(..), resourceOptions)`, plus
/// `map.setBounds(BoundOptions().withMaxPitch(..))` when `maxPitch` is set.
pub type ImplFactory =
    Box<dyn Fn(&TestMetadata, &ResourceOptions, &ImplOptions) -> Result<Box<dyn TestRunnerImpl>, String>>;

/// Port-only: how the runner's options that C++ does not have set up a map.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImplOptions {
    /// `--tileWorkerCodec=<n>`: the map's tile workers are in n worker
    /// instances, reached through the worker codec (0: on the map's thread
    /// pool, as in C++).
    pub tile_worker_codec: usize,
}

/// `TestContext` (metadata.hpp).
pub struct TestContext<'a> {
    metadata: &'a mut TestMetadata,
    runner_impl: Option<&'a mut dyn TestRunnerImpl>,
    pub active_gfx_probe: GfxProbe,
    pub baseline_gfx_probe: GfxProbe,
    pub gfx_probe_active: bool,
}

impl<'a> TestContext<'a> {
    pub fn new(metadata: &'a mut TestMetadata) -> Self {
        TestContext {
            metadata,
            runner_impl: None,
            active_gfx_probe: GfxProbe::default(),
            baseline_gfx_probe: GfxProbe::default(),
            gfx_probe_active: false,
        }
    }

    /// A context whose `runnerImpl` is already set.
    pub fn with_impl(metadata: &'a mut TestMetadata, runner_impl: &'a mut dyn TestRunnerImpl) -> Self {
        TestContext { runner_impl: Some(runner_impl), ..TestContext::new(metadata) }
    }

    pub fn get_metadata(&mut self) -> &mut TestMetadata {
        self.metadata
    }

    /// `getFrontend()` / `getMap()` (C++ asserts `runnerImpl`).
    pub fn get_map(&mut self) -> &mut dyn TestRunnerImpl {
        &mut **self.runner_impl.as_mut().expect("runnerImpl")
    }

    /// For an uncaught C++ exception (`MapError::Exception`): errors the
    /// test instead of ending the run.
    pub fn fail_exception(&mut self, what: &str) -> bool {
        log::warning(Event::General, &format!("{}: uncaught exception: {what}", self.metadata.id));
        self.metadata.error_message = what.to_string();
        self.metadata.render_errored += 1;
        false
    }

    /// For operations the port cannot execute yet: logs, records the
    /// message and errors the test, so it can never pass by skipping work.
    pub fn fail_not_ported(&mut self, what: &str) -> bool {
        let message = format!("TODO(port): {what}");
        log::warning(Event::General, &format!("{}: {message}", self.metadata.id));
        self.metadata.error_message = message;
        self.metadata.render_errored += 1;
        false
    }
}

/// `TestOperation`: `std::function<bool(TestContext&)>`.
pub type TestOperation = Box<dyn Fn(&mut TestContext<'_>) -> bool>;
pub type TestOperations = Vec<TestOperation>;

/// `TestRunner::UpdateResults`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum UpdateResults {
    No,
    Default,
    Platform,
    Metrics,
    Rebaseline,
}

/// `TestRunner`.
pub struct TestRunner {
    maps: HashMap<String, Box<dyn TestRunnerImpl>>,
    manifest: Manifest,
    update_results: UpdateResults,
    /// Creates the map for a test; `None` without a map implementation.
    factory: Option<ImplFactory>,
    /// `--self-check`: the "rendered" image is the test's expected image.
    self_check: bool,
    /// Where actual.png and diff.png go. C++ writes them next to the test's
    /// style.json; the port never writes into the C++ tree.
    output_path: PathBuf,
    /// The per-test wall-clock limit (watchdog.rs).
    watchdog: Option<crate::watchdog::Watchdog>,
    /// What the factory sets up the maps with.
    impl_options: ImplOptions,
}

impl TestRunner {
    pub fn new(
        manifest: Manifest,
        update_results: UpdateResults,
        factory: Option<ImplFactory>,
        self_check: bool,
        output_path: PathBuf,
    ) -> Self {
        TestRunner {
            maps: HashMap::new(),
            manifest,
            update_results,
            factory,
            self_check,
            output_path,
            watchdog: None,
            impl_options: ImplOptions::default(),
        }
    }

    /// Sets what the factory sets up the maps with from now on.
    pub fn set_impl_options(&mut self, options: ImplOptions) {
        self.impl_options = options;
    }

    /// Errors a test that runs longer than `limit` (`None`: no limit).
    pub fn set_test_timeout(&mut self, limit: Option<Duration>) {
        self.watchdog = limit.map(crate::watchdog::Watchdog::new);
    }

    pub fn get_manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn do_shuffle(&mut self, seed: u32) {
        self.manifest.do_shuffle(seed);
    }

    pub fn reset(&mut self) {
        self.maps.clear();
    }

    fn test_output_path(&self, metadata: &TestMetadata, file: &str) -> PathBuf {
        self.output_path.join(&metadata.id).join(file)
    }

    /// `mln::util::write_file`, into the output directory. C++ throws on a
    /// failed write (uncaught); the port logs it.
    fn write_output(&self, metadata: &TestMetadata, file: &str, data: &[u8]) {
        let path = self.test_output_path(metadata, file);
        let result = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&path, data));
        if let Err(e) = result {
            log::error(Event::General, &format!("Failed to write file {}: {e}", path.display()));
        }
    }

    /// `checkQueryTestResults`.
    fn check_query_test_results(
        &self,
        actual_image: PremultipliedImage,
        features: Vec<Feature>,
        metadata: &mut TestMetadata,
    ) {
        let expectations = metadata.paths.expectations.clone();

        metadata.actual = encode_png(&actual_image);

        if actual_image.size.is_empty() {
            metadata.error_message = "Invalid size for actual image".to_string();
            metadata.render_errored += 1;
            return;
        }

        metadata.actual_json = to_json_features(&features);

        if metadata.actual_json.is_empty() {
            metadata.error_message = "Invalid size for actual JSON".to_string();
            metadata.render_errored += 1;
            return;
        }

        // The update modes are rejected when parsing arguments (the port
        // does not write baselines).
        debug_assert!(!matches!(self.update_results, UpdateResults::Platform | UpdateResults::Default));

        let actual_json = metadata.actual_json.clone();
        self.write_output(metadata, "actual.json", actual_json.as_bytes());

        let mut expected_json_paths = Vec::new();
        for dir in expectations.iter().rev() {
            if filesystem::exists(dir) {
                expected_json_paths = read_expected_json_entries(dir);
                if !expected_json_paths.is_empty() {
                    break;
                }
            }
        }

        if expected_json_paths.is_empty() {
            metadata.error_message = format!("Failed to find expectations for: {}", metadata.paths.style_path);
            metadata.render_errored += 1;
            return;
        }

        for entry in &expected_json_paths {
            let Ok(expected) = read_json(entry) else {
                metadata.error_message = format!("Failed to load expected JSON {entry}");
                metadata.render_errored += 1;
                return;
            };

            let Ok(actual) = serde_json::from_str::<serde_json::Value>(&metadata.actual_json) else {
                metadata.error_message = format!("Error parsing actual JSON for: {}", metadata.paths.style_path);
                metadata.render_errored += 1;
                return;
            };

            let actual_val = geojson::convert_value(&actual);
            let expected_val = geojson::convert_value(&expected);
            let equal = deep_equal(&actual_val, &expected_val);

            metadata.difference = if equal { 0.0 } else { 1.0 };
            if equal {
                metadata.diff = b"Match".to_vec();
            } else {
                metadata.diff = simple_diff(&actual_val, &expected_val).into_bytes();
                metadata.render_failed += 1;
            }
        }
    }

    /// `checkRenderTestResults`.
    pub fn check_render_test_results(&self, actual_image: PremultipliedImage, metadata: &mut TestMetadata) {
        let expectations = metadata.paths.expectations.clone();

        if metadata.outputs_image {
            metadata.actual = encode_png(&actual_image);

            if actual_image.size.is_empty() {
                metadata.error_message = "Invalid size for actual image".to_string();
                metadata.render_errored += 1;
                return;
            }

            // The update modes are rejected when parsing arguments (the port
            // does not write baselines).
            debug_assert!(!matches!(self.update_results, UpdateResults::Platform | UpdateResults::Default));

            self.write_output(metadata, "actual.png", &metadata.actual);

            let mut image_diff = PremultipliedImage::new(actual_image.size);

            let mut pixels: f64;
            let mut expected_images_paths = Vec::new();
            for dir in expectations.iter().rev() {
                if filesystem::exists(dir) {
                    expected_images_paths = read_expected_image_entries(dir);
                    if !expected_images_paths.is_empty() {
                        break;
                    }
                }
            }

            if expected_images_paths.is_empty() {
                metadata.error_message = format!("Failed to find expectations for: {}", metadata.paths.style_path);
                metadata.render_errored += 1;
                return;
            }

            for entry in &expected_images_paths {
                let Ok(maybe_expected_image) = std::fs::read(entry) else {
                    metadata.error_message = format!("Failed to load expected image {entry}");
                    metadata.render_errored += 1;
                    return;
                };

                // C++ `decodeImage` throws on a corrupt image (uncaught).
                let expected_image = match decode_image(&maybe_expected_image) {
                    Ok(image) => image,
                    Err(e) => {
                        metadata.error_message = format!("Failed to decode expected image {entry}: {e}");
                        metadata.render_errored += 1;
                        return;
                    }
                };
                metadata.expected = maybe_expected_image;

                let width = expected_image.size.width as usize;
                let height = expected_image.size.height as usize;
                // C++ compares `width * height` pixels of the expected image's
                // size against the actual image's buffer. When the expected
                // image has more pixels than the actual one, that reads and
                // writes out of bounds (undefined behaviour); the port counts
                // every pixel as different instead.
                if width * height * 4 <= actual_image.data.len() {
                    pixels = pixelmatch::pixelmatch(
                        &actual_image.data,
                        &expected_image.data,
                        width,
                        height,
                        Some(&mut image_diff.data),
                        0.1285, // Defined in GL JS
                        false,
                    ) as f64;
                } else {
                    log::warning(
                        Event::General,
                        &format!(
                            "{}: expected image {}x{} is larger than the actual image {}x{}",
                            metadata.id,
                            expected_image.size.width,
                            expected_image.size.height,
                            actual_image.size.width,
                            actual_image.size.height
                        ),
                    );
                    pixels = f64::from(expected_image.size.area());
                }

                metadata.diff = encode_png(&image_diff);

                self.write_output(metadata, "diff.png", &metadata.diff);

                metadata.difference = pixels / f64::from(expected_image.size.area());
                if metadata.difference <= metadata.allowed {
                    break;
                }
            }

            if metadata.difference > metadata.allowed {
                metadata.render_failed += 1;
            }
        }
    }

    /// `checkProbingResults`.
    pub fn check_probing_results(&self, result_metadata: &mut TestMetadata) {
        if result_metadata.metrics.is_empty() {
            return;
        }
        // TODO(port): writeMetrics (serializeMetrics) for the metrics and
        // rebaseline update modes; the port does not write baselines.
        let write_metrics = |metadata: &mut TestMetadata, message: &str| {
            let _ = message;
            let msg = "TODO(port): writing metrics.json is not supported";
            metadata.error_message += msg;
            log::warning(Event::General, msg);
        };

        let expected_metrics = result_metadata.paths.expected_metrics.clone();
        if self.update_results == UpdateResults::Metrics {
            write_metrics(result_metadata, " Updated expected metrics.");
            result_metadata.metrics_errored += 1;
            return;
        }

        // Check the possible paths in reverse order, so that the default path with
        // the test style will only be checked in the very end.
        let mut expected_metrics_paths = Vec::new();
        for dir in expected_metrics.iter().rev() {
            if filesystem::exists(dir) {
                expected_metrics_paths = read_expected_metric_entries(dir);
                if !expected_metrics_paths.is_empty() {
                    break;
                }
            }
        }

        // In case no metrics.json is found, skip assigning the expectedMetrics to
        // metadata, otherwise, take the first found metrics.
        if let Some(entry) = expected_metrics_paths.first() {
            let maybe_expected_metrics = read_expected_metrics(entry);
            if maybe_expected_metrics.is_empty() {
                result_metadata.error_message = format!("Failed to load expected metrics {entry}");
                result_metadata.metrics_errored += 1;
                return;
            }
            result_metadata.expected_metrics = maybe_expected_metrics;
        }

        if result_metadata.expected_metrics.is_empty() {
            result_metadata.error_message =
                format!("Failed to find metric expectations for: {}", result_metadata.paths.style_path);
            if self.update_results == UpdateResults::Rebaseline && !result_metadata.ignored_test {
                write_metrics(result_metadata, ". Created baseline for missing metrics.");
            }
            result_metadata.metrics_errored += 1;
            return;
        }

        fn append(metadata: &mut TestMetadata, s: &str) {
            if metadata.error_message.is_empty() {
                metadata.error_message += s;
            } else {
                metadata.error_message += &format!("\n{s}");
            }
        }

        // Check file size metrics.
        let check_file_size = |metadata: &mut TestMetadata| {
            if metadata.metrics.file_size.is_empty() {
                return;
            }
            for (key, expected) in metadata.expected_metrics.file_size.clone() {
                let Some(actual) = metadata.metrics.file_size.get(&key).cloned() else {
                    metadata.error_message = format!("Failed to find fileSize probe: {key}");
                    metadata.metrics_errored += 1;
                    return;
                };
                if actual.path != expected.path {
                    metadata.error_message = format!(
                        "Comparing different files at probe \"{key}\": {}, expected is {}.",
                        actual.path, expected.path
                    );
                    metadata.metrics_errored += 1;
                    return;
                }

                let result = check_value(expected.size as f32, actual.size as f32, actual.tolerance);
                if !result.0 {
                    let ss = format!(
                        "File size does not match at probe \"{key}\" for file \"{}\": {}, expected is {}.",
                        expected.path, actual.size, expected.size
                    );
                    append(metadata, &ss);
                    metadata.metrics_failed += 1;
                }
            }
        };
        let check_memory = |metadata: &mut TestMetadata| {
            if metadata.metrics.memory.is_empty() {
                return;
            }
            // Check memory metrics.
            for (key, expected) in metadata.expected_metrics.memory.clone() {
                let Some(actual) = metadata.metrics.memory.get(&key).cloned() else {
                    metadata.error_message = format!("Failed to find memory probe: {key}");
                    metadata.metrics_errored += 1;
                    return;
                };
                let mut error_stream = String::new();
                let (passed, delta) = MemoryProbe::check_peak(&expected, &actual);
                if !passed {
                    error_stream += &format!(
                        "Allocated memory peak size at probe \"{key}\" is {} bytes, expected is {}±{} bytes.",
                        actual.peak,
                        expected.peak,
                        ostream_float(f64::from(delta))
                    );
                    metadata.metrics_failed += 1;
                }

                let (passed, delta) = MemoryProbe::check_allocations(&expected, &actual);
                if !passed {
                    error_stream += &format!(
                        "Number of allocations at probe \"{key}\" is {}, expected is {}±{} allocations.",
                        actual.allocations,
                        expected.allocations,
                        ostream_float(f64::from(delta.round()))
                    );
                    metadata.metrics_failed += 1;
                }

                append(metadata, &error_stream);
            }
        };

        // Check network metrics.
        let check_network = |metadata: &mut TestMetadata| {
            if metadata.metrics.network.is_empty() {
                return;
            }
            for (key, expected) in metadata.expected_metrics.network.clone() {
                let Some(actual) = metadata.metrics.network.get(&key).cloned() else {
                    metadata.error_message = format!("Failed to find network probe: {key}");
                    metadata.metrics_errored += 1;
                    return;
                };
                let mut ss = String::new();
                if actual.requests != expected.requests {
                    ss += &format!(
                        "Number of requests at probe \"{key}\" is {}, expected is {}. ",
                        actual.requests, expected.requests
                    );
                    metadata.metrics_failed += 1;
                }
                if actual.transferred != expected.transferred {
                    ss += &format!(
                        "Transferred data at probe \"{key}\" is {} bytes, expected is {} bytes.",
                        actual.transferred, expected.transferred
                    );
                    metadata.metrics_failed += 1;
                }
                append(metadata, &ss);
            }
        };
        // Check fps metrics
        let check_fps = |metadata: &mut TestMetadata| {
            if metadata.metrics.fps.is_empty() {
                return;
            }
            for (key, expected) in metadata.expected_metrics.fps.clone() {
                let Some(actual) = metadata.metrics.fps.get(&key).cloned() else {
                    metadata.error_message = format!("Failed to find fps probe: {key}");
                    metadata.metrics_errored += 1;
                    return;
                };
                let mut ss = String::new();
                if !check_value(expected.average, actual.average, expected.tolerance).0 {
                    ss += &format!(
                        "Average fps at probe \"{key}\" is {}, expected to be {} with tolerance of {}",
                        ostream_float(f64::from(actual.average)),
                        ostream_float(f64::from(expected.average)),
                        ostream_float(f64::from(expected.tolerance))
                    );
                    metadata.metrics_failed += 1;
                }
                if !check_value(expected.min_one_pc, actual.min_one_pc, expected.tolerance).0 {
                    ss += &format!(
                        "Minimum(1%) fps at probe \"{key}\" is {}, expected to be {} with tolerance of {}",
                        ostream_float(f64::from(actual.min_one_pc)),
                        ostream_float(f64::from(expected.min_one_pc)),
                        ostream_float(f64::from(expected.tolerance))
                    );
                    metadata.metrics_failed += 1;
                }
                append(metadata, &ss);
            }
        };
        // Check gfx metrics
        let check_gfx = |metadata: &mut TestMetadata| {
            if metadata.metrics.gfx.is_empty() {
                return;
            }
            for (probe_name, expected_value) in metadata.expected_metrics.gfx.clone() {
                let Some(actual_value) = metadata.metrics.gfx.get(&probe_name).cloned() else {
                    metadata.error_message = format!("Failed to find gfx probe: {probe_name}");
                    metadata.metrics_errored += 1;
                    return;
                };
                let mut ss = String::new();
                let mut check = |metadata: &mut TestMetadata, differs: bool, text: String| {
                    if differs {
                        if !metadata.error_message.is_empty() {
                            ss.push('\n');
                        }
                        ss += &text;
                        metadata.metrics_failed += 1;
                    }
                };

                check(
                    metadata,
                    expected_value.num_draw_calls != actual_value.num_draw_calls,
                    format!(
                        "Number of draw calls at probe\"{probe_name}\" is {}, expected is {}",
                        actual_value.num_draw_calls, expected_value.num_draw_calls
                    ),
                );
                check(
                    metadata,
                    expected_value.num_textures != actual_value.num_textures,
                    format!(
                        "Number of textures at probe \"{probe_name}\" is {}, expected is {}",
                        actual_value.num_textures, expected_value.num_textures
                    ),
                );
                check(
                    metadata,
                    expected_value.num_buffers != actual_value.num_buffers,
                    format!(
                        "Number of vertex and index buffers at probe \"{probe_name}\" is {}, expected is {}",
                        actual_value.num_buffers, expected_value.num_buffers
                    ),
                );
                check(
                    metadata,
                    expected_value.num_frame_buffers != actual_value.num_frame_buffers,
                    format!(
                        "Number of frame buffers at probe \"{probe_name}\" is {}, expected is {}",
                        actual_value.num_frame_buffers, expected_value.num_frame_buffers
                    ),
                );
                check(
                    metadata,
                    expected_value.mem_textures.peak != actual_value.mem_textures.peak,
                    format!(
                        "Allocated texture memory peak size at probe \"{probe_name}\" is {} bytes, expected is {} bytes",
                        actual_value.mem_textures.peak, expected_value.mem_textures.peak
                    ),
                );
                check(
                    metadata,
                    expected_value.mem_index_buffers.peak != actual_value.mem_index_buffers.peak,
                    format!(
                        "Allocated index buffer memory peak size at probe \"{probe_name}\" is {} bytes, expected is {} bytes",
                        actual_value.mem_index_buffers.peak, expected_value.mem_index_buffers.peak
                    ),
                );
                check(
                    metadata,
                    expected_value.mem_vertex_buffers.peak != actual_value.mem_vertex_buffers.peak,
                    format!(
                        "Allocated vertex buffer memory peak size at probe \"{probe_name}\" is {} bytes, expected is {} bytes",
                        actual_value.mem_vertex_buffers.peak, expected_value.mem_vertex_buffers.peak
                    ),
                );

                append(metadata, &ss);
            }
        };

        check_file_size(result_metadata);
        check_memory(result_metadata);
        check_network(result_metadata);
        check_fps(result_metadata);
        check_gfx(result_metadata);

        if result_metadata.ignored_test {
            return;
        }

        if (result_metadata.metrics_errored != 0 || result_metadata.metrics_failed != 0)
            && self.update_results == UpdateResults::Rebaseline
        {
            write_metrics(result_metadata, " Rebaselined expected metric for failed test.");
        }
    }

    /// `appendLabelCutOffResults`.
    pub fn append_label_cut_off_results(
        result_metadata: &mut TestMetadata,
        cut_off_labels_report: &str,
        duplications_report: &str,
    ) {
        if result_metadata.label_cut_off_found != 0 {
            // Append label cut-off statistics.
            result_metadata.error_message += "\n Label cut-offs:";
            result_metadata.error_message += cut_off_labels_report;
            if result_metadata.duplications_count != 0 {
                result_metadata.error_message += "\n\n Label duplications:";
                result_metadata.error_message += duplications_report;
            }
        }
    }

    /// `TestRunner::run(metadata)`.
    pub fn run(&mut self, metadata: &mut TestMetadata) {
        // TODO(port): AllocationIndex::setActive(false); AllocationIndex::reset();
        // (no allocation index; its probes are TODO(port) operations).
        ProxyFileSource::set_tracking_active(false);

        let mut ctx = TestContext::new(metadata);
        // The per-test time limit, stopped when `run` returns.
        let _watchdog_guard = self.watchdog.as_ref().map(crate::watchdog::Watchdog::arm);

        if !ctx.metadata.ignored_test {
            for operation in get_before_operations(&self.manifest) {
                if !operation(&mut ctx) {
                    return;
                }
            }
        }

        let key = format!(
            "{}/{}/{}",
            ctx.metadata.map_mode as u32,
            ctx.metadata.pixel_ratio,
            u32::from(ctx.metadata.cross_source_collisions)
        );

        let mut cut_off_labels_report = String::new();
        let mut duplications_report = String::new();

        let result_image = if self.self_check {
            // The operations are parsed (with their parse-time effects on
            // the metadata) but not executed: there is no map.
            let _ = parse_test_operations(ctx.metadata);
            self_check_image(ctx.metadata)
        } else {
            if !self.maps.contains_key(&key) {
                let Some(factory) = &self.factory else {
                    ctx.metadata.error_message =
                        "TODO(port): no map implementation linked into this runner".to_string();
                    ctx.metadata.render_errored += 1;
                    return;
                };
                let resource_options = ResourceOptions {
                    cache_path: self.manifest.get_cache_path().to_string(),
                    api_key: self.manifest.get_api_key().to_string(),
                };
                let created =
                    catch_unwind(AssertUnwindSafe(|| factory(ctx.metadata, &resource_options, &self.impl_options)));
                match created {
                    Ok(Ok(runner_impl)) => {
                        self.maps.insert(key.clone(), runner_impl);
                    }
                    Ok(Err(message)) => {
                        ctx.metadata.error_message = format!("Failed to create the map: {message}");
                        ctx.metadata.render_errored += 1;
                        return;
                    }
                    Err(panic) => {
                        ctx.metadata.error_message = format!("Creating the map panicked: {}", panic_message(&panic));
                        ctx.metadata.render_errored += 1;
                        return;
                    }
                }
            }

            ctx.runner_impl = Some(self.maps.get_mut(&key).expect("inserted above").as_mut());
            // C++ catches exceptions only around frontend.render(); a Rust
            // panic anywhere in the map code errors this test instead of
            // ending the run, and drops the map, which may be inconsistent.
            let rendered = catch_unwind(AssertUnwindSafe(|| {
                render_with_map(&mut ctx, &mut cut_off_labels_report, &mut duplications_report)
            }));
            match rendered {
                Ok(image) => image,
                Err(panic) => {
                    ctx.runner_impl = None;
                    ctx.metadata.error_message = panic_error_message("Rendering panicked", &panic);
                    ctx.metadata.render_errored += 1;
                    drop_map(&mut self.maps, &key, &panic);
                    return;
                }
            }
        };

        if !ctx.metadata.ignored_test {
            // TODO(port): ctx.activeGfxProbe = GfxProbe(result.stats, ctx.activeGfxProbe);
            for operation in get_after_operations(&self.manifest) {
                if !operation(&mut ctx) {
                    return;
                }
            }
        }

        // C++ queries after the operations, on the frontend's renderer.
        let mut features = Vec::new();
        if !ctx.metadata.render_test && ctx.runner_impl.is_some() {
            // `queryGeometry[0]` and `[1]` are numbers: a point; else a box.
            let is_point = ctx
                .metadata
                .document
                .pointer("/metadata/test/queryGeometry/0")
                .is_some_and(serde_json::Value::is_number)
                && ctx
                    .metadata
                    .document
                    .pointer("/metadata/test/queryGeometry/1")
                    .is_some_and(serde_json::Value::is_number);
            let geometry = if is_point {
                QueryGeometry::Point(ctx.metadata.query_geometry)
            } else {
                QueryGeometry::Box(ctx.metadata.query_geometry_box)
            };
            let options = ctx.metadata.query_options.clone();
            let queried = catch_unwind(AssertUnwindSafe(|| ctx.get_map().query_rendered_features(&geometry, &options)));
            match queried {
                Ok(Ok(result)) => features = result,
                Ok(Err(MapError::NotPorted(what))) => {
                    ctx.fail_not_ported(&what);
                }
                Ok(Err(error)) => {
                    ctx.metadata.error_message = format!("{error:?}");
                    ctx.metadata.render_errored += 1;
                }
                Err(panic) => {
                    ctx.runner_impl = None;
                    ctx.metadata.error_message = panic_error_message("Query panicked", &panic);
                    ctx.metadata.render_errored += 1;
                    drop_map(&mut self.maps, &key, &panic);
                    return;
                }
            }
        }

        if metadata.render_test {
            if !metadata.ignore_probing {
                self.check_probing_results(metadata);
            }
            Self::append_label_cut_off_results(metadata, &cut_off_labels_report, &duplications_report);
            self.check_render_test_results(result_image, metadata);
        } else if metadata.render_errored == 0 {
            self.check_query_test_results(result_image, features, metadata);
        }
    }
}

/// The map part of `TestRunner::run`: `resetContext`, the default camera,
/// then `runTest` (or, in tile mode, one `runTest` per covering tile).
fn render_with_map(
    ctx: &mut TestContext<'_>,
    cut_off_labels_report: &mut String,
    duplications_report: &mut String,
) -> PremultipliedImage {
    let runner_impl = &mut **ctx.runner_impl.as_mut().expect("runnerImpl");
    if let Err(error) = runner_impl.reset_context(ctx.metadata) {
        return map_error_image(ctx, error);
    }

    if ctx.metadata.map_mode == MapMode::Tile {
        return render_tiles(ctx, cut_off_labels_report, duplications_report);
    }

    if let Err(error) = ctx.get_map().jump_to_default_camera() {
        return map_error_image(ctx, error);
    }
    run_test(ctx)
}

/// `getTileCenterCoordinates(tileId)`.
fn get_tile_center_coordinates(tile_id: &TileId) -> LatLng {
    let scale = f64::from(1u32 << tile_id.z);
    let tile_center = Point::new(
        (f64::from(tile_id.x) + 0.5) * mln_util::constants::TILE_SIZE_D,
        (f64::from(tile_id.y) + 0.5) * mln_util::constants::TILE_SIZE_D,
    );
    // Projection::unproject(tileCenter, scale) (LatLng::Unwrapped); tile
    // centers are always valid coordinates.
    mln_util::projection::Projection::unproject(&tile_center, scale, mln_util::geo::WrapMode::Unwrapped)
        .expect("tile center")
}

/// `getTileScreenPixelSize(pixelRatio)`.
fn get_tile_screen_pixel_size(pixel_ratio: f32) -> u32 {
    (mln_util::constants::TILE_SIZE_D * f64::from(pixel_ratio)) as u32
}

/// `getImageTileOffset(dims, dim, pixelRatio)`.
fn get_image_tile_offset(dims: &std::collections::BTreeSet<u32>, dim: u32, pixel_ratio: f32) -> u32 {
    match dims.iter().position(|&d| d == dim) {
        Some(index) => index as u32 * get_tile_screen_pixel_size(pixel_ratio),
        None => {
            debug_assert!(false);
            0
        }
    }
}

/// `util::toString(UnwrappedTileID)`.
fn tile_id_string(tile_id: &TileId) -> String {
    format!("{}/{}/{}{}{}", tile_id.z, tile_id.x, tile_id.y, if tile_id.wrap >= 0 { "+" } else { "" }, tile_id.wrap)
}

/// `SymbolLocationAndStatus`.
struct SymbolLocationAndStatus {
    tile_id: TileId,
    location: [f32; 4],
    placed: bool,
    is_icon: bool,
}

/// The `MapMode::Tile` branch of `TestRunner::run`: one `runTest` per tile
/// covering the default camera's view, each centered on its tile, copied
/// into one image, with the label cut-off and duplication reports.
fn render_tiles(
    ctx: &mut TestContext<'_>,
    cut_off_labels_report: &mut String,
    duplications_report: &mut String,
) -> PremultipliedImage {
    let camera = match ctx.get_map().default_camera() {
        Ok(camera) => camera,
        Err(error) => return map_error_image(ctx, error),
    };
    debug_assert!(camera.zoom.is_some());
    debug_assert!(camera.center.is_some());
    let zoom = camera.zoom.unwrap_or(0.0) as u8;
    let tile_ids = match ctx.get_map().tile_cover_for_camera(&camera, zoom) {
        Ok(tile_ids) => tile_ids,
        Err(error) => return map_error_image(ctx, error),
    };
    debug_assert!(!tile_ids.is_empty());
    let mut x_dims = std::collections::BTreeSet::new();
    let mut y_dims = std::collections::BTreeSet::new();
    for tile_id in &tile_ids {
        x_dims.insert(tile_id.x);
        y_dims.insert(tile_id.y);
        debug_assert!(tile_id.z == zoom);
    }
    let pixel_ratio = ctx.metadata.pixel_ratio;
    let tile_screen_size = get_tile_screen_pixel_size(pixel_ratio);

    let mut symbol_index: HashMap<Vec<u16>, Vec<SymbolLocationAndStatus>> = HashMap::new();

    let mut result = PremultipliedImage::new(Size::new(
        x_dims.len() as u32 * tile_screen_size,
        y_dims.len() as u32 * tile_screen_size,
    ));
    for tile_id in &tile_ids {
        let reset = {
            let runner_impl = &mut **ctx.runner_impl.as_mut().expect("runnerImpl");
            runner_impl.reset_context(ctx.metadata)
        };
        if let Err(error) = reset {
            return map_error_image(ctx, error);
        }
        ctx.get_map().collect_placed_symbol_data(true);
        let camera_for_tile = CameraOptions { center: Some(get_tile_center_coordinates(tile_id)), ..camera.clone() };
        if let Err(error) = ctx.get_map().jump_to(&camera_for_tile) {
            return map_error_image(ctx, error);
        }

        let result_for_tile = run_test(ctx);
        if !result_for_tile.valid() {
            // C++ returns from `run` here without checking the results (the
            // test then counts as passed); the port errors it.
            ctx.metadata.error_message = format!("Failed rendering tile: {}/{}/{}", tile_id.z, tile_id.x, tile_id.y);
            ctx.metadata.render_errored += 1;
            return PremultipliedImage::default();
        }
        let x_offset = get_image_tile_offset(&x_dims, tile_id.x, pixel_ratio);
        let y_offset = get_image_tile_offset(&y_dims, tile_id.y, pixel_ratio);

        let placed_symbols = ctx.get_map().get_placed_symbols_data();

        let mut find_cut_offs = |metadata: &mut TestMetadata,
                                 placed_symbol: &PlacedSymbol,
                                 mut bbox: [f32; 4],
                                 placed: bool,
                                 is_icon: bool| {
            // box.min.x += xOffset - viewportPadding (unsigned + float)
            let dx = x_offset as f32 - placed_symbol.viewport_padding;
            let dy = y_offset as f32 - placed_symbol.viewport_padding;
            bbox[0] += dx;
            bbox[2] += dx;
            bbox[1] += dy;
            bbox[3] += dy;

            let symbols = symbol_index.entry(placed_symbol.key.clone()).or_default();
            let Some(it) = symbols.iter().find(|a| a.is_icon == is_icon && a.location == bbox) else {
                symbols.push(SymbolLocationAndStatus { tile_id: *tile_id, location: bbox, placed, is_icon });
                return;
            };
            let key = String::from_utf16_lossy(&placed_symbol.key);
            let kind = if is_icon { "icon" } else { "text" };
            let location = format!(
                "(({}, {}), ({}, {}))",
                ostream_float(f64::from(bbox[0])),
                ostream_float(f64::from(bbox[1])),
                ostream_float(f64::from(bbox[2])),
                ostream_float(f64::from(bbox[3]))
            );
            if *tile_id == it.tile_id && is_icon == it.is_icon {
                metadata.duplications_count += 1;
                *duplications_report += &format!(
                    "\n{}. \"{key}\" {kind} {location} from layer: {} at {}",
                    metadata.duplications_count,
                    placed_symbol.layer,
                    tile_id_string(tile_id)
                );
                return;
            }
            if it.placed != placed && is_icon == it.is_icon {
                metadata.label_cut_off_found += 1;
                *cut_off_labels_report += &format!(
                    "\n{}. \"{key}\" {kind} {location} from layer: {}{}{} and{}{}",
                    metadata.label_cut_off_found,
                    placed_symbol.layer,
                    if placed { " is placed at " } else { " is not placed at " },
                    tile_id_string(tile_id),
                    if it.placed { " placed at " } else { " not placed at " },
                    tile_id_string(&it.tile_id)
                );
            }
        };

        for placed_symbol in &placed_symbols {
            if placed_symbol.intersects_tile_border {
                if let Some(text_box) = placed_symbol.text_collision_box {
                    find_cut_offs(ctx.metadata, placed_symbol, text_box, placed_symbol.text_placed, false);
                }
                if let Some(icon_box) = placed_symbol.icon_collision_box {
                    find_cut_offs(ctx.metadata, placed_symbol, icon_box, placed_symbol.icon_placed, true);
                }
            }
        }

        let size = result_for_tile.size;
        if let Err(e) = PremultipliedImage::copy(
            &result_for_tile,
            &mut result,
            Point::new(0, 0),
            Point::new(x_offset, y_offset),
            size,
        ) {
            ctx.metadata.error_message = format!("Failed to copy tile image: {e:?}");
            ctx.metadata.render_errored += 1;
            return PremultipliedImage::default();
        }
    }
    result
}

fn map_error_image(ctx: &mut TestContext<'_>, error: MapError) -> PremultipliedImage {
    match error {
        MapError::NotPorted(what) => {
            ctx.fail_not_ported(&what);
        }
        MapError::Exception(what) => {
            ctx.fail_exception(&what);
        }
        other => {
            ctx.metadata.error_message = format!("{other:?}");
            ctx.metadata.render_errored += 1;
        }
    }
    PremultipliedImage::default()
}

/// `TestRunner::runTest(metadata, ctx)`.
fn run_test(ctx: &mut TestContext<'_>) -> PremultipliedImage {
    let mut result = PremultipliedImage::default();
    for operation in parse_test_operations(ctx.get_metadata()) {
        if !operation(ctx) {
            return result;
        }
    }

    if ctx.metadata.outputs_image {
        match ctx.get_map().render() {
            Ok(image) => result = image,
            Err(e) => {
                // (sic) "Renering", as in C++.
                ctx.get_metadata().error_message = format!("Renering raised an exception: {e}");
            }
        }
    }
    result
}

/// `--self-check`: the image `checkRenderTestResults` compares first (the
/// first `expected*.png` of the last existing expectation directory). Without
/// one, a transparent image of the test's physical size, as a renderer
/// would return; the comparison then reports the missing expectation.
fn self_check_image(metadata: &TestMetadata) -> PremultipliedImage {
    for dir in metadata.paths.expectations.iter().rev() {
        if filesystem::exists(dir) {
            let entries = read_expected_image_entries(dir);
            if let Some(first) = entries.first() {
                if let Ok(image) = std::fs::read(first).map_err(|e| e.to_string()).and_then(|b| decode_image(&b)) {
                    return image;
                }
                break;
            }
        }
    }
    PremultipliedImage::new(Size::new(
        (metadata.size.width as f32 * metadata.pixel_ratio) as u32,
        (metadata.size.height as f32 * metadata.pixel_ratio) as u32,
    ))
}

/// Drops the map a test panicked in, which may be inconsistent. If the test
/// timed out, it first logs the map's state (`dumpDebugLogs`), so the output
/// says whether a tile never loaded or the frame never drew.
fn drop_map(maps: &mut HashMap<String, Box<dyn TestRunnerImpl>>, key: &str, panic: &Box<dyn std::any::Any + Send>) {
    let Some(map) = maps.remove(key) else {
        return;
    };
    if panic.downcast_ref::<crate::watchdog::TestTimedOut>().is_some() {
        // The dump reads a map that a panic left; it must not end the run.
        let _ = catch_unwind(AssertUnwindSafe(|| map.dump_debug_logs()));
    }
}

/// The error of a test that panicked: for a `TestTimedOut` (watchdog.rs),
/// the timeout message and what the test was waiting for, else
/// `"<what>: <panic message>"`.
fn panic_error_message(what: &str, panic: &Box<dyn std::any::Any + Send>) -> String {
    match panic.downcast_ref::<crate::watchdog::TestTimedOut>() {
        Some(timed_out) => format!("{}; {}", timed_out.message(), waiting_for(&ProxyFileSource::pending_requests())),
        None => format!("{what}: {}", panic_message(panic)),
    }
}

/// The requests a test that timed out was still waiting for, or that it
/// waited for none, which points at the map itself.
pub(crate) fn waiting_for(pending: &[(String, std::time::Duration)]) -> String {
    const SHOWN: usize = 5;
    if pending.is_empty() {
        return "no resource request was outstanding".to_string();
    }
    let mut list: Vec<String> =
        pending.iter().take(SHOWN).map(|(url, age)| format!("{url} ({:.1} s)", age.as_secs_f64())).collect();
    if pending.len() > SHOWN {
        list.push(format!("and {} more", pending.len() - SHOWN));
    }
    let plural = if pending.len() == 1 { "" } else { "s" };
    format!("waiting for {} request{plural}: {}", pending.len(), list.join(", "))
}

fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

/// `unsupportedOperation(operation)`: C++ sets the message and stops the
/// test without marking it errored (so it passes); kept as is.
fn unsupported_operation(operation: String) -> TestOperation {
    Box::new(move |ctx: &mut TestContext<'_>| {
        ctx.get_metadata().error_message = format!("Unsupported operation: {operation}");
        false
    })
}

/// `getBeforeOperations(manifest)`.
fn get_before_operations(manifest: &Manifest) -> TestOperations {
    let mut result: TestOperations = Vec::new();
    for probe in manifest.get_probes() {
        if probe == NETWORK_PROBE_OP {
            result.push(Box::new(|ctx: &mut TestContext<'_>| {
                assert!(!ProxyFileSource::is_tracking_active());
                ProxyFileSource::set_tracking_active(true);
                ctx.get_metadata().metrics.network.entry(format!("{NETWORK_PROBE_OP} - default - start")).or_insert(
                    NetworkProbe::new(ProxyFileSource::get_request_count(), ProxyFileSource::get_transferred_size()),
                );
                true
            }));
            continue;
        }
        if probe == MEMORY_PROBE_OP || probe == GFX_PROBE_OP {
            // TODO(port): the probeMemory (AllocationIndex) and probeGFX
            // (gfx::RenderingStats) probes.
            let what = format!("manifest probe {probe}");
            result.push(Box::new(move |ctx: &mut TestContext<'_>| ctx.fail_not_ported(&what)));
            continue;
        }
        result.push(unsupported_operation(probe.clone()));
    }
    result
}

/// `getAfterOperations(manifest)`.
fn get_after_operations(manifest: &Manifest) -> TestOperations {
    let mut result: TestOperations = Vec::new();
    for probe in manifest.get_probes() {
        if probe == NETWORK_PROBE_OP {
            result.push(Box::new(|ctx: &mut TestContext<'_>| {
                ctx.get_metadata().metrics.network.entry(format!("{NETWORK_PROBE_OP} - default - end")).or_insert(
                    NetworkProbe::new(ProxyFileSource::get_request_count(), ProxyFileSource::get_transferred_size()),
                );
                ProxyFileSource::set_tracking_active(false);
                true
            }));
            continue;
        }
        if probe == MEMORY_PROBE_OP || probe == GFX_PROBE_OP {
            // TODO(port): see get_before_operations.
            let what = format!("manifest probe {probe}");
            result.push(Box::new(move |ctx: &mut TestContext<'_>| ctx.fail_not_ported(&what)));
            continue;
        }
        result.push(unsupported_operation(probe.clone()));
    }
    result
}

/// `simpleDiff(result, expected)`: a line diff of the two values' JSON for
/// the report.
pub fn simple_diff(result: &Value, expected: &Value) -> String {
    let result_tokens = tokenize(&to_json_value(result));
    let expected_tokens = tokenize(&to_json_value(expected));
    let max_length = result_tokens.len().max(expected_tokens.len());
    let mut diff = String::new();

    diff += "<pre>\n";
    let flush = |vec: &[String], pos: usize, out: &mut String, separator: &str| {
        for item in &vec[pos..] {
            *out += &format!("{separator}{item}\n");
        }
    };

    for i in 0..max_length {
        if result_tokens.len() <= i {
            flush(&expected_tokens, i, &mut diff, "-");
            break;
        }

        if expected_tokens.len() <= i {
            flush(&result_tokens, i, &mut diff, "+");
            break;
        }

        if result_tokens[i] != expected_tokens[i] {
            diff += &format!("<b>-{}</b>\n", expected_tokens[i]);
            diff += &format!("<b>+{}</b>\n", result_tokens[i]);
        } else {
            diff += &format!("{}\n", result_tokens[i]);
        }
    }
    diff += "</pre>\n";
    diff
}

/// `std::ostream << float` with the default format (`%g`, precision 6).
pub fn ostream_float(v: f64) -> String {
    if v.is_nan() {
        return if v.is_sign_negative() { "-nan" } else { "nan" }.to_string();
    }
    if v.is_infinite() {
        return if v < 0.0 { "-inf" } else { "inf" }.to_string();
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0" } else { "0" }.to_string();
    }
    let scientific = format!("{v:.5e}");
    let (mantissa, exponent) = scientific.split_once('e').expect("exponent");
    let exponent: i32 = exponent.parse().expect("exponent");
    let trim = |s: &str| -> String {
        if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s.to_string() }
    };
    if !(-4..6).contains(&exponent) {
        format!("{}e{}{:02}", trim(mantissa), if exponent < 0 { '-' } else { '+' }, exponent.abs())
    } else {
        let decimals = (5 - exponent) as usize;
        trim(&format!("{v:.decimals$}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ostream_float_matches_printf_g() {
        let cases = [
            (0.0, "0"),
            (1.0, "1"),
            (0.5, "0.5"),
            (123456.0, "123456"),
            (1234567.0, "1.23457e+06"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (2.5e-7, "2.5e-07"),
            (1.23456789, "1.23457"),
            (100.0, "100"),
            (-42.25, "-42.25"),
            (999999.5, "1e+06"),
        ];
        for (v, expected) in cases {
            assert_eq!(ostream_float(v), expected, "{v}");
        }
    }
}
