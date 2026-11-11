// Ports test/storage/offline_download.test.cpp (but ResourceOfflineUsageUnset,
// which renders a map: tests/offline_download_map.rs). Fixtures are read in
// place from test/fixtures/offline_download.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use feature::Value;
use mln_actor::{AsyncTask, LocalObject, RunLoop};
use mln_storage::file_source::MAX_CONCURRENT_REQUESTS_KEY;
use mln_storage::offline::{
    OfflineRegion, OfflineRegionDefinition, OfflineRegionDownloadState, OfflineRegionMetadata, OfflineRegionObserver,
    OfflineRegionStatus, OfflineTilePyramidRegionDefinition,
};
use mln_storage::offline_database::{DatabaseError, OfflineDatabase};
use mln_storage::online_file_source::OnlineFileSource;
use mln_storage::resource::{Priority, Usage};
use mln_storage::response::{Error, Reason};
use mln_storage::stub_file_source::{ResponseType, StubFileSource};
use mln_storage::{AsyncRequest, Callback, FileSource, Kind, LoadingMethod, Resource, ResourceOptions, Response};
use mln_text::glyph::NON_IDEOGRAPH_GLYPH_RANGES_PER_FONT_STACK;
use mln_util::chrono::{Timestamp, now};
use mln_util::compression::compress;
use mln_util::geo::{LatLng, LatLngBounds};
use mln_util::tile_server_options::TileServerOptions;
use mln_util::tileset::Scheme;

use super::cpp_path;
use crate::offline_download::{OfflineDownload, WeakOfflineDownload};

/// `MockObserver`.
#[derive(Default)]
struct MockObserver {
    status_changed_fn: Option<Box<dyn Fn(OfflineRegionStatus)>>,
    response_error_fn: Option<Box<dyn Fn(Error)>>,
    mapbox_tile_count_limit_exceeded_fn: Option<Box<dyn Fn(u64)>>,
}

impl OfflineRegionObserver for MockObserver {
    fn status_changed(&self, status: OfflineRegionStatus) {
        if let Some(f) = &self.status_changed_fn {
            f(status);
        }
    }

    fn response_error(&self, error: Error) {
        if let Some(f) = &self.response_error_fn {
            f(error);
        }
    }

    fn mapbox_tile_count_limit_exceeded(&self, limit: u64) {
        if let Some(f) = &self.mapbox_tile_count_limit_exceeded_fn {
            f(limit);
        }
    }
}

/// `test.response(path)`: the fixture, adding its stored size to `size`.
fn fixture_response(size: &Cell<u64>, path: &str) -> Response {
    let mut result = Response::new();
    let data = std::fs::read(cpp_path(&format!("test/fixtures/offline_download/{path}")))
        .unwrap_or_else(|e| panic!("Cannot read file {path}: {e}"));
    let uncompressed = data.len() as u64;
    let compressed = compress(&data).unwrap().len() as u64;
    size.set(size.get() + uncompressed.min(compressed));
    result.data = Some(Arc::new(data));
    result
}

/// `OfflineTest`.
struct OfflineTest {
    run_loop: RunLoop,
    file_source: Arc<StubFileSource>,
    db: Rc<RefCell<OfflineDatabase>>,
    size: Rc<Cell<u64>>,
}

impl OfflineTest {
    fn new() -> Self {
        Self::with_path(":memory:")
    }

    fn with_path(path: &str) -> Self {
        let run_loop = RunLoop::new();
        OfflineTest {
            run_loop,
            file_source: Arc::new(StubFileSource::new(ResponseType::Asynchronous)),
            db: Rc::new(RefCell::new(OfflineDatabase::new(path, &TileServerOptions::map_tiler_configuration()))),
            size: Rc::new(Cell::new(0)),
        }
    }

    fn create_region(&self) -> Result<OfflineRegion, DatabaseError> {
        let definition = OfflineTilePyramidRegionDefinition::new(
            "",
            LatLngBounds::hull(LatLng::new(1.0, 2.0).unwrap(), LatLng::new(3.0, 4.0).unwrap()),
            5.0,
            6.0,
            1.0,
            true,
        )
        .unwrap();
        let metadata = OfflineRegionMetadata::new();
        self.db.borrow_mut().create_region(&definition.into(), &metadata)
    }

    fn response(&self, path: &str) -> Response {
        fixture_response(&self.size, path)
    }

    /// `fileSource.<kind>Response = [&](const Resource&) { ... }`.
    fn respond(&self, kind: Kind, f: impl Fn(&Resource) -> Response + 'static) {
        self.file_source.set_response(kind, move |resource| Some(f(resource)));
    }

    /// A responder that returns the fixture at `path`.
    fn respond_with(&self, kind: Kind, path: &'static str, check: impl Fn(&Resource) + 'static) {
        let size = self.size.clone();
        self.respond(kind, move |resource| {
            check(resource);
            fixture_response(&size, path)
        });
    }

    fn download(&self, region_id: i64, style_url: &str, include_ideographs: bool) -> OfflineDownload {
        self.download_with_zoom(region_id, style_url, 0.0, 0.0, include_ideographs, self.file_source.clone())
    }

    fn download_with_zoom(
        &self,
        region_id: i64,
        style_url: &str,
        min_zoom: f64,
        max_zoom: f64,
        include_ideographs: bool,
        file_source: Arc<dyn FileSource>,
    ) -> OfflineDownload {
        OfflineDownload::new(
            region_id,
            OfflineTilePyramidRegionDefinition::new(
                style_url,
                LatLngBounds::world(),
                min_zoom,
                max_zoom,
                1.0,
                include_ideographs,
            )
            .unwrap()
            .into(),
            self.db.clone(),
            file_source,
        )
    }
}

const STYLE_URL: &str = "http://127.0.0.1:3000/style.json";

fn expect_style_url(resource: &Resource) {
    assert_eq!(STYLE_URL, resource.url);
}

fn expect_tile_0_0_0(template: &'static str) -> impl Fn(&Resource) {
    move |resource: &Resource| {
        let tile = resource.tile_data.as_ref().expect("tile data");
        assert_eq!(template, tile.url_template);
        assert_eq!(1, tile.pixel_ratio);
        assert_eq!(0, tile.x);
        assert_eq!(0, tile.y);
        assert_eq!(0, tile.z);
    }
}

/// The responders of the tests that download style.json and all its
/// resources (Activate, ExcludeIdeographs, InterruptAndResume,
/// NoFreezingOnNotFoundError but its sprite JSON).
fn respond_to_full_style(test: &OfflineTest, sprite_json: bool) {
    test.respond_with(Kind::Style, "style.json", expect_style_url);
    test.respond_with(Kind::SpriteImage, "sprite.png", |resource| {
        assert!(
            resource.url == "http://127.0.0.1:3000/sprite.png" || resource.url == "http://127.0.0.1:3000/sprite@2x.png"
        );
    });
    test.respond_with(Kind::Image, "radar.gif", |resource| {
        assert_eq!("http://127.0.0.1:3000/radar.gif", resource.url);
    });
    if sprite_json {
        test.respond_with(Kind::SpriteJSON, "sprite.json", |resource| {
            assert!(
                resource.url == "http://127.0.0.1:3000/sprite.json"
                    || resource.url == "http://127.0.0.1:3000/sprite@2x.json"
            );
        });
    }
    test.respond_with(Kind::Glyphs, "glyph.pbf", |_| {});
    test.respond_with(Kind::Source, "streets.json", |resource| {
        assert_eq!("http://127.0.0.1:3000/streets.json", resource.url);
    });
    test.respond_with(
        Kind::Tile,
        "0-0-0.vector.pbf",
        expect_tile_0_0_0("http://127.0.0.1:3000/{z}-{x}-{y}.vector.pbf"),
    );
}

// gtest: OfflineDownload.NoSubresources
#[test]
fn no_subresources() {
    let test = OfflineTest::new();
    let region = test.create_region().expect("region");
    let download = test.download(region.get_id(), STYLE_URL, false);

    test.respond_with(Kind::Style, "empty.style.json", expect_style_url);

    let expects_inactive_status = Cell::new(false);
    let (size, stop) = (test.size.clone(), test.run_loop.clone());
    let observer = MockObserver {
        status_changed_fn: Some(Box::new(move |status: OfflineRegionStatus| {
            if status.complete() {
                if !expects_inactive_status.get() {
                    expects_inactive_status.set(true);
                    assert_eq!(OfflineRegionDownloadState::Active, status.download_state);
                    assert_eq!(1, status.completed_resource_count);
                    assert_eq!(size.get(), status.completed_resource_size);
                    assert!(status.required_resource_count_is_precise);
                } else {
                    assert_eq!(OfflineRegionDownloadState::Inactive, status.download_state);
                    assert_eq!(1, status.completed_resource_count);
                    assert_eq!(size.get(), status.completed_resource_size);
                    assert!(status.required_resource_count_is_precise);
                    stop.stop();
                }
            }
        })),
        ..Default::default()
    };

    download.set_observer(Some(Rc::new(observer)));
    download.set_state(OfflineRegionDownloadState::Active);

    test.run_loop.run();
}

// gtest: OfflineDownload.InlineSource
#[test]
fn inline_source() {
    let test = OfflineTest::new();
    let region = test.create_region().expect("region");
    let download = test.download(region.get_id(), STYLE_URL, true);

    test.respond_with(Kind::Style, "inline_source.style.json", expect_style_url);
    test.respond_with(
        Kind::Tile,
        "0-0-0.vector.pbf",
        expect_tile_0_0_0("http://127.0.0.1:3000/{z}-{x}-{y}.vector.pbf"),
    );

    let (size, stop) = (test.size.clone(), test.run_loop.clone());
    let observer = MockObserver {
        status_changed_fn: Some(Box::new(move |status: OfflineRegionStatus| {
            if status.complete() {
                assert_eq!(1, status.completed_tile_count);
                assert_eq!(2, status.completed_resource_count);
                assert_eq!(size.get(), status.completed_resource_size);
                assert!(status.required_resource_count_is_precise);
                stop.stop();
            }
        })),
        ..Default::default()
    };

    download.set_observer(Some(Rc::new(observer)));
    download.set_state(OfflineRegionDownloadState::Active);

    test.run_loop.run();
}

// gtest: OfflineDownload.GeoJSONSource
#[test]
fn geojson_source() {
    let test = OfflineTest::new();
    let region = test.create_region().expect("region");
    let download = test.download(region.get_id(), STYLE_URL, false);

    test.respond_with(Kind::Style, "geojson_source.style.json", expect_style_url);
    test.respond_with(Kind::Source, "geojson.json", |resource| {
        assert_eq!("http://127.0.0.1:3000/geojson.json", resource.url);
    });

    let (size, stop) = (test.size.clone(), test.run_loop.clone());
    let observer = MockObserver {
        status_changed_fn: Some(Box::new(move |status: OfflineRegionStatus| {
            if status.complete() {
                assert_eq!(2, status.completed_resource_count);
                assert_eq!(size.get(), status.completed_resource_size);
                assert!(status.required_resource_count_is_precise);
                stop.stop();
            }
        })),
        ..Default::default()
    };

    download.set_observer(Some(Rc::new(observer)));
    download.set_state(OfflineRegionDownloadState::Active);

    test.run_loop.run();
}

/// The completion check of Activate and ExcludeIdeographs: deactivate, then
/// compare the computed status with the reported one.
fn check_completed_and_computed(
    download: &WeakOfflineDownload,
    size: u64,
    expected_resources: u64,
    status: OfflineRegionStatus,
) {
    assert_eq!(status.completed_tile_count, status.required_tile_count);
    assert_eq!(expected_resources, status.completed_resource_count);
    assert_eq!(size, status.completed_resource_size);

    download.set_state(OfflineRegionDownloadState::Inactive);
    let computed_status = download.get_status().expect("download");
    assert_eq!(OfflineRegionDownloadState::Inactive, computed_status.download_state);
    assert_eq!(status.required_resource_count, computed_status.required_resource_count);
    assert_eq!(status.required_tile_count, computed_status.required_tile_count);
    assert_eq!(status.completed_resource_count, computed_status.completed_resource_count);
    assert_eq!(status.completed_resource_size, computed_status.completed_resource_size);
    assert_eq!(status.completed_tile_count, computed_status.completed_tile_count);
    assert_eq!(status.completed_tile_size, computed_status.completed_tile_size);
    assert!(status.required_resource_count_is_precise);
}

// gtest: OfflineDownload.Activate
#[test]
fn activate() {
    let test = OfflineTest::new();
    let region = test.create_region().expect("region");
    let download = test.download(region.get_id(), STYLE_URL, true);

    respond_to_full_style(&test, true);

    let (size, stop, weak) = (test.size.clone(), test.run_loop.clone(), download.downgrade());
    let observer = MockObserver {
        status_changed_fn: Some(Box::new(move |status: OfflineRegionStatus| {
            if status.complete() {
                // 256 glyphs, 2 sprite images, 2 sprite jsons, 1 tile, 1
                // style, source, image
                check_completed_and_computed(&weak, size.get(), 264, status);
                stop.stop();
            }
        })),
        ..Default::default()
    };

    download.set_observer(Some(Rc::new(observer)));
    download.set_state(OfflineRegionDownloadState::Active);

    test.run_loop.run();
}

// gtest: OfflineDownload.ExcludeIdeographs
#[test]
fn exclude_ideographs() {
    let test = OfflineTest::new();
    let region = test.create_region().expect("region");
    let download = test.download(region.get_id(), STYLE_URL, false);

    respond_to_full_style(&test, true);

    let (size, stop, weak) = (test.size.clone(), test.run_loop.clone(), download.downgrade());
    let observer = MockObserver {
        status_changed_fn: Some(Box::new(move |status: OfflineRegionStatus| {
            if status.complete() {
                // glyph ranges, 2 sprite images, 2 sprite jsons, 1 tile, 1
                // style, source, image
                check_completed_and_computed(
                    &weak,
                    size.get(),
                    u64::from(NON_IDEOGRAPH_GLYPH_RANGES_PER_FONT_STACK) + 8,
                    status,
                );
                stop.stop();
            }
        })),
        ..Default::default()
    };

    download.set_observer(Some(Rc::new(observer)));
    download.set_state(OfflineRegionDownloadState::Active);

    test.run_loop.run();
}

/// `FakeFileSource::FakeFileRequest`.
struct FakeFileRequest {
    id: u64,
    requests: Rc<RefCell<Vec<FakeRequestEntry>>>,
}

struct FakeRequestEntry {
    id: u64,
    resource: Resource,
    callback: Rc<RefCell<Callback>>,
}

impl AsyncRequest for FakeFileRequest {}

impl Drop for FakeFileRequest {
    fn drop(&mut self) {
        self.requests.borrow_mut().retain(|entry| entry.id != self.id);
    }
}

/// `FakeOnlineFileSource` (test/src/mln/test/fake_file_source.hpp): records
/// the requests; `respond` answers the first of a kind. Requests come from
/// the thread that created it.
struct FakeOnlineFileSource {
    requests: LocalObject<RefCell<Vec<FakeRequestEntry>>>,
    next_id: std::sync::atomic::AtomicU64,
    online_fs: OnlineFileSource,
    resource_options: Mutex<ResourceOptions>,
}

impl FakeOnlineFileSource {
    fn new() -> Self {
        FakeOnlineFileSource {
            requests: LocalObject::new(RefCell::new(Vec::new())),
            next_id: std::sync::atomic::AtomicU64::new(0),
            online_fs: OnlineFileSource::new(&ResourceOptions::default_options()),
            resource_options: Mutex::new(ResourceOptions::default_options()),
        }
    }

    fn requests(&self) -> Rc<RefCell<Vec<FakeRequestEntry>>> {
        // LocalObject<RefCell<..>> holds an Rc<RefCell<..>>.
        self.requests.get().expect("FakeOnlineFileSource used from another thread")
    }

    fn respond(&self, kind: Kind, response: &Response) -> bool {
        let callback = self
            .requests()
            .borrow()
            .iter()
            .find(|entry| entry.resource.kind == kind)
            .map(|entry| entry.callback.clone());

        let request_found = callback.is_some();

        if let Some(callback) = callback {
            // Copy the callback, in case calling it deallocates the
            // AsyncRequest.
            (callback.borrow_mut())(response.clone());
        }

        request_found
    }
}

impl FileSource for FakeOnlineFileSource {
    fn request(&self, resource: &Resource, callback: Callback) -> Box<dyn AsyncRequest> {
        let id = self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let requests = self.requests();
        requests.borrow_mut().push(FakeRequestEntry {
            id,
            resource: resource.clone(),
            callback: Rc::new(RefCell::new(callback)),
        });
        Box::new(FakeFileRequest { id, requests })
    }

    fn can_request(&self, _resource: &Resource) -> bool {
        true
    }

    fn get_property(&self, property: &str) -> Value {
        self.online_fs.get_property(property)
    }

    fn set_resource_options(&self, options: ResourceOptions) {
        *self.resource_options.lock().unwrap() = options;
    }

    fn get_resource_options(&self) -> ResourceOptions {
        self.resource_options.lock().unwrap().clone()
    }
}

// gtest: OfflineDownload.DoesNotFloodTheFileSourceWithRequests
#[test]
fn does_not_flood_the_file_source_with_requests() {
    let test = OfflineTest::new();
    let file_source = Arc::new(FakeOnlineFileSource::new());
    let region = test.create_region().expect("region");
    let download = test.download_with_zoom(region.get_id(), STYLE_URL, 0.0, 0.0, true, file_source.clone());

    download.set_observer(Some(Rc::new(MockObserver::default())));
    download.set_state(OfflineRegionDownloadState::Active);
    test.run_loop.run_once();

    assert_eq!(1, file_source.requests().borrow().len());

    file_source.respond(Kind::Style, &test.response("style.json"));
    test.run_loop.run_once();

    let Value::Uint(max) = file_source.get_property(MAX_CONCURRENT_REQUESTS_KEY) else {
        panic!("max-concurrent-requests is not a uint");
    };
    assert_eq!(max as usize, file_source.requests().borrow().len());
}

// gtest: OfflineDownload.GetStatusNoResources
#[test]
fn get_status_no_resources() {
    let test = OfflineTest::new();
    let region = test.create_region().expect("region");
    let download = test.download(region.get_id(), STYLE_URL, false);
    let status = download.get_status();

    assert_eq!(OfflineRegionDownloadState::Inactive, status.download_state);
    assert_eq!(0, status.completed_resource_count);
    assert_eq!(0, status.completed_resource_size);
    assert_eq!(1, status.required_resource_count);
    assert_eq!(0, status.completed_tile_count);
    assert_eq!(0, status.required_tile_count);
    assert!(!status.required_resource_count_is_precise);
    assert!(!status.complete());
}

// gtest: OfflineDownload.GetStatusStyleComplete
#[test]
fn get_status_style_complete() {
    let test = OfflineTest::new();
    let region = test.create_region().expect("region");
    let download = test.download(region.get_id(), STYLE_URL, true);

    let response = test.response("style.json");
    test.db.borrow_mut().put_region_resource(1, &Resource::style(STYLE_URL), &response).unwrap();

    let status = download.get_status();

    assert_eq!(OfflineRegionDownloadState::Inactive, status.download_state);
    assert_eq!(1, status.completed_resource_count);
    assert_eq!(test.size.get(), status.completed_resource_size);
    assert_eq!(263, status.required_resource_count);
    assert_eq!(0, status.completed_tile_count);
    assert_eq!(0, status.required_tile_count);
    assert!(!status.required_resource_count_is_precise);
    assert!(!status.complete());
}

// gtest: OfflineDownload.GetStatusStyleAndSourceComplete
#[test]
fn get_status_style_and_source_complete() {
    let test = OfflineTest::new();
    let region = test.create_region().expect("region");
    let download = test.download(region.get_id(), STYLE_URL, true);

    let response = test.response("style.json");
    test.db.borrow_mut().put_region_resource(1, &Resource::style(STYLE_URL), &response).unwrap();

    let response = test.response("streets.json");
    test.db
        .borrow_mut()
        .put_region_resource(1, &Resource::source("http://127.0.0.1:3000/streets.json"), &response)
        .unwrap();

    let status = download.get_status();

    assert_eq!(OfflineRegionDownloadState::Inactive, status.download_state);
    assert_eq!(2, status.completed_resource_count);
    assert_eq!(test.size.get(), status.completed_resource_size);
    assert_eq!(264, status.required_resource_count);
    assert_eq!(0, status.completed_tile_count);
    assert_eq!(1, status.required_tile_count);
    assert!(status.required_resource_count_is_precise);
    assert!(!status.complete());
}

// gtest: OfflineDownload.GetStatusInfiniteMaxZoom
#[test]
fn get_status_infinite_max_zoom() {
    let test = OfflineTest::new();
    let region = test.create_region().expect("region");
    let download = OfflineDownload::new(
        region.get_id(),
        OfflineTilePyramidRegionDefinition::new(
            STYLE_URL,
            LatLngBounds::hull(LatLng::new(0.5, 0.5).unwrap(), LatLng::new(0.5001, 0.5001).unwrap()),
            14.0,
            f64::INFINITY,
            1.0,
            true,
        )
        .unwrap()
        .into(),
        test.db.clone(),
        test.file_source.clone(),
    );

    let response = test.response("style.json");
    test.db.borrow_mut().put_region_resource(1, &Resource::style(STYLE_URL), &response).unwrap();
    let response = test.response("streets.json");
    test.db
        .borrow_mut()
        .put_region_resource(1, &Resource::source("http://127.0.0.1:3000/streets.json"), &response)
        .unwrap();

    let status = download.get_status();

    // One tile at z14 and one at the source's maxzoom, z15.
    assert_eq!(2, status.required_tile_count);
}

fn connection_error() -> Response {
    let mut response = Response::new();
    response.error = Some(Box::new(Error::new(Reason::Connection, "connection error")));
    response
}

// gtest: OfflineDownload.RequestError
#[test]
fn request_error() {
    let test = OfflineTest::new();
    let region = test.create_region().expect("region");
    let download = test.download(region.get_id(), STYLE_URL, false);

    test.respond(Kind::Style, |_| connection_error());

    let stop = test.run_loop.clone();
    let observer = MockObserver {
        response_error_fn: Some(Box::new(move |error: Error| {
            assert_eq!(Reason::Connection, error.reason);
            assert_eq!("connection error", error.message);
            stop.stop();
        })),
        ..Default::default()
    };

    download.set_observer(Some(Rc::new(observer)));
    download.set_state(OfflineRegionDownloadState::Active);

    test.run_loop.run();
}

// gtest: OfflineDownload.RequestErrorsAreRetried
#[test]
fn request_errors_are_retried() {
    let test = OfflineTest::new();
    let region = test.create_region().expect("region");
    let download = test.download(region.get_id(), STYLE_URL, true);

    let (file_source, size) = (Arc::downgrade(&test.file_source), test.size.clone());
    test.respond(Kind::Style, move |_| {
        let size = size.clone();
        if let Some(file_source) = file_source.upgrade() {
            file_source.set_response(Kind::Style, move |_| Some(fixture_response(&size, "empty.style.json")));
        }

        connection_error()
    });

    let stop = test.run_loop.clone();
    let observer = MockObserver {
        status_changed_fn: Some(Box::new(move |status: OfflineRegionStatus| {
            if status.complete() {
                assert_eq!(1, status.completed_resource_count);
                stop.stop();
            }
        })),
        ..Default::default()
    };

    download.set_observer(Some(Rc::new(observer)));
    download.set_state(OfflineRegionDownloadState::Active);

    test.run_loop.run();
}

// gtest: OfflineDownload.TileCountLimitExceededNoTileResponse
#[test]
fn tile_count_limit_exceeded_no_tile_response() {
    let test = OfflineTest::new();
    let region = test.create_region().expect("region");
    let download = test.download(region.get_id(), STYLE_URL, false);

    let tile_limit: u64 = 0;

    test.db.borrow_mut().set_offline_mapbox_tile_count_limit(tile_limit);

    test.respond_with(Kind::Style, "mapbox_source.style.json", expect_style_url);

    let mapbox_tile_count_limit_exceeded_called = Rc::new(Cell::new(false));

    let (called, called2, stop) = (
        mapbox_tile_count_limit_exceeded_called.clone(),
        mapbox_tile_count_limit_exceeded_called.clone(),
        test.run_loop.clone(),
    );
    let observer = MockObserver {
        mapbox_tile_count_limit_exceeded_fn: Some(Box::new(move |limit: u64| {
            assert!(!called.get());
            assert_eq!(tile_limit, limit);
            called.set(true);
        })),
        status_changed_fn: Some(Box::new(move |status: OfflineRegionStatus| {
            if !called2.get() {
                assert!(!status.complete());
                assert_eq!(OfflineRegionDownloadState::Active, status.download_state);
            } else {
                assert_eq!(OfflineRegionDownloadState::Inactive, status.download_state);
                stop.stop();
            }
        })),
        ..Default::default()
    };

    download.set_observer(Some(Rc::new(observer)));
    download.set_state(OfflineRegionDownloadState::Active);

    test.run_loop.run();
}

// gtest: OfflineDownload.TileCountLimitExceededWithTileResponse
#[test]
fn tile_count_limit_exceeded_with_tile_response() {
    let test = OfflineTest::new();
    let region = test.create_region().expect("region");
    let download = test.download(region.get_id(), STYLE_URL, true);

    let tile_limit: u64 = 1;

    test.db.borrow_mut().set_offline_mapbox_tile_count_limit(tile_limit);

    test.respond_with(Kind::Style, "mapbox_source.style.json", expect_style_url);
    test.respond_with(Kind::Tile, "0-0-0.vector.pbf", expect_tile_0_0_0("maptiler://{z}-{x}-{y}.vector.pbf"));

    let mapbox_tile_count_limit_exceeded_called = Rc::new(Cell::new(false));

    let (called, called2, stop) = (
        mapbox_tile_count_limit_exceeded_called.clone(),
        mapbox_tile_count_limit_exceeded_called.clone(),
        test.run_loop.clone(),
    );
    let observer = MockObserver {
        mapbox_tile_count_limit_exceeded_fn: Some(Box::new(move |limit: u64| {
            assert!(!called.get());
            assert_eq!(tile_limit, limit);
            called.set(true);
        })),
        status_changed_fn: Some(Box::new(move |status: OfflineRegionStatus| {
            if !called2.get() {
                assert_eq!(OfflineRegionDownloadState::Active, status.download_state);
            } else {
                assert_eq!(OfflineRegionDownloadState::Inactive, status.download_state);
                stop.stop();
            }
            if status.completed_resource_count > tile_limit {
                stop.stop();
            }
        })),
        ..Default::default()
    };

    download.set_observer(Some(Rc::new(observer)));
    download.set_state(OfflineRegionDownloadState::Active);

    test.run_loop.run();
}

/// `test.db.put(Resource::tile(...), test.response(...))`.
fn put_tile(test: &OfflineTest, x: i32, y: i32, z: i8) {
    let response = test.response("0-0-0.vector.pbf");
    test.db.borrow_mut().put(
        &Resource::tile("http://127.0.0.1:3000/{z}-{x}-{y}.vector.pbf", 1.0, x, y, z, Scheme::XYZ, LoadingMethod::All),
        &response,
    );
}

// gtest: OfflineDownload.WithPreviouslyExistingTile
#[test]
fn with_previously_existing_tile() {
    let test = OfflineTest::new();
    let region = test.create_region().expect("region");
    let download = test.download(region.get_id(), STYLE_URL, false);

    test.respond_with(Kind::Style, "inline_source.style.json", expect_style_url);

    put_tile(&test, 0, 0, 0);

    let (size, stop) = (test.size.clone(), test.run_loop.clone());
    let observer = MockObserver {
        status_changed_fn: Some(Box::new(move |status: OfflineRegionStatus| {
            if status.complete() {
                assert_eq!(2, status.completed_resource_count);
                assert_eq!(size.get(), status.completed_resource_size);
                assert!(status.required_resource_count_is_precise);
                stop.stop();
            }
        })),
        ..Default::default()
    };

    download.set_observer(Some(Rc::new(observer)));
    download.set_state(OfflineRegionDownloadState::Active);

    test.run_loop.run();
}

// gtest: OfflineDownload.ReactivatePreviouslyCompletedDownload
#[test]
fn reactivate_previously_completed_download() {
    let test = OfflineTest::new();
    let region = test.create_region().expect("region");
    let download = test.download(region.get_id(), STYLE_URL, true);

    test.respond_with(Kind::Style, "inline_source.style.json", expect_style_url);

    put_tile(&test, 0, 0, 0);

    let stop = test.run_loop.clone();
    let observer = MockObserver {
        status_changed_fn: Some(Box::new(move |status: OfflineRegionStatus| {
            if status.complete() {
                stop.stop();
            }
        })),
        ..Default::default()
    };

    download.set_observer(Some(Rc::new(observer)));
    download.set_state(OfflineRegionDownloadState::Active);

    test.run_loop.run();

    let redownload = test.download(region.get_id(), STYLE_URL, false);

    let statuses_after_reactivate: Rc<RefCell<Vec<OfflineRegionStatus>>> = Rc::new(RefCell::new(Vec::new()));

    let (statuses, stop) = (statuses_after_reactivate.clone(), test.run_loop.clone());
    let observer = MockObserver {
        status_changed_fn: Some(Box::new(move |status: OfflineRegionStatus| {
            statuses.borrow_mut().push(status);
            if status.complete() {
                stop.stop();
            }
        })),
        ..Default::default()
    };

    redownload.set_observer(Some(Rc::new(observer)));
    redownload.set_state(OfflineRegionDownloadState::Active);

    test.run_loop.run();

    let statuses = statuses_after_reactivate.borrow();
    assert_eq!(4, statuses.len());

    assert_eq!(OfflineRegionDownloadState::Active, statuses[0].download_state);
    assert!(!statuses[0].required_resource_count_is_precise);
    assert_eq!(1, statuses[0].required_resource_count);
    assert_eq!(0, statuses[0].completed_resource_count);

    assert_eq!(OfflineRegionDownloadState::Active, statuses[1].download_state);
    assert!(statuses[1].required_resource_count_is_precise);
    assert_eq!(2, statuses[1].required_resource_count);
    assert_eq!(1, statuses[1].completed_resource_count);

    assert_eq!(OfflineRegionDownloadState::Active, statuses[2].download_state);
    assert!(statuses[2].required_resource_count_is_precise);
    assert_eq!(2, statuses[2].required_resource_count);
    assert_eq!(2, statuses[2].completed_resource_count);
}

// gtest: OfflineDownload.Deactivate
#[test]
fn deactivate() {
    let test = OfflineTest::new();
    let region = test.create_region().expect("region");
    let download = test.download(region.get_id(), STYLE_URL, false);

    test.respond_with(Kind::Style, "mapbox_source.style.json", expect_style_url);

    let (stop, weak) = (test.run_loop.clone(), download.downgrade());
    let observer = MockObserver {
        status_changed_fn: Some(Box::new(move |status: OfflineRegionStatus| {
            if status.download_state == OfflineRegionDownloadState::Inactive {
                stop.stop();
            } else {
                weak.set_state(OfflineRegionDownloadState::Inactive);
            }
        })),
        ..Default::default()
    };

    download.set_observer(Some(Rc::new(observer)));
    download.set_state(OfflineRegionDownloadState::Active);

    test.run_loop.run();
}

// gtest: OfflineDownload.AllOfflineRequestsHaveLowPriorityAndOfflineUsage
#[test]
fn all_offline_requests_have_low_priority_and_offline_usage() {
    let test = OfflineTest::new();
    let region = test.create_region().expect("region");
    let download = test.download(region.get_id(), STYLE_URL, true);

    let low_offline = |resource: &Resource| {
        assert!(resource.priority == Priority::Low);
        assert!(resource.usage == Usage::Offline);
    };
    test.respond_with(Kind::Style, "style.json", low_offline);
    test.respond_with(Kind::SpriteImage, "sprite.png", low_offline);
    test.respond_with(Kind::Image, "radar.gif", low_offline);
    test.respond_with(Kind::SpriteJSON, "sprite.json", low_offline);
    test.respond_with(Kind::Glyphs, "glyph.pbf", low_offline);
    test.respond_with(Kind::Source, "streets.json", low_offline);
    test.respond_with(Kind::Tile, "0-0-0.vector.pbf", low_offline);

    let stop = test.run_loop.clone();
    let observer = MockObserver {
        status_changed_fn: Some(Box::new(move |status: OfflineRegionStatus| {
            if status.complete() {
                stop.stop();
            }
        })),
        ..Default::default()
    };

    download.set_observer(Some(Rc::new(observer)));
    download.set_state(OfflineRegionDownloadState::Active);

    test.run_loop.run();
}

// gtest: OfflineDownload.DiskFull
#[test]
#[ignore = "TODO(port): test::SQLite3TestFS (test/src/mln/test/sqlite3_test_fs.cpp), a custom SQLite VFS that needs unsafe FFI"]
fn disk_full() {}

// Test verifies that resource batch buffer is cleared at loading interrupt.
// gtest: OfflineDownload.InterruptAndResume
#[test]
fn interrupt_and_resume() {
    let test = OfflineTest::new();
    let region = test.create_region().expect("region");
    let download = test.download(region.get_id(), STYLE_URL, true);

    respond_to_full_style(&test, true);

    let interrupted = Cell::new(false);
    let (stop, weak) = (test.run_loop.clone(), download.downgrade());
    // `test.loop.schedule([&] { ... })`: a task on the test's RunLoop.
    let interrupt = Rc::new(RefCell::new(None::<AsyncTask>));
    let interrupt2 = interrupt.clone();
    let observer = MockObserver {
        status_changed_fn: Some(Box::new(move |status: OfflineRegionStatus| {
            if !interrupted.get() && status.completed_resource_count > 0 {
                interrupted.set(true);
                assert!(!status.complete());
                let (stop, weak) = (stop.clone(), weak.clone());
                let task = AsyncTask::new(move || {
                    weak.set_state(OfflineRegionDownloadState::Inactive);
                    stop.stop();
                });
                task.send();
                *interrupt2.borrow_mut() = Some(task);
            }
        })),
        ..Default::default()
    };

    download.set_observer(Some(Rc::new(observer)));
    download.set_state(OfflineRegionDownloadState::Active);
    test.run_loop.run();

    let stop = test.run_loop.clone();
    let new_observer = MockObserver {
        status_changed_fn: Some(Box::new(move |status: OfflineRegionStatus| {
            if status.complete() {
                assert_eq!(status.completed_tile_count, status.required_tile_count);
                // 256 glyphs, 2 sprite images, 2 sprite jsons, 1 tile, 1
                // style, source, image
                assert_eq!(264, status.completed_resource_count);
                stop.stop();
            }
        })),
        ..Default::default()
    };

    download.set_observer(Some(Rc::new(new_observer)));
    download.set_state(OfflineRegionDownloadState::Active);
    test.run_loop.run();
    drop(interrupt);
}

// gtest: OfflineDownload.NoFreezingOnCachedTilesAndNewStyle
#[test]
fn no_freezing_on_cached_tiles_and_new_style() {
    let test = OfflineTest::new();
    let region = test.create_region().expect("region");
    let download = test.download_with_zoom(region.get_id(), STYLE_URL, 1.0, 1.0, true, test.file_source.clone());

    test.file_source.set_property(MAX_CONCURRENT_REQUESTS_KEY, &Value::Uint(2));
    test.respond_with(Kind::Style, "inline_source.style.json", |_| {});
    // Number of resources must exceed MAX_CONCURRENT_REQUESTS_KEY
    put_tile(&test, 0, 0, 1);
    put_tile(&test, 0, 1, 1);
    put_tile(&test, 1, 0, 1);
    put_tile(&test, 1, 1, 1);

    let stop = test.run_loop.clone();
    let observer = MockObserver {
        status_changed_fn: Some(Box::new(move |status: OfflineRegionStatus| {
            if status.complete() {
                stop.stop();
            }
        })),
        ..Default::default()
    };

    download.set_observer(Some(Rc::new(observer)));
    download.set_state(OfflineRegionDownloadState::Active);

    test.run_loop.run();
    // Passes if does not freeze.
}

// gtest: OfflineDownload.NoFreezingOnNotFoundError
#[test]
fn no_freezing_on_not_found_error() {
    let test = OfflineTest::new();
    let region = test.create_region().expect("region");
    let download = test.download(region.get_id(), STYLE_URL, true);

    respond_to_full_style(&test, false);
    test.respond(Kind::SpriteJSON, |_| {
        let mut response = Response::new();
        response.error = Some(Box::new(Error::new(Reason::NotFound, "")));
        response
    });

    let error_reported = Rc::new(Cell::new(false));
    let (reported, reported2, stop) = (error_reported.clone(), error_reported.clone(), test.run_loop.clone());
    let observer = MockObserver {
        response_error_fn: Some(Box::new(move |error: Error| {
            assert_eq!(Reason::NotFound, error.reason);
            reported.set(true);
        })),
        status_changed_fn: Some(Box::new(move |status: OfflineRegionStatus| {
            if status.complete() {
                assert!(reported2.get());
                stop.stop();
            }
        })),
        ..Default::default()
    };

    download.set_observer(Some(Rc::new(observer)));
    download.set_state(OfflineRegionDownloadState::Active);

    test.run_loop.run();
    // Passes if does not freeze.
}

/// `storedSize(data)`: the size the database stores `data` in.
fn stored_size(data: &[u8]) -> u64 {
    (data.len() as u64).min(compress(data).unwrap().len() as u64)
}

/// `util::read_file("test/fixtures/offline_download/" + path)`.
fn read_fixture(path: &str) -> Vec<u8> {
    std::fs::read(cpp_path(&format!("test/fixtures/offline_download/{path}"))).unwrap()
}

fn with_data(data: impl Into<Vec<u8>>) -> Response {
    let mut response = Response::new();
    response.data = Some(Arc::new(data.into()));
    response
}

fn no_content() -> Response {
    let mut response = Response::new();
    response.no_content = true;
    response
}

fn not_found() -> Response {
    let mut response = Response::new();
    response.error = Some(Box::new(Error::new(Reason::NotFound, "")));
    response
}

fn hours_from_now(hours: i64) -> Timestamp {
    Timestamp::from_secs(now().secs_since_epoch() + hours * 3600)
}

fn xyz_tile(url_template: &str, x: i32, y: i32, z: i8) -> Resource {
    Resource::tile(url_template, 1.0, x, y, z, Scheme::XYZ, LoadingMethod::All)
}

fn world_at_zoom(max_zoom: f64) -> OfflineRegionDefinition {
    OfflineTilePyramidRegionDefinition::new(STYLE_URL, LatLngBounds::world(), 0.0, max_zoom, 1.0, false).unwrap().into()
}

/// `runUntilInactive`: runs `download` until it goes inactive, and returns
/// its last active status. Collects the errors the download reports in
/// `errors`, and fails on any if it's `None`.
fn run_until_inactive(
    test: &OfflineTest,
    download: &OfflineDownload,
    errors: Option<Rc<RefCell<Vec<Error>>>>,
) -> OfflineRegionStatus {
    let completed = Rc::new(Cell::new(OfflineRegionStatus::default()));
    let (last, stop) = (completed.clone(), test.run_loop.clone());
    let observer = MockObserver {
        response_error_fn: Some(Box::new(move |error: Error| match &errors {
            Some(errors) => errors.borrow_mut().push(error),
            None => panic!("unexpected error: {}", error.message),
        })),
        status_changed_fn: Some(Box::new(move |status: OfflineRegionStatus| {
            if status.download_state == OfflineRegionDownloadState::Active {
                last.set(status);
            } else {
                stop.stop();
            }
        })),
        ..Default::default()
    };
    download.set_observer(Some(Rc::new(observer)));
    download.set_state(OfflineRegionDownloadState::Active);
    test.run_loop.run();
    completed.get()
}

/// `downloadUntilInactive`: runs a download of `region_id` until it goes
/// inactive, and returns its last active status. The completed counts and
/// sizes that the database reports afterwards must match it.
fn download_until_inactive(
    test: &OfflineTest,
    region_id: i64,
    definition: &OfflineRegionDefinition,
    errors: Option<Rc<RefCell<Vec<Error>>>>,
) -> OfflineRegionStatus {
    let download = OfflineDownload::new(region_id, definition.clone(), test.db.clone(), test.file_source.clone());
    let completed = run_until_inactive(test, &download, errors);
    assert!(completed.complete());
    let stored = download.get_status();
    assert_eq!(completed.completed_resource_count, stored.completed_resource_count);
    assert_eq!(completed.completed_resource_size, stored.completed_resource_size);
    assert_eq!(completed.completed_tile_count, stored.completed_tile_count);
    assert_eq!(completed.completed_tile_size, stored.completed_tile_size);
    completed
}

fn invalidate_region(test: &OfflineTest, region_id: i64) {
    assert!(test.db.borrow_mut().invalidate_region(region_id).is_none());
}

/// `testRefreshInvalidatedRegion`: downloads a region of a style and one
/// tile, invalidates it, and downloads it again. The server answers 304 Not
/// Modified for the resources marked, and sends new data for the others.
fn test_refresh_invalidated_region(style_not_modified: bool, tile_not_modified: bool) {
    let test = OfflineTest::new();
    let region = test.create_region().expect("region");
    let definition = world_at_zoom(0.0);
    let tile_url = "http://127.0.0.1:3000/{z}-{x}-{y}.vector.pbf";
    let modified = hours_from_now(-1);
    let expires = hours_from_now(1);

    let old_style = read_fixture("inline_source.style.json");
    let new_style = [old_style.as_slice(), b"\n"].concat();
    let old_tile = read_fixture("0-0-0.vector.pbf");
    let new_tile = b"new tile".to_vec();

    let response = move |data: &[u8], etag: &str| {
        let mut response = with_data(data);
        response.etag = Some(etag.to_owned());
        response.modified = Some(modified);
        response
    };

    let (style, tile) = (old_style.clone(), old_tile.clone());
    test.respond(Kind::Style, move |_| response(&style, "style-1"));
    test.respond(Kind::Tile, move |_| response(&tile, "tile-1"));
    download_until_inactive(&test, region.get_id(), &definition, None);

    invalidate_region(&test, region.get_id());

    let revalidate = move |resource: &Resource, prior_etag: &str, not_modified: bool, data: &[u8], etag: &str| {
        assert_eq!(Some(prior_etag), resource.prior_etag.as_deref());
        assert_eq!(Some(modified), resource.prior_modified);
        let mut result = if not_modified {
            let mut result = Response::new();
            result.not_modified = true;
            result
        } else {
            response(data, etag)
        };
        result.expires = Some(expires);
        result
    };

    let style_requests = Rc::new(Cell::new(0));
    let tile_requests = Rc::new(Cell::new(0));
    let (requests, style) = (style_requests.clone(), new_style.clone());
    test.respond(Kind::Style, move |resource| {
        requests.set(requests.get() + 1);
        revalidate(resource, "style-1", style_not_modified, &style, "style-2")
    });
    let (requests, tile) = (tile_requests.clone(), new_tile.clone());
    test.respond(Kind::Tile, move |resource| {
        requests.set(requests.get() + 1);
        revalidate(resource, "tile-1", tile_not_modified, &tile, "tile-2")
    });
    let status = download_until_inactive(&test, region.get_id(), &definition, None);

    assert_eq!(1, style_requests.get());
    assert_eq!(1, tile_requests.get());

    let style = if style_not_modified { &old_style } else { &new_style };
    let tile = if tile_not_modified { &old_tile } else { &new_tile };
    assert_eq!(2, status.completed_resource_count);
    assert_eq!(stored_size(style) + stored_size(tile), status.completed_resource_size);
    assert_eq!(1, status.completed_tile_count);
    assert_eq!(stored_size(tile), status.completed_tile_size);

    let check_stored = |resource: &Resource, data: &[u8], etag: &str| {
        let stored = test.db.borrow_mut().get(resource).expect("stored");
        assert!(!stored.must_revalidate);
        assert_eq!(Some(expires), stored.expires);
        assert!(stored.is_usable());
        assert_eq!(data, stored.data.expect("data").as_slice());
        assert_eq!(Some(etag), stored.etag.as_deref());
    };
    check_stored(&Resource::style(STYLE_URL), style, if style_not_modified { "style-1" } else { "style-2" });
    check_stored(&xyz_tile(tile_url, 0, 0, 0), tile, if tile_not_modified { "tile-1" } else { "tile-2" });
}

// gtest: OfflineDownload.RefreshInvalidatedRegionStyleNotModified
#[test]
fn refresh_invalidated_region_style_not_modified() {
    test_refresh_invalidated_region(true, false);
}

// gtest: OfflineDownload.RefreshInvalidatedRegionTileNotModified
#[test]
fn refresh_invalidated_region_tile_not_modified() {
    test_refresh_invalidated_region(false, true);
}

// gtest: OfflineDownload.RevalidatesOnlyStoredResourcesThatMustBeRevalidated
#[test]
fn revalidates_only_stored_resources_that_must_be_revalidated() {
    let test = OfflineTest::new();
    let region = test.create_region().expect("region");
    let definition = world_at_zoom(1.0);
    let tile_url = "http://127.0.0.1:3000/{z}-{x}-{y}.vector.pbf";
    let past = hours_from_now(-1);
    let future = hours_from_now(1);

    // The style was sent with Cache-Control: must-revalidate, and has
    // expired.
    let mut style = test.response("inline_source.style.json");
    style.etag = Some("style-1".to_owned());
    style.must_revalidate = true;
    style.expires = Some(past);
    test.db.borrow_mut().put(&Resource::style(STYLE_URL), &style);

    // The z0 tile has expired, and the z1 tiles must be revalidated once they
    // expire.
    let mut expired_tile = test.response("0-0-0.vector.pbf");
    expired_tile.expires = Some(past);
    test.db.borrow_mut().put(&xyz_tile(tile_url, 0, 0, 0), &expired_tile);
    for x in 0..2 {
        for y in 0..2 {
            let mut fresh_tile = test.response("0-0-0.vector.pbf");
            fresh_tile.must_revalidate = true;
            fresh_tile.expires = Some(future);
            test.db.borrow_mut().put(&xyz_tile(tile_url, x, y, 1), &fresh_tile);
        }
    }

    let style_requests = Rc::new(Cell::new(0));
    let requests = style_requests.clone();
    test.respond(Kind::Style, move |resource| {
        requests.set(requests.get() + 1);
        assert_eq!(Some("style-1"), resource.prior_etag.as_deref());
        let mut response = Response::new();
        response.not_modified = true;
        response.expires = Some(future);
        response
    });
    test.respond(Kind::Tile, |resource| panic!("requested usable tile {}", resource.url));

    let status = download_until_inactive(&test, region.get_id(), &definition, None);

    assert_eq!(1, style_requests.get());
    assert_eq!(6, status.completed_resource_count);
    assert_eq!(test.size.get(), status.completed_resource_size);
    assert_eq!(5, status.completed_tile_count);
}

fn inline_source_style(tile_url: &str) -> String {
    format!(r#"{{"version":8,"sources":{{"inline":{{"type":"vector","tiles":["{tile_url}"]}}}},"layers":[]}}"#)
}

const TILE_JSON_SOURCE_STYLE: &str =
    r#"{"version":8,"sources":{"remote":{"type":"vector","url":"http://127.0.0.1:3000/tiles.json"}},"layers":[]}"#;

fn tile_json(tile_url: &str) -> String {
    format!(r#"{{"tilejson":"2.1.0","tiles":["{tile_url}"]}}"#)
}

/// `storedStatus`: the status of the region `region_id` as the database
/// reports it while no download runs.
fn stored_status(test: &OfflineTest, region_id: i64) -> OfflineRegionStatus {
    OfflineDownload::new(region_id, world_at_zoom(0.0), test.db.clone(), test.file_source.clone()).get_status()
}

fn collect_errors() -> Rc<RefCell<Vec<Error>>> {
    Rc::new(RefCell::new(Vec::new()))
}

// gtest: OfflineDownload.RefreshKeepsTilesThatOnlyAnEarlierStyleListed
#[test]
fn refresh_keeps_tiles_that_only_an_earlier_style_listed() {
    let test = OfflineTest::new();
    let region = test.create_region().expect("region");
    let v1 = "http://127.0.0.1:3000/v1/{z}-{x}-{y}.pbf";
    let v2 = "http://127.0.0.1:3000/v2/{z}-{x}-{y}.pbf";
    let tile_url = Rc::new(RefCell::new(v1));
    let current_url = tile_url.clone();
    test.respond(Kind::Style, move |_| with_data(inline_source_style(&current_url.borrow())));
    test.respond(Kind::Tile, |resource| {
        with_data(format!("tile {}", resource.tile_data.as_ref().unwrap().url_template))
    });
    download_until_inactive(&test, region.get_id(), &world_at_zoom(0.0), None);

    // The refreshed style moves the tile to another URL.
    *tile_url.borrow_mut() = v2;
    invalidate_region(&test, region.get_id());
    let download = OfflineDownload::new(region.get_id(), world_at_zoom(0.0), test.db.clone(), test.file_source.clone());
    let active = run_until_inactive(&test, &download, None);

    // While it runs, the refresh counts what the current style lists.
    assert!(active.complete());
    assert_eq!(2, active.completed_resource_count);
    assert_eq!(1, active.completed_tile_count);
    assert_eq!(stored_size(format!("tile {v2}").as_bytes()), active.completed_tile_size);

    // Once it stops, the status counts the earlier tile as well.
    let stored = download.get_status();
    assert!(stored.complete());
    assert_eq!(2, stored.required_resource_count);
    assert_eq!(1, stored.required_tile_count);
    assert_eq!(3, stored.completed_resource_count);
    assert_eq!(2, stored.completed_tile_count);
    assert_eq!(
        stored_size(format!("tile {v1}").as_bytes()) + stored_size(format!("tile {v2}").as_bytes()),
        stored.completed_tile_size
    );

    let earlier = test.db.borrow_mut().get(&xyz_tile(v1, 0, 0, 0)).expect("earlier tile");
    assert!(earlier.must_revalidate);
    let current = test.db.borrow_mut().get(&xyz_tile(v2, 0, 0, 0)).expect("current tile");
    assert_eq!(format!("tile {v2}").as_bytes(), current.data.expect("data").as_slice());
}

// gtest: OfflineDownload.InlineSourceWithoutTileURLsHasNoTiles
#[test]
fn inline_source_without_tile_urls_has_no_tiles() {
    let test = OfflineTest::new();
    let region = test.create_region().expect("region");
    test.respond(Kind::Style, |_| {
        with_data(r#"{"version":8,"sources":{"inline":{"type":"vector","tiles":[]}},"layers":[]}"#)
    });
    let status = download_until_inactive(&test, region.get_id(), &world_at_zoom(0.0), None);
    assert_eq!(1, status.completed_resource_count);
    assert_eq!(0, status.required_tile_count);

    let stored = stored_status(&test, region.get_id());
    assert_eq!(1, stored.required_resource_count);
    assert_eq!(0, stored.required_tile_count);
    assert!(stored.required_resource_count_is_precise);
    assert!(stored.complete());
}

// gtest: OfflineDownload.ReportsATileJSONWithoutTileURLs
#[test]
fn reports_a_tile_json_without_tile_urls() {
    let test = OfflineTest::new();
    let region = test.create_region().expect("region");
    test.respond(Kind::Style, |_| with_data(TILE_JSON_SOURCE_STYLE));
    test.respond(Kind::Source, |_| with_data(r#"{"tilejson":"2.1.0","tiles":[]}"#));
    let errors = collect_errors();
    let status = download_until_inactive(&test, region.get_id(), &world_at_zoom(0.0), Some(errors.clone()));
    assert_eq!(1, errors.borrow().len());
    assert_eq!(Reason::Other, errors.borrow()[0].reason);
    assert_eq!(2, status.completed_resource_count);
    assert_eq!(0, status.required_tile_count);
    assert!(!status.required_resource_count_is_precise);

    let stored = stored_status(&test, region.get_id());
    assert_eq!(2, stored.required_resource_count);
    assert_eq!(0, stored.required_tile_count);
    assert!(!stored.required_resource_count_is_precise);
    assert!(stored.complete());
}

// gtest: OfflineDownload.ReportsAStyleWithNoContent
#[test]
fn reports_a_style_with_no_content() {
    let test = OfflineTest::new();
    let region = test.create_region().expect("region");
    test.respond(Kind::Style, |_| no_content());
    let errors = collect_errors();
    let status = download_until_inactive(&test, region.get_id(), &world_at_zoom(0.0), Some(errors.clone()));
    assert_eq!(1, errors.borrow().len());
    assert_eq!(Reason::Other, errors.borrow()[0].reason);
    assert_eq!(1, status.completed_resource_count);
    assert!(!status.required_resource_count_is_precise);

    let stored = stored_status(&test, region.get_id());
    assert_eq!(1, stored.required_resource_count);
    assert!(!stored.required_resource_count_is_precise);
}

// gtest: OfflineDownload.RestartingFromTheTileLimitKeepsTheDownloadActive
#[test]
fn restarting_from_the_tile_limit_keeps_the_download_active() {
    let test = OfflineTest::new();
    let region = test.create_region().expect("region");
    test.db.borrow_mut().set_offline_mapbox_tile_count_limit(0);
    test.respond_with(Kind::Style, "mapbox_source.style.json", |_| {});
    test.respond_with(Kind::Tile, "0-0-0.vector.pbf", |_| {});

    // The observer raises the limit and restarts the download.
    let download = OfflineDownload::new(region.get_id(), world_at_zoom(0.0), test.db.clone(), test.file_source.clone());
    let limits_exceeded = Rc::new(Cell::new(0));
    let restarting = Rc::new(Cell::new(false));
    let last = Rc::new(Cell::new(OfflineRegionStatus::default()));
    let (exceeded, restart, db, weak) =
        (limits_exceeded.clone(), restarting.clone(), test.db.clone(), download.downgrade());
    let (restarting_now, last_status, stop) = (restarting.clone(), last.clone(), test.run_loop.clone());
    let observer = MockObserver {
        mapbox_tile_count_limit_exceeded_fn: Some(Box::new(move |_| {
            exceeded.set(exceeded.get() + 1);
            db.borrow_mut().set_offline_mapbox_tile_count_limit(10);
            restart.set(true);
            weak.set_state(OfflineRegionDownloadState::Inactive);
            weak.set_state(OfflineRegionDownloadState::Active);
            restart.set(false);
        })),
        status_changed_fn: Some(Box::new(move |status: OfflineRegionStatus| {
            if status.download_state == OfflineRegionDownloadState::Inactive && !restarting_now.get() {
                last_status.set(status);
                stop.stop();
            }
        })),
        ..Default::default()
    };
    download.set_observer(Some(Rc::new(observer)));
    download.set_state(OfflineRegionDownloadState::Active);
    test.run_loop.run();

    assert_eq!(1, limits_exceeded.get());
    assert!(last.get().complete());
    assert_eq!(1, last.get().completed_tile_count);
}

/// `RefreshedRegion`: downloads a region of a style, a TileJSON, and one
/// tile, then invalidates it. The style and the TileJSON have the bodies
/// the case sets.
struct RefreshedRegion {
    test: OfflineTest,
    region: OfflineRegion,
    style: Rc<RefCell<Response>>,
    source: Rc<RefCell<Response>>,
    tile_requests: Rc<Cell<i32>>,
    downloaded: OfflineRegionStatus,
}

const REFRESHED_TILE_URL: &str = "http://127.0.0.1:3000/{z}-{x}-{y}.pbf";

impl RefreshedRegion {
    fn new() -> Self {
        let test = OfflineTest::new();
        let region = test.create_region().expect("region");
        let style = Rc::new(RefCell::new(with_data(TILE_JSON_SOURCE_STYLE)));
        let source = Rc::new(RefCell::new(with_data(tile_json(REFRESHED_TILE_URL))));
        let tile_requests = Rc::new(Cell::new(0));
        let body = style.clone();
        test.respond(Kind::Style, move |_| body.borrow().clone());
        let body = source.clone();
        test.respond(Kind::Source, move |_| body.borrow().clone());
        let requests = tile_requests.clone();
        test.respond(Kind::Tile, move |_| {
            requests.set(requests.get() + 1);
            with_data("tile")
        });
        let downloaded = download_until_inactive(&test, region.get_id(), &world_at_zoom(0.0), None);
        invalidate_region(&test, region.get_id());
        RefreshedRegion { test, region, style, source, tile_requests, downloaded }
    }

    /// Refreshes the region, expecting one error of `reason`, and checks that
    /// the region keeps every resource, and that the stale tile was
    /// refreshed.
    fn refresh_keeping_resources(&self, reason: Reason) {
        let errors = collect_errors();
        let status =
            download_until_inactive(&self.test, self.region.get_id(), &world_at_zoom(0.0), Some(errors.clone()));
        assert_eq!(1, errors.borrow().len());
        assert_eq!(reason, errors.borrow()[0].reason);
        assert!(status.required_resource_count_is_precise);
        assert_eq!(self.downloaded.completed_resource_count, status.completed_resource_count);
        assert_eq!(self.downloaded.completed_resource_size, status.completed_resource_size);
        assert_eq!(self.downloaded.completed_tile_count, status.completed_tile_count);
        assert_eq!(2, self.tile_requests.get());
        let tile = self.test.db.borrow_mut().get(&xyz_tile(REFRESHED_TILE_URL, 0, 0, 0)).expect("tile");
        assert!(!tile.must_revalidate);
    }
}

// gtest: OfflineDownload.RefreshContinuesFromAStoredStyleThatIsNotFound
#[test]
fn refresh_continues_from_a_stored_style_that_is_not_found() {
    let refreshed = RefreshedRegion::new();
    *refreshed.style.borrow_mut() = not_found();
    refreshed.refresh_keeping_resources(Reason::NotFound);
}

// gtest: OfflineDownload.RefreshContinuesFromAStoredTileJSONThatIsNotFound
#[test]
fn refresh_continues_from_a_stored_tile_json_that_is_not_found() {
    let refreshed = RefreshedRegion::new();
    *refreshed.source.borrow_mut() = not_found();
    refreshed.refresh_keeping_resources(Reason::NotFound);
}

// gtest: OfflineDownload.RefreshContinuesFromAStoredStyleWhenTheNewOneIsInvalid
#[test]
fn refresh_continues_from_a_stored_style_when_the_new_one_is_invalid() {
    let refreshed = RefreshedRegion::new();
    *refreshed.style.borrow_mut() = with_data("not JSON");
    refreshed.refresh_keeping_resources(Reason::Other);
    let style = refreshed.test.db.borrow_mut().get(&Resource::style(STYLE_URL)).expect("style");
    assert_eq!(TILE_JSON_SOURCE_STYLE.as_bytes(), style.data.expect("data").as_slice());
}

// gtest: OfflineDownload.RefreshContinuesFromAStoredTileJSONWhenTheNewOneIsInvalid
#[test]
fn refresh_continues_from_a_stored_tile_json_when_the_new_one_is_invalid() {
    let refreshed = RefreshedRegion::new();
    *refreshed.source.borrow_mut() = with_data("not JSON");
    refreshed.refresh_keeping_resources(Reason::Other);
}

// gtest: OfflineDownload.RefreshContinuesFromAStoredStyleWhenTheNewOneHasNoContent
#[test]
fn refresh_continues_from_a_stored_style_when_the_new_one_has_no_content() {
    let refreshed = RefreshedRegion::new();
    *refreshed.style.borrow_mut() = no_content();
    refreshed.refresh_keeping_resources(Reason::Other);
}

// gtest: OfflineDownload.RefreshContinuesFromAStoredTileJSONWhenTheNewOneHasNoContent
#[test]
fn refresh_continues_from_a_stored_tile_json_when_the_new_one_has_no_content() {
    let refreshed = RefreshedRegion::new();
    *refreshed.source.borrow_mut() = no_content();
    refreshed.refresh_keeping_resources(Reason::Other);
}

// gtest: OfflineDownload.RefreshContinuesFromAStoredTileJSONWhenTheNewOneHasNoTileURLs
#[test]
fn refresh_continues_from_a_stored_tile_json_when_the_new_one_has_no_tile_urls() {
    let refreshed = RefreshedRegion::new();
    *refreshed.source.borrow_mut() = with_data(r#"{"tilejson":"2.1.0","tiles":[]}"#);
    refreshed.refresh_keeping_resources(Reason::Other);
}

// gtest: OfflineDownload.PausingFromAnErrorStopsTheRefresh
#[test]
fn pausing_from_an_error_stops_the_refresh() {
    let refreshed = RefreshedRegion::new();
    let test = &refreshed.test;
    // One request at a time, so that work the pause failed to stop would
    // still be queued when the download resumes.
    test.file_source.set_property(MAX_CONCURRENT_REQUESTS_KEY, &Value::Uint(1));
    *refreshed.style.borrow_mut() = with_data("not JSON");

    // The observer pauses the download when it reports the invalid style.
    let download =
        OfflineDownload::new(refreshed.region.get_id(), world_at_zoom(0.0), test.db.clone(), test.file_source.clone());
    let (weak, stop) = (download.downgrade(), test.run_loop.clone());
    let observer = MockObserver {
        response_error_fn: Some(Box::new(move |_| {
            weak.set_state(OfflineRegionDownloadState::Inactive);
            stop.stop();
        })),
        ..Default::default()
    };
    download.set_observer(Some(Rc::new(observer)));
    download.set_state(OfflineRegionDownloadState::Active);
    test.run_loop.run();
    assert_eq!(1, refreshed.tile_requests.get());

    // Resumed with a valid style, the refresh counts each resource once.
    *refreshed.style.borrow_mut() = with_data(TILE_JSON_SOURCE_STYLE);
    let completed = run_until_inactive(test, &download, None);
    let stored = download.get_status();
    assert_eq!(3, completed.completed_resource_count);
    assert_eq!(1, completed.completed_tile_count);
    assert_eq!(completed.completed_resource_count, stored.completed_resource_count);
    assert_eq!(completed.completed_resource_size, stored.completed_resource_size);
    assert_eq!(2, refreshed.tile_requests.get());
}

// gtest: OfflineDownload.MissingTileJSONLeavesAPreciseResourceCount
#[test]
fn missing_tile_json_leaves_a_precise_resource_count() {
    let test = OfflineTest::new();
    let region = test.create_region().expect("region");
    test.respond(Kind::Style, |_| with_data(TILE_JSON_SOURCE_STYLE));
    test.respond(Kind::Source, |_| not_found());
    let errors = collect_errors();
    let status = download_until_inactive(&test, region.get_id(), &world_at_zoom(0.0), Some(errors.clone()));
    assert_eq!(1, errors.borrow().len());
    assert_eq!(1, status.required_resource_count);
    assert!(status.required_resource_count_is_precise);
}
