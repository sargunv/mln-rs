//! A gesture's momentum, as `mln_camera_delta.momentum` gives it: each
//! setting whose bit is set replaces the part's default.

use ::mln_map::{CameraMomentum, MomentumDecay, MomentumDecayModel, MomentumEasing, MomentumSnap};

use super::{duration, is_duration_ms};
use crate::abi::Fields;
use crate::error::Error;
use crate::generated::types::{
    MLN_ANCHORED_MOMENTUM_ANCHOR, MLN_ANCHORED_MOMENTUM_SNAP, MLN_ANCHORED_MOMENTUM_VELOCITY,
    MLN_MOMENTUM_DECAY_DISTANCE, MLN_MOMENTUM_DECAY_DURATION_MS, MLN_MOMENTUM_DECAY_EASING,
    MLN_MOMENTUM_DECAY_HALF_LIFE_MS, MLN_MOMENTUM_DECAY_MAX_DISTANCE, MLN_MOMENTUM_DECAY_MAX_SPEED,
    MLN_MOMENTUM_DECAY_MIN_SPEED, MLN_MOMENTUM_DECAY_MODEL, MLN_MOMENTUM_DECAY_MODEL_EASE,
    MLN_MOMENTUM_DECAY_MODEL_FRICTION, MLN_MOMENTUM_DECAY_STOP_SPEED, MLN_MOMENTUM_SNAP_DURATION_MS,
    MLN_MOMENTUM_SNAP_OFFSET, MLN_MOMENTUM_SNAP_SPACING, MLN_MOMENTUM_SNAP_TOLERANCE, MLN_PAN_MOMENTUM_VELOCITY,
    mln_anchored_momentum, mln_gesture_momentum, mln_momentum_decay, mln_momentum_snap,
};
use crate::geo;

/// A number that is finite and greater than 0.
fn positive(value: f64) -> bool {
    value.is_finite() && value > 0.0
}

/// `options` over the part's `decay`. `part` names the part in diagnostics.
fn decay(options: &mln_momentum_decay, mut decay: MomentumDecay, part: &str) -> Result<MomentumDecay, Error> {
    let fail = |problem: &str| Error::invalid_argument(format!("the momentum decay of {part} {problem}"));
    let known = MLN_MOMENTUM_DECAY_MODEL
        | MLN_MOMENTUM_DECAY_HALF_LIFE_MS
        | MLN_MOMENTUM_DECAY_STOP_SPEED
        | MLN_MOMENTUM_DECAY_DISTANCE
        | MLN_MOMENTUM_DECAY_DURATION_MS
        | MLN_MOMENTUM_DECAY_EASING
        | MLN_MOMENTUM_DECAY_MIN_SPEED
        | MLN_MOMENTUM_DECAY_MAX_SPEED
        | MLN_MOMENTUM_DECAY_MAX_DISTANCE;
    if options.fields & !known != 0 {
        return Err(fail("contains unknown bits"));
    }
    let has = |bit: u32| options.fields & bit != 0;
    if has(MLN_MOMENTUM_DECAY_MODEL) {
        decay.model = match options.model {
            MLN_MOMENTUM_DECAY_MODEL_FRICTION => MomentumDecayModel::Friction,
            MLN_MOMENTUM_DECAY_MODEL_EASE => MomentumDecayModel::Ease,
            _ => return Err(fail("has an invalid model")),
        };
    }
    if has(MLN_MOMENTUM_DECAY_HALF_LIFE_MS) {
        if !positive(options.half_life_ms) || !is_duration_ms(options.half_life_ms) {
            return Err(fail("needs a positive half-life in the native duration range"));
        }
        decay.half_life = duration(options.half_life_ms);
    }
    if (has(MLN_MOMENTUM_DECAY_STOP_SPEED) && !positive(options.stop_speed))
        || (has(MLN_MOMENTUM_DECAY_DISTANCE) && !positive(options.distance))
        || (has(MLN_MOMENTUM_DECAY_MIN_SPEED) && !(options.min_speed.is_finite() && options.min_speed >= 0.0))
        || (has(MLN_MOMENTUM_DECAY_MAX_SPEED) && !positive(options.max_speed))
        || (has(MLN_MOMENTUM_DECAY_MAX_DISTANCE) && !positive(options.max_distance))
    {
        return Err(fail("needs a finite minimum speed that isn't negative, and finite positive values for the rest"));
    }
    if has(MLN_MOMENTUM_DECAY_STOP_SPEED) {
        decay.stop_speed = options.stop_speed;
    }
    if decay.model == MomentumDecayModel::Ease {
        if !has(MLN_MOMENTUM_DECAY_DISTANCE) || !has(MLN_MOMENTUM_DECAY_DURATION_MS) {
            return Err(fail("needs a distance and a duration for an eased glide"));
        }
        if !positive(options.duration_ms) || !is_duration_ms(options.duration_ms) {
            return Err(fail("needs a positive duration in the native duration range"));
        }
        if has(MLN_MOMENTUM_DECAY_EASING) {
            let curve = options.easing;
            if ![curve.x1, curve.y1, curve.x2, curve.y2].iter().all(|value| (0.0..=1.0).contains(value)) {
                return Err(fail("needs an easing whose control points are within [0, 1]"));
            }
            decay.easing = MomentumEasing { x1: curve.x1, y1: curve.y1, x2: curve.x2, y2: curve.y2 };
        }
        decay.distance = options.distance;
        decay.duration = duration(options.duration_ms);
    }
    if has(MLN_MOMENTUM_DECAY_MIN_SPEED) {
        decay.min_speed = options.min_speed;
    }
    if has(MLN_MOMENTUM_DECAY_MAX_SPEED) {
        decay.max_speed = options.max_speed;
    }
    if has(MLN_MOMENTUM_DECAY_MAX_DISTANCE) {
        decay.max_distance = options.max_distance;
    }
    Ok(decay)
}

/// `options` over the part's `snap`.
fn snap(options: &mln_momentum_snap, mut snap: MomentumSnap) -> Result<MomentumSnap, Error> {
    let fields = Fields::new(
        options.fields,
        MLN_MOMENTUM_SNAP_SPACING
            | MLN_MOMENTUM_SNAP_OFFSET
            | MLN_MOMENTUM_SNAP_TOLERANCE
            | MLN_MOMENTUM_SNAP_DURATION_MS,
        "mln_momentum_snap.fields",
    )?;
    snap.spacing = fields.get(MLN_MOMENTUM_SNAP_SPACING, options.spacing).unwrap_or(snap.spacing);
    snap.offset = fields.get(MLN_MOMENTUM_SNAP_OFFSET, options.offset).unwrap_or(snap.offset);
    snap.tolerance = fields.get(MLN_MOMENTUM_SNAP_TOLERANCE, options.tolerance).unwrap_or(snap.tolerance);
    let duration_ms = fields.get(MLN_MOMENTUM_SNAP_DURATION_MS, options.duration_ms);
    if !positive(snap.spacing)
        || !snap.offset.is_finite()
        || !(snap.tolerance.is_finite() && snap.tolerance >= 0.0)
        || !duration_ms.is_none_or(is_duration_ms)
    {
        return Err(Error::invalid_argument(
            "a momentum snap needs a positive spacing, a finite offset, a tolerance that isn't negative, and a \
             duration in the native range",
        ));
    }
    if let Some(duration_ms) = duration_ms {
        snap.duration = duration(duration_ms);
    }
    Ok(snap)
}

/// A zoom, bearing or pitch part: its velocity, anchor, decay and snap.
type Anchored = (Option<f64>, Option<mln_util::geo::ScreenCoordinate>, MomentumDecay, Option<MomentumSnap>);

fn anchored(
    part: &mln_anchored_momentum,
    defaults: MomentumDecay,
    snap_defaults: MomentumSnap,
    name: &str,
) -> Result<Anchored, Error> {
    let fields = Fields::new(
        part.fields,
        MLN_ANCHORED_MOMENTUM_VELOCITY | MLN_ANCHORED_MOMENTUM_ANCHOR | MLN_ANCHORED_MOMENTUM_SNAP,
        "mln_anchored_momentum.fields",
    )?;
    let velocity = fields.get(MLN_ANCHORED_MOMENTUM_VELOCITY, part.velocity);
    if velocity.is_some_and(|velocity| !velocity.is_finite()) {
        return Err(Error::invalid_argument("gesture momentum velocities must be finite"));
    }
    let anchor = fields.get(MLN_ANCHORED_MOMENTUM_ANCHOR, part.anchor).map(geo::screen_point).transpose()?;
    let decay = decay(&part.decay, defaults, name)?;
    let snap =
        fields.get(MLN_ANCHORED_MOMENTUM_SNAP, &part.snap).map(|options| snap(options, snap_defaults)).transpose()?;
    Ok((velocity, anchor, decay, snap))
}

/// The momentum that `momentum` sets over the header's defaults.
pub(super) fn momentum(momentum: &mln_gesture_momentum) -> Result<CameraMomentum, Error> {
    let pan = &momentum.pan;
    let pan_fields = Fields::new(pan.fields, MLN_PAN_MOMENTUM_VELOCITY, "mln_pan_momentum.fields")?;
    let pan_velocity = pan_fields.get(MLN_PAN_MOMENTUM_VELOCITY, pan.velocity).map(geo::screen_point).transpose()?;
    let pan_decay = decay(&pan.decay, MomentumDecay::PAN, "pan")?;
    let (zoom_velocity, zoom_anchor, zoom_decay, zoom_snap) =
        anchored(&momentum.zoom, MomentumDecay::ZOOM, MomentumSnap::ZOOM, "zoom")?;
    let (bearing_velocity, bearing_anchor, bearing_decay, bearing_snap) =
        anchored(&momentum.bearing, MomentumDecay::BEARING, MomentumSnap::BEARING, "bearing")?;
    let (pitch_velocity, pitch_anchor, pitch_decay, pitch_snap) =
        anchored(&momentum.pitch, MomentumDecay::PITCH, MomentumSnap::PITCH, "pitch")?;
    Ok(CameraMomentum {
        pan_velocity,
        pan_decay,
        zoom_velocity,
        zoom_decay,
        zoom_anchor,
        zoom_snap,
        bearing_velocity,
        bearing_decay,
        bearing_anchor,
        bearing_snap,
        pitch_velocity,
        pitch_decay,
        pitch_anchor,
        pitch_snap,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::types::{mln_pan_momentum, mln_screen_point, mln_unit_bezier};

    fn zeroed<T>() -> T {
        // SAFETY: the headers' momentum structs hold numbers only, for which
        // zeros are valid.
        unsafe { core::mem::zeroed() }
    }

    #[test]
    fn unset_settings_take_each_parts_defaults() {
        let resolved = momentum(&zeroed()).unwrap();
        assert_eq!(resolved, CameraMomentum::default());

        let mut options: mln_gesture_momentum = zeroed();
        options.pan = mln_pan_momentum {
            fields: MLN_PAN_MOMENTUM_VELOCITY,
            velocity: mln_screen_point { x: 300.0, y: 0.0 },
            decay: mln_momentum_decay { fields: MLN_MOMENTUM_DECAY_HALF_LIFE_MS, half_life_ms: 346.0, ..zeroed() },
        };
        options.zoom.fields = MLN_ANCHORED_MOMENTUM_SNAP;
        let resolved = momentum(&options).unwrap();
        assert_eq!(resolved.pan_velocity.map(|velocity| velocity.x), Some(300.0));
        assert_eq!(resolved.pan_decay, MomentumDecay { half_life: duration(346.0), ..MomentumDecay::PAN });
        assert_eq!(resolved.zoom_snap, Some(MomentumSnap::ZOOM));
        assert_eq!(resolved.bearing_snap, None);
    }

    #[test]
    fn an_eased_glide_needs_its_plan() {
        let mut options: mln_gesture_momentum = zeroed();
        options.zoom.decay.fields = MLN_MOMENTUM_DECAY_MODEL | MLN_MOMENTUM_DECAY_DURATION_MS;
        options.zoom.decay.model = MLN_MOMENTUM_DECAY_MODEL_EASE;
        options.zoom.decay.duration_ms = 300.0;
        let error = momentum(&options).unwrap_err();
        assert_eq!(error.message(), "the momentum decay of zoom needs a distance and a duration for an eased glide");

        options.zoom.decay.fields |= MLN_MOMENTUM_DECAY_DISTANCE | MLN_MOMENTUM_DECAY_EASING;
        options.zoom.decay.distance = 1.0;
        options.zoom.decay.easing = mln_unit_bezier { x1: 0.2, y1: 2.0, x2: 0.6, y2: 1.0 };
        assert!(momentum(&options).unwrap_err().message().contains("control points are within [0, 1]"));
        options.zoom.decay.easing.y1 = 0.5;
        let decay = momentum(&options).unwrap().zoom_decay;
        assert_eq!((decay.model, decay.distance, decay.duration), (MomentumDecayModel::Ease, 1.0, duration(300.0)));
    }

    #[test]
    fn settings_outside_their_range_fail() {
        let mut options: mln_gesture_momentum = zeroed();
        options.bearing.fields = MLN_ANCHORED_MOMENTUM_SNAP;
        options.bearing.snap = mln_momentum_snap { fields: MLN_MOMENTUM_SNAP_SPACING, ..zeroed() };
        assert!(momentum(&options).unwrap_err().message().contains("positive spacing"));

        let mut options: mln_gesture_momentum = zeroed();
        options.pitch.decay = mln_momentum_decay { fields: MLN_MOMENTUM_DECAY_MAX_SPEED, max_speed: -1.0, ..zeroed() };
        assert!(momentum(&options).unwrap_err().message().contains("of pitch needs a finite minimum speed"));

        let mut options: mln_gesture_momentum = zeroed();
        options.zoom.fields = MLN_ANCHORED_MOMENTUM_VELOCITY;
        options.zoom.velocity = f64::NAN;
        assert_eq!(momentum(&options).unwrap_err().message(), "gesture momentum velocities must be finite");
    }
}
