// Ports test/tile/tile_lod.test.cpp.

mod support;

use std::f64::consts::PI;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};

use geojson::{Feature, Geometry, Point};
use mln_actor::RunLoop;
use mln_headless::HeadlessFrontend;
use mln_map::{CameraOptions, Map, MapDebugOptions, MapMode, MapOptions, NullMapObserver};
use mln_storage::{FileSourceManager, ResourceOptions};
use mln_style::PropertyValue;
use mln_style::layers::circle_layer::CircleLayer;
use mln_style::sources::custom_geometry_source::{CustomGeometrySource, Options as CustomGeometrySourceOptions};
use mln_util::Color;
use mln_util::geo::LatLngBounds;
use support::*;

/// `TileLODTest`.
struct TileLODTest {
    _run_loop: RunLoop,
    frontend: HeadlessFrontend,
    map: Map,
    tile_count: Arc<AtomicU32>,
}

impl TileLODTest {
    fn new(camera_options: &CameraOptions) -> Self {
        let run_loop = RunLoop::new();
        let frontend = HeadlessFrontend::with_pixel_ratio(1.0);
        let mut map = Map::with_resource_options(
            &frontend,
            Rc::new(NullMapObserver),
            MapOptions::new().with_map_mode(MapMode::Static).with_size(frontend.get_size()),
            &FileSourceManager::new(),
            &ResourceOptions::default()
                .with_cache_path(":memory:")
                .with_asset_path(cpp_root().join("test/fixtures/api/assets").to_str().unwrap()),
        )
        .unwrap();
        map.jump_to(camera_options);

        map.set_debug(MapDebugOptions::TILE_BORDERS);
        map.get_style_mut().load_json(&read_file("test/fixtures/api/empty.json"));

        let tile_count = Arc::new(AtomicU32::new(0));
        let handle = Arc::new(OnceLock::new());
        let counted = tile_count.clone();
        let options = CustomGeometrySourceOptions {
            fetch_tile_function: Some(answer_with(&handle, move |tile_id| {
                let center = LatLngBounds::from_canonical_tile(tile_id.z, tile_id.x, tile_id.y).center();
                counted.fetch_add(1, Ordering::SeqCst);
                vec![Feature::new(Geometry::Point(Point::new(center.longitude(), center.latitude())))]
            })),
            ..Default::default()
        };
        let source = CustomGeometrySource::new("custom", options).unwrap();
        handle.set(source.handle()).unwrap();
        map.get_style_mut().add_source(Box::new(source)).unwrap();

        let mut layer = CircleLayer::new("circles", "custom");
        layer.set_circle_color(&PropertyValue::Constant(Color::new(0.0, 0.0, 1.0, 0.0)));
        map.get_style_mut().add_layer(Box::new(layer), None).unwrap();

        TileLODTest { _run_loop: run_loop, frontend, map, tile_count }
    }

    fn render(&mut self) -> mln_headless::RenderResult {
        self.frontend.render(&mut self.map).expect("render")
    }

    fn tile_count(&self) -> u32 {
        self.tile_count.load(Ordering::SeqCst)
    }

    #[track_caller]
    fn check_image(&mut self, image: &str, expected_tile_count: u32) {
        self.check_images(&[image], expected_tile_count, 0.0002);
    }

    #[track_caller]
    fn check_images(&mut self, images: &[&str], expected_tile_count: u32, image_threshold: f64) {
        let result = self.render();

        // check this after frontend.render()
        assert_eq!(self.tile_count(), expected_tile_count);

        let bases: Vec<String> = images.iter().map(|image| format!("test/fixtures/tile_lod/{image}")).collect();
        let bases: Vec<&str> = bases.iter().map(String::as_str).collect();
        check_images(&bases, &result.image, image_threshold, 0.01);
    }
}

fn zoom(zoom: f64) -> CameraOptions {
    CameraOptions { zoom: Some(zoom), ..Default::default() }
}

fn pitch(pitch: f64) -> CameraOptions {
    CameraOptions { pitch: Some(pitch), ..Default::default() }
}

// gtest: TileLOD.disabled
#[test]
fn disabled() {
    let mut test = TileLODTest::new(&zoom(10.0));

    test.map.set_tile_lod_pitch_threshold(PI);

    test.check_image("disabled", 4);
}

// gtest: TileLOD.zoomShift
#[test]
fn zoom_shift() {
    let mut test = TileLODTest::new(&zoom(12.0));

    test.map.set_tile_lod_zoom_shift(4.0);
    test.render();
    assert_eq!(test.tile_count(), 64);

    test.tile_count.store(0, Ordering::SeqCst);

    test.map.set_tile_lod_zoom_shift(-4.0);
    test.render();
    assert_eq!(test.tile_count(), 4);
}

// gtest: TileLOD.pitchThreshold
#[test]
fn pitch_threshold() {
    let mut test = TileLODTest::new(&zoom(10.0));

    let threshold = 45.0;
    test.map.set_tile_lod_zoom_shift(4.0);
    test.map.set_tile_lod_min_radius(1.0);
    test.map.set_tile_lod_scale(1.0);
    test.map.set_tile_lod_pitch_threshold(threshold / 180.0 * PI);

    test.map.jump_to(&pitch(threshold - 1.0));
    test.render();
    assert_eq!(test.tile_count(), 140);
    test.tile_count.store(0, Ordering::SeqCst);

    test.map.jump_to(&pitch(threshold + 1.0));
    test.check_images(&["pitchThreshold-line", "pitchThreshold-polyline"], 22, 0.0015);
}

// gtest: TileLOD.scale
#[test]
fn scale() {
    let mut test = TileLODTest::new(&zoom(0.0));

    test.map.set_tile_lod_min_radius(1.0);
    test.map.set_tile_lod_scale(0.4);
    test.map.set_tile_lod_pitch_threshold(-1.0);

    test.map.set_tile_lod_zoom_shift(5.0);

    test.check_image("scale", 112);
}

// gtest: TileLOD.radius
#[test]
fn radius() {
    let mut test = TileLODTest::new(&zoom(0.0));

    test.map.set_tile_lod_min_radius(5.0);
    test.map.set_tile_lod_scale(1.0);
    test.map.set_tile_lod_pitch_threshold(-1.0);

    test.map.set_tile_lod_zoom_shift(5.0);

    test.check_image("radius", 172);
}
