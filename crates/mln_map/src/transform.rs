// Ports src/mln/map/transform.hpp and src/mln/map/transform.cpp, with
// maplibre-native-ffi's patches 0015 (independent camera animations,
// maplibre/maplibre-native#4637), 0043 (cancel transitions by id) and 0085
// (gesture momentum).
//
// The C++ transition lambdas capture `this`. Here a command's frame function
// takes the state as an argument and returns the camera at its progress,
// which `apply_transitions` composes with the other commands' cameras.
//
// The C++ guards against callbacks that re-enter the Transform (an observer
// or an AnimationOptions callback that cancels or starts a command while
// `startTransition` or `updateTransitions` runs) have no port: the Rust
// callbacks can't borrow the Transform that is calling them.

use std::cell::{Cell, RefCell};
use std::f64::consts::PI;
use std::rc::Rc;
use std::sync::Arc;

use geometry::Point;
use mln_map_types::{
    AnimationOptions, CameraDelta, CameraError, CameraMomentum, CameraOptions, CameraUpdate, CameraUpdateMode,
    CameraVelocity, ConstrainMode, FreeCameraOptions, GesturePhase, MomentumDecay, MomentumDecayModel, MomentumEasing,
    MomentumSnap, NorthOrientation, ProjectionMode, TransformState, TransformStateProperties, TransitionEnd,
    TransitionOutcome, ViewportMode,
};
use mln_util::UnitBezier;
use mln_util::chrono::{Clock, Duration, TimePoint};
use mln_util::constants::{DEFAULT_TRANSITION_EASE, M2PI, PITCH_MAX};
use mln_util::error::Error;
use mln_util::geo::{EdgeInsets, LatLng, LatLngBounds, ScreenCoordinate, WrapMode};
use mln_util::interpolate::interpolate;
use mln_util::logging::{Event, Log};
use mln_util::projection::Projection;
use mln_util::size::Size;
use mln_util::sync::SyncCell;

use crate::map_event::{CameraChange, CameraChangeMode, CameraChangeSource};

/// A frame time older than this is stale (see [`Transform::frame_time`]):
/// longer than a frame at 10 fps, so any host that draws sets a fresh one
/// first.
pub const STALE_FRAME_TIME: Duration = Duration::from_millis(100);

/// `std::min`.
fn std_min(a: f64, b: f64) -> f64 {
    if b < a { b } else { a }
}

/// `std::max`.
fn std_max(a: f64, b: f64) -> f64 {
    if a < b { b } else { a }
}

/// Converts the given angle (in radians) to be numerically close to the
/// anchor angle, allowing it to be interpolated properly without sudden
/// jumps.
fn normalize_angle(mut angle: f64, anchor_angle: f64) -> f64 {
    if angle.is_nan() || anchor_angle.is_nan() {
        return 0.0;
    }

    angle = mln_math::wrap(angle, -PI, PI);
    if angle == -PI {
        angle = PI;
    }
    let diff = (angle - anchor_angle).abs();
    if (angle - M2PI - anchor_angle).abs() < diff {
        angle -= M2PI;
    }
    if (angle + M2PI - anchor_angle).abs() < diff {
        angle += M2PI;
    }

    angle
}

/// `util::angle_between` (util/math.hpp).
fn angle_between(a: &Point<f64>, b: &Point<f64>) -> f64 {
    (a.x * b.y - a.y * b.x).atan2(a.x * b.x + a.y * b.y)
}

fn lat_lng(lat: f64, lon: f64) -> LatLng {
    // C++ constructs LatLng directly, which throws std::domain_error.
    LatLng::new(lat, lon).unwrap_or_else(|e| panic!("{e}"))
}

/// An animation frame's `EdgeInsets(top, left, bottom, right)`. C++ throws
/// std::domain_error for NaN (infinite insets interpolate to NaN) out of
/// the frame, and nothing catches it; the port logs it, and the frame keeps
/// the insets.
fn frame_edge_insets(top: f64, left: f64, bottom: f64, right: f64) -> Option<EdgeInsets> {
    EdgeInsets::new(top, left, bottom, right).map_err(|e| Log::error(Event::General, &e.message)).ok()
}

/// An animation frame's `Projection::unproject`, which constructs a LatLng
/// and so can throw std::domain_error (NaN); handled as
/// [`frame_edge_insets`]: the frame keeps the center and zoom.
fn frame_unproject(p: &Point<f64>, scale: f64, wrap_mode: WrapMode) -> Option<LatLng> {
    Projection::unproject(p, scale, wrap_mode).map_err(|e| Log::error(Event::General, &e.message)).ok()
}

/// `std::to_string(double)`: `%f`.
fn std_to_string(value: f64) -> String {
    format!("{value:.6}")
}

/// `TransformObserver` (src/mln/map/transform.hpp).
///
/// The camera callbacks take a [`CameraChange`].
pub trait TransformObserver {
    fn on_camera_will_change(&self, _change: CameraChange) {}
    fn on_camera_is_changing(&self) {}
    fn on_camera_did_change(&self, _change: CameraChange) {}
}

/// `TransformObserver::nullObserver()`.
pub struct NullTransformObserver;
impl TransformObserver for NullTransformObserver {}

/// A command's camera at an eased progress: every field set, except a
/// center or padding the frame could not compute (logged), which keeps the
/// current one.
type FrameFn = Rc<dyn Fn(&TransformState, f64) -> CameraOptions>;

// `Transform::CameraField`.
const CENTER: u16 = 1 << 0;
const ZOOM: u16 = 1 << 1;
const BEARING: u16 = 1 << 2;
const PITCH: u16 = 1 << 3;
const PADDING: u16 = 1 << 4;
const ALTITUDE: u16 = 1 << 5;
const ROLL: u16 = 1 << 6;
const FOV: u16 = 1 << 7;

/// `Transform::cameraFields`: the fields a command sets.
fn camera_fields(camera: &CameraOptions) -> u16 {
    let field = |set: bool, field: u16| if set { field } else { 0 };
    field(camera.center.is_some(), CENTER)
        | field(camera.zoom.is_some(), ZOOM)
        | field(camera.bearing.is_some(), BEARING)
        | field(camera.pitch.is_some(), PITCH)
        | field(camera.padding.is_some(), PADDING)
        | field(camera.center_altitude.is_some(), ALTITUDE)
        | field(camera.roll.is_some(), ROLL)
        | field(camera.fov.is_some(), FOV)
}

/// `Transform::Transition`: one camera command.
struct Transition {
    /// Remaining property ownership; replacement removes fields from this
    /// mask.
    fields: Cell<u16>,
    /// Replacing any coupled field relinquishes all coupled fields together.
    coupled_fields: u16,
    /// Properties classified as moving at command start; intersected with
    /// `fields` when reporting movement, so replaced properties no longer
    /// contribute. A momentum adds the fields its settling moves.
    moving_fields: Cell<u16>,
    start: TimePoint,
    /// A momentum extends it for its settling.
    duration: Cell<Duration>,
    animation: AnimationOptions,
    frame: FrameFn,
    anchor: Option<ScreenCoordinate>,
    anchor_lat_lng: LatLng,
    /// Set for a momentum, which moves the camera relative to where it
    /// stands instead of through `frame`.
    momentum: Option<RefCell<MomentumState>>,
    /// Why the command ends, once something other than time ends it:
    /// `Cancelled`, or `Replaced` when later commands took all its fields.
    /// Set through [`Transition::ended_by`], where cancellation wins.
    stopped: Cell<Option<TransitionOutcome>>,
    /// What started the command, which the change that ends it reports.
    source: CameraChangeSource,
    /// For a gesture's momentum, the gesture's serial: its end ends the
    /// gesture, unless another one started since.
    gesture: Option<u64>,
}

impl Transition {
    fn new(
        fields: u16,
        coupled_fields: u16,
        moving_fields: u16,
        start: TimePoint,
        duration: Duration,
        animation: &AnimationOptions,
        frame: FrameFn,
        source: CameraChangeSource,
    ) -> Transition {
        Transition {
            fields: Cell::new(fields),
            coupled_fields,
            moving_fields: Cell::new(moving_fields),
            start,
            duration: Cell::new(duration),
            animation: animation.clone(),
            frame,
            anchor: None,
            anchor_lat_lng: LatLng::default(),
            momentum: None,
            stopped: Cell::new(None),
            source,
            gesture: None,
        }
    }

    fn mode(&self) -> CameraChangeMode {
        if self.duration.get() != Duration::zero() { CameraChangeMode::Animated } else { CameraChangeMode::Immediate }
    }

    /// Records why the command stops: `Cancelled` or `Replaced`. A
    /// cancellation wins over a replacement, as maplibre-native-ffi reports
    /// it.
    fn ended_by(&self, outcome: TransitionOutcome) {
        if self.stopped.get() != Some(TransitionOutcome::Cancelled) {
            self.stopped.set(Some(outcome));
        }
    }

    /// How the command ended, with a momentum's leftover speed: why it
    /// stopped, else `Replaced` if later commands took every momentum part,
    /// else `Completed`.
    fn end(&self) -> TransitionEnd {
        let (velocity, momentum_replaced) =
            self.momentum.as_ref().map(|m| m.borrow().end()).unwrap_or((CameraVelocity::default(), false));
        let outcome = self.stopped.get().unwrap_or(if momentum_replaced {
            TransitionOutcome::Replaced
        } else {
            TransitionOutcome::Completed
        });
        TransitionEnd { outcome, velocity, generation: 0 }
    }

    /// The elapsed share of the duration, in [0, 1]; 1 for an immediate
    /// command.
    fn progress(&self, now: TimePoint) -> f64 {
        let duration = self.duration.get();
        if duration == Duration::zero() {
            return 1.0;
        }
        mln_math::clamp_f64((now - self.start).count() as f64 / duration.count() as f64, 0.0, 1.0)
    }
}

/// A `Duration` in seconds.
fn seconds_f64(duration: Duration) -> f64 {
    duration.count() as f64 / 1e9
}

/// `MomentumState::Glide`: how far one part moves, over how long, and along
/// which curve.
#[derive(Clone, Copy, Default)]
struct Glide {
    distance: f64,
    duration: Duration,
    /// For friction, the decay rate per second.
    rate: f64,
    /// For an eased glide, the curve it follows.
    easing: Option<MomentumEasing>,
}

impl Glide {
    /// Plans a release at `speed`, or returns nothing when it doesn't move.
    fn plan(speed: f64, decay: &MomentumDecay) -> Option<Glide> {
        if !(speed > 0.0) || speed < decay.min_speed {
            return None;
        }
        let mut speed = std_min(speed, decay.max_speed);
        let mut glide = Glide::default();
        let seconds;
        match decay.model {
            MomentumDecayModel::Friction => {
                let half_life = seconds_f64(decay.half_life);
                if !(half_life > 0.0) || !(decay.stop_speed > 0.0) {
                    return None;
                }
                glide.rate = std::f64::consts::LN_2 / half_life;
                speed = std_min(speed, decay.stop_speed + glide.rate * decay.max_distance);
                if speed <= decay.stop_speed {
                    return None;
                }
                seconds = (speed / decay.stop_speed).ln() / glide.rate;
                glide.distance = (speed - decay.stop_speed) / glide.rate;
            }
            MomentumDecayModel::Ease => {
                glide.distance = std_min(decay.distance, decay.max_distance);
                seconds = seconds_f64(decay.duration);
                glide.easing = Some(decay.easing);
            }
        }
        // A glide too long for the clock doesn't move. The bound is
        // exclusive, as in flyTo(), because Duration::max() rounds up as a
        // double.
        if !(glide.distance > 0.0) || !(seconds > 0.0) || seconds >= seconds_f64(Duration::max()) {
            return None;
        }
        // duration_cast truncates.
        glide.duration = Duration::from_nanos((seconds * 1e9) as i64);
        if glide.duration <= Duration::zero() {
            return None;
        }
        Some(glide)
    }

    /// The share of the distance covered after `elapsed`, from 0 to 1.
    fn progress(&self, elapsed: Duration) -> f64 {
        if elapsed >= self.duration {
            return 1.0;
        }
        if elapsed <= Duration::zero() {
            return 0.0;
        }
        let t = seconds_f64(elapsed);
        let total = seconds_f64(self.duration);
        match self.easing {
            None => (-self.rate * t).exp_m1() / (-self.rate * total).exp_m1(),
            Some(easing) => bezier(&easing).solve(t / total, 1e-6),
        }
    }

    /// The share of the distance covered per second after `elapsed`.
    fn speed(&self, elapsed: Duration) -> f64 {
        if elapsed >= self.duration {
            return 0.0;
        }
        let t = std_max(0.0, seconds_f64(elapsed));
        let total = seconds_f64(self.duration);
        let Some(easing) = self.easing else {
            return self.rate * (-self.rate * t).exp() / -(-self.rate * total).exp_m1();
        };
        // The slope dy/dx of the curve, from its derivatives in its own
        // parameter, which starts just past 0 where both can be 0.
        let ease = bezier(&easing);
        let p = std_max(ease.solve_curve_x(t / total, 1e-6), 1e-6);
        let cy = 3.0 * easing.y1;
        let by = 3.0 * (easing.y2 - easing.y1) - cy;
        let ay = 1.0 - cy - by;
        let dy = (3.0 * ay * p + 2.0 * by) * p + cy;
        let dx = ease.sample_curve_derivative_x(p);
        if dx > 0.0 { dy / dx / total } else { 0.0 }
    }
}

fn bezier(easing: &MomentumEasing) -> UnitBezier {
    UnitBezier::new(easing.x1, easing.y1, easing.x2, easing.y2)
}

/// `MomentumState::Part`: one part of a running momentum.
#[derive(Clone, Copy, Default)]
struct MomentumPart {
    /// The fields that this part needs to keep moving or to settle.
    fields: u16,
    glide: Glide,
    /// The share of the distance already applied.
    applied: f64,
    moving: bool,
    /// Where the part may settle once it stops moving.
    snap: Option<MomentumSnap>,
    snap_pending: bool,
    /// The change that the settling makes, while it runs, when it started,
    /// and its eased share applied.
    snap_delta: Option<f64>,
    snap_start: Duration,
    snap_applied: f64,
}

impl MomentumPart {
    fn active(&self) -> bool {
        self.moving || self.snap_pending || self.snap_delta.is_some()
    }
}

// The scalar parts of a momentum, in `MomentumState::scalars`.
const MOMENTUM_ZOOM: usize = 0;
const MOMENTUM_BEARING: usize = 1;
const MOMENTUM_PITCH: usize = 2;

/// `Transform::MomentumState`: a momentum under way.
struct MomentumState {
    options: CameraMomentum,
    /// The pan's distance, with its direction.
    pan_total: ScreenCoordinate,
    pan: MomentumPart,
    /// The zoom, bearing and pitch parts, and each one's signed distance.
    scalars: [MomentumPart; 3],
    scalar_totals: [f64; 3],
    /// Whether another command took a field that a moving part needed.
    replaced: bool,
    /// When the last frame applied the momentum, from its start.
    elapsed: Duration,
}

impl MomentumState {
    fn anchor(&self, part: usize) -> Option<ScreenCoordinate> {
        match part {
            MOMENTUM_ZOOM => self.options.zoom_anchor,
            MOMENTUM_BEARING => self.options.bearing_anchor,
            _ => self.options.pitch_anchor,
        }
    }

    /// Stops the parts that lost a field they need, and returns the fields
    /// that the rest still need.
    fn retain(&mut self, owned: u16) -> u16 {
        let mut needed = 0;
        let mut replaced = false;
        for part in std::iter::once(&mut self.pan).chain(self.scalars.iter_mut()) {
            if !part.active() {
                continue;
            }
            if part.fields & owned != part.fields {
                replaced |= part.moving || part.snap_delta.is_some();
                part.moving = false;
                part.snap_pending = false;
                part.snap_delta = None;
                continue;
            }
            needed |= part.fields;
        }
        self.replaced |= replaced;
        needed
    }

    /// The speed of the parts still moving, as of the last frame, which left
    /// the camera where it stands, and whether another command stopped a
    /// part early by taking a field it needed.
    fn end(&self) -> (CameraVelocity, bool) {
        let mut velocity = CameraVelocity::default();
        if self.pan.moving {
            let share = self.pan.glide.speed(self.elapsed);
            velocity.pan = ScreenCoordinate::new(self.pan_total.x * share, self.pan_total.y * share);
        }
        let speed = |i: usize| {
            let part = &self.scalars[i];
            if part.moving { self.scalar_totals[i] * part.glide.speed(self.elapsed) } else { 0.0 }
        };
        velocity.zoom = speed(MOMENTUM_ZOOM);
        velocity.bearing = speed(MOMENTUM_BEARING);
        velocity.pitch = speed(MOMENTUM_PITCH);
        (velocity, self.replaced)
    }
}

/// The change that `snap` makes to `value`, or zero when it stays. A
/// circular value, the bearing, has its targets within one turn from the
/// offset, and takes the shorter way around to the nearest.
fn snap_delta(value: f64, snap: &MomentumSnap, circular: bool) -> f64 {
    if !(snap.spacing > 0.0) {
        return 0.0;
    }
    let delta = if circular {
        let turn = value - snap.offset - 360.0 * ((value - snap.offset) / 360.0).floor();
        let below = (turn / snap.spacing).floor() * snap.spacing;
        let above = std_min(below + snap.spacing, 360.0);
        if turn - below <= above - turn { below - turn } else { above - turn }
    } else {
        // `f64::round` rounds halfway away from zero, as std::round does.
        snap.offset + ((value - snap.offset) / snap.spacing).round() * snap.spacing - value
    };
    if delta.abs() <= snap.tolerance { delta } else { 0.0 }
}

/// The value that a part of a momentum moves, for its snap.
fn momentum_value(state: &TransformState, part: usize) -> f64 {
    match part {
        MOMENTUM_ZOOM => state.get_zoom(),
        MOMENTUM_BEARING => -mln_math::rad2deg(state.get_bearing()),
        _ => mln_math::rad2deg(state.get_pitch()),
    }
}

pub struct Transform {
    observer: Rc<dyn TransformObserver>,
    state: TransformState,

    latest_transition_start: TimePoint,
    latest_transition_duration: Duration,
    transitions: Vec<Rc<Transition>>,
    /// What the commands being started come from: a command unless a gesture
    /// or a momentum starts them (see [`Self::with_source`]).
    source: CameraChangeSource,
    gesture: GestureState,
    /// The host's current frame time (see [`Self::set_frame_time`]).
    frame_time: Option<TimePoint>,
}

/// The gesture that camera deltas and updates with a phase make, as
/// maplibre-native-ffi tracks it.
#[derive(Clone, Copy, Debug, Default)]
struct GestureState {
    /// Between its first `Begin` or `Update` and its `End` or `Cancel`.
    active: bool,
    /// Counts gestures, so that an earlier gesture's momentum doesn't end a
    /// later gesture.
    serial: u64,
    /// The fields the gesture changed (`ZOOM`, `BEARING`, `PITCH`), which
    /// alone may settle.
    changed: u16,
}

impl GestureState {
    /// Records the fields a gesture's change moves.
    fn change(&mut self, zoom: bool, bearing: bool, pitch: bool) {
        self.changed |=
            (if zoom { ZOOM } else { 0 }) | (if bearing { BEARING } else { 0 }) | (if pitch { PITCH } else { 0 });
    }

    /// Drops the snaps of the parts the gesture didn't change: only what it
    /// changed settles.
    fn mask_snaps(&self, momentum: &mut CameraMomentum) {
        if self.changed & ZOOM == 0 {
            momentum.zoom_snap = None;
        }
        if self.changed & BEARING == 0 {
            momentum.bearing_snap = None;
        }
        if self.changed & PITCH == 0 {
            momentum.pitch_snap = None;
        }
    }
}

impl Transform {
    pub fn new(
        observer: Rc<dyn TransformObserver>,
        constrain_mode: ConstrainMode,
        viewport_mode: ViewportMode,
    ) -> Self {
        Transform {
            observer,
            state: TransformState::new(constrain_mode, viewport_mode),
            latest_transition_start: TimePoint::default(),
            latest_transition_duration: Duration::zero(),
            transitions: Vec::new(),
            source: CameraChangeSource::Command,
            gesture: GestureState::default(),
            frame_time: None,
        }
    }

    /// `Transform(const TransformState&)`.
    pub fn from_state(state: TransformState) -> Self {
        Transform {
            observer: Rc::new(NullTransformObserver),
            state,
            latest_transition_start: TimePoint::default(),
            latest_transition_duration: Duration::zero(),
            transitions: Vec::new(),
            source: CameraChangeSource::Command,
            gesture: GestureState::default(),
            frame_time: None,
        }
    }

    // MARK: - Map View

    pub fn resize(&mut self, size: Size) -> Result<(), Error> {
        if size.is_empty() {
            return Err(Error::invalid_argument("failed to resize: size is empty"));
        }

        if self.state.get_size() == size {
            return Ok(());
        }

        self.observer.on_camera_will_change(CameraChange::default());
        self.state.set_size(size);
        let mut scale = self.state.get_scale();
        let mut x = self.state.get_x();
        let mut y = self.state.get_y();
        let z = self.state.get_z();

        let mut lat = 0.0;
        let mut lon = 0.0;
        if self.state.constrain_screen(&mut scale, &mut lat, &mut lon) {
            // Turns out that if you resize during a transition any changes
            // made to the state will be ignored :( So if we have to constrain
            // because of a resize and a transition is in progress - cancel
            // the transition!
            if self.in_transition() {
                self.cancel_transitions();
            }

            // It also turns out that state.setProperties isn't enough if you
            // change the center, you need to set Cc and Bc too, which
            // setLatLngZoom does.
            let zoom = self.state.scale_zoom(scale);
            self.state.set_lat_lng_zoom(&lat_lng(lat, lon), zoom);
            self.observer.on_camera_did_change(CameraChange::default());

            return Ok(());
        }
        self.state.constrain(&mut scale, &mut x, &mut y);
        self.state.set_properties(
            &TransformStateProperties::default()
                .with_scale(Some(scale))
                .with_x(Some(x))
                .with_y(Some(y))
                .with_z(Some(z)),
        );

        self.observer.on_camera_did_change(CameraChange::default());
        Ok(())
    }

    // MARK: - Camera

    /// Returns the current camera options.
    pub fn get_camera_options(&self, padding: Option<EdgeInsets>) -> CameraOptions {
        self.state.get_camera_options(padding)
    }

    /// Change any combination of center, zoom, bearing, and pitch, without a
    /// transition. Omitted options keep their current animations.
    pub fn jump_to(&mut self, camera: &CameraOptions) {
        self.ease_to(camera, &AnimationOptions::default());
    }

    /// Change any combination of center, zoom, bearing, pitch and edgeInsets,
    /// with a smooth animation between old and new values. Omitted options
    /// keep their current animations and timing.
    pub fn ease_to(&mut self, input_camera: &CameraOptions, animation: &AnimationOptions) {
        let mut camera = *input_camera;

        let duration = animation.duration.unwrap_or(Duration::zero());
        if self.state.get_lat_lng_bounds() == LatLngBounds::default()
            && !self.is_gesture_in_progress()
            && duration != Duration::zero()
        {
            // reuse flyTo, without exaggerated animation, to achieve constant
            // ground speed.
            self.fly_to(&camera, animation, true);
            return;
        }

        let mut zoom = camera.zoom.unwrap_or(self.get_zoom());
        self.state.constrain_camera_and_zoom_to_bounds(&mut camera, &mut zoom);

        let padding = camera.padding.unwrap_or(self.state.get_edge_insets());
        let mut start_lat_lng = self.get_lat_lng(WrapMode::Unwrapped);
        let unwrapped_lat_lng = camera.center.unwrap_or(start_lat_lng);
        let lat_lng_end = if self.state.get_lat_lng_bounds() != LatLngBounds::default() {
            unwrapped_lat_lng
        } else {
            unwrapped_lat_lng.wrapped()
        };

        let mut bearing = match camera.bearing {
            Some(b) => mln_math::deg2rad(-b),
            None => self.get_bearing(),
        };
        let mut pitch = match camera.pitch {
            Some(p) => mln_math::deg2rad(p),
            None => self.get_pitch(),
        };
        let mut fov = match camera.fov {
            Some(f) => mln_math::deg2rad(f),
            None => self.get_field_of_view(),
        };
        let center_alt = camera.center_altitude.unwrap_or(self.state.get_center_altitude());
        let roll = match camera.roll {
            Some(r) => mln_math::deg2rad(r),
            None => self.get_roll(),
        };

        if zoom.is_nan() || bearing.is_nan() || pitch.is_nan() || roll.is_nan() || fov.is_nan() {
            end_unstarted(animation, TransitionOutcome::Cancelled);
            return;
        }

        if self.state.get_lat_lng_bounds() == LatLngBounds::default() {
            if self.is_gesture_in_progress() {
                // If gesture in progress, we transfer the wrap rounds from the
                // end longitude into start, so the "scroll effect" of rounding
                // the world is the same while assuring the end longitude
                // remains wrapped.
                let wrap = unwrapped_lat_lng.longitude() - lat_lng_end.longitude();
                start_lat_lng = lat_lng(start_lat_lng.latitude(), start_lat_lng.longitude() - wrap);
            } else {
                // Find the shortest path otherwise.
                start_lat_lng.unwrap_for_shortest_path(&lat_lng_end);
            }
        }
        let start_center_alt = self.state.get_center_altitude();

        let start_point = Projection::project(&start_lat_lng, self.state.get_scale());
        let end_point = Projection::project(&lat_lng_end, self.state.get_scale());

        // Constrain camera options.
        zoom = mln_math::clamp_f64(zoom, self.state.get_min_zoom(), self.state.get_max_zoom());
        pitch = mln_math::clamp_f64(pitch, self.state.get_min_pitch(), self.state.get_max_pitch());
        fov = mln_math::clamp_f64(fov, self.state.get_min_field_of_view(), self.state.get_max_field_of_view());

        // Minimize rotation by taking the shorter path around the circle.
        bearing = normalize_angle(bearing, self.state.get_bearing());

        let start_zoom = self.state.get_zoom();
        let start_bearing = normalize_angle(self.state.get_bearing(), bearing);
        let start_pitch = self.state.get_pitch();
        let start_roll = self.state.get_roll();
        let start_fov = self.state.get_field_of_view() as f64;
        let start_edge_insets = self.state.get_edge_insets();

        let frame: FrameFn = Rc::new(move |state: &TransformState, t: f64| {
            let frame_point = interpolate(&start_point, &end_point, t);
            let frame_lat_lng = frame_unproject(&frame_point, state.zoom_scale(start_zoom), WrapMode::Unwrapped);
            let frame_zoom = interpolate(&start_zoom, &zoom, t);
            CameraOptions::default()
                .with_center(frame_lat_lng)
                .with_zoom(Some(frame_zoom))
                .with_center_altitude(Some(interpolate(&start_center_alt, &center_alt, t)))
                .with_bearing(Some(-mln_math::rad2deg(mln_math::wrap(
                    interpolate(&start_bearing, &bearing, t),
                    -PI,
                    PI,
                ))))
                .with_padding(interpolate_edge_insets(&start_edge_insets, &padding, t))
                .with_pitch(Some(mln_math::rad2deg(interpolate(&start_pitch, &pitch, t))))
                .with_roll(Some(mln_math::rad2deg(interpolate(&start_roll, &roll, t))))
                .with_fov(Some(mln_math::rad2deg(interpolate(&start_fov, &fov, t))))
        });

        self.start_transition(input_camera, animation, frame, duration, false);
    }

    /// This method implements an “optimal path” animation, as detailed in:
    ///
    /// Van Wijk, Jarke J.; Nuij, Wim A. A. “Smooth and efficient zooming and
    /// panning.” INFOVIS ’03. pp. 15–22.
    /// <https://www.win.tue.nl/~vanwijk/zoompan.pdf#page=5>.
    ///
    /// Where applicable, local variable documentation begins with the
    /// associated variable or function in van Wijk (2003).
    pub fn fly_to(
        &mut self,
        input_camera: &CameraOptions,
        animation: &AnimationOptions,
        linear_zoom_interpolation: bool,
    ) {
        let mut camera = *input_camera;

        let mut zoom = camera.zoom.unwrap_or(self.get_zoom());
        self.state.constrain_camera_and_zoom_to_bounds(&mut camera, &mut zoom);

        let padding = camera.padding.unwrap_or(self.state.get_edge_insets());
        let lat_lng_end = camera.center.unwrap_or(self.get_lat_lng(WrapMode::Unwrapped)).wrapped();
        let center_alt = camera.center_altitude.unwrap_or(self.state.get_center_altitude());
        let mut bearing = match camera.bearing {
            Some(b) => mln_math::deg2rad(-b),
            None => self.get_bearing(),
        };
        let mut pitch = match camera.pitch {
            Some(p) => mln_math::deg2rad(p),
            None => self.get_pitch(),
        };
        let roll = match camera.roll {
            Some(r) => mln_math::deg2rad(r),
            None => self.get_roll(),
        };
        let mut fov = match camera.fov {
            Some(f) => mln_math::deg2rad(f),
            None => self.get_field_of_view(),
        };

        if zoom.is_nan()
            || bearing.is_nan()
            || pitch.is_nan()
            || roll.is_nan()
            || fov.is_nan()
            || self.state.get_size().is_empty()
        {
            end_unstarted(animation, TransitionOutcome::Cancelled);
            return;
        }

        // Determine endpoints.
        let mut start_lat_lng = self.get_lat_lng(WrapMode::Unwrapped).wrapped();
        start_lat_lng.unwrap_for_shortest_path(&lat_lng_end);
        let start_center_alt = self.state.get_center_altitude();

        let start_point = Projection::project(&start_lat_lng, self.state.get_scale());
        let end_point = Projection::project(&lat_lng_end, self.state.get_scale());

        // Constrain camera options.
        zoom = mln_math::clamp_f64(zoom, self.state.get_min_zoom(), self.state.get_max_zoom());
        pitch = mln_math::clamp_f64(pitch, self.state.get_min_pitch(), self.state.get_max_pitch());
        fov = mln_math::clamp_f64(fov, self.state.get_min_field_of_view(), self.state.get_max_field_of_view());

        // Minimize rotation by taking the shorter path around the circle.
        bearing = normalize_angle(bearing, self.state.get_bearing());
        let start_zoom = self.state.scale_zoom(self.state.get_scale());
        let start_bearing = normalize_angle(self.state.get_bearing(), bearing);
        let start_pitch = self.state.get_pitch();
        let start_roll = self.state.get_roll();
        let start_fov = self.state.get_field_of_view() as f64;

        // w₀: Initial visible span, measured in pixels at the initial scale.
        // Known henceforth as a <i>screenful</i>.
        let size = self.state.get_size();
        let w0 = std_max(
            size.width as f64 - padding.left - padding.right,
            size.height as f64 - padding.top - padding.bottom,
        );
        // w₁: Final visible span, measured in pixels with respect to the
        // initial scale.
        let w1 = w0 / self.state.zoom_scale(zoom - start_zoom);
        // Length of the flight path as projected onto the ground plane,
        // measured in pixels from the world image origin at the initial scale.
        let u1 = (end_point.x - start_point.x).hypot(end_point.y - start_point.y);

        // ρ: The relative amount of zooming that takes place along the flight
        // path. A high value maximizes zooming for an exaggerated animation,
        // while a low value minimizes zooming for something closer to
        // easeTo().
        //
        // 1.42 is the average value selected by participants in the user
        // study in van Wijk (2003). A value of 6<sup>¼</sup> would be
        // equivalent to the root mean squared average velocity,
        // V<sub>RMS</sub>. A value of 1 produces a circular motion.
        let mut rho = 1.42;
        if animation.min_zoom.is_some() || linear_zoom_interpolation {
            // util::min(a, b, c) = min(a, min(b, c)), fmin on doubles
            let mut min_zoom =
                mln_math::min_f64(animation.min_zoom.unwrap_or(start_zoom), mln_math::min_f64(start_zoom, zoom));
            min_zoom = mln_math::clamp_f64(min_zoom, self.state.get_min_zoom(), self.state.get_max_zoom());
            // w<sub>m</sub>: Maximum visible span, measured in pixels with
            // respect to the initial scale.
            let w_max = w0 / self.state.zoom_scale(min_zoom - start_zoom);
            rho = if u1 != 0.0 { (w_max / u1 * 2.0).sqrt() } else { 1.0 };
        }
        // ρ²
        let rho2 = rho * rho;

        // rᵢ: Returns the zoom-out factor at one end of the animation.
        //
        // @param i 0 for the ascent or 1 for the descent.
        let r = |i: f64| -> f64 {
            // bᵢ
            let b = (w1 * w1 - w0 * w0 + (if i != 0.0 { -1.0 } else { 1.0 }) * rho2 * rho2 * u1 * u1)
                / (2.0 * (if i != 0.0 { w1 } else { w0 }) * rho2 * u1);
            ((b * b + 1.0).sqrt() - b).ln()
        };

        // r₀: Zoom-out factor during ascent.
        let r0 = if u1 != 0.0 { r(0.0) } else { f64::INFINITY }; // Silence division by 0 on sanitize bot.
        let r1 = if u1 != 0.0 { r(1.0) } else { f64::INFINITY };

        // When u₀ = u₁, the optimal path doesn’t require both ascent and
        // descent.
        let is_close = u1.abs() < 0.000001 || !r0.is_finite() || !r1.is_finite();

        // w(s): Returns the visible span on the ground, measured in pixels
        // with respect to the initial scale.
        //
        // Assumes an angular field of view of 2 arctan ½ ≈ 53°.
        let w = move |s: f64| -> f64 {
            if is_close {
                ((if w1 < w0 { -1.0 } else { 1.0 }) * rho * s).exp()
            } else {
                r0.cosh() / (r0 + rho * s).cosh()
            }
        };
        // u(s): Returns the distance along the flight path as projected onto
        // the ground plane, measured in pixels from the world image origin at
        // the initial scale.
        let u = move |s: f64| -> f64 {
            if is_close { 0.0 } else { w0 * (r0.cosh() * (r0 + rho * s).tanh() - r0.sinh()) / rho2 / u1 }
        };
        // S: Total length of the flight path, measured in ρ-screenfuls.
        let total_s = if is_close { (w1 / w0).ln().abs() / rho } else { (r1 - r0) / rho };

        let duration = if let Some(duration) = animation.duration {
            duration
        } else {
            // V: Average velocity, measured in ρ-screenfuls per second.
            let mut velocity = 1.2;
            if let Some(v) = animation.velocity {
                velocity = v / rho;
            }
            let seconds = total_s / velocity;
            // A duration Duration can't hold jumps, as one past max_duration
            // does, rather than saturating to a flight that never ends. The
            // bound is exclusive: Duration::max() rounds up as a double.
            if seconds.is_finite() && seconds >= 0.0 && seconds < Duration::max().count() as f64 / 1_000_000_000.0 {
                // duration_cast<Duration>(duration<double>(S / velocity))
                Duration::from_nanos((seconds * 1_000_000_000.0) as i64)
            } else {
                Duration::zero()
            }
        };
        let duration = match animation.max_duration {
            Some(max) if !linear_zoom_interpolation && duration > max => Duration::zero(),
            _ => duration,
        };
        let start_scale = self.state.get_scale();
        let start_edge_insets = self.state.get_edge_insets();

        let frame: FrameFn = Rc::new(move |state: &TransformState, k: f64| {
            // s: The distance traveled along the flight path, measured in
            // ρ-screenfuls.
            let s = k * total_s;
            let us = if k == 1.0 { 1.0 } else { u(s) };

            // Calculate the current point and zoom level along the flight path.
            let frame_point = interpolate(&start_point, &end_point, us);
            let mut frame_zoom = if linear_zoom_interpolation {
                interpolate(&start_zoom, &zoom, k)
            } else {
                start_zoom + state.scale_zoom(1.0 / w(s))
            };

            // Zoom can be NaN if size is empty.
            if frame_zoom.is_nan() {
                frame_zoom = zoom;
            }

            // Convert to geographic coordinates and set the new viewpoint.
            let frame_lat_lng = frame_unproject(&frame_point, start_scale, WrapMode::Unwrapped);
            CameraOptions::default()
                .with_center(frame_lat_lng)
                .with_zoom(Some(frame_zoom))
                .with_center_altitude(Some(interpolate(&start_center_alt, &center_alt, us)))
                .with_bearing(Some(-mln_math::rad2deg(mln_math::wrap(
                    interpolate(&start_bearing, &bearing, k),
                    -PI,
                    PI,
                ))))
                .with_padding(interpolate_edge_insets(&start_edge_insets, &padding, k))
                .with_pitch(Some(mln_math::rad2deg(interpolate(&start_pitch, &pitch, k))))
                .with_roll(Some(mln_math::rad2deg(interpolate(&start_roll, &roll, k))))
                .with_fov(Some(mln_math::rad2deg(interpolate(&start_fov, &fov, k))))
        });

        self.start_transition(input_camera, animation, frame, duration, !linear_zoom_interpolation);
    }

    // MARK: - Position

    /// Pans the map by the given amount.
    /// Fails with [`ErrorKind::InvalidArgument`](mln_util::ErrorKind) for a NaN
    /// or infinite offset.
    pub fn move_by(&mut self, offset: &ScreenCoordinate, animation: &AnimationOptions) -> Result<(), Error> {
        let mut center_offset = *offset;

        // Reduce the offset so that it never goes past the horizon. If it
        // goes past the horizon, the pan direction is opposite of the
        // intended direction.
        let pitch = self.state.get_pitch();
        let offset_length = offset.x.hypot(offset.y);
        if pitch > 0.0 && offset_length > 0.0 {
            let camera_to_center =
                0.5 * self.state.get_size().height as f64 / (self.state.get_field_of_view() as f64 / 2.0).tan();
            let pixels_to_horizon = (camera_to_center / pitch.tan()).abs();
            // must be < 1 to keep the offset short of the horizon
            let horizon_factor = 0.75;
            let scale = pixels_to_horizon * horizon_factor / offset_length;
            if scale < 1.0 {
                center_offset = ScreenCoordinate::new(offset.x * scale, offset.y * scale);
            }
        }

        let size = self.state.get_size();
        let center = self.state.get_edge_insets().get_center(size.width as u16, size.height as u16);
        let point_on_screen = ScreenCoordinate::new(center.x - center_offset.x, center.y - center_offset.y);
        // Use unwrapped LatLng to carry information about moveBy direction.
        let center = self.screen_coordinate_to_lat_lng(&point_on_screen, WrapMode::Unwrapped)?;
        self.ease_to(&CameraOptions::default().with_center(Some(center)), animation);
        Ok(())
    }

    /// `getLatLng(LatLng::WrapMode = LatLng::Wrapped)`.
    pub fn get_lat_lng(&self, wrap: WrapMode) -> LatLng {
        self.state.get_lat_lng(wrap)
    }

    // MARK: - Zoom

    pub fn get_zoom(&self) -> f64 {
        self.state.get_zoom()
    }

    // MARK: - Bounds

    pub fn set_lat_lng_bounds(&mut self, bounds: LatLngBounds) -> Result<(), Error> {
        if !bounds.valid() {
            return Err(Error::invalid_argument("failed to set bounds: bounds are invalid"));
        }
        self.state.set_lat_lng_bounds(bounds);
        Ok(())
    }

    pub fn set_min_zoom(&mut self, min_zoom: f64) {
        if min_zoom.is_nan() {
            return;
        }
        self.state.set_min_zoom(min_zoom);
    }

    pub fn set_max_zoom(&mut self, max_zoom: f64) {
        if max_zoom.is_nan() {
            return;
        }
        self.state.set_max_zoom(max_zoom);
    }

    pub fn set_min_pitch(&mut self, min_pitch: f64) {
        if min_pitch.is_nan() {
            return;
        }
        if mln_math::deg2rad(min_pitch) < mln_util::constants::PITCH_MIN {
            Log::warning(
                Event::General,
                format!(
                    "Trying to set minimum pitch below the limit ({} degrees), the value will be clamped.",
                    std_to_string(mln_math::rad2deg(mln_util::constants::PITCH_MIN))
                ),
            );
        }
        self.state.set_min_pitch(mln_math::deg2rad(min_pitch));
    }

    pub fn set_max_pitch(&mut self, max_pitch: f64) {
        if max_pitch.is_nan() {
            return;
        }
        if mln_math::deg2rad(max_pitch) > PITCH_MAX {
            Log::warning(
                Event::General,
                format!(
                    "Trying to set maximum pitch above the limit ({} degrees), the value will be clamped.",
                    std_to_string(mln_math::rad2deg(PITCH_MAX))
                ),
            );
        }
        self.state.set_max_pitch(mln_math::deg2rad(max_pitch));
    }

    pub fn set_frustum_offset(&mut self, frustum_offset: &EdgeInsets) {
        self.state.set_frustum_offset(frustum_offset);
    }

    pub fn get_frustum_offset(&self) -> EdgeInsets {
        self.state.get_frustum_offset()
    }

    // MARK: - Bearing

    pub fn rotate_by(&mut self, first: &ScreenCoordinate, second: &ScreenCoordinate, animation: &AnimationOptions) {
        let size = self.state.get_size();
        let mut center = self.state.get_edge_insets().get_center(size.width as u16, size.height as u16);
        let offset = ScreenCoordinate::new(first.x - center.x, first.y - center.y);
        // C++: std::sqrt(std::pow(2, offset.x) + std::pow(2, offset.y)) (sic).
        let distance = (2.0f64.powf(offset.x) + 2.0f64.powf(offset.y)).sqrt();

        // If the first click was too close to the center, move the center of
        // rotation by 200 pixels in the direction of the click.
        if distance < 200.0 {
            let height_offset = -200.0;
            let rotate_bearing = offset.y.atan2(offset.x);
            center.x = first.x + rotate_bearing.cos() * height_offset;
            center.y = first.y + rotate_bearing.sin() * height_offset;
        }

        let bearing = -mln_math::rad2deg(
            self.state.get_bearing()
                + angle_between(
                    &Point::new(first.x - center.x, first.y - center.y),
                    &Point::new(second.x - center.x, second.y - center.y),
                ),
        );
        self.ease_to(&CameraOptions::default().with_bearing(Some(bearing)), animation);
    }

    pub fn get_bearing(&self) -> f64 {
        self.state.get_bearing()
    }

    // MARK: - Pitch

    pub fn get_pitch(&self) -> f64 {
        self.state.get_pitch()
    }

    pub fn get_roll(&self) -> f64 {
        self.state.get_roll()
    }

    /// Returns `state.getFieldOfView()` (a float) as double.
    pub fn get_field_of_view(&self) -> f64 {
        self.state.get_field_of_view() as f64
    }

    // MARK: - North Orientation

    pub fn set_north_orientation(&mut self, orientation: NorthOrientation) {
        self.state.set_north_orientation(orientation);
        self.constrain_and_set_properties();
    }

    pub fn get_north_orientation(&self) -> NorthOrientation {
        self.state.get_north_orientation()
    }

    fn constrain_and_set_properties(&mut self) {
        let mut scale = self.state.get_scale();
        let mut x = self.state.get_x();
        let mut y = self.state.get_y();
        let z = self.state.get_z();
        self.state.constrain(&mut scale, &mut x, &mut y);
        self.state.set_properties(
            &TransformStateProperties::default()
                .with_scale(Some(scale))
                .with_x(Some(x))
                .with_y(Some(y))
                .with_z(Some(z)),
        );
    }

    // MARK: - Constrain mode

    pub fn set_constrain_mode(&mut self, mode: ConstrainMode) {
        self.state.set_constrain_mode(mode);
        self.constrain_and_set_properties();
    }

    pub fn get_constrain_mode(&self) -> ConstrainMode {
        self.state.get_constrain_mode()
    }

    // MARK: - Viewport mode

    pub fn set_viewport_mode(&mut self, mode: ViewportMode) {
        self.state.set_viewport_mode(mode);
    }

    pub fn get_viewport_mode(&self) -> ViewportMode {
        self.state.get_viewport_mode()
    }

    // MARK: - Projection mode

    pub fn set_projection_mode(&mut self, options: &ProjectionMode) {
        let properties = TransformStateProperties::default()
            .with_axonometric(Some(options.axonometric.unwrap_or(self.state.get_axonometric())))
            .with_x_skew(Some(options.x_skew.unwrap_or(self.state.get_x_skew())))
            .with_y_skew(Some(options.y_skew.unwrap_or(self.state.get_y_skew())));
        self.state.set_properties(&properties);
    }

    pub fn get_projection_mode(&self) -> ProjectionMode {
        ProjectionMode::default()
            .with_axonometric(self.state.get_axonometric())
            .with_x_skew(self.state.get_x_skew())
            .with_y_skew(self.state.get_y_skew())
    }

    // MARK: - Transition

    /// Starts one camera command. `camera` is the command as given: the
    /// fields it sets are the fields it takes over from the running
    /// commands. A flight couples center and zoom; an anchored command
    /// couples center with its fields. Replacing any field of a coupled
    /// group stops the whole group, and a command left with no fields
    /// finishes.
    fn start_transition(
        &mut self,
        camera: &CameraOptions,
        animation: &AnimationOptions,
        frame: FrameFn,
        duration: Duration,
        flight: bool,
    ) {
        let mut fields = camera_fields(camera);
        let mut coupled_fields = if flight { CENTER | ZOOM } else { 0 };
        fields |= coupled_fields;

        // An anchor controls center together with the properties it
        // transforms. An explicit center takes precedence over an anchor.
        let mut anchor = if camera.center.is_some() { None } else { camera.anchor };
        let mut anchor_lat_lng = LatLng::default();
        if let Some(a) = anchor.as_mut() {
            a.y = self.state.get_size().height as f64 - a.y;
            // C++ throws std::domain_error out of the camera change for an
            // anchor that has no LatLng (NaN or infinite); the port logs it
            // and ignores the anchor.
            match self.state.screen_coordinate_to_lat_lng(a, WrapMode::Unwrapped) {
                Ok(lat_lng) => {
                    anchor_lat_lng = lat_lng;
                    fields |= CENTER;
                    coupled_fields = fields;
                }
                Err(e) => {
                    Log::error(Event::General, &e.message);
                    anchor = None;
                }
            }
        }

        let target = frame(&self.state, 1.0);
        let panning = flight || anchor.is_some() || camera.center != Some(self.get_lat_lng(WrapMode::Unwrapped));
        let scaling = flight || target.zoom != Some(self.get_zoom());
        let rotating = target.bearing != Some(-mln_math::rad2deg(self.get_bearing()));
        let moving_fields = (if panning { CENTER } else { 0 })
            | (if scaling { ZOOM } else { 0 })
            | (if rotating { BEARING } else { 0 });

        let start = self.now();
        let duration = if fields != 0 { duration } else { Duration::zero() };
        self.latest_transition_start = start;
        self.latest_transition_duration = duration;
        let transition = Rc::new(Transition {
            anchor,
            anchor_lat_lng,
            ..Transition::new(fields, coupled_fields, moving_fields, start, duration, animation, frame, self.source)
        });

        let finished = self.release_fields(fields);
        self.transitions.push(transition.clone());
        self.update_movement_flags();
        self.finish_transitions(&finished);

        let animated = duration != Duration::zero();
        self.observer.on_camera_will_change(CameraChange { mode: transition.mode(), source: self.source });

        if !animated {
            self.apply_transitions(std::slice::from_ref(&transition), start);
            self.transitions.retain(|active| !Rc::ptr_eq(active, &transition));
            self.update_movement_flags();
            self.finish_transitions(&[transition]);
        }
    }

    /// Composes the current camera with each of `active`'s cameras at `now`,
    /// for the fields it still owns, then applies the result.
    fn apply_transitions(&mut self, active: &[Rc<Transition>], now: TimePoint) {
        // The center is unwrapped (TransformState::getCameraOptions).
        let current = self.state.get_camera_options(None);
        let mut camera = current;

        for transition in active {
            if transition.momentum.is_some() {
                continue;
            }
            let t = transition.progress(now);
            let ease = transition.animation.easing.unwrap_or(DEFAULT_TRANSITION_EASE);
            let value = (transition.frame)(&self.state, if t == 1.0 { 1.0 } else { ease.solve(t, 0.001) });
            let fields = transition.fields.get();
            // A frame without a center or padding (see FrameFn) keeps the
            // current one.
            if fields & CENTER != 0 && value.center.is_some() {
                camera.center = value.center;
            }
            if fields & ZOOM != 0 {
                camera.zoom = value.zoom;
            }
            if fields & BEARING != 0 {
                camera.bearing = value.bearing;
            }
            if fields & PITCH != 0 {
                camera.pitch = value.pitch;
            }
            if fields & PADDING != 0 && value.padding.is_some() {
                camera.padding = value.padding;
            }
            if fields & ALTITUDE != 0 {
                camera.center_altitude = value.center_altitude;
            }
            if fields & ROLL != 0 {
                camera.roll = value.roll;
            }
            if fields & FOV != 0 {
                camera.fov = value.fov;
            }
        }

        // An active anchor also applies to immediate changes of other
        // properties.
        let anchored = self
            .transitions
            .iter()
            .rev()
            .find(|transition| transition.anchor.is_some() && transition.fields.get() & CENTER != 0)
            .cloned();

        // Compose the camera before applying constraints. In particular,
        // padding and field of view determine the pitch limit, and center
        // and zoom jointly determine the geographic bounds and altitude
        // scale.
        //
        // C++ sets every angle from its degrees. The port leaves the angles
        // no command changed as they are, so that converting them to degrees
        // and back doesn't move them.
        let padding = camera.padding.unwrap_or_default();
        self.state.set_edge_insets(&padding);
        if let Some(fov) = camera.fov.filter(|_| camera.fov != current.fov) {
            self.state.set_field_of_view(mln_math::deg2rad(fov));
        }
        if let Some(bearing) = camera.bearing.filter(|_| camera.bearing != current.bearing) {
            self.state.set_bearing(mln_math::deg2rad(-bearing));
        }
        if let Some(roll) = camera.roll.filter(|_| camera.roll != current.roll) {
            self.state.set_roll(mln_math::deg2rad(roll));
        }
        let pitch = match camera.pitch.filter(|_| camera.pitch != current.pitch) {
            Some(pitch) => mln_math::deg2rad(pitch),
            None => self.state.get_pitch(),
        };
        self.state.set_pitch(std_min(get_max_pitch_for_edge_insets(&self.state, &padding), pitch));
        let mut zoom = camera.zoom.unwrap_or_default();
        self.state.constrain_camera_and_zoom_to_bounds(&mut camera, &mut zoom);
        self.state.set_lat_lng_zoom(&camera.center.unwrap_or_default(), zoom);
        // The anchor had a LatLng at the start; a frame can't return the C++
        // exception, so it is logged, as in frame_edge_insets.
        if let Some(anchored) = anchored
            && let Some(anchor) = anchored.anchor
            && let Err(e) = self.state.move_lat_lng(&anchored.anchor_lat_lng, &anchor)
        {
            Log::error(Event::General, &e.message);
        }
        self.state.set_center_altitude(camera.center_altitude.unwrap_or_default());
    }

    /// Reports the fields the running commands move as the state's panning,
    /// scaling and rotating flags.
    fn update_movement_flags(&mut self) {
        let moving = self.transitions.iter().fold(0, |moving, t| moving | (t.fields.get() & t.moving_fields.get()));
        self.state.set_properties(
            &TransformStateProperties::default()
                .with_panning_in_progress(Some(moving & CENTER != 0))
                .with_scaling_in_progress(Some(moving & ZOOM != 0))
                .with_rotating_in_progress(Some(moving & BEARING != 0)),
        );
    }

    fn finish_transitions(&mut self, finished: &[Rc<Transition>]) {
        for transition in finished {
            self.end_gesture_momentum(transition.gesture);
            if let Some(end_fn) = &transition.animation.transition_end_fn {
                end_fn(&transition.end());
            }
            // This notification ends a command, not necessarily all camera
            // motion.
            self.observer.on_camera_did_change(CameraChange { mode: transition.mode(), source: transition.source });
        }
    }

    /// Whether any camera command is still animating.
    pub fn in_transition(&self) -> bool {
        !self.transitions.is_empty()
    }

    pub fn update_transitions(&mut self, now: TimePoint) {
        if self.transitions.is_empty() {
            return;
        }

        let active = self.transitions.clone();
        let absolute: Vec<_> = active.iter().filter(|t| t.momentum.is_none()).cloned().collect();
        if !absolute.is_empty() {
            self.apply_transitions(&absolute, now);
        }
        for transition in &active {
            if transition.momentum.is_some() {
                self.advance_momentum(transition, now);
            }
        }

        // Detach completed commands before calling user code, so that each
        // command finishes exactly once.
        let mut finished = Vec::new();
        for transition in &active {
            if now - transition.start >= transition.duration.get() || transition.fields.get() == 0 {
                self.transitions.retain(|t| !Rc::ptr_eq(t, transition));
                finished.push(transition.clone());
            }
        }
        self.update_movement_flags();

        for transition in &self.transitions {
            if let Some(frame_fn) = &transition.animation.transition_frame_fn
                && transition.duration.get() != Duration::zero()
            {
                frame_fn(transition.progress(now));
            }
        }
        if !self.transitions.is_empty() {
            self.observer.on_camera_is_changing();
        }

        self.finish_transitions(&finished);
    }

    /// `releaseFields`: takes `fields` from the running commands, and returns
    /// the ones left with no fields, which the caller finishes.
    fn release_fields(&mut self, fields: u16) -> Vec<Rc<Transition>> {
        let mut finished = Vec::new();
        for active in &self.transitions {
            let mut replaced = fields;
            if active.coupled_fields & replaced != 0 {
                replaced |= active.coupled_fields;
            }
            active.fields.set(active.fields.get() & !replaced);
            if let Some(momentum) = &active.momentum {
                active.fields.set(momentum.borrow_mut().retain(active.fields.get()));
            }
            if active.fields.get() == 0 {
                active.ended_by(TransitionOutcome::Replaced);
                finished.push(active.clone());
            }
        }
        self.transitions.retain(|active| active.fields.get() != 0);
        finished
    }

    // MARK: - Momentum

    /// Continues a gesture with momentum. The momentum replaces the running
    /// animations of the fields that it moves, as a camera command does.
    pub fn start_momentum(&mut self, options: &CameraMomentum, animation: &AnimationOptions) {
        self.with_source(CameraChangeSource::Momentum, |transform| {
            transform.start_momentum_now(options, animation, None)
        });
    }

    /// Starts a momentum; `gesture` is the serial of the gesture it
    /// continues, which ends with it.
    fn start_momentum_now(&mut self, options: &CameraMomentum, animation: &AnimationOptions, gesture: Option<u64>) {
        let momentum_change = |mode: CameraChangeMode| CameraChange { mode, source: CameraChangeSource::Momentum };
        let mut momentum = MomentumState {
            options: *options,
            pan_total: ScreenCoordinate::new(0.0, 0.0),
            pan: MomentumPart::default(),
            scalars: [MomentumPart::default(); 3],
            scalar_totals: [0.0; 3],
            replaced: false,
            elapsed: Duration::zero(),
        };
        let mut moving = 0;
        let mut settling = 0;
        let mut motion = Duration::zero();
        if let Some(velocity) = options.pan_velocity {
            let speed = velocity.x.hypot(velocity.y);
            if let Some(glide) = Glide::plan(speed, &options.pan_decay) {
                momentum.pan = MomentumPart { fields: CENTER, glide, moving: true, ..Default::default() };
                moving |= CENTER;
                motion = glide.duration;
                let scale = glide.distance / speed;
                momentum.pan_total = ScreenCoordinate::new(velocity.x * scale, velocity.y * scale);
            }
        }
        let scalars = [
            (options.zoom_velocity, &options.zoom_decay, options.zoom_anchor, options.zoom_snap, ZOOM),
            (options.bearing_velocity, &options.bearing_decay, options.bearing_anchor, options.bearing_snap, BEARING),
            (options.pitch_velocity, &options.pitch_decay, options.pitch_anchor, options.pitch_snap, PITCH),
        ];
        let mut instant = true;
        for (i, (velocity, decay, anchor, snap, field)) in scalars.into_iter().enumerate() {
            let part = &mut momentum.scalars[i];
            part.fields = field | if anchor.is_some() { CENTER } else { 0 };
            if let Some(velocity) = velocity
                && let Some(glide) = Glide::plan(velocity.abs(), decay)
            {
                part.glide = glide;
                part.moving = true;
                moving |= part.fields;
                motion = motion.max(glide.duration);
                momentum.scalar_totals[i] = glide.distance.copysign(velocity);
            }
            // Without motion, a part settles only if it is near a target now.
            part.snap = snap;
            part.snap_pending = snap.is_some_and(|snap| {
                part.moving || snap_delta(momentum_value(&self.state, i), &snap, i == MOMENTUM_BEARING) != 0.0
            });
            if part.snap_pending {
                settling |= part.fields;
                if snap.is_some_and(|snap| snap.duration != Duration::zero()) {
                    instant = false;
                }
            }
        }

        let start = self.now();
        self.latest_transition_start = start;
        self.latest_transition_duration = motion;

        // A momentum that doesn't move ends at once, without a camera change,
        // and instant snaps without motion settle at once.
        if moving == 0 && (settling == 0 || instant) {
            if settling != 0 {
                let finished = self.release_fields(settling);
                self.update_movement_flags();
                self.finish_transitions(&finished);
                self.observer.on_camera_will_change(momentum_change(CameraChangeMode::Immediate));
                for i in 0..momentum.scalars.len() {
                    let part = &mut momentum.scalars[i];
                    let Some(snap) = part.snap.filter(|_| part.snap_pending) else { continue };
                    part.snap_pending = false;
                    let delta = snap_delta(momentum_value(&self.state, i), &snap, i == MOMENTUM_BEARING);
                    self.step_scalar(i, delta, momentum.anchor(i));
                }
            }
            self.end_gesture_momentum(gesture);
            end_unstarted(animation, TransitionOutcome::Completed);
            if settling != 0 {
                self.observer.on_camera_did_change(momentum_change(CameraChangeMode::Immediate));
            }
            return;
        }

        let fields = moving | settling;
        let transition = Rc::new(Transition {
            momentum: Some(RefCell::new(momentum)),
            gesture,
            ..Transition::new(
                fields,
                0,
                moving,
                start,
                motion,
                animation,
                Rc::new(|_, _| CameraOptions::default()),
                CameraChangeSource::Momentum,
            )
        });
        let finished = self.release_fields(fields);
        self.transitions.push(transition);
        self.update_movement_flags();
        self.finish_transitions(&finished);
        // C++ checks here that the finish callbacks didn't replace or cancel
        // the momentum, which they can't in Rust (see the top of the file).
        self.observer.on_camera_will_change(momentum_change(CameraChangeMode::Animated));
    }

    /// Runs `start` with the commands it starts attributed to `source`.
    fn with_source<R>(&mut self, source: CameraChangeSource, start: impl FnOnce(&mut Self) -> R) -> R {
        let previous = std::mem::replace(&mut self.source, source);
        let result = start(self);
        self.source = previous;
        result
    }

    /// Steps the zoom, bearing or pitch part of a momentum.
    fn step_scalar(&mut self, part: usize, amount: f64, anchor: Option<ScreenCoordinate>) -> bool {
        match part {
            MOMENTUM_ZOOM => self.step_zoom(amount, anchor),
            MOMENTUM_BEARING => {
                self.step_bearing(amount, anchor);
                true
            }
            _ => self.step_pitch(amount, anchor),
        }
    }

    fn advance_momentum(&mut self, transition: &Transition, now: TimePoint) {
        let Some(momentum) = &transition.momentum else { return };
        let mut momentum = momentum.borrow_mut();
        let elapsed = now - transition.start;
        momentum.elapsed = elapsed;

        let advance = |part: &mut MomentumPart| -> f64 {
            if !part.moving {
                return 0.0;
            }
            let progress = part.glide.progress(elapsed);
            let fraction = progress - part.applied;
            part.applied = progress;
            if progress >= 1.0 {
                part.moving = false;
            }
            fraction
        };
        // A part that bounds or limits hold in place stops.
        let fraction = advance(&mut momentum.pan);
        if fraction != 0.0 {
            let total = momentum.pan_total;
            if !self.step_pan(ScreenCoordinate::new(total.x * fraction, total.y * fraction)) {
                momentum.pan.moving = false;
            }
        }
        for i in 0..momentum.scalars.len() {
            let anchor = momentum.anchor(i);
            let total = momentum.scalar_totals[i];
            let part = &mut momentum.scalars[i];
            let fraction = advance(part);
            if fraction != 0.0 && !self.step_scalar(i, total * fraction, anchor) {
                part.moving = false;
            }
            // The part settles once it stops moving, which can be before its
            // planned end when bounds, limits, or commands stopped it.
            if part.snap_pending && !part.moving {
                part.snap_pending = false;
                let snap = part.snap.unwrap_or_default();
                let delta = snap_delta(momentum_value(&self.state, i), &snap, i == MOMENTUM_BEARING);
                if delta != 0.0 {
                    part.snap_delta = Some(delta);
                    part.snap_start = elapsed;
                    transition.duration.set(transition.duration.get().max(elapsed + snap.duration));
                    transition.moving_fields.set(transition.moving_fields.get() | part.fields);
                }
            }
            if let Some(delta) = part.snap_delta {
                let duration = part.snap.unwrap_or_default().duration;
                let t = if duration == Duration::zero() {
                    1.0
                } else {
                    mln_math::clamp_f64(seconds_f64(elapsed - part.snap_start) / seconds_f64(duration), 0.0, 1.0)
                };
                let eased = if t >= 1.0 { 1.0 } else { DEFAULT_TRANSITION_EASE.solve(t, 0.001) };
                let fraction = eased - part.snap_applied;
                part.snap_applied = eased;
                let moved = fraction == 0.0 || self.step_scalar(i, delta * fraction, anchor);
                if t >= 1.0 || !moved {
                    part.snap_delta = None;
                }
            }
        }

        // Parts that ran to their end give up their fields.
        transition.fields.set(momentum.retain(transition.fields.get()));
    }

    /// The anchor `anchor` (from the top left) as a state point, from the
    /// bottom left, with the LatLng under it; `None` without one. C++
    /// throws std::domain_error for an anchor that has no LatLng; the port
    /// logs it and ignores the anchor.
    fn momentum_anchor(&self, anchor: Option<ScreenCoordinate>) -> Option<(ScreenCoordinate, LatLng)> {
        let anchor = anchor?;
        let point = ScreenCoordinate::new(anchor.x, self.state.get_size().height as f64 - anchor.y);
        match self.state.screen_coordinate_to_lat_lng(&point, WrapMode::Unwrapped) {
            Ok(lat_lng) => Some((point, lat_lng)),
            Err(e) => {
                Log::error(Event::General, &e.message);
                None
            }
        }
    }

    /// Moves the center so that `lat_lng` is at `point` again, keeping the
    /// altitude in meters, which moving the center keeps in pixels.
    fn hold_momentum_anchor(&mut self, anchor: Option<(ScreenCoordinate, LatLng)>) {
        let Some((point, lat_lng)) = anchor else { return };
        let altitude = self.state.get_center_altitude();
        if let Err(e) = self.state.move_lat_lng(&lat_lng, &point) {
            Log::error(Event::General, &e.message);
        }
        self.state.set_center_altitude(altitude);
    }

    // Momentum steps, applied to the state as it stands. Each returns whether
    // it moved the camera, which bounds and limits can prevent.

    fn step_pan(&mut self, offset: ScreenCoordinate) -> bool {
        // Each step moves the content no farther than a drag moves it in one
        // event, so the travel on screen stays even at high pitch. A frame
        // that would need more steps than a frame can afford takes longer
        // ones.
        const MAX_STEP: f64 = 16.0;
        const MAX_STEPS: f64 = 100.0;
        let distance = offset.x.hypot(offset.y);
        if !(distance > 0.0) {
            return true;
        }
        let before = self.get_lat_lng(WrapMode::Unwrapped);
        let altitude = self.state.get_center_altitude();
        let steps = std_min((distance / MAX_STEP).ceil(), MAX_STEPS);
        let step = ScreenCoordinate::new(offset.x / steps, offset.y / steps);
        let bounded = self.state.get_lat_lng_bounds() != LatLngBounds::default();
        for _ in 0..steps as u32 {
            let size = self.state.get_size();
            let center = self.state.get_edge_insets().get_center(size.width as u16, size.height as u16);
            let point = ScreenCoordinate::new(center.x - step.x, center.y - step.y);
            // C++ throws std::domain_error for a step that has no LatLng; the
            // port logs it and stops the step.
            let unwrapped = match self.screen_coordinate_to_lat_lng(&point, WrapMode::Unwrapped) {
                Ok(lat_lng) => lat_lng,
                Err(e) => {
                    Log::error(Event::General, &e.message);
                    break;
                }
            };
            let mut camera =
                CameraOptions::default().with_center(Some(if bounded { unwrapped } else { unwrapped.wrapped() }));
            let mut zoom = self.state.get_zoom();
            self.state.constrain_camera_and_zoom_to_bounds(&mut camera, &mut zoom);
            self.state.set_lat_lng_zoom(&camera.center.unwrap_or(unwrapped), zoom);
        }
        self.state.set_center_altitude(altitude);
        self.get_lat_lng(WrapMode::Unwrapped) != before
    }

    fn step_zoom(&mut self, zoom: f64, anchor: Option<ScreenCoordinate>) -> bool {
        let before = self.state.get_zoom();
        let altitude = self.state.get_center_altitude();
        let anchor = self.momentum_anchor(anchor);
        let mut target = mln_math::clamp_f64(before + zoom, self.state.get_min_zoom(), self.state.get_max_zoom());
        let center = self.get_lat_lng(WrapMode::Unwrapped);
        let mut camera = CameraOptions::default().with_center(Some(center));
        self.state.constrain_camera_and_zoom_to_bounds(&mut camera, &mut target);
        self.state.set_lat_lng_zoom(&camera.center.unwrap_or(center), target);
        // The altitude in meters holds before the anchor correction, which
        // reads the projection, and through the correction's change of
        // center.
        self.state.set_center_altitude(altitude);
        self.hold_momentum_anchor(anchor);
        self.state.get_zoom() != before
    }

    fn step_bearing(&mut self, bearing: f64, anchor: Option<ScreenCoordinate>) {
        let anchor = self.momentum_anchor(anchor);
        self.state.set_bearing(mln_math::wrap(self.state.get_bearing() - mln_math::deg2rad(bearing), -PI, PI));
        self.hold_momentum_anchor(anchor);
    }

    fn step_pitch(&mut self, pitch: f64, anchor: Option<ScreenCoordinate>) -> bool {
        let anchor = self.momentum_anchor(anchor);
        let before = self.state.get_pitch();
        let max_pitch = get_max_pitch_for_edge_insets(&self.state, &self.state.get_edge_insets());
        self.state.set_pitch(mln_math::clamp_f64(
            before + mln_math::deg2rad(pitch),
            self.state.get_min_pitch(),
            max_pitch,
        ));
        if self.state.get_pitch() == before {
            return false;
        }
        self.hold_momentum_anchor(anchor);
        true
    }

    /// Puts the camera's animations on the host's frame clock: commands start
    /// at the latest frame's time rather than at `Clock::now()` when they are
    /// made, and the map samples them at it. Each frame then shows its
    /// animations exactly one frame further on, whenever the input or the
    /// map's update ran, and an animation started between frames moves by
    /// that frame's interval on the next one: no frame repeats the
    /// camera, and none skips ahead. `None` goes back to `Clock::now()`.
    ///
    /// Orbis starts animations on the first frame after the call, which
    /// keeps the camera still for that frame; a glide that continues a drag
    /// would pause there.
    pub fn set_frame_time(&mut self, time: Option<TimePoint>) {
        self.frame_time = time;
    }

    /// The host's frame time, unless it is stale: older than
    /// [`STALE_FRAME_TIME`], as when the host stopped drawing frames or
    /// forgot to set it this frame. A stale frame time would start a command
    /// in the past, which the next frame would show already under way or
    /// over.
    pub fn frame_time(&self) -> Option<TimePoint> {
        self.fresh_frame_time(Clock::now())
    }

    fn fresh_frame_time(&self, now: TimePoint) -> Option<TimePoint> {
        self.frame_time.filter(|&time| now - time <= STALE_FRAME_TIME)
    }

    /// When commands start: the frame time, or now.
    fn now(&self) -> TimePoint {
        let now = Clock::now();
        self.fresh_frame_time(now).unwrap_or(now)
    }

    /// Timing of the most recently started command, even after it finishes.
    /// Other commands may still be active.
    pub fn get_latest_transition_start(&self) -> TimePoint {
        self.latest_transition_start
    }

    /// See [`get_latest_transition_start`](Self::get_latest_transition_start).
    pub fn get_latest_transition_duration(&self) -> Duration {
        self.latest_transition_duration
    }

    pub fn cancel_transitions(&mut self) {
        let finished = std::mem::take(&mut self.transitions);
        for transition in &finished {
            transition.ended_by(TransitionOutcome::Cancelled);
        }
        self.update_movement_flags();
        self.finish_transitions(&finished);
    }

    /// `cancelTransitions(uint64_t transitionId)`: cancels the commands whose
    /// [`AnimationOptions::transition_id`] equals `transition_id`, leaves the
    /// others running, and returns whether any command ended.
    pub fn cancel_transitions_with_id(&mut self, transition_id: u64) -> bool {
        let (finished, running): (Vec<_>, Vec<_>) = std::mem::take(&mut self.transitions)
            .into_iter()
            .partition(|transition| transition.animation.transition_id == Some(transition_id));
        self.transitions = running;
        if finished.is_empty() {
            return false;
        }
        for transition in &finished {
            transition.ended_by(TransitionOutcome::Cancelled);
        }
        self.update_movement_flags();
        self.finish_transitions(&finished);
        true
    }

    // MARK: - Gesture

    /// Moves the camera relative to where it is, as a gesture does
    /// (maplibre-native-ffi's `mln_map_apply_camera_delta`). See
    /// [`CameraDelta`].
    pub fn apply_camera_delta(&mut self, delta: &CameraDelta) -> Result<(), CameraError> {
        delta.validate()?;
        let source = gesture_source(delta.phase);
        let pans = delta.offset.is_some();
        let eases = delta.scale.is_some() || delta.bearing.is_some() || delta.pitch.is_some();
        // With momentum, the command's end is the momentum's, and the delta's
        // own changes, which can't animate, apply before it starts.
        // Otherwise the pan and the ease end the command together.
        let parts = match delta.momentum {
            Some(_) => AnimationOptions::default(),
            None => joint_end(&delta.animation, u32::from(pans) + u32::from(eases)),
        };
        self.gesture_phase_before(delta.phase);
        if delta.phase != GesturePhase::None {
            self.gesture.change(
                delta.scale.is_some_and(|scale| scale != 1.0),
                delta.bearing.is_some_and(|bearing| bearing != 0.0),
                delta.pitch.is_some_and(|pitch| pitch != 0.0),
            );
        }
        self.with_source(source, |transform| {
            if let Some(offset) = delta.offset {
                // Validation keeps the offset finite, so it has a LatLng.
                let _ = transform.move_by(&offset, &parts);
            }
            if eases {
                let current = transform.get_camera_options(None);
                let target = CameraOptions {
                    zoom: delta.scale.map(|scale| current.zoom.unwrap_or(0.0) + scale.log2()),
                    bearing: delta.bearing.map(|bearing| current.bearing.unwrap_or(0.0) + bearing),
                    pitch: delta.pitch.map(|pitch| current.pitch.unwrap_or(0.0) + pitch),
                    anchor: delta.anchor,
                    ..CameraOptions::default()
                };
                transform.ease_to(&target, &parts);
            }
        });
        match delta.momentum {
            Some(mut momentum) => {
                self.gesture.mask_snaps(&mut momentum);
                // The gesture ends as its momentum starts, rather than in
                // `gesture_phase_after`, so that a momentum with nothing to
                // move, which ends at once, ends it too.
                self.gesture.active = false;
                let serial = self.gesture.serial;
                self.with_source(CameraChangeSource::Momentum, |transform| {
                    transform.start_momentum_now(&momentum, &delta.animation, Some(serial))
                });
            }
            None if !pans && !eases => end_unstarted(&delta.animation, TransitionOutcome::Completed),
            None => {}
        }
        self.gesture_phase_after(delta.phase, delta.momentum.is_some());
        Ok(())
    }

    /// Moves the camera to absolute values, as a command or a gesture
    /// (maplibre-native-ffi's `mln_map_update_camera`). See [`CameraUpdate`].
    pub fn update_camera(&mut self, update: &CameraUpdate) {
        self.gesture_phase_before(update.phase);
        if update.phase != GesturePhase::None {
            // A gesture's update can repeat a field it doesn't change.
            let current = self.get_camera_options(None);
            let changes = |value: Option<f64>, now: Option<f64>| value.is_some() && value != now;
            self.gesture.change(
                changes(update.camera.zoom, current.zoom),
                changes(update.camera.bearing, current.bearing),
                changes(update.camera.pitch, current.pitch),
            );
        }
        self.with_source(gesture_source(update.phase), |transform| match update.mode {
            CameraUpdateMode::Jump => {
                transform.ease_to(&update.camera, &AnimationOptions { duration: None, ..update.animation.clone() })
            }
            CameraUpdateMode::Ease => transform.ease_to(&update.camera, &update.animation),
            CameraUpdateMode::Fly => transform.fly_to(&update.camera, &update.animation, false),
        });
        self.gesture_phase_after(update.phase, false);
    }

    /// `Begin` cancels the running commands, and `Begin` and `Update` mark
    /// the gesture, before its change applies, so that the change follows
    /// the gesture rules (an ease doesn't fly, and wraps the longitude as a
    /// drag). The first phase after a gesture ended starts a new one, which
    /// an earlier gesture's momentum no longer ends.
    fn gesture_phase_before(&mut self, phase: GesturePhase) {
        if phase != GesturePhase::None && (phase == GesturePhase::Begin || !self.gesture.active) {
            self.gesture = GestureState { serial: self.gesture.serial.wrapping_add(1), ..GestureState::default() };
        }
        if phase == GesturePhase::Begin {
            self.cancel_transitions();
        }
        if matches!(phase, GesturePhase::Begin | GesturePhase::Update) {
            self.gesture.active = true;
            self.set_gesture_in_progress(true);
        }
    }

    /// `Cancel` cancels the running commands after its change applies, and
    /// `End` and `Cancel` end the gesture: at once, or when its momentum
    /// ends.
    fn gesture_phase_after(&mut self, phase: GesturePhase, momentum: bool) {
        if phase == GesturePhase::Cancel {
            self.cancel_transitions();
        }
        if matches!(phase, GesturePhase::End | GesturePhase::Cancel) {
            self.gesture.active = false;
            if !momentum {
                self.set_gesture_in_progress(false);
            }
        }
    }

    /// The end of a gesture's momentum ends the gesture, unless another
    /// gesture started since.
    fn end_gesture_momentum(&mut self, gesture: Option<u64>) {
        if gesture == Some(self.gesture.serial) && !self.gesture.active {
            self.set_gesture_in_progress(false);
        }
    }

    pub fn set_gesture_in_progress(&mut self, in_progress: bool) {
        self.state.set_gesture_in_progress(in_progress);
    }

    pub fn is_gesture_in_progress(&self) -> bool {
        self.state.is_gesture_in_progress()
    }

    // MARK: - Transform state

    pub fn get_state(&self) -> &TransformState {
        &self.state
    }

    pub fn is_rotating(&self) -> bool {
        self.state.is_rotating()
    }

    pub fn is_scaling(&self) -> bool {
        self.state.is_scaling()
    }

    pub fn is_panning(&self) -> bool {
        self.state.is_panning()
    }

    // MARK: Conversion and projection

    pub fn lat_lng_to_screen_coordinate(&self, lat_lng: &LatLng) -> ScreenCoordinate {
        let mut point = self.state.lat_lng_to_screen_coordinate(lat_lng);
        point.y = self.state.get_size().height as f64 - point.y;
        point
    }

    /// `screenCoordinateToLatLng(const ScreenCoordinate&, LatLng::WrapMode = LatLng::Wrapped)`.
    /// Fails with [`ErrorKind::InvalidArgument`](mln_util::ErrorKind) for a NaN
    /// or infinite point.
    pub fn screen_coordinate_to_lat_lng(&self, point: &ScreenCoordinate, wrap_mode: WrapMode) -> Result<LatLng, Error> {
        let mut flipped_point = *point;
        flipped_point.y = self.state.get_size().height as f64 - flipped_point.y;
        self.state.screen_coordinate_to_lat_lng(&flipped_point, wrap_mode)
    }

    pub fn get_free_camera_options(&self) -> FreeCameraOptions {
        self.state.get_free_camera_options()
    }

    pub fn set_free_camera_options(&mut self, options: &FreeCameraOptions) {
        self.cancel_transitions();
        self.state.set_free_camera_options(options);
    }
}

impl Drop for Transform {
    /// Ends the running commands as closed, as maplibre-native-ffi does when
    /// a map closes, so that every command's end function runs once.
    fn drop(&mut self) {
        for transition in std::mem::take(&mut self.transitions) {
            if let Some(end_fn) = &transition.animation.transition_end_fn {
                end_fn(&TransitionEnd { outcome: TransitionOutcome::Closed, ..transition.end() });
            }
        }
    }
}

fn gesture_source(phase: GesturePhase) -> CameraChangeSource {
    if phase == GesturePhase::None { CameraChangeSource::Command } else { CameraChangeSource::Gesture }
}

/// Animation options for the `count` commands that one camera delta starts,
/// whose end function runs once, when the last of them ends, with the
/// outcome that covers them all: cancelled if any was, otherwise closed if
/// any was, otherwise replaced if any was (maplibre-native-ffi's
/// `CommandTransitions`).
fn joint_end(animation: &AnimationOptions, count: u32) -> AnimationOptions {
    let Some(end_fn) = animation.transition_end_fn.clone().filter(|_| count > 1) else {
        return animation.clone();
    };
    // The remaining commands and the outcome so far.
    let state = SyncCell::new((count, TransitionOutcome::Completed));
    let joint = move |end: &TransitionEnd| {
        let (remaining, outcome) = state.get();
        let worst = match (outcome, end.outcome) {
            (TransitionOutcome::Cancelled, _) | (_, TransitionOutcome::Cancelled) => TransitionOutcome::Cancelled,
            (TransitionOutcome::Closed, _) | (_, TransitionOutcome::Closed) => TransitionOutcome::Closed,
            (TransitionOutcome::Replaced, _) | (_, TransitionOutcome::Replaced) => TransitionOutcome::Replaced,
            _ => TransitionOutcome::Completed,
        };
        state.set((remaining - 1, worst));
        if remaining == 1 {
            end_fn(&TransitionEnd { outcome: worst, ..*end });
        }
    };
    AnimationOptions { transition_end_fn: Some(Arc::new(joint)), ..animation.clone() }
}

/// Ends a command that started no transition: one that failed before it
/// moved the camera, or a momentum with nothing to move.
fn end_unstarted(animation: &AnimationOptions, outcome: TransitionOutcome) {
    if let Some(end_fn) = &animation.transition_end_fn {
        end_fn(&TransitionEnd { outcome, ..TransitionEnd::default() });
    }
}

/// An animation frame's padding, `t` of the way from `start` to `end`; `None`
/// when it has none (see [`frame_edge_insets`]). C++ interpolates the insets
/// even when they don't change; the port keeps them, so that infinite insets
/// don't log on every frame.
fn interpolate_edge_insets(start: &EdgeInsets, end: &EdgeInsets, t: f64) -> Option<EdgeInsets> {
    if start == end {
        return Some(*end);
    }
    frame_edge_insets(
        interpolate(&start.top, &end.top, t),
        interpolate(&start.left, &end.left, t),
        interpolate(&start.bottom, &end.bottom, t),
        interpolate(&start.right, &end.right, t),
    )
}

/// We don't want to show horizon: limit max pitch based on edge insets.
/// (`Transform::getMaxPitchForEdgeInsets`, called on the state inside the
/// frame function.)
fn get_max_pitch_for_edge_insets(state: &TransformState, insets: &EdgeInsets) -> f64 {
    // See TransformState::getCenterOffset.
    let center_offset_y = 0.5 * (insets.top - insets.bottom);
    if center_offset_y == 0.0 {
        return state.get_max_pitch();
    }

    let height = state.get_size().height;
    debug_assert!(height != 0);
    let field_of_view = state.get_field_of_view() as f64;
    // Half of fov is the field of view above perspective center.
    let tangent_of_fov_above_center_angle = (0.5 + center_offset_y / height as f64) * 2.0 * (field_of_view / 2.0).tan();
    let fov_above_center = tangent_of_fov_above_center_angle.atan();
    std_max(state.get_min_pitch(), state.get_max_pitch() + field_of_view / 2.0 - fov_above_center)
}

#[cfg(test)]
#[path = "transform_test.rs"]
mod transform_test;
