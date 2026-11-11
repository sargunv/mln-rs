// Ports include/mln/storage/online_file_source.hpp and
// platform/default/src/mln/storage/online_file_source.cpp, with
// maplibre-native-ffi's patch 0068 (a resource transform can fail a request).
//
// The requests run on the "OnlineFileSource" thread (`util::Thread`) as in
// C++: an `OnlineFileRequest` per caller request, scheduled by a
// `util::Timer` (first request at once, then revalidation when the response
// expires, or retries with backoff after errors), queued while
// `maximumConcurrentRequests` transfers are active (regular priority before
// low priority), and performed by the HTTPFileSource.
//
// C++ keys its sets by `OnlineFileRequest*`; here every request has an id
// (the caller's request id), so `allRequests`/`activeRequests` iterate in id
// order rather than pointer order.
//
// `OnlineFileRequest::actor()` (the transformed URL's way back) is a mailbox
// on the online thread whose target finds the request by id.
//
// TODO(port): thread priority.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use feature::Value;
use mln_actor::{ActorRef, AsyncTask, Mailbox, Thread, Timer, scheduler};
use mln_util::chrono::{Duration, Timestamp, now};
use mln_util::constants::{
    ASSET_PROTOCOL, CLOCK_SKEW_RETRY_TIMEOUT, DEFAULT_MAXIMUM_CONCURRENT_REQUESTS, FILE_PROTOCOL,
};
use mln_util::logging::{Event, Log};
use mln_util::mapbox;
use mln_util::sync::MutexExt;
use mln_util::tile_server_options::TileServerOptions;
use mln_vec_map::{VecMap, VecSet};

use crate::async_request::AsyncRequest;
use crate::file_source::{API_BASE_URL_KEY, API_KEY_KEY, Callback, FileSource, MAX_CONCURRENT_REQUESTS_KEY};
use crate::file_source_request::{FileSourceRequest, FileSourceRequestState};
use crate::http_file_source::HTTPFileSource;
use crate::http_header_transform::HttpHeaderTransform;
use crate::http_timeout::{error_retry_timeout, expiration_timeout};
use crate::network_status::{NetworkStatus, Status};
use crate::resource::{Kind, Priority, Resource};
use crate::resource_options::ResourceOptions;
use crate::resource_transform::{ResourceTransform, TransformResult};
use crate::response::{Error, Reason, Response};

/// For testing only
pub const ONLINE_STATUS_KEY: &str = "online-status";

/// `OnlineFileRequest`
struct OnlineFileRequest {
    id: u64,
    impl_: Weak<ThreadState>,
    resource: RefCell<Resource>,
    request: RefCell<Option<Box<dyn AsyncRequest>>>,
    timer: Timer,
    callback: Box<dyn Fn(Response)>,

    /// Counts the number of times a response was already expired when
    /// received. We're using this to add a delay when making a new request
    /// so we don't keep retrying immediately in case of a server serving
    /// expired tiles.
    expired_requests: Cell<u32>,

    /// Counts the number of subsequent failed requests. We're using this
    /// value for exponential backoff when retrying requests.
    failed_requests: Cell<u32>,
    failed_request_reason: Cell<Reason>,
    retry_after: Cell<Option<Timestamp>>,

    /// Whether the resource transform is computing the URL to load.
    transforming: Cell<bool>,
    /// Whether the resource transform failed the request. A retry calls the
    /// transform again instead of loading the original URL.
    transform_failed: Cell<bool>,

    /// Lazily created for the ResourceTransform's reply (`actor()`).
    mailbox: RefCell<Option<Arc<Mailbox>>>,
}

/// The target of an `OnlineFileRequest`'s mailbox.
struct OnlineFileRequestRef {
    impl_: Weak<ThreadState>,
    id: u64,
}

impl OnlineFileRequestRef {
    /// `OnlineFileRequest::setTransformResult(ResourceTransform::Result)`.
    fn set_transform_result(&self, result: TransformResult) {
        if let Some(impl_) = self.impl_.upgrade()
            && let Some(req) = impl_.get(self.id)
        {
            req.set_transform_result(result);
        }
    }
}

impl Drop for OnlineFileRequest {
    fn drop(&mut self) {
        if let Some(mailbox) = self.mailbox.borrow_mut().take() {
            mailbox.close();
        }
    }
}

/// `PendingRequests`: a FIFO preferring regular over low priority requests.
#[derive(Default)]
struct PendingRequests {
    regular: VecDeque<u64>,
    low: VecDeque<u64>,
}

impl PendingRequests {
    fn remove(&mut self, id: u64) {
        self.regular.retain(|&r| r != id);
        self.low.retain(|&r| r != id);
    }

    fn insert(&mut self, id: u64, priority: Priority) {
        if priority == Priority::Regular {
            self.regular.push_back(id);
        } else {
            self.low.push_back(id);
        }
    }

    fn pop(&mut self) -> Option<u64> {
        self.regular.pop_front().or_else(|| self.low.pop_front())
    }

    fn contains(&self, id: u64) -> bool {
        self.regular.contains(&id) || self.low.contains(&id)
    }
}

/// The members of `OnlineFileSourceThread`.
struct ThreadState {
    resource_options: RefCell<ResourceOptions>,

    /// The lifetime of a request is:
    ///
    /// 1. Waiting for timeout (revalidation or retry)
    /// 2. Pending (waiting for room in the active set)
    /// 3. Active (open network connection)
    /// 4. Back to #1
    ///
    /// Requests in any state are in `all_requests`. Requests in the pending
    /// state are in `pending_requests`. Requests in the active state are in
    /// `active_requests`.
    all_requests: RefCell<VecSet<u64>>,
    pending_requests: RefCell<PendingRequests>,
    active_requests: RefCell<VecSet<u64>>,

    online: Cell<bool>,
    maximum_concurrent_requests: Cell<u32>,
    http_file_source: HTTPFileSource,
    resource_transform: RefCell<ResourceTransform>,
    tasks: RefCell<VecMap<u64, Rc<OnlineFileRequest>>>,
}

/// `OnlineFileSourceThread`
pub struct OnlineFileSourceThread {
    state: Rc<ThreadState>,
    _reachability: AsyncTask,
    reachability_id: u64,
}

/// `interpolateExpiration(current, prior, expired)`
fn interpolate_expiration(current: Timestamp, prior: Option<Timestamp>, expired: &mut bool) -> Timestamp {
    let now = now();
    if current > now {
        return current;
    }

    let Some(prior) = prior else {
        *expired = true;
        return current;
    };

    // Expiring date is going backwards, fallback to exponential backoff.
    if current < prior {
        *expired = true;
        return current;
    }

    let delta = current.secs_since_epoch() - prior.secs_since_epoch();

    // Server is serving the same expired resource over and over, fallback
    // to exponential backoff.
    if delta == 0 {
        *expired = true;
        return current;
    }

    // Assume that either the client or server clock is wrong and try to
    // interpolate a valid expiration date (from the client POV) observing a
    // minimum timeout.
    let minimum = CLOCK_SKEW_RETRY_TIMEOUT.count() / 1_000_000_000;
    Timestamp::from_secs(now.secs_since_epoch() + delta.max(minimum))
}

/// `sanitizeURL`: the URL without its query string.
fn sanitize_url(url: &str) -> &str {
    match url.find('?') {
        Some(query_index) => &url[..query_index],
        None => url,
    }
}

impl ThreadState {
    fn get(&self, id: u64) -> Option<Rc<OnlineFileRequest>> {
        self.tasks.borrow().get(&id).cloned()
    }

    /// `add(OnlineFileRequest*)`
    fn add(&self, req: &OnlineFileRequest) {
        self.all_requests.borrow_mut().insert(req.id);
        self.transform(req);
    }

    /// `transform(OnlineFileRequest*)`
    fn transform(&self, req: &OnlineFileRequest) {
        req.transform_failed.set(false);
        let transform = self.resource_transform.borrow().clone();
        if transform.is_set() {
            // Request the ResourceTransform actor a new url and replace the
            // resource url with the transformed one before proceeding to
            // schedule the request.
            req.transforming.set(true);
            let r = req.actor();
            let (kind, url) = {
                let resource = req.resource.borrow();
                (resource.kind, resource.url.clone())
            };
            transform.transform(
                kind,
                &url,
                Box::new(move |result: TransformResult| {
                    r.invoke(move |req: &OnlineFileRequestRef| req.set_transform_result(result));
                }),
            );
        } else {
            req.activate();
        }
    }

    /// `remove(OnlineFileRequest*)`
    fn remove(self: &Rc<Self>, id: u64) {
        self.all_requests.borrow_mut().remove(&id);
        let was_active = self.active_requests.borrow_mut().remove(&id);
        if was_active {
            self.activate_pending_request();
        } else {
            self.pending_requests.borrow_mut().remove(id);
        }
    }

    /// `activateOrQueueRequest(OnlineFileRequest*)`
    fn activate_or_queue_request(self: &Rc<Self>, id: u64) {
        let Some(req) = self.get(id) else {
            return;
        };
        debug_assert!(self.all_requests.borrow().contains(&id));
        debug_assert!(!self.active_requests.borrow().contains(&id));
        debug_assert!(req.request.borrow().is_none());

        if self.active_requests.borrow().len() >= self.maximum_concurrent_requests.get() as usize {
            let priority = req.resource.borrow().priority;
            self.pending_requests.borrow_mut().insert(id, priority);
        } else {
            self.activate_request(&req);
        }
    }

    /// `activateRequest(OnlineFileRequest*)`
    fn activate_request(self: &Rc<Self>, req: &Rc<OnlineFileRequest>) {
        let weak = Rc::downgrade(self);
        let id = req.id;
        let callback = move |response: Response| {
            let Some(this) = weak.upgrade() else {
                return;
            };
            this.active_requests.borrow_mut().remove(&id);
            if let Some(req) = this.get(id) {
                // The transfer is done; dropping its request is a no-op.
                let finished = req.request.borrow_mut().take();
                drop(finished);
                req.completed(response);
            }
            this.activate_pending_request();
        };

        self.active_requests.borrow_mut().insert(req.id);

        if self.online.get() {
            let resource = req.resource.borrow().clone();
            let request = self.http_file_source.request(&resource, Box::new(callback));
            *req.request.borrow_mut() = Some(request);
        } else {
            let mut response = Response::new();
            response.error = Some(Box::new(Error::new(Reason::Connection, "Online connectivity is disabled.")));
            callback(response);
        }
    }

    /// `activatePendingRequest()`
    fn activate_pending_request(self: &Rc<Self>) {
        let next = self.pending_requests.borrow_mut().pop();
        if let Some(id) = next
            && let Some(req) = self.get(id)
        {
            self.activate_request(&req);
        }
    }

    fn is_pending(&self, id: u64) -> bool {
        self.pending_requests.borrow().contains(id)
    }

    fn is_active(&self, id: u64) -> bool {
        self.active_requests.borrow().contains(&id)
    }

    /// `networkIsReachableAgain()`
    fn network_is_reachable_again(&self) {
        let requests: Vec<Rc<OnlineFileRequest>> =
            self.all_requests.borrow().iter().filter_map(|id| self.get(*id)).collect();
        // Notify regular priority requests, then low priority requests.
        for priority in [Priority::Regular, Priority::Low] {
            for req in &requests {
                if req.resource.borrow().priority == priority {
                    req.network_is_reachable_again();
                }
            }
        }
    }
}

impl OnlineFileRequest {
    /// `setTransformResult(ResourceTransform::Result)`
    fn set_transform_result(&self, result: TransformResult) {
        self.transforming.set(false);
        match result {
            Ok(url) => {
                self.resource.borrow_mut().url = url;
                self.activate();
            }
            Err(error) => {
                self.transform_failed.set(true);
                let mut response = Response::new();
                response.error = Some(Box::new(error));
                self.completed(response);
            }
        }
    }

    /// `actor()`
    fn actor(&self) -> ActorRef<OnlineFileRequestRef> {
        let mut mailbox = self.mailbox.borrow_mut();
        // Lazy constructed because this can be costly and the
        // ResourceTransform is not used by many apps.
        let mailbox = mailbox.get_or_insert_with(|| {
            let target = Rc::new(OnlineFileRequestRef { impl_: self.impl_.clone(), id: self.id });
            Mailbox::with_target(&scheduler::get_current(), target)
        });
        ActorRef::new(mailbox)
    }

    /// `activate()`
    fn activate(&self) {
        // Force an immediate first request if we don't have an expiration time.
        let prior_expires = self.resource.borrow().prior_expires;
        let timeout = match prior_expires {
            Some(_) => self.get_update_interval(prior_expires),
            None => Duration::zero(),
        };
        self.schedule(timeout);
    }

    /// `schedule(Duration timeout)`
    fn schedule(&self, mut timeout: Duration) {
        let Some(impl_) = self.impl_.upgrade() else {
            return;
        };
        if self.transforming.get() || impl_.is_pending(self.id) || impl_.is_active(self.id) {
            // There's already a request in progress; don't start another one.
            return;
        }

        if timeout == Duration::max() {
            return;
        }

        // Emulate a Connection error when the Offline mode is forced with a
        // really long timeout. The request will get re-triggered when the
        // NetworkStatus is set back to Online.
        if NetworkStatus::get() == Status::Offline {
            self.failed_request_reason.set(Reason::Connection);
            self.failed_requests.set(1);
            timeout = Duration::max();
        }

        let weak = self.impl_.clone();
        let id = self.id;
        self.timer.start(timeout, Duration::zero(), move || {
            let Some(impl_) = weak.upgrade() else {
                return;
            };
            match impl_.get(id) {
                Some(req) if req.transform_failed.get() => impl_.transform(&req),
                _ => impl_.activate_or_queue_request(id),
            }
        });
    }

    /// `getUpdateInterval(std::optional<Timestamp> expires)`
    fn get_update_interval(&self, expires: Option<Timestamp>) -> Duration {
        // Calculate a timeout that depends on how many consecutive errors
        // we've encountered, and on the expiration time, if present.
        let error_retry =
            error_retry_timeout(self.failed_request_reason.get(), self.failed_requests.get(), self.retry_after.get());
        let expiration = expiration_timeout(expires, self.expired_requests.get());
        let minimum = self.resource.borrow().minimum_update_interval;
        let expiration = if expiration < minimum { minimum } else { expiration };
        if expiration < error_retry { expiration } else { error_retry }
    }

    /// `completed(Response)`
    fn completed(&self, mut response: Response) {
        // If we didn't get various caching headers in the response, continue
        // using the previous values. Otherwise, update the previous values
        // to the new values.
        {
            let mut resource = self.resource.borrow_mut();
            if response.modified.is_none() {
                response.modified = resource.prior_modified;
            } else {
                resource.prior_modified = response.modified;
            }

            if response.not_modified && resource.prior_data.is_some() {
                // When the priorData field is set, it indicates that we had
                // to revalidate the request and that the requestor hasn't
                // gotten data yet. If we get a 304 response, this means that
                // we have send the cached data to give the requestor a
                // chance to actually obtain the data.
                response.data = resource.prior_data.take();
                response.not_modified = false;
            }
        }

        let mut is_expired = false;

        if let Some(expires) = response.expires {
            let prior = self.resource.borrow().prior_expires;
            self.resource.borrow_mut().prior_expires = Some(expires);
            response.expires = Some(interpolate_expiration(expires, prior, &mut is_expired));
        }

        if is_expired {
            self.expired_requests.set(self.expired_requests.get() + 1);
        } else {
            self.expired_requests.set(0);
        }

        {
            let mut resource = self.resource.borrow_mut();
            if response.etag.is_none() {
                response.etag = resource.prior_etag.clone();
            } else {
                resource.prior_etag = response.etag.clone();
            }
        }

        if let Some(error) = &response.error {
            if error.reason == Reason::NotFound {
                let url = self.resource.borrow().url.clone();
                Log::error(Event::General, format!("The resource `{}` not found", sanitize_url(&url)));
            }
            self.failed_requests.set(self.failed_requests.get() + 1);
            self.failed_request_reason.set(error.reason);
            self.retry_after.set(error.retry_after);
        } else {
            self.failed_requests.set(0);
            self.failed_request_reason.set(Reason::Success);
        }

        self.schedule(self.get_update_interval(response.expires));

        // Calling the callback may result in `this` being deleted. It needs
        // to be done last.
        (self.callback)(response);
    }

    /// `networkIsReachableAgain()`
    fn network_is_reachable_again(&self) {
        // We need all requests to fail at least once before we are going to
        // start retrying them, and we only immediately restart request that
        // failed due to connection issues.
        if self.failed_request_reason.get() == Reason::Connection {
            self.schedule(Duration::zero());
        }
    }
}

impl OnlineFileSourceThread {
    fn new(resource_options: ResourceOptions) -> Self {
        let http_file_source = HTTPFileSource::new(&resource_options);
        let state = Rc::new(ThreadState {
            resource_options: RefCell::new(resource_options),
            all_requests: RefCell::new(VecSet::new()),
            pending_requests: RefCell::new(PendingRequests::default()),
            active_requests: RefCell::new(VecSet::new()),
            online: Cell::new(true),
            maximum_concurrent_requests: Cell::new(DEFAULT_MAXIMUM_CONCURRENT_REQUESTS),
            http_file_source,
            resource_transform: RefCell::new(ResourceTransform::default()),
            tasks: RefCell::new(VecMap::new()),
        });
        let weak = Rc::downgrade(&state);
        let reachability = AsyncTask::new(move || {
            if let Some(state) = weak.upgrade() {
                state.network_is_reachable_again();
            }
        });
        let reachability_id = NetworkStatus::subscribe(reachability.sender());
        OnlineFileSourceThread { state, _reachability: reachability, reachability_id }
    }

    /// `request(AsyncRequest* req, Resource, const ActorRef<FileSourceRequest>&)`
    pub fn request(&self, req: u64, resource: Resource, r: &ActorRef<FileSourceRequestState>) {
        let r = r.clone();
        let callback = move |res: Response| {
            r.invoke(move |request: &FileSourceRequestState| request.set_response(res));
        };
        let request = Rc::new(OnlineFileRequest {
            id: req,
            impl_: Rc::downgrade(&self.state),
            resource: RefCell::new(resource),
            request: RefCell::new(None),
            timer: Timer::new(),
            callback: Box::new(callback),
            expired_requests: Cell::new(0),
            failed_requests: Cell::new(0),
            failed_request_reason: Cell::new(Reason::Success),
            retry_after: Cell::new(None),
            transforming: Cell::new(false),
            transform_failed: Cell::new(false),
            mailbox: RefCell::new(None),
        });
        self.state.tasks.borrow_mut().insert(req, request.clone());
        self.state.add(&request);
    }

    /// `cancel(AsyncRequest* req)`
    pub fn cancel(&self, req: u64) {
        self.state.remove(req);
        let task = self.state.tasks.borrow_mut().remove(&req);
        drop(task);
    }

    /// `setResourceTransform(ResourceTransform)`
    fn set_resource_transform(&self, transform: ResourceTransform) {
        *self.state.resource_transform.borrow_mut() = transform;
    }

    fn set_http_header_transform(&self, transform: HttpHeaderTransform) {
        self.state.http_file_source.set_http_header_transform(transform);
    }

    fn set_resource_options(&self, options: ResourceOptions) {
        *self.state.resource_options.borrow_mut() = options;
    }

    /// `setOnlineStatus(bool)`
    fn set_online_status(&self, status: bool) {
        self.state.online.set(status);
        if status {
            self.state.network_is_reachable_again();
        }
    }

    fn set_maximum_concurrent_requests(&self, maximum: u32) {
        self.state.maximum_concurrent_requests.set(maximum);
    }

    fn set_api_base_url(&self, url: String) {
        let mut options = self.state.resource_options.borrow_mut();
        let updated = options.clone().with_tile_server_options(TileServerOptions::new().with_base_url(url));
        *options = updated;
    }

    fn set_api_key(&self, key: String) {
        let mut options = self.state.resource_options.borrow_mut();
        let updated = options.clone().with_api_key(key);
        *options = updated;
    }
}

impl Drop for OnlineFileSourceThread {
    fn drop(&mut self) {
        NetworkStatus::unsubscribe(self.reachability_id);
        // Requests hold timers on this thread; drop them here.
        self.state.tasks.borrow_mut().clear();
    }
}

/// `OnlineFileSource::Impl`. `thread` is declared last, as in C++, but
/// dropped explicitly first (see `Drop`).
struct Impl {
    cached_resource_options: Mutex<ResourceOptions>,
    cached_maximum_concurrent_requests: Mutex<u32>,
    thread: Thread<OnlineFileSourceThread>,
}

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

impl Impl {
    fn new(resource_options: &ResourceOptions) -> Self {
        let ro = resource_options.clone();
        // TODO(port): makeThreadPrioritySetter(EXPERIMENTAL_THREAD_PRIORITY_NETWORK).
        let thread = Thread::new("OnlineFileSource", move |_| OnlineFileSourceThread::new(ro));
        Impl {
            cached_resource_options: Mutex::new(resource_options.clone()),
            cached_maximum_concurrent_requests: Mutex::new(DEFAULT_MAXIMUM_CONCURRENT_REQUESTS),
            thread,
        }
    }

    fn request(&self, callback: Callback, res: Resource) -> Box<dyn AsyncRequest> {
        let mut req = FileSourceRequest::new(callback);
        let req_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        let actor_ref = self.thread.actor();
        req.on_cancel(Box::new(move || {
            actor_ref.invoke(move |t: &OnlineFileSourceThread| t.cancel(req_id));
        }));
        let req_ref = req.actor();
        self.thread.actor().invoke(move |t: &OnlineFileSourceThread| t.request(req_id, res, &req_ref));
        Box::new(req)
    }

    fn get_api_key(&self) -> String {
        self.cached_resource_options.lock_unpoisoned().api_key().to_owned()
    }
}

/// `OnlineFileSource`: the `Network` file source.
pub struct OnlineFileSource {
    impl_: Impl,
}

impl OnlineFileSource {
    pub fn new(resource_options: &ResourceOptions) -> Self {
        OnlineFileSource { impl_: Impl::new(resource_options) }
    }
}

impl FileSource for OnlineFileSource {
    fn request(&self, resource: &Resource, callback: Callback) -> Box<dyn AsyncRequest> {
        let mut res = resource.clone();
        let options = self.impl_.cached_resource_options.lock_unpoisoned().tile_server_options();
        let api_key = self.impl_.get_api_key();

        match resource.kind {
            Kind::Unknown | Kind::Image => {}
            Kind::Style => res.url = mapbox::normalize_style_url(&options, &resource.url, &api_key),
            Kind::Source => match mapbox::normalize_source_url(&options, &resource.url, &api_key) {
                Ok(url) => res.url = url,
                // C++ throws out of `request`; the port answers with the error.
                Err(message) => {
                    let mut response = Response::new();
                    response.error = Some(Box::new(Error::new(Reason::Other, message)));
                    let req = FileSourceRequest::new(callback);
                    let r = req.actor();
                    r.invoke(move |request: &FileSourceRequestState| request.set_response(response));
                    return Box::new(req);
                }
            },
            Kind::Glyphs => res.url = mapbox::normalize_glyphs_url(&options, &resource.url, &api_key),
            Kind::SpriteImage | Kind::SpriteJSON => {
                res.url = mapbox::normalize_sprite_url(&options, &resource.url, &api_key)
            }
            Kind::Tile => res.url = mapbox::normalize_tile_url(&options, &resource.url, &api_key),
        }

        self.impl_.request(callback, res)
    }

    fn can_request(&self, resource: &Resource) -> bool {
        resource.has_loading_method(crate::resource::LoadingMethod::Network)
            && !resource.url.starts_with(ASSET_PROTOCOL)
            && !resource.url.starts_with(FILE_PROTOCOL)
    }

    fn pause(&self) {
        self.impl_.thread.pause();
    }

    fn resume(&self) {
        self.impl_.thread.resume();
    }

    fn set_property(&self, key: &str, value: &Value) {
        if key == API_KEY_KEY {
            if let Value::String(api_key) = value {
                let key = api_key.clone();
                self.impl_.thread.actor().invoke(move |t: &OnlineFileSourceThread| t.set_api_key(key));
                let mut options = self.impl_.cached_resource_options.lock_unpoisoned();
                *options = options.clone().with_api_key(api_key.clone());
            } else {
                Log::error(Event::General, "Invalid apiKey property value type.");
            }
        } else if key == API_BASE_URL_KEY {
            if let Value::String(base_url) = value {
                let url = base_url.clone();
                self.impl_.thread.actor().invoke(move |t: &OnlineFileSourceThread| t.set_api_base_url(url));
                let mut options = self.impl_.cached_resource_options.lock_unpoisoned();
                let tile_server_options = options.tile_server_options().with_base_url(base_url.clone());
                *options = options.clone().with_tile_server_options(tile_server_options);
            } else {
                Log::error(Event::General, "Invalid base-url property value type.");
            }
        } else if key == MAX_CONCURRENT_REQUESTS_KEY {
            if let Value::Uint(maximum) = value {
                debug_assert!(*maximum < u64::from(u32::MAX));
                let maximum = *maximum as u32;
                self.impl_
                    .thread
                    .actor()
                    .invoke(move |t: &OnlineFileSourceThread| t.set_maximum_concurrent_requests(maximum));
                *self.impl_.cached_maximum_concurrent_requests.lock_unpoisoned() = maximum;
            } else {
                Log::error(Event::General, "Invalid max-concurrent-requests property value type.");
            }
        } else if key == ONLINE_STATUS_KEY {
            // For testing only
            if let Value::Bool(status) = value {
                let status = *status;
                self.impl_.thread.actor().invoke(move |t: &OnlineFileSourceThread| t.set_online_status(status));
            }
        } else {
            Log::error(Event::General, format!("Resource provider does not support property {key}"));
        }
    }

    fn get_property(&self, key: &str) -> Value {
        if key == API_KEY_KEY {
            Value::String(self.impl_.get_api_key())
        } else if key == API_BASE_URL_KEY {
            Value::String(
                self.impl_.cached_resource_options.lock_unpoisoned().tile_server_options().base_url().to_owned(),
            )
        } else if key == MAX_CONCURRENT_REQUESTS_KEY {
            Value::Uint(u64::from(*self.impl_.cached_maximum_concurrent_requests.lock_unpoisoned()))
        } else {
            Log::error(Event::General, format!("Resource provider does not support property {key}"));
            Value::Null
        }
    }

    fn set_resource_transform(&self, transform: ResourceTransform) {
        self.impl_.thread.actor().invoke(move |t: &OnlineFileSourceThread| t.set_resource_transform(transform));
    }

    fn set_http_header_transform(&self, transform: HttpHeaderTransform) {
        self.impl_.thread.actor().invoke(move |t: &OnlineFileSourceThread| t.set_http_header_transform(transform));
    }

    fn set_resource_options(&self, options: ResourceOptions) {
        let o = options.clone();
        self.impl_.thread.actor().invoke(move |t: &OnlineFileSourceThread| t.set_resource_options(o));
        *self.impl_.cached_resource_options.lock_unpoisoned() = options;
    }

    fn get_resource_options(&self) -> ResourceOptions {
        self.impl_.cached_resource_options.lock_unpoisoned().clone()
    }
}
