//! `Map::snapshot`, the map's generation counters, and projections from a
//! snapshot's view.

use std::cell::Cell;
use std::rc::{Rc, Weak};
use std::sync::Arc;

use mln_actor::RunLoop;
use mln_map::{
    AnimationOptions, BoundOptions, CameraBounds, CameraOptions, ConstrainMode, FreeCamera, FreeCameraOptions, Map,
    MapDebugOptions, MapMode, MapOptions, MapProjection, MapView, NorthOrientation, NullMapObserver, ProjectionMode,
    TileLodMode, ViewportMode,
};
use mln_renderer::{RendererFrontend, RendererObserver, UpdateParameters};
use mln_storage::stub_file_source::{ResponseType, StubFileSource};
use mln_util::chrono::Duration;
use mln_util::geo::{EdgeInsets, LatLng, LatLngBounds, ScreenCoordinate, WrapMode};
use mln_util::size::Size;

/// Counts the updates that the map sends.
#[derive(Clone, Default)]
struct CountingFrontend {
    updates: Rc<Cell<u64>>,
}

impl RendererFrontend for CountingFrontend {
    fn reset(&self) {}
    fn set_observer(&self, _observer: Weak<dyn RendererObserver>) {}
    fn update(&self, _parameters: Arc<UpdateParameters>) {
        self.updates.set(self.updates.get() + 1);
    }
}

fn map(frontend: &CountingFrontend, options: MapOptions) -> Map {
    Map::with_file_source(
        frontend,
        Rc::new(NullMapObserver),
        Arc::new(StubFileSource::new(ResponseType::Asynchronous)),
        options,
    )
    .expect("map")
}

fn ll(lat: f64, lon: f64) -> LatLng {
    LatLng::new(lat, lon).expect("LatLng")
}

fn insets(top: f64, left: f64, bottom: f64, right: f64) -> EdgeInsets {
    EdgeInsets::new(top, left, bottom, right).expect("EdgeInsets")
}

fn assert_near(actual: f64, expected: f64) {
    assert!((actual - expected).abs() < 1e-9, "{actual} is not {expected}");
}

#[test]
fn snapshot_reports_every_member() {
    let _loop = RunLoop::new();
    let frontend = CountingFrontend::default();
    let mut map = map(
        &frontend,
        MapOptions::new()
            .with_size(Size::new(512, 256))
            .with_pixel_ratio(2.0)
            .with_constrain_mode(ConstrainMode::WidthAndHeight),
    );

    let snapshot = map.snapshot();
    assert_eq!(snapshot.debug, MapDebugOptions::NO_DEBUG);
    assert!(!snapshot.fully_loaded);
    assert!(!snapshot.rendering_stats_view_enabled);
    assert!(!snapshot.gesture_in_progress);
    assert!(!snapshot.transition_in_progress);
    assert_eq!(snapshot.bounds.bounds, None);
    assert_eq!(snapshot.tile.prefetch_zoom_delta, 4);
    assert_near(snapshot.tile.lod_pitch_threshold, 60.0);

    let bounds = LatLngBounds::hull(ll(-10.0, -20.0), ll(30.0, 40.0));
    map.set_bounds(
        &BoundOptions::default()
            .with_lat_lng_bounds(bounds)
            .with_min_zoom(1.0)
            .with_max_zoom(18.0)
            .with_min_pitch(5.0)
            .with_max_pitch(50.0),
    )
    .unwrap();
    map.set_debug(MapDebugOptions::TILE_BORDERS | MapDebugOptions::COLLISION);
    map.set_prefetch_zoom_delta(2);
    map.set_tile_lod_min_radius(2.5);
    map.set_tile_lod_scale(1.5);
    map.set_tile_lod_pitch_threshold(mln_math::deg2rad(45.0));
    map.set_tile_lod_zoom_shift(-0.5);
    map.set_tile_lod_mode(TileLodMode::Distance);
    map.set_north_orientation(NorthOrientation::Rightwards);
    map.set_viewport_mode(ViewportMode::FlippedY);
    map.set_frustum_offset(&insets(1.0, 2.0, 3.0, 4.0));
    map.set_projection_mode(&ProjectionMode::default().with_axonometric(true).with_x_skew(0.25).with_y_skew(0.5));
    map.enable_rendering_stats_view(true);
    map.set_gesture_in_progress(true);
    let camera = CameraOptions::default()
        .with_center(Some(ll(10.0, 20.0)))
        .with_padding(Some(insets(10.0, 20.0, 30.0, 40.0)))
        .with_zoom(Some(6.0))
        .with_bearing(Some(30.0))
        .with_pitch(Some(20.0))
        .with_roll(Some(5.0))
        .with_fov(Some(40.0));
    map.jump_to(&camera);
    map.ease_to(
        &CameraOptions::default().with_zoom(Some(7.0)),
        &AnimationOptions { duration: Some(Duration::from_secs(10)), ..AnimationOptions::default() },
    );

    let snapshot = map.snapshot();
    assert_eq!(snapshot.debug, MapDebugOptions::TILE_BORDERS | MapDebugOptions::COLLISION);
    assert!(snapshot.rendering_stats_view_enabled);
    assert!(snapshot.gesture_in_progress);
    assert!(snapshot.transition_in_progress);
    assert!(!snapshot.fully_loaded);

    let tile = snapshot.tile;
    assert_eq!(tile.prefetch_zoom_delta, 2);
    assert_eq!(tile.lod_min_radius, 2.5);
    assert_eq!(tile.lod_scale, 1.5);
    assert_near(tile.lod_pitch_threshold, 45.0);
    assert_eq!(tile.lod_zoom_shift, -0.5);
    assert_eq!(tile.lod_mode, TileLodMode::Distance);

    assert_eq!(
        snapshot.bounds,
        CameraBounds { bounds: Some(bounds), min_zoom: 1.0, max_zoom: 18.0, min_pitch: 5.0, max_pitch: 50.0 }
    );
    assert_eq!(BoundOptions::from(snapshot.bounds), map.get_bounds());

    let free_camera = map.get_free_camera_options();
    assert_eq!(
        snapshot.free_camera,
        FreeCamera { position: free_camera.position.unwrap(), orientation: free_camera.orientation.unwrap() }
    );
    assert_eq!(FreeCameraOptions::from(snapshot.free_camera), free_camera);

    let view = snapshot.view;
    let camera = view.camera;
    assert_near(camera.center.latitude(), 10.0);
    assert_near(camera.center.longitude(), 20.0);
    assert_eq!(camera.padding, insets(10.0, 20.0, 30.0, 40.0));
    assert_near(camera.zoom, 6.0);
    assert_near(camera.bearing, 30.0);
    assert_near(camera.pitch, 20.0);
    assert_near(camera.roll, 5.0);
    assert_near(camera.fov, 40.0);
    assert_eq!(CameraOptions::from(camera), map.get_camera_options(None));
    assert_eq!(view.extent.size, Size::new(512, 256));
    assert_eq!(view.extent.scale_factor, 2.0);
    assert_eq!(view.projection_mode, map.get_projection_mode());
    assert_eq!(view.viewport.north_orientation, NorthOrientation::Rightwards);
    assert_eq!(view.viewport.constrain_mode, ConstrainMode::WidthAndHeight);
    assert_eq!(view.viewport.viewport_mode, ViewportMode::FlippedY);
    assert_eq!(view.viewport.frustum_offset, insets(1.0, 2.0, 3.0, 4.0));

    map.cancel_transitions();
    map.set_gesture_in_progress(false);
    let snapshot = map.snapshot();
    assert!(!snapshot.transition_in_progress);
    assert!(!snapshot.gesture_in_progress);
}

#[test]
fn generation_grows_with_each_change() {
    let _loop = RunLoop::new();
    let frontend = CountingFrontend::default();
    let mut map = map(&frontend, MapOptions::new().with_size(Size::new(256, 256)));

    let mut last = map.generation();
    let mut assert_changed = |map: &Map, changed: bool, what: &str| {
        let generation = map.generation();
        assert_eq!(map.snapshot().generation, generation);
        if changed {
            assert!(generation > last, "{what} kept the generation");
        } else {
            assert_eq!(generation, last, "{what} changed the generation");
        }
        last = generation;
    };

    map.snapshot();
    map.get_camera_options(None);
    assert_changed(&map, false, "reading");

    map.jump_to(&CameraOptions::default().with_zoom(Some(3.0)));
    assert_changed(&map, true, "jump_to");
    map.set_tile_lod_scale(2.0);
    assert_changed(&map, true, "set_tile_lod_scale");
    map.set_tile_lod_scale(2.0);
    assert_changed(&map, false, "set_tile_lod_scale to the same value");
    map.set_debug(MapDebugOptions::OVERDRAW);
    assert_changed(&map, true, "set_debug");
    map.set_bounds(&BoundOptions::default().with_min_pitch(10.0)).unwrap();
    assert_changed(&map, true, "set_bounds");
    map.set_bounds(&BoundOptions::default()).unwrap();
    assert_changed(&map, false, "set_bounds with no limits");
    map.enable_rendering_stats_view(true);
    assert_changed(&map, true, "enable_rendering_stats_view");
    map.enable_rendering_stats_view(true);
    assert_changed(&map, false, "enable_rendering_stats_view when enabled");
    map.get_style_mut().load_json(r#"{"version":8,"sources":{},"layers":[]}"#);
    assert_changed(&map, true, "load_json");
    map.set_free_camera_options(&FreeCameraOptions { position: Some([0.4, 0.4, 0.1]), orientation: None });
    assert_changed(&map, true, "set_free_camera_options");
}

#[test]
fn update_generation_counts_the_updates_sent() {
    let _loop = RunLoop::new();
    let frontend = CountingFrontend::default();
    let mut map = map(&frontend, MapOptions::new().with_size(Size::new(256, 256)));

    map.jump_to(&CameraOptions::default().with_zoom(Some(2.0)));
    map.set_prefetch_zoom_delta(1);
    assert!(frontend.updates.get() >= 2);
    assert_eq!(map.update_generation(), frontend.updates.get());
    assert_eq!(map.snapshot().update_generation, frontend.updates.get());
}

#[test]
fn a_static_map_changes_without_sending_updates() {
    let _loop = RunLoop::new();
    let frontend = CountingFrontend::default();
    let mut map = map(&frontend, MapOptions::new().with_size(Size::new(256, 256)).with_map_mode(MapMode::Static));

    let before = map.snapshot();
    map.jump_to(&CameraOptions::default().with_zoom(Some(2.0)));
    map.set_debug(MapDebugOptions::TILE_BORDERS);
    let after = map.snapshot();
    assert!(after.generation > before.generation);
    assert_eq!(after.update_generation, 0);
    assert_eq!(frontend.updates.get(), 0);
}

#[test]
fn projection_from_a_view_matches_the_map() {
    let _loop = RunLoop::new();
    let frontend = CountingFrontend::default();
    let mut map = map(&frontend, MapOptions::new().with_size(Size::new(400, 300)).with_pixel_ratio(1.5));
    // Bounds past the antimeridian keep the center's copy of the world, which
    // the view keeps too.
    map.set_bounds(&BoundOptions::default().with_lat_lng_bounds(LatLngBounds::hull(ll(-20.0, 170.0), ll(20.0, 200.0))))
        .unwrap();
    map.jump_to(
        &CameraOptions::default()
            .with_center(Some(ll(5.0, 185.0)))
            .with_zoom(Some(4.0))
            .with_bearing(Some(15.0))
            .with_pitch(Some(30.0))
            .with_padding(Some(insets(0.0, 40.0, 0.0, 0.0))),
    );

    let view = map.snapshot().view;
    assert!(view.camera.center.longitude() > 180.0);
    let projection = MapProjection::from_view(&view).unwrap();
    for lat_lng in [ll(5.0, 185.0), ll(-10.0, 178.0), ll(12.0, -170.0)] {
        let expected = map.pixel_for_lat_lng(&lat_lng);
        let actual = projection.pixel_for_lat_lng(&lat_lng);
        assert!((actual.x - expected.x).abs() < 1e-6 && (actual.y - expected.y).abs() < 1e-6, "{lat_lng:?}");
    }

    let round_trip = MapView::from_state(&view.to_state().unwrap(), view.extent.scale_factor);
    assert_eq!(round_trip.extent, view.extent);
    assert_eq!(round_trip.viewport, view.viewport);
    assert_eq!(round_trip.projection_mode, view.projection_mode);
    assert_near(round_trip.camera.center.latitude(), view.camera.center.latitude());
    assert_near(round_trip.camera.center.longitude(), view.camera.center.longitude());
    assert_near(round_trip.camera.zoom, view.camera.zoom);
    assert_near(round_trip.camera.bearing, view.camera.bearing);
    assert_near(round_trip.camera.pitch, view.camera.pitch);
    assert_eq!(round_trip.camera.padding, view.camera.padding);

    let center = ScreenCoordinate::new(220.0, 150.0);
    assert!(projection.lat_lng_for_pixel(&center, WrapMode::Unwrapped).is_ok());
}

#[test]
fn projection_from_a_view_rejects_views_no_map_shows() {
    let _loop = RunLoop::new();
    let frontend = CountingFrontend::default();
    let map = map(&frontend, MapOptions::new().with_size(Size::new(256, 256)));
    let view = map.snapshot().view;
    assert!(MapProjection::from_view(&view).is_ok());

    let invalid: [fn(&mut MapView); 9] = [
        |view| view.camera.zoom = f64::NAN,
        |view| view.camera.zoom = 26.0,
        |view| view.camera.pitch = -1.0,
        |view| view.camera.fov = 170.0,
        |view| view.camera.padding.left = -1.0,
        |view| view.viewport.frustum_offset.top = f64::INFINITY,
        |view| view.extent.size = Size::new(0, 256),
        |view| view.extent.scale_factor = 0.0,
        |view| view.projection_mode.x_skew = Some(f64::NAN),
    ];
    for (index, change) in invalid.iter().enumerate() {
        let mut changed = view;
        change(&mut changed);
        assert!(MapProjection::from_view(&changed).is_err(), "view {index}");
    }
}
