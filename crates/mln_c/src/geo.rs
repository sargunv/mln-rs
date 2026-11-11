//! The headers' geometry values: coordinates, screen points, insets and tile
//! IDs, checked as they enter and converted to the core's types.

use mln_tile_id::CanonicalTileID;
use mln_util::geo::{EdgeInsets, LatLng, ScreenCoordinate};

use crate::error::Error;
use crate::generated::types::{mln_canonical_tile_id, mln_edge_insets, mln_lat_lng, mln_screen_point};

/// A coordinate with a finite latitude from -90 to 90 and a finite longitude,
/// which keeps the copy of the world that the longitude names.
pub(crate) fn lat_lng(coordinate: mln_lat_lng) -> Result<LatLng, Error> {
    let mln_lat_lng { latitude, longitude } = coordinate;
    if !(-90.0..=90.0).contains(&latitude) || !longitude.is_finite() {
        return Err(Error::invalid_argument(
            "latitude must be finite and within [-90, 90], and longitude must be finite",
        ));
    }
    Ok(LatLng::new(latitude, longitude)?)
}

pub(crate) fn from_lat_lng(coordinate: LatLng) -> mln_lat_lng {
    mln_lat_lng { latitude: coordinate.latitude(), longitude: coordinate.longitude() }
}

/// Each coordinate of a caller's array, checked: `count` of them at
/// `coordinates`, which may be null only when `count` is 0.
///
/// # Safety
///
/// `coordinates` is null or points to `count` readable coordinates.
pub(crate) unsafe fn lat_lngs(coordinates: *const mln_lat_lng, count: usize) -> Result<Vec<LatLng>, Error> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if coordinates.is_null() {
        return Err(Error::invalid_argument("coordinates must not be null"));
    }
    // SAFETY: non-null, and the caller's contract.
    let coordinates = unsafe { core::slice::from_raw_parts(coordinates, count) };
    coordinates.iter().copied().map(lat_lng).collect()
}

/// A screen point with finite members.
pub(crate) fn screen_point(point: mln_screen_point) -> Result<ScreenCoordinate, Error> {
    if !point.x.is_finite() || !point.y.is_finite() {
        return Err(Error::invalid_argument("screen point values must be finite"));
    }
    Ok(ScreenCoordinate::new(point.x, point.y))
}

pub(crate) fn from_screen_point(point: ScreenCoordinate) -> mln_screen_point {
    mln_screen_point { x: point.x, y: point.y }
}

/// Each point of a caller's array, checked, as [`lat_lngs`] reads
/// coordinates.
///
/// # Safety
///
/// `points` is null or points to `count` readable points.
pub(crate) unsafe fn screen_points(
    points: *const mln_screen_point,
    count: usize,
) -> Result<Vec<ScreenCoordinate>, Error> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if points.is_null() {
        return Err(Error::invalid_argument("points must not be null"));
    }
    // SAFETY: non-null, and the caller's contract.
    let points = unsafe { core::slice::from_raw_parts(points, count) };
    points.iter().copied().map(screen_point).collect()
}

/// Insets that are finite and at least 0. `name` names them in diagnostics.
pub(crate) fn edge_insets(insets: mln_edge_insets, name: &str) -> Result<EdgeInsets, Error> {
    let mln_edge_insets { top, left, bottom, right } = insets;
    if ![top, left, bottom, right].iter().all(|value| value.is_finite() && *value >= 0.0) {
        return Err(Error::invalid_argument(format!("{name} values must be finite and greater than or equal to 0")));
    }
    Ok(EdgeInsets { top, left, bottom, right })
}

pub(crate) fn from_edge_insets(insets: EdgeInsets) -> mln_edge_insets {
    mln_edge_insets { top: insets.top, left: insets.left, bottom: insets.bottom, right: insets.right }
}

/// A tile ID at a zoom from 0 to 32 whose `x` and `y` are within the zoom.
pub(crate) fn canonical_tile_id(tile: mln_canonical_tile_id, name: &str) -> Result<CanonicalTileID, Error> {
    let Some(z) = u8::try_from(tile.z).ok().filter(|z| *z <= 32) else {
        return Err(Error::invalid_argument(format!("{name}.z must be within [0, 32]")));
    };
    let limit = 1u64 << z;
    if u64::from(tile.x) >= limit || u64::from(tile.y) >= limit {
        return Err(Error::invalid_argument(format!("{name}.x and {name}.y must be less than 2^{name}.z")));
    }
    Ok(CanonicalTileID::new(z, tile.x, tile.y))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinates_must_be_on_the_globe() {
        let inside = lat_lng(mln_lat_lng { latitude: 90.0, longitude: 540.0 }).unwrap();
        assert_eq!((inside.latitude(), inside.longitude()), (90.0, 540.0));
        for (latitude, longitude) in [(90.5, 0.0), (f64::NAN, 0.0), (0.0, f64::INFINITY)] {
            let error = lat_lng(mln_lat_lng { latitude, longitude }).unwrap_err();
            assert!(error.message().contains("within [-90, 90]"), "{error}");
        }
    }

    #[test]
    fn arrays_may_be_null_only_when_empty() {
        // SAFETY: null pointers, and a local array of the count passed.
        unsafe {
            assert!(lat_lngs(core::ptr::null(), 0).unwrap().is_empty());
            assert_eq!(lat_lngs(core::ptr::null(), 1).unwrap_err().message(), "coordinates must not be null");
            let points = [mln_screen_point { x: 1.0, y: 2.0 }, mln_screen_point { x: f64::NAN, y: 0.0 }];
            assert_eq!(screen_points(points.as_ptr(), 1).unwrap(), vec![ScreenCoordinate::new(1.0, 2.0)]);
            assert!(screen_points(points.as_ptr(), 2).is_err());
            assert_eq!(screen_points(core::ptr::null(), 2).unwrap_err().message(), "points must not be null");
        }
    }

    #[test]
    fn insets_are_finite_and_not_negative() {
        let insets = mln_edge_insets { top: 1.0, left: 2.0, bottom: 3.0, right: 4.0 };
        assert_eq!(from_edge_insets(edge_insets(insets, "padding").unwrap()).right, 4.0);
        let negative = mln_edge_insets { left: -1.0, ..insets };
        assert!(edge_insets(negative, "padding").unwrap_err().message().starts_with("padding values"));
        assert!(edge_insets(mln_edge_insets { top: f64::INFINITY, ..insets }, "padding").is_err());
    }

    #[test]
    fn tile_ids_stay_within_their_zoom() {
        let tile = canonical_tile_id(mln_canonical_tile_id { z: 32, x: u32::MAX, y: 0 }, "tile").unwrap();
        assert_eq!((tile.z, tile.x), (32, u32::MAX));
        assert!(canonical_tile_id(mln_canonical_tile_id { z: 33, x: 0, y: 0 }, "tile").is_err());
        assert!(canonical_tile_id(mln_canonical_tile_id { z: u32::MAX, x: 0, y: 0 }, "tile").is_err());
        let outside = canonical_tile_id(mln_canonical_tile_id { z: 1, x: 2, y: 0 }, "tile_id").unwrap_err();
        assert_eq!(outside.message(), "tile_id.x and tile_id.y must be less than 2^tile_id.z");
    }
}
