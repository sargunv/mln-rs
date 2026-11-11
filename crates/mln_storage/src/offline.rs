// Ports include/mln/storage/offline.hpp and
// platform/default/src/mln/storage/offline.cpp: offline region
// definitions (and their private JSON encoding), metadata, status and
// `OfflineRegion`.
//
// rapidjson parsing is serde_json (as everywhere in the port); `IsDouble()`
// is `is_f64()` (a number written with a fraction or exponent, or an integer
// too large for 64 bits). Encoding writes through //vendor/rapidjson's
// `Writer`, so the stored definitions are byte-identical to C++'s.

use mln_util::error::Error;
use mln_util::geo::{LatLng, LatLngBounds};
use mln_util::geometry::Geometry;
use rapidjson::Writer;
use rapidjson::writer::Type;
use serde_json::Value as Json;

use crate::response::ResourceError;

fn check_definition(min_zoom: f64, max_zoom: f64, pixel_ratio: f32) -> Result<(), Error> {
    if !min_zoom.is_finite() || min_zoom < 0.0 || max_zoom.is_nan() || max_zoom < min_zoom {
        return Err(Error::invalid_argument("offline region zoom range is invalid"));
    }
    if !pixel_ratio.is_finite() || pixel_ratio < 0.0 {
        return Err(Error::invalid_argument("offline region pixel ratio is invalid"));
    }
    Ok(())
}

/// An offline region defined by a style URL, geographic bounding box, zoom
/// range, and device pixel ratio.
///
/// Both minZoom and maxZoom must be ≥ 0, and maxZoom must be ≥ minZoom.
///
/// maxZoom may be ∞, in which case for each tile source, the region will
/// include tiles from minZoom up to the maximum zoom level provided by that
/// source.
///
/// pixelRatio must be ≥ 0 and should typically be 1.0 or 2.0.
#[derive(Clone, Debug, PartialEq)]
pub struct OfflineTilePyramidRegionDefinition {
    pub style_url: String,
    pub bounds: LatLngBounds,
    pub min_zoom: f64,
    pub max_zoom: f64,
    pub pixel_ratio: f32,
    pub include_ideographs: bool,
}

impl OfflineTilePyramidRegionDefinition {
    /// Fails with [`ErrorKind::InvalidArgument`](mln_util::ErrorKind) for a
    /// negative or reversed zoom range, or a negative or infinite pixel ratio.
    pub fn new(
        style_url: impl Into<String>,
        bounds: LatLngBounds,
        min_zoom: f64,
        max_zoom: f64,
        pixel_ratio: f32,
        include_ideographs: bool,
    ) -> Result<Self, Error> {
        check_definition(min_zoom, max_zoom, pixel_ratio)?;
        Ok(OfflineTilePyramidRegionDefinition {
            style_url: style_url.into(),
            bounds,
            min_zoom,
            max_zoom,
            pixel_ratio,
            include_ideographs,
        })
    }
}

/// An offline region defined by a style URL, geometry, zoom range, and
/// device pixel ratio.
///
/// Both minZoom and maxZoom must be ≥ 0, and maxZoom must be ≥ minZoom.
///
/// maxZoom may be ∞, in which case for each tile source, the region will
/// include tiles from minZoom up to the maximum zoom level provided by that
/// source.
///
/// pixelRatio must be ≥ 0 and should typically be 1.0 or 2.0.
#[derive(Clone, Debug, PartialEq)]
pub struct OfflineGeometryRegionDefinition {
    pub style_url: String,
    pub geometry: Geometry<f64>,
    pub min_zoom: f64,
    pub max_zoom: f64,
    pub pixel_ratio: f32,
    pub include_ideographs: bool,
}

impl OfflineGeometryRegionDefinition {
    /// Fails with [`ErrorKind::InvalidArgument`](mln_util::ErrorKind) for a
    /// negative or reversed zoom range, or a negative or infinite pixel ratio.
    pub fn new(
        style_url: impl Into<String>,
        geometry: Geometry<f64>,
        min_zoom: f64,
        max_zoom: f64,
        pixel_ratio: f32,
        include_ideographs: bool,
    ) -> Result<Self, Error> {
        check_definition(min_zoom, max_zoom, pixel_ratio)?;
        Ok(OfflineGeometryRegionDefinition {
            style_url: style_url.into(),
            geometry,
            min_zoom,
            max_zoom,
            pixel_ratio,
            include_ideographs,
        })
    }
}

/// The offline region definition types supported
/// (`std::variant<OfflineTilePyramidRegionDefinition, OfflineGeometryRegionDefinition>`).
#[derive(Clone, Debug, PartialEq)]
pub enum OfflineRegionDefinition {
    TilePyramid(OfflineTilePyramidRegionDefinition),
    Geometry(OfflineGeometryRegionDefinition),
}

impl From<OfflineTilePyramidRegionDefinition> for OfflineRegionDefinition {
    fn from(definition: OfflineTilePyramidRegionDefinition) -> Self {
        OfflineRegionDefinition::TilePyramid(definition)
    }
}

impl From<OfflineGeometryRegionDefinition> for OfflineRegionDefinition {
    fn from(definition: OfflineGeometryRegionDefinition) -> Self {
        OfflineRegionDefinition::Geometry(definition)
    }
}

impl OfflineRegionDefinition {
    // The common members, for the C++ `std::visit([](auto& region) {...})`.

    pub fn style_url(&self) -> &str {
        match self {
            OfflineRegionDefinition::TilePyramid(r) => &r.style_url,
            OfflineRegionDefinition::Geometry(r) => &r.style_url,
        }
    }

    pub fn min_zoom(&self) -> f64 {
        match self {
            OfflineRegionDefinition::TilePyramid(r) => r.min_zoom,
            OfflineRegionDefinition::Geometry(r) => r.min_zoom,
        }
    }

    pub fn max_zoom(&self) -> f64 {
        match self {
            OfflineRegionDefinition::TilePyramid(r) => r.max_zoom,
            OfflineRegionDefinition::Geometry(r) => r.max_zoom,
        }
    }

    pub fn pixel_ratio(&self) -> f32 {
        match self {
            OfflineRegionDefinition::TilePyramid(r) => r.pixel_ratio,
            OfflineRegionDefinition::Geometry(r) => r.pixel_ratio,
        }
    }

    pub fn include_ideographs(&self) -> bool {
        match self {
            OfflineRegionDefinition::TilePyramid(r) => r.include_ideographs,
            OfflineRegionDefinition::Geometry(r) => r.include_ideographs,
        }
    }
}

/// `decodeOfflineRegionDefinition(const std::string&)`. A definition that
/// isn't one fails with [`ErrorKind::Native`](mln_util::ErrorKind), and one
/// with invalid values with the constructors' error.
pub fn decode_offline_region_definition(region: &[u8]) -> Result<OfflineRegionDefinition, Error> {
    let malformed = || Error::native("Malformed offline region definition");

    // `doc.Parse<0>(region.c_str())` (stops at the first NUL byte).
    let end = region.iter().position(|&b| b == 0).unwrap_or(region.len());
    let doc: Option<Json> = serde_json::from_slice(&region[..end]).ok();
    let Some(doc) = doc else {
        return Err(malformed());
    };
    // `HasMember` on a non-object asserts in C++; a definition is always an
    // object.
    let member = |name: &str| doc.as_object().and_then(|o| o.get(name));
    let is_double = |v: Option<&Json>| v.is_some_and(Json::is_f64);

    // validation

    let has_valid_bounds =
        || member("bounds").and_then(Json::as_array).is_some_and(|b| b.len() == 4 && b.iter().all(Json::is_f64));

    let has_valid_geometry = || member("geometry").is_some_and(Json::is_object);

    if !member("style_url").is_some_and(Json::is_string)
        || !(has_valid_bounds() || has_valid_geometry())
        || !is_double(member("min_zoom"))
        || (member("max_zoom").is_some() && !is_double(member("max_zoom")))
        || !is_double(member("pixel_ratio"))
        || (member("include_ideographs").is_some() && !member("include_ideographs").is_some_and(Json::is_boolean))
    {
        return Err(malformed());
    }

    // Common properties

    let style_url = member("style_url").and_then(Json::as_str).unwrap_or_default().to_owned();
    let min_zoom = member("min_zoom").and_then(Json::as_f64).unwrap_or_default();
    let max_zoom = member("max_zoom").map_or(f64::INFINITY, |v| v.as_f64().unwrap_or_default());
    let pixel_ratio = member("pixel_ratio").and_then(Json::as_f64).unwrap_or_default() as f32;
    let include_ideographs = member("include_ideographs").is_none_or(|v| v.as_bool().unwrap_or_default());

    if let Some(bounds) = member("bounds") {
        // `bounds` is present but may be invalid when `geometry` is valid:
        // C++ then reads the members of whatever it holds (undefined for a
        // non-array); the port treats that as malformed.
        let bounds = bounds.as_array().filter(|b| b.len() == 4).ok_or_else(malformed)?;
        let get = |i: usize| bounds[i].as_f64().unwrap_or_default();
        let latlng = |lat: f64, lon: f64| LatLng::new(lat, lon);
        Ok(OfflineTilePyramidRegionDefinition::new(
            style_url,
            LatLngBounds::hull(latlng(get(0), get(1))?, latlng(get(2), get(3))?),
            min_zoom,
            max_zoom,
            pixel_ratio,
            include_ideographs,
        )?
        .into())
    } else {
        let geometry = geojson::convert_geometry(member("geometry").expect("validated geometry"))
            .map_err(|e| Error::native(e.message()))?;
        Ok(OfflineGeometryRegionDefinition::new(
            style_url,
            geometry,
            min_zoom,
            max_zoom,
            pixel_ratio,
            include_ideographs,
        )?
        .into())
    }
}

/// `encodeOfflineRegionDefinition(const OfflineRegionDefinition&)`.
pub fn encode_offline_region_definition(region: &OfflineRegionDefinition) -> String {
    let mut w = Writer::new();
    w.start_object();

    // Encode common properties
    w.key("style_url");
    w.string(region.style_url());
    w.key("min_zoom");
    w.double(region.min_zoom());
    if region.max_zoom().is_finite() {
        w.key("max_zoom");
        w.double(region.max_zoom());
    }

    w.key("pixel_ratio");
    w.double(f64::from(region.pixel_ratio()));
    w.key("include_ideographs");
    w.bool_(region.include_ideographs());

    // Encode specific properties
    match region {
        OfflineRegionDefinition::TilePyramid(region) => {
            w.key("bounds");
            w.start_array();
            w.double(region.bounds.south());
            w.double(region.bounds.west());
            w.double(region.bounds.north());
            w.double(region.bounds.east());
            w.end_array();
        }
        OfflineRegionDefinition::Geometry(region) => {
            w.key("geometry");
            let geometry = geojson::stringify_geometry(&region.geometry);
            let type_ = if matches!(region.geometry, Geometry::Empty) { Type::Null } else { Type::Object };
            w.raw_value(&geometry, type_);
        }
    }

    w.end_object();
    w.into_string()
}

/// Arbitrary binary region metadata. The contents are opaque to the mbgl
/// implementation; it just stores and retrieves a BLOB.
pub type OfflineRegionMetadata = Vec<u8>;

/// `OfflineRegionDownloadState`: a region is either inactive (not
/// downloading, but previously-downloaded resources are available for use),
/// or active (resources are being downloaded or will be downloaded, if
/// necessary, when network access is available).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OfflineRegionDownloadState {
    #[default]
    Inactive,
    Active,
}

/// `OfflineRegionStatus`: a region's status includes its active/inactive
/// state as well as counts of the number of resources that have completed
/// downloading, their total size in bytes, and the total number of
/// resources that are required.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OfflineRegionStatus {
    pub download_state: OfflineRegionDownloadState,

    /// The number of resources that have been fully downloaded and are ready
    /// for offline access.
    pub completed_resource_count: u64,

    /// The cumulative size, in bytes, of all resources (inclusive of tiles)
    /// that have been fully downloaded.
    pub completed_resource_size: u64,

    /// The number of tiles that are known to be required for this region.
    /// This is a subset of `completedResourceCount`.
    pub completed_tile_count: u64,

    /// The number of tiles that are known to be required for this region.
    pub required_tile_count: u64,

    /// The cumulative size, in bytes, of all tiles that have been fully
    /// downloaded. This is a subset of `completedResourceSize`.
    pub completed_tile_size: u64,

    /// The number of resources that are known to be required for this
    /// region. See the documentation for `requiredResourceCountIsPrecise`
    /// for an important caveat about this number.
    pub required_resource_count: u64,

    /// This property is true when the value of requiredResourceCount is a
    /// precise count of the number of required resources, and false when it
    /// is merely a lower bound.
    pub required_resource_count_is_precise: bool,
}

impl OfflineRegionStatus {
    /// `complete()`.
    pub fn complete(&self) -> bool {
        self.completed_resource_count >= self.required_resource_count
    }
}

/// `OfflineRegionObserver`: a region can have a single observer, which gets
/// notified whenever a change to the region's status occurs. The methods
/// run on the database thread. They take `&self` (C++: non-const virtuals):
/// the download calls them re-entrantly, e.g. an observer that deactivates
/// the download from `statusChanged` gets a nested `statusChanged`.
pub trait OfflineRegionObserver {
    /// Notified of a change in the status of an offline region.
    fn status_changed(&self, _status: OfflineRegionStatus) {}

    /// Notified of errors encountered while downloading regional resources.
    fn response_error(&self, _error: ResourceError) {}

    /// Notified when the limit on the number of Mapbox tiles stored for
    /// offline regions has been reached.
    fn mapbox_tile_count_limit_exceeded(&self, _limit: u64) {}
}

/// `OfflineRegion`.
#[derive(Clone, Debug, PartialEq)]
pub struct OfflineRegion {
    id: i64,
    definition: OfflineRegionDefinition,
    metadata: OfflineRegionMetadata,
}

impl OfflineRegion {
    /// The private constructor (`friend class OfflineDatabase`).
    #[cfg(feature = "sqlite")]
    pub(crate) fn new(id: i64, definition: OfflineRegionDefinition, metadata: OfflineRegionMetadata) -> Self {
        OfflineRegion { id, definition, metadata }
    }

    pub fn get_id(&self) -> i64 {
        self.id
    }

    pub fn get_definition(&self) -> &OfflineRegionDefinition {
        &self.definition
    }

    pub fn get_metadata(&self) -> &OfflineRegionMetadata {
        &self.metadata
    }
}

/// `OfflineRegions`.
pub type OfflineRegions = Vec<OfflineRegion>;
