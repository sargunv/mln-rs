// Ports src/mln/renderer/render_tile.hpp and src/mln/renderer/render_tile.cpp.
//
// C++ `RenderTile` keeps a `Tile&`. A Rust RenderTile cannot borrow the tile
// out of the TilePyramid for the frame, so it copies what it reads from the
// tile (its id and hold-for-fade state) when it is created and prepared;
// both are only changed by the TilePyramid between frames.

use std::rc::Rc;

use geometry::Point;
use mln_gfx::UploadPass;
use mln_map_types::{MapDebugOptions, TransformState};
use mln_style::layer;
use mln_style_types::TranslateAnchorType;
use mln_style_types::image::ImagePosition;
use mln_tile_id::{OverscaledTileID, UnwrappedTileID};
use mln_util::mat4::{Mat4, matrix};

use crate::bucket::BucketPtr;
use crate::buckets::debug_bucket::DebugBucket; // layers: debug
use crate::render_layer::LayerRenderData;
use crate::render_source::SourcePrepareParameters;
use crate::tile::tile::Tile;
use crate::tile_render_data::{TileAtlasTextures, TileRenderData};

/// `util::rotate(Point<float>, float angle)` (util/math.hpp).
fn rotate(a: Point<f32>, angle: f32) -> Point<f32> {
    let cos = angle.cos();
    let sin = angle.sin();
    let x = cos * a.x - sin * a.y;
    let y = sin * a.x + cos * a.y;
    Point::new(x, y)
}

/// `RenderTile`.
pub struct RenderTile {
    pub id: UnwrappedTileID,
    pub matrix: Mat4,
    pub near_clipped_matrix: Mat4,
    /// layers: debug
    pub debug_bucket: Option<Box<DebugBucket>>,

    /// `tile.id`.
    tile_id: OverscaledTileID,
    /// `tile.holdForFade()`.
    tile_hold_for_fade: bool,
    // The following members are reset at placement stage.
    render_data: Option<Box<dyn TileRenderData>>,
    needs_rendering: bool,
}

impl RenderTile {
    /// `RenderTile(UnwrappedTileID, Tile&)`.
    pub fn new(id: UnwrappedTileID, tile: &dyn Tile) -> Self {
        RenderTile {
            id,
            // C++ default-constructs `mat4` (uninitialized); prepare sets it.
            matrix: [0.0; 16],
            near_clipped_matrix: [0.0; 16],
            tile_id: tile.id(),
            tile_hold_for_fade: tile.hold_for_fade(),
            render_data: None,
            needs_rendering: false,
            debug_bucket: None, // layers: debug
        }
    }

    /// `translateVtxMatrix(id, tileMatrix, translation, anchor, state,
    /// inViewportPixelUnits)` (static).
    pub fn translate_vtx_matrix_for(
        id: &UnwrappedTileID,
        tile_matrix: &Mat4,
        translation: &[f32; 2],
        anchor: TranslateAnchorType,
        state: &TransformState,
        in_viewport_pixel_units: bool,
    ) -> Mat4 {
        if translation[0] == 0.0 && translation[1] == 0.0 {
            return *tile_matrix;
        }

        let mut vtx_matrix: Mat4 = [0.0; 16];

        let angle = if in_viewport_pixel_units {
            if anchor == TranslateAnchorType::Map { state.get_bearing() as f32 } else { 0.0f32 }
        } else if anchor == TranslateAnchorType::Viewport {
            (-state.get_bearing()) as f32
        } else {
            0.0f32
        };

        let translate = rotate(Point::new(translation[0], translation[1]), angle);

        if in_viewport_pixel_units {
            matrix::translate(&mut vtx_matrix, tile_matrix, f64::from(translate.x), f64::from(translate.y), 0.0);
        } else {
            matrix::translate(
                &mut vtx_matrix,
                tile_matrix,
                f64::from(id.pixels_to_tile_units(translate.x, state.get_zoom() as f32)),
                f64::from(id.pixels_to_tile_units(translate.y, state.get_zoom() as f32)),
                0.0,
            );
        }

        vtx_matrix
    }

    /// `translateVtxMatrix(tileMatrix, translation, anchor, state,
    /// inViewportPixelUnits)`.
    pub fn translate_vtx_matrix(
        &self,
        tile_matrix: &Mat4,
        translation: &[f32; 2],
        anchor: TranslateAnchorType,
        state: &TransformState,
        in_viewport_pixel_units: bool,
    ) -> Mat4 {
        Self::translate_vtx_matrix_for(&self.id, tile_matrix, translation, anchor, state, in_viewport_pixel_units)
    }

    pub fn translated_matrix(
        &self,
        translation: &[f32; 2],
        anchor: TranslateAnchorType,
        state: &TransformState,
    ) -> Mat4 {
        self.translate_vtx_matrix(&self.matrix, translation, anchor, state, false)
    }

    pub fn translated_clip_matrix(
        &self,
        translation: &[f32; 2],
        anchor: TranslateAnchorType,
        state: &TransformState,
    ) -> Mat4 {
        self.translate_vtx_matrix(&self.near_clipped_matrix, translation, anchor, state, false)
    }

    pub fn get_overscaled_tile_id(&self) -> &OverscaledTileID {
        &self.tile_id
    }

    pub fn hold_for_fade(&self) -> bool {
        self.tile_hold_for_fade
    }

    fn render_data(&self) -> &dyn TileRenderData {
        // C++ asserts renderData.
        self.render_data.as_deref().expect("RenderTile::prepare")
    }

    pub fn get_bucket(&self, impl_: &dyn layer::Impl) -> Option<&BucketPtr> {
        self.render_data().get_bucket(impl_)
    }

    /// symbols: the feature index of the tile's layout result.
    pub fn get_feature_index(&self) -> Option<std::sync::Arc<crate::geometry::feature_index::FeatureIndex>> {
        self.render_data.as_ref().and_then(|data| data.get_feature_index())
    }

    /// Port-only: the render thread's copies of the tile's bucket geometry
    /// (bucket_geometry.rs).
    pub fn get_bucket_geometry(&self) -> Option<&crate::bucket_geometry::BucketGeometry> {
        self.render_data.as_deref().and_then(|data| data.get_bucket_geometry())
    }

    pub fn get_layer_render_data(&self, impl_: &dyn layer::Impl) -> Option<&LayerRenderData> {
        self.render_data().get_layer_render_data(impl_)
    }

    pub fn get_pattern(&self, pattern: &str) -> Option<ImagePosition> {
        self.render_data().get_pattern(pattern)
    }

    pub fn get_atlas_textures(&self) -> Option<&Rc<TileAtlasTextures>> {
        self.render_data.as_deref().and_then(|data| data.get_atlas_textures())
    }

    pub fn get_needs_rendering(&self) -> bool {
        self.needs_rendering
    }

    pub fn upload(&self, upload_pass: &dyn UploadPass) {
        self.render_data().upload(upload_pass);

        // layers: debug
        if let Some(debug_bucket) = &self.debug_bucket {
            debug_bucket.upload(upload_pass);
        }
    }

    /// `prepare(const SourcePrepareParameters&)`; `tile` is the tile this
    /// RenderTile was created for.
    pub fn prepare(&mut self, tile: &dyn Tile, parameters: &SourcePrepareParameters<'_>) {
        let mut render_data = tile.create_render_data();
        render_data.prepare(parameters);
        self.render_data = Some(render_data);

        self.needs_rendering = tile.base().used_by_rendered_layers.get();

        // layers: debug
        let tile_base = tile.base();
        let needs_debug_bucket = match &self.debug_bucket {
            None => true,
            Some(debug_bucket) => {
                debug_bucket.renderable != tile.is_renderable()
                    || debug_bucket.complete != tile.is_complete()
                    || debug_bucket.modified != tile_base.modified.get()
                    || debug_bucket.expires != tile_base.expires.get()
                    || debug_bucket.debug_mode != parameters.debug_options
            }
        };
        if parameters.debug_options != MapDebugOptions::NO_DEBUG && needs_debug_bucket {
            self.debug_bucket = Some(Box::new(DebugBucket::new(
                &tile.id(),
                tile.is_renderable(),
                tile.is_complete(),
                tile_base.modified.get(),
                tile_base.expires.get(),
                parameters.debug_options,
                parameters.source_name.clone(),
            )));
        } else if parameters.debug_options == MapDebugOptions::NO_DEBUG {
            self.debug_bucket = None;
        }

        // Calculate two matrices for this tile: matrix is the standard tile
        // matrix; nearClippedMatrix has near plane moved further, to enhance
        // depth buffer precision
        let transform = parameters.transform;
        transform.state.matrix_for(&mut self.matrix, &self.id);
        transform.state.matrix_for(&mut self.near_clipped_matrix, &self.id);
        let m = self.matrix;
        matrix::multiply(&mut self.matrix, &transform.proj_matrix, &m);
        let m = self.near_clipped_matrix;
        matrix::multiply(&mut self.near_clipped_matrix, &transform.near_clipped_proj_matrix, &m);
    }

    // TODO(port): setFeatureState (feature state, M6).
}
