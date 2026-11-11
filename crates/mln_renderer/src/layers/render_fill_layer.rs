// Ports src/mln/renderer/layers/render_fill_layer.hpp and
// src/mln/renderer/layers/render_fill_layer.cpp (drawable path,
// MLN_TRIANGULATE_FILL_OUTLINES = 1 as for WebGPU).
//
// TODO(port): captureRenderedFeatures.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use mln_gfx::shader_group::{ShaderGroupPtr, StringIDSetsPair};
use mln_gfx::shaders::shader_defines::*;
use mln_gfx::vertex_vector::VertexVectorBasePtr;
use mln_gfx::{
    AttributeDataType, ColorMode, Context, CullFaceMode, DepthMaskType, DrawMode, DrawableBuilder, ShaderRegistry,
    UniqueDrawableBuilder, VertexAttributeArrayPtr,
};
use mln_map_types::TransformState;
use mln_style::layer::static_immutable_cast;
use mln_style::layer_properties::{LayerProperties, get_crossfade, get_evaluated};
use mln_style::layers::fill_layer_impl::Impl as FillLayerImpl;
use mln_style::layers::fill_layer_properties::*;
use mln_style::{PropertyEvaluationParameters, TransitionParameters, layer};
use mln_util::Color;
use mln_util::identity::SimpleIdentity;
use mln_util::immutable::Immutable;
use mln_util::sync::RwLockExt;

use super::fill_layer_tweaker::FillLayerTweaker;
use super::{
    TileDrawables, drawable_binders_slot, drawable_has_layer_tweaker, drawable_render_tile_slot,
    refresh_if_bucket_updated,
};
use crate::bucket_geometry::{bucket_indexes, bucket_vertices, paint_vertices};
use crate::buckets::fill_bucket::{FillBinders, FillBucket, FillLayoutVertex};
use crate::buckets::line_bucket::LineLayoutVertex;
use crate::change_request::{ChangeRequest, UniqueChangeRequestVec};
use crate::layer_tweaker::{LayerTweakerPtr, layer_tweaker_slot};
use crate::paint_parameters::PaintParameters;
use crate::paint_property_binder::read_data_driven_paint_property;
use crate::render_layer::{RenderLayer, RenderLayerBase};
use crate::render_pass::RenderPass;
use crate::render_tree::RenderTree;
use crate::update_parameters::UpdateParameters;

const FILL_SHADER_NAME: &str = "FillShader";
const FILL_OUTLINE_SHADER_NAME: &str = "FillOutlineShader";
const FILL_PATTERN_SHADER_NAME: &str = "FillPatternShader";
const FILL_OUTLINE_PATTERN_SHADER_NAME: &str = "FillOutlinePatternShader";
const FILL_OUTLINE_TRIANGULATED_SHADER_NAME: &str = "FillOutlineTriangulatedShader";

fn impl_cast(impl_: &Immutable<dyn layer::Impl>) -> &FillLayerImpl {
    debug_assert!(std::ptr::eq(impl_.get_type_info(), FillLayerImpl::static_type_info()));
    impl_.downcast_ref::<FillLayerImpl>().expect("RenderFillLayer: not a fill layer impl")
}

/// `RenderFillLayer::FillVariant`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum FillVariant {
    Fill,
    FillPattern,
    FillOutline,
    FillOutlinePattern,
    FillOutlineTriangulated,
}

impl FillVariant {
    /// `static_cast<FillVariant>(drawable.getType())`; `None` for the
    /// values the tweaker's `default:` handles.
    pub fn from_type(type_: usize) -> Option<FillVariant> {
        match type_ {
            0 => Some(FillVariant::Fill),
            1 => Some(FillVariant::FillPattern),
            2 => Some(FillVariant::FillOutline),
            3 => Some(FillVariant::FillOutlinePattern),
            4 => Some(FillVariant::FillOutlineTriangulated),
            _ => None,
        }
    }
}

/// `vertexAttrs->readDataDrivenPaintProperties<FillColor, FillOpacity,
/// FillOutlineColor, FillPattern>(binders, evaluated, propertiesAsUniforms,
/// idFillColorVertexAttribute)`.
fn read_fill_attributes(
    attrs: &mut mln_gfx::vertex_attribute::VertexAttributeArray,
    binders: &FillBinders,
    shared: &VertexVectorBasePtr,
    evaluated: &FillPaintProperties::PossiblyEvaluated,
    pau: &mut StringIDSetsPair,
) {
    let buffer = &binders.interleaved_vertex_buffer;
    let mut id = ID_FILL_COLOR_VERTEX_ATTRIBUTE;
    read_data_driven_paint_property(
        attrs,
        &binders.fill_color,
        buffer,
        shared,
        &["a_color"],
        evaluated.get::<FillColor>().is_constant(),
        pau,
        &mut id,
    );
    read_data_driven_paint_property(
        attrs,
        &binders.fill_opacity,
        buffer,
        shared,
        &["a_opacity"],
        evaluated.get::<FillOpacity>().is_constant(),
        pau,
        &mut id,
    );
    read_data_driven_paint_property(
        attrs,
        &binders.fill_outline_color,
        buffer,
        shared,
        &["a_outline_color"],
        evaluated.get::<FillOutlineColor>().is_constant(),
        pau,
        &mut id,
    );
    read_data_driven_paint_property(
        attrs,
        &binders.fill_pattern,
        buffer,
        shared,
        &["a_pattern_to", "a_pattern_from"],
        evaluated.get::<FillPattern>().is_constant(),
        pau,
        &mut id,
    );
}

pub struct RenderFillLayer {
    base: RenderLayerBase,
    // Paint properties
    unevaluated: FillPaintProperties::Unevaluated,

    fill_shader_group: Option<ShaderGroupPtr>,
    outline_shader_group: Option<ShaderGroupPtr>,
    pattern_shader_group: Option<ShaderGroupPtr>,
    outline_pattern_shader_group: Option<ShaderGroupPtr>,
    outline_triangulated_shader_group: Option<ShaderGroupPtr>,
}

impl RenderFillLayer {
    pub fn new(impl_: Immutable<FillLayerImpl>) -> Self {
        let unevaluated = impl_.paint.untransitioned();
        let properties: Immutable<dyn LayerProperties> = Arc::new(FillLayerProperties::new(impl_));
        let mut base = RenderLayerBase::new(properties);
        base.style_dependencies = unevaluated.get_dependencies();
        RenderFillLayer {
            base,
            unevaluated,
            fill_shader_group: None,
            outline_shader_group: None,
            pattern_shader_group: None,
            outline_pattern_shader_group: None,
            outline_triangulated_shader_group: None,
        }
    }
}

impl RenderLayer for RenderFillLayer {
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
        let previous_evaluated = get_evaluated::<FillLayerProperties>(&self.base.evaluated_properties).clone();
        let mut properties = FillLayerProperties::with_evaluated(
            static_immutable_cast::<FillLayerImpl>(&self.base.base_impl),
            parameters.get_crossfade_parameters(),
            self.unevaluated.evaluate_with_previous(parameters, &previous_evaluated),
        );
        let evaluated = &mut properties.evaluated;

        if self.unevaluated.get::<FillOutlineColor>().is_undefined() {
            *evaluated.get_mut::<FillOutlineColor>() = evaluated.get::<FillColor>().clone();
        }

        self.base.passes = RenderPass::Translucent;

        if self.unevaluated.get::<FillPattern>().is_undefined()
            && evaluated.get::<FillColor>().constant_or(&Color::default()).a >= 1.0
            && evaluated.get::<FillOpacity>().constant_or(&0.0) >= 1.0
        {
            // Supply both - evaluated based on opaquePassCutoff in render().
            self.base.passes |= RenderPass::Opaque;
        }
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
        get_crossfade::<FillLayerProperties>(&self.base.evaluated_properties).t != 1.0
    }

    // query
    fn query_intersects_feature(
        &self,
        query_geometry: &mln_tile_id::geometry_tile_data::GeometryCoordinates,
        feature: &dyn mln_tile_id::geometry_tile_data::GeometryTileFeature,
        _zoom: f32,
        transform_state: &TransformState,
        pixels_to_tile_units: f32,
        _pos_matrix: &mln_util::mat4::Mat4,
        _feature_state: &mln_style::expression::expression::FeatureState,
    ) -> bool {
        let evaluated = get_evaluated::<FillLayerProperties>(&self.base.evaluated_properties);
        let translated_query_geometry = crate::geometry::feature_index::FeatureIndex::translate_query_geometry(
            query_geometry,
            evaluated.get::<FillTranslate>(),
            *evaluated.get::<FillTranslateAnchor>(),
            transform_state.get_bearing() as f32,
            pixels_to_tile_units,
        );

        mln_util::intersection_tests::polygon_intersects_multi_polygon(
            translated_query_geometry.as_ref().unwrap_or(query_geometry),
            feature.get_geometries(),
        )
    }

    fn update(
        &mut self,
        shaders: &ShaderRegistry,
        context: &dyn Context,
        _state: &TransformState,
        _update_parameters: &Arc<UpdateParameters>,
        _paint_parameters: &PaintParameters<'_>,
        _render_tree: &RenderTree,
        changes: &mut UniqueChangeRequestVec,
    ) {
        // TODO(port): stats.renderedFeatures.clear() (rendered-feature capture).

        let Some(render_tiles) = self.base.render_tiles.clone().filter(|t| !t.is_empty()) else {
            self.remove_all_drawables();
            return;
        };

        // Set up a layer group for fill
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

        if self.outline_triangulated_shader_group.is_none() {
            self.outline_triangulated_shader_group = shaders.get_shader_group(FILL_OUTLINE_TRIANGULATED_SHADER_NAME);
        }

        if self.base.layer_tweaker.is_none() {
            let tweaker: LayerTweakerPtr = Rc::new(RefCell::new(FillLayerTweaker::new(
                self.get_id().to_owned(),
                self.base.evaluated_properties.clone(),
            )));
            self.base.layer_tweaker = Some(tweaker.clone());
            changes.push(ChangeRequest::AddLayerTweaker(layer_group.clone(), Rc::downgrade(&tweaker)));
        }

        if self.fill_shader_group.is_none() {
            self.fill_shader_group = shaders.get_shader_group(FILL_SHADER_NAME);
        }
        if self.outline_shader_group.is_none() {
            self.outline_shader_group = shaders.get_shader_group(FILL_OUTLINE_SHADER_NAME);
        }
        if self.pattern_shader_group.is_none() {
            self.pattern_shader_group = shaders.get_shader_group(FILL_PATTERN_SHADER_NAME);
        }
        if self.outline_pattern_shader_group.is_none() {
            self.outline_pattern_shader_group = shaders.get_shader_group(FILL_OUTLINE_PATTERN_SHADER_NAME);
        }
        let (Some(fill_shader_group), Some(outline_shader_group), Some(_), Some(_)) = (
            self.fill_shader_group.clone(),
            self.outline_shader_group.clone(),
            self.pattern_shader_group.clone(),
            self.outline_pattern_shader_group.clone(),
        ) else {
            self.remove_all_drawables();
            return;
        };

        let mut fill_builder: Option<UniqueDrawableBuilder> = None;
        let mut outline_builder: Option<UniqueDrawableBuilder> = None;
        // conformance: fill-pattern
        let mut pattern_builder: Option<UniqueDrawableBuilder> = None;
        let mut outline_pattern_builder: Option<UniqueDrawableBuilder> = None;

        let layer_prefix = format!("{}/", self.get_id());
        let render_pass = RenderPass::Translucent;
        const LINE_WIDTH: f32 = 2.0;

        let common_init = |builder: &mut dyn DrawableBuilder| {
            builder.set_cull_face_mode(&CullFaceMode::disabled());
            builder.set_enable_stencil(true);
        };

        {
            let mut group = layer_group.borrow_mut();
            let tile_layer_group = group.as_tile_layer_group_mut().expect("fill layer group is a TileLayerGroup");
            let base = &self.base;
            // If the render pass has changed or the tile has dropped out of
            // the cover set, remove it.
            let removed = tile_layer_group.remove_drawables_if(|drawable| {
                drawable.get_tile_id().is_some_and(|id| !base.render_tile_ids.contains_key(&id))
            });
            self.base.stats.drawables_removed += removed;

            tile_layer_group.set_stencil_tiles(Some(Rc::new(render_tiles.iter().map(|tile| tile.id).collect())));
        }

        let layer_tweaker = self.base.layer_tweaker.clone();
        let mut properties_as_uniforms: StringIDSetsPair = Default::default();
        for tile in render_tiles.iter() {
            let tile_id = *tile.get_overscaled_tile_id();

            let base_impl = self.base.base_impl.clone();
            let render_data = super::render_data_for_pass(tile, &*base_impl, render_pass);
            let Some(render_data) = render_data.filter(|r| r.bucket.has_data()) else {
                self.remove_tile(render_pass, &tile_id);
                continue;
            };

            let Some(bucket) = (*render_data.bucket).downcast_ref::<FillBucket>() else {
                continue;
            };
            let Some(binders_lock) = bucket.paint_property_binders.get(self.get_id()) else {
                continue;
            };
            let binders_guard = binders_lock.read_unpoisoned();
            let binders = &*binders_guard;

            let prev_bucket_id = self.get_render_tile_bucket_id(&tile_id);
            if prev_bucket_id != SimpleIdentity::EMPTY && prev_bucket_id != bucket.base.bucket_id {
                // This tile was previously set up from a different bucket,
                // drop and re-create any drawables for it.
                self.remove_tile(render_pass, &tile_id);
            }
            self.set_render_tile_bucket_id(&tile_id, bucket.base.bucket_id);

            let evaluated = get_evaluated::<FillLayerProperties>(&render_data.layer_properties);
            let geometry = tile.get_bucket_geometry();
            let paint = || {
                paint_vertices(geometry, bucket, &binders.interleaved_vertex_buffer, binders.depends_on_feature_state())
            };

            // (getAtlasTweaker: in the pattern branch.) TODO(port):
            // captureRenderedFeatures.

            // Drawables' stored attributes point directly into the bucket's
            // shared vectors; the backend upload guard fires automatically
            // when those change.
            let update_existing = |drawable: &mut dyn mln_gfx::Drawable| {
                let current = drawable_has_layer_tweaker(drawable, &layer_tweaker);
                if current && FillVariant::from_type(drawable.get_type()) != Some(FillVariant::FillOutlineTriangulated)
                {
                    refresh_if_bucket_updated(drawable, |attrs| {
                        read_fill_attributes(attrs, binders, &paint(), evaluated, &mut Default::default())
                    });
                }
                current
            };
            if self.update_tile(render_pass, &tile_id, update_existing) {
                continue;
            }

            properties_as_uniforms.0.clear();
            properties_as_uniforms.1.clear();

            // `Fill*Program` all use `style::FillPaintProperties`
            let vertex_attrs: VertexAttributeArrayPtr = context.create_vertex_attribute_array();
            read_fill_attributes(
                &mut vertex_attrs.borrow_mut(),
                binders,
                &paint(),
                evaluated,
                &mut properties_as_uniforms,
            );

            // Outline always occurs in translucent pass, defaults to fill
            // color. Outline does not default to fill in the pattern case
            let do_outline = *evaluated.get::<FillAntialias>()
                && (self.unevaluated.get::<FillPattern>().is_undefined()
                    || self.unevaluated.get::<FillOutlineColor>().is_undefined());
            let data_driven_outline =
                !evaluated.get::<FillOutlineColor>().is_constant() || !evaluated.get::<FillOpacity>().is_constant();

            let fill_vertex_count = bucket.vertices.len();

            if let Some(attr) =
                vertex_attrs.borrow_mut().set(ID_FILL_POS_VERTEX_ATTRIBUTE, -1, AttributeDataType::Invalid, 0)
            {
                attr.set_shared_raw_data(
                    Some(bucket_vertices(geometry, bucket, &bucket.vertices)),
                    0, // offsetof(FillLayoutVertex, a1)
                    /*vertexOffset=*/ 0,
                    std::mem::size_of::<FillLayoutVertex>() as u32,
                    AttributeDataType::Short2,
                );
            }

            let line_vertex_count = bucket.line_vertices.len();
            let get_triangulated_attributes = || {
                let attrs = context.create_vertex_attribute_array();
                let shared = bucket_vertices(geometry, bucket, &bucket.line_vertices);
                if let Some(attr) =
                    attrs.borrow_mut().set(ID_LINE_POS_NORMAL_VERTEX_ATTRIBUTE, -1, AttributeDataType::Invalid, 0)
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
                    attrs.borrow_mut().set(ID_LINE_DATA_VERTEX_ATTRIBUTE, -1, AttributeDataType::Invalid, 0)
                {
                    attr.set_shared_raw_data(
                        Some(shared),
                        4, // offsetof(LineLayoutVertex, a2)
                        /*vertexOffset=*/ 0,
                        std::mem::size_of::<LineLayoutVertex>() as u32,
                        AttributeDataType::UByte4,
                    );
                }
                attrs
            };

            let base_stats = &mut self.base.stats;
            let layer_id = self.base.base_impl.id.clone();
            let finish =
                |builder: &mut dyn DrawableBuilder, type_: FillVariant, base_stats: &mut crate::render_layer::Stats| {
                    builder.flush(context);

                    let mut group = layer_group.borrow_mut();
                    let tile_layer_group = group.as_tile_layer_group_mut().expect("TileLayerGroup");
                    for mut drawable in builder.clear_drawables() {
                        drawable.set_tile_id(tile_id);
                        drawable.set_type(type_ as usize);
                        drawable.set_layer_tweaker(layer_tweaker.as_ref().and_then(layer_tweaker_slot));
                        drawable.set_binders(drawable_binders_slot(&render_data.bucket, &layer_id));
                        drawable.set_render_tile(drawable_render_tile_slot(render_tiles.owner(), tile));
                        tile_layer_group.add_drawable(render_pass, &tile_id, drawable);
                        base_stats.drawables_added += 1;
                    }
                };

            if self.unevaluated.get::<FillPattern>().is_undefined() {
                // Simple fill
                let fill_shader = fill_shader_group.get_or_create_shader(context, &properties_as_uniforms, "a_pos");

                let outline_triangulated_shader = if do_outline && !data_driven_outline {
                    let outline_properties_as_uniforms: StringIDSetsPair = (
                        ["a_color", "a_opacity", "a_width"].iter().map(|s| (*s).to_owned()).collect(),
                        [
                            ID_LINE_COLOR_VERTEX_ATTRIBUTE,
                            ID_LINE_OPACITY_VERTEX_ATTRIBUTE,
                            ID_LINE_WIDTH_VERTEX_ATTRIBUTE,
                        ]
                        .into_iter()
                        .collect(),
                    );
                    self.outline_triangulated_shader_group
                        .as_ref()
                        .and_then(|group| group.get_or_create_shader(context, &outline_properties_as_uniforms, "a_pos"))
                } else {
                    None
                };

                let outline_shader = if do_outline {
                    outline_shader_group.get_or_create_shader(context, &properties_as_uniforms, "a_pos")
                } else {
                    None
                };

                if fill_builder.is_none() && fill_shader.is_some() {
                    let mut builder = context.create_drawable_builder(format!("{layer_prefix}fill"));
                    // Only write opaque fills to the depth buffer, matching
                    // `fillRenderPass` in legacy rendering
                    let opaque = evaluated.get::<FillColor>().constant_or(&Color::default()).a >= 1.0
                        && evaluated.get::<FillOpacity>().constant_or(&0.0) >= 1.0;

                    common_init(builder.as_mut());
                    builder.set_depth_type(if opaque { DepthMaskType::ReadWrite } else { DepthMaskType::ReadOnly });
                    builder.set_color_mode(&ColorMode::alpha_blended());
                    builder.set_sub_layer_index(1);
                    builder.set_render_pass(render_pass);
                    fill_builder = Some(builder);
                }
                if do_outline && outline_builder.is_none() && outline_shader.is_some() {
                    let mut builder = context.create_drawable_builder(format!("{layer_prefix}fill-outline"));
                    common_init(builder.as_mut());
                    builder.set_depth_type(DepthMaskType::ReadOnly);
                    builder.set_line_width(LINE_WIDTH);
                    builder.set_sub_layer_index(if self.unevaluated.get::<FillOutlineColor>().is_undefined() {
                        2
                    } else {
                        0
                    });
                    builder.set_color_mode(&ColorMode::alpha_blended());
                    builder.set_render_pass(RenderPass::Translucent);
                    outline_builder = Some(builder);
                }

                if let Some(builder) = fill_builder.as_mut() {
                    if !bucket.triangles.is_empty() {
                        builder.set_shader(fill_shader.clone());
                        if do_outline && data_driven_outline {
                            if let Some(outline) = outline_builder.as_mut() {
                                outline.set_vertex_attributes(Some(vertex_attrs.clone()));
                            }
                        }
                        builder.set_vertex_attributes(Some(vertex_attrs.clone()));

                        builder.set_raw_vertices(Vec::new(), fill_vertex_count, AttributeDataType::Short2);
                        builder.set_segments_shared(
                            DrawMode::Triangles(),
                            Some(bucket_indexes(geometry, bucket, &bucket.triangles)),
                            &bucket.triangle_segments,
                        );
                        finish(builder.as_mut(), FillVariant::Fill, base_stats);
                    }
                }

                if do_outline {
                    if let Some(builder) = outline_builder.as_mut() {
                        if !data_driven_outline {
                            builder.set_sub_layer_index(if self.unevaluated.get::<FillOutlineColor>().is_undefined() {
                                2
                            } else {
                                0
                            });
                            // createOutlineTriangulated(outlineBuilder)
                            if line_vertex_count != 0 {
                                builder.set_shader(outline_triangulated_shader.clone());
                                builder.set_raw_vertices(Vec::new(), line_vertex_count, AttributeDataType::Short2);
                                builder.set_vertex_attributes(Some(get_triangulated_attributes()));
                                builder.set_segments_shared(
                                    DrawMode::Triangles(),
                                    Some(bucket_indexes(geometry, bucket, &bucket.line_indexes)),
                                    &bucket.line_segments,
                                );
                                finish(builder.as_mut(), FillVariant::FillOutlineTriangulated, base_stats);
                            }
                        } else if !bucket.basic_lines.is_empty() {
                            builder.set_shader(outline_shader.clone());
                            builder.set_raw_vertices(Vec::new(), fill_vertex_count, AttributeDataType::Short2);
                            builder.set_segments_shared(
                                DrawMode::Lines(LINE_WIDTH),
                                Some(bucket_indexes(geometry, bucket, &bucket.basic_lines)),
                                &bucket.basic_line_segments,
                            );
                            finish(builder.as_mut(), FillVariant::FillOutline, base_stats);
                        }
                    }
                }
            } else {
                // Fill with pattern (conformance)
                if (render_pass.underlying_type() & RenderPass::Translucent.underlying_type()) == 0 {
                    continue;
                }

                let (Some(pattern_shader_group), Some(outline_pattern_shader_group)) =
                    (self.pattern_shader_group.clone(), self.outline_pattern_shader_group.clone())
                else {
                    continue;
                };

                let fill_shader = pattern_shader_group.get_or_create_shader(context, &properties_as_uniforms, "a_pos");
                let outline_shader = if do_outline {
                    outline_pattern_shader_group.get_or_create_shader(context, &properties_as_uniforms, "a_pos")
                } else {
                    None
                };

                // getAtlasTweaker()
                let atlas_tweaker: Option<mln_gfx::drawable_tweaker::DrawableTweakerPtr> =
                    tile.get_atlas_textures().filter(|a| a.icon.borrow().is_some()).map(|atlases| {
                        Rc::new(crate::gfx::drawable_atlases_tweaker::DrawableAtlasesTweaker::new(
                            Some(atlases.clone()),
                            None,
                            Some(ID_FILL_IMAGE_TEXTURE),
                            /*is_text*/ false,
                            /*sdf_icons*/ true, // to force linear filter
                            /*rotation_alignment*/ mln_style_types::AlignmentType::Auto,
                            /*icon_scaled*/ false,
                            /*text_size_is_zoom_constant*/ false,
                        )) as mln_gfx::drawable_tweaker::DrawableTweakerPtr
                    });

                if pattern_builder.is_none() {
                    let mut builder = context.create_drawable_builder(format!("{layer_prefix}fill-pattern"));
                    common_init(builder.as_mut());
                    builder.set_shader(fill_shader.clone());
                    builder.set_depth_type(DepthMaskType::ReadWrite);
                    builder.set_color_mode(&ColorMode::alpha_blended());
                    builder.set_sub_layer_index(1);
                    builder.set_render_pass(RenderPass::Translucent);
                    pattern_builder = Some(builder);
                }
                if do_outline && outline_pattern_builder.is_none() {
                    let mut builder = context.create_drawable_builder(format!("{layer_prefix}fill-outline-pattern"));
                    common_init(builder.as_mut());
                    builder.set_shader(outline_shader.clone());
                    builder.set_line_width(LINE_WIDTH);
                    builder.set_depth_type(DepthMaskType::ReadOnly);
                    builder.set_color_mode(&ColorMode::alpha_blended());
                    builder.set_sub_layer_index(2);
                    builder.set_render_pass(RenderPass::Translucent);
                    outline_pattern_builder = Some(builder);
                }

                if let Some(builder) = pattern_builder.as_mut() {
                    builder.clear_tweakers();
                    if let Some(tweaker) = &atlas_tweaker {
                        builder.add_tweaker(tweaker.clone());
                    }
                }
                if do_outline && let Some(builder) = outline_pattern_builder.as_mut() {
                    builder.clear_tweakers();
                    if let Some(tweaker) = &atlas_tweaker {
                        builder.add_tweaker(tweaker.clone());
                    }
                }

                if let Some(builder) = pattern_builder.as_mut()
                    && !bucket.triangles.is_empty()
                {
                    builder.set_shader(fill_shader.clone());
                    builder.set_render_pass(render_pass);
                    if do_outline && let Some(outline) = outline_pattern_builder.as_mut() {
                        outline.set_vertex_attributes(Some(vertex_attrs.clone()));
                    }
                    builder.set_vertex_attributes(Some(vertex_attrs.clone()));
                    builder.set_raw_vertices(Vec::new(), fill_vertex_count, AttributeDataType::Short2);
                    builder.set_segments_shared(
                        DrawMode::Triangles(),
                        Some(bucket_indexes(geometry, bucket, &bucket.triangles)),
                        &bucket.triangle_segments,
                    );

                    finish(builder.as_mut(), FillVariant::FillPattern, base_stats);
                }

                if do_outline
                    && let Some(builder) = outline_pattern_builder.as_mut()
                    && !bucket.basic_lines.is_empty()
                {
                    builder.set_shader(outline_shader.clone());
                    builder.set_render_pass(render_pass);
                    builder.set_raw_vertices(Vec::new(), fill_vertex_count, AttributeDataType::Short2);
                    builder.set_segments_shared(
                        DrawMode::Lines(LINE_WIDTH),
                        Some(bucket_indexes(geometry, bucket, &bucket.basic_lines)),
                        &bucket.basic_line_segments,
                    );

                    finish(builder.as_mut(), FillVariant::FillOutlinePattern, base_stats);
                }
            }
        }
    }
}
