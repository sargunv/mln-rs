// Ports include/mln/map/map.hpp and src/mln/map/map.cpp (the parts M1 runs).

use std::cell::Ref;
use std::rc::Rc;
use std::sync::Arc;

use mln_map_types::{
    AnimationOptions, BoundOptions, CameraDelta, CameraError, CameraMomentum, CameraOptions, CameraUpdate,
    ConstrainMode, MapDebugOptions, MapMode, MapView, NorthOrientation, ProjectionMode, TileLodMode, TransformState,
    ViewportMode,
};
use mln_renderer::{ExceptionPtr, FeatureInfo, LayerFeaturesMap, RendererFrontend, RenderingStats, SourceLayerID};
use mln_storage::{ClientOptions, FileSource, FileSourceManager, FileSourceType, ResourceOptions};
use mln_style::Style;
use mln_util::chrono::TimePoint;
use mln_util::exception::MisuseException;
use mln_util::geo::{EdgeInsets, LatLng, LatLngBounds, ScreenCoordinate, WrapMode};
use mln_util::geometry::{Geometry, for_each_point};
use mln_util::size::Size;

use crate::map_impl::{MapImpl, StillImageCallback, StillImageRequest, StyleMut};
use crate::map_observer::MapObserver;
use crate::map_options::MapOptions;
use crate::map_snapshot::{CameraBounds, FreeCamera, MapSnapshot, TileSettings};
use crate::rendering_stats_view::{RenderingStatsView, RenderingStatsViewOptions};

pub struct Map {
    impl_: Rc<MapImpl>,
}

impl Map {
    /// `Map(RendererFrontend&, MapObserver&, const MapOptions&)` with the
    /// default `ResourceOptions()` and `ClientOptions()`.
    ///
    /// C++ keeps a reference to the frontend; frontends are handles here
    /// (`Clone` shares the same renderer), so the map keeps a clone.
    ///
    /// `Err` where C++ throws: "failed to resize: size is empty" for an
    /// empty `map_options.size()`.
    pub fn new<F>(frontend: &F, observer: Rc<dyn MapObserver>, map_options: MapOptions) -> Result<Map, String>
    where
        F: RendererFrontend + Clone + 'static,
    {
        Self::with_resource_options(
            frontend,
            observer,
            map_options,
            &ResourceOptions::default(),
            &ClientOptions::default(),
        )
    }

    /// `Map(RendererFrontend&, MapObserver&, const MapOptions&, const
    /// ResourceOptions&, const ClientOptions&)`: the file source is the
    /// FileSourceManager's `ResourceLoader` for these options.
    // TODO(port): ActionJournalOptions.
    pub fn with_resource_options<F>(
        frontend: &F,
        observer: Rc<dyn MapObserver>,
        map_options: MapOptions,
        resource_options: &ResourceOptions,
        client_options: &ClientOptions,
    ) -> Result<Map, String>
    where
        F: RendererFrontend + Clone + 'static,
    {
        let file_source =
            FileSourceManager::get().get_file_source(FileSourceType::ResourceLoader, resource_options, client_options);
        Ok(Map { impl_: MapImpl::new(Box::new(frontend.clone()), observer, file_source, &map_options)? })
    }

    // gtests-core: the C++ tests' `MapAdapter` (test/include/mln/test/
    // map_adapter.hpp), i.e. `Map(std::make_unique<Map::Impl>(frontend,
    // observer, fileSource, options))`: a map over a given file source.
    /// `Map(std::unique_ptr<Impl>)` with `Impl(frontend, observer, fileSource, mapOptions)`.
    pub fn with_file_source<F>(
        frontend: &F,
        observer: Rc<dyn MapObserver>,
        file_source: Arc<dyn FileSource>,
        map_options: MapOptions,
    ) -> Result<Map, String>
    where
        F: RendererFrontend + Clone + 'static,
    {
        Ok(Map { impl_: MapImpl::new(Box::new(frontend.clone()), observer, Some(file_source), &map_options)? })
    }

    /// `getClientOptions()`.
    pub fn get_client_options(&self) -> ClientOptions {
        match &self.impl_.file_source {
            Some(file_source) => file_source.get_client_options(),
            None => ClientOptions::default(),
        }
    }

    /// Register a callback that will get called (on the render thread) when
    /// all resources have been loaded and a complete render occurs.
    pub fn render_still(&mut self, callback: StillImageCallback) {
        let impl_ = &self.impl_;
        if impl_.mode != MapMode::Static && impl_.mode != MapMode::Tile {
            callback(Some(misuse("Map is not in static or tile image render modes")));
            return;
        }

        if impl_.still_image_request.borrow().is_some() {
            callback(Some(misuse("Map is currently rendering an image")));
            return;
        }

        let last_error = impl_.style.borrow().impl_.get_last_error();
        if let Some(error) = last_error {
            callback(Some(error));
            return;
        }

        *impl_.still_image_request.borrow_mut() = Some(StillImageRequest { callback });
        impl_.still_image_request_id.set(impl_.still_image_request_id.get() + 1);

        impl_.on_update();
    }

    /// `renderStill(const CameraOptions&, MapDebugOptions, StillImageCallback)`.
    pub fn render_still_with_camera(
        &mut self,
        camera: &CameraOptions,
        debug_options: MapDebugOptions,
        callback: StillImageCallback,
    ) {
        self.impl_.changed();
        self.impl_.camera_mutated.set(true);
        self.impl_.debug_options.set(debug_options);
        self.impl_.transform.borrow_mut().jump_to(camera);
        self.render_still(callback);
    }

    /// Triggers a repaint.
    pub fn trigger_repaint(&mut self) {
        self.impl_.on_update();
    }

    // MARK: - Style

    /// `const style::Style& getStyle() const`.
    pub fn get_style(&self) -> Ref<'_, Style> {
        self.impl_.style.borrow()
    }

    /// `style::Style& getStyle()`: a guard that derefs to `&mut Style`; the
    /// style's observer calls made through it reach the map when it drops.
    pub fn get_style_mut(&mut self) -> StyleMut<'_> {
        self.impl_.changed();
        StyleMut::new(&self.impl_)
    }

    // TODO(port): setStyle(std::unique_ptr<style::Style>).

    // MARK: - Transitions

    pub fn cancel_transitions(&mut self) {
        self.impl_.transform.borrow_mut().cancel_transitions();
        self.impl_.on_update();
    }

    /// `cancelTransitions(uint64_t transitionId)`: cancels the commands whose
    /// [`AnimationOptions::transition_id`] equals `transition_id`, and leaves
    /// the others running. Requests an update only when a command ended.
    pub fn cancel_transitions_with_id(&mut self, transition_id: u64) {
        let ended = self.impl_.transform.borrow_mut().cancel_transitions_with_id(transition_id);
        if ended {
            self.impl_.on_update();
        }
    }

    /// Moves the camera relative to where it is, as a gesture does
    /// (maplibre-native-ffi's `mln_map_apply_camera_delta`): a pan, a zoom,
    /// turn and tilt around an anchor, a gesture phase, and the momentum a
    /// gesture's end continues with. See [`CameraDelta`].
    pub fn apply_camera_delta(&mut self, delta: &CameraDelta) -> Result<(), CameraError> {
        self.impl_.transform.borrow_mut().apply_camera_delta(delta)?;
        self.impl_.camera_mutated.set(true);
        self.impl_.on_update();
        Ok(())
    }

    /// Moves the camera to absolute values, by a jump, an ease or a flight,
    /// as a command or as part of a gesture (maplibre-native-ffi's
    /// `mln_map_update_camera`). See [`CameraUpdate`].
    pub fn update_camera(&mut self, update: &CameraUpdate) {
        self.impl_.camera_mutated.set(true);
        self.impl_.transform.borrow_mut().update_camera(update);
        self.impl_.on_update();
    }

    /// Continues a gesture with momentum, which moves the camera on each
    /// frame until its parts end. See [`CameraMomentum`].
    ///
    /// A low-level call, kept for parity with the C++ core's: it doesn't end
    /// a gesture or report the change as momentum. Gestures end with an
    /// `End` delta that carries their momentum
    /// ([`apply_camera_delta`](Self::apply_camera_delta)).
    pub fn start_momentum(&mut self, momentum: &CameraMomentum, animation: &AnimationOptions) {
        self.impl_.camera_mutated.set(true);
        self.impl_.transform.borrow_mut().start_momentum(momentum, animation);
        self.impl_.on_update();
    }

    /// `setGestureInProgress`. A low-level call, kept for parity with the
    /// C++ core's: the gesture phases of
    /// [`apply_camera_delta`](Self::apply_camera_delta) and
    /// [`update_camera`](Self::update_camera) set it for a gesture and its
    /// momentum, and setting it directly while one runs gets out of step with
    /// them.
    pub fn set_gesture_in_progress(&mut self, in_progress: bool) {
        self.impl_.transform.borrow_mut().set_gesture_in_progress(in_progress);
        self.impl_.on_update();
    }

    pub fn is_gesture_in_progress(&self) -> bool {
        self.impl_.transform.borrow().is_gesture_in_progress()
    }

    pub fn is_rotating(&self) -> bool {
        self.impl_.transform.borrow().is_rotating()
    }

    pub fn is_scaling(&self) -> bool {
        self.impl_.transform.borrow().is_scaling()
    }

    pub fn is_panning(&self) -> bool {
        self.impl_.transform.borrow().is_panning()
    }

    /// Samples the camera's animations at the host's frame time, from now on,
    /// and starts new ones from it: call it at the start of each frame with
    /// `Clock::now()`, before rendering. One older than
    /// [`STALE_FRAME_TIME`](crate::transform::STALE_FRAME_TIME) counts as
    /// unset, so commands made while the host isn't drawing start now. If an
    /// animation runs, the map updates for this frame at once. `None` goes
    /// back to sampling at `Clock::now()` whenever the map updates. Still
    /// images ignore it. See
    /// [`Transform::set_frame_time`](crate::transform::Transform::set_frame_time).
    pub fn set_frame_time(&mut self, time: Option<TimePoint>) {
        self.impl_.transform.borrow_mut().set_frame_time(time);
        if self.impl_.transform.borrow().in_transition() {
            self.impl_.on_update();
        }
    }

    /// Whether any camera command is still animating. A command's
    /// `MapObserver::on_camera_did_change` ends that command only, so this
    /// tells whether the camera stopped once the last one ended.
    pub fn in_transition(&self) -> bool {
        self.impl_.transform.borrow().in_transition()
    }

    // MARK: - Camera

    pub fn get_camera_options(&self, padding: Option<EdgeInsets>) -> CameraOptions {
        self.impl_.transform.borrow().get_camera_options(padding)
    }

    pub fn jump_to(&mut self, camera: &CameraOptions) {
        self.impl_.jump_to(camera);
    }

    // query: the render-test runner's tile mode uses it.
    /// `latLngBoundsForCamera(camera)`.
    /// `Err`: the std::domain_error of the C++ LatLng constructor, for a
    /// camera whose corners have no LatLng.
    pub fn lat_lng_bounds_for_camera(&self, camera: &CameraOptions) -> Result<mln_util::geo::LatLngBounds, String> {
        let mut shallow = crate::transform::Transform::from_state(self.impl_.transform.borrow().get_state().clone());
        let size = shallow.get_state().get_size();

        shallow.jump_to(camera);
        Ok(mln_util::geo::LatLngBounds::hull(
            shallow.screen_coordinate_to_lat_lng(&ScreenCoordinate::new(0.0, 0.0), mln_util::geo::WrapMode::Wrapped)?,
            shallow.screen_coordinate_to_lat_lng(
                &ScreenCoordinate::new(f64::from(size.width), f64::from(size.height)),
                mln_util::geo::WrapMode::Wrapped,
            )?,
        ))
    }

    pub fn ease_to(&mut self, camera: &CameraOptions, animation: &AnimationOptions) {
        self.impl_.camera_mutated.set(true);
        self.impl_.transform.borrow_mut().ease_to(camera, animation);
        self.impl_.on_update();
    }

    pub fn fly_to(&mut self, camera: &CameraOptions, animation: &AnimationOptions) {
        self.impl_.camera_mutated.set(true);
        self.impl_.transform.borrow_mut().fly_to(camera, animation, false);
        self.impl_.on_update();
    }

    /// `Err`: the std::domain_error of the C++ LatLng constructor (a NaN or
    /// infinite point).
    pub fn move_by(&mut self, point: &ScreenCoordinate, animation: &AnimationOptions) -> Result<(), String> {
        self.impl_.camera_mutated.set(true);
        self.impl_.transform.borrow_mut().move_by(point, animation)?;
        self.impl_.on_update();
        Ok(())
    }

    pub fn pitch_by(&mut self, pitch: f64, animation: &AnimationOptions) {
        let current = mln_math::rad2deg(self.impl_.transform.borrow().get_pitch());
        self.ease_to(&CameraOptions::default().with_pitch(Some(current - pitch)), animation);
    }

    pub fn scale_by(&mut self, scale: f64, anchor: Option<ScreenCoordinate>, animation: &AnimationOptions) {
        let zoom = {
            let transform = self.impl_.transform.borrow();
            transform.get_zoom() + transform.get_state().scale_zoom(scale)
        };
        self.ease_to(&CameraOptions::default().with_zoom(Some(zoom)).with_anchor(anchor), animation);
    }

    pub fn rotate_by(&mut self, first: &ScreenCoordinate, second: &ScreenCoordinate, animation: &AnimationOptions) {
        self.impl_.camera_mutated.set(true);
        self.impl_.transform.borrow_mut().rotate_by(first, second, animation);
        self.impl_.on_update();
    }

    // gtests-core: cameraForLatLngBounds, cameraForLatLngs,
    // latLngBoundsForCameraUnwrapped (test/map/map.test.cpp).
    /// `cameraForLatLngBounds(bounds, padding, bearing, pitch)`. `Err`: the
    /// std::domain_error of the C++ LatLng constructor, for a center that
    /// has none (padding that pushes it to infinity).
    pub fn camera_for_lat_lng_bounds(
        &self,
        bounds: &LatLngBounds,
        padding: &EdgeInsets,
        bearing: Option<f64>,
        pitch: Option<f64>,
    ) -> Result<CameraOptions, String> {
        self.camera_for_lat_lngs(
            &[bounds.northwest(), bounds.southwest(), bounds.southeast(), bounds.northeast()],
            padding,
            bearing,
            pitch,
        )
    }

    /// `cameraForLatLngs(latLngs, padding, bearing, pitch)`. `Err`: as
    /// [`camera_for_lat_lng_bounds`](Self::camera_for_lat_lng_bounds).
    pub fn camera_for_lat_lngs(
        &self,
        lat_lngs: &[LatLng],
        padding: &EdgeInsets,
        bearing: Option<f64>,
        pitch: Option<f64>,
    ) -> Result<CameraOptions, String> {
        if bearing.is_none() && pitch.is_none() {
            return camera_for_lat_lngs(lat_lngs, &self.impl_.transform.borrow(), padding);
        }

        let mut transform = crate::transform::Transform::from_state(self.impl_.transform.borrow().get_state().clone());

        transform.jump_to(&CameraOptions::default().with_bearing(bearing).with_pitch(pitch));

        Ok(camera_for_lat_lngs(lat_lngs, &transform, padding)?
            .with_bearing(Some(mln_math::rad2deg(-transform.get_bearing())))
            .with_pitch(Some(mln_math::rad2deg(transform.get_pitch()))))
    }

    /// `cameraForGeometry(geometry, padding, bearing, pitch)`: the camera
    /// that fits every point of `geometry` (x longitude, y latitude), as
    /// [`camera_for_lat_lngs`](Self::camera_for_lat_lngs) fits them. `Err`:
    /// as that, and the std::domain_error of the C++ LatLng constructor for
    /// a point with no LatLng (an infinite or NaN coordinate, or a latitude
    /// beyond ±90°).
    pub fn camera_for_geometry(
        &self,
        geometry: &Geometry<f64>,
        padding: &EdgeInsets,
        bearing: Option<f64>,
        pitch: Option<f64>,
    ) -> Result<CameraOptions, String> {
        let mut points = Vec::new();
        for_each_point(geometry, &mut |point| points.push(*point));
        let lat_lngs = points.iter().map(|point| LatLng::new(point.y, point.x)).collect::<Result<Vec<_>, _>>()?;
        self.camera_for_lat_lngs(&lat_lngs, padding, bearing, pitch)
    }

    /// `latLngBoundsForCameraUnwrapped(camera)`.
    /// `Err`: as [`lat_lng_bounds_for_camera`](Self::lat_lng_bounds_for_camera).
    pub fn lat_lng_bounds_for_camera_unwrapped(&self, camera: &CameraOptions) -> Result<LatLngBounds, String> {
        let mut shallow = crate::transform::Transform::from_state(self.impl_.transform.borrow().get_state().clone());
        let size = shallow.get_state().get_size();

        shallow.jump_to(camera);
        let width = f64::from(size.width);
        let height = f64::from(size.height);
        let mut nw = shallow.screen_coordinate_to_lat_lng(&ScreenCoordinate::new(0.0, 0.0), WrapMode::Wrapped)?;
        let mut se = shallow.screen_coordinate_to_lat_lng(&ScreenCoordinate::new(width, height), WrapMode::Wrapped)?;
        let mut ne = shallow.screen_coordinate_to_lat_lng(&ScreenCoordinate::new(width, 0.0), WrapMode::Wrapped)?;
        let mut sw = shallow.screen_coordinate_to_lat_lng(&ScreenCoordinate::new(0.0, height), WrapMode::Wrapped)?;
        let center = shallow
            .screen_coordinate_to_lat_lng(&ScreenCoordinate::new(width / 2.0, height / 2.0), WrapMode::Wrapped)?;
        nw.unwrap_for_shortest_path(&center);
        se.unwrap_for_shortest_path(&center);
        ne.unwrap_for_shortest_path(&center);
        sw.unwrap_for_shortest_path(&center);
        let mut bounds = LatLngBounds::hull(nw, se);
        bounds.extend(&ne);
        bounds.extend(&sw);
        bounds.extend(&center);
        Ok(bounds)
    }

    // MARK: - Bounds

    pub fn set_bounds(&mut self, options: &BoundOptions) -> Result<(), String> {
        if *options != BoundOptions::default() {
            self.impl_.changed();
        }
        let mut change_camera = false;
        let mut camera_options = CameraOptions::default();

        {
            let mut transform = self.impl_.transform.borrow_mut();

            if let Some(bounds) = options.bounds {
                change_camera = true;
                transform.set_lat_lng_bounds(bounds)?;
            }

            if let Some(min_zoom) = options.min_zoom {
                transform.set_min_zoom(min_zoom);
                if transform.get_zoom() < min_zoom {
                    change_camera = true;
                    camera_options = camera_options.with_zoom(options.min_zoom);
                }
            }

            if let Some(max_zoom) = options.max_zoom {
                transform.set_max_zoom(max_zoom);
                if transform.get_zoom() > max_zoom {
                    change_camera = true;
                    camera_options = camera_options.with_zoom(options.max_zoom);
                }
            }

            if let Some(max_pitch) = options.max_pitch {
                transform.set_max_pitch(max_pitch);
                if transform.get_pitch() > transform.get_state().get_max_pitch() {
                    change_camera = true;
                    camera_options = camera_options.with_pitch(options.max_pitch);
                }
            }

            if let Some(min_pitch) = options.min_pitch {
                transform.set_min_pitch(min_pitch);
                if transform.get_pitch() < transform.get_state().get_min_pitch() {
                    change_camera = true;
                    camera_options = camera_options.with_pitch(options.min_pitch);
                }
            }
        }

        if change_camera {
            self.jump_to(&camera_options);
        }
        Ok(())
    }

    /// Returns the current map bound options. All optional fields in
    /// BoundOptions are set.
    pub fn get_bounds(&self) -> BoundOptions {
        let transform = self.impl_.transform.borrow();
        let state = transform.get_state();
        BoundOptions::default()
            .with_lat_lng_bounds(state.get_lat_lng_bounds())
            .with_min_zoom(state.get_min_zoom())
            .with_max_zoom(state.get_max_zoom())
            .with_min_pitch(mln_math::rad2deg(state.get_min_pitch()))
            .with_max_pitch(mln_math::rad2deg(state.get_max_pitch()))
    }

    // MARK: - Map options

    pub fn set_north_orientation(&mut self, orientation: NorthOrientation) {
        self.impl_.transform.borrow_mut().set_north_orientation(orientation);
        self.impl_.on_update();
    }

    pub fn set_constrain_mode(&mut self, mode: ConstrainMode) {
        self.impl_.transform.borrow_mut().set_constrain_mode(mode);
        self.impl_.on_update();
    }

    pub fn set_viewport_mode(&mut self, mode: ViewportMode) {
        self.impl_.transform.borrow_mut().set_viewport_mode(mode);
        self.impl_.on_update();
    }

    /// C++ throws "failed to resize: size is empty"; the port returns it.
    pub fn set_size(&mut self, size: Size) -> Result<(), String> {
        self.impl_.transform.borrow_mut().resize(size)?;
        self.impl_.on_update();
        Ok(())
    }

    pub fn set_frustum_offset(&mut self, frustum_offset: &EdgeInsets) {
        self.impl_.transform.borrow_mut().set_frustum_offset(frustum_offset);
        self.impl_.on_update();
    }

    pub fn get_frustum_offset(&self) -> EdgeInsets {
        self.impl_.transform.borrow().get_frustum_offset()
    }

    pub fn get_map_options(&self) -> MapOptions {
        let transform = self.impl_.transform.borrow();
        let state = transform.get_state();
        MapOptions::new()
            .with_north_orientation(transform.get_north_orientation())
            .with_map_mode(self.impl_.mode)
            .with_constrain_mode(state.get_constrain_mode())
            .with_viewport_mode(state.get_viewport_mode())
            .with_cross_source_collisions(self.impl_.cross_source_collisions)
            .with_size(state.get_size())
            .with_pixel_ratio(self.impl_.pixel_ratio)
    }

    // MARK: - Projection mode

    pub fn set_projection_mode(&mut self, options: &ProjectionMode) {
        self.impl_.transform.borrow_mut().set_projection_mode(options);
        self.impl_.on_update();
    }

    pub fn get_projection_mode(&self) -> ProjectionMode {
        self.impl_.transform.borrow().get_projection_mode()
    }

    // MARK: - Transform

    pub fn get_transform_state(&self) -> TransformState {
        self.impl_.transform.borrow().get_state().clone()
    }

    // MARK: - Tile LOD

    pub fn set_tile_lod_min_radius(&mut self, radius: f64) {
        if self.impl_.tile_lod_min_radius.get() == radius {
            return;
        }
        self.impl_.tile_lod_min_radius.set(radius);
        self.impl_.on_update();
    }

    pub fn get_tile_lod_min_radius(&self) -> f64 {
        self.impl_.tile_lod_min_radius.get()
    }

    pub fn set_tile_lod_scale(&mut self, scale: f64) {
        if self.impl_.tile_lod_scale.get() == scale {
            return;
        }
        self.impl_.tile_lod_scale.set(scale);
        self.impl_.on_update();
    }

    pub fn get_tile_lod_scale(&self) -> f64 {
        self.impl_.tile_lod_scale.get()
    }

    pub fn set_tile_lod_pitch_threshold(&mut self, threshold: f64) {
        if self.impl_.tile_lod_pitch_threshold.get() == threshold {
            return;
        }
        self.impl_.tile_lod_pitch_threshold.set(threshold);
        self.impl_.on_update();
    }

    pub fn get_tile_lod_pitch_threshold(&self) -> f64 {
        self.impl_.tile_lod_pitch_threshold.get()
    }

    pub fn set_tile_lod_zoom_shift(&mut self, shift: f64) {
        if self.impl_.tile_lod_zoom_shift.get() == shift {
            return;
        }
        self.impl_.tile_lod_zoom_shift.set(shift);
        self.impl_.on_update();
    }

    pub fn get_tile_lod_zoom_shift(&self) -> f64 {
        self.impl_.tile_lod_zoom_shift.get()
    }

    pub fn set_tile_lod_mode(&mut self, mode: TileLodMode) {
        if self.impl_.tile_lod_mode.get() == mode {
            return;
        }
        self.impl_.tile_lod_mode.set(mode);
        self.impl_.on_update();
    }

    pub fn get_tile_lod_mode(&self) -> TileLodMode {
        self.impl_.tile_lod_mode.get()
    }

    // MARK: - Rendered features
    //
    // With `MapOptions::with_rendered_feature_info(true)`, these give the
    // features rendered in the most recently completed frame.

    /// The stats of the most recently completed frame, whose
    /// `frame_rendered_features` holds every rendered feature by source and
    /// layer. `None` before the first frame.
    pub fn get_last_frame_stats(&self) -> Option<Arc<RenderingStats>> {
        self.impl_.last_frame_stats.borrow().clone()
    }

    /// Calls `result` with the ID and info of each feature rendered in the
    /// last frame that matches the given feature, layer and source IDs
    /// (`None` matches any), until it returns `false`.
    pub fn get_rendered_features(
        &self,
        feature_id: Option<&str>,
        layer_id: Option<&str>,
        source_id: Option<&str>,
        mut result: impl FnMut(&str, &FeatureInfo) -> bool,
    ) {
        let Some(stats) = self.get_last_frame_stats() else {
            return;
        };
        let features = &stats.frame_rendered_features;

        let mut search_features = |layer_features: &LayerFeaturesMap| {
            for (id, info) in layer_features {
                if feature_id.is_none_or(|feature_id| id == feature_id) && !result(id, info) {
                    return false;
                }
            }
            true
        };

        if let (Some(source_id), Some(layer_id)) = (source_id, layer_id) {
            // Both source and layer are specified, look for an exact match
            let key = SourceLayerID { source_id: source_id.to_owned(), layer_id: layer_id.to_owned() };
            if let Some(layer_features) = features.get(&key) {
                search_features(layer_features);
            }
        } else {
            // Linear search for matching items
            for (id, layer_features) in features {
                if source_id.is_none_or(|source_id| id.source_id == source_id)
                    && layer_id.is_none_or(|layer_id| id.layer_id == layer_id)
                    && !search_features(layer_features)
                {
                    break;
                }
            }
        }
    }

    /// The number of matching features rendered in the last frame (see
    /// [`get_rendered_features`](Self::get_rendered_features)).
    pub fn get_rendered_feature_count(
        &self,
        feature_id: Option<&str>,
        layer_id: Option<&str>,
        source_id: Option<&str>,
    ) -> usize {
        let mut count = 0;
        self.get_rendered_features(feature_id, layer_id, source_id, |_, _| {
            count += 1;
            true
        });
        count
    }

    /// Whether any matching feature was rendered in the last frame (see
    /// [`get_rendered_features`](Self::get_rendered_features)).
    pub fn is_feature_rendered(
        &self,
        feature_id: Option<&str>,
        layer_id: Option<&str>,
        source_id: Option<&str>,
    ) -> bool {
        let mut found = false;
        self.get_rendered_features(feature_id, layer_id, source_id, |_, _| {
            found = true;
            false
        });
        found
    }

    // gtests-core: projection and prefetch zoom delta (test/map, test/api).
    // MARK: - Projection

    /// `pixelForLatLng(latLng)`.
    pub fn pixel_for_lat_lng(&self, lat_lng: &LatLng) -> ScreenCoordinate {
        let transform = self.impl_.transform.borrow();
        transform.lat_lng_to_screen_coordinate(&transform.get_state().lat_lng_near_center(lat_lng))
    }

    /// `latLngForPixel(pixel, wrapMode)`. `Err`: the std::domain_error of
    /// the C++ LatLng constructor (a NaN or infinite pixel).
    pub fn lat_lng_for_pixel(&self, pixel: &ScreenCoordinate, wrap_mode: WrapMode) -> Result<LatLng, String> {
        self.impl_.transform.borrow().screen_coordinate_to_lat_lng(pixel, wrap_mode)
    }

    /// `pixelsForLatLngs(latLngs)`.
    pub fn pixels_for_lat_lngs(&self, lat_lngs: &[LatLng]) -> Vec<ScreenCoordinate> {
        lat_lngs.iter().map(|lat_lng| self.pixel_for_lat_lng(lat_lng)).collect()
    }

    /// `latLngsForPixels(screenCoords, wrapMode)`.
    pub fn lat_lngs_for_pixels(
        &self,
        screen_coords: &[ScreenCoordinate],
        wrap_mode: WrapMode,
    ) -> Result<Vec<LatLng>, String> {
        screen_coords.iter().map(|point| self.lat_lng_for_pixel(point, wrap_mode)).collect()
    }

    /// `setPrefetchZoomDelta(delta)`.
    pub fn set_prefetch_zoom_delta(&mut self, delta: u8) {
        if self.impl_.prefetch_zoom_delta.get() == delta {
            return;
        }
        self.impl_.prefetch_zoom_delta.set(delta);
        self.impl_.on_update();
    }

    /// `getPrefetchZoomDelta()`.
    pub fn get_prefetch_zoom_delta(&self) -> u8 {
        self.impl_.prefetch_zoom_delta.get()
    }

    /// `getFreeCameraOptions()`.
    pub fn get_free_camera_options(&self) -> mln_map_types::FreeCameraOptions {
        self.impl_.transform.borrow().get_free_camera_options()
    }

    /// `setFreeCameraOptions(camera)`: places the camera by its position
    /// and orientation, and cancels the running camera commands. A NaN
    /// position, or a NaN or zero-length orientation, is ignored; the
    /// position's height is clamped to the zoom range.
    pub fn set_free_camera_options(&mut self, camera: &mln_map_types::FreeCameraOptions) {
        self.impl_.transform.borrow_mut().set_free_camera_options(camera);
        self.impl_.camera_mutated.set(true);
        self.impl_.on_update();
    }

    // TODO(port): annotations, rendered features.

    // MARK: - Debug

    pub fn set_debug(&mut self, debug_options: MapDebugOptions) {
        self.impl_.debug_options.set(debug_options);
        self.impl_.on_update();
    }

    pub fn get_debug(&self) -> MapDebugOptions {
        self.impl_.debug_options.get()
    }

    /// `isRenderingStatsViewEnabled()`.
    pub fn is_rendering_stats_view_enabled(&self) -> bool {
        self.impl_.rendering_stats_view.borrow().is_some()
    }

    /// `enableRenderingStatsView(value)`: shows the rendering stats of the
    /// last frames in the top right corner, with a symbol layer (and its
    /// source) that the view adds to the style and keeps there, even when
    /// the style is replaced. The text needs the style's glyphs. Disabling
    /// the view removes them.
    pub fn enable_rendering_stats_view(&mut self, value: bool) {
        if value != self.is_rendering_stats_view_enabled() {
            self.impl_.changed();
        }
        if value {
            if self.is_rendering_stats_view_enabled() {
                return;
            }
            let view = RenderingStatsView::new(RenderingStatsViewOptions::default());
            view.create(&mut self.get_style_mut());
            *self.impl_.rendering_stats_view.borrow_mut() = Some(view);
        } else {
            let view = self.impl_.rendering_stats_view.borrow_mut().take();
            if let Some(view) = view {
                view.destroy(&mut self.get_style_mut());
            }
        }
    }

    /// Grows with each change to the map's state: each call that changes it,
    /// and each change that loading, rendering and animations make.
    pub fn generation(&self) -> u64 {
        self.impl_.generation.get()
    }

    /// Counts the updates that the map has sent to its renderer frontend.
    pub fn update_generation(&self) -> u64 {
        self.impl_.update_generation.get()
    }

    /// Copies the map's state.
    pub fn snapshot(&self) -> MapSnapshot {
        let impl_ = &self.impl_;
        let transform = impl_.transform.borrow();
        let state = transform.get_state();
        let bounds = state.get_lat_lng_bounds();
        let free_camera = state.get_free_camera_options();
        MapSnapshot {
            generation: impl_.generation.get(),
            update_generation: impl_.update_generation.get(),
            debug: impl_.debug_options.get(),
            fully_loaded: self.is_fully_loaded(),
            rendering_stats_view_enabled: self.is_rendering_stats_view_enabled(),
            gesture_in_progress: transform.is_gesture_in_progress(),
            transition_in_progress: transform.in_transition(),
            tile: TileSettings {
                prefetch_zoom_delta: impl_.prefetch_zoom_delta.get(),
                lod_min_radius: impl_.tile_lod_min_radius.get(),
                lod_scale: impl_.tile_lod_scale.get(),
                lod_pitch_threshold: mln_math::rad2deg(impl_.tile_lod_pitch_threshold.get()),
                lod_zoom_shift: impl_.tile_lod_zoom_shift.get(),
                lod_mode: impl_.tile_lod_mode.get(),
            },
            bounds: CameraBounds {
                bounds: (bounds != LatLngBounds::infinite()).then_some(bounds),
                min_zoom: state.get_min_zoom(),
                max_zoom: state.get_max_zoom(),
                min_pitch: mln_math::rad2deg(state.get_min_pitch()),
                max_pitch: mln_math::rad2deg(state.get_max_pitch()),
            },
            free_camera: FreeCamera {
                position: free_camera.position.unwrap_or_default(),
                orientation: free_camera.orientation.unwrap_or_default(),
            },
            view: MapView::from_state(state, f64::from(impl_.pixel_ratio)),
        }
    }

    pub fn is_fully_loaded(&self) -> bool {
        self.impl_.style.borrow().impl_.is_loaded() && self.impl_.renderer_fully_loaded.get()
    }

    /// `dumpDebugLogs()`: logs the style's sources and their state.
    pub fn dump_debug_logs(&self) {
        use mln_util::logging::{Event, Log};
        let rule = "-".repeat(80);
        Log::info(Event::General, &rule);
        self.impl_.style.borrow().impl_.dump_debug_logs();
        Log::info(Event::General, &rule);
    }
}

fn misuse(message: &str) -> ExceptionPtr {
    MisuseException::ptr(message)
}

// gtests-core: the free function `cameraForLatLngs(latLngs, transform,
// padding)` of map.cpp (Map::cameraForLatLngs and MapProjection use it).
pub(crate) fn camera_for_lat_lngs(
    lat_lngs: &[LatLng],
    transform: &crate::transform::Transform,
    padding: &EdgeInsets,
) -> Result<CameraOptions, String> {
    if lat_lngs.is_empty() {
        return Ok(CameraOptions::default());
    }
    let size = transform.get_state().get_size();
    // Calculate the bounds of the possibly rotated shape with respect to the viewport.
    let mut ne_pixel = ScreenCoordinate::new(f64::NEG_INFINITY, f64::NEG_INFINITY);
    let mut sw_pixel = ScreenCoordinate::new(f64::INFINITY, f64::INFINITY);
    for lat_lng in lat_lngs {
        let pixel = transform.lat_lng_to_screen_coordinate(lat_lng);
        // std::min / std::max
        sw_pixel.x = if pixel.x < sw_pixel.x { pixel.x } else { sw_pixel.x };
        ne_pixel.x = if ne_pixel.x < pixel.x { pixel.x } else { ne_pixel.x };
        sw_pixel.y = if pixel.y < sw_pixel.y { pixel.y } else { sw_pixel.y };
        ne_pixel.y = if ne_pixel.y < pixel.y { pixel.y } else { ne_pixel.y };
    }
    let width = ne_pixel.x - sw_pixel.x;
    let height = ne_pixel.y - sw_pixel.y;

    // Calculate the zoom level.
    let mut min_scale = f64::INFINITY;
    if width > 0.0 || height > 0.0 {
        let mut scale_x = f64::from(size.width) / width;
        let mut scale_y = f64::from(size.height) / height;
        scale_x -= (padding.left() + padding.right()) / width;
        scale_y -= (padding.top() + padding.bottom()) / height;
        min_scale = mln_math::min_f64(scale_x, scale_y);
    }

    let mut zoom = transform.get_zoom();
    if min_scale > 0.0 {
        zoom = mln_math::clamp_f64(
            zoom + mln_math::log2(min_scale),
            transform.get_state().get_min_zoom(),
            transform.get_state().get_max_zoom(),
        );
    } else {
        mln_util::logging::Log::error(
            mln_util::logging::Event::General,
            "Unable to calculate appropriate zoom level for bounds. Vertical or horizontal padding is greater than \
             map's height or width.",
        );
    }

    // Calculate the center point of a virtual bounds that is extended in all directions by padding.
    let center_pixel = ScreenCoordinate::new((ne_pixel.x + sw_pixel.x) / 2.0, (ne_pixel.y + sw_pixel.y) / 2.0);

    Ok(CameraOptions::default()
        .with_center(Some(transform.screen_coordinate_to_lat_lng(&center_pixel, WrapMode::Wrapped)?))
        .with_padding(Some(*padding))
        .with_zoom(Some(zoom)))
}
