// Ports test/storage/database_file_source.test.cpp.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use feature::Value;
use mln_actor::{RunLoop, Timer};
use mln_util::chrono::Duration;
use mln_util::logging::{Event, EventSeverity, Log, Observer};

use super::{local_callback, req, temp_path};
use crate::database_file_source::DatabaseFileSource;
use crate::file_source::{FileSource, FileSourceType, READ_ONLY_MODE_KEY};
use crate::file_source_manager::FileSourceManager;
use crate::offline_database::{DatabaseError, OfflineDatabase};
use crate::resource::{Kind, LoadingMethod, Resource, StoragePolicy};
use crate::resource_options::ResourceOptions;
use crate::response::{Reason, Response};
use crate::sqlite3::{Database, OpenFlag, Query, Statement};

fn dbfs() -> Arc<dyn FileSource> {
    FileSourceManager::new()
        .get_file_source(FileSourceType::Database, &ResourceOptions::default())
        .expect("database file source")
}

// gtest: DatabaseFileSource.PauseResume
#[test]
fn pause_resume() {
    let run_loop = RunLoop::new();

    let dbfs = dbfs();
    dbfs.pause();

    let res = Resource::with_tile_data(Kind::Unknown, "http://127.0.0.1:3000/test", None, LoadingMethod::CacheOnly);
    let stop = run_loop.clone();
    let _req = dbfs.request(&res, Box::new(move |_: Response| stop.stop()));

    let resume_timer = Timer::new();
    let resumed = dbfs.clone();
    resume_timer.start(Duration::from_millis(5), Duration::zero(), move || resumed.resume());

    run_loop.run();
}

// gtest: DatabaseFileSource.VolatileResource
#[test]
fn volatile_resource() {
    let run_loop = RunLoop::new();

    let dbfs = dbfs();

    let resource = Rc::new(RefCell::new(Resource::with_tile_data(
        Kind::Unknown,
        "http://127.0.0.1:3000/test",
        None,
        LoadingMethod::CacheOnly,
    )));
    let mut response = Response::new();
    response.data = Some(Arc::new(b"Cached value".to_vec()));
    let req = req();

    let (r, fs, res, stop) = (req.clone(), dbfs.clone(), resource.clone(), run_loop.clone());
    let forwarded = resource.borrow().clone();
    dbfs.forward(
        &forwarded,
        &response,
        Some(local_callback(move || {
            let (r1, fs1, res1) = (r.clone(), fs.clone(), res.clone());
            let request = res.borrow().clone();
            *r.borrow_mut() = Some(fs.request(
                &request,
                Box::new(move |res_1: Response| {
                    assert_eq!(None, res_1.error);
                    assert!(res_1.data.is_some());
                    assert!(!res_1.no_content);
                    assert_eq!(b"Cached value", res_1.data.unwrap().as_slice());
                    res1.borrow_mut().storage_policy = StoragePolicy::Volatile;
                    let r2 = r1.clone();
                    let stop = stop.clone();
                    let request = res1.borrow().clone();
                    *r1.borrow_mut() = Some(fs1.request(
                        &request,
                        Box::new(move |res_2: Response| {
                            r2.borrow_mut().take();
                            let error = res_2.error.as_ref().expect("error");
                            assert!(res_2.no_content);
                            assert_eq!(Reason::NotFound, error.reason);
                            assert_eq!("Not found in offline database", error.message);
                            stop.stop();
                        }),
                    ));
                }),
            ));
        })),
    );
    run_loop.run();
}

// gtest: DatabaseFileSource.VolatileResourceIsNotStored
#[test]
fn volatile_resource_is_not_stored() {
    let run_loop = RunLoop::new();

    let dbfs = dbfs();

    let mut resource = Resource::with_tile_data(
        Kind::Unknown,
        "http://127.0.0.1:3000/volatile-not-stored",
        None,
        LoadingMethod::CacheOnly,
    );
    resource.storage_policy = StoragePolicy::Volatile;
    let mut response = Response::new();
    response.data = Some(Arc::new(b"Volatile value".to_vec()));

    // Forwarding a volatile response must not write it to the database, so
    // a regular request for the same resource finds nothing.
    dbfs.forward(&resource, &response, None);
    resource.storage_policy = StoragePolicy::Permanent;
    let req = req();
    let result: Rc<RefCell<Option<Response>>> = Rc::new(RefCell::new(None));
    let (r, out, stop) = (req.clone(), result.clone(), run_loop.clone());
    *req.borrow_mut() = Some(dbfs.request(
        &resource,
        Box::new(move |res: Response| {
            r.borrow_mut().take();
            stop.stop();
            *out.borrow_mut() = Some(res);
        }),
    ));
    run_loop.run();

    let res = result.borrow_mut().take().expect("response");
    assert!(res.no_content);
    let error = res.error.expect("error");
    assert_eq!(Reason::NotFound, error.reason);
}

// Not a C++ test: `canRequest`.
#[test]
fn can_request() {
    let dbfs = DatabaseFileSource::new(&FileSourceManager::new(), &ResourceOptions::default());
    let cache_only = |url: &str| Resource::with_tile_data(Kind::Unknown, url, None, LoadingMethod::CacheOnly);
    assert!(dbfs.can_request(&cache_only("http://example.com")));
    assert!(!dbfs.can_request(&cache_only("asset://example.json")));
    assert!(!dbfs.can_request(&cache_only("file://example.json")));
    assert!(!dbfs.can_request(&Resource::with_tile_data(
        Kind::Unknown,
        "http://example.com",
        None,
        LoadingMethod::NetworkOnly
    )));
}

/// Counts the database thread's failed `accessed` flushes whose message
/// contains `marker`; other records go on to the default output.
struct FlushFailures {
    count: Arc<AtomicUsize>,
    marker: String,
}

impl Observer for FlushFailures {
    fn on_record(&self, _: EventSeverity, event: Event, _: i64, msg: &str) -> bool {
        if event != Event::Database
            || !msg.starts_with("Can't flush accessed timestamps")
            || !msg.contains(&self.marker)
        {
            return false;
        }
        self.count.fetch_add(1, Ordering::SeqCst);
        true
    }
}

/// Deletes a database and its journals, now and when dropped.
struct RemoveObserver;

impl Drop for RemoveObserver {
    fn drop(&mut self) {
        Log::remove_observer();
    }
}

fn cached_request(run_loop: &RunLoop, source: &DatabaseFileSource, resource: &Resource) -> Response {
    let result: Rc<RefCell<Option<Response>>> = Rc::new(RefCell::new(None));
    let (out, stop) = (result.clone(), run_loop.clone());
    let _req = source.request(
        resource,
        Box::new(move |res: Response| {
            *out.borrow_mut() = Some(res);
            stop.stop();
        }),
    );
    run_loop.run();
    result.borrow_mut().take().expect("response")
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !done() {
        assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

// The real database thread and timer: a failed flush of cache hits'
// `accessed` timestamps must not retry while idle, and the next cache hit
// must retry it.
// gtest: DatabaseFileSource.FailedAccessedFlushWaitsForNextRead
#[test]
fn failed_accessed_flush_waits_for_next_read() {
    let run_loop = RunLoop::new();
    let path = temp_path("maintenance.db");
    let resource =
        Resource::with_tile_data(Kind::Unknown, "https://example.com/maintenance", None, LoadingMethod::CacheOnly);
    let mut response = Response::new();
    response.data = Some(Arc::new(b"cached".to_vec()));
    OfflineDatabase::new(&path, &ResourceOptions::default().tile_server_options()).put(&resource, &response);

    let marker = format!("accessed failure {path}");
    let sql = Database::open(&path, OpenFlag::ReadWriteCreate).unwrap();
    sql.exec("UPDATE resources SET accessed = 0").unwrap();
    sql.exec(&format!(
        "CREATE TRIGGER fail_accessed BEFORE UPDATE OF accessed ON resources \
         BEGIN SELECT RAISE(ABORT, '{marker}'); END"
    ))
    .unwrap();
    let failures = Arc::new(AtomicUsize::new(0));
    Log::set_observer(Box::new(FlushFailures { count: failures.clone(), marker }));
    let _remove_observer = RemoveObserver;

    let source = DatabaseFileSource::new(&FileSourceManager::new(), &ResourceOptions::default().with_cache_path(&path));
    let cached = cached_request(&run_loop, &source, &resource);
    assert!(cached.error.is_none());
    assert_eq!(response.data, cached.data);

    wait_until("the failed flush", || failures.load(Ordering::SeqCst) > 0);
    std::thread::sleep(std::time::Duration::from_millis(800));
    assert_eq!(1, failures.load(Ordering::SeqCst), "failed flush retried while idle");

    sql.exec("DROP TRIGGER fail_accessed").unwrap();
    let cached = cached_request(&run_loop, &source, &resource);
    assert!(cached.error.is_none());
    wait_until("the retried flush", || {
        let conn = sql.conn();
        let mut stmt = Statement::new(&conn, "SELECT accessed FROM resources").unwrap();
        let mut query = Query::new(&mut stmt);
        assert!(query.run().unwrap());
        query.get_i64(0).unwrap() > 0
    });
    assert_eq!(1, failures.load(Ordering::SeqCst));
}

/// Runs a maintenance operation and waits for its callback on the database
/// thread.
fn maintain(operation: impl FnOnce(Box<dyn FnOnce(Result<(), DatabaseError>) + Send>)) -> Result<(), DatabaseError> {
    let (tx, rx) = std::sync::mpsc::channel();
    operation(Box::new(move |result| tx.send(result).unwrap()));
    rx.recv_timeout(std::time::Duration::from_secs(5)).expect("callback")
}

// Not a C++ test: the cache maintenance operations reach the database and
// report its errors.
#[test]
fn cache_maintenance() {
    let run_loop = RunLoop::new();
    let path = temp_path("cache-maintenance.db");
    let source = DatabaseFileSource::new(&FileSourceManager::new(), &ResourceOptions::default().with_cache_path(&path));
    let resource =
        Resource::with_tile_data(Kind::Unknown, "https://example.com/maintained", None, LoadingMethod::CacheOnly);
    let mut response = Response::new();
    response.data = Some(Arc::new(b"cached".to_vec()));
    let not_found = |res: Response| res.error.expect("error").reason == Reason::NotFound;

    source.put(&resource, &response);
    assert_eq!(response.data, cached_request(&run_loop, &source, &resource).data);
    maintain(|done| source.invalidate_ambient_cache(done)).unwrap();
    let invalidated = cached_request(&run_loop, &source, &resource);
    assert_eq!("Cached resource is unusable", invalidated.error.expect("error").message);

    source.put(&resource, &response);
    maintain(|done| source.clear_ambient_cache(done)).unwrap();
    assert!(not_found(cached_request(&run_loop, &source, &resource)));

    source.put(&resource, &response);
    maintain(|done| source.reset_database(done)).unwrap();
    assert!(not_found(cached_request(&run_loop, &source, &resource)));

    maintain(|done| source.pack_database(done)).unwrap();

    maintain(|done| source.set_maximum_ambient_cache_size(0, done)).unwrap();
    source.put(&resource, &response);
    assert!(not_found(cached_request(&run_loop, &source, &resource)));

    source.set_property(READ_ONLY_MODE_KEY, &Value::Bool(true));
    let error = maintain(|done| source.clear_ambient_cache(done)).unwrap_err();
    assert_eq!("Cannot modify database in read-only mode", error.to_string());
}
