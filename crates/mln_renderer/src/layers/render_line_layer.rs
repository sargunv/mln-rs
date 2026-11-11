// Ports src/mln/renderer/layers/render_line_layer.hpp and
// src/mln/renderer/layers/render_line_layer.cpp (drawable path).
//
// TODO(port): captureRenderedFeatures.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use mln_gfx::shader_group::{ShaderGroupPtr, StringIDSetsPair};
use mln_gfx::shaders::shader_defines::*;
use mln_gfx::shaders::shader_program_base::ShaderProgramBasePtr;
use mln_gfx::texture2d::{SamplerState, Texture2DPtr};
use mln_gfx::types::{TextureFilterType, TextureWrapType};
use mln_gfx::vertex_vector::VertexVectorBasePtr;
use mln_gfx::{
    AttributeDataType, ColorMode, Context, CullFaceMode, DepthMaskType, DrawMode, DrawableBuilder, ShaderRegistry,
    UniqueDrawableBuilder, VertexAttributeArrayPtr,
};
use mln_map_types::TransformState;
use mln_style::expression::{Dependency, GlobalStateMap, global_state_refs_intersect};
use mln_style::layer::static_immutable_cast;
use mln_style::layer_properties::{LayerProperties, get_crossfade, get_evaluated};
use mln_style::layers::line_layer_impl::Impl as LineLayerImpl;
use mln_style::layers::line_layer_properties::*;
use mln_style::{PropertyEvaluationParameters, TransitionParameters, layer};
use mln_style_types::LineCapType;
use mln_util::Color;
use mln_util::identity::SimpleIdentity;
use mln_util::image::PremultipliedImage;
use mln_util::immutable::Immutable;
use mln_util::size::Size;
use mln_util::sync::RwLockExt;

use super::line_layer_tweaker::{LineDrawableData, LineLayerTweaker, LineType};
use super::{
    TileDrawables, drawable_binders_slot, drawable_has_layer_tweaker, drawable_render_tile_slot,
    refresh_if_bucket_updated,
};
use crate::bucket_geometry::{BucketGeometry, bucket_indexes, bucket_vertices, paint_vertices};
use crate::buckets::line_bucket::{LineBinders, LineBucket, LineLayoutVertex};
use crate::change_request::{ChangeRequest, UniqueChangeRequestVec};
use crate::layer_tweaker::{LayerTweakerPtr, layer_tweaker_slot};
use crate::line_atlas::LinePatternCap;
use crate::paint_parameters::PaintParameters;
use crate::paint_property_binder::read_data_driven_paint_property;
use crate::render_layer::{LayerPrepareParameters, RenderLayer, RenderLayerBase};
use crate::render_pass::RenderPass;
use crate::render_tree::RenderTree;
use crate::update_parameters::UpdateParameters;

fn impl_cast(impl_: &Immutable<dyn layer::Impl>) -> &LineLayerImpl {
    debug_assert!(std::ptr::eq(impl_.get_type_info(), LineLayerImpl::static_type_info()));
    impl_.downcast_ref::<LineLayerImpl>().expect("RenderLineLayer: not a line layer impl")
}

const POS_NORMAL_ATTRIB_NAME: &str = "a_pos_normal";

/// `RenderLayer::applyColorRamp(colorValue, image, globalState)`.
pub fn apply_color_ramp(
    color_value: &mln_style::color_ramp_property_value::ColorRampPropertyValue,
    image: &mut PremultipliedImage,
    global_state: Option<&GlobalStateMap>,
) -> bool {
    if color_value.is_undefined() {
        return false;
    }

    let length = image.bytes();

    let mut i: u32 = 0;
    while (i as usize) < length {
        let color = color_value.evaluate_ramp(i as f64 / length as f64, global_state);
        let i_ = i as usize;
        image.data[i_] = (color.r * 255.0).floor() as u8;
        image.data[i_ + 1] = (color.g * 255.0).floor() as u8;
        image.data[i_ + 2] = (color.b * 255.0).floor() as u8;
        image.data[i_ + 3] = (color.a * 255.0).floor() as u8;
        i += 4;
    }
    true
}

/// `vertexAttrs->readDataDrivenPaintProperties<LineColor, LineBlur,
/// LineOpacity, LineGapWidth, LineOffset, LineWidth, LineFloorWidth,
/// LinePattern>(binders, evaluated, propertiesAsUniforms,
/// idLineColorVertexAttribute)`.
fn read_line_attributes(
    attrs: &mut mln_gfx::vertex_attribute::VertexAttributeArray,
    b: &LineBinders,
    shared: &VertexVectorBasePtr,
    evaluated: &LinePaintProperties::PossiblyEvaluated,
    pau: &mut StringIDSetsPair,
) {
    let buffer = &b.interleaved_vertex_buffer;
    let mut id = ID_LINE_COLOR_VERTEX_ATTRIBUTE;
    read_data_driven_paint_property(
        attrs,
        &b.line_color,
        buffer,
        shared,
        &["a_color"],
        evaluated.get::<LineColor>().is_constant(),
        pau,
        &mut id,
    );
    read_data_driven_paint_property(
        attrs,
        &b.line_blur,
        buffer,
        shared,
        &["a_blur"],
        evaluated.get::<LineBlur>().is_constant(),
        pau,
        &mut id,
    );
    read_data_driven_paint_property(
        attrs,
        &b.line_opacity,
        buffer,
        shared,
        &["a_opacity"],
        evaluated.get::<LineOpacity>().is_constant(),
        pau,
        &mut id,
    );
    read_data_driven_paint_property(
        attrs,
        &b.line_gap_width,
        buffer,
        shared,
        &["a_gapwidth"],
        evaluated.get::<LineGapWidth>().is_constant(),
        pau,
        &mut id,
    );
    read_data_driven_paint_property(
        attrs,
        &b.line_offset,
        buffer,
        shared,
        &["a_offset"],
        evaluated.get::<LineOffset>().is_constant(),
        pau,
        &mut id,
    );
    read_data_driven_paint_property(
        attrs,
        &b.line_width,
        buffer,
        shared,
        &["a_width"],
        evaluated.get::<LineWidth>().is_constant(),
        pau,
        &mut id,
    );
    read_data_driven_paint_property(
        attrs,
        &b.line_floor_width,
        buffer,
        shared,
        &["a_floorwidth"],
        evaluated.get::<LineFloorWidth>().is_constant(),
        pau,
        &mut id,
    );
    read_data_driven_paint_property(
        attrs,
        &b.line_pattern,
        buffer,
        shared,
        &["a_pattern_to", "a_pattern_from"],
        evaluated.get::<LinePattern>().is_constant(),
        pau,
        &mut id,
    );
}

pub struct RenderLineLayer {
    base: RenderLayerBase,
    // Paint properties
    unevaluated: LinePaintProperties::Unevaluated,

    color_ramp: Rc<PremultipliedImage>,
    /// The global-state snapshot used to generate the cached color ramp and
    /// detect when it needs to be rebuilt.
    color_ramp_global_state: Option<Arc<GlobalStateMap>>,
    color_ramp_texture_2d: Option<Texture2DPtr>,

    line_shader_group: Option<ShaderGroupPtr>,
    line_gradient_shader_group: Option<ShaderGroupPtr>,
    line_sdf_shader_group: Option<ShaderGroupPtr>,
    line_gradient_sdf_shader_group: Option<ShaderGroupPtr>,
    // TODO(port): used by the line-pattern branch.
    #[allow(dead_code)]
    line_pattern_shader_group: Option<ShaderGroupPtr>,
}

impl RenderLineLayer {
    pub fn new(impl_: Immutable<LineLayerImpl>) -> Self {
        let unevaluated = impl_.paint.untransitioned();
        let properties: Immutable<dyn LayerProperties> = Arc::new(LineLayerProperties::new(impl_));
        let mut base = RenderLayerBase::new(properties);
        base.style_dependencies = unevaluated.get_dependencies();
        RenderLineLayer {
            base,
            unevaluated,
            color_ramp: Rc::new(PremultipliedImage::new(Size::new(256, 1))),
            color_ramp_global_state: None,
            color_ramp_texture_2d: None,
            line_shader_group: None,
            line_gradient_shader_group: None,
            line_sdf_shader_group: None,
            line_gradient_sdf_shader_group: None,
            line_pattern_shader_group: None,
        }
    }

    fn update_color_ramp(&mut self) {
        let color_value = self.unevaluated.get::<LineGradient>().get_value().clone();
        let mut image = (*self.color_ramp).clone();
        if !apply_color_ramp(&color_value, &mut image, self.color_ramp_global_state.as_deref()) {
            return;
        }
        // The ramp image is shared with the texture (C++ shared_ptr); a new
        // one replaces it.
        self.color_ramp = Rc::new(image);

        if self.color_ramp_texture_2d.is_some() {
            self.color_ramp_texture_2d = None;

            // delete all gradient drawables
            if let Some(layer_group) = &self.base.layer_group {
                self.base.stats.drawables_removed += layer_group.borrow().get_drawable_count();
                layer_group.borrow_mut().clear_drawables();
            }
        }
    }

    // query
    /// `getLineWidth(feature, zoom, featureState)`.
    fn get_line_width(
        &self,
        feature: &dyn mln_tile_id::geometry_tile_data::GeometryTileFeature,
        zoom: f32,
        feature_state: &mln_style::expression::expression::FeatureState,
    ) -> f32 {
        use mln_style::properties::Property;

        let evaluated = get_evaluated::<LineLayerProperties>(&self.base.evaluated_properties);
        let line_width = evaluated.get::<LineWidth>().evaluate_feature_state(
            feature,
            zoom,
            feature_state,
            LineWidth::default_value(),
        );
        let gap_width = evaluated.get::<LineGapWidth>().evaluate_feature_state(
            feature,
            zoom,
            feature_state,
            LineGapWidth::default_value(),
        );
        if gap_width != 0.0 { gap_width + 2.0 * line_width } else { line_width }
    }
}

// query
/// `offsetLine(rings, offset)` (render_line_layer.cpp).
fn offset_line(
    rings: &mln_tile_id::geometry_tile_data::GeometryCollection,
    offset: f64,
) -> Vec<mln_tile_id::geometry_tile_data::GeometryCoordinates> {
    use geometry::Point;
    use mln_util::math::{perp, unit};

    debug_assert!(offset != 0.0);
    debug_assert!(!rings.is_empty());

    let to_f64 = |p: Point<i16>| Point::new(f64::from(p.x), f64::from(p.y));
    let sub = |a: Point<i16>, b: Point<i16>| Point::new(a.x.wrapping_sub(b.x), a.y.wrapping_sub(b.y));

    let zero = Point::new(0.0, 0.0);
    let mut new_rings = Vec::with_capacity(rings.len());
    for ring in rings.iter() {
        let mut new_ring = Vec::with_capacity(ring.len());

        for i in 0..ring.len() {
            let p = ring[i];

            let a_to_b = if i == 0 { zero } else { perp(unit(to_f64(sub(p, ring[i - 1])))) };
            // At the last vertex, use the incoming normal so the endpoint receives the full offset.
            let b_to_c = if i + 1 == ring.len() { a_to_b } else { perp(unit(to_f64(sub(ring[i + 1], p)))) };
            let mut extrude = unit(Point::new(a_to_b.x + b_to_c.x, a_to_b.y + b_to_c.y));

            let cos_half_angle = extrude.x * b_to_c.x + extrude.y * b_to_c.y;
            let scale = if cos_half_angle != 0.0 { 1.0 / cos_half_angle } else { 0.0 };
            extrude = Point::new(extrude.x * scale, extrude.y * scale);

            // convertPoint<int16_t>(extrude * offset) + p
            let shifted = Point::new((extrude.x * offset) as i16, (extrude.y * offset) as i16);
            new_ring.push(Point::new(shifted.x.wrapping_add(p.x), shifted.y.wrapping_add(p.y)));
        }
        new_rings.push(new_ring);
    }

    new_rings
}

impl RenderLayer for RenderLineLayer {
    fn base(&self) -> &RenderLayerBase {
        &self.base
    }

    fn base_mut(&mut self) -> &mut RenderLayerBase {
        &mut self.base
    }

    fn transition(&mut self, parameters: &TransitionParameters) {
        let prior = std::mem::take(&mut self.unevaluated);
        self.unevaluated = impl_cast(&self.base.base_impl).paint.transitioned(parameters, prior);
        self.base.style_dependencies = self.unevaluated.get_dependencies();
    }

    fn evaluate(&mut self, parameters: &PropertyEvaluationParameters) {
        // Build the ramp here rather than in transition(), which has no global
        // state. A new or changed layer always needs one; otherwise only a
        // change to a state key that the ramp reads does.
        let global_state_differs = match (&self.color_ramp_global_state, &parameters.global_state) {
            (None, None) => false,
            (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
            _ => true,
        };
        let ramp_value = self.unevaluated.get::<LineGradient>().get_value();
        let ramp_affected = parameters.layer_changed
            || (global_state_differs
                && ramp_value.get_dependencies().intersects(Dependency::GLOBAL_STATE)
                && global_state_refs_intersect(
                    ramp_value.get_global_state_refs(),
                    parameters.changed_global_state_keys.as_deref(),
                ));
        self.color_ramp_global_state = parameters.global_state.clone();
        if ramp_affected {
            self.update_color_ramp();
        }

        let previous_evaluated = get_evaluated::<LineLayerProperties>(&self.base.evaluated_properties).clone();
        let mut properties = LineLayerProperties::with_evaluated(
            static_immutable_cast::<LineLayerImpl>(&self.base.base_impl),
            parameters.get_crossfade_parameters(),
            self.unevaluated.evaluate_with_previous(parameters, &previous_evaluated),
        );
        let evaluated = &properties.evaluated;

        self.base.passes = if evaluated.get::<LineOpacity>().constant_or(&1.0) > 0.0
            && evaluated.get::<LineColor>().constant_or(&Color::black()).a > 0.0
            && evaluated.get::<LineWidth>().constant_or(&1.0) > 0.0
        {
            RenderPass::Translucent
        } else {
            RenderPass::None
        };
        properties.base.render_passes = self.base.passes.underlying_type();
        self.base.evaluated_properties = Arc::new(properties);

        if let Some(layer_tweaker) = &self.base.layer_tweaker {
            layer_tweaker.borrow_mut().update_properties(self.base.evaluated_properties.clone());
        }
    }

    fn has_transition(&self) -> bool {
        self.unevaluated.has_transition()
    }

    fn has_crossfade(&self) -> bool {
        get_crossfade::<LineLayerProperties>(&self.base.evaluated_properties).t != 1.0
    }

    // query
    fn query_intersects_feature(
        &self,
        query_geometry: &mln_tile_id::geometry_tile_data::GeometryCoordinates,
        feature: &dyn mln_tile_id::geometry_tile_data::GeometryTileFeature,
        zoom: f32,
        transform_state: &TransformState,
        pixels_to_tile_units: f32,
        _pos_matrix: &mln_util::mat4::Mat4,
        feature_state: &mln_style::expression::expression::FeatureState,
    ) -> bool {
        use mln_style::properties::Property;

        let evaluated = get_evaluated::<LineLayerProperties>(&self.base.evaluated_properties);
        // Translate query geometry
        let translated_query_geometry = crate::geometry::feature_index::FeatureIndex::translate_query_geometry(
            query_geometry,
            evaluated.get::<LineTranslate>(),
            *evaluated.get::<LineTranslateAnchor>(),
            transform_state.get_bearing() as f32,
            pixels_to_tile_units,
        );
        let query = translated_query_geometry.as_ref().unwrap_or(query_geometry);

        // Evaluate function
        let offset = evaluated.get::<LineOffset>().evaluate_feature_state(
            feature,
            zoom,
            feature_state,
            LineOffset::default_value(),
        ) * pixels_to_tile_units;
        // Test intersection
        let half_width = (f64::from(self.get_line_width(feature, zoom, feature_state)) / 2.0
            * f64::from(pixels_to_tile_units)) as f32;

        // Apply offset to geometry
        if offset != 0.0 && !feature.get_geometries().is_empty() {
            return mln_util::intersection_tests::polygon_intersects_buffered_multi_line(
                query,
                &offset_line(feature.get_geometries(), f64::from(offset)),
                half_width,
            );
        }

        mln_util::intersection_tests::polygon_intersects_buffered_multi_line(
            query,
            feature.get_geometries(),
            half_width,
        )
    }

    fn prepare(&mut self, params: &mut LayerPrepareParameters<'_>) {
        // RenderLayer::prepare(params)
        crate::render_layer::prepare_base(self, params);
        let Some(render_tiles) = self.base.render_tiles.clone() else {
            return;
        };
        let base_impl = self.base.base_impl.clone();
        for tile in render_tiles.iter() {
            let Some(render_data) = tile.get_layer_render_data(&*base_impl) else {
                continue;
            };

            let evaluated = get_evaluated::<LineLayerProperties>(&render_data.layer_properties);
            if evaluated.get::<LineDasharray>().from.is_empty() {
                continue;
            }

            let Some(bucket) = (*render_data.bucket).downcast_ref::<LineBucket>() else {
                continue;
            };
            let cap = if *bucket.layout.get::<LineCap>() == LineCapType::Round {
                LinePatternCap::Round
            } else {
                LinePatternCap::Square
            };
            // Ensures that the dash data gets added to the atlas.
            params.line_atlas.borrow_mut().get_dash_pattern_texture(
                &evaluated.get::<LineDasharray>().from,
                &evaluated.get::<LineDasharray>().to,
                cap,
            );
        }
    }

    fn update(
        &mut self,
        shaders: &ShaderRegistry,
        context: &dyn Context,
        _state: &TransformState,
        _parameters: &Arc<UpdateParameters>,
        _paint_parameters: &PaintParameters<'_>,
        _render_tree: &RenderTree,
        changes: &mut UniqueChangeRequestVec,
    ) {
        // TODO(port): stats.renderedFeatures.clear() (rendered-feature capture).

        let Some(render_tiles) = self.base.render_tiles.clone().filter(|t| !t.is_empty()) else {
            self.remove_all_drawables();
            return;
        };

        // Set up a layer group
        if self.base.layer_group.is_none() {
            match context.create_tile_layer_group(
                self.base.layer_index,
                /*initial_capacity=*/ 64,
                self.get_id().to_owned(),
            ) {
                Some(layer_group) => self.set_layer_group(Some(layer_group), changes),
                None => return,
            }
        }
        let layer_group = self.base.layer_group.clone().expect("layer group");

        if self.base.layer_tweaker.is_none() {
            let tweaker: LayerTweakerPtr = Rc::new(RefCell::new(LineLayerTweaker::new(
                self.get_id().to_owned(),
                self.base.evaluated_properties.clone(),
            )));
            self.base.layer_tweaker = Some(tweaker.clone());
            changes.push(ChangeRequest::AddLayerTweaker(layer_group.clone(), Rc::downgrade(&tweaker)));
        }

        let render_pass =
            RenderPass(self.base.evaluated_properties.render_passes & !RenderPass::Opaque.underlying_type());

        {
            let mut group = layer_group.borrow_mut();
            let tile_layer_group = group.as_tile_layer_group_mut().expect("line layer group is a TileLayerGroup");
            let base = &self.base;
            let passes = self.base.passes;
            // If the render pass has changed or the tile has dropped out of
            // the cover set, remove it.
            let removed = tile_layer_group.remove_drawables_if(|drawable| {
                drawable.get_render_pass() != passes
                    || drawable.get_tile_id().is_some_and(|id| !base.render_tile_ids.contains_key(&id))
            });
            self.base.stats.drawables_removed += removed;

            tile_layer_group.set_stencil_tiles(Some(Rc::new(render_tiles.iter().map(|tile| tile.id).collect())));
        }

        let create_line_builder = |name: &str, shader: ShaderProgramBasePtr| -> UniqueDrawableBuilder {
            let mut builder = context.create_drawable_builder(name.to_owned());
            builder.set_shader(Some(shader));
            builder.set_render_pass(render_pass);
            builder.set_sub_layer_index(0);
            builder.set_depth_type(DepthMaskType::ReadOnly);
            builder.set_color_mode(&if render_pass == RenderPass::Translucent {
                ColorMode::alpha_blended()
            } else {
                ColorMode::unblended()
            });
            builder.set_cull_face_mode(&CullFaceMode::disabled());
            builder.set_enable_stencil(true);
            builder
        };

        let add_attributes = |builder: &mut dyn DrawableBuilder,
                              geometry: Option<&BucketGeometry>,
                              bucket: &LineBucket,
                              vertex_attrs: VertexAttributeArrayPtr| {
            let vertex_count = bucket.vertices.len();
            builder.set_raw_vertices(Vec::new(), vertex_count, AttributeDataType::Short4);

            let shared = bucket_vertices(geometry, bucket, &bucket.vertices);
            if let Some(attr) =
                vertex_attrs.borrow_mut().set(ID_LINE_POS_NORMAL_VERTEX_ATTRIBUTE, -1, AttributeDataType::Invalid, 0)
            {
                attr.set_shared_raw_data(
                    Some(shared.clone()),
                    0, // offsetof(LineLayoutVertex, a1)
                    /*vertexOffset=*/ 0,
                    std::mem::size_of::<LineLayoutVertex>() as u32,
                    AttributeDataType::Short2,
                );
            }

            if let Some(attr) =
                vertex_attrs.borrow_mut().set(ID_LINE_DATA_VERTEX_ATTRIBUTE, -1, AttributeDataType::Invalid, 0)
            {
                attr.set_shared_raw_data(
                    Some(shared),
                    4, // offsetof(LineLayoutVertex, a2)
                    /*vertexOffset=*/ 0,
                    std::mem::size_of::<LineLayoutVertex>() as u32,
                    AttributeDataType::UByte4,
                );
            }

            builder.set_vertex_attributes(Some(vertex_attrs));
        };

        let set_segments =
            |builder: &mut dyn DrawableBuilder, geometry: Option<&BucketGeometry>, bucket: &LineBucket| {
                builder.set_segments_shared(
                    DrawMode::Triangles(),
                    Some(bucket_indexes(geometry, bucket, &bucket.triangles)),
                    &bucket.segments,
                );
            };

        let layer_tweaker = self.base.layer_tweaker.clone();
        let layer_id = self.get_id().to_owned();
        let mut properties_as_uniforms: StringIDSetsPair = Default::default();
        for tile in render_tiles.iter() {
            let tile_id = *tile.get_overscaled_tile_id();

            let base_impl = self.base.base_impl.clone();
            let Some(render_data) = super::render_data_for_pass(tile, &*base_impl, render_pass) else {
                self.remove_tile(render_pass, &tile_id);
                continue;
            };

            let Some(bucket) = (*render_data.bucket).downcast_ref::<LineBucket>() else {
                continue;
            };
            if bucket.triangles.is_empty() {
                self.remove_tile(render_pass, &tile_id);
                continue;
            }

            let Some(binders_lock) = bucket.paint_property_binders.get(&layer_id) else {
                continue;
            };
            let binders_guard = binders_lock.read_unpoisoned();
            let paint_property_binders = &*binders_guard;
            let geometry = tile.get_bucket_geometry();
            let paint = || {
                paint_vertices(
                    geometry,
                    bucket,
                    &paint_property_binders.interleaved_vertex_buffer,
                    paint_property_binders.depends_on_feature_state(),
                )
            };
            let evaluated = get_evaluated::<LineLayerProperties>(&render_data.layer_properties);

            // TODO(port): captureRenderedFeatures.

            let prev_bucket_id = self.get_render_tile_bucket_id(&tile_id);
            if prev_bucket_id != SimpleIdentity::EMPTY && prev_bucket_id != bucket.base.bucket_id {
                // This tile was previously set up from a different bucket,
                // drop and re-create any drawables for it.
                self.remove_tile(render_pass, &tile_id);
            }
            self.set_render_tile_bucket_id(&tile_id, bucket.base.bucket_id);

            let update_existing = |drawable: &mut dyn mln_gfx::Drawable| {
                // A drawable produced on a previous style/bucket should not be updated.
                let current = drawable_has_layer_tweaker(drawable, &layer_tweaker);
                if current {
                    refresh_if_bucket_updated(drawable, |attrs| {
                        read_line_attributes(
                            attrs,
                            paint_property_binders,
                            &paint(),
                            evaluated,
                            &mut Default::default(),
                        )
                    });
                }
                current
            };
            if self.update_tile(render_pass, &tile_id, update_existing) {
                continue;
            }

            let round_cap = *bucket.layout.get::<LineCap>() == LineCapType::Round;
            let cap_type = if round_cap { LinePatternCap::Round } else { LinePatternCap::Square };
            let stats = &mut self.base.stats;
            let mut add_drawable = |mut drawable: mln_gfx::UniqueDrawable, type_: LineType| {
                drawable.set_tile_id(tile_id);
                drawable.set_type(type_ as usize);
                drawable.set_layer_tweaker(layer_tweaker.as_ref().and_then(layer_tweaker_slot));
                drawable.set_render_tile(drawable_render_tile_slot(render_tiles.owner(), tile));
                drawable.set_binders(drawable_binders_slot(&render_data.bucket, &layer_id));

                drawable.set_data(Some(Box::new(LineDrawableData { line_pattern_cap: cap_type })));

                let mut group = layer_group.borrow_mut();
                let tile_layer_group = group.as_tile_layer_group_mut().expect("TileLayerGroup");
                tile_layer_group.add_drawable(render_pass, &tile_id, drawable);
                stats.drawables_added += 1;
            };

            properties_as_uniforms.0.clear();
            properties_as_uniforms.1.clear();

            let vertex_attrs = context.create_vertex_attribute_array();
            read_line_attributes(
                &mut vertex_attrs.borrow_mut(),
                paint_property_binders,
                &paint(),
                evaluated,
                &mut properties_as_uniforms,
            );

            let has_dasharray = !evaluated.get::<LineDasharray>().from.is_empty();
            let has_gradient = !self.unevaluated.get::<LineGradient>().get_value().is_undefined();

            // The ramp texture is shared by every tile of the layer.
            let ensure_color_ramp_texture =
                |texture_2d: &mut Option<Texture2DPtr>, color_ramp: &Rc<PremultipliedImage>| {
                    if texture_2d.is_none() && color_ramp.valid() {
                        let texture = context.create_texture_2d();
                        {
                            let mut t = texture.borrow_mut();
                            t.set_image(Some(color_ramp.clone()));
                            t.set_sampler_configuration(SamplerState {
                                filter: TextureFilterType::Linear,
                                wrap_u: TextureWrapType::Clamp,
                                wrap_v: TextureWrapType::Clamp,
                                ..Default::default()
                            });
                        }
                        *texture_2d = Some(texture);
                    }
                };

            if has_dasharray && has_gradient {
                // dash array over a gradient
                if self.line_gradient_sdf_shader_group.is_none() {
                    self.line_gradient_sdf_shader_group = shaders.get_shader_group("LineGradientSDFShader");
                }
                let Some(group) = self.line_gradient_sdf_shader_group.clone() else {
                    continue;
                };
                let Some(shader) = group.get_or_create_shader(context, &properties_as_uniforms, POS_NORMAL_ATTRIB_NAME)
                else {
                    continue;
                };

                let mut builder = create_line_builder("lineGradientSDF", shader);

                // vertices and attributes
                add_attributes(builder.as_mut(), geometry, bucket, vertex_attrs);

                ensure_color_ramp_texture(&mut self.color_ramp_texture_2d, &self.color_ramp);
                if let Some(texture) = &self.color_ramp_texture_2d {
                    builder.set_texture(Some(texture.clone()), ID_LINE_IMAGE_TEXTURE);

                    set_segments(builder.as_mut(), geometry, bucket);

                    builder.flush(context);
                    for drawable in builder.clear_drawables() {
                        add_drawable(drawable, LineType::GradientSDF);
                    }
                }
            } else if has_dasharray {
                // dash array line (SDF)
                if self.line_sdf_shader_group.is_none() {
                    self.line_sdf_shader_group = shaders.get_shader_group("LineSDFShader");
                }
                let Some(group) = self.line_sdf_shader_group.clone() else {
                    continue;
                };
                let Some(shader) = group.get_or_create_shader(context, &properties_as_uniforms, POS_NORMAL_ATTRIB_NAME)
                else {
                    continue;
                };

                let mut builder = create_line_builder("lineSDF", shader);

                // vertices, attributes and segments
                add_attributes(builder.as_mut(), geometry, bucket, vertex_attrs);
                set_segments(builder.as_mut(), geometry, bucket);

                builder.flush(context);
                for drawable in builder.clear_drawables() {
                    add_drawable(drawable, LineType::SDF);
                }
            } else if !self.unevaluated.get::<LinePattern>().is_undefined() {
                // pattern line (conformance)
                if self.line_pattern_shader_group.is_none() {
                    self.line_pattern_shader_group = shaders.get_shader_group("LinePatternShader");
                }
                let Some(group) = self.line_pattern_shader_group.clone() else {
                    continue;
                };
                let Some(shader) = group.get_or_create_shader(context, &properties_as_uniforms, POS_NORMAL_ATTRIB_NAME)
                else {
                    continue;
                };

                let mut builder = create_line_builder("linePattern", shader);

                // vertices and attributes
                add_attributes(builder.as_mut(), geometry, bucket, vertex_attrs);

                // texture
                if let Some(atlases) = tile.get_atlas_textures().filter(|a| a.icon.borrow().is_some()) {
                    let icon_tweaker: mln_gfx::drawable_tweaker::DrawableTweakerPtr =
                        Rc::new(crate::gfx::drawable_atlases_tweaker::DrawableAtlasesTweaker::new(
                            Some(atlases.clone()),
                            None,
                            Some(ID_LINE_IMAGE_TEXTURE),
                            /*is_text*/ false,
                            /*sdf_icons*/ true, // to force linear filter
                            /*rotation_alignment*/ mln_style_types::AlignmentType::Auto,
                            /*icon_scaled*/ false,
                            /*text_size_is_zoom_constant*/ false,
                        ));

                    builder.add_tweaker(icon_tweaker);

                    set_segments(builder.as_mut(), geometry, bucket);

                    builder.flush(context);
                    for drawable in builder.clear_drawables() {
                        add_drawable(drawable, LineType::Pattern);
                    }
                }
            } else if has_gradient {
                // gradient line
                if self.line_gradient_shader_group.is_none() {
                    self.line_gradient_shader_group = shaders.get_shader_group("LineGradientShader");
                }
                let Some(group) = self.line_gradient_shader_group.clone() else {
                    continue;
                };
                let Some(shader) = group.get_or_create_shader(context, &properties_as_uniforms, POS_NORMAL_ATTRIB_NAME)
                else {
                    continue;
                };

                let mut builder = create_line_builder("lineGradient", shader);

                // vertices and attributes
                add_attributes(builder.as_mut(), geometry, bucket, vertex_attrs);

                ensure_color_ramp_texture(&mut self.color_ramp_texture_2d, &self.color_ramp);
                if let Some(texture) = &self.color_ramp_texture_2d {
                    builder.set_texture(Some(texture.clone()), ID_LINE_IMAGE_TEXTURE);

                    set_segments(builder.as_mut(), geometry, bucket);

                    builder.flush(context);
                    for drawable in builder.clear_drawables() {
                        add_drawable(drawable, LineType::Gradient);
                    }
                }
            } else {
                // simple line
                if self.line_shader_group.is_none() {
                    self.line_shader_group = shaders.get_shader_group("LineShader");
                }
                let Some(group) = self.line_shader_group.clone() else {
                    continue;
                };
                let Some(shader) = group.get_or_create_shader(context, &properties_as_uniforms, POS_NORMAL_ATTRIB_NAME)
                else {
                    continue;
                };

                let mut builder = create_line_builder("line", shader);

                // vertices, attributes and segments
                add_attributes(builder.as_mut(), geometry, bucket, vertex_attrs);
                set_segments(builder.as_mut(), geometry, bucket);

                builder.flush(context);
                for drawable in builder.clear_drawables() {
                    add_drawable(drawable, LineType::Simple);
                }
            }
        }
    }
}
