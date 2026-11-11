// Ports src/mln/map/transform_state.hpp and src/mln/map/transform_state.cpp.
//
// The C++ class caches its matrices and camera in `mutable` members that
// const getters refresh (`updateMatricesIfNeeded`). The port keeps that
// laziness with `Cell`s, so getters that returned `const mat4&` return the
// matrix by value. TransformState is therefore `!Sync`, like the C++ class
// (not thread-safe either).
//
// `updateMatricesIfNeeded` throws `std::runtime_error` when a matrix can't be
// inverted; nothing catches it on the paths that run, so the port panics
// with the same message.

use std::cell::Cell;
use std::f64::consts::PI;

use geometry::Point;
use mln_tile_id::UnwrappedTileID;
use mln_util::constants;
use mln_util::constants::{DEFAULT_FOV, DEFAULT_MAX_ZOOM, DEFAULT_PITCH_MAX, M2PI, PITCH_MAX, TILE_SIZE_D};
pub use mln_util::geo::NorthOrientation;
use mln_util::geo::{EdgeInsets, LatLng, LatLngBounds, ScreenCoordinate, WrapMode};
use mln_util::interpolate::interpolate;
use mln_util::logging::{Event, Log};
use mln_util::mat4::{Mat4, matrix};
use mln_util::projection::Projection;
use mln_util::quaternion::Quaternion;
use mln_util::size::Size;
use mln_util::vectors::{Vec3, Vec4};

use crate::camera::{CameraOptions, FreeCameraOptions};
use crate::map_view::Camera as MapCamera;
use crate::mode::{ConstrainMode, ViewportMode};
use crate::tile_coordinate::TileCoordinate;
use crate::util_camera::Camera;

/// The maximum angle to use for the Mercator horizon. This must be less than
/// 90 to prevent errors in `MercatorTransform::_calcMatrices()`. It shouldn't
/// be too close to 90, or the distance to the horizon will become very large,
/// unnecessarily increasing the number of tiles needed to render the map.
fn max_mercator_horizon_angle() -> f64 {
    mln_math::deg2rad(89.25)
}

fn lat_lng(lat: f64, lon: f64, mode: WrapMode) -> LatLng {
    // C++ constructs LatLng directly, which throws std::domain_error on
    // invalid input; nothing catches it there.
    LatLng::with_mode(lat, lon, mode).unwrap_or_else(|e| panic!("{e}"))
}

fn lat_lng_from_mercator(mercator_coordinate: Point<f64>, wrap_mode: WrapMode) -> LatLng {
    lat_lng(
        mln_math::rad2deg(2.0 * (PI - mercator_coordinate.y * 2.0 * PI).exp().atan() - (PI / 2.0)),
        mercator_coordinate.x * 360.0 - 180.0,
        wrap_mode,
    )
}

const K_EPSILON: f64 = 1e-9;

/// To avoid flickering issue due to "zoom = 13.9999999..".
fn round_for_accuracy(x: f64) -> f64 {
    let round_x = x.round();
    let diff = (round_x - x).abs();
    if diff < K_EPSILON && diff > 0.0 { round_x } else { x }
}

/// `std::min` (returns `a` unless `b < a`).
fn std_min(a: f64, b: f64) -> f64 {
    if b < a { b } else { a }
}

/// `std::max` (returns `a` unless `a < b`).
fn std_max(a: f64, b: f64) -> f64 {
    if a < b { b } else { a }
}

/// `std::modf` fractional part (same sign as `x`).
fn modf_fraction(x: f64) -> f64 {
    if x.is_infinite() {
        return 0.0f64.copysign(x);
    }
    (x - x.trunc()).copysign(x)
}

/// `TransformStateProperties`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TransformStateProperties {
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub z: Option<f64>,
    pub fov: Option<f64>,
    pub scale: Option<f64>,
    pub bearing: Option<f64>,
    pub pitch: Option<f64>,
    pub roll: Option<f64>,
    pub x_skew: Option<f64>,
    pub y_skew: Option<f64>,
    pub axonometric: Option<bool>,
    pub panning: Option<bool>,
    pub scaling: Option<bool>,
    pub rotating: Option<bool>,
    pub edge_insets: Option<EdgeInsets>,
    pub size: Option<Size>,
    pub constrain: Option<ConstrainMode>,
    pub north_orientation: Option<NorthOrientation>,
    pub view_port_mode: Option<ViewportMode>,
    pub frustum_offset: Option<EdgeInsets>,
}

impl TransformStateProperties {
    pub fn with_x(mut self, val: Option<f64>) -> Self {
        self.x = val;
        self
    }
    pub fn with_y(mut self, val: Option<f64>) -> Self {
        self.y = val;
        self
    }
    pub fn with_z(mut self, val: Option<f64>) -> Self {
        self.z = val;
        self
    }
    pub fn with_scale(mut self, val: Option<f64>) -> Self {
        self.scale = val;
        self
    }
    pub fn with_fov(mut self, val: Option<f64>) -> Self {
        self.fov = val;
        self
    }
    pub fn with_bearing(mut self, val: Option<f64>) -> Self {
        self.bearing = val;
        self
    }
    pub fn with_pitch(mut self, val: Option<f64>) -> Self {
        self.pitch = val;
        self
    }
    pub fn with_roll(mut self, val: Option<f64>) -> Self {
        self.roll = val;
        self
    }
    pub fn with_x_skew(mut self, val: Option<f64>) -> Self {
        self.x_skew = val;
        self
    }
    pub fn with_y_skew(mut self, val: Option<f64>) -> Self {
        self.y_skew = val;
        self
    }
    pub fn with_axonometric(mut self, val: Option<bool>) -> Self {
        self.axonometric = val;
        self
    }
    pub fn with_panning_in_progress(mut self, val: Option<bool>) -> Self {
        self.panning = val;
        self
    }
    pub fn with_scaling_in_progress(mut self, val: Option<bool>) -> Self {
        self.scaling = val;
        self
    }
    pub fn with_rotating_in_progress(mut self, val: Option<bool>) -> Self {
        self.rotating = val;
        self
    }
    pub fn with_edge_insets(mut self, val: Option<EdgeInsets>) -> Self {
        self.edge_insets = val;
        self
    }
    pub fn with_size(mut self, val: Option<Size>) -> Self {
        self.size = val;
        self
    }
    pub fn with_constrain_mode(mut self, val: Option<ConstrainMode>) -> Self {
        self.constrain = val;
        self
    }
    pub fn with_north_orientation(mut self, val: Option<NorthOrientation>) -> Self {
        self.north_orientation = val;
        self
    }
    pub fn with_viewport_mode(mut self, val: Option<ViewportMode>) -> Self {
        self.view_port_mode = val;
        self
    }
    pub fn with_frustum_offset(mut self, val: Option<EdgeInsets>) -> Self {
        self.frustum_offset = val;
        self
    }
}

#[derive(Clone, Debug)]
pub struct TransformState {
    bounds: LatLngBounds,

    // Limit the amount of zooming possible on the map.
    min_scale: f64,
    max_scale: f64,

    // Limit the amount of pitch
    min_pitch: f64,
    max_pitch: f64,
    min_fov: f64,
    max_fov: f64,

    orientation: NorthOrientation,

    // logical dimensions
    size: Size,
    frustum_offset: EdgeInsets,

    constrain_mode: ConstrainMode,
    viewport_mode: ViewportMode,

    // animation state
    rotating: bool,
    scaling: bool,
    panning: bool,
    gesture_in_progress: bool,

    // map position
    x: f64,
    y: f64,
    z: f64,
    bearing: f64,
    scale: f64,
    fov: f64,
    pitch: f64,
    roll: f64,
    x_skew: f64,
    y_skew: f64,
    axonometric: bool,

    edge_insets: EdgeInsets,
    camera: Cell<Camera>,

    // cache values for spherical mercator math
    bc: f64,
    cc: f64,

    request_matrices_update: Cell<bool>,
    projection_matrix: Cell<Mat4>,
    inv_projection_matrix: Cell<Mat4>,
    coord_matrix: Cell<Mat4>,
    inverted_matrix: Cell<Mat4>,
}

impl Default for TransformState {
    fn default() -> Self {
        TransformState::new(ConstrainMode::HeightOnly, ViewportMode::Default)
    }
}

impl TransformState {
    pub fn new(constrain_mode: ConstrainMode, viewport_mode: ViewportMode) -> Self {
        let scale = 1.0;
        TransformState {
            bounds: LatLngBounds::default(),
            min_scale: 2.0f64.powf(0.0),
            max_scale: 2.0f64.powf(DEFAULT_MAX_ZOOM as f64),
            min_pitch: constants::PITCH_MIN,
            max_pitch: DEFAULT_PITCH_MAX,
            min_fov: mln_math::deg2rad(0.1),
            max_fov: mln_math::deg2rad(150.0),
            orientation: NorthOrientation::Upwards,
            size: Size::default(),
            frustum_offset: EdgeInsets::default(),
            constrain_mode,
            viewport_mode,
            rotating: false,
            scaling: false,
            panning: false,
            gesture_in_progress: false,
            x: 0.0,
            y: 0.0,
            z: 0.0,
            bearing: 0.0,
            scale,
            fov: DEFAULT_FOV,
            pitch: 0.0,
            roll: 0.0,
            x_skew: 0.0,
            y_skew: 1.0,
            axonometric: false,
            edge_insets: EdgeInsets::default(),
            camera: Cell::new(Camera::new()),
            bc: Projection::world_size(scale) / constants::DEGREES_MAX,
            cc: Projection::world_size(scale) / M2PI,
            request_matrices_update: Cell::new(true),
            projection_matrix: Cell::new([0.0; 16]),
            inv_projection_matrix: Cell::new([0.0; 16]),
            coord_matrix: Cell::new([0.0; 16]),
            inverted_matrix: Cell::new([0.0; 16]),
        }
    }

    pub fn set_properties(&mut self, properties: &TransformStateProperties) {
        if let Some(x) = properties.x {
            self.set_x(x);
        }
        if let Some(y) = properties.y {
            self.set_y(y);
        }
        if let Some(z) = properties.z {
            self.set_z(z);
        }
        if let Some(scale) = properties.scale {
            self.set_scale(scale);
        }
        if let Some(bearing) = properties.bearing {
            self.set_bearing(bearing);
        }
        if let Some(fov) = properties.fov {
            self.set_field_of_view(fov);
        }
        if let Some(pitch) = properties.pitch {
            self.set_pitch(pitch);
        }
        if let Some(roll) = properties.roll {
            self.set_roll(roll);
        }
        if let Some(x_skew) = properties.x_skew {
            self.set_x_skew(x_skew);
        }
        if let Some(y_skew) = properties.y_skew {
            self.set_y_skew(y_skew);
        }
        if let Some(axonometric) = properties.axonometric {
            self.set_axonometric(axonometric);
        }
        if let Some(panning) = properties.panning {
            self.set_panning_in_progress(panning);
        }
        if let Some(scaling) = properties.scaling {
            self.set_scaling_in_progress(scaling);
        }
        if let Some(rotating) = properties.rotating {
            self.set_rotating_in_progress(rotating);
        }
        if let Some(edge_insets) = properties.edge_insets {
            self.set_edge_insets(&edge_insets);
        }
        if let Some(size) = properties.size {
            self.set_size(size);
        }
        if let Some(constrain) = properties.constrain {
            self.set_constrain_mode(constrain);
        }
        if let Some(north_orientation) = properties.north_orientation {
            self.set_north_orientation(north_orientation);
        }
        if let Some(view_port_mode) = properties.view_port_mode {
            self.set_viewport_mode(view_port_mode);
        }
        if let Some(frustum_offset) = properties.frustum_offset {
            self.set_frustum_offset(&frustum_offset);
        }
    }

    // MARK: - Matrix

    pub fn matrix_for(&self, matrix: &mut Mat4, tile_id: &UnwrappedTileID) {
        let tile_scale: u64 = 1u64 << tile_id.canonical.z;
        let s = Projection::world_size(self.scale) / tile_scale as f64;

        matrix::identity(matrix);
        let a = *matrix;
        matrix::translate(
            matrix,
            &a,
            (tile_id.canonical.x as i64 + tile_id.wrap as i64 * tile_scale as i64) as f64 * s,
            (tile_id.canonical.y as i64) as f64 * s,
            0.0,
        );
        let a = *matrix;
        matrix::scale(matrix, &a, s / constants::EXTENT as f64, s / constants::EXTENT as f64, 1.0);
    }

    /// `getProjMatrix(mat4&, uint16_t nearZ = 1, bool aligned = false)`.
    pub fn get_proj_matrix(&self, proj_matrix: &mut Mat4, near_z: u16, aligned: bool) {
        if self.size.is_empty() {
            return;
        }

        let camera_to_center_distance = self.get_camera_to_center_distance() as f64;
        let offset = self.get_center_offset();

        let limited_pitch = mln_math::clamp_f64(self.get_pitch(), 0.0, max_mercator_horizon_angle());
        let camera_to_sea_level_distance = camera_to_center_distance + self.z.abs() / limited_pitch.cos();

        // Find the Z distance from the viewport center point
        // [width/2 + offset.x, height/2 + offset.y] to the top edge; to point
        // [width/2 + offset.x, 0] in Z units.
        // 1 Z unit is equivalent to 1 horizontal px at the center of the map
        // (the distance between[width/2, height/2] and [width/2 + 1, height/2])
        // See https://github.com/mapbox/mapbox-gl-native/pull/15195 for details.
        // See TransformState::fov description: fov = 2 * arctan((height / 2) / (height * 1.5)).
        let width = self.size.width as f64;
        let height = self.size.height as f64;
        let tan_fov_above_center = (0.5 + (offset.y - self.frustum_offset.top) / height)
            * 2.0
            * (self.fov / 2.0).tan()
            * (self.roll.cos().abs() + self.roll.sin().abs() * width / height);
        let tan_multiple = mln_math::clamp_f64(tan_fov_above_center * limited_pitch.tan(), 0.0, 0.99);
        debug_assert!(tan_multiple < 1.0);
        // Calculate z distance of the farthest fragment that should be rendered.
        let furthest_distance = camera_to_sea_level_distance / (1.0 - tan_multiple);
        // Add a bit extra to avoid precision problems when a fragment's
        // distance is exactly `furthestDistance`
        let far_z = furthest_distance * 1.01;

        // Make sure the camera state is up-to-date
        self.update_camera_state();

        let camera = self.camera.get();
        let world_to_camera = camera.get_world_to_camera(self.scale, self.viewport_mode == ViewportMode::FlippedY);
        // getFieldOfView() returns float.
        let mut camera_to_clip = camera.get_camera_to_clip_perspective(
            self.get_field_of_view() as f64,
            width / height,
            near_z as f64,
            far_z,
        );

        // Move the center of perspective to center of specified edgeInsets.
        // Values are in range [-1, 1] where the upper and lower range values
        // position viewport center to the screen edges. This is overridden
        // if using axonometric perspective (not in public API yet, Issue
        // #11882).
        // TODO(astojilj): Issue #11882 should take edge insets into account, too.
        if !self.axonometric {
            camera_to_clip[8] = -offset.x * 2.0 / width;
            camera_to_clip[9] = offset.y * 2.0 / height;
        }

        // Apply north orientation angle
        if self.get_north_orientation() != NorthOrientation::Upwards {
            let a = camera_to_clip;
            matrix::rotate_z(&mut camera_to_clip, &a, -self.get_north_orientation_angle());
        }

        matrix::multiply(proj_matrix, &camera_to_clip, &world_to_camera);

        if self.axonometric {
            // mat[11] controls perspective
            proj_matrix[11] = 0.0;

            // mat[8], mat[9] control x-skew, y-skew
            let pixels_per_meter = 1.0
                / Projection::get_meters_per_pixel_at_latitude(
                    self.get_lat_lng(WrapMode::Unwrapped).latitude(),
                    self.get_zoom(),
                );
            proj_matrix[8] = self.x_skew * pixels_per_meter;
            proj_matrix[9] = self.y_skew * pixels_per_meter;
        }

        // Make a second projection matrix that is aligned to a pixel grid for
        // rendering raster tiles. We're rounding the (floating point) x/y
        // values to achieve to avoid rendering raster images to fractional
        // coordinates. Additionally, we adjust by half a pixel in either
        // direction in case that viewport dimension is an odd integer to
        // preserve rendering to the pixel grid. We're rotating this shift
        // based on the angle of the transformation so that 0°, 90°, 180°, and
        // 270° rasters are crisp, and adjust the shift so that it is always
        // <= 0.5 pixels.

        if aligned {
            let world_size = Projection::world_size(self.scale);
            let dx = self.x - 0.5 * world_size;
            let dy = self.y - 0.5 * world_size;

            let x_shift = (self.size.width % 2) as f64 / 2.0;
            let y_shift = (self.size.height % 2) as f64 / 2.0;
            let bearing_cos = self.bearing.cos();
            let bearing_sin = self.bearing.sin();
            let dxa = -modf_fraction(dx) + bearing_cos * x_shift + bearing_sin * y_shift;
            let dya = -modf_fraction(dy) + bearing_cos * y_shift + bearing_sin * x_shift;
            let a = *proj_matrix;
            matrix::translate(
                proj_matrix,
                &a,
                if dxa > 0.5 { dxa - 1.0 } else { dxa },
                if dya > 0.5 { dya - 1.0 } else { dya },
                0.0,
            );
        }
    }

    fn update_camera_state(&self) {
        if !self.valid() {
            return;
        }

        let world_size = Projection::world_size(self.scale);
        let camera_to_center_distance = self.get_camera_to_center_distance() as f64;

        // x & y tracks the center of the map in pixels. However as rendering
        // is done in pixel coordinates the rendering origo is actually in the
        // middle of the map (0.5 * worldSize). x&y positions have to be
        // negated because it defines position of the map, not the camera.
        // Moving map 10 units left has the same effect as moving camera 10
        // units to the right.
        let dx = 0.5 * world_size - self.x;
        let dy = 0.5 * world_size - self.y;

        // Set camera orientation and move it to a proper distance from the map
        let mut camera = self.camera.get();
        camera.set_orientation(self.get_roll(), self.get_pitch(), self.get_bearing());

        let forward = camera.forward();
        let orbit_position: Vec3 = [
            -forward[0] * camera_to_center_distance,
            -forward[1] * camera_to_center_distance,
            -forward[2] * camera_to_center_distance,
        ];
        let mut camera_position: Vec3 = [dx + orbit_position[0], dy + orbit_position[1], self.z + orbit_position[2]];

        camera_position[0] /= world_size;
        camera_position[1] /= world_size;
        camera_position[2] /= world_size;

        camera.set_position(&camera_position);
        self.camera.set(camera);
    }

    fn update_state_from_camera(&mut self) {
        let camera = self.camera.get();
        let position = camera.get_position();
        let forward = camera.forward();

        let dx = forward[0];
        let dy = forward[1];
        let dz = forward[2];

        // Compute bearing and pitch
        let (new_roll, mut new_pitch, new_bearing) = camera.get_orientation_angles();
        new_pitch = mln_math::clamp_f64(new_pitch, self.min_pitch, self.max_pitch);

        // Compute zoom level from the camera altitude
        let center_distance = self.get_camera_to_center_distance() as f64;
        let zoom;
        let new_scale;
        let travel;
        if dz < -1.0e-9 && position[2] > 1.0e-9 && new_pitch <= max_mercator_horizon_angle() {
            zoom = (center_distance / (position[2] / new_pitch.cos() * TILE_SIZE_D)).log2();
            new_scale = mln_math::clamp_f64(2.0f64.powf(zoom), self.min_scale, self.max_scale);
            travel = -position[2] / dz;
        } else {
            zoom = 14.0;
            new_scale = mln_math::clamp_f64(2.0f64.powf(zoom), self.min_scale, self.max_scale);
            travel = center_distance / new_scale / TILE_SIZE_D;
        }

        // Compute center point of the map
        let mercator_point = Point::new(position[0] + dx * travel, position[1] + dy * travel);
        let scale_zoom = self.scale_zoom(new_scale);
        self.set_lat_lng_zoom(&lat_lng_from_mercator(mercator_point, WrapMode::Unwrapped), scale_zoom);

        let mercator_z = position[2] + dz * travel;
        let alt_m = mercator_z
            * Projection::get_meters_per_pixel_at_latitude(self.get_lat_lng(WrapMode::Unwrapped).latitude(), 0.0)
            * TILE_SIZE_D;
        self.set_center_altitude(alt_m);
        self.set_bearing(new_bearing);
        self.set_pitch(new_pitch);
        self.set_roll(new_roll);
    }

    pub fn get_free_camera_options(&self) -> FreeCameraOptions {
        self.update_camera_state();

        let camera = self.camera.get();
        FreeCameraOptions { position: Some(camera.get_position()), orientation: Some(camera.get_orientation().m()) }
    }

    fn set_camera_position(&mut self, position: &Vec3) -> bool {
        if position[0].is_nan() || position[1].is_nan() || position[2].is_nan() {
            return false;
        }

        let max_world_size = Projection::world_size(2.0f64.powf(self.get_max_zoom()));
        let min_world_size = Projection::world_size(2.0f64.powf(self.get_min_zoom()));
        let dist_to_center = self.get_camera_to_center_distance() as f64;

        let updated_pos: Vec3 = [
            position[0],
            position[1],
            mln_math::clamp_f64(position[2], dist_to_center / max_world_size, dist_to_center / min_world_size),
        ];

        let mut camera = self.camera.get();
        camera.set_position(&updated_pos);
        self.camera.set(camera);
        true
    }

    fn set_camera_orientation(&mut self, orientation: &Quaternion) -> bool {
        let c = orientation.m();
        if c[0].is_nan() || c[1].is_nan() || c[2].is_nan() || c[3].is_nan() {
            return false;
        }

        // Zero-length quaternions are not valid
        if orientation.length() == 0.0 {
            return false;
        }

        let unit_quat = orientation.normalized();
        let forward = unit_quat.transform(&[0.0, 0.0, -1.0]);
        let up = unit_quat.transform(&[0.0, -1.0, 0.0]);

        let Some(updated_orientation) = Camera::orientation_from_frame(&forward, &up) else {
            return false;
        };

        let mut camera = self.camera.get();
        camera.set_orientation_quaternion(&updated_orientation);
        self.camera.set(camera);
        true
    }

    pub fn set_free_camera_options(&mut self, options: &FreeCameraOptions) {
        if !self.valid() {
            return;
        }

        if options.position.is_none() && options.orientation.is_none() {
            return;
        }

        // Check if the state is dirty and camera needs to be synchronized
        self.update_matrices_if_needed();

        let mut changed = false;
        if let Some(orientation) = options.orientation {
            if orientation != self.camera.get().get_orientation().m() {
                changed |= self.set_camera_orientation(&Quaternion::from_vec4(&orientation));
            }
        }

        if let Some(position) = options.position {
            if position != self.camera.get().get_position() {
                changed |= self.set_camera_position(&position);
            }
        }

        if changed {
            self.update_state_from_camera();
            self.request_matrices_update.set(true);
        }
    }

    fn update_matrices_if_needed(&self) {
        if !self.needs_matrices_update() || self.size.is_empty() {
            return;
        }

        let mut projection_matrix = self.projection_matrix.get();
        self.get_proj_matrix(&mut projection_matrix, 1, false);
        self.projection_matrix.set(projection_matrix);
        let coord_matrix = self.coordinate_point_matrix(&projection_matrix);
        self.coord_matrix.set(coord_matrix);

        let mut inv_projection_matrix = self.inv_projection_matrix.get();
        let err = matrix::invert(&mut inv_projection_matrix, &projection_matrix);
        self.inv_projection_matrix.set(inv_projection_matrix);
        if err {
            panic!("failed to invert projectionMatrix");
        }

        let mut inverted_matrix = self.inverted_matrix.get();
        let err = matrix::invert(&mut inverted_matrix, &coord_matrix);
        self.inverted_matrix.set(inverted_matrix);
        if err {
            panic!("failed to invert coordinatePointMatrix");
        }

        self.request_matrices_update.set(false);
    }

    fn needs_matrices_update(&self) -> bool {
        self.request_matrices_update.get()
    }

    pub fn get_projection_matrix(&self) -> Mat4 {
        self.update_matrices_if_needed();
        self.projection_matrix.get()
    }

    pub fn get_inv_projection_matrix(&self) -> Mat4 {
        self.update_matrices_if_needed();
        self.inv_projection_matrix.get()
    }

    fn get_coord_matrix(&self) -> Mat4 {
        self.update_matrices_if_needed();
        self.coord_matrix.get()
    }

    fn get_inverted_matrix(&self) -> Mat4 {
        self.update_matrices_if_needed();
        self.inverted_matrix.get()
    }

    // MARK: - Dimensions

    pub fn get_size(&self) -> Size {
        self.size
    }

    pub fn set_size(&mut self, size: Size) {
        if self.size != size {
            self.size = size;
            self.request_matrices_update.set(true);
        }
    }

    pub fn get_frustum_offset(&self) -> EdgeInsets {
        self.frustum_offset
    }

    pub fn set_frustum_offset(&mut self, frustum_offset: &EdgeInsets) {
        if self.frustum_offset != *frustum_offset {
            self.frustum_offset = *frustum_offset;
            self.request_matrices_update.set(true);
        }
    }

    // MARK: - North Orientation

    pub fn get_north_orientation(&self) -> NorthOrientation {
        self.orientation
    }

    pub fn set_north_orientation(&mut self, val: NorthOrientation) {
        if self.orientation != val {
            self.orientation = val;
            self.request_matrices_update.set(true);
        }
    }

    pub fn get_north_orientation_angle(&self) -> f64 {
        let mut angle_orientation = 0.0;
        if self.orientation == NorthOrientation::Rightwards {
            angle_orientation += PI / 2.0;
        } else if self.orientation == NorthOrientation::Downwards {
            angle_orientation += PI;
        } else if self.orientation == NorthOrientation::Leftwards {
            angle_orientation -= PI / 2.0;
        }
        angle_orientation
    }

    // MARK: - Constrain mode

    pub fn get_constrain_mode(&self) -> ConstrainMode {
        self.constrain_mode
    }

    pub fn set_constrain_mode(&mut self, val: ConstrainMode) {
        if self.constrain_mode != val {
            self.constrain_mode = val;
            self.request_matrices_update.set(true);
        }
    }

    // MARK: - ViewportMode

    pub fn get_viewport_mode(&self) -> ViewportMode {
        self.viewport_mode
    }

    pub fn set_viewport_mode(&mut self, val: ViewportMode) {
        if self.viewport_mode != val {
            self.viewport_mode = val;
            self.request_matrices_update.set(true);
        }
    }

    // MARK: - Camera options

    pub fn get_camera_options(&self, padding: Option<EdgeInsets>) -> CameraOptions {
        let camera = MapCamera { padding: padding.unwrap_or(self.edge_insets), ..self.camera() };
        CameraOptions::from(camera)
    }

    /// The camera, with the center in the copy of the world that it is in.
    pub fn camera(&self) -> MapCamera {
        MapCamera {
            center: self.get_lat_lng(WrapMode::Unwrapped),
            center_altitude: self.get_center_altitude(),
            padding: self.edge_insets,
            zoom: self.get_zoom(),
            bearing: mln_math::rad2deg(-self.bearing),
            pitch: mln_math::rad2deg(self.pitch),
            roll: mln_math::rad2deg(self.roll),
            fov: mln_math::rad2deg(self.fov),
        }
    }

    // MARK: - EdgeInsets

    pub fn get_edge_insets(&self) -> EdgeInsets {
        self.edge_insets
    }

    pub fn set_edge_insets(&mut self, val: &EdgeInsets) {
        if self.edge_insets != *val {
            self.edge_insets = *val;
            self.request_matrices_update.set(true);
        }
    }

    // MARK: - Position

    /// `getLatLng(LatLng::WrapMode = LatLng::Unwrapped)`.
    pub fn get_lat_lng(&self, wrap_mode: WrapMode) -> LatLng {
        lat_lng(mln_math::rad2deg(2.0 * (self.y / self.cc).exp().atan() - 0.5 * PI), -self.x / self.bc, wrap_mode)
    }

    /// `latLngNearCenter(latLng)`: the world copy of a location nearest the
    /// center, which bounds can keep past the antimeridian.
    pub fn lat_lng_near_center(&self, location: &LatLng) -> LatLng {
        let center = self.get_lat_lng(WrapMode::Unwrapped);
        let wrapped_center = center.wrapped();
        let mut nearest = location.wrapped();
        nearest.unwrap_for_shortest_path(&wrapped_center);
        lat_lng(
            nearest.latitude(),
            nearest.longitude() + center.longitude() - wrapped_center.longitude(),
            WrapMode::Unwrapped,
        )
    }

    pub fn get_center_altitude(&self) -> f64 {
        self.z
            * Projection::get_meters_per_pixel_at_latitude(
                self.get_lat_lng(WrapMode::Unwrapped).latitude(),
                self.get_zoom(),
            )
    }

    pub fn pixel_x(&self) -> f64 {
        let center = (self.size.width as f64 - Projection::world_size(self.scale)) / 2.0;
        center + self.x
    }

    pub fn pixel_y(&self) -> f64 {
        let center = (self.size.height as f64 - Projection::world_size(self.scale)) / 2.0;
        center + self.y
    }

    // MARK: - Zoom

    pub fn get_zoom(&self) -> f64 {
        self.scale_zoom(self.scale)
    }

    /// `static_cast<uint8_t>(getZoom())`.
    pub fn get_integer_zoom(&self) -> u8 {
        self.get_zoom() as u8
    }

    pub fn get_zoom_fraction(&self) -> f64 {
        self.get_zoom() - self.get_integer_zoom() as f64
    }

    // MARK: - Bounds

    pub fn set_lat_lng_bounds(&mut self, bounds: LatLngBounds) {
        if bounds != self.bounds {
            self.bounds = bounds;
            let lat_lng = self.get_lat_lng(WrapMode::Unwrapped);
            let zoom = self.get_zoom();
            self.set_lat_lng_zoom(&lat_lng, zoom);
        }
    }

    pub fn get_lat_lng_bounds(&self) -> LatLngBounds {
        self.bounds
    }

    pub fn set_min_zoom(&mut self, min_zoom: f64) {
        if min_zoom <= self.get_max_zoom() {
            self.min_scale = self.zoom_scale(mln_math::clamp_f64(min_zoom, constants::MIN_ZOOM, constants::MAX_ZOOM));
        }
    }

    pub fn get_min_zoom(&self) -> f64 {
        let mut test_scale = self.min_scale;
        let mut unused_x = self.x;
        let mut unused_y = self.y;
        self.constrain(&mut test_scale, &mut unused_x, &mut unused_y);

        self.scale_zoom(test_scale)
    }

    pub fn set_max_zoom(&mut self, max_zoom: f64) {
        if max_zoom >= self.get_min_zoom() {
            self.max_scale = self.zoom_scale(mln_math::clamp_f64(max_zoom, constants::MIN_ZOOM, constants::MAX_ZOOM));
        }
    }

    pub fn get_max_zoom(&self) -> f64 {
        self.scale_zoom(self.max_scale)
    }

    pub fn set_min_pitch(&mut self, pitch: f64) {
        if pitch <= self.max_pitch {
            self.min_pitch = mln_math::clamp_f64(pitch, constants::PITCH_MIN, self.max_pitch);
        } else {
            Log::warning(Event::General, "Trying to set minimum pitch to larger than maximum pitch, no changes made.");
        }
    }

    pub fn get_min_pitch(&self) -> f64 {
        self.min_pitch
    }

    pub fn set_max_pitch(&mut self, pitch: f64) {
        if pitch >= self.min_pitch {
            self.max_pitch = mln_math::clamp_f64(pitch, self.min_pitch, PITCH_MAX);
        } else {
            Log::warning(Event::General, "Trying to set maximum pitch to smaller than minimum pitch, no changes made.");
        }
    }

    pub fn get_max_pitch(&self) -> f64 {
        self.max_pitch
    }

    pub fn get_min_field_of_view(&self) -> f64 {
        self.min_fov
    }

    pub fn get_max_field_of_view(&self) -> f64 {
        self.max_fov
    }

    // MARK: - Scale

    pub fn get_scale(&self) -> f64 {
        self.scale
    }

    pub fn set_scale(&mut self, val: f64) {
        if self.scale != val {
            self.scale = val;
            self.request_matrices_update.set(true);
        }
    }

    // MARK: - Positions

    pub fn get_x(&self) -> f64 {
        self.x
    }

    pub fn set_x(&mut self, val: f64) {
        if self.x != val {
            self.x = val;
            self.request_matrices_update.set(true);
        }
    }

    pub fn get_y(&self) -> f64 {
        self.y
    }

    pub fn set_y(&mut self, val: f64) {
        if self.y != val {
            self.y = val;
            self.request_matrices_update.set(true);
        }
    }

    pub fn get_z(&self) -> f64 {
        self.z
    }

    pub fn set_z(&mut self, val: f64) {
        if self.z != val {
            self.z = val;
            self.request_matrices_update.set(true);
        }
    }

    // MARK: - Rotation

    pub fn get_bearing(&self) -> f64 {
        self.bearing
    }

    pub fn set_bearing(&mut self, val: f64) {
        if self.bearing != val {
            self.bearing = val;
            self.request_matrices_update.set(true);
        }
    }

    /// Returns float, as C++ does.
    pub fn get_field_of_view(&self) -> f32 {
        self.fov as f32
    }

    pub fn set_field_of_view(&mut self, val: f64) {
        if self.fov != val {
            self.fov = val;
            self.request_matrices_update.set(true);
        }
    }

    pub fn get_roll(&self) -> f64 {
        self.roll
    }

    pub fn set_roll(&mut self, val: f64) {
        if self.roll != val {
            self.roll = val;
            self.request_matrices_update.set(true);
        }
    }

    /// Returns float, as C++ does.
    pub fn get_camera_to_center_distance(&self) -> f32 {
        (0.5 * self.size.height as f64 / (self.fov / 2.0).tan()) as f32
    }

    pub fn get_pitch(&self) -> f64 {
        self.pitch
    }

    pub fn set_pitch(&mut self, val: f64) {
        if self.pitch != val {
            self.pitch = val;
            self.request_matrices_update.set(true);
        }
    }

    pub fn get_x_skew(&self) -> f64 {
        self.x_skew
    }

    pub fn set_x_skew(&mut self, val: f64) {
        if self.x_skew != val {
            self.x_skew = val;
            self.request_matrices_update.set(true);
        }
    }

    pub fn get_y_skew(&self) -> f64 {
        self.y_skew
    }

    pub fn set_y_skew(&mut self, val: f64) {
        if self.y_skew != val {
            self.y_skew = val;
            self.request_matrices_update.set(true);
        }
    }

    pub fn get_axonometric(&self) -> bool {
        self.axonometric
    }

    pub fn set_axonometric(&mut self, val: bool) {
        if self.axonometric != val {
            self.axonometric = val;
            self.request_matrices_update.set(true);
        }
    }

    // MARK: - State

    pub fn is_changing(&self) -> bool {
        self.rotating || self.scaling || self.panning || self.gesture_in_progress
    }

    pub fn is_rotating(&self) -> bool {
        self.rotating
    }

    pub fn set_rotating_in_progress(&mut self, val: bool) {
        self.rotating = val;
    }

    pub fn is_scaling(&self) -> bool {
        self.scaling
    }

    pub fn set_scaling_in_progress(&mut self, val: bool) {
        self.scaling = val;
    }

    pub fn is_panning(&self) -> bool {
        self.panning
    }

    pub fn set_panning_in_progress(&mut self, val: bool) {
        self.panning = val;
    }

    pub fn is_gesture_in_progress(&self) -> bool {
        self.gesture_in_progress
    }

    pub fn set_gesture_in_progress(&mut self, val: bool) {
        self.gesture_in_progress = val;
    }

    // MARK: - Projection

    pub fn zoom_scale(&self, zoom: f64) -> f64 {
        round_for_accuracy(2.0f64.powf(zoom))
    }

    pub fn scale_zoom(&self, s: f64) -> f64 {
        round_for_accuracy(s.log2())
    }

    pub fn valid(&self) -> bool {
        !self.size.is_empty() && (self.scale >= self.min_scale && self.scale <= self.max_scale)
    }

    pub fn lat_lng_to_screen_coordinate(&self, lat_lng: &LatLng) -> ScreenCoordinate {
        let mut p: Vec4 = [0.0; 4];
        self.lat_lng_to_screen_coordinate_with(lat_lng, &mut p)
    }

    /// `latLngToScreenCoordinate(const LatLng&, vec4&)`.
    pub fn lat_lng_to_screen_coordinate_with(&self, lat_lng: &LatLng, p: &mut Vec4) -> ScreenCoordinate {
        if self.size.is_empty() {
            return ScreenCoordinate::new(0.0, 0.0);
        }

        let pt = Projection::project(lat_lng, self.scale);
        let pt = Point::new(pt.x / TILE_SIZE_D, pt.y / TILE_SIZE_D);
        let c: Vec4 = [pt.x, pt.y, 0.0, 1.0];
        matrix::transform_mat4(p, &c, &self.get_coord_matrix());
        ScreenCoordinate::new(p[0] / p[3], self.size.height as f64 - p[1] / p[3])
    }

    /// Implements mapbox-gl-js pointCoordinate() : MercatorCoordinate.
    pub fn screen_coordinate_to_tile_coordinate(&self, point: &ScreenCoordinate, at_zoom: u8) -> TileCoordinate {
        if self.size.is_empty() {
            return TileCoordinate { p: Point::new(0.0, 0.0), z: 0.0 };
        }

        let target_z: f32 = 0.0;

        let flipped_y = self.size.height as f64 - point.y;

        // since we don't know the correct projected z value for the point,
        // unproject two points to get a line and then find the point on that
        // line with z=0

        let mut coord0: Vec4 = [0.0; 4];
        let mut coord1: Vec4 = [0.0; 4];
        let point0: Vec4 = [point.x, flipped_y, 0.0, 1.0];
        let point1: Vec4 = [point.x, flipped_y, 1.0, 1.0];
        let inverted = self.get_inverted_matrix();
        matrix::transform_mat4(&mut coord0, &point0, &inverted);
        matrix::transform_mat4(&mut coord1, &point1, &inverted);

        let w0 = coord0[3];
        let w1 = coord1[3];

        let p0 = Point::new(coord0[0] / w0, coord0[1] / w0);
        let p1 = Point::new(coord1[0] / w1, coord1[1] / w1);

        let z0 = coord0[2] / w0;
        let z1 = coord1[2] / w1;
        let t = if z0 <= z1 { 0.0 } else { (target_z as f64 - z0) / (z1 - z0) };

        let p = interpolate(&p0, &p1, t);
        // C++ `static_cast<double>(1 << atZoom)`: an int shift.
        let factor = 1i32.wrapping_shl(at_zoom as u32) as f64;
        TileCoordinate { p: Point::new(p.x / self.scale * factor, p.y / self.scale * factor), z: at_zoom as f64 }
    }

    /// `screenCoordinateToLatLng(const ScreenCoordinate&, LatLng::WrapMode = LatLng::Unwrapped)`.
    /// `Err`: the std::domain_error of the C++ LatLng constructor (a NaN or
    /// infinite point).
    pub fn screen_coordinate_to_lat_lng(
        &self,
        point: &ScreenCoordinate,
        wrap_mode: WrapMode,
    ) -> Result<LatLng, String> {
        let coord = self.screen_coordinate_to_tile_coordinate(point, 0);
        Projection::unproject(&coord.p, 1.0 / TILE_SIZE_D, wrap_mode)
    }

    fn coordinate_point_matrix(&self, proj_matrix: &Mat4) -> Mat4 {
        let mut proj = *proj_matrix;
        let a = proj;
        matrix::scale(&mut proj, &a, TILE_SIZE_D, TILE_SIZE_D, 1.0);
        let b = proj;
        matrix::multiply(&mut proj, &self.get_pixel_matrix(), &b);
        proj
    }

    fn get_pixel_matrix(&self) -> Mat4 {
        let mut m: Mat4 = [0.0; 16];
        matrix::identity(&mut m);
        let a = m;
        matrix::scale(&mut m, &a, self.size.width as f64 / 2.0, -(self.size.height as f64) / 2.0, 1.0);
        let a = m;
        matrix::translate(&mut m, &a, 1.0, -1.0, 0.0);
        m
    }

    // MARK: - (private helper functions)

    fn rotated_north(&self) -> bool {
        self.orientation == NorthOrientation::Leftwards || self.orientation == NorthOrientation::Rightwards
    }

    /// `constrainScreen(double& scale_, double& lat, double& lon)`.
    pub fn constrain_screen(&self, scale: &mut f64, lat: &mut f64, lon: &mut f64) -> bool {
        if self.constrain_mode == ConstrainMode::Screen {
            let mut zoom = self.scale_zoom(*scale);
            let mut options = CameraOptions::default();
            self.constrain_camera_and_zoom_to_bounds(&mut options, &mut zoom);

            *scale = self.zoom_scale(zoom);

            if let Some(center) = options.center {
                *lat = center.latitude();
                *lon = center.longitude();

                return true;
            }
        }
        false
    }

    /// `constrain(double& scale, double& x, double& y)`.
    pub fn constrain(&self, scale: &mut f64, x: &mut f64, y: &mut f64) {
        if self.constrain_mode == ConstrainMode::None || self.constrain_mode == ConstrainMode::Screen {
            return;
        }

        let width = self.size.width as f64;
        let height = self.size.height as f64;

        // Constrain scale to avoid zooming out far enough to show off-world
        // areas on the Y axis.
        let ratio_y = (if self.rotated_north() { width } else { height }) / TILE_SIZE_D;
        // util::max: fmax
        *scale = mln_math::max_f64(*scale, ratio_y);

        // Constrain min/max pan to avoid showing off-world areas on the Y axis.
        let max_y = (*scale * TILE_SIZE_D - (if self.rotated_north() { width } else { height })) / 2.0;
        *y = std_max(-max_y, std_min(*y, max_y));

        if self.constrain_mode == ConstrainMode::WidthAndHeight {
            // Constrain min/max pan to avoid showing off-world areas on the X axis.
            let max_x = (*scale * TILE_SIZE_D - (if self.rotated_north() { height } else { width })) / 2.0;
            *x = std_max(-max_x, std_min(*x, max_x));
        }
    }

    pub fn constrain_camera_and_zoom_to_bounds(&self, requested_camera: &mut CameraOptions, requested_zoom: &mut f64) {
        if self.constrain_mode != ConstrainMode::Screen || self.get_lat_lng_bounds() == LatLngBounds::default() {
            return;
        }

        let mut center_lat_lng = self.get_lat_lng(WrapMode::Unwrapped);

        if let Some(center) = requested_camera.center {
            center_lat_lng = center;
        }

        let mut anchor_offset = Point::new(0.0, 0.0);
        let requested_scale = self.zoom_scale(*requested_zoom);

        // Since the transition calculations will include any specified anchor
        // in the result we need to do the same when testing if the requested
        // center and zoom is outside the bounds or not.
        if let Some(mut anchor) = requested_camera.anchor {
            anchor.y = self.get_size().height as f64 - anchor.y;
            // The screenCoordinateToLatLng function requires the matrices
            // inside the state to reflect the requested scale. So we create a
            // copy and set the requested zoom before the conversion. This will
            // give us the same result as the transition calculations.
            let mut state = self.clone();
            state.set_lat_lng_zoom(&self.get_lat_lng(WrapMode::Unwrapped), self.scale_zoom(requested_scale));
            // C++ throws std::domain_error out of the camera change for an
            // anchor that has no LatLng; the port ignores the anchor, as
            // Transform::startTransition does.
            if let (Ok(anchor_lat_lng), Ok(screen_lat_lng)) = (
                self.screen_coordinate_to_lat_lng(&anchor, WrapMode::Unwrapped),
                state.screen_coordinate_to_lat_lng(&anchor, WrapMode::Unwrapped),
            ) {
                let lat_lng_coord = Projection::project(&anchor_lat_lng, requested_scale);
                let anchor_coord = Projection::project(&screen_lat_lng, requested_scale);
                anchor_offset = Point::new(lat_lng_coord.x - anchor_coord.x, lat_lng_coord.y - anchor_coord.y);
            }
        }

        let current_bounds = self.get_lat_lng_bounds();
        let ne_bounds = Projection::project(&current_bounds.northeast(), requested_scale);
        let sw_bounds = Projection::project(&current_bounds.southwest(), requested_scale);
        let center = Projection::project(&center_lat_lng, requested_scale);
        let current_center = Projection::project(&self.get_lat_lng(WrapMode::Unwrapped), requested_scale);

        let min_y = ne_bounds.y;
        let max_y = sw_bounds.y;
        let min_x = sw_bounds.x;
        let max_x = ne_bounds.x;

        let start_x = center.x;
        let start_y = center.y;

        let mut result_x = start_x;
        let mut result_y = start_y;

        let screen_width = self.get_size().width;
        let screen_height = self.get_size().height;

        let h2 = screen_height as f64 / 2.0;
        if start_y - h2 + anchor_offset.y < min_y {
            result_y = min_y + h2;
        }
        if start_y + anchor_offset.y + h2 > max_y {
            result_y = max_y - h2;
        }

        let w2 = screen_width as f64 / 2.0;
        if start_x + anchor_offset.x - w2 < min_x {
            result_x = min_x + w2;
        }
        if start_x + anchor_offset.x + w2 > max_x {
            result_x = max_x - w2;
        }

        let mut scale_y = 0.0;
        if max_y - min_y < screen_height as f64 {
            scale_y = screen_height as f64 / (max_y - min_y);
            result_y = (max_y + min_y) / 2.0;
        }

        let mut scale_x = 0.0;
        if max_x - min_x < screen_width as f64 {
            scale_x = screen_width as f64 / (max_x - min_x);
            result_x = (max_x + min_x) / 2.0;
        }

        let max_scale = if scale_x > scale_y { scale_x } else { scale_y };

        // Max scale will be 1 when the screen is exactly the same size as the
        // max bounds in either the X or Y direction. To avoid numerical
        // instabilities we add small amount to the check to make sure we
        // don't try to scale when we don't actually need it.
        if max_scale > 1.000001 {
            *requested_zoom += self.scale_zoom(max_scale);

            if scale_y > scale_x {
                // If we scaled the y direction we want the resulting x
                // position to be the same as the current x position.
                result_x = current_center.x;
            } else {
                // If we scaled the x direction we want the resulting y
                // position to be the same as the current y position.
                result_y = current_center.y;
            }

            // Since we changed the scale, we might display something outside
            // the bounds. When checking we need to take into consideration
            // that we just changed the scale, since the resultX and minX were
            // calculated with the requested scale, and not the scale we just
            // calculated to make sure we stay inside the bounds.
            if result_x * max_scale - w2 <= min_x * max_scale {
                result_x = min_x * max_scale + w2;
                result_x /= max_scale;
            } else if result_x * max_scale + w2 >= max_x * max_scale {
                result_x = max_x * max_scale - w2;
                result_x /= max_scale;
            }

            if result_y * max_scale - h2 <= min_y * max_scale {
                result_y = min_y * max_scale + h2;
                result_y /= max_scale;
            } else if result_y * max_scale + h2 >= max_y * max_scale {
                result_y = max_y * max_scale - h2;
                result_y /= max_scale;
            }
        }

        if result_x != start_x || result_y != start_y {
            // If we made changes just drop any anchor point
            requested_camera.anchor = None;
            // The clamped point is inside the bounds, so it has a LatLng.
            requested_camera.center =
                Projection::unproject(&Point::new(result_x, result_y), requested_scale, WrapMode::Unwrapped).ok();
        }
    }

    /// Viewport center offset, from [size.width / 2, size.height / 2],
    /// defined by |edgeInsets| in screen coordinates, with top left origin.
    fn get_center_offset(&self) -> ScreenCoordinate {
        ScreenCoordinate::new(
            0.5 * (self.edge_insets.left - self.edge_insets.right),
            0.5 * (self.edge_insets.top - self.edge_insets.bottom),
        )
    }

    /// Recenter the map so that the given coordinate is located at the given
    /// point on screen.
    /// `Err`: the std::domain_error of the C++ LatLng constructor (a NaN or
    /// infinite anchor).
    pub fn move_lat_lng(&mut self, lat_lng: &LatLng, anchor: &ScreenCoordinate) -> Result<(), String> {
        let center_coord = Projection::project(&self.get_lat_lng(WrapMode::Unwrapped), self.scale);
        let lat_lng_coord = Projection::project(lat_lng, self.scale);
        let anchor_coord =
            Projection::project(&self.screen_coordinate_to_lat_lng(anchor, WrapMode::Unwrapped)?, self.scale);
        let center = Projection::unproject(
            &Point::new(
                center_coord.x + lat_lng_coord.x - anchor_coord.x,
                center_coord.y + lat_lng_coord.y - anchor_coord.y,
            ),
            self.scale,
            WrapMode::Unwrapped,
        )?;
        // Wrapped, as easeTo and GL JS's setLocationAtPoint leave it, unless
        // bounds keep it in their range.
        let target = if self.bounds == LatLngBounds::default() { center.wrapped() } else { center };
        let zoom = self.get_zoom();
        self.set_lat_lng_zoom(&target, zoom);
        Ok(())
    }

    pub fn set_lat_lng_zoom(&mut self, lat_lng: &LatLng, zoom: f64) {
        let constrained = self.bounds.constrain(lat_lng);

        let new_scale = mln_math::clamp_f64(self.zoom_scale(zoom), self.min_scale, self.max_scale);
        let new_world_size = new_scale * TILE_SIZE_D;
        self.bc = new_world_size / constants::DEGREES_MAX;
        self.cc = new_world_size / M2PI;

        let m = 1.0 - 1e-15;
        let f = mln_math::clamp_f64(mln_math::deg2rad(constrained.latitude()).sin(), -m, m);

        let point =
            ScreenCoordinate::new(-constrained.longitude() * self.bc, 0.5 * self.cc * ((1.0 + f) / (1.0 - f)).ln());
        self.set_scale_point(new_scale, &point);
    }

    pub fn set_center_altitude(&mut self, alt_m: f64) {
        self.z = alt_m
            / Projection::get_meters_per_pixel_at_latitude(
                self.get_lat_lng(WrapMode::Unwrapped).latitude(),
                self.get_zoom(),
            );
        self.request_matrices_update.set(true);
    }

    fn set_scale_point(&mut self, new_scale: f64, point: &ScreenCoordinate) {
        let mut constrained_scale = new_scale;
        let mut constrained_point = *point;
        self.constrain(&mut constrained_scale, &mut constrained_point.x, &mut constrained_point.y);

        self.scale = constrained_scale;
        self.x = constrained_point.x;
        self.y = constrained_point.y;
        self.bc = Projection::world_size(self.scale) / constants::DEGREES_MAX;
        self.cc = Projection::world_size(self.scale) / M2PI;
        self.request_matrices_update.set(true);
    }

    pub fn get_camera_to_tile_distance(&self, tile_id: &UnwrappedTileID) -> f32 {
        let mut tile_projection_matrix: Mat4 = [0.0; 16];
        self.matrix_for(&mut tile_projection_matrix, tile_id);
        let b = tile_projection_matrix;
        matrix::multiply(&mut tile_projection_matrix, &self.get_projection_matrix(), &b);
        let tile_center: Vec4 = [TILE_SIZE_D / 2.0, TILE_SIZE_D / 2.0, 0.0, 1.0];
        let mut projected_center: Vec4 = [0.0; 4];
        matrix::transform_mat4(&mut projected_center, &tile_center, &tile_projection_matrix);
        projected_center[3] as f32
    }

    pub fn max_pitch_scale_factor(&self) -> f32 {
        if self.size.is_empty() {
            return 0.0;
        }
        // C++ builds the point from `static_cast<float>(height)`. A point on
        // the screen has a LatLng unless the state holds NaN, where C++
        // throws; the port renders without the pitch factor.
        let Ok(lat_lng) = self.screen_coordinate_to_lat_lng(
            &ScreenCoordinate::new(0.0, self.get_size().height as f32 as f64),
            WrapMode::Unwrapped,
        ) else {
            return 0.0;
        };

        let pt = Projection::project(&lat_lng, self.scale);
        let p: Vec4 = [pt.x / TILE_SIZE_D, pt.y / TILE_SIZE_D, 0.0, 1.0];
        let mut top_point: Vec4 = [0.0; 4];
        matrix::transform_mat4(&mut top_point, &p, &self.get_coord_matrix());
        top_point[3] as f32 / self.get_camera_to_center_distance()
    }
}
