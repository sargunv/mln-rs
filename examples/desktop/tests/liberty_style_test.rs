// Loads OpenFreeMap Liberty (main.cpp's default style) over HTTPS through
// the path mln-desktop uses: Map -> its ResourceLoader ->
// OnlineFileSource -> HTTPFileSource (the platform transport),
// `Style::loadURL`, the Map's style-response event and the parser. Needs
// network access.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::Arc;

use mln_actor::{RunLoop, Timer};
use mln_map::{Map, MapObserver, MapOptions};
use mln_renderer::{RendererFrontend, RendererObserver, UpdateParameters};
use mln_util::chrono::Duration;
use mln_util::size::Size;

const LIBERTY: &str = "https://tiles.openfreemap.org/styles/liberty";

/// A frontend that only records updates (no renderer).
#[derive(Clone, Default)]
struct RecordingFrontend {
    updates: Rc<Cell<usize>>,
}

impl RendererFrontend for RecordingFrontend {
    fn reset(&self) {}
    fn set_observer(&self, _observer: Weak<dyn RendererObserver>) {}
    fn update(&self, _parameters: Arc<UpdateParameters>) {
        self.updates.set(self.updates.get() + 1);
    }
}

#[derive(Default)]
struct Observer {
    loaded: Cell<bool>,
    failed: RefCell<Option<String>>,
    run_loop: RefCell<Option<RunLoop>>,
}

impl MapObserver for Observer {
    fn on_did_finish_loading_style(&self) {
        self.loaded.set(true);
        if let Some(run_loop) = self.run_loop.borrow().as_ref() {
            run_loop.stop();
        }
    }
    fn on_did_fail_loading_map(&self, _error: mln_map::MapLoadError, what: &str) {
        *self.failed.borrow_mut() = Some(what.to_owned());
        if let Some(run_loop) = self.run_loop.borrow().as_ref() {
            run_loop.stop();
        }
    }
}

#[test]
#[ignore = "needs network access"]
fn loads_liberty_over_https() {
    let run_loop = RunLoop::new();
    let observer = Rc::new(Observer::default());
    *observer.run_loop.borrow_mut() = Some(run_loop.clone());
    let frontend = RecordingFrontend::default();
    let mut map =
        Map::new(&frontend, observer.clone(), MapOptions::new().with_size(Size::new(256, 256)).with_pixel_ratio(1.0))
            .unwrap();

    map.get_style_mut().load_url(LIBERTY);
    assert!(!map.get_style().is_loaded());

    // Give up after 60 s.
    let timeout = Timer::new();
    let handle = run_loop.handle();
    timeout.start(Duration::from_secs(60), Duration::zero(), move || handle.stop());
    run_loop.run();

    assert_eq!(None, *observer.failed.borrow());
    assert!(observer.loaded.get(), "the style did not load within 60 s");

    let style = map.get_style();
    assert_eq!(LIBERTY, style.get_url());
    let json: serde_json::Value = serde_json::from_str(&style.get_json()).unwrap();
    assert_eq!(json["sources"]["openmaptiles"]["type"], "vector");
    let layers = style.get_layers();
    assert!(layers.len() > 100, "{} layers", layers.len());
    assert_eq!("background", layers[0].get_id());
    assert!(style.get_sources().iter().any(|s| s.get_id() == "openmaptiles"));
    assert!(frontend.updates.get() > 0);
}
