// Camera commands: when their ends report, fits, and the viewport and tile
// settings. They follow maplibre-native-ffi's ABI tests
// (tests/native/abi/map/camera.c, camera_transitions.c and options.c at
// b20632a) where those test the same rules.

mod support;

use std::sync::{Arc, Mutex};

use mln_map::{
    AnimationOptions, CameraOptions, CameraUpdate, CameraUpdateMode, NorthOrientation, TileLodMode, TransitionEnd,
    TransitionOutcome,
};
use mln_runtime::{
    CameraFitArea, CameraFitOptions, CameraFitUpdate, ErrorKind, EventMask, TileOptions, ViewportOptions,
};
use mln_util::chrono::Duration;
use mln_util::geo::{EdgeInsets, LatLng, LatLngBounds};
use support::{map, runtime, wait, wait_err};

/// An animation whose ends `ends` records.
fn recorded(ends: &Arc<Mutex<Vec<TransitionEnd>>>, duration: Duration) -> AnimationOptions {
    let ends = ends.clone();
    AnimationOptions {
        duration: Some(duration),
        transition_end_fn: Some(Arc::new(move |end: &TransitionEnd| ends.lock().unwrap().push(*end))),
        ..AnimationOptions::default()
    }
}

fn zoom_to(zoom: f64, animation: AnimationOptions) -> CameraUpdate {
    CameraUpdate {
        mode: CameraUpdateMode::Ease,
        camera: CameraOptions::default().with_zoom(Some(zoom)),
        animation,
        ..CameraUpdate::default()
    }
}

#[test]
fn an_end_reports_the_snapshot_that_shows_it_before_its_completion() {
    let runtime = runtime();
    let map = map(&runtime);
    let ends = Arc::new(Mutex::new(Vec::new()));

    let jumped = wait(map.update_camera(zoom_to(3.0, recorded(&ends, Duration::zero()))));
    let end = ends.lock().unwrap().pop().unwrap();
    assert_eq!((end.outcome, end.generation), (TransitionOutcome::Completed, jumped.generation));
    assert_eq!(jumped.generation, map.snapshot().unwrap().generation);

    drop(map.update_camera(zoom_to(9.0, recorded(&ends, Duration::from_secs(60)))).unwrap());
    let cancelled = wait(map.cancel_camera_transitions());
    let end = ends.lock().unwrap().pop().unwrap();
    assert_eq!((end.outcome, end.generation), (TransitionOutcome::Cancelled, cancelled.generation));

    drop(map.update_camera(zoom_to(9.0, recorded(&ends, Duration::from_secs(60)))).unwrap());
    wait(map.release());
    let end = ends.lock().unwrap().pop().unwrap();
    assert_eq!((end.outcome, end.generation), (TransitionOutcome::Closed, 0));
    wait(runtime.release());
}

#[test]
fn a_fit_moves_the_camera_where_the_query_fits() {
    let runtime = runtime();
    let map = map(&runtime);
    let area = CameraFitArea::Bounds(LatLngBounds::hull(
        LatLng::new(35.0, -125.0).unwrap(),
        LatLng::new(39.0, -115.0).unwrap(),
    ));
    let fit = CameraFitOptions { bearing: Some(5.0), ..CameraFitOptions::default() };
    let camera = wait(map.camera_for(area.clone(), fit));
    assert!((camera.center.unwrap().longitude() + 120.0).abs() < 1e-9);
    assert_eq!(camera.bearing, Some(5.0));

    wait(map.fit_camera(area.clone(), CameraFitUpdate { fit, ..CameraFitUpdate::default() }));
    let moved = wait(map.camera()).camera;
    assert!((moved.zoom - camera.zoom.unwrap()).abs() < 1e-9);
    assert!((moved.bearing - 5.0).abs() < 1e-9);

    let ends = Arc::new(Mutex::new(Vec::new()));
    let crowded =
        CameraFitOptions { padding: EdgeInsets { left: 128.0, right: 128.0, ..EdgeInsets::default() }, ..fit };
    let refused =
        CameraFitUpdate { fit: crowded, animation: recorded(&ends, Duration::from_secs(1)), ..Default::default() };
    let error = wait_err(map.fit_camera(area.clone(), refused));
    assert_eq!(error.kind, ErrorKind::InvalidArgument);
    assert!(error.message.contains("padding"), "{}", error.message);
    assert_eq!(ends.lock().unwrap()[0].outcome, TransitionOutcome::Cancelled);
    assert_eq!(wait(map.camera()).camera, moved);

    assert_eq!(ErrorKind::InvalidArgument, wait_err(map.camera_for(CameraFitArea::Coordinates(Vec::new()), fit)).kind);
    let empty = CameraFitArea::Geometry(mln_util::geometry::Geometry::Empty);
    assert_eq!(ErrorKind::InvalidArgument, wait_err(map.camera_for(empty, fit)).kind);
    wait(map.release());
    wait(runtime.release());
}

#[test]
fn the_visible_bounds_unwrap_across_the_antimeridian() {
    let runtime = runtime();
    let map = map(&runtime);
    // 256 pixels at zoom 1 span 90 degrees.
    let camera = CameraOptions::default().with_center(Some(LatLng::new(0.0, 170.0).unwrap())).with_zoom(Some(1.0));
    let bounds = wait(map.lat_lng_bounds_for_camera(camera));
    assert!((bounds.west() - 125.0).abs() < 1e-6, "{bounds:?}");
    assert!((bounds.east() - 215.0).abs() < 1e-6, "{bounds:?}");
    wait(map.release());
    wait(runtime.release());
}

#[test]
fn viewport_and_tile_options_keep_what_they_leave_out() {
    let runtime = runtime();
    let map = map(&runtime);
    wait(map.set_viewport_options(ViewportOptions {
        north_orientation: Some(NorthOrientation::Rightwards),
        ..ViewportOptions::default()
    }));
    wait(map.set_viewport_options(ViewportOptions {
        frustum_offset: Some(EdgeInsets { top: 4.0, ..EdgeInsets::default() }),
        ..ViewportOptions::default()
    }));
    let viewport = map.snapshot().unwrap().view.viewport;
    assert_eq!(viewport.north_orientation, NorthOrientation::Rightwards);
    assert_eq!(viewport.frustum_offset.top, 4.0);

    let set = wait(map.set_tile_options(TileOptions {
        prefetch_zoom_delta: Some(2),
        lod_mode: Some(TileLodMode::Distance),
        lod_pitch_threshold: Some(50.0),
        ..TileOptions::default()
    }));
    let tile = map.snapshot().unwrap().tile;
    assert_eq!((tile.prefetch_zoom_delta, tile.lod_mode), (2, TileLodMode::Distance));
    // Sending back the threshold that the snapshot reports changes nothing.
    let again = wait(map.set_tile_options(TileOptions {
        lod_pitch_threshold: Some(tile.lod_pitch_threshold),
        ..TileOptions::default()
    }));
    assert_eq!(set.generation, again.generation);
    wait(map.release());
    wait(runtime.release());
}

#[test]
fn an_event_mask_change_is_a_new_generation() {
    let runtime = runtime();
    let map = map(&runtime);
    let before = map.snapshot().unwrap().generation;
    let changed = wait(map.set_event_mask(EventMask::NONE));
    assert!(changed.generation > before);
    assert_eq!(changed.generation, wait(map.set_event_mask(EventMask::NONE)).generation);
    wait(map.release());
    wait(runtime.release());
}
