// Ports src/mln/renderer/buckets/fill_bucket.hpp and fill_bucket.cpp
// (`MLN_TRIANGULATE_FILL_OUTLINES` = 1, as for the WebGPU backend: the
// outlines are generated both as triangulated lines and as line primitives).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};

use mln_gfx::UploadPass;
use mln_gfx::shaders::segment::SegmentBase;
use mln_style::expression::{Image, Value};
use mln_style::layer_properties::{LayerProperties, get_evaluated};
use mln_style::layers::fill_layer_properties::*;
use mln_style_types::image::ImagePositions;
use mln_tile_id::CanonicalTileID;
use mln_tile_id::geometry_tile_data::{GeometryCollection, GeometryCoordinate, GeometryTileFeature};
use mln_util::Color;
use mln_util::exception::ExceptionPtr;
use mln_util::immutable::Immutable;
use mln_util::logging::{Event, Log};
use mln_util::serialization::sorted;
use mln_util::sync::RwLockExt;
use mln_vec_map::VecMap;
use serde::{Deserialize, Serialize};

use crate::bucket::{Bucket, BucketBase};
use crate::bucket_geometry::GeometryVec;
use crate::buckets::line_bucket::LineLayoutVertex;
use crate::gfx::fill_generator::generate_fill_and_outline_buffers;
use crate::layout::pattern_layout::PatternLayerMap;
use crate::paint_property_binder::{CrossFadedPaintPropertyBinder, PaintPropertyBinder};
use crate::render_layer::RenderLayer;

crate::paint_property_binders!(
    /// `FillBinders = PaintPropertyBinders<FillPaintProperties::DataDrivenProperties>`.
    pub struct FillBinders for FillPaintProperties::PossiblyEvaluated {
        fill_color: FillColor => PaintPropertyBinder<Color>,
        fill_opacity: FillOpacity => PaintPropertyBinder<f32>,
        fill_outline_color: FillOutlineColor => PaintPropertyBinder<Color>,
        fill_pattern: FillPattern => CrossFadedPaintPropertyBinder<Image>,
    }
);

/// `FillLayoutVertex = gfx::Vertex<TypeList<attributes::pos>>`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct FillLayoutVertex {
    pub a1: [i16; 2],
}

#[derive(Serialize, Deserialize)]
pub struct FillBucket {
    pub base: BucketBase,

    // MLN_TRIANGULATE_FILL_OUTLINES
    pub line_vertices: GeometryVec<LineLayoutVertex>,
    /// `gfx::IndexVector<gfx::Triangles>`.
    pub line_indexes: GeometryVec<u16>,
    pub line_segments: Vec<SegmentBase>,

    /// `gfx::IndexVector<gfx::Lines>`.
    pub basic_lines: GeometryVec<u16>,
    pub basic_line_segments: Vec<SegmentBase>,

    pub vertices: GeometryVec<FillLayoutVertex>,
    /// `gfx::IndexVector<gfx::Triangles>`.
    pub triangles: GeometryVec<u16>,
    pub triangle_segments: Vec<SegmentBase>,

    /// Behind `RwLock`s: `update` (feature state) changes them while the
    /// bucket is shared.
    pub paint_property_binders: VecMap<String, RwLock<FillBinders>>,

    #[serde(with = "sorted::map")]
    sdf_patterns: HashMap<String, bool>,
    #[serde(with = "sorted::set")]
    mixed_sdf_pattern_layers: HashSet<String>,
}

static EMPTY_FORMATTED_SECTION: Value = Value::Null;

impl FillBucket {
    /// `FillBucket(layout, layerPaintProperties, zoom, overscaling)`.
    pub fn new(
        _layout: &FillLayoutProperties::PossiblyEvaluated,
        layer_paint_properties: &VecMap<String, Immutable<dyn LayerProperties>>,
        zoom: f32,
        _overscaling: u32,
    ) -> Self {
        let mut paint_property_binders = VecMap::new();
        for (id, properties) in layer_paint_properties {
            paint_property_binders.entry(id.clone()).or_insert_with(|| {
                RwLock::new(FillBinders::new(get_evaluated::<FillLayerProperties>(properties), zoom))
            });
        }
        FillBucket {
            base: BucketBase::default(),
            line_vertices: GeometryVec::default(),
            line_indexes: GeometryVec::default(),
            line_segments: Vec::new(),
            basic_lines: GeometryVec::default(),
            basic_line_segments: Vec::new(),
            vertices: GeometryVec::default(),
            triangles: GeometryVec::default(),
            triangle_segments: Vec::new(),
            paint_property_binders,
            sdf_patterns: HashMap::new(),
            mixed_sdf_pattern_layers: HashSet::new(),
        }
    }

    /// `layoutVertex(Point<int16_t> p)`.
    pub fn layout_vertex(p: GeometryCoordinate) -> FillLayoutVertex {
        FillLayoutVertex { a1: [p.x, p.y] }
    }

    fn generate_buffers(&mut self, geometry: &GeometryCollection) -> Result<(), ExceptionPtr> {
        generate_fill_and_outline_buffers(
            geometry,
            self.vertices.get_mut(),
            self.triangles.get_mut(),
            &mut self.triangle_segments,
            self.line_vertices.get_mut(),
            self.line_indexes.get_mut(),
            &mut self.line_segments,
            self.basic_lines.get_mut(),
            &mut self.basic_line_segments,
        )
        .map_err(|e| Arc::new(e) as ExceptionPtr)
    }

    fn populate_binders(
        &mut self,
        feature: &dyn GeometryTileFeature,
        pattern_positions: &ImagePositions,
        pattern_dependencies: &PatternLayerMap,
        index: usize,
        canonical: &CanonicalTileID,
    ) {
        let length = self.vertices.len();
        for (id, binders) in self.paint_property_binders.iter_mut() {
            binders.get_mut_unpoisoned().populate_vertex_vectors(
                feature,
                length,
                index,
                pattern_positions,
                pattern_dependencies.get(id),
                canonical,
                &EMPTY_FORMATTED_SECTION,
            );
        }
    }
}

impl Bucket for FillBucket {
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
        pattern_positions: &ImagePositions,
        pattern_dependencies: &PatternLayerMap,
        index: usize,
        canonical: &CanonicalTileID,
    ) -> Result<(), ExceptionPtr> {
        let vertex_offset = self.vertices.len();

        self.generate_buffers(geometry)?;
        self.populate_binders(feature, pattern_positions, pattern_dependencies, index, canonical);

        if self.base.retain_features_by_id {
            let vertex_count = self.vertices.len() - vertex_offset;
            if vertex_count > 0 {
                self.retain_feature(feature, vertex_offset, vertex_count);
            }
        }
        Ok(())
    }

    fn record_sdf_pattern(&mut self, layer_id: &str, sdf: bool) {
        match self.sdf_patterns.get(layer_id) {
            None => {
                self.sdf_patterns.insert(layer_id.to_owned(), sdf);
            }
            Some(&existing) => {
                if existing != sdf && self.mixed_sdf_pattern_layers.insert(layer_id.to_owned()) {
                    Log::warning(
                        Event::Style,
                        format!(
                            "Style sheet warning: Cannot mix SDF and non-SDF fill patterns in layer \"{layer_id}\""
                        ),
                    );
                }
            }
        }
    }

    fn is_sdf_pattern(&self, layer_id: &str) -> bool {
        self.sdf_patterns.get(layer_id).copied().unwrap_or(false)
    }

    fn upload(&self, _upload_pass: &dyn UploadPass) {
        self.base.uploaded.store(true, Ordering::SeqCst);
    }

    fn has_data(&self) -> bool {
        !self.triangle_segments.is_empty() || !self.basic_line_segments.is_empty()
    }

    fn get_query_radius(&self, layer: &dyn RenderLayer) -> f32 {
        let evaluated = get_evaluated::<FillLayerProperties>(&layer.base().evaluated_properties);
        let translate = evaluated.get::<FillTranslate>();
        (translate[0] * translate[0] + translate[1] * translate[1]).sqrt()
    }

    fn update(
        &self,
        states: &crate::bucket::FeatureStates,
        layer: &dyn mln_tile_id::geometry_tile_data::GeometryTileLayer,
        layer_id: &str,
        image_positions: &ImagePositions,
    ) {
        if let Some(binders) = self.paint_property_binders.get(layer_id) {
            binders.write_unpoisoned().update_vertex_vectors(states, layer, image_positions);
            self.base.uploaded.store(false, Ordering::SeqCst);

            // sharedVertices->updateModified()
            self.base.version.fetch_add(1, Ordering::SeqCst);
        }
    }
}
