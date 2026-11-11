// Ports include/mln/map/camera.hpp (CameraOptions, AnimationOptions,
// FreeCameraOptions; the FreeCameraOptions methods are in util/camera.cpp,
// see util_camera.rs).

use mln_util::UnitBezier;
use mln_util::chrono::Duration;
use mln_util::geo::{EdgeInsets, LatLng, ScreenCoordinate};
use mln_util::vectors::{Vec3, Vec4};

/// Various options for describing the viewpoint of a map. All fields are
/// optional.
/// A partial command replaces animations of its specified fields. Omitted
/// fields keep their original animation timing. Flights couple center and
/// zoom; anchored commands couple center with their specified fields.
/// Replacing any member of a coupled group stops that group.
/// Anchor and center points are mutually exclusive, with preference for the
/// center point when both are set.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CameraOptions {
    /// Coordinate at the center of the map.
    pub center: Option<LatLng>,
    /// Altitude of the center of the map, in meters above sea level.
    pub center_altitude: Option<f64>,
    /// Padding around the interior of the view that affects the frame of
    /// reference for `center`.
    pub padding: Option<EdgeInsets>,
    /// Point of reference for `zoom` and `angle`, assuming an origin at the
    /// top-left corner of the view.
    pub anchor: Option<ScreenCoordinate>,
    /// Zero-based zoom level. Constrained to the minimum and maximum zoom
    /// levels.
    pub zoom: Option<f64>,
    /// Bearing, measured in degrees from true north. Wrapped to [0, 360).
    pub bearing: Option<f64>,
    /// Pitch toward the horizon measured in degrees, with 0 deg resulting in
    /// a two-dimensional map.
    pub pitch: Option<f64>,
    /// Roll, measured in degrees.
    pub roll: Option<f64>,
    /// Field of view, measured in degrees.
    pub fov: Option<f64>,
}

impl CameraOptions {
    pub fn with_center(mut self, o: Option<LatLng>) -> Self {
        self.center = o;
        self
    }
    pub fn with_center_altitude(mut self, o: Option<f64>) -> Self {
        self.center_altitude = o;
        self
    }
    pub fn with_padding(mut self, p: Option<EdgeInsets>) -> Self {
        self.padding = p;
        self
    }
    pub fn with_anchor(mut self, o: Option<ScreenCoordinate>) -> Self {
        self.anchor = o;
        self
    }
    pub fn with_zoom(mut self, o: Option<f64>) -> Self {
        self.zoom = o;
        self
    }
    pub fn with_bearing(mut self, o: Option<f64>) -> Self {
        self.bearing = o;
        self
    }
    pub fn with_pitch(mut self, o: Option<f64>) -> Self {
        self.pitch = o;
        self
    }
    pub fn with_roll(mut self, o: Option<f64>) -> Self {
        self.roll = o;
        self
    }
    pub fn with_fov(mut self, o: Option<f64>) -> Self {
        self.fov = o;
        self
    }
}

/// [`AnimationOptions::transition_frame_fn`].
pub type TransitionFrameFn = std::sync::Arc<dyn Fn(f64) + Send + Sync>;

/// [`AnimationOptions::transition_end_fn`].
pub type TransitionEndFn = std::sync::Arc<dyn Fn(&TransitionEnd) + Send + Sync>;

/// Various options for describing a transition between viewpoints with
/// animation. All fields are optional; the default values depend on how
/// this struct is used.
///
/// The callbacks are shared so the struct can be copied into the transition
/// closures, as the C++ lambdas copy their `std::function`s, and are `Send +
/// Sync` so a command can cross to the thread of the map it moves. They run
/// on that thread.
#[derive(Clone, Default)]
pub struct AnimationOptions {
    /// Time to animate to the viewpoint defined herein.
    pub duration: Option<Duration>,
    /// Average velocity of a flyTo() transition, measured in screenfuls per
    /// second, assuming a linear timing curve.
    pub velocity: Option<f64>,
    /// Zero-based zoom level at the peak of the flyTo() transition's flight
    /// path.
    pub min_zoom: Option<f64>,
    /// The longest a flyTo() transition may take, as GL JS's `maxDuration`:
    /// a flight whose duration, set or derived from the velocity, exceeds it
    /// jumps instead. `None` caps nothing beyond what [`Duration`] holds: a
    /// derived duration it can't hold jumps too. Other camera commands
    /// ignore it.
    pub max_duration: Option<Duration>,
    /// The easing timing curve of the transition.
    pub easing: Option<UnitBezier>,
    /// A function that is called on each frame of the transition, just before
    /// a screen update, except on the last frame. The first parameter
    /// indicates the elapsed time as a percentage of the duration.
    pub transition_frame_fn: Option<TransitionFrameFn>,
    /// Called once when every field of the command has ended, with how it
    /// ended: completed, cancelled, replaced by later commands, or closed with
    /// the map. Other commands may still be animating when this callback runs.
    /// It replaces C++'s `transitionFinishFn`, which can't tell those apart;
    /// maplibre-native-ffi's C API reports the same outcomes.
    pub transition_end_fn: Option<TransitionEndFn>,
    /// Caller-chosen identity of the command, which
    /// `Map::cancel_transitions_with_id` matches. Several commands may share
    /// one.
    pub transition_id: Option<u64>,
}

/// How a camera command ended, as maplibre-native-ffi's
/// `mln_camera_transition_outcome` reports it. A command that ended in more
/// than one way reports [`Cancelled`](Self::Cancelled) if any of it was
/// cancelled, otherwise [`Closed`](Self::Closed) if any of it was closed,
/// and otherwise [`Replaced`](Self::Replaced) if any of it was replaced.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum TransitionOutcome {
    /// The camera finished the change.
    #[default]
    Completed,
    /// `cancel_transitions` or `cancel_transitions_with_id`, a gesture's
    /// begin or cancel phase, free camera options, or a resize that moved the
    /// camera back inside its bounds stopped it. A command that fails before
    /// it moves the camera also reports this.
    Cancelled,
    /// A later command took every field that the command (or a momentum part)
    /// was still animating, so it stopped. A later command that takes only
    /// some of them leaves it animating the rest.
    Replaced,
    /// The map closed before the change ended.
    Closed,
}

/// How fast each part of a camera change moves, in its own unit per second.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CameraVelocity {
    /// Logical pixels per second that the map's content moves on screen.
    pub pan: ScreenCoordinate,
    /// Zoom levels per second.
    pub zoom: f64,
    /// Degrees per second.
    pub bearing: f64,
    /// Degrees per second.
    pub pitch: f64,
}

/// The end of a camera command
/// ([`AnimationOptions::transition_end_fn`]).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TransitionEnd {
    pub outcome: TransitionOutcome,
    /// How fast a momentum still moved when it was cancelled, as of the last
    /// frame that moved the camera, which the next gesture can add to its own
    /// release. Zero for every other end.
    pub velocity: CameraVelocity,
    /// The generation of the first snapshot of the map that shows where the
    /// change left the camera, as `mln_runtime`'s map handles report it. 0
    /// when the map closed first, and from the map itself, which ends a
    /// change before it counts the change's generation.
    pub generation: u64,
}

impl AnimationOptions {
    /// `AnimationOptions(Duration d)`.
    pub fn new(d: Duration) -> Self {
        AnimationOptions { duration: Some(d), ..Default::default() }
    }
}

/// Various options for accessing physical properties of the underlying camera
/// entity. A direct access to these properties allows more flexible and
/// precise controlling of the camera while also being fully compatible and
/// interchangeable with CameraOptions. All fields are optional.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FreeCameraOptions {
    /// Position of the camera in slightly modified web mercator coordinates.
    /// The size of 1 unit is the width of the projected world instead of the
    /// "mercator meter". Coordinate [0, 0, 0] is the north-west corner and
    /// [1, 1, 0] is the south-east corner. Z coordinate is conformal and must
    /// respect minimum and maximum zoom values. Zoom is automatically
    /// computed from the altitude (z).
    pub position: Option<Vec3>,
    /// Orientation of the camera represented as a unit quaternion [x, y, z,
    /// w].
    pub orientation: Option<Vec4>,
}

/// Where a camera change stands in a gesture, as maplibre-native-ffi's
/// `mln_gesture_phase` defines it. `Begin` and `Update` mark a gesture in
/// progress, and `End` and `Cancel` end it; a gesture whose end starts
/// momentum stays in progress until the momentum ends.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum GesturePhase {
    /// The change isn't part of a gesture: a command.
    #[default]
    None,
    /// The gesture starts. This cancels every running camera animation
    /// before the change applies.
    Begin,
    /// The gesture continues. To start a gesture that keeps the running
    /// animations, start it with this phase instead of `Begin`.
    Update,
    /// The gesture's last change, which may carry its momentum.
    End,
    /// The platform cancelled the gesture. This also cancels every running
    /// camera animation after the change applies.
    Cancel,
}

/// A change to the camera from where it is now, as a gesture makes it
/// (maplibre-native-ffi's `mln_camera_delta`).
///
/// The pan and the anchored change are separate camera commands, because an
/// explicit center overrides an anchor; the anchored change reads the camera
/// after the pan. Both end through the one
/// [`AnimationOptions::transition_end_fn`], once both have ended.
#[derive(Clone, Default)]
pub struct CameraDelta {
    /// Moves the map's content this far on the screen, as a drag does.
    pub offset: Option<ScreenCoordinate>,
    /// Multiplies the map's scale: 2 zooms in by one level.
    pub scale: Option<f64>,
    /// Degrees to add to the bearing.
    pub bearing: Option<f64>,
    /// Degrees to add to the pitch.
    pub pitch: Option<f64>,
    /// The screen point that `scale`, `bearing` and `pitch` keep in place.
    /// It needs one of them, and it combines with `offset` only in a delta
    /// without a duration.
    pub anchor: Option<ScreenCoordinate>,
    /// How the gesture keeps moving after this delta, which only a delta in
    /// the [`GesturePhase::End`] phase carries. Its parts' snaps apply only
    /// to the parts the gesture changed. With momentum, `animation` may set
    /// only its transition id and end function, and the command ends when
    /// the momentum does.
    pub momentum: Option<crate::CameraMomentum>,
    pub animation: AnimationOptions,
    pub phase: GesturePhase,
}

/// How [`CameraUpdate`] moves the camera.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CameraUpdateMode {
    /// At once; the animation's duration is ignored.
    #[default]
    Jump,
    Ease,
    Fly,
}

/// Moves the camera to absolute values, as maplibre-native-ffi's
/// `mln_camera_update` does: the counterpart of [`CameraDelta`] for
/// commands and gestures that know where the camera should go.
#[derive(Clone, Default)]
pub struct CameraUpdate {
    pub mode: CameraUpdateMode,
    pub camera: CameraOptions,
    pub animation: AnimationOptions,
    pub phase: GesturePhase,
}

/// Why a [`CameraDelta`] was rejected, as maplibre-native-ffi rejects it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CameraError {
    /// An offset, anchor, bearing or pitch isn't finite.
    NotFinite,
    /// The scale isn't finite and positive.
    InvalidScale,
    /// An anchor needs a scale, bearing or pitch to hold in place.
    AnchorWithoutChange,
    /// Only a delta in the end phase carries momentum.
    MomentumOutsideEnd,
    /// A delta with momentum sets only its animation's transition id and end
    /// function: the momentum, not an animation, moves the camera.
    MomentumWithAnimation,
    /// An anchor combines with an offset only in an immediate delta: an
    /// anchored ease couples the center with its fields, so it would replace
    /// an animated pan.
    AnimatedAnchoredOffset,
}

impl std::fmt::Display for CameraError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            CameraError::NotFinite => "camera delta offset, anchor, bearing and pitch must be finite",
            CameraError::InvalidScale => "camera delta scale must be finite and positive",
            CameraError::AnchorWithoutChange => "a camera delta anchor requires scale, bearing, or pitch",
            CameraError::MomentumOutsideEnd => "only a camera delta in the end phase carries momentum",
            CameraError::MomentumWithAnimation => {
                "a camera delta with momentum sets only the transition ID and end function of its animation"
            }
            CameraError::AnimatedAnchoredOffset => {
                "a camera delta anchor combines with offset only in an immediate delta"
            }
        })
    }
}

impl std::error::Error for CameraError {}

impl CameraDelta {
    /// Checks the delta as maplibre-native-ffi's `validate_camera_delta`
    /// does.
    pub fn validate(&self) -> Result<(), CameraError> {
        let finite_point = |p: Option<ScreenCoordinate>| p.is_none_or(|p| p.x.is_finite() && p.y.is_finite());
        if !finite_point(self.offset)
            || !finite_point(self.anchor)
            || !self.bearing.is_none_or(f64::is_finite)
            || !self.pitch.is_none_or(f64::is_finite)
        {
            return Err(CameraError::NotFinite);
        }
        if self.scale.is_some_and(|scale| !scale.is_finite() || scale <= 0.0) {
            return Err(CameraError::InvalidScale);
        }
        let anchored = self.scale.is_some() || self.bearing.is_some() || self.pitch.is_some();
        if self.anchor.is_some() && !anchored {
            return Err(CameraError::AnchorWithoutChange);
        }
        if self.momentum.is_some() {
            if self.phase != GesturePhase::End {
                return Err(CameraError::MomentumOutsideEnd);
            }
            let a = &self.animation;
            if a.duration.is_some()
                || a.velocity.is_some()
                || a.min_zoom.is_some()
                || a.max_duration.is_some()
                || a.easing.is_some()
                || a.transition_frame_fn.is_some()
            {
                return Err(CameraError::MomentumWithAnimation);
            }
        }
        if self.offset.is_some()
            && self.anchor.is_some()
            && self.animation.duration.is_some_and(|duration| duration > Duration::zero())
        {
            return Err(CameraError::AnimatedAnchoredOffset);
        }
        Ok(())
    }
}
