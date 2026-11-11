//! One view of a map: the camera, size and projection that a map snapshot or a
//! rendered frame shows.

use mln_util::constants;
use mln_util::geo::{EdgeInsets, LatLng, NorthOrientation};
use mln_util::size::Size;

use crate::camera::CameraOptions;
use crate::mode::{ConstrainMode, ViewportMode};
use crate::projection_mode::ProjectionMode;
use crate::transform_state::TransformState;

/// A camera with every member set: the camera that a map has, where
/// [`CameraOptions`] is a change to one.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Camera {
    /// The coordinate at the center. It keeps the copy of the world that the
    /// camera is in, so its longitude can be outside -180 to 180.
    pub center: LatLng,
    /// The altitude of the center, in meters above sea level.
    pub center_altitude: f64,
    /// Insets from the map's edges. The camera centers within the rest.
    pub padding: EdgeInsets,
    pub zoom: f64,
    /// The rotation from north, in degrees.
    pub bearing: f64,
    /// The tilt toward the horizon, in degrees. 0 looks straight down.
    pub pitch: f64,
    /// The roll, in degrees.
    pub roll: f64,
    /// The vertical field of view, in degrees.
    pub fov: f64,
}

impl From<Camera> for CameraOptions {
    /// Options that set every member of the camera, and no anchor.
    fn from(camera: Camera) -> Self {
        CameraOptions {
            center: Some(camera.center),
            center_altitude: Some(camera.center_altitude),
            padding: Some(camera.padding),
            anchor: None,
            zoom: Some(camera.zoom),
            bearing: Some(camera.bearing),
            pitch: Some(camera.pitch),
            roll: Some(camera.roll),
            fov: Some(camera.fov),
        }
    }
}

/// A size in logical pixels, and the physical pixels in each logical pixel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LogicalExtent {
    pub size: Size,
    pub scale_factor: f64,
}

/// How a map orients and constrains its view.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Viewport {
    pub north_orientation: NorthOrientation,
    pub constrain_mode: ConstrainMode,
    pub viewport_mode: ViewportMode,
    /// Insets that move the vanishing point off the center of the map.
    pub frustum_offset: EdgeInsets,
}

/// One view of a map: its camera, size, and projection.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MapView {
    pub camera: Camera,
    pub extent: LogicalExtent,
    /// Sets every member when it comes from a map. A member that a view leaves
    /// unset takes the default of [`TransformState`].
    pub projection_mode: ProjectionMode,
    pub viewport: Viewport,
}

impl MapView {
    /// The view that `state` shows, at `scale_factor` physical pixels in each
    /// logical pixel.
    pub fn from_state(state: &TransformState, scale_factor: f64) -> MapView {
        MapView {
            camera: state.camera(),
            extent: LogicalExtent { size: state.get_size(), scale_factor },
            projection_mode: ProjectionMode {
                axonometric: Some(state.get_axonometric()),
                x_skew: Some(state.get_x_skew()),
                y_skew: Some(state.get_y_skew()),
            },
            viewport: Viewport {
                north_orientation: state.get_north_orientation(),
                constrain_mode: state.get_constrain_mode(),
                viewport_mode: state.get_viewport_mode(),
                frustum_offset: state.get_frustum_offset(),
            },
        }
    }

    /// A transform state that shows exactly this view, to convert coordinates
    /// for it.
    ///
    /// The state takes the camera as it is: it keeps the center's copy of the
    /// world, and has no bounds and the widest zoom and pitch limits, so no
    /// constraint of the map that the view came from moves the camera.
    ///
    /// `Err` for a view that no transform can show: a camera member that isn't
    /// finite, a zoom, pitch or field of view outside the limits of every map,
    /// a padding or frustum offset below zero, an empty size, a scale factor
    /// that isn't positive, or a skew that isn't finite.
    pub fn to_state(&self) -> Result<TransformState, String> {
        self.validate()?;
        let camera = &self.camera;
        let viewport = &self.viewport;
        let mut state = TransformState::new(ConstrainMode::None, viewport.viewport_mode);
        state.set_max_zoom(constants::MAX_ZOOM);
        state.set_max_pitch(constants::PITCH_MAX);
        state.set_size(self.extent.size);
        state.set_north_orientation(viewport.north_orientation);
        state.set_frustum_offset(&viewport.frustum_offset);
        state.set_edge_insets(&camera.padding);
        state.set_lat_lng_zoom(&camera.center, camera.zoom);
        state.set_center_altitude(camera.center_altitude);
        state.set_bearing(-mln_math::deg2rad(camera.bearing));
        state.set_pitch(mln_math::deg2rad(camera.pitch));
        state.set_roll(mln_math::deg2rad(camera.roll));
        state.set_field_of_view(mln_math::deg2rad(camera.fov));
        let mode = &self.projection_mode;
        if let Some(axonometric) = mode.axonometric {
            state.set_axonometric(axonometric);
        }
        if let Some(x_skew) = mode.x_skew {
            state.set_x_skew(x_skew);
        }
        if let Some(y_skew) = mode.y_skew {
            state.set_y_skew(y_skew);
        }
        // Last, so that no constraint applies while the members are set.
        state.set_constrain_mode(viewport.constrain_mode);
        Ok(state)
    }

    fn validate(&self) -> Result<(), String> {
        let camera = &self.camera;
        let finite = [camera.center_altitude, camera.zoom, camera.bearing, camera.pitch, camera.roll, camera.fov];
        if !finite.iter().all(|value| value.is_finite()) {
            return Err("view camera members must be finite".to_owned());
        }
        let limits = TransformState::default();
        if !(constants::MIN_ZOOM..=constants::MAX_ZOOM).contains(&camera.zoom)
            || !(mln_math::rad2deg(constants::PITCH_MIN)..=mln_math::rad2deg(constants::PITCH_MAX))
                .contains(&camera.pitch)
            || !(mln_math::rad2deg(limits.get_min_field_of_view())..=mln_math::rad2deg(limits.get_max_field_of_view()))
                .contains(&camera.fov)
        {
            return Err("view camera zoom, pitch and field of view must be within the limits of a map".to_owned());
        }
        if !valid_insets(&camera.padding) || !valid_insets(&self.viewport.frustum_offset) {
            return Err("view padding and frustum offset must be finite and at least 0".to_owned());
        }
        let extent = &self.extent;
        if extent.size.is_empty() || !extent.scale_factor.is_finite() || extent.scale_factor <= 0.0 {
            return Err("view extent dimensions and scale factor must be positive".to_owned());
        }
        let mode = &self.projection_mode;
        if !mode.x_skew.is_none_or(f64::is_finite) || !mode.y_skew.is_none_or(f64::is_finite) {
            return Err("view projection skews must be finite".to_owned());
        }
        Ok(())
    }
}

fn valid_insets(insets: &EdgeInsets) -> bool {
    [insets.top, insets.left, insets.bottom, insets.right].iter().all(|value| value.is_finite() && *value >= 0.0)
}
