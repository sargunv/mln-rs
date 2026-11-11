//! A copy of a map's state, which [`Map::snapshot`](crate::Map::snapshot)
//! takes.

use mln_map_types::{BoundOptions, FreeCameraOptions, MapDebugOptions, MapView, TileLodMode};
use mln_util::geo::LatLngBounds;
use mln_util::vectors::{Vec3, Vec4};

/// A map's state at one moment.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MapSnapshot {
    /// Grows with each change to the map's state. Two snapshots with the same
    /// generation show the same state.
    pub generation: u64,
    /// Counts the updates that the map has sent to its renderer. A frame shows
    /// the map's state as of this snapshot once it renders this update.
    pub update_generation: u64,
    pub debug: MapDebugOptions,
    /// Whether the style and every resource that the view needs have loaded,
    /// and the renderer has drawn them.
    pub fully_loaded: bool,
    pub rendering_stats_view_enabled: bool,
    pub gesture_in_progress: bool,
    /// Whether any camera command is still animating.
    pub transition_in_progress: bool,
    pub tile: TileSettings,
    pub bounds: CameraBounds,
    pub free_camera: FreeCamera,
    pub view: MapView,
}

/// How a map picks the tiles to load.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TileSettings {
    /// How many zoom levels lower the map also loads tiles for, so it can show
    /// a coarse map while the current tiles load.
    pub prefetch_zoom_delta: u8,
    /// The radius around the view point, in tiles, that keeps full detail.
    pub lod_min_radius: f64,
    /// Scales the distance to the view point. Above 1 lowers the detail.
    pub lod_scale: f64,
    /// The pitch, in degrees, above which the map lowers the detail.
    pub lod_pitch_threshold: f64,
    /// Zoom levels to add for picking the detail. Below 0 loads fewer tiles.
    pub lod_zoom_shift: f64,
    pub lod_mode: TileLodMode,
}

/// All the limits on a map's camera.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraBounds {
    /// The area that the camera's center stays within, or `None` when no area
    /// limits it.
    pub bounds: Option<LatLngBounds>,
    pub min_zoom: f64,
    pub max_zoom: f64,
    /// The lowest pitch, in degrees.
    pub min_pitch: f64,
    /// The highest pitch, in degrees.
    pub max_pitch: f64,
}

impl From<CameraBounds> for BoundOptions {
    /// Options that set every limit.
    fn from(bounds: CameraBounds) -> Self {
        BoundOptions {
            bounds: Some(bounds.bounds.unwrap_or_else(LatLngBounds::infinite)),
            min_zoom: Some(bounds.min_zoom),
            max_zoom: Some(bounds.max_zoom),
            min_pitch: Some(bounds.min_pitch),
            max_pitch: Some(bounds.max_pitch),
        }
    }
}

/// A camera's whole position and orientation in three dimensions.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FreeCamera {
    /// The position in Web Mercator units, where the world is 1 wide, (0, 0)
    /// is its northwest corner and (1, 1) its southeast corner. `z` is the
    /// altitude in the same units.
    pub position: Vec3,
    /// The rotation from looking straight down with north at the top of the
    /// screen, as a unit quaternion `[x, y, z, w]`.
    pub orientation: Vec4,
}

impl From<FreeCamera> for FreeCameraOptions {
    /// Options that set the position and the orientation.
    fn from(camera: FreeCamera) -> Self {
        FreeCameraOptions { position: Some(camera.position), orientation: Some(camera.orientation) }
    }
}
