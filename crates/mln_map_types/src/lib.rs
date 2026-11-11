//! Ports the `include/mln/map` vocabulary that crates below the map need
//! (cut point, like `mln_style_types`): `camera.hpp` (style's default
//! camera), `camera_momentum.hpp`, `mode.hpp`, `projection_mode.hpp`,
//! `bound_options.hpp`. The `Map` itself is in `mln_map`, which re-exports
//! these.

pub mod bound_options;
pub mod camera;
pub mod camera_momentum;
pub mod map_view;
pub mod mode;
pub mod projection_mode;

pub use bound_options::BoundOptions;
pub use camera::{
    AnimationOptions, CameraDelta, CameraError, CameraOptions, CameraUpdate, CameraUpdateMode, CameraVelocity,
    GesturePhase, TransitionEnd, TransitionOutcome,
};
pub use camera_momentum::{CameraMomentum, MomentumDecay, MomentumDecayModel, MomentumEasing, MomentumSnap};
pub use map_view::{Camera, LogicalExtent, MapView, Viewport};
pub use mode::{ConstrainMode, MapDebugOptions, MapMode, TileLodMode, ViewportMode};
pub use projection_mode::ProjectionMode;
pub mod tile_coordinate;
pub mod transform_state;
pub mod util_camera;

pub use camera::FreeCameraOptions;
pub use tile_coordinate::TileCoordinate;
pub use transform_state::{NorthOrientation, TransformState, TransformStateProperties};
