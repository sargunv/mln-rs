// Ports the render-source tests of test/style/source.test.cpp (the ones that
// need a RenderSource; the style-side ones are in mln_style's tests).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use geojson::GeoJSON;
use mln_actor::{RunLoop, TaggedScheduler, Timer, scheduler};
use mln_map::transform::{NullTransformObserver, Transform};
use mln_map_types::{
    CameraOptions, ConstrainMode, MapDebugOptions, MapMode, TileLodMode, TransformState, ViewportMode,
};
use mln_renderer::render_source::{self as render_source, RenderSource, RenderSourceObserver};
use mln_renderer::sources::render_geojson_source::RenderGeoJSONSource;
use mln_renderer::tile_parameters::TileParameters;
use mln_storage::response::{Error as ResponseError, Reason};
use mln_storage::stub_file_source::{ResponseType, StubFileSource};
use mln_storage::{AsyncRequest, Callback, FileSource, Kind, Resource, ResourceOptions, Response};
use mln_style::layer::{Layer, static_immutable_cast};
use mln_style::layer_properties::LayerProperties;
use mln_style::layers::circle_layer::Impl as CircleLayerImpl;
use mln_style::layers::hillshade_layer::Impl as HillshadeLayerImpl;
use mln_style::layers::line_layer::Impl as LineLayerImpl;
use mln_style::layers::raster_layer::Impl as RasterLayerImpl;
use mln_style::layers::{
    CircleLayer, CircleLayerProperties, HillshadeLayer, HillshadeLayerProperties, LineLayer, LineLayerProperties,
    RasterLayer, RasterLayerProperties,
};
use mln_style::observer::SourceObserver;
use mln_style::source::{self, Source};
use mln_style::sources::custom_geometry_source::{CustomGeometrySource, Options as CustomGeometrySourceOptions};
use mln_style::sources::custom_vector_source::{
    CustomVectorSource, Options as CustomVectorSourceOptions, TileDataFormat,
};
use mln_style::sources::geojson_source::{GeoJSONOptions, GeoJSONSource, create_geojson_data};
use mln_style::sources::geojson_source_impl;
use mln_style::sources::tile_source::{RasterDEMSource, RasterSource, UrlOrTileset, VectorSource};
use mln_style_types::SourceType;
use mln_tile_id::{CanonicalTileID, OverscaledTileID};
use mln_util::chrono::{Duration, milliseconds};
use mln_util::exception::{ExceptionPtr, RuntimeError};
use mln_util::geo::LatLng;
use mln_util::identity::SimpleIdentity;
use mln_util::immutable::Immutable;
use mln_util::logging::{Event, EventSeverity, Log, Observer as LogObserver};
use mln_util::size::Size;
use mln_util::tileset::{Tileset, VectorEncoding};

/// `SourceTest`.
struct SourceTest {
    run_loop: RunLoop,
    transform: Transform,
    thread_pool: TaggedScheduler,
}

/// `SourceTest::tileParameters(mapMode)`, from a transform state the caller
/// owns (the C++ fixture keeps `transformState` as a member).
fn tile_parameters<'a>(
    transform_state: &'a TransformState,
    thread_pool: &TaggedScheduler,
    map_mode: MapMode,
    file_source: Option<Arc<dyn FileSource>>,
) -> TileParameters<'a> {
    TileParameters {
        pixel_ratio: 1.0,
        debug_options: MapDebugOptions::NO_DEBUG,
        transform_state,
        file_source,
        mode: map_mode,
        prefetch_zoom_delta: 0,
        thread_pool: thread_pool.clone(),
        tile_lod_min_radius: 3.0,
        tile_lod_scale: 1.0,
        tile_lod_pitch_threshold: (60.0 / 180.0) * std::f64::consts::PI,
        tile_lod_zoom_shift: 0.0,
        tile_lod_mode: TileLodMode::Default,
        global_state: None,
        is_update_synchronous: false,
        capture_rendered_features: false,
        image_manager: mln_renderer::image_manager::ImageManager::create(),
        glyph_manager: mln_renderer::text::glyph_manager::GlyphManager::new(None),
        dynamic_texture_atlas: std::sync::Arc::new(mln_renderer::gfx::dynamic_texture_atlas::DynamicTextureAtlas::new()),
        tile_worker_transport: Rc::new(mln_renderer::tile::tile_worker_transport::InProcessTransport::new(
            thread_pool.clone(),
        )),
    }
}

impl SourceTest {
    fn new() -> Self {
        let mut transform =
            Transform::new(Rc::new(NullTransformObserver), ConstrainMode::HeightOnly, ViewportMode::Default);
        transform.resize(Size::new(512, 512)).unwrap();
        transform.jump_to(&CameraOptions { center: Some(LatLng::default()), zoom: Some(0.0), ..Default::default() });
        SourceTest {
            run_loop: RunLoop::new(),
            transform,
            thread_pool: TaggedScheduler::new(scheduler::get_background(), SimpleIdentity::new()),
        }
    }

    fn tile_parameters(&self, map_mode: MapMode) -> TileParameters<'_> {
        self.tile_parameters_with(map_mode, None)
    }

    fn tile_parameters_with(&self, map_mode: MapMode, file_source: Option<Arc<dyn FileSource>>) -> TileParameters<'_> {
        tile_parameters(self.transform.get_state(), &self.thread_pool, map_mode, file_source)
    }

    /// `test.end()`, usable from `'static` callbacks.
    fn end_handle(&self) -> impl Fn() + Clone + 'static {
        let handle = self.run_loop.handle();
        move || handle.stop()
    }

    /// `test.run()`.
    fn run(&self) {
        self.run_loop.run();
    }
}

impl Drop for SourceTest {
    fn drop(&mut self) {
        self.thread_pool.wait_for_empty();
    }
}

/// `StubRenderSourceObserver` (test/src/mln/test/stub_render_source_observer.hpp).
#[derive(Default)]
struct StubRenderSourceObserver {
    tile_changed: RefCell<Option<Box<dyn Fn(&str, &OverscaledTileID)>>>,
    tile_error: RefCell<Option<Box<dyn Fn(&str, &OverscaledTileID, ExceptionPtr)>>>,
}

impl RenderSourceObserver for StubRenderSourceObserver {
    fn on_tile_changed(&self, source_id: &str, tile_id: &OverscaledTileID) {
        if let Some(f) = &*self.tile_changed.borrow() {
            f(source_id, tile_id);
        }
    }

    fn on_tile_error(&self, source_id: &str, tile_id: &OverscaledTileID, error: ExceptionPtr) {
        if let Some(f) = &*self.tile_error.borrow() {
            f(source_id, tile_id, error);
        }
    }
}

/// `StubStyleObserver` (test/src/mln/test/stub_style_observer.hpp), the
/// SourceObserver half.
#[derive(Default)]
struct StubStyleObserver {
    source_changed: RefCell<Option<Box<dyn Fn(&dyn Source)>>>,
}

impl SourceObserver for StubStyleObserver {
    fn on_source_changed(&self, source: &dyn Source) {
        if let Some(f) = &*self.source_changed.borrow() {
            f(source);
        }
    }
}

/// The three tiled source types the C++ tests repeat each case for.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Tiled {
    Raster,
    RasterDEM,
    Vector,
}

impl Tiled {
    /// `RasterLayer` / `HillshadeLayer` / `LineLayer` (source-layer "water")
    /// "id" on "source", as layer properties.
    fn layers(self) -> Vec<Immutable<dyn LayerProperties>> {
        let properties: Immutable<dyn LayerProperties> = match self {
            Tiled::Raster => {
                let layer = RasterLayer::new("id", "source");
                Arc::new(RasterLayerProperties::new(static_immutable_cast::<RasterLayerImpl>(&layer.base_impl())))
            }
            Tiled::RasterDEM => {
                let layer = HillshadeLayer::new("id", "source");
                Arc::new(HillshadeLayerProperties::new(static_immutable_cast::<HillshadeLayerImpl>(&layer.base_impl())))
            }
            Tiled::Vector => {
                let mut layer = LineLayer::new("id", "source");
                layer.set_source_layer("water");
                Arc::new(LineLayerProperties::new(static_immutable_cast::<LineLayerImpl>(&layer.base_impl())))
            }
        };
        vec![properties]
    }

    /// `RasterSource("source", tileset, 512)` / `RasterDEMSource(...)` /
    /// `VectorSource("source", tileset)` with `tileset.tiles = {"tiles"}`.
    fn source(self, url_or_tileset: UrlOrTileset) -> Box<dyn Source> {
        match self {
            Tiled::Raster => Box::new(RasterSource::new("source", url_or_tileset, 512, SourceType::Raster, None, None)),
            Tiled::RasterDEM => Box::new(RasterDEMSource::new("source", url_or_tileset, 512, None)),
            Tiled::Vector => Box::new(VectorSource::new("source", url_or_tileset, None, None, VectorEncoding::Mapbox)),
        }
    }

    fn source_type(self) -> SourceType {
        match self {
            Tiled::Raster => SourceType::Raster,
            Tiled::RasterDEM => SourceType::RasterDEM,
            Tiled::Vector => SourceType::Vector,
        }
    }
}

fn tiles_tileset() -> UrlOrTileset {
    UrlOrTileset::Tileset(Tileset { tiles: vec!["tiles".to_owned()], ..Default::default() })
}

/// The shared body of the `<Type>Tile<Case>` tests: a source of `kind`
/// whose tiles get `tile_response`, a render source observed by `observer`,
/// one update, then `test.run()`. Returns the render source's type.
fn run_tile_case(
    test: &SourceTest,
    kind: Tiled,
    file_source: Arc<StubFileSource>,
    observer: Rc<StubRenderSourceObserver>,
) -> SourceType {
    let layers = kind.layers();
    let source = kind.source(tiles_tileset());
    source.load_description(&*file_source);

    let mut render_source = render_source::create(&source.base_impl(), &test.thread_pool).unwrap();
    render_source.set_observer(observer);
    let fs: Arc<dyn FileSource> = file_source;
    render_source.update(
        source.base_impl(),
        &layers,
        true,
        true,
        &test.tile_parameters_with(MapMode::Continuous, Some(fs)),
    );

    test.run();
    render_source.base().base_impl.type_
}

/// `Source.<Type>TileEmpty`.
fn tile_empty(kind: Tiled) {
    let test = SourceTest::new();
    let file_source = Arc::new(StubFileSource::new(ResponseType::Asynchronous));
    file_source.set_response(Kind::Tile, |_resource| {
        let mut response = Response::new();
        response.no_content = true;
        Some(response)
    });

    let observer = Rc::new(StubRenderSourceObserver::default());
    let changed = Rc::new(RefCell::new(false));
    {
        let changed = changed.clone();
        let end = test.end_handle();
        *observer.tile_changed.borrow_mut() = Some(Box::new(move |source_id, _tile_id| {
            assert_eq!("source", source_id);
            *changed.borrow_mut() = true;
            end();
        }));
    }
    *observer.tile_error.borrow_mut() = Some(Box::new(|_, _, _| panic!("Should never be called")));

    run_tile_case(&test, kind, file_source, observer);
    assert!(*changed.borrow());
}

/// `Source.<Type>TileFail`.
fn tile_fail(kind: Tiled) {
    let test = SourceTest::new();
    let file_source = Arc::new(StubFileSource::new(ResponseType::Asynchronous));
    file_source.set_response(Kind::Tile, |_resource| {
        let mut response = Response::new();
        response.error = Some(Box::new(ResponseError::new(Reason::Other, "Failed by the test case")));
        Some(response)
    });

    let observer = Rc::new(StubRenderSourceObserver::default());
    let failed = Rc::new(RefCell::new(false));
    {
        let failed = failed.clone();
        let end = test.end_handle();
        *observer.tile_error.borrow_mut() = Some(Box::new(move |source_id, tile_id, error| {
            assert_eq!("source", source_id);
            assert_eq!(OverscaledTileID::from_zxy(0, 0, 0), *tile_id);
            assert_eq!("Failed by the test case", error.to_string());
            *failed.borrow_mut() = true;
            end();
        }));
    }

    // C++ checks `source_.baseImpl->type` in the callback; the port's
    // observer gets the source id, so the type is checked here.
    assert_eq!(kind.source_type(), run_tile_case(&test, kind, file_source, observer));
    assert!(*failed.borrow());
}

/// `Source.<Type>TileCorrupt`.
fn tile_corrupt(kind: Tiled) {
    let test = SourceTest::new();
    let file_source = Arc::new(StubFileSource::new(ResponseType::Asynchronous));
    file_source.set_response(Kind::Tile, |_resource| {
        let mut response = Response::new();
        response.data = Some(Arc::new(b"CORRUPTED".to_vec()));
        Some(response)
    });

    let observer = Rc::new(StubRenderSourceObserver::default());
    let failed = Rc::new(RefCell::new(false));
    {
        let failed = failed.clone();
        let end = test.end_handle();
        *observer.tile_error.borrow_mut() = Some(Box::new(move |source_id, tile_id, error| {
            assert_eq!("source", source_id);
            assert_eq!(OverscaledTileID::from_zxy(0, 0, 0), *tile_id);
            if kind == Tiled::Vector {
                assert_eq!(error.to_string(), "unknown pbf field type exception");
            } else {
                // Not asserting on platform-specific error text.
                assert!(!error.to_string().is_empty());
            }
            *failed.borrow_mut() = true;
            end();
        }));
    }

    assert_eq!(kind.source_type(), run_tile_case(&test, kind, file_source, observer));
    assert!(*failed.borrow());
}

/// `Source.<Type>TileCancel`.
fn tile_cancel(kind: Tiled) {
    let test = SourceTest::new();
    let file_source = Arc::new(StubFileSource::new(ResponseType::Asynchronous));
    let requested = Rc::new(RefCell::new(false));
    {
        let requested = requested.clone();
        let end = test.end_handle();
        file_source.set_response(Kind::Tile, move |_resource| {
            *requested.borrow_mut() = true;
            end();
            None
        });
    }

    let observer = Rc::new(StubRenderSourceObserver::default());
    *observer.tile_changed.borrow_mut() = Some(Box::new(|_, _| panic!("Should never be called")));
    *observer.tile_error.borrow_mut() = Some(Box::new(|_, _, _| panic!("Should never be called")));

    run_tile_case(&test, kind, file_source, observer);
    assert!(*requested.borrow());
}

/// A file source whose tile requests never answer, as tiles in flight: it
/// records each request's tile and each cancellation.
#[derive(Default)]
struct PendingTileSource {
    requested: Arc<Mutex<Vec<(i8, i32, i32)>>>,
    cancelled: Arc<Mutex<Vec<(i8, i32, i32)>>>,
}

struct PendingTileRequest {
    tile: (i8, i32, i32),
    cancelled: Arc<Mutex<Vec<(i8, i32, i32)>>>,
}

impl AsyncRequest for PendingTileRequest {}

impl Drop for PendingTileRequest {
    fn drop(&mut self) {
        self.cancelled.lock().unwrap().push(self.tile);
    }
}

impl FileSource for PendingTileSource {
    fn request(&self, resource: &Resource, _callback: Callback) -> Box<dyn AsyncRequest> {
        let data = resource.tile_data.as_ref().expect("only tiles are requested");
        let tile = (data.z, data.x, data.y);
        self.requested.lock().unwrap().push(tile);
        Box::new(PendingTileRequest { tile, cancelled: self.cancelled.clone() })
    }

    fn can_request(&self, _resource: &Resource) -> bool {
        true
    }

    fn set_resource_options(&self, _options: ResourceOptions) {}

    fn get_resource_options(&self) -> ResourceOptions {
        ResourceOptions::default()
    }
}

/// A tile that leaves the view while it loads goes on loading, and is the
/// one the view gets back when it turns back, as a map rotated back and
/// forth does at its corners: its request is neither cancelled nor made
/// again.
#[test]
fn tile_out_of_view_keeps_loading() {
    let mut test = SourceTest::new();
    test.transform.resize(Size::new(1024, 128)).unwrap();
    let file_source = Arc::new(PendingTileSource::default());
    let (requested, cancelled) = (file_source.requested.clone(), file_source.cancelled.clone());
    let layers = Tiled::Vector.layers();
    let source = Tiled::Vector.source(tiles_tileset());
    source.load_description(&*file_source);
    let mut render_source = render_source::create(&source.base_impl(), &test.thread_pool).unwrap();
    let fs: Arc<dyn FileSource> = file_source;
    // Requests made so far, after an update with the camera at `bearing`.
    let mut turn = |test: &mut SourceTest, bearing: f64, needs_relayout: bool| {
        test.transform.jump_to(&CameraOptions {
            center: Some(LatLng::new(20.0, 20.0).unwrap()),
            zoom: Some(4.0),
            bearing: Some(bearing),
            ..Default::default()
        });
        let parameters = test.tile_parameters_with(MapMode::Continuous, Some(fs.clone()));
        render_source.update(source.base_impl(), &layers, true, needs_relayout, &parameters);
        requested.lock().unwrap().clone()
    };

    let unturned = turn(&mut test, 0.0, true);
    let turned = turn(&mut test, 90.0, false);
    assert!(turned.len() > unturned.len(), "turning shows other tiles");
    let back = turn(&mut test, 0.0, false);
    assert_eq!(turned, back, "no tile is requested again");
    assert!(cancelled.lock().unwrap().is_empty(), "no request is cancelled");
}

// gtest: Source.RasterTileEmpty
#[test]
fn raster_tile_empty() {
    tile_empty(Tiled::Raster);
}

// gtest: Source.RasterDEMTileEmpty
#[test]
fn raster_dem_tile_empty() {
    tile_empty(Tiled::RasterDEM);
}

// gtest: Source.VectorTileEmpty
#[test]
fn vector_tile_empty() {
    tile_empty(Tiled::Vector);
}

// gtest: Source.RasterTileFail
#[test]
fn raster_tile_fail() {
    tile_fail(Tiled::Raster);
}

// gtest: Source.RasterDEMTileFail
#[test]
fn raster_dem_tile_fail() {
    tile_fail(Tiled::RasterDEM);
}

// gtest: Source.VectorTileFail
#[test]
fn vector_tile_fail() {
    tile_fail(Tiled::Vector);
}

// gtest: Source.RasterTileCorrupt
#[test]
fn raster_tile_corrupt() {
    tile_corrupt(Tiled::Raster);
}

// gtest: Source.RasterDEMTileCorrupt
#[test]
fn raster_dem_tile_corrupt() {
    tile_corrupt(Tiled::RasterDEM);
}

// gtest: Source.VectorTileCorrupt
#[test]
fn vector_tile_corrupt() {
    tile_corrupt(Tiled::Vector);
}

// gtest: Source.RasterTileCancel
#[test]
fn raster_tile_cancel() {
    tile_cancel(Tiled::Raster);
}

// gtest: Source.RasterDEMTileCancel
#[test]
fn raster_dem_tile_cancel() {
    tile_cancel(Tiled::RasterDEM);
}

// gtest: Source.VectorTileCancel
#[test]
fn vector_tile_cancel() {
    tile_cancel(Tiled::Vector);
}

/// `Source.<Type>TileAttribution`.
fn tile_attribution(kind: Tiled, attribution: &'static str) {
    let test = SourceTest::new();
    let layers = kind.layers();

    let file_source = Arc::new(StubFileSource::new(ResponseType::Asynchronous));
    file_source.set_response(Kind::Tile, |_resource| {
        let mut response = Response::new();
        response.no_content = true;
        Some(response)
    });
    file_source.set_response(Kind::Source, move |resource| {
        assert_eq!("url", resource.url);
        let mut response = Response::new();
        response.data = Some(Arc::new(
            format!(r#"{{ "tilejson": "2.1.0", "attribution": "{attribution}", "tiles": [ "tiles" ] }}"#).into_bytes(),
        ));
        Some(response)
    });

    let observer = Rc::new(StubStyleObserver::default());
    let changed = Rc::new(RefCell::new(false));
    {
        let changed = changed.clone();
        let end = test.end_handle();
        *observer.source_changed.borrow_mut() = Some(Box::new(move |source| {
            assert_eq!(Some(attribution.to_owned()), source.get_attribution());
            *changed.borrow_mut() = true;
            end();
        }));
    }

    let source = kind.source(UrlOrTileset::Url("url".to_owned()));
    source.set_observer(Some(observer.clone()));
    source.load_description(&*file_source);

    let mut render_source = render_source::create(&source.base_impl(), &test.thread_pool).unwrap();
    let fs: Arc<dyn FileSource> = file_source.clone();
    render_source.update(
        source.base_impl(),
        &layers,
        true,
        true,
        &test.tile_parameters_with(MapMode::Continuous, Some(fs)),
    );

    test.run();
    assert!(*changed.borrow());
}

// gtest: Source.RasterTileAttribution
#[test]
fn raster_tile_attribution() {
    let mapbox_osm = "<a href='https://www.mapbox.com/about/maps/' target='_blank'>&copy; Mapbox</a> \
                      <a href='http://www.openstreetmap.org/about/' target='_blank'>©️ OpenStreetMap</a>";
    assert!(mapbox_osm.contains("©️ OpenStreetMap"));
    tile_attribution(Tiled::Raster, mapbox_osm);
}

// gtest: Source.RasterDEMTileAttribution
#[test]
fn raster_dem_tile_attribution() {
    let mapbox = "<a href='https://www.mapbox.com/about/maps/' target='_blank'>&copy; Mapbox</a> ";
    tile_attribution(Tiled::RasterDEM, mapbox);
}

/// A loaded custom geometry source "source", a line layer for it, and its
/// render source after one update, observed by `observer`.
fn custom_geometry_case(
    test: &SourceTest,
    observer: Rc<StubRenderSourceObserver>,
) -> (CustomGeometrySource, Box<dyn RenderSource>) {
    let file_source = StubFileSource::new(ResponseType::Asynchronous);
    let source = CustomGeometrySource::new("source", CustomGeometrySourceOptions::default()).unwrap();
    source.load_description(&file_source);

    let mut layer = LineLayer::new("id", "source");
    layer.set_source_layer("water");
    let layer_properties: Immutable<dyn LayerProperties> =
        Arc::new(LineLayerProperties::new(static_immutable_cast::<LineLayerImpl>(&layer.base_impl())));
    let layers = vec![layer_properties];

    let mut render_source = render_source::create(&source.base_impl(), &test.thread_pool).unwrap();
    render_source.set_observer(observer);
    render_source.update(source.base_impl(), &layers, true, true, &test.tile_parameters(MapMode::Continuous));
    (source, render_source)
}

// gtest: Source.CustomGeometrySourceSetTileData
#[test]
fn custom_geometry_source_set_tile_data() {
    let test = SourceTest::new();
    let observer = Rc::new(StubRenderSourceObserver::default());
    {
        let end = test.end_handle();
        *observer.tile_changed.borrow_mut() = Some(Box::new(move |source_id, _tile_id| {
            assert_eq!("source", source_id);
            end();
        }));
    }
    *observer.tile_error.borrow_mut() = Some(Box::new(|_, _, _| panic!("Should never be called")));

    let (source, _render_source) = custom_geometry_case(&test, observer);

    let handle = source.handle();
    test.run_loop.invoke(move || {
        // Set Tile Data
        handle.set_tile_data(CanonicalTileID::new(0, 0, 0), GeoJSON::FeatureCollection(Vec::new()));
    });

    test.run();
}

// gtest: Source.CustomGeometrySourceSetTileError
#[test]
fn custom_geometry_source_set_tile_error() {
    let test = SourceTest::new();
    let observer = Rc::new(StubRenderSourceObserver::default());
    {
        let end = test.end_handle();
        *observer.tile_error.borrow_mut() = Some(Box::new(move |source_id, tile_id, error| {
            assert_eq!("source", source_id);
            assert_eq!(OverscaledTileID::from_zxy(0, 0, 0), *tile_id);
            assert_eq!("failed", error.to_string());
            end();
        }));
    }

    let (source, _render_source) = custom_geometry_case(&test, observer);

    let handle = source.handle();
    test.run_loop.invoke(move || {
        handle.set_tile_error(CanonicalTileID::new(0, 0, 0), RuntimeError::ptr("failed"));
    });

    test.run();
}

// Port-only: Source.CustomGeometrySourceSetTileData for a custom vector
// source.
#[test]
fn custom_vector_source_set_tile_data() {
    let test = SourceTest::new();
    let observer = Rc::new(StubRenderSourceObserver::default());
    {
        let end = test.end_handle();
        *observer.tile_changed.borrow_mut() = Some(Box::new(move |source_id, _tile_id| {
            assert_eq!("source", source_id);
            end();
        }));
    }
    *observer.tile_error.borrow_mut() = Some(Box::new(|_, _, _| panic!("Should never be called")));

    let file_source = StubFileSource::new(ResponseType::Asynchronous);
    let source = CustomVectorSource::new("source", CustomVectorSourceOptions::default());
    source.load_description(&file_source);

    let mut layer = LineLayer::new("id", "source");
    layer.set_source_layer("water");
    let layer_properties: Immutable<dyn LayerProperties> =
        Arc::new(LineLayerProperties::new(static_immutable_cast::<LineLayerImpl>(&layer.base_impl())));

    let mut render_source = render_source::create(&source.base_impl(), &test.thread_pool).unwrap();
    render_source.set_observer(observer);
    render_source.update(
        source.base_impl(),
        &[layer_properties],
        true,
        true,
        &test.tile_parameters(MapMode::Continuous),
    );

    let handle = source.handle();
    test.run_loop.invoke(move || handle.set_tile_data(CanonicalTileID::new(0, 0, 0), None, TileDataFormat::Mvt));

    test.run();
}

// gtest: Source.InvisibleSourcesTileNecessity
#[test]
#[ignore = "TODO(port): RenderTileSetSource as an overridable base (the C++ FakeTileSource overrides updateInternal and creates FakeTiles; the port has one update per source type)"]
fn invisible_sources_tile_necessity() {
    // TODO(port): body: tiles get TileNecessity::Required on a visible update,
    // Optional when needsRendering is false, Required again when visible.
}

// gtest: Source.SourceMinimumUpdateInterval
#[test]
#[ignore = "TODO(port): RenderTileSetSource as an overridable base (FakeTileSource / FakeTile record setUpdateParameters)"]
fn source_minimum_update_interval() {
    // TODO(port): body: setMinimumTileUpdateInterval replaces the impl only on
    // a change; visible updates pass the interval to the tiles, invisible
    // ones do not.
}

// gtest: Source.RenderTileSetSourceUpdate
#[test]
#[ignore = "TODO(port): RenderTileSetSource as an overridable base (FakeRenderTileSetSource mocks updateInternal)"]
fn render_tile_set_source_update() {
    // TODO(port): body: updateInternal runs on both updates, also for a
    // source whose description has not loaded yet.
}

// gtest: Source.EmptyTileset
#[test]
fn empty_tileset() {
    let test = SourceTest::new();
    let file_source = Arc::new(StubFileSource::new(ResponseType::Asynchronous));
    let source =
        VectorSource::new("source", UrlOrTileset::Tileset(Tileset::default()), None, None, VectorEncoding::Mapbox);
    source.load_description(&*file_source);
    let mut render_source = render_source::create(&source.base_impl(), &test.thread_pool).unwrap();
    let errors = observe_tile_errors(&mut *render_source);

    let layers = Tiled::Vector.layers();
    let fs: Arc<dyn FileSource> = file_source;
    let parameters = test.tile_parameters_with(MapMode::Continuous, Some(fs));

    render_source.update(source.base_impl(), &layers, true, true, &parameters);
    assert!(render_source.is_loaded());

    source.set_tiles(&["tiles".to_owned()]);
    render_source.update(source.base_impl(), &layers, true, false, &parameters);
    assert!(!render_source.is_loaded());

    source.set_tiles(&[]);
    render_source.update(source.base_impl(), &layers, true, false, &parameters);
    assert!(render_source.is_loaded());

    source.set_tiles(&["tiles".to_owned()]);
    render_source.update(source.base_impl(), &layers, true, false, &parameters);
    assert!(!render_source.is_loaded());

    assert_eq!(0, *errors.borrow());
}

// As Source.EmptyTileset, for the raster and raster-dem render sources.
#[test]
fn empty_raster_tilesets_make_no_tiles() {
    for kind in [Tiled::Raster, Tiled::RasterDEM] {
        let test = SourceTest::new();
        let file_source = Arc::new(StubFileSource::new(ResponseType::Asynchronous));
        let source = kind.source(UrlOrTileset::Tileset(Tileset::default()));
        source.load_description(&*file_source);
        let mut render_source = render_source::create(&source.base_impl(), &test.thread_pool).unwrap();
        let errors = observe_tile_errors(&mut *render_source);

        let fs: Arc<dyn FileSource> = file_source;
        let parameters = test.tile_parameters_with(MapMode::Continuous, Some(fs));
        render_source.update(source.base_impl(), &kind.layers(), true, true, &parameters);

        assert!(render_source.is_loaded());
        assert_eq!(0, *errors.borrow());
    }
}

/// Counts the tile errors `render_source` reports.
fn observe_tile_errors(render_source: &mut dyn RenderSource) -> Rc<RefCell<usize>> {
    let errors = Rc::new(RefCell::new(0));
    let observer = Rc::new(StubRenderSourceObserver::default());
    {
        let errors = errors.clone();
        *observer.tile_error.borrow_mut() = Some(Box::new(move |_, _, _| *errors.borrow_mut() += 1));
    }
    render_source.set_observer(observer);
    errors
}

// gtest: Source.GeoJSONSourceTilesAfterDataReset
#[test]
fn geojson_source_tiles_after_data_reset() {
    let test = SourceTest::new();
    let source = GeoJSONSource::with_default_options("source");
    let geojson = geojson::parse(
        r#"{"geometry": {"type": "Point", "coordinates": [1.1, 1.1]}, "type": "Feature", "properties": {}}"#,
    )
    .unwrap();
    let geojson_data =
        create_geojson_data(&geojson, scheduler::get_sequenced(), &GeoJSONOptions::default_options()).unwrap();
    source.set_geojson_data(Some(geojson_data.clone()));
    let mut render_source = RenderGeoJSONSource::new(
        source::static_immutable_cast::<geojson_source_impl::Impl>(&source.base_impl()),
        &test.thread_pool,
    );

    let layer = CircleLayer::new("id", "source");
    let layer_properties: Immutable<dyn LayerProperties> =
        Arc::new(CircleLayerProperties::new(static_immutable_cast::<CircleLayerImpl>(&layer.base_impl())));
    let layers = vec![layer_properties];

    let continuous = test.tile_parameters(MapMode::Continuous);
    render_source.update(source.base_impl(), &layers, true, true, &continuous);
    // loaded == false, means that the source contains pending tiles.
    assert!(!render_source.is_loaded());

    source.set_geojson_data(None);
    render_source.update(source.base_impl(), &layers, true, true, &continuous);
    assert!(!render_source.is_loaded()); // Tiles remain in continuous mode.

    source.set_geojson_data(Some(geojson_data));
    render_source.update(source.base_impl(), &layers, true, true, &continuous);
    assert!(!render_source.is_loaded());

    source.set_geojson_data(None);
    render_source.update(source.base_impl(), &layers, true, true, &test.tile_parameters(MapMode::Static));
    assert!(render_source.is_loaded()); // Tiles are reset in static mode.
}

/// Runs `run_loop` until a task or async handle has run.
fn run_until_work(run_loop: &RunLoop) {
    while !run_loop.run_once() {
        run_loop.wait_for_work();
    }
}

/// A circle layer "id" on "source", as layer properties.
fn circle_layers() -> Vec<Immutable<dyn LayerProperties>> {
    let layer = CircleLayer::new("id", "source");
    vec![Arc::new(CircleLayerProperties::new(static_immutable_cast::<CircleLayerImpl>(&layer.base_impl())))]
}

/// A GeoJSON source rendered with a circle layer. Its data is clustered,
/// which slices a tile within the call in either mode, so whether the tiles
/// complete within an update shows only how they lay out.
struct SynchronousUpdateSource {
    options: Immutable<GeoJSONOptions>,
    source: GeoJSONSource,
    render_source: RenderGeoJSONSource,
    layers: Vec<Immutable<dyn LayerProperties>>,
}

impl SynchronousUpdateSource {
    fn new(test: &SourceTest, synchronous_update: bool) -> Self {
        let options: Immutable<GeoJSONOptions> =
            Arc::new(GeoJSONOptions { cluster: true, synchronous_update, ..Default::default() });
        let source = GeoJSONSource::new("source", options.clone());
        let render_source = RenderGeoJSONSource::new(
            source::static_immutable_cast::<geojson_source_impl::Impl>(&source.base_impl()),
            &test.thread_pool,
        );
        SynchronousUpdateSource { options, source, render_source, layers: circle_layers() }
    }

    /// Sets new data and updates the render source, as a frame would.
    fn update(&mut self, test: &SourceTest) {
        let geojson = geojson::parse(
            r#"{"type": "FeatureCollection", "features": [{"type": "Feature", "properties": {},
                "geometry": {"type": "Point", "coordinates": [1.1, 1.1]}}]}"#,
        )
        .unwrap();
        self.source
            .set_geojson_data(Some(create_geojson_data(&geojson, scheduler::get_sequenced(), &self.options).unwrap()));
        let mut parameters = test.tile_parameters(MapMode::Continuous);
        parameters.is_update_synchronous = self.source.is_update_synchronous();
        self.render_source.update(self.source.base_impl(), &self.layers, true, true, &parameters);
    }
}

// gtest: Source.GeoJSONSourceSynchronousUpdateOverrideAppliesToExistingTiles
#[test]
fn geojson_source_synchronous_update_override_applies_to_existing_tiles() {
    let test = SourceTest::new();

    let mut synchronous = SynchronousUpdateSource::new(&test, true);
    synchronous.update(&test);
    assert!(synchronous.render_source.is_loaded()); // The tiles complete within the update.
    synchronous.source.set_override_synchronous_update(false);
    synchronous.update(&test);
    assert!(!synchronous.render_source.is_loaded()); // The tiles now lay out in the background.

    let mut asynchronous = SynchronousUpdateSource::new(&test, false);
    asynchronous.update(&test);
    assert!(!asynchronous.render_source.is_loaded());
    while !asynchronous.render_source.is_loaded() {
        run_until_work(&test.run_loop);
    }
    asynchronous.source.set_override_synchronous_update(true);
    asynchronous.update(&test);
    assert!(asynchronous.render_source.is_loaded()); // The tiles now complete within the update.
}

/// Holds the tasks it is given until the test runs them.
#[derive(Default)]
struct HeldScheduler {
    tasks: Mutex<Vec<scheduler::Task>>,
}

impl scheduler::Scheduler for HeldScheduler {
    fn schedule(&self, task: scheduler::Task) {
        self.tasks.lock().unwrap().push(task);
    }

    fn wait_for_empty(&self) {}
}

// gtest: Source.GeoJSONSourceSlicesInlineWhileABackgroundSliceRuns
#[test]
fn geojson_source_slices_inline_while_a_background_slice_runs() {
    let mut test = SourceTest::new();
    // At zoom 3 the index slices its tiles on demand, which fills its tile cache.
    test.transform.jump_to(&CameraOptions { center: Some(LatLng::default()), zoom: Some(3.0), ..Default::default() });

    // Unclustered data slices on this scheduler while the source is asynchronous.
    let slicer = Arc::new(HeldScheduler::default());
    let source = GeoJSONSource::with_default_options("source");
    let geojson = geojson::parse(
        r#"{"type": "Polygon", "coordinates": [[[-10, -10], [10, -10], [10, 10], [-10, 10], [-10, -10]]]}"#,
    )
    .unwrap();
    source.set_geojson_data(Some(
        create_geojson_data(&geojson, slicer.clone(), &GeoJSONOptions::default_options()).unwrap(),
    ));
    let mut render_source = RenderGeoJSONSource::new(
        source::static_immutable_cast::<geojson_source_impl::Impl>(&source.base_impl()),
        &test.thread_pool,
    );
    let layers = circle_layers();
    let update = |render_source: &mut RenderGeoJSONSource| {
        let mut parameters = test.tile_parameters(MapMode::Continuous);
        parameters.is_update_synchronous = source.is_update_synchronous();
        render_source.update(source.base_impl(), &layers, true, true, &parameters);
    };

    update(&mut render_source);
    assert!(!slicer.tasks.lock().unwrap().is_empty());
    assert!(!render_source.is_loaded());

    // The queued slices run in the background while the source turns
    // synchronous and recreates its tiles, which slice the same index inline.
    source.set_override_synchronous_update(true);
    let tasks = std::mem::take(&mut *slicer.tasks.lock().unwrap());
    let background = std::thread::spawn(move || {
        for task in tasks {
            task();
        }
    });
    update(&mut render_source);
    background.join().unwrap();
    assert!(render_source.is_loaded()); // The new tiles complete within the update.

    // The background slices reply to tiles that no longer exist.
    run_until_work(&test.run_loop);
    assert!(render_source.is_loaded());
}

type Records = Arc<Mutex<Vec<((EventSeverity, Event, String), bool)>>>;

struct Recorder(Records);

impl LogObserver for Recorder {
    fn on_record(&self, severity: EventSeverity, event: Event, _code: i64, msg: &str) -> bool {
        self.0.lock().unwrap().push(((severity, event, msg.to_owned()), false));
        true
    }
}

/// `FixtureLog` (test/src/mln/test/fixture_log_observer.cpp) for this
/// thread.
struct FixtureLog {
    messages: Records,
}

impl FixtureLog {
    fn new() -> Self {
        let messages: Records = Arc::new(Mutex::new(Vec::new()));
        Log::set_thread_observer(Some(Box::new(Recorder(messages.clone()))));
        FixtureLog { messages }
    }

    /// `count({severity, event, -1, msg})`.
    fn count(&self, severity: EventSeverity, event: Event, msg: &str) -> usize {
        let mut count = 0;
        for (m, checked) in self.messages.lock().unwrap().iter_mut() {
            if m.0 == severity && m.1 == event && m.2 == msg {
                *checked = true;
                count += 1;
            }
        }
        count
    }

    fn unchecked_count(&self) -> usize {
        self.messages.lock().unwrap().iter().filter(|(_, checked)| !checked).count()
    }
}

impl Drop for FixtureLog {
    fn drop(&mut self) {
        Log::set_thread_observer(None);
    }
}

// gtest: Source.SetMaxParentOverscaleFactor
#[test]
fn set_max_parent_overscale_factor() {
    let mut test = SourceTest::new();
    test.transform.jump_to(&CameraOptions { center: Some(LatLng::default()), zoom: Some(8.0), ..Default::default() });
    let timer = Rc::new(Timer::new());
    let log = FixtureLog::new();

    let file_source = Arc::new(StubFileSource::new(ResponseType::Asynchronous));
    {
        let timer = timer.clone();
        let end = test.end_handle();
        file_source.set_response(Kind::Tile, move |res| {
            let z = res.tile_data.as_ref().unwrap().z;
            if z == 5 {
                let end = end.clone();
                timer.start(milliseconds(10), Duration::zero(), end);
            }
            // No tiles above zoom level 5 should be requested.
            assert!(5 <= i32::from(z), "requested z{z}");
            let mut response = Response::new();
            response.no_content = true;
            Some(response)
        });
    }

    let layers = Tiled::Raster.layers();
    let source = Tiled::Raster.source(tiles_tileset());
    assert_eq!(None, source.get_max_overscale_factor_for_parent_tiles());
    source::set_max_overscale_factor_for_parent_tiles(&*source, Some(3));
    assert_eq!(Some(3), source.get_max_overscale_factor_for_parent_tiles());
    source.load_description(&*file_source);
    let base_impl = source.base_impl();

    let observer = Rc::new(StubRenderSourceObserver::default());
    let render_source: Rc<RefCell<Box<dyn RenderSource>>> =
        Rc::new(RefCell::new(render_source::create(&base_impl, &test.thread_pool).unwrap()));
    render_source.borrow_mut().set_observer(observer.clone());
    let fs: Arc<dyn FileSource> = file_source.clone();
    render_source.borrow_mut().update(
        base_impl.clone(),
        &layers,
        true,
        true,
        &test.tile_parameters_with(MapMode::Continuous, Some(fs.clone())),
    );

    {
        let render_source = Rc::downgrade(&render_source);
        let state = test.transform.get_state().clone();
        let thread_pool = test.thread_pool.clone();
        *observer.tile_changed.borrow_mut() = Some(Box::new(move |_, _| {
            if let Some(render_source) = render_source.upgrade() {
                let parameters = tile_parameters(&state, &thread_pool, MapMode::Continuous, Some(fs.clone()));
                render_source.borrow_mut().update(base_impl.clone(), &layers, true, true, &parameters);
            }
        }));
    }

    test.run();

    assert_eq!(
        1,
        log.count(EventSeverity::Warning, Event::Style, "Parent tile overscale factor will cap prefetch delta to 3")
    );
    assert_eq!(0, log.unchecked_count());
}

// gtest: Source.RasterDEMTileRoundsCoveringZoom
#[test]
fn raster_dem_tile_rounds_covering_zoom() {
    let mut test = SourceTest::new();
    test.transform.jump_to(&CameraOptions { center: Some(LatLng::default()), zoom: Some(4.6), ..Default::default() });

    let requested_zooms = Rc::new(RefCell::new(Vec::<i8>::new()));
    let file_source = Arc::new(StubFileSource::new(ResponseType::Asynchronous));
    {
        let requested_zooms = requested_zooms.clone();
        file_source.set_response(Kind::Tile, move |res| {
            requested_zooms.borrow_mut().push(res.tile_data.as_ref().unwrap().z);
            let mut response = Response::new();
            response.no_content = true;
            Some(response)
        });
    }

    let layers = Tiled::RasterDEM.layers();
    let source = Tiled::RasterDEM.source(tiles_tileset());
    source.load_description(&*file_source);

    let observer = Rc::new(StubRenderSourceObserver::default());
    let mut render_source = render_source::create(&source.base_impl(), &test.thread_pool).unwrap();
    render_source.set_observer(observer.clone());
    let fs: Arc<dyn FileSource> = file_source.clone();
    render_source.update(
        source.base_impl(),
        &layers,
        true,
        true,
        &test.tile_parameters_with(MapMode::Continuous, Some(fs)),
    );

    {
        let end = test.end_handle();
        *observer.tile_changed.borrow_mut() = Some(Box::new(move |_, _| end()));
    }
    test.run();

    assert!(!requested_zooms.borrow().is_empty());
    for &z in requested_zooms.borrow().iter() {
        assert_eq!(5, z);
    }
}
