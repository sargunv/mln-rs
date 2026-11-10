// Ports src/mln/layout/pattern_layout.hpp
//
// The C++ template `PatternLayout<BucketType, LayerPropertiesType,
// PatternPropertyType, LayoutPropertiesType, SortKeyPropertyType>` is
// `PatternLayout<S: PatternLayoutSpec>`; the fill and line specs are below.
// The unsorted/sorted instantiations (`SortKeyPropertyType = void` or the
// sort key) are one type with a `sorted` flag, chosen as the C++ factories
// choose the instantiation.
//
// A Rust `GeometryTileLayer` borrows its tile data, so the layout keeps the
// feature *indices* it accepted (`PatternFeature::i`) and gets the features
// again in `create_bucket` (C++ keeps the `unique_ptr<GeometryTileFeature>`).
// Getting a feature twice yields the same feature.

use std::collections::HashMap;
use std::sync::Arc;

use mln_style::PropertyEvaluationParameters;
use mln_style::expression::{EvaluationContext, Image};
use mln_style::layer_properties::{LayerProperties, get_evaluated};
use mln_style::layers::fill_layer_properties::{FillLayerProperties, FillLayoutProperties, FillPattern, FillSortKey};
use mln_style::layers::line_layer_properties::{LineLayerProperties, LineLayoutProperties, LinePattern, LineSortKey};
use mln_style::properties::Property;
use mln_style::renderer::cross_faded_property_evaluator::Faded;
use mln_style::renderer::possibly_evaluated_property_value::PossiblyEvaluatedFadedPropertyValue;
use mln_style_types::image::{ImageDependencies, ImagePositions, ImageType};
use mln_tile_id::CanonicalTileID;
use mln_tile_id::geometry_tile_data::{GeometryCollection, GeometryTileFeature, GeometryTileLayer};
use mln_util::exception::ExceptionPtr;
use mln_util::immutable::Immutable;
use mln_vec_map::{VecMap, VecSet};

use crate::bucket::{Bucket, BucketPtr};
use crate::bucket_parameters::BucketParameters;
use crate::buckets::fill_bucket::FillBucket;
use crate::buckets::line_bucket::LineBucket;
use crate::geometry::feature_index::FeatureIndex;
use crate::layout::layout::{Layout, LayoutParameters, SourceLayer};
use crate::render_layer::LayerRenderData;

/// `PatternDependency`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PatternDependency {
    pub min: String,
    pub mid: String,
    pub max: String,
}

/// `PatternLayerMap = mln::unordered_map<std::string, PatternDependency>`.
pub type PatternLayerMap = HashMap<String, PatternDependency>;

/// `PatternFeature` (the feature itself is re-read from the source layer,
/// see the file comment).
#[derive(Clone, Debug)]
pub struct PatternFeature {
    pub i: usize,
    pub sort_key: f32,
    patterns: PatternLayerMap,
}

impl PatternFeature {
    pub fn get_patterns(&self) -> &PatternLayerMap {
        &self.patterns
    }
}

/// The template arguments of a `PatternLayout` instantiation.
pub trait PatternLayoutSpec: 'static {
    /// `BucketType`
    type Bucket: Bucket;
    /// `LayoutPropertiesType::PossiblyEvaluated`
    type Layout: Clone + Send + Sync;

    /// `leaderLayerProperties->layerImpl().layout.evaluate(parameters)`.
    fn evaluate_layout(
        properties: &Immutable<dyn LayerProperties>,
        parameters: &PropertyEvaluationParameters,
    ) -> Self::Layout;
    /// `getEvaluated<LayerPropertiesType>(p).get<PatternPropertyType>()`.
    fn pattern_property(properties: &Immutable<dyn LayerProperties>) -> &PossiblyEvaluatedFadedPropertyValue<Image>;
    /// `PatternPropertyType::defaultValue()`.
    fn pattern_default() -> Image;
    /// `layout.get<SortKeyPropertyType>().evaluate(feature, zoom, canonical, default)`.
    fn sort_key(
        layout: &Self::Layout,
        feature: &dyn GeometryTileFeature,
        zoom: f32,
        canonical: &CanonicalTileID,
    ) -> f32;
    /// The sort key when it is the same for every feature.
    fn constant_sort_key(layout: &Self::Layout) -> Option<f32>;
    /// `std::make_shared<BucketType>(layout, layerPropertiesMap, zoom, overscaling)`.
    fn create_bucket(
        layout: &Self::Layout,
        layer_properties_map: &VecMap<String, Immutable<dyn LayerProperties>>,
        zoom: f32,
        overscaling: u32,
    ) -> Self::Bucket;
}

/// `PatternLayout<...>`.
pub struct PatternLayout<S: PatternLayoutSpec> {
    pub layer_properties_map: VecMap<String, Immutable<dyn LayerProperties>>,
    pub bucket_leader_id: String,

    pub features: Vec<PatternFeature>,
    pub layout: S::Layout,

    pub zoom: f32,
    pub overscaling: u32,
    pub source_layer_id: String,
    pub has_pattern: bool,
    pub retain_features_by_id: bool,

    /// `const std::unique_ptr<GeometryTileLayer> sourceLayer` (see
    /// layout.rs); `None` when built from a borrowed layer.
    pub source_layer: Option<SourceLayer>,
}

/// `constantOr(Faded<expression::Image>{.from = "", .to = ""})`.
fn empty_faded() -> Faded<Image> {
    Faded { from: Image::new(""), to: Image::new("") }
}

impl<S: PatternLayoutSpec> PatternLayout<S> {
    /// `PatternLayout(parameters, group, sourceLayer, layoutParameters)`.
    /// `sorted` selects the `SortKeyPropertyType` instantiation.
    pub fn new(
        parameters: &BucketParameters,
        group: &[Immutable<dyn LayerProperties>],
        source_layer: &dyn GeometryTileLayer,
        image_dependencies: &mut ImageDependencies,
        available_images: &VecSet<String>,
        sorted: bool,
    ) -> Result<Self, ExceptionPtr> {
        debug_assert!(!group.is_empty());
        let zoom = parameters.tile_id.overscaled_z as f32;
        let overscaling = parameters.tile_id.overscale_factor();
        let leader_layer_properties = &group[0];
        let mut evaluation_parameters = PropertyEvaluationParameters::new(zoom);
        evaluation_parameters.global_state = parameters.global_state.clone();
        let layout = S::evaluate_layout(leader_layer_properties, &evaluation_parameters);
        let leader_impl = &leader_layer_properties.base_impl;
        let source_layer_id = leader_impl.source_layer.clone();
        let bucket_leader_id = leader_impl.id.clone();

        let mut has_pattern = false;
        let mut layer_properties_map: VecMap<String, Immutable<dyn LayerProperties>> = VecMap::new();
        for layer_properties in group {
            let layer_id = layer_properties.base_impl.id.clone();
            let pattern_property = S::pattern_property(layer_properties);
            let constant_pattern = pattern_property.constant_or(&empty_faded());
            // determine if layer group has any layers that use *-pattern
            // property and add constant pattern dependencies.
            if !pattern_property.is_constant() {
                has_pattern = true;
            } else if !constant_pattern.to.id().is_empty() {
                has_pattern = true;
                image_dependencies.entry(constant_pattern.to.id().to_owned()).or_insert(ImageType::Pattern);
                image_dependencies.entry(constant_pattern.from.id().to_owned()).or_insert(ImageType::Pattern);
            }
            layer_properties_map.entry(layer_id).or_insert_with(|| layer_properties.clone());
        }

        let constant_sort_key = if sorted { S::constant_sort_key(&layout) } else { None };
        let mut features: Vec<PatternFeature> = Vec::new();
        let feature_count = source_layer.feature_count();
        for i in 0..feature_count {
            let feature = source_layer.get_feature(i)?;
            let passes = leader_impl.filter.evaluate(
                &EvaluationContext::with_zoom_feature(zoom, Some(&*feature))
                    .with_canonical_tile_id(Some(&parameters.tile_id.canonical))
                    .with_global_state(parameters.global_state.as_deref()),
            );
            feature.check()?;
            if !passes {
                continue;
            }

            let mut pattern_dependency_map = PatternLayerMap::new();
            if has_pattern {
                for layer_properties in group {
                    let layer_id = &layer_properties.base_impl.id;
                    if let Some(properties) = layer_properties_map.get(layer_id) {
                        let pattern_property = S::pattern_property(properties);
                        if !pattern_property.is_constant() {
                            // For layers with non-data-constant pattern
                            // properties, evaluate their expression and add
                            // the patterns to the dependency vector
                            let canonical = &parameters.tile_id.canonical;
                            let min = pattern_property.evaluate(
                                &*feature,
                                zoom - 1.0,
                                available_images,
                                canonical,
                                S::pattern_default(),
                            );
                            let mid = pattern_property.evaluate(
                                &*feature,
                                zoom,
                                available_images,
                                canonical,
                                S::pattern_default(),
                            );
                            let max = pattern_property.evaluate(
                                &*feature,
                                zoom + 1.0,
                                available_images,
                                canonical,
                                S::pattern_default(),
                            );

                            image_dependencies.entry(min.to.id().to_owned()).or_insert(ImageType::Pattern);
                            image_dependencies.entry(mid.to.id().to_owned()).or_insert(ImageType::Pattern);
                            image_dependencies.entry(max.to.id().to_owned()).or_insert(ImageType::Pattern);
                            pattern_dependency_map.entry(layer_id.clone()).or_insert(PatternDependency {
                                min: min.to.id().to_owned(),
                                mid: mid.to.id().to_owned(),
                                max: max.to.id().to_owned(),
                            });
                        }
                    }
                }
            }

            feature.check()?;

            if let Some(sort_key) = constant_sort_key {
                features.push(PatternFeature { i, sort_key, patterns: pattern_dependency_map });
            } else if sorted {
                // PatternFeatureInserter<SortKeyPropertyType>::insert
                let sort_key = S::sort_key(&layout, &*feature, zoom, &parameters.tile_id.canonical);
                feature.check()?;
                let pattern_feature = PatternFeature { i, sort_key, patterns: pattern_dependency_map };
                // std::lower_bound: the first feature whose key is not less.
                let lower_bound = features.partition_point(|f| f.sort_key < sort_key);
                features.insert(lower_bound, pattern_feature);
            } else {
                // PatternFeatureInserter<void>::insert
                features.push(PatternFeature { i, sort_key: 0.0, patterns: pattern_dependency_map });
            }
        }
        if constant_sort_key.is_some() {
            // Inserting at the lower bound puts each feature before the
            // earlier ones with an equal key: keep that order without a sort.
            features.reverse();
        }

        Ok(PatternLayout {
            layer_properties_map,
            bucket_leader_id,
            features,
            layout,
            zoom,
            overscaling,
            source_layer_id,
            has_pattern,
            retain_features_by_id: parameters.retain_features_by_id,
            source_layer: None,
        })
    }

    /// `PatternLayout(parameters, group, std::move(sourceLayer),
    /// layoutParameters)` as the factories call it.
    pub fn with_source_layer(
        parameters: &mut LayoutParameters<'_>,
        group: &[Immutable<dyn LayerProperties>],
        source_layer: SourceLayer,
        sorted: bool,
    ) -> Result<Self, ExceptionPtr> {
        let mut layout = PatternLayout::new(
            parameters.bucket_parameters,
            group,
            &*source_layer.layer()?,
            parameters.image_dependencies,
            parameters.available_images,
            sorted,
        )?;
        layout.source_layer = Some(source_layer);
        Ok(layout)
    }

    /// `hasDependencies()`.
    pub fn has_dependencies(&self) -> bool {
        self.has_pattern
    }

    /// `createBucket(patternPositions, featureIndex, renderData, firstLoad,
    /// showCollisionBoxes, canonical)`. `insert_feature` is
    /// `featureIndex->insert(geometries, i, sourceLayerID, bucketLeaderID)`.
    pub fn create_bucket_with(
        &mut self,
        source_layer: &dyn GeometryTileLayer,
        pattern_positions: &ImagePositions,
        insert_feature: &mut dyn FnMut(&GeometryCollection, usize, &str, &str),
        render_data: &mut HashMap<String, LayerRenderData>,
        canonical: &CanonicalTileID,
    ) -> Result<(), ExceptionPtr> {
        let mut bucket = S::create_bucket(&self.layout, &self.layer_properties_map, self.zoom, self.overscaling);
        bucket.set_retain_features_by_id(self.retain_features_by_id);
        bucket.reserve_features(self.features.len());

        let record_sdf_pattern = |bucket: &mut S::Bucket, layer_id: &str, image_id: &str| {
            if let Some(position) = pattern_positions.get(image_id) {
                bucket.record_sdf_pattern(layer_id, position.sdf);
            }
        };

        for (layer_id, layer_properties) in &self.layer_properties_map {
            let pattern_property = S::pattern_property(layer_properties);
            if pattern_property.is_constant() {
                let pattern = pattern_property.constant_or(&empty_faded());
                record_sdf_pattern(&mut bucket, layer_id, pattern.from.id());
                record_sdf_pattern(&mut bucket, layer_id, pattern.to.id());
            }
        }

        for pattern_feature in &self.features {
            let i = pattern_feature.i;
            let feature = source_layer.get_feature(i)?;
            let patterns = pattern_feature.get_patterns();
            let geometries = feature.get_geometries();
            feature.check()?;

            for (layer_id, pattern) in patterns {
                record_sdf_pattern(&mut bucket, layer_id, &pattern.min);
                record_sdf_pattern(&mut bucket, layer_id, &pattern.mid);
                record_sdf_pattern(&mut bucket, layer_id, &pattern.max);
            }

            insert_feature(geometries, i, &self.source_layer_id, &self.bucket_leader_id);
            bucket.add_feature(&*feature, geometries, pattern_positions, patterns, i, canonical)?;
            feature.check()?;
        }
        if bucket.has_data() {
            let bucket: BucketPtr = Arc::new(bucket);
            for (layer_id, layer_properties) in &self.layer_properties_map {
                render_data.entry(layer_id.clone()).or_insert_with(|| LayerRenderData {
                    bucket: bucket.clone(),
                    layer_properties: layer_properties.clone(),
                });
            }
        }
        Ok(())
    }
}

/// `PatternLayout<FillBucket, FillLayerProperties, FillPattern,
/// FillLayoutProperties[, FillSortKey]>`.
pub struct FillLayoutSpec;

impl PatternLayoutSpec for FillLayoutSpec {
    type Bucket = FillBucket;
    type Layout = FillLayoutProperties::PossiblyEvaluated;

    fn evaluate_layout(
        properties: &Immutable<dyn LayerProperties>,
        parameters: &PropertyEvaluationParameters,
    ) -> Self::Layout {
        let properties = (**properties).as_any().downcast_ref::<FillLayerProperties>().expect("FillLayerProperties");
        properties.layer_impl().layout.evaluate(parameters)
    }
    fn pattern_property(properties: &Immutable<dyn LayerProperties>) -> &PossiblyEvaluatedFadedPropertyValue<Image> {
        get_evaluated::<FillLayerProperties>(properties).get::<FillPattern>()
    }
    fn pattern_default() -> Image {
        FillPattern::default_value()
    }
    fn sort_key(
        layout: &Self::Layout,
        feature: &dyn GeometryTileFeature,
        zoom: f32,
        canonical: &CanonicalTileID,
    ) -> f32 {
        layout.get::<FillSortKey>().evaluate_feature_canonical(feature, zoom, canonical, FillSortKey::default_value())
    }
    fn constant_sort_key(layout: &Self::Layout) -> Option<f32> {
        layout.get::<FillSortKey>().constant()
    }
    fn create_bucket(
        layout: &Self::Layout,
        layer_properties_map: &VecMap<String, Immutable<dyn LayerProperties>>,
        zoom: f32,
        overscaling: u32,
    ) -> FillBucket {
        FillBucket::new(layout, layer_properties_map, zoom, overscaling)
    }
}

/// `PatternLayout<LineBucket, LineLayerProperties, LinePattern,
/// LineLayoutProperties[, LineSortKey]>`.
pub struct LineLayoutSpec;

impl PatternLayoutSpec for LineLayoutSpec {
    type Bucket = LineBucket;
    type Layout = LineLayoutProperties::PossiblyEvaluated;

    fn evaluate_layout(
        properties: &Immutable<dyn LayerProperties>,
        parameters: &PropertyEvaluationParameters,
    ) -> Self::Layout {
        let properties = (**properties).as_any().downcast_ref::<LineLayerProperties>().expect("LineLayerProperties");
        properties.layer_impl().layout.evaluate(parameters)
    }
    fn pattern_property(properties: &Immutable<dyn LayerProperties>) -> &PossiblyEvaluatedFadedPropertyValue<Image> {
        get_evaluated::<LineLayerProperties>(properties).get::<LinePattern>()
    }
    fn pattern_default() -> Image {
        LinePattern::default_value()
    }
    fn sort_key(
        layout: &Self::Layout,
        feature: &dyn GeometryTileFeature,
        zoom: f32,
        canonical: &CanonicalTileID,
    ) -> f32 {
        layout.get::<LineSortKey>().evaluate_feature_canonical(feature, zoom, canonical, LineSortKey::default_value())
    }
    fn constant_sort_key(layout: &Self::Layout) -> Option<f32> {
        layout.get::<LineSortKey>().constant()
    }
    fn create_bucket(
        layout: &Self::Layout,
        layer_properties_map: &VecMap<String, Immutable<dyn LayerProperties>>,
        zoom: f32,
        overscaling: u32,
    ) -> LineBucket {
        LineBucket::new(layout.clone(), layer_properties_map, zoom, overscaling)
    }
}

/// `FillLayout*` / `LineLayout*`: whether the factory picks the sorted
/// instantiation (`layout.get<SortKey>().isUndefined()` on the leader's
/// unevaluated layout).
pub fn fill_layout_is_sorted(leader: &Immutable<dyn LayerProperties>) -> bool {
    let properties = (**leader).as_any().downcast_ref::<FillLayerProperties>().expect("FillLayerProperties");
    !properties.layer_impl().layout.get::<FillSortKey>().is_undefined()
}

pub fn line_layout_is_sorted(leader: &Immutable<dyn LayerProperties>) -> bool {
    let properties = (**leader).as_any().downcast_ref::<LineLayerProperties>().expect("LineLayerProperties");
    !properties.layer_impl().layout.get::<LineSortKey>().is_undefined()
}

impl<S: PatternLayoutSpec> Layout for PatternLayout<S> {
    fn has_dependencies(&self) -> bool {
        PatternLayout::has_dependencies(self)
    }

    fn create_bucket(
        &mut self,
        pattern_positions: &ImagePositions,
        feature_index: &mut FeatureIndex,
        render_data: &mut HashMap<String, LayerRenderData>,
        _first_load: bool,
        _show_collision_boxes: bool,
        canonical: &CanonicalTileID,
    ) -> Result<(), ExceptionPtr> {
        let source_layer = self.source_layer.clone().expect("PatternLayout: source layer");
        let layer = source_layer.layer()?;
        self.create_bucket_with(
            &*layer,
            pattern_positions,
            &mut |geometries, i, source_layer_id, bucket_leader_id| {
                feature_index.insert(geometries, i, source_layer_id, bucket_leader_id)
            },
            render_data,
            canonical,
        )
    }
}
