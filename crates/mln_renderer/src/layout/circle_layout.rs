// Ports src/mln/layout/circle_layout.hpp
//
// As in pattern_layout.rs, the layout keeps the indices (and sort keys) of
// the features its filter accepted and gets the features again from the
// source layer in `create_bucket`.

use std::collections::HashMap;
use std::sync::Arc;

use mln_map_types::MapMode;
use mln_style::PropertyEvaluationParameters;
use mln_style::expression::{EvaluationContext, Value};
use mln_style::layer_properties::LayerProperties;
use mln_style::layers::circle_layer_properties::{CircleLayerProperties, CircleSortKey};
use mln_style::properties::Property;
use mln_style_types::image::ImagePositions;
use mln_tile_id::CanonicalTileID;
use mln_tile_id::geometry_tile_data::{GeometryCollection, GeometryTileFeature, GeometryTileLayer};
use mln_util::constants::EXTENT;
use mln_util::exception::ExceptionPtr;
use mln_util::immutable::Immutable;
use mln_util::sync::RwLockExt;
use mln_vec_map::VecMap;

use crate::bucket::{Bucket, BucketPtr};
use crate::bucket_parameters::BucketParameters;
use crate::buckets::circle_bucket::CircleBucket;
use crate::geometry::feature_index::FeatureIndex;
use crate::layout::layout::{Layout, SourceLayer};
use crate::render_layer::LayerRenderData;

/// `CircleLayout::CircleFeature` (without the feature, see above).
#[derive(Clone, Copy, Debug)]
pub struct CircleFeature {
    pub i: usize,
    pub sort_key: f32,
}

/// `CircleLayout`.
pub struct CircleLayout {
    pub layer_properties_map: VecMap<String, Immutable<dyn LayerProperties>>,
    pub bucket_leader_id: String,

    /// `std::list<CircleFeature> features`.
    pub features: Vec<CircleFeature>,

    pub zoom: f32,
    pub mode: MapMode,
    pub source_layer_id: String,
    pub retain_features_by_id: bool,

    /// `const std::unique_ptr<GeometryTileLayer> sourceLayer` (see
    /// layout.rs); `None` when built from a borrowed layer.
    pub source_layer: Option<SourceLayer>,
}

static EMPTY_FORMATTED_SECTION: Value = Value::Null;

impl CircleLayout {
    /// `CircleLayout(parameters, group, sourceLayer)`.
    pub fn new(
        parameters: &BucketParameters,
        group: &[Immutable<dyn LayerProperties>],
        source_layer: &dyn GeometryTileLayer,
    ) -> Result<Self, ExceptionPtr> {
        debug_assert!(!group.is_empty());
        let zoom = parameters.tile_id.overscaled_z as f32;
        let leader_layer_properties = (*group[0])
            .as_any()
            .downcast_ref::<CircleLayerProperties>()
            .expect("CircleLayout: not circle layer properties");
        let leader_impl = leader_layer_properties.layer_impl();
        let unevaluated_layout = &leader_impl.layout;
        let sort_features_by_key = !unevaluated_layout.get::<CircleSortKey>().is_undefined();
        let mut evaluation_parameters = PropertyEvaluationParameters::new(zoom);
        evaluation_parameters.global_state = parameters.global_state.clone();
        let layout = unevaluated_layout.evaluate(&evaluation_parameters);
        let sort_key_property = layout.get::<CircleSortKey>();
        let constant_sort_key = if sort_features_by_key { sort_key_property.constant() } else { None };
        let source_layer_id = leader_impl.source_layer.clone();
        let bucket_leader_id = leader_impl.id.clone();

        let mut layer_properties_map: VecMap<String, Immutable<dyn LayerProperties>> = VecMap::new();
        for layer_properties in group {
            let layer_id = layer_properties.base_impl.id.clone();
            layer_properties_map.entry(layer_id).or_insert_with(|| layer_properties.clone());
        }

        let mut features: Vec<CircleFeature> = Vec::new();
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

            if !sort_features_by_key {
                features.push(CircleFeature { i, sort_key: CircleSortKey::default_value() });
                continue;
            }

            if let Some(sort_key) = constant_sort_key {
                features.push(CircleFeature { i, sort_key });
                continue;
            }

            let sort_key = sort_key_property.evaluate_feature(&*feature, zoom, CircleSortKey::default_value());
            feature.check()?;
            let circle_feature = CircleFeature { i, sort_key };
            // std::lower_bound: the first feature whose key is not less.
            let sort_position = features.partition_point(|f| f.sort_key < sort_key);
            features.insert(sort_position, circle_feature);
        }
        if constant_sort_key.is_some() {
            // Inserting at the lower bound puts each feature before the
            // earlier ones with an equal key: keep that order without a sort.
            features.reverse();
        }

        Ok(CircleLayout {
            layer_properties_map,
            bucket_leader_id,
            features,
            zoom,
            mode: parameters.mode,
            source_layer_id,
            retain_features_by_id: parameters.retain_features_by_id,
            source_layer: None,
        })
    }

    /// `CircleLayout(parameters, group, std::move(sourceLayer))` as the
    /// factory calls it.
    pub fn with_source_layer(
        parameters: &BucketParameters,
        group: &[Immutable<dyn LayerProperties>],
        source_layer: SourceLayer,
    ) -> Result<Self, ExceptionPtr> {
        let mut layout = CircleLayout::new(parameters, group, &*source_layer.layer()?)?;
        layout.source_layer = Some(source_layer);
        Ok(layout)
    }

    /// `hasDependencies()`.
    pub fn has_dependencies(&self) -> bool {
        false
    }

    /// `createBucket(..., featureIndex, renderData, ..., canonical)`;
    /// `insert_feature` is `featureIndex->insert(geometries, i,
    /// sourceLayerID, bucketLeaderID)`.
    pub fn create_bucket_with(
        &mut self,
        source_layer: &dyn GeometryTileLayer,
        insert_feature: &mut dyn FnMut(&GeometryCollection, usize, &str, &str),
        render_data: &mut HashMap<String, LayerRenderData>,
        canonical: &CanonicalTileID,
    ) -> Result<(), ExceptionPtr> {
        let mut bucket = CircleBucket::new(&self.layer_properties_map, self.mode, self.zoom);
        bucket.set_retain_features_by_id(self.retain_features_by_id);
        bucket.reserve_features(self.features.len());

        for circle_feature in &self.features {
            let i = circle_feature.i;
            let feature = source_layer.get_feature(i)?;
            let geometries = feature.get_geometries();
            feature.check()?;
            let vertex_offset = bucket.vertices.len();

            Self::add_circle(&mut bucket, self.mode, &*feature, geometries, i, circle_feature.sort_key, canonical);
            feature.check()?;

            if self.retain_features_by_id {
                let vertex_count = bucket.vertices.len() - vertex_offset;
                if vertex_count > 0 {
                    bucket.retain_feature(&*feature, vertex_offset, vertex_count);
                }
            }

            insert_feature(geometries, i, &self.source_layer_id, &self.bucket_leader_id);
        }

        if !bucket.has_data() {
            return Ok(());
        }

        let bucket: BucketPtr = Arc::new(bucket);
        for (layer_id, layer_properties) in &self.layer_properties_map {
            render_data.entry(layer_id.clone()).or_insert_with(|| LayerRenderData {
                bucket: bucket.clone(),
                layer_properties: layer_properties.clone(),
            });
        }
        Ok(())
    }

    /// `addCircle(bucket, feature, geometry, featureIndex, sortKey, canonical)`.
    fn add_circle(
        bucket: &mut CircleBucket,
        mode: MapMode,
        feature: &dyn GeometryTileFeature,
        geometry: &GeometryCollection,
        feature_index: usize,
        sort_key: f32,
        canonical: &CanonicalTileID,
    ) {
        const VERTEX_LENGTH: usize = 4;

        for circle in geometry.iter() {
            for point in circle {
                let x = point.x;
                let y = point.y;

                // Do not include points that are outside the tile
                // boundaries. Include all points in Still mode. You need to
                // include points from neighbouring tiles so that they are
                // not clipped at tile boundaries.
                if mode == MapMode::Continuous && (x < 0 || x as i32 >= EXTENT || y < 0 || y as i32 >= EXTENT) {
                    continue;
                }

                let needs_segment = match bucket.segments.last() {
                    None => true,
                    Some(last) => last.vertex_length + VERTEX_LENGTH > u16::MAX as usize,
                };
                if needs_segment {
                    // Move to a new segments because the old one can't hold
                    // the geometry.
                    let mut segment =
                        mln_gfx::shaders::segment::SegmentBase::new(bucket.vertices.len(), bucket.triangles.len());
                    segment.sort_key = sort_key;
                    bucket.segments.push(segment);
                }

                // this geometry will be of the Point type, and we'll derive
                // two triangles from it.
                //
                // ┌─────────┐
                // │ 4     3 │
                // │         │
                // │ 1     2 │
                // └─────────┘
                //
                bucket.vertices.push(CircleBucket::vertex(*point, -1.0, -1.0)); // 1
                bucket.vertices.push(CircleBucket::vertex(*point, 1.0, -1.0)); // 2
                bucket.vertices.push(CircleBucket::vertex(*point, 1.0, 1.0)); // 3
                bucket.vertices.push(CircleBucket::vertex(*point, -1.0, 1.0)); // 4

                let segment = bucket.segments.last_mut().expect("segment");
                debug_assert!(segment.vertex_length <= u16::MAX as usize);
                let index = segment.vertex_length as u16;

                // 1, 2, 3
                // 1, 4, 3
                // (`index + 1` etc. are `int`s narrowed to uint16_t)
                bucket.triangles.extend_from_slice(&[
                    index,
                    (index as i32 + 1) as u16,
                    (index as i32 + 2) as u16,
                    index,
                    (index as i32 + 3) as u16,
                    (index as i32 + 2) as u16,
                ]);

                segment.vertex_length += VERTEX_LENGTH;
                segment.index_length += 6;
            }
        }

        let length = bucket.vertices.len();
        for binders in bucket.paint_property_binders.values_mut() {
            binders.get_mut_unpoisoned().populate_vertex_vectors(
                feature,
                length,
                feature_index,
                &Default::default(),
                None,
                canonical,
                &EMPTY_FORMATTED_SECTION,
            );
        }
    }
}

impl Layout for CircleLayout {
    fn has_dependencies(&self) -> bool {
        CircleLayout::has_dependencies(self)
    }

    fn create_bucket(
        &mut self,
        _pattern_positions: &ImagePositions,
        feature_index: &mut FeatureIndex,
        render_data: &mut HashMap<String, LayerRenderData>,
        _first_load: bool,
        _show_collision_boxes: bool,
        canonical: &CanonicalTileID,
    ) -> Result<(), ExceptionPtr> {
        let source_layer = self.source_layer.clone().expect("CircleLayout: source layer");
        let layer = source_layer.layer()?;
        self.create_bucket_with(
            &*layer,
            &mut |geometries, i, source_layer_id, bucket_leader_id| {
                feature_index.insert(geometries, i, source_layer_id, bucket_leader_id)
            },
            render_data,
            canonical,
        )
    }
}
