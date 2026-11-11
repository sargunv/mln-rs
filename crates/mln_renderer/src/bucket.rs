// Ports src/mln/renderer/bucket.hpp and src/mln/renderer/bucket.cpp
//
// C++ `Bucket` is an abstract class with data members: the data is
// `BucketBase`, the virtual interface is the `Bucket` trait. Buckets are
// built on the worker thread and then shared with the render thread
// (`std::shared_ptr<Bucket>` in `LayerRenderData`): `BucketPtr = Arc<dyn
// Bucket>`, `Bucket: Send + Sync`. The render thread only reads them, except
// `upload` (an atomic flag, as in C++) and the geometry it moves out to draw
// (bucket_geometry.rs).
//
// TODO(port): `registerAtCrossTileIndex`, `place`,
// `updateVertices` (symbol buckets, M5), `getRenderThreadID`, `check`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use mln_gfx::UploadPass;
use mln_style::util::AsAny;
use mln_style_types::image::ImagePositions;
use mln_tile_id::CanonicalTileID;
use mln_tile_id::geometry_tile_data::{GeometryCollection, GeometryTileFeature, GeometryTileLayer};
use mln_util::exception::ExceptionPtr;
use mln_util::feature::feature_id_to_string;
use mln_util::identity::SimpleIdentity;
use serde::{Deserialize, Serialize};

use crate::layout::pattern_layout::PatternLayerMap;
use crate::render_layer::RenderLayer;

pub type BucketPtr = Arc<dyn Bucket>;

/// `FeatureStates = mln::unordered_map<std::string, FeatureState>`
/// (util/feature.hpp).
pub type FeatureStates = std::collections::HashMap<String, mln_style::expression::expression::FeatureState>;
/// `LayerFeatureStates = mln::unordered_map<std::string, FeatureStates>`.
pub type LayerFeatureStates = std::collections::HashMap<String, FeatureStates>;

/// `Bucket::RetainedFeature`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetainedFeature {
    pub feature_id: String,
    pub vertex_offset: usize,
    pub vertex_count: usize,
}

/// The data members of `Bucket`.
///
/// Port-only: serde writes what a worker sets (tile/worker_codec); the
/// upload flag, the id and the version are the side's that has the bucket.
#[derive(Debug, Serialize, Deserialize)]
pub struct BucketBase {
    #[serde(skip)]
    pub uploaded: AtomicBool,
    #[serde(skip, default = "SimpleIdentity::new")]
    pub bucket_id: SimpleIdentity,
    pub retain_features_by_id: bool,
    pub retained_features: Vec<RetainedFeature>,
    /// Port addition: bumped by `update` (feature state). C++ updates the
    /// shared vertex vectors in place (`updateModified()`); the render
    /// layers compare this to refresh their drawables' copies.
    #[serde(skip)]
    pub version: AtomicU64,
}

impl Default for BucketBase {
    fn default() -> Self {
        BucketBase {
            version: AtomicU64::new(0),
            uploaded: AtomicBool::new(false),
            bucket_id: SimpleIdentity::new(),
            retain_features_by_id: false,
            retained_features: Vec::new(),
        }
    }
}

/// The C++ `GeometryTooLongException` (gfx/fill_generator.cpp), thrown when
/// a polygon has more vertices than a `uint16_t` index can address. It
/// derives from `std::exception` without overriding `what()`.
#[derive(Debug)]
pub struct GeometryTooLongException;

impl std::fmt::Display for GeometryTooLongException {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("std::exception")
    }
}

impl std::error::Error for GeometryTooLongException {}

pub trait Bucket: AsAny + Send + Sync {
    fn base(&self) -> &BucketBase;
    fn base_mut(&mut self) -> &mut BucketBase;

    fn reserve_features(&mut self, vertex_count: usize) {
        self.base_mut().retained_features.reserve(vertex_count);
    }

    /// Feature geometries are also used to populate the feature index.
    /// Obtaining these is a costly operation, so we do it only once, and
    /// pass-by-const-ref the geometries as a second parameter.
    ///
    /// C++ lets `GeometryTooLongException` propagate (the worker reports
    /// it as the tile's error); here it is the `Err`.
    fn add_feature(
        &mut self,
        _feature: &dyn GeometryTileFeature,
        _geometries: &GeometryCollection,
        _image_positions: &ImagePositions,
        _patterns: &PatternLayerMap,
        _index: usize,
        _canonical: &CanonicalTileID,
    ) -> Result<(), ExceptionPtr> {
        Ok(())
    }

    /// `update(const FeatureStates&, const GeometryTileLayer&, layerID,
    /// const ImagePositions&)`: re-evaluates the feature-state dependent
    /// attribute values. `&self`: the bucket is shared (see the file
    /// comment); implementations lock their binders.
    fn update(
        &self,
        _states: &FeatureStates,
        _layer: &dyn GeometryTileLayer,
        _layer_id: &str,
        _image_positions: &ImagePositions,
    ) {
    }

    /// The `version` counter (see `BucketBase::version`).
    fn get_version(&self) -> u64 {
        self.base().version.load(Ordering::SeqCst)
    }

    fn record_sdf_pattern(&mut self, _layer_id: &str, _sdf: bool) {}
    fn is_sdf_pattern(&self, _layer_id: &str) -> bool {
        false
    }

    /// As long as this bucket has a Prepare render pass, this function is
    /// getting called. Typically, this only happens once when the bucket is
    /// being rendered for the first time.
    fn upload(&self, upload_pass: &dyn UploadPass);

    fn has_data(&self) -> bool;

    fn get_query_radius(&self, _layer: &dyn RenderLayer) -> f32 {
        0.0
    }

    fn needs_upload(&self) -> bool {
        self.has_data() && !self.base().uploaded.load(Ordering::SeqCst)
    }

    fn get_id(&self) -> SimpleIdentity {
        self.base().bucket_id
    }

    fn set_retain_features_by_id(&mut self, value: bool) {
        self.base_mut().retain_features_by_id = value;
    }

    fn retain_feature(&mut self, feature: &dyn GeometryTileFeature, vertex_offset: usize, vertex_count: usize) {
        let feature_id = get_retain_feature_id(feature);
        self.base_mut().retained_features.push(RetainedFeature { feature_id, vertex_offset, vertex_count });
    }

    fn get_retained_features(&self) -> &[RetainedFeature] {
        &self.base().retained_features
    }
}

impl dyn Bucket {
    /// `static_cast<B&>(*bucket)`, checked.
    pub fn downcast_ref<B: Bucket>(&self) -> Option<&B> {
        self.as_any().downcast_ref::<B>()
    }
}

static UNIDENTIFIED_FEATURE_INDEX: AtomicUsize = AtomicUsize::new(0);

/// `Bucket::getRetainFeatureID(const GeometryTileFeature&)`.
pub fn get_retain_feature_id(feature: &dyn GeometryTileFeature) -> String {
    if let Some(id_str) = feature_id_to_string(&feature.get_id()) {
        if !id_str.is_empty() {
            return id_str;
        }
    }
    // Assign a unique number to unidentified features
    format!("maplibre:{}", UNIDENTIFIED_FEATURE_INDEX.fetch_add(1, Ordering::SeqCst))
}
