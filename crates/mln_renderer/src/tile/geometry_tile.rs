// Ports src/mln/tile/geometry_tile.hpp and src/mln/tile/geometry_tile.cpp.
//
// C++ `GeometryTile` is both the tile the TilePyramid owns and the target of
// its worker's replies (an actor reference to `this`). Here the state the
// replies change is `GeometryTileInner`, an `Rc` registered as the target of
// the tile's mailbox on the render thread; `GeometryTile` owns it, the
// mailbox and the worker actor. Correlation between GeometryTile and
// GeometryTileWorker works as in C++ (see the C++ file's comment):
// `correlationID` for the tile's pending state, the worker's
// `imageCorrelationID` for image replies.
//
// The tile reaches its worker through the `TileWorkerTransport` of its
// TileParameters (tile_worker_transport.rs): it sends `WorkerMessage`s to
// the transport's endpoint, and the worker's `TileMessage`s come back
// through the tile's mailbox (`handle_worker_message`). As in C++, the
// worker runs synchronously on the render thread when the source asks for
// it (`TileParameters::isUpdateSynchronous`, e.g. a GeoJSON source with
// `synchronousUpdate`); its messages to the tile are then direct calls.
//
// symbols: the tile is the GlyphRequestor and ImageRequestor of its worker
// (getGlyphs/getImages through the managers, onGlyphsAvailable/
// onImagesAvailable forwarded to the worker), and LayoutResult carries the
// glyph/image atlases of the shared DynamicTextureAtlas.
//
// TODO(port): image patches in GeometryTileRenderData (same-size runtime
// image updates), queries, feature state, HarfBuzz shaping of font-face
// glyphs in onGlyphsAvailable.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use mln_actor::{ActorRef, LocalObject, Mailbox, scheduler};
use mln_gfx::UploadPass;
use mln_map_types::{MapDebugOptions, MapMode};
use mln_storage::FileSource;
use mln_style::expression::GlobalStateMap;
use mln_style::layer;
use mln_style::layer::layer_type_info;
use mln_style::layer_properties::LayerProperties;
use mln_style_types::VisibilityType;
use mln_style_types::image::{ImageMap, ImagePosition};
use mln_text::BiDi;
use mln_text::glyph::{GlyphDependencies, GlyphMap, HBShapeRequests};
use mln_tile_id::OverscaledTileID;
use mln_util::exception::ExceptionPtr;
use mln_util::immutable::Immutable;
use mln_vec_map::VecSet;
use serde::{Deserialize, Serialize};

use crate::bucket::BucketPtr;
use crate::bucket_geometry::BucketGeometry;
use crate::geometry::feature_index::FeatureIndex;
use crate::gfx::dynamic_texture_atlas::{GlyphAtlas, ImageAtlas, ImageVersionMap, TextureAtlas};
use crate::image_manager::{ImageManager, ImageRequestPair, ImageRequestor, ImageRequestorBase, requestor_key};
use crate::render_layer::LayerRenderData;
use crate::render_source::SourcePrepareParameters;
use crate::text::glyph_manager::{GlyphManager, GlyphRequestor, HBShapeResults, glyph_requestor_key};
use crate::tile::geometry_tile_worker::{SharedTileData, TileMessage, WorkerMessage};
use crate::tile::tile::{Kind, Tile, TileBase, TileOperation};
use crate::tile::tile_observer::TileObserver;
use crate::tile::tile_worker_transport::{SynchronousTile, TileEndpoint, WorkerEndpoint, WorkerParameters};
use crate::tile_parameters::TileParameters;
use crate::tile_render_data::{TileAtlasTextures, TileRenderData};

/// The atlases of a `LayoutResult`; dropping the last reference releases
/// their bins (C++ `~LayoutResult`). Shared, so that copies of a
/// LayoutResult release them once.
///
/// Port-only: serde writes the atlases without the atlas they release their
/// bins to, which is the side's that gets them (tile/worker_codec).
#[derive(Default, Serialize, Deserialize)]
pub struct LayoutAtlases {
    pub glyph_atlas: GlyphAtlas,
    pub image_atlas: ImageAtlas,
    #[serde(skip)]
    pub dynamic_texture_atlas: Option<Arc<dyn TextureAtlas>>,
}

impl Drop for LayoutAtlases {
    fn drop(&mut self) {
        if let Some(atlas) = &self.dynamic_texture_atlas {
            atlas.remove_textures(&self.glyph_atlas.texture_handles, &self.glyph_atlas.dynamic_texture);
            atlas.remove_textures(&self.image_atlas.texture_handles, &self.image_atlas.dynamic_texture);
        }
    }
}

/// `GeometryTile::LayoutResult`.
#[derive(Clone)]
pub struct LayoutResult {
    pub layer_render_data: HashMap<String, LayerRenderData>,
    pub feature_index: Arc<FeatureIndex>,
    pub atlases: Arc<LayoutAtlases>,
}

impl LayoutResult {
    pub fn new(
        layer_render_data: HashMap<String, LayerRenderData>,
        feature_index: Arc<FeatureIndex>,
        glyph_atlas: GlyphAtlas,
        image_atlas: ImageAtlas,
        dynamic_texture_atlas: Option<Arc<dyn TextureAtlas>>,
    ) -> Self {
        LayoutResult {
            layer_render_data,
            feature_index,
            atlases: Arc::new(LayoutAtlases { glyph_atlas, image_atlas, dynamic_texture_atlas }),
        }
    }

    pub fn glyph_atlas(&self) -> &GlyphAtlas {
        &self.atlases.glyph_atlas
    }

    pub fn image_atlas(&self) -> &ImageAtlas {
        &self.atlases.image_atlas
    }

    /// `getLayerRenderData(const style::Layer::Impl&)`.
    pub fn get_layer_render_data(&self, layer_impl: &dyn layer::Impl) -> Option<&LayerRenderData> {
        let result = self.layer_render_data.get(&layer_impl.id)?;
        if !std::ptr::eq(result.layer_properties.base_impl.get_type_info(), layer_impl.get_type_info()) {
            // Layer data might be outdated, see issue #12432.
            return None;
        }
        Some(result)
    }
}

/// `GeometryTileRenderData`.
struct GeometryTileRenderData {
    atlas_textures: Option<Rc<TileAtlasTextures>>,
    layout_result: Option<Rc<LayoutResult>>,
    bucket_geometry: Rc<BucketGeometry>,
    // TODO(port): std::vector<ImagePatch> imagePatches (sprite updates).
}

impl TileRenderData for GeometryTileRenderData {
    fn get_atlas_textures(&self) -> Option<&Rc<TileAtlasTextures>> {
        self.atlas_textures.as_ref()
    }

    fn get_pattern(&self, pattern: &str) -> Option<ImagePosition> {
        let layout_result = self.layout_result.as_ref()?;
        layout_result.image_atlas().pattern_positions.get(pattern).cloned()
    }

    fn get_layer_render_data(&self, layer_impl: &dyn layer::Impl) -> Option<&LayerRenderData> {
        self.layout_result.as_ref().and_then(|result| result.get_layer_render_data(layer_impl))
    }

    fn get_bucket(&self, layer_impl: &dyn layer::Impl) -> Option<&BucketPtr> {
        self.get_layer_render_data(layer_impl).map(|data| &data.bucket)
    }

    fn get_feature_index(&self) -> Option<Arc<FeatureIndex>> {
        self.layout_result.as_ref().map(|result| result.feature_index.clone())
    }

    fn get_bucket_geometry(&self) -> Option<&BucketGeometry> {
        Some(&self.bucket_geometry)
    }

    fn upload(&self, upload_pass: &dyn UploadPass) {
        let Some(layout_result) = &self.layout_result else {
            return;
        };

        for entry in layout_result.layer_render_data.values() {
            if entry.bucket.needs_upload() {
                entry.bucket.upload(upload_pass);
            }
        }

        debug_assert!(self.atlas_textures.is_some());
        let Some(atlas_textures) = &self.atlas_textures else {
            return;
        };

        let context = upload_pass.get_context();
        if let Some(glyph_dynamic_texture) = &layout_result.glyph_atlas().dynamic_texture {
            glyph_dynamic_texture.upload_deferred_images(context);
            *atlas_textures.glyph.borrow_mut() = glyph_dynamic_texture.get_texture();
        }
        if let Some(image_dynamic_texture) = &layout_result.image_atlas().dynamic_texture {
            image_dynamic_texture.upload_deferred_images(context);
            *atlas_textures.icon.borrow_mut() = image_dynamic_texture.get_texture();
        }

        // TODO(port): imagePatches (same-size runtime image updates).
    }

    fn prepare(&mut self, _parameters: &SourcePrepareParameters<'_>) {
        if self.layout_result.is_none() {
            // return
        }
        // TODO(port): populateImagePatches (sprite image updates).
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FadeState {
    Loaded,
    NeedsFirstPlacement,
    NeedsSecondPlacement,
    CanRemove,
}

/// The render-thread state of a `GeometryTile`, also the target of its
/// worker's replies.
pub struct GeometryTileInner {
    pub base: TileBase,

    // symbols: the requestor side (C++ GlyphRequestor/ImageRequestor bases).
    self_weak: Weak<GeometryTileInner>,
    image_requestor_base: ImageRequestorBase,
    file_source: Option<Arc<dyn FileSource>>,
    glyph_manager: Rc<GlyphManager>,
    image_manager: Rc<ImageManager>,

    /// Used to signal the worker that it should abandon parsing this tile as
    /// soon as possible.
    obsolete: Arc<AtomicBool>,

    /// `OptionalActor<GeometryTileWorker> worker`, behind its transport.
    worker: Box<dyn WorkerEndpoint>,
    mailbox: Arc<Mailbox>,

    correlation_id: Cell<u64>,

    layout_result: RefCell<Option<Rc<LayoutResult>>>,
    /// The render thread's copies of the layout result's bucket geometry.
    bucket_geometry: RefCell<Rc<BucketGeometry>>,
    atlas_textures: RefCell<Option<Rc<TileAtlasTextures>>>,

    mode: MapMode,

    show_collision_boxes: Cell<bool>,

    fade_state: Cell<FadeState>,
}

impl GeometryTileInner {
    fn available_images(&self) -> Immutable<VecSet<String>> {
        self.image_manager.get_available_images()
    }

    /// The `obsolete` flag.
    pub fn is_obsolete(&self) -> bool {
        self.obsolete.load(Ordering::SeqCst)
    }

    fn mark_obsolete(&self) {
        self.obsolete.store(true, Ordering::SeqCst);
        // `mailbox->abandon()`: no reply reaches the tile any more.
        self.mailbox.close();
    }

    /// `setError(std::exception_ptr)`.
    pub fn set_error(&self, err: ExceptionPtr) {
        self.base.loaded.set(true);
        self.base.observer().on_tile_error(&self.base, err);
    }

    /// `setData(std::unique_ptr<const GeometryTileData>)`.
    pub fn set_data(&self, data: Option<SharedTileData>) {
        if self.obsolete.load(Ordering::SeqCst) {
            return;
        }

        if !self.base.pending.get() {
            self.base.on_tile_action(TileOperation::StartParse);
        }

        // Mark the tile as pending again if it was complete before to
        // prevent signaling a complete state despite pending parse
        // operations.
        self.base.pending.set(true);

        self.correlation_id.set(self.correlation_id.get() + 1);
        let correlation_id = self.correlation_id.get();
        let available_images = self.available_images();
        self.worker.send(WorkerMessage::SetData { data, available_images, correlation_id });
    }

    /// `reset()`: resets the tile's data and layers and leaves the tile in
    /// pending state, waiting for the new data and layers to come.
    pub fn reset(&self) {
        // If there is pending work, indicate that work has been cancelled.
        // Clear the pending status.
        if self.base.pending.get() {
            self.base.on_tile_action(TileOperation::Cancelled);
            self.base.pending.set(false);
        }

        // Reset the tile to an unloaded state to avoid signaling completion
        // after clearing the tile's pending status.
        self.base.loaded.set(false);

        // Reset the worker to the `NeedsParse` state.
        self.correlation_id.set(self.correlation_id.get() + 1);
        let correlation_id = self.correlation_id.get();
        self.worker.send(WorkerMessage::Reset { correlation_id });
    }

    /// `onLayout(std::shared_ptr<LayoutResult>, correlationID)`.
    pub fn on_layout(&self, result: LayoutResult, result_correlation_id: u64) {
        self.base.loaded.set(true);
        self.base.renderable.set(true);
        if result_correlation_id == self.correlation_id.get() {
            self.base.pending.set(false);
            self.base.on_tile_action(TileOperation::EndParse);
        }

        // TODO(port): ErrorScope, bucket->check (symbol guards).

        *self.layout_result.borrow_mut() = Some(Rc::new(result));
        *self.bucket_geometry.borrow_mut() = Rc::default();
        if self.atlas_textures.borrow().is_none() {
            *self.atlas_textures.borrow_mut() = Some(Rc::new(TileAtlasTextures::default()));
        }

        self.base.observer().on_tile_changed(&self.base);
    }

    /// `onError(std::exception_ptr, correlationID)`.
    pub fn on_error(&self, err: ExceptionPtr, result_correlation_id: u64) {
        self.base.loaded.set(true);
        if result_correlation_id == self.correlation_id.get() {
            self.base.pending.set(false);
            self.base.on_tile_action(TileOperation::Error);
        }
        self.base.observer().on_tile_error(&self.base, err);
    }

    /// `getImages(ImageRequestPair)`: `imageManager->getImages(*this, pair)`.
    pub fn get_images(&self, pair: ImageRequestPair) {
        let Some(this) = self.self_weak.upgrade() else {
            return;
        };
        let requestor: Rc<dyn ImageRequestor> = this;
        self.image_manager.get_images(&requestor, pair);
    }

    /// `getGlyphs(GlyphDependencies)`.
    pub fn get_glyphs(&self, glyph_dependencies: GlyphDependencies) {
        if let Some(file_source) = &self.file_source {
            let Some(this) = self.self_weak.upgrade() else {
                return;
            };
            let requestor: Rc<dyn GlyphRequestor> = this;
            self.glyph_manager.get_glyphs(&requestor, glyph_dependencies, Some(file_source));
        }
    }

    /// Runs the member function a worker's message stands for.
    pub fn handle_worker_message(&self, message: TileMessage) {
        match message {
            TileMessage::Layout { result, correlation_id } => self.on_layout(result, correlation_id),
            TileMessage::Error { error, correlation_id } => self.on_error(error, correlation_id),
            TileMessage::GetGlyphs(dependencies) => self.get_glyphs(dependencies),
            TileMessage::GetImages(pair) => self.get_images(pair),
        }
    }

    /// `getFeatureIndex()`.
    pub fn get_feature_index(&self) -> Option<Arc<FeatureIndex>> {
        self.layout_result.borrow().as_ref().map(|result| result.feature_index.clone())
    }
}

impl GlyphRequestor for GeometryTileInner {
    /// `onGlyphsAvailable(GlyphMap, HBShapeRequests)`.
    fn on_glyphs_available(&self, mut glyph_map: GlyphMap, requests: HBShapeRequests) {
        let mut results = HBShapeResults::new();
        let mut bidi = BiDi::new();
        for (font_stack, font_types) in &requests {
            for (type_, strs) in font_types {
                for str_ in strs {
                    let mut shaped_glyph_ids = Vec::new();
                    let mut shaped_adjusts = Vec::new();
                    // Shapes each run in its own direction, in visual order.
                    for run in bidi.visual_runs(str_) {
                        self.glyph_manager.hb_shaping(
                            &run.text,
                            run.rtl,
                            font_stack,
                            *type_,
                            &mut shaped_glyph_ids,
                            &mut shaped_adjusts,
                        );
                    }
                    let mut shaped_str: Vec<u16> = Vec::with_capacity(shaped_glyph_ids.len());
                    for glyph_id in &shaped_glyph_ids {
                        shaped_str.push(glyph_id.code);

                        let font_stack_hash = mln_text::tagged_string::font_stack_hash(font_stack);
                        let need_shape =
                            !glyph_map.get(&font_stack_hash).is_some_and(|glyphs| glyphs.contains_key(glyph_id));
                        if need_shape {
                            let glyph = self.glyph_manager.get_glyph(font_stack, *glyph_id);
                            glyph_map.entry(font_stack_hash).or_default().entry(glyph.id).or_insert(Some(glyph));
                        }
                    }

                    results.entry(font_stack.clone()).or_default().entry(*type_).or_default().insert(
                        str_.clone(),
                        crate::text::glyph_manager::HBShapeResult {
                            str_: shaped_str,
                            adjusts: Arc::new(shaped_adjusts),
                        },
                    );
                }
            }
        }
        self.worker.send(WorkerMessage::GlyphsAvailable { glyph_map, results });
    }
}

impl ImageRequestor for GeometryTileInner {
    fn requestor_base(&self) -> &ImageRequestorBase {
        &self.image_requestor_base
    }

    /// `onImagesAvailable(images, patterns, versionMap, imageCorrelationID)`.
    fn on_images_available(
        &self,
        images: ImageMap,
        patterns: ImageMap,
        version_map: ImageVersionMap,
        image_correlation_id: u64,
    ) {
        self.worker.send(WorkerMessage::ImagesAvailable {
            icon_map: images,
            pattern_map: patterns,
            version_map,
            image_correlation_id,
        });
    }
}

/// `GeometryTile`.
pub struct GeometryTile {
    inner: Rc<GeometryTileInner>,
}

impl GeometryTile {
    /// `GeometryTile(const OverscaledTileID&, std::string sourceID, const
    /// TileParameters&, TileObserver*)`.
    pub fn new(
        id: OverscaledTileID,
        source_id: String,
        parameters: &TileParameters<'_>,
        observer: Option<Rc<dyn TileObserver>>,
    ) -> Self {
        let obsolete = Arc::new(AtomicBool::new(false));
        let mailbox = Mailbox::new();
        let show_collision_boxes = parameters.debug_options.contains(MapDebugOptions::COLLISION);
        let synchronous = parameters.is_update_synchronous;
        let worker_parameters = WorkerParameters {
            id,
            source_id: source_id.clone(),
            obsolete: obsolete.clone(),
            mode: parameters.mode,
            pixel_ratio: parameters.pixel_ratio,
            show_collision_boxes,
            dynamic_texture_atlas: Some(parameters.dynamic_texture_atlas.clone()),
            font_faces: parameters.glyph_manager.get_font_faces(),
            capture_rendered_features: parameters.capture_rendered_features,
            synchronous,
        };
        let inner = Rc::new_cyclic(|self_weak: &Weak<GeometryTileInner>| {
            // OptionalActorRef<GeometryTile>(isUpdateSynchronous, *this,
            // mailbox).
            let tile: Box<dyn TileEndpoint> = if synchronous {
                Box::new(SynchronousTile(LocalObject::new(self_weak.clone())))
            } else {
                Box::new(ActorRef::<GeometryTileInner>::new(&mailbox))
            };
            GeometryTileInner {
                base: TileBase::new(Kind::Geometry, id, source_id, observer),
                self_weak: self_weak.clone(),
                image_requestor_base: ImageRequestorBase::default(),
                file_source: parameters.file_source.clone(),
                glyph_manager: parameters.glyph_manager.clone(),
                image_manager: parameters.image_manager.clone(),
                obsolete,
                worker: parameters.tile_worker_transport.create(worker_parameters, tile),
                mailbox: mailbox.clone(),
                correlation_id: Cell::new(0),
                layout_result: RefCell::new(None),
                bucket_geometry: RefCell::default(),
                atlas_textures: RefCell::new(None),
                mode: parameters.mode,
                show_collision_boxes: Cell::new(show_collision_boxes),
                fade_state: Cell::new(FadeState::Loaded),
            }
        });
        // `mailbox(std::make_shared<Mailbox>(*Scheduler::GetCurrent()))`
        mailbox.open(&scheduler::get_current(), inner.clone() as Rc<dyn Any>);
        GeometryTile { inner }
    }

    pub fn inner(&self) -> &Rc<GeometryTileInner> {
        &self.inner
    }

    pub fn set_error(&self, err: ExceptionPtr) {
        self.inner.set_error(err);
    }

    pub fn set_data(&self, data: Option<SharedTileData>) {
        self.inner.set_data(data);
    }

    pub fn reset(&self) {
        self.inner.reset();
    }

    pub fn get_feature_index(&self) -> Option<Arc<FeatureIndex>> {
        self.inner.get_feature_index()
    }
}

impl Drop for GeometryTile {
    fn drop(&mut self) {
        self.inner.mark_obsolete();

        self.inner.glyph_manager.remove_requestor_key(glyph_requestor_key(&self.inner));
        self.inner.image_manager.remove_requestor_key(requestor_key(&self.inner));

        if self.inner.base.pending.get() {
            // This tile never finished loading or was abandoned, emit a
            // cancellation event
            self.inner.base.on_tile_action(TileOperation::Cancelled);
        }

        // C++ runs `dynamicTextureAtlas->removeUnusedDynamicTextures()` on
        // the render thread once the layout result is gone.
        let layout_result = self.inner.layout_result.borrow_mut().take();
        if let Some(layout_result) = layout_result {
            let atlas = layout_result.atlases.dynamic_texture_atlas.clone();
            drop(layout_result);
            if let Some(atlas) = atlas {
                atlas.remove_unused_dynamic_textures();
            }
        }
    }
}

impl Tile for GeometryTile {
    fn base(&self) -> &TileBase {
        &self.inner.base
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn create_render_data(&self) -> Box<dyn TileRenderData> {
        Box::new(GeometryTileRenderData {
            atlas_textures: self.inner.atlas_textures.borrow().clone(),
            layout_result: self.inner.layout_result.borrow().clone(),
            bucket_geometry: self.inner.bucket_geometry.borrow().clone(),
        })
    }

    fn cancel(&self) {
        self.inner.mark_obsolete();
    }

    fn set_layers(&self, layers: &[Immutable<dyn LayerProperties>], global_state: Option<Arc<GlobalStateMap>>) {
        let inner = &self.inner;
        // Mark the tile as pending again if it was complete before to
        // prevent signaling a complete state despite pending parse
        // operations.
        if !inner.base.pending.get() {
            inner.base.pending.set(true);
            inner.base.on_tile_action(TileOperation::StartParse);
        }

        let id = inner.base.id();
        let mut impls: Vec<Immutable<dyn LayerProperties>> = Vec::with_capacity(layers.len());

        for layer in layers {
            // Skip irrelevant layers.
            let layer_impl = &layer.base_impl;
            debug_assert!(layer_impl.get_type_info().source != layer_type_info::Source::NotRequired);
            debug_assert!(layer_impl.source == inner.base.source_id);
            debug_assert!(layer_impl.visibility != VisibilityType::None);
            let overscaled_z = f32::from(id.overscaled_z);
            if overscaled_z < layer_impl.min_zoom.floor() || overscaled_z >= layer_impl.max_zoom.ceil() {
                continue;
            }

            impls.push(layer.clone());
        }

        inner.correlation_id.set(inner.correlation_id.get() + 1);
        let correlation_id = inner.correlation_id.get();
        let available_images = inner.available_images();
        inner.worker.send(WorkerMessage::SetLayers { layers: impls, global_state, available_images, correlation_id });
    }

    fn set_show_collision_boxes(&self, show_collision_boxes: bool) {
        let inner = &self.inner;
        if inner.show_collision_boxes.get() != show_collision_boxes {
            inner.show_collision_boxes.set(show_collision_boxes);
            inner.correlation_id.set(inner.correlation_id.get() + 1);
            let correlation_id = inner.correlation_id.get();
            inner.worker.send(WorkerMessage::SetShowCollisionBoxes { show_collision_boxes, correlation_id });
        }
    }

    fn set_observer(&self, observer: Rc<dyn TileObserver>) {
        self.inner.base.set_observer(observer);
        // TODO(port): worker.self().invoke(&GeometryTileWorker::setObserver,
        // observer) (symbol errors reported from the worker thread).
    }

    // layers: feature state
    /// `setFeatureState(const LayerFeatureStates&)`.
    fn set_feature_state(&self, states: &crate::bucket::LayerFeatureStates) {
        let layout_result = self.inner.layout_result.borrow();
        let Some(layout_result) = layout_result.as_ref() else {
            return;
        };
        // getData(): latestFeatureIndex->getData()
        let Some(layers) = layout_result.feature_index.get_data() else {
            return;
        };
        if states.is_empty() {
            return;
        }

        for (layer_id, render_data) in &layout_result.layer_render_data {
            let source_layer_id = &render_data.layer_properties.base_impl.source_layer;
            // The worker got the layer from the same data.
            if let Ok(Some(source_layer)) = layers.get_layer(source_layer_id) {
                let Some(feature_states) = states.get(source_layer_id) else {
                    continue;
                };
                if feature_states.is_empty() {
                    continue;
                }
                if render_data.bucket.has_data() {
                    render_data.bucket.update(
                        feature_states,
                        &*source_layer,
                        layer_id,
                        &layout_result.image_atlas().pattern_positions,
                    );
                }
            }
        }
    }

    fn layer_properties_updated(&self, layer_properties: &Immutable<dyn LayerProperties>) -> bool {
        let mut layout_result = self.inner.layout_result.borrow_mut();
        let Some(result) = layout_result.as_mut() else {
            return false;
        };
        let layer_impl = &layer_properties.base_impl;
        let Some(render_data) = result.get_layer_render_data(&**layer_impl) else {
            return false;
        };

        if !Arc::ptr_eq(&render_data.layer_properties, layer_properties)
            && render_data.layer_properties.constants_mask() == layer_properties.constants_mask()
        {
            // C++ updates the shared LayoutResult in place; render data
            // created from it before (the previous frame's RenderTiles) see
            // the change too, but are replaced in the next prepare. The port
            // updates a copy if the result is shared.
            let result = Rc::make_mut(result);
            if let Some(render_data) = result.layer_render_data.get_mut(&layer_impl.id) {
                render_data.layer_properties = layer_properties.clone();
            }
        }

        true
    }

    fn hold_for_fade(&self) -> bool {
        self.inner.mode == MapMode::Continuous
            && matches!(self.inner.fade_state.get(), FadeState::NeedsFirstPlacement | FadeState::NeedsSecondPlacement)
    }

    fn mark_rendered_ideal(&self) {
        self.inner.fade_state.set(FadeState::Loaded);
    }

    fn mark_rendered_previously(&self) {
        if self.inner.fade_state.get() == FadeState::Loaded {
            self.inner.fade_state.set(FadeState::NeedsFirstPlacement);
        }
    }

    fn performed_fade_placement(&self) {
        match self.inner.fade_state.get() {
            FadeState::NeedsFirstPlacement => self.inner.fade_state.set(FadeState::NeedsSecondPlacement),
            FadeState::NeedsSecondPlacement => self.inner.fade_state.set(FadeState::CanRemove),
            _ => {}
        }
    }

    // query
    fn get_query_padding(&self, layers: &QueryLayers<'_>) -> f32 {
        let layout_result = self.inner.layout_result.borrow();
        let mut query_padding: f32 = 0.0;
        for layer in layers.values() {
            let Some(data) =
                layout_result.as_ref().and_then(|result| result.get_layer_render_data(&*layer.base().base_impl))
            else {
                continue;
            };
            if data.bucket.has_data() {
                // std::max(queryPadding, radius)
                let radius = data.bucket.get_query_radius(*layer);
                if query_padding < radius {
                    query_padding = radius;
                }
            }
        }
        query_padding
    }

    fn query_rendered_features(
        &self,
        result: &mut QueryResults,
        query_geometry: &mln_tile_id::geometry_tile_data::GeometryCoordinates,
        transform_state: &mln_map_types::TransformState,
        layers: &QueryLayers<'_>,
        options: &RenderedQueryOptions,
        global_state: Option<&GlobalStateMap>,
        proj_matrix: &Mat4,
        feature_state: &SourceFeatureState,
    ) -> Result<(), mln_util::exception::ExceptionPtr> {
        let layout_result = self.inner.layout_result.borrow().clone();
        // `if (!getData()) return;`
        let Some(layout_result) = layout_result else {
            return Ok(());
        };
        if layout_result.feature_index.get_data().is_none() {
            return Ok(());
        }

        let query_padding = self.get_query_padding(layers);

        let id = self.inner.base.id();
        let mut pos_matrix: Mat4 = [0.0; 16];
        transform_state.matrix_for(&mut pos_matrix, &id.to_unwrapped());
        let tile_matrix = pos_matrix;
        mln_util::mat4::matrix::multiply(&mut pos_matrix, proj_matrix, &tile_matrix);

        layout_result.feature_index.query(
            result,
            query_geometry,
            transform_state,
            &pos_matrix,
            mln_util::constants::TILE_SIZE_D * f64::from(id.overscale_factor()),
            2f64.powf(transform_state.get_zoom() - f64::from(id.overscaled_z)),
            options,
            global_state,
            &id,
            layers,
            query_padding * transform_state.max_pitch_scale_factor(),
            feature_state,
        )
    }

    fn query_source_features(
        &self,
        result: &mut Vec<Feature>,
        options: &SourceQueryOptions,
        global_state: Option<&GlobalStateMap>,
    ) -> Result<(), mln_util::exception::ExceptionPtr> {
        let layout_result = self.inner.layout_result.borrow().clone();
        // Data not yet available, or tile is empty
        let Some(layout_result) = layout_result else {
            return Ok(());
        };
        let Some(data) = layout_result.feature_index.get_data() else {
            return Ok(());
        };

        // No source layers, specified, nothing to do
        let Some(source_layers) = &options.source_layers else {
            mln_util::logging::Log::warning(mln_util::logging::Event::General, "At least one sourceLayer required");
            return Ok(());
        };

        let id = self.inner.base.id();
        for source_layer in source_layers {
            // Go through all sourceLayers, if any
            // to gather all the features
            if let Some(layer) = data.get_layer(source_layer)? {
                query_layer_source_features(&*layer, result, options, global_state, &id)?;
            }
        }
        Ok(())
    }
}

// query
use mln_util::feature::Feature;
use mln_util::mat4::Mat4;

use crate::geometry::feature_index::{QueryLayers, QueryResults};
use crate::query::{RenderedQueryOptions, SourceQueryOptions};
use crate::source_state::SourceFeatureState;

/// The feature loop of `GeometryTile::querySourceFeatures` and
/// `GeoJSONTile::querySourceFeatures`.
pub fn query_layer_source_features(
    layer: &dyn mln_tile_id::geometry_tile_data::GeometryTileLayer,
    result: &mut Vec<Feature>,
    options: &SourceQueryOptions,
    global_state: Option<&GlobalStateMap>,
    id: &OverscaledTileID,
) -> Result<(), ExceptionPtr> {
    let feature_count = layer.feature_count();
    for i in 0..feature_count {
        let feature = layer.get_feature(i)?;

        // Apply filter, if any
        if let Some(filter) = &options.filter {
            let context = mln_style::expression::EvaluationContext::with_zoom_feature(
                f32::from(id.overscaled_z),
                Some(&*feature),
            )
            .with_global_state(global_state);
            let passes = filter.evaluate(&context);
            feature.check()?;
            if !passes {
                continue;
            }
        }

        // C++ throws out of the query for a malformed geometry (from
        // `convertFeature`) or value.
        let converted = mln_tile::geometry_tile_data::convert_feature(&*feature, &id.canonical)
            .map_err(|e| Arc::new(e) as ExceptionPtr)?;
        feature.check()?;
        result.push(Feature::from(converted));
    }
    Ok(())
}

impl GeometryTile {
    /// `getData()`: the tile data of the latest layout, if any.
    pub fn has_data(&self) -> bool {
        self.inner.layout_result.borrow().as_ref().is_some_and(|result| result.feature_index.get_data().is_some())
    }
}
