//! The map's event queue: what it reports to its owner, what it does with
//! the events of its style and renderer, and its waker.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use mln_actor::RunLoop;
use mln_map::{
    CameraChange, CameraChangeMode, CameraChangeSource, CameraOptions, Map, MapEvent, MapMode, MapOptions,
    UnusedStyleImagePolicy,
};
use mln_renderer::RendererEvent;
use mln_storage::stub_file_source::{ResponseType, StubFileSource};
use mln_util::image::PremultipliedImage;
use mln_util::size::Size;

fn map(options: MapOptions) -> Map {
    Map::with_file_source(Arc::new(StubFileSource::new(ResponseType::Asynchronous)), options).expect("map")
}

/// Reports `event` to the map as its renderer would.
fn report(map: &Map, event: RendererEvent) {
    map.renderer_events().push(event);
}

const STYLE: &str = r#"{"version": 8, "center": [10, 20], "zoom": 3, "sources": {}, "layers": []}"#;

#[test]
fn camera_commands_report_their_changes_in_order() {
    let _loop = RunLoop::new();
    let mut map = map(MapOptions::new());
    map.drain_events();

    map.jump_to(&CameraOptions::default().with_zoom(Some(2.0)));
    let change = CameraChange { mode: CameraChangeMode::Immediate, source: CameraChangeSource::Command };
    let events = map.drain_events();
    assert!(
        matches!(events.as_slice(), [MapEvent::CameraWillChange(will), MapEvent::CameraDidChange(did)] if *will == change && *did == change),
        "{events:?}"
    );
    assert!(map.drain_events().is_empty());
}

#[test]
fn a_style_change_applies_at_the_next_call() {
    let _loop = RunLoop::new();
    let mut map = map(MapOptions::new());
    map.get_style_mut().load_json(STYLE);
    // The style reports its load to the map, which applies the style's
    // camera when it handles the report.
    assert_eq!(map.get_camera_options(None).zoom, Some(0.0));
    let updates = map.update_generation();
    map.process_events();
    assert_eq!(map.get_camera_options(None).zoom, Some(3.0));
    assert!(map.update_generation() > updates);

    let events = map.drain_events();
    let loading = events.iter().position(|event| matches!(event, MapEvent::LoadingStarted));
    let loaded = events.iter().position(|event| matches!(event, MapEvent::StyleLoaded));
    assert!(loading.is_some() && loading < loaded, "{events:?}");
}

#[test]
fn a_camera_command_after_a_style_load_wins_over_the_style_camera() {
    let _loop = RunLoop::new();
    let mut map = map(MapOptions::new());
    map.get_style_mut().load_json(STYLE);
    // The map handles the load before the command, so the style's center
    // stays and the command's zoom applies.
    map.jump_to(&CameraOptions::default().with_zoom(Some(5.0)));
    let camera = map.get_camera_options(None);
    assert_eq!(camera.zoom, Some(5.0));
    let center = camera.center.expect("center");
    assert!((center.latitude() - 20.0).abs() < 1e-9 && (center.longitude() - 10.0).abs() < 1e-9, "{center:?}");
}

#[test]
fn the_waker_runs_when_the_map_has_events() {
    let _loop = RunLoop::new();
    let mut map = map(MapOptions::new());
    map.drain_events();
    let wakes = Arc::new(AtomicUsize::new(0));
    let counter = wakes.clone();
    map.set_waker(Some(Arc::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    })));
    assert_eq!(wakes.load(Ordering::SeqCst), 0);

    report(&map, RendererEvent::Invalidate);
    assert_eq!(wakes.load(Ordering::SeqCst), 1);
    let updates = map.update_generation();
    map.process_events();
    assert_eq!(map.update_generation(), updates + 1);
}

#[test]
fn missing_images_reach_the_owner() {
    let _loop = RunLoop::new();
    let mut map = map(MapOptions::new());
    map.get_style_mut().load_json(STYLE);
    map.get_style_mut().add_image(image("present"));
    map.drain_events();

    report(&map, RendererEvent::StyleImageMissing("present".to_owned()));
    report(&map, RendererEvent::StyleImageMissing("absent".to_owned()));
    let missing: Vec<String> = map
        .drain_events()
        .into_iter()
        .filter_map(|event| match event {
            MapEvent::StyleImageMissing { id } => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(missing, ["absent"]);
}

fn image(id: &str) -> Arc<mln_style::image::Impl> {
    let pixels = PremultipliedImage::new(Size::new(1, 1));
    Arc::new(
        mln_style::image::Impl::new(id.to_owned(), pixels, 1.0, false, Vec::new(), Vec::new(), None, None, None)
            .expect("image"),
    )
}

#[test]
fn the_unused_image_policy_decides_what_stays() {
    let _loop = RunLoop::new();
    for (policy, kept) in [(UnusedStyleImagePolicy::Remove, false), (UnusedStyleImagePolicy::Keep, true)] {
        let mut map = map(MapOptions::new().with_unused_style_image_policy(policy));
        map.get_style_mut().load_json(STYLE);
        map.get_style_mut().add_image(image("marker"));
        report(&map, RendererEvent::UnusedStyleImages(vec!["marker".to_owned()]));
        map.process_events();
        assert_eq!(map.get_style().get_image("marker").is_some(), kept, "{policy:?}");
    }
}

#[test]
fn an_image_that_keeps_itself_outlasts_the_remove_policy() {
    let _loop = RunLoop::new();
    let mut map = map(MapOptions::new());
    map.get_style_mut().load_json(STYLE);
    let mut kept = (*image("kept")).clone();
    kept.keep_unused = true;
    map.get_style_mut().add_image(Arc::new(kept));
    map.get_style_mut().add_image(image("dropped"));
    report(&map, RendererEvent::UnusedStyleImages(vec!["kept".to_owned(), "dropped".to_owned()]));
    map.process_events();
    assert!(map.get_style().get_image("kept").is_some());
    assert!(map.get_style().get_image("dropped").is_none());
}

#[test]
fn a_still_image_completes_with_a_full_frame() {
    let _loop = RunLoop::new();
    let mut map = map(MapOptions::new().with_map_mode(MapMode::Static));
    map.get_style_mut().load_json(STYLE);
    let outcome = Rc::new(RefCell::new(None));
    let result = outcome.clone();
    map.render_still(Box::new(move |error| *result.borrow_mut() = Some(error)));

    let stats = Arc::default();
    let frame = |mode| RendererEvent::RenderFrameFinished {
        mode,
        needs_repaint: false,
        placement_changed: false,
        stats: Arc::clone(&stats),
    };
    report(&map, frame(mln_renderer::RenderMode::Partial));
    map.process_events();
    assert!(outcome.borrow().is_none());
    report(&map, frame(mln_renderer::RenderMode::Full));
    map.process_events();
    assert_eq!(*outcome.borrow(), Some(None));
    assert!(map.is_fully_loaded());
}
