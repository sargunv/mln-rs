// Ports test/storage/main_resource_loader.test.cpp against the test server
// (mln_test_server; C++: 127.0.0.1:3000). Every test takes `serial()`:
// NetworkStatus is process-wide, and gtest runs one test at a time.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use feature::Value;
use mln_actor::{ActorRef, Mailbox, RunLoop, Timer, scheduler};
use mln_test_server::HttpServer;
use mln_util::chrono::{Duration, Timestamp, now};
use mln_util::tile_server_options::TileServerOptions;

use super::{local_callback, req, serial, temp_path};
use crate::database_file_source::DatabaseFileSource;
use crate::file_source::{FileSource, FileSourceType, MAX_CONCURRENT_REQUESTS_KEY};
use crate::file_source_manager::FileSourceManager;
use crate::main_resource_loader::MainResourceLoader;
use crate::network_status::{NetworkStatus, Status};
use crate::resource::{Kind, LoadingMethod, Resource, StoragePolicy};
use crate::resource_options::ResourceOptions;
use crate::resource_transform::{FinishedCallback, ResourceTransform};
use crate::response::{Reason, Response};

fn main_resource_loader(file_sources: &FileSourceManager) -> Rc<MainResourceLoader> {
    Rc::new(MainResourceLoader::new(file_sources, &ResourceOptions::default()))
}

fn database_file_source(file_sources: &FileSourceManager) -> Arc<dyn FileSource> {
    file_sources.get_file_source(FileSourceType::Database, &ResourceOptions::default()).expect("database file source")
}

fn online_file_source(file_sources: &FileSourceManager) -> Arc<dyn FileSource> {
    file_sources.get_file_source(FileSourceType::Network, &ResourceOptions::default()).expect("online file source")
}

fn data(res: &Response) -> &[u8] {
    res.data.as_deref().map(Vec::as_slice).expect("data")
}

fn bytes(s: &str) -> Option<crate::response::Data> {
    Some(Arc::new(s.as_bytes().to_vec()))
}

fn in_one_hour() -> Timestamp {
    Timestamp::from_secs(now().secs_since_epoch() + 3600)
}

fn one_hour_ago() -> Timestamp {
    Timestamp::from_secs(now().secs_since_epoch() - 3600)
}

fn cache_only(url: &str) -> Resource {
    Resource::with_tile_data(Kind::Unknown, url, None, LoadingMethod::CacheOnly)
}

// gtest: MainResourceLoader.CacheResponse
#[test]
fn cache_response() {
    let _serial = serial();
    let file_sources = FileSourceManager::new();
    let server = HttpServer::new();
    let run_loop = RunLoop::new();
    let fs = main_resource_loader(&file_sources);

    let resource = Resource::new(Kind::Unknown, server.url("/cache"));

    let req1 = req();
    let req2 = req();

    let (r1, r2, fs2, stop) = (req1.clone(), req2.clone(), fs.clone(), run_loop.clone());
    let res2_resource = resource.clone();
    *req1.borrow_mut() = Some(fs.request(
        &resource,
        Box::new(move |res: Response| {
            r1.borrow_mut().take();
            assert_eq!(None, res.error);
            assert_eq!(b"Response 1", data(&res));
            assert!(res.expires.is_some());
            assert!(!res.must_revalidate);
            assert!(res.modified.is_none());
            assert!(res.etag.is_none());
            let response = res;

            // Now test that we get the same values as in the previous
            // request. If we'd go to the server again, we'd get different
            // values.
            let (r2c, stop) = (r2.clone(), stop.clone());
            *r2.borrow_mut() = Some(fs2.request(
                &res2_resource,
                Box::new(move |res2: Response| {
                    r2c.borrow_mut().take();
                    assert_eq!(response.error, res2.error);
                    assert_eq!(data(&response), data(&res2));
                    assert_eq!(response.expires, res2.expires);
                    assert_eq!(response.must_revalidate, res2.must_revalidate);
                    assert_eq!(response.modified, res2.modified);
                    assert_eq!(response.etag, res2.etag);

                    stop.stop();
                }),
            ));
        }),
    ));

    run_loop.run();
}

// gtest: MainResourceLoader.VolatileStoragePolicy
#[test]
fn volatile_storage_policy() {
    let _serial = serial();
    let file_sources = FileSourceManager::new();
    let server = HttpServer::new();
    let run_loop = RunLoop::new();
    let fs = main_resource_loader(&file_sources);

    let mut resource = Resource::new(Kind::Unknown, server.url("/cache"));
    resource.storage_policy = StoragePolicy::Volatile;

    let req = req();
    let (r, fs2, stop, resource2) = (req.clone(), fs.clone(), run_loop.clone(), resource.clone());
    *req.borrow_mut() = Some(fs.request(
        &resource,
        Box::new(move |res1: Response| {
            assert_eq!(None, res1.error);
            let first_data = data(&res1).to_vec();

            // Volatile resources are not stored in cache, so we always get
            // new data from the server ("Response N+1").
            let (rc, stop) = (r.clone(), stop.clone());
            *r.borrow_mut() = Some(fs2.request(
                &resource2,
                Box::new(move |res2: Response| {
                    rc.borrow_mut().take();
                    assert_eq!(None, res2.error);
                    assert_ne!(first_data.as_slice(), data(&res2));
                    stop.stop();
                }),
            ));
        }),
    ));

    run_loop.run();
}

/// CacheRevalidateSame and CacheRevalidateModified share their structure.
fn cache_revalidate(path: &str, check: fn(&Response)) {
    let _serial = serial();
    let file_sources = FileSourceManager::new();
    let server = HttpServer::new();
    let run_loop = RunLoop::new();
    let fs = main_resource_loader(&file_sources);

    let revalidate = Resource::new(Kind::Unknown, server.url(path));
    let req1 = req();
    let req2 = req();
    let got_response = Rc::new(Cell::new(false));

    let (r1, r2, fs2, stop, revalidate2) =
        (req1.clone(), req2.clone(), fs.clone(), run_loop.clone(), revalidate.clone());
    // First request causes the response to get cached.
    *req1.borrow_mut() = Some(fs.request(
        &revalidate,
        Box::new(move |res: Response| {
            r1.borrow_mut().take();

            assert_eq!(None, res.error);
            assert!(!res.not_modified);
            assert_eq!(b"Response", data(&res));
            assert!(res.expires.is_none());
            assert!(res.must_revalidate);
            check(&res);

            // The first response is stored in the cache, but it has
            // 'must-revalidate' set. This means it can't return the cached
            // response right away and we must wait for the revalidation
            // request to complete. We can distinguish the cached response
            // from the revalided response because the latter has an
            // expiration date, while the cached response doesn't.
            let (r2c, stop, got_response) = (r2.clone(), stop.clone(), got_response.clone());
            *r2.borrow_mut() = Some(fs2.request(
                &revalidate2,
                Box::new(move |res2: Response| {
                    if !got_response.get() {
                        // Even though we could find the response in the
                        // database, we send a revalidation request and get a
                        // 304 response. Since we haven't sent a reply yet,
                        // we're forcing notModified to be false so that
                        // implementations can continue to use the
                        // notModified flag to skip parsing new data.
                        got_response.set(true);
                        assert_eq!(None, res2.error);
                        assert!(!res2.not_modified);
                        assert_eq!(b"Response", data(&res2));
                        assert!(res2.expires.is_some());
                        assert!(res2.must_revalidate);
                        check(&res2);
                    } else {
                        // The test server sends a Cache-Control header with
                        // a max-age of 1 second. This means that our
                        // OnlineFileSource implementation will request the
                        // tile again after 1 second. This time, our request
                        // already had a prior response, so we don't need to
                        // send the data again, and instead can actually
                        // forward the notModified flag.
                        r2c.borrow_mut().take();
                        assert_eq!(None, res2.error);
                        assert!(res2.not_modified);
                        assert!(res2.data.is_none());
                        assert!(res2.expires.is_some());
                        assert!(res2.must_revalidate);
                        check(&res2);
                        stop.stop();
                    }
                }),
            ));
        }),
    ));

    run_loop.run();
}

// gtest: MainResourceLoader.CacheRevalidateSame
#[test]
fn cache_revalidate_same() {
    cache_revalidate("/revalidate-same", |res| {
        assert!(res.modified.is_none());
        assert_eq!(Some("snowfall"), res.etag.as_deref());
    });
}

// gtest: MainResourceLoader.CacheRevalidateModified
#[test]
fn cache_revalidate_modified() {
    cache_revalidate("/revalidate-modified", |res| {
        assert_eq!(Some(Timestamp::from_secs(1420070400)), res.modified);
        assert!(res.etag.is_none());
    });
}

// gtest: MainResourceLoader.CacheRevalidateEtag
#[test]
fn cache_revalidate_etag() {
    let _serial = serial();
    let file_sources = FileSourceManager::new();
    let server = HttpServer::new();
    let run_loop = RunLoop::new();
    let fs = main_resource_loader(&file_sources);

    let revalidate_etag = Resource::new(Kind::Unknown, server.url("/revalidate-etag"));
    let req1 = req();
    let req2 = req();

    let (r1, r2, fs2, stop, revalidate2) =
        (req1.clone(), req2.clone(), fs.clone(), run_loop.clone(), revalidate_etag.clone());
    // First request causes the response to get cached.
    *req1.borrow_mut() = Some(fs.request(
        &revalidate_etag,
        Box::new(move |res: Response| {
            r1.borrow_mut().take();

            assert_eq!(None, res.error);
            assert_eq!(b"Response 1", data(&res));
            assert!(res.expires.is_none());
            assert!(res.must_revalidate);
            assert!(res.modified.is_none());
            assert_eq!(Some("response-1"), res.etag.as_deref());

            // Second request does not return the cached response, since it
            // had Cache-Control: must-revalidate set.
            let (r2c, stop) = (r2.clone(), stop.clone());
            *r2.borrow_mut() = Some(fs2.request(
                &revalidate2,
                Box::new(move |res2: Response| {
                    r2c.borrow_mut().take();

                    assert_eq!(None, res2.error);
                    assert!(res2.data.is_some());
                    assert_ne!(res.data, res2.data);
                    assert_eq!(b"Response 2", data(&res2));
                    assert!(res2.expires.is_none());
                    assert!(res2.must_revalidate);
                    assert!(res2.modified.is_none());
                    assert_eq!(Some("response-2"), res2.etag.as_deref());

                    stop.stop();
                }),
            ));
        }),
    ));

    run_loop.run();
}

// Test for https://github.com/mapbox/mapbox-gl-native/issue/1369
//
// A request for http://example.com is made. This triggers a cache get. While
// the cache get is pending, the request is canceled. This removes it from
// pending. Then, still while the cache get is pending, a second request is
// made for the same resource. This adds an entry back to pending and queues
// another cache request, even though the first one is still pending. Now
// both cache requests resolve to misses, resulting in two HTTP requests for
// the same resource. The first one will notify as expected, the second one
// will have bound a DefaultFileRequest* in the lambda that gets invalidated
// by the first notify's pending.erase, and when it gets notified, the crash
// occurs.
// gtest: MainResourceLoader.HTTPIssue1369
#[test]
fn http_issue_1369() {
    let _serial = serial();
    let file_sources = FileSourceManager::new();
    let server = HttpServer::new();
    let run_loop = RunLoop::new();
    let fs = main_resource_loader(&file_sources);

    let resource = Resource::new(Kind::Unknown, server.url("/test"));

    let req = req();
    *req.borrow_mut() = Some(fs.request(&resource, Box::new(|_: Response| panic!("Callback should not be called"))));
    req.borrow_mut().take();
    let (r, stop) = (req.clone(), run_loop.clone());
    *req.borrow_mut() = Some(fs.request(
        &resource,
        Box::new(move |res: Response| {
            r.borrow_mut().take();
            assert_eq!(None, res.error);
            assert_eq!(b"Hello World!", data(&res));
            assert!(res.expires.is_none());
            assert!(!res.must_revalidate);
            assert!(res.modified.is_none());
            assert!(res.etag.is_none());
            stop.stop();
        }),
    ));

    run_loop.run();
}

/// OptionalNonExpired and OptionalExpired: a cached value is returned for a
/// cache-only request whether it has expired or not.
fn optional_cached(expires: Timestamp) {
    let _serial = serial();
    let file_sources = FileSourceManager::new();
    let run_loop = RunLoop::new();
    let fs = main_resource_loader(&file_sources);

    let optional_resource = cache_only("http://127.0.0.1:3000/test");

    let mut response = Response::new();
    response.data = bytes("Cached value");
    response.expires = Some(expires);

    let req = req();
    let dbfs = database_file_source(&file_sources);
    let (r, fs2, stop, resource2, expected) =
        (req.clone(), fs.clone(), run_loop.clone(), optional_resource.clone(), response.clone());
    dbfs.forward(
        &optional_resource,
        &response,
        Some(local_callback(move || {
            let rc = r.clone();
            *r.borrow_mut() = Some(fs2.request(
                &resource2,
                Box::new(move |res: Response| {
                    rc.borrow_mut().take();
                    assert_eq!(None, res.error);
                    assert_eq!(b"Cached value", data(&res));
                    assert!(res.expires.is_some());
                    assert_eq!(expected.expires, res.expires);
                    assert!(!res.must_revalidate);
                    assert!(res.modified.is_none());
                    assert!(res.etag.is_none());
                    stop.stop();
                }),
            ));
        })),
    );

    run_loop.run();
}

// gtest: MainResourceLoader.OptionalNonExpired
#[test]
fn optional_non_expired() {
    optional_cached(in_one_hour());
}

// gtest: MainResourceLoader.OptionalExpired
#[test]
fn optional_expired() {
    optional_cached(one_hour_ago());
}

// gtest: MainResourceLoader.OptionalNotFound
#[test]
fn optional_not_found() {
    let _serial = serial();
    let file_sources = FileSourceManager::new();
    let run_loop = RunLoop::new();
    let fs = main_resource_loader(&file_sources);

    let optional_resource = cache_only("http://127.0.0.1:3000/test");

    let req = req();
    let (r, stop) = (req.clone(), run_loop.clone());
    *req.borrow_mut() = Some(fs.request(
        &optional_resource,
        Box::new(move |res: Response| {
            r.borrow_mut().take();
            let error = res.error.as_ref().expect("error");
            assert_eq!(Reason::NotFound, error.reason);
            assert_eq!("Not found in offline database", error.message);
            assert!(res.data.is_none());
            assert!(res.expires.is_none());
            assert!(!res.must_revalidate);
            assert!(res.modified.is_none());
            assert!(res.etag.is_none());
            stop.stop();
        }),
    ));

    run_loop.run();
}

/// The NoCache* tests: put a fake value into the cache to make sure we're
/// not retrieving anything from the cache, then request network-only.
fn no_cache(resource: Resource, check: impl Fn(&Response) + 'static) {
    let file_sources = FileSourceManager::new();
    let run_loop = RunLoop::new();
    let fs = main_resource_loader(&file_sources);

    let mut response = Response::new();
    response.data = bytes("Cached value");
    response.expires = Some(in_one_hour());
    let req = req();
    let dbfs = database_file_source(&file_sources);
    let (r, fs2, stop, resource2) = (req.clone(), fs.clone(), run_loop.clone(), resource.clone());
    dbfs.forward(
        &resource,
        &response,
        Some(local_callback(move || {
            let rc = r.clone();
            *r.borrow_mut() = Some(fs2.request(
                &resource2,
                Box::new(move |res: Response| {
                    rc.borrow_mut().take();
                    assert_eq!(None, res.error);
                    check(&res);
                    stop.stop();
                }),
            ));
        })),
    );

    run_loop.run();
}

// Test that a network only request doesn't attempt to load data from the
// cache.
// gtest: MainResourceLoader.NoCacheRefreshEtagNotModified
#[test]
fn no_cache_refresh_etag_not_modified() {
    let _serial = serial();
    let server = HttpServer::new();
    let mut resource = Resource::new(Kind::Unknown, server.url("/revalidate-same"));
    resource.loading_method = LoadingMethod::NetworkOnly;
    resource.prior_etag = Some("snowfall".to_owned());

    no_cache(resource, |res| {
        assert!(res.not_modified);
        assert!(res.data.is_none());
        assert!(res.expires.is_some());
        assert!(Some(now()) < res.expires);
        assert!(res.must_revalidate);
        assert!(res.modified.is_none());
        assert_eq!(Some("snowfall"), res.etag.as_deref());
    });
}

// Test that a network only request doesn't attempt to load data from the
// cache.
// gtest: MainResourceLoader.NoCacheRefreshEtagModified
#[test]
fn no_cache_refresh_etag_modified() {
    let _serial = serial();
    let server = HttpServer::new();
    let mut resource = Resource::new(Kind::Unknown, server.url("/revalidate-same"));
    resource.loading_method = LoadingMethod::NetworkOnly;
    resource.prior_etag = Some("sunshine".to_owned());

    no_cache(resource, |res| {
        assert!(!res.not_modified);
        assert_eq!(b"Response", data(res));
        assert!(res.expires.is_none());
        assert!(res.must_revalidate);
        assert!(res.modified.is_none());
        assert_eq!(Some("snowfall"), res.etag.as_deref());
    });
}

// Test that a network only request doesn't attempt to load data from the
// cache.
// gtest: MainResourceLoader.NoCacheFull
#[test]
fn no_cache_full() {
    let _serial = serial();
    let server = HttpServer::new();
    let mut resource = Resource::new(Kind::Unknown, server.url("/revalidate-same"));
    resource.loading_method = LoadingMethod::NetworkOnly;

    no_cache(resource, |res| {
        assert!(!res.not_modified);
        assert_eq!(b"Response", data(res));
        assert!(res.expires.is_none());
        assert!(res.must_revalidate);
        assert!(res.modified.is_none());
        assert_eq!(Some("snowfall"), res.etag.as_deref());
    });
}

// Test that we can make a request with a Modified field that doesn't first
// try to load from cache like a regular request
// gtest: MainResourceLoader.NoCacheRefreshModifiedNotModified
#[test]
fn no_cache_refresh_modified_not_modified() {
    let _serial = serial();
    let server = HttpServer::new();
    let mut resource = Resource::new(Kind::Unknown, server.url("/revalidate-modified"));
    resource.loading_method = LoadingMethod::NetworkOnly;
    resource.prior_modified = Some(Timestamp::from_secs(1420070400)); // January 1, 2015

    no_cache(resource, |res| {
        assert!(res.not_modified);
        assert!(res.data.is_none());
        assert!(res.expires.is_some());
        assert!(Some(now()) < res.expires);
        assert!(res.must_revalidate);
        assert_eq!(Some(Timestamp::from_secs(1420070400)), res.modified);
        assert!(res.etag.is_none());
    });
}

// Test that we can make a request with a Modified field that doesn't first
// try to load from cache like a regular request
// gtest: MainResourceLoader.NoCacheRefreshModifiedModified
#[test]
fn no_cache_refresh_modified_modified() {
    let _serial = serial();
    let server = HttpServer::new();
    let mut resource = Resource::new(Kind::Unknown, server.url("/revalidate-modified"));
    resource.loading_method = LoadingMethod::NetworkOnly;
    resource.prior_modified = Some(Timestamp::from_secs(1417392000)); // December 1, 2014

    no_cache(resource, |res| {
        assert!(!res.not_modified);
        assert_eq!(b"Response", data(res));
        assert!(res.expires.is_none());
        assert!(res.must_revalidate);
        assert_eq!(Some(Timestamp::from_secs(1420070400)), res.modified);
        assert!(res.etag.is_none());
    });
}

/// The target of the test's `Actor<ResourceTransform::TransformCallback>`.
struct TransformActor {
    test_url: String,
}

impl TransformActor {
    /// The actor's callback: translates "localhost://test" to the test
    /// server's /test.
    fn call(&self, url: &str, cb: FinishedCallback) {
        if url == "localhost://test" {
            cb(Ok(self.test_url.clone()));
        } else {
            cb(Ok(url.to_owned()));
        }
    }
}

// gtest: MainResourceLoader.SetResourceTransform
#[test]
fn set_resource_transform() {
    let _serial = serial();
    let file_sources = FileSourceManager::new();
    let server = HttpServer::new();
    let run_loop = RunLoop::new();
    let resource_loader = main_resource_loader(&file_sources);
    let onlinefs = online_file_source(&file_sources);

    // Translates the URL "localhost://test to http://127.0.0.1:3000/test
    let transform =
        Mailbox::with_target(&scheduler::get_current(), Rc::new(TransformActor { test_url: server.url("/test") }));

    let actor_ref = ActorRef::<TransformActor>::new(&transform);
    onlinefs.set_resource_transform(ResourceTransform::new(Some(Arc::new(
        move |_kind: Kind, url: &str, cb: FinishedCallback| {
            let url = url.to_owned();
            actor_ref.invoke(move |transform: &TransformActor| transform.call(&url, cb));
        },
    ))));
    let resource1 = Resource::new(Kind::Unknown, "localhost://test");

    let req = req();
    let check = |res: &Response| {
        assert_eq!(None, res.error);
        assert_eq!(b"Hello World!", data(res));
        assert!(res.expires.is_none());
        assert!(!res.must_revalidate);
        assert!(res.modified.is_none());
        assert!(res.etag.is_none());
    };
    let (r, stop) = (req.clone(), run_loop.clone());
    *req.borrow_mut() = Some(resource_loader.request(
        &resource1,
        Box::new(move |res: Response| {
            r.borrow_mut().take();
            check(&res);
            stop.stop();
        }),
    ));

    run_loop.run();

    onlinefs.set_resource_transform(ResourceTransform::default());
    let resource2 = Resource::new(Kind::Unknown, server.url("/test"));

    let (r, stop) = (req.clone(), run_loop.clone());
    *req.borrow_mut() = Some(resource_loader.request(
        &resource2,
        Box::new(move |res: Response| {
            r.borrow_mut().take();
            check(&res);
            stop.stop();
        }),
    ));

    run_loop.run();
    transform.close();
}

// gtest: MainResourceLoader.SetResourceCachePath
#[test]
fn set_resource_cache_path() {
    let _serial = serial();
    let file_sources = FileSourceManager::new();
    let run_loop = RunLoop::new();
    let _resource_loader = main_resource_loader(&file_sources);
    let fs = database_file_source(&file_sources);
    // `std::static_pointer_cast<DatabaseFileSource>(fs)`.
    let dbfs = fs.as_any().and_then(|fs| fs.downcast_ref::<DatabaseFileSource>()).expect("DatabaseFileSource");
    let stop = run_loop.handle();
    // C++: "./new_offline.db".
    let path = temp_path("new_offline.db");
    dbfs.set_database_path(&path, Some(Box::new(move || stop.stop())));
    run_loop.run();
}

// Test that a stale cache file that has must-revalidate set will trigger a
// response.
// gtest: MainResourceLoader.RespondToStaleMustRevalidate
#[test]
fn respond_to_stale_must_revalidate() {
    let _serial = serial();
    let file_sources = FileSourceManager::new();
    let server = HttpServer::new();
    let run_loop = RunLoop::new();
    let fs = main_resource_loader(&file_sources);

    let resource = Rc::new(RefCell::new(Resource::new(Kind::Unknown, server.url("/revalidate-same"))));
    resource.borrow_mut().loading_method = LoadingMethod::CacheOnly;

    // Put an existing value in the cache that has expired, and has
    // must-revalidate set.
    let mut response = Response::new();
    response.data = bytes("Cached value");
    response.modified = Some(Timestamp::from_secs(1417392000)); // December 1, 2014
    response.expires = Some(Timestamp::from_secs(1417392000));
    response.must_revalidate = true;
    response.etag = Some("snowfall".to_owned());
    let req = req();
    let dbfs = database_file_source(&file_sources);
    let (r, fs2, stop, resource2) = (req.clone(), fs.clone(), run_loop.clone(), resource.clone());
    let forwarded = resource.borrow().clone();
    dbfs.forward(
        &forwarded,
        &response,
        Some(local_callback(move || {
            let rc = r.clone();
            let request = resource2.borrow().clone();
            *r.borrow_mut() = Some(fs2.request(
                &request,
                Box::new(move |res: Response| {
                    rc.borrow_mut().take();
                    let error = res.error.as_ref().expect("error");
                    assert_eq!(Reason::NotFound, error.reason);
                    assert_eq!("Cached resource is unusable", error.message);
                    assert!(!res.not_modified);
                    assert_eq!(b"Cached value", data(&res));
                    assert_eq!(Some(Timestamp::from_secs(1417392000)), res.expires);
                    assert!(res.must_revalidate);
                    assert_eq!(Some(Timestamp::from_secs(1417392000)), res.modified);
                    assert_eq!(Some("snowfall"), res.etag.as_deref());

                    let mut resource = resource2.borrow_mut();
                    resource.prior_etag = res.etag.clone();
                    resource.prior_modified = res.modified;
                    resource.prior_expires = res.expires;
                    resource.prior_data = res.data.clone();

                    stop.stop();
                }),
            ));
        })),
    );

    run_loop.run();

    // Now run this request again, with the data we gathered from the
    // previous stale/unusable request. We're replacing the data so that we
    // can check that the MainResourceLoader doesn't attempt another database
    // access if we already have the value.
    resource.borrow_mut().loading_method = LoadingMethod::NetworkOnly;
    resource.borrow_mut().prior_data = bytes("Prior value");

    let (r, stop) = (req.clone(), run_loop.clone());
    let request = resource.borrow().clone();
    *req.borrow_mut() = Some(fs.request(
        &request,
        Box::new(move |res: Response| {
            r.borrow_mut().take();
            assert_eq!(None, res.error);
            // Since the data was found in the cache, we're doing a
            // revalidation request. Yet, since this request hasn't returned
            // data before, we're setting notModified to false in the
            // OnlineFileSource to ensure that requestors know that this is
            // the first time they're seeing this data.
            assert!(!res.not_modified);
            // Ensure that it's the value that we manually inserted into the
            // cache rather than the value the server returns, since we should
            // be executing a revalidation request which doesn't return new
            // data, only a 304 Not Modified response.
            assert_eq!(b"Prior value", data(&res));
            assert!(res.expires.is_some());
            assert!(Some(now()) <= res.expires);
            assert!(res.must_revalidate);
            assert_eq!(Some(Timestamp::from_secs(1417392000)), res.modified);
            assert_eq!(Some("snowfall"), res.etag.as_deref());
            stop.stop();
        }),
    ));

    run_loop.run();
}

// Test that requests for expired resources have lower priority than
// requests for new resources
// Flaky https://github.com/maplibre/maplibre-native/issues/1071
// gtest: MainResourceLoader.CachedResourceLowPriority
#[test]
fn cached_resource_low_priority() {
    let _serial = serial();
    let file_sources = FileSourceManager::new();
    let server = HttpServer::new();
    let run_loop = RunLoop::new();
    let fs = main_resource_loader(&file_sources);

    let mut response = Response::new();
    let online_response_counter = Rc::new(Cell::new(0usize));

    response.expires = Some(one_hour_ago());

    let dbfs = database_file_source(&file_sources);
    let online_fs = online_file_source(&file_sources);

    // Put existing values into the cache.
    let resource1 = Resource::with_tile_data(Kind::Unknown, server.url("/load/3"), None, LoadingMethod::All);
    response.data = bytes("Cached Request 3");
    dbfs.forward(&resource1, &response, Some(Box::new(|| {})));

    let resource2 = Resource::with_tile_data(Kind::Unknown, server.url("/load/4"), None, LoadingMethod::All);
    response.data = bytes("Cached Request 4");
    dbfs.forward(&resource2, &response, Some(Box::new(|| {})));

    online_fs.set_property(MAX_CONCURRENT_REQUESTS_KEY, &Value::Uint(1));
    fs.pause();
    NetworkStatus::set(Status::Offline);

    // Ensure that the online requests for new resources are processed first.
    let non_cached0 = Resource::with_tile_data(Kind::Unknown, server.url("/load/0"), None, LoadingMethod::All);
    let req0 = req();
    let (r, counter) = (req0.clone(), online_response_counter.clone());
    *req0.borrow_mut() = Some(fs.request(
        &non_cached0,
        Box::new(move |res: Response| {
            r.borrow_mut().take();
            assert_eq!(counter.get(), 0); // make sure this is responded first
            assert_eq!(b"Request 0", data(&res));
        }),
    ));

    let non_cached1 = Resource::with_tile_data(Kind::Unknown, server.url("/load/1"), None, LoadingMethod::All);
    let req1 = req();
    let (r, counter) = (req1.clone(), online_response_counter.clone());
    *req1.borrow_mut() = Some(fs.request(
        &non_cached1,
        Box::new(move |res: Response| {
            counter.set(counter.get() + 1);
            r.borrow_mut().take();
            assert_eq!(counter.get(), 1); // make sure this is responded second
            assert_eq!(b"Request 1", data(&res));
        }),
    ));

    let req3_cached_response_received = Rc::new(Cell::new(false));
    let req3 = req();
    let (r, counter, received) = (req3.clone(), online_response_counter.clone(), req3_cached_response_received.clone());
    *req3.borrow_mut() = Some(fs.request(
        &resource1,
        Box::new(move |res: Response| {
            // Offline callback is received first
            if !received.get() {
                assert_eq!(b"Cached Request 3", data(&res));
                received.set(true);
            } else {
                counter.set(counter.get() + 1);
                r.borrow_mut().take();
                assert_eq!(counter.get(), 3);
                assert_eq!(b"Request 3", data(&res));
            }
        }),
    ));

    let req4_cached_response_received = Rc::new(Cell::new(false));
    let req4 = req();
    let (r, counter, received, stop) =
        (req4.clone(), online_response_counter.clone(), req4_cached_response_received.clone(), run_loop.clone());
    *req4.borrow_mut() = Some(fs.request(
        &resource2,
        Box::new(move |res: Response| {
            // Offline callback is received first
            if !received.get() {
                assert_eq!(b"Cached Request 4", data(&res));
                received.set(true);
            } else {
                counter.set(counter.get() + 1);
                r.borrow_mut().take();
                assert_eq!(counter.get(), 4);
                assert_eq!(b"Request 4", data(&res));
                stop.stop();
            }
        }),
    ));

    let non_cached2 = Resource::with_tile_data(Kind::Unknown, server.url("/load/2"), None, LoadingMethod::All);
    let req2 = req();
    let (r, counter) = (req2.clone(), online_response_counter.clone());
    *req2.borrow_mut() = Some(fs.request(
        &non_cached2,
        Box::new(move |res: Response| {
            counter.set(counter.get() + 1);
            r.borrow_mut().take();
            assert_eq!(counter.get(), 2); // make sure this is responded third
            assert_eq!(b"Request 2", data(&res));
        }),
    ));

    fs.resume();
    NetworkStatus::set(Status::Online);

    run_loop.run();
}

// gtest: MainResourceLoader.NoDoubleDispatch
#[test]
fn no_double_dispatch() {
    let _serial = serial();
    let file_sources = FileSourceManager::new();
    let server = HttpServer::new();
    let run_loop = RunLoop::new();
    let fs = main_resource_loader(&file_sources);

    let resource = Resource::new(Kind::Unknown, server.url("/revalidate-same"));
    let mut response = Response::new();
    response.data = bytes("data");
    response.etag = Some("snowfall".to_owned());

    let req = req();
    let response_count = Rc::new(Cell::new(0u32));
    let dbfs = database_file_source(&file_sources);
    let (r, fs2, resource2, count) = (req.clone(), fs.clone(), resource.clone(), response_count.clone());
    dbfs.forward(
        &resource,
        &response,
        Some(local_callback(move || {
            *r.borrow_mut() = Some(fs2.request(
                &resource2,
                Box::new(move |res: Response| {
                    assert_eq!(None, res.error);
                    assert!(res.modified.is_none());
                    assert!(res.etag.is_some());
                    assert_eq!(Some("snowfall"), res.etag.as_deref());
                    if !res.not_modified {
                        assert_eq!(b"data", data(&res));
                        count.set(count.get() + 1);
                    }
                }),
            ));
        })),
    );

    let timer = Timer::new();
    let (stop, count) = (run_loop.clone(), response_count.clone());
    timer.start(Duration::from_millis(100), Duration::zero(), move || {
        assert_eq!(1, count.get());
        stop.stop();
    });

    run_loop.run();
}

// gtest: MainResourceLoader.ResourceOptions
#[test]
fn resource_options() {
    let _serial = serial();
    let fs = MainResourceLoader::new(
        &FileSourceManager::new(),
        &ResourceOptions::default().with_tile_server_options(
            TileServerOptions::new().with_base_url("originalBaseURL").with_uri_scheme_alias("originalAlias"),
        ),
    );

    fs.set_resource_options(ResourceOptions::default().with_tile_server_options(
        TileServerOptions::new().with_base_url("updatedBaseURL").with_uri_scheme_alias("updatedAlias"),
    ));

    let updated_options = fs.get_resource_options().tile_server_options();

    assert_eq!(updated_options.base_url(), "updatedBaseURL");
    assert_eq!(updated_options.uri_scheme_alias(), "updatedAlias");
}

// Not a C++ test: the "Unsupported resource request." branch (a request
// with no loading method that no file source accepts).
#[test]
fn unsupported_resource_request() {
    let _serial = serial();
    let run_loop = RunLoop::new();
    let fs = MainResourceLoader::new(&FileSourceManager::new(), &ResourceOptions::default());
    let resource = Resource::with_tile_data(Kind::Unknown, "http://example.com/x.json", None, LoadingMethod::None);
    let result: Rc<RefCell<Option<Response>>> = Rc::new(RefCell::new(None));
    let (out, stop) = (result.clone(), run_loop.clone());
    let _req = fs.request(
        &resource,
        Box::new(move |res: Response| {
            *out.borrow_mut() = Some(res);
            stop.stop();
        }),
    );
    run_loop.run();
    let res = result.borrow_mut().take().expect("response");
    assert!(res.no_content);
    assert_eq!("Unsupported resource request.", res.error.unwrap().message);
}
