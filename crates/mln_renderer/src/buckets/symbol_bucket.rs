// Ports src/mln/renderer/buckets/symbol_bucket.hpp and
// src/mln/renderer/buckets/symbol_bucket.cpp (the non-instanced vertex
// layout: MLN_USE_SYMBOL_INSTANCING is off for WebGPU).
//
// The bucket is built on the worker thread and then shared (`BucketPtr =
// Arc<dyn Bucket>`); placement, cross-tile indexing and sorting change it on
// the render thread afterwards, as C++ does through the shared pointer. The
// state they change lives behind a mutex (`state`); the worker fills it
// through `state_mut` before the bucket is shared.
//
// Not ported: the MLN_SYMBOL_GUARDS `check` (memory-corruption guards).

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock};

use geometry::Point;
use mln_gfx::UploadPass;
use mln_gfx::shaders::segment::SegmentBase;
use mln_style::layer_properties::{LayerProperties, get_evaluated};
use mln_style::layers::symbol_layer_impl::SymbolLayerPaintPropertyOverrides;
use mln_style::layers::symbol_layer_properties::*;
use mln_style::property_expression::PropertyExpression;
use mln_style::property_value::PropertyValue;
use mln_style_types::image::ImagePositions;
use mln_style_types::{TextWritingModeType, VariableAnchorOffsetCollection};
use mln_text::glyph::WritingModeType;
use mln_tile_id::geometry_tile_data::{GeometryCoordinates, GeometryTileFeature, GeometryTileLayer};
use mln_util::Color;
use mln_util::immutable::Immutable;
use mln_util::range::Range;
use mln_util::serialization::payload::pod;
use mln_util::sync::{MutexExt, RwLockExt};
use mln_vec_map::VecMap;
use serde::{Deserialize, Serialize};

use crate::bucket::{Bucket, BucketBase, FeatureStates};
use crate::bucket_geometry::GeometryVec;
use crate::layout::symbol_instance::SymbolInstance;
use crate::paint_property_binder::PaintPropertyBinder;

crate::paint_property_binders!(
    /// `SymbolIconBinders = PaintPropertyBinders<IconPaintProperties::DataDrivenProperties>`.
    pub struct SymbolIconBinders for SymbolPaintProperties::PossiblyEvaluated {
        icon_opacity: IconOpacity => PaintPropertyBinder<f32>,
        icon_color: IconColor => PaintPropertyBinder<Color>,
        icon_halo_color: IconHaloColor => PaintPropertyBinder<Color>,
        icon_halo_width: IconHaloWidth => PaintPropertyBinder<f32>,
        icon_halo_blur: IconHaloBlur => PaintPropertyBinder<f32>,
    }
);

crate::paint_property_binders!(
    /// `SymbolTextBinders = PaintPropertyBinders<TextPaintProperties::DataDrivenProperties>`.
    pub struct SymbolTextBinders for SymbolPaintProperties::PossiblyEvaluated {
        text_opacity: TextOpacity => PaintPropertyBinder<f32>,
        text_color: TextColor => PaintPropertyBinder<Color>,
        text_halo_color: TextHaloColor => PaintPropertyBinder<Color>,
        text_halo_width: TextHaloWidth => PaintPropertyBinder<f32>,
        text_halo_blur: TextHaloBlur => PaintPropertyBinder<f32>,
    }
);

/// `SymbolLayoutAttributes = gfx::Vertex<TypeList<attributes::pos_offset,
/// attributes::data<uint16_t, 4>, attributes::pixeloffset>>`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct SymbolLayoutVertex {
    pub a1: [i16; 4],
    pub a2: [u16; 4],
    pub a3: [i16; 4],
}

/// `SymbolDynamicLayoutAttributes = gfx::Vertex<TypeList<attributes::projected_pos>>`.
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct SymbolDynamicLayoutVertex {
    pub a1: [f32; 3],
}

/// `SymbolOpacityAttributes = gfx::Vertex<TypeList<attributes::fade_opacity>>`.
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct SymbolOpacityVertex {
    pub a1: [f32; 1],
}

/// `CollisionBoxLayoutVertexAttributes`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct CollisionBoxLayoutVertex {
    pub a1: [i16; 2],
    pub a2: [i16; 2],
    pub a3: [i16; 2],
}

/// `CollisionBoxDynamicVertexAttributes`.
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct CollisionBoxDynamicVertex {
    pub a1: [u16; 2],
    pub a2: [f32; 2],
}

pub const MAX_GLYPH_ICON_SIZE: u16 = 255;
pub const SIZE_PACK_FACTOR: u16 = 128;
pub const MAX_PACKED_SIZE: u16 = MAX_GLYPH_ICON_SIZE * SIZE_PACK_FACTOR;

/// `SortKeyRange`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SortKeyRange {
    pub sort_key: f32,
    pub start: usize,
    pub end: usize,
}

/// `ZoomEvaluatedSize`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ZoomEvaluatedSize {
    pub is_zoom_constant: bool,
    pub is_feature_constant: bool,
    pub size_t: f32,
    pub size: f32,
    pub layout_size: f32,
}

/// `SymbolSizeBinder` and its three implementations.
#[derive(Debug, Serialize, Deserialize)]
pub enum SymbolSizeBinder {
    /// `ConstantSymbolSizeBinder`.
    Constant {
        layout_size: f32,
        covering_ranges: Option<(Range<f32>, Range<f32>)>,
        expression: Option<PropertyExpression<f32>>,
    },
    /// `SourceFunctionSymbolSizeBinder`.
    SourceFunction { expression: PropertyExpression<f32>, default_value: f32 },
    /// `CompositeFunctionSymbolSizeBinder`.
    CompositeFunction {
        expression: PropertyExpression<f32>,
        default_value: f32,
        layout_zoom: f32,
        covering_zoom_stops: Range<f32>,
    },
}

impl SymbolSizeBinder {
    /// `SymbolSizeBinder::create(tileZoom, sizeProperty, defaultValue)`.
    pub fn create(tile_zoom: f32, size_property: &PropertyValue<f32>, default_value: f32) -> Self {
        if size_property.is_undefined() {
            return SymbolSizeBinder::Constant { layout_size: default_value, covering_ranges: None, expression: None };
        }
        if size_property.is_constant() {
            return SymbolSizeBinder::Constant {
                layout_size: *size_property.as_constant(),
                covering_ranges: None,
                expression: None,
            };
        }
        let expression = size_property.as_expression().clone();
        if expression.is_feature_constant() {
            let layout_size = expression.evaluate_zoom(tile_zoom + 1.0);
            // A feature-constant expression without a zoom dependency (e.g.
            // one that only depends on the global state) has no zoom curve.
            let covering_ranges = if !expression.is_zoom_constant() {
                let zoom_levels = expression.get_covering_stops(tile_zoom, tile_zoom + 1.0);
                Some((
                    zoom_levels,
                    Range {
                        min: expression.evaluate_zoom(zoom_levels.min),
                        max: expression.evaluate_zoom(zoom_levels.max),
                    },
                ))
            } else {
                None
            };
            SymbolSizeBinder::Constant { layout_size, covering_ranges, expression: Some(expression) }
        } else if expression.is_zoom_constant() {
            SymbolSizeBinder::SourceFunction { expression, default_value }
        } else {
            let covering_zoom_stops = expression.get_covering_stops(tile_zoom, tile_zoom + 1.0);
            SymbolSizeBinder::CompositeFunction {
                expression,
                default_value,
                layout_zoom: tile_zoom + 1.0,
                covering_zoom_stops,
            }
        }
    }

    /// `getVertexSizeData(feature)`.
    pub fn get_vertex_size_data(&self, feature: &dyn GeometryTileFeature) -> Range<f32> {
        match self {
            SymbolSizeBinder::Constant { .. } => Range { min: 0.0, max: 0.0 },
            SymbolSizeBinder::SourceFunction { expression, default_value } => {
                let size = expression.evaluate_feature(feature, *default_value);
                Range { min: size, max: size }
            }
            SymbolSizeBinder::CompositeFunction { expression, default_value, covering_zoom_stops, .. } => Range {
                min: expression.evaluate_zoom_feature(covering_zoom_stops.min, feature, *default_value),
                max: expression.evaluate_zoom_feature(covering_zoom_stops.max, feature, *default_value),
            },
        }
    }

    /// `evaluateForZoom(currentZoom)`.
    pub fn evaluate_for_zoom(&self, current_zoom: f32) -> ZoomEvaluatedSize {
        let unused = 0.0_f32;
        match self {
            SymbolSizeBinder::Constant { layout_size, covering_ranges, expression } => {
                let mut size = *layout_size;
                let is_zoom_constant = !(covering_ranges.is_some() || expression.is_some());
                if let Some((zoom_levels, size_levels)) = covering_ranges {
                    // Even though we could get the exact value of the camera
                    // function at z = currentZoom, we intentionally do not:
                    // instead, we interpolate between the camera function
                    // values at a pair of zoom stops covering [tileZoom,
                    // tileZoom + 1] in order to be consistent with this
                    // restriction on composite functions.
                    let expression = expression.as_ref().expect("expression");
                    let t = mln_math::clamp_f32(expression.interpolation_factor(*zoom_levels, current_zoom), 0.0, 1.0);
                    size = size_levels.min + t * (size_levels.max - size_levels.min);
                } else if let Some(expression) = expression {
                    size = expression.evaluate_zoom(current_zoom);
                }
                ZoomEvaluatedSize {
                    is_zoom_constant,
                    is_feature_constant: true,
                    size_t: unused,
                    size,
                    layout_size: *layout_size,
                }
            }
            SymbolSizeBinder::SourceFunction { .. } => ZoomEvaluatedSize {
                is_zoom_constant: true,
                is_feature_constant: false,
                size_t: unused,
                size: unused,
                layout_size: unused,
            },
            SymbolSizeBinder::CompositeFunction { expression, covering_zoom_stops, .. } => {
                let size_interpolation_t =
                    mln_math::clamp_f32(expression.interpolation_factor(*covering_zoom_stops, current_zoom), 0.0, 1.0);
                ZoomEvaluatedSize {
                    is_zoom_constant: false,
                    is_feature_constant: false,
                    size_t: size_interpolation_t,
                    size: unused,
                    layout_size: unused,
                }
            }
        }
    }
}

/// `PlacedSymbol`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlacedSymbol {
    pub anchor_point: Point<f32>,
    pub segment: usize,
    pub lower_size: f32,
    pub upper_size: f32,
    pub line_offset: [f32; 2],
    pub writing_modes: WritingModeType,
    pub line: GeometryCoordinates,
    pub tile_distances: Vec<f32>,
    pub glyph_offsets: Vec<f32>,
    pub hidden: bool,
    pub start_index: usize,
    /// The crossTileID is only filled/used on the foreground for variable
    /// text anchors
    pub cross_tile_id: u32,
    /// The placedOrientation is only used when symbol layer's property is
    /// set to support placement for orientation variants.
    pub placed_orientation: Option<TextWritingModeType>,
    pub angle: f32,
    /// Reference to placed icon, only applicable for text symbols.
    pub placed_icon_index: Option<usize>,
    pub feature_id: Option<String>,
}

impl PlacedSymbol {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        anchor_point: Point<f32>,
        segment: usize,
        lower_size: f32,
        upper_size: f32,
        line_offset: [f32; 2],
        writing_modes: WritingModeType,
        line: GeometryCoordinates,
        tile_distances: Vec<f32>,
        feature_id: Option<String>,
        placed_icon_index: Option<usize>,
    ) -> Self {
        PlacedSymbol {
            anchor_point,
            segment,
            lower_size,
            upper_size,
            line_offset,
            writing_modes,
            line,
            tile_distances,
            glyph_offsets: Vec::new(),
            hidden: false,
            start_index: 0,
            cross_tile_id: 0,
            placed_orientation: None,
            angle: 0.0,
            placed_icon_index,
            feature_id,
        }
    }
}

/// `SymbolBucket::Buffer`.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Buffer {
    pub attribute_data: GeometryVec<SymbolLayoutVertex>,
    #[serde(with = "pod")]
    pub dynamic_attribute_data: Vec<SymbolDynamicLayoutVertex>,
    #[serde(with = "pod")]
    pub opacity_attribute_data: Vec<SymbolOpacityVertex>,
    /// `TriangleIndexVector` (3 indices per triangle).
    #[serde(with = "pod")]
    pub triangles: Vec<u16>,
    pub segments: Vec<SegmentBase>,
    pub placed_symbols: Vec<PlacedSymbol>,
    /// Bumped by `updateModified` (the drawables re-read changed vectors).
    pub modified: u64,
}

impl Buffer {
    /// `updateModified()`.
    pub fn update_modified(&mut self) {
        self.modified += 1;
    }
}

/// `SymbolBucket::CollisionBuffer` (+ box lines / circle triangles).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CollisionBuffer {
    #[serde(with = "pod")]
    pub vertices: Vec<CollisionBoxLayoutVertex>,
    #[serde(with = "pod")]
    pub dynamic_vertices: Vec<CollisionBoxDynamicVertex>,
    pub segments: Vec<SegmentBase>,
    /// `lines` (box buffers: 2 per line) or `triangles` (circle buffers: 3
    /// per triangle).
    #[serde(with = "pod")]
    pub indices: Vec<u16>,
}

/// The paint property binders of one layer (`SymbolBucket::PaintProperties`).
#[derive(Debug, Serialize, Deserialize)]
pub struct PaintProperties {
    pub icon_binders: SymbolIconBinders,
    pub text_binders: SymbolTextBinders,
}

/// The state of a SymbolBucket that placement and sorting change after the
/// bucket is shared.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SymbolBucketState {
    pub sorted_angle: f32,
    pub static_uploaded: bool,
    pub placement_changes_uploaded: bool,
    pub dynamic_uploaded: bool,
    pub sort_uploaded: bool,
    // Set and used by placement.
    pub just_reloaded: bool,
    pub has_variable_placement: bool,
    pub has_uninitialized_symbols: bool,

    pub symbol_instances: Vec<SymbolInstance>,

    pub text: Buffer,
    pub icon: Buffer,
    pub sdf_icon: Buffer,

    pub icon_collision_box: Option<CollisionBuffer>,
    pub text_collision_box: Option<CollisionBuffer>,
    pub icon_collision_circle: Option<CollisionBuffer>,
    pub text_collision_circle: Option<CollisionBuffer>,

    pub feature_sort_order: Option<Arc<Vec<usize>>>,
}

static MAX_BUCKET_INSTANCE_ID: AtomicU32 = AtomicU32::new(0);

fn next_bucket_instance_id() -> u32 {
    MAX_BUCKET_INSTANCE_ID.fetch_add(1, Ordering::SeqCst) + 1
}

/// `SymbolBucket`.
#[derive(Serialize, Deserialize)]
pub struct SymbolBucket {
    pub base: BucketBase,
    pub layout: Immutable<SymbolLayoutProperties::PossiblyEvaluated>,
    pub bucket_leader_id: String,

    // Flags
    pub icons_need_linear: bool,
    pub sort_features_by_y: bool,
    pub icons_in_text: bool,

    pub sort_key_ranges: Vec<SortKeyRange>,

    pub paint_properties: VecMap<String, RwLock<PaintProperties>>,

    pub text_size_binder: SymbolSizeBinder,
    pub icon_size_binder: SymbolSizeBinder,

    pub tile_pixel_ratio: f32,
    /// Port-only: a bucket another side made gets an id of this side's.
    #[serde(skip, default = "next_bucket_instance_id")]
    pub bucket_instance_id: u32,
    pub allow_vertical_placement: bool,
    pub placement_modes: Vec<TextWritingModeType>,
    #[serde(skip)]
    has_format_section_overrides: OnceLock<bool>,

    /// Bumped whenever the vertex data changes after the bucket is shared
    /// (placement, sorting), so drawables can refresh their attributes.
    #[serde(skip)]
    pub version: AtomicU32,
    /// What changed (the C++ shared vectors' `updateModified`): the
    /// opacity vertices (placement), the dynamic vertices (line labels,
    /// variable anchors), the triangle order (sortFeatures) and the paint
    /// property binders (feature state). RenderSymbolLayer refreshes only
    /// those in its drawables.
    #[serde(skip)]
    pub opacity_version: AtomicU32,
    #[serde(skip)]
    pub dynamic_version: AtomicU32,
    #[serde(skip)]
    pub sort_version: AtomicU32,
    #[serde(skip)]
    pub binder_version: AtomicU32,

    state: Mutex<SymbolBucketState>,
}

impl std::fmt::Debug for SymbolBucket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SymbolBucket").field("bucket_leader_id", &self.bucket_leader_id).finish()
    }
}

impl SymbolBucket {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        layout: Immutable<SymbolLayoutProperties::PossiblyEvaluated>,
        paint_properties: &VecMap<String, Immutable<dyn LayerProperties>>,
        text_size: &PropertyValue<f32>,
        icon_size: &PropertyValue<f32>,
        zoom: f32,
        icons_need_linear: bool,
        sort_features_by_y: bool,
        bucket_name: String,
        symbol_instances: Vec<SymbolInstance>,
        sort_key_ranges: Vec<SortKeyRange>,
        tile_pixel_ratio: f32,
        allow_vertical_placement: bool,
        placement_modes: Vec<TextWritingModeType>,
        icons_in_text: bool,
    ) -> Self {
        let icon_size_data_driven_or_zoom = icon_size.is_expression()
            && (!icon_size.as_expression().is_feature_constant() || !icon_size.as_expression().is_zoom_constant());
        let mut binders = VecMap::new();
        for (id, properties) in paint_properties {
            let evaluated = get_evaluated::<SymbolLayerProperties>(properties);
            binders.entry(id.clone()).or_insert_with(|| {
                RwLock::new(PaintProperties {
                    icon_binders: SymbolIconBinders::new(evaluated, zoom),
                    text_binders: SymbolTextBinders::new(evaluated, zoom),
                })
            });
        }
        SymbolBucket {
            base: BucketBase::default(),
            layout,
            bucket_leader_id: bucket_name,
            icons_need_linear: icons_need_linear || icon_size_data_driven_or_zoom,
            sort_features_by_y,
            icons_in_text,
            sort_key_ranges,
            paint_properties: binders,
            text_size_binder: SymbolSizeBinder::create(zoom, text_size, TextSize::default_value_f32()),
            icon_size_binder: SymbolSizeBinder::create(zoom, icon_size, IconSize::default_value_f32()),
            tile_pixel_ratio,
            bucket_instance_id: next_bucket_instance_id(),
            allow_vertical_placement,
            placement_modes,
            has_format_section_overrides: OnceLock::new(),
            version: AtomicU32::new(0),
            opacity_version: AtomicU32::new(0),
            dynamic_version: AtomicU32::new(0),
            sort_version: AtomicU32::new(0),
            binder_version: AtomicU32::new(0),
            state: Mutex::new(SymbolBucketState { sorted_angle: f32::MAX, symbol_instances, ..Default::default() }),
        }
    }

    /// The mutable state (render thread, after sharing).
    pub fn state(&self) -> MutexGuard<'_, SymbolBucketState> {
        self.state.lock_unpoisoned()
    }

    /// The mutable state while the bucket is still owned (worker thread).
    pub fn state_mut(&mut self) -> &mut SymbolBucketState {
        self.state.get_mut_unpoisoned()
    }

    pub fn bump_version(&self) {
        self.version.fetch_add(1, Ordering::SeqCst);
    }

    pub fn has_text_data(&self) -> bool {
        !self.state().text.segments.is_empty()
    }
    pub fn has_icon_data(&self) -> bool {
        !self.state().icon.segments.is_empty()
    }
    pub fn has_sdf_icon_data(&self) -> bool {
        !self.state().sdf_icon.segments.is_empty()
    }
    pub fn has_icon_collision_box_data(&self) -> bool {
        self.state().icon_collision_box.as_ref().is_some_and(|b| !b.segments.is_empty())
    }
    pub fn has_icon_collision_circle_data(&self) -> bool {
        self.state().icon_collision_circle.as_ref().is_some_and(|b| !b.segments.is_empty())
    }
    pub fn has_text_collision_box_data(&self) -> bool {
        self.state().text_collision_box.as_ref().is_some_and(|b| !b.segments.is_empty())
    }
    pub fn has_text_collision_circle_data(&self) -> bool {
        self.state().text_collision_circle.as_ref().is_some_and(|b| !b.segments.is_empty())
    }

    /// `hasFormatSectionOverrides()`.
    pub fn has_format_section_overrides(&self) -> bool {
        *self
            .has_format_section_overrides
            .get_or_init(|| SymbolLayerPaintPropertyOverrides::has_overrides(self.layout.get::<TextField>()))
    }

    /// `hasVariableTextAnchors()`.
    pub fn has_variable_text_anchors(&self) -> bool {
        let tvao = self.layout.get::<TextVariableAnchorOffset>();
        let has_text_variable_anchor_offset = if tvao.is_constant() {
            tvao.constant().is_some_and(|c: VariableAnchorOffsetCollection| !c.is_empty())
        } else {
            // C++ `isExpression()`: a PossiblyEvaluatedPropertyValue is either.
            true
        };
        has_text_variable_anchor_offset || !self.layout.get::<TextVariableAnchor>().is_empty()
    }

    /// `sortFeatures(angle)`.
    pub fn sort_features(&self, angle: f32) {
        if !self.sort_features_by_y {
            return;
        }
        debug_assert!(angle != f32::MAX);

        let mut state = self.state();
        if state.sorted_angle == angle {
            return;
        }

        state.sorted_angle = angle;

        // The current approach to sorting doesn't sort across text and icon
        // segments so don't try. Sorting within segments separately seemed
        // not to be worth the complexity.
        if state.text.segments.len() > 1 || (state.icon.segments.len() > 1 || state.sdf_icon.segments.len() > 1) {
            return;
        }

        state.sort_uploaded = false;
        self.base.uploaded.store(false, Ordering::SeqCst);
        self.sort_version.fetch_add(1, Ordering::SeqCst);
        self.bump_version();

        let state = &mut *state;
        state.text.triangles.clear();
        state.icon.triangles.clear();
        state.sdf_icon.triangles.clear();

        let mut symbols_sort_order = Vec::with_capacity(state.symbol_instances.len());

        // If the symbols are allowed to overlap sort them by their vertical
        // screen position. The index array buffer is rewritten to reference
        // the (unchanged) vertices in the sorted order.
        for index in get_sorted_symbols(&state.symbol_instances, angle) {
            let symbol_instance = &state.symbol_instances[index];
            symbols_sort_order.push(symbol_instance.get_data_feature_index());

            if let Some(i) = symbol_instance.placed_right_text_index {
                add_placed_symbol(&mut state.text.triangles, &state.text.placed_symbols[i]);
            }
            if let Some(i) = symbol_instance.placed_center_text_index {
                if !symbol_instance.get_single_line() {
                    add_placed_symbol(&mut state.text.triangles, &state.text.placed_symbols[i]);
                }
            }
            if let Some(i) = symbol_instance.placed_left_text_index {
                if !symbol_instance.get_single_line() {
                    add_placed_symbol(&mut state.text.triangles, &state.text.placed_symbols[i]);
                }
            }
            if let Some(i) = symbol_instance.placed_vertical_text_index {
                add_placed_symbol(&mut state.text.triangles, &state.text.placed_symbols[i]);
            }

            let icon_buffer = if symbol_instance.has_sdf_icon() { &mut state.sdf_icon } else { &mut state.icon };
            if let Some(i) = symbol_instance.placed_icon_index {
                let placed = icon_buffer.placed_symbols[i].clone();
                add_placed_symbol(&mut icon_buffer.triangles, &placed);
            }
            if let Some(i) = symbol_instance.placed_vertical_icon_index {
                let placed = icon_buffer.placed_symbols[i].clone();
                add_placed_symbol(&mut icon_buffer.triangles, &placed);
            }
        }

        state.feature_sort_order = Some(Arc::new(symbols_sort_order));
    }

    /// `getSymbols(range)`: indices into `symbolInstances`.
    pub fn get_symbols(&self, range: Option<&SortKeyRange>, count: usize) -> std::ops::Range<usize> {
        match range {
            Some(range) if range.start < range.end && range.end <= count => range.start..range.end,
            _ => 0..count,
        }
    }

    /// The end of `upload`/`updateVertices` bookkeeping: `uploaded = false`
    /// and the buffers' `updateModified`.
    pub fn mark_vertices_updated(&self, update_opacities: bool, dynamic_updated: bool) {
        let mut state = self.state();
        if update_opacities {
            state.placement_changes_uploaded = false;
            self.base.uploaded.store(false, Ordering::SeqCst);
            self.opacity_version.fetch_add(1, Ordering::SeqCst);
        }
        if dynamic_updated {
            state.dynamic_uploaded = false;
            self.base.uploaded.store(false, Ordering::SeqCst);
            self.dynamic_version.fetch_add(1, Ordering::SeqCst);
        }
        if !self.base.uploaded.load(Ordering::SeqCst) {
            state.text.update_modified();
            state.icon.update_modified();
            state.sdf_icon.update_modified();
            self.bump_version();
        }
    }

    /// `layoutAttributes(labelAnchor, o, glyphOffsetY, tx, ty, sizeData,
    /// isSDF, pixelOffset, minFontScale)`.
    #[allow(clippy::too_many_arguments)]
    pub fn layout_attributes(
        label_anchor: Point<f32>,
        o: Point<f32>,
        glyph_offset_y: f32,
        tx: u16,
        ty: u16,
        size_data: &Range<f32>,
        is_sdf: bool,
        pixel_offset: Point<f32>,
        min_font_scale: Point<f32>,
    ) -> SymbolLayoutVertex {
        let a_size_min =
            (MAX_PACKED_SIZE.min(cast_u16(size_data.min * SIZE_PACK_FACTOR as f32)) << 1).wrapping_add(is_sdf as u16);
        let a_size_max = MAX_PACKED_SIZE.min(cast_u16(size_data.max * SIZE_PACK_FACTOR as f32));
        SymbolLayoutVertex {
            // combining pos and offset to reduce number of vertex attributes
            // passed to shader (8 max for some devices)
            a1: [
                cast_i16(label_anchor.x),
                cast_i16(label_anchor.y),
                cast_i16((o.x * 32.0).round()), // use 1/32 pixels for placement
                cast_i16(((o.y + glyph_offset_y) * 32.0).round()),
            ],
            a2: [tx, ty, a_size_min, a_size_max],
            a3: [
                cast_i16(pixel_offset.x * 16.0),
                cast_i16(pixel_offset.y * 16.0),
                cast_i16(min_font_scale.x * 256.0),
                cast_i16(min_font_scale.y * 256.0),
            ],
        }
    }

    /// `dynamicLayoutAttributes(anchorPoint, labelAngle)`.
    pub fn dynamic_layout_attributes(anchor_point: Point<f32>, label_angle: f32) -> SymbolDynamicLayoutVertex {
        SymbolDynamicLayoutVertex { a1: [anchor_point.x, anchor_point.y, label_angle] }
    }

    /// `opacityAttributes(placed, opacity)`.
    pub fn opacity_attributes(placed: bool, opacity: f32) -> SymbolOpacityVertex {
        SymbolOpacityVertex { a1: [(((cast_u8(opacity * 127.0) as u32) << 1) | placed as u32) as f32] }
    }

    /// `collisionLayoutVertex(a, anchor, o)`.
    pub fn collision_layout_vertex(a: Point<f32>, anchor: Point<f32>, o: Point<f32>) -> CollisionBoxLayoutVertex {
        CollisionBoxLayoutVertex {
            a1: [cast_i16(a.x), cast_i16(a.y)],
            a2: [cast_i16(anchor.x), cast_i16(anchor.y)],
            a3: [cast_i16(o.x.round()), cast_i16(o.y.round())],
        }
    }

    /// `collisionDynamicVertex(placed, notUsed, shift)`.
    pub fn collision_dynamic_vertex(placed: bool, not_used: bool, shift: Point<f32>) -> CollisionBoxDynamicVertex {
        CollisionBoxDynamicVertex { a1: [placed as u16, not_used as u16], a2: [shift.x, shift.y] }
    }
}

/// `static_cast<int16_t>(float)` (clang x86-64: truncation toward zero; the
/// values are in range).
fn cast_i16(v: f32) -> i16 {
    libstdcxx::cvt::f64_to_i32(v as f64) as i16
}

/// `static_cast<uint16_t>(float)`.
fn cast_u16(v: f32) -> u16 {
    libstdcxx::cvt::f64_to_i32(v as f64) as u16
}

/// `static_cast<uint8_t>(float)`.
fn cast_u8(v: f32) -> u8 {
    libstdcxx::cvt::f64_to_i32(v as f64) as u8
}

/// `addPlacedSymbol(triangles, placedSymbol)`.
fn add_placed_symbol(triangles: &mut Vec<u16>, placed_symbol: &PlacedSymbol) {
    let end_index = placed_symbol.start_index + placed_symbol.glyph_offsets.len() * 4;
    let mut vertex_index = placed_symbol.start_index;
    while vertex_index < end_index {
        triangles.extend_from_slice(&[(vertex_index + 0) as u16, (vertex_index + 1) as u16, (vertex_index + 2) as u16]);
        triangles.extend_from_slice(&[(vertex_index + 1) as u16, (vertex_index + 2) as u16, (vertex_index + 3) as u16]);
        vertex_index += 4;
    }
}

/// `getSortedSymbols(angle)`: indices of `symbolInstances`, sorted by
/// viewport Y. `std::ranges::sort` is unstable, but the comparator is a
/// strict total order on distinct data feature indices.
pub fn get_sorted_symbols(symbol_instances: &[SymbolInstance], angle: f32) -> Vec<usize> {
    let mut result: Vec<usize> = (0..symbol_instances.len()).collect();
    let sin = angle.sin();
    let cos = angle.cos();
    let rotated = |s: &SymbolInstance| -> i64 {
        let p = s.get_anchor().point;
        (sin * p.x + cos * p.y).round() as i64
    };
    libstdcxx::sort_by(&mut result, |&a, &b| {
        let (sa, sb) = (&symbol_instances[a], &symbol_instances[b]);
        let a_rotated = rotated(sa);
        let b_rotated = rotated(sb);
        if a_rotated != b_rotated {
            return a_rotated < b_rotated;
        }
        sa.get_data_feature_index() > sb.get_data_feature_index() // aRotated == bRotated
    });
    result
}

impl Bucket for SymbolBucket {
    fn base(&self) -> &BucketBase {
        &self.base
    }

    fn base_mut(&mut self) -> &mut BucketBase {
        &mut self.base
    }

    fn upload(&self, _upload_pass: &dyn UploadPass) {
        self.base.uploaded.store(true, Ordering::SeqCst);
        let mut state = self.state();
        state.static_uploaded = true;
        state.placement_changes_uploaded = true;
        state.dynamic_uploaded = true;
        state.sort_uploaded = true;
    }

    fn has_data(&self) -> bool {
        self.has_text_data()
            || self.has_icon_data()
            || self.has_sdf_icon_data()
            || self.has_icon_collision_box_data()
            || self.has_text_collision_box_data()
            || self.has_icon_collision_circle_data()
            || self.has_text_collision_circle_data()
    }

    /// `update(states, layer, layerID, imagePositions)` (feature state).
    /// text-conformance: this is the `Bucket` override (it was an inherent
    /// method, so GeometryTile's `bucket.update` hit the no-op default).
    fn update(
        &self,
        states: &FeatureStates,
        layer: &dyn GeometryTileLayer,
        layer_id: &str,
        image_positions: &ImagePositions,
    ) {
        if let Some(binders) = self.paint_properties.get(layer_id) {
            let mut binders = binders.write_unpoisoned();
            binders.icon_binders.update_vertex_vectors(states, layer, image_positions);
            binders.text_binders.update_vertex_vectors(states, layer, image_positions);
            self.base.uploaded.store(false, Ordering::SeqCst);
            self.binder_version.fetch_add(1, Ordering::SeqCst);
            self.bump_version();
        }
    }
}

/// `TextSize::defaultValue()` / `IconSize::defaultValue()` as `f32`.
trait DefaultValueF32 {
    fn default_value_f32() -> f32;
}

impl DefaultValueF32 for TextSize {
    fn default_value_f32() -> f32 {
        <TextSize as mln_style::properties::Property>::default_value()
    }
}

impl DefaultValueF32 for IconSize {
    fn default_value_f32() -> f32 {
        <IconSize as mln_style::properties::Property>::default_value()
    }
}
