//! `projection.h`: conversions between coordinates, screen points and
//! projected meters, for one view of a map or for none.

use std::sync::{Arc, Mutex};

use mln_map::MapProjection;
use mln_map_types::{Camera, ConstrainMode, LogicalExtent, MapView, ProjectionMode, Viewport, ViewportMode};
use mln_util::geo::{NorthOrientation, WrapMode};
use mln_util::projection::{ProjectedMeters, Projection};
use mln_util::size::Size;

use crate::error::Error;
use crate::generated::types::{
    MLN_CONSTRAIN_MODE_HEIGHT_ONLY, MLN_CONSTRAIN_MODE_NONE, MLN_CONSTRAIN_MODE_SCREEN,
    MLN_CONSTRAIN_MODE_WIDTH_AND_HEIGHT, MLN_NORTH_ORIENTATION_DOWN, MLN_NORTH_ORIENTATION_LEFT,
    MLN_NORTH_ORIENTATION_RIGHT, MLN_NORTH_ORIENTATION_UP, MLN_VIEWPORT_MODE_DEFAULT, MLN_VIEWPORT_MODE_FLIPPED_Y,
    mln_camera, mln_camera_options, mln_diagnostic, mln_lat_lng, mln_map_projection, mln_map_projection_camera_result,
    mln_map_view, mln_projected_meters, mln_screen_point, mln_status,
};
use crate::handles::{self, HandleKind, HandleTable};
use crate::{abi, camera, geo, lock};

abi::versioned!(mln_map_projection_camera_result, mln_map_view);

/// A projection's calls run one at a time, on whichever thread makes them.
static PROJECTIONS: HandleTable<Mutex<MapProjection>> = HandleTable::new(HandleKind::MapProjection);

/// Runs `call` on the live projection that `projection` names.
fn with_projection<R>(projection: mln_map_projection, call: impl FnOnce(&mut MapProjection) -> R) -> Result<R, Error> {
    let projection = PROJECTIONS.get(projection)?;
    let mut projection = lock(&projection);
    Ok(call(&mut projection))
}

fn north_orientation(value: u32) -> Result<NorthOrientation, Error> {
    Ok(match value {
        MLN_NORTH_ORIENTATION_UP => NorthOrientation::Upwards,
        MLN_NORTH_ORIENTATION_RIGHT => NorthOrientation::Rightwards,
        MLN_NORTH_ORIENTATION_DOWN => NorthOrientation::Downwards,
        MLN_NORTH_ORIENTATION_LEFT => NorthOrientation::Leftwards,
        _ => return Err(Error::invalid_argument(format!("north_orientation {value} is invalid"))),
    })
}

fn constrain_mode(value: u32) -> Result<ConstrainMode, Error> {
    Ok(match value {
        MLN_CONSTRAIN_MODE_NONE => ConstrainMode::None,
        MLN_CONSTRAIN_MODE_HEIGHT_ONLY => ConstrainMode::HeightOnly,
        MLN_CONSTRAIN_MODE_WIDTH_AND_HEIGHT => ConstrainMode::WidthAndHeight,
        MLN_CONSTRAIN_MODE_SCREEN => ConstrainMode::Screen,
        _ => return Err(Error::invalid_argument(format!("constrain_mode {value} is invalid"))),
    })
}

fn viewport_mode(value: u32) -> Result<ViewportMode, Error> {
    Ok(match value {
        MLN_VIEWPORT_MODE_DEFAULT => ViewportMode::Default,
        MLN_VIEWPORT_MODE_FLIPPED_Y => ViewportMode::FlippedY,
        _ => return Err(Error::invalid_argument(format!("viewport_mode {value} is invalid"))),
    })
}

/// The view that a caller's `mln_map_view` describes. [`MapView::to_state`]
/// checks the ranges that only a whole view decides.
fn map_view(view: &mln_map_view) -> Result<MapView, Error> {
    let camera: &mln_camera = &view.camera;
    let viewport = &view.viewport;
    Ok(MapView {
        camera: Camera {
            center: geo::lat_lng(camera.center)?,
            center_altitude: camera.center_altitude,
            padding: geo::edge_insets(camera.padding, "view padding")?,
            zoom: camera.zoom,
            bearing: camera.bearing,
            pitch: camera.pitch,
            roll: camera.roll,
            fov: camera.field_of_view,
        },
        extent: LogicalExtent {
            size: Size { width: view.extent.width, height: view.extent.height },
            scale_factor: view.extent.scale_factor,
        },
        projection_mode: ProjectionMode {
            axonometric: Some(view.projection_mode.axonometric),
            x_skew: Some(view.projection_mode.x_skew),
            y_skew: Some(view.projection_mode.y_skew),
        },
        viewport: Viewport {
            north_orientation: north_orientation(viewport.north_orientation)?,
            constrain_mode: constrain_mode(viewport.constrain_mode)?,
            viewport_mode: viewport_mode(viewport.viewport_mode)?,
            frustum_offset: geo::edge_insets(viewport.frustum_offset, "view frustum offset")?,
        },
    })
}

/// `mln_map_projection_create()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_projection_create(
    view: *const mln_map_view,
    out_projection: *mut mln_map_projection,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let view = unsafe { abi::read_sized(view) }?;
        // SAFETY: the caller's contract.
        let out_projection = unsafe { handles::out_handle(out_projection, "out_projection") }?;
        let projection = MapProjection::from_view(&map_view(&view)?)?;
        *out_projection = PROJECTIONS.insert(Arc::new(Mutex::new(projection)))?;
        Ok(())
    })
}

/// `mln_map_projection_release()`.
#[unsafe(no_mangle)]
pub extern "C" fn mln_map_projection_release(projection: mln_map_projection) {
    // A call that is running finishes with its own reference.
    let _ = PROJECTIONS.remove(projection);
}

/// `mln_map_projection_get_camera()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_projection_get_camera(
    projection: mln_map_projection,
    out_result: *mut mln_map_projection_camera_result,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, non_null(out_result), {
        let camera = with_projection(projection, |projection| camera::from_camera(projection.camera()))?;
        let result = mln_map_projection_camera_result { size: 0, camera };
        // SAFETY: the caller's contract.
        unsafe { abi::write_sized(out_result, &result) }
    })
}

/// `mln_map_projection_set_camera()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_projection_set_camera(
    projection: mln_map_projection,
    camera: *const mln_camera_options,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let camera = unsafe { camera::read_camera_options(camera) }?;
        with_projection(projection, |projection| projection.set_camera(&camera))
    })
}

/// `mln_map_projection_pixel_for_lat_lng()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_projection_pixel_for_lat_lng(
    projection: mln_map_projection,
    coordinate: mln_lat_lng,
    out_point: *mut mln_screen_point,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, non_null(out_point), {
        let coordinate = geo::lat_lng(coordinate)?;
        let point = with_projection(projection, |projection| projection.pixel_for_lat_lng(&coordinate))?;
        // SAFETY: non-null, and the caller's contract.
        unsafe { out_point.write(geo::from_screen_point(point)) };
        Ok(())
    })
}

/// The coordinate at `point`, for the single conversions.
///
/// # Safety
///
/// `out_coordinate` is null or points to a writable coordinate.
unsafe fn lat_lng_for_pixel(
    projection: mln_map_projection,
    point: mln_screen_point,
    out_coordinate: *mut mln_lat_lng,
    wrap_mode: WrapMode,
) -> Result<(), Error> {
    if out_coordinate.is_null() {
        return Err(Error::invalid_argument("out_coordinate must not be null"));
    }
    let point = geo::screen_point(point)?;
    let coordinate = with_projection(projection, |projection| projection.lat_lng_for_pixel(&point, wrap_mode))??;
    // SAFETY: non-null, and the caller's contract.
    unsafe { out_coordinate.write(geo::from_lat_lng(coordinate)) };
    Ok(())
}

/// `mln_map_projection_lat_lng_for_pixel()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_projection_lat_lng_for_pixel(
    projection: mln_map_projection,
    point: mln_screen_point,
    out_coordinate: *mut mln_lat_lng,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        unsafe { lat_lng_for_pixel(projection, point, out_coordinate, WrapMode::Wrapped) }
    })
}

/// `mln_map_projection_lat_lng_for_pixel_unwrapped()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_projection_lat_lng_for_pixel_unwrapped(
    projection: mln_map_projection,
    point: mln_screen_point,
    out_coordinate: *mut mln_lat_lng,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        unsafe { lat_lng_for_pixel(projection, point, out_coordinate, WrapMode::Unwrapped) }
    })
}

/// Writes `items` to a caller's array of `items.len()`, which may be null only
/// when it is empty. A batch converts every item before it writes any, so a
/// failure leaves the array as it was.
///
/// # Safety
///
/// `out` is null or points to `items.len()` writable items.
unsafe fn write_all<T: Copy>(items: &[T], out: *mut T) {
    if !items.is_empty() {
        // SAFETY: the caller's contract, and `items` is ours, so they don't
        // overlap.
        unsafe { core::ptr::copy_nonoverlapping(items.as_ptr(), out, items.len()) };
    }
}

/// `mln_map_projection_pixels_for_lat_lngs()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_projection_pixels_for_lat_lngs(
    projection: mln_map_projection,
    coordinates: *const mln_lat_lng,
    coordinate_count: usize,
    out_points: *mut mln_screen_point,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        if coordinate_count != 0 && out_points.is_null() {
            return Err(Error::invalid_argument("out_points must not be null when coordinate_count is nonzero"));
        }
        // SAFETY: the caller's contract.
        let coordinates = unsafe { geo::lat_lngs(coordinates, coordinate_count) }?;
        let points: Vec<mln_screen_point> = with_projection(projection, |projection| {
            coordinates
                .iter()
                .map(|coordinate| geo::from_screen_point(projection.pixel_for_lat_lng(coordinate)))
                .collect()
        })?;
        // SAFETY: the caller's contract: `out_points` has room for as many
        // points as there are coordinates.
        unsafe { write_all(&points, out_points) };
        Ok(())
    })
}

/// The coordinates at `points`, for the batch conversions.
///
/// # Safety
///
/// `points` is null or points to `point_count` readable points, and
/// `out_coordinates` is null or has room for as many coordinates.
unsafe fn lat_lngs_for_pixels(
    projection: mln_map_projection,
    points: *const mln_screen_point,
    point_count: usize,
    out_coordinates: *mut mln_lat_lng,
    wrap_mode: WrapMode,
) -> Result<(), Error> {
    if point_count != 0 && out_coordinates.is_null() {
        return Err(Error::invalid_argument("out_coordinates must not be null when point_count is nonzero"));
    }
    // SAFETY: the caller's contract.
    let points = unsafe { geo::screen_points(points, point_count) }?;
    let coordinates = with_projection(projection, |projection| {
        points
            .iter()
            .map(|point| projection.lat_lng_for_pixel(point, wrap_mode).map(geo::from_lat_lng))
            .collect::<Result<Vec<_>, _>>()
    })??;
    // SAFETY: the caller's contract.
    unsafe { write_all(&coordinates, out_coordinates) };
    Ok(())
}

/// `mln_map_projection_lat_lngs_for_pixels()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_projection_lat_lngs_for_pixels(
    projection: mln_map_projection,
    points: *const mln_screen_point,
    point_count: usize,
    out_coordinates: *mut mln_lat_lng,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        unsafe { lat_lngs_for_pixels(projection, points, point_count, out_coordinates, WrapMode::Wrapped) }
    })
}

/// `mln_map_projection_lat_lngs_for_pixels_unwrapped()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_projection_lat_lngs_for_pixels_unwrapped(
    projection: mln_map_projection,
    points: *const mln_screen_point,
    point_count: usize,
    out_coordinates: *mut mln_lat_lng,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        unsafe { lat_lngs_for_pixels(projection, points, point_count, out_coordinates, WrapMode::Unwrapped) }
    })
}

/// `mln_map_projection_meters_per_pixel_at_latitude()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_projection_meters_per_pixel_at_latitude(
    projection: mln_map_projection,
    latitude: f64,
    out_meters_per_pixel: *mut f64,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, non_null(out_meters_per_pixel), {
        if !(-90.0..=90.0).contains(&latitude) {
            return Err(Error::invalid_argument("latitude must be finite and within [-90, 90]"));
        }
        let zoom = with_projection(projection, |projection| projection.camera().zoom)?;
        // SAFETY: non-null, and the caller's contract.
        unsafe { out_meters_per_pixel.write(Projection::get_meters_per_pixel_at_latitude(latitude, zoom)) };
        Ok(())
    })
}

/// `mln_projected_meters_for_lat_lng()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_projected_meters_for_lat_lng(
    coordinate: mln_lat_lng,
    out_meters: *mut mln_projected_meters,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, non_null(out_meters), {
        let meters = Projection::projected_meters_for_lat_lng(&geo::lat_lng(coordinate)?);
        let meters = mln_projected_meters { northing: meters.northing(), easting: meters.easting() };
        // SAFETY: non-null, and the caller's contract.
        unsafe { out_meters.write(meters) };
        Ok(())
    })
}

/// `mln_lat_lng_for_projected_meters()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_lat_lng_for_projected_meters(
    meters: mln_projected_meters,
    out_coordinate: *mut mln_lat_lng,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, non_null(out_coordinate), {
        if !meters.northing.is_finite() || !meters.easting.is_finite() {
            return Err(Error::invalid_argument("projected meter values must be finite"));
        }
        let meters = ProjectedMeters::new(meters.northing, meters.easting)?;
        let coordinate = Projection::lat_lng_for_projected_meters(&meters);
        // SAFETY: non-null, and the caller's contract.
        unsafe { out_coordinate.write(geo::from_lat_lng(coordinate)) };
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostic::tests::{diagnostic, message};
    use crate::generated::types::{
        MLN_CAMERA_OPTION_CENTER, MLN_CAMERA_OPTION_PITCH, MLN_CAMERA_OPTION_ZOOM, MLN_STATUS_INVALID_ARGUMENT,
        MLN_STATUS_INVALID_STATE, MLN_STATUS_OK, mln_edge_insets, mln_logical_extent, mln_map_viewport,
        mln_projection_mode,
    };

    const NULL_DIAGNOSTIC: *mut mln_diagnostic = core::ptr::null_mut();
    const NO_INSETS: mln_edge_insets = mln_edge_insets { top: 0.0, left: 0.0, bottom: 0.0, right: 0.0 };

    /// The view of a 200 by 100 map centered on 0, 0 at zoom 2.
    fn view() -> mln_map_view {
        mln_map_view {
            size: size_of::<mln_map_view>() as u32,
            camera: mln_camera {
                center: mln_lat_lng { latitude: 0.0, longitude: 0.0 },
                center_altitude: 0.0,
                padding: NO_INSETS,
                zoom: 2.0,
                bearing: 0.0,
                pitch: 0.0,
                roll: 0.0,
                field_of_view: 36.87,
            },
            extent: mln_logical_extent { width: 200, height: 100, scale_factor: 1.0 },
            projection_mode: mln_projection_mode { axonometric: false, x_skew: 0.0, y_skew: 1.0 },
            viewport: mln_map_viewport {
                north_orientation: MLN_NORTH_ORIENTATION_UP,
                constrain_mode: MLN_CONSTRAIN_MODE_HEIGHT_ONLY,
                viewport_mode: MLN_VIEWPORT_MODE_DEFAULT,
                frustum_offset: NO_INSETS,
            },
        }
    }

    fn create(view: &mln_map_view) -> mln_map_projection {
        let mut projection = handles::NULL;
        // SAFETY: locals, and a null diagnostic.
        assert_eq!(unsafe { mln_map_projection_create(view, &mut projection, NULL_DIAGNOSTIC) }, MLN_STATUS_OK);
        projection
    }

    fn pixel(projection: mln_map_projection, latitude: f64, longitude: f64) -> mln_screen_point {
        let mut point = mln_screen_point { x: -1.0, y: -1.0 };
        let coordinate = mln_lat_lng { latitude, longitude };
        // SAFETY: a local output and a null diagnostic.
        let status =
            unsafe { mln_map_projection_pixel_for_lat_lng(projection, coordinate, &mut point, NULL_DIAGNOSTIC) };
        assert_eq!(status, MLN_STATUS_OK);
        point
    }

    fn camera(projection: mln_map_projection) -> Result<mln_camera, mln_status> {
        let mut result = mln_map_projection_camera_result {
            size: size_of::<mln_map_projection_camera_result>() as u32,
            camera: view().camera,
        };
        // SAFETY: a local output and a null diagnostic.
        match unsafe { mln_map_projection_get_camera(projection, &mut result, NULL_DIAGNOSTIC) } {
            MLN_STATUS_OK => Ok(result.camera),
            status => Err(status),
        }
    }

    fn camera_options(fields: u32) -> mln_camera_options {
        mln_camera_options {
            size: size_of::<mln_camera_options>() as u32,
            fields,
            center: mln_lat_lng { latitude: 20.0, longitude: 40.0 },
            center_altitude: 0.0,
            padding: NO_INSETS,
            anchor: mln_screen_point { x: 0.0, y: 0.0 },
            zoom: 24.0,
            bearing: 0.0,
            pitch: 70.0,
            roll: 0.0,
            field_of_view: 0.0,
        }
    }

    #[test]
    fn conversions_agree_singly_and_in_batches_and_invert() {
        let projection = create(&view());
        let center = pixel(projection, 0.0, 0.0);
        assert!((center.x - 100.0).abs() < 1e-6 && (center.y - 50.0).abs() < 1e-6, "{center:?}");
        let north_east = pixel(projection, 10.0, 20.0);
        assert!(north_east.x > center.x && north_east.y < center.y);

        let coordinates =
            [mln_lat_lng { latitude: 0.0, longitude: 0.0 }, mln_lat_lng { latitude: 10.0, longitude: 20.0 }];
        let mut points = [mln_screen_point { x: 0.0, y: 0.0 }; 2];
        let mut back = [mln_lat_lng { latitude: 0.0, longitude: 0.0 }; 2];
        // SAFETY: local arrays of the counts passed, and null diagnostics.
        unsafe {
            let status = mln_map_projection_pixels_for_lat_lngs(
                projection,
                coordinates.as_ptr(),
                2,
                points.as_mut_ptr(),
                NULL_DIAGNOSTIC,
            );
            assert_eq!(status, MLN_STATUS_OK);
            let status = mln_map_projection_lat_lngs_for_pixels(
                projection,
                points.as_ptr(),
                2,
                back.as_mut_ptr(),
                NULL_DIAGNOSTIC,
            );
            assert_eq!(status, MLN_STATUS_OK);
        }
        assert_eq!((points[1].x, points[1].y), (north_east.x, north_east.y));
        assert!((back[1].latitude - 10.0).abs() < 1e-9 && (back[1].longitude - 20.0).abs() < 1e-9);

        // Empty batches take null arrays.
        // SAFETY: null arrays with a zero count.
        unsafe {
            let status = mln_map_projection_pixels_for_lat_lngs(
                projection,
                core::ptr::null(),
                0,
                core::ptr::null_mut(),
                NULL_DIAGNOSTIC,
            );
            assert_eq!(status, MLN_STATUS_OK);
            let status = mln_map_projection_lat_lngs_for_pixels_unwrapped(
                projection,
                core::ptr::null(),
                0,
                core::ptr::null_mut(),
                NULL_DIAGNOSTIC,
            );
            assert_eq!(status, MLN_STATUS_OK);
        }
        mln_map_projection_release(projection);
    }

    #[test]
    fn unwrapped_conversion_keeps_the_world_copy() {
        let mut far_east = view();
        far_east.camera.center.longitude = 540.0;
        let projection = create(&far_east);
        let mut wrapped = mln_lat_lng { latitude: 0.0, longitude: 0.0 };
        let mut unwrapped = wrapped;
        let center = mln_screen_point { x: 100.0, y: 50.0 };
        // SAFETY: local outputs and null diagnostics.
        unsafe {
            let status = mln_map_projection_lat_lng_for_pixel(projection, center, &mut wrapped, NULL_DIAGNOSTIC);
            assert_eq!(status, MLN_STATUS_OK);
            let status =
                mln_map_projection_lat_lng_for_pixel_unwrapped(projection, center, &mut unwrapped, NULL_DIAGNOSTIC);
            assert_eq!(status, MLN_STATUS_OK);
        }
        assert!((wrapped.longitude.abs() - 180.0).abs() < 1e-6, "{wrapped:?}");
        assert!((unwrapped.longitude - 540.0).abs() < 1e-6, "{unwrapped:?}");
        assert!((camera(projection).unwrap().center.longitude - 540.0).abs() < 1e-6);
        mln_map_projection_release(projection);
    }

    #[test]
    fn creation_rejects_invalid_views() {
        let cases: [(&str, fn(&mut mln_map_view), &str); 10] = [
            ("undersized", |view| view.size -= 1, "size"),
            ("past the pole", |view| view.camera.center.latitude = 90.5, "within [-90, 90]"),
            ("negative padding", |view| view.camera.padding.left = -1.0, "padding"),
            ("NaN bearing", |view| view.camera.bearing = f64::NAN, "finite"),
            ("zoom past the limit", |view| view.camera.zoom = 26.0, "limits"),
            ("zero width", |view| view.extent.width = 0, "extent"),
            ("zero scale factor", |view| view.extent.scale_factor = 0.0, "extent"),
            ("infinite skew", |view| view.projection_mode.x_skew = f64::INFINITY, "skews"),
            ("negative frustum offset", |view| view.viewport.frustum_offset.top = -1.0, "frustum offset"),
            ("unknown orientation", |view| view.viewport.north_orientation = 9, "north_orientation"),
        ];
        for (name, change, expected) in cases {
            let mut view = view();
            change(&mut view);
            let mut out = diagnostic();
            let mut projection = handles::NULL;
            // SAFETY: locals.
            let status = unsafe { mln_map_projection_create(&view, &mut projection, &mut *out) };
            assert_eq!(status, MLN_STATUS_INVALID_ARGUMENT, "{name}");
            assert!(message(&out).contains(expected), "{name}: {}", message(&out));
            assert_eq!(projection, handles::NULL);
        }
        let mut occupied = 1;
        // SAFETY: locals, and null pointers.
        unsafe {
            assert_eq!(mln_map_projection_create(&view(), &mut occupied, NULL_DIAGNOSTIC), MLN_STATUS_INVALID_ARGUMENT);
            assert_eq!(occupied, 1);
            let status = mln_map_projection_create(core::ptr::null(), &mut 0, NULL_DIAGNOSTIC);
            assert_eq!(status, MLN_STATUS_INVALID_ARGUMENT);
        }
    }

    #[test]
    fn the_camera_moves_without_the_maps_constraints() {
        let projection = create(&view());
        let options = camera_options(MLN_CAMERA_OPTION_CENTER | MLN_CAMERA_OPTION_ZOOM | MLN_CAMERA_OPTION_PITCH);
        // SAFETY: a local camera and a null diagnostic.
        assert_eq!(unsafe { mln_map_projection_set_camera(projection, &options, NULL_DIAGNOSTIC) }, MLN_STATUS_OK);
        let moved = camera(projection).unwrap();
        assert!((moved.zoom - 24.0).abs() < 1e-9 && (moved.pitch - 70.0).abs() < 1e-9, "{moved:?}");
        assert!((moved.center.latitude - 20.0).abs() < 1e-7 && (moved.center.longitude - 40.0).abs() < 1e-7);

        let mut meters = -1.0;
        // SAFETY: a local output and a null diagnostic.
        unsafe {
            let status = mln_map_projection_meters_per_pixel_at_latitude(projection, 0.0, &mut meters, NULL_DIAGNOSTIC);
            assert_eq!(status, MLN_STATUS_OK);
            assert!((meters - Projection::get_meters_per_pixel_at_latitude(0.0, 24.0)).abs() < 1e-12);
            let status =
                mln_map_projection_meters_per_pixel_at_latitude(projection, f64::NAN, &mut meters, NULL_DIAGNOSTIC);
            assert_eq!(status, MLN_STATUS_INVALID_ARGUMENT);
        }

        let invalid = mln_camera_options { zoom: f64::NAN, ..camera_options(MLN_CAMERA_OPTION_ZOOM) };
        // SAFETY: a local camera and a null diagnostic.
        assert_eq!(
            unsafe { mln_map_projection_set_camera(projection, &invalid, NULL_DIAGNOSTIC) },
            MLN_STATUS_INVALID_ARGUMENT
        );
        assert_eq!(camera(projection).unwrap().zoom, moved.zoom);
        let small = mln_camera_options { size: options.size - 1, ..options };
        // SAFETY: as above.
        assert_eq!(
            unsafe { mln_map_projection_set_camera(projection, &small, NULL_DIAGNOSTIC) },
            MLN_STATUS_INVALID_ARGUMENT
        );
        mln_map_projection_release(projection);
    }

    #[test]
    fn invalid_inputs_leave_the_outputs_alone() {
        let projection = create(&view());
        let mut point = mln_screen_point { x: -1.0, y: -2.0 };
        let mut coordinate = mln_lat_lng { latitude: -1.0, longitude: -2.0 };
        let coordinates =
            [mln_lat_lng { latitude: 0.0, longitude: 0.0 }, mln_lat_lng { latitude: 91.0, longitude: 0.0 }];
        let mut points = [mln_screen_point { x: -1.0, y: -2.0 }; 2];
        // SAFETY: locals, nulls and null diagnostics.
        unsafe {
            let status = mln_map_projection_pixel_for_lat_lng(projection, coordinates[1], &mut point, NULL_DIAGNOSTIC);
            assert_eq!(status, MLN_STATUS_INVALID_ARGUMENT);
            let nan = mln_screen_point { x: 0.0, y: f64::NAN };
            let status = mln_map_projection_lat_lng_for_pixel(projection, nan, &mut coordinate, NULL_DIAGNOSTIC);
            assert_eq!(status, MLN_STATUS_INVALID_ARGUMENT);
            let status =
                mln_map_projection_lat_lng_for_pixel(projection, point, core::ptr::null_mut(), NULL_DIAGNOSTIC);
            assert_eq!(status, MLN_STATUS_INVALID_ARGUMENT);
            let status = mln_map_projection_pixels_for_lat_lngs(
                projection,
                coordinates.as_ptr(),
                2,
                points.as_mut_ptr(),
                NULL_DIAGNOSTIC,
            );
            assert_eq!(status, MLN_STATUS_INVALID_ARGUMENT);
            let status = mln_map_projection_pixels_for_lat_lngs(
                handles::NULL,
                coordinates.as_ptr(),
                1,
                points.as_mut_ptr(),
                NULL_DIAGNOSTIC,
            );
            assert_eq!(status, MLN_STATUS_INVALID_ARGUMENT);
        }
        assert_eq!((point.x, point.y), (-1.0, -2.0));
        assert_eq!((coordinate.latitude, coordinate.longitude), (-1.0, -2.0));
        assert!(points.iter().all(|point| (point.x, point.y) == (-1.0, -2.0)));
        mln_map_projection_release(projection);
    }

    #[test]
    fn a_projection_works_and_releases_on_another_thread() {
        let projection = create(&view());
        let zoom = std::thread::spawn(move || {
            let zoom = camera(projection).map(|camera| camera.zoom);
            mln_map_projection_release(projection);
            zoom
        })
        .join()
        .unwrap();
        assert_eq!(zoom, Ok(2.0));
        assert_eq!(camera(projection).err(), Some(MLN_STATUS_INVALID_STATE));
        mln_map_projection_release(projection);
        mln_map_projection_release(handles::NULL);
    }

    #[test]
    fn projected_meters_round_trip_and_check_their_inputs() {
        let coordinate = mln_lat_lng { latitude: 37.7749, longitude: -122.4194 };
        let mut meters = mln_projected_meters { northing: -1.0, easting: -1.0 };
        let mut back = mln_lat_lng { latitude: -1.0, longitude: 0.0 };
        // SAFETY: locals, nulls and null diagnostics.
        unsafe {
            assert_eq!(mln_projected_meters_for_lat_lng(coordinate, &mut meters, NULL_DIAGNOSTIC), MLN_STATUS_OK);
            assert!(meters.northing > 0.0 && meters.easting < 0.0);
            assert_eq!(mln_lat_lng_for_projected_meters(meters, &mut back, NULL_DIAGNOSTIC), MLN_STATUS_OK);
            assert!((back.latitude - coordinate.latitude).abs() < 1e-9);
            assert!((back.longitude - coordinate.longitude).abs() < 1e-9);

            let north = mln_lat_lng { latitude: 90.5, longitude: 0.0 };
            let before = meters;
            let status = mln_projected_meters_for_lat_lng(north, &mut meters, NULL_DIAGNOSTIC);
            assert_eq!(status, MLN_STATUS_INVALID_ARGUMENT);
            assert_eq!(meters.northing, before.northing);
            let nan = mln_projected_meters { northing: f64::NAN, easting: 0.0 };
            assert_eq!(mln_lat_lng_for_projected_meters(nan, &mut back, NULL_DIAGNOSTIC), MLN_STATUS_INVALID_ARGUMENT);
            let status = mln_lat_lng_for_projected_meters(meters, core::ptr::null_mut(), NULL_DIAGNOSTIC);
            assert_eq!(status, MLN_STATUS_INVALID_ARGUMENT);
        }
    }
}
