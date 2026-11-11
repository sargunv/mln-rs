//! A network over the benchmarks' local files: while [`set_tile_latency`]
//! sets a delay, every scene's file source answers tile requests that late,
//! fresh for an hour as a tile server's are, and doesn't ask again for a
//! tile it answered until then, as the online file source doesn't. It
//! counts the tile requests that reach the files. Without a delay it passes
//! everything on at once.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use mln_actor::timer::Timer;
use mln_storage::{
    AsyncRequest, Callback, FileSource, FileSourceManager, FileSourceType, Kind, Resource, ResourceOptions, Response,
};
use mln_util::chrono::Timestamp;

/// How long a tile stays fresh: a tile server's `Cache-Control: max-age`.
const FRESH_SECONDS: i64 = 3600;

static LATENCY_MS: AtomicU64 = AtomicU64::new(0);
static TILE_REQUESTS: AtomicUsize = AtomicUsize::new(0);

/// Delays every tile request by `latency` from now on, as a network would
/// answer it; zero passes them on at once.
pub fn set_tile_latency(latency: Duration) {
    LATENCY_MS.store(latency.as_millis() as u64, Ordering::SeqCst);
}

/// The tile requests that reached the files since the last call, and those
/// cancelled while they waited out the delay.
pub fn take_tile_requests() -> usize {
    TILE_REQUESTS.swap(0, Ordering::SeqCst)
}

/// Wraps the `ResourceLoader` factory of `file_sources`. Must run before
/// the first map asks it for its file source.
pub fn register(file_sources: &FileSourceManager) {
    let inner =
        file_sources.unregister_file_source_factory(FileSourceType::ResourceLoader).expect("a ResourceLoader factory");
    file_sources.register_file_source_factory(
        FileSourceType::ResourceLoader,
        Arc::new(move |file_sources: &FileSourceManager, resource_options: &ResourceOptions| {
            Arc::new(Delayed { inner: inner(file_sources, resource_options) }) as Arc<dyn FileSource>
        }),
    );
}

struct Delayed {
    inner: Arc<dyn FileSource>,
}

/// A tile request waiting out its latency, then the inner request.
/// Dropping it cancels either.
struct DelayedRequest {
    _timer: Timer,
    _request: Rc<RefCell<Option<Box<dyn AsyncRequest>>>>,
}

impl AsyncRequest for DelayedRequest {}

/// A request for a tile that is still fresh, which the online file source
/// holds until it expires: within a case, never answered.
struct Fresh;

impl AsyncRequest for Fresh {}

impl FileSource for Delayed {
    fn request(&self, resource: &Resource, callback: Callback) -> Box<dyn AsyncRequest> {
        if resource.kind != Kind::Tile {
            return self.inner.request(resource, callback);
        }
        let latency = LATENCY_MS.load(Ordering::SeqCst);
        if latency == 0 {
            TILE_REQUESTS.fetch_add(1, Ordering::SeqCst);
            return self.inner.request(resource, callback);
        }
        let now = mln_util::chrono::now();
        if resource.prior_expires.is_some_and(|expires| expires > now) {
            return Box::new(Fresh);
        }
        TILE_REQUESTS.fetch_add(1, Ordering::SeqCst);
        let request: Rc<RefCell<Option<Box<dyn AsyncRequest>>>> = Rc::default();
        let timer = Timer::new();
        let (inner, resource, slot) = (self.inner.clone(), resource.clone(), Rc::downgrade(&request));
        let mut callback = Some(callback);
        let delay = mln_util::chrono::Duration::from_millis(latency as i64);
        timer.start(delay, mln_util::chrono::Duration::zero(), move || {
            if let (Some(slot), Some(mut callback)) = (slot.upgrade(), callback.take()) {
                let fresh = Box::new(move |mut response: Response| {
                    let expires = mln_util::chrono::now().secs_since_epoch() + FRESH_SECONDS;
                    response.expires = Some(Timestamp::from_secs(expires));
                    callback(response);
                });
                *slot.borrow_mut() = Some(inner.request(&resource, fresh));
            }
        });
        Box::new(DelayedRequest { _timer: timer, _request: request })
    }

    fn supports_cache_only_requests(&self) -> bool {
        self.inner.supports_cache_only_requests()
    }

    fn can_request(&self, resource: &Resource) -> bool {
        self.inner.can_request(resource)
    }

    fn set_resource_options(&self, options: ResourceOptions) {
        self.inner.set_resource_options(options);
    }

    fn get_resource_options(&self) -> ResourceOptions {
        self.inner.get_resource_options()
    }
}
