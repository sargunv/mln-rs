// Command order, completions and the lifecycle of runtimes and maps. They
// follow maplibre-native-ffi's ABI tests (tests/native/abi/runtime/
// lifecycle.c, completion/completion.c and map/command_groups.c at b20632a)
// where those test the same rules.

mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};

use mln_map::{CameraOptions, CameraUpdate};
use mln_runtime::{ErrorKind, MapOptions, ResourceRequest, Runtime, RuntimeOptions};
use mln_util::size::Size;
use support::{EMPTY_STYLE, TIMEOUT, TempDir, eventually, map, map_options, runtime, wait, wait_err};

#[test]
fn commands_run_in_the_order_each_thread_sent_them() {
    let runtime = runtime();
    let map = map(&runtime);
    // The gate holds the runtime's thread until every command is sent, so
    // each completion's callback runs there, in the order they apply.
    let (open, gate) = channel::<()>();
    drop(map.with_core(move |_| gate.recv_timeout(TIMEOUT)).unwrap());
    let ran = Arc::new(Mutex::new(Vec::new()));
    let applied = Arc::new(Mutex::new(Vec::new()));
    let senders: Vec<_> = (0..4)
        .map(|thread| {
            let (map, ran, applied) = (map.clone(), ran.clone(), applied.clone());
            std::thread::spawn(move || {
                for i in 0..50 {
                    let ran = ran.clone();
                    drop(map.with_core(move |_| ran.lock().unwrap().push((thread, i))).unwrap());
                    let applied = applied.clone();
                    map.request_repaint().unwrap().on_complete(move |result| {
                        applied.lock().unwrap().push((thread, result.unwrap().sequence));
                    });
                }
            })
        })
        .collect();
    for sender in senders {
        sender.join().unwrap();
    }
    open.send(()).unwrap();
    wait(map.request_repaint());

    let applied = applied.lock().unwrap();
    assert_eq!(200, applied.len());
    assert!(applied.windows(2).all(|pair| pair[0].1 < pair[1].1), "{applied:?}");
    let ran = ran.lock().unwrap();
    assert_eq!(200, ran.len());
    for thread in 0..4 {
        let order: Vec<_> = ran.iter().filter(|(t, _)| *t == thread).map(|(_, i)| *i).collect();
        assert_eq!((0..50).collect::<Vec<_>>(), order);
    }
    assert_eq!(map.sent_sequence(), map.applied_sequence());
}

#[test]
fn completions_arrive_in_send_order_across_maps_and_the_runtime() {
    let runtime = runtime();
    let (first, second) = (map(&runtime), map(&runtime));
    let order = Arc::new(Mutex::new(Vec::new()));
    let record = |name: &'static str| {
        let order = order.clone();
        move |ok: bool| {
            assert!(ok);
            order.lock().unwrap().push(name);
        }
    };
    let (repainted, cleared) = (record("first"), record("runtime"));
    let (resized, resized_again) = (record("second"), record("first again"));
    first.request_repaint().unwrap().on_complete(move |result| repainted(result.is_ok()));
    runtime.clear_event_handler().unwrap().on_complete(move |result| cleared(result.is_ok()));
    second.resize(Size::new(100, 100)).unwrap().on_complete(move |result| resized(result.is_ok()));
    first.resize(Size::new(100, 100)).unwrap().on_complete(move |result| resized_again(result.is_ok()));
    wait(runtime.clear_resource_transform());
    assert_eq!(["first", "runtime", "second", "first again"], order.lock().unwrap().as_slice());
}

#[test]
fn a_completion_is_a_future() {
    let runtime = runtime();
    let map = map(&runtime);
    let applied = pollster::block_on(map.resize(Size::new(300, 200)).unwrap()).unwrap();
    assert_eq!(applied.sequence, map.applied_sequence());
    let snapshot = map.snapshot().unwrap();
    assert!(snapshot.generation >= applied.generation);
    assert_eq!(Size::new(300, 200), snapshot.view.extent.size);
}

#[test]
fn waiting_on_the_runtimes_thread_fails_instead_of_hanging() {
    let runtime = runtime();
    let map = map(&runtime);
    // The gate holds the runtime's thread until the callback is registered,
    // so the callback runs there.
    let (open, gate) = channel::<()>();
    drop(map.with_core(move |_| gate.recv_timeout(TIMEOUT)).unwrap());
    let (tx, rx) = channel();
    let inner = map.clone();
    map.request_repaint().unwrap().on_complete(move |_| {
        let _ = tx.send(inner.request_repaint().unwrap().wait());
    });
    open.send(()).unwrap();
    assert_eq!(ErrorKind::InvalidState, rx.recv_timeout(TIMEOUT).unwrap().unwrap_err().kind);
}

#[test]
fn host_code_can_call_back_into_the_library() {
    let runtime = runtime();
    let map = map(&runtime);
    let (tx, rx) = channel();
    let (inner_runtime, inner_map) = (runtime.clone(), map.clone());
    wait(runtime.set_event_handler(move |_| {
        // No library lock is held while the handler runs.
        let _ = inner_map.snapshot();
        let _ = inner_map.request_repaint();
        let _ = inner_runtime.clear_resource_transform();
        let _ = tx.send(());
    }));
    let inner = map.clone();
    let (done, finished) = channel();
    map.set_style_json(EMPTY_STYLE).unwrap().on_complete(move |_| {
        let _ = inner.event_mask();
        let _ = inner.snapshot();
        let _ = done.send(inner.request_repaint().is_ok());
    });
    rx.recv_timeout(TIMEOUT).unwrap();
    assert!(finished.recv_timeout(TIMEOUT).unwrap());
    wait(runtime.clear_event_handler());
    wait(map.release());
}

#[test]
fn commands_sent_before_the_map_is_built_wait_for_it() {
    let runtime = runtime();
    let (map, created) = runtime.create_map(map_options()).unwrap();
    let resized = map.resize(Size::new(320, 240)).unwrap();
    assert_eq!(1, created.wait().unwrap().sequence);
    assert_eq!(2, resized.wait().unwrap().sequence);
    assert_eq!(Size::new(320, 240), map.snapshot().unwrap().view.extent.size);
}

#[test]
fn a_map_that_fails_to_build_fails_its_commands() {
    let runtime = runtime();
    let (map, created) = runtime.create_map(map_options().with_fallback_font_faces("not JSON")).unwrap();
    let repaint = map.request_repaint().unwrap();
    assert_eq!(ErrorKind::InvalidArgument, created.wait().unwrap_err().kind);
    assert_eq!(ErrorKind::InvalidState, repaint.wait().unwrap_err().kind);
    assert_eq!(ErrorKind::InvalidState, map.snapshot().unwrap_err().kind);
    wait(map.release());
}

#[test]
fn creation_rejects_options_that_no_map_can_have() {
    let runtime = runtime();
    let empty = MapOptions::new(mln_map::MapOptions::new().with_size(Size::new(0, 10)));
    assert_eq!(ErrorKind::InvalidArgument, runtime.create_map(empty).unwrap_err().kind);
    let scale = MapOptions::new(mln_map::MapOptions::new().with_pixel_ratio(f32::INFINITY));
    assert_eq!(ErrorKind::InvalidArgument, runtime.create_map(scale).unwrap_err().kind);
}

#[test]
fn a_released_map_rejects_its_commands() {
    let runtime = runtime();
    let map = map(&runtime);
    let clone = map.clone();
    wait(map.release());
    assert_eq!(ErrorKind::InvalidState, clone.request_repaint().unwrap_err().kind);
    assert_eq!(ErrorKind::InvalidState, clone.snapshot().unwrap_err().kind);
    assert_eq!(ErrorKind::InvalidState, clone.release().unwrap_err().kind);
}

#[test]
fn a_runtime_releases_after_its_maps() {
    let runtime = runtime();
    let map = map(&runtime);
    assert_eq!(ErrorKind::InvalidState, runtime.release().unwrap_err().kind);
    wait(runtime.clear_resource_transform());

    wait(map.release());
    let clone = runtime.clone();
    wait(runtime.release());
    assert_eq!(ErrorKind::InvalidState, clone.create_map(map_options()).unwrap_err().kind);
    assert_eq!(ErrorKind::InvalidState, clone.clear_event_handler().unwrap_err().kind);
    assert_eq!(ErrorKind::InvalidState, clone.release().unwrap_err().kind);
    assert_eq!(ErrorKind::InvalidState, clone.dispose().unwrap_err().kind);
}

#[test]
fn a_runtime_and_map_outlive_the_thread_that_made_them() {
    let (runtime, map) = std::thread::spawn(|| {
        let runtime = runtime();
        let map = map(&runtime);
        (runtime, map)
    })
    .join()
    .unwrap();
    wait(map.resize(Size::new(320, 240)));
    assert_eq!(Size::new(320, 240), map.snapshot().unwrap().view.extent.size);
    wait(map.release());
    wait(runtime.release());
}

/// A provider that reports when it drops.
struct DropFlag(Arc<AtomicBool>);

impl mln_runtime::ResourceProvider for DropFlag {
    fn request(&self, request: ResourceRequest) {
        let _ = request.pass_through();
    }
}

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[test]
fn a_disposed_runtime_shuts_down_after_its_last_map() {
    let runtime = runtime();
    let dropped = Arc::new(AtomicBool::new(false));
    wait(runtime.set_resource_provider(DropFlag(dropped.clone())));
    let map = map(&runtime);
    runtime.dispose().unwrap();
    assert_eq!(ErrorKind::InvalidState, runtime.clear_resource_provider().unwrap_err().kind);

    wait(map.request_repaint());
    assert!(!dropped.load(Ordering::SeqCst));
    drop(map);
    assert!(eventually(|| dropped.load(Ordering::SeqCst)));
}

#[test]
fn a_runtime_whose_handles_all_drop_shuts_down() {
    let runtime = runtime();
    let dropped = Arc::new(AtomicBool::new(false));
    wait(runtime.set_resource_provider(DropFlag(dropped.clone())));
    let map = map(&runtime);
    drop(runtime);
    wait(map.request_repaint());
    assert!(!dropped.load(Ordering::SeqCst));
    drop(map);
    assert!(eventually(|| dropped.load(Ordering::SeqCst)));
}

#[test]
fn a_cache_file_that_cant_be_created_fails_the_runtime() {
    let dir = TempDir::new();
    let unwritable = dir.path("missing-directory").join("cache.db");
    let options = RuntimeOptions { cache_path: Some(unwritable.clone()), ..RuntimeOptions::default() };
    let error = Runtime::new(options).unwrap_err();
    assert_eq!(ErrorKind::InvalidArgument, error.kind);
    assert!(error.message.contains(unwritable.to_str().unwrap()), "{}", error.message);

    let writable = dir.path("cache.db");
    let runtime = Runtime::new(RuntimeOptions { cache_path: Some(writable.clone()), ..RuntimeOptions::default() });
    assert!(writable.exists());
    wait(runtime.unwrap().release());
}

#[test]
fn a_command_group_holds_updates_back_until_it_ends() {
    let runtime = runtime();
    let map = map(&runtime);
    let before = wait(map.request_repaint()).render_update_generation;

    let group = map.command_group().unwrap();
    let nested = map.command_group().unwrap();
    let held = wait(map.update_camera(CameraUpdate {
        camera: CameraOptions::default().with_zoom(Some(3.0)),
        ..CameraUpdate::default()
    }));
    // The held change names the update that the outermost end shows.
    assert_eq!(before + 1, held.render_update_generation);
    assert_eq!(before, map.snapshot().unwrap().update_generation);
    assert_eq!(3.0, map.snapshot().unwrap().view.camera.zoom);
    assert_eq!(before + 1, wait(nested.end()).render_update_generation);

    drop(group);
    assert!(wait(map.request_repaint()).render_update_generation > before);
    assert_eq!(ErrorKind::InvalidState, wait_err(map.end_command_group()).kind);
}

#[test]
fn the_camera_query_sees_the_commands_before_it() {
    let runtime = runtime();
    let map = map(&runtime);
    let moved = map.update_camera(CameraUpdate {
        camera: CameraOptions::default().with_zoom(Some(5.0)).with_bearing(Some(30.0)),
        ..CameraUpdate::default()
    });
    let camera = wait(map.camera());
    assert_eq!(5.0, camera.camera.zoom);
    assert!((camera.camera.bearing - 30.0).abs() < 1e-9, "{}", camera.camera.bearing);
    assert!(camera.generation >= wait(moved).generation);
}

#[test]
fn with_core_runs_on_the_map_in_its_place() {
    let runtime = runtime();
    let map = map(&runtime);
    drop(map.resize(Size::new(128, 64)).unwrap());
    let size = wait(map.with_core(|core| core.get_transform_state().get_size()));
    assert_eq!(Size::new(128, 64), size);
}

#[test]
fn a_resize_to_the_same_size_renders_nothing() {
    let runtime = runtime();
    let map = map(&runtime);
    assert_eq!(ErrorKind::InvalidArgument, map.resize(Size::new(0, 64)).unwrap_err().kind);
    let resized = wait(map.resize(Size::new(128, 64)));
    let again = wait(map.resize(Size::new(128, 64)));
    assert_eq!(resized.render_update_generation, again.render_update_generation);
    assert_eq!(Size::new(128, 64), map.snapshot().unwrap().view.extent.size);
}

#[test]
fn only_a_continuous_map_repaints() {
    let runtime = runtime();
    let options = map_options();
    let still = MapOptions { map: options.map.clone().with_map_mode(mln_map::MapMode::Static), ..options };
    let (map, created) = runtime.create_map(still).unwrap();
    created.wait().unwrap();
    assert_eq!(ErrorKind::InvalidState, map.request_repaint().unwrap_err().kind);
}

#[test]
fn a_disposed_map_lets_its_runtime_release() {
    let runtime = runtime();
    let map = map(&runtime);
    let clone = map.clone();
    map.dispose().unwrap();
    assert_eq!(ErrorKind::InvalidState, clone.dispose().unwrap_err().kind);
    assert!(eventually(|| runtime.release().is_ok()));
}
