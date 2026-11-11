// Ports the map side of render-test/runner.{hpp,cpp}: `TestRunnerMapObserver`,
// `TestRunner::Impl` (observer, HeadlessFrontend, Map), `swapBehavior`,
// `resetContext` and the map/style calls of the operations, over mln_map and
// mln_headless.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use mln_actor::RunLoop;
use mln_gfx::{ContextMode, SwapBehaviour};
use mln_headless::HeadlessFrontend;
use mln_map::{BoundOptions, Map, MapLoadError, MapObserver, MapOptions, ProjectionMode, RenderMode};
use mln_renderer::tile::worker_codec::{SerializingTransport, ThreadWorkerInstances};
use mln_storage::{FileSource, FileSourceManager, FileSourceType};
use mln_util::image::PremultipliedImage;

use crate::file_source::proxy_file_sources;
use crate::metadata::{MapMode, TestMetadata};
use crate::runner::{
    CameraOptions, ImplFactory, ImplOptions, MapError, QueryGeometry, ResourceOptions, TestRunnerImpl,
};
use crate::style_ops;

/// `TestRunnerMapObserver` (runner.hpp).
#[derive(Default)]
pub struct TestRunnerMapObserver {
    pub map_load_failure: Cell<bool>,
    pub finish_rendering_map: Cell<bool>,
    pub idle: Cell<bool>,
}

impl TestRunnerMapObserver {
    pub fn reset(&self) {
        self.map_load_failure.set(false);
        self.finish_rendering_map.set(false);
        self.idle.set(false);
    }
}

impl MapObserver for TestRunnerMapObserver {
    fn on_did_fail_loading_map(&self, _error: MapLoadError, _what: &str) {
        self.map_load_failure.set(true);
    }

    fn on_did_finish_rendering_map(&self, mode: RenderMode) {
        if !self.finish_rendering_map.get() {
            self.finish_rendering_map.set(mode == RenderMode::Full);
        }
    }

    fn on_did_become_idle(&self) {
        self.idle.set(true);
    }
}

fn map_mode(mode: MapMode) -> mln_map::MapMode {
    match mode {
        MapMode::Continuous => mln_map::MapMode::Continuous,
        MapMode::Static => mln_map::MapMode::Static,
        MapMode::Tile => mln_map::MapMode::Tile,
    }
}

/// `swapBehavior(mode)` (runner.cpp).
fn swap_behavior(mode: MapMode) -> SwapBehaviour {
    if mode == MapMode::Continuous { SwapBehaviour::Flush } else { SwapBehaviour::NoFlush }
}

/// `TestRunner::Impl`.
pub struct Impl {
    pub observer: Rc<TestRunnerMapObserver>,
    pub frontend: HeadlessFrontend,
    /// `std::shared_ptr<FileSource> fileSource`: the ProxyFileSource over
    /// the cache database of ResourceOptions::cachePath.
    pub file_source: Option<Arc<dyn FileSource>>,
    pub map: Map,
}

impl Impl {
    /// `TestRunner::Impl::Impl(metadata, resourceOptions)` over
    /// `file_sources`, plus the port's options (`ImplOptions`).
    pub fn new(
        metadata: &TestMetadata,
        file_sources: &FileSourceManager,
        resource_options: &ResourceOptions,
        impl_options: &ImplOptions,
    ) -> Result<Impl, String> {
        let resource_options = mln_storage::ResourceOptions::default()
            .with_cache_path(resource_options.cache_path.clone())
            .with_api_key(resource_options.api_key.clone());
        let observer = Rc::new(TestRunnerMapObserver::default());
        let frontend = HeadlessFrontend::with_options(
            metadata.size,
            metadata.pixel_ratio,
            swap_behavior(metadata.map_mode),
            ContextMode::Unique,
            true,
        );
        if impl_options.tile_worker_codec > 0 {
            let count = impl_options.tile_worker_codec;
            let transport = SerializingTransport::new(|replies| Box::new(ThreadWorkerInstances::new(count, replies)));
            frontend.with_renderer(|renderer| renderer.set_tile_worker_transport(transport));
        }
        let file_source = file_sources.get_file_source(FileSourceType::ResourceLoader, &resource_options);
        let mut map = Map::with_resource_options(
            &frontend,
            observer.clone(),
            MapOptions::new()
                .with_map_mode(map_mode(metadata.map_mode))
                .with_size(metadata.size)
                .with_pixel_ratio(metadata.pixel_ratio)
                .with_cross_source_collisions(metadata.cross_source_collisions),
            file_sources,
            &resource_options,
        )?;
        if let Some(max_pitch) = metadata.max_pitch {
            map.set_bounds(&BoundOptions::default().with_max_pitch(f64::from(max_pitch)))?;
        }
        Ok(Impl { observer, frontend, file_source, map })
    }
}

impl TestRunnerImpl for Impl {
    fn reset_context(&mut self, metadata: &TestMetadata) -> Result<(), MapError> {
        self.frontend.with_renderer(|renderer| renderer.clear_data());
        self.frontend.set_size(metadata.size);
        self.map.set_size(metadata.size).map_err(MapError::Exception)?;
        self.map.set_projection_mode(
            &ProjectionMode::default()
                .with_axonometric(metadata.axonometric)
                .with_x_skew(metadata.x_skew)
                .with_y_skew(metadata.y_skew),
        );
        self.map.set_debug(mln_map::MapDebugOptions(metadata.debug.0));
        style_ops::load_json(&mut self.map.get_style_mut(), &metadata.document_json);
        Ok(())
    }

    fn jump_to_default_camera(&mut self) -> Result<(), MapError> {
        let camera = style_ops::default_camera(&self.map.get_style());
        self.map.jump_to(&camera);
        Ok(())
    }

    fn render(&mut self) -> Result<PremultipliedImage, String> {
        self.frontend.render(&mut self.map).map(|result| result.image)
    }

    fn dump_debug_logs(&self) {
        self.map.dump_debug_logs();
        self.frontend.with_renderer(|renderer| renderer.dump_debug_logs());
    }

    // query: tile mode.
    fn default_camera(&mut self) -> Result<CameraOptions, MapError> {
        let camera = style_ops::default_camera(&self.map.get_style());
        Ok(CameraOptions {
            center: camera.center,
            center_altitude: camera.center_altitude,
            zoom: camera.zoom,
            bearing: camera.bearing,
            pitch: camera.pitch,
            roll: camera.roll,
        })
    }

    fn tile_cover_for_camera(
        &mut self,
        camera: &CameraOptions,
        zoom: u8,
    ) -> Result<Vec<crate::runner::TileId>, MapError> {
        let camera = mln_map::CameraOptions::default()
            .with_center(camera.center)
            .with_center_altitude(camera.center_altitude)
            .with_zoom(camera.zoom)
            .with_bearing(camera.bearing)
            .with_pitch(camera.pitch)
            .with_roll(camera.roll);
        let bounds = self.map.lat_lng_bounds_for_camera(&camera).map_err(MapError::Exception)?;
        Ok(mln_renderer::util::tile_cover::tile_cover_bounds(&bounds, zoom)
            .into_iter()
            .map(|id| crate::runner::TileId { z: id.canonical.z, x: id.canonical.x, y: id.canonical.y, wrap: id.wrap })
            .collect())
    }

    fn collect_placed_symbol_data(&mut self, enable: bool) {
        self.frontend.with_renderer(|renderer| renderer.collect_placed_symbol_data(enable));
    }

    fn get_placed_symbols_data(&mut self) -> Vec<crate::runner::PlacedSymbol> {
        let to_box = |b: &mln_util::grid_index::BBox| [b.min.x, b.min.y, b.max.x, b.max.y];
        self.frontend.with_renderer(|renderer| {
            renderer
                .get_placed_symbols_data()
                .iter()
                .map(|data| crate::runner::PlacedSymbol {
                    key: data.key.clone(),
                    text_collision_box: data.text_collision_box.as_ref().map(to_box),
                    icon_collision_box: data.icon_collision_box.as_ref().map(to_box),
                    text_placed: data.text_placed,
                    icon_placed: data.icon_placed,
                    intersects_tile_border: data.intersects_tile_border,
                    viewport_padding: data.viewport_padding,
                    layer: data.layer.clone(),
                })
                .collect()
        })
    }

    fn jump_to(&mut self, camera: &CameraOptions) -> Result<(), MapError> {
        let camera = mln_map::CameraOptions::default()
            .with_center(camera.center)
            .with_center_altitude(camera.center_altitude)
            .with_zoom(camera.zoom)
            .with_bearing(camera.bearing)
            .with_pitch(camera.pitch)
            .with_roll(camera.roll);
        self.map.jump_to(&camera);
        Ok(())
    }

    fn load_style_json(&mut self, json: &str) -> Result<(), MapError> {
        style_ops::load_json(&mut self.map.get_style_mut(), json);
        Ok(())
    }

    fn load_style_url(&mut self, url: &str) -> Result<(), MapError> {
        style_ops::load_url(&mut self.map.get_style_mut(), url);
        Ok(())
    }

    fn set_layer_property(&mut self, layer: &str, name: &str, value: &serde_json::Value) -> Result<(), MapError> {
        style_ops::set_layer_property(&mut self.map.get_style_mut(), layer, name, value)
    }

    fn set_layer_zoom_range(&mut self, layer: &str, min: f32, max: f32) -> Result<(), MapError> {
        style_ops::set_layer_zoom_range(&mut self.map.get_style_mut(), layer, min, max)
    }

    fn add_layer(&mut self, json: &str) -> Result<(), MapError> {
        style_ops::add_layer(&mut self.map.get_style_mut(), json)
    }

    fn remove_layer(&mut self, id: &str) -> Result<bool, MapError> {
        Ok(style_ops::remove_layer(&mut self.map.get_style_mut(), id))
    }

    fn set_layer_filter(&mut self, layer: &str, filter: &serde_json::Value) -> Result<(), MapError> {
        style_ops::set_layer_filter(&mut self.map.get_style_mut(), layer, filter)
    }

    fn set_global_state_property(&mut self, name: &str, value: &feature::Value) -> Result<(), MapError> {
        style_ops::set_global_state_property(&mut self.map.get_style_mut(), name, value)
    }

    fn set_feature_state(
        &mut self,
        source_id: &str,
        source_layer: Option<&str>,
        feature_id: &str,
        state: &feature::PropertyMap,
    ) -> Result<bool, MapError> {
        if self.frontend.render(&mut self.map).is_err() {
            return Ok(false);
        }
        self.frontend.with_renderer(|renderer| renderer.set_feature_state(source_id, source_layer, feature_id, state));
        Ok(true)
    }

    fn get_feature_state(
        &mut self,
        source_id: &str,
        source_layer: Option<&str>,
        feature_id: &str,
    ) -> Result<bool, MapError> {
        if self.frontend.render(&mut self.map).is_err() {
            return Ok(false);
        }
        self.frontend.with_renderer(|renderer| {
            let _ = renderer.get_feature_state(source_id, source_layer, feature_id);
        });
        Ok(true)
    }

    fn remove_feature_state(
        &mut self,
        source_id: &str,
        source_layer: Option<&str>,
        feature_id: Option<&str>,
        state_key: Option<&str>,
    ) -> Result<bool, MapError> {
        if self.frontend.render(&mut self.map).is_err() {
            return Ok(false);
        }
        self.frontend
            .with_renderer(|renderer| renderer.remove_feature_state(source_id, source_layer, feature_id, state_key));
        Ok(true)
    }

    // query
    fn query_rendered_features(
        &mut self,
        geometry: &QueryGeometry,
        options: &crate::metadata::RenderedQueryOptions,
    ) -> Result<Vec<mln_util::feature::Feature>, MapError> {
        // The parser's `convert<style::Filter>(filter)` (C++ asserts it
        // converts).
        let filter = match &options.filter {
            Some(json) => {
                let mut error = mln_conversion::Error::default();
                match mln_style::conversion::convert::<mln_style::filter::Filter>(
                    &mln_conversion::Convertible::from(json),
                    &mut error,
                ) {
                    Some(filter) => Some(filter),
                    None => return Err(MapError::Conversion(error.message)),
                }
            }
            None => None,
        };
        let options = mln_renderer::query::RenderedQueryOptions::new(options.layer_ids.clone(), filter);
        self.frontend
            .with_renderer(|renderer| match geometry {
                QueryGeometry::Point(point) => renderer.query_rendered_features_at_point(point, &options),
                QueryGeometry::Box(screen_box) => {
                    renderer.query_rendered_features_in_box(&screen_box.min, &screen_box.max, &options)
                }
            })
            .map_err(|e| MapError::Exception(e.to_string()))
    }

    // conformance: setTileLod* (Map::setTileLod*, glfw ported them).
    fn set_tile_lod_min_radius(&mut self, value: f64) -> Result<(), MapError> {
        self.map.set_tile_lod_min_radius(value);
        Ok(())
    }

    fn set_tile_lod_scale(&mut self, value: f64) -> Result<(), MapError> {
        self.map.set_tile_lod_scale(value);
        Ok(())
    }

    fn set_tile_lod_pitch_threshold(&mut self, value: f64) -> Result<(), MapError> {
        self.map.set_tile_lod_pitch_threshold(value);
        Ok(())
    }

    fn set_tile_lod_zoom_shift(&mut self, value: f64) -> Result<(), MapError> {
        self.map.set_tile_lod_zoom_shift(value);
        Ok(())
    }

    fn set_tile_lod_mode(&mut self, mode: crate::parser::TileLodMode) -> Result<(), MapError> {
        self.map.set_tile_lod_mode(match mode {
            crate::parser::TileLodMode::Default => mln_map::TileLodMode::Default,
            crate::parser::TileLodMode::Distance => mln_map::TileLodMode::Distance,
        });
        Ok(())
    }

    // query: the remaining runner operations.
    fn set_light(&mut self, json: &str) -> Result<(), MapError> {
        style_ops::set_light(&mut self.map.get_style_mut(), json)
    }

    fn add_source(&mut self, id: &str, json: &str) -> Result<(), MapError> {
        style_ops::add_source(&mut self.map.get_style_mut(), id, json)
    }

    fn remove_source(&mut self, id: &str) -> Result<bool, MapError> {
        Ok(style_ops::remove_source(&mut self.map.get_style_mut(), id))
    }

    fn add_image(&mut self, name: &str, image_path: &str, pixel_ratio: f32, sdf: bool) -> Result<(), MapError> {
        // `ctx.getFileSource().request(Resource::image("mapbox://render-tests/" +
        // imagePath), ...)`, running the run loop until it completes.
        let Some(file_source) = &self.file_source else {
            return Err(MapError::LoadFailed);
        };
        let maybe_image: Rc<std::cell::RefCell<Option<Vec<u8>>>> = Rc::default();
        let request_completed = Rc::new(Cell::new(false));
        let _req = {
            let maybe_image = maybe_image.clone();
            let request_completed = request_completed.clone();
            file_source.request(
                &mln_storage::Resource::image(&format!("mapbox://render-tests/{image_path}")),
                Box::new(move |response: mln_storage::Response| {
                    if let Some(data) = &response.data {
                        *maybe_image.borrow_mut() = Some(data.to_vec());
                    }
                    request_completed.set(true);
                }),
            )
        };

        while !request_completed.get() {
            RunLoop::get().run_once();
        }

        let Some(bytes) = maybe_image.borrow_mut().take() else {
            return Err(MapError::LoadFailed);
        };

        // C++ `decodeImage` and the `style::Image` constructor throw (not
        // caught by the runner).
        let image = mln_image::decode_image(&bytes).map_err(MapError::Exception)?;
        let image = mln_style::image::Impl::new(
            name.to_string(),
            image,
            pixel_ratio,
            sdf,
            Vec::new(),
            Vec::new(),
            None,
            None,
            None,
        )
        .map_err(|e| MapError::Exception(e.to_string()))?;
        self.map.get_style_mut().add_image(Arc::new(image));
        Ok(())
    }

    fn remove_image(&mut self, name: &str) -> Result<(), MapError> {
        self.map.get_style_mut().remove_image(name);
        Ok(())
    }

    fn sleep(&mut self, duration: std::time::Duration) -> Result<(), MapError> {
        // util::Timer sleepTimer; sleepTimer.start(duration, zero, ...);
        // while (sleeping) RunLoop::Get()->runOnce();
        let sleeping = Rc::new(Cell::new(true));
        let sleep_timer = mln_actor::Timer::new();
        {
            let sleeping = sleeping.clone();
            sleep_timer.start(
                mln_util::chrono::Duration::from_millis(duration.as_millis() as i64),
                mln_util::chrono::Duration::zero(),
                move || sleeping.set(false),
            );
        }
        while sleeping.get() {
            RunLoop::get().run_once();
        }
        Ok(())
    }
}

/// The factory `TestRunner::run` uses for `maps[key]`. Creates the thread's
/// `util::RunLoop` first, as `runRenderTests` does, and keeps it alive. Its
/// maps share one set of [`proxy_file_sources`].
pub fn factory() -> ImplFactory {
    let run_loop = RunLoop::new();
    let file_sources = proxy_file_sources();
    Box::new(move |metadata: &TestMetadata, resource_options: &ResourceOptions, impl_options: &ImplOptions| {
        let _ = &run_loop;
        Ok(Box::new(Impl::new(metadata, &file_sources, resource_options, impl_options)?) as Box<dyn TestRunnerImpl>)
    })
}
