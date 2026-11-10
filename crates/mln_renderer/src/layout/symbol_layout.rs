// Ports src/mln/layout/symbol_layout.hpp and src/mln/layout/symbol_layout.cpp
// (the non-instanced vertex path; MLN_USE_SYMBOL_INSTANCING is off for
// WebGPU).
//
// The layout keeps the tile's `SourceLayer` (shared tile data) and owned
// `SymbolFeature`s (see symbol_feature.rs), so it can wait for glyphs and
// images across worker messages.
//

use std::collections::HashMap;
use std::sync::Arc;

use geometry::{LinearRing, Point, Polygon};
use mln_expression::Value;
use mln_expression::formatted::K_FORMATTED_SECTION_TEXT_COLOR;
use mln_map_types::MapMode;
use mln_style::PropertyEvaluationParameters;
use mln_style::expression::{EvaluationContext, GlobalStateMap};
use mln_style::layer_properties::LayerProperties;
use mln_style::layers::symbol_layer_properties::*;
use mln_style::properties::Property;
use mln_style::property_value::PropertyValue;
use mln_style::renderer::possibly_evaluated_property_value::PossiblyEvaluatedPropertyValue;
use mln_style_types::image::{ImageMap, ImagePositions, ImageType};
use mln_style_types::{
    AlignmentType, AnchorOffsetPair, IconTextFitType, SymbolAnchorType, SymbolPlacementType, SymbolZOrderType,
    TextJustifyType, TextTransformType, TextVariableAnchorType, TextWritingModeType, VariableAnchorOffsetCollection,
};
use mln_text::glyph::{GlyphID, GlyphIDType, GlyphMap, GlyphPositions, Shaping, WritingModeType};
use mln_text::shaping::{PositionedIcon, get_anchor_justification, get_shaping};
use mln_text::tagged_string::SectionOptions;
use mln_text::{BiDi, TaggedString, apply_arabic_shaping, starts_right_to_left};
use mln_tile_id::CanonicalTileID;
use mln_tile_id::geometry_tile_data::{GeometryCoordinates, GeometryTileFeature};
use mln_util::constants::{EXTENT, ONE_EM, TILE_SIZE_D};
use mln_util::exception::ExceptionPtr;
use mln_util::feature::feature_id_to_string;
use mln_util::geometry::FeatureType;
use mln_util::i18n;
use mln_util::immutable::Immutable;
use mln_util::range::Range;
use mln_util::sync::RwLockExt;
use mln_vec_map::VecMap;

use crate::bucket::{Bucket, BucketPtr, get_retain_feature_id};
use crate::bucket_parameters::BucketParameters;
use crate::buckets::symbol_bucket::{
    Buffer, CollisionBuffer, PlacedSymbol, SortKeyRange, SymbolBucket, SymbolBucketState,
};
use crate::geometry::anchor::{Anchor, Anchors};
use crate::geometry::feature_index::{FeatureIndex, IndexedSubfeature};
use crate::layout::clip_lines::clip_lines;
use crate::layout::layout::{Layout, LayoutParameters, SourceLayer};
use crate::layout::merge_lines::merge_lines;
use crate::layout::symbol_feature::SymbolFeature;
use crate::layout::symbol_instance::{ShapedTextOrientations, SymbolContent, SymbolInstance, SymbolInstanceSharedData};
use crate::render_layer::LayerRenderData;
use crate::text::get_anchors::{get_anchors, get_center_anchor};
use crate::text::glyph_manager::HBShapeResults;
use crate::text::quads::{SymbolQuad, SymbolQuads};

/// `has<Property>(layout)`: a constant that is not empty, or an expression.
fn has_text_field(layout: &SymbolLayoutProperties::PossiblyEvaluated) -> bool {
    layout.get::<TextField>().constant().is_none_or(|t| !t.empty())
}
fn has_text_font(layout: &SymbolLayoutProperties::PossiblyEvaluated) -> bool {
    layout.get::<TextFont>().constant().is_none_or(|t| !t.is_empty())
}
fn has_icon_image(layout: &SymbolLayoutProperties::PossiblyEvaluated) -> bool {
    layout.get::<IconImage>().constant().is_none_or(|t| !t.empty())
}

/// `getCharGlyphIDType(ch, stack, faces, lastCharType)`.
fn get_char_glyph_id_type(
    ch: u16,
    stack: &[String],
    faces: &Option<Arc<mln_text::glyph::FontFaces>>,
    last_char_type: GlyphIDType,
) -> GlyphIDType {
    let Some(faces) = faces else {
        return GlyphIDType::FontPBF;
    };

    if i18n::is_variation_selector1(ch) {
        return last_char_type;
    }

    for name in stack {
        for face in faces.iter() {
            if &face.name == name {
                for range in &face.ranges {
                    if u32::from(ch) >= range.0 && u32::from(ch) <= range.1 {
                        return face.type_;
                    }
                }
            }
        }
    }

    GlyphIDType::FontPBF
}

/// `sectionOptionsToValue(options)`.
pub fn section_options_to_value(options: &SectionOptions) -> Value {
    let mut result = mln_expression::value::ObjectType::new();
    // TODO: Data driven properties that can be overridden on per section
    // basis. TextOpacity TextHaloColor TextHaloWidth TextHaloBlur
    if let Some(text_color) = options.text_color {
        result.insert(K_FORMATTED_SECTION_TEXT_COLOR.to_owned(), Value::Color(text_color));
    }
    Value::Object(result)
}

/// `createLayout(unevaluated, zoom, globalState)`.
fn create_layout(
    unevaluated: &SymbolLayoutProperties::Unevaluated,
    zoom: f32,
    global_state: Option<Arc<GlobalStateMap>>,
) -> SymbolLayoutProperties::PossiblyEvaluated {
    let mut evaluation_parameters = PropertyEvaluationParameters::new(zoom);
    evaluation_parameters.global_state = global_state;
    let mut layout = unevaluated.evaluate(&evaluation_parameters);

    let placement = *layout.get::<SymbolPlacement>();
    if *layout.get::<IconRotationAlignment>() == AlignmentType::Auto {
        *layout.get_mut::<IconRotationAlignment>() =
            if placement != SymbolPlacementType::Point { AlignmentType::Map } else { AlignmentType::Viewport };
    }

    if *layout.get::<TextRotationAlignment>() == AlignmentType::Auto {
        *layout.get_mut::<TextRotationAlignment>() =
            if placement != SymbolPlacementType::Point { AlignmentType::Map } else { AlignmentType::Viewport };
    }

    // If unspecified `*-pitch-alignment` inherits `*-rotation-alignment`
    if *layout.get::<TextPitchAlignment>() == AlignmentType::Auto {
        *layout.get_mut::<TextPitchAlignment>() = *layout.get::<TextRotationAlignment>();
    }
    if *layout.get::<IconPitchAlignment>() == AlignmentType::Auto {
        *layout.get_mut::<IconPitchAlignment>() = *layout.get::<IconRotationAlignment>();
    }

    layout
}

/// `util::dist<float>(a, b)`.
fn dist(ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let dx = bx - ax;
    let dy = by - ay;
    (dx * dx + dy * dy).sqrt()
}

// The radial offset is to the edge of the text box. In the horizontal
// direction, the edge of the text box is where glyphs start. But in the
// vertical direction, the glyphs appear to "start" at the baseline. We don't
// actually load baseline data, but we assume an offset of ONE_EM - 17 (see
// "yOffset" in shaping.js)
const BASELINE_OFFSET: f32 = 7.0;

/// `getDefaultHorizontalShaping`: we don't care which shaping we get
/// because this is used for collision purposes and all the justifications
/// have the same collision box.
fn get_default_horizontal_shaping(shaped_text_orientations: &ShapedTextOrientations) -> &Shaping {
    if shaped_text_orientations.right().is_some() {
        return shaped_text_orientations.right();
    }
    if shaped_text_orientations.center.is_some() {
        return &shaped_text_orientations.center;
    }
    if shaped_text_orientations.left.is_some() {
        return &shaped_text_orientations.left;
    }
    &shaped_text_orientations.horizontal
}

/// `shapingForTextJustifyType`.
fn shaping_for_text_justify_type(
    shaped_text_orientations: &mut ShapedTextOrientations,
    type_: TextJustifyType,
) -> &mut Shaping {
    match type_ {
        TextJustifyType::Right => shaped_text_orientations.right_mut(),
        TextJustifyType::Left => &mut shaped_text_orientations.left,
        TextJustifyType::Center => &mut shaped_text_orientations.center,
        _ => {
            debug_assert!(false);
            &mut shaped_text_orientations.horizontal
        }
    }
}

/// `evaluateRadialOffset(anchor, radialOffset)`.
fn evaluate_radial_offset(anchor: SymbolAnchorType, radial_offset: f32) -> [f32; 2] {
    let mut result = [0.0_f32, 0.0_f32];
    let radial_offset = mln_math::max_f32(radial_offset, 0.0); // Ignore negative offset.
    // solve for r where r^2 + r^2 = radialOffset^2
    let hypotenuse = radial_offset / std::f32::consts::SQRT_2;

    match anchor {
        SymbolAnchorType::TopRight | SymbolAnchorType::TopLeft => result[1] = hypotenuse - BASELINE_OFFSET,
        SymbolAnchorType::BottomRight | SymbolAnchorType::BottomLeft => result[1] = -hypotenuse + BASELINE_OFFSET,
        SymbolAnchorType::Bottom => result[1] = -radial_offset + BASELINE_OFFSET,
        SymbolAnchorType::Top => result[1] = radial_offset - BASELINE_OFFSET,
        _ => {}
    }

    match anchor {
        SymbolAnchorType::TopRight | SymbolAnchorType::BottomRight => result[0] = -hypotenuse,
        SymbolAnchorType::TopLeft | SymbolAnchorType::BottomLeft => result[0] = hypotenuse,
        SymbolAnchorType::Left => result[0] = radial_offset,
        SymbolAnchorType::Right => result[0] = -radial_offset,
        _ => {}
    }

    result
}

/// `SymbolLayout`.
pub struct SymbolLayout {
    pub layer_paint_properties: VecMap<String, Immutable<dyn LayerProperties>>,
    pub bucket_leader_id: String,
    pub symbol_instances: Vec<SymbolInstance>,
    pub sort_key_ranges: Vec<SortKeyRange>,

    compare_text: VecMap<Vec<u16>, Vec<Anchor>>,

    // Stores the layer so that we can hold on to GeometryTileFeature
    // instances in SymbolFeature, which may reference data from this object.
    source_layer: SourceLayer,
    overscaling: f32,
    zoom: f32,
    canonical_id: CanonicalTileID,
    mode: MapMode,
    pixel_ratio: f32,
    #[allow(dead_code)]
    tile_size: u32,
    tile_pixel_ratio: f32,

    icons_need_linear: bool,
    sort_features_by_y: bool,
    sort_features_by_key: bool,
    allow_vertical_placement: bool,
    icons_in_text: bool,
    placement_modes: Vec<TextWritingModeType>,

    text_size: PropertyValue<f32>,
    icon_size: PropertyValue<f32>,
    text_radial_offset: PropertyValue<f32>,
    text_variable_anchor_offset: PropertyValue<VariableAnchorOffsetCollection>,

    layout: Immutable<SymbolLayoutProperties::PossiblyEvaluated>,

    features: Vec<SymbolFeature>,

    // Consider moving this up to geometry tile worker to reduce
    // reinstantiation costs; use of BiDi/ubiditransform object must be
    // constrained to one thread
    bidi: BiDi,

    need_finalize_symbols_val: bool,
    retain_features_by_id: bool,
}

impl SymbolLayout {
    pub const INVALID_OFFSET_VALUE: f32 = f32::MAX;

    /// `SymbolLayout(parameters, layers, sourceLayer, layoutParameters)`.
    pub fn new(
        parameters: &BucketParameters,
        layers: &[Immutable<dyn LayerProperties>],
        source_layer: SourceLayer,
        layout_parameters: &mut LayoutParameters<'_>,
    ) -> Result<Self, ExceptionPtr> {
        let leader = layers[0]
            .base_impl
            .downcast_ref::<mln_style::layers::symbol_layer_impl::Impl>()
            .expect("SymbolLayout: not a symbol layer");
        let overscaling = parameters.tile_id.overscale_factor() as f32;
        let zoom = f32::from(parameters.tile_id.overscaled_z);
        let tile_size = (TILE_SIZE_D * overscaling as f64) as u32;
        let tile_pixel_ratio = EXTENT as f32 / tile_size as f32;
        let layout = Arc::new(create_layout(&leader.layout, zoom, parameters.global_state.clone()));

        let mut this = SymbolLayout {
            layer_paint_properties: VecMap::new(),
            bucket_leader_id: layers[0].base_impl.id.clone(),
            symbol_instances: Vec::new(),
            sort_key_ranges: Vec::new(),
            compare_text: VecMap::new(),
            source_layer,
            overscaling,
            zoom,
            canonical_id: parameters.tile_id.canonical,
            mode: parameters.mode,
            pixel_ratio: parameters.pixel_ratio,
            tile_size,
            tile_pixel_ratio,
            icons_need_linear: false,
            sort_features_by_y: false,
            sort_features_by_key: false,
            allow_vertical_placement: false,
            icons_in_text: false,
            placement_modes: Vec::new(),
            text_size: leader.layout.get::<TextSize>().clone(),
            icon_size: leader.layout.get::<IconSize>().clone(),
            text_radial_offset: leader.layout.get::<TextRadialOffset>().clone(),
            text_variable_anchor_offset: leader.layout.get::<TextVariableAnchorOffset>().clone(),
            layout,
            features: Vec::new(),
            bidi: BiDi::new(),
            need_finalize_symbols_val: false,
            retain_features_by_id: parameters.retain_features_by_id,
        };

        // These property values are retained unevaluated; capture the
        // global state so that later evaluations can resolve "global-state"
        // expressions.
        fn capture<T>(property_value: &mut PropertyValue<T>, global_state: &Option<Arc<GlobalStateMap>>) {
            if property_value.is_expression() {
                property_value.as_expression_mut().capture_global_state(global_state.clone());
            }
        }
        capture(&mut this.text_size, &parameters.global_state);
        capture(&mut this.icon_size, &parameters.global_state);
        capture(&mut this.text_radial_offset, &parameters.global_state);
        capture(&mut this.text_variable_anchor_offset, &parameters.global_state);

        let layout = this.layout.clone();
        let has_text = has_text_field(&layout) && has_text_font(&layout);
        let has_icon = has_icon_image(&layout);

        if !has_text && !has_icon {
            return Ok(this);
        }

        let has_symbol_sort_key = !leader.layout.get::<SymbolSortKey>().is_undefined();
        let symbol_z_order = *layout.get::<SymbolZOrder>();
        this.sort_features_by_key = symbol_z_order != SymbolZOrderType::ViewportY && has_symbol_sort_key;
        let z_order_by_viewport_y = symbol_z_order == SymbolZOrderType::ViewportY
            || (symbol_z_order == SymbolZOrderType::Auto && !this.sort_features_by_key);
        this.sort_features_by_y = z_order_by_viewport_y
            && (*layout.get::<TextAllowOverlap>()
                || *layout.get::<IconAllowOverlap>()
                || *layout.get::<TextIgnorePlacement>()
                || *layout.get::<IconIgnorePlacement>());
        let sort_features = this.sort_features_by_key && !layout.get::<SymbolSortKey>().is_constant();
        if *layout.get::<SymbolPlacement>() == SymbolPlacementType::Point {
            let modes = layout.get::<TextWritingMode>().clone();
            // Remove duplicates and preserve order.
            let mut seen: Vec<TextWritingModeType> = Vec::new();
            let mut placement_modes = Vec::new();
            for placement_mode in modes {
                this.allow_vertical_placement =
                    this.allow_vertical_placement || placement_mode == TextWritingModeType::Vertical;
                if !seen.contains(&placement_mode) {
                    seen.push(placement_mode);
                    placement_modes.push(placement_mode);
                }
            }
            this.placement_modes = placement_modes;
        }

        for layer in layers {
            this.layer_paint_properties.entry(layer.base_impl.id.clone()).or_insert_with(|| layer.clone());
        }

        // Determine glyph dependencies
        let source_layer = this.source_layer.clone();
        let geometry_layer = source_layer.layer()?;
        let feature_count = geometry_layer.feature_count();
        let canonical_id = this.canonical_id;
        for i in 0..feature_count {
            let feature = geometry_layer.get_feature(i)?;
            let passes = leader.filter.evaluate(
                &EvaluationContext::with_zoom_feature(this.zoom, Some(&*feature))
                    .with_canonical_tile_id(Some(&parameters.tile_id.canonical))
                    .with_global_state(parameters.global_state.as_deref())
                    .with_font_faces(layout_parameters.font_faces.as_deref()),
            );
            feature.check()?;
            if !passes {
                continue;
            }

            // C++ keeps the feature and reads its values lazily; the copy
            // reads all of them, so a malformed one is an error here even if
            // nothing reads it.
            let mut ft = SymbolFeature::new(&*feature);
            feature.check()?;
            drop(feature);

            ft.index = i;

            if has_text {
                let formatted = match layout.get::<TextField>() {
                    PossiblyEvaluatedPropertyValue::Constant(constant) => constant.clone(),
                    PossiblyEvaluatedPropertyValue::Expression(expression) => expression.evaluate(
                        &EvaluationContext::with_zoom_feature(zoom, Some(&ft))
                            .with_available_images(Some(layout_parameters.available_images))
                            .with_canonical_tile_id(Some(&canonical_id))
                            .with_font_faces(layout_parameters.font_faces.as_deref()),
                        TextField::default_value(),
                    ),
                };
                let text_transform = layout.evaluate_canonical::<TextTransform>(zoom, &ft, &canonical_id);
                let base_font_stack: Vec<String> = layout.evaluate_canonical::<TextFont>(zoom, &ft, &canonical_id);

                let mut formatted_text = TaggedString::default();
                let mut section_table: VecMap<usize, usize> = VecMap::new();

                for (section_index, section) in formatted.sections.iter().enumerate() {
                    if let Some(image) = &section.image {
                        layout_parameters.image_dependencies.entry(image.id().to_owned()).or_insert(ImageType::Icon);
                        formatted_text.add_image_section(image.id());
                        continue;
                    }
                    let mut u8string = section.text.clone();
                    if text_transform == TextTransformType::Uppercase {
                        u8string = nunicode::uppercase(&u8string);
                    } else if text_transform == TextTransformType::Lowercase {
                        u8string = nunicode::lowercase(&u8string);
                    }

                    // Only the sections that draw from glyph PBFs take ICU's
                    // Arabic presentation forms. HarfBuzz needs the letters
                    // themselves, to apply the font's own joining rules.
                    let u16_string = u8string.encode_utf16().collect::<Vec<u16>>();
                    // `while (*u16Char)`: the text ends at the first NUL.
                    let end = u16_string.iter().position(|&c| c == 0).unwrap_or(u16_string.len());
                    let chars = &u16_string[..end];
                    let section_scale = section.font_scale.unwrap_or(1.0);
                    let section_font_stack = section.font_stack.clone().unwrap_or_else(|| base_font_stack.clone());
                    let font_faces = &layout_parameters.font_faces;

                    let mut sub_string: Vec<u16> = Vec::new();
                    // `*u16Char` of an empty string is the terminating NUL.
                    let mut sub_string_type = get_char_glyph_id_type(
                        chars.first().copied().unwrap_or(0),
                        &section_font_stack,
                        font_faces,
                        GlyphIDType::FontPBF,
                    );

                    let mut add_sub_string =
                        |sub_string: &mut Vec<u16>,
                         sub_string_type: GlyphIDType,
                         key_section: bool,
                         formatted_text: &mut TaggedString| {
                            if sub_string.is_empty() {
                                return;
                            }
                            if sub_string_type == GlyphIDType::FontPBF {
                                *sub_string = apply_arabic_shaping(sub_string);
                            }
                            formatted_text.add_text_section(
                                sub_string,
                                section_scale,
                                &section_font_stack,
                                sub_string_type,
                                key_section,
                                section.text_color,
                            );
                            section_table.insert(formatted_text.get_sections().len() - 1, section_index);
                            if sub_string_type != GlyphIDType::FontPBF {
                                layout_parameters
                                    .glyph_dependencies
                                    .shapes
                                    .entry(section_font_stack.clone())
                                    .or_default()
                                    .entry(sub_string_type)
                                    .or_default()
                                    .insert(sub_string.clone());
                            }
                        };

                    for &ch in chars {
                        let ch_type = get_char_glyph_id_type(ch, &section_font_stack, font_faces, sub_string_type);
                        if ch_type != sub_string_type {
                            add_sub_string(&mut sub_string, sub_string_type, false, &mut formatted_text);
                            sub_string.clear();
                            sub_string_type = ch_type;
                        }

                        sub_string.push(ch);
                    }

                    add_sub_string(&mut sub_string, sub_string_type, true, &mut formatted_text);
                }

                let can_verticalize_text = *layout.get::<TextRotationAlignment>() == AlignmentType::Map
                    && *layout.get::<SymbolPlacement>() != SymbolPlacementType::Point
                    && formatted_text.allows_vertical_writing_mode();
                let allows_vertical = formatted_text.allows_vertical_writing_mode();

                // Loop through all characters of this text and collect
                // unique codepoints.
                for j in 0..formatted_text.length() {
                    let section_index = formatted_text.get_section_index(j) as usize;
                    let section = &formatted_text.get_sections()[section_index];
                    if section.image_id.is_some() {
                        continue;
                    }
                    let section_font_stack =
                        section_table.get(&section_index).and_then(|&i| formatted.sections[i].font_stack.clone());
                    let font_stack = section_font_stack.unwrap_or_else(|| base_font_stack.clone());
                    let dependencies = layout_parameters.glyph_dependencies.glyphs.entry(font_stack).or_default();
                    if section.type_ != GlyphIDType::FontPBF {
                        dependencies.insert(GlyphID::with_type(0, section.type_));
                        this.need_finalize_symbols_val = true;
                    } else {
                        let code_point = formatted_text.get_char_code_at(j);
                        dependencies.insert(GlyphID::from(code_point));
                        if can_verticalize_text || (this.allow_vertical_placement && allows_vertical) {
                            let vertical_chr = i18n::verticalize_punctuation(code_point);
                            if vertical_chr != 0 {
                                dependencies.insert(GlyphID::from(vertical_chr));
                            }
                        }
                    }
                }
                ft.formatted_text = Some(formatted_text);
            }

            if has_icon {
                let icon = layout.evaluate_images_canonical::<IconImage>(
                    zoom,
                    &ft,
                    layout_parameters.available_images,
                    &canonical_id,
                );
                layout_parameters.image_dependencies.entry(icon.id().to_owned()).or_insert(ImageType::Icon);
                ft.icon = Some(icon);
            }

            if ft.formatted_text.is_some() || ft.icon.is_some() {
                if this.sort_features_by_key {
                    ft.sort_key = layout.evaluate_canonical::<SymbolSortKey>(zoom, &ft, &canonical_id);
                }
                if sort_features {
                    let lower_bound = this.features.partition_point(|f| f.sort_key < ft.sort_key);
                    this.features.insert(lower_bound, ft);
                } else {
                    this.features.push(ft);
                }
            }
        }
        drop(geometry_layer);

        if this.sort_features_by_key && !sort_features {
            // Inserting at the lower bound puts each feature before the
            // earlier ones with an equal key: keep that order without a sort.
            this.features.reverse();
        }

        if *layout.get::<SymbolPlacement>() == SymbolPlacementType::Line {
            merge_lines(&mut this.features);
        }

        Ok(this)
    }

    /// `evaluateVariableOffset(anchor, textOffset)`.
    pub fn evaluate_variable_offset(anchor: SymbolAnchorType, mut offset: [f32; 2]) -> [f32; 2] {
        if offset[1] == Self::INVALID_OFFSET_VALUE {
            return evaluate_radial_offset(anchor, offset[0]);
        }
        let mut result = [0.0_f32, 0.0_f32];
        offset[0] = offset[0].abs();
        offset[1] = offset[1].abs();

        match anchor {
            SymbolAnchorType::TopRight | SymbolAnchorType::TopLeft | SymbolAnchorType::Top => {
                result[1] = offset[1] - BASELINE_OFFSET;
            }
            SymbolAnchorType::BottomRight | SymbolAnchorType::BottomLeft | SymbolAnchorType::Bottom => {
                result[1] = -offset[1] + BASELINE_OFFSET;
            }
            SymbolAnchorType::Center | SymbolAnchorType::Left | SymbolAnchorType::Right => {}
        }

        match anchor {
            SymbolAnchorType::TopRight | SymbolAnchorType::BottomRight | SymbolAnchorType::Right => {
                result[0] = -offset[0];
            }
            SymbolAnchorType::TopLeft | SymbolAnchorType::BottomLeft | SymbolAnchorType::Left => {
                result[0] = offset[0];
            }
            SymbolAnchorType::Center | SymbolAnchorType::Top | SymbolAnchorType::Bottom => {}
        }

        result
    }

    /// `getTextVariableAnchorOffset(feature)`: helper to support both
    /// text-variable-anchor and text-variable-anchor-offset. Offset values
    /// converted from EMs to PXs.
    fn get_text_variable_anchor_offset(&self, feature: &SymbolFeature) -> Option<VariableAnchorOffsetCollection> {
        let zoom = self.zoom;
        let canonical = &self.canonical_id;
        // If style specifies text-variable-anchor-offset, just return it
        if !self.text_variable_anchor_offset.is_undefined() {
            let variable_anchor_offset =
                self.layout.evaluate_canonical::<TextVariableAnchorOffset>(zoom, feature, canonical);
            if variable_anchor_offset.is_empty() {
                return None;
            }
            let mut anchor_offsets = Vec::with_capacity(variable_anchor_offset.len());
            // Convert offsets from EM to PX, and apply baseline shift
            for anchor_offset in variable_anchor_offset.iter() {
                let mut variable_text_offset = [anchor_offset.offset[0] * ONE_EM, anchor_offset.offset[1] * ONE_EM];
                match anchor_offset.anchor_type {
                    SymbolAnchorType::TopRight | SymbolAnchorType::TopLeft | SymbolAnchorType::Top => {
                        variable_text_offset[1] -= BASELINE_OFFSET;
                    }
                    SymbolAnchorType::BottomRight | SymbolAnchorType::BottomLeft | SymbolAnchorType::Bottom => {
                        variable_text_offset[1] += BASELINE_OFFSET;
                    }
                    SymbolAnchorType::Center | SymbolAnchorType::Left | SymbolAnchorType::Right => {}
                }
                anchor_offsets
                    .push(AnchorOffsetPair { anchor_type: anchor_offset.anchor_type, offset: variable_text_offset });
            }
            return Some(VariableAnchorOffsetCollection::new(anchor_offsets));
        }

        let variable_text_anchor: Vec<TextVariableAnchorType> =
            self.layout.evaluate_canonical::<TextVariableAnchor>(zoom, feature, canonical);
        if variable_text_anchor.is_empty() {
            return None;
        }
        let variable_text_offset = if !self.text_radial_offset.is_undefined() {
            [
                self.layout.evaluate_canonical::<TextRadialOffset>(zoom, feature, canonical) * ONE_EM,
                Self::INVALID_OFFSET_VALUE,
            ]
        } else {
            let text_offset = self.layout.evaluate_canonical::<TextOffset>(zoom, feature, canonical);
            [text_offset[0] * ONE_EM, text_offset[1] * ONE_EM]
        };

        let anchor_offset_pairs = variable_text_anchor
            .into_iter()
            .map(|anchor| AnchorOffsetPair {
                anchor_type: anchor,
                offset: Self::evaluate_variable_offset(anchor, variable_text_offset),
            })
            .collect();
        Some(VariableAnchorOffsetCollection::new(anchor_offset_pairs))
    }

    /// `addFeature(layoutFeatureIndex, feature, shapedTextOrientations,
    /// shapedIcon, imageMap, textOffset, layoutTextSize, layoutIconSize,
    /// iconType)`.
    #[allow(clippy::too_many_arguments)]
    fn add_feature(
        &mut self,
        layout_feature_index: usize,
        feature: &SymbolFeature,
        shaped_text_orientations: &ShapedTextOrientations,
        mut shaped_icon: Option<PositionedIcon>,
        image_map: &ImageMap,
        text_offset: [f32; 2],
        layout_text_size: f32,
        layout_icon_size: f32,
        icon_type: SymbolContent,
    ) {
        let min_scale = 0.5_f32;
        let glyph_size = 24.0_f32;
        let zoom = self.zoom;
        let canonical = self.canonical_id;
        let layout = self.layout.clone();

        let icon_offset = layout.evaluate_canonical::<IconOffset>(zoom, feature, &canonical);

        // To reduce the number of labels that jump around when zooming we
        // need to use a text-size value that is the same for all zoom
        // levels. This calculates text-size at a high zoom level so that all
        // tiles can use the same value when calculating anchor positions.
        let text_max_size = layout.evaluate_canonical::<TextSize>(18.0, feature, &canonical);

        let font_scale = layout_text_size / glyph_size;
        let text_box_scale = self.tile_pixel_ratio * font_scale;
        let text_max_box_scale = self.tile_pixel_ratio * text_max_size / glyph_size;
        let icon_box_scale = self.tile_pixel_ratio * layout_icon_size;
        let symbol_spacing = self.tile_pixel_ratio * *layout.get::<SymbolSpacing>();
        let text_padding = *layout.get::<TextPadding>() * self.tile_pixel_ratio;
        let icon_padding = layout.evaluate_canonical::<IconPadding>(zoom, feature, &canonical) * self.tile_pixel_ratio;
        let text_max_angle = (*layout.get::<TextMaxAngle>()).to_radians_cpp();
        let icon_rotation = layout.evaluate_canonical::<IconRotate>(zoom, feature, &canonical);
        let text_rotation = layout.evaluate_canonical::<TextRotate>(zoom, feature, &canonical);
        let variable_anchor_offsets = self.get_text_variable_anchor_offset(feature);

        let text_placement = if *layout.get::<TextRotationAlignment>() != AlignmentType::Map {
            SymbolPlacementType::Point
        } else {
            *layout.get::<SymbolPlacement>()
        };

        let text_repeat_distance = symbol_spacing / 2.0;
        // `layout->evaluate(zoom, feature)`: C++ calls the overload without
        // a canonical tile ID.
        let evaluated_layout_properties = layout.evaluate_all(zoom, feature, &canonical);
        let indexed_feature = IndexedSubfeature {
            index: feature.index,
            source_layer_name: self.source_layer.name.clone(),
            bucket_leader_id: self.bucket_leader_id.clone(),
            sort_index: self.symbol_instances.len(),
            bucket_instance_id: 0,
            collision_group_id: 0,
        };

        let icon_text_fit = *evaluated_layout_properties.get::<IconTextFit>();
        let has_icon_text_fit = icon_text_fit != IconTextFitType::None;
        // Adjust shaped icon size when icon-text-fit is used.
        let mut vertically_shaped_icon: Option<PositionedIcon> = None;
        if let Some(icon) = shaped_icon.as_mut() {
            if has_icon_text_fit {
                // Create vertically shaped icon for vertical writing mode if needed.
                if self.allow_vertical_placement && shaped_text_orientations.vertical.is_some() {
                    let mut vertical = icon.clone();
                    vertical.fit_icon_to_text(
                        &shaped_text_orientations.vertical,
                        icon_text_fit,
                        layout.get::<IconTextFitPadding>(),
                        &icon_offset,
                        font_scale,
                    );
                    vertically_shaped_icon = Some(vertical);
                }
                let shaped_text = get_default_horizontal_shaping(shaped_text_orientations);
                if shaped_text.is_some() {
                    icon.fit_icon_to_text(
                        shaped_text,
                        icon_text_fit,
                        layout.get::<IconTextFitPadding>(),
                        &icon_offset,
                        font_scale,
                    );
                }
            }
        }

        let key: Vec<u16> = feature.formatted_text.as_ref().map(|t| t.raw_text().to_vec()).unwrap_or_default();

        let allow_vertical_placement = self.allow_vertical_placement;
        let create_symbol_instance_shared_data = |line: GeometryCoordinates| {
            Arc::new(SymbolInstanceSharedData::new(
                line,
                shaped_text_orientations,
                &shaped_icon,
                &vertically_shaped_icon,
                &evaluated_layout_properties,
                text_placement,
                &text_offset,
                image_map,
                icon_rotation,
                icon_type,
                has_icon_text_fit,
                allow_vertical_placement,
            ))
        };

        let mode = self.mode;
        let overscaling = self.overscaling;
        let sort_features_by_key = self.sort_features_by_key;
        let add_symbol_instance =
            |this: &mut SymbolLayout, anchor: Anchor, shared_data: Arc<SymbolInstanceSharedData>| {
                let anchor_inside_tile = anchor.point.x >= 0.0
                    && anchor.point.x < EXTENT as f32
                    && anchor.point.y >= 0.0
                    && anchor.point.y < EXTENT as f32;

                if mode == MapMode::Tile || anchor_inside_tile {
                    // For static/continuous rendering, only add symbols anchored
                    // within this tile: neighboring symbols will be added as part
                    // of the neighboring tiles. In tiled rendering mode, add all
                    // symbols in the buffers so that we can: (1) render symbols
                    // that overlap into this tile (2) approximate collision
                    // detection effects from neighboring symbols
                    this.symbol_instances.push(SymbolInstance::new(
                        anchor,
                        shared_data,
                        shaped_text_orientations,
                        &shaped_icon,
                        &vertically_shaped_icon,
                        text_box_scale,
                        text_padding,
                        text_placement,
                        text_offset,
                        icon_box_scale,
                        icon_padding,
                        icon_offset,
                        &indexed_feature,
                        layout_feature_index,
                        feature.index,
                        key.clone(),
                        overscaling,
                        icon_rotation,
                        text_rotation,
                        &variable_anchor_offsets,
                        allow_vertical_placement,
                        icon_type,
                    ));

                    if sort_features_by_key {
                        let len = this.symbol_instances.len();
                        match this.sort_key_ranges.last_mut() {
                            Some(last) if last.sort_key == feature.sort_key => last.end = len,
                            _ => this.sort_key_ranges.push(SortKeyRange {
                                sort_key: feature.sort_key,
                                start: len - 1,
                                end: len,
                            }),
                        }
                    }
                }
            };

        let feature_type = feature.get_type();
        let text_shaping = if shaped_text_orientations.vertical.is_some() {
            &shaped_text_orientations.vertical
        } else {
            get_default_horizontal_shaping(shaped_text_orientations)
        };
        let (text_left, text_right) = (text_shaping.left, text_shaping.right);
        let (icon_left, icon_right) = shaped_icon.as_ref().map_or((0.0, 0.0), |icon| (icon.left(), icon.right()));

        let placement = *layout.get::<SymbolPlacement>();
        if placement == SymbolPlacementType::Line {
            let clipped_lines = clip_lines(&feature.geometry, 0, 0, EXTENT as i16, EXTENT as i16);
            for line in clipped_lines.iter() {
                let anchors: Anchors = get_anchors(
                    line,
                    symbol_spacing,
                    text_max_angle,
                    text_left,
                    text_right,
                    icon_left,
                    icon_right,
                    glyph_size,
                    text_max_box_scale,
                    self.overscaling,
                );
                let shared_data = create_symbol_instance_shared_data(line.clone());
                for anchor in anchors {
                    let too_close = feature
                        .formatted_text
                        .as_ref()
                        .is_some_and(|text| self.anchor_is_too_close(text.raw_text(), text_repeat_distance, &anchor));
                    if !too_close {
                        add_symbol_instance(self, anchor, shared_data.clone());
                    }
                }
            }
        } else if placement == SymbolPlacementType::LineCenter {
            // No clipping, multiple lines per feature are allowed
            // "lines" with only one point are ignored as in clipLines
            for line in feature.geometry.iter() {
                if line.len() > 1 {
                    let anchor = get_center_anchor(
                        line,
                        text_max_angle,
                        text_left,
                        text_right,
                        icon_left,
                        icon_right,
                        glyph_size,
                        text_max_box_scale,
                    );
                    if let Some(anchor) = anchor {
                        add_symbol_instance(self, anchor, create_symbol_instance_shared_data(line.clone()));
                    }
                }
            }
        } else if feature_type == FeatureType::Polygon {
            for polygon in mln_tile::geometry_tile_data::classify_rings(&feature.geometry) {
                let mut poly: Polygon<f64> = Polygon::new();
                for ring in polygon.iter() {
                    let r: LinearRing<f64> = ring.iter().map(|p| Point::new(f64::from(p.x), f64::from(p.y))).collect();
                    poly.push(r);
                }

                // 1 pixel worth of precision, in tile coordinates
                let Ok(poi) = polylabel::polylabel(&poly, EXTENT as f64 / TILE_SIZE_D) else {
                    continue;
                };
                let anchor = Anchor::new(poi.x as f32, poi.y as f32, 0.0, Some(min_scale as usize));
                add_symbol_instance(self, anchor, create_symbol_instance_shared_data(polygon[0].clone()));
            }
        } else if feature_type == FeatureType::LineString {
            for line in feature.geometry.iter() {
                // Skip invalid LineStrings.
                if line.is_empty() {
                    continue;
                }

                let anchor = Anchor::new(f32::from(line[0].x), f32::from(line[0].y), 0.0, Some(min_scale as usize));
                add_symbol_instance(self, anchor, create_symbol_instance_shared_data(line.clone()));
            }
        } else if feature_type == FeatureType::Point {
            for points in feature.geometry.iter() {
                for point in points.iter() {
                    let anchor = Anchor::new(f32::from(point.x), f32::from(point.y), 0.0, Some(min_scale as usize));
                    add_symbol_instance(self, anchor, create_symbol_instance_shared_data(vec![*point]));
                }
            }
        }
    }

    /// `anchorIsTooClose(text, repeatDistance, anchor)`.
    fn anchor_is_too_close(&mut self, text: &[u16], repeat_distance: f32, anchor: &Anchor) -> bool {
        if let Some(other_anchors) = self.compare_text.get(text) {
            for other_anchor in other_anchors {
                if dist(anchor.point.x, anchor.point.y, other_anchor.point.x, other_anchor.point.y) < repeat_distance {
                    return true;
                }
            }
        }
        self.compare_text.entry(text.to_vec()).or_default().push(*anchor);
        false
    }

    /// `calculateTileDistances(line, anchor)`: analog of
    /// `addToLineVertexArray` in JS. This version doesn't need to build up a
    /// line array like the JS version does, but it uses the same logic to
    /// calculate tile distances.
    pub fn calculate_tile_distances(line: &GeometryCoordinates, anchor: &Anchor) -> Vec<f32> {
        let mut tile_distances = vec![0.0_f32; line.len()];
        if let Some(segment) = anchor.segment {
            debug_assert!(segment < line.len());
            let p = |i: usize| (f32::from(line[i].x), f32::from(line[i].y));
            let mut sum_forward_length = if segment + 1 < line.len() {
                let (x, y) = p(segment + 1);
                dist(anchor.point.x, anchor.point.y, x, y)
            } else {
                0.0
            };
            let (sx, sy) = p(segment);
            let mut sum_backward_length = dist(anchor.point.x, anchor.point.y, sx, sy);
            for i in segment + 1..line.len() {
                tile_distances[i] = sum_forward_length;
                if i < line.len() - 1 {
                    let (ax, ay) = p(i + 1);
                    let (bx, by) = p(i);
                    sum_forward_length += dist(ax, ay, bx, by);
                }
            }
            let mut i = segment;
            loop {
                tile_distances[i] = sum_backward_length;
                if i != 0 {
                    let (ax, ay) = p(i - 1);
                    let (bx, by) = p(i);
                    sum_backward_length += dist(ax, ay, bx, by);
                    i -= 1;
                } else {
                    break; // Add break to avoid unsigned integer overflow when i==0
                }
            }
        }
        tile_distances
    }

    /// `updatePaintPropertiesForSection(bucket, feature, sectionIndex, canonical)`.
    fn update_paint_properties_for_section(
        bucket: &SymbolBucket,
        state: &SymbolBucketState,
        feature: &SymbolFeature,
        section_index: usize,
        canonical: &CanonicalTileID,
    ) {
        let formatted_section =
            section_options_to_value(feature.formatted_text.as_ref().expect("text").section_at(section_index));
        let length = state.text.attribute_data.len();
        for binders in bucket.paint_properties.values() {
            let mut binders = binders.write_unpoisoned();
            binders.text_binders.populate_vertex_vectors(
                feature,
                length,
                feature.index,
                &ImagePositions::new(),
                None,
                canonical,
                &formatted_section,
            );
        }
    }

    /// `addSymbolGlyphQuads(bucket, symbolInstance, feature, writingMode,
    /// placedIndex, glyphQuads, canonical, lastAddedSection)`: adds symbol
    /// quads to bucket and returns formatted section index of last added
    /// quad.
    #[allow(clippy::too_many_arguments)]
    fn add_symbol_glyph_quads(
        &self,
        bucket: &SymbolBucket,
        state: &mut SymbolBucketState,
        symbol_instance: &SymbolInstance,
        feature: &SymbolFeature,
        writing_mode: WritingModeType,
        glyph_quads: &SymbolQuads,
        canonical: &CanonicalTileID,
        mut last_added_section: Option<usize>,
    ) -> (usize, usize) {
        let size_data = bucket.text_size_binder.get_vertex_size_data(feature);
        let has_format_section_overrides = bucket.has_format_section_overrides();
        let placed_icon_index = if writing_mode == WritingModeType::Vertical {
            symbol_instance.placed_vertical_icon_index
        } else {
            symbol_instance.placed_icon_index
        };

        let feature_id = if self.retain_features_by_id { feature_id_to_string(&feature.get_id()) } else { None };

        state.text.placed_symbols.push(PlacedSymbol::new(
            symbol_instance.get_anchor().point,
            symbol_instance.get_anchor().segment.unwrap_or(0),
            size_data.min,
            size_data.max,
            symbol_instance.get_text_offset(),
            writing_mode,
            symbol_instance.line().clone(),
            Self::calculate_tile_distances(symbol_instance.line(), symbol_instance.get_anchor()),
            feature_id,
            placed_icon_index,
        ));
        let placed_index = state.text.placed_symbols.len() - 1;
        state.text.placed_symbols[placed_index].angle =
            if self.allow_vertical_placement && writing_mode == WritingModeType::Vertical {
                std::f32::consts::FRAC_PI_2
            } else {
                0.0
            };

        let mut first_symbol = true;
        for symbol_quad in glyph_quads {
            if has_format_section_overrides {
                if let Some(last) = last_added_section {
                    if last != symbol_quad.section_index {
                        Self::update_paint_properties_for_section(bucket, state, feature, last, canonical);
                    }
                }
                last_added_section = Some(symbol_quad.section_index);
            }
            let index = Self::add_symbol(
                &mut state.text,
                &size_data,
                symbol_quad,
                symbol_instance.get_anchor(),
                placed_index,
                feature.sort_key,
            );
            if first_symbol {
                state.text.placed_symbols[placed_index].start_index = index;
                first_symbol = false;
            }
        }

        (last_added_section.unwrap_or(0), placed_index)
    }

    /// `addSymbol(buffer, sizeData, symbol, labelAnchor, placedSymbol,
    /// sortKey)`: adds placed items to the buffer.
    fn add_symbol(
        buffer: &mut Buffer,
        size_data: &Range<f32>,
        symbol: &SymbolQuad,
        label_anchor: &Anchor,
        placed_symbol: usize,
        sort_key: f32,
    ) -> usize {
        const VERTEX_LENGTH: usize = 4;

        let tl = symbol.tl;
        let tr = symbol.tr;
        let bl = symbol.bl;
        let br = symbol.br;
        let tex = symbol.tex;
        let pixel_offset_tl = symbol.pixel_offset_tl;
        let pixel_offset_br = symbol.pixel_offset_br;
        let min_font_scale = symbol.min_font_scale;

        let needs_segment = match buffer.segments.last() {
            None => true,
            Some(last) => {
                last.vertex_length + VERTEX_LENGTH > u16::MAX as usize
                    || (last.sort_key - sort_key).abs() > f32::EPSILON
            }
        };
        if needs_segment {
            // text-conformance: C++ `triangles.elements()` counts indices
            // (three per triangle), not triangles.
            let mut segment = mln_gfx::shaders::segment::SegmentBase::with_lengths(
                buffer.attribute_data.len(),
                buffer.triangles.len(),
                0,
                0,
            );
            segment.sort_key = sort_key;
            buffer.segments.push(segment);
        }

        // We're generating triangle fans, so we always start with the first
        // coordinate in this polygon.
        let segment = buffer.segments.last_mut().expect("segment");
        debug_assert!(segment.vertex_length <= u16::MAX as usize);
        let index = segment.vertex_length as u16;

        let anchor = label_anchor.point;
        let glyph_offset_y = symbol.glyph_offset.y;
        // coordinates (2 triangles)
        buffer.attribute_data.push(SymbolBucket::layout_attributes(
            anchor,
            tl,
            glyph_offset_y,
            tex.x,
            tex.y,
            size_data,
            symbol.is_sdf,
            pixel_offset_tl,
            min_font_scale,
        ));
        buffer.attribute_data.push(SymbolBucket::layout_attributes(
            anchor,
            tr,
            glyph_offset_y,
            tex.x + tex.w,
            tex.y,
            size_data,
            symbol.is_sdf,
            Point::new(pixel_offset_br.x, pixel_offset_tl.y),
            min_font_scale,
        ));
        buffer.attribute_data.push(SymbolBucket::layout_attributes(
            anchor,
            bl,
            glyph_offset_y,
            tex.x,
            tex.y + tex.h,
            size_data,
            symbol.is_sdf,
            Point::new(pixel_offset_tl.x, pixel_offset_br.y),
            min_font_scale,
        ));
        buffer.attribute_data.push(SymbolBucket::layout_attributes(
            anchor,
            br,
            glyph_offset_y,
            tex.x + tex.w,
            tex.y + tex.h,
            size_data,
            symbol.is_sdf,
            pixel_offset_br,
            min_font_scale,
        ));

        // Dynamic/Opacity vertices are initialized so that the vertex count
        // always agrees with the layout vertex buffer, but they will always
        // be updated before rendering happens
        let dynamic_attributes = SymbolBucket::dynamic_layout_attributes(anchor, 0.0);
        buffer.dynamic_attribute_data.extend_from_slice(&[dynamic_attributes; 4]);

        let opacity_attributes = SymbolBucket::opacity_attributes(true, 1.0);
        buffer.opacity_attribute_data.extend_from_slice(&[opacity_attributes; 4]);

        // add the two triangles, referencing the four coordinates we just
        // inserted.
        buffer.triangles.extend_from_slice(&[index, index + 1, index + 2]);
        buffer.triangles.extend_from_slice(&[index + 1, index + 2, index + 3]);

        segment.vertex_length += VERTEX_LENGTH;
        segment.index_length += 6;

        buffer.placed_symbols[placed_symbol].glyph_offsets.push(symbol.glyph_offset.x);

        index as usize
    }

    /// `addSymbols(buffer, sizeData, symbols, labelAnchor, placedSymbol, sortKey)`.
    fn add_symbols(
        buffer: &mut Buffer,
        size_data: &Range<f32>,
        symbols: &SymbolQuads,
        label_anchor: &Anchor,
        placed_symbol: usize,
        sort_key: f32,
    ) -> usize {
        let mut first_symbol = true;
        let mut first_index = 0;
        for symbol in symbols {
            let index = Self::add_symbol(buffer, size_data, symbol, label_anchor, placed_symbol, sort_key);
            if first_symbol {
                first_index = index;
                first_symbol = false;
            }
        }
        first_index
    }

    /// `addToDebugBuffers(bucket)`.
    fn add_to_debug_buffers(&self, state: &mut SymbolBucketState) {
        if !self.has_symbol_instances_in(state) {
            return;
        }

        let instances = std::mem::take(&mut state.symbol_instances);
        for symbol_instance in &instances {
            let mut populate_collision_box = |feature: &crate::text::collision_feature::CollisionFeature,
                                              is_text: bool| {
                let collision_buffer: &mut CollisionBuffer = if feature.along_line {
                    if is_text {
                        state.text_collision_circle.get_or_insert_with(Default::default)
                    } else {
                        state.icon_collision_circle.get_or_insert_with(Default::default)
                    }
                } else if is_text {
                    state.text_collision_box.get_or_insert_with(Default::default)
                } else {
                    state.icon_collision_box.get_or_insert_with(Default::default)
                };

                for b in &feature.boxes {
                    let anchor = b.anchor;

                    let tl = Point::new(b.x1, b.y1);
                    let tr = Point::new(b.x2, b.y1);
                    let bl = Point::new(b.x1, b.y2);
                    let br = Point::new(b.x2, b.y2);

                    const VERTEX_LENGTH: usize = 4;
                    let index_length = if feature.along_line { 6 } else { 8 };

                    if collision_buffer
                        .segments
                        .last()
                        .is_none_or(|last| last.vertex_length + VERTEX_LENGTH > u16::MAX as usize)
                    {
                        // text-conformance: C++ `triangles/lines.elements()`
                        // counts indices, not primitives.
                        let index_offset = collision_buffer.indices.len();
                        collision_buffer.segments.push(mln_gfx::shaders::segment::SegmentBase::new(
                            collision_buffer.vertices.len(),
                            index_offset,
                        ));
                    }

                    let segment = collision_buffer.segments.last_mut().expect("segment");
                    let index = segment.vertex_length as u16;

                    let symbol_anchor = symbol_instance.get_anchor().point;
                    collision_buffer.vertices.push(SymbolBucket::collision_layout_vertex(anchor, symbol_anchor, tl));
                    collision_buffer.vertices.push(SymbolBucket::collision_layout_vertex(anchor, symbol_anchor, tr));
                    collision_buffer.vertices.push(SymbolBucket::collision_layout_vertex(anchor, symbol_anchor, br));
                    collision_buffer.vertices.push(SymbolBucket::collision_layout_vertex(anchor, symbol_anchor, bl));

                    // Dynamic vertices are initialized so that the vertex
                    // count always agrees with the layout vertex buffer, but
                    // they will always be updated before rendering happens
                    let dynamic_vertex = SymbolBucket::collision_dynamic_vertex(false, false, Point::new(0.0, 0.0));
                    collision_buffer.dynamic_vertices.extend_from_slice(&[dynamic_vertex; 4]);

                    if feature.along_line {
                        collision_buffer.indices.extend_from_slice(&[index, index + 1, index + 2]);
                        collision_buffer.indices.extend_from_slice(&[index, index + 2, index + 3]);
                    } else {
                        collision_buffer.indices.extend_from_slice(&[index, index + 1]);
                        collision_buffer.indices.extend_from_slice(&[index + 1, index + 2]);
                        collision_buffer.indices.extend_from_slice(&[index + 2, index + 3]);
                        collision_buffer.indices.extend_from_slice(&[index + 3, index]);
                    }

                    segment.vertex_length += VERTEX_LENGTH;
                    segment.index_length += index_length;
                }
            };
            populate_collision_box(symbol_instance.get_text_collision_feature(), true);
            if let Some(feature) = symbol_instance.get_vertical_text_collision_feature() {
                populate_collision_box(feature, true);
            }
            if let Some(feature) = symbol_instance.get_vertical_icon_collision_feature() {
                populate_collision_box(feature, false);
            }
            populate_collision_box(symbol_instance.get_icon_collision_feature(), false);
        }
        state.symbol_instances = instances;
    }

    fn has_symbol_instances_in(&self, state: &SymbolBucketState) -> bool {
        !state.symbol_instances.is_empty()
    }
}

/// `deg2radf` as C++ computes it (`deg * pi / 180` in float).
trait ToRadiansCpp {
    fn to_radians_cpp(self) -> f32;
}

impl ToRadiansCpp for f32 {
    fn to_radians_cpp(self) -> f32 {
        self * std::f32::consts::PI / 180.0
    }
}

impl Layout for SymbolLayout {
    fn has_dependencies(&self) -> bool {
        !self.features.is_empty()
    }

    fn has_symbol_instances(&self) -> bool {
        !self.symbol_instances.is_empty()
    }

    fn need_finalize_symbols(&self) -> bool {
        self.need_finalize_symbols_val
    }

    /// `finalizeSymbols(HBShapeResults&)`.
    fn finalize_symbols(&mut self, results: &mut HBShapeResults) {
        for feature in self.features.iter_mut() {
            if feature.geometry.is_empty() {
                continue;
            }

            let Some(formatted_text) = &feature.formatted_text else {
                continue;
            };
            if !formatted_text.has_need_hb_shape_text() || formatted_text.hb_shaped() {
                continue;
            }

            let mut shaped_string = TaggedString::default();

            let sections = formatted_text.get_sections();
            let style_text = formatted_text.get_styled_text();

            let mut sub_string: Vec<u16> = Vec::new();
            let mut section_index = style_text.1.first().copied().unwrap_or(0);

            let mut apply_sub_string = |sub_string: &[u16], section_index: u8, shaped_string: &mut TaggedString| {
                if sub_string.is_empty() {
                    return;
                }
                let section = &sections[section_index as usize];
                if section.type_ == GlyphIDType::FontPBF {
                    shaped_string.add_text_section(
                        sub_string,
                        section.scale,
                        &section.font_stack,
                        section.type_,
                        section.key_section,
                        section.text_color,
                    );
                } else {
                    let result = results
                        .entry(section.font_stack.clone())
                        .or_default()
                        .entry(section.type_)
                        .or_default()
                        .entry(sub_string.to_vec())
                        .or_default();

                    shaped_string.add_text_section_with_adjusts(
                        &result.str_,
                        section.scale,
                        &section.font_stack,
                        section.type_,
                        &Some(result.adjusts.clone()),
                        starts_right_to_left(sub_string),
                        section.key_section,
                        section.text_color,
                    );
                }
            };

            for (ch, sec) in style_text.0.iter().zip(style_text.1.iter()) {
                if section_index != *sec {
                    apply_sub_string(&sub_string, section_index, &mut shaped_string);

                    sub_string.clear();
                    section_index = *sec;
                }

                sub_string.push(*ch);
            }

            apply_sub_string(&sub_string, section_index, &mut shaped_string);

            shaped_string.set_hb_shaped(true);
            feature.formatted_text = Some(shaped_string);
        }

        self.need_finalize_symbols_val = false;
    }

    /// `prepareSymbols(glyphMap, glyphPositions, imageMap, imagePositions)`.
    // C++'s `if (ft.formattedText && ...)`: the text is borrowed where it is
    // used, as `feature` is also passed on in between.
    #[allow(clippy::unnecessary_unwrap)]
    fn prepare_symbols(
        &mut self,
        glyph_map: &GlyphMap,
        glyph_positions: &GlyphPositions,
        image_map: &ImageMap,
        image_positions: &ImagePositions,
    ) {
        let layout = self.layout.clone();
        let zoom = self.zoom;
        let canonical = self.canonical_id;
        let is_point_placement = *layout.get::<SymbolPlacement>() == SymbolPlacementType::Point;
        let text_along_line = *layout.get::<TextRotationAlignment>() == AlignmentType::Map && !is_point_placement;

        let mut features = std::mem::take(&mut self.features);
        for (feature_index, feature) in features.iter_mut().enumerate() {
            if feature.geometry.is_empty() {
                continue;
            }

            let mut shaped_text_orientations = ShapedTextOrientations::default();
            let mut shaped_icon: Option<PositionedIcon> = None;
            let mut text_offset = [0.0_f32, 0.0_f32];
            let layout_text_size = layout.evaluate_canonical::<TextSize>(zoom + 1.0, feature, &canonical);
            let layout_text_size_at_bucket_zoom_level =
                layout.evaluate_canonical::<TextSize>(zoom, feature, &canonical);
            let layout_icon_size = layout.evaluate_canonical::<IconSize>(zoom + 1.0, feature, &canonical);

            // if feature has text, shape the text
            if feature.formatted_text.is_some() && layout_text_size > 0.0 {
                let line_height = *layout.get::<TextLineHeight>() * ONE_EM;
                let raw_text = feature.formatted_text.as_ref().expect("text").raw_text().to_vec();
                let spacing = if i18n::allows_letter_spacing(&raw_text) {
                    layout.evaluate_canonical::<TextLetterSpacing>(zoom, feature, &canonical) * ONE_EM
                } else {
                    0.0
                };
                let max_width = if is_point_placement {
                    layout.evaluate_canonical::<TextMaxWidth>(zoom, feature, &canonical) * ONE_EM
                } else {
                    0.0
                };

                let variable_anchor_offsets = self.get_text_variable_anchor_offset(feature);
                let text_anchor = layout.evaluate_canonical::<TextAnchor>(zoom, feature, &canonical);
                if variable_anchor_offsets.as_ref().is_none_or(|v| v.is_empty()) {
                    // Layers with variable anchors use the
                    // `text-radial-offset` property and the [x, y] offset
                    // vector is calculated at placement time instead of
                    // layout time
                    let radial_offset = layout.evaluate_canonical::<TextRadialOffset>(zoom, feature, &canonical);
                    if radial_offset > 0.0 {
                        // The style spec says don't use `text-offset` and
                        // `text-radial-offset` together but doesn't actually
                        // specify what happens if you use both. We go with
                        // the radial offset.
                        text_offset = evaluate_radial_offset(text_anchor, radial_offset * ONE_EM);
                    } else {
                        let offset = layout.evaluate_canonical::<TextOffset>(zoom, feature, &canonical);
                        text_offset = [offset[0] * ONE_EM, offset[1] * ONE_EM];
                    }
                }
                let mut text_justify = if text_along_line {
                    TextJustifyType::Center
                } else {
                    layout.evaluate_canonical::<TextJustify>(zoom, feature, &canonical)
                };

                let bidi = &mut self.bidi;
                let allow_vertical_placement = self.allow_vertical_placement;
                let mut apply_shaping = |formatted_text: &TaggedString,
                                         writing_mode: WritingModeType,
                                         text_anchor: SymbolAnchorType,
                                         text_justify: TextJustifyType,
                                         text_offset: &[f32; 2]|
                 -> Shaping {
                    get_shaping(
                        formatted_text,
                        max_width,
                        line_height,
                        text_anchor,
                        text_justify,
                        spacing,
                        text_offset,
                        writing_mode,
                        bidi,
                        glyph_map,
                        glyph_positions,
                        image_positions,
                        layout_text_size,
                        layout_text_size_at_bucket_zoom_level,
                        allow_vertical_placement,
                    )
                    .unwrap_or_default()
                };

                let formatted_text = feature.formatted_text.as_mut().expect("text");

                // If this layer uses text-variable-anchor, generate shapings
                // for all justification possibilities.
                if !text_along_line && variable_anchor_offsets.as_ref().is_some_and(|v| !v.is_empty()) {
                    let variable_anchor_offsets = variable_anchor_offsets.as_ref().expect("offsets");
                    let mut justifications: Vec<TextJustifyType> = Vec::new();
                    if text_justify != TextJustifyType::Auto {
                        justifications.push(text_justify);
                    } else {
                        for anchor_offset in variable_anchor_offsets.iter() {
                            justifications.push(get_anchor_justification(anchor_offset.anchor_type));
                        }
                    }
                    for justification in justifications {
                        if shaping_for_text_justify_type(&mut shaped_text_orientations, justification).is_some() {
                            continue;
                        }
                        // If using text-variable-anchor for the layer, we use
                        // a center anchor for all shapings and apply the
                        // offsets for the anchor in the placement step.
                        let shaping = apply_shaping(
                            formatted_text,
                            WritingModeType::Horizontal,
                            SymbolAnchorType::Center,
                            justification,
                            &text_offset,
                        );
                        if shaping.is_some() {
                            let single = shaping.positioned_lines.len() == 1;
                            *shaping_for_text_justify_type(&mut shaped_text_orientations, justification) = shaping;
                            if single {
                                shaped_text_orientations.single_line = true;
                                break;
                            }
                        }
                    }

                    // Vertical point label shaping if allowVerticalPlacement
                    // is enabled.
                    if allow_vertical_placement && formatted_text.allows_vertical_writing_mode() {
                        formatted_text.verticalize_punctuation();
                        // Vertical POI label placement is meant to be used
                        // for scripts that support vertical writing mode,
                        // thus, default style::TextJustifyType::Left
                        // justification is used. If Latin scripts would need
                        // to be supported, this should take into account
                        // other justifications.
                        shaped_text_orientations.vertical = apply_shaping(
                            formatted_text,
                            WritingModeType::Vertical,
                            text_anchor,
                            TextJustifyType::Left,
                            &text_offset,
                        );
                    }
                } else {
                    if text_justify == TextJustifyType::Auto {
                        text_justify = get_anchor_justification(text_anchor);
                    }

                    // Horizontal point or line label.
                    let shaping = apply_shaping(
                        formatted_text,
                        WritingModeType::Horizontal,
                        text_anchor,
                        text_justify,
                        &text_offset,
                    );
                    if shaping.is_some() {
                        shaped_text_orientations.horizontal = shaping;
                    }

                    // Vertical point label shaping if allowVerticalPlacement
                    // is enabled.
                    if allow_vertical_placement && formatted_text.allows_vertical_writing_mode() {
                        formatted_text.verticalize_punctuation();
                        shaped_text_orientations.vertical = apply_shaping(
                            formatted_text,
                            WritingModeType::Vertical,
                            text_anchor,
                            TextJustifyType::Left,
                            &text_offset,
                        );
                    }

                    // Verticalized line label.
                    if text_along_line && formatted_text.allows_vertical_writing_mode() {
                        formatted_text.verticalize_punctuation();
                        shaped_text_orientations.vertical = apply_shaping(
                            formatted_text,
                            WritingModeType::Vertical,
                            text_anchor,
                            text_justify,
                            &text_offset,
                        );
                    }
                }
            }

            // if feature has icon, get sprite atlas position
            let mut icon_type = SymbolContent::None;
            if let Some(icon) = &feature.icon {
                if let Some(image) = image_map.get(icon.id()) {
                    icon_type = SymbolContent::IconRGBA;
                    if let Some(position) = image_positions.get(icon.id()) {
                        shaped_icon = Some(PositionedIcon::shape_icon(
                            position,
                            &layout.evaluate_canonical::<IconOffset>(zoom, feature, &canonical),
                            layout.evaluate_canonical::<IconAnchor>(zoom, feature, &canonical),
                        ));
                    }
                    if image.sdf {
                        icon_type = SymbolContent::IconSDF;
                    }
                    if image.pixel_ratio != self.pixel_ratio || layout.get::<IconRotate>().constant_or(&1.0) != 0.0 {
                        self.icons_need_linear = true;
                    }
                }
            }

            // if either shapedText or icon position is present, add the
            // feature
            let default_shaping = get_default_horizontal_shaping(&shaped_text_orientations);
            self.icons_in_text |= default_shaping.is_some() && default_shaping.icons_in_text;
            if default_shaping.is_some() || shaped_icon.is_some() {
                self.add_feature(
                    feature_index,
                    feature,
                    &shaped_text_orientations,
                    shaped_icon,
                    image_map,
                    text_offset,
                    layout_text_size,
                    layout_icon_size,
                    icon_type,
                );
            }

            feature.geometry.clear();
        }
        self.features = features;

        self.compare_text.clear();
    }

    /// `createBucket(patternPositions, featureIndex, renderData, firstLoad,
    /// showCollisionBoxes, canonical)`.
    fn create_bucket(
        &mut self,
        _pattern_positions: &ImagePositions,
        _feature_index: &mut FeatureIndex,
        render_data: &mut HashMap<String, LayerRenderData>,
        first_load: bool,
        show_collision_boxes: bool,
        canonical: &CanonicalTileID,
    ) -> Result<(), ExceptionPtr> {
        let mut bucket = SymbolBucket::new(
            self.layout.clone(),
            &self.layer_paint_properties,
            &self.text_size,
            &self.icon_size,
            self.zoom,
            self.icons_need_linear,
            self.sort_features_by_y,
            self.bucket_leader_id.clone(),
            std::mem::take(&mut self.symbol_instances),
            std::mem::take(&mut self.sort_key_ranges),
            self.tile_pixel_ratio,
            self.allow_vertical_placement,
            std::mem::take(&mut self.placement_modes),
            self.icons_in_text,
        );

        bucket.set_retain_features_by_id(self.retain_features_by_id);

        let mut instances = std::mem::take(&mut bucket.state_mut().symbol_instances);
        {
            let bucket_ref = &bucket;
            // The bucket is still owned here; lock-free access through
            // `state_mut` is not possible while `bucket_ref` is borrowed, so
            // take the lock (uncontended).
            let mut state = bucket_ref.state();
            let state = &mut *state;
            for symbol_instance in instances.iter_mut() {
                let has_text = symbol_instance.has_text();
                let has_icon = symbol_instance.has_icon();
                let single_line = symbol_instance.get_single_line();

                let feature = &self.features[symbol_instance.get_layout_feature_index()];
                let retain_feature_id =
                    if self.retain_features_by_id { Some(get_retain_feature_id(feature)) } else { None };

                // Insert final placement into collision tree and add
                // glyphs/icons to buffers

                // Process icon first, so that text symbols would have
                // reference to iconIndex which is used when dynamic vertices
                // for icon-text-fit image have to be updated.
                if has_icon {
                    let size_data = bucket_ref.icon_size_binder.get_vertex_size_data(feature);
                    let sdf = symbol_instance.has_sdf_icon();
                    let mut place_icon = |icon_quads: &SymbolQuads, writing_mode: WritingModeType| -> usize {
                        let icon_buffer = if sdf { &mut state.sdf_icon } else { &mut state.icon };
                        icon_buffer.placed_symbols.push(PlacedSymbol::new(
                            symbol_instance.get_anchor().point,
                            symbol_instance.get_anchor().segment.unwrap_or(0),
                            size_data.min,
                            size_data.max,
                            symbol_instance.get_icon_offset(),
                            writing_mode,
                            symbol_instance.line().clone(),
                            Vec::new(),
                            retain_feature_id.clone(),
                            None,
                        ));
                        let index = icon_buffer.placed_symbols.len() - 1;
                        icon_buffer.placed_symbols[index].angle =
                            if self.allow_vertical_placement && writing_mode == WritingModeType::Vertical {
                                std::f32::consts::FRAC_PI_2
                            } else {
                                0.0
                            };
                        let start_index = Self::add_symbols(
                            icon_buffer,
                            &size_data,
                            icon_quads,
                            symbol_instance.get_anchor(),
                            index,
                            feature.sort_key,
                        );
                        icon_buffer.placed_symbols[index].start_index = start_index;
                        index
                    };

                    let icon_quads = symbol_instance.icon_quads().clone().unwrap_or_default();
                    let index = place_icon(&icon_quads, WritingModeType::None);
                    let vertical_index = symbol_instance
                        .vertical_icon_quads()
                        .clone()
                        .map(|vertical_icon_quads| place_icon(&vertical_icon_quads, WritingModeType::Vertical));
                    symbol_instance.placed_icon_index = Some(index);
                    if vertical_index.is_some() {
                        symbol_instance.placed_vertical_icon_index = vertical_index;
                    }

                    let length =
                        if sdf { state.sdf_icon.attribute_data.len() } else { state.icon.attribute_data.len() };
                    for binders in bucket_ref.paint_properties.values() {
                        let mut binders = binders.write_unpoisoned();
                        binders.icon_binders.populate_vertex_vectors(
                            feature,
                            length,
                            symbol_instance.get_data_feature_index(),
                            &ImagePositions::new(),
                            None,
                            canonical,
                            &Value::Null,
                        );
                    }
                }

                if has_text && feature.formatted_text.is_some() {
                    let mut last_added_section: Option<usize> = None;
                    let writing_modes = symbol_instance.get_writing_modes();
                    if single_line {
                        let quads = symbol_instance.right_justified_glyph_quads().clone();
                        let (section, placed) = self.add_symbol_glyph_quads(
                            bucket_ref,
                            state,
                            symbol_instance,
                            feature,
                            writing_modes,
                            &quads,
                            canonical,
                            last_added_section,
                        );
                        last_added_section = Some(section);
                        symbol_instance.placed_right_text_index = Some(placed);
                        symbol_instance.placed_center_text_index = Some(placed);
                        symbol_instance.placed_left_text_index = Some(placed);
                    } else {
                        if symbol_instance.get_right_justified_glyph_quads_size() != 0 {
                            let quads = symbol_instance.right_justified_glyph_quads().clone();
                            let (section, placed) = self.add_symbol_glyph_quads(
                                bucket_ref,
                                state,
                                symbol_instance,
                                feature,
                                writing_modes,
                                &quads,
                                canonical,
                                last_added_section,
                            );
                            last_added_section = Some(section);
                            symbol_instance.placed_right_text_index = Some(placed);
                        }
                        if symbol_instance.get_center_justified_glyph_quads_size() != 0 {
                            let quads = symbol_instance.center_justified_glyph_quads().clone();
                            let (section, placed) = self.add_symbol_glyph_quads(
                                bucket_ref,
                                state,
                                symbol_instance,
                                feature,
                                writing_modes,
                                &quads,
                                canonical,
                                last_added_section,
                            );
                            last_added_section = Some(section);
                            symbol_instance.placed_center_text_index = Some(placed);
                        }
                        if symbol_instance.get_left_justified_glyph_quads_size() != 0 {
                            let quads = symbol_instance.left_justified_glyph_quads().clone();
                            let (section, placed) = self.add_symbol_glyph_quads(
                                bucket_ref,
                                state,
                                symbol_instance,
                                feature,
                                writing_modes,
                                &quads,
                                canonical,
                                last_added_section,
                            );
                            last_added_section = Some(section);
                            symbol_instance.placed_left_text_index = Some(placed);
                        }
                    }
                    if (writing_modes & WritingModeType::Vertical) != WritingModeType::None
                        && symbol_instance.get_vertical_glyph_quads_size() != 0
                    {
                        let quads = symbol_instance.vertical_glyph_quads().clone();
                        let (section, placed) = self.add_symbol_glyph_quads(
                            bucket_ref,
                            state,
                            symbol_instance,
                            feature,
                            WritingModeType::Vertical,
                            &quads,
                            canonical,
                            last_added_section,
                        );
                        last_added_section = Some(section);
                        symbol_instance.placed_vertical_text_index = Some(placed);
                    }
                    debug_assert!(last_added_section.is_some()); // True, as hasText == true;
                    Self::update_paint_properties_for_section(
                        bucket_ref,
                        state,
                        feature,
                        last_added_section.unwrap_or(0),
                        canonical,
                    );
                }

                symbol_instance.release_shared_data();
            }
            state.symbol_instances = instances;

            if show_collision_boxes {
                self.add_to_debug_buffers(state);
            }
            if !first_load {
                state.just_reloaded = true;
            }
        }

        if bucket.has_data() {
            let bucket: BucketPtr = Arc::new(bucket);
            for (id, properties) in &self.layer_paint_properties {
                render_data.entry(id.clone()).or_insert_with(|| LayerRenderData {
                    bucket: bucket.clone(),
                    layer_properties: properties.clone(),
                });
            }
        }
        Ok(())
    }
}
