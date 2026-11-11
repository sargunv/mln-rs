// Ports src/mln/renderer/buckets/line_bucket.hpp and line_bucket.cpp

use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};

use geometry::Point;
use mln_gfx::UploadPass;
use mln_gfx::polyline_generator::{PolylineGenerator, PolylineGeneratorDistances, PolylineGeneratorOptions, dist};
use mln_gfx::shaders::segment::SegmentBase;
use mln_style::expression::{Image, Value};
use mln_style::layer_properties::{LayerProperties, get_evaluated};
use mln_style::layers::line_layer_properties::*;
use mln_style::properties::Property;
use mln_style_types::image::ImagePositions;
use mln_style_types::{LineCapType, LineJoinType};
use mln_tile_id::CanonicalTileID;
use mln_tile_id::geometry_tile_data::{GeometryCollection, GeometryCoordinates, GeometryTileFeature};
use mln_util::Color;
use mln_util::exception::ExceptionPtr;
use mln_util::feature::numeric_value_f64;
use mln_util::geometry::FeatureType;
use mln_util::immutable::Immutable;
use mln_util::logging::{Event, Log};
use mln_util::sync::RwLockExt;
use mln_vec_map::VecMap;
use serde::{Deserialize, Serialize};

use crate::bucket::{Bucket, BucketBase};
use crate::bucket_geometry::GeometryVec;
use crate::layout::pattern_layout::PatternLayerMap;
use crate::paint_property_binder::{CrossFadedPaintPropertyBinder, PaintPropertyBinder};
use crate::render_layer::RenderLayer;

crate::paint_property_binders!(
    /// `LineBinders = PaintPropertyBinders<LinePaintProperties::DataDrivenProperties>`.
    pub struct LineBinders for LinePaintProperties::PossiblyEvaluated {
        line_blur: LineBlur => PaintPropertyBinder<f32>,
        line_color: LineColor => PaintPropertyBinder<Color>,
        line_floor_width: LineFloorWidth => PaintPropertyBinder<f32>,
        line_gap_width: LineGapWidth => PaintPropertyBinder<f32>,
        line_offset: LineOffset => PaintPropertyBinder<f32>,
        line_opacity: LineOpacity => PaintPropertyBinder<f32>,
        line_pattern: LinePattern => CrossFadedPaintPropertyBinder<Image>,
        line_width: LineWidth => PaintPropertyBinder<f32>,
    }
);

/// `LineLayoutVertex = gfx::Vertex<TypeList<attributes::pos_normal,
/// attributes::data<uint8_t, 4>>>` (8 bytes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct LineLayoutVertex {
    pub a1: [i16; 2],
    pub a2: [u8; 4],
}

#[derive(Serialize, Deserialize)]
pub struct LineBucket {
    pub base: BucketBase,
    pub layout: LineLayoutProperties::PossiblyEvaluated,
    pub vertices: GeometryVec<LineLayoutVertex>,
    /// `gfx::IndexVector<gfx::Triangles>`.
    pub triangles: GeometryVec<u16>,
    pub segments: Vec<SegmentBase>,
    /// Behind `RwLock`s: `update` (feature state) changes them while the
    /// bucket is shared.
    pub paint_property_binders: VecMap<String, RwLock<LineBinders>>,
    zoom: f32,
    overscaling: u32,
}

/// Scale the extrusion vector so that the normal length is this value.
/// Contains the "texture" normals (-1..1). This is distinct from the
/// extrude normals for line joins, because the x-value remains 0 for the
/// texture normal array, while the extrude normal actually moves the vertex
/// to create the acute/bevelled line join.
pub const EXTRUDE_SCALE: i8 = 63;

static WARNED_INVALID_GEOMETRY: AtomicBool = AtomicBool::new(false);

impl LineBucket {
    /// `LineBucket(layout, layerPaintProperties, zoom, overscaling)`.
    pub fn new(
        layout: LineLayoutProperties::PossiblyEvaluated,
        layer_paint_properties: &VecMap<String, Immutable<dyn LayerProperties>>,
        zoom: f32,
        overscaling: u32,
    ) -> Self {
        let mut paint_property_binders = VecMap::new();
        for (id, properties) in layer_paint_properties {
            paint_property_binders.entry(id.clone()).or_insert_with(|| {
                RwLock::new(LineBinders::new(get_evaluated::<LineLayerProperties>(properties), zoom))
            });
        }
        LineBucket {
            base: BucketBase::default(),
            layout,
            vertices: GeometryVec::default(),
            triangles: GeometryVec::default(),
            segments: Vec::new(),
            paint_property_binders,
            zoom,
            overscaling,
        }
    }

    /// `layoutVertex(p, e, round, up, dir, linesofar = 0)`.
    ///
    /// @param p vertex position
    /// @param e extrude normal
    /// @param round whether the vertex uses a round line cap
    /// @param up whether the line normal points up or down
    /// @param dir direction of the line cap (-1/0/1)
    pub fn layout_vertex(
        p: Point<i16>,
        e: Point<f64>,
        round: bool,
        up: bool,
        dir: i8,
        linesofar: i32,
    ) -> LineLayoutVertex {
        let extrude = |v: f64| -> u8 {
            // add 128 to store a byte in an unsigned byte
            mln_math::clamp_f64((EXTRUDE_SCALE as f64 * v).round() + 128.0, 0.0, 255.0) as u8
        };
        let dir_value: i32 = if dir == 0 {
            0
        } else if dir < 0 {
            -1
        } else {
            1
        };
        LineLayoutVertex {
            a1: [((p.x as i32 * 2) | round as i32) as i16, ((p.y as i32 * 2) | up as i32) as i16],
            a2: [
                extrude(e.x),
                extrude(e.y),
                // Encode the -1/0/1 direction value into the first two bits
                // of .z of a_data. Combine it with the lower 6 bits of
                // `linesofar` (shifted by 2 bites to make room for the
                // direction value). The upper 8 bits of `linesofar` are
                // placed in the `w` component. `linesofar` is scaled down by
                // `LINE_DISTANCE_SCALE` so that we can store longer distances
                // while sacrificing precision.
                ((dir_value + 1) | ((linesofar & 0x3F) << 2)) as u8,
                (linesofar >> 6) as u8,
            ],
        }
    }

    fn add_geometry(
        &mut self,
        coordinates: &GeometryCoordinates,
        feature: &dyn GeometryTileFeature,
        canonical: &CanonicalTileID,
    ) {
        // Ignore empty coordinates.
        if coordinates.is_empty() {
            return;
        }

        let mut options = PolylineGeneratorOptions { type_: feature.get_type(), ..Default::default() };
        let len = {
            let mut l = coordinates.len();
            // If the line has duplicate vertices at the end, adjust length to remove them.
            while l >= 2 && coordinates[l - 1] == coordinates[l - 2] {
                l -= 1;
            }
            l
        };

        let first = {
            let mut i = 0;
            // If the line has duplicate vertices at the start, adjust index to remove them.
            while i + 1 < len && coordinates[i] == coordinates[i + 1] {
                i += 1;
            }
            i
        };

        // Ignore invalid geometry.
        let min_len = if options.type_ == FeatureType::Polygon { 3 } else { 2 };
        if len < min_len {
            // Warn once, but only if the source geometry is invalid, not if
            // de-duplication made it invalid. This happens, e.g., when
            // attempting to use a GeoJSON `MultiPoint` or
            // `MLNPointCollectionFeature` as the source for a line layer.
            // Unfortunately, we cannot show the layer or source name from
            // here.
            if coordinates.len() < min_len && !WARNED_INVALID_GEOMETRY.swap(true, Ordering::Relaxed) {
                Log::warning(Event::General, "Invalid geometry in line layer");
            }
            return;
        }

        if let Some(clip_start) = feature.get_value("mapbox_clip_start") {
            if let Some(clip_end) = feature.get_value("mapbox_clip_end") {
                let start_value = numeric_value_f64(&clip_start);
                let end_value = numeric_value_f64(&clip_end);
                if let (Some(start_value), Some(end_value)) = (start_value, end_value) {
                    if start_value < end_value {
                        let mut total_length = 0.0;
                        for i in first..len - 1 {
                            total_length += dist(coordinates[i], coordinates[i + 1]);
                        }

                        options.clip_distances =
                            Some(PolylineGeneratorDistances::new(start_value, end_value, total_length));
                    }
                }
            }
        }

        options.join_type = self.layout.evaluate_canonical::<LineJoin>(self.zoom, feature, canonical);
        options.miter_limit =
            if options.join_type == LineJoinType::Bevel { 1.05 } else { *self.layout.get::<LineMiterLimit>() };
        options.begin_cap = *self.layout.get::<LineCap>();
        options.end_cap =
            if options.type_ == FeatureType::Polygon { LineCapType::Butt } else { *self.layout.get::<LineCap>() };
        options.round_limit = *self.layout.get::<LineRoundLimit>();
        options.overscaling = self.overscaling;

        let mut generator = PolylineGenerator::new(
            self.vertices.get_mut(),
            LineBucket::layout_vertex,
            &mut self.segments,
            &SegmentBase::new,
            |segment: &mut SegmentBase| segment,
            self.triangles.get_mut(),
        );
        generator.generate(coordinates, &options);
    }
}

/// The anonymous-namespace `get<Property>(evaluated, id, binders)` of
/// line_bucket.cpp.
fn get_or_max(evaluated_constant: Option<f32>, default_value: f32, statistics_max: Option<Option<f32>>) -> f32 {
    match statistics_max {
        Some(Some(max)) => max,
        _ => evaluated_constant.unwrap_or(default_value),
    }
}

static EMPTY_FORMATTED_SECTION: Value = Value::Null;

impl Bucket for LineBucket {
    fn base(&self) -> &BucketBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut BucketBase {
        &mut self.base
    }

    fn add_feature(
        &mut self,
        feature: &dyn GeometryTileFeature,
        geometry_collection: &GeometryCollection,
        pattern_positions: &ImagePositions,
        pattern_dependencies: &PatternLayerMap,
        index: usize,
        canonical: &CanonicalTileID,
    ) -> Result<(), ExceptionPtr> {
        let vertex_offset = self.vertices.len();

        for line in geometry_collection.iter() {
            self.add_geometry(line, feature, canonical);
        }

        if self.base.retain_features_by_id {
            let vertex_count = self.vertices.len() - vertex_offset;
            if vertex_count > 0 {
                self.retain_feature(feature, vertex_offset, vertex_count);
            }
        }

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
        Ok(())
    }

    fn upload(&self, _upload_pass: &dyn UploadPass) {
        self.base.uploaded.store(true, Ordering::SeqCst);
    }

    fn has_data(&self) -> bool {
        !self.segments.is_empty()
    }

    fn get_query_radius(&self, layer: &dyn RenderLayer) -> f32 {
        let evaluated = get_evaluated::<LineLayerProperties>(&layer.base().evaluated_properties);
        let binders = self.paint_property_binders.get(layer.get_id());
        let translate = evaluated.get::<LineTranslate>();
        let offset = get_or_max(
            evaluated.get::<LineOffset>().constant(),
            LineOffset::default_value(),
            binders.map(|b| b.read_unpoisoned().line_offset.statistics.max()),
        );
        let mut line_width = get_or_max(
            evaluated.get::<LineWidth>().constant(),
            LineWidth::default_value(),
            binders.map(|b| b.read_unpoisoned().line_width.statistics.max()),
        );
        let gap_width = get_or_max(
            evaluated.get::<LineGapWidth>().constant(),
            LineGapWidth::default_value(),
            binders.map(|b| b.read_unpoisoned().line_gap_width.statistics.max()),
        );
        if gap_width != 0.0 {
            line_width = gap_width + 2.0 * line_width;
        }

        line_width / 2.0 + offset.abs() + (translate[0] * translate[0] + translate[1] * translate[1]).sqrt()
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
