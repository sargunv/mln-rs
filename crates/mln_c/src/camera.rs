//! `camera.h`: the camera's commands and queries, the map's viewport, tile and
//! debug settings, and the camera that shows one tile.

mod momentum;

use core::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ::mln_map::{
    AnimationOptions, BoundOptions, Camera, CameraDelta, CameraOptions, CameraUpdate, CameraUpdateMode, ConstrainMode,
    FreeCameraOptions, GesturePhase, MapDebugOptions, NorthOrientation, ProjectionMode, TileLodMode, TransitionEnd,
    TransitionOutcome, ViewportMode,
};
use mln_runtime::{CameraFitArea, CameraFitOptions, CameraFitUpdate, TileOptions, ViewportOptions};
use mln_util::UnitBezier;
use mln_util::chrono::Duration;
use mln_util::constants::{MAX_ZOOM, MIN_ZOOM, TILE_SIZE_D, TILE_SIZE_I};
use mln_util::geo::{EdgeInsets, LatLngBounds, ScreenCoordinate, WrapMode};
use mln_util::geometry::for_each_point;
use mln_util::projection::Projection;

use crate::abi::{self, Fields};
use crate::callback::Registration;
use crate::completion::Plain;
use crate::error::Error;
use crate::generated::types::{
    MLN_ANIMATION_OPTION_DURATION, MLN_ANIMATION_OPTION_EASING, MLN_ANIMATION_OPTION_MIN_ZOOM,
    MLN_ANIMATION_OPTION_TRANSITION_ID, MLN_ANIMATION_OPTION_VELOCITY, MLN_BOUND_OPTION_BOUNDS,
    MLN_BOUND_OPTION_MAX_PITCH, MLN_BOUND_OPTION_MAX_ZOOM, MLN_BOUND_OPTION_MIN_PITCH, MLN_BOUND_OPTION_MIN_ZOOM,
    MLN_BOUND_OPTION_UNBOUNDED, MLN_CAMERA_DELTA_ANCHOR, MLN_CAMERA_DELTA_BEARING, MLN_CAMERA_DELTA_MOMENTUM,
    MLN_CAMERA_DELTA_OFFSET, MLN_CAMERA_DELTA_PITCH, MLN_CAMERA_DELTA_SCALE, MLN_CAMERA_FIT_OPTION_BEARING,
    MLN_CAMERA_FIT_OPTION_PADDING, MLN_CAMERA_FIT_OPTION_PITCH, MLN_CAMERA_OPTION_ANCHOR, MLN_CAMERA_OPTION_BEARING,
    MLN_CAMERA_OPTION_CENTER, MLN_CAMERA_OPTION_CENTER_ALTITUDE, MLN_CAMERA_OPTION_FOV, MLN_CAMERA_OPTION_PADDING,
    MLN_CAMERA_OPTION_PITCH, MLN_CAMERA_OPTION_ROLL, MLN_CAMERA_OPTION_ZOOM, MLN_CAMERA_TRANSITION_OUTCOME_CANCELLED,
    MLN_CAMERA_TRANSITION_OUTCOME_CLOSED, MLN_CAMERA_TRANSITION_OUTCOME_COMPLETED,
    MLN_CAMERA_TRANSITION_OUTCOME_REPLACED, MLN_CAMERA_UPDATE_MODE_EASE, MLN_CAMERA_UPDATE_MODE_FLY,
    MLN_CAMERA_UPDATE_MODE_JUMP, MLN_CONSTRAIN_MODE_HEIGHT_ONLY, MLN_CONSTRAIN_MODE_NONE, MLN_CONSTRAIN_MODE_SCREEN,
    MLN_CONSTRAIN_MODE_WIDTH_AND_HEIGHT, MLN_FREE_CAMERA_OPTION_ORIENTATION, MLN_FREE_CAMERA_OPTION_POSITION,
    MLN_GESTURE_PHASE_BEGIN, MLN_GESTURE_PHASE_CANCEL, MLN_GESTURE_PHASE_END, MLN_GESTURE_PHASE_NONE,
    MLN_GESTURE_PHASE_UPDATE, MLN_MAP_DEBUG_COLLISION, MLN_MAP_DEBUG_DEPTH_BUFFER, MLN_MAP_DEBUG_OVERDRAW,
    MLN_MAP_DEBUG_PARSE_STATUS, MLN_MAP_DEBUG_STENCIL_CLIP, MLN_MAP_DEBUG_TILE_BORDERS, MLN_MAP_DEBUG_TIMESTAMPS,
    MLN_MAP_TILE_OPTION_LOD_MIN_RADIUS, MLN_MAP_TILE_OPTION_LOD_MODE, MLN_MAP_TILE_OPTION_LOD_PITCH_THRESHOLD,
    MLN_MAP_TILE_OPTION_LOD_SCALE, MLN_MAP_TILE_OPTION_LOD_ZOOM_SHIFT, MLN_MAP_TILE_OPTION_PREFETCH_ZOOM_DELTA,
    MLN_MAP_VIEWPORT_OPTION_CONSTRAIN_MODE, MLN_MAP_VIEWPORT_OPTION_FRUSTUM_OFFSET,
    MLN_MAP_VIEWPORT_OPTION_NORTH_ORIENTATION, MLN_MAP_VIEWPORT_OPTION_VIEWPORT_MODE, MLN_NORTH_ORIENTATION_DOWN,
    MLN_NORTH_ORIENTATION_LEFT, MLN_NORTH_ORIENTATION_RIGHT, MLN_NORTH_ORIENTATION_UP,
    MLN_PROJECTION_MODE_OPTION_AXONOMETRIC, MLN_PROJECTION_MODE_OPTION_X_SKEW, MLN_PROJECTION_MODE_OPTION_Y_SKEW,
    MLN_TILE_LOD_MODE_DEFAULT, MLN_TILE_LOD_MODE_DISTANCE, MLN_VIEWPORT_MODE_DEFAULT, MLN_VIEWPORT_MODE_FLIPPED_Y,
    mln_animation_options, mln_bound_options, mln_buffer_view, mln_camera, mln_camera_delta, mln_camera_fit_options,
    mln_camera_fit_update, mln_camera_options, mln_camera_query_result, mln_camera_transition_end,
    mln_camera_transition_handler, mln_camera_update, mln_camera_velocity, mln_canonical_tile_id, mln_completion,
    mln_diagnostic, mln_edge_insets, mln_free_camera_options, mln_lat_lng, mln_lat_lng_bounds, mln_logical_size,
    mln_map, mln_map_debug_flags, mln_map_tile_options, mln_map_viewport_options, mln_projection_mode_options,
    mln_screen_point, mln_status,
};
use crate::map::{command, query};
use crate::{buffer, geo, handles, lock};

abi::versioned!(
    mln_bound_options,
    mln_camera_delta,
    mln_camera_fit_options,
    mln_camera_fit_update,
    mln_camera_options,
    mln_camera_update,
    mln_free_camera_options,
    mln_map_tile_options,
    mln_map_viewport_options,
    mln_projection_mode_options,
);

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

/// Camera options with the members that `camera` sets.
fn from_camera_options(camera: &CameraOptions) -> mln_camera_options {
    let presence = [
        (MLN_CAMERA_OPTION_CENTER, camera.center.is_some()),
        (MLN_CAMERA_OPTION_CENTER_ALTITUDE, camera.center_altitude.is_some()),
        (MLN_CAMERA_OPTION_PADDING, camera.padding.is_some()),
        (MLN_CAMERA_OPTION_ANCHOR, camera.anchor.is_some()),
        (MLN_CAMERA_OPTION_ZOOM, camera.zoom.is_some()),
        (MLN_CAMERA_OPTION_BEARING, camera.bearing.is_some()),
        (MLN_CAMERA_OPTION_PITCH, camera.pitch.is_some()),
        (MLN_CAMERA_OPTION_ROLL, camera.roll.is_some()),
        (MLN_CAMERA_OPTION_FOV, camera.fov.is_some()),
    ];
    mln_camera_options {
        size: size_of::<mln_camera_options>() as u32,
        fields: presence.iter().filter(|(_, set)| *set).fold(0, |fields, (bit, _)| fields | bit),
        center: camera.center.map_or(mln_lat_lng { latitude: 0.0, longitude: 0.0 }, geo::from_lat_lng),
        center_altitude: camera.center_altitude.unwrap_or_default(),
        padding: geo::from_edge_insets(camera.padding.unwrap_or_default()),
        anchor: geo::from_screen_point(camera.anchor.unwrap_or_default()),
        zoom: camera.zoom.unwrap_or_default(),
        bearing: camera.bearing.unwrap_or_default(),
        pitch: camera.pitch.unwrap_or_default(),
        roll: camera.roll.unwrap_or_default(),
        field_of_view: camera.fov.unwrap_or_default(),
    }
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

/// Copies a caller's struct as [`abi::read_sized`] does, failing with
/// `message` when it is null or smaller than the headers' struct.
///
/// # Safety
///
/// As for [`abi::read_sized`].
unsafe fn read_struct<T: abi::Versioned>(pointer: *const T, message: &'static str) -> Result<T, Error> {
    // SAFETY: the caller's contract.
    unsafe { abi::read_sized(pointer) }.map_err(|_| Error::invalid_argument(message))
}

// MARK: - Animations

/// Whether `milliseconds` converts to a [`Duration`]: finite, not negative,
/// and below its largest count, which the nearest double rounds up past.
fn is_duration_ms(milliseconds: f64) -> bool {
    milliseconds.is_finite() && milliseconds >= 0.0 && milliseconds < Duration::max().count() as f64 / 1e6
}

/// `milliseconds`, which [`is_duration_ms`] accepts, in whole nanoseconds.
fn duration(milliseconds: f64) -> Duration {
    Duration::from_nanos((milliseconds * 1e6) as i64)
}

type EndCallback = unsafe extern "C" fn(*mut c_void, *const mln_camera_transition_end);

/// A camera command's `mln_camera_transition_handler`, which receives the end
/// of the command's camera change once and then releases its user data.
///
/// The command accepts it as it sends, and gives it back to the caller when
/// the send fails. An accepted handler whose command drops without ending
/// its change, as when its map is gone, reports the change as cancelled.
struct EndHandler {
    callback: EndCallback,
    registration: Mutex<Option<Registration>>,
    accepted: AtomicBool,
}

impl EndHandler {
    /// Checks a caller's handler: none when it has neither a callback nor a
    /// queue.
    fn new(handler: &mln_camera_transition_handler) -> Result<Option<Arc<Self>>, Error> {
        let Some(callback) = handler.callback else {
            if handler.queue != handles::NULL {
                return Err(Error::invalid_argument("camera transition handler requires a callback"));
            }
            if handler.release_user_data.is_some() {
                return Err(Error::invalid_argument("a disabled camera transition handler must not retain user data"));
            }
            return Ok(None);
        };
        let registration = Registration::new(
            "camera transition handler",
            true,
            handler.user_data,
            handler.release_user_data,
            handler.queue,
        )?;
        Ok(Some(Arc::new(Self {
            callback,
            registration: Mutex::new(Some(registration)),
            accepted: AtomicBool::new(false),
        })))
    }

    fn end_fn(self: &Arc<Self>) -> Arc<dyn Fn(&TransitionEnd) + Send + Sync> {
        let handler = Arc::clone(self);
        Arc::new(move |end: &TransitionEnd| handler.end(end))
    }

    fn set_accepted(&self, accepted: bool) {
        self.accepted.store(accepted, Ordering::Release);
        if let Some(registration) = &*lock(&self.registration) {
            if accepted {
                registration.accept();
            } else {
                registration.revoke();
            }
        }
    }

    fn end(&self, end: &TransitionEnd) {
        let Some(registration) = lock(&self.registration).take() else { return };
        let callback = self.callback;
        let end = from_transition_end(end);
        // SAFETY: the host's end callback with its user data, and an end that
        // stays valid until it returns.
        registration.deliver(move |user_data| unsafe { callback(user_data.get(), &end) });
        // The accepted registration drops here, which releases the user data
        // after the call.
    }
}

impl Drop for EndHandler {
    fn drop(&mut self) {
        if *self.accepted.get_mut() {
            self.end(&TransitionEnd { outcome: TransitionOutcome::Cancelled, ..TransitionEnd::default() });
        }
    }
}

fn from_transition_end(end: &TransitionEnd) -> mln_camera_transition_end {
    let velocity = end.velocity;
    mln_camera_transition_end {
        size: size_of::<mln_camera_transition_end>() as u32,
        outcome: match end.outcome {
            TransitionOutcome::Completed => MLN_CAMERA_TRANSITION_OUTCOME_COMPLETED,
            TransitionOutcome::Cancelled => MLN_CAMERA_TRANSITION_OUTCOME_CANCELLED,
            TransitionOutcome::Replaced => MLN_CAMERA_TRANSITION_OUTCOME_REPLACED,
            TransitionOutcome::Closed => MLN_CAMERA_TRANSITION_OUTCOME_CLOSED,
        },
        generation: end.generation,
        velocity: mln_camera_velocity {
            pan: geo::from_screen_point(velocity.pan),
            zoom: velocity.zoom,
            bearing: velocity.bearing,
            pitch: velocity.pitch,
        },
    }
}

/// A caller's animation options, checked, and the end handler that they
/// register.
struct Animation {
    options: AnimationOptions,
    handler: Option<Arc<EndHandler>>,
}

impl Animation {
    fn new(animation: &mln_animation_options) -> Result<Self, Error> {
        let fields = Fields::new(
            animation.fields,
            MLN_ANIMATION_OPTION_DURATION
                | MLN_ANIMATION_OPTION_VELOCITY
                | MLN_ANIMATION_OPTION_MIN_ZOOM
                | MLN_ANIMATION_OPTION_EASING
                | MLN_ANIMATION_OPTION_TRANSITION_ID,
            "mln_animation_options.fields",
        )?;
        let duration_ms = fields.get(MLN_ANIMATION_OPTION_DURATION, animation.duration_ms);
        if duration_ms.is_some_and(|duration_ms| !is_duration_ms(duration_ms)) {
            return Err(Error::invalid_argument("animation duration_ms must fit the native duration range"));
        }
        let velocity = fields.get(MLN_ANIMATION_OPTION_VELOCITY, animation.velocity);
        if velocity.is_some_and(|velocity| !velocity.is_finite() || velocity <= 0.0) {
            return Err(Error::invalid_argument("animation velocity must be positive and finite"));
        }
        let min_zoom = fields.get(MLN_ANIMATION_OPTION_MIN_ZOOM, animation.min_zoom);
        if min_zoom.is_some_and(|min_zoom| !min_zoom.is_finite()) {
            return Err(Error::invalid_argument("animation min_zoom must be finite"));
        }
        let easing = fields.get(MLN_ANIMATION_OPTION_EASING, animation.easing);
        if let Some(easing) = easing
            && (![easing.x1, easing.y1, easing.x2, easing.y2].iter().all(|value| value.is_finite())
                || !(0.0..=1.0).contains(&easing.x1)
                || !(0.0..=1.0).contains(&easing.x2))
        {
            return Err(Error::invalid_argument(
                "animation easing x values must be within [0, 1] and all easing values must be finite",
            ));
        }
        let handler = EndHandler::new(&animation.end_handler)?;
        Ok(Self {
            options: AnimationOptions {
                duration: duration_ms.map(duration),
                velocity,
                min_zoom,
                easing: easing.map(|easing| UnitBezier::new(easing.x1, easing.y1, easing.x2, easing.y2)),
                transition_end_fn: handler.as_ref().map(EndHandler::end_fn),
                transition_id: fields.get(MLN_ANIMATION_OPTION_TRANSITION_ID, animation.transition_id),
                ..AnimationOptions::default()
            },
            handler,
        })
    }
}

/// Sends a camera command, which takes `handler`'s user data from the caller
/// once it is on its way. A handler that ends before `send` returns, on the
/// map's thread, finds it accepted.
fn send_with<T>(
    handler: Option<Arc<EndHandler>>,
    send: impl FnOnce() -> Result<T, mln_util::Error>,
) -> Result<T, mln_util::Error> {
    if let Some(handler) = &handler {
        handler.set_accepted(true);
    }
    // The command holds the handler too, and drops it when the send fails:
    // this one keeps it until it is given back.
    let sent = send();
    if let (Err(_), Some(handler)) = (&sent, &handler) {
        handler.set_accepted(false);
    }
    sent
}

fn update_mode(mode: u32) -> Option<CameraUpdateMode> {
    match mode {
        MLN_CAMERA_UPDATE_MODE_JUMP => Some(CameraUpdateMode::Jump),
        MLN_CAMERA_UPDATE_MODE_EASE => Some(CameraUpdateMode::Ease),
        MLN_CAMERA_UPDATE_MODE_FLY => Some(CameraUpdateMode::Fly),
        _ => None,
    }
}

fn gesture_phase(phase: u32) -> Option<GesturePhase> {
    match phase {
        MLN_GESTURE_PHASE_NONE => Some(GesturePhase::None),
        MLN_GESTURE_PHASE_BEGIN => Some(GesturePhase::Begin),
        MLN_GESTURE_PHASE_UPDATE => Some(GesturePhase::Update),
        MLN_GESTURE_PHASE_END => Some(GesturePhase::End),
        MLN_GESTURE_PHASE_CANCEL => Some(GesturePhase::Cancel),
        _ => None,
    }
}

// MARK: - Settings

const KNOWN_DEBUG_FLAGS: u32 = MLN_MAP_DEBUG_TILE_BORDERS
    | MLN_MAP_DEBUG_PARSE_STATUS
    | MLN_MAP_DEBUG_TIMESTAMPS
    | MLN_MAP_DEBUG_COLLISION
    | MLN_MAP_DEBUG_OVERDRAW
    | MLN_MAP_DEBUG_STENCIL_CLIP
    | MLN_MAP_DEBUG_DEPTH_BUFFER;

/// `mln_map_set_debug_flags()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_set_debug_flags(
    map: mln_map,
    flags: mln_map_debug_flags,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        if flags & !KNOWN_DEBUG_FLAGS != 0 {
            return Err(Error::invalid_argument("debug flags contain unknown bits"));
        }
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| map.set_debug_flags(MapDebugOptions(flags))) }
    })
}

/// `mln_map_set_rendering_stats_view_enabled()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_set_rendering_stats_view_enabled(
    map: mln_map,
    enabled: bool,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| map.set_rendering_stats_view_enabled(enabled)) }
    })
}

/// `mln_map_dump_debug_logs()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_dump_debug_logs(
    map: mln_map,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, ::mln_runtime::Map::dump_debug_logs) }
    })
}

/// A frustum offset with members that are finite and at least 0.
fn frustum_offset(offset: mln_edge_insets) -> Result<EdgeInsets, Error> {
    let mln_edge_insets { top, left, bottom, right } = offset;
    let members = [top, left, bottom, right];
    if !members.iter().all(|value| value.is_finite()) {
        return Err(Error::invalid_argument("frustum offset values must be finite"));
    }
    if members.iter().any(|value| *value < 0.0) {
        return Err(Error::invalid_argument("frustum offset values must be greater than or equal to 0"));
    }
    Ok(EdgeInsets { top, left, bottom, right })
}

fn viewport_options(options: &mln_map_viewport_options) -> Result<ViewportOptions, Error> {
    let fields = Fields::new(
        options.fields,
        MLN_MAP_VIEWPORT_OPTION_NORTH_ORIENTATION
            | MLN_MAP_VIEWPORT_OPTION_CONSTRAIN_MODE
            | MLN_MAP_VIEWPORT_OPTION_VIEWPORT_MODE
            | MLN_MAP_VIEWPORT_OPTION_FRUSTUM_OFFSET,
        "mln_map_viewport_options.fields",
    )?;
    let invalid = |name: &str| Error::invalid_argument(format!("{name} is invalid"));
    let north_orientation = match fields.get(MLN_MAP_VIEWPORT_OPTION_NORTH_ORIENTATION, options.north_orientation) {
        None => None,
        Some(MLN_NORTH_ORIENTATION_UP) => Some(NorthOrientation::Upwards),
        Some(MLN_NORTH_ORIENTATION_RIGHT) => Some(NorthOrientation::Rightwards),
        Some(MLN_NORTH_ORIENTATION_DOWN) => Some(NorthOrientation::Downwards),
        Some(MLN_NORTH_ORIENTATION_LEFT) => Some(NorthOrientation::Leftwards),
        Some(_) => return Err(invalid("north_orientation")),
    };
    let constrain_mode = match fields.get(MLN_MAP_VIEWPORT_OPTION_CONSTRAIN_MODE, options.constrain_mode) {
        None => None,
        Some(MLN_CONSTRAIN_MODE_NONE) => Some(ConstrainMode::None),
        Some(MLN_CONSTRAIN_MODE_HEIGHT_ONLY) => Some(ConstrainMode::HeightOnly),
        Some(MLN_CONSTRAIN_MODE_WIDTH_AND_HEIGHT) => Some(ConstrainMode::WidthAndHeight),
        Some(MLN_CONSTRAIN_MODE_SCREEN) => Some(ConstrainMode::Screen),
        Some(_) => return Err(invalid("constrain_mode")),
    };
    let viewport_mode = match fields.get(MLN_MAP_VIEWPORT_OPTION_VIEWPORT_MODE, options.viewport_mode) {
        None => None,
        Some(MLN_VIEWPORT_MODE_DEFAULT) => Some(ViewportMode::Default),
        Some(MLN_VIEWPORT_MODE_FLIPPED_Y) => Some(ViewportMode::FlippedY),
        Some(_) => return Err(invalid("viewport_mode")),
    };
    let frustum_offset =
        fields.get(MLN_MAP_VIEWPORT_OPTION_FRUSTUM_OFFSET, options.frustum_offset).map(frustum_offset).transpose()?;
    Ok(ViewportOptions { north_orientation, constrain_mode, viewport_mode, frustum_offset })
}

/// `mln_map_set_viewport_options()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_set_viewport_options(
    map: mln_map,
    options: *const mln_map_viewport_options,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let options = viewport_options(&unsafe { abi::read_sized(options) }?)?;
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| map.set_viewport_options(options)) }
    })
}

fn tile_options(options: &mln_map_tile_options) -> Result<TileOptions, Error> {
    let fields = Fields::new(
        options.fields,
        MLN_MAP_TILE_OPTION_PREFETCH_ZOOM_DELTA
            | MLN_MAP_TILE_OPTION_LOD_MIN_RADIUS
            | MLN_MAP_TILE_OPTION_LOD_SCALE
            | MLN_MAP_TILE_OPTION_LOD_PITCH_THRESHOLD
            | MLN_MAP_TILE_OPTION_LOD_ZOOM_SHIFT
            | MLN_MAP_TILE_OPTION_LOD_MODE,
        "mln_map_tile_options.fields",
    )?;
    let prefetch_zoom_delta = fields
        .get(MLN_MAP_TILE_OPTION_PREFETCH_ZOOM_DELTA, options.prefetch_zoom_delta)
        .map(|delta| {
            u8::try_from(delta).map_err(|_| Error::invalid_argument("prefetch_zoom_delta must be at most 255"))
        })
        .transpose()?;
    let number = |bit, value: f64| match fields.get(bit, value) {
        Some(value) if !value.is_finite() => Err(Error::invalid_argument("tile LOD values must be finite")),
        value => Ok(value),
    };
    let lod_min_radius = number(MLN_MAP_TILE_OPTION_LOD_MIN_RADIUS, options.lod_min_radius)?;
    let lod_scale = number(MLN_MAP_TILE_OPTION_LOD_SCALE, options.lod_scale)?;
    let lod_pitch_threshold = number(MLN_MAP_TILE_OPTION_LOD_PITCH_THRESHOLD, options.lod_pitch_threshold)?;
    let lod_zoom_shift = number(MLN_MAP_TILE_OPTION_LOD_ZOOM_SHIFT, options.lod_zoom_shift)?;
    let lod_mode = match fields.get(MLN_MAP_TILE_OPTION_LOD_MODE, options.lod_mode) {
        None => None,
        Some(MLN_TILE_LOD_MODE_DEFAULT) => Some(TileLodMode::Default),
        Some(MLN_TILE_LOD_MODE_DISTANCE) => Some(TileLodMode::Distance),
        Some(_) => return Err(Error::invalid_argument("lod_mode is invalid")),
    };
    Ok(TileOptions { prefetch_zoom_delta, lod_min_radius, lod_scale, lod_pitch_threshold, lod_zoom_shift, lod_mode })
}

/// `mln_map_set_tile_options()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_set_tile_options(
    map: mln_map,
    options: *const mln_map_tile_options,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let options = tile_options(&unsafe { abi::read_sized(options) }?)?;
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| map.set_tile_options(options)) }
    })
}

// MARK: - Camera commands

/// `mln_map_update_camera()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_update_camera(
    map: mln_map,
    update: *const mln_camera_update,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let update = unsafe { read_struct(update, "camera update must have a valid size") }?;
        let (Some(mode), Some(phase)) = (update_mode(update.mode), gesture_phase(update.gesture_phase)) else {
            return Err(Error::invalid_argument("camera update mode or gesture phase is invalid"));
        };
        // The update versions the camera that it holds.
        let camera = camera_options(&update.camera)?;
        let Animation { options: animation, handler } = Animation::new(&update.animation)?;
        let update = CameraUpdate { mode, camera, animation, phase };
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| send_with(handler, || map.update_camera(update))) }
    })
}

const KNOWN_DELTA_FIELDS: u32 = MLN_CAMERA_DELTA_OFFSET
    | MLN_CAMERA_DELTA_SCALE
    | MLN_CAMERA_DELTA_BEARING
    | MLN_CAMERA_DELTA_PITCH
    | MLN_CAMERA_DELTA_ANCHOR
    | MLN_CAMERA_DELTA_MOMENTUM;

/// `mln_map_apply_camera_delta()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_apply_camera_delta(
    map: mln_map,
    delta: *const mln_camera_delta,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let delta = unsafe { read_struct(delta, "camera delta must have a valid size") }?;
        let fields = Fields::new(delta.fields, KNOWN_DELTA_FIELDS, "mln_camera_delta.fields")?;
        let phase = gesture_phase(delta.gesture_phase)
            .ok_or_else(|| Error::invalid_argument("camera delta gesture phase is invalid"))?;
        let offset = fields.get(MLN_CAMERA_DELTA_OFFSET, delta.offset).map(geo::screen_point).transpose()?;
        let anchor = fields.get(MLN_CAMERA_DELTA_ANCHOR, delta.anchor);
        let momentum = fields.get(MLN_CAMERA_DELTA_MOMENTUM, &delta.momentum).map(momentum::momentum).transpose()?;
        let Animation { options: animation, handler } = Animation::new(&delta.animation)?;
        let delta = CameraDelta {
            offset,
            scale: fields.get(MLN_CAMERA_DELTA_SCALE, delta.scale),
            bearing: fields.get(MLN_CAMERA_DELTA_BEARING, delta.bearing),
            pitch: fields.get(MLN_CAMERA_DELTA_PITCH, delta.pitch),
            anchor: anchor.map(|anchor| ScreenCoordinate::new(anchor.x, anchor.y)),
            momentum,
            animation,
            phase,
        };
        delta.validate().map_err(|error| Error::invalid_argument(error.to_string()))?;
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| send_with(handler, || map.apply_camera_delta(delta))) }
    })
}

/// `mln_map_cancel_camera_transitions()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_cancel_camera_transitions(
    map: mln_map,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, ::mln_runtime::Map::cancel_camera_transitions) }
    })
}

/// `mln_map_cancel_camera_transitions_with_id()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_cancel_camera_transitions_with_id(
    map: mln_map,
    transition_id: u64,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| map.cancel_camera_transitions_with_id(transition_id)) }
    })
}

/// `mln_map_get_camera()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_get_camera(
    map: mln_map,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        unsafe {
            query(map, completion, ::mln_runtime::Map::camera, |queried| {
                Plain(mln_camera_query_result { generation: queried.generation, camera: from_camera(queried.camera) })
            })
        }
    })
}

/// `mln_map_lat_lng_bounds_for_camera()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_lat_lng_bounds_for_camera(
    map: mln_map,
    camera: *const mln_camera_options,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let camera = unsafe { read_camera_options(camera) }?;
        // SAFETY: the caller's contract.
        unsafe {
            query(
                map,
                completion,
                |map| map.lat_lng_bounds_for_camera(camera),
                |bounds| Plain(geo::from_lat_lng_bounds(bounds)),
            )
        }
    })
}

// MARK: - Fits

/// The members of fit options, whose size the struct that holds them
/// versions.
fn fit_fields(fit: &mln_camera_fit_options) -> Result<CameraFitOptions, Error> {
    let fields = Fields::new(
        fit.fields,
        MLN_CAMERA_FIT_OPTION_PADDING | MLN_CAMERA_FIT_OPTION_BEARING | MLN_CAMERA_FIT_OPTION_PITCH,
        "mln_camera_fit_options.fields",
    )?;
    let padding = fields
        .get(MLN_CAMERA_FIT_OPTION_PADDING, fit.padding)
        .map(|padding| geo::edge_insets(padding, "padding"))
        .transpose()?
        .unwrap_or_default();
    let bearing = fields.get(MLN_CAMERA_FIT_OPTION_BEARING, fit.bearing);
    let pitch = fields.get(MLN_CAMERA_FIT_OPTION_PITCH, fit.pitch);
    if !bearing.is_none_or(f64::is_finite) || !pitch.is_none_or(f64::is_finite) {
        return Err(Error::invalid_argument("camera fit bearing and pitch must be finite"));
    }
    Ok(CameraFitOptions { padding, bearing, pitch })
}

/// A caller's fit options, which may be null for the defaults.
///
/// # Safety
///
/// As for [`abi::read_sized`].
unsafe fn fit_options(options: *const mln_camera_fit_options) -> Result<CameraFitOptions, Error> {
    if options.is_null() {
        return Ok(CameraFitOptions::default());
    }
    // SAFETY: the caller's contract.
    fit_fields(&unsafe { abi::read_sized(options) }?)
}

fn bounds_area(bounds: mln_lat_lng_bounds) -> Result<CameraFitArea, Error> {
    geo::lat_lng_bounds(bounds).map(CameraFitArea::Bounds)
}

/// # Safety
///
/// As for [`geo::lat_lngs`].
unsafe fn coordinates_area(coordinates: *const mln_lat_lng, count: usize) -> Result<CameraFitArea, Error> {
    if count == 0 {
        return Err(Error::invalid_argument("coordinate_count must be greater than 0"));
    }
    // SAFETY: the caller's contract.
    unsafe { geo::lat_lngs(coordinates, count) }.map(CameraFitArea::Coordinates)
}

/// # Safety
///
/// As for [`buffer::text`].
unsafe fn geometry_area(geometry: mln_buffer_view) -> Result<CameraFitArea, Error> {
    // SAFETY: the caller's contract.
    let geometry = geo::geometry(unsafe { buffer::text(geometry, "geometry") }?, "geometry")?;
    let mut empty = true;
    for_each_point(&geometry, &mut |_| empty = false);
    if empty {
        return Err(Error::invalid_argument("geometry must contain at least one coordinate"));
    }
    Ok(CameraFitArea::Geometry(geometry))
}

/// Sends the query for the camera that fits `area`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
unsafe fn camera_for(
    map: mln_map,
    area: CameraFitArea,
    options: *const mln_camera_fit_options,
    completion: *const mln_completion,
) -> Result<(), Error> {
    // SAFETY: the caller's contract.
    let options = unsafe { fit_options(options) }?;
    // SAFETY: the caller's contract.
    unsafe { query(map, completion, |map| map.camera_for(area, options), |camera| Plain(from_camera_options(&camera))) }
}

/// `mln_map_camera_for_lat_lng_bounds()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_camera_for_lat_lng_bounds(
    map: mln_map,
    bounds: mln_lat_lng_bounds,
    fit_options: *const mln_camera_fit_options,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        let area = bounds_area(bounds)?;
        // SAFETY: the caller's contract.
        unsafe { camera_for(map, area, fit_options, completion) }
    })
}

/// `mln_map_camera_for_lat_lngs()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_camera_for_lat_lngs(
    map: mln_map,
    coordinates: *const mln_lat_lng,
    coordinate_count: usize,
    fit_options: *const mln_camera_fit_options,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let area = unsafe { coordinates_area(coordinates, coordinate_count) }?;
        // SAFETY: the caller's contract.
        unsafe { camera_for(map, area, fit_options, completion) }
    })
}

/// `mln_map_camera_for_geometry()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_camera_for_geometry(
    map: mln_map,
    geometry: mln_buffer_view,
    fit_options: *const mln_camera_fit_options,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let area = unsafe { geometry_area(geometry) }?;
        // SAFETY: the caller's contract.
        unsafe { camera_for(map, area, fit_options, completion) }
    })
}

/// Sends the command that moves the camera to fit `area`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
unsafe fn fit_camera(
    map: mln_map,
    area: CameraFitArea,
    update: *const mln_camera_fit_update,
    completion: *const mln_completion,
) -> Result<(), Error> {
    // SAFETY: the caller's contract.
    let update = unsafe { read_struct(update, "camera fit update must have a valid size") }?;
    let mode = update_mode(update.mode).ok_or_else(|| Error::invalid_argument("camera fit update mode is invalid"))?;
    // The update versions the fit options that it holds.
    let fit = fit_fields(&update.fit)?;
    let Animation { options: animation, handler } = Animation::new(&update.animation)?;
    let update = CameraFitUpdate { mode, fit, animation };
    // SAFETY: the caller's contract.
    unsafe { command(map, completion, |map| send_with(handler, || map.fit_camera(area, update))) }
}

/// `mln_map_fit_camera_to_lat_lng_bounds()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_fit_camera_to_lat_lng_bounds(
    map: mln_map,
    bounds: mln_lat_lng_bounds,
    update: *const mln_camera_fit_update,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        let area = bounds_area(bounds)?;
        // SAFETY: the caller's contract.
        unsafe { fit_camera(map, area, update, completion) }
    })
}

/// `mln_map_fit_camera_to_lat_lngs()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_fit_camera_to_lat_lngs(
    map: mln_map,
    coordinates: *const mln_lat_lng,
    coordinate_count: usize,
    update: *const mln_camera_fit_update,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let area = unsafe { coordinates_area(coordinates, coordinate_count) }?;
        // SAFETY: the caller's contract.
        unsafe { fit_camera(map, area, update, completion) }
    })
}

/// `mln_map_fit_camera_to_geometry()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_fit_camera_to_geometry(
    map: mln_map,
    geometry: mln_buffer_view,
    update: *const mln_camera_fit_update,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let area = unsafe { geometry_area(geometry) }?;
        // SAFETY: the caller's contract.
        unsafe { fit_camera(map, area, update, completion) }
    })
}

// MARK: - Limits and placement

fn bound_options(options: &mln_bound_options) -> Result<BoundOptions, Error> {
    let fields = Fields::new(
        options.fields,
        MLN_BOUND_OPTION_BOUNDS
            | MLN_BOUND_OPTION_MIN_ZOOM
            | MLN_BOUND_OPTION_MAX_ZOOM
            | MLN_BOUND_OPTION_MIN_PITCH
            | MLN_BOUND_OPTION_MAX_PITCH
            | MLN_BOUND_OPTION_UNBOUNDED,
        "mln_bound_options.fields",
    )?;
    let unbounded = fields.get(MLN_BOUND_OPTION_UNBOUNDED, LatLngBounds::infinite());
    let bounds = fields.get(MLN_BOUND_OPTION_BOUNDS, options.bounds);
    if bounds.is_some() && unbounded.is_some() {
        return Err(Error::invalid_argument(
            "MLN_BOUND_OPTION_BOUNDS and MLN_BOUND_OPTION_UNBOUNDED are mutually exclusive",
        ));
    }
    let bounds = bounds.map(geo::lat_lng_bounds).transpose()?.or(unbounded);
    let min_zoom = fields.get(MLN_BOUND_OPTION_MIN_ZOOM, options.min_zoom);
    let max_zoom = fields.get(MLN_BOUND_OPTION_MAX_ZOOM, options.max_zoom);
    let min_pitch = fields.get(MLN_BOUND_OPTION_MIN_PITCH, options.min_pitch);
    let max_pitch = fields.get(MLN_BOUND_OPTION_MAX_PITCH, options.max_pitch);
    if ![min_zoom, max_zoom, min_pitch, max_pitch].iter().all(|value| value.is_none_or(f64::is_finite)) {
        return Err(Error::invalid_argument("bound numeric fields must be finite"));
    }
    if let (Some(min), Some(max)) = (min_zoom, max_zoom)
        && min > max
    {
        return Err(Error::invalid_argument("min_zoom must be less than or equal to max_zoom"));
    }
    if let (Some(min), Some(max)) = (min_pitch, max_pitch)
        && min > max
    {
        return Err(Error::invalid_argument("min_pitch must be less than or equal to max_pitch"));
    }
    Ok(BoundOptions { bounds, max_zoom, min_zoom, max_pitch, min_pitch })
}

/// `mln_map_set_bounds()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_set_bounds(
    map: mln_map,
    options: *const mln_bound_options,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let options = bound_options(&unsafe { abi::read_sized(options) }?)?;
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| map.set_bounds(options)) }
    })
}

fn free_camera_options(options: &mln_free_camera_options) -> Result<FreeCameraOptions, Error> {
    let fields = Fields::new(
        options.fields,
        MLN_FREE_CAMERA_OPTION_POSITION | MLN_FREE_CAMERA_OPTION_ORIENTATION,
        "mln_free_camera_options.fields",
    )?;
    let position = fields
        .get(MLN_FREE_CAMERA_OPTION_POSITION, options.position)
        .map(|position| [position.x, position.y, position.z]);
    if position.is_some_and(|position| !position.iter().all(|value| value.is_finite())) {
        return Err(Error::invalid_argument("free camera position values must be finite"));
    }
    let orientation = fields
        .get(MLN_FREE_CAMERA_OPTION_ORIENTATION, options.orientation)
        .map(|orientation| [orientation.x, orientation.y, orientation.z, orientation.w]);
    if let Some(orientation) = orientation {
        if !orientation.iter().all(|value| value.is_finite()) {
            return Err(Error::invalid_argument("free camera orientation values must be finite"));
        }
        if orientation.iter().all(|value| *value == 0.0) {
            return Err(Error::invalid_argument("free camera orientation must not be zero length"));
        }
    }
    Ok(FreeCameraOptions { position, orientation })
}

/// `mln_map_set_free_camera_options()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_set_free_camera_options(
    map: mln_map,
    options: *const mln_free_camera_options,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let options = free_camera_options(&unsafe { abi::read_sized(options) }?)?;
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| map.set_free_camera_options(options)) }
    })
}

fn projection_mode(options: &mln_projection_mode_options) -> Result<ProjectionMode, Error> {
    let fields = Fields::new(
        options.fields,
        MLN_PROJECTION_MODE_OPTION_AXONOMETRIC | MLN_PROJECTION_MODE_OPTION_X_SKEW | MLN_PROJECTION_MODE_OPTION_Y_SKEW,
        "mln_projection_mode_options.fields",
    )?;
    let x_skew = fields.get(MLN_PROJECTION_MODE_OPTION_X_SKEW, options.x_skew);
    let y_skew = fields.get(MLN_PROJECTION_MODE_OPTION_Y_SKEW, options.y_skew);
    if !x_skew.is_none_or(f64::is_finite) || !y_skew.is_none_or(f64::is_finite) {
        return Err(Error::invalid_argument("projection skew values must be finite"));
    }
    Ok(ProjectionMode {
        axonometric: fields.get(MLN_PROJECTION_MODE_OPTION_AXONOMETRIC, options.axonometric),
        x_skew,
        y_skew,
    })
}

/// `mln_map_set_projection_mode()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_set_projection_mode(
    map: mln_map,
    options: *const mln_projection_mode_options,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let options = unsafe { read_struct(options, "options must not be null and must have a valid size") }?;
        let mode = projection_mode(&options)?;
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| map.set_projection_mode(mode)) }
    })
}

// MARK: - Tiles

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
    use crate::generated::types::{MLN_STATUS_INVALID_ARGUMENT, MLN_STATUS_OK};

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

    #[test]
    fn camera_options_round_trip_their_fields() {
        let sent = options(MLN_CAMERA_OPTION_CENTER | MLN_CAMERA_OPTION_ZOOM | MLN_CAMERA_OPTION_PADDING);
        let returned = from_camera_options(&camera_options(&sent).unwrap());
        assert_eq!(returned.fields, sent.fields);
        assert_eq!((returned.center.longitude, returned.zoom, returned.padding.top), (20.0, 3.0, 1.0));
        assert_eq!(from_camera_options(&CameraOptions::default()).fields, 0);
    }

    #[test]
    fn durations_stay_within_the_native_range() {
        assert!(is_duration_ms(0.0) && is_duration_ms(300.0));
        assert!(!is_duration_ms(-1.0) && !is_duration_ms(f64::NAN) && !is_duration_ms(1e300));
        assert!(!is_duration_ms(Duration::max().count() as f64 / 1e6));
        assert_eq!(duration(1.5), Duration::from_nanos(1_500_000));
    }

    fn animation(fields: u32) -> mln_animation_options {
        // SAFETY: zeros are valid animation options: no handler.
        let mut animation: mln_animation_options = unsafe { core::mem::zeroed() };
        animation.fields = fields;
        animation
    }

    #[test]
    fn animations_check_the_members_they_set() {
        let mut unread = animation(0);
        unread.duration_ms = f64::NAN;
        unread.velocity = -1.0;
        assert!(Animation::new(&unread).is_ok());
        for (fields, edit, expected) in [
            (
                MLN_ANIMATION_OPTION_DURATION,
                (|a: &mut mln_animation_options| a.duration_ms = 1e300) as fn(&mut _),
                "native duration range",
            ),
            (MLN_ANIMATION_OPTION_VELOCITY, |a| a.velocity = 0.0, "positive and finite"),
            (MLN_ANIMATION_OPTION_MIN_ZOOM, |a| a.min_zoom = f64::INFINITY, "min_zoom must be finite"),
            (MLN_ANIMATION_OPTION_EASING, |a| a.easing.x1 = 1.5, "within [0, 1]"),
            (1 << 31, |_| {}, "unknown bits"),
        ] {
            let mut options = animation(fields);
            edit(&mut options);
            let message = Animation::new(&options).err().unwrap().message().to_owned();
            assert!(message.contains(expected), "{message}");
        }
        let mut retaining = animation(0);
        retaining.end_handler.release_user_data = Some(crate::delivery::tests::Log::release);
        assert!(Animation::new(&retaining).err().unwrap().message().contains("must not retain user data"));
    }

    #[test]
    fn an_end_handler_ends_once_and_cancels_an_unended_command() {
        use crate::delivery::tests::Log;

        unsafe extern "C" fn record(user_data: *mut c_void, end: *const mln_camera_transition_end) {
            // SAFETY: the log's pointer, and the end that the handler lends.
            let outcome = unsafe { (*end).outcome };
            Log::record(crate::callback::UserData::new(user_data), char::from(b'0' + outcome as u8));
        }
        let log = Log::new();
        let handler = |token| mln_camera_transition_handler {
            callback: Some(record),
            user_data: log.pointer(token),
            release_user_data: Some(Log::release),
            queue: handles::NULL,
        };

        let ended = EndHandler::new(&handler('e')).unwrap().unwrap();
        let end_fn = ended.end_fn();
        ended.set_accepted(true);
        end_fn(&TransitionEnd { outcome: TransitionOutcome::Replaced, ..TransitionEnd::default() });
        end_fn(&TransitionEnd::default());
        drop((ended, end_fn));
        assert_eq!(log.take(), "e3er");

        let dropped = EndHandler::new(&handler('d')).unwrap().unwrap();
        dropped.set_accepted(true);
        drop(dropped);
        assert_eq!(log.take(), "d1dr");

        let refused = EndHandler::new(&handler('x')).unwrap().unwrap();
        refused.set_accepted(true);
        refused.set_accepted(false);
        drop(refused);
        assert_eq!(log.take(), "");
    }

    #[test]
    fn bounds_options_keep_unbounded_apart_from_bounds() {
        // SAFETY: zeros are valid bound options.
        let mut options: mln_bound_options = unsafe { core::mem::zeroed() };
        options.fields = MLN_BOUND_OPTION_UNBOUNDED;
        assert_eq!(bound_options(&options).unwrap().bounds, Some(LatLngBounds::infinite()));
        options.fields |= MLN_BOUND_OPTION_BOUNDS;
        assert!(bound_options(&options).unwrap_err().message().contains("mutually exclusive"));
        options.fields = MLN_BOUND_OPTION_MIN_PITCH | MLN_BOUND_OPTION_MAX_PITCH;
        options.min_pitch = 30.0;
        assert!(bound_options(&options).unwrap_err().message().starts_with("min_pitch"));
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
