// Offscreen render sessions: frame demands and their results, the session's
// work, still images, queries and the session's lifecycle. They follow
// maplibre-native-ffi's ABI tests (tests/native/abi/render/{demands,driver,
// offscreen,queries}.c and map/still_image.c at b20632a) where those test
// the same rules.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::channel;

use mln_map::{CameraOptions, CameraUpdate, MapMode};
use mln_runtime::{
    AttachOptions, DemandMode, Driver, ErrorKind, FrameDemand, FrameDisposition, FrameResult, Map, MapOptions,
    OffscreenDevice, OffscreenDeviceOptions, RenderSession, RenderSessionState, RenderTarget, RenderedQueryGeometry,
    RenderedQueryOptions, Runtime, STILL_IMAGE_TOKEN, SourceQueryOptions,
};
use mln_util::geo::ScreenCoordinate;
use mln_util::size::Size;
use support::{TIMEOUT, eventually, runtime, wait, wait_err};

const SIZE: Size = Size { width: 64, height: 64 };

fn style(color: &str) -> String {
    format!(
        r#"{{"version":8,"transition":{{"duration":0,"delay":0}},"sources":{{}},"layers":[{{"id":"bg","type":"background","paint":{{"background-color":"{color}"}}}}]}}"#
    )
}

fn map_in_mode(runtime: &Runtime, mode: MapMode) -> Map {
    let options = MapOptions::new(mln_map::MapOptions::new().with_size(Size::new(256, 256)).with_map_mode(mode));
    let (map, created) = runtime.create_map(options).unwrap();
    created.wait().unwrap();
    map
}

/// A continuous map with a red style.
fn red_map(runtime: &Runtime) -> Map {
    let map = map_in_mode(runtime, MapMode::Continuous);
    wait(map.set_style_json(style("#ff0000")));
    map
}

fn offscreen(size: Size) -> RenderTarget {
    RenderTarget::Offscreen { size, device: None }
}

/// An offscreen session of `SIZE` that has attached.
fn attach(map: &Map) -> RenderSession {
    let (session, attached) = map.attach(offscreen(SIZE), AttachOptions::default()).unwrap();
    attached.wait().unwrap();
    session
}

fn demand(session: &RenderSession, mode: DemandMode, token: u64) {
    session.demand_frame(FrameDemand::new(mode, token)).unwrap();
}

/// Waits until the session has `count` results, and takes them.
fn take_results(session: &RenderSession, count: usize) -> Vec<FrameResult> {
    let mut results = Vec::new();
    let done = eventually(|| {
        results.extend(session.drain_frame_results().unwrap());
        results.len() >= count
    });
    assert!(done, "waited for {count} frame results, got {results:?}");
    results
}

fn take_one(session: &RenderSession) -> FrameResult {
    let results = take_results(session, 1);
    assert_eq!(1, results.len(), "{results:?}");
    results[0]
}

/// Renders until a frame shows the map settled: fully loaded, wanting no
/// further frame, and drawing the newest update. A later demand that doesn't
/// force a frame then finds nothing new.
fn render_until_idle(map: &Map, session: &RenderSession) {
    wait(map.camera());
    let mut mode = DemandMode::IfChanged;
    let deadline = std::time::Instant::now() + TIMEOUT;
    while std::time::Instant::now() < deadline {
        demand(session, mode, 0);
        let frame = take_one(session);
        match frame.disposition {
            FrameDisposition::Rendered => {
                let snapshot = map.snapshot().unwrap();
                if frame.mode == mln_map::RenderMode::Full
                    && !frame.needs_repaint
                    && !snapshot.transition_in_progress
                    && frame.map_update_generation >= snapshot.update_generation
                {
                    return;
                }
                mode = DemandMode::WaitForUpdate;
            }
            FrameDisposition::NoUpdate => mode = DemandMode::Force,
            disposition => panic!("a frame finished as {disposition:?}"),
        }
    }
    panic!("the map never settled");
}

/// Holds the runtime's thread until the returned sender sends or drops.
fn hold_runtime(map: &Map) -> std::sync::mpsc::Sender<()> {
    let (open, gate) = channel::<()>();
    let (entered, parked) = channel::<()>();
    drop(
        map.with_core(move |_| {
            let _ = entered.send(());
            let _ = gate.recv_timeout(TIMEOUT);
        })
        .unwrap(),
    );
    parked.recv_timeout(TIMEOUT).unwrap();
    open
}

fn zoom(map: &Map, zoom: f64) -> mln_runtime::Completion<mln_runtime::Applied> {
    map.update_camera(CameraUpdate { camera: CameraOptions::default().with_zoom(Some(zoom)), ..Default::default() })
        .unwrap()
}

/// The premultiplied RGBA of the center pixel of the session's last frame.
fn center_pixel(session: &RenderSession, size: Size) -> [u8; 4] {
    let image = wait(session.read_texture());
    assert_eq!(size, image.size);
    let offset = ((size.height / 2 * size.width + size.width / 2) * 4) as usize;
    image.data[offset..offset + 4].try_into().unwrap()
}

// MARK: - Frames

#[test]
fn a_forced_demand_renders_the_map() {
    let runtime = runtime();
    let map = red_map(&runtime);
    let session = attach(&map);
    let capabilities = session.capabilities().unwrap();
    assert!(capabilities.readback);
    assert_eq!(Driver::CoreWorker, capabilities.driver);
    assert_eq!(RenderSessionState::Attached, session.snapshot().unwrap().state);

    demand(&session, DemandMode::Force, 7);
    let result = take_one(&session);
    assert_eq!(7, result.token);
    assert_eq!(FrameDisposition::Rendered, result.disposition);
    assert_eq!(1, result.frame_generation);
    assert_eq!(SIZE, result.view.unwrap().extent.size);
    assert_eq!([255, 0, 0, 255], center_pixel(&session, SIZE));
    assert_eq!(1, session.snapshot().unwrap().frame_generation);
}

#[test]
fn frame_results_wake_the_host_once_until_it_drains() {
    let runtime = runtime();
    let map = red_map(&runtime);
    let wakes = Arc::new(AtomicUsize::new(0));
    let counter = wakes.clone();
    let options = AttachOptions {
        frame_wake: Some(Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        })),
        ..AttachOptions::default()
    };
    let (session, attached) = map.attach(offscreen(SIZE), options).unwrap();
    attached.wait().unwrap();
    demand(&session, DemandMode::Force, 1);
    demand(&session, DemandMode::Force, 2);
    assert!(eventually(|| session.snapshot().unwrap().frame_generation == 2));
    assert_eq!(1, wakes.load(Ordering::SeqCst));
    let tokens: Vec<u64> = session.drain_frame_results().unwrap().iter().map(|result| result.token).collect();
    assert_eq!([1, 2], tokens.as_slice());
    assert!(session.drain_frame_results().unwrap().is_empty());
    demand(&session, DemandMode::Force, 3);
    assert_eq!(3, take_one(&session).token);
    assert!(eventually(|| wakes.load(Ordering::SeqCst) == 2));
}

#[test]
fn a_demand_rejects_the_still_image_token() {
    let runtime = runtime();
    let map = red_map(&runtime);
    let session = attach(&map);
    let error = session.demand_frame(FrameDemand::new(DemandMode::IfChanged, STILL_IMAGE_TOKEN)).unwrap_err();
    assert_eq!(ErrorKind::InvalidArgument, error.kind);
}

#[test]
fn a_demand_without_changes_renders_nothing() {
    let runtime = runtime();
    let map = red_map(&runtime);
    let session = attach(&map);
    render_until_idle(&map, &session);
    demand(&session, DemandMode::IfChanged, 2);
    let result = take_one(&session);
    assert_eq!(FrameDisposition::NoUpdate, result.disposition);
    assert_eq!(0, result.frame_generation);
    assert_eq!(None, result.view);
}

// The runtime's thread is held while the first command is sent, so the
// session has an older update to draw when the demand reaches it, and the
// frame must show the command.
#[test]
fn a_demand_shows_the_commands_sent_before_it() {
    let runtime = runtime();
    let map = red_map(&runtime);
    let session = attach(&map);
    render_until_idle(&map, &session);

    let open = hold_runtime(&map);
    let moved = zoom(&map, 3.0);
    demand(&session, DemandMode::IfChanged, 631);
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(session.drain_frame_results().unwrap().is_empty());
    drop(open);

    let applied = moved.wait().unwrap();
    let result = take_one(&session);
    assert_eq!(631, result.token);
    assert_eq!(FrameDisposition::Rendered, result.disposition);
    assert!(result.map_update_generation >= applied.render_update_generation);
    assert_eq!(3.0, result.view.unwrap().camera.zoom);
}

#[test]
fn a_demand_behind_a_command_without_an_update_finishes() {
    let runtime = runtime();
    let map = red_map(&runtime);
    let session = attach(&map);
    render_until_idle(&map, &session);

    let open = hold_runtime(&map);
    let logged = map.dump_debug_logs().unwrap();
    demand(&session, DemandMode::IfChanged, 641);
    drop(open);
    logged.wait().unwrap();
    let result = take_one(&session);
    assert_eq!(641, result.token);
    assert_eq!(FrameDisposition::NoUpdate, result.disposition);
}

#[test]
fn a_waiting_demand_renders_the_next_map_update() {
    let runtime = runtime();
    let map = red_map(&runtime);
    let session = attach(&map);
    render_until_idle(&map, &session);
    let idle = map.snapshot().unwrap().update_generation;

    demand(&session, DemandMode::WaitForUpdate, 601);
    // Work that the session runs after the demand, and that publishes no
    // update.
    wait(session.dump_debug_logs());
    assert!(session.drain_frame_results().unwrap().is_empty());
    zoom(&map, 2.0).wait().unwrap();
    let result = take_one(&session);
    assert_eq!(601, result.token);
    assert_eq!(FrameDisposition::Rendered, result.disposition);
    assert!(result.map_update_generation > idle);
}

#[test]
fn a_waiting_demand_ends_by_supersession_barrier_or_detach() {
    let runtime = runtime();
    let map = red_map(&runtime);
    let session = attach(&map);
    render_until_idle(&map, &session);

    demand(&session, DemandMode::WaitForUpdate, 611);
    demand(&session, DemandMode::WaitForUpdate, 612);
    wait(session.barrier());
    let results = take_results(&session, 2);
    assert_eq!((611, FrameDisposition::Superseded), (results[0].token, results[0].disposition));
    assert_eq!((612, FrameDisposition::NoUpdate), (results[1].token, results[1].disposition));

    demand(&session, DemandMode::WaitForUpdate, 613);
    wait(session.detach());
    let result = take_one(&session);
    assert_eq!((613, FrameDisposition::TargetNotReady), (result.token, result.disposition));
}

#[test]
fn a_detach_ends_a_demand_waiting_for_commands() {
    let runtime = runtime();
    let map = red_map(&runtime);
    let session = attach(&map);
    render_until_idle(&map, &session);

    let open = hold_runtime(&map);
    let moved = zoom(&map, 3.0);
    demand(&session, DemandMode::Force, 661);
    let detached = session.detach().unwrap();
    drop(open);
    detached.wait().unwrap();
    let result = take_one(&session);
    assert_eq!((661, FrameDisposition::TargetNotReady), (result.token, result.disposition));
    moved.wait().unwrap();
}

#[test]
fn a_frame_leaves_out_what_an_open_command_group_holds() {
    let runtime = runtime();
    let map = red_map(&runtime);
    let session = attach(&map);
    render_until_idle(&map, &session);

    let group = map.command_group().unwrap();
    wait(map.set_style_layer_property("bg", "background-color", "\"#0000ff\""));
    demand(&session, DemandMode::Force, 1);
    assert_eq!(FrameDisposition::Rendered, take_one(&session).disposition);
    assert_eq!([255, 0, 0, 255], center_pixel(&session, SIZE));

    wait(group.end());
    demand(&session, DemandMode::IfChanged, 2);
    assert_eq!(FrameDisposition::Rendered, take_one(&session).disposition);
    assert_eq!([0, 0, 255, 255], center_pixel(&session, SIZE));
}

#[test]
fn the_redraw_wake_runs_once_per_demand() {
    let runtime = runtime();
    let map = red_map(&runtime);
    let redraws = Arc::new(AtomicUsize::new(0));
    let counter = redraws.clone();
    let options = AttachOptions {
        redraw_wake: Some(Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        })),
        ..AttachOptions::default()
    };
    let (session, attached) = map.attach(offscreen(SIZE), options).unwrap();
    attached.wait().unwrap();
    render_until_idle(&map, &session);
    let before = redraws.load(Ordering::SeqCst);

    zoom(&map, 2.0).wait().unwrap();
    assert!(eventually(|| redraws.load(Ordering::SeqCst) == before + 1));
    zoom(&map, 3.0).wait().unwrap();
    wait(session.dump_debug_logs());
    assert_eq!(before + 1, redraws.load(Ordering::SeqCst));
    demand(&session, DemandMode::IfChanged, 1);
    assert_eq!(FrameDisposition::Rendered, take_one(&session).disposition);
    zoom(&map, 4.0).wait().unwrap();
    assert!(eventually(|| redraws.load(Ordering::SeqCst) == before + 2));
}

// MARK: - Resizing

#[test]
fn resizes_apply_in_order_and_publish_generations() {
    let runtime = runtime();
    let map = red_map(&runtime);
    let session = attach(&map);
    demand(&session, DemandMode::Force, 301);
    let old = take_one(&session);

    assert_eq!(ErrorKind::InvalidArgument, session.resize(Size::new(0, 48)).unwrap_err().kind);
    demand(&session, DemandMode::Force, 302);
    let size = Size::new(96, 48);
    let applied = wait(session.resize(size));
    assert_eq!(old.generation, take_one(&session).generation);
    assert_eq!(ErrorKind::InvalidState, wait_err(session.read_texture()).kind);

    demand(&session, DemandMode::Force, 303);
    let new = take_one(&session);
    assert!(new.generation > old.generation);
    assert!(new.frame_generation > old.frame_generation);
    assert!(new.map_update_generation >= applied.render_update_generation);
    assert_eq!(size, new.view.unwrap().extent.size);
    assert_eq!([255, 0, 0, 255], center_pixel(&session, size));
    let snapshot = session.snapshot().unwrap();
    assert_eq!(size, snapshot.extent.size);
    assert_eq!(1.0, snapshot.extent.scale_factor);
    assert_eq!(new.generation, snapshot.generation);
    assert_eq!(size, map.snapshot().unwrap().view.extent.size);
}

#[test]
fn a_session_renders_at_its_maps_pixel_ratio() {
    let runtime = runtime();
    let options = MapOptions::new(mln_map::MapOptions::new().with_size(SIZE).with_pixel_ratio(2.0));
    let (map, created) = runtime.create_map(options).unwrap();
    created.wait().unwrap();
    wait(map.set_style_json(style("#ff0000")));
    let session = attach(&map);
    assert_eq!(2.0, session.snapshot().unwrap().extent.scale_factor);
    render_until_idle(&map, &session);
    assert_eq!([255, 0, 0, 255], center_pixel(&session, Size::new(128, 128)));
}

#[test]
fn a_resize_applies_while_a_command_group_holds_the_update() {
    let runtime = runtime();
    let map = red_map(&runtime);
    let session = attach(&map);
    wait(map.begin_command_group());
    wait(session.resize(Size::new(96, 48)));
    assert_eq!(Size::new(96, 48), map.snapshot().unwrap().view.extent.size);
    assert_eq!(Size::new(96, 48), session.snapshot().unwrap().extent.size);
    wait(session.detach());
    wait(map.end_command_group());
}

#[test]
fn work_sent_after_a_resize_waits_for_it() {
    let runtime = runtime();
    let map = red_map(&runtime);
    let session = attach(&map);
    let before = session.snapshot().unwrap().generation;
    // The resize runs on the map's thread first, and the barrier waits for
    // it to reach the session.
    let open = hold_runtime(&map);
    drop(session.resize(Size::new(96, 80)).unwrap());
    let barrier = session.barrier().unwrap();
    drop(open);
    barrier.wait().unwrap();
    let snapshot = session.snapshot().unwrap();
    assert_eq!(Size::new(96, 80), snapshot.extent.size);
    assert_eq!(before + 1, snapshot.generation);
}

// MARK: - Lifecycle

#[test]
fn attaching_sizes_the_map_and_holds_it() {
    let runtime = runtime();
    let map = red_map(&runtime);
    let session = attach(&map);
    assert_eq!(SIZE, map.snapshot().unwrap().view.extent.size);

    let second = map.attach(offscreen(SIZE), AttachOptions::default()).unwrap_err();
    assert_eq!(ErrorKind::InvalidState, second.kind);
    assert_eq!(ErrorKind::InvalidState, map.release().unwrap_err().kind);
    assert_eq!(ErrorKind::InvalidState, map.resize(Size::new(32, 48)).unwrap_err().kind);
    assert_eq!(SIZE, map.snapshot().unwrap().view.extent.size);

    wait(session.release());
    let other = attach(&map);
    wait(other.release());
    wait(map.release());
}

#[test]
fn attach_rejects_what_an_offscreen_session_cannot_take() {
    let runtime = runtime();
    let map = red_map(&runtime);
    let empty = map.attach(offscreen(Size::new(0, 64)), AttachOptions::default()).unwrap_err();
    assert_eq!(ErrorKind::InvalidArgument, empty.kind);
    let options = AttachOptions { driver: Driver::CallerThread, ..AttachOptions::default() };
    assert_eq!(ErrorKind::Unsupported, map.attach(offscreen(SIZE), options).unwrap_err().kind);
    // Neither left the map with a session.
    drop(attach(&map));
}

#[test]
fn a_demand_made_while_attaching_renders_after_the_attach() {
    let runtime = runtime();
    let map = red_map(&runtime);
    let (session, attached) = map.attach(offscreen(SIZE), AttachOptions::default()).unwrap();
    demand(&session, DemandMode::Force, 5);
    attached.wait().unwrap();
    let result = take_one(&session);
    assert_eq!((5, FrameDisposition::Rendered), (result.token, result.disposition));
}

#[test]
fn a_detached_session_frees_its_map_and_refuses_work() {
    let runtime = runtime();
    let map = red_map(&runtime);
    let session = attach(&map);
    wait(session.detach());
    assert_eq!(RenderSessionState::Detached, session.snapshot().unwrap().state);
    let other = attach(&map);
    drop(other);

    let refused = |error: mln_runtime::Error| assert_eq!(ErrorKind::InvalidState, error.kind);
    refused(session.demand_frame(FrameDemand::default()).unwrap_err());
    refused(session.barrier().unwrap_err());
    refused(session.resize(SIZE).unwrap_err());
    refused(session.reduce_memory_use().unwrap_err());
    refused(session.clear_data().unwrap_err());
    refused(session.dump_debug_logs().unwrap_err());
    refused(session.read_texture().unwrap_err());
    refused(session.detach().unwrap_err());
    refused(session.abandon().unwrap_err());
    assert_eq!(RenderSessionState::Detached, session.snapshot().unwrap().state);

    wait(session.release());
    refused(session.snapshot().unwrap_err());
    refused(session.dispose().unwrap_err());
}

#[test]
fn abandon_ends_pending_work_and_frees_the_map_at_once() {
    let runtime = runtime();
    let map = red_map(&runtime);
    let session = attach(&map);
    render_until_idle(&map, &session);
    demand(&session, DemandMode::WaitForUpdate, 9);
    wait(session.dump_debug_logs());

    session.abandon().unwrap();
    assert_eq!(RenderSessionState::Abandoned, session.snapshot().unwrap().state);
    let other = attach(&map);
    let result = take_one(&session);
    assert_eq!((9, FrameDisposition::TargetNotReady), (result.token, result.disposition));
    assert_eq!(ErrorKind::InvalidState, session.demand_frame(FrameDemand::default()).unwrap_err().kind);
    assert_eq!(ErrorKind::InvalidState, session.abandon().unwrap_err().kind);
    wait(session.release());

    render_until_idle(&map, &other);
}

#[test]
fn work_queued_before_an_abandon_completes_as_target_lost() {
    let runtime = runtime();
    let map = red_map(&runtime);
    let session = attach(&map);
    let open = hold_runtime(&map);
    // The resize waits for the runtime's thread, and reaches the session
    // after the abandon.
    let resized = session.resize(Size::new(32, 32)).unwrap();
    session.abandon().unwrap();
    drop(open);
    assert_eq!(ErrorKind::TargetLost, resized.wait().unwrap_err().kind);
}

#[test]
fn dropping_the_last_handle_detaches_the_session() {
    let runtime = runtime();
    let map = red_map(&runtime);
    drop(attach(&map));
    assert!(eventually(|| map.attach(offscreen(SIZE), AttachOptions::default()).is_ok()));
}

#[test]
fn maintenance_work_runs_in_order_with_frames() {
    let runtime = runtime();
    let map = red_map(&runtime);
    let session = attach(&map);
    demand(&session, DemandMode::Force, 1);
    wait(session.reduce_memory_use());
    assert_eq!(1, session.drain_frame_results().unwrap().len());
    wait(session.clear_data());
    demand(&session, DemandMode::IfChanged, 2);
    wait(session.dump_debug_logs());
    let result = session.drain_frame_results().unwrap();
    assert_eq!(1, result.len());
    assert_eq!((2, FrameDisposition::Rendered), (result[0].token, result[0].disposition));
    assert_eq!(ErrorKind::Unsupported, session.release_target().unwrap_err().kind);
}

#[test]
fn sessions_on_two_maps_share_one_device() {
    let device = OffscreenDevice::new(OffscreenDeviceOptions::default()).unwrap();
    let runtime = runtime();
    let (red, green) = (red_map(&runtime), map_in_mode(&runtime, MapMode::Continuous));
    wait(green.set_style_json(style("#00ff00")));
    let sessions: Vec<RenderSession> = [&red, &green]
        .into_iter()
        .map(|map| {
            let target = RenderTarget::Offscreen { size: SIZE, device: Some(device.clone()) };
            let (session, attached) = map.attach(target, AttachOptions::default()).unwrap();
            attached.wait().unwrap();
            session
        })
        .collect();
    // The device lasts while its sessions use it.
    drop(device);
    for (map, session) in [&red, &green].into_iter().zip(&sessions) {
        render_until_idle(map, session);
    }
    assert_eq!([255, 0, 0, 255], center_pixel(&sessions[0], SIZE));
    assert_eq!([0, 255, 0, 255], center_pixel(&sessions[1], SIZE));
}

#[test]
fn a_device_reports_a_missing_adapter() {
    let error = OffscreenDevice::new(OffscreenDeviceOptions { adapter_index: Some(usize::MAX) }).unwrap_err();
    assert_eq!(ErrorKind::NotFound, error.kind);
}

// MARK: - Still images

#[test]
fn a_static_map_renders_still_images() {
    let runtime = runtime();
    let map = map_in_mode(&runtime, MapMode::Static);
    wait(map.set_style_json(style("#ff0000")));
    let session = attach(&map);

    wait(map.request_still_image());
    let results = session.drain_frame_results().unwrap();
    let last = results.last().unwrap();
    assert_eq!((STILL_IMAGE_TOKEN, FrameDisposition::Rendered), (last.token, last.disposition));
    assert_eq!([255, 0, 0, 255], center_pixel(&session, SIZE));

    wait(map.set_style_json(style("#00ff00")));
    wait(map.request_still_image());
    assert_eq!([0, 255, 0, 255], center_pixel(&session, SIZE));
}

#[test]
fn a_tile_map_session_resizes_between_still_images() {
    let runtime = runtime();
    let map = map_in_mode(&runtime, MapMode::Tile);
    wait(map.set_style_json(style("#ff0000")));
    let session = attach(&map);
    wait(map.request_still_image());
    let size = Size::new(96, 48);
    wait(session.resize(size));
    wait(map.request_still_image());
    assert_eq!([255, 0, 0, 255], center_pixel(&session, size));
}

#[test]
fn a_still_image_waits_for_a_session() {
    let runtime = runtime();
    let map = map_in_mode(&runtime, MapMode::Static);
    wait(map.set_style_json(style("#ff0000")));
    let still = map.request_still_image().unwrap();
    // A read behind the request.
    wait(map.camera());
    assert!(!still.is_complete());
    let session = attach(&map);
    still.wait().unwrap();
    assert_eq!([255, 0, 0, 255], center_pixel(&session, SIZE));
}

#[test]
fn a_continuous_map_refuses_a_still_image() {
    let runtime = runtime();
    let map = red_map(&runtime);
    assert_eq!(ErrorKind::InvalidState, map.request_still_image().unwrap_err().kind);
}

#[test]
fn a_still_image_inside_a_command_group_fails() {
    let runtime = runtime();
    let map = map_in_mode(&runtime, MapMode::Static);
    wait(map.begin_command_group());
    assert_eq!(ErrorKind::InvalidState, wait_err(map.request_still_image()).kind);
    wait(map.end_command_group());
}

#[test]
fn map_release_cancels_the_pending_still_image() {
    let runtime = runtime();
    let map = map_in_mode(&runtime, MapMode::Static);
    let pending = map.request_still_image().unwrap();
    assert_eq!(ErrorKind::InvalidState, wait_err(map.request_still_image()).kind);
    wait(map.release());
    assert_eq!(ErrorKind::Cancelled, pending.wait().unwrap_err().kind);
}

// MARK: - Queries

/// Two points on the center row, "west" at x 18 and "east" at x 46, which
/// the "circles" layer draws, and a clustered source of three points close
/// together, which no layer draws.
const QUERY_STYLE: &str = r#"{
  "version": 8,
  "sources": {
    "points": {"type": "geojson", "data": {"type": "FeatureCollection", "features": [
      {"type": "Feature", "id": 1, "properties": {"name": "west"}, "geometry": {"type": "Point", "coordinates": [-10, 0]}},
      {"type": "Feature", "id": 2, "properties": {"name": "east"}, "geometry": {"type": "Point", "coordinates": [10, 0]}}
    ]}},
    "clustered": {"type": "geojson", "cluster": true, "clusterMaxZoom": 5, "data": {"type": "FeatureCollection", "features": [
      {"type": "Feature", "properties": {"n": 1}, "geometry": {"type": "Point", "coordinates": [0, 0]}},
      {"type": "Feature", "properties": {"n": 2}, "geometry": {"type": "Point", "coordinates": [0.01, 0]}},
      {"type": "Feature", "properties": {"n": 3}, "geometry": {"type": "Point", "coordinates": [0, 0.01]}}
    ]}}
  },
  "layers": [
    {"id": "circles", "type": "circle", "source": "points", "paint": {"circle-radius": 4}},
    {"id": "clusters", "type": "circle", "source": "clustered", "paint": {"circle-radius": 2, "circle-opacity": 0}}
  ]
}"#;

/// A session of a map at zoom 0 around (0, 0) that has drawn `QUERY_STYLE`
/// with every source loaded.
fn query_session(runtime: &Runtime) -> (Map, RenderSession) {
    let map = map_in_mode(runtime, MapMode::Continuous);
    wait(map.set_style_json(QUERY_STYLE));
    wait(map.update_camera(CameraUpdate {
        camera: CameraOptions::default().with_zoom(Some(0.0)).with_center(Some(mln_util::geo::LatLng::default())),
        ..Default::default()
    }));
    let session = attach(&map);
    let loaded = eventually(|| {
        demand(&session, DemandMode::Force, 0);
        let result = take_one(&session);
        result.mode == mln_map::RenderMode::Full
            && wait(session.query_source_features("points", SourceQueryOptions::default())).len() == 2
    });
    assert!(loaded, "the sources never loaded");
    (map, session)
}

#[test]
fn rendered_queries_select_by_geometry_and_layer() {
    let runtime = runtime();
    let (_map, session) = query_session(&runtime);
    let names = |features: Vec<mln_util::feature::Feature>| -> Vec<String> {
        let mut names: Vec<String> = features
            .iter()
            .filter_map(|feature| feature.properties.get("name").map(|name| format!("{name:?}")))
            .collect();
        names.sort();
        names
    };

    let east = RenderedQueryGeometry::Point(ScreenCoordinate::new(46.0, 32.0));
    let hits = wait(session.query_rendered_features(east.clone(), RenderedQueryOptions::default()));
    assert_eq!(1, hits.len(), "{hits:?}");
    assert_eq!("circles", hits[0].layer);
    assert_eq!("points", hits[0].source);

    let everything =
        RenderedQueryGeometry::Box { min: ScreenCoordinate::new(0.0, 0.0), max: ScreenCoordinate::new(64.0, 64.0) };
    let circles = RenderedQueryOptions::new(Some(vec!["circles".into()]), None);
    assert_eq!(2, names(wait(session.query_rendered_features(everything.clone(), circles))).len());
    let line =
        RenderedQueryGeometry::LineString(vec![ScreenCoordinate::new(10.0, 32.0), ScreenCoordinate::new(26.0, 32.0)]);
    assert_eq!(1, wait(session.query_rendered_features(line, RenderedQueryOptions::default())).len());
    let none = RenderedQueryOptions::new(Some(vec!["missing".into()]), None);
    assert!(wait(session.query_rendered_features(east, none)).is_empty());

    // A box past every edge covers the whole target, and one outside it
    // covers nothing.
    let beyond = RenderedQueryGeometry::Box {
        min: ScreenCoordinate::new(-4096.0, -4096.0),
        max: ScreenCoordinate::new(4096.0, 4096.0),
    };
    let circles = RenderedQueryOptions::new(Some(vec!["circles".into()]), None);
    assert_eq!(2, wait(session.query_rendered_features(beyond, circles)).len());
    let outside =
        RenderedQueryGeometry::Box { min: ScreenCoordinate::new(100.0, 0.0), max: ScreenCoordinate::new(200.0, 64.0) };
    assert!(wait(session.query_rendered_features(outside, RenderedQueryOptions::default())).is_empty());

    let invalid =
        |geometry| session.query_rendered_features(geometry, RenderedQueryOptions::default()).unwrap_err().kind;
    assert_eq!(ErrorKind::InvalidArgument, invalid(RenderedQueryGeometry::LineString(Vec::new())));
    assert_eq!(ErrorKind::InvalidArgument, invalid(RenderedQueryGeometry::Point(ScreenCoordinate::new(f64::NAN, 0.0))));
    let empty_source = session.query_source_features("", SourceQueryOptions::default()).unwrap_err();
    assert_eq!(ErrorKind::InvalidArgument, empty_source.kind);
}

#[test]
fn cluster_queries_read_a_clustered_source() {
    let runtime = runtime();
    let (_map, session) = query_session(&runtime);
    let clusters = wait(session.query_source_features("clustered", SourceQueryOptions::default()));
    let cluster_id = clusters
        .iter()
        .find_map(|feature| match feature.properties.get("cluster_id") {
            Some(feature::Value::Uint(id)) => Some(*id as u32),
            Some(feature::Value::Int(id)) => Some(*id as u32),
            Some(feature::Value::Double(id)) => Some(*id as u32),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no cluster in {clusters:?}"));

    assert!(!wait(session.cluster_children("clustered", cluster_id)).is_empty());
    assert_eq!(3, wait(session.cluster_leaves("clustered", cluster_id, 10, 0)).len());
    assert_eq!(1, wait(session.cluster_leaves("clustered", cluster_id, 1, 2)).len());
    assert!(wait(session.cluster_expansion_zoom("clustered", cluster_id)) > 0);

    let kind = |completion| wait_err(completion).kind;
    assert_eq!(ErrorKind::NotFound, kind(session.cluster_children("missing", cluster_id)));
    assert_eq!(ErrorKind::InvalidArgument, kind(session.cluster_children("points", cluster_id)));
    assert_eq!(ErrorKind::NotFound, kind(session.cluster_children("clustered", 0)));
}
