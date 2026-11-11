// The offline region API of DatabaseFileSource. C++ has no gtests for it;
// these follow the region lifecycle that maplibre-native-ffi's ABI tests
// drive through it (tests/native/abi/resources/offline.c).

use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use httpmock::MockServer;
use mln_storage::offline::{
    OfflineRegion, OfflineRegionDefinition, OfflineRegionDownloadState, OfflineRegionObserver, OfflineRegionStatus,
    OfflineTilePyramidRegionDefinition,
};
use mln_storage::offline_database::{DatabaseError, OfflineDatabase};
use mln_storage::{DatabaseFileSource, FileSourceManager, ResourceOptions};
use mln_util::geo::{LatLng, LatLngBounds};

use super::TempDir;
use crate::database_file_source::DatabaseFileSourceExt;

const TIMEOUT: Duration = Duration::from_secs(10);

/// Calls `operation` with a callback and waits for its result.
fn wait<T: Send + 'static>(operation: impl FnOnce(Box<dyn FnOnce(T) + Send>)) -> T {
    let (tx, rx) = channel();
    operation(Box::new(move |result| tx.send(result).unwrap()));
    rx.recv_timeout(TIMEOUT).expect("callback")
}

/// A database file source with its database in `dir`.
fn database_file_source(dir: &TempDir) -> DatabaseFileSource {
    DatabaseFileSource::new(
        &FileSourceManager::new(),
        &ResourceOptions::default().with_cache_path(dir.path("cache.db")),
    )
}

fn definition(style_url: &str) -> OfflineRegionDefinition {
    let bounds = LatLngBounds::hull(LatLng::new(1.0, 2.0).unwrap(), LatLng::new(3.0, 4.0).unwrap());
    OfflineTilePyramidRegionDefinition::new(style_url, bounds, 0.0, 0.0, 1.0, false).unwrap().into()
}

/// A server with a style that has no subresources.
fn style_server() -> MockServer {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method("GET").path("/style.json");
        then.status(200).body(r#"{"version":8,"sources":{},"layers":[]}"#);
    });
    server
}

/// Sends every status the region reports.
struct StatusObserver(Sender<OfflineRegionStatus>);

impl OfflineRegionObserver for StatusObserver {
    fn status_changed(&self, status: OfflineRegionStatus) {
        let _ = self.0.send(status);
    }
}

fn observe(source: &DatabaseFileSource, region: &OfflineRegion) -> Receiver<OfflineRegionStatus> {
    let (tx, rx) = channel();
    source.set_offline_region_observer(region, Some(Box::new(StatusObserver(tx))));
    rx
}

/// Waits for the download to finish.
fn wait_until_complete(statuses: &Receiver<OfflineRegionStatus>) -> OfflineRegionStatus {
    loop {
        let status = statuses.recv_timeout(TIMEOUT).expect("status");
        if status.complete() && status.download_state == OfflineRegionDownloadState::Inactive {
            return status;
        }
    }
}

#[test]
fn a_region_lives_from_creation_to_deletion() {
    let dir = TempDir::new();
    let source = database_file_source(&dir);
    let server = style_server();
    let style_url = server.url("/style.json");

    let region = wait(|done| source.create_offline_region(&definition(&style_url), &b"one".to_vec(), done)).unwrap();
    assert_eq!(b"one", region.get_metadata().as_slice());
    assert_eq!(vec![region.clone()], wait(|done| source.list_offline_regions(done)).unwrap());
    assert_eq!(Some(region.clone()), wait(|done| source.get_offline_region(region.get_id(), done)).unwrap());
    assert_eq!(None, wait(|done| source.get_offline_region(region.get_id() + 1, done)).unwrap());

    let metadata = wait(|done| source.update_offline_metadata(region.get_id(), &b"two".to_vec(), done)).unwrap();
    assert_eq!(b"two", metadata.as_slice());
    let updated = wait(|done| source.get_offline_region(region.get_id(), done)).unwrap().unwrap();
    assert_eq!(b"two", updated.get_metadata().as_slice());

    let status = wait(|done| source.get_offline_region_status(&region, done)).unwrap();
    assert_eq!(OfflineRegionDownloadState::Inactive, status.download_state);
    assert_eq!(0, status.completed_resource_count);

    let statuses = observe(&source, &region);
    source.set_offline_region_download_state(&region, OfflineRegionDownloadState::Active);
    let status = wait_until_complete(&statuses);
    assert_eq!(1, status.completed_resource_count);
    let status = wait(|done| source.get_offline_region_status(&region, done)).unwrap();
    assert_eq!(1, status.completed_resource_count);
    assert!(status.complete());

    wait(|done| source.invalidate_offline_region(&region, done)).unwrap();
    wait(|done| source.delete_offline_region(&region, done)).unwrap();
    assert!(wait(|done| source.list_offline_regions(done)).unwrap().is_empty());
    assert!(wait(|done| source.get_offline_region_status(&region, done)).is_err());
}

#[test]
fn a_deleted_region_stops_downloading() {
    let dir = TempDir::new();
    let source = database_file_source(&dir);
    let server = MockServer::start();
    let style = server.mock(|when, then| {
        when.method("GET").path("/style.json");
        then.status(200).delay(Duration::from_millis(200)).body(r#"{"version":8,"sources":{},"layers":[]}"#);
    });

    let region =
        wait(|done| source.create_offline_region(&definition(&server.url("/style.json")), &Vec::new(), done)).unwrap();
    let statuses = observe(&source, &region);
    source.set_offline_region_download_state(&region, OfflineRegionDownloadState::Active);
    assert_eq!(OfflineRegionDownloadState::Active, statuses.recv_timeout(TIMEOUT).unwrap().download_state);
    wait(|done| source.delete_offline_region(&region, done)).unwrap();

    // The style answers after the deletion; the download it belonged to is
    // gone, so the observer hears nothing more.
    std::thread::sleep(Duration::from_millis(400));
    assert!(statuses.try_recv().is_err());
    assert!(style.calls() <= 1);
}

// maplibre-native-ffi's ABI test a_download_refreshes_an_invalidated_region:
// a download after invalidating the region revalidates the style with the
// server, and a 304 keeps it.
#[test]
fn a_download_refreshes_an_invalidated_region() {
    let dir = TempDir::new();
    let source = database_file_source(&dir);
    let server = MockServer::start();
    let revalidated = server.mock(|when, then| {
        when.method("GET").path("/style.json").header("If-None-Match", "\"style-1\"");
        then.status(304).header("Cache-Control", "max-age=3600");
    });
    let fetched = server.mock(|when, then| {
        when.method("GET").path("/style.json");
        then.status(200).header("ETag", "\"style-1\"").body(r#"{"version":8,"sources":{},"layers":[]}"#);
    });

    let region =
        wait(|done| source.create_offline_region(&definition(&server.url("/style.json")), &Vec::new(), done)).unwrap();
    let statuses = observe(&source, &region);
    source.set_offline_region_download_state(&region, OfflineRegionDownloadState::Active);
    let downloaded = wait_until_complete(&statuses);
    assert_eq!((1, 0), (fetched.calls(), revalidated.calls()));

    wait(|done| source.invalidate_offline_region(&region, done)).unwrap();
    source.set_offline_region_download_state(&region, OfflineRegionDownloadState::Active);
    let refreshed = wait_until_complete(&statuses);
    assert_eq!((1, 1), (fetched.calls(), revalidated.calls()));
    assert_eq!(downloaded.completed_resource_count, refreshed.completed_resource_count);
    assert_eq!(downloaded.completed_resource_size, refreshed.completed_resource_size);
}

#[test]
fn merging_copies_the_regions_of_another_database() {
    let dir = TempDir::new();
    let side_path = dir.path("side.db");
    let side_region = OfflineDatabase::new(&side_path, &ResourceOptions::default().tile_server_options())
        .create_region(&definition("http://example.com/style.json"), &b"side".to_vec())
        .unwrap();
    let source = database_file_source(&dir);

    let merged = wait(|done| source.merge_offline_regions(&side_path, done)).unwrap();
    assert_eq!(1, merged.len());
    assert_eq!(side_region.get_definition(), merged[0].get_definition());
    assert_eq!(b"side", merged[0].get_metadata().as_slice());
    assert_eq!(merged, wait(|done| source.list_offline_regions(done)).unwrap());

    let missing = wait(|done| source.merge_offline_regions(&dir.path("missing.db"), done));
    assert!(
        matches!(&missing, Err(DatabaseError::Runtime(message)) if message.contains("user_version")),
        "{missing:?}"
    );
}
