// Ports test/map/map_snapshotter.test.cpp (a module of the gtest_map test
// crate), against the test server (mln_test_server; C++: 127.0.0.1:3000).

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use mln_actor::{RunLoop, Timer};
use mln_headless::map_snapshotter::{Callback, MapSnapshotter, MapSnapshotterObserver};
use mln_storage::ResourceOptions;
use mln_style::layers::fill_layer::FillLayer;
use mln_style::{Layer, PropertyValue};
use mln_test_server::HttpServer;
use mln_util::Color;
use mln_util::chrono::Duration;
use mln_util::size::Size;

use crate::support::{check_image, run_with_timeout};

const GREEN_BACKGROUND: &str = r#"{
        "version": 8,
        "layers": [{
            "id": "background",
            "type": "background",
            "paint": {"background-color": "green"}
        }]
    }"#;

/// `MapSnapshotter snapshotter(size, 1.0f, ResourceOptions())`.
fn snapshotter(size: Size) -> MapSnapshotter {
    MapSnapshotter::new(size, 1.0, &ResourceOptions::default()).unwrap()
}

/// A snapshot callback that expects an image of `size`, then stops the run
/// loop.
fn expect_size(run_loop: &RunLoop, size: Size) -> Callback {
    let run_loop = run_loop.clone();
    Box::new(move |ptr, image, _, _, _| {
        assert!(ptr.is_none(), "{ptr:?}");
        assert_eq!(size.width, image.size.width);
        assert_eq!(size.height, image.size.height);
        run_loop.stop();
    })
}

/// `SnapshotterObserver` of the C++ test.
#[derive(Default)]
struct SnapshotterObserver {
    did_finish_loading_style_callback: RefCell<Option<Box<dyn Fn()>>>,
}

impl MapSnapshotterObserver for SnapshotterObserver {
    fn on_did_finish_loading_style(&self) {
        if let Some(callback) = self.did_finish_loading_style_callback.borrow().as_ref() {
            callback();
        }
    }
}

// gtest: MapSnapshotter.setStyleJSON
#[test]
fn set_style_json() {
    let run_loop = RunLoop::new();
    let snapshotter = snapshotter(Size::new(32, 16));
    snapshotter.set_style_json(GREEN_BACKGROUND);

    snapshotter.snapshot(expect_size(&run_loop, Size::new(32, 16)));

    run_with_timeout(&run_loop, 10);
}

// gtest: MapSnapshotter.setSize
#[test]
fn set_size() {
    let run_loop = RunLoop::new();
    let snapshotter = snapshotter(Size::new(32, 16));
    snapshotter.set_style_json(GREEN_BACKGROUND);

    snapshotter.set_size(Size::new(16, 32));

    snapshotter.snapshot(expect_size(&run_loop, Size::new(16, 32)));

    run_with_timeout(&run_loop, 10);
}

// gtest: MapSnapshotter.setStyleURL
#[test]
fn set_style_url() {
    let server = HttpServer::new();
    let run_loop = RunLoop::new();
    let snapshotter = snapshotter(Size::new(64, 32));
    snapshotter.set_style_url(&server.url("/online/style.json"));

    snapshotter.snapshot(expect_size(&run_loop, Size::new(64, 32)));

    run_with_timeout(&run_loop, 10);
}

// gtest: MapSnapshotter.cancel
#[test]
fn cancel() {
    let server = HttpServer::new();
    let run_loop = RunLoop::new();
    let snapshotter = snapshotter(Size::new(1, 1));
    snapshotter.set_style_url(&server.url("/online/style.json"));
    snapshotter.snapshot(Box::new(|_, _, _, _, _| panic!("a canceled snapshot calls back")));

    snapshotter.cancel();

    let timer = Timer::new();
    let stop = run_loop.clone();
    timer.start(Duration::from_millis(100), Duration::zero(), move || stop.stop());

    run_with_timeout(&run_loop, 10);
}

// gtest: MapSnapshotter.runtimeStyling
#[test]
fn runtime_styling() {
    let server = HttpServer::new();
    let run_loop = RunLoop::new();
    let observer = Rc::new(SnapshotterObserver::default());
    let snapshotter = Rc::new(
        MapSnapshotter::with_observer(Size::new(256, 128), 1.0, &ResourceOptions::default(), observer.clone()).unwrap(),
    );
    snapshotter.set_style_url(&server.url("/online/style.json"));

    let weak: Weak<MapSnapshotter> = Rc::downgrade(&snapshotter);
    let stop = run_loop.clone();
    *observer.did_finish_loading_style_callback.borrow_mut() = Some(Box::new(move || {
        let Some(snapshotter) = weak.upgrade() else {
            return;
        };
        let mut fill_layer = FillLayer::new("green_fill", "mapbox");
        fill_layer.set_source_layer("water");
        fill_layer.set_fill_color(&PropertyValue::Constant(Color::new(0.25, 0.88, 0.82, 1.0)));
        snapshotter.map().get_style_mut().add_layer(Box::new(fill_layer), None).unwrap();
        let stop = stop.clone();
        snapshotter.snapshot(Box::new(move |ptr, image, _, _, _| {
            assert!(ptr.is_none(), "{ptr:?}");
            assert_eq!(256, image.size.width);
            assert_eq!(128, image.size.height);
            check_image("test/fixtures/map/map_snapshotter_overlay", &image, 0.005, 0.1);
            stop.stop();
        }));
    }));

    run_with_timeout(&run_loop, 10);
}

// gtest: MapSnapshotter.annotation
#[test]
#[ignore = "TODO(port): annotations (MapSnapshotter::addAnnotationImage/addAnnotation, Map::addAnnotation*, AnnotationManager); see map_snapshotter.test.cpp:150"]
fn annotation() {
    // Symbol, fill and line annotations show in the snapshot
    // (test/fixtures/map/map_snapshotter_annotation, 0.005, 0.1).
}
