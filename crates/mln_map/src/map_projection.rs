// Ports include/mln/map/map_projection.hpp and src/mln/map/map_projection.cpp.
//
// gtests-core: added for test/map/map.test.cpp (LatLngForPixelUnwrapped,
// PixelForLatLngAfterZoomAroundAPointAcrossTheAntimeridian).

use mln_map_types::{Camera, CameraOptions, MapView, TransformState};
use mln_util::error::Error;
use mln_util::geo::{EdgeInsets, LatLng, ScreenCoordinate, WrapMode};

use crate::map::{Map, camera_for_lat_lngs};
use crate::transform::Transform;

/// Converts coordinates for one camera and size, without a map. It holds only
/// a transform state, so it can move to another thread.
pub struct MapProjection {
    state: TransformState,
}

const _: () = {
    const fn sendable<T: Send>() {}
    sendable::<MapProjection>();
};

impl MapProjection {
    /// `MapProjection(const Map&)`.
    pub fn new(map: &Map) -> Self {
        Self::from_state(map.get_transform_state())
    }

    /// `MapProjection(const TransformState&)`.
    pub fn from_state(state: TransformState) -> Self {
        MapProjection { state }
    }

    /// A projection for one view of a map, from a [`MapSnapshot`] or a
    /// rendered frame. `Err`: as [`MapView::to_state`].
    ///
    /// [`MapSnapshot`]: crate::MapSnapshot
    pub fn from_view(view: &MapView) -> Result<Self, Error> {
        Ok(Self::from_state(view.to_state()?))
    }

    /// The screen point of `lat_lng`, in the copy of the world nearest the
    /// camera's center.
    pub fn pixel_for_lat_lng(&self, lat_lng: &LatLng) -> ScreenCoordinate {
        let mut point = self.state.lat_lng_to_screen_coordinate(&self.state.lat_lng_near_center(lat_lng));
        point.y = f64::from(self.state.get_size().height) - point.y;
        point
    }

    /// Fails with [`ErrorKind::InvalidArgument`](mln_util::ErrorKind) for a NaN or infinite
    /// pixel.
    pub fn lat_lng_for_pixel(&self, pixel: &ScreenCoordinate, wrap_mode: WrapMode) -> Result<LatLng, Error> {
        let mut flipped = *pixel;
        flipped.y = f64::from(self.state.get_size().height) - flipped.y;
        self.state.screen_coordinate_to_lat_lng(&flipped, wrap_mode)
    }

    pub fn set_camera(&mut self, camera: &CameraOptions) {
        let mut transform = self.transform();
        transform.jump_to(camera);
        self.state = transform.get_state().clone();
    }

    /// The camera, with its center in the copy of the world that it is in.
    pub fn camera(&self) -> Camera {
        self.state.camera()
    }

    /// Set the underneath camera so the requested coordinates are visible
    /// with the inset.
    /// Fails as [`Map::camera_for_lat_lngs`](crate::Map::camera_for_lat_lngs)
    /// does.
    pub fn set_visible_coordinates(&mut self, lat_lngs: &[LatLng], padding: &EdgeInsets) -> Result<(), Error> {
        let mut transform = self.transform();
        let camera = camera_for_lat_lngs(lat_lngs, &transform, padding)?
            .with_bearing(Some(mln_math::rad2deg(-transform.get_bearing())))
            .with_pitch(Some(mln_math::rad2deg(transform.get_pitch())));
        transform.jump_to(&camera);
        self.state = transform.get_state().clone();
        Ok(())
    }

    /// A transform over a copy of the state, to move its camera as a map
    /// would.
    fn transform(&self) -> Transform {
        Transform::from_state(self.state.clone())
    }
}
