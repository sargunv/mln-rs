// Ports src/mln/storage/main_resource_loader.hpp and
// platform/default/src/mln/storage/main_resource_loader.cpp.
//
// The waterfall runs on the "ResourceLoaderThread" (`util::Thread`), as in
// C++. `tasks` is keyed by a request id instead of the `AsyncRequest*` of
// the caller's request. Callbacks of the chained requests reach the thread
// object through a `Weak` (C++ captures `this`).
//
// TODO(port): `setResourceTransform`; MBGL_TIMING logs.

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use mln_actor::{ActorRef, Thread};
use mln_util::sync::MutexExt;
use mln_vec_map::VecMap;

use crate::async_request::AsyncRequest;
use crate::file_source::{Callback, FileSource, FileSourceType};
use crate::file_source_manager::FileSourceManager;
use crate::file_source_request::{FileSourceRequest, FileSourceRequestState};
use crate::resource::{LoadingMethod, Priority, Resource};
use crate::resource_options::ResourceOptions;
use crate::response::{Error, Reason, Response};

type FileSourcePtr = Option<Arc<dyn FileSource>>;

struct ThreadInner {
    asset_file_source: FileSourcePtr,
    database_file_source: FileSourcePtr,
    local_file_source: FileSourcePtr,
    online_file_source: FileSourcePtr,
    mbtiles_file_source: FileSourcePtr,
    pmtiles_file_source: FileSourcePtr,
    tasks: RefCell<VecMap<u64, Box<dyn AsyncRequest>>>,
}

/// `MainResourceLoaderThread`.
pub struct MainResourceLoaderThread {
    inner: Rc<ThreadInner>,
}

fn make_callback(r: &ActorRef<FileSourceRequestState>) -> impl Fn(Response) + Clone + 'static {
    let r = r.clone();
    move |res: Response| {
        r.invoke(move |request: &FileSourceRequestState| request.set_response(res));
    }
}

impl ThreadInner {
    /// The `requestFromNetwork` lambda of `request`.
    fn request_from_network(
        self: &Rc<Self>,
        resource: &Resource,
        res: &Resource,
        parent: Option<Box<dyn AsyncRequest>>,
        callback: impl Fn(Response) + 'static,
    ) -> Option<Box<dyn AsyncRequest>> {
        let online_file_source = match &self.online_file_source {
            Some(online) if online.can_request(resource) => online.clone(),
            _ => return parent,
        };

        // Keep parent request alive while chained request is being processed.
        let parent_keep_alive: Rc<Option<Box<dyn AsyncRequest>>> = Rc::new(parent);

        let weak = Rc::downgrade(self);
        let forwarded = res.clone();
        Some(online_file_source.request(
            res,
            Box::new(move |response: Response| {
                let _ptr = &parent_keep_alive;
                if let Some(this) = weak.upgrade() {
                    if let Some(database_file_source) = &this.database_file_source {
                        database_file_source.forward(&forwarded, &response, None);
                    }
                }
                // TODO(port): MBGL_TIMING_FINISH for tiles.
                callback(response);
            }),
        ))
    }
}

impl MainResourceLoaderThread {
    fn new(
        asset_file_source: FileSourcePtr,
        database_file_source: FileSourcePtr,
        local_file_source: FileSourcePtr,
        online_file_source: FileSourcePtr,
        mbtiles_file_source: FileSourcePtr,
        pmtiles_file_source: FileSourcePtr,
    ) -> Self {
        MainResourceLoaderThread {
            inner: Rc::new(ThreadInner {
                asset_file_source,
                database_file_source,
                local_file_source,
                online_file_source,
                mbtiles_file_source,
                pmtiles_file_source,
                tasks: RefCell::new(VecMap::new()),
            }),
        }
    }

    /// `request(AsyncRequest* req, const Resource&, const ActorRef<FileSourceRequest>&)`.
    pub fn request(&self, req: u64, resource: &Resource, r: &ActorRef<FileSourceRequestState>) {
        let inner = &self.inner;
        let callback = make_callback(r);

        let can = |fs: &FileSourcePtr| fs.as_ref().filter(|fs| fs.can_request(resource)).cloned();

        // Waterfall resource request processing and return early once
        // resource was requested.
        let task: Option<Box<dyn AsyncRequest>> = if let Some(asset) = can(&inner.asset_file_source) {
            // Asset request
            Some(asset.request(resource, Box::new(callback.clone())))
        } else if let Some(mbtiles) = can(&inner.mbtiles_file_source) {
            // Local file request
            Some(mbtiles.request(resource, Box::new(callback.clone())))
        } else if let Some(pmtiles) = can(&inner.pmtiles_file_source) {
            // Local file request
            Some(pmtiles.request(resource, Box::new(callback.clone())))
        } else if let Some(local) = can(&inner.local_file_source) {
            // Local file request
            Some(local.request(resource, Box::new(callback.clone())))
        } else if let Some(database) = can(&inner.database_file_source) {
            // Try cache only request if needed.
            if resource.loading_method == LoadingMethod::CacheOnly {
                Some(database.request(resource, Box::new(callback.clone())))
            } else {
                // Cache request with fallback to network with cache control
                let weak: Weak<ThreadInner> = Rc::downgrade(inner);
                let resource_copy = resource.clone();
                let callback = callback.clone();
                Some(database.request(
                    resource,
                    Box::new(move |response: Response| {
                        let Some(this) = weak.upgrade() else {
                            return;
                        };
                        let mut res = resource_copy.clone();

                        // Resource is in the cache
                        if !response.no_content {
                            if response.is_usable() {
                                callback(response.clone());
                                // Set the priority of existing resource to
                                // low if it's expired but usable.
                                res.set_priority(Priority::Low);
                            } else {
                                // Set prior data only if it was not returned
                                // to the requester. Once we get 304 response
                                // from the network, we will forward response
                                // to the requester.
                                res.prior_data = response.data.clone();
                            }

                            // Copy response fields for cache control request
                            res.prior_modified = response.modified;
                            res.prior_expires = response.expires;
                            res.prior_etag = response.etag.clone();
                        }

                        let parent = this.tasks.borrow_mut().remove(&req);
                        let task = this.request_from_network(&resource_copy, &res, parent, callback.clone());
                        if let Some(task) = task {
                            this.tasks.borrow_mut().insert(req, task);
                        }
                    }),
                ))
            }
        } else {
            // Get from the online file source
            inner.request_from_network(resource, resource, None, callback.clone())
        };

        match task {
            Some(task) => {
                inner.tasks.borrow_mut().insert(req, task);
            }
            None => {
                // If no new tasks were added, notify client that request
                // cannot be processed.
                let mut response = Response::new();
                response.no_content = true;
                response.error = Some(Box::new(Error::new(Reason::Other, "Unsupported resource request.")));
                callback(response);
            }
        }
    }

    /// `cancel(AsyncRequest* req)`.
    pub fn cancel(&self, req: u64) {
        let task = self.inner.tasks.borrow_mut().remove(&req);
        drop(task);
    }
}

/// `MainResourceLoader::Impl`. `thread` comes first: it is dropped (joined)
/// before the file sources, as C++ destroys members in reverse order.
struct Impl {
    thread: Thread<MainResourceLoaderThread>,
    asset_file_source: FileSourcePtr,
    database_file_source: FileSourcePtr,
    local_file_source: FileSourcePtr,
    online_file_source: FileSourcePtr,
    mbtiles_file_source: FileSourcePtr,
    pmtiles_file_source: FileSourcePtr,
    supports_cache_only_requests: bool,
    resource_options: Mutex<ResourceOptions>,
}

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

impl Impl {
    #[allow(clippy::too_many_arguments)]
    fn new(
        resource_options: &ResourceOptions,
        asset_file_source: FileSourcePtr,
        database_file_source: FileSourcePtr,
        local_file_source: FileSourcePtr,
        online_file_source: FileSourcePtr,
        mbtiles_file_source: FileSourcePtr,
        pmtiles_file_source: FileSourcePtr,
    ) -> Self {
        let sources = (
            asset_file_source.clone(),
            database_file_source.clone(),
            local_file_source.clone(),
            online_file_source.clone(),
            mbtiles_file_source.clone(),
            pmtiles_file_source.clone(),
        );
        // TODO(port): makeThreadPrioritySetter(EXPERIMENTAL_THREAD_PRIORITY_WORKER).
        let thread = Thread::new("ResourceLoaderThread", move |_| {
            let (asset, database, local, online, mbtiles, pmtiles) = sources;
            MainResourceLoaderThread::new(asset, database, local, online, mbtiles, pmtiles)
        });
        Impl {
            thread,
            supports_cache_only_requests: database_file_source.is_some(),
            asset_file_source,
            database_file_source,
            local_file_source,
            online_file_source,
            mbtiles_file_source,
            pmtiles_file_source,
            resource_options: Mutex::new(resource_options.clone()),
        }
    }

    fn request(&self, resource: &Resource, callback: Callback) -> Box<dyn AsyncRequest> {
        let mut req = FileSourceRequest::new(callback);
        let req_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);

        let actor_ref = self.thread.actor();
        req.on_cancel(Box::new(move || {
            actor_ref.invoke(move |t: &MainResourceLoaderThread| t.cancel(req_id));
        }));
        let req_ref = req.actor();
        let resource = resource.clone();
        self.thread.actor().invoke(move |t: &MainResourceLoaderThread| t.request(req_id, &resource, &req_ref));
        Box::new(req)
    }

    fn all(&self) -> [&FileSourcePtr; 6] {
        [
            &self.asset_file_source,
            &self.local_file_source,
            &self.database_file_source,
            &self.online_file_source,
            &self.mbtiles_file_source,
            &self.pmtiles_file_source,
        ]
    }

    fn in_member_order(&self) -> [&FileSourcePtr; 6] {
        [
            &self.asset_file_source,
            &self.database_file_source,
            &self.local_file_source,
            &self.online_file_source,
            &self.mbtiles_file_source,
            &self.pmtiles_file_source,
        ]
    }

    fn can_request(&self, resource: &Resource) -> bool {
        self.all().iter().any(|fs| fs.as_ref().is_some_and(|fs| fs.can_request(resource)))
    }
}

/// `MainResourceLoader`: the `ResourceLoader` file source.
pub struct MainResourceLoader {
    impl_: Impl,
}

impl MainResourceLoader {
    /// A loader over `file_sources`' asset, database, file system,
    /// network, MBTiles and PMTiles file sources for these options.
    pub fn new(file_sources: &FileSourceManager, resource_options: &ResourceOptions) -> Self {
        let get = |type_| file_sources.get_file_source(type_, resource_options);
        MainResourceLoader {
            impl_: Impl::new(
                resource_options,
                get(FileSourceType::Asset),
                get(FileSourceType::Database),
                get(FileSourceType::FileSystem),
                get(FileSourceType::Network),
                get(FileSourceType::Mbtiles),
                get(FileSourceType::Pmtiles),
            ),
        }
    }
}

impl FileSource for MainResourceLoader {
    fn supports_cache_only_requests(&self) -> bool {
        self.impl_.supports_cache_only_requests
    }

    fn request(&self, resource: &Resource, callback: Callback) -> Box<dyn AsyncRequest> {
        self.impl_.request(resource, callback)
    }

    fn can_request(&self, resource: &Resource) -> bool {
        self.impl_.can_request(resource)
    }

    fn pause(&self) {
        self.impl_.thread.pause();
    }

    fn resume(&self) {
        self.impl_.thread.resume();
    }

    fn set_resource_options(&self, options: ResourceOptions) {
        let mut resource_options = self.impl_.resource_options.lock_unpoisoned();
        *resource_options = options.clone();
        for fs in self.impl_.in_member_order().into_iter().flatten() {
            fs.set_resource_options(options.clone());
        }
    }

    fn get_resource_options(&self) -> ResourceOptions {
        self.impl_.resource_options.lock_unpoisoned().clone()
    }
}
