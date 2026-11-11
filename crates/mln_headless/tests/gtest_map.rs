// Ports test/map/map.test.cpp (the `Map` and `BackgroundLayer` gtests).
// test/map/prefetch.test.cpp and test/map/map_snapshotter.test.cpp are the
// modules below; the fixture (`MapTest`, `StubMapObserver`, `checkImage`,
// `FakeFileSource`) is in support/mod.rs.
//
// Markers: `// gtest: Group.Name` above each ported test; tests that need
// code that is not ported yet are `#[ignore]`d with the missing part.

mod gtest_map_snapshotter;
mod gtest_prefetch;
mod support;

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, OnceLock};

use feature::Value;
use geojson::{Feature, GeoJSON, Geometry, Point};
use mln_actor::{RunLoop, Timer};
use mln_gfx::{BackendScope, ScopeType};
use mln_headless::HeadlessFrontend;
use mln_map::{
    AnimationOptions, BoundOptions, CameraOptions, ConstrainMode, Map, MapMode, MapOptions, MapProjection,
    NorthOrientation, ProjectionMode, RenderFrameStatus, RenderMode, TileLodMode, ViewportMode,
};
use mln_renderer::{OverscaledTileID, RendererFrontend, RendererObserver, TileOperation, UpdateParameters};
use mln_storage::network_status::{NetworkStatus, Status as NetworkStatusType};
use mln_storage::response::{Error as ResponseError, Reason};
use mln_storage::stub_file_source::{ResponseType, StubFileSource};
use mln_storage::{
    FileSource, FileSourceManager, FileSourceType, Kind, LoadingMethod, MainResourceLoader, OnlineFileSource, Resource,
    ResourceOptions, Response, StoragePolicy,
};
use mln_style::conversion::layer::convert_layer;
use mln_style::conversion::{Convertible, Error as ConversionError};
use mln_style::custom_tile_loader::TileFunction;
use mln_style::layer::Layer;
use mln_style::layers::background_layer::BackgroundLayer;
use mln_style::layers::fill_layer::FillLayer;
use mln_style::layers::line_layer::LineLayer;
use mln_style::layers::location_indicator_layer::LocationIndicatorLayer;
use mln_style::layers::raster_layer::RasterLayer;
use mln_style::layers::symbol_layer::SymbolLayer;
use mln_style::property_expression::PropertyExpression;
use mln_style::sources::custom_geometry_source::{
    CustomGeometrySource, CustomGeometrySourceHandle, Options as CustomGeometrySourceOptions,
};
use mln_style::sources::geojson_source::{GeoJSONOptions, GeoJSONSource};
use mln_style::sources::image_source::ImageSource;
use mln_style::sources::tile_source::{UrlOrTileset, VectorSource};
use mln_style::sprite::Sprite;
use mln_style::style_property::Kind as StylePropertyKind;
use mln_style::{PropertyValue, TransitionOptions, source};
use mln_style_types::VisibilityType;
use mln_test_server::HttpServer;
use mln_text::glyph::{GlyphIDType, GlyphRange};
use mln_tile_id::CanonicalTileID;
use mln_util::Color;
use mln_util::chrono::{Duration, Timestamp};
use mln_util::geo::{EdgeInsets, LatLng, LatLngBounds, ScreenCoordinate, WrapMode};
use mln_util::image::PremultipliedImage;
use mln_util::logging::{Event, EventSeverity};
use mln_util::size::Size;
use mln_util::tileset::{Scheme, Tileset, VectorEncoding};
use support::*;

fn ll(lat: f64, lon: f64) -> LatLng {
    LatLng::new(lat, lon).expect("LatLng")
}

fn cam() -> CameraOptions {
    CameraOptions::default()
}

fn insets(top: f64, left: f64, bottom: f64, right: f64) -> EdgeInsets {
    EdgeInsets::new(top, left, bottom, right).expect("EdgeInsets")
}

/// `std::make_unique<style::Image>(id, image, pixelRatio)`.
pub fn style_image(id: &str, image: PremultipliedImage, pixel_ratio: f32) -> Arc<mln_style::image::Impl> {
    Arc::new(
        mln_style::image::Impl::new(id.to_owned(), image, pixel_ratio, false, Vec::new(), Vec::new(), None, None, None)
            .expect("style::Image"),
    )
}

fn red_background(id: &str) -> Box<BackgroundLayer> {
    let mut layer = BackgroundLayer::new(id);
    layer.set_background_color(&PropertyValue::Constant(Color::new(1.0, 0.0, 0.0, 1.0)));
    Box::new(layer)
}

fn vector_source(id: &str, tiles: &str) -> VectorSource {
    VectorSource::new(
        id,
        UrlOrTileset::Tileset(Tileset { tiles: vec![tiles.to_owned()], ..Tileset::default() }),
        None,
        None,
        VectorEncoding::Mapbox,
    )
}

fn no_content() -> Option<Response> {
    let mut res = Response::new();
    res.no_content = true;
    Some(res)
}

// gtest: Map.RendererState
#[test]
fn renderer_state() {
    let test = MapTest::new();

    // Map hasn't notified the frontend about an update yet.
    let null_options = CameraOptions::default();
    assert_eq!(test.frontend.get_camera_options(), null_options);

    let coordinate = ll(1.0, 1.0);
    let zoom = 12.0;
    let pitch_in_degrees = 45.0;
    let bearing_in_degrees = 30.0;

    test.load_json(&read_file("test/fixtures/api/empty.json"));
    test.map().jump_to(
        &cam()
            .with_center(Some(coordinate))
            .with_zoom(Some(zoom))
            .with_pitch(Some(pitch_in_degrees))
            .with_bearing(Some(bearing_in_degrees)),
    );

    test.run_loop.run_once();
    test.render();

    // RendererState::getCameraOptions
    let options = test.frontend.get_camera_options();
    assert_near(options.center.unwrap().latitude(), coordinate.latitude(), 1e-7);
    assert_near(options.center.unwrap().longitude(), coordinate.longitude(), 1e-7);
    assert_double_eq(options.zoom.unwrap(), zoom);
    assert_double_eq(options.pitch.unwrap(), pitch_in_degrees);
    assert_near(options.bearing.unwrap(), bearing_in_degrees, 1e-7);

    {
        let lat_lng = test.frontend.lat_lng_for_pixel(&ScreenCoordinate::new(0.0, 0.0)).unwrap();
        let point = test.frontend.pixel_for_lat_lng(&coordinate);
        assert_near(coordinate.latitude(), lat_lng.latitude(), 1e-1);
        assert_near(coordinate.longitude(), lat_lng.longitude(), 1e-1);
        let size = test.map().get_map_options().size();
        assert_near(point.x, f64::from(size.width) / 2.0, 1e-7);
        assert_near(point.y, f64::from(size.height) / 2.0, 1e-7);
    }

    // RendererState::hasImage
    test.map().get_style_mut().add_image(style_image(
        "default_marker",
        decode_image("test/fixtures/sprites/default_marker.png"),
        1.0,
    ));

    // The frontend has not yet been notified about the newly-added image.
    assert!(!test.frontend.has_image("default_marker"));

    test.run_loop.run_once();
    test.render();

    assert!(test.frontend.has_image("default_marker"));

    // RendererState::hasSource
    let source = GeoJSONSource::with_default_options("GeoJSONSource");
    source.set_geojson(&GeoJSON::Geometry(Geometry::Point(Point::new(0.0, 0.0)))).unwrap();
    test.map().get_style_mut().add_source(Box::new(source)).unwrap();

    // The frontend has not yet been notified about the newly-added source.
    assert!(!test.frontend.has_source("GeoJSONSource"));

    test.run_loop.run_once();
    test.render();

    assert!(test.frontend.has_source("GeoJSONSource"));

    // RendererState::hasLayer
    test.map().get_style_mut().add_layer(Box::new(SymbolLayer::new("SymbolLayer", "GeoJSONSource")), None).unwrap();

    // The frontend has not yet been notified about the newly-added source.
    assert!(!test.frontend.has_layer("SymbolLayer"));

    test.run_loop.run_once();
    test.render();

    assert!(test.frontend.has_layer("SymbolLayer"));
}

// gtest: Map.LatLngBehavior
#[test]
fn lat_lng_behavior() {
    let test = MapTest::new();

    test.map().jump_to(&cam().with_center(Some(ll(1.0, 1.0))).with_zoom(Some(0.0)));
    let lat_lng1 = test.map().get_camera_options(None).center.unwrap();

    test.map().jump_to(&cam().with_center(Some(ll(1.0, 1.0))));
    let lat_lng2 = test.map().get_camera_options(None).center.unwrap();

    assert_double_eq(lat_lng1.latitude(), lat_lng2.latitude());
    assert_double_eq(lat_lng1.longitude(), lat_lng2.longitude());
}

fn new_york_test() -> (MapTest, LatLngBounds) {
    let test = MapTest::new();
    test.map().jump_to(&cam().with_center(Some(ll(40.712730, -74.005953))).with_zoom(Some(16.0)));
    let bounds = LatLngBounds::hull(ll(15.68169, 73.499857), ll(53.560711, 134.77281));
    (test, bounds)
}

// gtest: Map.LatLngBoundsToCamera
#[test]
fn lat_lng_bounds_to_camera() {
    let (test, bounds) = new_york_test();

    let virtual_camera = test.map().camera_for_lat_lng_bounds(&bounds, &EdgeInsets::default(), None, None).unwrap();
    assert!(bounds.contains(&virtual_camera.center.unwrap()));
    assert_near(virtual_camera.zoom.unwrap(), 1.55467, 1e-5);
}

// gtest: Map.LatLngBoundsToCameraWithExcessivePadding
#[test]
fn lat_lng_bounds_to_camera_with_excessive_padding() {
    let (test, bounds) = new_york_test();

    let virtual_camera =
        test.map().camera_for_lat_lng_bounds(&bounds, &insets(500.0, 0.0, 1200.0, 0.0), None, None).unwrap();
    assert!(bounds.contains(&virtual_camera.center.unwrap()));
    assert_near(virtual_camera.zoom.unwrap(), 16.0, 1e-5);
}

// gtest: Map.LatLngBoundsToCameraWithBearing
#[test]
fn lat_lng_bounds_to_camera_with_bearing() {
    let (test, bounds) = new_york_test();

    let virtual_camera =
        test.map().camera_for_lat_lng_bounds(&bounds, &EdgeInsets::default(), Some(35.0), None).unwrap();
    assert!(bounds.contains(&virtual_camera.center.unwrap()));
    assert_near(virtual_camera.zoom.unwrap(), 1.21385, 1e-5);
    assert_near(virtual_camera.bearing.unwrap_or(0.0), 35.0, 1e-5);
}

// gtest: Map.LatLngBoundsToCameraWithBearingPitchAndPadding
#[test]
fn lat_lng_bounds_to_camera_with_bearing_pitch_and_padding() {
    let (test, bounds) = new_york_test();

    let virtual_camera =
        test.map().camera_for_lat_lng_bounds(&bounds, &EdgeInsets::default(), Some(35.0), Some(20.0)).unwrap();
    assert!(bounds.contains(&virtual_camera.center.unwrap()));
    assert_near(virtual_camera.zoom.unwrap(), 13.66272, 1e-5);
    assert_double_eq(virtual_camera.pitch.unwrap(), 20.0);
    assert_near(virtual_camera.bearing.unwrap_or(0.0), 35.0, 1e-5);

    let padding = insets(10.0, 20.0, 30.0, 40.0);
    let virtual_camera_padded =
        test.map().camera_for_lat_lng_bounds(&bounds, &padding, Some(35.0), Some(20.0)).unwrap();
    assert!(bounds.contains(&virtual_camera_padded.center.unwrap()));
    assert_double_eq(virtual_camera_padded.center.unwrap().latitude(), virtual_camera.center.unwrap().latitude());
    assert_double_eq(virtual_camera_padded.center.unwrap().longitude(), virtual_camera.center.unwrap().longitude());

    let size = test.map().get_map_options().size();
    let width = f64::from(size.width);
    let height = f64::from(size.height);
    // std::min
    let a = (width - padding.left() - padding.right()) / width;
    let b = (height - padding.top() - padding.bottom()) / height;
    let scale_change = if b < a { b } else { a };
    assert_double_eq(virtual_camera_padded.zoom.unwrap(), virtual_camera.zoom.unwrap() + mln_math::log2(scale_change));
    assert_double_eq(virtual_camera_padded.pitch.unwrap(), virtual_camera.pitch.unwrap());
    assert_double_eq(virtual_camera_padded.bearing.unwrap(), virtual_camera.bearing.unwrap());
}

fn lat_lngs() -> Vec<LatLng> {
    vec![ll(40.712730, 74.005953), ll(15.68169, 73.499857), ll(30.82678, 83.4082)]
}

// gtest: Map.LatLngsToCamera
#[test]
fn lat_lngs_to_camera() {
    let test = MapTest::new();

    let virtual_camera = test.map().camera_for_lat_lngs(&lat_lngs(), &EdgeInsets::default(), Some(23.0), None).unwrap();
    assert_near(virtual_camera.bearing.unwrap_or(0.0), 23.0, 1e-5);
    assert_near(virtual_camera.zoom.unwrap_or(0.0), 2.75434, 1e-5);
    assert_near(virtual_camera.center.unwrap().latitude(), 28.49288, 1e-5);
    assert_near(virtual_camera.center.unwrap().longitude(), 74.97437, 1e-5);
}

// gtest: Map.LatLngsToCameraWithBearingAndPitch
#[test]
fn lat_lngs_to_camera_with_bearing_and_pitch() {
    let test = MapTest::new();

    let virtual_camera =
        test.map().camera_for_lat_lngs(&lat_lngs(), &EdgeInsets::default(), Some(23.0), Some(20.0)).unwrap();
    assert_near(virtual_camera.bearing.unwrap_or(0.0), 23.0, 1e-5);
    assert_near(virtual_camera.zoom.unwrap_or(0.0), 3.04378, 1e-5);
    assert_near(virtual_camera.center.unwrap().latitude(), 28.53718, 1e-5);
    assert_near(virtual_camera.center.unwrap().longitude(), 74.31746, 1e-5);
    assert_double_eq(virtual_camera.pitch.unwrap(), 20.0);
}

// gtest: Map.LatLngsToCameraWithBearingAndPitchMinMax
#[test]
fn lat_lngs_to_camera_with_bearing_and_pitch_min_max() {
    let test = MapTest::new();

    test.map().set_bounds(&BoundOptions::default().with_min_pitch(0.0).with_max_pitch(0.0)).unwrap();
    let mut virtual_camera =
        test.map().camera_for_lat_lngs(&lat_lngs(), &EdgeInsets::default(), Some(23.0), Some(45.0)).unwrap();
    assert_near(virtual_camera.bearing.unwrap_or(0.0), 23.0, 1e-5);
    assert_near(virtual_camera.zoom.unwrap_or(0.0), 2.75434, 1e-5);
    assert_near(virtual_camera.center.unwrap().latitude(), 28.49288, 1e-5);
    assert_near(virtual_camera.center.unwrap().longitude(), 74.97437, 1e-5);
    assert_double_eq(virtual_camera.pitch.unwrap(), 0.0);
    assert_double_eq(test.map().get_bounds().min_pitch.unwrap(), 0.0);
    assert_double_eq(test.map().get_bounds().max_pitch.unwrap(), 0.0);

    test.map().set_bounds(&BoundOptions::default().with_min_pitch(20.0).with_max_pitch(60.0)).unwrap();
    virtual_camera =
        test.map().camera_for_lat_lngs(&lat_lngs(), &EdgeInsets::default(), Some(23.0), Some(0.0)).unwrap();
    assert_near(virtual_camera.bearing.unwrap_or(0.0), 23.0, 1e-5);
    assert_near(virtual_camera.zoom.unwrap_or(0.0), 3.04378, 1e-5);
    assert_near(virtual_camera.center.unwrap().latitude(), 28.53718, 1e-5);
    assert_near(virtual_camera.center.unwrap().longitude(), 74.31746, 1e-5);
    assert_double_eq(virtual_camera.pitch.unwrap(), 20.0);
    assert_double_eq(test.map().get_bounds().min_pitch.unwrap(), 20.0);
    assert_double_eq(test.map().get_bounds().max_pitch.unwrap(), 60.0);
}

/// A geometry fits as its points do, wherever they sit in it.
#[test]
fn geometry_to_camera() {
    use mln_util::geometry::{Geometry as G, Point as P};
    let test = MapTest::new();
    let points: Vec<P<f64>> = lat_lngs().iter().map(|ll| P::new(ll.longitude(), ll.latitude())).collect();
    let geometry = G::GeometryCollection(vec![
        G::Point(points[0]),
        G::Polygon(vec![vec![points[1], points[2], points[1]]]),
        G::Empty,
    ]);

    let expected = test.map().camera_for_lat_lngs(&lat_lngs(), &EdgeInsets::default(), Some(23.0), Some(20.0)).unwrap();
    let fitted = test.map().camera_for_geometry(&geometry, &EdgeInsets::default(), Some(23.0), Some(20.0)).unwrap();
    assert_double_eq(fitted.zoom.unwrap(), expected.zoom.unwrap());
    assert_double_eq(fitted.center.unwrap().latitude(), expected.center.unwrap().latitude());
    assert_double_eq(fitted.center.unwrap().longitude(), expected.center.unwrap().longitude());
    assert_double_eq(fitted.bearing.unwrap(), 23.0);
    assert_double_eq(fitted.pitch.unwrap(), 20.0);

    let fitted = test.map().camera_for_geometry(&G::LineString(points), &EdgeInsets::default(), None, None).unwrap();
    let expected = test.map().camera_for_lat_lngs(&lat_lngs(), &EdgeInsets::default(), None, None).unwrap();
    assert_double_eq(fitted.zoom.unwrap(), expected.zoom.unwrap());

    // No points: the camera as cameraForLatLngs leaves it, unset.
    assert!(test.map().camera_for_geometry(&G::Empty, &EdgeInsets::default(), None, None).unwrap().zoom.is_none());
    // A point with no LatLng.
    let bad = G::MultiPoint(vec![P::new(0.0, 0.0), P::new(0.0, 91.0)]);
    assert!(test.map().camera_for_geometry(&bad, &EdgeInsets::default(), None, None).is_err());
}

/// A style with no sources publishes an update once it loads: a continuous
/// map whose camera was set first (so the style's default camera doesn't
/// jump it) still renders the style and finishes loading
/// (maplibre/maplibre-native#4674).
#[test]
fn a_sourceless_style_loaded_after_the_camera_renders() {
    let test = MapTest::with_mode(1.0, MapMode::Continuous);
    test.map().jump_to(&cam().with_center(Some(ll(10.0, 20.0))).with_zoom(Some(3.0)));
    test.load_json(
        r##"{"version": 8, "sources": {}, "layers": [
            {"id": "bg", "type": "background", "paint": {"background-color": "#00ff00"}}
        ]}"##,
    );

    let loaded = Rc::new(Cell::new(false));
    let timeout = Timer::new();
    {
        let run_loop = test.run_loop.clone();
        timeout.start(Duration::from_secs(5), Duration::zero(), move || run_loop.stop());
    }
    {
        let loaded = loaded.clone();
        let run_loop = test.run_loop.clone();
        test.observer.did_finish_loading_map_callback.set(move |_| {
            loaded.set(true);
            run_loop.stop();
        });
    }
    test.run_loop.run();
    test.observer.did_finish_loading_map_callback.clear();
    assert!(loaded.get(), "the map never finished loading");

    let image = {
        let _scope = BackendScope::new(test.frontend.get_backend(), ScopeType::Explicit);
        test.frontend.read_still_image()
    };
    assert_eq!(center_pixel(&image), [0, 255, 0, 255]);
}

/// A flight too slow for its duration to be held jumps, so the map goes idle
/// rather than animating for centuries.
#[test]
fn fly_too_slow_to_time_jumps_and_the_map_goes_idle() {
    let test = MapTest::with_mode(1.0, MapMode::Continuous);
    test.load_json(r##"{"version": 8, "sources": {}, "layers": [{"id": "bg", "type": "background"}]}"##);
    let run_until_idle = || {
        let idle = Rc::new(Cell::new(false));
        let timeout = Timer::new();
        {
            let run_loop = test.run_loop.clone();
            timeout.start(Duration::from_secs(5), Duration::zero(), move || run_loop.stop());
        }
        {
            let idle = idle.clone();
            let run_loop = test.run_loop.clone();
            test.observer.did_become_idle_callback.set(move |_| {
                idle.set(true);
                run_loop.stop();
            });
        }
        test.run_loop.run();
        test.observer.did_become_idle_callback.clear();
        idle.get()
    };
    assert!(run_until_idle());

    let target = cam().with_center(Some(ll(40.0, -74.0))).with_zoom(Some(10.0));
    test.map().fly_to(&target, &AnimationOptions { velocity: Some(f64::from_bits(1)), ..AnimationOptions::default() });
    assert!(!test.map().in_transition());
    assert_near(test.map().get_camera_options(None).zoom.unwrap(), 10.0, 1e-9);
    assert!(run_until_idle());
}

/// Free camera options place the camera, and cancel the running commands.
#[test]
fn free_camera_options_place_the_camera_and_cancel_commands() {
    let test = MapTest::new();
    let ended = Rc::new(Cell::new(None));
    let record = ended.clone();
    let animation = AnimationOptions {
        transition_end_fn: Some(Rc::new(move |end: &mln_map::TransitionEnd| record.set(Some(end.outcome)))),
        ..AnimationOptions::new(Duration::from_secs(10))
    };
    test.map().ease_to(&cam().with_zoom(Some(10.0)), &animation);
    assert!(test.map().in_transition());

    let position = [0.25, 0.25, 0.01];
    test.map().set_free_camera_options(&mln_map::FreeCameraOptions { position: Some(position), orientation: None });
    assert_eq!(ended.get(), Some(mln_map::TransitionOutcome::Cancelled));
    assert!(!test.map().in_transition());
    let placed = test.map().get_free_camera_options().position.unwrap();
    for (placed, wanted) in placed.iter().zip(position) {
        assert_near(*placed, wanted, 1e-9);
    }
    // Looking straight down from a quarter of the world's width and height:
    // the center is 90° W, north of the equator.
    let center = test.map().get_camera_options(None).center.unwrap();
    assert_near(center.longitude(), -90.0, 1e-6);
    assert!(center.latitude() > 0.0);

    // A NaN position is ignored.
    test.map().set_free_camera_options(&mln_map::FreeCameraOptions {
        position: Some([f64::NAN, 0.0, 0.0]),
        orientation: None,
    });
    assert_near(test.map().get_camera_options(None).center.unwrap().longitude(), -90.0, 1e-6);
}

// gtest: Map.CameraToLatLngBounds
#[test]
fn camera_to_lat_lng_bounds() {
    let test = MapTest::new();

    test.map().jump_to(&cam().with_center(Some(ll(45.0, 90.0))).with_zoom(Some(16.0)));

    let size = test.map().get_map_options().size();
    let bounds = {
        let map = test.map();
        LatLngBounds::hull(
            map.lat_lng_for_pixel(&ScreenCoordinate::default(), WrapMode::Wrapped).unwrap(),
            map.lat_lng_for_pixel(
                &ScreenCoordinate::new(f64::from(size.width), f64::from(size.height)),
                WrapMode::Wrapped,
            )
            .unwrap(),
        )
    };

    let camera = test.map().get_camera_options(None);

    assert_eq!(bounds, test.map().lat_lng_bounds_for_camera(&camera).unwrap());

    // Map::cameraForLatLngBounds only sets zoom and center.
    let virtual_camera = test.map().camera_for_lat_lng_bounds(&bounds, &EdgeInsets::default(), None, None).unwrap();
    assert_near(camera.zoom.unwrap(), virtual_camera.zoom.unwrap(), 1e-7);
    assert_near(camera.center.unwrap().latitude(), virtual_camera.center.unwrap().latitude(), 1e-7);
    assert_near(camera.center.unwrap().longitude(), virtual_camera.center.unwrap().longitude(), 1e-7);
}

fn corners_and_center(size: Size) -> [ScreenCoordinate; 5] {
    let (w, h) = (f64::from(size.width), f64::from(size.height));
    [
        ScreenCoordinate::default(),
        ScreenCoordinate::new(0.0, h),
        ScreenCoordinate::new(w, 0.0),
        ScreenCoordinate::new(w, h),
        ScreenCoordinate::new(w / 2.0, h / 2.0),
    ]
}

// gtest: Map.CameraToLatLngBoundsUnwrappedWithRotation
#[test]
fn camera_to_lat_lng_bounds_unwrapped_with_rotation() {
    let test = MapTest::new();

    test.map().jump_to(&cam().with_center(Some(ll(45.0, 90.0))).with_zoom(Some(16.0)).with_bearing(Some(45.0)));

    let size = test.map().get_map_options().size();

    let camera = test.map().get_camera_options(None);

    let map = test.map();
    for point in corners_and_center(size) {
        assert!(
            map.lat_lng_bounds_for_camera_unwrapped(&camera)
                .unwrap()
                .contains(&map.lat_lng_for_pixel(&point, WrapMode::Wrapped).unwrap()),
            "{point:?}"
        );
    }
}

// gtest: Map.CameraToLatLngBoundsUnwrappedCrossDateLine
#[test]
fn camera_to_lat_lng_bounds_unwrapped_cross_date_line() {
    let test = MapTest::new();

    test.map().jump_to(&cam().with_center(Some(ll(0.0, 180.0))).with_zoom(Some(16.0)));

    let size = test.map().get_map_options().size();

    let camera = test.map().get_camera_options(None);

    let map = test.map();
    let points = corners_and_center(size);
    for point in &points[..4] {
        assert!(
            map.lat_lng_bounds_for_camera_unwrapped(&camera)
                .unwrap()
                .contains_with_mode(&map.lat_lng_for_pixel(point, WrapMode::Wrapped).unwrap(), WrapMode::Wrapped),
            "{point:?}"
        );
    }
    assert!(
        map.lat_lng_bounds_for_camera_unwrapped(&camera)
            .unwrap()
            .contains(&map.lat_lng_for_pixel(&points[4], WrapMode::Wrapped).unwrap())
    );

    assert!(map.lat_lng_bounds_for_camera_unwrapped(&camera).unwrap().crosses_antimeridian());
}

// gtest: Map.LatLngForPixelUnwrapped
#[test]
fn lat_lng_for_pixel_unwrapped() {
    let test = MapTest::new();
    test.map().set_size(Size::new(2048, 512)).unwrap();

    let left_pixel = ScreenCoordinate::new(0.0, 256.0);
    let right_pixel = ScreenCoordinate::new(2048.0, 256.0);

    test.map().jump_to(&cam().with_center(Some(ll(0.0, 0.0))).with_zoom(Some(0.0)));
    let left = test.map().lat_lng_for_pixel(&left_pixel, WrapMode::Unwrapped).unwrap();
    let right = test.map().lat_lng_for_pixel(&right_pixel, WrapMode::Unwrapped).unwrap();
    assert_near(-720.0, left.longitude(), 1e-4);
    assert_near(720.0, right.longitude(), 1e-4);
    assert_near(0.0, test.map().lat_lng_for_pixel(&left_pixel, WrapMode::Wrapped).unwrap().longitude(), 1e-4);
    assert_near(0.0, test.map().lat_lng_for_pixel(&right_pixel, WrapMode::Wrapped).unwrap().longitude(), 1e-4);

    test.map().jump_to(&cam().with_center(Some(ll(0.0, 180.0))).with_zoom(Some(4.0)));
    let west = test.map().lat_lng_for_pixel(&left_pixel, WrapMode::Unwrapped).unwrap();
    let east = test.map().lat_lng_for_pixel(&right_pixel, WrapMode::Unwrapped).unwrap();
    assert_near(-225.0, west.longitude(), 1e-4);
    assert_near(-135.0, east.longitude(), 1e-4);
    assert_near(135.0, test.map().lat_lng_for_pixel(&left_pixel, WrapMode::Wrapped).unwrap().longitude(), 1e-4);
    assert_near(-135.0, test.map().lat_lng_for_pixel(&right_pixel, WrapMode::Wrapped).unwrap().longitude(), 1e-4);
    assert_near(90.0, east.longitude() - west.longitude(), 1e-4);

    let both = test.map().lat_lngs_for_pixels(&[left_pixel, right_pixel], WrapMode::Unwrapped).unwrap();
    assert_eq!(2, both.len());
    assert_double_eq(west.longitude(), both[0].longitude());
    assert_double_eq(east.longitude(), both[1].longitude());

    let projection = MapProjection::new(&test.map());
    assert_double_eq(
        east.longitude(),
        projection.lat_lng_for_pixel(&right_pixel, WrapMode::Unwrapped).unwrap().longitude(),
    );
    assert_near(-135.0, projection.lat_lng_for_pixel(&right_pixel, WrapMode::Wrapped).unwrap().longitude(), 1e-4);
}

// A zoom around a point keeps the place under it. Across the antimeridian the
// center comes back wrapped, as GL JS's setLocationAtPoint leaves it, or
// pixelForLatLng puts every place a world away.
// gtest: Map.PixelForLatLngAfterZoomAroundAPointAcrossTheAntimeridian
#[test]
fn pixel_for_lat_lng_after_zoom_around_a_point_across_the_antimeridian() {
    let test = MapTest::new();
    test.map().set_size(Size::new(256, 256)).unwrap();
    test.map().jump_to(&cam().with_center(Some(ll(0.0, 179.9))).with_zoom(Some(5.0)));

    let anchor = ScreenCoordinate::new(230.0, 128.0);
    let place = test.map().lat_lng_for_pixel(&anchor, WrapMode::Wrapped).unwrap();
    test.map().jump_to(&cam().with_zoom(Some(6.0)).with_anchor(Some(anchor)));

    assert!(test.map().get_camera_options(None).center.unwrap().longitude().abs() <= 180.0);
    let point = test.map().pixel_for_lat_lng(&place);
    assert_near(anchor.x, point.x, 1e-6);
    assert_near(anchor.y, point.y, 1e-6);
    let projected = MapProjection::new(&test.map()).pixel_for_lat_lng(&place);
    assert_near(anchor.x, projected.x, 1e-6);
    assert_near(anchor.y, projected.y, 1e-6);
}

// gtest: Map.PixelForLatLngWithTheCenterPastTheAntimeridian
#[test]
fn pixel_for_lat_lng_with_the_center_past_the_antimeridian() {
    let test = MapTest::new();
    test.map().set_size(Size::new(512, 512)).unwrap();
    test.map()
        .set_bounds(&BoundOptions::default().with_lat_lng_bounds(LatLngBounds::hull(ll(-10.0, 170.0), ll(10.0, 190.0))))
        .unwrap();
    test.map().jump_to(&cam().with_center(Some(ll(0.0, 181.0))).with_zoom(Some(4.0)));
    assert_near(181.0, test.map().get_camera_options(None).center.unwrap().longitude(), 1e-9);

    // A degree is 512 * 2^4 / 360 pixels at zoom 4.
    let degree = 512.0 * 16.0 / 360.0;
    let projection = MapProjection::new(&test.map());
    for (longitude, x) in [(179.0, 256.0 - 2.0 * degree), (-179.5, 256.0 - 0.5 * degree)] {
        assert_near(x, test.map().pixel_for_lat_lng(&ll(0.0, longitude)).x, 1e-6);
        assert_near(x, projection.pixel_for_lat_lng(&ll(0.0, longitude)).x, 1e-6);
    }
}

// https://github.com/maplibre/maplibre-native/issues/3580
// iOS ends a pan with a moveBy that has a duration while the gesture is still
// in progress. Across the antimeridian the move starts a world away, and
// annotation views placed through pixelForLatLng vanished until the center
// crossed.
// gtest: Map.PixelForLatLngDuringAFlingAcrossTheAntimeridian
#[test]
fn pixel_for_lat_lng_during_a_fling_across_the_antimeridian() {
    let test = MapTest::with_mode(1.0, MapMode::Continuous);
    test.map().set_size(Size::new(512, 512)).unwrap();
    test.map().jump_to(&cam().with_center(Some(ll(0.0, 179.0))).with_zoom(Some(4.0)));

    let degree = 512.0 * 16.0 / 360.0;
    test.map().set_gesture_in_progress(true);
    test.map()
        .move_by(&ScreenCoordinate::new(-2.0 * degree, 0.0), &AnimationOptions::new(mln_util::chrono::seconds(10)))
        .unwrap();
    let center = test.map().get_camera_options(None).center.unwrap().longitude();
    assert!(center < -180.0, "{center}");

    let projection = MapProjection::new(&test.map());
    for (longitude, nearest) in [(179.5, -180.5), (-179.5, -179.5)] {
        let x = 256.0 + (nearest - center) * degree;
        assert_near(x, test.map().pixel_for_lat_lng(&ll(0.0, longitude)).x, 1e-6);
        assert_near(x, projection.pixel_for_lat_lng(&ll(0.0, longitude)).x, 1e-6);
    }
}

/// `MapTest<MainResourceLoader> test{cachePath, assetPath}`, over
/// `file_sources`.
fn main_resource_loader_test(
    file_sources: &FileSourceManager,
    cache_path: &str,
    asset_path: &str,
) -> MapTest<MainResourceLoader> {
    let run_loop = RunLoop::new();
    let file_source = Arc::new(MainResourceLoader::new(
        file_sources,
        &ResourceOptions::default().with_cache_path(cache_path).with_asset_path(asset_path),
    ));
    MapTest::create(run_loop, file_source, 1.0, MapMode::Static)
}

/// `getFileSource(FileSourceType::Database, ResourceOptions{})`.
fn database_file_source(file_sources: &FileSourceManager) -> Arc<dyn FileSource> {
    file_sources.get_file_source(FileSourceType::Database, &ResourceOptions::default()).expect("database file source")
}

/// A `dbfs->forward(..., [] {})` callback.
fn no_op() -> Option<Box<dyn FnOnce() + Send>> {
    Some(Box::new(|| {}))
}

// gtest: Map.Offline
#[test]
fn offline() {
    let file_sources = FileSourceManager::new();
    let test = main_resource_loader_test(&file_sources, ":memory:", ".");

    // C++ CI leaves this test out on the macOS runners
    // (.github/workflows/macos-ci.yml, --gtest_filter=-...:Map.Offline, for
    // both its Dawn and wgpu builds). Their VMs render through Metal's
    // "Apple Paravirtual device", where a few labels' glyphs land on other
    // labels (image difference 0.0176). Placement is identical to a run on an
    // Apple GPU, where the image matches. Skip on that device, as C++ does.
    let adapter = test
        .frontend
        .get_backend()
        .as_any()
        .downcast_ref::<mln_webgpu::HeadlessBackend>()
        .and_then(|backend| backend.get_adapter_info());
    if adapter.is_some_and(|info| info.name == "Apple Paravirtual device") {
        eprintln!("Map.Offline: skipped on the Apple Paravirtual device, as in C++ CI");
        return;
    }

    let expired_item = |path: &str| {
        let mut response = file_response(&format!("test/fixtures/map/offline/{path}"));
        response.expires = Some(Timestamp::from_secs(0));
        response
    };

    NetworkStatus::set(NetworkStatusType::Offline);
    let prefix = "http://127.0.0.1:3000/";
    let dbfs = database_file_source(&file_sources);
    dbfs.forward(&Resource::style(&format!("{prefix}style.json")), &expired_item("style.json"), no_op());
    dbfs.forward(&Resource::source(&format!("{prefix}streets.json")), &expired_item("streets.json"), no_op());
    dbfs.forward(&Resource::sprite_json(&format!("{prefix}sprite"), 1.0), &expired_item("sprite.json"), no_op());
    dbfs.forward(&Resource::sprite_image(&format!("{prefix}sprite"), 1.0), &expired_item("sprite.png"), no_op());
    dbfs.forward(
        &Resource::tile(
            &format!("{prefix}{{z}}-{{x}}-{{y}}.vector.pbf"),
            1.0,
            0,
            0,
            0,
            Scheme::XYZ,
            LoadingMethod::All,
        ),
        &expired_item("0-0-0.vector.pbf"),
        no_op(),
    );
    // C++ calls `test.map.getStyle().loadURL(...)` in this callback, which
    // runs inside `frontend.render(map)`'s run loop. Here render borrows the
    // map mutably, so the test runs the loop until the callback has run and
    // then loads the style (the same requests, just before renderStill).
    let handle = test.run_loop.handle();
    dbfs.forward(
        &Resource::glyphs(&format!("{prefix}{{fontstack}}/{{range}}.pbf"), &vec!["Helvetica".to_owned()], (0, 255)),
        &expired_item("glyph.pbf"),
        Some(Box::new(move || handle.stop())),
    );
    run_with_timeout(&test.run_loop, 60);
    test.map().get_style_mut().load_url(&format!("{prefix}style.json"));

    check_image("test/fixtures/map/offline", &test.render().image, 0.0015, 0.1);
    NetworkStatus::set(NetworkStatusType::Online);
}

// gtest: Map.SetStyleDefaultCamera
#[test]
fn set_style_default_camera() {
    let test = MapTest::new();
    test.load_json(&read_file("test/fixtures/api/empty.json"));
    let mut camera = test.map().get_camera_options(None);
    assert_double_eq(camera.zoom.unwrap(), 0.0);
    assert_double_eq(camera.pitch.unwrap(), 0.0);
    assert_double_eq(camera.bearing.unwrap(), 0.0);
    assert_eq!(camera.center.unwrap(), LatLng::default());

    test.load_json(&read_file("test/fixtures/api/empty-zoomed.json"));
    camera = test.map().get_camera_options(None);
    assert_double_eq(camera.zoom.unwrap(), 0.0);

    let default_camera = test.map().get_style().get_default_camera();
    test.map().jump_to(&default_camera);
    camera = test.map().get_camera_options(None);
    assert_double_eq(camera.zoom.unwrap(), 0.5);
}

// gtest: Map.ProjectionMode
#[test]
fn projection_mode() {
    let test = MapTest::new();

    test.map().set_projection_mode(&ProjectionMode::default().with_axonometric(true).with_x_skew(1.0).with_y_skew(0.0));
    let options = test.map().get_projection_mode();

    assert!(options.axonometric.unwrap());
    assert_eq!(options.x_skew.unwrap(), 1.0);
    assert_eq!(options.y_skew.unwrap(), 0.0);
}

// gtest: Map.BoundOptions
#[test]
fn bound_options() {
    let test = MapTest::new();

    let llb = LatLngBounds::hull(ll(-10.0, -10.0), ll(10.0, 10.0));
    test.map()
        .set_bounds(&BoundOptions::default().with_min_zoom(4.0).with_max_zoom(10.0).with_lat_lng_bounds(llb))
        .unwrap();
    let bounds = test.map().get_bounds();

    assert_eq!(bounds.min_zoom.unwrap(), 4.0);
    assert_eq!(bounds.max_zoom.unwrap(), 10.0);
    assert_eq!(bounds.bounds.unwrap(), llb);
}

// gtest: Map.DefaultBoundOptions
#[test]
fn default_bound_options() {
    let test = MapTest::new();

    let bounds = test.map().get_bounds();

    assert!(bounds.min_zoom.is_some());
    assert!(bounds.max_zoom.is_some());
    assert!(bounds.bounds.is_some());

    assert_eq!(bounds.min_zoom.unwrap(), mln_util::constants::MIN_ZOOM);
    assert_eq!(bounds.max_zoom.unwrap(), f64::from(mln_util::constants::DEFAULT_MAX_ZOOM));
    assert_eq!(bounds.bounds.unwrap(), LatLngBounds::default());
}

// gtest: Map.MapOptions
#[test]
fn map_options() {
    let pixel_ratio = 2.0;
    let test = MapTest::with_mode(pixel_ratio, MapMode::Continuous);

    test.map().set_north_orientation(NorthOrientation::Rightwards);
    test.map().set_constrain_mode(ConstrainMode::None);
    test.map().set_viewport_mode(ViewportMode::FlippedY);
    let size = Size::new(512, 512);
    test.map().set_size(size).unwrap();

    let options = test.map().get_map_options();
    assert_eq!(options.map_mode(), MapMode::Continuous);
    assert_eq!(options.viewport_mode(), ViewportMode::FlippedY);
    assert_eq!(options.constrain_mode(), ConstrainMode::None);
    assert_eq!(options.north_orientation(), NorthOrientation::Rightwards);
    assert_eq!(options.size(), size);
    assert_eq!(options.pixel_ratio(), pixel_ratio);
}

// gtest: Map.DefaultMapOptions
#[test]
fn default_map_options() {
    let test = MapTest::new();

    let options = test.map().get_map_options();
    assert_eq!(options.map_mode(), MapMode::Static);
    assert_eq!(options.viewport_mode(), ViewportMode::Default);
    assert_eq!(options.constrain_mode(), ConstrainMode::HeightOnly);
    assert_eq!(options.north_orientation(), NorthOrientation::Upwards);
    assert!(options.cross_source_collisions());
    assert_eq!(options.size().width, 256);
    assert_eq!(options.size().height, 256);
    assert_eq!(options.pixel_ratio(), 1.0);
}

// gtest: Map.SetStyleInvalidJSON
#[test]
fn set_style_invalid_json() {
    let log = FixtureLog::new();

    let fail = Rc::new(Cell::new(false));

    {
        let test = MapTest::new();
        let fail = fail.clone();
        test.observer.did_fail_loading_map_callback.set(move |_| fail.set(true));
        test.load_json("invalid");
    }

    assert!(fail.get());

    // C++: "Failed to parse style: Invalid value. at offset 0" (rapidjson);
    // the port parses with serde_json, so the message after the prefix is
    // serde_json's.
    let serde_error = serde_json::from_str::<serde_json::Value>("invalid").unwrap_err();
    assert_eq!(
        1,
        log.count(EventSeverity::Error, Event::ParseStyle, -1, &format!("Failed to parse style: {serde_error}"))
    );
    let unchecked = log.unchecked();
    assert!(unchecked.is_empty(), "{unchecked:?}");
}

// gtest: Map.SetStyleInvalidURL
#[test]
fn set_style_invalid_url() {
    let test = MapTest::new();

    test.file_source.set_response(Kind::Style, |_| {
        let mut response = Response::new();
        response.error = Some(Box::new(ResponseError::new(Reason::Other, "Failed by the test case")));
        Some(response)
    });

    let run_loop = test.run_loop.clone();
    test.observer.did_fail_loading_map_callback.set(move |_| run_loop.stop());

    test.map().get_style_mut().load_url("maptiler://bar");

    run_with_timeout(&test.run_loop, 60);
}

// gtest: Map.DoubleStyleLoad
#[test]
fn double_style_load() {
    let test = MapTest::new();

    test.load_json("");
    test.load_json("");
}

fn expired_or_fresh_response(fixture: &str, expires: Timestamp) -> Response {
    let mut response = file_response(fixture);
    response.expires = Some(expires);
    response
}

fn hours_from_now(hours: i64) -> Timestamp {
    Timestamp::from_secs(mln_util::chrono::now().secs_since_epoch() + hours * 3600)
}

// gtest: Map.StyleFresh
#[test]
fn style_fresh() {
    // The map should continue to revalidate fresh styles.

    let test = MapTest::fake();

    test.map().get_style_mut().load_url("maptiler://maps/test");
    assert_eq!(1, test.file_source.requests_len());

    // Timestamp::max()
    let response = expired_or_fresh_response("test/fixtures/api/empty.json", Timestamp::from_secs(i64::MAX));

    test.file_source.respond(Kind::Style, &response);
    assert_eq!(1, test.file_source.requests_len());
}

// gtest: Map.StyleExpired
#[test]
fn style_expired() {
    // The map should allow expired styles to be revalidated until we get a fresh style.

    let test = MapTest::fake();

    test.map().get_style_mut().load_url("maptiler://maps/test");
    assert_eq!(1, test.file_source.requests_len());

    let mut response = expired_or_fresh_response("test/fixtures/api/empty.json", hours_from_now(-1));

    test.file_source.respond(Kind::Style, &response);
    assert_eq!(1, test.file_source.requests_len());

    // Mutate layer. From now on, sending a response to the style won't
    // overwrite it anymore, but we should continue to wait for a fresh
    // response.
    test.map().get_style_mut().add_layer(Box::new(BackgroundLayer::new("bg")), None).unwrap();
    assert_eq!(1, test.file_source.requests_len());

    // Send another expired response, and confirm that we didn't overwrite the
    // style, but continue to wait for a fresh response.
    test.file_source.respond(Kind::Style, &response);
    assert_eq!(1, test.file_source.requests_len());
    assert!(test.map().get_style().get_layer("bg").is_some());

    // Send a fresh response, and confirm that we didn't overwrite the style,
    // but continue to wait for a fresh response.
    response.expires = Some(hours_from_now(1));
    test.file_source.respond(Kind::Style, &response);
    assert_eq!(1, test.file_source.requests_len());
    assert!(test.map().get_style().get_layer("bg").is_some());
}

// gtest: Map.StyleExpiredWithAnnotations
#[test]
#[ignore = "TODO(port): annotations (Map::addAnnotation, AnnotationManager); see map.test.cpp:647"]
fn style_expired_with_annotations() {
    // Adding an annotation should not prevent revalidation of an expired style.
    let test = MapTest::fake();

    test.map().get_style_mut().load_url("maptiler://maps/test");
    assert_eq!(1, test.file_source.requests_len());

    let response = expired_or_fresh_response("test/fixtures/api/empty.json", hours_from_now(-1));

    test.file_source.respond(Kind::Style, &response);
    assert_eq!(1, test.file_source.requests_len());

    // TODO(port): test.map.addAnnotation(LineAnnotation{LineString<double>{{{0, 0}, {10, 10}}}});
    assert_eq!(1, test.file_source.requests_len());

    test.file_source.respond(Kind::Style, &response);
    assert_eq!(1, test.file_source.requests_len());
}

// gtest: Map.StyleExpiredWithRender
#[test]
fn style_expired_with_render() {
    // Rendering should not prevent revalidation of an expired style.

    let test = MapTest::fake();

    test.map().get_style_mut().load_url("maptiler://maps/test");
    assert_eq!(1, test.file_source.requests_len());

    let response = expired_or_fresh_response("test/fixtures/api/empty.json", hours_from_now(-1));

    test.file_source.respond(Kind::Style, &response);
    assert_eq!(1, test.file_source.requests_len());

    test.render();
    assert_eq!(1, test.file_source.requests_len());

    test.file_source.respond(Kind::Style, &response);
    assert_eq!(1, test.file_source.requests_len());
}

// gtest: Map.StyleEarlyMutation
#[test]
fn style_early_mutation() {
    // An early mutation should not prevent the initial style load.

    let test = MapTest::fake();

    test.map().get_style_mut().load_url("maptiler://maps/test");
    test.map().get_style_mut().add_layer(Box::new(BackgroundLayer::new("bg")), None).unwrap();

    let response = file_response("test/fixtures/api/water.json");
    test.file_source.respond(Kind::Style, &response);

    assert_eq!(1, test.file_source.requests_len());
    assert!(test.map().get_style().get_layer("water").is_some());
}

// gtest: Map.MapLoadingSignal
#[test]
fn map_loading_signal() {
    let test = MapTest::new();

    let emitted = Rc::new(Cell::new(false));
    {
        let emitted = emitted.clone();
        test.observer.will_start_loading_map_callback.set(move |_| emitted.set(true));
    }
    test.load_json(&read_file("test/fixtures/api/empty.json"));
    assert!(emitted.get());
}

// gtest: Map.MapLoadedSignal
#[test]
fn map_loaded_signal() {
    let test = MapTest::with_mode(1.0, MapMode::Continuous);

    let run_loop = test.run_loop.clone();
    test.observer.did_finish_loading_map_callback.set(move |_| run_loop.stop());

    test.load_json(&read_file("test/fixtures/api/empty.json"));
    run_with_timeout(&test.run_loop, 60);
}

// gtest: Map.StyleLoadedSignal
#[test]
fn style_loaded_signal() {
    let test = MapTest::new();

    // The map should emit a signal on style loaded
    let emitted = Rc::new(Cell::new(false));
    {
        let emitted = emitted.clone();
        test.observer.did_finish_loading_style_callback.set(move |_| emitted.set(true));
    }
    test.load_json(&read_file("test/fixtures/api/empty.json"));
    assert!(emitted.get());

    // But not when the style couldn't be parsed
    emitted.set(false);
    test.load_json("invalid");
    assert!(!emitted.get());
}

/// `MapTest<OnlineFileSource> test(ResourceOptions::Default())`.
fn online_file_source_test() -> MapTest<OnlineFileSource> {
    let run_loop = RunLoop::new();
    let file_source = Arc::new(OnlineFileSource::new(&ResourceOptions::default_options()));
    MapTest::create(run_loop, file_source, 1.0, MapMode::Static)
}

// Test for https://github.com/mapbox/mapbox-gl-native/issues/7902
// gtest: Map.StyleNetworkErrorRetry
#[test]
fn style_network_error_retry() {
    let server = HttpServer::new();
    let test = online_file_source_test();

    test.map().get_style_mut().load_url(&server.url("/style-fail-once-500"));

    let run_loop = test.run_loop.clone();
    test.observer.did_finish_loading_style_callback.set(move |_| run_loop.stop());

    run_with_timeout(&test.run_loop, 10);
}

// gtest: Map.StyleNotFound
#[test]
fn style_not_found() {
    let server = HttpServer::new();
    let test = online_file_source_test();

    test.map().get_style_mut().load_url(&server.url("/style-fail-once-404"));

    let timer = Rc::new(Timer::new());

    // Not found errors should not trigger a retry like other errors.
    test.observer.did_finish_loading_style_callback.set(|_| panic!("Should not retry on not found!"));

    {
        let (timer, run_loop) = (timer.clone(), test.run_loop.clone());
        test.observer.did_fail_loading_map_callback.set(move |_| {
            let run_loop = run_loop.clone();
            timer.start(Duration::from_millis(1100), Duration::zero(), move || run_loop.stop());
        });
    }

    run_with_timeout(&test.run_loop, 10);

    // Should also not retry if the response has cache headers.
    test.map().get_style_mut().load_url(&server.url("/style-fail-once-404-cache"));
    run_with_timeout(&test.run_loop, 10);
}

// gtest: Map.AddLayer
#[test]
fn add_layer() {
    let test = MapTest::new();

    test.load_json(&read_file("test/fixtures/api/empty.json"));

    test.map().get_style_mut().add_layer(red_background("background"), None).unwrap();

    check_image("test/fixtures/map/add_layer", &test.render().image, 0.0, 0.0);
}

// gtest: Map.RemoveLayer
#[test]
fn remove_layer() {
    let test = MapTest::new();

    test.load_json(&read_file("test/fixtures/api/empty.json"));

    test.map().get_style_mut().add_layer(red_background("background"), None).unwrap();
    test.map().get_style_mut().remove_layer("background");

    check_image("test/fixtures/map/remove_layer", &test.render().image, 0.0, 0.0);
}

// gtest: Map.DisabledSources
#[test]
fn disabled_sources() {
    let test = MapTest::new();

    // Always load the same image tile for raster layers.
    test.file_source.set_global_response(|res: &Resource| {
        if res.url == "asset://tile.png" {
            return Some(file_response("test/fixtures/map/disabled_layers/tile.png"));
        }
        None
    });

    test.map().jump_to(&cam().with_zoom(Some(1.0)));

    // This stylesheet has two raster layers, one that starts at zoom 1, the
    // other at zoom 0. We first render a map at zoom level 1, which should show
    // both layers (both are "visible" due to an opacity of 0.5). Then, we are
    // zooming back out to a zoom level of 0.5 and rerender. The "raster1" layer
    // should not be visible anymore since it has minzoom 1, while "raster2"
    // should still be there. Both layers have a distinct color through
    // "raster-hue-rotate".
    test.load_json(
        r#"
{
  "version": 8,
  "name": "Test",
  "sources": {
    "raster": {
      "type": "raster",
      "tiles": [ "asset://tile.png" ],
      "tileSize": 256
    }
  },
  "layers": [{
    "id": "background",
    "type": "background",
    "paint": {
      "background-color": "white"
    }
  }, {
    "id": "raster1",
    "type": "raster",
    "source": "raster",
    "minzoom": 0
  }, {
    "id": "raster2",
    "type": "raster",
    "source": "raster",
    "minzoom": 1,
    "paint": {
      "raster-hue-rotate": 180
    }
  }]
}
"#,
    );

    check_image("test/fixtures/map/disabled_layers/first", &test.render().image, 0.0, 0.0);
    test.map().jump_to(&cam().with_zoom(Some(0.5)));
    check_image("test/fixtures/map/disabled_layers/second", &test.render().image, 0.0, 0.0);
}

// gtest: Map.DontLoadUnneededTiles
#[test]
fn dont_load_unneeded_tiles() {
    let test = MapTest::new();

    test.load_json(
        r#"{
  "sources": {
    "a": { "type": "vector", "tiles": [ "a/{z}/{x}/{y}" ] }
  },
  "layers": [{
    "id": "a",
    "type": "fill",
    "source": "a",
    "source-layer": "a",
    "minzoom": 0.3,
    "maxzoom": 1.6
  }]
}"#,
    );

    let tiles: Rc<RefCell<HashSet<String>>> = Rc::new(RefCell::new(HashSet::new()));

    {
        let tiles = tiles.clone();
        test.file_source.set_response(Kind::Tile, move |rsc: &Resource| {
            tiles.borrow_mut().insert(rsc.url.clone());
            no_content()
        });
    }

    let reference_tiles = |z: f64| -> HashSet<String> {
        let tiles: &[&str] = if z == 0.3 {
            // Since the layer's minzoom is 0.3, we shouldn't load tiles before z0.3
            &["a/0/0/0"]
        } else if z == 1.0 {
            &["a/1/1/0", "a/1/0/1", "a/1/0/0", "a/1/1/1"]
        } else {
            // Since the layer's maxzoom is 1.6, we should never load z2 or z3 tiles.
            &[]
        };
        tiles.iter().map(|s| s.to_string()).collect()
    };

    // Loop through zoom levels from 0 to 3 and check that the correct tiles are
    // loaded at every step. The source is marked with maxzoom 1.0, which means
    // that it won't be visible anymore after z1.0, so we should under no
    // circumstances attempt to load z2 tiles.
    for zoom in 0..=30u32 {
        // times 10
        // Note: using z += 0.1 in the loop doesn't produce accurate floating point numbers.
        let z = f64::from(zoom) / 10.0;
        tiles.borrow_mut().clear();
        test.map().jump_to(&cam().with_zoom(Some(z)));
        test.render();
        assert_eq!(reference_tiles(z), *tiles.borrow(), "zoom level {z}");
    }
}

// gtest: Map.ContinuousRendering
#[test]
#[ignore = "TEST_DISABLED_ON_CI (timing-dependent); see map.test.cpp:922"]
fn continuous_rendering() {
    let run_loop = RunLoop::new();

    let did_not_stop = Rc::new(Cell::new(false));
    let emergency_shutoff = Timer::new();
    {
        let run_loop = run_loop.clone();
        let did_not_stop = did_not_stop.clone();
        emergency_shutoff.start(Duration::from_secs(10), Duration::zero(), move || {
            run_loop.stop();
            did_not_stop.set(true);
        });
    }

    let timer = Rc::new(Timer::new());

    let frontend = HeadlessFrontend::with_pixel_ratio(1.0);

    let observer = Rc::new(StubMapObserver::default());
    {
        let timer = Rc::downgrade(&timer);
        let run_loop = run_loop.clone();
        observer.did_finish_rendering_frame_callback.set(move |_| {
            // Start a timer that ends the test one second from now. If we are
            // continuing to render indefinitely, the timer will be constantly
            // restarted and never trigger. Instead, the emergency shutoff above
            // will trigger, failing the test.
            if let Some(timer) = timer.upgrade() {
                let run_loop = run_loop.clone();
                timer.start(Duration::from_secs(1), Duration::zero(), move || run_loop.stop());
            }
        });
    }

    let mut map = Map::with_resource_options(
        &frontend,
        observer.clone(),
        MapOptions::new().with_map_mode(MapMode::Continuous).with_size(frontend.get_size()),
        &FileSourceManager::new(),
        &ResourceOptions::default()
            .with_cache_path(":memory:")
            .with_asset_path(cpp_root().join("test/fixtures/api/assets").to_str().unwrap()),
    )
    .unwrap();
    map.get_style_mut().load_json(&read_file("test/fixtures/api/water.json"));

    run_loop.run();
    assert!(!did_not_stop.get(), "Did not stop rendering");
}

// gtest: Map.NoContentTiles
#[test]
fn no_content_tiles() {
    let file_sources = FileSourceManager::new();
    let test = main_resource_loader_test(&file_sources, ":memory:", ".");

    // Insert a 204 No Content response for the 0/0/0 tile
    let mut response = Response::new();
    response.no_content = true;
    response.expires = Some(hours_from_now(1));
    let dbfs = database_file_source(&file_sources);
    // C++ loads the style in the callback (inside render's run loop); see
    // Map.Offline for why the port waits for it first.
    let handle = test.run_loop.handle();
    dbfs.forward(
        &Resource::tile("http://example.com/{z}-{x}-{y}.vector.pbf", 1.0, 0, 0, 0, Scheme::XYZ, LoadingMethod::All),
        &response,
        Some(Box::new(move || handle.stop())),
    );
    run_with_timeout(&test.run_loop, 60);
    test.load_json(
        r#"{
                        "version": 8,
                        "name": "Water",
                        "sources": {
                            "mapbox": {
                            "type": "vector",
                            "tiles": ["http://example.com/{z}-{x}-{y}.vector.pbf"]
                            }
                        },
                        "layers": [{
                            "id": "background",
                            "type": "background",
                            "paint": {
                            "background-color": "red"
                            }
                        }, {
                            "id": "water",
                            "type": "fill",
                            "source": "mapbox",
                            "source-layer": "water"
                        }]
                        }"#,
    );
    check_image("test/fixtures/map/nocontent", &test.render().image, 0.0015, 0.1);
}

// A glyph range that fails to load or doesn't parse reaches the map observer
// with its font stack and range, beside the ranges that are requested and
// load.
#[test]
fn glyph_range_errors_reach_the_observer() {
    let test = MapTest::with_mode(1.0, MapMode::Continuous);

    test.file_source.set_response(Kind::Glyphs, |resource| {
        let mut response = Response::new();
        if resource.url.contains("/Missing/") {
            response.error = Some(Box::new(ResponseError::new(Reason::NotFound, "Not found by the test")));
        } else if resource.url.contains("/Corrupt/") {
            response.data = Some(Arc::new(b"CORRUPTED".to_vec()));
        } else {
            response = file_response("test/fixtures/resources/glyphs.pbf");
        }
        Some(response)
    });

    let symbol_layer = |font: &str| {
        format!(
            r#"{{"id": "{font}", "type": "symbol", "source": "point",
                "layout": {{"text-field": "a", "text-font": ["{font}"]}}}}"#
        )
    };
    test.load_json(&format!(
        r#"{{
            "version": 8,
            "glyphs": "test://glyphs/{{fontstack}}/{{range}}.pbf",
            "sources": {{
                "point": {{"type": "geojson", "data": {{"type": "Point", "coordinates": [0, 0]}}}}
            }},
            "layers": [{}, {}, {}]
        }}"#,
        symbol_layer("Missing"),
        symbol_layer("Corrupt"),
        symbol_layer("Good"),
    ));

    type Reported = Rc<RefCell<Vec<(Vec<String>, GlyphRange)>>>;
    let requested: Reported = Rc::default();
    let loaded: Reported = Rc::default();
    let errors: Rc<RefCell<Vec<(Vec<String>, GlyphRange, String)>>> = Rc::default();

    let r = requested.clone();
    test.observer.on_glyphs_requested_callback.set(move |(font_stack, range)| r.borrow_mut().push((font_stack, range)));
    let (l, e, run_loop) = (loaded.clone(), errors.clone(), test.run_loop.clone());
    let stop_when_settled = move || {
        if l.borrow().len() == 1 && e.borrow().len() == 2 {
            run_loop.stop();
        }
    };
    let (l, stop) = (loaded.clone(), stop_when_settled.clone());
    test.observer.on_glyphs_loaded_callback.set(move |(font_stack, range)| {
        l.borrow_mut().push((font_stack, range));
        stop();
    });
    // The file source retries a failed request, so a failure can repeat.
    let (e, stop) = (errors.clone(), stop_when_settled);
    test.observer.on_glyphs_error_callback.set(move |(font_stack, range, error)| {
        let error = (font_stack, range, error.to_string());
        if !e.borrow().contains(&error) {
            e.borrow_mut().push(error);
        }
        stop();
    });

    run_with_timeout(&test.run_loop, 10);

    let first_range = GlyphRange::new(0, 255, GlyphIDType::FontPBF);
    let stack = |font: &str| vec![font.to_owned()];
    let mut requested = requested.take();
    requested.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        requested,
        [(stack("Corrupt"), first_range), (stack("Good"), first_range), (stack("Missing"), first_range)]
    );
    assert_eq!(loaded.take(), [(stack("Good"), first_range)]);
    let mut errors = errors.take();
    errors.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        errors,
        [
            (stack("Corrupt"), first_range, "unknown pbf field type exception".to_owned()),
            (stack("Missing"), first_range, "Not found by the test".to_owned()),
        ]
    );
}

// The map observer hears each sprite the style requests, and whether it
// loaded or failed.
#[test]
fn sprite_events_reach_the_observer() {
    let test = MapTest::with_mode(1.0, MapMode::Continuous);

    let serve = |kind: Kind, file: &'static str| {
        test.file_source.set_response(kind, move |resource| {
            if resource.url.starts_with("test://sprites/good") {
                Some(file_response(file))
            } else {
                let mut response = Response::new();
                response.error = Some(Box::new(ResponseError::new(Reason::NotFound, "Not found by the test")));
                Some(response)
            }
        });
    };
    serve(Kind::SpriteJSON, "test/fixtures/resources/sprite.json");
    serve(Kind::SpriteImage, "test/fixtures/resources/sprite.png");

    let requested: Rc<RefCell<Vec<Option<Sprite>>>> = Rc::default();
    let loaded: Rc<RefCell<Vec<Option<Sprite>>>> = Rc::default();
    let errors: Rc<RefCell<Vec<(Option<Sprite>, String)>>> = Rc::default();
    let r = requested.clone();
    test.observer.on_sprite_requested_callback.set(move |sprite| r.borrow_mut().push(sprite));
    let (l, e, run_loop) = (loaded.clone(), errors.clone(), test.run_loop.clone());
    let stop_when_settled = move || {
        if l.borrow().len() == 1 && !e.borrow().is_empty() {
            run_loop.stop();
        }
    };
    let (l, stop) = (loaded.clone(), stop_when_settled.clone());
    test.observer.on_sprite_loaded_callback.set(move |sprite| {
        l.borrow_mut().push(sprite);
        stop();
    });
    // The file source retries a failed request, so a failure can repeat.
    let (e, stop) = (errors.clone(), stop_when_settled);
    test.observer.on_sprite_error_callback.set(move |(sprite, error)| {
        let error = (sprite, error.to_string());
        if !e.borrow().contains(&error) {
            e.borrow_mut().push(error);
        }
        stop();
    });

    test.load_json(
        r#"{
            "version": 8,
            "sprite": [{"id": "good", "url": "test://sprites/good"}, {"id": "bad", "url": "test://sprites/bad"}],
            "sources": {},
            "layers": [{"id": "background", "type": "background"}]
        }"#,
    );
    run_with_timeout(&test.run_loop, 10);

    let good = Some(Sprite::new("good", "test://sprites/good"));
    let bad = Some(Sprite::new("bad", "test://sprites/bad"));
    assert_eq!(requested.take(), [good.clone(), bad.clone()]);
    assert_eq!(loaded.take(), [good]);
    assert_eq!(errors.take(), [(bad, "Not found by the test".to_owned())]);
}

// A style without a sprite loads an empty one, which the observer hears as
// `None`, with no request before it.
#[test]
fn a_style_without_a_sprite_reports_it_loaded() {
    let test = MapTest::with_mode(1.0, MapMode::Continuous);
    let requested = Rc::new(Cell::new(0));
    let loaded: Rc<RefCell<Vec<Option<Sprite>>>> = Rc::default();
    let r = requested.clone();
    test.observer.on_sprite_requested_callback.set(move |_| r.set(r.get() + 1));
    let l = loaded.clone();
    test.observer.on_sprite_loaded_callback.set(move |sprite| l.borrow_mut().push(sprite));

    test.load_json(r#"{"version": 8, "sources": {}, "layers": []}"#);

    assert_eq!(requested.get(), 0);
    assert_eq!(loaded.take(), [None]);
}

// The observer hears a source change both when it comes from a file source
// response, as a TileJSON, and when it's made through the map's style, as
// new GeoJSON data, which it hears once the style is no longer borrowed.
#[test]
fn source_changes_reach_the_observer() {
    let test = MapTest::with_mode(1.0, MapMode::Continuous);
    test.file_source.set_response(Kind::Source, |_| Some(file_response("test/fixtures/resources/source_vector.json")));

    let changed: Rc<RefCell<Vec<String>>> = Rc::default();
    let (c, run_loop) = (changed.clone(), test.run_loop.clone());
    test.observer.on_source_changed_callback.set(move |source_id| {
        c.borrow_mut().push(source_id);
        run_loop.stop();
    });

    test.load_json(
        r#"{
            "version": 8,
            "sources": {
                "described": {"type": "vector", "url": "test://tiles.json"},
                "points": {"type": "geojson", "data": {"type": "Point", "coordinates": [0, 0]}}
            },
            "layers": []
        }"#,
    );
    run_with_timeout(&test.run_loop, 10);
    assert_eq!(changed.take(), ["described"]);

    {
        let mut map = test.map();
        let style = map.get_style_mut();
        let source = style.get_source("points").unwrap().as_any().downcast_ref::<GeoJSONSource>().unwrap();
        source.set_geojson(&GeoJSON::Geometry(Geometry::Point(Point::new(1.0, 1.0)))).unwrap();
        assert!(changed.borrow().is_empty());
    }
    assert_eq!(changed.take(), ["points"]);
}

const ISSUE12432_STYLE: &str = r#"{
      "version": 8,
      "sources": {
        "mapbox": {
          "type": "vector",
          "tiles": ["http://example.com/{z}-{x}-{y}.vector.pbf"]
        }
      },
      "layers": [{
        "id": "water",
        "type": "fill",
        "source": "mapbox",
        "source-layer": "water"
      }]
    }"#;

// https://github.com/mapbox/mapbox-gl-native/issues/12432
// gtest: Map.Issue12432
#[test]
fn issue12432() {
    let test = MapTest::with_mode(1.0, MapMode::Continuous);

    test.file_source.set_response(Kind::Tile, |_| Some(file_response("test/fixtures/map/issue12432/0-0-0.mvt")));

    test.load_json(ISSUE12432_STYLE);

    let map = test.weak_map();
    let run_loop = test.run_loop.clone();
    test.observer.did_finish_loading_map_callback.set(move |_| {
        if let Some(map) = map.upgrade() {
            map.borrow_mut().get_style_mut().load_json(
                r#"{
          "version": 8,
          "sources": {
            "mapbox": {
              "type": "vector",
              "tiles": ["http://example.com/{z}-{x}-{y}.vector.pbf"]
            }
          },
          "layers": [{
            "id": "water",
            "type": "line",
            "source": "mapbox",
            "source-layer": "water"
          }]
        }"#,
            );
        }

        run_loop.stop();
    });

    run_with_timeout(&test.run_loop, 60);
}

// https://github.com/mapbox/mapbox-gl-native/issues/15216
// gtest: Map.Issue15216
#[test]
fn issue15216() {
    let test = MapTest::with_mode(1.0, MapMode::Continuous);
    test.map().get_style_mut().add_source(Box::new(ImageSource::new("ImageSource", [LatLng::default(); 4]))).unwrap();
    test.map().get_style_mut().add_layer(Box::new(RasterLayer::new("RasterLayer", "ImageSource")), None).unwrap();
    // Passes, if there is no assertion hit.
    test.run_loop.run_once();
}

// https://github.com/maplibre/maplibre-native/pull/4753
// An image source wholly beyond the latitudes Web Mercator can show has an
// empty tile cover, which used to crash RenderImageSource::update.
// gtest: Map.ImageSourceBeyondMercatorLatitudes
#[test]
fn image_source_beyond_mercator_latitudes() {
    let test = MapTest::with_mode(1.0, MapMode::Continuous);
    test.load_json(&read_file("test/fixtures/api/empty.json"));

    let source = ImageSource::new("image", [ll(88.0, -10.0), ll(88.0, 10.0), ll(86.0, 10.0), ll(86.0, -10.0)]);
    source.set_image(PremultipliedImage::new(Size::new(4, 4)));
    test.map().get_style_mut().add_source(Box::new(source)).unwrap();
    test.map().get_style_mut().add_layer(Box::new(RasterLayer::new("image", "image")), None).unwrap();

    // Passes, if rendering a frame doesn't crash.
    test.frontend.render_frame();
}

// https://github.com/mapbox/mapbox-gl-native/issues/15342
// Tests the fix for constant repaint caused by `RenderSource::hasFadingTiles()`
// returning `true` all the time.
// gtest: Map.Issue15342
#[test]
fn issue15342() {
    let test = MapTest::with_mode(1.0, MapMode::Continuous);

    test.file_source.set_response(Kind::Tile, |_| Some(file_response("test/fixtures/map/issue12432/0-0-0.mvt")));
    test.map().jump_to(&cam().with_zoom(Some(3.0)));
    test.load_json(ISSUE12432_STYLE);

    let map = test.weak_map();
    let observer = Rc::downgrade(&test.observer);
    let run_loop = test.run_loop.clone();
    test.observer.did_finish_loading_map_callback.set(move |_| {
        let (Some(map), Some(observer)) = (map.upgrade(), observer.upgrade()) else {
            return;
        };
        map.borrow_mut().get_style_mut().load_json(
            r#"{
          "version": 8,
          "sources": {
            "mapbox": {
              "type": "vector",
              "tiles": ["http://example.com/{z}-{x}-{y}.vector.pbf"]
            }
          },
          "layers": []
        }"#,
        );
        map.borrow_mut().jump_to(&cam().with_zoom(Some(20.0)));
        let run_loop = run_loop.clone();
        observer.did_finish_rendering_frame_callback.set(move |status| {
            if !status.needs_repaint {
                run_loop.stop();
            }
        });
    });

    run_with_timeout(&test.run_loop, 60);
}

// gtest: Map.UniversalStyleGetter
#[test]
fn universal_style_getter() {
    let test = MapTest::new();

    test.load_json(
        r#"{
        "sources": {
            "mapbox": {
                "type": "vector",
                "tiles": ["http://example.com/{z}-{x}-{y}.vector.pbf"]
            }
        },
        "layers": [{
            "id": "line",
            "type": "line",
            "source": "mapbox",
            "paint": {
                "line-color": "red",
                "line-opacity": 0.5,
                "line-width": ["get", "width"],
                "line-opacity-transition": {
                    "duration": 400,
                    "delay": 500
                }
            },
            "layout": {
                "line-cap": "butt"
            }
        }]
        }"#,
    );

    let map = test.map();
    let style = map.get_style();
    let line_layer = style.get_layer("line").expect("line layer");

    let nonexistent = line_layer.get_property("nonexistent");
    assert_eq!(*nonexistent.get_value(), Value::Null);
    assert_eq!(StylePropertyKind::Undefined, nonexistent.get_kind());

    let undefined = line_layer.get_property("line-blur");
    assert_eq!(*undefined.get_value(), Value::Null);
    assert_eq!(StylePropertyKind::Undefined, undefined.get_kind());

    let line_color = line_layer.get_property("line-color");
    assert_ne!(*line_color.get_value(), Value::Null);
    assert_eq!(StylePropertyKind::Expression, line_color.get_kind());
    let Value::Array(color) = line_color.get_value() else {
        panic!("line-color: {:?}", line_color.get_value());
    };
    assert_eq!(5, color.len());
    assert_eq!(Value::String("rgba".into()), color[0]);
    assert_eq!(Value::Double(255.0), color[1]);
    assert_eq!(Value::Double(0.0), color[2]);
    assert_eq!(Value::Double(0.0), color[3]);
    assert_eq!(Value::Double(1.0), color[4]);

    let line_opacity = line_layer.get_property("line-opacity");
    assert_ne!(*line_opacity.get_value(), Value::Null);
    assert_eq!(StylePropertyKind::Constant, line_opacity.get_kind());
    assert_eq!(Value::Double(0.5), *line_opacity.get_value());

    let line_opacity_transition = line_layer.get_property("line-opacity-transition");
    assert_ne!(*line_opacity_transition.get_value(), Value::Null);
    assert_eq!(StylePropertyKind::Transition, line_opacity_transition.get_kind());
    let Value::Object(transition) = line_opacity_transition.get_value() else {
        panic!("line-opacity-transition: {:?}", line_opacity_transition.get_value());
    };
    assert_eq!(2, transition.len());

    let line_color_transition = line_layer.get_property("line-color-transition");
    assert_eq!(StylePropertyKind::Undefined, line_color_transition.get_kind());

    let line_width = line_layer.get_property("line-width");
    assert_ne!(*line_width.get_value(), Value::Null);
    assert_eq!(StylePropertyKind::Expression, line_width.get_kind());
    let Value::Array(expression) = line_width.get_value() else {
        panic!("line-width: {:?}", line_width.get_value());
    };

    assert_eq!(2, expression.len());
    assert_eq!(Value::String("number".into()), expression[0]);
    let Value::Array(operation) = &expression[1] else {
        panic!("line-width operation: {:?}", expression[1]);
    };
    assert_eq!(2, operation.len());
    assert_eq!(Value::String("get".into()), operation[0]);
    assert_eq!(Value::String("width".into()), operation[1]);

    let line_cap = line_layer.get_property("line-cap");
    assert_ne!(*line_cap.get_value(), Value::Null);
    assert_eq!(StylePropertyKind::Constant, line_cap.get_kind());
    assert_eq!(Value::String("butt".into()), *line_cap.get_value());
}

// gtest: Map.NoHangOnMissingImage
#[test]
fn no_hang_on_missing_image() {
    let test = MapTest::new();

    test.file_source.set_response(Kind::Tile, |_| Some(file_response("test/fixtures/map/issue12432/0-0-0.mvt")));
    test.file_source.set_response(Kind::SpriteImage, |_| Some(file_response("test/fixtures/resources/sprite.png")));
    test.file_source.set_response(Kind::SpriteJSON, |_| Some(file_response("test/fixtures/resources/sprite.json")));

    let style = r#"{
      "version": 8,
      "sprite": "http://example.com/sprites/sprite",
      "sources": {
        "mapbox": {
          "type": "vector",
          "tiles": ["http://example.com/{z}-{x}-{y}.vector.pbf"]
        }
      },
      "layers": [{
            "id": "background",
            "type": "background",
            "paint": {"background-color": "white"}
        },{
            "id": "water",
            "type": "fill",
            "source": "mapbox",
            "source-layer": "water",
            "paint": {"fill-pattern": "missing"}
      }]
    }"#;
    test.load_json(style);
    test.render();

    test.load_json(style);
    let default_camera = test.map().get_style().get_default_camera();
    test.map().jump_to(&default_camera);
    // The test passes if the following call does not hang.
    test.render();
}

// gtest: Map.EmptyImageNameIsNoImage
#[test]
fn empty_image_name_is_no_image() {
    let test = MapTest::new();
    let missing = Rc::new(RefCell::new(Vec::<String>::new()));
    {
        let missing = missing.clone();
        test.observer.on_style_image_missing_callback.set(move |id| missing.borrow_mut().push(id));
    }

    test.load_json(
        r#"{
      "version": 8,
      "sources": {
        "polygon": {"type": "geojson", "data": {"type": "Feature", "properties": {"image": ""},
          "geometry": {"type": "Polygon", "coordinates": [[[-10, -10], [10, -10], [10, 10], [-10, 10], [-10, -10]]]}}}
      },
      "layers": [
        {"id": "fill", "type": "fill", "source": "polygon", "paint": {"fill-pattern": ["get", "image"]}},
        {"id": "symbol", "type": "symbol", "source": "polygon", "layout": {"icon-image": ["get", "image"]}}
      ]
    }"#,
    );
    test.render();

    assert!(missing.borrow().is_empty(), "{:?}", missing.borrow());
}

// gtest: Map.PrefetchDeltaOverride
#[test]
fn prefetch_delta_override() {
    let test = MapTest::with_mode(1.0, MapMode::Continuous);

    test.load_json(
        r#"{
                "layers": [{
                    "id": "vector",
                    "type": "fill",
                    "source": "vector",
                    "minzoom": 0,
                    "maxzoom": 24
                },
                {
                    "id": "custom",
                    "type": "fill",
                    "source": "custom",
                    "minzoom": 0,
                    "maxzoom": 24
                }]
                }"#,
    );

    // Vector source
    let vector = vector_source("vector", "a/{z}/{x}/{y}");
    source::set_prefetch_zoom_delta(&vector, Some(0));
    test.map().get_style_mut().add_source(Box::new(vector)).unwrap();

    let requested_tiles = Arc::new(AtomicI32::new(0));
    {
        let requested_tiles = requested_tiles.clone();
        test.file_source.set_response(Kind::Tile, move |_| {
            requested_tiles.fetch_add(1, Ordering::SeqCst);
            no_content()
        });
    }

    // Custom source
    test.map().get_style_mut().add_source(Box::new(counted_custom_source(&requested_tiles, Some(0)))).unwrap();

    test.map().jump_to(&cam().with_zoom(Some(16.0)));
    {
        let run_loop = test.run_loop.clone();
        test.observer.did_finish_loading_map_callback.set(move |_| run_loop.stop());
    }
    test.run_loop.run();
    // 2 sources x 4 tiles
    assert_eq!(requested_tiles.load(Ordering::SeqCst), 8);

    requested_tiles.store(0, Ordering::SeqCst);

    // Should request z12 tiles when delta is set back to default, that is 4.
    stop_on_full_frame(&test);

    for id in ["vector", "custom"] {
        let map = test.map();
        source::set_prefetch_zoom_delta(map.get_style().get_source(id).unwrap(), None);
    }
    test.run_loop.run();

    // Each source requests 4 additional parent tiles.
    assert_eq!(requested_tiles.load(Ordering::SeqCst), 8);
}

/// A custom geometry source "custom" that counts its fetches in
/// `requested_tiles` and answers each with empty data.
fn counted_custom_source(requested_tiles: &Arc<AtomicI32>, prefetch_zoom_delta: Option<u8>) -> CustomGeometrySource {
    let handle = Arc::new(OnceLock::<CustomGeometrySourceHandle>::new());
    let fetch: TileFunction = {
        let handle = handle.clone();
        let requested_tiles = requested_tiles.clone();
        Arc::new(move |tile_id: &CanonicalTileID| {
            requested_tiles.fetch_add(1, Ordering::SeqCst);
            if let Some(handle) = handle.get() {
                handle.set_tile_data(*tile_id, GeoJSON::default());
            }
        })
    };
    let options = CustomGeometrySourceOptions {
        fetch_tile_function: Some(fetch),
        cancel_tile_function: Some(Arc::new(|_: &CanonicalTileID| {})),
        ..Default::default()
    };
    let custom = CustomGeometrySource::new("custom", options).unwrap();
    handle.set(custom.handle()).unwrap();
    if prefetch_zoom_delta.is_some() {
        source::set_prefetch_zoom_delta(&custom, prefetch_zoom_delta);
    }
    custom
}

/// Stops the run loop at the next full frame.
fn stop_on_full_frame<FS: mln_storage::FileSource + 'static>(test: &MapTest<FS>) {
    let run_loop = test.run_loop.clone();
    test.observer.did_finish_rendering_frame_callback.set(move |status: RenderFrameStatus| {
        if status.mode == RenderMode::Full {
            run_loop.stop();
        }
    });
}

// Test that custom source's tile pyramid is reset
// if there is a significant change.
// gtest: Map.PrefetchDeltaOverrideCustomSource
#[test]
fn prefetch_delta_override_custom_source() {
    let test = MapTest::with_mode(1.0, MapMode::Continuous);

    test.load_json(
        r#"{
                "layers": [{
                    "id": "custom",
                    "type": "fill",
                    "source": "custom",
                    "source-layer": "a",
                    "minzoom": 0,
                    "maxzoom": 24
                }]
                }"#,
    );

    let requested_tiles = Arc::new(AtomicI32::new(0));

    test.map().get_style_mut().add_source(Box::new(counted_custom_source(&requested_tiles, Some(0)))).unwrap();

    test.map().jump_to(&cam().with_zoom(Some(16.0)));
    {
        let run_loop = test.run_loop.clone();
        test.observer.did_finish_loading_map_callback.set(move |_| run_loop.stop());
    }
    test.run_loop.run();
    assert_eq!(requested_tiles.load(Ordering::SeqCst), 4);
    requested_tiles.store(0, Ordering::SeqCst);

    stop_on_full_frame(&test);

    {
        let mut map = test.map();
        let mut style = map.get_style_mut();
        let layer = style.remove_layer("custom").unwrap();
        style.remove_source("custom");
        style.add_layer(layer, None).unwrap();
        style.add_source(Box::new(counted_custom_source(&requested_tiles, None))).unwrap();
    }
    test.run_loop.run();

    // Source was significantly mutated, therefore, tile pyramid would be
    // cleared and current zoom level + parent tiles would be re-requested.
    assert_eq!(requested_tiles.load(Ordering::SeqCst), 8);
}

/// The `ForwardingHeadlessFrontend` of Map.ExpiredSpriteSheet: a
/// HeadlessFrontend whose `update` first calls `check_params`.
#[derive(Clone)]
struct ForwardingHeadlessFrontend {
    frontend: HeadlessFrontend,
    check_params: Rc<RefCell<Option<Box<dyn Fn(&UpdateParameters)>>>>,
}

impl RendererFrontend for ForwardingHeadlessFrontend {
    fn reset(&self) {
        self.frontend.reset();
    }
    fn set_observer(&self, observer: Weak<dyn RendererObserver>) {
        self.frontend.set_observer(observer);
    }
    fn update(&self, parameters: Arc<UpdateParameters>) {
        if let Some(check_params) = self.check_params.borrow().as_ref() {
            check_params(&parameters);
        }
        self.frontend.update(parameters);
    }
}

// Test verifies that Style::Impl::onSpriteLoaded does not create duplicate
// images when we get new spritesheet from the server and we merge it with
// currently used spritesheet.
// gtest: Map.ExpiredSpriteSheet
#[test]
fn expired_sprite_sheet() {
    let server = HttpServer::new();

    // `MapTest<MainResourceLoader, ForwardingHeadlessFrontend>
    // test{":memory:", ".", 1, MapMode::Continuous}`.
    let run_loop = RunLoop::new();
    let file_sources = FileSourceManager::new();
    let file_source = Arc::new(MainResourceLoader::new(
        &file_sources,
        &ResourceOptions::default().with_cache_path(":memory:").with_asset_path("."),
    ));
    let observer = Rc::new(StubMapObserver::default());
    let frontend =
        ForwardingHeadlessFrontend { frontend: HeadlessFrontend::with_pixel_ratio(1.0), check_params: Rc::default() };
    let options = MapOptions::new()
        .with_map_mode(MapMode::Continuous)
        .with_size(frontend.frontend.get_size())
        .with_pixel_ratio(1.0);
    let map = Rc::new(RefCell::new(Map::with_file_source(&frontend, observer.clone(), file_source, options).unwrap()));

    // The server's own address replaces 127.0.0.1:3000 in the fixtures, as in
    // what it serves.
    let make_response = |path: &str, expires: bool| {
        let mut response = Response::new();
        response.data = Some(Arc::new(server.online_fixture(path)));
        response.expires = Some(if expires { Timestamp::from_secs(0) } else { Timestamp::from_secs(i64::MAX) });
        response
    };

    {
        let check_params = frontend.check_params.clone();
        let weak_observer = Rc::downgrade(&observer);
        let run_loop = run_loop.clone();
        observer.did_finish_loading_map_callback.set(move |_| {
            *check_params.borrow_mut() = Some(Box::new(|params: &UpdateParameters| {
                assert!(params.images.windows(2).all(|pair| pair[0].id <= pair[1].id));
                assert!(params.images.len() < 4);
            }));

            NetworkStatus::set(NetworkStatusType::Online);

            if let Some(observer) = weak_observer.upgrade() {
                let run_loop = run_loop.clone();
                observer.did_become_idle_callback.set(move |_| run_loop.stop());
            }
        });
    }

    NetworkStatus::set(NetworkStatusType::Offline);
    let prefix = server.url("/online/");
    let dbfs = database_file_source(&file_sources);
    dbfs.forward(&Resource::style(&format!("{prefix}style.json")), &make_response("style.json", false), no_op());
    dbfs.forward(&Resource::source(&format!("{prefix}streets.json")), &make_response("streets.json", false), no_op());
    dbfs.forward(&Resource::sprite_json(&format!("{prefix}sprite"), 1.0), &make_response("sprite.json", true), no_op());
    dbfs.forward(&Resource::sprite_image(&format!("{prefix}sprite"), 1.0), &make_response("sprite.png", true), no_op());
    // C++ loads the style in this callback; see Map.Offline for why the port
    // runs the loop until the callback has run and then loads it.
    let handle = run_loop.handle();
    dbfs.forward(
        &Resource::tile(
            &format!("{prefix}{{z}}-{{x}}-{{y}}.vector.pbf"),
            1.0,
            0,
            0,
            0,
            Scheme::XYZ,
            LoadingMethod::All,
        ),
        &make_response("0-0-0.vector.pbf", false),
        Some(Box::new(move || handle.stop())),
    );
    run_with_timeout(&run_loop, 60);
    map.borrow_mut().get_style_mut().load_url(&format!("{prefix}style.json"));

    run_with_timeout(&run_loop, 60);
}

// gtest: Map.SourceMinimumUpdateIntervalOverride
#[test]
fn source_minimum_update_interval_override() {
    let test = MapTest::with_mode(1.0, MapMode::Continuous);

    test.load_json(
        r#"{
                "layers": [{
                    "id": "a",
                    "type": "fill",
                    "source": "source-a",
                    "minzoom": 0,
                    "maxzoom": 24
                },
                {
                    "id": "b",
                    "type": "fill",
                    "source": "source-b",
                    "minzoom": 0,
                    "maxzoom": 24
                }]
                }"#,
    );

    // Vector source
    let vector_source_a = vector_source("source-a", "a/{z}/{x}/{y}");
    mln_style::source::set_minimum_tile_update_interval(&vector_source_a, Duration::from_secs(1));
    test.map().get_style_mut().add_source(Box::new(vector_source_a)).unwrap();

    let vector_source_b = vector_source("source-b", "b/{z}/{x}/{y}");
    test.map().get_style_mut().add_source(Box::new(vector_source_b)).unwrap();

    let requested_tiles_a = Rc::new(Cell::new(0));
    let requested_tiles_b = Rc::new(Cell::new(0));
    {
        let (a, b) = (requested_tiles_a.clone(), requested_tiles_b.clone());
        test.file_source.set_response(Kind::Tile, move |resource: &Resource| {
            assert!(!resource.url.is_empty());
            let first_symbol = resource.url.as_bytes()[0];
            if first_symbol == b'a' {
                assert_eq!(Duration::from_secs(1), resource.minimum_update_interval);
                a.set(a.get() + 1);
            } else if first_symbol == b'b' {
                assert_eq!(Duration::zero(), resource.minimum_update_interval);
                b.set(b.get() + 1);
            } else {
                panic!("Never reached");
            }

            no_content()
        });
    }

    test.map().jump_to(&cam().with_zoom(Some(16.0)));
    let run_loop = test.run_loop.clone();
    test.observer.did_finish_loading_map_callback.set(move |_| run_loop.stop());
    run_with_timeout(&test.run_loop, 60);

    assert_eq!(8, requested_tiles_a.get());
    assert_eq!(8, requested_tiles_b.get());
}

/// The `makeResponse(file, incrementCounter)` of map.test.cpp.
fn make_response(file: &'static str, counter: Option<Rc<Cell<i32>>>) -> impl Fn(&Resource) -> Option<Response> {
    move |_| {
        if let Some(counter) = &counter {
            counter.set(counter.get() + 1);
        }
        Some(file_response(&format!("test/fixtures/resources/{file}")))
    }
}

// gtest: Map.KeepRenderData
#[test]
fn keep_render_data() {
    let test = MapTest::new();
    let requests_count = Rc::new(Cell::new(0));

    test.file_source.set_response(Kind::Tile, make_response("vector.tile", Some(requests_count.clone())));
    test.file_source.set_response(Kind::Glyphs, make_response("glyphs.pbf", Some(requests_count.clone())));
    // The resources below belong to style and requested on style re-load.
    test.file_source.set_response(Kind::Style, make_response("style_vector.json", None));
    test.file_source.set_response(Kind::Source, make_response("source_vector.json", None));
    test.file_source.set_response(Kind::SpriteJSON, make_response("sprite.json", None));
    test.file_source.set_response(Kind::SpriteImage, make_response("sprite.png", None));

    test.map().jump_to(&cam().with_zoom(Some(10.0)));
    test.map().get_style_mut().load_url("maptiler://maps/streets");
    let iterations = 3;
    let resources_count = 4; /*tiles*/

    requests_count.set(0);
    // Keep render data.
    for _ in 1..=iterations {
        test.render();
        assert_eq!(resources_count, requests_count.get());
    }

    requests_count.set(0);
    // Clear render data.
    for i in 1..=iterations {
        test.frontend.with_renderer(|renderer| renderer.clear_data());
        test.render();
        assert_eq!(resources_count * i, requests_count.get());
    }
}

// Disabled intentionally, no symbol placement occurs in MapTiler streets style
// gtest: Map.DISABLED_PlacedSymbolData
#[test]
#[ignore = "DISABLED_ in C++ (no symbol placement occurs in the MapTiler streets style fixture)"]
fn disabled_placed_symbol_data() {
    fn is_inside_tile(box_: &mln_util::grid_index::BBox, padding: f32, viewport_size: Size) -> bool {
        if box_.min.x - padding < 0.0 {
            return false;
        }
        if box_.min.y - padding < 0.0 {
            return false;
        }
        if box_.max.x - padding > viewport_size.width as f32 {
            return false;
        }
        if box_.max.y - padding > viewport_size.height as f32 {
            return false;
        }
        true
    }

    let run_loop = RunLoop::new();
    let file_source = Arc::new(StubFileSource::new(ResponseType::Asynchronous));
    let test = MapTest::create(run_loop, file_source, 1.0, MapMode::Tile);
    let requests_count = Rc::new(Cell::new(0));

    test.file_source.set_response(Kind::Tile, make_response("vector.tile", Some(requests_count.clone())));
    test.file_source.set_response(Kind::Glyphs, make_response("glyphs.pbf", Some(requests_count.clone())));
    test.file_source.set_response(Kind::Style, make_response("style_vector.json", None));
    test.file_source.set_response(Kind::Source, make_response("source_vector.json", None));
    test.file_source.set_response(Kind::SpriteJSON, make_response("sprite.json", None));
    test.file_source.set_response(Kind::SpriteImage, make_response("sprite.png", None));

    // Camera options will give exactly one tile (12/1171/1566)
    test.map().jump_to(&cam().with_zoom(Some(16.0)).with_center(Some(ll(50.072317, 14.444827))));

    test.map().get_style_mut().load_url("maptiler://maps/streets");
    let viewport_size = test.frontend.get_size();
    test.frontend.with_renderer(|renderer| renderer.collect_placed_symbol_data(true));
    test.render();

    let placed_symbols = test.frontend.with_renderer(|renderer| renderer.get_placed_symbols_data());
    assert!(!placed_symbols.is_empty());

    let mut placed_text_inside_tile = 0;
    let mut placed_text = 0;
    let mut placed_icon_inside_tile = 0;
    let mut placed_icon = 0;
    let mut placed_total = 0;

    let symbol_layers: HashSet<String> = test
        .map()
        .get_style()
        .get_layers()
        .iter()
        .filter(|l| l.base_impl().get_type_info().type_ == "symbol")
        .map(|l| l.get_id())
        .collect();

    for placed_symbol in &placed_symbols {
        assert!(symbol_layers.contains(&placed_symbol.layer));
        if placed_symbol.text_placed {
            if let Some(box_) = &placed_symbol.text_collision_box {
                if is_inside_tile(box_, placed_symbol.viewport_padding, viewport_size) {
                    assert!(!placed_symbol.intersects_tile_border);
                    placed_text_inside_tile += 1;
                }
                placed_text += 1;
            }
        }
        if placed_symbol.icon_placed {
            if let Some(box_) = &placed_symbol.icon_collision_box {
                if is_inside_tile(box_, placed_symbol.viewport_padding, viewport_size) {
                    assert!(!placed_symbol.intersects_tile_border);
                    placed_icon_inside_tile += 1;
                }
                placed_icon += 1;
            }
        }
        placed_total += 1;
    }
    assert_eq!(1, placed_text_inside_tile);
    assert_eq!(28, placed_text);

    assert_eq!(2, placed_icon_inside_tile);
    assert_eq!(29, placed_icon);

    assert_eq!(50, placed_total);
    test.frontend.with_renderer(|renderer| renderer.collect_placed_symbol_data(false));
    test.render();

    assert!(test.frontend.with_renderer(|renderer| renderer.get_placed_symbols_data()).is_empty());
}

// gtest: Map.VolatileSource
#[test]
fn volatile_source() {
    let test = MapTest::with_mode(1.0, MapMode::Continuous);

    let requested_tiles = Rc::new(Cell::new(0));
    let is_volatile = true;
    {
        let requested_tiles = requested_tiles.clone();
        test.file_source.set_response(Kind::Tile, move |resource: &Resource| {
            let expected_policy = if is_volatile { StoragePolicy::Volatile } else { StoragePolicy::Permanent };
            assert_eq!(expected_policy, resource.storage_policy);
            requested_tiles.set(requested_tiles.get() + 1);
            no_content()
        });
    }

    test.load_json(
        r#"{
      "version": 8,
      "layers": [{
        "id": "water",
        "type": "fill",
        "source": "vector",
        "source-layer": "water"
      }]
    }"#,
    );
    let source = vector_source("vector", "a/{z}/{x}/{y}");
    mln_style::source::set_volatile(&source, is_volatile);
    test.map().get_style_mut().add_source(Box::new(source)).unwrap();

    test.map().jump_to(&cam().with_zoom(Some(16.0)));
    let run_loop = test.run_loop.clone();
    test.observer.did_finish_loading_map_callback.set(move |_| run_loop.stop());
    run_with_timeout(&test.run_loop, 60);
    assert_eq!(8, requested_tiles.get());
}

// gtest: Map.ObserveShaderRegistration
#[test]
#[ignore = "TODO(port): MapObserver::onRegisterShaders (gfx::ShaderRegistry observer hook); see map.test.cpp:1671"]
fn observe_shader_registration() {
    // test/map/map.test.cpp:1671: the first render calls onRegisterShaders,
    // the second does not.
}

// gtest: Map.ResourceError
#[test]
fn resource_error() {
    let test = MapTest::new();
    test.file_source.set_response(Kind::Glyphs, |_| {
        let mut response = Response::new();
        response.error = Some(Box::new(ResponseError::new(Reason::Server, "Font file Server failed")));
        Some(response)
    });

    test.load_json(
        r#"
{
  "version": 8,
  "zoom": 0,
  "center": [-14.41400, 39.09187],
  "sources": {
    "mapbox": {
      "type": "geojson",
      "data": {
        "type": "FeatureCollection",
        "features": [
          {
            "type": "Feature",
            "properties": {
              "name": "ជនជាប់សង្ស័យ"
            },
            "geometry": {
              "type": "LineString",
              "coordinates": [
                [
                  -14.4195556640625,
                  39.091699613104595
                ],
                [
                  102.3046875,
                  39.36827914916014
                ]
              ]
            }
          }
        ]
      }
    }
  },
  "glyphs": "local://glyphs/{fontstack}/{range}.pbf",
  "fonts": "local://glyphs/{fontstack}/{language}.pbf",
  "layers": [
    {
      "id": "background",
      "type": "background",
      "paint": {
        "background-color": "white"
      }
    },
    {
      "id": "lines-symbol",
      "type": "symbol",
      "source": "mapbox",
      "layout": {
        "text-field": "{name}",
        "symbol-placement": "point",
        "symbol-spacing": 150,
        "text-allow-overlap": true,
        "text-font": [ "abc" ],
        "text-size": 24
      }
    }
  ]
}"#,
    );
    // C++: try { render } catch (...) { EXPECT_EQ(toString(error), "Font file Server failed"); }
    if let Err(error) = test.try_render() {
        assert_eq!(error, "Font file Server failed");
    }
}

/// Answers each request of `kind` once, as a file source that doesn't retry
/// does: with an error while the returned flag is set, then with `response`.
/// Also returns the request count.
fn fail_requests(test: &MapTest, kind: Kind, response: Response) -> (Rc<Cell<bool>>, Rc<Cell<usize>>) {
    let failing = Rc::new(Cell::new(true));
    let count = Rc::new(Cell::new(0));
    let (f, c) = (failing.clone(), count.clone());
    test.file_source.set_response(kind, move |_| {
        c.set(c.get() + 1);
        if f.get() {
            let mut failed = Response::new();
            failed.error = Some(Box::new(ResponseError::new(Reason::Server, "failed")));
            Some(failed)
        } else {
            Some(response.clone())
        }
    });
    (failing, count)
}

fn assert_still_image_retries_failed_tile(source: &str, layer: &str) {
    let test = MapTest::new();
    let mut empty = Response::new();
    empty.no_content = true;
    let (failing, requests) = fail_requests(&test, Kind::Tile, empty);
    test.load_json(&format!(r#"{{"version": 8, "sources": {{"tiles": {source}}}, "layers": [{layer}]}}"#));

    assert_eq!(test.try_render().err().as_deref(), Some("failed"));
    assert_eq!(requests.get(), 1);

    failing.set(false);
    assert!(test.try_render().is_ok());
    assert_eq!(requests.get(), 2);

    // Nothing failed, so nothing is requested again.
    assert!(test.try_render().is_ok());
    assert_eq!(requests.get(), 2);
}

// gtest: Map.StillImageRetriesFailedTile
#[test]
fn still_image_retries_failed_tile() {
    assert_still_image_retries_failed_tile(
        r#"{"type": "vector", "tiles": ["http://example.com/{z}/{x}/{y}.pbf"], "maxzoom": 0}"#,
        r#"{"id": "fill", "type": "fill", "source": "tiles", "source-layer": "any"}"#,
    );
}

#[test]
fn still_image_retries_failed_raster_tile() {
    assert_still_image_retries_failed_tile(
        r#"{"type": "raster", "tiles": ["http://example.com/{z}/{x}/{y}.png"], "maxzoom": 0}"#,
        r#"{"id": "raster", "type": "raster", "source": "tiles"}"#,
    );
}

// gtest: Map.StillImageRetriesFailedGlyphRange
#[test]
fn still_image_retries_failed_glyph_range() {
    let test = MapTest::new();
    let (failing, requests) = fail_requests(&test, Kind::Glyphs, file_response("test/fixtures/resources/glyphs.pbf"));
    test.load_json(
        r#"{
        "version": 8,
        "glyphs": "http://example.com/{fontstack}/{range}.pbf",
        "sources": {"point": {"type": "geojson", "data": {"type": "Point", "coordinates": [0, 0]}}},
        "layers": [{"id": "label", "type": "symbol", "source": "point", "layout": {"text-field": "A"}}]
    }"#,
    );
    let labels = |test: &MapTest| {
        let size = test.frontend.get_size();
        let center = ScreenCoordinate::new(f64::from(size.width) / 2.0, f64::from(size.height) / 2.0);
        let options = mln_renderer::query::RenderedQueryOptions::default();
        test.frontend
            .with_renderer(|renderer| renderer.query_rendered_features_at_point(&center, &options))
            .unwrap()
            .len()
    };

    assert_eq!(test.try_render().err().as_deref(), Some("failed"));
    assert_eq!(requests.get(), 1);
    assert_eq!(labels(&test), 0);

    // The tile laid out without the range lays out again with it.
    failing.set(false);
    assert!(test.try_render().is_ok());
    assert_eq!(requests.get(), 2);
    assert_eq!(labels(&test), 1);

    assert!(test.try_render().is_ok());
    assert_eq!(requests.get(), 2);
}

// gtest: Map.StencilOverflow
#[test]
fn stencil_overflow() {
    let test = MapTest::new();

    test.load_json("{}");

    // Create a tile source with small tiles that produces enough tiles to
    // exceed the stencil ID limit and causes a clearing of the stencil buffer.
    let handle = Arc::new(OnceLock::<CustomGeometrySourceHandle>::new());
    let fetch: TileFunction = {
        let handle = handle.clone();
        Arc::new(move |tile_id: &CanonicalTileID| {
            if let Some(handle) = handle.get() {
                handle.set_tile_data(*tile_id, GeoJSON::default());
            }
        })
    };
    let mut options = CustomGeometrySourceOptions { fetch_tile_function: Some(fetch), ..Default::default() };
    options.tile_options.tile_size = mln_util::constants::TILE_SIZE_I / 32;
    let custom = CustomGeometrySource::new("custom", options).unwrap();
    handle.set(custom.handle()).unwrap();

    test.map().get_style_mut().add_source(Box::new(custom)).unwrap();
    test.map().get_style_mut().add_layer(Box::new(FillLayer::new("fill", "custom")), None).unwrap();

    test.map().jump_to(&cam().with_zoom(Some(5.0)));
    let result = test.render();

    // No drawables are built because no bucket/tiledata is available.
    assert!(0 <= result.stats.stencil_updates);

    // TODO: confirm that the stencil masking actually worked
}

// gtest: Map.InvalidUTF8InTile
#[test]
fn invalid_utf8_in_tile() {
    let _log = FixtureLog::new();

    let test = MapTest::with_mode(1.0, MapMode::Continuous);

    test.file_source
        .set_response(Kind::Tile, |_| Some(file_response("test/fixtures/map/invalid_utf8/invalidutf8feature.mvt")));
    test.file_source.set_response(Kind::Glyphs, make_response("glyphs.pbf", Some(Rc::new(Cell::new(0)))));

    test.map().jump_to(&cam().with_zoom(Some(11.0)));
    test.load_json(
        r#"{
      "version": 8,
      "sources": {
        "invalidutf8": {
          "type": "vector",
          "tiles": ["http://example.com/{z}-{x}-{y}.vector.pbf"]
        }
      },
      "layers": [{
        "id": "mountain_peak",
        "type": "symbol",
        "source": "invalidutf8",
        "source-layer": "points",
        "layout": {
            "text-field": "{name}"
        }
      }]
    }"#,
    );

    // The map reports that it finished loading after the frame that
    // finished it. When that frame already needs no repaint (the symbol
    // fade ended before the last tile's placement, as on a slow or loaded
    // machine), no frame follows, so the C++ test, which stops only on a
    // later frame, waits forever: stop on that frame's status instead.
    let last_needs_repaint = Rc::new(Cell::new(true));
    {
        let last_needs_repaint = last_needs_repaint.clone();
        test.observer
            .did_finish_rendering_frame_callback
            .set(move |status| last_needs_repaint.set(status.needs_repaint));
    }
    let observer = Rc::downgrade(&test.observer);
    let run_loop = test.run_loop.clone();
    test.observer.did_finish_loading_map_callback.set(move |_| {
        if !last_needs_repaint.get() {
            run_loop.stop();
            return;
        }
        let Some(observer) = observer.upgrade() else {
            return;
        };
        let run_loop = run_loop.clone();
        observer.did_finish_rendering_frame_callback.set(move |status| {
            if !status.needs_repaint {
                run_loop.stop();
            }
        });
    });

    run_with_timeout(&test.run_loop, 60);
}

// gtest: Map.ObserveTileLifecycle
// TODO(port): the half of the gtest that checks the pre/post compile events
// of the fill and fill-outline shaders (MapObserver has no shader callbacks).
#[test]
fn observe_tile_lifecycle() {
    let _run_loop = RunLoop::new();

    let tile_ops: Rc<RefCell<Vec<(OverscaledTileID, TileOperation)>>> = Rc::default();
    // We expect to see a valid lifecycle for every tile in this list.
    let expected_tiles = [
        OverscaledTileID::with_zxy(10, 0, 10, 163, 395),
        OverscaledTileID::with_zxy(10, 0, 10, 163, 396),
        OverscaledTileID::with_zxy(10, 0, 10, 164, 395),
        OverscaledTileID::with_zxy(10, 0, 10, 164, 396),
        // Lower zooms can also be seen, but not always, so we ignore them.
    ];

    let observer = Rc::new(StubMapObserver::default());
    {
        let tile_ops = tile_ops.clone();
        observer.on_tile_action_callback.set(move |(op, id, source_id)| {
            if source_id == "mapbox" {
                tile_ops.borrow_mut().push((id, op));
            }
        });
    }

    let mut frontend = HeadlessFrontend::new(Size::new(512, 512), 1.0);
    let mut map = Map::with_resource_options(
        &frontend,
        observer.clone(),
        MapOptions::new().with_map_mode(MapMode::Static).with_size(frontend.get_size()),
        &FileSourceManager::new(),
        &ResourceOptions::default()
            .with_cache_path(":memory:")
            .with_asset_path(cpp_root().join("test/fixtures/api/assets").to_str().unwrap()),
    )
    .unwrap();
    map.get_style_mut().load_json(&read_file("test/fixtures/api/water.json"));
    let mut layer = FillLayer::new("landcover", "mapbox");
    layer.set_source_layer("landcover");
    layer.set_fill_color(&PropertyValue::Constant(Color::new(0.0, 1.0, 0.0, 1.0)));
    map.get_style_mut().add_layer(Box::new(layer), None).unwrap();
    map.jump_to(&cam().with_center(Some(ll(37.8, -122.5))).with_zoom(Some(10.0)));
    frontend.render(&mut map).unwrap();

    use TileOperation::*;
    let tile_ops = tile_ops.borrow();
    for tile in expected_tiles {
        let mut stage = NullOp;
        let mut parsing = false;

        for (index, (id, op)) in tile_ops.iter().enumerate() {
            if *id != tile {
                continue;
            }
            let context = format!("{tile} op {index}: {op:?} after {stage:?}");
            match op {
                RequestedFromCache => {
                    assert_eq!(stage, NullOp, "{context}");
                }
                RequestedFromNetwork => {
                    // Parsing happens concurrently with the file source
                    // request and can start and stop between requests
                    assert!(matches!(stage, StartParse | EndParse | LoadFromCache), "{context}");
                }
                LoadFromNetwork => {
                    // Parsing happens concurrently with the file source
                    // request and can start and stop between requests
                    assert!(matches!(stage, StartParse | EndParse | RequestedFromNetwork), "{context}");
                }
                LoadFromCache => {
                    assert!(matches!(stage, RequestedFromCache | StartParse), "{context}");
                }
                StartParse => {
                    // Parsing is expected to be started early during the
                    // request process by a call to `setLayers`
                    assert!(
                        matches!(stage, RequestedFromCache | RequestedFromNetwork | LoadFromCache | LoadFromNetwork),
                        "{context}"
                    );
                    // We must not already be parsing when seeing this marker.
                    assert!(!parsing, "{context}");
                    parsing = true;
                }
                Cancelled => {
                    assert!(
                        matches!(
                            stage,
                            RequestedFromCache | RequestedFromNetwork | LoadFromNetwork | LoadFromCache | StartParse
                        ),
                        "{context}"
                    );
                    parsing = false;
                }
                EndParse => {
                    // A parse that a newer response interrupts isn't
                    // cancelled, so EndParse can follow the network request
                    // or response as well as StartParse.
                    assert!(matches!(stage, StartParse | LoadFromNetwork | RequestedFromNetwork), "{context}");
                    // We must have been parsing to see the EndParse marker.
                    assert!(parsing, "{context}");
                    parsing = false;
                }
                NullOp | Error => panic!("{context}"),
            }
            stage = *op;
        }

        assert!(matches!(stage, EndParse | Cancelled), "{tile} ends at {stage:?}");
        assert!(!parsing, "{tile} is still parsing");
    }
}

// gtest: Map.EmptyPlacementDoesNotRequestRepaint
#[test]
fn empty_placement_does_not_request_repaint() {
    let test = MapTest::with_mode(1.0, MapMode::Continuous);
    let status = Rc::new(Cell::new(None));
    {
        let status = status.clone();
        test.observer.did_finish_rendering_frame_callback.set(move |value| status.set(Some(value)));
    }
    test.load_json(
        r#"{
        "version": 8, "sources": {}, "transition": {"duration": 0},
        "layers": [{"id": "background", "type": "background", "paint": {"background-color": "white"}}]
    }"#,
    );
    test.run_loop.run_once();

    // Reuse the same update timestamp to stay within the placement interval.
    for frame in 0..2 {
        status.set(None);
        test.frontend.render_frame();
        let value = status.take().expect("rendered a frame");
        assert!(!value.needs_repaint, "frame {frame}");
    }

    test.map().get_style_mut().set_transition_options(&TransitionOptions {
        duration: Some(Duration::from_millis(300)),
        ..TransitionOptions::default()
    });
    {
        let mut map = test.map();
        let mut style = map.get_style_mut();
        let mut layer = style.get_layer_mut("background").expect("background");
        layer
            .downcast_mut::<BackgroundLayer>()
            .expect("BackgroundLayer")
            .set_background_color(&PropertyValue::Constant(Color::red()));
    }
    test.run_loop.run_once();
    status.set(None);
    test.frontend.render_frame();
    let value = status.take().expect("rendered a frame");
    assert!(value.needs_repaint);
}

// gtest: BackgroundLayer.ImmediateStyleReplacementRetainsColor
#[test]
fn immediate_style_replacement_retains_color() {
    let test = MapTest::new();
    for iteration in 0..2 {
        // The underlay keeps the tested background out of the clear-color optimization.
        test.load_json(
            r#"{
            "version": 8,
            "sources": {},
            "layers": [{"id": "underlay", "type": "background", "paint": {"background-color": "white"}}]
        }"#,
        );
        let mut layer = BackgroundLayer::new("background");
        layer.set_background_color(&PropertyValue::Constant(Color::red()));
        test.map().get_style_mut().add_layer(Box::new(layer), None).unwrap();

        let image = test.render().image;
        assert_eq!(center_pixel(&image), [255, 0, 0, 255], "iteration {iteration}");
    }
}

// gtest: BackgroundLayer.SwitchesBetweenSolidAndPatternedDrawables
#[test]
fn switches_between_solid_and_patterned_drawables() {
    let test = MapTest::new();
    // The underlay forces shader selection through the drawable path.
    test.load_json(
        r#"{
        "version": 8,
        "sources": {},
        "layers": [
            {"id": "underlay", "type": "background", "paint": {"background-color": "white"}},
            {"id": "background", "type": "background", "paint": {"background-color": "red"}}
        ]
    }"#,
    );
    let blue = vec![0u8, 0, 255, 255];
    test.map().get_style_mut().add_image(style_image("blue", PremultipliedImage::from_vec(Size::new(1, 1), blue), 1.0));
    let set_pattern = |value: PropertyValue<mln_style::expression::Image>| {
        let mut map = test.map();
        let mut style = map.get_style_mut();
        let mut layer = style.get_layer_mut("background").expect("background");
        layer.downcast_mut::<BackgroundLayer>().expect("BackgroundLayer").set_background_pattern(&value);
    };
    let expect_color = |red: u8, blue_channel: u8| {
        let image = test.render().image;
        let pixel = center_pixel(&image);
        assert_eq!(pixel, [red, 0, blue_channel, 255]);
    };
    expect_color(255, 0);
    set_pattern(PropertyValue::Constant(mln_style::expression::Image::new("blue")));
    expect_color(0, 255);
    set_pattern(PropertyValue::Undefined);
    expect_color(255, 0);
}

// gtest: BackgroundLayer.StyleUpdateZoomDependency
#[test]
fn style_update_zoom_dependency() {
    use mln_style::expression::Value as ExpressionValue;
    use mln_style::expression::dsl::{interpolate, linear, literal, zoom};

    let test = MapTest::new();
    test.load_json(&read_file("test/fixtures/map/style_update_zoom_dependency/style.json"));
    test.map().jump_to(&cam().with_zoom(Some(0.0)));

    // Initial render: background should be red at zoom 0
    check_image("test/fixtures/map/style_update_zoom_dependency/initial_red", &test.render().image, 0.0006, 0.1);

    // Update background layer to use zoom-dependent color
    {
        let mut map = test.map();
        let mut style = map.get_style_mut();
        let mut layer = style.get_layer_mut("background").expect("background");
        layer.downcast_mut::<BackgroundLayer>().expect("BackgroundLayer").set_background_color(
            &PropertyValue::Expression(PropertyExpression::from_box(
                interpolate(
                    linear(),
                    zoom(),
                    vec![
                        (0.0, literal(ExpressionValue::Color(Color::red()))),
                        (14.0, literal(ExpressionValue::Color(Color::green()))),
                    ],
                ),
                None,
            )),
        );
    }

    test.render();

    // Change zoom to 14 and re-render
    test.map().jump_to(&cam().with_zoom(Some(14.0)));
    check_image("test/fixtures/map/style_update_zoom_dependency/after_update_green", &test.render().image, 0.0006, 0.1);
}

// gtest: Map.LineLayerDepthDistribution
#[test]
#[ignore = "TODO(port): annotations: in C++ the AnnotationManager's points layer is the style's first layer, so the background added here is drawn per tile; the port has no annotation layer, so the background becomes the clear color and also fills the 5 rows above the pitched world that C++ leaves transparent (measured: 0.01953125 > 0.0006, exactly those 5 rows); see map.test.cpp:2105"]
fn line_layer_depth_distribution() {
    let test = MapTest::new();

    test.load_json(&read_file("test/fixtures/api/empty.json"));

    test.map().jump_to(&cam().with_zoom(Some(0.0)).with_pitch(Some(45.0)));

    let mut background_layer = BackgroundLayer::new("Background");
    background_layer.set_background_color(&PropertyValue::Constant(Color::new(1.0, 1.0, 0.0, 1.0)));
    test.map().get_style_mut().add_layer(Box::new(background_layer), None).unwrap();

    const LAYER_COUNT: u32 = 1 << 11;
    let colors = [Color::red(), Color::green(), Color::blue()];
    let bounds = {
        let map = test.map();
        map.lat_lng_bounds_for_camera(&map.get_camera_options(None)).unwrap()
    };

    let get_source = |index: u32| {
        let y = (bounds.north() - bounds.south()) * 0.95 / f64::from(LAYER_COUNT) * f64::from(index + 1) / 2.0;
        let x = (bounds.east() - bounds.west()) * 0.95 / f64::from(LAYER_COUNT) * f64::from(index + 1) / 2.0;

        let geo = vec![Point::new(-x, y), Point::new(x, y), Point::new(x, -y), Point::new(-x, -y), Point::new(-x, y)];

        let features = vec![Feature::new(Geometry::LineString(geo))];

        let source = GeoJSONSource::with_default_options(format!("GeoJSONSource_{index}"));
        source.set_geojson(&GeoJSON::FeatureCollection(features)).unwrap();

        source
    };

    for i in 0..LAYER_COUNT {
        // add one source per layer
        test.map().get_style_mut().add_source(Box::new(get_source(i))).unwrap();

        let mut layer = LineLayer::new(&format!("LineLayer{i}"), &format!("GeoJSONSource_{i}"));
        layer.set_line_color(&PropertyValue::Constant(colors[i as usize % colors.len()]));

        test.map().get_style_mut().add_layer(Box::new(layer), None).unwrap();
    }

    check_image("test/fixtures/map/layer_depth_distribution/line", &test.render().image, 0.0006, 0.1);
}

// gtest: Map.SetFrustumOffset
#[test]
#[ignore = "TODO(port): annotations: in C++ the AnnotationManager's points layer is the style's first layer, so the background added here is drawn as a drawable inside the frustum-offset scissor; the port has no annotation layer, so the background becomes the clear color, which fills the whole frame (measured on `after`: 0.628662109375 > 0.0006; `before` passes); see map.test.cpp:2156"]
fn set_frustum_offset() {
    let test = MapTest::new();

    test.load_json(&read_file("test/fixtures/api/empty.json"));

    test.map().get_style_mut().add_layer(red_background("background"), None).unwrap();

    check_image("test/fixtures/map/setFrustumOffset/before", &test.render().image, 0.0006, 0.1);

    test.map().set_frustum_offset(&insets(50.0, 50.0, 50.0, 50.0));

    check_image("test/fixtures/map/setFrustumOffset/after", &test.render().image, 0.0006, 0.1);
}

/// A `width` x `height` opaque red image.
fn red_image(width: u32, height: u32) -> PremultipliedImage {
    let pixels = [255, 0, 0, 255].repeat((width * height) as usize);
    PremultipliedImage::from_vec(Size::new(width, height), pixels)
}

/// The shared setup of the GeoJSON symbol tests: a 512x512 map with a white
/// background, glyphs from test/fixtures/resources/glyphs.pbf, counting the
/// glyph requests.
fn geojson_symbol_map() -> (MapTest, Rc<Cell<u32>>) {
    let test = MapTest::new();
    test.frontend.clone().set_size(Size::new(512, 512));
    test.map().set_size(Size::new(512, 512)).unwrap();

    let glyph_requests = Rc::new(Cell::new(0));
    let counter = glyph_requests.clone();
    test.file_source.set_response(Kind::Glyphs, move |_| {
        counter.set(counter.get() + 1);
        Some(file_response("test/fixtures/resources/glyphs.pbf"))
    });
    test.load_json(
        r#"{
        "version": 8,
        "glyphs": "https://example.com/{fontstack}/{range}.pbf",
        "sources": {},
        "layers": [{"id": "background", "type": "background", "paint": {"background-color": "white"}}]
    }"#,
    );
    (test, glyph_requests)
}

/// Adds a GeoJSON source with a point at `longitude` and a symbol layer
/// drawing the "marker" icon above it and, `with_text`, a label below it.
fn add_geojson_symbol(test: &MapTest, index: usize, longitude: f64, synchronous: bool, with_text: bool) {
    let source_id = format!("point-{index}");
    let options = GeoJSONOptions { synchronous_update: synchronous, ..GeoJSONOptions::default() };
    let source = GeoJSONSource::new(source_id.clone(), Arc::new(options));
    source.set_geojson(&GeoJSON::Geometry(Geometry::Point(Point::new(longitude, 0.0)))).unwrap();
    test.map().get_style_mut().add_source(Box::new(source)).unwrap();

    let text = if with_text { "New place" } else { "" };
    let layer_json = format!(
        r#"{{
            "id": "symbol-{index}", "type": "symbol", "source": "{source_id}",
            "layout": {{
                "icon-image": "marker", "icon-anchor": "bottom",
                "icon-allow-overlap": true, "icon-ignore-placement": true,
                "text-field": "{text}", "text-font": ["Open Sans Regular"], "text-anchor": "top",
                "text-allow-overlap": true, "text-ignore-placement": true
            }}
        }}"#
    );
    let layer_value: serde_json::Value = serde_json::from_str(&layer_json).unwrap();
    let mut error = ConversionError::default();
    let layer = convert_layer(&Convertible::from(&layer_value), &mut error);
    let layer = layer.unwrap_or_else(|| panic!("{}", error.message));
    test.map().get_style_mut().add_layer(layer, None).unwrap();
}

// gtest: GeoJSON/GeoJSONSymbolTest.RetainsIconsAndTextAcrossZoom
fn retains_icons_and_text_across_zoom(synchronous: bool, with_text: bool) {
    let (test, glyph_requests) = geojson_symbol_map();
    test.map().get_style_mut().add_image(style_image("marker", red_image(40, 48), 1.0));
    for i in 0..3 {
        add_geojson_symbol(&test, i, (i as f64 - 1.0) * 0.01, synchronous, with_text);
    }

    for zoom in [14.0, 13.5, 13.0, 12.5, 14.0] {
        test.map().jump_to(&cam().with_center(Some(ll(0.0, 0.0))).with_zoom(Some(zoom)));
        let image = test.render().image;
        let mut red_pixels = 0;
        let mut black_pixels = 0;
        for pixel in image.data.chunks_exact(4) {
            red_pixels += usize::from(pixel[0] > 200 && pixel[1] < 50 && pixel[2] < 50);
            black_pixels += usize::from(pixel[0] < 100 && pixel[1] < 100 && pixel[2] < 100);
        }
        assert!(red_pixels >= 5000, "zoom {zoom}: {red_pixels} red pixels");
        if with_text {
            assert!(black_pixels > 100, "zoom {zoom}: {black_pixels} black pixels");
            assert_eq!(glyph_requests.get(), 1, "zoom {zoom}");
        }
    }
}

// gtest: GeoJSON/GeoJSONSymbolTest.RetainsIconsAndTextAcrossZoom/AsynchronousIconOnly
#[test]
fn geojson_symbol_retains_icons_and_text_across_zoom_asynchronous_icon_only() {
    retains_icons_and_text_across_zoom(false, false);
}

// gtest: GeoJSON/GeoJSONSymbolTest.RetainsIconsAndTextAcrossZoom/AsynchronousWithText
#[test]
fn geojson_symbol_retains_icons_and_text_across_zoom_asynchronous_with_text() {
    retains_icons_and_text_across_zoom(false, true);
}

// gtest: GeoJSON/GeoJSONSymbolTest.RetainsIconsAndTextAcrossZoom/SynchronousIconOnly
#[test]
fn geojson_symbol_retains_icons_and_text_across_zoom_synchronous_icon_only() {
    retains_icons_and_text_across_zoom(true, false);
}

// gtest: GeoJSON/GeoJSONSymbolTest.RetainsIconsAndTextAcrossZoom/SynchronousWithText
#[test]
fn geojson_symbol_retains_icons_and_text_across_zoom_synchronous_with_text() {
    retains_icons_and_text_across_zoom(true, true);
}

/// The RGBA pixel at `x`, `y`.
fn pixel_at(image: &PremultipliedImage, x: u32, y: u32) -> [u8; 4] {
    let offset = ((y * image.size.width + x) * 4) as usize;
    image.data[offset..offset + 4].try_into().unwrap()
}

// gtest: GeoJSON/LateStyleImageTest.RendersImageAddedAfterLayout
fn renders_image_added_after_layout(synchronous: bool, with_text: bool) {
    let (test, _) = geojson_symbol_map();
    add_geojson_symbol(&test, 0, 0.0, synchronous, with_text);

    // Complete tile layout with the image missing, then add it without
    // changing the camera or layers.
    let icon_size = Size::new(32, 32);
    let before = test.render().image;
    let x = before.size.width / 2;
    let y = before.size.height / 2 - icon_size.height / 2;
    assert_eq!(pixel_at(&before, x, y)[..3], [255, 255, 255]);

    test.map().get_style_mut().add_image(style_image("marker", red_image(icon_size.width, icon_size.height), 1.0));
    let after = test.render().image;
    assert_eq!(pixel_at(&after, x, y), [255, 0, 0, 255]);
}

// gtest: GeoJSON/LateStyleImageTest.RendersImageAddedAfterLayout/AsynchronousIconOnly
#[test]
fn late_style_image_renders_image_added_after_layout_asynchronous_icon_only() {
    renders_image_added_after_layout(false, false);
}

// gtest: GeoJSON/LateStyleImageTest.RendersImageAddedAfterLayout/AsynchronousWithText
#[test]
fn late_style_image_renders_image_added_after_layout_asynchronous_with_text() {
    renders_image_added_after_layout(false, true);
}

// gtest: GeoJSON/LateStyleImageTest.RendersImageAddedAfterLayout/SynchronousIconOnly
#[test]
fn late_style_image_renders_image_added_after_layout_synchronous_icon_only() {
    renders_image_added_after_layout(true, false);
}

// gtest: GeoJSON/LateStyleImageTest.RendersImageAddedAfterLayout/SynchronousWithText
#[test]
fn late_style_image_renders_image_added_after_layout_synchronous_with_text() {
    renders_image_added_after_layout(true, true);
}

// gtest: Map.UnrelatedImagePreservesTileLayoutAndCache
#[test]
fn unrelated_image_preserves_tile_layout_and_cache() {
    let test = MapTest::new();
    test.frontend.clone().set_size(Size::new(128, 128));
    test.map().set_size(Size::new(128, 128)).unwrap();
    test.map().set_prefetch_zoom_delta(0);
    test.load_json(
        r#"{
        "version": 8,
        "sources": {"point": {"type": "geojson", "data": {"type": "Point", "coordinates": [-90, 40]}}},
        "layers": [{"id": "symbol", "type": "symbol", "source": "point", "layout": {"icon-image": "marker"}}]
    }"#,
    );
    test.map().jump_to(&cam().with_center(Some(ll(40.0, -90.0))).with_zoom(Some(3.0)));
    let parses = Rc::new(Cell::new(0));
    {
        let parses = parses.clone();
        test.observer.on_tile_action_callback.set(move |(op, _, _)| {
            if op == TileOperation::StartParse {
                parses.set(parses.get() + 1);
            }
        });
    }
    test.render();
    assert!(parses.get() > 0);

    // Move the original tiles into the cache and finish layout in another
    // region.
    test.map().jump_to(&cam().with_center(Some(ll(40.0, 0.0))).with_zoom(Some(3.0)));
    test.render();
    parses.set(0);
    test.map().get_style_mut().add_image(style_image("unrelated", red_image(16, 16), 1.0));
    test.render();
    assert_eq!(parses.get(), 0);

    test.map().jump_to(&cam().with_center(Some(ll(40.0, -90.0))).with_zoom(Some(3.0)));
    test.render();
    assert_eq!(parses.get(), 0);
}

// Port-only: an image added after layout that a `coalesce` skipped as
// unavailable relayouts its tile, which then draws it instead of the
// fallback.
#[test]
fn coalesced_image_added_after_layout_replaces_fallback() {
    let test = MapTest::new();
    test.frontend.clone().set_size(Size::new(128, 128));
    test.map().set_size(Size::new(128, 128)).unwrap();
    test.load_json(
        r#"{
        "version": 8,
        "sources": {"point": {"type": "geojson", "data": {"type": "Point", "coordinates": [0, 0]}}},
        "layers": [
            {"id": "background", "type": "background", "paint": {"background-color": "white"}},
            {"id": "symbol", "type": "symbol", "source": "point", "layout": {
                "icon-image": ["coalesce", ["image", "late"], ["image", "fallback"]],
                "icon-allow-overlap": true, "icon-ignore-placement": true
            }}
        ]
    }"#,
    );
    let blue = PremultipliedImage::from_vec(Size::new(32, 32), [0, 0, 255, 255].repeat(32 * 32));
    test.map().get_style_mut().add_image(style_image("fallback", blue, 1.0));
    let parses = Rc::new(Cell::new(0));
    {
        let parses = parses.clone();
        test.observer.on_tile_action_callback.set(move |(op, _, _)| {
            if op == TileOperation::StartParse {
                parses.set(parses.get() + 1);
            }
        });
    }
    let before = test.render().image;
    assert_eq!(pixel_at(&before, 64, 64), [0, 0, 255, 255]);

    parses.set(0);
    test.map().get_style_mut().add_image(style_image("late", red_image(32, 32), 1.0));
    let after = test.render().image;
    assert!(parses.get() > 0);
    assert_eq!(pixel_at(&after, 64, 64), [255, 0, 0, 255]);
}

// End-to-end: a feature-state change must alter the *rendered* output, not just
// the value read back through the API. A fill covering the viewport is colored
// by a data-driven expression on feature-state "active" (blue by default, red
// when set), and we sample the centre pixel of the rendered frame after each
// change. This exercises the full path: setFeatureState -> coalesce during
// prepare -> data-driven paint re-evaluation -> pixels.
// gtest: Map.FeatureStateChangesRenderedStyle
#[test]
fn feature_state_changes_rendered_style() {
    let test = MapTest::new();

    test.load_json(
        r##"{
      "version": 8,
      "sources": {
        "fs": {
          "type": "geojson",
          "data": {
            "type": "Feature",
            "id": 1,
            "properties": {},
            "geometry": {
              "type": "Polygon",
              "coordinates": [[[-180, -85], [180, -85], [180, 85], [-180, 85], [-180, -85]]]
            }
          }
        }
      },
      "layers": [{
        "id": "fill",
        "type": "fill",
        "source": "fs",
        "paint": {
          "fill-color": ["case", ["boolean", ["feature-state", "active"], false], "#ff0000", "#0000ff"]
        }
      }]
    }"##,
    );

    test.map().jump_to(&cam().with_center(Some(ll(0.0, 0.0))).with_zoom(Some(0.0)));

    // Reads one channel (0=R, 1=G, 2=B) of the centre pixel
    let center_channel =
        |image: &PremultipliedImage, channel: usize| -> i32 { i32::from(center_pixel(image)[channel]) };

    // Default state: the fill renders blue
    {
        let result = test.render();
        assert!(center_channel(&result.image, 0) < 40, "red channel (default)");
        assert!(center_channel(&result.image, 2) > 200, "blue channel (default)");
    }

    // Setting feature-state "active" flips the rendered fill to red on the next
    // frame, with no style edit
    let mut active = mln_style::expression::expression::FeatureState::new();
    active.insert("active".into(), Value::Bool(true));
    test.frontend.with_renderer(|renderer| renderer.set_feature_state("fs", None, "1", &active));
    {
        let result = test.render();
        assert!(center_channel(&result.image, 0) > 200, "red channel (active)");
        assert!(center_channel(&result.image, 2) < 40, "blue channel (active)");
    }

    // Removing the state reverts the rendered fill to blue.
    test.frontend.with_renderer(|renderer| renderer.remove_feature_state("fs", None, Some("1"), None));
    {
        let result = test.render();
        assert!(center_channel(&result.image, 0) < 40, "red channel (removed)");
        assert!(center_channel(&result.image, 2) > 200, "blue channel (removed)");
    }
}

fn load_selectable_halves_style(test: &MapTest) {
    test.load_json(
        r##"{
      "version": 8,
      "sources": {
        "fs": {
          "type": "geojson",
          "data": {
            "type": "FeatureCollection",
            "features": [{
              "type": "Feature",
              "id": 1,
              "properties": {},
              "geometry": {
                "type": "Polygon",
                "coordinates": [[[-180, -85], [0, -85], [0, 85], [-180, 85], [-180, -85]]]
              }
            }, {
              "type": "Feature",
              "id": 2,
              "properties": {},
              "geometry": {
                "type": "Polygon",
                "coordinates": [[[0, -85], [180, -85], [180, 85], [0, 85], [0, -85]]]
              }
            }]
          }
        }
      },
      "layers": [{
        "id": "fill",
        "type": "fill",
        "source": "fs",
        "paint": {
          "fill-color": ["case", ["boolean", ["feature-state", "selected"], false], "#ff0000", "#0000ff"]
        }
      }]
    }"##,
    );
    test.map().jump_to(&cam().with_center(Some(ll(0.0, 0.0))).with_zoom(Some(0.0)));
}

/// Whether feature 1 (the left half) and feature 2 (the right half) render as
/// selected.
fn render_selected_halves(test: &MapTest) -> (bool, bool) {
    let image = test.render().image;
    let is_selected = |x: usize| {
        let offset = (image.size.height as usize / 2) * image.stride() + x * 4;
        image.data[offset] > 200 && image.data[offset + 2] < 40
    };
    let width = image.size.width as usize;
    (is_selected(width / 4), is_selected(width * 3 / 4))
}

fn feature_state(key: &str) -> mln_style::expression::expression::FeatureState {
    let mut state = mln_style::expression::expression::FeatureState::new();
    state.insert(key.into(), Value::Bool(true));
    state
}

fn set_state(test: &MapTest, feature_id: &str, state: &mln_style::expression::expression::FeatureState) {
    test.frontend.with_renderer(|renderer| renderer.set_feature_state("fs", None, feature_id, state));
}

fn remove_state(test: &MapTest, feature_id: Option<&str>, state_key: Option<&str>) {
    test.frontend.with_renderer(|renderer| renderer.remove_feature_state("fs", None, feature_id, state_key));
}

// gtest: Map.FeatureStateRemovalAndChangeOfAnotherFeatureBeforeOneRender
#[test]
fn feature_state_removal_and_change_of_another_feature_before_one_render() {
    let test = MapTest::new();
    load_selectable_halves_style(&test);
    assert_eq!(render_selected_halves(&test), (false, false));
    let selected = feature_state("selected");

    set_state(&test, "1", &selected);
    assert_eq!(render_selected_halves(&test), (true, false));

    remove_state(&test, Some("1"), None);
    set_state(&test, "2", &selected);
    assert_eq!(render_selected_halves(&test), (false, true));
}

// gtest: Map.FeatureStateKeyRemovalAndChangeOfSameKeyBeforeOneRender
#[test]
fn feature_state_key_removal_and_change_of_same_key_before_one_render() {
    let test = MapTest::new();
    load_selectable_halves_style(&test);
    assert_eq!(render_selected_halves(&test), (false, false));
    let selected = feature_state("selected");

    set_state(&test, "1", &selected);
    assert_eq!(render_selected_halves(&test), (true, false));

    remove_state(&test, Some("1"), Some("selected"));
    set_state(&test, "1", &selected);
    assert_eq!(render_selected_halves(&test), (true, false));

    set_state(&test, "1", &selected);
    remove_state(&test, Some("1"), Some("selected"));
    assert_eq!(render_selected_halves(&test), (false, false));
}

// gtest: Map.FeatureStateChangeAndRemovalOfSameFeatureBeforeOneRender
#[test]
fn feature_state_change_and_removal_of_same_feature_before_one_render() {
    let test = MapTest::new();
    load_selectable_halves_style(&test);
    assert_eq!(render_selected_halves(&test), (false, false));

    set_state(&test, "1", &feature_state("selected"));
    assert_eq!(render_selected_halves(&test), (true, false));

    set_state(&test, "1", &feature_state("hovered"));
    remove_state(&test, Some("1"), None);
    assert_eq!(render_selected_halves(&test), (false, false));
}

// gtest: Map.FeatureStateKeyRemovalsOfTwoFeaturesBeforeOneRender
#[test]
fn feature_state_key_removals_of_two_features_before_one_render() {
    let test = MapTest::new();
    load_selectable_halves_style(&test);
    assert_eq!(render_selected_halves(&test), (false, false));
    let selected = feature_state("selected");

    set_state(&test, "1", &selected);
    set_state(&test, "2", &selected);
    assert_eq!(render_selected_halves(&test), (true, true));

    remove_state(&test, Some("1"), Some("selected"));
    remove_state(&test, Some("2"), Some("selected"));
    assert_eq!(render_selected_halves(&test), (false, false));
}

// gtest: Map.FeatureStateChangeSourceRemovalAndChangeBeforeOneRender
#[test]
fn feature_state_change_source_removal_and_change_before_one_render() {
    let test = MapTest::new();
    load_selectable_halves_style(&test);
    assert_eq!(render_selected_halves(&test), (false, false));
    let selected = feature_state("selected");

    set_state(&test, "1", &selected);
    remove_state(&test, None, None);
    set_state(&test, "2", &selected);
    assert_eq!(render_selected_halves(&test), (false, true));
}

// gtest: Map.FeatureStateSourceRemovalAndChangeBeforeOneRender
#[test]
fn feature_state_source_removal_and_change_before_one_render() {
    let test = MapTest::new();
    load_selectable_halves_style(&test);
    assert_eq!(render_selected_halves(&test), (false, false));
    let selected = feature_state("selected");

    set_state(&test, "1", &selected);
    set_state(&test, "2", &selected);
    assert_eq!(render_selected_halves(&test), (true, true));

    remove_state(&test, None, None);
    set_state(&test, "2", &selected);
    assert_eq!(render_selected_halves(&test), (false, true));
}

fn count_green_pixels(image: &PremultipliedImage) -> usize {
    let mut green_pixels = 0;
    for pixel in image.data.chunks_exact(4) {
        if pixel[0] < 40 && pixel[1] > 200 && pixel[2] < 40 {
            green_pixels += 1;
        }
    }
    green_pixels
}

// gtest: Map.LocationIndicatorAccuracyColorsTransition
#[test]
#[ignore = "TODO(port): RenderLocationIndicatorLayer (the location-indicator layer type is registered in the style but not rendered); see map.test.cpp:2255"]
fn location_indicator_accuracy_colors_transition() {
    let test = MapTest::with_mode(1.0, MapMode::Continuous);
    test.load_json(
        r#"{
      "version": 8,
      "sources": {},
      "layers": [{
        "id": "location",
        "type": "location-indicator",
        "paint": {
          "location": [0, 0, 0],
          "accuracy-radius": 100,
          "accuracy-radius-color": "blue",
          "accuracy-radius-border-color": "white"
        }
      }]
    }"#,
    );
    test.map().jump_to(&cam().with_center(Some(ll(0.0, 0.0))).with_zoom(Some(16.0)));

    let render_until_idle = || {
        let idle = Rc::new(Cell::new(false));
        let timeout = Timer::new();
        {
            let run_loop = test.run_loop.clone();
            timeout.start(Duration::from_secs(5), Duration::zero(), move || run_loop.stop());
        }
        {
            let idle = idle.clone();
            let run_loop = test.run_loop.clone();
            test.observer.did_finish_rendering_frame_callback.set(move |status| {
                if status.mode == RenderMode::Full && !status.needs_repaint {
                    idle.set(true);
                    run_loop.stop();
                }
            });
        }
        test.run_loop.run();
        test.observer.did_finish_rendering_frame_callback.clear();
        idle.get()
    };
    let center_channel = |image: &PremultipliedImage, channel: usize| center_pixel(image)[channel];

    let read_image = || {
        let _scope = BackendScope::new(test.frontend.get_backend(), ScopeType::Explicit);
        test.frontend.read_still_image()
    };

    assert!(render_until_idle());
    let initial = read_image();
    assert!(center_channel(&initial, 2) > 240);
    assert!(center_channel(&initial, 0) < 10);

    {
        let mut map = test.map();
        let mut style = map.get_style_mut();
        let mut layer = style.get_layer_mut("location").expect("location layer");
        let layer = layer.downcast_mut::<LocationIndicatorLayer>().expect("LocationIndicatorLayer");
        let transition =
            TransitionOptions { duration: Some(Duration::from_millis(100)), ..TransitionOptions::default() };
        layer.set_accuracy_radius_color_transition(&transition);
        layer.set_accuracy_radius_border_color_transition(&transition);
        layer.set_accuracy_radius_color(&PropertyValue::Constant(Color::red()));
        layer.set_accuracy_radius_border_color(&PropertyValue::Constant(Color::new(0.0, 1.0, 0.0, 1.0)));
    }

    assert!(render_until_idle());
    let result = read_image();
    assert!(center_channel(&result, 0) > 240);
    assert!(center_channel(&result, 2) < 10);
    assert!(count_green_pixels(&result) > 0, "The accuracy border should finish transitioning to green");
}

const CIRCLE_STYLE: &str = r#"{"version":8,"sources":{"points":{"type":"geojson","data":{
    "type":"Point","coordinates":[0,0]}}},"layers":[{"id":"circle","type":"circle","source":"points"}]}"#;

// gtest: Map.RenderingStatisticsTrackDrawsAcrossFrames
#[test]
fn rendering_statistics_track_draws_across_frames() {
    let test = MapTest::new();
    test.load_json(CIRCLE_STYLE);
    let first = test.render().stats;
    assert!(first.num_frames > 0);
    assert!(first.num_draw_calls > 0);
    assert!(first.total_draw_calls >= first.num_draw_calls);

    let second = test.render().stats;
    assert_eq!(first.num_frames + 1, second.num_frames);
    assert!(second.num_draw_calls > 0);
    assert_eq!(first.total_draw_calls + second.num_draw_calls, second.total_draw_calls);

    test.map().get_style_mut().get_layer_mut("circle").expect("circle layer").set_visibility(VisibilityType::None);
    let empty = test.render().stats;
    assert_eq!(second.num_frames + 1, empty.num_frames);
    assert_eq!(0, empty.num_draw_calls);
    assert_eq!(second.total_draw_calls, empty.total_draw_calls);
}

// The map observer gets each frame's rendering stats with its status.
#[test]
fn frame_status_carries_rendering_stats() {
    let test = MapTest::with_mode(1.0, MapMode::Continuous);
    let last = Rc::new(RefCell::new(None::<RenderFrameStatus>));
    {
        let last = last.clone();
        let run_loop = test.run_loop.clone();
        test.observer.did_finish_rendering_frame_callback.set(move |status| {
            let done = status.mode == RenderMode::Full && !status.needs_repaint;
            *last.borrow_mut() = Some(status);
            if done {
                run_loop.stop();
            }
        });
    }
    test.load_json(CIRCLE_STYLE);
    run_with_timeout(&test.run_loop, 10);
    test.observer.did_finish_rendering_frame_callback.clear();

    let status = last.borrow_mut().take().expect("rendered a frame");
    let stats = &status.rendering_stats;
    assert!(stats.num_frames > 0);
    assert!(stats.num_draw_calls > 0);
    assert!(stats.total_draw_calls >= stats.num_draw_calls);
    let _scope = BackendScope::new(test.frontend.get_backend(), ScopeType::Explicit);
    assert_eq!(**stats, *test.frontend.get_backend().get_context().rendering_stats());
}

// gtest: Map.LocationIndicatorWithoutImages
#[test]
#[ignore = "TODO(port): RenderLocationIndicatorLayer; until it draws, this passes without checking anything"]
fn location_indicator_without_images() {
    let test = MapTest::new();
    test.load_json(
        r#"{
      "version": 8,
      "sources": {},
      "layers": [{
        "id": "location",
        "type": "location-indicator",
        "paint": {"location": [0, 0, 0], "accuracy-radius": 0}
      }]
    }"#,
    );
    test.map().jump_to(&cam().with_center(Some(ll(0.0, 0.0))).with_zoom(Some(16.0)));

    // Neither the accuracy circle nor any image should produce a draw call.
    assert_eq!(test.render().stats.num_draw_calls, 0);
}

/// The capturing `RendererFrontend` of TileSelectionOptionsPublishUpdates.
#[derive(Clone, Default)]
struct CapturingFrontend {
    parameters: Rc<RefCell<Option<Arc<UpdateParameters>>>>,
}

impl CapturingFrontend {
    fn parameters(&self) -> Arc<UpdateParameters> {
        self.parameters.borrow().clone().expect("parameters")
    }
}

impl RendererFrontend for CapturingFrontend {
    fn reset(&self) {}
    fn set_observer(&self, _observer: Weak<dyn RendererObserver>) {}
    fn update(&self, value: Arc<UpdateParameters>) {
        *self.parameters.borrow_mut() = Some(value);
    }
    // TODO(port): getThreadPool() (not in the Rust RendererFrontend yet).
}

/// Renders a frame and returns the RGB of the pixel at the centre of the map.
fn render_center_pixel(test: &MapTest) -> [u8; 3] {
    test.map().trigger_repaint();
    test.frontend.render_frame();
    let _scope = BackendScope::new(test.frontend.get_backend(), ScopeType::Explicit);
    let [r, g, b, _] = center_pixel(&test.frontend.read_still_image());
    [r, g, b]
}

// gtest: Map.GlobalStateChangeTransitionsPaintProperty
#[test]
fn global_state_change_transitions_paint_property() {
    let test = MapTest::with_mode(1.0, MapMode::Continuous);
    test.load_json(
        r##"{
      "version": 8,
      "transition": { "duration": 100, "delay": 0 },
      "state": { "color": { "default": "#ff0000" } },
      "sources": {},
      "layers": [{
        "id": "background",
        "type": "background",
        "paint": { "background-color": ["global-state", "color"] }
      }]
    }"##,
    );

    let initial = render_center_pixel(&test);
    assert!(initial[0] > 240, "{initial:?}");
    assert!(initial[2] < 10, "{initial:?}");

    test.map().get_style_mut().set_global_state_property("color", &Value::String("#0000ff".to_owned())).unwrap();

    // The frame that picks up the change starts the transition at its own time point, so it still shows red.
    let changed = render_center_pixel(&test);
    assert!(changed[0] > 200, "{changed:?}");
    assert!(changed[2] < 55, "{changed:?}");

    // Once the transition has run its course, the new state shows.
    std::thread::sleep(std::time::Duration::from_millis(200));
    let finished = render_center_pixel(&test);
    assert!(finished[0] < 10, "{finished:?}");
    assert!(finished[2] > 240, "{finished:?}");
}

// gtest: Map.LayerAddedWithGlobalStateChangeShowsNewState
#[test]
fn layer_added_with_global_state_change_shows_new_state() {
    let test = MapTest::with_mode(1.0, MapMode::Continuous);
    test.load_json(
        r##"{
      "version": 8,
      "transition": { "duration": 100, "delay": 0 },
      "state": { "color": { "default": "#0000ff" } },
      "sources": {},
      "layers": [{
        "id": "base",
        "type": "background",
        "paint": { "background-color": "#ff0000" }
      }]
    }"##,
    );

    let initial = render_center_pixel(&test);
    assert!(initial[0] > 240, "{initial:?}");
    assert!(initial[1] < 10, "{initial:?}");

    // The state changes and a layer reading it is added before the next frame.
    test.map().get_style_mut().set_global_state_property("color", &Value::String("#00ff00".to_owned())).unwrap();

    let layer_value: serde_json::Value = serde_json::from_str(
        r#"{"id": "added", "type": "background", "paint": {"background-color": ["global-state", "color"]}}"#,
    )
    .unwrap();
    let mut error = ConversionError::default();
    let layer = convert_layer(&Convertible::from(&layer_value), &mut error);
    let layer = layer.unwrap_or_else(|| panic!("{}", error.message));
    test.map().get_style_mut().add_layer(layer, None).unwrap();

    // The new layer never showed the prior state, so it must show the current one at once.
    let added = render_center_pixel(&test);
    assert!(added[0] < 10, "{added:?}");
    assert!(added[1] > 240, "{added:?}");
    assert!(added[2] < 10, "{added:?}");
}

// gtest: Map.TileSelectionOptionsPublishUpdates
#[test]
fn tile_selection_options_publish_updates() {
    let _loop = RunLoop::new();
    let frontend = CapturingFrontend::default();
    let observer = Rc::new(StubMapObserver::default());
    let mut map = Map::with_file_source(
        &frontend,
        observer,
        Arc::new(StubFileSource::new(ResponseType::Asynchronous)),
        MapOptions::new().with_map_mode(MapMode::Continuous),
    )
    .unwrap();
    map.get_style_mut().load_json(r#"{"version":8,"sources":{},"layers":[]}"#);
    assert!(frontend.parameters.borrow().is_some());

    fn check<T: PartialEq + std::fmt::Debug + Copy>(
        map: &mut Map,
        frontend: &CapturingFrontend,
        setter: impl Fn(&mut Map, T),
        member: impl Fn(&UpdateParameters) -> T,
        value: T,
    ) {
        let before = frontend.parameters();
        setter(map, value);
        assert!(!Arc::ptr_eq(&before, &frontend.parameters()));
        assert_eq!(value, member(&frontend.parameters()));
        let after = frontend.parameters();
        setter(map, value);
        assert!(Arc::ptr_eq(&after, &frontend.parameters()));
    }
    check(&mut map, &frontend, Map::set_prefetch_zoom_delta, |p| p.prefetch_zoom_delta, 2u8);
    check(&mut map, &frontend, Map::set_tile_lod_min_radius, |p| p.tile_lod_min_radius, 4.0);
    check(&mut map, &frontend, Map::set_tile_lod_scale, |p| p.tile_lod_scale, 2.0);
    check(&mut map, &frontend, Map::set_tile_lod_pitch_threshold, |p| p.tile_lod_pitch_threshold, 0.5);
    check(&mut map, &frontend, Map::set_tile_lod_zoom_shift, |p| p.tile_lod_zoom_shift, 1.0);
    check(&mut map, &frontend, Map::set_tile_lod_mode, |p| p.tile_lod_mode, TileLodMode::Distance);
}

// gtest: Map.FallbackFontFacesCoverEveryFontStack
#[test]
fn fallback_font_faces_cover_every_font_stack() {
    let test = MapTest::new();
    let requested = Rc::new(RefCell::new(Vec::<String>::new()));
    let r = requested.clone();
    test.file_source.set_response(Kind::Glyphs, move |resource: &Resource| {
        r.borrow_mut().push(resource.url.clone());
        let mut response = Response::new();
        response.error = Some(Box::new(ResponseError::new(Reason::NotFound, "not found")));
        Some(response)
    });

    test.map()
        .get_style_mut()
        .set_fallback_font_faces(r#"[{"url": "fallback.ttf", "unicode-range": ["U+1000-109F"]}]"#)
        .unwrap();
    test.load_json(
        r#"{
        "version": 8,
        "sources": {"point": {"type": "geojson", "data": {
            "type": "Feature", "properties": {"name": "မြန်မာ"}, "geometry": {"type": "Point", "coordinates": [0, 0]}}}},
        "glyphs": "{fontstack}/{range}.pbf",
        "layers": [{"id": "symbol", "type": "symbol", "source": "point",
                    "layout": {"text-field": ["get", "name"], "text-font": ["Any Font"]}}]
    }"#,
    );
    let _ = test.try_render();

    assert!(requested.borrow().iter().any(|url| url == "fallback.ttf"));
}

/// `ColorRampGlobalStateTest.RendersCurrentStateAfterUnrelatedStateChange`
/// for a layer type: "line", "heatmap" or "color-relief".
fn color_ramp_renders_current_state_after_unrelated_state_change(type_: &str) {
    let ramp_property = if type_ == "line" { "line-gradient".to_owned() } else { format!("{type_}-color") };
    let ramp = |color: &str| {
        let input = match type_ {
            "line" => r#"["line-progress"], 0, "#,
            "heatmap" => r#"["heatmap-density"], 0, "#,
            _ => r#"["elevation"], -20000, "#,
        };
        let end = if type_ == "color-relief" { ", 20000, " } else { ", 1, " };
        format!(r#"["interpolate", ["linear"], {input}{color}{end}{color}]"#)
    };
    let state_color = r#"["global-state", "color"]"#;

    let test = MapTest::new();
    test.file_source.set_response(Kind::Tile, |_: &Resource| {
        // Opaque black decodes to an elevation of -10000 m.
        let mut dem = PremultipliedImage::new(Size::new(256, 256));
        for i in (3..dem.bytes()).step_by(4) {
            dem.data[i] = 255;
        }
        let mut response = Response::new();
        response.data = Some(Arc::new(mln_image::encode_png(&dem)));
        Some(response)
    });
    test.load_json(
        r#"{
        "version": 8,
        "state": {"color": {"default": "red"}, "unrelated": {"default": 0}},
        "sources": {
            "line": {
                "type": "geojson",
                "lineMetrics": true,
                "data": {"type": "LineString", "coordinates": [[-20, 0], [20, 0]]}
            },
            "heatmap": {"type": "geojson", "data": {"type": "Point", "coordinates": [0, 0]}},
            "color-relief": {"type": "raster-dem", "tiles": ["https://example.com/{z}/{x}/{y}.png"]}
        },
        "layers": [{"id": "background", "type": "background", "paint": {"background-color": "white"}}]
    }"#,
    );
    test.map().jump_to(&cam().with_center(Some(ll(0.0, 0.0))).with_zoom(Some(2.0)));
    test.render();

    let expect_center = |expected: Color| {
        let pixel = center_pixel(&test.render().image);
        assert_near(f64::from(pixel[0]), f64::from(expected.r) * 255.0, 2.0);
        assert_near(f64::from(pixel[1]), f64::from(expected.g) * 255.0, 2.0);
        assert_near(f64::from(pixel[2]), f64::from(expected.b) * 255.0, 2.0);
    };
    let set_ramp = |color: &str| {
        let value: serde_json::Value = serde_json::from_str(&ramp(color)).unwrap();
        let mut map = test.map();
        let mut style = map.get_style_mut();
        let mut layer = style.get_layer_mut("ramp").unwrap();
        assert!(layer.set_property(&ramp_property, &Convertible::from(&value), None).is_none());
    };
    let set_state = |name: &str, value: Value| {
        test.map().get_style_mut().set_global_state_property(name, &value).unwrap();
    };

    let extra_paint = match type_ {
        "line" => r#", "line-width": 16"#,
        "heatmap" => r#", "heatmap-radius": 64"#,
        _ => "",
    };
    let layer_json = format!(
        r#"{{"id": "ramp", "type": "{type_}", "source": "{type_}", "paint": {{"{ramp_property}": {}{extra_paint}}}}}"#,
        ramp(state_color)
    );
    let layer_value: serde_json::Value = serde_json::from_str(&layer_json).unwrap();
    let mut error = ConversionError::default();
    let layer = convert_layer(&Convertible::from(&layer_value), &mut error);
    let layer = layer.unwrap_or_else(|| panic!("{}", error.message));

    // The layer joins the render tree in the same update as a write to a key its ramp does not read.
    set_state("unrelated", Value::Double(1.0));
    test.map().get_style_mut().add_layer(layer, None).unwrap();
    expect_center(Color::red());

    // While the ramp reads no state, the layer skips evaluation for state changes.
    set_ramp(r#""lime""#);
    expect_center(Color::new(0.0, 1.0, 0.0, 1.0));
    set_state("color", Value::String("blue".to_owned()));
    expect_center(Color::new(0.0, 1.0, 0.0, 1.0));

    // The ramp reads the state again in the same update as another unrelated write.
    set_state("unrelated", Value::Double(2.0));
    set_ramp(state_color);
    expect_center(Color::blue());
}

// INSTANTIATE_TEST_SUITE_P(Rendering, ColorRampGlobalStateTest, testing::Values("line", "heatmap", "color-relief"))
// gtest: ColorRampGlobalStateTest.RendersCurrentStateAfterUnrelatedStateChange
#[test]
fn color_ramp_renders_current_state_after_unrelated_state_change_line() {
    color_ramp_renders_current_state_after_unrelated_state_change("line");
}

// gtest: ColorRampGlobalStateTest.RendersCurrentStateAfterUnrelatedStateChange
#[test]
fn color_ramp_renders_current_state_after_unrelated_state_change_heatmap() {
    color_ramp_renders_current_state_after_unrelated_state_change("heatmap");
}

// gtest: ColorRampGlobalStateTest.RendersCurrentStateAfterUnrelatedStateChange
#[test]
fn color_ramp_renders_current_state_after_unrelated_state_change_color_relief() {
    color_ramp_renders_current_state_after_unrelated_state_change("color-relief");
}

// Port-only: the C++ Map constructor throws for an empty size; the Rust one
// returns the error.
#[test]
fn empty_size_is_an_error() {
    let _loop = RunLoop::new();
    let frontend = CapturingFrontend::default();
    for size in [Size::new(0, 0), Size::new(256, 0), Size::new(0, 256)] {
        let result = Map::with_file_source(
            &frontend,
            Rc::new(StubMapObserver::default()),
            Arc::new(StubFileSource::new(ResponseType::Asynchronous)),
            MapOptions::new().with_size(size),
        );
        assert_eq!(result.err().as_deref(), Some("failed to resize: size is empty"));
    }
}

// gtest: Map.SourcelessLayersRenderAfterLeavingZoomRange
// Layers without a source rebuild what they draw when they return to their
// zoom range. The C++ test draws a location-indicator layer, which mln-rs
// core doesn't render; a second background layer, which isn't drawn as the
// clear color, stands in for it.
#[test]
fn sourceless_layers_render_after_leaving_zoom_range() {
    let test = MapTest::new();
    test.load_json(
        r#"{
      "version": 8,
      "sources": {},
      "layers": [{
        "id": "background",
        "type": "background",
        "minzoom": 10,
        "paint": {"background-color": "red"}
      }, {
        "id": "tint",
        "type": "background",
        "minzoom": 10,
        "paint": {"background-color": "blue", "background-opacity": 0.5}
      }]
    }"#,
    );
    let render = |zoom: f64| {
        test.map().jump_to(&cam().with_center(Some(ll(0.0, 0.0))).with_zoom(Some(zoom)));
        test.render().image
    };
    let is_blank = |image: &PremultipliedImage| image.data.iter().all(|&v| v == 0);

    let image = render(16.0);
    let center = center_pixel(&image);
    assert!(center[0] > 0 && center[2] > 0 && center[3] == 255, "{center:?}");
    assert!(is_blank(&render(5.0)));

    let again = render(16.0);
    assert_eq!(image.bytes(), again.bytes());
    assert!(image.data == again.data);
}

/// `InactiveLayerCase`: a source, a layer object without "id", "source",
/// "minzoom" or "maxzoom", and a layer of the same source that renders
/// nothing.
struct InactiveLayerCase {
    source: &'static str,
    layer: &'static str,
    keeper: &'static str,
}

/// At zoom 16.1 to 16.45, a 256 pixel viewport shows one raster tile at the
/// small center, and four at the large one.
const INACTIVE_SMALL_TILE: CanonicalTileID = CanonicalTileID { z: 15, x: 16484, y: 16283 };

fn inactive_small_center() -> LatLng {
    let id = INACTIVE_SMALL_TILE;
    LatLngBounds::from_canonical_tile(id.z, id.x, id.y).center()
}

fn inactive_large_center() -> LatLng {
    ll(0.0, 0.0)
}

fn inactive_layer_features() -> String {
    let mut features: Vec<String> = Vec::new();
    let mut feature = |geometry: String| {
        features.push(format!(r#"{{"type":"Feature","properties":{{}},"geometry":{geometry}}}"#));
    };
    let point = |center: &LatLng, dx: f64, dy: f64| format!("[{},{}]", center.longitude() + dx, center.latitude() + dy);

    let small = inactive_small_center();
    feature(format!(
        r#"{{"type":"Polygon","coordinates":[[{},{},{},{}]]}}"#,
        point(&small, 0.0, 0.0),
        point(&small, 0.001, 0.0),
        point(&small, 0.0, 0.001),
        point(&small, 0.0, 0.0)
    ));
    feature(format!(
        r#"{{"type":"LineString","coordinates":[{},{}]}}"#,
        point(&small, 0.0, 0.0),
        point(&small, 0.001, 0.001)
    ));
    feature(format!(r#"{{"type":"Point","coordinates":{}}}"#, point(&small, 0.0, 0.0)));

    let large = inactive_large_center();
    let ring: Vec<String> = (0..=256)
        .map(|i| {
            let angle = std::f64::consts::TAU * f64::from(i % 256) / 256.0;
            point(&large, 0.002 * angle.cos(), 0.002 * angle.sin())
        })
        .collect();
    feature(format!(r#"{{"type":"Polygon","coordinates":[[{}]]}}"#, ring.join(",")));
    for row in 0..32 {
        let mut line = Vec::new();
        for column in 0..32 {
            let coordinates = point(&large, -0.002 + f64::from(column) * 0.000125, -0.002 + f64::from(row) * 0.000125);
            feature(format!(r#"{{"type":"Point","coordinates":{coordinates}}}"#));
            line.push(coordinates);
        }
        feature(format!(r#"{{"type":"LineString","coordinates":[{}]}}"#, line.join(",")));
    }
    format!(r#"{{"type":"FeatureCollection","features":[{}]}}"#, features.join(","))
}

/// The GPU resources of `RenderingStats` the test compares.
fn inactive_layer_resources(stats: &mln_gfx::rendering_stats::RenderingStats) -> [i32; 5] {
    [
        stats.num_vertex_buffers,
        stats.mem_vertex_buffers,
        stats.num_index_buffers,
        stats.mem_index_buffers,
        stats.num_active_textures,
    ]
}

// gtest: InactiveLayerTest.ReleasesTilesOutsideZoomRange
// A layer outside its zoom range, and a source whose layers all are, release
// the tiles they last rendered.
fn releases_tiles_outside_zoom_range(param: InactiveLayerCase) {
    let test = MapTest::new();
    test.map().set_prefetch_zoom_delta(0);
    test.frontend.with_renderer(|renderer| renderer.set_tile_cache_enabled(false));
    test.file_source.set_response(Kind::Glyphs, |_| Some(file_response("test/fixtures/resources/glyphs.pbf")));
    let small_tile = Arc::new(mln_image::encode_png(&PremultipliedImage::new(Size::new(4, 4))));
    let large_tile = Arc::new(read_bytes("test/fixtures/resources/raster.tile"));
    test.file_source.set_response(Kind::Tile, move |resource: &Resource| {
        let tile = resource.tile_data.as_ref().expect("a tile resource");
        let (z, x, y) = (tile.z, tile.x, tile.y);
        let mut response = Response::new();
        if z == 15 && x == INACTIVE_SMALL_TILE.x as i32 && y == INACTIVE_SMALL_TILE.y as i32 {
            response.data = Some(small_tile.clone());
        } else if z == 15 && (x == 16383 || x == 16384) && (y == 16383 || y == 16384) {
            response.data = Some(large_tile.clone());
        } else {
            response.no_content = true;
        }
        Some(response)
    });

    let source = if param.source == "geojson" {
        format!(r#"{{"type":"geojson","data":{}}}"#, inactive_layer_features())
    } else {
        format!(r#"{{"type":"{}","tiles":["https://example.com/{{z}}/{{x}}/{{y}}.png"]}}"#, param.source)
    };
    let layer = |id: &str, zoom: &str, json: &str| format!(r#"{{"id":"{id}","source":"data",{zoom},{}"#, &json[1..]);
    test.load_json(&format!(
        r#"{{"version":8,"glyphs":"https://example.com/{{fontstack}}/{{range}}.pbf","sources":{{"data":{source}}},"layers":[{},{}]}}"#,
        layer("layer", r#""minzoom":16.4"#, param.layer),
        layer("keeper", r#""minzoom":16.2,"maxzoom":16.35"#, param.keeper),
    ));

    // Tiles a source drops are released on the render thread during the
    // frame, so the frame's stats already count them out.
    let render = |center: LatLng, zoom: f64| {
        test.map().jump_to(&cam().with_center(Some(center)).with_zoom(Some(zoom)));
        test.render()
    };
    let leave_zoom_range = |center: LatLng| {
        // The layer leaves its zoom range while the keeper still renders the
        // source.
        render(center, 16.3);
        // The keeper leaves its zoom range too, so the source stops
        // rendering.
        render(center, 16.1).stats
    };

    // Render the layer with the large data first: the context's buffers of
    // default attribute values grow to the largest drawable they serve, and
    // stay.
    render(inactive_large_center(), 16.45);
    leave_zoom_range(inactive_large_center());
    // Render the layer once with little data, so it creates whatever it
    // keeps between frames.
    assert!(render(inactive_small_center(), 16.45).stats.num_draw_calls > 0);
    let released = leave_zoom_range(inactive_small_center());

    let rendered = render(inactive_large_center(), 16.45);
    let image = &rendered.image;
    assert!(image.data.iter().any(|&v| v != 0));
    assert_eq!(
        inactive_layer_resources(&released),
        inactive_layer_resources(&leave_zoom_range(inactive_large_center())),
        "released"
    );

    let rendered_again = render(inactive_large_center(), 16.45);
    assert_eq!(
        inactive_layer_resources(&rendered.stats),
        inactive_layer_resources(&rendered_again.stats),
        "rendered again"
    );
    assert_eq!(image.bytes(), rendered_again.image.bytes());
    assert!(image.data == rendered_again.image.data, "rendered again");
}

const INACTIVE_NO_FEATURES: &str = r#"{"type":"circle","filter":["==",["get","missing"],1]}"#;
const INACTIVE_DEM_KEEPER: &str = r#"{"type":"color-relief","paint":{"color-relief-opacity":0}}"#;

// gtest: InactiveLayerTest.ReleasesTilesOutsideZoomRange
#[test]
fn releases_tiles_outside_zoom_range_fill() {
    releases_tiles_outside_zoom_range(InactiveLayerCase {
        source: "geojson",
        layer: r#"{"type":"fill","paint":{"fill-color":"red"}}"#,
        keeper: INACTIVE_NO_FEATURES,
    });
}

// gtest: InactiveLayerTest.ReleasesTilesOutsideZoomRange
#[test]
fn releases_tiles_outside_zoom_range_line() {
    releases_tiles_outside_zoom_range(InactiveLayerCase {
        source: "geojson",
        layer: r#"{"type":"line","paint":{"line-width":4}}"#,
        keeper: INACTIVE_NO_FEATURES,
    });
}

// gtest: InactiveLayerTest.ReleasesTilesOutsideZoomRange
#[test]
fn releases_tiles_outside_zoom_range_circle() {
    releases_tiles_outside_zoom_range(InactiveLayerCase {
        source: "geojson",
        layer: r#"{"type":"circle"}"#,
        keeper: INACTIVE_NO_FEATURES,
    });
}

// gtest: InactiveLayerTest.ReleasesTilesOutsideZoomRange
#[test]
fn releases_tiles_outside_zoom_range_fill_extrusion() {
    releases_tiles_outside_zoom_range(InactiveLayerCase {
        source: "geojson",
        layer: r#"{"type":"fill-extrusion","paint":{"fill-extrusion-height":100}}"#,
        keeper: INACTIVE_NO_FEATURES,
    });
}

// gtest: InactiveLayerTest.ReleasesTilesOutsideZoomRange
#[test]
fn releases_tiles_outside_zoom_range_symbol() {
    releases_tiles_outside_zoom_range(InactiveLayerCase {
        source: "geojson",
        layer: r#"{"type":"symbol","layout":{"text-field":"x","text-font":["Open Sans Regular"],"text-allow-overlap":true}}"#,
        keeper: INACTIVE_NO_FEATURES,
    });
}

// gtest: InactiveLayerTest.ReleasesTilesOutsideZoomRange
#[test]
fn releases_tiles_outside_zoom_range_heatmap() {
    releases_tiles_outside_zoom_range(InactiveLayerCase {
        source: "geojson",
        layer: r#"{"type":"heatmap"}"#,
        keeper: INACTIVE_NO_FEATURES,
    });
}

// gtest: InactiveLayerTest.ReleasesTilesOutsideZoomRange
#[test]
fn releases_tiles_outside_zoom_range_raster() {
    releases_tiles_outside_zoom_range(InactiveLayerCase {
        source: "raster",
        layer: r#"{"type":"raster"}"#,
        keeper: r#"{"type":"raster","paint":{"raster-opacity":0}}"#,
    });
}

// gtest: InactiveLayerTest.ReleasesTilesOutsideZoomRange
#[test]
fn releases_tiles_outside_zoom_range_hillshade() {
    releases_tiles_outside_zoom_range(InactiveLayerCase {
        source: "raster-dem",
        layer: r#"{"type":"hillshade"}"#,
        keeper: INACTIVE_DEM_KEEPER,
    });
}

// gtest: InactiveLayerTest.ReleasesTilesOutsideZoomRange
#[test]
fn releases_tiles_outside_zoom_range_color_relief() {
    releases_tiles_outside_zoom_range(InactiveLayerCase {
        source: "raster-dem",
        layer: r#"{"type":"color-relief","paint":{"color-relief-color":["interpolate",["linear"],["elevation"],-10000,"black",10000,"white"]}}"#,
        keeper: INACTIVE_DEM_KEEPER,
    });
}
