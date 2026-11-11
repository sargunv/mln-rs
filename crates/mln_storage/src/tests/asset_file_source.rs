// Ports test/storage/asset_file_source.test.cpp. The asset path is the
// fixture directory read in place (C++: "test/fixtures/storage/assets"
// relative to the working directory).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use mln_actor::{RunLoop, Thread};

use super::{cpp_path, req};
use crate::asset_file_source::AssetFileSource;
use crate::async_request::AsyncRequest;
use crate::file_source::FileSource;
use crate::resource::{Kind, Resource};
use crate::resource_options::ResourceOptions;
use crate::response::{Reason, Response};

fn asset_file_source() -> AssetFileSource {
    AssetFileSource::new(
        &ResourceOptions::default_options().with_asset_path(cpp_path("test/fixtures/storage/assets").to_str().unwrap()),
    )
}

/// Requests `resource`, runs the loop until the response and returns it.
fn request(fs: &AssetFileSource, resource: Resource) -> Response {
    let run_loop = RunLoop::new();
    let req = req();
    let result = Rc::new(RefCell::new(None));
    let (r, out, stop) = (req.clone(), result.clone(), run_loop.clone());
    *req.borrow_mut() = Some(fs.request(
        &resource,
        Box::new(move |res: Response| {
            r.borrow_mut().take();
            *out.borrow_mut() = Some(res);
            stop.stop();
        }),
    ));
    run_loop.run();

    result.borrow_mut().take().expect("response")
}

fn data(res: &Response) -> &[u8] {
    res.data.as_deref().map(Vec::as_slice).expect("data")
}

/// The state of a `TestWorker`'s request chain.
struct WorkerChain {
    fs: Arc<AssetFileSource>,
    num_requests: Cell<u32>,
    request: RefCell<Option<Box<dyn AsyncRequest>>>,
    end_callback: Box<dyn Fn()>,
    failures: Arc<Mutex<Vec<String>>>,
}

impl WorkerChain {
    fn request(self: &Rc<Self>) {
        let asset = "asset://nonempty";
        let this = self.clone();
        let request = self.fs.request(
            &Resource::new(Kind::Unknown, asset),
            Box::new(move |res: Response| {
                // EXPECT_*: record, keep going.
                if res.error.is_some() || res.data.as_deref().map(Vec::as_slice) != Some(b"content is here\n") {
                    this.failures.lock().unwrap().push(format!("{res:?}"));
                }

                let remaining = this.num_requests.get() - 1;
                this.num_requests.set(remaining);
                if remaining == 0 {
                    (this.end_callback)();
                    this.request.borrow_mut().take();
                } else {
                    this.request();
                }
            }),
        );
        *self.request.borrow_mut() = Some(request);
    }
}

/// `TestWorker`.
struct TestWorker {
    fs: Arc<AssetFileSource>,
    chain: RefCell<Option<Rc<WorkerChain>>>,
}

impl TestWorker {
    fn run(&self, end_callback: Box<dyn Fn() + Send>, failures: Arc<Mutex<Vec<String>>>) {
        let chain = Rc::new(WorkerChain {
            fs: self.fs.clone(),
            num_requests: Cell::new(1000),
            request: RefCell::new(None),
            end_callback,
            failures,
        });
        chain.request();
        *self.chain.borrow_mut() = Some(chain);
    }
}

impl Drop for TestWorker {
    fn drop(&mut self) {
        // Break the chain's request -> callback -> chain cycle.
        if let Some(chain) = self.chain.borrow_mut().take() {
            chain.request.borrow_mut().take();
        }
    }
}

// gtest: AssetFileSource.Load
#[test]
fn load() {
    let run_loop = RunLoop::new();

    let fs = Arc::new(asset_file_source());

    let num_threads = 50;
    let completed = Arc::new(AtomicUsize::new(num_threads));
    let failures = Arc::new(Mutex::new(Vec::new()));

    let mut threads = Vec::new();

    for _ in 0..num_threads {
        let worker_fs = fs.clone();
        let thread = Thread::new("Test", move |_| TestWorker { fs: worker_fs, chain: RefCell::new(None) });

        let (completed, stop, failures) = (completed.clone(), run_loop.handle(), failures.clone());
        thread.actor().invoke(move |worker: &TestWorker| {
            worker.run(
                Box::new(move || {
                    if completed.fetch_sub(1, Ordering::SeqCst) == 1 {
                        stop.stop();
                    }
                }),
                failures,
            )
        });
        threads.push(thread);
    }

    run_loop.run();
    drop(threads);
    assert!(failures.lock().unwrap().is_empty(), "{:?}", failures.lock().unwrap().first());
}

// gtest: AssetFileSource.AcceptsURL
#[test]
fn accepts_url() {
    let fs = asset_file_source();
    assert!(fs.can_request(&Resource::style("asset://empty")));
    assert!(fs.can_request(&Resource::style("asset:///test")));
    assert!(!fs.can_request(&Resource::style("assds://foo")));
    assert!(!fs.can_request(&Resource::style("asset:")));
    assert!(!fs.can_request(&Resource::style("style.json")));
    assert!(!fs.can_request(&Resource::style("")));
}

// gtest: AssetFileSource.EmptyFile
#[test]
fn empty_file() {
    let fs = asset_file_source();
    let res = request(&fs, Resource::new(Kind::Unknown, "asset://empty"));
    assert_eq!(None, res.error);
    assert_eq!(b"", data(&res));
}

// gtest: AssetFileSource.NonEmptyFile
#[test]
fn non_empty_file() {
    let fs = asset_file_source();
    let res = request(&fs, Resource::new(Kind::Unknown, "asset://nonempty"));
    assert_eq!(None, res.error);
    assert_eq!(b"content is here\n", data(&res));
}

// gtest: AssetFileSource.NonExistentFile
#[test]
fn non_existent_file() {
    let fs = asset_file_source();
    let res = request(&fs, Resource::new(Kind::Unknown, "asset://does_not_exist"));
    let error = res.error.as_ref().expect("error");
    assert_eq!(Reason::NotFound, error.reason);
    assert!(res.data.is_none());
    // Do not assert on platform-specific error message.
}

// gtest: AssetFileSource.InvalidURL
#[test]
fn invalid_url() {
    let fs = asset_file_source();
    let res = request(&fs, Resource::new(Kind::Unknown, "test://wrong-scheme"));
    let error = res.error.as_ref().expect("error");
    assert_eq!(Reason::Other, error.reason);
    assert_eq!("Invalid asset URL", error.message);
    assert!(res.data.is_none());
}

// gtest: AssetFileSource.ReadDirectory
#[test]
fn read_directory() {
    let fs = asset_file_source();
    let res = request(&fs, Resource::new(Kind::Unknown, "asset://directory"));
    let error = res.error.as_ref().expect("error");
    assert_eq!(Reason::NotFound, error.reason);
    assert!(res.data.is_none());
    // Do not assert on platform-specific error message.
}

// gtest: AssetFileSource.URLEncoding
#[test]
fn url_encoding() {
    let fs = asset_file_source();
    let res = request(&fs, Resource::new(Kind::Unknown, "asset://%6eonempty"));
    assert_eq!(None, res.error);
    assert_eq!(b"content is here\n", data(&res));
}

// gtest: AssetFileSource.RangeRequest
#[test]
fn range_request() {
    let fs = asset_file_source();

    let mut resource = Resource::new(Kind::Unknown, "asset://nonempty");
    resource.data_range = Some((4, 12));

    let res = request(&fs, resource);
    assert_eq!(None, res.error);
    assert_eq!(b"ent is he", data(&res));
}
