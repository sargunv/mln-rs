//! `camera.h`: cameras as the headers spell them, and the camera that shows
//! one tile.

use mln_map_types::{Camera, CameraOptions};
use mln_util::constants::{MAX_ZOOM, MIN_ZOOM, TILE_SIZE_D, TILE_SIZE_I};
use mln_util::geo::{ScreenCoordinate, WrapMode};
use mln_util::projection::Projection;

use crate::abi::{self, Fields};
use crate::error::Error;
use crate::generated::types::{
    MLN_CAMERA_OPTION_ANCHOR, MLN_CAMERA_OPTION_BEARING, MLN_CAMERA_OPTION_CENTER, MLN_CAMERA_OPTION_CENTER_ALTITUDE,
    MLN_CAMERA_OPTION_FOV, MLN_CAMERA_OPTION_PADDING, MLN_CAMERA_OPTION_PITCH, MLN_CAMERA_OPTION_ROLL,
    MLN_CAMERA_OPTION_ZOOM, mln_camera, mln_camera_options, mln_canonical_tile_id, mln_diagnostic, mln_edge_insets,
    mln_logical_size, mln_screen_point, mln_status,
};
use crate::geo;

abi::versioned!(mln_camera_options);

const KNOWN_CAMERA_FIELDS: u32 = MLN_CAMERA_OPTION_CENTER
    | MLN_CAMERA_OPTION_ZOOM
    | MLN_CAMERA_OPTION_BEARING
    | MLN_CAMERA_OPTION_PITCH
    | MLN_CAMERA_OPTION_CENTER_ALTITUDE
    | MLN_CAMERA_OPTION_PADDING
    | MLN_CAMERA_OPTION_ANCHOR
    | MLN_CAMERA_OPTION_ROLL
    | MLN_CAMERA_OPTION_FOV;

/// The change that a caller's camera options make, with each member that
/// `fields` sets checked.
pub(crate) fn camera_options(camera: &mln_camera_options) -> Result<CameraOptions, Error> {
    let fields = Fields::new(camera.fields, KNOWN_CAMERA_FIELDS, "mln_camera_options.fields")?;
    let number = |bit, value: f64| match fields.get(bit, value) {
        Some(value) if !value.is_finite() => {
            Err(Error::invalid_argument("enabled camera numeric fields must be finite"))
        }
        value => Ok(value),
    };
    Ok(CameraOptions {
        center: fields.get(MLN_CAMERA_OPTION_CENTER, camera.center).map(geo::lat_lng).transpose()?,
        center_altitude: number(MLN_CAMERA_OPTION_CENTER_ALTITUDE, camera.center_altitude)?,
        zoom: number(MLN_CAMERA_OPTION_ZOOM, camera.zoom)?,
        bearing: number(MLN_CAMERA_OPTION_BEARING, camera.bearing)?,
        pitch: number(MLN_CAMERA_OPTION_PITCH, camera.pitch)?,
        roll: number(MLN_CAMERA_OPTION_ROLL, camera.roll)?,
        fov: number(MLN_CAMERA_OPTION_FOV, camera.field_of_view)?,
        padding: fields
            .get(MLN_CAMERA_OPTION_PADDING, camera.padding)
            .map(|padding| geo::edge_insets(padding, "padding"))
            .transpose()?,
        anchor: fields.get(MLN_CAMERA_OPTION_ANCHOR, camera.anchor).map(geo::screen_point).transpose()?,
    })
}

/// [`camera_options`] for a caller's pointer.
///
/// # Safety
///
/// As for [`abi::read_sized`].
pub(crate) unsafe fn read_camera_options(camera: *const mln_camera_options) -> Result<CameraOptions, Error> {
    // SAFETY: the caller's contract.
    camera_options(&unsafe { abi::read_sized(camera) }?)
}

/// A camera with every member set.
pub(crate) fn from_camera(camera: Camera) -> mln_camera {
    mln_camera {
        center: geo::from_lat_lng(camera.center),
        center_altitude: camera.center_altitude,
        padding: geo::from_edge_insets(camera.padding),
        zoom: camera.zoom,
        bearing: camera.bearing,
        pitch: camera.pitch,
        roll: camera.roll,
        field_of_view: camera.fov,
    }
}

/// A tile map shows a tile that touches the view's edge within rounding as
/// well, so the camera zooms in by a sliver, which moves the tile's edges by
/// under 0.02 logical pixels, to keep the neighbors out.
const TILE_EDGE_MARGIN_ZOOM: f64 = 1e-4;

/// The camera that fills a `logical_size` map with `tile`: centered on it,
/// straight down with north up, and with no padding.
fn camera_for_tile(tile: mln_canonical_tile_id, logical_size: mln_logical_size) -> Result<mln_camera_options, Error> {
    // A vector source's tiles are TILE_SIZE logical pixels wide at their own
    // zoom, so a wider map would show more than one.
    let side = logical_size.width;
    if side == 0 || side != logical_size.height || side > u32::from(TILE_SIZE_I) {
        return Err(Error::invalid_argument(format!("logical_size must be square, from 1 to {TILE_SIZE_I} wide")));
    }
    // A narrower map shows the tile at a zoom offset by the ratio of the
    // widths.
    let fit_zoom = f64::from(tile.z) + (f64::from(side) / TILE_SIZE_D).log2();
    let zoom = fit_zoom + TILE_EDGE_MARGIN_ZOOM;
    if fit_zoom < MIN_ZOOM || zoom > MAX_ZOOM {
        return Err(Error::invalid_argument(format!(
            "a map of logical_size can't show a tile at zoom tile.z from zoom {MIN_ZOOM} to {MAX_ZOOM}"
        )));
    }
    // The zoom check bounds tile.z well below 32.
    let tiles = 1u64 << tile.z;
    if u64::from(tile.x) >= tiles || u64::from(tile.y) >= tiles {
        return Err(Error::invalid_argument("tile.x and tile.y must be less than 2^tile.z"));
    }
    // The tile's center, in world pixels at a scale of 2^z.
    let center =
        ScreenCoordinate::new((f64::from(tile.x) + 0.5) * TILE_SIZE_D, (f64::from(tile.y) + 0.5) * TILE_SIZE_D);
    let center = Projection::unproject(&center, tiles as f64, WrapMode::Unwrapped)?;
    Ok(mln_camera_options {
        size: size_of::<mln_camera_options>() as u32,
        fields: MLN_CAMERA_OPTION_CENTER
            | MLN_CAMERA_OPTION_ZOOM
            | MLN_CAMERA_OPTION_BEARING
            | MLN_CAMERA_OPTION_PITCH
            | MLN_CAMERA_OPTION_ROLL
            | MLN_CAMERA_OPTION_PADDING
            | MLN_CAMERA_OPTION_CENTER_ALTITUDE,
        center: geo::from_lat_lng(center),
        center_altitude: 0.0,
        padding: mln_edge_insets { top: 0.0, left: 0.0, bottom: 0.0, right: 0.0 },
        anchor: mln_screen_point { x: 0.0, y: 0.0 },
        zoom,
        bearing: 0.0,
        pitch: 0.0,
        roll: 0.0,
        field_of_view: 0.0,
    })
}

/// `mln_camera_for_tile()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_camera_for_tile(
    tile: mln_canonical_tile_id,
    logical_size: mln_logical_size,
    out_camera: *mut mln_camera_options,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        let camera = camera_for_tile(tile, logical_size)?;
        // SAFETY: the caller's contract.
        unsafe { abi::write_sized(out_camera, &camera) }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::types::{MLN_STATUS_INVALID_ARGUMENT, MLN_STATUS_OK, mln_lat_lng};

    fn options(fields: u32) -> mln_camera_options {
        mln_camera_options {
            size: size_of::<mln_camera_options>() as u32,
            fields,
            center: mln_lat_lng { latitude: 10.0, longitude: 20.0 },
            center_altitude: 0.0,
            padding: mln_edge_insets { top: 1.0, left: 0.0, bottom: 0.0, right: 0.0 },
            anchor: mln_screen_point { x: 0.0, y: 0.0 },
            zoom: 3.0,
            bearing: 0.0,
            pitch: 0.0,
            roll: 0.0,
            field_of_view: f64::NAN,
        }
    }

    #[test]
    fn camera_options_set_only_their_fields() {
        let camera = camera_options(&options(MLN_CAMERA_OPTION_CENTER | MLN_CAMERA_OPTION_ZOOM)).unwrap();
        assert_eq!(camera.center.map(|center| center.longitude()), Some(20.0));
        assert_eq!(camera.zoom, Some(3.0));
        assert_eq!((camera.padding, camera.fov), (None, None));

        let error = camera_options(&options(MLN_CAMERA_OPTION_FOV)).unwrap_err();
        assert_eq!(error.message(), "enabled camera numeric fields must be finite");
        assert!(camera_options(&options(1 << 9)).unwrap_err().message().contains("unknown bits"));
        let mut past_the_pole = options(MLN_CAMERA_OPTION_CENTER);
        past_the_pole.center.latitude = 91.0;
        assert!(camera_options(&past_the_pole).is_err());
    }

    fn call(tile: (u32, u32, u32), side: (u32, u32), out: &mut mln_camera_options) -> mln_status {
        let tile = mln_canonical_tile_id { z: tile.0, x: tile.1, y: tile.2 };
        // SAFETY: a local out-parameter and a null diagnostic.
        unsafe {
            mln_camera_for_tile(tile, mln_logical_size { width: side.0, height: side.1 }, out, core::ptr::null_mut())
        }
    }

    #[test]
    fn a_tile_camera_centers_the_tile_at_its_zoom() {
        let mut camera = options(0);
        assert_eq!(call((0, 0, 0), (512, 512), &mut camera), MLN_STATUS_OK);
        assert!(camera.center.latitude.abs() < 1e-9 && camera.center.longitude.abs() < 1e-9);
        assert!((camera.zoom - TILE_EDGE_MARGIN_ZOOM).abs() < 1e-12);
        assert_eq!(camera.padding.top, 0.0);

        assert_eq!(call((11, 1205, 1539), (256, 256), &mut camera), MLN_STATUS_OK);
        assert!((camera.zoom - (10.0 + TILE_EDGE_MARGIN_ZOOM)).abs() < 1e-12);
        assert!((camera.center.longitude - ((1205.5 / 2048.0) * 360.0 - 180.0)).abs() < 1e-9);
    }

    #[test]
    fn a_tile_camera_rejects_a_tile_the_map_cannot_fill() {
        let mut camera = options(0);
        for (tile, side) in [
            ((2, 1, 3), (512, 256)),
            ((2, 1, 3), (0, 0)),
            ((2, 1, 3), (1024, 1024)),
            ((2, 4, 3), (512, 512)),
            ((0, 0, 0), (256, 256)),
            ((26, 0, 0), (512, 512)),
            ((u32::MAX, 0, 0), (512, 512)),
        ] {
            assert_eq!(call(tile, side, &mut camera), MLN_STATUS_INVALID_ARGUMENT, "{tile:?} {side:?}");
        }
        assert_eq!(camera.zoom, 3.0);
        assert_eq!(call((25, 33554431, 33554431), (512, 512), &mut camera), MLN_STATUS_OK);
        assert_eq!(call((9, 0, 0), (1, 1), &mut camera), MLN_STATUS_OK);
    }
}
