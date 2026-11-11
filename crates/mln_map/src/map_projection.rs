// Ports include/mln/map/map_projection.hpp and src/mln/map/map_projection.cpp.
//
// gtests-core: added for test/map/map.test.cpp (LatLngForPixelUnwrapped,
// PixelForLatLngAfterZoomAroundAPointAcrossTheAntimeridian).

use mln_map_types::{CameraOptions, MapView, TransformState};
use mln_util::geo::{EdgeInsets, LatLng, ScreenCoordinate, WrapMode};

use crate::map::{Map, camera_for_lat_lngs};
use crate::transform::Transform;

pub struct MapProjection {
    transform: Transform,
}

impl MapProjection {
    /// `MapProjection(const Map&)`.
    pub fn new(map: &Map) -> Self {
        Self::from_state(map.get_transform_state())
    }

    /// `MapProjection(const TransformState&)`.
    pub fn from_state(state: TransformState) -> Self {
        MapProjection { transform: Transform::from_state(state) }
    }

    /// A projection for one view of a map, from a [`MapSnapshot`] or a
    /// rendered frame. `Err`: as [`MapView::to_state`].
    ///
    /// [`MapSnapshot`]: crate::MapSnapshot
    pub fn from_view(view: &MapView) -> Result<Self, String> {
        Ok(Self::from_state(view.to_state()?))
    }

    pub fn pixel_for_lat_lng(&self, lat_lng: &LatLng) -> ScreenCoordinate {
        // The implementation is just a copy from map.cpp
        self.transform.lat_lng_to_screen_coordinate(&self.transform.get_state().lat_lng_near_center(lat_lng))
    }

    /// `Err`: the std::domain_error of the C++ LatLng constructor.
    pub fn lat_lng_for_pixel(&self, pixel: &ScreenCoordinate, wrap_mode: WrapMode) -> Result<LatLng, String> {
        // The implementation is just a copy from map.cpp
        self.transform.screen_coordinate_to_lat_lng(pixel, wrap_mode)
    }

    pub fn set_camera(&mut self, camera: &CameraOptions) {
        self.transform.jump_to(camera);
    }

    pub fn get_camera(&self) -> CameraOptions {
        self.transform.get_camera_options(None)
    }

    /// Set the underneath camera so the requested coordinates are visible
    /// with the inset.
    /// `Err`: as `Map::camera_for_lat_lngs`.
    pub fn set_visible_coordinates(&mut self, lat_lngs: &[LatLng], padding: &EdgeInsets) -> Result<(), String> {
        let camera = camera_for_lat_lngs(lat_lngs, &self.transform, padding)?
            .with_bearing(Some(mln_math::rad2deg(-self.transform.get_bearing())))
            .with_pitch(Some(mln_math::rad2deg(self.transform.get_pitch())));
        self.transform.jump_to(&camera);
        Ok(())
    }
}
