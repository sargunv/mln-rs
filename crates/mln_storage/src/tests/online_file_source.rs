// Ports test/storage/online_file_source.test.cpp against the test server
// (mln_test_server, one per test) instead of the C++ one on port 3000. The
// tests that request through the network take `serial()`: NetworkStatus is
// process-wide and C++ runs one test at a time.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use feature::Value;
use mln_actor::{RunLoop, Timer};
use mln_test_server::{HttpServer, closed_port_url};
use mln_util::chrono::{Clock, Duration, Timestamp, now};
use mln_util::tile_server_options::TileServerOptions;

use super::http_file_source::{LoadState, load_request};
use super::{Req, req, serial};
use crate::file_source::{API_BASE_URL_KEY, API_KEY_KEY, FileSource, MAX_CONCURRENT_REQUESTS_KEY};
use crate::http_header_transform::HttpHeaderTransform;
use crate::network_status::{NetworkStatus, Status};
use crate::online_file_source::OnlineFileSource;
use crate::resource::{Kind, Priority, Resource};
use crate::resource_options::ResourceOptions;
use crate::resource_transform::{FinishedCallback, ResourceTransform};
use crate::response::{Error, Reason, Response};

/// Windows doesn't fail immediately like other OS (C++: 2.0 on WIN32).
const CONNECTION_TIMEOUT: f64 = 0.0;

fn online_file_source() -> Rc<dyn FileSource> {
    Rc::new(OnlineFileSource::new(&ResourceOptions::default_options()))
}

fn data(res: &Response) -> &[u8] {
    res.data.as_deref().map(Vec::as_slice).expect("data")
}

/// `std::chrono::duration<const double>(Clock::now() - start).count()`.
fn seconds_since(start: mln_util::chrono::TimePoint) -> f64 {
    (Clock::now() - start).count() as f64 / 1e9
}

fn string_property(fs: &dyn FileSource, key: &str) -> String {
    match fs.get_property(key) {
        Value::String(s) => s,
        other => panic!("{key}: {other:?}"),
    }
}

// gtest: OnlineFileSource.Cancel
#[test]
fn cancel() {
    let run_loop = RunLoop::new();
    let fs = online_file_source();

    drop(fs.request(
        &Resource::new(Kind::Unknown, "http://127.0.0.1:3000/test"),
        Box::new(|_: Response| panic!("Callback should not be called")),
    ));

    run_loop.run_once();
}

// gtest: OnlineFileSource.CancelMultiple
#[test]
fn cancel_multiple() {
    let _serial = serial();
    let server = HttpServer::new();
    let run_loop = RunLoop::new();
    let fs = online_file_source();

    let resource = Resource::new(Kind::Unknown, server.url("/test"));

    let req2 = fs.request(&resource, Box::new(|_: Response| panic!("Callback should not be called")));
    let req = req();
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
    drop(req2);

    run_loop.run();
}

// gtest: OnlineFileSource.TemporaryError
#[test]
fn temporary_error() {
    let _serial = serial();
    let server = HttpServer::new();
    let run_loop = RunLoop::new();
    let fs = online_file_source();

    let start = Clock::now();
    let counter = Cell::new(0);

    let stop = run_loop.clone();
    let _req = fs.request(
        &Resource::new(Kind::Unknown, server.url("/temporary-error")),
        Box::new(move |res: Response| {
            let n = counter.get();
            counter.set(n + 1);
            match n {
                0 => {
                    let duration = seconds_since(start);
                    assert!(0.2 > duration, "Initial error request took too long");
                    let error = res.error.as_ref().expect("error");
                    assert_eq!(Reason::Server, error.reason);
                    assert_eq!("HTTP status code 500", error.message);
                    assert!(res.data.is_none());
                    assert!(res.expires.is_none());
                    assert!(!res.must_revalidate);
                    assert!(res.modified.is_none());
                    assert!(res.etag.is_none());
                }
                1 => {
                    let duration = seconds_since(start);
                    assert!(0.99 < duration, "Backoff timer didn't wait 1 second");
                    assert!(1.2 > duration, "Backoff timer fired too late");
                    assert_eq!(None, res.error);
                    assert_eq!(b"Hello World!", data(&res));
                    assert!(res.expires.is_none());
                    assert!(!res.must_revalidate);
                    assert!(res.modified.is_none());
                    assert!(res.etag.is_none());
                    stop.stop();
                }
                _ => {}
            }
        }),
    );

    run_loop.run();
}

// gtest: OnlineFileSource.ConnectionError
#[test]
fn connection_error() {
    let _serial = serial();
    let run_loop = RunLoop::new();
    let fs = online_file_source();

    let start = Clock::now();
    let counter = Cell::new(0);
    let wait = Cell::new(CONNECTION_TIMEOUT);

    let req = req();
    let (r, stop) = (req.clone(), run_loop.clone());
    *req.borrow_mut() = Some(fs.request(
        &Resource::new(Kind::Unknown, closed_port_url("/")),
        Box::new(move |res: Response| {
            let duration = seconds_since(start);
            assert!(wait.get() - 0.01 < duration, "Backoff timer didn't wait 1 second");
            assert!(wait.get() + 0.3 > duration, "Backoff timer fired too late");
            let error = res.error.as_ref().expect("error");
            assert_eq!(Reason::Connection, error.reason);
            assert!(res.data.is_none());
            assert!(res.expires.is_none());
            assert!(!res.must_revalidate);
            assert!(res.modified.is_none());
            assert!(res.etag.is_none());

            if counter.get() == 2 {
                r.borrow_mut().take();
                stop.stop();
            }
            wait.set(wait.get() + f64::from(1 << counter.get()) + CONNECTION_TIMEOUT);
            counter.set(counter.get() + 1);
        }),
    ));

    run_loop.run();
}

// gtest: OnlineFileSource.Timeout
#[test]
fn timeout() {
    let _serial = serial();
    let server = HttpServer::new();
    let run_loop = RunLoop::new();
    let fs = online_file_source();

    let counter = Rc::new(Cell::new(0));

    let resource = Resource::new(Kind::Unknown, server.url("/test?cachecontrol=max-age=1"));
    let req = req();
    let (r, stop, c) = (req.clone(), run_loop.clone(), counter.clone());
    *req.borrow_mut() = Some(fs.request(
        &resource,
        Box::new(move |res: Response| {
            c.set(c.get() + 1);
            assert_eq!(None, res.error);
            assert_eq!(b"Hello World!", data(&res));
            assert!(res.expires.is_some());
            assert!(!res.must_revalidate);
            assert!(res.modified.is_none());
            assert!(res.etag.is_none());
            if c.get() == 4 {
                r.borrow_mut().take();
                stop.stop();
            }
        }),
    ));

    run_loop.run();

    assert_eq!(4, counter.get());
}

// gtest: OnlineFileSource.RetryDelayOnExpiredTile
#[test]
fn retry_delay_on_expired_tile() {
    let _serial = serial();
    let server = HttpServer::new();
    let run_loop = RunLoop::new();
    let fs = online_file_source();

    let counter = Rc::new(Cell::new(0));

    let resource = Resource::new(Kind::Unknown, server.url("/test?expires=10000"));
    let (stop, c) = (run_loop.clone(), counter.clone());
    let _req = fs.request(
        &resource,
        Box::new(move |res: Response| {
            c.set(c.get() + 1);
            assert_eq!(None, res.error);
            assert!(now() > res.expires.expect("expires"));
            assert!(!res.must_revalidate);
            stop.stop();
        }),
    );

    run_loop.run();

    assert_eq!(1, counter.get());
}

// gtest: OnlineFileSource.RetryOnClockSkew
#[test]
fn retry_on_clock_skew() {
    let _serial = serial();
    let server = HttpServer::new();
    let run_loop = RunLoop::new();
    let fs = online_file_source();

    let counter = Cell::new(0);

    let resource = Resource::new(Kind::Unknown, server.url("/clockskew"));
    let stop = run_loop.clone();
    let _req1 = fs.request(
        &resource,
        Box::new(move |res: Response| {
            assert!(!res.must_revalidate);
            let n = counter.get();
            counter.set(n + 1);
            match n {
                0 => {
                    assert_eq!(None, res.error);
                    assert!(now() > res.expires.expect("expires"));
                }
                1 => {
                    assert_eq!(None, res.error);

                    let now = now().secs_since_epoch();
                    let expires = res.expires.expect("expires").secs_since_epoch();
                    assert!(now + 40 < expires, "Expiration not interpolated to 60s");
                    assert!(now + 80 > expires, "Expiration not interpolated to 60s");

                    stop.stop();
                }
                _ => {}
            }
        }),
    );

    run_loop.run();
}

// gtest: OnlineFileSource.RespectPriorExpires
#[test]
fn respect_prior_expires() {
    let _serial = serial();
    let server = HttpServer::new();
    let run_loop = RunLoop::new();
    let fs = online_file_source();

    // Very long expiration time, should never arrive.
    let mut resource1 = Resource::new(Kind::Unknown, server.url("/test"));
    resource1.prior_expires = Some(Timestamp::from_secs(now().secs_since_epoch() + 100000));

    let _req1 = fs.request(&resource1, Box::new(|_: Response| panic!("Should never be called")));

    // No expiration time, should be requested immediately.
    let resource2 = Resource::new(Kind::Unknown, server.url("/test"));

    let stop = run_loop.clone();
    let _req2 = fs.request(&resource2, Box::new(move |_: Response| stop.stop()));

    // Very long expiration time, should never arrive.
    let mut resource3 = Resource::new(Kind::Unknown, server.url("/test"));
    resource3.prior_expires = Some(Timestamp::from_secs(now().secs_since_epoch() + 100000));

    let _req3 = fs.request(&resource3, Box::new(|_: Response| panic!("Should never be called")));

    run_loop.run();
}

// gtest: OnlineFileSource.RespectMinimumUpdateInterval
#[test]
fn respect_minimum_update_interval() {
    let _serial = serial();
    let server = HttpServer::new();
    let run_loop = RunLoop::new();
    let fs = online_file_source();

    let start = now();
    let mut resource = Resource::new(Kind::Unknown, server.url("/test"));
    // `start + duration_cast<Seconds>(Milliseconds(100))` = start.
    resource.prior_expires = Some(start);
    resource.minimum_update_interval = Duration::from_secs(1);

    let minimum_update_interval = resource.minimum_update_interval;
    let stop = run_loop.clone();
    let _req = fs.request(
        &resource,
        Box::new(move |_: Response| {
            let wait = Duration::from_secs(now().secs_since_epoch() - start.secs_since_epoch());
            assert!(wait >= minimum_update_interval);
            stop.stop();
        }),
    );

    run_loop.run();
}

// gtest: OnlineFileSource.Load
#[test]
fn load() {
    let _serial = serial();
    let server = HttpServer::new();
    let run_loop = RunLoop::new();
    let fs = online_file_source();

    const CONCURRENCY: usize = 50;
    const MAX: i32 = 10000;

    let state = Rc::new(LoadState { reqs: (0..CONCURRENCY).map(|_| req()).collect(), number: Cell::new(1) });
    let base = server.url("/load/");
    let url: Rc<dyn Fn(i32) -> String> = Rc::new(move |n| format!("{base}{n}"));

    for i in 0..CONCURRENCY {
        load_request(&fs, &state, &url, &run_loop, i, MAX);
    }

    run_loop.run();
}

// Test for https://github.com/mapbox/mapbox-gl-native/issues/2123
//
// A request is made. While the request is in progress, the network status
// changes. This should trigger an immediate retry of all requests that are
// not in progress. This test makes sure that we don't accidentally
// double-trigger the request.

// gtest: OnlineFileSource.NetworkStatusChange
#[test]
fn network_status_change() {
    let _serial = serial();
    let server = HttpServer::new();
    let run_loop = RunLoop::new();
    let fs = online_file_source();

    let resource = Resource::new(Kind::Unknown, server.url("/delayed"));

    // This request takes 200 milliseconds to answer.
    let req = req();
    let (r, stop) = (req.clone(), run_loop.clone());
    *req.borrow_mut() = Some(fs.request(
        &resource,
        Box::new(move |res: Response| {
            r.borrow_mut().take();
            assert_eq!(None, res.error);
            assert_eq!(b"Response", data(&res));
            assert!(res.expires.is_none());
            assert!(!res.must_revalidate);
            assert!(res.modified.is_none());
            assert!(res.etag.is_none());
            stop.stop();
        }),
    ));

    // After 50 milliseconds, we're going to trigger a NetworkStatus change.
    let reachable_timer = Timer::new();
    reachable_timer.start(Duration::from_millis(50), Duration::zero(), NetworkStatus::reachable);

    run_loop.run();
}

// Tests that a change in network status preempts requests that failed due to
// connection or reachability issues.

// gtest: OnlineFileSource.NetworkStatusChangePreempt
#[test]
fn network_status_change_preempt() {
    let _serial = serial();
    let run_loop = RunLoop::new();
    let fs = online_file_source();
    fs.pause();

    let start = Clock::now();
    let counter = Cell::new(0);

    let resource = Resource::new(Kind::Unknown, closed_port_url("/test"));

    let wait = Cell::new(CONNECTION_TIMEOUT);
    let req = req();
    let (r, stop) = (req.clone(), run_loop.clone());
    *req.borrow_mut() = Some(fs.request(
        &resource,
        Box::new(move |res: Response| {
            let duration = seconds_since(start);
            let n = counter.get();
            if n == 0 {
                assert!(wait.get() + 0.2 > duration, "Response came in too late");
            } else if n == 1 {
                assert!(wait.get() + 0.39 < duration, "Preempted retry triggered too early");
                assert!(wait.get() + 0.6 > duration, "Preempted retry triggered too late");
            } else if n > 1 {
                panic!("Retried too often");
            }
            let error = res.error.as_ref().expect("error");
            assert_eq!(Reason::Connection, error.reason);
            assert!(res.data.is_none());
            assert!(res.expires.is_none());
            assert!(!res.must_revalidate);
            assert!(res.modified.is_none());
            assert!(res.etag.is_none());

            wait.set(wait.get() + CONNECTION_TIMEOUT);

            counter.set(n + 1);
            if n == 1 {
                r.borrow_mut().take();
                stop.stop();
            }
        }),
    ));

    // After 400 milliseconds + connectionTimeout, we're going to trigger a
    // NetworkStatus change.
    let reachable_timer = Timer::new();
    reachable_timer.start(
        Duration::from_millis((CONNECTION_TIMEOUT * 1000.0) as i64 + 400),
        Duration::zero(),
        NetworkStatus::reachable,
    );

    fs.resume();
    run_loop.run();
}

// gtest: OnlineFileSource.NetworkStatusOnlineOffline
#[test]
fn network_status_online_offline() {
    let _serial = serial();
    let server = HttpServer::new();
    let run_loop = RunLoop::new();
    let fs = online_file_source();

    let resource = Resource::new(Kind::Unknown, server.url("/test"));

    assert_eq!(NetworkStatus::get(), Status::Online, "Default status should be Online");
    NetworkStatus::set(Status::Offline);

    let online_timer = Timer::new();
    online_timer.start(Duration::from_millis(100), Duration::zero(), || NetworkStatus::set(Status::Online));

    let req = req();
    let (r, stop) = (req.clone(), run_loop.clone());
    *req.borrow_mut() = Some(fs.request(
        &resource,
        Box::new(move |res: Response| {
            r.borrow_mut().take();

            assert_eq!(None, res.error);
            assert!(res.data.is_some());

            assert_eq!(NetworkStatus::get(), Status::Online, "Triggered before set back to Online");

            stop.stop();
        }),
    ));

    run_loop.run();
}

fn rate_limit_test(path: &str, expect_retry_after: bool) {
    let _serial = serial();
    let server = HttpServer::new();
    let run_loop = RunLoop::new();
    let fs = online_file_source();

    let stop = run_loop.clone();
    let _req = fs.request(
        &Resource::new(Kind::Unknown, server.url(path)),
        Box::new(move |res: Response| {
            let error = res.error.as_ref().expect("error");
            assert_eq!(Reason::RateLimit, error.reason);
            if expect_retry_after {
                let retry_after = error.retry_after.expect("retry after");
                assert!(now() < retry_after);
            } else {
                assert!(error.retry_after.is_none());
            }
            stop.stop();
        }),
    );

    run_loop.run();
}

// gtest: OnlineFileSource.RateLimitStandard
#[test]
fn rate_limit_standard() {
    rate_limit_test("/rate-limit?std=true", true);
}

// gtest: OnlineFileSource.RateLimitMBX
#[test]
fn rate_limit_mbx() {
    rate_limit_test("/rate-limit?mbx=true", true);
}

// gtest: OnlineFileSource.RateLimitDefault
#[test]
fn rate_limit_default() {
    rate_limit_test("/rate-limit", false);
}

// gtest: OnlineFileSource.GetBaseURLAndApiKeyWhilePaused
#[test]
fn get_base_url_and_api_key_while_paused() {
    let _run_loop = RunLoop::new();
    let fs = online_file_source();

    fs.pause();

    let base_url = "http://url";
    let api_key = "api_key";

    fs.set_property(API_BASE_URL_KEY, &Value::String(base_url.to_owned()));
    fs.set_property(API_KEY_KEY, &Value::String(api_key.to_owned()));

    assert_eq!(string_property(&*fs, API_BASE_URL_KEY), base_url);
    assert_eq!(string_property(&*fs, API_KEY_KEY), api_key);
}

// gtest: OnlineFileSource.ChangeAPIBaseURL
#[test]
fn change_api_base_url() {
    let _run_loop = RunLoop::new();
    let fs = online_file_source();
    assert_eq!(
        ResourceOptions::default_options().tile_server_options().base_url(),
        string_property(&*fs, API_BASE_URL_KEY)
    );
    let custom_url = "test.domain";
    fs.set_property(API_BASE_URL_KEY, &Value::String(custom_url.to_owned()));
    assert_eq!(custom_url, string_property(&*fs, API_BASE_URL_KEY));

    let updated_tile_server_options = fs.get_resource_options().tile_server_options();
    assert_eq!(custom_url, updated_tile_server_options.base_url());
}

// gtest: OnlineFileSource.ChangeTileServerOptions
#[test]
fn change_tile_server_options() {
    let _run_loop = RunLoop::new();
    let fs = online_file_source();

    let tile_server_opts_changed = TileServerOptions::new()
        .with_api_key_parameter_name("apiKeyChanged")
        .with_base_url("baseURLChanged")
        .with_default_style("defaultStyleChanged")
        .with_glyphs_template("defaultGlyphsTemplateChanged", "glyphsDomainChanged", None)
        .with_source_template("sourceTemplateChanged", "sourceDomainChanged", None)
        .with_sprites_template("spritesTemplateChanged", "spritesDomainChanged", None)
        .with_style_template("styleTemplateChanged", "styleDomainChanged", None)
        .with_tile_template("tileTemplateChanged", "tileDomainChanged", None)
        .with_uri_scheme_alias("uriSchemeAliasChanged");
    let res_opts =
        ResourceOptions::new().with_api_key("changedAPIKey").with_tile_server_options(tile_server_opts_changed.clone());

    fs.set_resource_options(res_opts.clone());

    let actual_resource_opts = fs.get_resource_options();

    assert_eq!(tile_server_opts_changed.base_url(), string_property(&*fs, API_BASE_URL_KEY));
    assert_eq!(actual_resource_opts.api_key(), string_property(&*fs, API_KEY_KEY));

    let a = res_opts.tile_server_options();
    let b = &tile_server_opts_changed;
    assert_eq!(res_opts.api_key(), actual_resource_opts.api_key());
    assert_eq!(a.default_style(), b.default_style());
    assert_eq!(a.base_url(), b.base_url());
    assert_eq!(a.glyphs_template(), b.glyphs_template());
    assert_eq!(a.glyphs_domain_name(), b.glyphs_domain_name());
    assert_eq!(a.glyphs_version_prefix(), b.glyphs_version_prefix());
    assert_eq!(a.source_template(), b.source_template());
    assert_eq!(a.source_domain_name(), b.source_domain_name());
    assert_eq!(a.source_version_prefix(), b.source_version_prefix());
    assert_eq!(a.sprites_template(), b.sprites_template());
    assert_eq!(a.sprites_domain_name(), b.sprites_domain_name());
    assert_eq!(a.sprites_version_prefix(), b.sprites_version_prefix());
    assert_eq!(a.tile_template(), b.tile_template());
    assert_eq!(a.tile_domain_name(), b.tile_domain_name());
    assert_eq!(a.tile_version_prefix(), b.tile_version_prefix());
    assert_eq!(a.style_template(), b.style_template());
    assert_eq!(a.style_domain_name(), b.style_domain_name());
    assert_eq!(a.style_version_prefix(), b.style_version_prefix());
}

// gtest: OnlineFileSource.LowHighPriorityRequests
#[test]
fn low_high_priority_requests() {
    let _serial = serial();
    let server = HttpServer::new();
    let run_loop = RunLoop::new();
    let fs = online_file_source();
    let response_counter = Rc::new(Cell::new(0usize));
    const NUM_REQUESTS: usize = 3;

    NetworkStatus::set(Status::Offline);
    fs.set_property(MAX_CONCURRENT_REQUESTS_KEY, &Value::Uint(1));
    // After DefaultFileSource was split, OnlineFileSource lives on a separate
    // thread. Pause OnlineFileSource, so that messages are queued for
    // processing.
    fs.pause();

    // First regular request.
    let regular1 = Resource::new(Kind::Unknown, server.url("/load/1"));
    let req_0 = req();
    let (r, c) = (req_0.clone(), response_counter.clone());
    *req_0.borrow_mut() = Some(fs.request(
        &regular1,
        Box::new(move |_: Response| {
            c.set(c.get() + 1);
            r.borrow_mut().take();
        }),
    ));

    // Low priority request that will be queued and should be requested last.
    let mut low_prio = Resource::new(Kind::Unknown, server.url("/load/2"));
    low_prio.set_priority(Priority::Low);
    let req_1 = req();
    let (r, c, stop) = (req_1.clone(), response_counter.clone(), run_loop.clone());
    *req_1.borrow_mut() = Some(fs.request(
        &low_prio,
        Box::new(move |_: Response| {
            c.set(c.get() + 1);
            r.borrow_mut().take();
            assert_eq!(c.get(), NUM_REQUESTS); // make sure this is responded last
            stop.stop();
        }),
    ));

    // Second regular priority request that should de-preoritize low priority
    // request.
    let regular2 = Resource::new(Kind::Unknown, server.url("/load/3"));
    let req_2 = req();
    let (r, c) = (req_2.clone(), response_counter.clone());
    *req_2.borrow_mut() = Some(fs.request(
        &regular2,
        Box::new(move |_: Response| {
            c.set(c.get() + 1);
            r.borrow_mut().take();
        }),
    ));

    // Online before resuming (C++ sets it after), so the online thread takes
    // every queued request with the network up. If it saw the switch between
    // two of them, those taken offline would wait for the reachability
    // notification and start after the later ones, low priority included.
    NetworkStatus::set(Status::Online);
    fs.resume();
    run_loop.run();
}

// gtest: OnlineFileSource.LowHighPriorityRequestsMany
#[test]
fn low_high_priority_requests_many() {
    let _serial = serial();
    let server = HttpServer::new();
    let run_loop = RunLoop::new();
    let fs = online_file_source();
    let response_counter = Rc::new(Cell::new(0));
    let correct_low = Rc::new(Cell::new(0));
    let correct_regular = Rc::new(Cell::new(0));

    NetworkStatus::set(Status::Offline);
    fs.set_property(MAX_CONCURRENT_REQUESTS_KEY, &Value::Uint(1));
    fs.pause();

    let collector: Rc<RefCell<Vec<Req>>> = Rc::new(RefCell::new(Vec::new()));

    for num_reqs in 0..20 {
        let r = req();
        if num_reqs % 2 == 0 {
            let (counter, correct_regular) = (response_counter.clone(), correct_regular.clone());
            *r.borrow_mut() = Some(fs.request(
                &Resource::new(Kind::Unknown, server.url(&format!("/load/{num_reqs}"))),
                Box::new(move |_: Response| {
                    counter.set(counter.get() + 1);

                    if counter.get() <= 10 {
                        // count the high priority requests that arrive late correctly
                        correct_regular.set(correct_regular.get() + 1);
                    }
                }),
            ));
        } else {
            let mut resource = Resource::new(Kind::Unknown, server.url(&format!("/load/{num_reqs}")));
            resource.set_priority(Priority::Low);

            let (counter, correct_low, correct_regular, stop, collector) = (
                response_counter.clone(),
                correct_low.clone(),
                correct_regular.clone(),
                run_loop.clone(),
                collector.clone(),
            );
            *r.borrow_mut() = Some(fs.request(
                &resource,
                Box::new(move |_: Response| {
                    counter.set(counter.get() + 1);

                    if counter.get() > 10 {
                        // count the low priority requests that arrive late correctly
                        correct_low.set(correct_low.get() + 1);
                    }

                    // stop and check correctness after last low priority
                    // request is responded
                    if 20 == counter.get() {
                        stop.stop();
                        for collected_req in collector.borrow().iter() {
                            collected_req.borrow_mut().take();
                        }
                        assert!(correct_low.get() >= 9);
                        assert!(correct_regular.get() >= 9);
                    }
                }),
            ));
        }
        collector.borrow_mut().push(r);
    }

    // Online before resuming, as in LowHighPriorityRequests.
    NetworkStatus::set(Status::Online);
    fs.resume();
    run_loop.run();
}

// gtest: OnlineFileSource.MaximumConcurrentRequests
#[test]
fn maximum_concurrent_requests() {
    let _run_loop = RunLoop::new();
    let fs = online_file_source();

    assert_eq!(fs.get_property(MAX_CONCURRENT_REQUESTS_KEY), Value::Uint(20));

    fs.set_property(MAX_CONCURRENT_REQUESTS_KEY, &Value::Uint(10));
    assert_eq!(fs.get_property(MAX_CONCURRENT_REQUESTS_KEY), Value::Uint(10));
}

// gtest: OnlineFileSource.RequestSameUrlMultipleTimes
#[test]
fn request_same_url_multiple_times() {
    let _serial = serial();
    let server = HttpServer::new();
    let run_loop = RunLoop::new();
    let fs = online_file_source();

    let count = Rc::new(Cell::new(0));
    let mut requests = Vec::new();

    for _ in 0..100 {
        let (count, stop) = (count.clone(), run_loop.clone());
        requests.push(fs.request(
            &Resource::new(Kind::Unknown, server.url("/load")),
            Box::new(move |_: Response| {
                count.set(count.get() + 1);
                if count.get() == 100 {
                    stop.stop();
                }
            }),
        ));
    }

    run_loop.run();
}

// gtest: OnlineFileSource.FailedResourceTransform
#[test]
fn failed_resource_transform() {
    let run_loop = RunLoop::new();
    let fs = online_file_source();
    fs.set_resource_transform(ResourceTransform::new(Some(Arc::new(
        |_kind: Kind, _url: &str, finished: FinishedCallback| {
            finished(Err(Error::new(Reason::Other, "No credential")));
        },
    ))));

    let stop = run_loop.clone();
    let _req = fs.request(
        &Resource::new(Kind::Unknown, "http://127.0.0.1:3000/test"),
        Box::new(move |res: Response| {
            let error = res.error.as_ref().expect("error");
            assert_eq!(Reason::Other, error.reason);
            assert_eq!("No credential", error.message);
            assert!(res.data.is_none());
            stop.stop();
        }),
    );

    run_loop.run();
}

// A retry of a request that the resource transform failed asks the transform
// again instead of loading the original URL.
// gtest: OnlineFileSource.RetryFailedResourceTransform
#[test]
fn retry_failed_resource_transform() {
    let _serial = serial();
    let server = HttpServer::new();
    let run_loop = RunLoop::new();
    let fs = online_file_source();

    let transforms = Arc::new(AtomicU32::new(0));
    let (delayed, test) = (server.url("/delayed"), server.url("/test"));
    let counter = transforms.clone();
    fs.set_resource_transform(ResourceTransform::new(Some(Arc::new(
        move |_kind: Kind, url: &str, finished: FinishedCallback| {
            assert_eq!(delayed, url);
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                finished(Err(Error::new(Reason::Connection, "Offline")));
            } else {
                finished(Ok(test.clone()));
            }
        },
    ))));

    let responses = Rc::new(Cell::new(0));
    let stop = run_loop.clone();
    let _req = fs.request(
        &Resource::new(Kind::Unknown, server.url("/delayed")),
        Box::new(move |res: Response| {
            responses.set(responses.get() + 1);
            match responses.get() {
                1 => {
                    let error = res.error.as_ref().expect("error");
                    assert_eq!(Reason::Connection, error.reason);
                    assert_eq!("Offline", error.message);
                    // Retry now instead of after the backoff.
                    NetworkStatus::reachable();
                }
                2 => {
                    assert_eq!(None, res.error);
                    assert_eq!(b"Hello World!", data(&res));
                    assert_eq!(2, transforms.load(Ordering::SeqCst));
                    stop.stop();
                }
                _ => {}
            }
        }),
    );

    run_loop.run();
}

// The header transform sees the URL that the resource transform wrote, and
// runs again for a retry.
#[test]
fn a_header_transform_runs_for_every_attempt() {
    let _serial = serial();
    let server = HttpServer::new();
    let run_loop = RunLoop::new();
    let fs = online_file_source();

    let target = server.url("/temporary-error");
    let rewritten = target.clone();
    fs.set_resource_transform(ResourceTransform::new(Some(Arc::new(
        move |_kind: Kind, _url: &str, finished: FinishedCallback| finished(Ok(rewritten.clone())),
    ))));
    let urls = Arc::new(Mutex::new(Vec::new()));
    let seen = urls.clone();
    fs.set_http_header_transform(HttpHeaderTransform::new(Some(Arc::new(move |kind: Kind, url: &str| {
        seen.lock().unwrap().push((kind, url.to_owned()));
        vec![("X-Test-Token".to_owned(), "secret-token".to_owned())]
    }))));

    let responses = Rc::new(Cell::new(0));
    let stop = run_loop.clone();
    let _req = fs.request(
        &Resource::new(Kind::Source, "localhost://temporary-error"),
        Box::new(move |res: Response| {
            responses.set(responses.get() + 1);
            if responses.get() == 2 {
                assert_eq!(None, res.error);
                assert_eq!(b"Hello World!", data(&res));
                stop.stop();
            }
        }),
    );

    run_loop.run();
    assert_eq!(vec![(Kind::Source, target.clone()), (Kind::Source, target)], *urls.lock().unwrap());
}
