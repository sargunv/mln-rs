// Ports src/mln/renderer/paint_property_binder.hpp and the
// `readDataDrivenPaintProperties` / `readDataDrivenPaintProperty` members of
// include/mln/gfx/vertex_attribute.hpp.
//
// The C++ class hierarchy (`PaintPropertyBinder<T, UniformValueType,
// PossiblyEvaluatedType, As...>` with Constant / SourceFunction /
// CompositeFunction / ConstantCrossFaded / CompositeCrossFaded subclasses)
// becomes two enums: `PaintPropertyBinder<T>` for the
// `PossiblyEvaluatedPropertyValue<T>` properties and
// `CrossFadedPaintPropertyBinder<T>` for the `Faded<T>` (pattern) ones.
// `PaintPropertyBinders<TypeList<Ps...>>` is generated per layer type by
// `paint_property_binders!` (circle, fill, line).
//
// The binders write into the bucket's `InterleavedVertexBuffer`; C++ keeps
// a pointer to it in every binder, here the buffer is passed in. Bytes are
// written with the C++ in-memory layout (native-endian floats/u16s), so the
// buffer is the GPU vertex data as C++ uploads it.
//
// Buckets are shared between the worker and the render thread (`Arc`), but
// the tweakers call `setPatternParameters` every frame: the state that call
// writes (cross-faded binders) is behind a `Mutex`.
//

use std::sync::Mutex;

use mln_expression::range::Range;
use mln_gfx::gfx_types::AttributeDataType;
use mln_gfx::shader_group::StringIDSetsPair;
use mln_gfx::vertex_attribute::{VertexAttribute, VertexAttributeArray};
use mln_gfx::vertex_vector::VertexVectorBasePtr;
use mln_style::CrossfadeParameters;
use mln_style::expression::{EvaluationContext, Image, Value, ValueConverter};
use mln_style::property_expression::PropertyExpression;
use mln_style::renderer::cross_faded_property_evaluator::Faded;
use mln_style::renderer::possibly_evaluated_property_value::{
    PossiblyEvaluatedFadedPropertyValue, PossiblyEvaluatedPropertyValue,
};
use mln_style_types::image::{ImagePosition, ImagePositions};
use mln_tile_id::CanonicalTileID;
use mln_tile_id::geometry_tile_data::{GeometryTileFeature, GeometryTileLayer};
use mln_util::Color;
use mln_util::feature::feature_id_to_string;
use mln_util::serialization::payload;
use mln_util::sync::MutexExt;
use mln_vec_map::VecMap;
use serde::{Deserialize, Serialize};

use crate::bucket_geometry::GeometryVec;
use crate::layout::pattern_layout::PatternDependency;
use crate::paint_property_statistics::{PaintPropertyStatistics, StatisticValue};

/// Maps vertex range to feature index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureVertexRange {
    pub feature_index: usize,
    pub start: usize,
    pub end: usize,
}

/// `std::map<std::string, std::vector<FeatureVertexRange>>`.
pub type FeatureVertexRangeMap = VecMap<String, Vec<FeatureVertexRange>>;

/// `InterleavedVertexBuffer`: the data-driven attribute values of all the
/// binders of one `PaintPropertyBinders`, one interleaved vertex per layout
/// vertex.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct InterleavedVertexBuffer {
    pub stride: usize,
    pub vertex_count: usize,
    /// `gfx::VertexVectorPtr<uint8_t> sharedVertexVector`.
    pub shared_vertex_vector: GeometryVec<u8>,
}

impl InterleavedVertexBuffer {
    /// `set<T>(index, offset, value)`.
    pub fn set(&mut self, index: usize, offset: usize, value: &[u8]) {
        let start = self.stride * index + offset;
        let data = self.shared_vertex_vector.get_mut();
        debug_assert!(start + value.len() <= data.len());
        data[start..start + value.len()].copy_from_slice(value);
    }

    /// `get<T>(index, offset)`, `len` = `sizeof(T)`.
    pub fn get(&self, index: usize, offset: usize, len: usize) -> Vec<u8> {
        let start = self.stride * index + offset;
        let data = self.shared_vertex_vector.data();
        debug_assert!(start + len <= data.len());
        data[start..start + len].to_vec()
    }

    /// `extendVertexFormat<T>()`, with `sizeof(T)` and `alignof(T)`.
    pub fn extend_vertex_format(&mut self, size: usize, alignment: usize) -> usize {
        let mut offset = self.stride & (alignment - 1);
        if offset != 0 {
            offset = alignment - offset;
        }

        let vertex_offset = self.stride + offset;
        self.stride += size + offset;

        vertex_offset
    }
}

/// The value types of data-driven paint properties with a single attribute
/// (`float` and `Color` in circle, fill and line): `attributeValue(v)` and
/// the attribute's type (`A::Type`).
pub trait BinderValue: Clone + Default + ValueConverter + StatisticValue + std::fmt::Debug {
    /// `A::Dimensions` (the floats of `attributeValue`).
    const DIMENSIONS: usize;
    /// `A::Type::DataType`.
    const BASE_DATA_TYPE: AttributeDataType;
    /// `ZoomInterpolatedAttributeType<A::Type>::DataType`.
    const INTERPOLATED_DATA_TYPE: AttributeDataType;

    /// `attributeValue(v)`; only the first `DIMENSIONS` entries are used.
    fn attribute_value(&self) -> [f32; 2];
}

impl BinderValue for f32 {
    const DIMENSIONS: usize = 1;
    const BASE_DATA_TYPE: AttributeDataType = AttributeDataType::Float;
    const INTERPOLATED_DATA_TYPE: AttributeDataType = AttributeDataType::Float2;

    /// `attributeValue(float v)`: `{{v}}`.
    fn attribute_value(&self) -> [f32; 2] {
        [*self, 0.0]
    }
}

/// `packUint8Pair<float>(a, b)`: `static_cast<uint16_t>(a) * 256 +
/// static_cast<uint16_t>(b)` (an `int`), returned as `uint16_t`.
fn pack_uint8_pair(a: f32, b: f32) -> u16 {
    ((a as u16) as i32 * 256 + (b as u16) as i32) as u16
}

impl BinderValue for Color {
    const DIMENSIONS: usize = 2;
    const BASE_DATA_TYPE: AttributeDataType = AttributeDataType::Float2;
    const INTERPOLATED_DATA_TYPE: AttributeDataType = AttributeDataType::Float4;

    /// Encode a four-component color value into a pair of floats. Since
    /// csscolorparser uses 8-bit precision for each color component, for
    /// each float we use the upper 8 bits for one component (e.g. (color.r *
    /// 255) * 256), and the lower 8 for another.
    ///
    /// Also note that colors come in as floats 0..1, so we scale by 255.
    fn attribute_value(&self) -> [f32; 2] {
        [pack_uint8_pair(255.0 * self.r, 255.0 * self.g) as f32, pack_uint8_pair(255.0 * self.b, 255.0 * self.a) as f32]
    }
}

/// True if `expression` reads `feature-state` anywhere.
fn uses_feature_state(expression: &dyn mln_style::expression::Expression) -> bool {
    if expression.get_operator() == "feature-state" {
        return true;
    }
    let mut found = false;
    expression.each_child(&mut |child| found = found || uses_feature_state(child));
    found
}

fn floats_to_bytes(values: &[f32]) -> Vec<u8> {
    bytemuck::cast_slice(values).to_vec()
}

/// The data of a `PaintPropertyBinder` subclass.
#[derive(Debug, Serialize, Deserialize)]
pub enum PaintPropertyBinderKind<T> {
    /// `ConstantPaintPropertyBinder<T, A>`
    Constant { constant: T },
    /// `SourceFunctionPaintPropertyBinder<T, A>`
    SourceFunction { expression: PropertyExpression<T>, default_value: T, feature_map: FeatureVertexRangeMap },
    /// `CompositeFunctionPaintPropertyBinder<T, A>`
    CompositeFunction {
        expression: PropertyExpression<T>,
        default_value: T,
        zoom_range: Range<f32>,
        feature_map: FeatureVertexRangeMap,
    },
}

/// `PaintPropertyBinder<T, T, PossiblyEvaluatedPropertyValue<T>, A>`.
#[derive(Debug, Serialize, Deserialize)]
pub struct PaintPropertyBinder<T> {
    pub kind: PaintPropertyBinderKind<T>,
    pub statistics: PaintPropertyStatistics<T>,
    /// `interleavedVertexBuffer != nullptr` (set by `setInterleavedBuffer`).
    pub has_interleaved_buffer: bool,
    /// `std::numeric_limits<size_t>::max()` until `setInterleavedBuffer`.
    pub vertex_offset: usize,
}

impl<T: BinderValue> PaintPropertyBinder<T> {
    /// `PaintPropertyBinder::create(value, zoom, defaultValue)`.
    pub fn create(value: &PossiblyEvaluatedPropertyValue<T>, zoom: f32, default_value: T) -> Self {
        let kind = match value {
            PossiblyEvaluatedPropertyValue::Constant(constant) => {
                PaintPropertyBinderKind::Constant { constant: constant.clone() }
            }
            PossiblyEvaluatedPropertyValue::Expression(expression) => {
                if expression.is_zoom_constant() {
                    PaintPropertyBinderKind::SourceFunction {
                        expression: expression.clone(),
                        default_value,
                        feature_map: FeatureVertexRangeMap::new(),
                    }
                } else {
                    PaintPropertyBinderKind::CompositeFunction {
                        expression: expression.clone(),
                        default_value,
                        zoom_range: Range::new(zoom, zoom + 1.0),
                        feature_map: FeatureVertexRangeMap::new(),
                    }
                }
            }
        };
        PaintPropertyBinder {
            kind,
            statistics: PaintPropertyStatistics::default(),
            has_interleaved_buffer: false,
            vertex_offset: usize::MAX,
        }
    }

    /// `sizeof(ZoomInterpolatedVertexType<A>)`: `2 * DIMENSIONS` floats.
    fn zoom_interpolated_size() -> usize {
        2 * T::DIMENSIONS * 4
    }

    /// `setInterleavedBuffer(InterleavedVertexBuffer&)`: the function
    /// binders extend the vertex format by their zoom-interpolated vertex;
    /// the constant binder keeps the base no-op.
    pub fn set_interleaved_buffer(&mut self, buffer: &mut InterleavedVertexBuffer) {
        if let PaintPropertyBinderKind::Constant { .. } = self.kind {
            return;
        }
        self.has_interleaved_buffer = true;
        let new_vertex_offset =
            buffer.extend_vertex_format(Self::zoom_interpolated_size(), std::mem::align_of::<f32>());
        if self.vertex_offset == usize::MAX {
            self.vertex_offset = new_vertex_offset;
        }
    }

    /// `getVertexCount()`.
    pub fn get_vertex_count(&self, buffer: &InterleavedVertexBuffer) -> usize {
        if self.has_interleaved_buffer { buffer.vertex_count } else { 0 }
    }

    /// `isInterpolated()`.
    pub fn is_interpolated(&self) -> bool {
        matches!(self.kind, PaintPropertyBinderKind::CompositeFunction { .. })
    }

    /// `populateVertexVector(feature, length, index, patternPositions,
    /// patternDependencies, canonical, formattedSection)`.
    #[allow(clippy::too_many_arguments)]
    pub fn populate_vertex_vector(
        &mut self,
        buffer: &mut InterleavedVertexBuffer,
        feature: &dyn GeometryTileFeature,
        length: usize,
        index: usize,
        canonical: &CanonicalTileID,
        formatted_section: &Value,
    ) {
        let elements = self.get_vertex_count(buffer);
        let vertex_offset = self.vertex_offset;
        match &mut self.kind {
            PaintPropertyBinderKind::Constant { .. } => {}
            PaintPropertyBinderKind::SourceFunction { expression, default_value, feature_map } => {
                let evaluated = expression.evaluate(
                    &EvaluationContext::from_feature(Some(feature))
                        .with_formatted_section(Some(formatted_section))
                        .with_canonical_tile_id(Some(canonical)),
                    default_value.clone(),
                );
                self.statistics.add(&evaluated);
                let value = evaluated.attribute_value();
                // BaseVertex{value}: the base attribute only.
                let bytes = floats_to_bytes(&value[..T::DIMENSIONS]);

                for i in elements..length {
                    buffer.set(i, vertex_offset, &bytes);
                }
                if let Some(id_str) = feature_id_to_string(&feature.get_id()) {
                    feature_map.entry(id_str).or_default().push(FeatureVertexRange {
                        feature_index: index,
                        start: elements,
                        end: length,
                    });
                }
            }
            PaintPropertyBinderKind::CompositeFunction { expression, default_value, zoom_range, feature_map } => {
                let min = expression.evaluate(
                    &EvaluationContext::with_zoom_feature(zoom_range.min, Some(feature))
                        .with_formatted_section(Some(formatted_section))
                        .with_canonical_tile_id(Some(canonical)),
                    default_value.clone(),
                );
                let max = expression.evaluate(
                    &EvaluationContext::with_zoom_feature(zoom_range.max, Some(feature))
                        .with_formatted_section(Some(formatted_section))
                        .with_canonical_tile_id(Some(canonical)),
                    default_value.clone(),
                );
                self.statistics.add(&min);
                self.statistics.add(&max);
                // zoomInterpolatedAttributeValue(attributeValue(min), attributeValue(max))
                let min_value = min.attribute_value();
                let max_value = max.attribute_value();
                let mut value = Vec::with_capacity(2 * T::DIMENSIONS);
                value.extend_from_slice(&min_value[..T::DIMENSIONS]);
                value.extend_from_slice(&max_value[..T::DIMENSIONS]);
                let bytes = floats_to_bytes(&value);

                for i in elements..length {
                    buffer.set(i, vertex_offset, &bytes);
                }
                if let Some(id_str) = feature_id_to_string(&feature.get_id()) {
                    feature_map.entry(id_str).or_default().push(FeatureVertexRange {
                        feature_index: index,
                        start: elements,
                        end: length,
                    });
                }
            }
        }
    }

    /// `updateVertexVectors(states, layer, imagePositions)`.
    pub fn update_vertex_vectors(
        &mut self,
        buffer: &mut InterleavedVertexBuffer,
        states: &crate::bucket::FeatureStates,
        layer: &dyn GeometryTileLayer,
    ) {
        let feature_map = match &self.kind {
            PaintPropertyBinderKind::Constant { .. } => return,
            PaintPropertyBinderKind::SourceFunction { feature_map, .. }
            | PaintPropertyBinderKind::CompositeFunction { feature_map, .. } => feature_map.clone(),
        };
        for (id, state) in states {
            let Some(positions) = feature_map.get(id) else {
                continue;
            };
            for pos in positions {
                // The worker got the feature from the same data. A value
                // that C++ throws for (out of the render call) reads as
                // missing here.
                let Ok(feature) = layer.get_feature(pos.feature_index) else {
                    continue;
                };
                self.update_vertex_vector(buffer, pos.start, pos.end, &*feature, state);
            }
        }
    }

    /// `updateVertexVector(start, end, feature, state)`.
    pub fn update_vertex_vector(
        &mut self,
        buffer: &mut InterleavedVertexBuffer,
        start: usize,
        end: usize,
        feature: &dyn GeometryTileFeature,
        state: &mln_style::expression::expression::FeatureState,
    ) {
        let vertex_offset = self.vertex_offset;
        match &self.kind {
            PaintPropertyBinderKind::Constant { .. } => {}
            PaintPropertyBinderKind::SourceFunction { expression, default_value, .. } => {
                let evaluated = expression.evaluate(
                    &EvaluationContext::from_feature(Some(feature)).with_feature_state(Some(state)),
                    default_value.clone(),
                );
                self.statistics.add(&evaluated);
                let value = evaluated.attribute_value();
                let bytes = floats_to_bytes(&value[..T::DIMENSIONS]);
                for i in start..end {
                    buffer.set(i, vertex_offset, &bytes);
                }
            }
            PaintPropertyBinderKind::CompositeFunction { expression, default_value, zoom_range, .. } => {
                let min = expression.evaluate(
                    &EvaluationContext::with_zoom_feature_state(zoom_range.min, Some(feature), Some(state)),
                    default_value.clone(),
                );
                let max = expression.evaluate(
                    &EvaluationContext::with_zoom_feature_state(zoom_range.max, Some(feature), Some(state)),
                    default_value.clone(),
                );
                self.statistics.add(&min);
                self.statistics.add(&max);
                let mut value = Vec::with_capacity(2 * T::DIMENSIONS);
                value.extend_from_slice(&min.attribute_value()[..T::DIMENSIONS]);
                value.extend_from_slice(&max.attribute_value()[..T::DIMENSIONS]);
                let bytes = floats_to_bytes(&value);
                for i in start..end {
                    buffer.set(i, vertex_offset, &bytes);
                }
            }
        }
    }

    /// `interpolationFactor(currentZoom)` (the single element of the tuple).
    pub fn interpolation_factor(&self, current_zoom: f32) -> f32 {
        match &self.kind {
            PaintPropertyBinderKind::Constant { .. } | PaintPropertyBinderKind::SourceFunction { .. } => 0.0,
            PaintPropertyBinderKind::CompositeFunction { expression, zoom_range, .. } => {
                let possibly_rounded_zoom =
                    if expression.get_use_integer_zoom() { current_zoom.floor() } else { current_zoom };

                mln_math::fmaxf(
                    0.0,
                    mln_math::fminf(1.0, expression.interpolation_factor(*zoom_range, possibly_rounded_zoom)),
                )
            }
        }
    }

    /// `uniformValue(currentValue)` (the single element of the tuple).
    pub fn uniform_value(&self, current_value: &PossiblyEvaluatedPropertyValue<T>) -> T {
        match &self.kind {
            PaintPropertyBinderKind::Constant { constant } => current_value.constant_or(constant),
            // Uniform values for vertex attribute arrays are unused.
            _ => current_value.constant().unwrap_or_default(),
        }
    }

    /// `getVertexValue(index)`: the zoom-interpolated vertex (`2 *
    /// DIMENSIONS` floats).
    pub fn get_vertex_value(&self, buffer: &InterleavedVertexBuffer, index: usize) -> Vec<f32> {
        let n = T::DIMENSIONS;
        match &self.kind {
            PaintPropertyBinderKind::Constant { .. } => vec![0.0; 2 * n],
            PaintPropertyBinderKind::SourceFunction { .. } => {
                let base: Vec<f32> = bytemuck::pod_collect_to_vec(&buffer.get(index, self.vertex_offset, n * 4));
                let mut value = base.clone();
                value.extend_from_slice(&base);
                value
            }
            PaintPropertyBinderKind::CompositeFunction { .. } => {
                bytemuck::pod_collect_to_vec(&buffer.get(index, self.vertex_offset, 2 * n * 4))
            }
        }
    }

    /// `applyPaintProperty<TAttribute>(attrIndex, attrib)`.
    pub fn apply_paint_property(
        &self,
        buffer: &InterleavedVertexBuffer,
        shared: &VertexVectorBasePtr,
        _attr_index: usize,
        attrib: &mut VertexAttribute,
    ) {
        if self.has_interleaved_buffer {
            let raw_size = buffer.stride as u32;
            let data_type = if self.is_interpolated() { T::INTERPOLATED_DATA_TYPE } else { T::BASE_DATA_TYPE };

            debug_assert!(shared.get_raw_count() / buffer.stride.max(1) == self.get_vertex_count(buffer));
            attrib.set_shared_raw_data(Some(shared.clone()), self.vertex_offset as u32, 0, raw_size, data_type);
        } else {
            // getVertexCount() is 0 without an interleaved buffer: nothing
            // to set.
            debug_assert_eq!(self.get_vertex_count(buffer), 0);
        }
    }
}

/// The pattern-related state the tweakers update every frame.
#[derive(Debug, Default)]
pub struct CrossFadedState {
    /// `constantPatternPositions` (Constant) as (posB.tlbr(), posA.tlbr()).
    pub constant_pattern_positions: ([u16; 4], [u16; 4]),
    /// `crossfade` (Composite).
    pub crossfade: CrossfadeParameters,
}

/// The data of a cross-faded `PaintPropertyBinder` subclass.
#[derive(Debug, Serialize, Deserialize)]
pub enum CrossFadedPaintPropertyBinderKind<T> {
    /// `ConstantCrossFadedPaintPropertyBinder<T, A1, A2>`
    Constant { constant: Faded<T> },
    /// `CompositeCrossFadedPaintPropertyBinder<T, A1, A2>`
    Composite {
        expression: PropertyExpression<T>,
        default_value: T,
        zoom_range: Range<f32>,
        /// `gfx::VertexVector<Vertex2> zoomInVertexVector` (ushort4).
        #[serde(with = "payload::pod")]
        zoom_in_vertex_vector: Vec<[u16; 4]>,
        /// `zoomOutVertexVector`.
        #[serde(with = "payload::pod")]
        zoom_out_vertex_vector: Vec<[u16; 4]>,
    },
}

/// `PaintPropertyBinder<T, std::array<uint16_t, 4>,
/// PossiblyEvaluatedPropertyValue<Faded<T>>, A1, A2>` (the `*-pattern`
/// properties; `A1`/`A2` are `ushort4` attributes).
#[derive(Debug, Serialize, Deserialize)]
pub struct CrossFadedPaintPropertyBinder<T> {
    pub kind: CrossFadedPaintPropertyBinderKind<T>,
    /// `PaintPropertyStatistics<T>`: always empty for images.
    #[serde(skip)]
    pub state: Mutex<CrossFadedState>,
    pub has_interleaved_buffer: bool,
    pub vertex_offset: usize,
}

/// `sizeof(ZoomInterpolatedVertexType<A1>)` for `ushort4`: `ushort8`.
const PATTERN_ZOOM_INTERPOLATED_SIZE: usize = 16;

impl CrossFadedPaintPropertyBinder<Image> {
    /// `PaintPropertyBinder::create(value, zoom, defaultValue)`.
    pub fn create(value: &PossiblyEvaluatedFadedPropertyValue<Image>, zoom: f32, default_value: Image) -> Self {
        let kind = match value {
            PossiblyEvaluatedFadedPropertyValue::Constant(constant) => {
                CrossFadedPaintPropertyBinderKind::Constant { constant: constant.clone() }
            }
            PossiblyEvaluatedFadedPropertyValue::Expression(expression) => {
                CrossFadedPaintPropertyBinderKind::Composite {
                    expression: expression.clone(),
                    default_value,
                    zoom_range: Range::new(zoom, zoom + 1.0),
                    zoom_in_vertex_vector: Vec::new(),
                    zoom_out_vertex_vector: Vec::new(),
                }
            }
        };
        CrossFadedPaintPropertyBinder {
            kind,
            state: Mutex::new(CrossFadedState::default()),
            has_interleaved_buffer: false,
            vertex_offset: usize::MAX,
        }
    }

    /// `setInterleavedBuffer`: only the composite binder has vertex data.
    pub fn set_interleaved_buffer(&mut self, buffer: &mut InterleavedVertexBuffer) {
        if let CrossFadedPaintPropertyBinderKind::Constant { .. } = self.kind {
            return;
        }
        self.has_interleaved_buffer = true;
        let new_vertex_offset =
            buffer.extend_vertex_format(PATTERN_ZOOM_INTERPOLATED_SIZE, std::mem::align_of::<u16>());
        if self.vertex_offset == usize::MAX {
            self.vertex_offset = new_vertex_offset;
        }
    }

    pub fn get_vertex_count(&self, buffer: &InterleavedVertexBuffer) -> usize {
        if self.has_interleaved_buffer { buffer.vertex_count } else { 0 }
    }

    /// `isInterpolated()`: the base `false` for both.
    pub fn is_interpolated(&self) -> bool {
        false
    }

    /// `populateVertexVector(...)`.
    pub fn populate_vertex_vector(
        &mut self,
        buffer: &mut InterleavedVertexBuffer,
        length: usize,
        pattern_positions: &ImagePositions,
        pattern_dependencies: Option<&PatternDependency>,
    ) {
        let vertex_offset = self.vertex_offset;
        let CrossFadedPaintPropertyBinderKind::Composite { zoom_in_vertex_vector, zoom_out_vertex_vector, .. } =
            &mut self.kind
        else {
            return;
        };
        match pattern_dependencies {
            Some(dependencies) if !dependencies.mid.is_empty() => {
                if !pattern_positions.is_empty() {
                    let (Some(image_min), Some(image_mid), Some(image_max)) = (
                        pattern_positions.get(&dependencies.min),
                        pattern_positions.get(&dependencies.mid),
                        pattern_positions.get(&dependencies.max),
                    ) else {
                        return;
                    };

                    for i in zoom_in_vertex_vector.len()..length {
                        zoom_in_vertex_vector.push(image_min.tlbr());
                        zoom_out_vertex_vector.push(image_max.tlbr());

                        buffer.set(i, vertex_offset, bytemuck::cast_slice(&image_mid.tlbr()));
                    }
                }
            }
            _ => {
                // Unlike other properties with expressions that evaluate to
                // null, the default value for `*-pattern` properties is an
                // empty string and will not have a valid entry in
                // patternPositions. We still need to populate the attribute
                // buffers to avoid crashes when we try to draw the layer
                // because we don't know at draw time if all features were
                // evaluated to valid pattern dependencies.
                for i in zoom_in_vertex_vector.len()..length {
                    buffer.set(i, vertex_offset, bytemuck::cast_slice(&[0u16; 4]));
                    zoom_in_vertex_vector.push([0; 4]);
                    zoom_out_vertex_vector.push([0; 4]);
                }
            }
        }
    }

    /// `setPatternParameters(posA, posB, crossfade)`.
    pub fn set_pattern_parameters(
        &self,
        pos_a: Option<&ImagePosition>,
        pos_b: Option<&ImagePosition>,
        crossfade: &CrossfadeParameters,
    ) {
        let mut state = self.state.lock_unpoisoned();
        match &self.kind {
            CrossFadedPaintPropertyBinderKind::Constant { .. } => {
                let (Some(pos_a), Some(pos_b)) = (pos_a, pos_b) else {
                    return;
                };
                state.constant_pattern_positions = (pos_b.tlbr(), pos_a.tlbr());
            }
            CrossFadedPaintPropertyBinderKind::Composite { .. } => {
                state.crossfade = *crossfade;
            }
        }
    }

    /// `interpolationFactor(float)`: `{0, 0}` for both.
    pub fn interpolation_factor(&self, _current_zoom: f32) -> (f32, f32) {
        (0.0, 0.0)
    }

    /// `uniformValue(...)`.
    pub fn uniform_value(&self, _current_value: &PossiblyEvaluatedFadedPropertyValue<Image>) -> ([u16; 4], [u16; 4]) {
        match &self.kind {
            CrossFadedPaintPropertyBinderKind::Constant { .. } => {
                self.state.lock_unpoisoned().constant_pattern_positions
            }
            // Uniform values for vertex attribute arrays are unused.
            CrossFadedPaintPropertyBinderKind::Composite { .. } => ([0; 4], [0; 4]),
        }
    }

    /// `getVertexValue(index)`: (A1 ushort8, A2 ushort8).
    pub fn get_vertex_value(&self, buffer: &InterleavedVertexBuffer, index: usize) -> ([u16; 8], [u16; 8]) {
        match &self.kind {
            CrossFadedPaintPropertyBinderKind::Constant { .. } => ([0; 8], [0; 8]),
            CrossFadedPaintPropertyBinderKind::Composite { zoom_in_vertex_vector, zoom_out_vertex_vector, .. } => {
                let pattern: Vec<u16> = bytemuck::pod_collect_to_vec(&buffer.get(index, self.vertex_offset, 8));
                let zoom_value = if self.state.lock_unpoisoned().crossfade.from_scale == 2.0 {
                    zoom_in_vertex_vector[index]
                } else {
                    zoom_out_vertex_vector[index]
                };
                let mut a = [0u16; 8];
                a[..4].copy_from_slice(&pattern);
                a[4..].copy_from_slice(&pattern);
                let mut b = [0u16; 8];
                b[..4].copy_from_slice(&zoom_value);
                b[4..].copy_from_slice(&zoom_value);
                (a, b)
            }
        }
    }

    /// `applyPaintProperty<TAttribute>(attrIndex, attrib)`: both attributes
    /// (`attrIndex` 0 and 1) get the same interleaved data in C++.
    pub fn apply_paint_property(
        &self,
        buffer: &InterleavedVertexBuffer,
        shared: &VertexVectorBasePtr,
        _attr_index: usize,
        attrib: &mut VertexAttribute,
    ) {
        if self.has_interleaved_buffer {
            let raw_size = buffer.stride as u32;
            // isInterpolated() is false: `Type::DataType`.
            let data_type = AttributeDataType::UShort4;
            attrib.set_shared_raw_data(Some(shared.clone()), self.vertex_offset as u32, 0, raw_size, data_type);
        }
    }
}

/// The virtual interface shared by both binder enums (the C++ base class
/// `PaintPropertyBinder`), as `PaintPropertyBinders` and
/// `readDataDrivenPaintProperty` use it.
pub trait PaintPropertyBinderBase {
    /// `setInterleavedBuffer(InterleavedVertexBuffer&)`.
    fn set_interleaved(&mut self, buffer: &mut InterleavedVertexBuffer);
    /// `populateVertexVector(...)`.
    #[allow(clippy::too_many_arguments)]
    fn populate(
        &mut self,
        buffer: &mut InterleavedVertexBuffer,
        feature: &dyn GeometryTileFeature,
        length: usize,
        index: usize,
        pattern_positions: &ImagePositions,
        pattern_dependencies: Option<&PatternDependency>,
        canonical: &CanonicalTileID,
        formatted_section: &Value,
    );
    /// `setPatternParameters(posA, posB, crossfade)`.
    fn pattern_parameters(
        &self,
        pos_a: Option<&ImagePosition>,
        pos_b: Option<&ImagePosition>,
        crossfade: &CrossfadeParameters,
    );
    /// `updateVertexVectors(states, layer, imagePositions)` (the base no-op
    /// for the constant and cross-faded binders).
    fn update_vectors(
        &mut self,
        _buffer: &mut InterleavedVertexBuffer,
        _states: &crate::bucket::FeatureStates,
        _layer: &dyn GeometryTileLayer,
    ) {
    }
    /// True if a feature state change rewrites this binder's vertex data.
    fn depends_on_feature_state(&self) -> bool {
        false
    }
    /// `getVertexCount()`.
    fn binder_vertex_count(&self, buffer: &InterleavedVertexBuffer) -> usize;
    /// `applyPaintProperty<Attribute>(attrIndex, attrib)`.
    fn apply(
        &self,
        buffer: &InterleavedVertexBuffer,
        shared: &VertexVectorBasePtr,
        attr_index: usize,
        attrib: &mut VertexAttribute,
    );
}

impl<T: BinderValue> PaintPropertyBinderBase for PaintPropertyBinder<T> {
    fn set_interleaved(&mut self, buffer: &mut InterleavedVertexBuffer) {
        self.set_interleaved_buffer(buffer);
    }
    fn populate(
        &mut self,
        buffer: &mut InterleavedVertexBuffer,
        feature: &dyn GeometryTileFeature,
        length: usize,
        index: usize,
        _pattern_positions: &ImagePositions,
        _pattern_dependencies: Option<&PatternDependency>,
        canonical: &CanonicalTileID,
        formatted_section: &Value,
    ) {
        self.populate_vertex_vector(buffer, feature, length, index, canonical, formatted_section);
    }
    fn pattern_parameters(&self, _: Option<&ImagePosition>, _: Option<&ImagePosition>, _: &CrossfadeParameters) {}
    fn update_vectors(
        &mut self,
        buffer: &mut InterleavedVertexBuffer,
        states: &crate::bucket::FeatureStates,
        layer: &dyn GeometryTileLayer,
    ) {
        self.update_vertex_vectors(buffer, states, layer);
    }
    fn depends_on_feature_state(&self) -> bool {
        match &self.kind {
            PaintPropertyBinderKind::Constant { .. } => false,
            PaintPropertyBinderKind::SourceFunction { expression, .. }
            | PaintPropertyBinderKind::CompositeFunction { expression, .. } => {
                uses_feature_state(expression.get_expression())
            }
        }
    }
    fn binder_vertex_count(&self, buffer: &InterleavedVertexBuffer) -> usize {
        self.get_vertex_count(buffer)
    }
    fn apply(
        &self,
        buffer: &InterleavedVertexBuffer,
        shared: &VertexVectorBasePtr,
        attr_index: usize,
        attrib: &mut VertexAttribute,
    ) {
        self.apply_paint_property(buffer, shared, attr_index, attrib);
    }
}

impl PaintPropertyBinderBase for CrossFadedPaintPropertyBinder<Image> {
    fn set_interleaved(&mut self, buffer: &mut InterleavedVertexBuffer) {
        self.set_interleaved_buffer(buffer);
    }
    fn populate(
        &mut self,
        buffer: &mut InterleavedVertexBuffer,
        _feature: &dyn GeometryTileFeature,
        length: usize,
        _index: usize,
        pattern_positions: &ImagePositions,
        pattern_dependencies: Option<&PatternDependency>,
        _canonical: &CanonicalTileID,
        _formatted_section: &Value,
    ) {
        self.populate_vertex_vector(buffer, length, pattern_positions, pattern_dependencies);
    }
    fn pattern_parameters(
        &self,
        pos_a: Option<&ImagePosition>,
        pos_b: Option<&ImagePosition>,
        crossfade: &CrossfadeParameters,
    ) {
        self.set_pattern_parameters(pos_a, pos_b, crossfade);
    }
    fn binder_vertex_count(&self, buffer: &InterleavedVertexBuffer) -> usize {
        self.get_vertex_count(buffer)
    }
    fn apply(
        &self,
        buffer: &InterleavedVertexBuffer,
        shared: &VertexVectorBasePtr,
        attr_index: usize,
        attrib: &mut VertexAttribute,
    ) {
        self.apply_paint_property(buffer, shared, attr_index, attrib);
    }
}

/// `VertexAttributeArray::readDataDrivenPaintProperty<P>(binder, isConstant,
/// propertiesAsUniforms, dataDrivenAttrId)`: one property with its
/// `AttributeNames`.
#[allow(clippy::too_many_arguments)]
pub fn read_data_driven_paint_property(
    attrs: &mut VertexAttributeArray,
    binder: &dyn PaintPropertyBinderBase,
    buffer: &InterleavedVertexBuffer,
    shared: &VertexVectorBasePtr,
    attribute_names: &[&str],
    is_constant: bool,
    properties_as_uniforms: &mut StringIDSetsPair,
    data_driven_attr_id: &mut usize,
) {
    // Consider each attribute name in the attribute (e.g., pattern_from, pattern_to)
    for (attr_index, attribute_name) in attribute_names.iter().enumerate() {
        // Apply the property, or add it to the uniforms collection if it's constant.
        if !is_constant && binder.binder_vertex_count(buffer) > 0 {
            if let Some(attr) = attrs.set(*data_driven_attr_id, -1, AttributeDataType::Invalid, 0) {
                binder.apply(buffer, shared, attr_index, attr);
            }
        } else {
            properties_as_uniforms.0.insert((*attribute_name).to_owned());
            properties_as_uniforms.1.insert(*data_driven_attr_id);
        }
        *data_driven_attr_id += 1;
    }
}

/// `PaintPropertyBinders<TypeList<Ps...>>` for one layer type's
/// data-driven paint properties: a struct with one binder per property
/// (fields in the C++ type-list order, which is also the interleaved
/// vertex order) and the shared `InterleavedVertexBuffer`.
#[macro_export]
macro_rules! paint_property_binders {
    (
        $(#[$meta:meta])*
        pub struct $name:ident for $props:path {
            $($field:ident: $prop:ty => $binder:ty),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, ::serde::Serialize, ::serde::Deserialize)]
        pub struct $name {
            pub interleaved_vertex_buffer: $crate::paint_property_binder::InterleavedVertexBuffer,
            $(pub $field: $binder,)+
        }

        impl $name {
            /// `PaintPropertyBinders(const EvaluatedProperties&, float z)`.
            pub fn new(properties: &$props, z: f32) -> Self {
                let mut result = $name {
                    interleaved_vertex_buffer: Default::default(),
                    $($field: <$binder>::create(
                        properties.get::<$prop>(),
                        z,
                        <$prop as mln_style::properties::Property>::default_value(),
                    ),)+
                };
                $($crate::paint_property_binder::PaintPropertyBinderBase::set_interleaved(
                    &mut result.$field, &mut result.interleaved_vertex_buffer);)+
                result
            }

            /// `populateVertexVectors(feature, length, index,
            /// patternPositions, patternDependencies, canonical,
            /// formattedSection = {})`.
            #[allow(clippy::too_many_arguments)]
            pub fn populate_vertex_vectors(
                &mut self,
                feature: &dyn mln_tile_id::geometry_tile_data::GeometryTileFeature,
                length: usize,
                index: usize,
                pattern_positions: &mln_style_types::image::ImagePositions,
                pattern_dependencies: Option<&$crate::layout::pattern_layout::PatternDependency>,
                canonical: &mln_tile_id::CanonicalTileID,
                formatted_section: &mln_style::expression::Value,
            ) {
                let buffer = &mut self.interleaved_vertex_buffer;
                let added = (length - buffer.vertex_count) * buffer.stride;
                let data = buffer.shared_vertex_vector.get_mut();
                data.resize(data.len() + added, 0);

                $($crate::paint_property_binder::PaintPropertyBinderBase::populate(
                    &mut self.$field,
                    &mut self.interleaved_vertex_buffer,
                    feature,
                    length,
                    index,
                    pattern_positions,
                    pattern_dependencies,
                    canonical,
                    formatted_section,
                );)+

                let buffer = &mut self.interleaved_vertex_buffer;
                buffer.vertex_count += length - buffer.vertex_count;
                // sharedVertexVector->updateModified(true): the render side
                // copies the vector when it builds drawables.
            }

            /// `updateVertexVectors(states, layer, imagePositions)`.
            pub fn update_vertex_vectors(
                &mut self,
                states: &$crate::bucket::FeatureStates,
                layer: &dyn mln_tile_id::geometry_tile_data::GeometryTileLayer,
                _image_positions: &mln_style_types::image::ImagePositions,
            ) {
                // Values that moved to the render thread don't depend on
                // feature state (`depends_on_feature_state`).
                if self.interleaved_vertex_buffer.shared_vertex_vector.is_moved() {
                    return;
                }
                $($crate::paint_property_binder::PaintPropertyBinderBase::update_vectors(
                    &mut self.$field, &mut self.interleaved_vertex_buffer, states, layer);)+
                // interleavedVertexBuffer.sharedVertexVector->updateModified(true)
            }

            /// True if a feature state change rewrites the data-driven values.
            pub fn depends_on_feature_state(&self) -> bool {
                false $(|| $crate::paint_property_binder::PaintPropertyBinderBase::depends_on_feature_state(
                    &self.$field))+
            }

            /// `setPatternParameters(posA, posB, crossfade)`.
            pub fn set_pattern_parameters(
                &self,
                pos_a: Option<&mln_style_types::image::ImagePosition>,
                pos_b: Option<&mln_style_types::image::ImagePosition>,
                crossfade: &mln_style::CrossfadeParameters,
            ) {
                $($crate::paint_property_binder::PaintPropertyBinderBase::pattern_parameters(
                    &self.$field, pos_a, pos_b, crossfade);)+
            }
        }
    };

}
