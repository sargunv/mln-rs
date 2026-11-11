// Ports render-test/file_source.hpp and render-test/file_source.cpp
// (`ProxyFileSource`), plus the file sources of
// `TestRunner::registerProxyFileSource` (render-test/runner.cpp).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use feature::Value;
use mln_storage::file_source::READ_ONLY_MODE_KEY;
use mln_storage::{
    AsyncRequest, Callback, FileSource, FileSourceManager, FileSourceType, Kind, LoadingMethod, Resource,
    ResourceOptions, Response,
};
use mln_util::logging::{Event, Log};

static REQUEST_COUNT: AtomicUsize = AtomicUsize::new(0);
static TRANSFERRED_SIZE: AtomicUsize = AtomicUsize::new(0);
static ACTIVE: AtomicBool = AtomicBool::new(false);
static OFFLINE: AtomicBool = AtomicBool::new(true);

/// Port-only: the requests passed on and neither answered nor cancelled yet,
/// by request number: each URL and when it was requested. A test that times
/// out reports them (watchdog.rs), so a hang says what it waited for.
static PENDING: Mutex<BTreeMap<u64, (String, Instant)>> = Mutex::new(BTreeMap::new());
static NEXT_REQUEST: AtomicU64 = AtomicU64::new(0);

/// Adds a request for `url` to `PENDING` and returns its number.
pub(crate) fn track(url: &str) -> u64 {
    let number = NEXT_REQUEST.fetch_add(1, Ordering::SeqCst);
    PENDING.lock().unwrap().insert(number, (url.to_string(), Instant::now()));
    number
}

/// Removes request `number` from `PENDING`, if it is there.
pub(crate) fn answered(number: u64) {
    PENDING.lock().unwrap().remove(&number);
}

/// A request passed on, which leaves `PENDING` when it is cancelled.
pub(crate) struct PendingRequest {
    pub(crate) number: u64,
    pub(crate) _request: Box<dyn AsyncRequest>,
}

impl AsyncRequest for PendingRequest {}

impl Drop for PendingRequest {
    fn drop(&mut self) {
        answered(self.number);
    }
}

/// `ProxyFileSource`: loads everything cache-only from the database while
/// offline and maps the test URLs (`local://`, `http://localhost:2900/`) to
/// the `http://` URLs the cache stores.
pub struct ProxyFileSource {
    default_resource_loader: Arc<dyn FileSource>,
    resource_options: Mutex<ResourceOptions>,
}

impl ProxyFileSource {
    /// A proxy over `default_resource_loader`. While offline, it makes
    /// `file_sources`' database for these options read-only.
    pub fn new(
        default_resource_loader: Arc<dyn FileSource>,
        file_sources: &FileSourceManager,
        resource_options: &ResourceOptions,
    ) -> Self {
        if OFFLINE.load(Ordering::SeqCst) {
            if let Some(dbfs) = file_sources.get_file_source(FileSourceType::Database, resource_options) {
                dbfs.set_property(READ_ONLY_MODE_KEY, &Value::Bool(true));
            }
        }
        ProxyFileSource { default_resource_loader, resource_options: Mutex::new(resource_options.clone()) }
    }

    /// Flag to change the networking mode of the file source.
    pub fn set_offline(status: bool) {
        OFFLINE.store(status, Ordering::SeqCst);
    }

    /// Starts/stops metrics tracking.
    pub fn set_tracking_active(active: bool) {
        ACTIVE.store(active, Ordering::SeqCst);
        REQUEST_COUNT.store(0, Ordering::SeqCst);
        TRANSFERRED_SIZE.store(0, Ordering::SeqCst);
    }

    /// Returns metrics tracking status.
    pub fn is_tracking_active() -> bool {
        ACTIVE.load(Ordering::SeqCst)
    }

    /// Returns the total amount of requests.
    pub fn get_request_count() -> usize {
        REQUEST_COUNT.load(Ordering::SeqCst)
    }

    /// Returns the size of transferred data (in bytes).
    pub fn get_transferred_size() -> usize {
        TRANSFERRED_SIZE.load(Ordering::SeqCst)
    }

    /// Port-only: the URLs of the requests that are neither answered nor
    /// cancelled, oldest first, with how long ago each was made.
    pub fn pending_requests() -> Vec<(String, Duration)> {
        let now = Instant::now();
        PENDING.lock().unwrap().values().map(|(url, since)| (url.clone(), now - *since)).collect()
    }
}

/// `str.compare(0, prefix.size(), prefix) == 0` + `replace(0, n, "http://")`.
fn replace_prefix(s: &mut String, prefix: &str) -> bool {
    if s.starts_with(prefix) {
        s.replace_range(..prefix.len(), "http://");
        true
    } else {
        false
    }
}

impl FileSource for ProxyFileSource {
    fn request(&self, resource: &Resource, mut callback: Callback) -> Box<dyn AsyncRequest> {
        let mut transformed = resource.clone();

        // If offline, force always loading the resource from the cache so we
        // don't make any network request.
        if OFFLINE.load(Ordering::SeqCst) {
            transformed.loading_method = LoadingMethod::CacheOnly;
        }

        // This is needed for compatibility with the style tests that are
        // using local:// instead of http:// which is the schema we support
        // for cached files.
        if replace_prefix(&mut transformed.url, "local://") && transformed.kind == Kind::Tile {
            if let Some(tile_data) = &mut transformed.tile_data {
                // C++ replaces the first 8 characters unconditionally.
                tile_data.url_template.replace_range(..8.min(tile_data.url_template.len()), "http://");
            }
        }

        if replace_prefix(&mut transformed.url, "http://localhost:2900/") && transformed.kind == Kind::Tile {
            if let Some(tile_data) = &mut transformed.tile_data {
                // C++ replaces the first 22 characters unconditionally.
                tile_data.url_template.replace_range(..22.min(tile_data.url_template.len()), "http://");
            }
        }

        let number = track(&transformed.url);
        let logged = transformed.clone();
        let request = self.default_resource_loader.request(
            &transformed,
            Box::new(move |response: Response| {
                answered(number);
                if logged.loading_method == LoadingMethod::CacheOnly && response.no_content {
                    match (&logged.tile_data, logged.kind) {
                        (Some(tile_data), Kind::Tile) => Log::info(
                            Event::Database,
                            format!("Resource not found in cache: {}({})", logged.url, tile_data.url_template),
                        ),
                        _ => Log::info(Event::Database, format!("Resource not found in cache: {}", logged.url)),
                    }
                }

                let size = response.data.as_ref().map_or(0, |data| data.len());
                if ACTIVE.load(Ordering::SeqCst) {
                    REQUEST_COUNT.fetch_add(1, Ordering::SeqCst);
                    TRANSFERRED_SIZE.fetch_add(size, Ordering::SeqCst);
                }
                callback(response);
            }),
        );
        Box::new(PendingRequest { number, _request: request })
    }

    fn can_request(&self, _resource: &Resource) -> bool {
        true
    }

    fn set_resource_options(&self, options: ResourceOptions) {
        *self.resource_options.lock().unwrap() = options;
    }

    fn get_resource_options(&self) -> ResourceOptions {
        self.resource_options.lock().unwrap().clone()
    }
}

/// File sources whose `ResourceLoader` is a [`ProxyFileSource`] over the
/// default one (`TestRunner::registerProxyFileSource()`).
pub fn proxy_file_sources() -> FileSourceManager {
    let file_sources = FileSourceManager::new();
    let resource_loader_factory = file_sources
        .unregister_file_source_factory(FileSourceType::ResourceLoader)
        .expect("default ResourceLoader factory");
    file_sources.register_file_source_factory(
        FileSourceType::ResourceLoader,
        Arc::new(move |file_sources: &FileSourceManager, resource_options: &ResourceOptions| {
            let file_source = resource_loader_factory(file_sources, resource_options);
            Arc::new(ProxyFileSource::new(file_source, file_sources, resource_options)) as Arc<dyn FileSource>
        }),
    );
    file_sources
}
