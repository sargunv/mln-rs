// Ports include/mln/storage/database_file_source.hpp and
// platform/default/src/mln/storage/database_file_source.cpp.
//
// The database runs on its own thread (`util::Thread<DatabaseFileSourceThread>`
// "DatabaseFileSource"); requests reach it as actor messages and the
// response goes back to the requesting thread's `FileSourceRequest`.
//
// C++ `DatabaseFileSourceThread` reads the tile server options from the
// online file source (`onlineFileSource_->getResourceOptions()`); so does
// the port, falling back to the ResourceOptions only when there is no
// Network file source (C++ would dereference null).
//
// The offline region API needs `OfflineDownload`, which parses styles and so
// sits above this crate: mln_storage_offline adds it to `DatabaseFileSource`
// through `invoke` and `DatabaseFileSourceThread::extension`.

use std::any::Any;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use feature::Value;
use mln_actor::{ActorRef, Thread, Timer, scheduler};
use mln_util::chrono::Duration;
use mln_util::constants::{ASSET_PROTOCOL, FILE_PROTOCOL};
use mln_util::logging::{Event, Log};
use mln_util::sync::MutexExt;
use mln_util::tile_server_options::TileServerOptions;

use crate::async_request::AsyncRequest;
use crate::file_source::{Callback, FileSource, FileSourceType, READ_ONLY_MODE_KEY};
use crate::file_source_manager::FileSourceManager;
use crate::file_source_request::{FileSourceRequest, FileSourceRequestState};
use crate::offline_database::{DatabaseError, OfflineDatabase};
use crate::resource::{LoadingMethod, Resource, StoragePolicy};
use crate::resource_options::ResourceOptions;
use crate::response::{Error, Reason, Response};

/// How long cache hits' `accessed` timestamps wait, so that the reads of a
/// map load share one write.
const ACCESSED_FLUSH_DELAY: Duration = Duration::from_millis(250);

/// `DatabaseFileSourceThread`: owns the OfflineDatabase on the database
/// thread.
pub struct DatabaseFileSourceThread {
    db: Rc<RefCell<OfflineDatabase>>,
    online_file_source: Option<Arc<dyn FileSource>>,
    /// The state extensions keep on the database thread, one per type.
    extensions: RefCell<Vec<Rc<dyn Any>>>,
    self_ref: ActorRef<DatabaseFileSourceThread>,
    accessed_flush_timer: Timer,
    /// A flush of the `accessed` timestamps is scheduled. A failed flush
    /// isn't rescheduled until the next cache hit, so an idle map stays
    /// idle.
    accessed_flush_scheduled: Arc<AtomicBool>,
}

impl DatabaseFileSourceThread {
    fn new(
        self_ref: ActorRef<DatabaseFileSourceThread>,
        online_file_source: Option<Arc<dyn FileSource>>,
        cache_path: &str,
        tile_server_options: &TileServerOptions,
    ) -> Self {
        let tile_server_options = match &online_file_source {
            Some(online) => online.get_resource_options().tile_server_options(),
            None => tile_server_options.clone(),
        };
        let mut db = OfflineDatabase::new(cache_path, &tile_server_options);
        let accessed_flush_scheduled = Arc::new(AtomicBool::new(false));
        let (scheduled, actor) = (accessed_flush_scheduled.clone(), self_ref.clone());
        db.set_accessed_callback(Box::new(move || {
            if !scheduled.swap(true, Ordering::Relaxed) {
                actor.invoke(DatabaseFileSourceThread::start_accessed_flush_timer);
            }
        }));
        DatabaseFileSourceThread {
            db: Rc::new(RefCell::new(db)),
            online_file_source,
            extensions: RefCell::new(Vec::new()),
            self_ref,
            accessed_flush_timer: Timer::new(),
            accessed_flush_scheduled,
        }
    }

    fn start_accessed_flush_timer(&self) {
        let actor = self.self_ref.clone();
        self.accessed_flush_timer.start(ACCESSED_FLUSH_DELAY, Duration::zero(), move || {
            actor.invoke(|t: &DatabaseFileSourceThread| {
                t.accessed_flush_scheduled.store(false, Ordering::Relaxed);
                t.db.borrow_mut().flush_accessed();
            });
        });
    }

    /// `request(const Resource&, const ActorRef<FileSourceRequest>&)`.
    pub fn request(&self, resource: &Resource, req: &ActorRef<FileSourceRequestState>) {
        let mut offline_response =
            if resource.storage_policy != StoragePolicy::Volatile { self.db.borrow_mut().get(resource) } else { None };
        match &mut offline_response {
            None => {
                let mut response = Response::new();
                response.no_content = true;
                response.error = Some(Box::new(Error::new(Reason::NotFound, "Not found in offline database")));
                offline_response = Some(response);
            }
            Some(response) => {
                if !response.is_usable() {
                    response.error = Some(Box::new(Error::new(Reason::NotFound, "Cached resource is unusable")));
                }
            }
        }
        let response = offline_response.expect("response");
        req.invoke(move |request: &FileSourceRequestState| request.set_response(response));
    }

    /// `setDatabasePath(path, callback)`.
    pub fn set_database_path(&self, path: &str, callback: Option<Box<dyn FnOnce() + Send>>) {
        self.db.borrow_mut().change_path(path);
        if let Some(callback) = callback {
            callback();
        }
    }

    /// `forward(resource, response, callback)`.
    pub fn forward(&self, resource: &Resource, response: &Response, callback: Option<Box<dyn FnMut() + Send>>) {
        self.db.borrow_mut().put(resource, response);
        if let Some(mut callback) = callback {
            callback();
        }
    }

    /// `resetDatabase(callback)`.
    pub fn reset_database(&self, callback: impl FnOnce(Result<(), DatabaseError>)) {
        callback(into_result(self.db.borrow_mut().reset_database()));
    }

    /// `packDatabase(callback)`.
    pub fn pack_database(&self, callback: impl FnOnce(Result<(), DatabaseError>)) {
        callback(into_result(self.db.borrow_mut().pack()));
    }

    /// `runPackDatabaseAutomatically(bool)`.
    pub fn run_pack_database_automatically(&self, autopack: bool) {
        self.db.borrow_mut().run_pack_database_automatically(autopack);
    }

    /// `put(resource, response)`.
    pub fn put(&self, resource: &Resource, response: &Response) {
        self.db.borrow_mut().put(resource, response);
    }

    /// `invalidateAmbientCache(callback)`.
    pub fn invalidate_ambient_cache(&self, callback: impl FnOnce(Result<(), DatabaseError>)) {
        callback(into_result(self.db.borrow_mut().invalidate_ambient_cache()));
    }

    /// `clearAmbientCache(callback)`.
    pub fn clear_ambient_cache(&self, callback: impl FnOnce(Result<(), DatabaseError>)) {
        callback(into_result(self.db.borrow_mut().clear_ambient_cache()));
    }

    /// `setMaximumAmbientCacheSize(size, callback)`.
    pub fn set_maximum_ambient_cache_size(&self, size: u64, callback: impl FnOnce(Result<(), DatabaseError>)) {
        callback(into_result(self.db.borrow_mut().set_maximum_ambient_cache_size(size)));
    }

    /// `setOfflineMapboxTileCountLimit(uint64_t)`.
    pub fn set_offline_mapbox_tile_count_limit(&self, limit: u64) {
        self.db.borrow_mut().set_offline_mapbox_tile_count_limit(limit);
    }

    /// `reopenDatabaseReadOnly(bool)`.
    pub fn reopen_database_read_only(&self, read_only: bool) {
        self.db.borrow_mut().reopen_database_read_only(read_only);
    }

    /// The database. Borrow it only for the duration of one call: requests
    /// and other messages on this thread borrow it too.
    pub fn database(&self) -> &Rc<RefCell<OfflineDatabase>> {
        &self.db
    }

    /// The Network file source, if there is one.
    pub fn online_file_source(&self) -> Option<&Arc<dyn FileSource>> {
        self.online_file_source.as_ref()
    }

    /// The `T` this thread keeps for an extension, created on first use. It
    /// lives on the database thread and is dropped with the database.
    pub fn extension<T: Default + 'static>(&self) -> Rc<T> {
        let existing = self.extensions.borrow().iter().find_map(|extension| extension.clone().downcast::<T>().ok());
        existing.unwrap_or_else(|| {
            let extension = Rc::new(T::default());
            self.extensions.borrow_mut().push(extension.clone());
            extension
        })
    }
}

/// `std::exception_ptr` (null on success) as a `Result`.
fn into_result(error: Option<DatabaseError>) -> Result<(), DatabaseError> {
    error.map_or(Ok(()), Err)
}

/// `DatabaseFileSource::Impl`.
struct Impl {
    thread: Thread<DatabaseFileSourceThread>,
    resource_options: Mutex<ResourceOptions>,
}

impl Impl {
    fn actor(&self) -> ActorRef<DatabaseFileSourceThread> {
        self.thread.actor()
    }
}

/// `DatabaseFileSource`.
pub struct DatabaseFileSource {
    impl_: Impl,
}

impl DatabaseFileSource {
    /// A database at `resource_options`' cache path. Offline downloads go
    /// through `file_sources`' network file source.
    pub fn new(file_sources: &FileSourceManager, resource_options: &ResourceOptions) -> Self {
        let online_file_source = file_sources.get_file_source(FileSourceType::Network, resource_options);
        let cache_path = resource_options.cache_path().to_owned();
        let tile_server_options = resource_options.tile_server_options();
        // TODO(port): makeThreadPrioritySetter(EXPERIMENTAL_THREAD_PRIORITY_DATABASE).
        let thread = Thread::new("DatabaseFileSource", move |self_ref| {
            DatabaseFileSourceThread::new(self_ref, online_file_source, &cache_path, &tile_server_options)
        });
        DatabaseFileSource { impl_: Impl { thread, resource_options: Mutex::new(resource_options.clone()) } }
    }

    /// `setDatabasePath(path, callback)`. The callback runs on the database
    /// thread, as in C++.
    pub fn set_database_path(&self, path: &str, callback: Option<Box<dyn FnOnce() + Send>>) {
        let path = path.to_owned();
        self.impl_.actor().invoke(move |t: &DatabaseFileSourceThread| t.set_database_path(&path, callback));
    }

    /// `resetDatabase(callback)`: deletes the database file and starts an
    /// empty one. The callback runs on the database thread.
    pub fn reset_database(&self, callback: impl FnOnce(Result<(), DatabaseError>) + Send + 'static) {
        self.invoke(move |t| t.reset_database(callback));
    }

    /// `packDatabase(callback)`: vacuums the database file to the space its
    /// data needs. The callback runs on the database thread.
    pub fn pack_database(&self, callback: impl FnOnce(Result<(), DatabaseError>) + Send + 'static) {
        self.invoke(move |t| t.pack_database(callback));
    }

    /// `runPackDatabaseAutomatically(bool)`: whether deleting a region or
    /// clearing the ambient cache packs the database (the default).
    pub fn run_pack_database_automatically(&self, autopack: bool) {
        self.invoke(move |t| t.run_pack_database_automatically(autopack));
    }

    /// `put(resource, response)`.
    pub fn put(&self, resource: &Resource, response: &Response) {
        let resource = resource.clone();
        let response = response.clone();
        self.invoke(move |t| t.put(&resource, &response));
    }

    /// `invalidateAmbientCache(callback)`: makes the ambient cache's
    /// resources revalidate with their server before use. Offline regions'
    /// resources are unaffected. The callback runs on the database thread.
    pub fn invalidate_ambient_cache(&self, callback: impl FnOnce(Result<(), DatabaseError>) + Send + 'static) {
        self.invoke(move |t| t.invalidate_ambient_cache(callback));
    }

    /// `clearAmbientCache(callback)`: erases the ambient cache's resources.
    /// Offline regions' resources are unaffected. The callback runs on the
    /// database thread.
    pub fn clear_ambient_cache(&self, callback: impl FnOnce(Result<(), DatabaseError>) + Send + 'static) {
        self.invoke(move |t| t.clear_ambient_cache(callback));
    }

    /// `setMaximumAmbientCacheSize(size, callback)`: limits the database to
    /// `size` bytes, evicting ambient resources to fit. Offline regions
    /// count towards the limit but are never evicted; 0 disables the
    /// ambient cache. The callback runs on the database thread.
    pub fn set_maximum_ambient_cache_size(
        &self,
        size: u64,
        callback: impl FnOnce(Result<(), DatabaseError>) + Send + 'static,
    ) {
        self.invoke(move |t| t.set_maximum_ambient_cache_size(size, callback));
    }

    /// `setOfflineMapboxTileCountLimit(uint64_t)`.
    pub fn set_offline_mapbox_tile_count_limit(&self, limit: u64) {
        self.invoke(move |t| t.set_offline_mapbox_tile_count_limit(limit));
    }

    /// Runs `f` on the database thread, after the requests and operations
    /// sent before it.
    pub fn invoke(&self, f: impl FnOnce(&DatabaseFileSourceThread) + Send + 'static) {
        self.impl_.actor().invoke(f);
    }
}

impl FileSource for DatabaseFileSource {
    fn request(&self, resource: &Resource, callback: Callback) -> Box<dyn AsyncRequest> {
        let req = FileSourceRequest::new(callback);
        let req_ref = req.actor();
        let resource = resource.clone();
        self.impl_.actor().invoke(move |t: &DatabaseFileSourceThread| t.request(&resource, &req_ref));
        Box::new(req)
    }

    fn forward(&self, res: &Resource, response: &Response, callback: Option<Box<dyn FnOnce() + Send>>) {
        if res.storage_policy == StoragePolicy::Volatile {
            return;
        }

        let wrapper: Option<Box<dyn FnMut() + Send>> =
            callback.map(|callback| Box::new(scheduler::bind_once(&scheduler::get_current(), callback)) as _);
        let res = res.clone();
        let response = response.clone();
        self.impl_.actor().invoke(move |t: &DatabaseFileSourceThread| t.forward(&res, &response, wrapper));
    }

    fn can_request(&self, resource: &Resource) -> bool {
        resource.has_loading_method(LoadingMethod::Cache)
            && !resource.url.starts_with(ASSET_PROTOCOL)
            && !resource.url.starts_with(FILE_PROTOCOL)
    }

    fn set_property(&self, key: &str, value: &Value) {
        match value {
            Value::Bool(read_only) if key == READ_ONLY_MODE_KEY => {
                let read_only = *read_only;
                self.impl_.actor().invoke(move |t: &DatabaseFileSourceThread| t.reopen_database_read_only(read_only));
            }
            _ => {
                let message = format!("Resource provider does not support property {key}");
                Log::error(Event::General, message);
            }
        }
    }

    fn pause(&self) {
        self.impl_.thread.pause();
    }

    fn resume(&self) {
        self.impl_.thread.resume();
    }

    fn set_resource_options(&self, options: ResourceOptions) {
        *self.impl_.resource_options.lock_unpoisoned() = options;
    }

    fn get_resource_options(&self) -> ResourceOptions {
        self.impl_.resource_options.lock_unpoisoned().clone()
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }
}
