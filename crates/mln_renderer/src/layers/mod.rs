//! Ports src/mln/renderer/layers (the layer types M1 and M3 render).

pub mod background_layer_tweaker;
pub mod circle_layer_tweaker;
pub mod collision_layer_tweaker;
pub mod color_relief_layer_tweaker; // dem-heatmap
pub mod fill_extrusion_layer_tweaker;
pub mod fill_layer_tweaker;
pub mod heatmap_layer_tweaker; // dem-heatmap
pub mod heatmap_texture_layer_tweaker; // dem-heatmap
pub mod hillshade_layer_tweaker; // dem-heatmap
pub mod hillshade_prepare_layer_tweaker; // dem-heatmap
pub mod line_layer_tweaker;
pub mod raster_layer_tweaker;
pub mod render_background_layer;
pub mod render_circle_layer;
pub mod render_color_relief_layer; // dem-heatmap
pub mod render_fill_extrusion_layer;
pub mod render_fill_layer;
pub mod render_heatmap_layer; // dem-heatmap
pub mod render_hillshade_layer; // dem-heatmap
pub mod render_line_layer;
pub mod render_raster_layer;
pub mod render_symbol_layer;
pub mod symbol_layer_tweaker;

use std::any::Any;
use std::rc::Rc;

pub use background_layer_tweaker::BackgroundLayerTweaker;
pub use circle_layer_tweaker::CircleLayerTweaker;
pub use fill_layer_tweaker::FillLayerTweaker;
pub use line_layer_tweaker::LineLayerTweaker;
use mln_gfx::Drawable;
use mln_gfx::vertex_vector::VertexVectorBasePtr;
use mln_style::CrossfadeParameters;
use mln_style::layer_properties::{LayerProperties, get_crossfade, get_evaluated};
use mln_style::layers::{BackgroundLayerProperties, BackgroundPaintProperties};
use mln_util::immutable::Immutable;
pub use render_background_layer::RenderBackgroundLayer;
pub use render_circle_layer::RenderCircleLayer;
pub use render_fill_extrusion_layer::RenderFillExtrusionLayer;
pub use render_fill_layer::RenderFillLayer;
pub use render_line_layer::RenderLineLayer;
pub use render_raster_layer::RenderRasterLayer;
pub use render_symbol_layer::RenderSymbolLayer;

use crate::bucket::{Bucket, BucketPtr};
use crate::layer_tweaker::LayerTweakerPtr;
use crate::render_tile::RenderTile;

/// `getEvaluated<BackgroundLayerProperties>(properties)`.
pub(crate) fn background_layer_properties(
    properties: &Immutable<dyn LayerProperties>,
) -> &BackgroundPaintProperties::PossiblyEvaluated {
    get_evaluated::<BackgroundLayerProperties>(properties)
}

/// `getCrossfade<BackgroundLayerProperties>(properties)`.
pub(crate) fn background_crossfade(properties: &Immutable<dyn LayerProperties>) -> &CrossfadeParameters {
    get_crossfade::<BackgroundLayerProperties>(properties)
}

/// What `drawable->setBinders(bucket, &binders)` stores: the bucket (kept
/// alive by the drawable) and the id of the layer whose binders
/// (`bucket.paintPropertyBinders.at(id)`) the drawable uses.
pub struct DrawableBinders {
    pub bucket: BucketPtr,
    pub layer_id: String,
    /// The bucket's `version` when the drawable's attribute data was copied
    /// (see `refresh_if_bucket_updated`).
    pub version: std::cell::Cell<u64>,
}

/// The value for `Drawable::set_binders`.
pub fn drawable_binders_slot(bucket: &BucketPtr, layer_id: &str) -> Option<Rc<dyn Any>> {
    Some(Rc::new(DrawableBinders {
        bucket: bucket.clone(),
        layer_id: layer_id.to_owned(),
        version: std::cell::Cell::new(bucket.get_version()),
    }) as Rc<dyn Any>)
}

/// C++ binders update the bucket's shared vertex vectors in place (feature
/// state), and the drawables' attributes see the modification. The port's
/// drawables hold copies: when the bucket's version moved since the copy,
/// `refresh(attrs)` re-reads the data-driven attributes into the drawable's
/// attribute array (new attributes are uploaded again).
pub fn refresh_if_bucket_updated(
    drawable: &mut dyn Drawable,
    refresh: impl FnOnce(&mut mln_gfx::vertex_attribute::VertexAttributeArray),
) {
    let Some(slot) = drawable.get_binders().cloned() else {
        return;
    };
    let Some(binders) = slot.downcast_ref::<DrawableBinders>() else {
        return;
    };
    let version = binders.bucket.get_version();
    if binders.version.get() == version {
        return;
    }
    binders.version.set(version);
    if let Some(attrs) = drawable.get_vertex_attributes().cloned() {
        refresh(&mut attrs.borrow_mut());
    }
}

/// `drawable.getBucket()` + `drawable.getBinders()`: the drawable's slot
/// (cloned, so that the drawable can be mutated while the binders are
/// used); see `slot_bucket`.
pub fn get_drawable_binders(drawable: &dyn Drawable) -> Option<Rc<dyn Any>> {
    drawable.get_binders().cloned()
}

/// The bucket (downcast to the type the layer created) and the layer id of
/// the binders in a `get_drawable_binders` slot.
pub fn slot_bucket<B: Bucket>(slot: &Rc<dyn Any>) -> Option<(&B, &str)> {
    let slot = slot.downcast_ref::<DrawableBinders>()?;
    let bucket = (*slot.bucket).downcast_ref::<B>()?;
    Some((bucket, slot.layer_id.as_str()))
}

/// What `drawable->setRenderTile(renderTilesOwner, &tile)` stores: the
/// owning vector and the tile's index in it.
pub struct DrawableRenderTile {
    pub owner: Rc<Vec<RenderTile>>,
    pub index: usize,
}

impl DrawableRenderTile {
    pub fn tile(&self) -> &RenderTile {
        &self.owner[self.index]
    }
}

/// The value for `Drawable::set_render_tile`.
pub fn drawable_render_tile_slot(owner: &Rc<Vec<RenderTile>>, tile: &RenderTile) -> Option<Rc<dyn Any>> {
    let index = owner.iter().position(|t| std::ptr::eq(t, tile))?;
    Some(Rc::new(DrawableRenderTile { owner: owner.clone(), index }) as Rc<dyn Any>)
}

/// `drawable.getRenderTile()`.
pub fn get_drawable_render_tile(drawable: &dyn Drawable) -> Option<&RenderTile> {
    let slot = drawable.get_render_tile()?.downcast_ref::<DrawableRenderTile>()?;
    Some(slot.tile())
}

/// `drawable.getLayerTweaker() == layerTweaker`.
pub fn drawable_has_layer_tweaker(drawable: &dyn Drawable, layer_tweaker: &Option<LayerTweakerPtr>) -> bool {
    let current = drawable.get_layer_tweaker().and_then(|slot| slot.downcast_ref::<LayerTweakerPtr>());
    match (current, layer_tweaker) {
        (None, None) => true,
        (Some(a), Some(b)) => Rc::ptr_eq(a, b),
        _ => false,
    }
}

/// A shared gfx vertex vector holding a copy of `vertices`, for data that
/// stays in the bucket or the layer (see `bucket_geometry::released_vertices`;
/// a bucket's geometry comes from `bucket_geometry::bucket_vertices`).
pub fn shared_vertices<V: bytemuck::Pod>(vertices: &[V]) -> VertexVectorBasePtr {
    crate::bucket_geometry::released_vertices(vertices)
}

/// `checkTweakDrawable` for a tweaker whose `RefCell` is already borrowed
/// mutably by the running `execute`: compares the drawable's tweaker with
/// `self_ptr`.
pub fn check_tweak_drawable_ptr<T: ?Sized>(drawable: &dyn Drawable, self_ptr: *const T) -> bool {
    match drawable.get_layer_tweaker().and_then(|slot| slot.downcast_ref::<LayerTweakerPtr>()) {
        None => true,
        Some(tweaker) => std::ptr::addr_eq(tweaker.as_ptr(), self_ptr),
    }
}

/// `RenderLayer::getRenderDataForPass(tile, pass)` as a free function, so
/// the result borrows only the tile (the layers mutate themselves while
/// they hold it).
pub fn render_data_for_pass<'a>(
    tile: &'a RenderTile,
    base_impl: &dyn mln_style::layer::Impl,
    pass: crate::render_pass::RenderPass,
) -> Option<&'a crate::render_layer::LayerRenderData> {
    let render_data = tile.get_layer_render_data(base_impl)?;
    if (render_data.layer_properties.render_passes & pass.underlying_type()) != 0 { Some(render_data) } else { None }
}

/// `util::cast<float>(mat4)`.
pub fn cast_matrix(m: &[f64; 16]) -> [f32; 16] {
    std::array::from_fn(|i| m[i] as f32)
}

/// The `RenderLayer` members that manage a tile's drawables
/// (`updateTile`, `removeTile`), for every render layer.
pub trait TileDrawables: crate::render_layer::RenderLayer {
    /// `removeTile(RenderPass, const OverscaledTileID&)`: remove all
    /// drawables for the tile from the layer group; returns the number of
    /// drawables actually removed.
    fn remove_tile(
        &mut self,
        render_pass: crate::render_pass::RenderPass,
        tile_id: &mln_tile_id::OverscaledTileID,
    ) -> usize {
        let Some(layer_group) = self.base().layer_group.clone() else {
            return 0;
        };
        let mut group = layer_group.borrow_mut();
        let Some(tile_group) = group.as_tile_layer_group_mut() else {
            return 0;
        };
        let n = tile_group.remove_drawables(render_pass, tile_id).len();
        self.base_mut().stats.drawables_removed += n;
        n
    }

    /// `updateTile(renderPass, tileID, update)`: update the drawables for a
    /// tile. `update` returns true if the drawable was updated or false if
    /// it was skipped because it's for a previous style. Returns true if
    /// drawables were updated.
    fn update_tile(
        &mut self,
        render_pass: crate::render_pass::RenderPass,
        tile_id: &mln_tile_id::OverscaledTileID,
        mut update: impl FnMut(&mut dyn Drawable) -> bool,
    ) -> bool {
        let Some(layer_group) = self.base().layer_group.clone() else {
            return false;
        };

        let mut any_updated = false;
        let mut un_updated_drawables = false;
        {
            let mut group = layer_group.borrow_mut();
            debug_assert!(group.as_tile_layer_group().is_some());
            let Some(tile_group) = group.as_tile_layer_group_mut() else {
                return false;
            };
            tile_group.visit_drawables_for(render_pass, tile_id, |drawable| {
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

impl<T: crate::render_layer::RenderLayer + ?Sized> TileDrawables for T {}
