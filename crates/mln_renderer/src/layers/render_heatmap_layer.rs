// Ports src/mln/renderer/layers/render_heatmap_layer.hpp and
// src/mln/renderer/layers/render_heatmap_layer.cpp (drawable path).
//
// The tiles are drawn (additively, kernel density) into an offscreen
// half-float render target; a second, non-tile layer group draws that
// texture through the color ramp onto the map.
//
// TODO(port): queryIntersectsFeature (queries, M6; C++ returns false).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use mln_gfx::shader_group::{ShaderGroupPtr, StringIDSetsPair};
use mln_gfx::shaders::segment::SegmentBase;
use mln_gfx::shaders::shader_defines::*;
use mln_gfx::texture2d::SamplerState;
use mln_gfx::types::{TextureChannelDataType, TextureFilterType, TextureWrapType};
use mln_gfx::vertex_vector::VertexVectorBasePtr;
use mln_gfx::{
    AttributeDataType, ColorMode, Context, CullFaceMode, DrawMode, Drawable, LayerGroupBasePtr, ShaderProgramBasePtr,
    ShaderRegistry,
};
use mln_map_types::TransformState;
use mln_style::expression::{Dependency, GlobalStateMap, global_state_refs_intersect};
use mln_style::layer::static_immutable_cast;
use mln_style::layer_properties::{LayerProperties, get_evaluated};
use mln_style::layers::HeatmapLayer;
use mln_style::layers::heatmap_layer_impl::Impl as HeatmapLayerImpl;
use mln_style::layers::heatmap_layer_properties::*;
use mln_style::{PropertyEvaluationParameters, TransitionParameters, layer};
use mln_tile_id::OverscaledTileID;
use mln_util::identity::SimpleIdentity;
use mln_util::image::PremultipliedImage;
use mln_util::immutable::Immutable;
use mln_util::size::Size;
use mln_util::sync::RwLockExt;

use super::heatmap_layer_tweaker::HeatmapLayerTweaker;
use super::heatmap_texture_layer_tweaker::HeatmapTextureLayerTweaker;
use super::render_line_layer::apply_color_ramp;
use super::{drawable_binders_slot, drawable_has_layer_tweaker, drawable_render_tile_slot, shared_vertices};
use crate::bucket_geometry::{bucket_indexes, bucket_vertices, paint_vertices, released_indexes};
use crate::buckets::heatmap_bucket::{HeatmapBucket, HeatmapLayoutVertex};
use crate::change_request::{ChangeRequest, UniqueChangeRequestVec};
use crate::layer_tweaker::{LayerTweakerPtr, layer_tweaker_slot};
use crate::paint_parameters::PaintParameters;
use crate::paint_property_binder::read_data_driven_paint_property;
use crate::render_layer::{RenderLayer, RenderLayerBase};
use crate::render_pass::RenderPass;
use crate::render_static_data::RenderStaticData;
use crate::render_target::{RenderTarget, RenderTargetPtr};
use crate::render_tree::RenderTree;
use crate::update_parameters::UpdateParameters;

fn impl_cast(impl_: &Immutable<dyn layer::Impl>) -> &HeatmapLayerImpl {
    debug_assert!(std::ptr::eq(impl_.get_type_info(), HeatmapLayerImpl::static_type_info()));
    impl_.downcast_ref::<HeatmapLayerImpl>().expect("RenderHeatmapLayer: not a heatmap layer impl")
}

const HEATMAP_SHADER_GROUP_NAME: &str = "HeatmapShader";
const HEATMAP_TEXTURE_SHADER_GROUP_NAME: &str = "HeatmapTextureShader";

/// `activateRenderTarget(renderTarget, activate, changes)`.
pub(crate) fn activate_render_target(
    render_target: &Option<RenderTargetPtr>,
    activate: bool,
    changes: &mut UniqueChangeRequestVec,
) {
    if let Some(render_target) = render_target {
        if activate {
            // The RenderTree has determined this render target should be
            // included in the renderable set for a frame
            changes.push(ChangeRequest::AddRenderTarget(render_target.clone()));
        } else {
            // The RenderTree is informing us we should not render anything
            changes.push(ChangeRequest::RemoveRenderTarget(render_target.clone()));
        }
    }
}

pub struct RenderHeatmapLayer {
    base: RenderLayerBase,
    // Paint properties
    unevaluated: HeatmapPaintProperties::Unevaluated,
    color_ramp: Rc<PremultipliedImage>,
    /// The global-state snapshot used to generate the cached color ramp and
    /// detect when it needs to be rebuilt.
    color_ramp_global_state: Option<Arc<GlobalStateMap>>,
    segments: Vec<SegmentBase>,

    heatmap_shader_group: Option<ShaderGroupPtr>,
    heatmap_texture_shader: Option<ShaderProgramBasePtr>,
    render_target: Option<RenderTargetPtr>,

    shared_texture_vertices: Option<VertexVectorBasePtr>,

    /// This is the layer tweaker for applying the off-screen texture to the
    /// framebuffer. The inherited layer tweaker is for applying tiles to the
    /// off-screen texture.
    texture_tweaker: Option<LayerTweakerPtr>,
}

impl RenderHeatmapLayer {
    pub fn new(impl_: Immutable<HeatmapLayerImpl>) -> Self {
        let unevaluated = impl_.paint.untransitioned();
        let properties: Immutable<dyn LayerProperties> = Arc::new(HeatmapLayerProperties::new(impl_));
        let mut base = RenderLayerBase::new(properties);
        base.style_dependencies = unevaluated.get_dependencies();
        RenderHeatmapLayer {
            base,
            unevaluated,
            color_ramp: Rc::new(PremultipliedImage::new(Size::new(256, 1))),
            color_ramp_global_state: None,
            segments: Vec::new(),
            heatmap_shader_group: None,
            heatmap_texture_shader: None,
            render_target: None,
            shared_texture_vertices: None,
            texture_tweaker: None,
        }
    }

    /// `updateColorRamp()`.
    fn update_color_ramp(&mut self) {
        let mut color_value = self.unevaluated.get::<HeatmapColor>().get_value().clone();
        if color_value.is_undefined() {
            color_value = HeatmapLayer::get_default_heatmap_color();
        }

        // The ramp image is shared with the texture (C++ shared_ptr); a new
        // one replaces it.
        let mut image = (*self.color_ramp).clone();
        apply_color_ramp(&color_value, &mut image, self.color_ramp_global_state.as_deref());
        self.color_ramp = Rc::new(image);
    }

    /// The tile layer group of the render target (`renderTarget->getLayerGroup(0)`).
    fn render_target_layer_group(&self) -> Option<LayerGroupBasePtr> {
        self.render_target.as_ref().and_then(|target| target.borrow().get_layer_group(0).cloned())
    }

    /// `removeTile(RenderPass, const OverscaledTileID&)`: the tile drawables
    /// live in the render target's layer group.
    fn remove_tile(&mut self, render_pass: RenderPass, tile_id: &OverscaledTileID) -> usize {
        let Some(layer_group) = self.render_target_layer_group() else {
            return 0;
        };
        let mut group = layer_group.borrow_mut();
        let Some(tile_layer_group) = group.as_tile_layer_group_mut() else {
            return 0;
        };
        let count = tile_layer_group.remove_drawables(render_pass, tile_id).len();
        self.base.stats.drawables_removed += count;
        count
    }

    /// `updateTile(tileLayerGroup, renderPass, tileID, update)`.
    fn update_tile(
        &mut self,
        layer_group: &LayerGroupBasePtr,
        render_pass: RenderPass,
        tile_id: &OverscaledTileID,
        mut update: impl FnMut(&mut dyn Drawable) -> bool,
    ) -> bool {
        let mut any_updated = false;
        let mut un_updated_drawables = false;
        {
            let mut group = layer_group.borrow_mut();
            let Some(tile_layer_group) = group.as_tile_layer_group_mut() else {
                return false;
            };
            tile_layer_group.visit_drawables_for(render_pass, tile_id, |drawable| {
                if update(drawable) {
                    any_updated = true;
                } else {
                    un_updated_drawables = true;
                }
            });
        }

        // If any are updated, the caller shouldn't add new ones. If none are
        // updated and some were skipped, remove those. This is to handle the
        // case that the style layer changes but the bucket is not re-created.
        if !any_updated && un_updated_drawables {
            self.remove_tile(render_pass, tile_id);
        }

        any_updated
    }
}

impl RenderLayer for RenderHeatmapLayer {
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

    fn layer_changed(
        &mut self,
        _parameters: &TransitionParameters,
        _new_layer: &Immutable<dyn layer::Impl>,
        _changes: &mut UniqueChangeRequestVec,
    ) {
        // RenderLayer::layerChanged
        self.base.layer_tweaker = None;
        self.texture_tweaker = None;
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
        let ramp_value = self.unevaluated.get::<HeatmapColor>().get_value();
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

        let previous_evaluated = get_evaluated::<HeatmapLayerProperties>(&self.base.evaluated_properties).clone();
        let mut properties = HeatmapLayerProperties::with_evaluated(
            static_immutable_cast::<HeatmapLayerImpl>(&self.base.base_impl),
            self.unevaluated.evaluate_with_previous(parameters, &previous_evaluated),
        );

        self.base.passes = if *properties.evaluated.get::<HeatmapOpacity>() > 0.0 {
            RenderPass::Translucent | RenderPass::Pass3D
        } else {
            RenderPass::None
        };
        properties.base.render_passes = self.base.passes.underlying_type();
        self.base.evaluated_properties = Arc::new(properties);

        if let Some(layer_tweaker) = &self.base.layer_tweaker {
            layer_tweaker.borrow_mut().update_properties(self.base.evaluated_properties.clone());
        }
        if let Some(texture_tweaker) = &self.texture_tweaker {
            texture_tweaker.borrow_mut().update_properties(self.base.evaluated_properties.clone());
        }
    }

    fn has_transition(&self) -> bool {
        self.unevaluated.has_transition()
    }

    fn has_crossfade(&self) -> bool {
        false
    }

    fn mark_layer_renderable(&mut self, will_render: bool, changes: &mut UniqueChangeRequestVec) {
        // RenderLayer::markLayerRenderable
        self.base.is_renderable = will_render;
        let group = self.base.layer_group.clone();
        crate::render_layer::activate_layer_group(&group, will_render, changes);

        activate_render_target(&self.render_target, will_render, changes);
    }

    fn remove_all_drawables(&mut self) -> usize {
        // RenderLayer::removeAllDrawables
        let mut removed = 0;
        if let Some(group) = &self.base.layer_group {
            let count = group.borrow().get_drawable_count();
            self.base.stats.drawables_removed += count;
            group.borrow_mut().clear_drawables();
            removed = count;
        }
        if let Some(layer_group) = self.render_target_layer_group() {
            let count = layer_group.borrow().get_drawable_count();
            removed += count;
            self.base.stats.drawables_removed += count;
            layer_group.borrow_mut().clear_drawables();
        }
        removed
    }

    fn update(
        &mut self,
        shaders: &ShaderRegistry,
        context: &dyn Context,
        state: &TransformState,
        _update_parameters: &Arc<UpdateParameters>,
        _paint_parameters: &PaintParameters<'_>,
        _render_tree: &RenderTree,
        changes: &mut UniqueChangeRequestVec,
    ) {
        let Some(render_tiles) = self.base.render_tiles.clone().filter(|t| !t.is_empty()) else {
            self.remove_all_drawables();
            return;
        };

        let viewport_size = state.get_size();
        let size = Size::new(viewport_size.width / 2, viewport_size.height / 2);

        // Set up a render target
        if self.render_target.is_none() {
            let render_target = RenderTarget::create(context, size, TextureChannelDataType::HalfFloat);
            self.render_target = Some(render_target.clone());
            activate_render_target(&self.render_target, self.base.is_renderable, changes);

            // Set up tile layer group
            let Some(tile_layer_group) =
                context.create_tile_layer_group(0, /*initial_capacity=*/ 64, self.get_id().to_owned())
            else {
                return;
            };
            render_target.borrow_mut().add_layer_group(tile_layer_group, /*replace=*/ true);
            self.texture_tweaker = None;
        }

        {
            // `renderTarget->getTexture()->setSize(size)`.
            // TODO(port): the WebGPU offscreen texture's resize callback
            // (handleResize) is not ported; the texture is resized, the
            // renderable keeps its size.
            let texture = self.render_target.as_ref().expect("render target").borrow().get_texture();
            if texture.borrow().get_size() != size {
                texture.borrow_mut().set_size(size);
            }
        }

        let Some(tile_layer_group) = self.render_target_layer_group() else {
            return;
        };

        if self.heatmap_shader_group.is_none() {
            self.heatmap_shader_group = shaders.get_shader_group(HEATMAP_SHADER_GROUP_NAME);
        }
        let Some(heatmap_shader_group) = self.heatmap_shader_group.clone() else {
            self.remove_all_drawables();
            return;
        };

        if self.base.layer_tweaker.is_none() {
            let tweaker: LayerTweakerPtr = Rc::new(RefCell::new(HeatmapLayerTweaker::new(
                self.get_id().to_owned(),
                self.base.evaluated_properties.clone(),
            )));
            self.base.layer_tweaker = Some(tweaker.clone());
            // tileLayerGroup->addLayerTweaker(layerTweaker)
            changes.push(ChangeRequest::AddLayerTweaker(tile_layer_group.clone(), Rc::downgrade(&tweaker)));
        }
        let layer_tweaker = self.base.layer_tweaker.clone();

        let render_pass = RenderPass::Translucent;

        if (render_pass.underlying_type() & self.base.evaluated_properties.render_passes) == 0 {
            return;
        }

        {
            let mut group = tile_layer_group.borrow_mut();
            let tile_group = group.as_tile_layer_group_mut().expect("heatmap render target group is a TileLayerGroup");
            let base = &self.base;
            let removed = tile_group.remove_drawables_if(|drawable| {
                drawable.get_tile_id().is_some_and(|id| !base.render_tile_ids.contains_key(&id))
            });
            self.base.stats.drawables_removed += removed;
        }

        let evaluated_properties = self.base.evaluated_properties.clone();
        let evaluated = get_evaluated::<HeatmapLayerProperties>(&evaluated_properties);
        let mut properties_as_uniforms: StringIDSetsPair = Default::default();

        let mut heatmap_shader: Option<ShaderProgramBasePtr> = None;

        for tile in render_tiles.iter() {
            let tile_id = *tile.get_overscaled_tile_id();

            let base_impl = self.base.base_impl.clone();
            let Some(render_data) = super::render_data_for_pass(tile, &*base_impl, render_pass) else {
                self.remove_tile(render_pass, &tile_id);
                continue;
            };

            let Some(bucket) = (*render_data.bucket).downcast_ref::<HeatmapBucket>() else {
                continue;
            };
            let vertex_count = bucket.vertices.len();
            let Some(binders_lock) = bucket.paint_property_binders.get(self.get_id()) else {
                continue;
            };
            let binders_guard = binders_lock.read_unpoisoned();
            let paint_property_binders = &*binders_guard;
            let geometry = tile.get_bucket_geometry();

            let prev_bucket_id = self.get_render_tile_bucket_id(&tile_id);
            if prev_bucket_id != SimpleIdentity::EMPTY && prev_bucket_id != bucket.base.bucket_id {
                // This tile was previously set up from a different bucket,
                // drop and re-create any drawables for it.
                self.remove_tile(render_pass, &tile_id);
            }
            self.set_render_tile_bucket_id(&tile_id, bucket.base.bucket_id);

            let update_existing = |drawable: &mut dyn Drawable| {
                // A drawable produced on a previous style/bucket should not
                // be updated.
                drawable_has_layer_tweaker(drawable, &layer_tweaker)
            };
            if self.update_tile(&tile_layer_group, render_pass, &tile_id, update_existing) {
                continue;
            }

            properties_as_uniforms.0.clear();
            properties_as_uniforms.1.clear();

            let heatmap_vertex_attrs = context.create_vertex_attribute_array();
            {
                let buffer = &paint_property_binders.interleaved_vertex_buffer;
                let shared =
                    paint_vertices(geometry, bucket, buffer, paint_property_binders.depends_on_feature_state());
                let mut attrs = heatmap_vertex_attrs.borrow_mut();
                let mut id = ID_HEATMAP_WEIGHT_VERTEX_ATTRIBUTE;
                let b = paint_property_binders;
                let pau = &mut properties_as_uniforms;
                read_data_driven_paint_property(
                    &mut attrs,
                    &b.heatmap_weight,
                    buffer,
                    &shared,
                    &["a_weight"],
                    evaluated.get::<HeatmapWeight>().is_constant(),
                    pau,
                    &mut id,
                );
                read_data_driven_paint_property(
                    &mut attrs,
                    &b.heatmap_radius,
                    buffer,
                    &shared,
                    &["a_radius"],
                    evaluated.get::<HeatmapRadius>().is_constant(),
                    pau,
                    &mut id,
                );
            }

            if heatmap_shader.is_none() {
                heatmap_shader = heatmap_shader_group.get_or_create_shader(context, &properties_as_uniforms, "a_pos");
                if heatmap_shader.is_none() {
                    continue;
                }
            }

            if let Some(attr) = heatmap_vertex_attrs.borrow_mut().set(
                ID_HEATMAP_POS_VERTEX_ATTRIBUTE,
                -1,
                AttributeDataType::Invalid,
                0,
            ) {
                attr.set_shared_raw_data(
                    Some(bucket_vertices(geometry, bucket, &bucket.vertices)),
                    0, // offsetof(HeatmapLayoutVertex, a1)
                    /*vertex_offset=*/ 0,
                    std::mem::size_of::<HeatmapLayoutVertex>() as u32,
                    AttributeDataType::Short2,
                );
            }

            let mut heatmap_builder = context.create_drawable_builder("heatmap".to_owned());
            heatmap_builder.set_shader(heatmap_shader.clone());
            heatmap_builder.set_enable_depth(false);
            heatmap_builder.set_color_mode(&ColorMode::additive());
            heatmap_builder.set_cull_face_mode(&CullFaceMode::disabled());
            heatmap_builder.set_render_pass(render_pass);
            heatmap_builder.set_vertex_attributes(Some(heatmap_vertex_attrs));
            heatmap_builder.set_raw_vertices(Vec::new(), vertex_count, AttributeDataType::Short2);
            heatmap_builder.set_segments_shared(
                DrawMode::Triangles(),
                Some(bucket_indexes(geometry, bucket, &bucket.triangles)),
                &bucket.segments,
            );

            heatmap_builder.flush(context);

            let mut group = tile_layer_group.borrow_mut();
            let tile_group = group.as_tile_layer_group_mut().expect("TileLayerGroup");
            for mut drawable in heatmap_builder.clear_drawables() {
                drawable.set_tile_id(tile_id);
                drawable.set_layer_tweaker(layer_tweaker.as_ref().and_then(layer_tweaker_slot));
                drawable.set_binders(drawable_binders_slot(&render_data.bucket, self.get_id()));
                drawable.set_render_tile(drawable_render_tile_slot(render_tiles.owner(), tile));
                tile_group.add_drawable(render_pass, &tile_id, drawable);
                self.base.stats.drawables_added += 1;
            }
        }

        // Set up texture layer group
        if self.base.layer_group.is_none() {
            match context.create_layer_group(
                self.base.layer_index,
                /*initial_capacity=*/ 1,
                self.get_id().to_owned(),
            ) {
                Some(layer_group) => {
                    if let Some(texture_tweaker) = &self.texture_tweaker {
                        changes
                            .push(ChangeRequest::AddLayerTweaker(layer_group.clone(), Rc::downgrade(texture_tweaker)));
                    }
                    self.set_layer_group(Some(layer_group), changes);
                }
                None => return,
            }
        }
        let layer_group = self.base.layer_group.clone().expect("layer group");
        if self.texture_tweaker.is_none() {
            let tweaker: LayerTweakerPtr = Rc::new(RefCell::new(HeatmapTextureLayerTweaker::new(
                self.get_id().to_owned(),
                self.base.evaluated_properties.clone(),
            )));
            self.texture_tweaker = Some(tweaker.clone());
            changes.push(ChangeRequest::AddLayerTweaker(layer_group.clone(), Rc::downgrade(&tweaker)));
        }
        let texture_tweaker = self.texture_tweaker.clone();

        if self.heatmap_texture_shader.is_none() {
            self.heatmap_texture_shader = context.get_generic_shader(shaders, HEATMAP_TEXTURE_SHADER_GROUP_NAME);
        }
        let Some(heatmap_texture_shader) = self.heatmap_texture_shader.clone() else {
            self.remove_all_drawables();
            return;
        };

        // TODO: Don't rebuild drawables every time
        layer_group.borrow_mut().clear_drawables();

        if self.shared_texture_vertices.is_none() {
            self.shared_texture_vertices = Some(shared_vertices(&RenderStaticData::heatmap_texture_vertices()));
        }
        let shared_texture_vertices = self.shared_texture_vertices.clone().expect("texture vertices");
        let texture_vertex_count = 4;

        let texture_vertex_attrs = context.create_vertex_attribute_array();
        if let Some(attr) =
            texture_vertex_attrs.borrow_mut().set(ID_HEATMAP_POS_VERTEX_ATTRIBUTE, -1, AttributeDataType::Invalid, 0)
        {
            attr.set_shared_raw_data(
                Some(shared_texture_vertices),
                0, // offsetof(HeatmapLayoutVertex, a1)
                /*vertex_offset=*/ 0,
                std::mem::size_of::<HeatmapLayoutVertex>() as u32,
                AttributeDataType::Short2,
            );
        }

        let mut heatmap_texture_builder = context.create_drawable_builder("heatmapTexture".to_owned());
        heatmap_texture_builder.set_shader(Some(heatmap_texture_shader));
        heatmap_texture_builder.set_enable_depth(false);
        heatmap_texture_builder.set_color_mode(&ColorMode::alpha_blended());
        heatmap_texture_builder.set_cull_face_mode(&CullFaceMode::disabled());
        heatmap_texture_builder.set_render_pass(render_pass);
        heatmap_texture_builder.set_vertex_attributes(Some(texture_vertex_attrs));
        heatmap_texture_builder.set_raw_vertices(Vec::new(), texture_vertex_count, AttributeDataType::Short2);
        if self.segments.is_empty() {
            self.segments = RenderStaticData::heatmap_texture_segments();
        }
        heatmap_texture_builder.set_segments_shared(
            DrawMode::Triangles(),
            Some(released_indexes(&RenderStaticData::quad_triangle_indices())),
            &self.segments,
        );

        let render_target_texture = self.render_target.as_ref().expect("render target").borrow().get_texture();
        heatmap_texture_builder.set_texture(Some(render_target_texture), ID_HEATMAP_IMAGE_TEXTURE);

        let texture = context.create_texture_2d();
        {
            let mut t = texture.borrow_mut();
            t.set_image(Some(self.color_ramp.clone()));
            t.set_sampler_configuration(SamplerState {
                filter: TextureFilterType::Linear,
                wrap_u: TextureWrapType::Clamp,
                wrap_v: TextureWrapType::Clamp,
                ..Default::default()
            });
        }
        heatmap_texture_builder.set_texture(Some(texture), ID_HEATMAP_COLOR_RAMP_TEXTURE);

        heatmap_texture_builder.flush(context);

        let mut group = layer_group.borrow_mut();
        let Some(texture_layer_group) = group.as_layer_group_mut() else {
            return;
        };
        for mut drawable in heatmap_texture_builder.clear_drawables() {
            drawable.set_layer_tweaker(texture_tweaker.as_ref().and_then(layer_tweaker_slot));
            texture_layer_group.add_drawable(drawable);
            self.base.stats.drawables_added += 1;
        }
    }
}
