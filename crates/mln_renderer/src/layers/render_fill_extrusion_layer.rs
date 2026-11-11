// Ports src/mln/renderer/layers/render_fill_extrusion_layer.hpp and
// src/mln/renderer/layers/render_fill_extrusion_layer.cpp (drawable path,
// `!MLN_USE_FILL_EXTRUSION_INSTANCING`).
//
// TODO(port): captureRenderedFeatures (UpdateParameters::captureRenderedFeatures).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use mln_gfx::drawable_tweaker::DrawableTweakerPtr;
use mln_gfx::shader_group::{ShaderGroupPtr, StringIDSetsPair};
use mln_gfx::shaders::shader_defines::*;
use mln_gfx::{
    AttributeDataType, ColorMode, Context, CullFaceMode, DrawMode, DrawableBuilder, ShaderRegistry,
    UniqueDrawableBuilder,
};
use mln_map_types::TransformState;
use mln_style::layer::static_immutable_cast;
use mln_style::layer_properties::{LayerProperties, get_crossfade, get_evaluated};
use mln_style::layers::fill_extrusion_layer_impl::Impl as FillExtrusionLayerImpl;
use mln_style::layers::fill_extrusion_layer_properties::*;
use mln_style::{PropertyEvaluationParameters, TransitionParameters, layer};
use mln_style_types::AlignmentType;
use mln_util::identity::SimpleIdentity;
use mln_util::immutable::Immutable;
use mln_util::sync::RwLockExt;

use super::fill_extrusion_layer_tweaker::FillExtrusionLayerTweaker;
use super::{TileDrawables, drawable_binders_slot, drawable_has_layer_tweaker, drawable_render_tile_slot};
use crate::bucket_geometry::{bucket_indexes, bucket_vertices, paint_vertices};
use crate::buckets::fill_extrusion_bucket::{FillExtrusionBucket, FillExtrusionLayoutVertex};
use crate::change_request::{ChangeRequest, UniqueChangeRequestVec};
use crate::gfx::drawable_atlases_tweaker::DrawableAtlasesTweaker;
use crate::layer_tweaker::{LayerTweakerPtr, layer_tweaker_slot};
use crate::paint_parameters::PaintParameters;
use crate::paint_property_binder::read_data_driven_paint_property;
use crate::render_layer::{RenderLayer, RenderLayerBase};
use crate::render_pass::RenderPass;
use crate::render_tree::RenderTree;
use crate::update_parameters::UpdateParameters;

fn impl_cast(impl_: &Immutable<dyn layer::Impl>) -> &FillExtrusionLayerImpl {
    debug_assert!(std::ptr::eq(impl_.get_type_info(), FillExtrusionLayerImpl::static_type_info()));
    impl_.downcast_ref::<FillExtrusionLayerImpl>().expect("RenderFillExtrusionLayer: not a fill-extrusion layer impl")
}

pub struct RenderFillExtrusionLayer {
    base: RenderLayerBase,
    // Paint properties
    unevaluated: FillExtrusionPaintProperties::Unevaluated,

    fill_extrusion_group: Option<ShaderGroupPtr>,
    fill_extrusion_pattern_group: Option<ShaderGroupPtr>,
}

impl RenderFillExtrusionLayer {
    pub fn new(impl_: Immutable<FillExtrusionLayerImpl>) -> Self {
        let unevaluated = impl_.paint.untransitioned();
        let properties: Immutable<dyn LayerProperties> = Arc::new(FillExtrusionLayerProperties::new(impl_));
        let mut base = RenderLayerBase::new(properties);
        base.style_dependencies = unevaluated.get_dependencies();
        RenderFillExtrusionLayer { base, unevaluated, fill_extrusion_group: None, fill_extrusion_pattern_group: None }
    }
}

impl RenderLayer for RenderFillExtrusionLayer {
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
        let previous_evaluated = get_evaluated::<FillExtrusionLayerProperties>(&self.base.evaluated_properties).clone();
        let mut properties = FillExtrusionLayerProperties::with_evaluated(
            static_immutable_cast::<FillExtrusionLayerImpl>(&self.base.base_impl),
            parameters.get_crossfade_parameters(),
            self.unevaluated.evaluate_with_previous(parameters, &previous_evaluated),
        );

        self.base.passes = if *properties.evaluated.get::<FillExtrusionOpacity>() > 0.0 {
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
        get_crossfade::<FillExtrusionLayerProperties>(&self.base.evaluated_properties).t != 1.0
    }

    fn is_3d(&self) -> bool {
        true
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
        let evaluated = get_evaluated::<FillExtrusionLayerProperties>(&self.base.evaluated_properties);
        let translated_query_geometry = crate::geometry::feature_index::FeatureIndex::translate_query_geometry(
            query_geometry,
            evaluated.get::<FillExtrusionTranslate>(),
            *evaluated.get::<FillExtrusionTranslateAnchor>(),
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

        let render_tiles = match self.base.render_tiles.clone() {
            Some(tiles) if !tiles.is_empty() && self.base.passes != RenderPass::None => tiles,
            _ => {
                self.remove_all_drawables();
                return;
            }
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
            let tweaker: LayerTweakerPtr = Rc::new(RefCell::new(FillExtrusionLayerTweaker::new(
                self.get_id().to_owned(),
                self.base.evaluated_properties.clone(),
            )));
            self.base.layer_tweaker = Some(tweaker.clone());
            // layerGroup->addLayerTweaker(layerTweaker)
            changes.push(ChangeRequest::AddLayerTweaker(layer_group.clone(), Rc::downgrade(&tweaker)));
        }
        let layer_tweaker = self.base.layer_tweaker.clone();

        if self.fill_extrusion_group.is_none() {
            self.fill_extrusion_group = shaders.get_shader_group("FillExtrusionShader");
        }
        if self.fill_extrusion_pattern_group.is_none() {
            self.fill_extrusion_pattern_group = shaders.get_shader_group("FillExtrusionPatternShader");
        }

        let evaluated_properties = self.base.evaluated_properties.clone();
        let evaluated = get_evaluated::<FillExtrusionLayerProperties>(&evaluated_properties);

        let draw_pass = RenderPass::Translucent;

        {
            let mut group = layer_group.borrow_mut();
            let tile_layer_group = group.as_tile_layer_group_mut().expect("TileLayerGroup");
            let base = &self.base;
            let removed = tile_layer_group.remove_drawables_if(|drawable| {
                // If the render pass has changed or the tile has dropped out
                // of the cover set, remove it.
                let tile_id = drawable.get_tile_id();
                !drawable.has_render_pass(draw_pass)
                    || tile_id.is_some_and(|id| !base.render_tile_ids.contains_key(&id))
            });
            self.base.stats.drawables_removed += removed;
        }

        let layer_prefix = format!("{}/", self.get_id());
        let has_pattern = !self.unevaluated.get::<FillExtrusionPattern>().is_undefined();
        let opaque = *evaluated.get::<FillExtrusionOpacity>() >= 1.0;

        let mut depth_builder: Option<UniqueDrawableBuilder> = None;
        let mut color_builder: Option<UniqueDrawableBuilder> = None;

        let shader_group =
            if has_pattern { self.fill_extrusion_pattern_group.clone() } else { self.fill_extrusion_group.clone() };
        let Some(shader_group) = shader_group else {
            self.remove_all_drawables();
            return;
        };

        {
            let mut group = layer_group.borrow_mut();
            let tile_layer_group = group.as_tile_layer_group_mut().expect("TileLayerGroup");
            tile_layer_group.set_stencil_tiles(Some(Rc::new(render_tiles.iter().map(|tile| tile.id).collect())));
        }

        let mut properties_as_uniforms: StringIDSetsPair = Default::default();
        let layer_id = self.get_id().to_owned();
        for tile in render_tiles.iter() {
            let tile_id = *tile.get_overscaled_tile_id();

            let base_impl = self.base.base_impl.clone();
            let render_data = super::render_data_for_pass(tile, &*base_impl, draw_pass);
            let Some(render_data) = render_data.filter(|r| r.bucket.has_data()) else {
                self.remove_tile(draw_pass, &tile_id);
                continue;
            };

            let Some(bucket) = (*render_data.bucket).downcast_ref::<FillExtrusionBucket>() else {
                continue;
            };

            let prev_bucket_id = self.get_render_tile_bucket_id(&tile_id);
            if prev_bucket_id != SimpleIdentity::EMPTY && prev_bucket_id != bucket.base.bucket_id {
                // This tile was previously set up from a different bucket,
                // drop and re-create any drawables for it.
                self.remove_tile(draw_pass, &tile_id);
            }
            self.set_render_tile_bucket_id(&tile_id, bucket.base.bucket_id);

            let mut tweaker: Option<DrawableTweakerPtr> = None;
            if let Some(builder) = &mut depth_builder {
                builder.clear_tweakers();
            }
            if let Some(builder) = &mut color_builder {
                builder.clear_tweakers();
            }

            let vertex_count = bucket.vertices.len();
            let Some(binders_lock) = bucket.paint_property_binders.get(&layer_id) else {
                continue;
            };
            let binders_guard = binders_lock.read_unpoisoned();
            let binders = &*binders_guard;
            let geometry = tile.get_bucket_geometry();

            // TODO(port): captureRenderedFeatures.

            // If we already have drawables for this tile, update them.
            let update_existing = |drawable: &mut dyn mln_gfx::Drawable| {
                // A drawable produced on a previous style/bucket should not
                // be updated.
                drawable_has_layer_tweaker(drawable, &layer_tweaker)
            };
            if self.update_tile(draw_pass, &tile_id, update_existing) {
                continue;
            }

            properties_as_uniforms.0.clear();
            properties_as_uniforms.1.clear();

            let vertex_attrs = context.create_vertex_attribute_array();
            {
                let buffer = &binders.interleaved_vertex_buffer;
                let shared = paint_vertices(geometry, bucket, buffer, binders.depends_on_feature_state());
                let mut attrs = vertex_attrs.borrow_mut();
                let mut id = ID_FILL_EXTRUSION_BASE_VERTEX_ATTRIBUTE;
                let b = binders;
                let pau = &mut properties_as_uniforms;
                read_data_driven_paint_property(
                    &mut attrs,
                    &b.fill_extrusion_base,
                    buffer,
                    &shared,
                    &["a_base"],
                    evaluated.get::<FillExtrusionBase>().is_constant(),
                    pau,
                    &mut id,
                );
                read_data_driven_paint_property(
                    &mut attrs,
                    &b.fill_extrusion_color,
                    buffer,
                    &shared,
                    &["a_color"],
                    evaluated.get::<FillExtrusionColor>().is_constant(),
                    pau,
                    &mut id,
                );
                read_data_driven_paint_property(
                    &mut attrs,
                    &b.fill_extrusion_height,
                    buffer,
                    &shared,
                    &["a_height"],
                    evaluated.get::<FillExtrusionHeight>().is_constant(),
                    pau,
                    &mut id,
                );
                read_data_driven_paint_property(
                    &mut attrs,
                    &b.fill_extrusion_pattern,
                    buffer,
                    &shared,
                    &["a_pattern_to", "a_pattern_from"],
                    evaluated.get::<FillExtrusionPattern>().is_constant(),
                    pau,
                    &mut id,
                );
            }

            let Some(shader) = shader_group.get_or_create_shader(context, &properties_as_uniforms, "a_pos") else {
                continue;
            };

            // The non-pattern path in `render()` only uses two-pass
            // rendering if there's translucency. The pattern path always
            // uses two passes.
            let do_depth_pass = !opaque || has_pattern;

            if do_depth_pass && depth_builder.is_none() {
                let mut builder = context.create_drawable_builder(format!("{layer_prefix}depth"));
                builder.set_shader(Some(shader.clone()));
                builder.set_is_3d(true);
                builder.set_enable_color(false);
                builder.set_render_pass(draw_pass);
                builder.set_cull_face_mode(&CullFaceMode::back_ccw());
                builder.set_draw_priority(0);
                if let Some(tweaker) = &tweaker {
                    builder.add_tweaker(tweaker.clone());
                }
                depth_builder = Some(builder);
            }
            if color_builder.is_none() {
                let mut builder = context.create_drawable_builder(format!("{layer_prefix}color"));
                builder.set_shader(Some(shader.clone()));
                builder.set_is_3d(true);
                builder.set_enable_color(true);
                builder.set_color_mode(&ColorMode::alpha_blended());
                builder.set_render_pass(draw_pass);
                builder.set_cull_face_mode(&CullFaceMode::back_ccw());
                builder.set_draw_priority(1);
                if let Some(tweaker) = &tweaker {
                    builder.add_tweaker(tweaker.clone());
                }
                color_builder = Some(builder);
            }

            if has_pattern && tweaker.is_none() {
                if let Some(atlases) = tile.get_atlas_textures() {
                    let new_tweaker: DrawableTweakerPtr = Rc::new(DrawableAtlasesTweaker::new(
                        Some(atlases.clone()),
                        None,
                        Some(ID_FILL_EXTRUSION_IMAGE_TEXTURE),
                        /*is_text=*/ false,
                        false,
                        AlignmentType::Auto,
                        false,
                        false,
                    ));
                    if let Some(builder) = &mut depth_builder {
                        builder.add_tweaker(new_tweaker.clone());
                    }
                    if let Some(builder) = &mut color_builder {
                        builder.add_tweaker(new_tweaker.clone());
                    }
                    tweaker = Some(new_tweaker);
                }
            }
            let _ = &tweaker;

            let vertices = bucket_vertices(geometry, bucket, &bucket.vertices);
            {
                let stride = std::mem::size_of::<FillExtrusionLayoutVertex>() as u32;
                let mut attrs = vertex_attrs.borrow_mut();
                if let Some(attr) = attrs.set(ID_FILL_EXTRUSION_POS_VERTEX_ATTRIBUTE, -1, AttributeDataType::Invalid, 0)
                {
                    // offsetof(FillExtrusionLayoutVertex, a1)
                    attr.set_shared_raw_data(Some(vertices.clone()), 0, 0, stride, AttributeDataType::Short2);
                }
                if let Some(attr) =
                    attrs.set(ID_FILL_EXTRUSION_DECIMALS_ED_ATTRIBUTE, -1, AttributeDataType::Invalid, 0)
                {
                    // offsetof(FillExtrusionLayoutVertex, a2)
                    attr.set_shared_raw_data(Some(vertices.clone()), 4, 0, stride, AttributeDataType::UShort2);
                }
                if let Some(attr) =
                    attrs.set(ID_FILL_EXTRUSION_NORMAL_2D_VERTEX_ATTRIBUTE, -1, AttributeDataType::Invalid, 0)
                {
                    // offsetof(FillExtrusionLayoutVertex, a3)
                    attr.set_shared_raw_data(Some(vertices.clone()), 8, 0, stride, AttributeDataType::Short2);
                }
            }

            if do_depth_pass && let Some(builder) = &mut depth_builder {
                builder.set_raw_vertices(Vec::new(), vertex_count, AttributeDataType::Short2);
                builder.set_vertex_attributes(Some(vertex_attrs.clone()));
            }

            let color = color_builder.as_mut().expect("color builder");
            color.set_enable_stencil(do_depth_pass);
            color.set_raw_vertices(Vec::new(), vertex_count, AttributeDataType::Short2);
            color.set_vertex_attributes(Some(vertex_attrs));

            let triangles = bucket_indexes(geometry, bucket, &bucket.triangles);
            let stats = &mut self.base.stats;
            let mut finish = |builder: &mut dyn DrawableBuilder| {
                if bucket.triangles.is_empty() {
                    return;
                }
                builder.set_segments_shared(DrawMode::Triangles(), Some(triangles.clone()), &bucket.triangle_segments);

                builder.flush(context);

                let mut group = layer_group.borrow_mut();
                let tile_layer_group = group.as_tile_layer_group_mut().expect("TileLayerGroup");
                for mut drawable in builder.clear_drawables() {
                    drawable.set_tile_id(tile_id);
                    drawable.set_type(usize::from(has_pattern));
                    drawable.set_layer_tweaker(layer_tweaker.as_ref().and_then(layer_tweaker_slot));
                    drawable.set_binders(drawable_binders_slot(&render_data.bucket, &layer_id));
                    drawable.set_render_tile(drawable_render_tile_slot(render_tiles.owner(), tile));

                    tile_layer_group.add_drawable(draw_pass, &tile_id, drawable);
                    stats.drawables_added += 1;
                }
            };
            if do_depth_pass && let Some(builder) = &mut depth_builder {
                finish(builder.as_mut());
            }
            finish(color_builder.as_mut().expect("color builder").as_mut());
        }
    }
}
