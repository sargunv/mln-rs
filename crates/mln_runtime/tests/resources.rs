// Resource providers, URL and header transforms, the cache database and
// offline regions. They follow maplibre-native-ffi's ABI tests
// (tests/native/abi/resources/{provider,transforms,ambient_cache,offline}.c
// at b20632a) where those test the same rules.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use httpmock::MockServer;
use mln_runtime::{
    CacheDatabaseOperation, ErrorKind, EventMask, EventSource, ResourceProvider, ResourceRequest, Runtime,
    RuntimeEventKind, RuntimeOptions,
};
use mln_storage::offline::{OfflineRegionDownloadState, OfflineTilePyramidRegionDefinition};
use mln_storage::resource::Kind;
use mln_storage::{ResourceError, Response};
use mln_util::geo::{LatLng, LatLngBounds};
use support::{EMPTY_STYLE, EventLog, TempDir, map, runtime, wait, wait_err};

fn style_response() -> Response {
    Response { data: Some(Arc::new(EMPTY_STYLE.as_bytes().to_vec())), ..Response::default() }
}

#[test]
fn a_provider_serves_a_scheme_the_library_cant_load() {
    let runtime = runtime();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let record = seen.clone();
    wait(runtime.set_resource_provider(move |request: ResourceRequest| {
        record.lock().unwrap().push((request.resource().kind, request.resolved_url().to_owned()));
        assert!(!request.is_cancelled());
        request.respond(style_response()).unwrap();
    }));
    let map = map(&runtime);
    wait(map.set_style_url("custom://styles/empty"));
    assert_eq!(vec![(Kind::Style, "custom://styles/empty".to_owned())], *seen.lock().unwrap());
    assert_eq!(EMPTY_STYLE, wait(map.loaded_style_json()));
}

#[test]
fn a_provider_answers_later_from_another_thread() {
    let runtime = runtime();
    wait(runtime.set_resource_provider(|request: ResourceRequest| {
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            request.respond(style_response()).unwrap();
        });
    }));
    let map = map(&runtime);
    wait(map.set_style_url("custom://late"));
}

/// Serves only the URLs under a prefix.
struct Routed {
    prefix: &'static str,
    served: Arc<AtomicUsize>,
}

impl ResourceProvider for Routed {
    fn serves(&self, _kind: Kind, url: &str) -> bool {
        url.starts_with(self.prefix)
    }

    fn request(&self, request: ResourceRequest) {
        self.served.fetch_add(1, Ordering::SeqCst);
        request.respond(style_response()).unwrap();
    }
}

#[test]
fn a_provider_passes_through_or_leaves_what_it_doesnt_serve() {
    let server = MockServer::start();
    let style = server.mock(|when, then| {
        when.method("GET").path("/style.json");
        then.status(200).body(EMPTY_STYLE);
    });
    let runtime = runtime();
    let served = Arc::new(AtomicUsize::new(0));
    wait(runtime.set_resource_provider(Routed { prefix: "custom://", served: served.clone() }));
    let map = map(&runtime);
    wait(map.set_style_url(server.url("/style.json")));
    assert_eq!((0, 1), (served.load(Ordering::SeqCst), style.calls()));

    wait(runtime.set_resource_provider(|request: ResourceRequest| request.pass_through().unwrap()));
    wait(runtime.run_cache_database_operation(CacheDatabaseOperation::Clear));
    wait(map.set_style_url(server.url("/style.json?again")));
    assert_eq!(2, style.calls());

    wait(runtime.clear_resource_provider());
    assert_eq!(ErrorKind::Native, wait_err(map.set_style_url("custom://styles/empty")).kind);
}

#[test]
fn a_request_dropped_unanswered_fails() {
    let runtime = runtime();
    wait(runtime.set_resource_provider(drop::<ResourceRequest>));
    let map = map(&runtime);
    let error = wait_err(map.set_style_url("custom://dropped"));
    assert_eq!(ErrorKind::Native, error.kind);
    assert!(error.message.contains("without a response"), "{}", error.message);
}

#[test]
fn a_cancelled_request_calls_back_and_takes_no_answer() {
    let runtime = runtime();
    let (requests, held) = std::sync::mpsc::channel();
    wait(runtime.set_resource_provider(move |request: ResourceRequest| requests.send(request).unwrap()));
    let map = map(&runtime);
    let load = map.set_style_url("custom://never").unwrap();
    let request = held.recv_timeout(support::TIMEOUT).unwrap();
    let (cancelled, on_cancel) = std::sync::mpsc::channel();
    request.on_cancel(move || cancelled.send(()).unwrap()).unwrap();
    assert_eq!(ErrorKind::InvalidState, request.on_cancel(|| {}).unwrap_err().kind);

    // A new style load drops the first one's request.
    wait(map.set_style_json(EMPTY_STYLE));
    assert_eq!(ErrorKind::Cancelled, load.wait().unwrap_err().kind);
    on_cancel.recv_timeout(support::TIMEOUT).unwrap();
    assert!(request.is_cancelled());
    assert_eq!(ErrorKind::Cancelled, request.on_cancel(|| {}).unwrap_err().kind);
    assert_eq!(ErrorKind::InvalidState, request.respond(style_response()).unwrap_err().kind);
}

#[test]
fn a_resource_transform_rewrites_or_fails_requests() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method("GET").path("/style.json");
        then.status(200).body(EMPTY_STYLE);
    });
    let runtime = runtime();
    let base = server.base_url();
    wait(runtime.set_resource_transform(move |kind, url, done| {
        if url.contains("blocked") {
            done(Err(ResourceError::new(mln_storage::response::Reason::Other, "blocked by the transform")));
        } else {
            assert_eq!(Kind::Style, kind);
            done(Ok(url.replace("http://nowhere.invalid", &base)));
        }
    }));
    let map = map(&runtime);
    wait(map.set_style_url("http://nowhere.invalid/style.json"));
    let error = wait_err(map.set_style_url("http://nowhere.invalid/blocked.json"));
    assert!(error.message.contains("blocked by the transform"), "{}", error.message);

    wait(runtime.clear_resource_transform());
    assert_eq!(ErrorKind::Native, wait_err(map.set_style_url("http://nowhere.invalid/style.json?2")).kind);
}

#[test]
fn a_header_transform_adds_headers() {
    let server = MockServer::start();
    let style = server.mock(|when, then| {
        when.method("GET").path("/style.json").header("x-token", "secret");
        then.status(200).body(EMPTY_STYLE);
    });
    let runtime = runtime();
    wait(runtime.set_http_header_transform(|kind, _url| {
        assert_eq!(Kind::Style, kind);
        vec![("X-Token".to_owned(), "secret".to_owned()), ("User-Agent".to_owned(), "spoofed".to_owned())]
    }));
    let map = map(&runtime);
    wait(map.set_style_url(server.url("/style.json")));
    assert_eq!(1, style.calls());

    wait(runtime.clear_http_header_transform());
    assert_eq!(ErrorKind::Native, wait_err(map.set_style_url(server.url("/style.json?2"))).kind);
}

#[test]
fn cache_operations_run_on_the_cache_file() {
    let dir = TempDir::new();
    let options = RuntimeOptions { cache_path: Some(dir.path("cache.db")), ..RuntimeOptions::default() };
    let runtime = Runtime::new(options).unwrap();
    for operation in [
        CacheDatabaseOperation::Invalidate,
        CacheDatabaseOperation::Clear,
        CacheDatabaseOperation::Pack,
        CacheDatabaseOperation::Reset,
    ] {
        wait(runtime.run_cache_database_operation(operation));
    }
    wait(runtime.set_maximum_ambient_cache_size(1 << 20));
    wait(runtime.release());
}

#[test]
fn a_merge_takes_only_a_readable_database_file() {
    let dir = TempDir::new();
    let side = dir.path("side.db");
    let missing = dir.path("missing.db");
    let bounds = LatLngBounds::hull(LatLng::new(1.0, 2.0).unwrap(), LatLng::new(3.0, 4.0).unwrap());
    let definition =
        OfflineTilePyramidRegionDefinition::new("custom://style.json", bounds, 0.0, 0.0, 1.0, false).unwrap().into();
    let writer = Runtime::new(RuntimeOptions { cache_path: Some(side.clone()), ..RuntimeOptions::default() }).unwrap();
    wait(writer.offline().create_region(definition, b"side".to_vec()));
    wait(writer.release());

    let runtime = runtime();
    let offline = runtime.offline();
    for path in [missing.to_str().unwrap(), "", ":memory:", "file::memory:"] {
        let error = offline.merge_regions(path).unwrap_err();
        assert_eq!(ErrorKind::InvalidArgument, error.kind, "{path}");
        assert!(error.message.contains("readable database file"), "{}", error.message);
    }
    assert!(!missing.exists());
    let merged = wait(offline.merge_regions(side.to_str().unwrap()));
    assert_eq!(vec![b"side".to_vec()], merged.iter().map(|region| region.get_metadata().clone()).collect::<Vec<_>>());
    assert_eq!(merged, wait(offline.regions()));
}

#[test]
fn an_offline_region_downloads_and_reports_its_progress() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method("GET").path("/style.json");
        then.status(200).body(EMPTY_STYLE);
    });
    let dir = TempDir::new();
    let options = RuntimeOptions { cache_path: Some(dir.path("cache.db")), ..RuntimeOptions::default() };
    let runtime = Runtime::new(options).unwrap();
    let log = EventLog::install(&runtime);
    let offline = runtime.offline();

    let bounds = LatLngBounds::hull(LatLng::new(1.0, 2.0).unwrap(), LatLng::new(3.0, 4.0).unwrap());
    let definition = OfflineTilePyramidRegionDefinition::new(server.url("/style.json"), bounds, 0.0, 0.0, 1.0, false)
        .unwrap()
        .into();
    let region = wait(offline.create_region(definition, b"one".to_vec()));
    let id = region.get_id();
    assert_eq!(vec![region.clone()], wait(offline.regions()));
    assert_eq!(Some(region), wait(offline.region(id)));
    assert_eq!(None, wait(offline.region(id + 1)));
    assert_eq!(b"two", wait(offline.update_region_metadata(id, b"two".to_vec())).get_metadata().as_slice());

    assert_eq!(ErrorKind::InvalidArgument, offline.set_region_observed(id, EventMask::ALL).unwrap_err().kind);
    wait(offline.set_region_observed(id, EventMask::ALL_RUNTIME_EVENTS));
    wait(offline.set_region_download_state(id, OfflineRegionDownloadState::Active));
    let done = log.wait_for(|event| {
        matches!(&event.kind, RuntimeEventKind::OfflineRegionStatusChanged { region_id, status }
            if *region_id == id && status.complete() && status.download_state == OfflineRegionDownloadState::Inactive)
    });
    let done = done.expect("the download finishes");
    assert_eq!((EventSource::Runtime, 0), (done.source, done.generation));
    assert!(wait(offline.region_status(id)).complete());

    wait(offline.invalidate_region(id));
    wait(offline.delete_region(id));
    assert!(wait(offline.regions()).is_empty());
    assert_eq!(ErrorKind::NotFound, wait_err(offline.region_status(id)).kind);
    assert_eq!(ErrorKind::NotFound, wait_err(offline.delete_region(id)).kind);
    assert_eq!(ErrorKind::NotFound, wait_err(offline.set_region_observed(id, EventMask::NONE)).kind);
    wait(runtime.release());
}
