//! The options of the camera commands that the core map takes as separate
//! calls: fitting an area, the viewport and the tile settings.

use mln_map::{
    AnimationOptions, CameraOptions, CameraUpdateMode, ConstrainMode, NorthOrientation, TileLodMode, ViewportMode,
};
use mln_util::error::Error;
use mln_util::geo::{EdgeInsets, LatLng, LatLngBounds};
use mln_util::geometry::{Geometry, for_each_point};

/// An area for a camera to fit in the map's view.
#[derive(Clone, Debug, PartialEq)]
pub enum CameraFitArea {
    Bounds(LatLngBounds),
    /// At least one coordinate.
    Coordinates(Vec<LatLng>),
    /// A geometry with at least one coordinate, whose points are longitude
    /// `x` and latitude `y`.
    Geometry(Geometry<f64>),
}

/// How a camera fits an area.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CameraFitOptions {
    /// The space to leave between the area and each edge of the map. The
    /// left and right insets add up to less than the map's width, and the top
    /// and bottom to less than its height.
    pub padding: EdgeInsets,
    /// The rotation from north, in degrees. `None` keeps the map's.
    pub bearing: Option<f64>,
    /// The tilt toward the horizon, in degrees. `None` keeps the map's.
    pub pitch: Option<f64>,
}

/// Moves the camera to fit an area: [`CameraUpdate`](mln_map::CameraUpdate)'s
/// counterpart for [`Map::fit_camera`](crate::Map::fit_camera).
#[derive(Clone, Default)]
pub struct CameraFitUpdate {
    pub mode: CameraUpdateMode,
    pub fit: CameraFitOptions,
    pub animation: AnimationOptions,
}

/// How the map orients and constrains its view. A setting that is `None`
/// keeps its value.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ViewportOptions {
    pub north_orientation: Option<NorthOrientation>,
    pub constrain_mode: Option<ConstrainMode>,
    pub viewport_mode: Option<ViewportMode>,
    pub frustum_offset: Option<EdgeInsets>,
}

/// How the map picks the tiles to load, as its snapshot's
/// [`TileSettings`](mln_map::TileSettings) report it. A setting that is
/// `None` keeps its value.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TileOptions {
    pub prefetch_zoom_delta: Option<u8>,
    pub lod_min_radius: Option<f64>,
    pub lod_scale: Option<f64>,
    /// In degrees.
    pub lod_pitch_threshold: Option<f64>,
    pub lod_zoom_shift: Option<f64>,
    pub lod_mode: Option<TileLodMode>,
}

/// The camera that fits `area` in `map`'s view. Fails with
/// [`ErrorKind::InvalidArgument`](mln_util::ErrorKind) for an empty area,
/// and for padding that leaves no room for it, for which the core would keep
/// the zoom.
pub(crate) fn camera_for(
    map: &mln_map::Map,
    area: &CameraFitArea,
    options: &CameraFitOptions,
) -> Result<CameraOptions, Error> {
    let size = map.get_transform_state().get_size();
    let padding = options.padding;
    if padding.left + padding.right >= f64::from(size.width) || padding.top + padding.bottom >= f64::from(size.height) {
        return Err(Error::invalid_argument("camera fit padding must be less than the map's width and height"));
    }
    let CameraFitOptions { padding, bearing, pitch } = *options;
    match area {
        CameraFitArea::Bounds(bounds) => map.camera_for_lat_lng_bounds(bounds, &padding, bearing, pitch),
        CameraFitArea::Coordinates(coordinates) if coordinates.is_empty() => {
            Err(Error::invalid_argument("coordinates must not be empty"))
        }
        CameraFitArea::Coordinates(coordinates) => map.camera_for_lat_lngs(coordinates, &padding, bearing, pitch),
        CameraFitArea::Geometry(geometry) => {
            let mut empty = true;
            for_each_point(geometry, &mut |_| empty = false);
            if empty {
                return Err(Error::invalid_argument("geometry must contain at least one coordinate"));
            }
            map.camera_for_geometry(geometry, &padding, bearing, pitch)
        }
    }
}

pub(crate) fn set_viewport(map: &mut mln_map::Map, options: &ViewportOptions) {
    if let Some(orientation) = options.north_orientation {
        map.set_north_orientation(orientation);
    }
    if let Some(mode) = options.constrain_mode {
        map.set_constrain_mode(mode);
    }
    if let Some(mode) = options.viewport_mode {
        map.set_viewport_mode(mode);
    }
    if let Some(offset) = options.frustum_offset {
        map.set_frustum_offset(&offset);
    }
}

pub(crate) fn set_tile_options(map: &mut mln_map::Map, options: &TileOptions) {
    if let Some(delta) = options.prefetch_zoom_delta {
        map.set_prefetch_zoom_delta(delta);
    }
    if let Some(radius) = options.lod_min_radius {
        map.set_tile_lod_min_radius(radius);
    }
    if let Some(scale) = options.lod_scale {
        map.set_tile_lod_scale(scale);
    }
    // Degrees don't round-trip through radians exactly, so the threshold that
    // a snapshot reports converts to another angle: sending it back keeps the
    // map's, rather than changing it a little.
    if let Some(threshold) = options.lod_pitch_threshold
        && threshold != mln_math::rad2deg(map.get_tile_lod_pitch_threshold())
    {
        map.set_tile_lod_pitch_threshold(mln_math::deg2rad(threshold));
    }
    if let Some(shift) = options.lod_zoom_shift {
        map.set_tile_lod_zoom_shift(shift);
    }
    if let Some(mode) = options.lod_mode {
        map.set_tile_lod_mode(mode);
    }
}
