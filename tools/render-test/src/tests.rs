// Tests of the runner port. The C++ runner has no unit tests of its own;
// these check the ported behaviour on the shared fixtures (testdata/, from
// the package directory `cargo test` runs them in) and on small generated
// suites with a mock TestRunnerImpl.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use mln_image::encode_png;
use mln_util::image::PremultipliedImage;
use mln_util::size::Size;

use crate::manifest_parser::ManifestParser;
use crate::metadata::{MapDebugOptions, MapMode, TestMetadata};
use crate::parser::{parse_test_metadata, parse_test_operations};
use crate::render_test::{Arguments, RunResult, parse_arguments, run_render_tests_with};
use crate::runner::{
    ImplFactory, ImplOptions, MapError, ResourceOptions, TestContext, TestRunner, TestRunnerImpl, UpdateResults,
};

const OUR_MANIFEST: &str = "manifests/linux-wgpu.json";
const CPP_MANIFEST: &str = "testdata/metrics/linux-opengl.json";
const INTEGRATION: &str = "testdata/metrics/integration";

const M1_FILTER: &str = "/render-tests/(background-color/(default|literal|function|colorSpace-hcl|colorSpace-lab|transition)|background-opacity/color|background-visibility/(none|visible))/style\\.json$";

fn style_paths(id: &str) -> crate::metadata::TestPaths {
    let dir = format!("{INTEGRATION}/{id}/");
    crate::metadata::TestPaths::new(format!("{dir}style.json"), vec![dir.clone()], vec![dir])
}

/// A fresh directory in the system's temporary directory.
fn tmp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("render-test").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(path: &Path, contents: impl AsRef<[u8]>) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn solid(size: Size, rgba: [u8; 4]) -> PremultipliedImage {
    let mut image = PremultipliedImage::new(size);
    for px in image.data.chunks_exact_mut(4) {
        px.copy_from_slice(&rgba);
    }
    image
}

// --- manifest_parser ---

#[test]
fn parses_our_manifest() {
    let manifest = ManifestParser::parse_manifest(OUR_MANIFEST, String::new()).unwrap();
    assert_eq!(manifest.get_manifest_path(), "manifests");
    assert_eq!(manifest.get_result_path(), "manifests");
    assert_eq!(manifest.get_test_root_path(), INTEGRATION);
    assert_eq!(manifest.get_cache_path(), "manifests/../testdata/metrics/cache-style.db");
    assert!(manifest.get_probes().is_empty());

    // Ignore files in manifest order, members in document order.
    let ignores = manifest.get_ignores();
    assert_eq!(
        ignores[0],
        (
            "expression-tests/collator/accent-equals-de".to_string(),
            "Locale-specific behavior changes based on platform.".to_string()
        )
    );
    assert!(ignores.contains(&(
        "render-tests/background-color/transition".to_string(),
        "https://github.com/mapbox/mapbox-gl-native/issues/10619".to_string()
    )));
    // The port's own ignore file is read last (its entries come and go).
    assert!(ignores.iter().any(|(id, _)| id.starts_with("query-tests/") || id.starts_with("render-tests/")));
    // M1 tests are not ignored by the port.
    assert!(!ignores.iter().any(|(id, _)| id == "render-tests/background-color/default"));

    // Every render and query test is found.
    let test_paths = manifest.get_test_paths();
    assert_eq!(test_paths.len(), 1356 + 126);
    let default = test_paths
        .iter()
        .find(|t| t.style_path.ends_with("/render-tests/background-color/default/style.json"))
        .unwrap();
    assert_eq!(default.style_path, format!("{INTEGRATION}/render-tests/background-color/default/style.json"));
    assert_eq!(
        default.expectations,
        vec![
            format!("{INTEGRATION}/render-tests/background-color/default/"),
            // platform-macos first: the runner checks the paths from the last
            // one, so a platform-linux baseline wins where both exist.
            "testdata/metrics/expectations/platform-macos/render-tests/background-color/default/".to_string(),
            "testdata/metrics/expectations/platform-linux/render-tests/background-color/default/".to_string(),
        ]
    );
    assert_eq!(
        default.expected_metrics,
        vec![
            format!("{INTEGRATION}/render-tests/background-color/default/"),
            "testdata/metrics/linux-gcc8-release/render-tests/background-color/default/".to_string(),
        ]
    );
}

#[test]
fn filters_tests_by_regex_search_on_the_path() {
    let manifest = ManifestParser::parse_manifest(OUR_MANIFEST, M1_FILTER.to_string()).unwrap();
    let mut ids: Vec<&str> =
        manifest.get_test_paths().iter().map(|t| t.style_path.strip_prefix(INTEGRATION).unwrap()).collect();
    ids.sort();
    assert_eq!(
        ids,
        vec![
            "/render-tests/background-color/colorSpace-hcl/style.json",
            "/render-tests/background-color/colorSpace-lab/style.json",
            "/render-tests/background-color/default/style.json",
            "/render-tests/background-color/function/style.json",
            "/render-tests/background-color/literal/style.json",
            "/render-tests/background-color/transition/style.json",
            "/render-tests/background-opacity/color/style.json",
            "/render-tests/background-visibility/none/style.json",
            "/render-tests/background-visibility/visible/style.json",
        ]
    );
}

#[test]
fn parses_the_cpp_linux_manifest() {
    let manifest = ManifestParser::parse_manifest(CPP_MANIFEST, "background-color".to_string()).unwrap();
    assert_eq!(manifest.get_manifest_path(), "testdata/metrics");
    assert_eq!(manifest.get_test_root_path(), INTEGRATION);
    assert_eq!(manifest.get_cache_path(), "testdata/metrics/cache-style.db");
    assert_eq!(manifest.get_probes().iter().collect::<Vec<_>>(), vec!["probeNetwork"]);
    assert_eq!(manifest.get_ignores().len(), 124 + 38);
    assert_eq!(manifest.get_test_paths().len(), 6);
}

#[test]
fn manifest_errors() {
    let dir = tmp_dir("manifest_errors");
    let integration = std::fs::canonicalize(INTEGRATION).unwrap();
    let integration = integration.to_str().unwrap();
    let parse = |name: &str, contents: &str| {
        let path = dir.join(name);
        write(&path, contents);
        ManifestParser::parse_manifest(path.to_str().unwrap(), String::new())
    };
    assert!(parse("not-json.json", "{").is_none());
    assert!(parse("result-path.json", r#"{"result_path": 1}"#).is_none());
    assert!(parse("missing-base.json", r#"{"base_test_path": "does-not-exist"}"#).is_none());
    assert!(parse("no-base.json", "{}").is_none());
    assert!(
        parse(
            "bad-expectation.json",
            &format!(r#"{{"base_test_path": "{integration}", "expectation_paths": ["nope"]}}"#)
        )
        .is_none()
    );
    assert!(
        parse("bad-ignores.json", &format!(r#"{{"base_test_path": "{integration}", "ignore_paths": "x"}}"#)).is_none()
    );
    assert!(parse("bad-probe.json", &format!(r#"{{"base_test_path": "{integration}", "probes": [1]}}"#)).is_none());

    // The manifest's own filter applies when none is given; a given one wins.
    let manifest = parse(
        "filter.json",
        &format!(
            r#"{{"base_test_path": "{integration}", "filter": "background-visibility/none/", "result_path": "."}}"#
        ),
    )
    .unwrap();
    assert_eq!(manifest.get_test_paths().len(), 1);
    assert_eq!(manifest.get_result_path(), dir.to_str().unwrap());
    let path = dir.join("filter.json");
    let manifest = ManifestParser::parse_manifest(path.to_str().unwrap(), "background-color/".to_string()).unwrap();
    assert_eq!(manifest.get_test_paths().len(), 6);
}

// --- parser: metadata ---

#[test]
fn parses_test_metadata() {
    let metadata = parse_test_metadata(&style_paths("render-tests/background-color/default"));
    assert_eq!(metadata.size, Size::new(64, 64));
    assert_eq!(metadata.pixel_ratio, 1.0);
    assert_eq!(metadata.allowed, 0.00015);
    assert_eq!(metadata.map_mode, MapMode::Static);
    assert!(metadata.render_test && metadata.outputs_image && metadata.ignore_probing);
    assert!(metadata.error_message.is_empty());
    assert_eq!(
        metadata.document_json,
        std::fs::read_to_string(format!("{INTEGRATION}/render-tests/background-color/default/style.json")).unwrap()
    );

    let overlay = parse_test_metadata(&style_paths("render-tests/background-opacity/overlay"));
    assert_eq!(overlay.size, Size::new(512, 256));

    let tile = parse_test_metadata(&style_paths("render-tests/symbol-placement/line-center-tile-map-mode"));
    assert_eq!(tile.map_mode, MapMode::Tile);
    assert_eq!(tile.size, Size::new(512, 512));
    assert_eq!(tile.debug, MapDebugOptions(MapDebugOptions::TILE_BORDERS.0 | MapDebugOptions::COLLISION.0));

    let retina = parse_test_metadata(&style_paths("render-tests/runtime-styling/image-add-1x-image-2x-screen"));
    assert_eq!(retina.pixel_ratio, 2.0);

    let allowed = parse_test_metadata(&style_paths("render-tests/text-pitch-alignment/viewport-text-depthtest"));
    assert_eq!(allowed.allowed, 0.00025);
    assert_eq!(allowed.size, Size::new(512, 256));

    let skew = parse_test_metadata(&style_paths("render-tests/projection/skew"));
    assert!(skew.axonometric);
    assert_eq!((skew.x_skew, skew.y_skew), (-1.0, -1.0));

    let query = parse_test_metadata(&style_paths("query-tests/circle-radius-features-in/inside"));
    assert!(!query.render_test);
    assert_eq!(query.query_geometry_box.min.x, 1.0);
    assert_eq!(query.query_geometry_box.max.y, 120.0);

    let missing = parse_test_metadata(&style_paths("render-tests/does-not-exist"));
    assert!(missing.document_object_empty());
    assert!(missing.error_message.starts_with("Unable to parse: "));
}

// --- parser + runner: operations, with a mock map ---

/// Records the calls; "renders" a solid image of `color` at the test's
/// physical size; knows one layer, "background", whose `background-color`
/// "white" turns the color white.
struct MockMap {
    calls: Rc<RefCell<Vec<String>>>,
    size: Size,
    color: [u8; 4],
}

impl MockMap {
    fn new(calls: Rc<RefCell<Vec<String>>>, color: [u8; 4]) -> Self {
        MockMap { calls, size: Size::default(), color }
    }
    fn log(&self, call: String) {
        self.calls.borrow_mut().push(call);
    }
}

impl TestRunnerImpl for MockMap {
    fn reset_context(&mut self, metadata: &TestMetadata) -> Result<(), MapError> {
        self.size = Size::new(
            (metadata.size.width as f32 * metadata.pixel_ratio) as u32,
            (metadata.size.height as f32 * metadata.pixel_ratio) as u32,
        );
        let style: serde_json::Value = serde_json::from_str(&metadata.document_json).unwrap();
        if let Some(color) = style.get("mock-color").and_then(|c| c.as_array()) {
            for (i, c) in color.iter().enumerate() {
                self.color[i] = c.as_u64().unwrap() as u8;
            }
        }
        self.log("reset".to_string());
        Ok(())
    }
    fn jump_to_default_camera(&mut self) -> Result<(), MapError> {
        self.log("camera".to_string());
        Ok(())
    }
    fn render(&mut self) -> Result<PremultipliedImage, String> {
        self.log("render".to_string());
        if self.color == [1, 2, 3, 4] {
            return Err("mock render failure".to_string());
        }
        Ok(solid(self.size, self.color))
    }
    fn set_layer_property(&mut self, layer: &str, name: &str, value: &serde_json::Value) -> Result<(), MapError> {
        self.log(format!("set {layer} {name} {value}"));
        if layer != "background" {
            return Err(MapError::LayerNotFound);
        }
        if name == "background-color" && value == "white" {
            self.color = [255, 255, 255, 255];
        }
        Ok(())
    }
}

fn run_operations(id: &str, map: &mut MockMap) -> (TestMetadata, bool) {
    let mut metadata = parse_test_metadata(&style_paths(id));
    let operations = parse_test_operations(&mut metadata);
    let mut ok = true;
    {
        let mut ctx = TestContext::with_impl(&mut metadata, map);
        for operation in &operations {
            if !operation(&mut ctx) {
                ok = false;
                break;
            }
        }
    }
    (metadata, ok)
}

#[test]
fn transition_operations_drive_the_map() {
    let calls = Rc::new(RefCell::new(Vec::new()));
    let mut map = MockMap::new(calls.clone(), [255, 0, 0, 255]);
    let (metadata, ok) = run_operations("render-tests/background-color/transition", &mut map);
    assert!(ok);
    assert!(metadata.error_message.is_empty());
    assert_eq!(*calls.borrow(), vec!["set background background-color \"white\"", "render", "render"]);
    assert_eq!(map.color, [255, 255, 255, 255]);
}

#[test]
fn unported_operations_error_the_test() {
    let calls = Rc::new(RefCell::new(Vec::new()));
    let mut map = MockMap::new(calls.clone(), [0, 0, 0, 255]);
    // addImage, addLayer, wait: addImage is not implemented by the mock.
    let (metadata, ok) = run_operations("render-tests/runtime-styling/image-add-1x-image-2x-screen", &mut map);
    assert!(!ok);
    assert_eq!(metadata.render_errored, 1);
    assert!(metadata.error_message.starts_with("TODO(port): "), "{}", metadata.error_message);
}

#[test]
fn unsupported_operation_drops_all_operations() {
    let mut metadata = TestMetadata {
        document: serde_json::json!({"metadata": {"test": {"operations": [["wait"], ["frobnicate"]]}}}),
        ..TestMetadata::default()
    };
    assert!(parse_test_operations(&mut metadata).is_empty());
    assert_eq!(metadata.error_message, "Unsupported operation: frobnicate");

    let mut pan = TestMetadata {
        document: serde_json::json!({"metadata": {"test": {"operations": [["panGesture", "x", 1, [0, 0, 1], [1, 1, 2]]]}}}),
        ..TestMetadata::default()
    };
    assert!(parse_test_operations(&mut pan).is_empty());
    assert_eq!(pan.error_message, "Map mode must be Continuous for panGesture operation");
}

// --- runner: checkRenderTestResults ---

fn check(dir: &Path, actual: PremultipliedImage, expectations: Vec<String>) -> TestMetadata {
    let output = dir.join("out");
    let manifest = ManifestParser::parse_manifest(OUR_MANIFEST, "background-color/default/".to_string()).unwrap();
    let runner = TestRunner::new(manifest, UpdateResults::No, None, false, output);
    let mut metadata = TestMetadata {
        id: "check".to_string(),
        paths: crate::metadata::TestPaths::new("style.json".to_string(), expectations, vec![]),
        ..TestMetadata::default()
    };
    runner.check_render_test_results(actual, &mut metadata);
    metadata
}

#[test]
fn compares_against_expectations_like_cpp() {
    let dir = tmp_dir("compare");
    let size = Size::new(8, 8);
    let red = solid(size, [255, 0, 0, 255]);
    let blue = solid(size, [0, 0, 255, 255]);
    let default_dir = dir.join("default/");
    let platform_dir = dir.join("platform/");
    write(&default_dir.join("expected.png"), encode_png(&red));
    let d = |p: &Path| format!("{}/", p.display());

    // Identical: passes, difference 0, diff.png and actual.png written.
    let m = check(&dir, red.clone(), vec![d(&default_dir)]);
    assert_eq!((m.render_failed, m.render_errored), (0, 0));
    assert_eq!(m.difference, 0.0);
    assert!(dir.join("out/check/actual.png").exists());
    assert!(dir.join("out/check/diff.png").exists());

    // Every pixel differs: fails with difference 1.
    let m = check(&dir, blue.clone(), vec![d(&default_dir)]);
    assert_eq!((m.render_failed, m.render_errored), (1, 0));
    assert_eq!(m.difference, 1.0);

    // One pixel of 64 differs: 1/64 > 0.00015 fails, unless `allowed` says so.
    let mut one_off = red.clone();
    one_off.data[0..4].copy_from_slice(&[0, 0, 255, 255]);
    let m = check(&dir, one_off.clone(), vec![d(&default_dir)]);
    assert_eq!(m.difference, 1.0 / 64.0);
    assert_eq!(m.render_failed, 1);

    // An alternate expected-*.png in the same directory can match.
    write(&default_dir.join("expected-blue.png"), encode_png(&blue));
    let m = check(&dir, blue.clone(), vec![d(&default_dir)]);
    assert_eq!((m.render_failed, m.render_errored), (0, 0));

    // A platform directory with expectations shadows the default one.
    write(&platform_dir.join("expected.png"), encode_png(&blue));
    let m = check(&dir, red.clone(), vec![d(&default_dir), d(&platform_dir)]);
    assert_eq!(m.render_failed, 1);
    let m = check(&dir, blue.clone(), vec![d(&default_dir), d(&platform_dir)]);
    assert_eq!(m.render_failed, 0);
    // A missing platform directory falls back to the default one.
    let m = check(&dir, red.clone(), vec![d(&default_dir), d(&dir.join("nope/"))]);
    assert_eq!(m.render_failed, 0);

    // No expectations at all.
    let m = check(&dir, red.clone(), vec![d(&dir.join("nope/"))]);
    assert_eq!(m.render_errored, 1);
    assert_eq!(m.error_message, "Failed to find expectations for: style.json");

    // An empty actual image.
    let m = check(&dir, PremultipliedImage::default(), vec![d(&default_dir)]);
    assert_eq!(m.render_errored, 1);
    assert_eq!(m.error_message, "Invalid size for actual image");

    // An expected image larger than the actual one (undefined behaviour in
    // C++) fails instead of reading out of bounds.
    let small_dir = dir.join("small/");
    write(&small_dir.join("expected.png"), encode_png(&solid(Size::new(16, 16), [255, 0, 0, 255])));
    let m = check(&dir, red, vec![d(&small_dir)]);
    assert_eq!((m.render_failed, m.difference), (1, 1.0));
}

// --- render_test: statuses and return codes over a generated suite ---

struct Suite {
    dir: PathBuf,
}

impl Suite {
    fn new(name: &str) -> Self {
        Suite { dir: tmp_dir(name) }
    }

    /// A test drawing `color` (see MockMap) with `expected` images in the
    /// test directory.
    fn test(&self, id: &str, color: [u8; 4], extra: &str, expected: &[(&str, [u8; 4])]) {
        let dir = self.dir.join("integration").join(id);
        let style = format!(
            r#"{{"version": 8, "mock-color": {color:?}, "metadata": {{"test": {{"width": 4, "height": 4{extra}}}}}, "sources": {{}}, "layers": [{{"id": "background", "type": "background"}}]}}"#
        );
        write(&dir.join("style.json"), style);
        for (name, rgba) in expected {
            write(&dir.join(name), encode_png(&solid(Size::new(4, 4), *rgba)));
        }
    }

    fn run(&self, ignores: &str, filter: &str) -> RunResult {
        write(&self.dir.join("ignores.json"), ignores);
        let integration = self.dir.join("integration");
        let manifest = format!(
            r#"{{"base_test_path": "{}", "expectation_paths": ["{}"], "ignore_paths": ["{}"]}}"#,
            integration.display(),
            self.dir.join("platform").display(),
            self.dir.join("ignores.json").display()
        );
        std::fs::create_dir_all(self.dir.join("platform")).unwrap();
        let manifest_path = self.dir.join("suite.json");
        write(&manifest_path, manifest);
        let args: Vec<String> = [
            "render-test",
            "-p",
            manifest_path.to_str().unwrap(),
            "--filter",
            filter,
            "--output",
            self.dir.join("out").to_str().unwrap(),
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let arguments: Arguments = parse_arguments(&args).unwrap();
        run_render_tests_with(&arguments, Some(mock_factory()))
    }
}

fn mock_factory() -> ImplFactory {
    Box::new(|_metadata: &TestMetadata, _options: &ResourceOptions, _impl_options: &ImplOptions| {
        Ok(Box::new(MockMap::new(Rc::new(RefCell::new(Vec::new())), [0, 0, 0, 0])) as Box<dyn TestRunnerImpl>)
    })
}

fn status<'a>(result: &'a RunResult, id: &str) -> &'a str {
    &result.metadatas.iter().find(|m| m.id == id).unwrap_or_else(|| panic!("{id} did not run")).status
}

const RED: [u8; 4] = [255, 0, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];

#[test]
fn statuses_and_summary() {
    let suite = Suite::new("statuses");
    suite.test("pass", RED, "", &[("expected.png", RED)]);
    suite.test("fail", RED, "", &[("expected.png", BLUE)]);
    suite.test("alternate", RED, "", &[("expected.png", BLUE), ("expected-red.png", RED)]);
    suite.test("noexpected", RED, "", &[]);
    suite.test("render-error", [1, 2, 3, 4], "", &[("expected.png", RED)]);
    suite.test(
        "op-layer-not-found",
        RED,
        r#", "operations": [["setPaintProperty", "nope", "background-color", "red"]]"#,
        &[("expected.png", RED)],
    );
    suite.test(
        "op-transition",
        RED,
        r#", "operations": [["setPaintProperty", "background", "background-color", "white"], ["wait"]]"#,
        &[("expected.png", [255, 255, 255, 255])],
    );
    suite.test("op-todo", RED, r#", "operations": [["probeMemoryStart"]]"#, &[("expected.png", RED)]);
    suite.test("tile", RED, r#", "mapMode": "tile""#, &[("expected.png", RED)]);
    suite.test("ignored-pass", RED, "", &[("expected.png", RED)]);
    suite.test("ignored-fail", RED, "", &[("expected.png", BLUE)]);
    suite.test("skipped", RED, "", &[("expected.png", BLUE)]);
    write(&suite.dir.join("integration/bad-json/style.json"), "{");
    let ignores = r#"{"ignored-pass": "reason a", "ignored-fail": "reason b", "skipped": "skip - reason c"}"#;
    let result = suite.run(ignores, "");

    assert_eq!(status(&result, "pass"), "passed");
    assert_eq!(status(&result, "fail"), "failed");
    assert_eq!(status(&result, "alternate"), "passed");
    assert_eq!(status(&result, "noexpected"), "errored");
    assert_eq!(status(&result, "render-error"), "errored");
    assert_eq!(status(&result, "op-layer-not-found"), "errored");
    assert_eq!(status(&result, "op-transition"), "passed");
    assert_eq!(status(&result, "op-todo"), "errored");
    assert_eq!(status(&result, "tile"), "errored");
    assert_eq!(status(&result, "bad-json"), "errored");
    assert_eq!(status(&result, "ignored-pass"), "ignored passed");
    assert_eq!(status(&result, "ignored-fail"), "ignored failed");
    assert!(!result.metadatas.iter().any(|m| m.id == "skipped"));

    let render_error = result.metadatas.iter().find(|m| m.id == "render-error").unwrap();
    // C++ overwrites the exception message with the empty-image error.
    assert_eq!(render_error.error_message, "Invalid size for actual image");

    let stats = result.stats;
    assert_eq!(
        (
            stats.passed_tests,
            stats.failed_tests,
            stats.errored_tests,
            stats.ignore_passed_tests,
            stats.ignore_failed_tests
        ),
        (3, 1, 6, 1, 1)
    );
    // The last non-passing, non-ignored test decides: 2 (errored) or 3 (failed).
    assert!(result.return_code == 2 || result.return_code == 3);
    let html = std::fs::read_to_string(suite.dir.join("out/suite.html")).unwrap();
    assert!(html.contains("7 tests failed."));
    assert!(html.contains("1 ignored failed, 1 ignored passed, 6 errored, 1 failed, 3 passed."));
    assert!(suite.dir.join("out/pass/actual.png").exists());
}

#[test]
fn return_codes() {
    let suite = Suite::new("return_codes");
    suite.test("a-pass", RED, "", &[("expected.png", RED)]);
    suite.test("b-fail", RED, "", &[("expected.png", BLUE)]);
    suite.test("c-error", RED, "", &[]);
    suite.test("d-ignored", RED, "", &[("expected.png", BLUE)]);
    let ignores = r#"{"d-ignored": "reason"}"#;
    assert_eq!(suite.run(ignores, "a-pass/").return_code, 0);
    assert_eq!(suite.run(ignores, "b-fail/").return_code, 3);
    assert_eq!(suite.run(ignores, "c-error/").return_code, 2);
    // Ignored failures do not fail the run.
    assert_eq!(suite.run(ignores, "(a-pass|d-ignored)/").return_code, 0);
}

#[test]
fn source_replace_reruns_with_swapped_sources() {
    let suite = Suite::new("source_replace");
    let dir = suite.dir.join("integration/replace");
    // The mock reads "mock-color" from the style it is given; the swap is
    // visible in the JSON it receives.
    write(
        &dir.join("style.json"),
        r#"{"version": 8, "metadata": {"test": {"width": 4, "height": 4, "source-replace": [{"replace": "a", "with": "b"}]}}, "sources": {"a": {"type": "geojson"}, "b": {"type": "vector"}}, "layers": []}"#,
    );
    write(&dir.join("expected.png"), encode_png(&solid(Size::new(4, 4), [0, 0, 0, 0])));
    let result = suite.run("{}", "");
    assert_eq!(status(&result, "replace"), "passed");
    let metadata = &result.metadatas[0];
    // Restored afterwards, in document order.
    assert_eq!(metadata.document["sources"]["a"]["type"], "geojson");
    let keys: Vec<&String> = metadata.document["sources"].as_object().unwrap().keys().collect();
    assert_eq!(keys, ["a", "b"]);
    assert!(metadata.document_json.contains(r#""a": {"type": "geojson"}"#));
}

#[test]
fn argument_parsing() {
    let args = |list: &[&str]| -> Vec<String> {
        std::iter::once("render-test").chain(list.iter().copied()).map(String::from).collect()
    };
    let parsed = parse_arguments(&args(&["-p", OUR_MANIFEST, "-f", "x", "-r", "--seed=7"])).unwrap();
    assert_eq!(parsed.manifest_path, OUR_MANIFEST);
    assert_eq!(parsed.test_filter, "x");
    assert!(parsed.recycle_map && !parsed.shuffle && !parsed.self_check);
    assert_eq!(parsed.seed, 7);
    let parsed = parse_arguments(&args(&[&format!("--manifestPath={OUR_MANIFEST}"), "--self-check"])).unwrap();
    assert!(parsed.self_check);
    assert_eq!(parse_arguments(&args(&["-h"])), Err(0));
    assert_eq!(parse_arguments(&args(&["--bogus"])), Err(1));
    assert_eq!(parse_arguments(&args(&[])), Err(2));
    assert_eq!(parse_arguments(&args(&["-p", "nope.json"])), Err(3));
    assert_eq!(parse_arguments(&args(&["-p", OUR_MANIFEST, "-u", "nonsense"])), Err(1));
    // Not ported: baselines are never written.
    assert_eq!(parse_arguments(&args(&["-p", OUR_MANIFEST, "-u", "platform"])), Err(1));
    assert!(parse_arguments(&args(&["-p", OUR_MANIFEST, "-s"])).unwrap().shuffle);
    // --jobs (jobs.rs).
    assert_eq!(parse_arguments(&args(&["-p", OUR_MANIFEST])).unwrap().jobs, 1);
    assert_eq!(parse_arguments(&args(&["-p", OUR_MANIFEST, "--jobs=4"])).unwrap().jobs, 4);
    assert_eq!(parse_arguments(&args(&["-p", OUR_MANIFEST, "--jobs", "0"])).unwrap().jobs, 0);
    assert_eq!(parse_arguments(&args(&["-p", OUR_MANIFEST, "--jobs=many"])), Err(1));
}

// A test that times out reports the proxy's requests that are neither
// answered nor cancelled (runner::waiting_for), so a hang says what it
// waited for.
#[test]
fn pending_requests_leave_when_answered_or_cancelled() {
    use crate::file_source::{PendingRequest, ProxyFileSource, answered, track};
    struct Request;
    impl mln_storage::AsyncRequest for Request {}
    let is_pending = |url: &str| ProxyFileSource::pending_requests().iter().any(|(pending, _)| pending == url);

    let number = track("http://tiles/answered.mvt");
    assert!(is_pending("http://tiles/answered.mvt"));
    answered(number);
    assert!(!is_pending("http://tiles/answered.mvt"));

    let request = PendingRequest { number: track("http://tiles/cancelled.mvt"), _request: Box::new(Request) };
    assert!(is_pending("http://tiles/cancelled.mvt"));
    drop(request);
    assert!(!is_pending("http://tiles/cancelled.mvt"));
}

#[test]
fn timeout_message_lists_the_pending_requests() {
    use std::time::Duration;

    use crate::runner::waiting_for;
    assert_eq!(waiting_for(&[]), "no resource request was outstanding");
    assert_eq!(
        waiting_for(&[("http://tiles/14-8802-5373.mvt".to_string(), Duration::from_millis(59_940))]),
        "waiting for 1 request: http://tiles/14-8802-5373.mvt (59.9 s)"
    );
    let many: Vec<_> = (0..7).map(|i| (format!("http://tiles/{i}.mvt"), Duration::from_secs(1))).collect();
    assert!(waiting_for(&many).starts_with("waiting for 7 requests: http://tiles/0.mvt (1.0 s), "));
    assert!(waiting_for(&many).ends_with("http://tiles/4.mvt (1.0 s), and 2 more"));
}
