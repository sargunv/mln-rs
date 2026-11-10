//! The benchmark cases (README.md). The basemap's frame and load cases
//! follow maplibre-native-ffi's of the same names, over the same data and
//! cameras, so their numbers are comparable to the C++ core's.

use std::sync::Barrier;
use std::time::{Duration, Instant};

use mln_map::RenderMode;
use mln_util::geo::LatLng;
use mln_util::image::PremultipliedImage;
use mln_util::size::Size;

use crate::meter::{Meter, ms};
use crate::scene::{
    BASEMAP_SIZE, BASEMAP_ZOOM, Mode, REAL_WORLD, REAL_WORLD_SIZE, REFRESH, Scene, basemap_center, basemap_north_west,
    basemap_south_east, basemap_style, write_frame,
};
use crate::stats::{median, percentile};
use crate::{Context, Metrics};

pub struct Case {
    pub name: String,
    run: Box<dyn Fn(&Context) -> Metrics>,
}

impl Case {
    fn new(name: impl Into<String>, run: impl Fn(&Context) -> Metrics + 'static) -> Case {
        Case { name: name.into(), run: Box::new(run) }
    }

    pub fn run(&self, cx: &Context) -> Metrics {
        (self.run)(cx)
    }
}

/// A style at a camera: where a scene's still and load cases look.
#[derive(Clone)]
struct View {
    name: &'static str,
    size: Size,
    center: LatLng,
    zoom: f64,
    style: Style,
}

#[derive(Clone)]
enum Style {
    Url(String),
    Basemap,
}

impl View {
    fn scene(&self, cx: &Context, mode: Mode) -> Scene {
        let mut scene = Scene::new(self.size, mode);
        scene.jump(self.center, self.zoom);
        match &self.style {
            Style::Url(url) => scene.load_url(url),
            Style::Basemap => scene.load_json(&basemap_style(&cx.fixtures)),
        }
        scene
    }
}

fn views() -> Vec<View> {
    let mut views: Vec<View> = REAL_WORLD
        .iter()
        .map(|scene| View {
            name: scene.name,
            size: REAL_WORLD_SIZE,
            center: scene.center(),
            zoom: scene.zoom,
            style: Style::Url(scene.style_url()),
        })
        .collect();
    views.push(View {
        name: "basemap",
        size: BASEMAP_SIZE,
        center: basemap_center(),
        zoom: BASEMAP_ZOOM,
        style: Style::Basemap,
    });
    views
}

/// Every case, in the order they run.
pub fn all() -> Vec<Case> {
    let mut cases = Vec::new();
    for view in views() {
        let name = format!("Still/{}", view.name);
        cases.push(Case::new(name, move |cx| still(cx, &view)));
    }
    cases.push(Case::new("Frame/Throughput", frame_throughput));
    cases.push(Case::new("Frame/Paced", frame_paced));
    cases.push(Case::new("Frame/Idle", frame_idle));
    cases.push(Case::new("Frame/SettleWithoutSymbols", frame_settle_without_symbols));
    for view in views() {
        let name = format!("Load/Style/{}", view.name);
        cases.push(Case::new(name, move |cx| load_style(cx, &view)));
    }
    cases.push(Case::new("Load/Concurrent", load_concurrent));
    cases.push(Case::new("Load/JumpWarm", |cx| load_jump(cx, false)));
    cases.push(Case::new("Load/JumpCold", |cx| load_jump(cx, true)));
    cases
}

/// A still frame of a map whose tiles and pipelines are ready, read back,
/// the camera alternating between two zooms 0.001 apart so that each is a
/// new frame over the same tiles.
fn still(cx: &Context, view: &View) -> Metrics {
    let mut scene = view.scene(cx, Mode::Still);
    let nudge = |scene: &mut Scene, i: usize| scene.jump(view.center, view.zoom + 0.001 * (i % 2) as f64);
    for i in 0..10 {
        nudge(&mut scene, i);
        scene.render_still();
    }
    let mut meter = Meter::new();
    for i in 0..cx.iterations(100) {
        meter.time(|| {
            nudge(&mut scene, i);
            scene.render_still()
        });
    }
    if let Some(dir) = &cx.frames {
        nudge(&mut scene, 0);
        write_frame(dir, &format!("Still/{}", view.name), &scene.render_still());
    }
    meter.finish()
}

/// From loading the style on a new map to its first complete still: the
/// style, sprite and glyph requests, tile parsing and layout, pipeline
/// creation and the frame. The GPU device is set up before.
fn load_style(cx: &Context, view: &View) -> Metrics {
    let mut meter = Meter::new();
    let mut last = None;
    for _ in 0..cx.iterations(10) {
        let mut scene = Scene::new(view.size, Mode::Still);
        scene.jump(view.center, view.zoom);
        meter.start();
        match &view.style {
            Style::Url(url) => scene.load_url(url),
            Style::Basemap => scene.load_json(&basemap_style(&cx.fixtures)),
        }
        last = Some(scene.render_still());
        meter.stop();
    }
    if let (Some(dir), Some(image)) = (&cx.frames, &last) {
        write_frame(dir, &format!("Load/Style/{}", view.name), image);
    }
    meter.finish()
}

/// Renderers on threads of their own, as the render tests run many at
/// once: each loads a real-world style on a new map and renders its first
/// complete still through `HeadlessFrontend::render`, the threads taking
/// the styles in turn. An iteration is every renderer's load
/// and still; the GPU devices are set up before.
fn load_concurrent(cx: &Context) -> Metrics {
    const RENDERERS: usize = 16;
    let mut meter = Meter::new();
    let mut last = None;
    for _ in 0..cx.iterations(5) {
        let ready = Barrier::new(RENDERERS + 1);
        let images: Vec<PremultipliedImage> = std::thread::scope(|scope| {
            let threads: Vec<_> = REAL_WORLD
                .iter()
                .cycle()
                .take(RENDERERS)
                .map(|view| {
                    let ready = &ready;
                    scope.spawn(move || {
                        let mut scene = Scene::new(REAL_WORLD_SIZE, Mode::Still);
                        scene.jump(view.center(), view.zoom);
                        ready.wait();
                        scene.load_url(&view.style_url());
                        scene.render()
                    })
                })
                .collect();
            ready.wait();
            meter.start();
            let images = threads.into_iter().map(|thread| thread.join().expect("a renderer")).collect();
            meter.stop();
            images
        });
        last = images.into_iter().next();
    }
    if let (Some(dir), Some(image)) = (&cx.frames, &last) {
        write_frame(dir, "Load/Concurrent", image);
    }
    meter.finish()
}

fn basemap(cx: &Context, mode: Mode) -> Scene {
    let mut scene = Scene::new(BASEMAP_SIZE, mode);
    scene.jump(basemap_center(), BASEMAP_ZOOM);
    scene.load_json(&basemap_style(&cx.fixtures));
    scene
}

/// The camera around a circle of 200 pixels around the basemap's center,
/// at `index` of `lap` steps; the circle stays inside the block's tiles.
fn circle(index: usize, lap: usize) -> LatLng {
    let center = basemap_center();
    // Pixels per degree at the zoom, for 512-pixel tiles.
    let per_degree_x = 512.0 * BASEMAP_ZOOM.exp2() / 360.0;
    let per_degree_y = per_degree_x / center.latitude().to_radians().cos();
    let angle = std::f64::consts::TAU * index as f64 / lap as f64;
    LatLng::new(
        center.latitude() + 200.0 * angle.sin() / per_degree_y,
        center.longitude() + 200.0 * angle.cos() / per_degree_x,
    )
    .expect("a point of the circle")
}

/// Frames as fast as the host takes them: from a camera jump around the
/// circle until the GPU has finished the frame that shows it, over tiles an
/// untimed lap loaded.
fn frame_throughput(cx: &Context) -> Metrics {
    const LAP: usize = 240;
    let mut scene = basemap(cx, Mode::OnDemand);
    scene.settle();
    for index in 0..LAP {
        scene.jump(circle(index, LAP), BASEMAP_ZOOM);
        scene.pump();
        scene.render_frame();
    }
    scene.settle();
    let mut meter = Meter::new();
    for index in 0..cx.iterations(300) {
        scene.jump(circle(index, LAP), BASEMAP_ZOOM);
        meter.time(|| {
            scene.pump();
            scene.render_frame()
        });
    }
    let (p95, p99) = (percentile(meter.times(), 95.0), percentile(meter.times(), 99.0));
    let mut metrics = meter.finish();
    metrics.lead("frame_p95_ms", p95);
    metrics.lead("frame_p99_ms", p99);
    metrics
}

/// The flight of Frame/Paced: from the block's north-west corner at zoom
/// 14 to its south-east corner at zoom 15.5, eased in and out, at
/// `progress` from 0 to 1. The tiles stop at zoom 14, so from zoom 15 on the
/// map lays the same data out again as zoom-15 tiles, as it does past a
/// basemap's deepest zoom.
fn fly(scene: &mut Scene, progress: f64) {
    let eased = progress * progress * (3.0 - 2.0 * progress);
    let (from, to) = (basemap_north_west(), basemap_south_east());
    let center = LatLng::new(
        from.latitude() + (to.latitude() - from.latitude()) * eased,
        from.longitude() + (to.longitude() - from.longitude()) * eased,
    )
    .expect("a point of the flight");
    scene.jump(center, BASEMAP_ZOOM + 1.5 * eased);
}

/// A two-second camera flight on a 60 Hz display's clock, with only the
/// first view's tiles loaded, until the final view shows complete. At each
/// refresh with no frame in flight, the host moves the camera to where the
/// animation will be at the next refresh and draws a frame; the frame is
/// shown at the first refresh after the GPU finishes it, and the next one
/// starts there. Refreshes are absolute deadlines, and the camera follows
/// the refresh count, so every run draws the same cameras however slow.
fn frame_paced(cx: &Context) -> Metrics {
    let ticks = (Duration::from_secs(2).as_secs_f64() / REFRESH.as_secs_f64()).ceil() as usize;
    let mut scene = basemap(cx, Mode::OnDemand);
    let mut meter = Meter::new();
    let (mut frames, mut missed, mut partial) = (Vec::new(), 0, 0);
    let (mut stalls, mut complete_after) = (Vec::new(), Vec::new());
    for _ in 0..cx.iterations(3) {
        scene.clear_data();
        fly(&mut scene, 0.0);
        scene.settle();
        meter.start();
        let start = Instant::now();
        let refresh = |tick: usize| start + REFRESH * tick as u32;
        let (mut tick, mut shown, mut longest) = (0, 0, 0);
        let mut complete = false;
        while tick < ticks {
            std::thread::sleep(refresh(tick).saturating_duration_since(Instant::now()));
            scene.pump();
            let progress = ((tick + 1) as f64 / ticks as f64).min(1.0);
            fly(&mut scene, progress);
            let demanded = Instant::now();
            let status = scene.render_frame();
            let finished = Instant::now();
            frames.push(ms(finished - demanded));
            missed += usize::from(finished > refresh(tick + 1));
            partial += usize::from(status.mode != RenderMode::Full);
            complete = progress == 1.0 && status.mode == RenderMode::Full;
            // Shown at the first refresh after it finished, where the next
            // frame starts.
            let next = ((finished - start).as_secs_f64() / REFRESH.as_secs_f64()).ceil() as usize;
            longest = longest.max(next - shown);
            (shown, tick) = (next, next);
        }
        if !complete {
            fly(&mut scene, 1.0);
            scene.present_until_complete();
        }
        complete_after.push(ms(Instant::now().saturating_duration_since(refresh(ticks))));
        stalls.push(ms(REFRESH * longest as u32));
        meter.stop();
    }
    if let Some(dir) = &cx.frames {
        write_frame(dir, "Frame/Paced", &scene.read_frame());
    }
    let mut metrics = meter.finish();
    metrics.demote("time_ms");
    metrics.other("frame_p50_ms", percentile(&frames, 50.0));
    metrics.lead("frame_p95_ms", percentile(&frames, 95.0));
    metrics.lead("frame_p99_ms", percentile(&frames, 99.0));
    metrics.lead("missed_pct", 100.0 * missed as f64 / frames.len() as f64);
    metrics.lead("partial_pct", 100.0 * partial as f64 / frames.len() as f64);
    metrics.lead("longest_stall_ms", median(&stalls));
    metrics.lead("complete_after_ms", median(&complete_after));
    metrics.other("frames", frames.len() as f64 / complete_after.len() as f64);
    metrics
}

/// A settled map in slices of 100 ms, the run loop drawing whenever the map
/// asks, as an idle app's: what it costs to do nothing, and how many frames
/// it draws when nothing changes.
fn frame_idle(cx: &Context) -> Metrics {
    const SLICE: Duration = Duration::from_millis(100);
    let scene = basemap(cx, Mode::Driven);
    let start = Instant::now();
    while !scene.is_settled() {
        scene.run_for(REFRESH);
        assert!(start.elapsed() < Duration::from_secs(60), "the basemap doesn't settle");
    }
    let frames_before = scene.frames();
    let slices = cx.iterations(10);
    let mut meter = Meter::new();
    for _ in 0..slices {
        meter.time(|| scene.run_for(SLICE));
    }
    let frames = (scene.frames() - frames_before) as f64;
    let mut metrics = meter.finish();
    metrics.demote("time_ms");
    metrics.lead("frames_per_s", frames / (SLICE.as_secs_f64() * slices as f64));
    metrics
}

/// From a camera nudge on the basemap with its symbol layers left out, which
/// has nothing to place, until the map stops asking for frames: the frames
/// drawn and the time. The nudge stays over the loaded tiles.
fn frame_settle_without_symbols(cx: &Context) -> Metrics {
    let mut style: serde_json::Value =
        serde_json::from_str(&basemap_style(&cx.fixtures)).expect("the basemap style is JSON");
    style["layers"].as_array_mut().expect("the style has layers").retain(|layer| layer["type"] != "symbol");
    let mut scene = Scene::new(BASEMAP_SIZE, Mode::Driven);
    scene.jump(basemap_center(), BASEMAP_ZOOM);
    scene.load_json(&style.to_string());
    // Until a frame after `before` finishes without asking for another.
    let settle = |scene: &Scene, before: u64| {
        let start = Instant::now();
        while scene.frames() == before || !scene.is_settled() {
            scene.run_for(REFRESH);
            assert!(start.elapsed() < Duration::from_secs(60), "the basemap doesn't settle");
        }
    };
    settle(&scene, 0);
    let mut meter = Meter::new();
    let mut frames = Vec::new();
    for index in 0..cx.iterations(10) {
        let before = scene.frames();
        meter.start();
        scene.jump(basemap_center(), BASEMAP_ZOOM + 0.001 * (index % 2) as f64);
        settle(&scene, before);
        meter.stop();
        frames.push((scene.frames() - before) as f64);
    }
    let mut metrics = meter.finish();
    metrics.lead("frames", median(&frames));
    metrics
}

/// From a jump between two views of the basemap that share no tile until
/// the first frame that shows the new view complete has finished on the
/// GPU, with frames on the display's clock. Both views load once before;
/// `cold` drops the loaded tiles before each jump, so it parses and lays
/// them out again.
fn load_jump(cx: &Context, cold: bool) -> Metrics {
    let views = [basemap_south_east(), basemap_north_west()];
    let mut scene = basemap(cx, Mode::OnDemand);
    for view in views {
        scene.jump(view, BASEMAP_ZOOM);
        scene.settle();
    }
    let mut meter = Meter::new();
    let mut frames = Vec::new();
    for index in 0..cx.iterations(10) {
        if cold {
            scene.clear_data();
        }
        meter.start();
        scene.jump(views[index % 2], BASEMAP_ZOOM);
        frames.push(scene.present_until_complete() as f64);
        meter.stop();
    }
    if let Some(dir) = &cx.frames {
        let name = if cold { "Load/JumpCold" } else { "Load/JumpWarm" };
        write_frame(dir, name, &scene.read_frame());
    }
    let mut metrics = meter.finish();
    metrics.other("frames", median(&frames));
    metrics
}
