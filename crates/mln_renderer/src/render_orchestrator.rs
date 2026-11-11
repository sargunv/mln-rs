// Ports src/mln/renderer/render_orchestrator.hpp and
// src/mln/renderer/render_orchestrator.cpp: the style diffing of layer
// impls into render layers, layer evaluation, the render tree, and the layer
// group bookkeeping of the drawable path.
//
// The orchestrator is the RenderSourceObserver of its render sources; their
// tiles call it from asynchronous replies while the orchestrator is owned by
// the renderer, so that half is `OrchestratorSourceObserver`, an `Rc` that
// shares the orchestrator's observer.
//
// Not yet ported (TODO(port) at each use): annotations, glyph eviction,
// image-manager memory trimming, DynamicFeatureIndex (location indicator),
// shape-annotation and feature-extension queries, debug layer groups.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::Arc;

use mln_actor::{TaggedScheduler, scheduler};
use mln_gfx::{Context, LayerGroupBase, LayerGroupBasePtr, ShaderRegistry};
use mln_map_types::{MapMode, TransformState};
use mln_style::collection::ImmutableVector;
use mln_style::expression::{Dependency, GlobalStateMap};
use mln_style::layer::layer_type_info;
use mln_style::layer_properties::LayerProperties;
use mln_style::map::zoom_history::ZoomHistory;
use mln_style::{PropertyEvaluationParameters, TransitionOptions, TransitionParameters, image, layer, source};
use mln_text::GlyphCache;
use mln_tile_id::OverscaledTileID;
use mln_util::chrono::{Duration, TimePoint};
use mln_util::constants::DEFAULT_TRANSITION_DURATION;
use mln_util::exception::{ExceptionPtr, RuntimeError};
use mln_util::identity::SimpleIdentity;
use mln_util::immutable::Immutable;
use mln_util::logging::{Event, Log};
use mln_vec_map::{VecMap, VecSet};

use crate::change_request::{ChangeRequest, UniqueChangeRequestVec};
use crate::gfx::dynamic_texture_atlas::{DynamicTextureAtlas, DynamicTextureAtlasPtr};
use crate::image_manager::ImageManager;
use crate::layer_tweaker::LayerTweaker;
use crate::layermanager;
use crate::line_atlas::LineAtlas;
use crate::paint_parameters::PaintParameters;
use crate::pattern_atlas::PatternAtlas;
use crate::render_layer::{LayerPrepareParameters, RenderLayerPtr};
use crate::render_source::{self, RenderSource, RenderSourceObserver, SourcePrepareParameters};
use crate::render_tree::{LayerRenderItem, RenderItem, RenderTree, RenderTreeParameters};
use crate::renderer_observer::RendererObserver;
use crate::style_diff::{diff_images, diff_layers, diff_sources, has_layout_difference};
use crate::text::glyph_manager::GlyphManager;
use crate::tile::tile::TileOperation;
use crate::tile::tile_worker_transport::{InProcessTransport, TileWorkerTransport};
use crate::tile_parameters::TileParameters;
use crate::update_parameters::UpdateParameters;

/// The keys whose values differ between two global state snapshots. Values
/// of unchanged keys are copied verbatim between snapshots, so exact
/// equality is sufficient here.
fn diff_global_state_keys(previous: &GlobalStateMap, current: &GlobalStateMap) -> VecSet<String> {
    let mut keys = VecSet::new();
    for (key, value) in current.iter() {
        match previous.get(key) {
            Some(prev) if prev == value => {}
            _ => {
                keys.insert(key.clone());
            }
        }
    }
    for key in previous.keys() {
        if !current.contains_key(key) {
            keys.insert(key.clone());
        }
    }
    keys
}

/// The orchestrator's `observer` member, shared with its
/// RenderSourceObserver half.
type SharedObserver = Rc<RefCell<Option<Weak<dyn RendererObserver>>>>;

fn shared_observer(observer: &SharedObserver) -> Option<Rc<dyn RendererObserver>> {
    observer.borrow().as_ref().and_then(Weak::upgrade)
}

/// The ImageManagerObserver half of `RenderOrchestrator`
/// (`onStyleImageMissing`, `onRemoveUnusedStyleImages` forward to the
/// renderer observer, which re-renders after `done`, as Map::Impl does).
struct OrchestratorImageObserver {
    observer: SharedObserver,
}

impl crate::image_manager::ImageManagerObserver for OrchestratorImageObserver {
    fn on_style_image_missing(&self, id: &str, done: Box<dyn FnOnce()>) {
        let mut done = Some(done);
        let mut call_done = || {
            if let Some(done) = done.take() {
                done();
            }
        };
        match shared_observer(&self.observer) {
            Some(observer) => observer.on_style_image_missing(id, &mut call_done),
            None => call_done(),
        }
    }

    fn on_remove_unused_style_images(&self, ids: &[String]) {
        if let Some(observer) = shared_observer(&self.observer) {
            observer.on_remove_unused_style_images(ids);
        }
    }
}

/// symbols: the GlyphManagerObserver half of `RenderOrchestrator`.
struct OrchestratorGlyphObserver {
    observer: SharedObserver,
}

impl crate::text::glyph_manager::GlyphManagerObserver for OrchestratorGlyphObserver {
    /// `onGlyphsError(fontStack, glyphRange, error)`.
    fn on_glyphs_error(
        &self,
        font_stack: &mln_util::font_stack::FontStack,
        glyph_range: &mln_text::glyph::GlyphRange,
        error: mln_util::exception::ExceptionPtr,
    ) {
        let range = if glyph_range.type_ == mln_text::glyph::GlyphIDType::FontPBF {
            format!("{}-{}", glyph_range.first, glyph_range.second)
        } else {
            format!("{}(font file)", glyph_range.type_.0)
        };
        Log::error(
            Event::Style,
            format!(
                "Failed to load glyph range {range} for font stack {}:( {error})",
                mln_util::font_stack::font_stack_to_string(font_stack)
            ),
        );
        if let Some(observer) = shared_observer(&self.observer) {
            observer.on_resource_error(error);
        }
    }
}

/// The RenderSourceObserver half of `RenderOrchestrator`.
struct OrchestratorSourceObserver {
    observer: SharedObserver,
}

impl RenderSourceObserver for OrchestratorSourceObserver {
    /// `onTileChanged(RenderSource&, const OverscaledTileID&)`.
    fn on_tile_changed(&self, _source_id: &str, _tile_id: &OverscaledTileID) {
        if let Some(observer) = shared_observer(&self.observer) {
            observer.on_invalidate();
        }
    }

    /// `onTileError(RenderSource&, const OverscaledTileID&, std::exception_ptr)`.
    fn on_tile_error(&self, source_id: &str, tile_id: &OverscaledTileID, error: ExceptionPtr) {
        Log::error(Event::Style, format!("Failed to load tile {tile_id} for source {source_id}: {error}"));
        if let Some(observer) = shared_observer(&self.observer) {
            observer.on_resource_error(error);
        }
    }

    /// `onTileAction(RenderSource&, TileOperation, const OverscaledTileID&,
    /// const std::string&)`.
    fn on_tile_action(&self, _source_id: &str, _op: TileOperation, _tile_id: &OverscaledTileID, _tile_source_id: &str) {
        // TODO(port): observer->onTileAction(op, id, sourceID)
        // (RendererObserver tile callbacks).
    }
}

fn ptr_eq_option<T: ?Sized>(a: &Option<Arc<T>>, b: &Option<Arc<T>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        _ => false,
    }
}

pub struct RenderOrchestrator {
    observer: SharedObserver,
    source_observer: Rc<OrchestratorSourceObserver>,

    zoom_history: ZoomHistory,
    transform_state: TransformState,

    // symbols: the glyph and image managers (shared with the tiles), and the
    // dynamic texture atlas (C++ Renderer::Impl owns it and passes it to
    // createRenderTree; see `reset_dynamic_texture_atlas`).
    glyph_manager: Rc<GlyphManager>,
    image_manager: Rc<ImageManager>,
    dynamic_texture_atlas: DynamicTextureAtlasPtr,
    line_atlas: Rc<RefCell<LineAtlas>>,
    pattern_atlas: Rc<RefCell<PatternAtlas>>,

    image_impls: Immutable<Vec<Immutable<image::Impl>>>,
    source_impls: ImmutableVector<dyn source::Impl>,
    layer_impls: ImmutableVector<dyn layer::Impl>,

    render_sources: HashMap<String, Box<dyn RenderSource>>,
    render_layers: HashMap<String, RenderLayerPtr>,
    // conformance: renderLight.
    render_light: crate::render_light::RenderLight,

    // symbols:
    cross_tile_symbol_index: crate::text::cross_tile_symbol_index::CrossTileSymbolIndex,
    placement_controller: crate::text::placement::PlacementController,
    // TODO(port):
    // layersNeedPlacement, filteredLayersForSource.
    background_layer_as_color: bool,
    context_lost: bool,
    placed_symbol_data_collected: bool,
    tile_cache_enabled: bool,

    /// Vector with reference wrappers to the layers in style order.
    ordered_layers: Vec<RenderLayerPtr>,
    filtered_layers_for_source: Vec<Immutable<dyn LayerProperties>>,

    thread_pool: TaggedScheduler,
    /// Port-only: creates the tiles' workers (tile_worker_transport.rs).
    tile_worker_transport: Rc<dyn TileWorkerTransport>,

    global_state: Option<Arc<GlobalStateMap>>,

    /// `std::multimap<int32_t, LayerGroupBasePtr>`: ordered by layer index,
    /// insertion order within an index.
    layer_groups_by_layer_index: VecMap<i32, Vec<LayerGroupBasePtr>>,

    /// The `layerTweakers` (weak pointers) each C++ LayerGroupBase holds,
    /// keyed by the layer group's id (see change_request.rs).
    layer_group_tweakers: HashMap<SimpleIdentity, Vec<Weak<RefCell<dyn LayerTweaker>>>>,

    /// `RenderItem::DebugLayerGroupMap debugLayerGroups` (layers: debug).
    debug_layer_groups: crate::render_tree::DebugLayerGroupMap,

    // dem-heatmap: `std::vector<RenderTargetPtr> renderTargets`.
    render_targets: Vec<crate::render_target::RenderTargetPtr>,

    pending_changes: UniqueChangeRequestVec,
}

impl RenderOrchestrator {
    /// `RenderOrchestrator(bool backgroundLayerAsColor, TaggedScheduler&,
    /// const std::optional<std::string>& localFontFamily,
    /// std::shared_ptr<GlyphCache>)`.
    // TODO(port): local font family (glyph manager). C++ gets the map's
    // thread pool; the port uses the background pool with its own tag.
    pub fn new(background_layer_as_color: bool, glyph_cache: Arc<GlyphCache>) -> Self {
        let observer: SharedObserver = Rc::new(RefCell::new(None));
        // `imageManager->setObserver(this)`.
        let image_manager = ImageManager::create();
        image_manager.set_observer(Some(Rc::new(OrchestratorImageObserver { observer: observer.clone() })));
        // symbols: `glyphManager->setObserver(this)`.
        let glyph_manager = GlyphManager::with_glyph_cache(None, glyph_cache);
        glyph_manager.set_observer(Some(Rc::new(OrchestratorGlyphObserver { observer: observer.clone() })));
        let thread_pool = TaggedScheduler::new(scheduler::get_background(), SimpleIdentity::new());
        RenderOrchestrator {
            observer: observer.clone(),
            source_observer: Rc::new(OrchestratorSourceObserver { observer }),
            zoom_history: ZoomHistory::default(),
            transform_state: TransformState::default(),
            glyph_manager,
            image_manager,
            cross_tile_symbol_index: Default::default(),
            placement_controller: crate::text::placement::PlacementController::new(),
            dynamic_texture_atlas: Arc::new(DynamicTextureAtlas::new()),
            line_atlas: Rc::new(RefCell::new(LineAtlas::new())),
            pattern_atlas: Rc::new(RefCell::new(PatternAtlas::new())),
            image_impls: Arc::new(Vec::new()),
            source_impls: Arc::new(Vec::new()),
            layer_impls: Arc::new(Vec::new()),
            render_sources: HashMap::new(),
            render_layers: HashMap::new(),
            // conformance: renderLight(makeMutable<Light::Impl>())
            render_light: crate::render_light::RenderLight::new(Arc::new(mln_style::light::Impl::default())),
            background_layer_as_color,
            context_lost: false,
            placed_symbol_data_collected: false,
            tile_cache_enabled: true,
            ordered_layers: Vec::new(),
            filtered_layers_for_source: Vec::new(),
            tile_worker_transport: Rc::new(InProcessTransport::new(thread_pool.clone())),
            thread_pool,
            global_state: None,
            layer_groups_by_layer_index: VecMap::new(),
            layer_group_tweakers: HashMap::new(),
            debug_layer_groups: VecMap::new(), // layers: debug
            render_targets: Vec::new(),        // dem-heatmap
            pending_changes: Vec::new(),
        }
    }

    pub fn mark_context_lost(&mut self) {
        self.context_lost = true;
    }

    /// Port-only: the transport that creates the workers of the tiles
    /// created from now on (by default `InProcessTransport`, on the thread
    /// pool).
    pub fn set_tile_worker_transport(&mut self, transport: Rc<dyn TileWorkerTransport>) {
        self.tile_worker_transport = transport;
    }

    pub fn set_observer(&mut self, observer: Option<Weak<dyn RendererObserver>>) {
        *self.observer.borrow_mut() = observer;
    }

    fn observer(&self) -> Option<Rc<dyn RendererObserver>> {
        shared_observer(&self.observer)
    }

    /// `getRenderSource(id)`.
    pub fn get_render_source(&self, id: &str) -> Option<&dyn RenderSource> {
        self.render_sources.get(id).map(|source| source.as_ref())
    }

    pub fn get_zoom_history(&self) -> &ZoomHistory {
        &self.zoom_history
    }

    /// `RenderTree::getLineAtlas()` (the orchestrator's atlas).
    pub fn get_line_atlas(&self) -> &Rc<RefCell<LineAtlas>> {
        &self.line_atlas
    }

    pub fn get_pattern_atlas(&self) -> &Rc<RefCell<PatternAtlas>> {
        &self.pattern_atlas
    }

    pub fn create_render_tree(&mut self, update_parameters: &Arc<UpdateParameters>) -> Option<RenderTree> {
        let start_time = mln_util::time::Instant::now();

        let is_map_mode_continuous = update_parameters.mode == MapMode::Continuous;
        if !is_map_mode_continuous {
            // Reset zoom history state.
            self.zoom_history.first = true;
        }

        // TODO(port): annotationManager->updateData() (annotations).

        let zoom_changed = self
            .zoom_history
            .update(update_parameters.transform_state.get_zoom() as f32, &update_parameters.time_point);

        let transition_options =
            if is_map_mode_continuous { update_parameters.transition_options } else { TransitionOptions::default() };

        let transition_parameters =
            TransitionParameters { now: update_parameters.time_point, transition: transition_options };
        let transition_duration = transition_options.duration.unwrap_or(if is_map_mode_continuous {
            DEFAULT_TRANSITION_DURATION
        } else {
            Duration::zero()
        });

        let global_state_changed = !ptr_eq_option(&self.global_state, &update_parameters.global_state);
        let mut changed_global_state_keys: Option<Arc<VecSet<String>>> = None;
        if global_state_changed {
            if let (Some(previous), Some(current)) = (&self.global_state, &update_parameters.global_state) {
                changed_global_state_keys = Some(Arc::new(diff_global_state_keys(previous, current)));
            }
        }
        self.global_state = update_parameters.global_state.clone();

        let mut evaluation_parameters = PropertyEvaluationParameters::with_zoom_history(
            self.zoom_history,
            update_parameters.time_point,
            transition_duration,
        );
        evaluation_parameters.zoom_changed = zoom_changed;
        evaluation_parameters.global_state = self.global_state.clone();
        evaluation_parameters.global_state_changed = global_state_changed;
        evaluation_parameters.changed_global_state_keys = changed_global_state_keys.clone();

        // TODO(port): annotationManager in TileParameters.
        let mut tile_parameters = TileParameters {
            pixel_ratio: update_parameters.pixel_ratio,
            debug_options: update_parameters.debug_options,
            transform_state: &update_parameters.transform_state,
            file_source: update_parameters.file_source.clone(),
            mode: update_parameters.mode,
            prefetch_zoom_delta: update_parameters.prefetch_zoom_delta,
            thread_pool: self.thread_pool.clone(),
            tile_lod_min_radius: update_parameters.tile_lod_min_radius,
            tile_lod_scale: update_parameters.tile_lod_scale,
            tile_lod_pitch_threshold: update_parameters.tile_lod_pitch_threshold,
            tile_lod_zoom_shift: update_parameters.tile_lod_zoom_shift,
            tile_lod_mode: update_parameters.tile_lod_mode,
            global_state: self.global_state.clone(),
            is_update_synchronous: false,
            capture_rendered_features: update_parameters.capture_rendered_features,
            image_manager: self.image_manager.clone(),
            glyph_manager: self.glyph_manager.clone(),
            dynamic_texture_atlas: self.dynamic_texture_atlas.clone(),
            tile_worker_transport: self.tile_worker_transport.clone(),
        };

        self.glyph_manager.set_url(&update_parameters.glyph_url);
        self.glyph_manager.set_font_faces(update_parameters.font_faces.clone());

        // Update light. (conformance)
        let light_changed = !Arc::ptr_eq(&self.render_light.impl_, &update_parameters.light);

        if light_changed {
            self.render_light.impl_ = update_parameters.light.clone();
            self.render_light.transition(&transition_parameters);
        }

        if light_changed || zoom_changed || global_state_changed || self.render_light.has_transition() {
            self.render_light.evaluate(&evaluation_parameters);
        }

        let image_diff = diff_images(&self.image_impls, &update_parameters.images);
        self.image_impls = update_parameters.images.clone();

        // Only trigger tile reparse for changed images. Changed images only
        // need a relayout when they have a different size.
        let mut _has_image_diff = !image_diff.removed.is_empty();

        // Remove removed images from sprite atlas.
        for id in image_diff.removed.keys() {
            self.image_manager.remove_image(id);
            // TODO(port): patternAtlas->removePattern(id) (background-pattern).
        }

        // Add added images to sprite atlas.
        for image in image_diff.added.values() {
            self.image_manager.add_image(image.clone());
        }

        // Update changed images.
        for change in image_diff.changed.values() {
            if self.image_manager.update_image(change.after.clone()) {
                // TODO(port): patternAtlas->removePattern(id) (background-pattern).
                _has_image_diff = true;
            }
        }

        self.image_manager.notify_if_missing_image_added();
        self.image_manager.set_loaded(update_parameters.sprite_loaded);

        let layer_diff = diff_layers(&self.layer_impls, &update_parameters.layers);
        self.layer_impls = update_parameters.layers.clone();
        let layers_added_or_removed = !layer_diff.added.is_empty() || !layer_diff.removed.is_empty();

        let mut changes: UniqueChangeRequestVec = Vec::new();

        // Remove render layers for removed layers.
        for id in layer_diff.removed.keys() {
            if let Some(layer) = self.render_layers.remove(id) {
                layer.borrow_mut().layer_removed(&mut changes);
            }
        }

        // Create render layers for newly added layers.
        for (id, impl_) in &layer_diff.added {
            if let Some(render_layer) = layermanager::create_render_layer(impl_) {
                render_layer.borrow_mut().transition(&transition_parameters);
                // `emplace` keeps an existing entry.
                self.render_layers.entry(id.clone()).or_insert(render_layer);
            }
        }

        // Update render layers for changed layers.
        for (id, change) in &layer_diff.changed {
            if let Some(render_layer) = self.render_layers.get(id) {
                let new_layer = &change.after;

                let mut render_layer = render_layer.borrow_mut();
                render_layer.layer_changed(&transition_parameters, new_layer, &mut changes);
                render_layer.transition_with_impl(&transition_parameters, new_layer.clone());
            }
        }

        if layers_added_or_removed {
            self.ordered_layers.clear();
            self.ordered_layers.reserve(self.layer_impls.len());
            let mut layer_index: i32 = 0;
            for layer_impl in self.layer_impls.iter() {
                // TODO(port): C++ uses renderLayers.at(id); a layer type
                // without a render layer port is skipped.
                let Some(layer) = self.render_layers.get(&layer_impl.id) else {
                    continue;
                };
                self.ordered_layers.push(layer.clone());

                // We're mutating the list of ordered layers and must notify
                // them of their new assigned indices
                layer.borrow_mut().layer_index_changed(layer_index, &mut changes);
                layer_index += 1;
            }
        }
        debug_assert_eq!(self.ordered_layers.len(), self.render_layers.len());

        // TODO(port): glyphManager->evict(fontStacks(*layerImpls)).

        // Update layers for class and zoom changes.
        let mut constants_mask_changed: VecSet<String> = VecSet::new();
        for layer in &self.ordered_layers {
            let mut layer = layer.borrow_mut();
            let id = layer.get_id().to_owned();
            let layer_added_or_changed = layer_diff.added.contains_key(&id) || layer_diff.changed.contains_key(&id);
            evaluation_parameters.layer_changed = layer_added_or_changed;
            evaluation_parameters.has_crossfade = layer.has_crossfade();

            // Only re-evaluate on change of zoom if the style has some
            // reference to it
            let zoom_changed_and_matters =
                zoom_changed && !layer_added_or_changed && layer.get_style_dependencies().intersects(Dependency::ZOOM);

            // Similarly, only re-evaluate on a global state change if the
            // layer references the global state.
            let global_state_changed_and_matters = global_state_changed
                && !layer_added_or_changed
                && layer.get_style_dependencies().intersects(Dependency::GLOBAL_STATE);

            if layer_added_or_changed
                || zoom_changed_and_matters
                || global_state_changed_and_matters
                || evaluation_parameters.has_crossfade
                || layer.has_transition()
            {
                let previous_mask = layer.base().evaluated_properties.constants_mask();
                layer.evaluate(&evaluation_parameters);
                if previous_mask != layer.base().evaluated_properties.constants_mask() {
                    constants_mask_changed.insert(id);
                }
            }
        }

        let source_diff = diff_sources(&self.source_impls, &update_parameters.sources);
        self.source_impls = update_parameters.sources.clone();

        // Remove render layers for removed sources.
        for id in source_diff.removed.keys() {
            self.render_sources.remove(id);
        }

        // Create render sources for newly added sources.
        for (id, impl_) in &source_diff.added {
            if let Some(mut render_source) = render_source::create(impl_, &self.thread_pool) {
                render_source.set_observer(self.source_observer.clone());
                render_source.set_cache_enabled(self.tile_cache_enabled);
                render_source.set_fast_pfor_enabled(update_parameters.fast_pfor_enabled);
                // `emplace` keeps an existing entry.
                self.render_sources.entry(id.clone()).or_insert(render_source);
            }
        }
        self.transform_state = update_parameters.transform_state.clone();
        let _tilted_view = self.transform_state.get_pitch() != 0.0;

        // Create parameters for the render tree.
        let mut render_tree_parameters = Box::new(RenderTreeParameters::new(
            &update_parameters.transform_state,
            update_parameters.mode,
            update_parameters.debug_options,
            update_parameters.time_point,
        ));
        // conformance: the RenderTreeParameters ctor's `renderLight.getEvaluated()`.
        render_tree_parameters.light = self.render_light.get_evaluated().clone();

        let mut layer_render_items: VecMap<u32, LayerRenderItem> = VecMap::new();

        // Reserve size for filteredLayersForSource if there are sources.
        if !self.source_impls.is_empty() {
            self.filtered_layers_for_source.reserve(self.layer_impls.len());
        }

        // Track which layers are flagged for rendering
        let mut update_list: Vec<bool> = vec![false; self.ordered_layers.len()];

        // Update all sources and initialize renderItems.
        //
        // DEVIATION: C++ always has at least one source, the annotation
        // source that AnnotationManager::onStyleLoaded adds to every style,
        // and handles the layers without a source (background) while it
        // iterates over the first source. Annotations are not ported yet, so
        // with no style sources the loop runs once without a source.
        let source_count = std::cmp::max(self.source_impls.len(), 1);
        for source_index in 0..source_count {
            let source_impl = self.source_impls.get(source_index).cloned();
            let mut source_needs_rendering = false;
            let mut source_needs_relayout = false;
            // C++ has a render source for every source type; the port skips
            // the layers of a source type without one (TODO(port)).
            let has_render_source = source_impl
                .as_ref()
                .is_some_and(|source_impl| self.render_sources.contains_key(&source_impl.base().id));

            for (index, layer_ptr) in self.ordered_layers.iter().enumerate() {
                let layer = layer_ptr.borrow();
                let base_impl = layer.base().base_impl.clone();
                let layer_info = base_impl.get_type_info();
                let layer_is_visible = base_impl.visibility != mln_style_types::VisibilityType::None;
                let zoom_fits_layer = layer.supports_zoom(self.zoom_history.last_zoom);
                render_tree_parameters.has_3d |= layer_info.pass3d == layer_type_info::Pass3D::Required;

                if layer_info.source != layer_type_info::Source::NotRequired {
                    if let Some(source_impl) = &source_impl {
                        if has_render_source && base_impl.source == source_impl.base().id {
                            let layer_id = layer.get_id().to_owned();
                            // A global state change requires a relayout of
                            // this layer's source if one of the changed state
                            // properties is referenced by anything that is
                            // baked into the tile's buckets.
                            let global_state_requires_relayout = {
                                if !global_state_changed
                                    || !(base_impl.get_layout_dependencies()
                                        | layer.base().evaluated_properties.get_evaluated_dependencies())
                                    .intersects(Dependency::GLOBAL_STATE)
                                {
                                    false
                                } else if changed_global_state_keys.is_none() {
                                    true
                                } else {
                                    let mut refs = VecSet::new();
                                    base_impl.collect_layout_global_state_refs(&mut refs);
                                    layer.base().evaluated_properties.collect_evaluated_global_state_refs(&mut refs);
                                    mln_style::expression::global_state_refs_intersect(
                                        Some(&refs),
                                        changed_global_state_keys.as_deref(),
                                    )
                                }
                            };
                            source_needs_relayout = source_needs_relayout
                                || _has_image_diff
                                || constants_mask_changed.contains(&layer_id)
                                || has_layout_difference(&layer_diff, &layer_id)
                                || global_state_requires_relayout;
                            if layer_is_visible {
                                self.filtered_layers_for_source.push(layer.base().evaluated_properties.clone());
                                if zoom_fits_layer {
                                    source_needs_rendering = true;
                                    let source_id = source_impl.base().id.clone();
                                    layer_render_items.entry(index as u32).or_insert_with(|| {
                                        LayerRenderItem::new(layer_ptr.clone(), Some(source_id), index as u32)
                                    });
                                    update_list[index] = true;
                                }
                            }
                        }
                    }
                    continue;
                }

                // Handle layers without source.
                let is_first_source = source_index == 0;
                if layer_is_visible && zoom_fits_layer && is_first_source {
                    if self.background_layer_as_color && Arc::ptr_eq(&base_impl, &self.layer_impls[0]) {
                        let solid_background = layer.get_solid_background();
                        if let Some(solid_background) = solid_background {
                            render_tree_parameters.background_color = solid_background;
                            continue; // This layer is shown with background color,
                            // and it shall not be added to render items.
                        }
                    }
                    layer_render_items
                        .entry(index as u32)
                        .or_insert_with(|| LayerRenderItem::new(layer_ptr.clone(), None, index as u32));
                    update_list[index] = true;
                }
            }
            if let Some(source_impl) = &source_impl {
                tile_parameters.is_update_synchronous = source_impl.is_update_synchronous();
                let filtered_layers = std::mem::take(&mut self.filtered_layers_for_source);
                // C++ `renderSources.at(sourceImpl->id)`; a source type
                // without a render source port has none (TODO(port)).
                if let Some(source) = self.render_sources.get_mut(&source_impl.base().id) {
                    source.update(
                        source_impl.clone(),
                        &filtered_layers,
                        source_needs_rendering,
                        source_needs_relayout,
                        &tile_parameters,
                    );
                }
                self.filtered_layers_for_source = filtered_layers;
                self.filtered_layers_for_source.clear();
            }

            // Update all layers with their new renderability status, if it
            // changed.
            for (i, will_render) in update_list.iter().enumerate() {
                let mut layer = self.ordered_layers[i].borrow_mut();
                if layer.is_layer_renderable() != *will_render {
                    layer.mark_layer_renderable(*will_render, &mut changes);
                }
            }
            self.add_changes(&mut changes);
        }

        render_tree_parameters.loaded = update_parameters.style_loaded && self.is_loaded();
        if !is_map_mode_continuous && !render_tree_parameters.loaded {
            return None;
        }

        // Prepare. Update all matrices and generate data that we should upload
        // to the GPU.
        for (name, render_source) in self.render_sources.iter_mut() {
            if render_source.is_enabled() {
                render_source.prepare(&SourcePrepareParameters {
                    transform: &render_tree_parameters.transform_params,
                    debug_options: update_parameters.debug_options,
                    image_manager: &self.image_manager,
                    source_name: name.clone(),
                });
            }
        }

        let mut layers_need_placement: Vec<RenderLayerPtr> = Vec::new();
        let mut opaque_pass_cut_off_estimation = layer_render_items.len();
        for render_item in layer_render_items.values() {
            let mut render_layer = render_item.layer.borrow_mut();
            let source =
                render_item.source.as_ref().and_then(|id| self.render_sources.get(id)).map(|source| source.as_ref());
            render_layer.prepare(&mut LayerPrepareParameters {
                source,
                image_manager: &self.image_manager, // conformance
                pattern_atlas: &self.pattern_atlas,
                line_atlas: &self.line_atlas,
                state: &update_parameters.transform_state,
            });
            if render_layer.needs_placement() {
                layers_need_placement.push(render_item.layer.clone());
            }
            if render_tree_parameters.opaque_pass_cut_off == 0 {
                opaque_pass_cut_off_estimation -= 1;
                if render_layer.is_3d() {
                    render_tree_parameters.opaque_pass_cut_off = opaque_pass_cut_off_estimation as u32;
                }
            }
        }

        // Symbol placement. (symbols)
        use crate::text::cross_tile_symbol_index::AddLayerResult;
        use crate::text::placement::Placement;
        let mut symbol_buckets_changed = false;
        let mut symbol_buckets_added = false;
        let mut used_symbol_layers: VecSet<String> = VecSet::new();
        let longitude =
            update_parameters.transform_state.get_lat_lng(mln_util::geo::WrapMode::Unwrapped).longitude() as f32;
        for layer in layers_need_placement.iter().rev() {
            let layer = layer.borrow();
            let result = self.cross_tile_symbol_index.add_layer(&*layer, longitude);
            if is_map_mode_continuous {
                used_symbol_layers.insert(layer.get_id().to_owned());
                symbol_buckets_added = symbol_buckets_added || result.contains(AddLayerResult::BUCKETS_ADDED);
                symbol_buckets_changed = symbol_buckets_changed || result != AddLayerResult::NO_CHANGES;
            }
        }

        if is_map_mode_continuous && layers_need_placement.is_empty() {
            // Discard deferred placement and query data when no symbol buckets remain.
            render_tree_parameters.placement_changed = self.placement_controller.reset();
            self.cross_tile_symbol_index.reset();
            // Source fades still need their placement bookkeeping to advance.
            for source in self.render_sources.values_mut() {
                source.update_fading_tiles();
            }
            render_tree_parameters.symbol_fade_change = 1.0;
            render_tree_parameters.needs_repaint = self.has_transitions(update_parameters.time_point);
        } else if is_map_mode_continuous {
            let tilted_view = update_parameters.transform_state.get_pitch() != 0.0;
            let mut placement_update_period_override: Option<Duration> = None;
            if symbol_buckets_added && !tilted_view {
                // If the view is not tilted, we want *the new* symbols to
                // show up faster, however simple setting `placementChanged`
                // to `true` would initiate placement too often as new
                // buckets usually come from several rendered tiles in a row
                // within a short period of time. Instead, we squeeze
                // placement update period to coalesce buckets updates from
                // several tiles. On contrary, with the tilted view it's more
                // important to make placement rarely for performance reasons
                // and as the new symbols are normally "far away" and the
                // user is not that interested to see them ASAP.
                placement_update_period_override = Some(Duration::from_millis(30));
            }

            render_tree_parameters.placement_changed = !self.placement_controller.placement_is_recent(
                update_parameters.time_point,
                update_parameters.transform_state.get_zoom() as f32,
                placement_update_period_override,
            );
            symbol_buckets_changed |= render_tree_parameters.placement_changed;
            if render_tree_parameters.placement_changed {
                let mut placement = Placement::create(
                    update_parameters.clone(),
                    Some(self.placement_controller.get_placement().clone()),
                );
                placement.place_layers(&layers_need_placement);
                self.placement_controller.set_placement(Rc::new(placement));
                self.cross_tile_symbol_index.prune_unused_layers(&used_symbol_layers);
                for source in self.render_sources.values_mut() {
                    source.update_fading_tiles();
                }
            } else {
                self.placement_controller.set_placement_stale();
            }
            render_tree_parameters.symbol_fade_change =
                self.placement_controller.get_placement().symbol_fade_change(update_parameters.time_point);
            render_tree_parameters.needs_repaint = self.has_transitions(update_parameters.time_point);
        } else {
            render_tree_parameters.placement_changed = !layers_need_placement.is_empty();
            symbol_buckets_changed = render_tree_parameters.placement_changed;
            if render_tree_parameters.placement_changed {
                let mut placement = Placement::create(update_parameters.clone(), None);
                placement.collect_placed_symbol_data(self.placed_symbol_data_collected);
                placement.place_layers(&layers_need_placement);
                self.placement_controller.set_placement(Rc::new(placement));
            }
            self.cross_tile_symbol_index.reset();
            render_tree_parameters.symbol_fade_change = 1.0;
            render_tree_parameters.needs_repaint = false;
        }

        if !render_tree_parameters.needs_repaint && render_tree_parameters.loaded {
            // Notify observer about unused images when map is fully loaded
            // and there are no ongoing transitions.
            // TODO(port): imageManager->reduceMemoryUseIfCacheSizeExceedsLimit().
        }

        let mut source_render_items: Vec<Box<dyn RenderItem>> = Vec::new();
        for render_source in self.render_sources.values_mut() {
            if render_source.is_enabled() {
                source_render_items.push(render_source.create_render_item());
            }
        }

        let mut render_tree =
            RenderTree::new(render_tree_parameters, layer_render_items, source_render_items, start_time);
        // symbols: layersNeedPlacement, placement, updateSymbolOpacities.
        render_tree.set_placement(
            layers_need_placement,
            self.placement_controller.get_placement().clone(),
            symbol_buckets_changed,
        );
        Some(render_tree)
    }

    pub fn has_transitions(&self, _time_point: TimePoint) -> bool {
        // conformance
        if self.render_light.has_transition() {
            return true;
        }

        for layer in self.render_layers.values() {
            if layer.borrow().has_transition() {
                return true;
            }
        }

        if self.placement_controller.has_transitions(_time_point) {
            return true;
        }

        for source in self.render_sources.values() {
            if source.has_fading_tiles() {
                return true;
            }
        }

        false
    }

    pub fn is_loaded(&self) -> bool {
        // do the simple boolean check before iterating over all the tiles in
        // all the sources
        if !self.image_manager.is_loaded() {
            return false;
        }

        for source in self.render_sources.values() {
            if !source.is_loaded() {
                return false;
            }
        }

        true
    }

    pub fn clear_data(&mut self) {
        if !self.source_impls.is_empty() {
            self.source_impls = Arc::new(Vec::new());
        }
        if !self.layer_impls.is_empty() {
            self.layer_impls = Arc::new(Vec::new());
        }
        if !self.image_impls.is_empty() {
            self.image_impls = Arc::new(Vec::new());
        }

        let mut changes: UniqueChangeRequestVec = Vec::new();
        for layer in self.render_layers.values() {
            layer.borrow_mut().layer_removed(&mut changes);
        }
        self.add_changes(&mut changes);

        self.debug_layer_groups.clear(); // layers: debug
        self.render_sources.clear();
        self.render_layers.clear();
        // C++ keeps orderedLayers here and rebuilds it on the next
        // createRenderTree (the layer diff then reports additions).
        self.ordered_layers.clear();

        self.cross_tile_symbol_index.reset();
        if !self.line_atlas.borrow().is_empty() {
            self.line_atlas = Rc::new(RefCell::new(LineAtlas::new()));
        }

        if !self.pattern_atlas.borrow().is_empty() {
            self.pattern_atlas = Rc::new(RefCell::new(PatternAtlas::new()));
        }

        self.image_manager.clear();
        // TODO(port): glyphManager->evict(fontStacks(*layerImpls)).
    }

    /// symbols: C++ `Renderer::render` recreates the dynamic texture atlas
    /// when the style changed (`impl->dynamicTextureAtlas` is owned by the
    /// renderer and handed to createRenderTree).
    pub fn reset_dynamic_texture_atlas(&mut self) {
        self.dynamic_texture_atlas = Arc::new(DynamicTextureAtlas::new());
    }

    pub fn get_glyph_manager(&self) -> &Rc<GlyphManager> {
        &self.glyph_manager
    }

    pub fn get_image_manager(&self) -> &Rc<ImageManager> {
        &self.image_manager
    }

    fn add_changes(&mut self, changes: &mut UniqueChangeRequestVec) {
        self.pending_changes.append(changes);
    }

    pub fn update_layer_index(&mut self, layer_group: &LayerGroupBasePtr, new_index: i32) {
        let current = layer_group.borrow().get_layer_index();
        if current == new_index {
            return;
        }
        if let Some(groups) = self.layer_groups_by_layer_index.get_mut(&current) {
            if let Some(pos) = groups.iter().position(|g| Rc::ptr_eq(g, layer_group)) {
                groups.remove(pos);
                if groups.is_empty() {
                    self.layer_groups_by_layer_index.remove(&current);
                }
                layer_group.borrow_mut().update_layer_index(new_index);
                self.layer_groups_by_layer_index.entry(new_index).or_default().push(layer_group.clone());
                return;
            }
        }
        // We're not tracking the layer, indicating that it's currently
        // disabled, so update it directly.
        layer_group.borrow_mut().update_layer_index(new_index);
    }

    pub fn add_layer_group(&mut self, layer_group: LayerGroupBasePtr) -> bool {
        let index = layer_group.borrow().get_layer_index();
        let groups = self.layer_groups_by_layer_index.entry(index).or_default();
        if groups.iter().any(|g| Rc::ptr_eq(g, &layer_group)) {
            false // not added
        } else {
            groups.push(layer_group);
            true // added
        }
    }

    pub fn remove_layer_group(&mut self, layer_group: &LayerGroupBasePtr) -> bool {
        let index = layer_group.borrow().get_layer_index();
        if let Some(groups) = self.layer_groups_by_layer_index.get_mut(&index) {
            if let Some(pos) = groups.iter().position(|g| Rc::ptr_eq(g, layer_group)) {
                groups.remove(pos);
                if groups.is_empty() {
                    self.layer_groups_by_layer_index.remove(&index);
                }
                return true;
            }
        }
        false
    }

    pub fn num_layer_groups(&self) -> usize {
        self.layer_groups_by_layer_index.values().map(Vec::len).sum()
    }

    /// The layer groups in multimap order (ascending layer index).
    pub fn layer_groups(&self) -> Vec<LayerGroupBasePtr> {
        self.layer_groups_by_layer_index.values().flatten().cloned().collect()
    }

    /// `visitLayerGroups(f)`.
    pub fn visit_layer_groups(&self, mut f: impl FnMut(&mut dyn LayerGroupBase)) {
        for group in self.layer_groups() {
            f(&mut *group.borrow_mut());
        }
    }

    /// `visitLayerGroupsReversed(f)`.
    pub fn visit_layer_groups_reversed(&self, mut f: impl FnMut(&mut dyn LayerGroupBase)) {
        for group in self.layer_groups().into_iter().rev() {
            f(&mut *group.borrow_mut());
        }
    }

    /// `updateDebugLayerGroups(renderTree, parameters)` (layers: debug).
    pub fn update_debug_layer_groups(&mut self, render_tree: &RenderTree, parameters: &PaintParameters<'_>) {
        let mut tweakers: crate::render_tree::DebugLayerTweakers = Vec::new();
        for item in render_tree.get_source_render_items() {
            item.update_debug_drawables(&mut self.debug_layer_groups, parameters, &mut tweakers);
        }
        // `layerGroup->addLayerTweaker(tweaker)`
        for (layer_group, tweaker) in tweakers {
            let id = layer_group.borrow().get_id();
            self.layer_group_tweakers.entry(id).or_default().push(tweaker);
        }
    }

    /// The debug layer groups, in `DebugType` order (`visitDebugLayerGroups`)
    /// (layers: debug).
    pub fn debug_layer_groups(&self) -> Vec<LayerGroupBasePtr> {
        self.debug_layer_groups.values().cloned().collect()
    }

    /// `LayerGroupBase::runTweakers(renderTree, parameters)`: runs the live
    /// tweakers of the group and drops the expired ones.
    pub fn run_tweakers(&mut self, layer_group: &LayerGroupBasePtr, parameters: &PaintParameters<'_>) {
        let id = layer_group.borrow().get_id();
        let Some(tweakers) = self.layer_group_tweakers.get_mut(&id) else {
            return;
        };
        let mut i = 0;
        while i < tweakers.len() {
            if let Some(tweaker) = tweakers[i].upgrade() {
                tweaker.borrow_mut().execute(&mut *layer_group.borrow_mut(), parameters);
                i += 1;
            } else {
                tweakers.remove(i);
            }
        }
    }

    pub fn update_layers(
        &mut self,
        shaders: &ShaderRegistry,
        context: &dyn Context,
        state: &TransformState,
        update_parameters: &Arc<UpdateParameters>,
        paint_parameters: &PaintParameters<'_>,
        render_tree: &RenderTree,
    ) {
        // TODO(port): the PropertyEvaluationParameters C++ builds here are
        // unused by the layers' update().

        let items = render_tree.get_layer_render_item_map();

        let mut changes: UniqueChangeRequestVec = Vec::with_capacity(items.len() * 3);

        for item in items.values() {
            let mut render_layer = item.layer.borrow_mut();
            // C++ catches exceptions from update() and reports them through
            // onRenderError; the Rust layers don't throw.
            render_layer.update(
                shaders,
                context,
                state,
                update_parameters,
                paint_parameters,
                render_tree,
                &mut changes,
            );

            // TODO(port): accumulate renderLayer.stats.renderedFeatures
            // (rendered-feature capture).
        }
        self.add_changes(&mut changes);
    }

    pub fn process_changes(&mut self) {
        let local_changes = std::mem::take(&mut self.pending_changes);
        for change in local_changes {
            match change {
                ChangeRequest::AddLayerGroup(layer_group) => {
                    self.add_layer_group(layer_group);
                }
                ChangeRequest::RemoveLayerGroup(layer_group) => {
                    self.remove_layer_group(&layer_group);
                }
                ChangeRequest::UpdateLayerGroupIndex(layer_group, new_index) => {
                    self.update_layer_index(&layer_group, new_index);
                }
                ChangeRequest::AddLayerTweaker(layer_group, tweaker) => {
                    let id = layer_group.borrow().get_id();
                    self.layer_group_tweakers.entry(id).or_default().push(tweaker);
                }
                // dem-heatmap
                ChangeRequest::AddRenderTarget(render_target) => {
                    self.add_render_target(render_target);
                }
                ChangeRequest::RemoveRenderTarget(render_target) => {
                    self.remove_render_target(&render_target);
                }
            }
        }
    }

    // dem-heatmap: render targets.
    /// `addRenderTarget(RenderTargetPtr)`.
    pub fn add_render_target(&mut self, render_target: crate::render_target::RenderTargetPtr) -> bool {
        if self.render_targets.iter().any(|t| Rc::ptr_eq(t, &render_target)) {
            false
        } else {
            self.render_targets.push(render_target);
            true
        }
    }

    /// `removeRenderTarget(const RenderTargetPtr&)`.
    pub fn remove_render_target(&mut self, render_target: &crate::render_target::RenderTargetPtr) -> bool {
        match self.render_targets.iter().position(|t| Rc::ptr_eq(t, render_target)) {
            Some(pos) => {
                self.render_targets.remove(pos);
                true
            }
            None => false,
        }
    }

    /// The render targets, for `visitRenderTargets` (cloned, so that the
    /// visitor may use the orchestrator).
    pub fn render_targets(&self) -> Vec<crate::render_target::RenderTargetPtr> {
        self.render_targets.clone()
    }

    // query
    /// `queryRenderedFeatures(geometry, options)`. `Err`: the exception C++
    /// throws out of the query (a malformed tile).
    pub fn query_rendered_features(
        &self,
        geometry: &[mln_util::geo::ScreenCoordinate],
        options: &crate::query::RenderedQueryOptions,
    ) -> Result<Vec<mln_util::feature::Feature>, ExceptionPtr> {
        let render_layers: Vec<&crate::render_layer::RenderLayerPtr> = match &options.layer_ids {
            Some(layer_ids) => layer_ids.iter().filter_map(|id| self.render_layers.get(id)).collect(),
            None => self.render_layers.values().collect(),
        };
        let borrowed: Vec<std::cell::Ref<'_, dyn crate::render_layer::RenderLayer>> =
            render_layers.iter().map(|layer| layer.borrow()).collect();
        let mut layers = crate::geometry::feature_index::QueryLayers::new();
        for layer in &borrowed {
            layers.entry(layer.get_id().to_owned()).or_insert(&**layer);
        }

        self.query_rendered_features_in(geometry, options, &layers)
    }

    /// `queryRenderedSymbols(resultsByLayer, geometry, layers, options)`.
    fn query_rendered_symbols(
        &self,
        results_by_layer: &mut crate::geometry::feature_index::QueryResults,
        geometry: &[mln_util::geo::ScreenCoordinate],
        layers: &crate::geometry::feature_index::QueryLayers<'_>,
        options: &crate::query::RenderedQueryOptions,
    ) -> Result<(), ExceptionPtr> {
        let cross_tile_symbol_index_layers: crate::geometry::feature_index::QueryLayers<'_> = layers
            .iter()
            .filter(|(_, layer)| {
                layer.base().base_impl.get_type_info().cross_tile_index == layer_type_info::CrossTileIndex::Required
            })
            .map(|(id, layer)| (id.clone(), *layer))
            .collect();

        if cross_tile_symbol_index_layers.is_empty() {
            return Ok(());
        }
        let placement = self.placement_controller.get_placement();
        let mut rendered_symbols = placement.get_collision_index().query_rendered_symbols(&geometry.to_vec());
        let mut bucket_query_data = Vec::with_capacity(rendered_symbols.len());
        for bucket_instance_id in rendered_symbols.keys() {
            // C++ `getQueryData` throws for an unknown bucket.
            bucket_query_data.push(placement.get_query_data(*bucket_instance_id).map_err(RuntimeError::ptr)?);
        }
        // Although symbol query is global, symbol results are only sortable within
        // a bucket For a predictable global sort renderItems, we sort the buckets
        // based on their corresponding tile position
        bucket_query_data.sort_by(|a, b| {
            (a.tile_id.canonical.z, a.tile_id.canonical.y, a.tile_id.wrap, a.tile_id.canonical.x).cmp(&(
                b.tile_id.canonical.z,
                b.tile_id.canonical.y,
                b.tile_id.wrap,
                b.tile_id.canonical.x,
            ))
        });

        for query_data in bucket_query_data {
            let Some(feature_index) = &query_data.feature_index else {
                continue;
            };
            let symbol_features = rendered_symbols.remove(&query_data.bucket_instance_id).unwrap_or_default();
            let bucket_symbols = feature_index.lookup_symbol_features(
                &symbol_features,
                options,
                self.global_state.as_deref(),
                &cross_tile_symbol_index_layers,
                &query_data.tile_id,
                &query_data.feature_sort_order.borrow().clone(),
            )?;

            for (layer_id, features) in bucket_symbols {
                results_by_layer.entry(layer_id).or_default().extend(features);
            }
        }
        Ok(())
    }

    /// `queryRenderedFeatures(geometry, options, layers)`.
    fn query_rendered_features_in(
        &self,
        geometry: &[mln_util::geo::ScreenCoordinate],
        options: &crate::query::RenderedQueryOptions,
        layers: &crate::geometry::feature_index::QueryLayers<'_>,
    ) -> Result<Vec<mln_util::feature::Feature>, ExceptionPtr> {
        let mut source_ids: VecSet<String> = VecSet::new();
        let mut filtered_layers = crate::geometry::feature_index::QueryLayers::new();
        for (id, layer) in layers {
            if !layer.needs_rendering() || !layer.supports_zoom(self.zoom_history.last_zoom) {
                continue;
            }
            filtered_layers.insert(id.clone(), *layer);
            source_ids.insert(layer.base().base_impl.source.clone());
        }

        let mut proj_matrix: mln_util::mat4::Mat4 = [0.0; 16];
        self.transform_state.get_proj_matrix(&mut proj_matrix, 1, false);

        let mut results_by_layer = crate::geometry::feature_index::QueryResults::new();
        for source_id in &source_ids {
            if let Some(render_source) = self.get_render_source(source_id) {
                let source_results = render_source.query_rendered_features(
                    geometry,
                    &self.transform_state,
                    &filtered_layers,
                    options,
                    self.global_state.as_deref(),
                    &proj_matrix,
                )?;
                for (layer_id, features) in source_results {
                    results_by_layer.entry(layer_id).or_insert(features);
                }
            }
        }

        self.query_rendered_symbols(&mut results_by_layer, geometry, &filtered_layers, options)?;

        // TODO(port): DynamicFeatureIndex (populateDynamicRenderFeatureIndex,
        // only the location indicator layer fills it).

        let mut result = Vec::new();

        if results_by_layer.is_empty() {
            return Ok(result);
        }

        // Combine all results based on the style layer renderItems.
        // DEVIATION: C++ iterates `filteredLayers`, an unordered_map (libstdc++
        // hash order); the port goes from the topmost style layer down, the
        // order GL JS (which wrote the expectations) returns.
        for layer_impl in self.layer_impls.iter().rev() {
            if !filtered_layers.contains_key(&layer_impl.id) {
                continue;
            }
            if let Some(features) = results_by_layer.remove(&layer_impl.id) {
                result.extend(features);
            }
        }

        Ok(result)
    }

    /// `getPlacedSymbolsData()` (a copy: the placement is shared).
    pub fn get_placed_symbols_data(&self) -> Vec<crate::text::placement::PlacedSymbolData> {
        self.placement_controller.get_placement().get_placed_symbols_data().to_vec()
    }

    /// `querySourceFeatures(sourceID, options)`. `Err`: as
    /// [`query_rendered_features`](Self::query_rendered_features).
    pub fn query_source_features(
        &self,
        source_id: &str,
        options: &crate::query::SourceQueryOptions,
    ) -> Result<Vec<mln_util::feature::Feature>, ExceptionPtr> {
        let Some(source) = self.get_render_source(source_id) else {
            return Ok(Vec::new());
        };

        source.query_source_features(options, self.global_state.as_deref())
    }

    /// `dumpDebugLogs()`.
    pub fn dump_debug_logs(&self) {
        for source in self.render_sources.values() {
            source.dump_debug_logs();
        }
        self.image_manager.dump_debug_logs();
    }

    // TODO(port): queryShapeAnnotations, queryFeatureExtensions, debug layer
    // groups and the glyph/tile observer callbacks.
    // layers: feature state
    /// `setFeatureState(sourceID, sourceLayerID, featureID, state)`.
    pub fn set_feature_state(
        &mut self,
        source_id: &str,
        source_layer_id: Option<&str>,
        feature_id: &str,
        state: &mln_style::expression::expression::FeatureState,
    ) {
        if let Some(render_source) = self.render_sources.get_mut(source_id) {
            render_source.set_feature_state(source_layer_id, feature_id, state);
        }
    }

    /// `getFeatureState(state, sourceID, sourceLayerID, featureID)`.
    pub fn get_feature_state(
        &self,
        source_id: &str,
        source_layer_id: Option<&str>,
        feature_id: &str,
    ) -> mln_style::expression::expression::FeatureState {
        match self.render_sources.get(source_id) {
            Some(render_source) => render_source.get_feature_state(source_layer_id, feature_id),
            None => Default::default(),
        }
    }

    /// `removeFeatureState(sourceID, sourceLayerID, featureID, stateKey)`.
    pub fn remove_feature_state(
        &mut self,
        source_id: &str,
        source_layer_id: Option<&str>,
        feature_id: Option<&str>,
        state_key: Option<&str>,
    ) {
        if let Some(render_source) = self.render_sources.get_mut(source_id) {
            render_source.remove_feature_state(source_layer_id, feature_id, state_key);
        }
    }

    pub fn collect_placed_symbol_data(&mut self, enable: bool) {
        self.placed_symbol_data_collected = enable;
    }

    pub fn set_tile_cache_enabled(&mut self, enable: bool) {
        self.tile_cache_enabled = enable;

        for source in self.render_sources.values_mut() {
            source.set_cache_enabled(enable);
        }
    }

    /// `reduceMemoryUse()`.
    pub fn reduce_memory_use(&mut self) {
        self.filtered_layers_for_source.shrink_to_fit();
        for source in self.render_sources.values_mut() {
            source.reduce_memory_use();
        }
        // TODO(port): imageManager->reduceMemoryUse(), observer->onInvalidate()
        // as C++ does afterwards.
    }

    pub fn get_tile_cache_enabled(&self) -> bool {
        self.tile_cache_enabled
    }

    pub fn get_observer(&self) -> Option<Rc<dyn RendererObserver>> {
        self.observer()
    }
}

impl Drop for RenderOrchestrator {
    fn drop(&mut self) {
        if self.context_lost {
            // Signal all RenderLayers that the context was lost before
            // cleaning up.
            for layer in self.render_layers.values() {
                layer.borrow_mut().mark_context_destroyed();
            }
        }
        // Destroy the render sources (and their tiles and workers) before
        // waiting for the thread pool, as the C++ members' destructors do.
        self.render_sources.clear();
        self.thread_pool.wait_for_empty();
    }
}
