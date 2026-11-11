//! A runtime: the thread that its maps live on, and the file sources, cache
//! and offline regions that they share.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::ThreadId;

use mln_actor::{RunLoop, RunLoopHandle};
use mln_storage::http_header_transform::{HttpHeaderTransform, HttpHeaders};
use mln_storage::resource::Kind;
use mln_storage::resource_transform::{FinishedCallback, ResourceTransform};
use mln_storage::{DatabaseFileSource, FileSource, FileSourceManager, FileSourceType, ResourceOptions};
use mln_text::GlyphCache;
use mln_util::error::Error;
use mln_util::sync::MutexExt;
use mln_util::tile_server_options::TileServerOptions;

use crate::completion::{Completion, Resolver};
use crate::event::{EventHandler, RuntimeEvent};
use crate::map::{Applied, Map, MapEntry, MapId, MapOptions, MapShared, Published};
use crate::offline::Offline;
use crate::resources::{CacheDatabaseOperation, ProviderFileSource, ResourceHooks, ResourceProvider};

/// Where a runtime keeps its assets and its cache.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuntimeOptions {
    /// The directory that `asset://` URLs load from. `None` uses the working
    /// directory.
    pub asset_path: Option<PathBuf>,
    /// The cache database file, which holds the ambient cache and the offline
    /// regions. Its directory must be writable. `None` keeps the cache in
    /// memory.
    pub cache_path: Option<PathBuf>,
}

/// A runtime: a thread of its own that holds maps, and the resource loading,
/// cache and offline regions that they share.
///
/// Every call returns at once. Commands run on the runtime's thread in the
/// order they were sent, across the runtime and all its maps, and report
/// through a [`Completion`]. `Clone` is another handle to the same runtime,
/// and any thread may use one.
///
/// The runtime lives until [`release`](Self::release) or
/// [`dispose`](Self::dispose), or until its last handle and its last map
/// are dropped. A handler or provider that holds a handle keeps it alive
/// until then.
#[derive(Clone)]
pub struct Runtime {
    pub(crate) shared: Arc<RuntimeShared>,
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime").field("thread", &self.shared.thread).finish()
    }
}

struct Lifecycle {
    released: bool,
    live_maps: usize,
    shut_down: bool,
}

pub(crate) struct RuntimeShared {
    run_loop: RunLoopHandle,
    thread: ThreadId,
    lifecycle: Mutex<Lifecycle>,
    next_map_id: AtomicU64,
    /// The glyphs that the renderers of the runtime's maps parse, which they
    /// share.
    pub(crate) glyph_cache: Arc<GlyphCache>,
}

impl RuntimeShared {
    /// Runs `task` on the runtime's thread, after everything sent before it.
    pub(crate) fn post(&self, task: impl FnOnce(&mut RuntimeCore, &mut Outbox) + Send + 'static) {
        post(&self.run_loop, task);
    }

    /// A completion that the runtime's thread resolves.
    pub(crate) fn completion<T: Send + 'static>(&self) -> (Completion<T>, Resolver<T>) {
        Completion::new(Some(self.thread))
    }

    fn check_live(&self) -> Result<(), Error> {
        if self.lifecycle.lock_unpoisoned().released { Err(released()) } else { Ok(()) }
    }

    /// A map of the runtime was released or dropped. A disposed runtime shuts
    /// down with its last map.
    pub(crate) fn map_released(&self) {
        let shut_down = {
            let mut lifecycle = self.lifecycle.lock_unpoisoned();
            lifecycle.live_maps -= 1;
            let shut_down = lifecycle.released && lifecycle.live_maps == 0 && !lifecycle.shut_down;
            lifecycle.shut_down |= shut_down;
            shut_down
        };
        if shut_down {
            self.shut_down(None);
        }
    }

    /// Tears the runtime down on its thread, after the commands sent before.
    fn shut_down(&self, done: Option<Resolver<()>>) {
        let run_loop = self.run_loop.clone();
        self.run_loop.invoke(move || {
            let core = CORE.with(|core| core.borrow_mut().take());
            drop(core);
            if let Some(done) = done {
                done.resolve(Ok(()));
            }
            run_loop.stop();
        });
    }
}

impl Drop for RuntimeShared {
    fn drop(&mut self) {
        if !self.lifecycle.get_mut_unpoisoned().shut_down {
            self.shut_down(None);
        }
    }
}

fn released() -> Error {
    Error::invalid_state("the runtime was released")
}

thread_local! {
    /// The state of the runtime whose thread this is.
    static CORE: RefCell<Option<RuntimeCore>> = const { RefCell::new(None) };
}

/// Posts `task` to the runtime thread whose loop is `run_loop`.
pub(crate) fn post(run_loop: &RunLoopHandle, task: impl FnOnce(&mut RuntimeCore, &mut Outbox) + Send + 'static) {
    run_loop.invoke(move || run(task));
}

/// Runs `task` on the runtime's state, then delivers what it sends to the
/// host, with the state unborrowed. A task that comes after the runtime shut
/// down drops, which cancels its completions.
fn run(task: impl FnOnce(&mut RuntimeCore, &mut Outbox)) {
    let mut outbox = Outbox::default();
    let unrun = CORE.with(|core| match core.borrow_mut().as_mut() {
        Some(core) => {
            task(core, &mut outbox);
            None
        }
        None => Some(task),
    });
    drop(unrun);
    outbox.deliver();
}

/// What a task sends to the host: event batches and results, in order. They
/// go out once the task is done with the runtime's state, so host code that
/// runs never finds it borrowed.
#[derive(Default)]
pub(crate) struct Outbox {
    deliveries: Vec<Box<dyn FnOnce()>>,
}

impl Outbox {
    pub(crate) fn push(&mut self, delivery: impl FnOnce() + 'static) {
        self.deliveries.push(Box::new(delivery));
    }

    pub(crate) fn resolve<T: 'static>(&mut self, resolver: Resolver<T>, result: Result<T, Error>) {
        self.push(move || resolver.resolve(result));
    }

    pub(crate) fn deliver(self) {
        for delivery in self.deliveries {
            delivery();
        }
    }
}

/// The runtime's state, on its thread.
pub(crate) struct RuntimeCore {
    /// Declared first, so the maps go before the file sources they use.
    pub(crate) maps: BTreeMap<MapId, MapEntry>,
    pub(crate) event_handler: Option<EventHandler>,
    hooks: Arc<ResourceHooks>,
    file_sources: FileSourceManager,
    resource_options: ResourceOptions,
    /// The runtime's file sources, held for its life so that each map, and
    /// each transform, finds the same ones.
    _loader: Option<Arc<dyn FileSource>>,
    database: Option<Arc<dyn FileSource>>,
    network: Option<Arc<dyn FileSource>>,
    pub(crate) run_loop: RunLoopHandle,
}

impl RuntimeCore {
    fn new(resource_options: ResourceOptions, run_loop: RunLoopHandle) -> Self {
        let hooks = Arc::new(ResourceHooks::default());
        let file_sources = FileSourceManager::new();
        let network_hooks = hooks.clone();
        file_sources.register_file_source_factory(
            FileSourceType::Network,
            Arc::new(move |_: &FileSourceManager, options: &ResourceOptions| {
                Arc::new(ProviderFileSource::new(options, network_hooks.clone())) as Arc<dyn FileSource>
            }),
        );
        let network = file_sources.get_file_source(FileSourceType::Network, &resource_options);
        let database = file_sources.get_file_source(FileSourceType::Database, &resource_options);
        let loader = file_sources.get_file_source(FileSourceType::ResourceLoader, &resource_options);
        RuntimeCore {
            maps: BTreeMap::new(),
            event_handler: None,
            hooks,
            file_sources,
            resource_options,
            _loader: loader,
            database,
            network,
            run_loop,
        }
    }

    pub(crate) fn file_sources(&self) -> (&FileSourceManager, &ResourceOptions) {
        (&self.file_sources, &self.resource_options)
    }

    /// Sends `events` to the event handler.
    pub(crate) fn send_events(&self, events: Vec<RuntimeEvent>, outbox: &mut Outbox) {
        if events.is_empty() {
            return;
        }
        if let Some(handler) = self.event_handler.clone() {
            outbox.push(move || handler(&events));
        }
    }

    /// The cache database, which every runtime has.
    pub(crate) fn database(&self) -> Result<&DatabaseFileSource, Error> {
        self.database
            .as_ref()
            .and_then(|database| database.as_any())
            .and_then(|database| database.downcast_ref::<DatabaseFileSource>())
            .ok_or_else(|| Error::unsupported("the runtime has no cache database"))
    }

    fn network(&self) -> Result<&Arc<dyn FileSource>, Error> {
        self.network.as_ref().ok_or_else(|| Error::unsupported("the runtime has no network file source"))
    }
}

/// Delivers a database result on the runtime's thread, so it keeps its
/// place among the runtime's other results and events.
pub(crate) fn reply_on<T: Send + 'static, E: Into<Error>>(
    run_loop: RunLoopHandle,
    resolver: Resolver<T>,
) -> impl FnOnce(Result<T, E>) + Send + 'static {
    move |result| {
        let result = result.map_err(Into::into);
        post(&run_loop, move |_, outbox| outbox.resolve(resolver, result));
    }
}

/// The runtime's resource options, with the cache file created when there
/// is one, so that a path the runtime can't use fails here. URLs of the
/// `maplibre://` scheme name MapLibre's demo tile server.
fn resource_options(options: &RuntimeOptions) -> Result<ResourceOptions, Error> {
    let mut resource_options =
        ResourceOptions::default().with_tile_server_options(TileServerOptions::default_configuration());
    if let Some(asset_path) = &options.asset_path {
        resource_options = resource_options.with_asset_path(utf8(asset_path, "asset path")?);
    }
    if let Some(cache_path) = &options.cache_path {
        let path = utf8(cache_path, "cache path")?;
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(cache_path)
            .map_err(|error| Error::invalid_argument(format!("can't open the cache database {path}: {error}")))?;
        resource_options = resource_options.with_cache_path(path);
    }
    Ok(resource_options)
}

fn utf8<'a>(path: &'a Path, what: &str) -> Result<&'a str, Error> {
    path.to_str().ok_or_else(|| Error::invalid_argument(format!("the {what} {} isn't UTF-8", path.display())))
}

impl Runtime {
    /// Starts a runtime and its thread.
    ///
    /// Fails with [`ErrorKind::InvalidArgument`](mln_util::ErrorKind) when the
    /// cache database file can't be opened or created.
    pub fn new(options: RuntimeOptions) -> Result<Runtime, Error> {
        let resource_options = resource_options(&options)?;
        let (started, start) = mpsc::channel();
        let thread = mln_util::thread::builder("Runtime")
            .spawn(move || {
                let run_loop = RunLoop::new();
                let core = RuntimeCore::new(resource_options, run_loop.handle());
                CORE.with(|cell| *cell.borrow_mut() = Some(core));
                let _ = started.send(run_loop.handle());
                run_loop.run();
                let core = CORE.with(|cell| cell.borrow_mut().take());
                drop(core);
            })
            .map_err(|error| Error::native(format!("can't start the runtime's thread: {error}")))?;
        let thread_id = thread.thread().id();
        // The thread ends by itself once the runtime shuts down, so nothing
        // joins it: a runtime may shut down from its own thread.
        drop(thread);
        let run_loop = start.recv().map_err(|_| Error::native("the runtime's thread stopped as it started"))?;
        Ok(Runtime {
            shared: Arc::new(RuntimeShared {
                run_loop,
                thread: thread_id,
                lifecycle: Mutex::new(Lifecycle { released: false, live_maps: 0, shut_down: false }),
                next_map_id: AtomicU64::new(1),
                glyph_cache: Arc::default(),
            }),
        })
    }

    /// Sends a command to the runtime's thread.
    fn command<T: Send + 'static>(
        &self,
        command: impl FnOnce(&mut RuntimeCore, &mut Outbox, Resolver<T>) + Send + 'static,
    ) -> Result<Completion<T>, Error> {
        self.shared.check_live()?;
        let (completion, resolver) = self.shared.completion();
        self.shared.post(move |core, outbox| command(core, outbox, resolver));
        Ok(completion)
    }

    /// Sends a command that runs on the runtime's state and completes at once.
    fn apply<T: Send + 'static>(
        &self,
        apply: impl FnOnce(&mut RuntimeCore) -> Result<T, Error> + Send + 'static,
    ) -> Result<Completion<T>, Error> {
        self.command(move |core, outbox, resolver| {
            let result = apply(core);
            outbox.resolve(resolver, result);
        })
    }

    /// Creates a map on the runtime.
    ///
    /// The map's handle is ready at once, and the commands sent to it wait
    /// for the map to be built. If building fails, the completion reports
    /// why and those commands fail with
    /// [`ErrorKind::InvalidState`](mln_util::ErrorKind).
    ///
    /// Fails with [`ErrorKind::InvalidArgument`](mln_util::ErrorKind) for an
    /// empty size or a pixel ratio that isn't finite and positive.
    pub fn create_map(&self, options: MapOptions) -> Result<(Map, Completion<Applied>), Error> {
        let size = options.map.size();
        let pixel_ratio = options.map.pixel_ratio();
        if size.is_empty() {
            return Err(Error::invalid_argument("the map's size must not be empty"));
        }
        if !pixel_ratio.is_finite() || pixel_ratio <= 0.0 {
            return Err(Error::invalid_argument("the map's pixel ratio must be finite and positive"));
        }
        {
            let mut lifecycle = self.shared.lifecycle.lock_unpoisoned();
            if lifecycle.released {
                return Err(released());
            }
            lifecycle.live_maps += 1;
        }
        let id = MapId(self.shared.next_map_id.fetch_add(1, Ordering::Relaxed));
        let published = Arc::new(Published::new(options.event_mask));
        let map = Map::new(MapShared::new(id, self.shared.clone(), &options.map, published.clone()));
        let (completion, resolver) = self.shared.completion();
        self.shared.post(move |core, outbox| crate::map::create(core, id, options, published, resolver, outbox));
        Ok((map, completion))
    }

    /// Sends the events of the runtime and its maps to `handler`, in batches
    /// on the runtime's thread. It replaces the handler before.
    pub fn set_event_handler(
        &self,
        handler: impl Fn(&[RuntimeEvent]) + Send + Sync + 'static,
    ) -> Result<Completion<()>, Error> {
        let handler: EventHandler = Arc::new(handler);
        self.command(move |core, outbox, resolver| {
            let previous = core.event_handler.replace(handler);
            outbox.push(move || drop(previous));
            outbox.resolve(resolver, Ok(()));
        })
    }

    /// Stops sending events to a handler.
    pub fn clear_event_handler(&self) -> Result<Completion<()>, Error> {
        self.command(move |core, outbox, resolver| {
            let previous = core.event_handler.take();
            outbox.push(move || drop(previous));
            outbox.resolve(resolver, Ok(()));
        })
    }

    /// Serves the runtime's network requests that `provider` serves through
    /// it, and the rest as usual. It replaces the provider before.
    pub fn set_resource_provider(&self, provider: impl ResourceProvider) -> Result<Completion<()>, Error> {
        let provider: Arc<dyn ResourceProvider> = Arc::new(provider);
        self.replace_provider(Some(provider))
    }

    /// Removes the resource provider, so the library loads every request
    /// itself.
    pub fn clear_resource_provider(&self) -> Result<Completion<()>, Error> {
        self.replace_provider(None)
    }

    fn replace_provider(&self, provider: Option<Arc<dyn ResourceProvider>>) -> Result<Completion<()>, Error> {
        self.command(move |core, outbox, resolver| {
            let previous = core.hooks.set_provider(provider);
            outbox.push(move || drop(previous));
            outbox.resolve(resolver, Ok(()));
        })
    }

    /// Rewrites the URLs of the runtime's network requests before they go
    /// out: `transform` gets the resource's kind and URL, and answers through
    /// the callback, now or later and on any thread, with the URL to load or
    /// the error to fail the request with. It replaces the transform before.
    pub fn set_resource_transform(
        &self,
        transform: impl Fn(Kind, &str, FinishedCallback) + Send + Sync + 'static,
    ) -> Result<Completion<()>, Error> {
        let transform = ResourceTransform::new(Some(Arc::new(transform)));
        self.apply(move |core| {
            core.network()?.set_resource_transform(transform);
            core.hooks.set_transform_set(true);
            Ok(())
        })
    }

    /// Stops rewriting request URLs.
    pub fn clear_resource_transform(&self) -> Result<Completion<()>, Error> {
        self.apply(move |core| {
            core.network()?.set_resource_transform(ResourceTransform::default());
            core.hooks.set_transform_set(false);
            Ok(())
        })
    }

    /// Adds the headers that `transform` returns to the runtime's HTTP
    /// requests: it gets the resource's kind and the URL after any resource
    /// transform, and runs for each attempt of a request, which waits for
    /// it. Names that the library manages, such as `Range` or `User-Agent`,
    /// are dropped, and a later value of a name replaces an earlier one. It
    /// replaces the transform before.
    ///
    /// Fails with [`ErrorKind::Unsupported`](mln_util::ErrorKind) in the
    /// browser, where the browser sends the requests.
    pub fn set_http_header_transform(
        &self,
        transform: impl Fn(Kind, &str) -> HttpHeaders + Send + Sync + 'static,
    ) -> Result<Completion<()>, Error> {
        if cfg!(target_family = "wasm") {
            return Err(Error::unsupported("the browser sends the requests and sets their headers"));
        }
        let transform = HttpHeaderTransform::new(Some(Arc::new(move |kind: Kind, url: &str| {
            crate::resources::sanitize_headers(transform(kind, url))
        })));
        self.apply(move |core| {
            core.network()?.set_http_header_transform(transform);
            Ok(())
        })
    }

    /// Stops adding headers to HTTP requests.
    pub fn clear_http_header_transform(&self) -> Result<Completion<()>, Error> {
        self.apply(move |core| {
            core.network()?.set_http_header_transform(HttpHeaderTransform::default());
            Ok(())
        })
    }

    /// Runs a maintenance action on the cache database.
    pub fn run_cache_database_operation(&self, operation: CacheDatabaseOperation) -> Result<Completion<()>, Error> {
        self.command(move |core, outbox, resolver| {
            let database = match core.database() {
                Ok(database) => database,
                Err(error) => return outbox.resolve(resolver, Err(error)),
            };
            let reply = reply_on(core.run_loop.clone(), resolver);
            match operation {
                CacheDatabaseOperation::Clear => database.clear_ambient_cache(reply),
                CacheDatabaseOperation::Invalidate => database.invalidate_ambient_cache(reply),
                CacheDatabaseOperation::Pack => database.pack_database(reply),
                CacheDatabaseOperation::Reset => database.reset_database(reply),
            }
        })
    }

    /// Limits the ambient cache to `size` bytes. Offline regions don't count
    /// toward it.
    pub fn set_maximum_ambient_cache_size(&self, size: u64) -> Result<Completion<()>, Error> {
        self.command(move |core, outbox, resolver| match core.database() {
            Ok(database) => database.set_maximum_ambient_cache_size(size, reply_on(core.run_loop.clone(), resolver)),
            Err(error) => outbox.resolve(resolver, Err(error)),
        })
    }

    /// The offline regions in the runtime's cache database.
    pub fn offline(&self) -> Offline<'_> {
        Offline::new(self)
    }

    /// Shuts the runtime down. The completion reports when its thread has
    /// let go of its maps' resources and its file sources.
    ///
    /// Fails with [`ErrorKind::InvalidState`](mln_util::ErrorKind) while the
    /// runtime has maps that aren't released, or when the runtime was
    /// released already. Every call on the runtime fails the same way after.
    pub fn release(&self) -> Result<Completion<()>, Error> {
        {
            let mut lifecycle = self.shared.lifecycle.lock_unpoisoned();
            if lifecycle.released {
                return Err(released());
            }
            if lifecycle.live_maps > 0 {
                return Err(Error::invalid_state("release the runtime's maps first"));
            }
            lifecycle.released = true;
            lifecycle.shut_down = true;
        }
        let (completion, resolver) = self.shared.completion();
        self.shared.shut_down(Some(resolver));
        Ok(completion)
    }

    /// Releases the runtime whatever its state, without waiting: it shuts
    /// down once its last map is released. Every call on the runtime fails
    /// with [`ErrorKind::InvalidState`](mln_util::ErrorKind) after.
    pub fn dispose(&self) -> Result<(), Error> {
        let shut_down = {
            let mut lifecycle = self.shared.lifecycle.lock_unpoisoned();
            if lifecycle.released {
                return Err(released());
            }
            lifecycle.released = true;
            let shut_down = lifecycle.live_maps == 0;
            lifecycle.shut_down |= shut_down;
            shut_down
        };
        if shut_down {
            self.shared.shut_down(None);
        }
        Ok(())
    }

    /// Sends a command for the offline API.
    pub(crate) fn offline_command<T: Send + 'static>(
        &self,
        command: impl FnOnce(&DatabaseFileSource, &RuntimeCore, Resolver<T>) + Send + 'static,
    ) -> Result<Completion<T>, Error> {
        self.command(move |core, outbox, resolver| match core.database() {
            Ok(database) => command(database, core, resolver),
            Err(error) => outbox.resolve(resolver, Err(error)),
        })
    }
}

// Handles cross threads; the core never leaves its own.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Runtime>();
    assert_send_sync::<Map>();
};
