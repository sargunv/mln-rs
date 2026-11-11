//! Ports `src/mln/map` + `include/mln/map` (Map, Map::Impl, MapOptions,
//! Transform, the observers). The map vocabulary that lower crates need
//! (`CameraOptions`, `MapMode`, `TransformState`, ...) lives in
//! `mln_map_types` and is re-exported here.

pub mod map;
pub mod map_impl;
pub mod map_observer;
pub mod map_options;
pub mod map_snapshot;
// gtests-core: MapProjection (test/map/map.test.cpp).
pub mod map_projection;
pub mod rendering_stats_view;
pub mod transform;

pub use map::Map;
pub use map_impl::{StillImageCallback, StyleMut};
pub use map_observer::{
    CameraChange, CameraChangeMode, CameraChangeSource, MapLoadError, MapObserver, NullMapObserver, RenderFrameStatus,
    RenderMode,
};
pub use map_options::MapOptions;
pub use map_projection::MapProjection;
pub use map_snapshot::{CameraBounds, FreeCamera, MapSnapshot, TileSettings};
pub use mln_map_types::{
    AnimationOptions, BoundOptions, Camera, CameraDelta, CameraError, CameraMomentum, CameraOptions, CameraUpdate,
    CameraUpdateMode, CameraVelocity, ConstrainMode, FreeCameraOptions, GesturePhase, LogicalExtent, MapDebugOptions,
    MapMode, MapView, MomentumDecay, MomentumDecayModel, MomentumEasing, MomentumSnap, NorthOrientation,
    ProjectionMode, TileLodMode, TransformState, TransitionEnd, TransitionOutcome, Viewport, ViewportMode,
};
pub use mln_renderer::{
    FeatureInfo, FrameRenderedFeaturesMap, LayerFeaturesMap, NDCBound, OverscaledTileID, RenderingStats, SourceLayerID,
    TileOperation,
};
