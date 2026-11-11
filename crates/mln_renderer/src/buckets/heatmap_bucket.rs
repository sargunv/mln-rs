// Ports src/mln/renderer/buckets/heatmap_bucket.hpp and heatmap_bucket.cpp
//
// As in circle_bucket.rs, the vertex/index vectors are plain `Vec`s (the
// bucket is built on the worker thread and shared with `Arc`); the render
// layer copies them into the shared gfx vectors its drawables reference.

use std::sync::RwLock;
use std::sync::atomic::Ordering;

use mln_gfx::UploadPass;
use mln_gfx::shaders::segment::SegmentBase;
use mln_map_types::MapMode;
use mln_style::expression::Value;
use mln_style::layer_properties::{LayerProperties, get_evaluated};
use mln_style::layers::heatmap_layer_properties::*;
use mln_style_types::image::ImagePositions;
use mln_tile_id::CanonicalTileID;
use mln_tile_id::geometry_tile_data::{GeometryCollection, GeometryCoordinate, GeometryTileFeature};
use mln_util::constants::EXTENT;
use mln_util::exception::ExceptionPtr;
use mln_util::immutable::Immutable;
use mln_util::sync::RwLockExt;
use mln_vec_map::VecMap;
use serde::{Deserialize, Serialize};

use crate::bucket::{Bucket, BucketBase};
use crate::bucket_geometry::GeometryVec;
use crate::bucket_parameters::BucketParameters;
use crate::layout::pattern_layout::PatternLayerMap;
use crate::paint_property_binder::PaintPropertyBinder;
use crate::render_layer::RenderLayer;

crate::paint_property_binders!(
    /// `HeatmapBinders = PaintPropertyBinders<HeatmapPaintProperties::DataDrivenProperties>`.
    pub struct HeatmapBinders for HeatmapPaintProperties::PossiblyEvaluated {
        heatmap_radius: HeatmapRadius => PaintPropertyBinder<f32>,
        heatmap_weight: HeatmapWeight => PaintPropertyBinder<f32>,
    }
);

/// `HeatmapLayoutVertex = gfx::Vertex<TypeList<attributes::pos>>` (also
/// `HeatmapTextureLayoutVertex`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct HeatmapLayoutVertex {
    pub a1: [i16; 2],
}

pub type HeatmapTextureLayoutVertex = HeatmapLayoutVertex;

#[derive(Serialize, Deserialize)]
pub struct HeatmapBucket {
    pub base: BucketBase,
    pub vertices: GeometryVec<HeatmapLayoutVertex>,
    /// `gfx::IndexVector<gfx::Triangles>`.
    pub triangles: GeometryVec<u16>,
    pub segments: Vec<SegmentBase>,
    /// Behind `RwLock`s like the other buckets' binders (the bucket is
    /// shared; C++ heatmap buckets have no feature-state update).
    pub paint_property_binders: VecMap<String, RwLock<HeatmapBinders>>,
    pub mode: MapMode,
}

static EMPTY_FORMATTED_SECTION: Value = Value::Null;

impl HeatmapBucket {
    /// `HeatmapBucket(const BucketParameters&, const std::vector<Immutable<style::LayerProperties>>&)`.
    pub fn new(parameters: &BucketParameters, layers: &[Immutable<dyn LayerProperties>]) -> Self {
        let mut paint_property_binders = VecMap::new();
        for layer in layers {
            // `emplace` keeps the first entry for an id.
            paint_property_binders.entry(layer.base_impl.id.clone()).or_insert_with(|| {
                RwLock::new(HeatmapBinders::new(
                    get_evaluated::<HeatmapLayerProperties>(layer),
                    f32::from(parameters.tile_id.overscaled_z),
                ))
            });
        }
        HeatmapBucket {
            base: BucketBase::default(),
            vertices: GeometryVec::default(),
            triangles: GeometryVec::default(),
            segments: Vec::new(),
            paint_property_binders,
            mode: parameters.mode,
        }
    }

    /// `vertex(Point<int16_t> p, float ex, float ey)`: `p.x * 2` is an
    /// `int`, plus a `float`, converted to `int16_t` (truncation).
    pub fn vertex(p: GeometryCoordinate, ex: f32, ey: f32) -> HeatmapLayoutVertex {
        HeatmapLayoutVertex {
            a1: [
                ((p.x as i32 * 2) as f32 + ((ex + 1.0) / 2.0)) as i32 as i16,
                ((p.y as i32 * 2) as f32 + ((ey + 1.0) / 2.0)) as i32 as i16,
            ],
        }
    }

    /// `textureVertex(Point<int16_t> p)`.
    pub fn texture_vertex(p: GeometryCoordinate) -> HeatmapTextureLayoutVertex {
        HeatmapLayoutVertex { a1: [p.x, p.y] }
    }
}

impl Bucket for HeatmapBucket {
    fn base(&self) -> &BucketBase {
        &self.base
    }

    fn base_mut(&mut self) -> &mut BucketBase {
        &mut self.base
    }

    fn add_feature(
        &mut self,
        feature: &dyn GeometryTileFeature,
        geometry: &GeometryCollection,
        _image_positions: &ImagePositions,
        _patterns: &PatternLayerMap,
        feature_index: usize,
        canonical: &CanonicalTileID,
    ) -> Result<(), ExceptionPtr> {
        const VERTEX_LENGTH: usize = 4;

        for points in geometry.iter() {
            for &point in points {
                let x = point.x;
                let y = point.y;

                // Do not include points that are outside the tile boundaries.
                if x < 0 || i32::from(x) >= EXTENT || y < 0 || i32::from(y) >= EXTENT {
                    continue;
                }

                if self
                    .segments
                    .last()
                    .is_none_or(|segment| segment.vertex_length + VERTEX_LENGTH > usize::from(u16::MAX))
                {
                    // Move to a new segments because the old one can't hold
                    // the geometry.
                    self.segments.push(SegmentBase::new(self.vertices.len(), self.triangles.len()));
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
                self.vertices.get_mut().push(HeatmapBucket::vertex(point, -1.0, -1.0)); // 1
                self.vertices.get_mut().push(HeatmapBucket::vertex(point, 1.0, -1.0)); // 2
                self.vertices.get_mut().push(HeatmapBucket::vertex(point, 1.0, 1.0)); // 3
                self.vertices.get_mut().push(HeatmapBucket::vertex(point, -1.0, 1.0)); // 4

                let segment = self.segments.last_mut().expect("segment");
                debug_assert!(segment.vertex_length <= usize::from(u16::MAX));
                let index = segment.vertex_length as u16;

                // 1, 2, 3
                // 1, 4, 3
                self.triangles.get_mut().extend_from_slice(&[index, index + 1, index + 2]);
                self.triangles.get_mut().extend_from_slice(&[index, index + 3, index + 2]);

                segment.vertex_length += VERTEX_LENGTH;
                segment.index_length += 6;
            }
        }

        let length = self.vertices.len();
        for binders in self.paint_property_binders.values_mut() {
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
        Ok(())
    }

    fn upload(&self, _upload_pass: &dyn UploadPass) {
        self.base.uploaded.store(true, Ordering::SeqCst);
    }

    fn has_data(&self) -> bool {
        !self.segments.is_empty()
    }

    fn get_query_radius(&self, _layer: &dyn RenderLayer) -> f32 {
        0.0
    }
}
