//! A render session's thread: its renderer, its frame demands, and the work
//! that the handle sends it.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use mln_actor::RunLoopHandle;
use mln_gfx::headless_backend::HeadlessBackend as _;
use mln_gfx::{BackendScope, RendererBackend, ScopeType};
use mln_map::{MapView, RenderMode};
use mln_renderer::{Renderer, RendererEvent, RendererEvents, Update, UpdateReader};
use mln_style::collection::ImmutableVector;
use mln_style::source_impl;
use mln_style::sources::{GeoJSONData, geojson_source_impl};
use mln_text::GlyphCache;
use mln_util::error::Error;
use mln_util::image::PremultipliedImage;
use mln_util::size::Size;
use mln_util::sync::MutexExt;
use mln_webgpu::HeadlessBackend;

use super::frame::{DemandMode, FrameDemand, FrameDisposition, FrameResult, STILL_IMAGE_TOKEN};
use super::session::{RenderSessionState, SessionShared, physical_size};
use super::target::OffscreenDevice;
use crate::completion::Resolver;
use crate::runtime::Outbox;

thread_local! {
    /// The session whose thread this is.
    static WORKER: RefCell<Option<Worker>> = const { RefCell::new(None) };
}

/// Runs `task` on the session's thread whose loop is `run_loop`, after the
/// work sent before.
pub(crate) fn post(run_loop: &RunLoopHandle, task: impl FnOnce(&mut Worker, &mut Outbox) + Send + 'static) {
    run_loop.invoke(move || run(task));
}

/// Runs `task` on the session, then delivers what it sends to the host, with
/// the session unborrowed and unlocked. A session that was abandoned lets go
/// of its graphics objects first.
fn run(task: impl FnOnce(&mut Worker, &mut Outbox)) {
    let mut outbox = Outbox::default();
    let unrun = WORKER.with(|cell| {
        let mut slot = cell.borrow_mut();
        let Some(worker) = slot.as_mut() else { return Some(task) };
        let shared = worker.shared.clone();
        let _gate = shared.gate.lock_unpoisoned();
        if !worker.torn_down && shared.state.lock_unpoisoned().lifecycle == RenderSessionState::Abandoned {
            worker.tear_down(&mut outbox);
        }
        task(worker, &mut outbox);
        None
    });
    drop(unrun);
    outbox.deliver();
}

/// Lets go of the session as its thread ends.
pub(crate) fn exit() {
    let worker = WORKER.with(|cell| cell.borrow_mut().take());
    drop(worker);
}

/// Work for the session's thread.
pub(crate) type Task = Box<dyn FnOnce(&mut Worker, &mut Outbox) + Send>;

/// Tells the session's thread that a resize sent to the map has landed:
/// [`land`](Self::land) when the resize reaches the session, or on drop
/// when it never does.
pub(crate) struct ResizeLanding(Option<RunLoopHandle>);

impl ResizeLanding {
    pub(crate) fn new(run_loop: RunLoopHandle) -> Self {
        ResizeLanding(Some(run_loop))
    }

    pub(crate) fn land(mut self, worker: &mut Worker, outbox: &mut Outbox) {
        self.0 = None;
        worker.resize_landed(outbox);
    }
}

impl Drop for ResizeLanding {
    fn drop(&mut self) {
        if let Some(run_loop) = self.0.take() {
            post(&run_loop, |worker, outbox| worker.resize_landed(outbox));
        }
    }
}

/// What a session's thread needs to build its renderer.
pub(crate) struct Init {
    pub(crate) device: Option<OffscreenDevice>,
    pub(crate) size: Size,
    pub(crate) pixel_ratio: f32,
    pub(crate) glyph_cache: Arc<GlyphCache>,
    pub(crate) attach: Resolver<()>,
}

/// Builds the session's renderer on its thread, before any other work.
pub(crate) fn init(shared: &Arc<SessionShared>, init: Init) {
    let shared = shared.clone();
    shared.run_loop.clone().invoke(move || {
        let mut outbox = Outbox::default();
        let gate = shared.gate.lock_unpoisoned();
        let worker = Worker::new(shared.clone(), init, &mut outbox);
        WORKER.with(|cell| *cell.borrow_mut() = Some(worker));
        drop(gate);
        outbox.deliver();
    });
}

/// The renderer and what it draws into.
struct Gfx {
    renderer: Renderer,
    backend: Rc<HeadlessBackend>,
}

impl Gfx {
    fn new(
        device: Option<OffscreenDevice>,
        size: Size,
        pixel_ratio: f32,
        glyph_cache: Arc<GlyphCache>,
    ) -> Result<Gfx, Error> {
        let device = match device {
            Some(device) => device,
            None => OffscreenDevice::new(Default::default())?,
        };
        let backend = Rc::new(offscreen_backend(device, physical_size(size, pixel_ratio))?);
        let renderer_backend: Rc<dyn RendererBackend> = backend.clone();
        let renderer = Renderer::with_glyph_cache(renderer_backend, pixel_ratio, None, glyph_cache);
        Ok(Gfx { renderer, backend })
    }

    fn scope(&self) -> BackendScope<'_> {
        BackendScope::new(self.backend.get_renderer_backend(), ScopeType::Explicit)
    }
}

/// An offscreen backend on `device`, of `size` pixels.
#[cfg(not(target_family = "wasm"))]
fn offscreen_backend(device: OffscreenDevice, size: Size) -> Result<HeadlessBackend, Error> {
    Ok(HeadlessBackend::with_device(device.device, device.queue, size, mln_gfx::ContextMode::Unique))
}

#[cfg(target_family = "wasm")]
fn offscreen_backend(_: OffscreenDevice, _: Size) -> Result<HeadlessBackend, Error> {
    Err(Error::unsupported("the browser has no offscreen devices"))
}

/// A host's frame demand on its way, and the last command sent before it.
struct PendingDemand {
    demand: FrameDemand,
    sequence: u64,
}

/// What the renderer reported about the frame it drew.
struct FrameReport {
    mode: RenderMode,
    needs_repaint: bool,
}

/// A render session on its thread.
pub(crate) struct Worker {
    shared: Arc<SessionShared>,
    /// `None` once the session failed to build it, or let go of it.
    gfx: Option<Gfx>,
    pixel_ratio: f32,
    /// The attach's completion, until the map binds the session.
    attach: Option<Resolver<()>>,
    reader: Option<UpdateReader>,
    /// The map's queue for the renderer's events.
    map_events: Option<RendererEvents>,
    /// The renderer's own queue, which the session reads before it passes
    /// the events on to the map.
    renderer_events: RendererEvents,
    update_scheduled: Arc<AtomicBool>,
    /// The sequence number of the map's last command that ran.
    map_applied: u64,
    /// The demands in the order they came, each waiting until the map ran
    /// the commands sent before it.
    queue: VecDeque<PendingDemand>,
    /// The demands that wait for a newer update than the one they found.
    waiting: Vec<(PendingDemand, u64)>,
    /// The update generation and session generation of the last render.
    rendered_update: u64,
    rendered_generation: u64,
    /// The latest update that the session tried a still image with.
    still_attempted: u64,
    /// Whether the target holds a frame since it attached or resized.
    has_frame: bool,
    /// The sources of the last render.
    sources: Option<ImmutableVector<dyn source_impl::Impl>>,
    torn_down: bool,
    /// Counts the resizes that landed, of those that the session sent.
    resizes_landed: u64,
    /// The work that waits for resizes to land, each with how many.
    behind_resizes: VecDeque<(u64, Task)>,
}

fn target_lost() -> Error {
    Error::target_lost("the render session lost its renderer")
}

impl Worker {
    fn new(shared: Arc<SessionShared>, init: Init, outbox: &mut Outbox) -> Worker {
        let renderer_events = RendererEvents::new();
        let run_loop = shared.run_loop.clone();
        renderer_events.set_waker(Some(Arc::new(move || {
            post(&run_loop, |worker, _| {
                worker.forward_events();
            });
        })));
        let mut attach = Some(init.attach);
        let gfx = match Gfx::new(init.device, init.size, init.pixel_ratio, init.glyph_cache) {
            Ok(mut gfx) => {
                gfx.renderer.set_event_queue(Some(renderer_events.clone()));
                Some(gfx)
            }
            Err(error) => {
                {
                    let mut state = shared.state.lock_unpoisoned();
                    if state.lifecycle == RenderSessionState::Attaching {
                        state.lifecycle = RenderSessionState::Failed;
                    }
                    state.failure = Some(error.clone());
                }
                if let Some(attach) = attach.take() {
                    outbox.resolve(attach, Err(error));
                }
                None
            }
        };
        Worker {
            shared,
            gfx,
            pixel_ratio: init.pixel_ratio,
            attach,
            reader: None,
            map_events: None,
            renderer_events,
            update_scheduled: Arc::new(AtomicBool::new(false)),
            map_applied: 0,
            queue: VecDeque::new(),
            waiting: Vec::new(),
            rendered_update: 0,
            rendered_generation: 0,
            still_attempted: 0,
            has_frame: false,
            sources: None,
            torn_down: false,
            resizes_landed: 0,
            behind_resizes: VecDeque::new(),
        }
    }

    // MARK: - The map

    /// The map bound the session in its command `sequence`: from now on the
    /// session draws its updates and reports to it.
    pub(crate) fn bound(&mut self, sequence: u64, reader: UpdateReader, events: RendererEvents, outbox: &mut Outbox) {
        if self.torn_down {
            return;
        }
        self.map_applied = self.map_applied.max(sequence);
        let scheduled = self.update_scheduled.clone();
        let run_loop = self.shared.run_loop.clone();
        reader.set_waker(Some(Arc::new(move || {
            if !scheduled.swap(true, Ordering::SeqCst) {
                post(&run_loop, |worker, outbox| worker.on_update(outbox));
            }
        })));
        self.reader = Some(reader);
        self.map_events = Some(events);
        {
            let mut state = self.shared.state.lock_unpoisoned();
            if state.lifecycle == RenderSessionState::Attaching {
                state.lifecycle = RenderSessionState::Attached;
            }
        }
        if let Some(attach) = self.attach.take() {
            outbox.resolve(attach, Ok(()));
        }
        self.pump(outbox);
    }

    /// The map couldn't bind the session.
    pub(crate) fn bind_failed(&mut self, error: Error, outbox: &mut Outbox) {
        {
            let mut state = self.shared.state.lock_unpoisoned();
            if state.lifecycle == RenderSessionState::Attaching {
                state.lifecycle = RenderSessionState::Failed;
                state.failure = Some(error.clone());
            }
        }
        self.gfx = None;
        if let Some(attach) = self.attach.take() {
            outbox.resolve(attach, Err(error));
        }
        self.end_demands(FrameDisposition::Failed, outbox);
    }

    /// The map ran its command `sequence`, and published what it changed.
    pub(crate) fn map_applied(&mut self, sequence: u64, outbox: &mut Outbox) {
        self.map_applied = self.map_applied.max(sequence);
        self.pump(outbox);
    }

    /// The map published an update.
    fn on_update(&mut self, outbox: &mut Outbox) {
        self.update_scheduled.store(false, Ordering::SeqCst);
        if self.gfx.is_some() && self.latest().is_some_and(|update| update.generation != self.rendered_update) {
            let wake = {
                let mut state = self.shared.state.lock_unpoisoned();
                let wake = state.redraw_armed && state.lifecycle == RenderSessionState::Attached;
                state.redraw_armed &= !wake;
                wake.then(|| self.shared.redraw_wake.clone()).flatten()
            };
            if let Some(wake) = wake {
                outbox.push(move || wake());
            }
        }
        self.pump(outbox);
    }

    fn latest(&self) -> Option<Update> {
        self.reader.as_ref()?.latest()
    }

    fn latest_generation(&self) -> u64 {
        self.reader.as_ref().map_or(0, UpdateReader::generation)
    }

    /// Passes the renderer's events on to the map.
    fn forward_events(&mut self) {
        let events = self.renderer_events.take();
        self.forward(events);
    }

    fn forward(&self, events: Vec<RendererEvent>) {
        if let Some(map_events) = &self.map_events {
            for event in events {
                map_events.push(event);
            }
        }
    }

    // MARK: - Frames

    /// Takes a host's demand, which supersedes the demands with its mode on
    /// their way unless it forces a frame.
    pub(crate) fn demand(&mut self, demand: FrameDemand, sequence: u64, outbox: &mut Outbox) {
        if demand.mode != DemandMode::Force {
            let latest = self.latest_generation();
            let (superseded, kept): (Vec<_>, Vec<_>) =
                self.queue.drain(..).partition(|pending| pending.demand.mode == demand.mode);
            self.queue = kept.into();
            let (waiting_superseded, waiting): (Vec<_>, Vec<_>) =
                self.waiting.drain(..).partition(|(pending, _)| pending.demand.mode == demand.mode);
            self.waiting = waiting;
            let superseded = superseded.into_iter().chain(waiting_superseded.into_iter().map(|(pending, _)| pending));
            for pending in superseded {
                let result = FrameResult::unrendered(pending.demand.token, FrameDisposition::Superseded, latest);
                self.publish(result, outbox);
            }
        }
        self.queue.push_back(PendingDemand { demand, sequence });
        self.pump(outbox);
    }

    /// Evaluates the demands whose commands ran and those whose update came,
    /// then a still image that the map waits for.
    fn pump(&mut self, outbox: &mut Outbox) {
        if self.torn_down || self.reader.is_none() {
            return;
        }
        while self.queue.front().is_some_and(|pending| pending.sequence <= self.map_applied) {
            if let Some(pending) = self.queue.pop_front() {
                self.evaluate(pending, outbox);
            }
        }
        let latest = self.latest_generation();
        let (updated, waiting): (Vec<_>, Vec<_>) = self.waiting.drain(..).partition(|(_, seen)| latest > *seen);
        self.waiting = waiting;
        for (pending, _) in updated {
            self.evaluate(pending, outbox);
        }
        self.render_still_image(outbox);
    }

    fn evaluate(&mut self, pending: PendingDemand, outbox: &mut Outbox) {
        let token = pending.demand.token;
        let latest = self.latest();
        let latest_generation = latest.as_ref().map_or(0, |update| update.generation);
        if self.gfx.is_none() {
            let failed = self.shared.state.lock_unpoisoned().lifecycle == RenderSessionState::Failed;
            let disposition = if failed { FrameDisposition::Failed } else { FrameDisposition::TargetNotReady };
            return self.publish(FrameResult::unrendered(token, disposition, latest_generation), outbox);
        }
        let changed = latest_generation != self.rendered_update || self.generation() != self.rendered_generation;
        match (latest, pending.demand.mode) {
            (Some(update), DemandMode::Force) => self.render(token, update, outbox),
            (Some(update), DemandMode::IfChanged | DemandMode::WaitForUpdate) if changed => {
                self.render(token, update, outbox);
            }
            (_, DemandMode::WaitForUpdate) => self.waiting.push((pending, latest_generation)),
            _ => self.publish(FrameResult::unrendered(token, FrameDisposition::NoUpdate, latest_generation), outbox),
        }
    }

    /// Renders an update that asks for a still image, once for each update.
    fn render_still_image(&mut self, outbox: &mut Outbox) {
        let Some(update) = self.latest() else { return };
        if self.gfx.is_none() || !update.parameters.still_image_request || update.generation <= self.still_attempted {
            return;
        }
        self.still_attempted = update.generation;
        self.render(STILL_IMAGE_TOKEN, update, outbox);
    }

    fn generation(&self) -> u64 {
        self.shared.state.lock_unpoisoned().generation
    }

    /// Renders `update` for the demand `token`, and publishes its result. A
    /// still map draws no frame until everything that it shows has loaded.
    fn render(&mut self, token: u64, update: Update, outbox: &mut Outbox) {
        let Some(gfx) = self.gfx.as_mut() else { return };
        {
            let backend = gfx.backend.clone();
            let _scope = BackendScope::new(backend.get_renderer_backend(), ScopeType::Explicit);
            gfx.renderer.render(&update.parameters);
        }
        self.rendered_update = update.generation;
        self.rendered_generation = self.generation();
        self.sources = Some(update.parameters.sources.clone());
        let events = self.renderer_events.take();
        let report = events.iter().rev().find_map(|event| match event {
            RendererEvent::RenderFrameFinished { mode, needs_repaint, .. } => Some(FrameReport {
                mode: match mode {
                    mln_renderer::RenderMode::Full => RenderMode::Full,
                    mln_renderer::RenderMode::Partial => RenderMode::Partial,
                },
                needs_repaint: *needs_repaint,
            }),
            _ => None,
        });
        let result = match report {
            Some(report) => {
                self.has_frame = true;
                let (generation, frame_generation) = {
                    let mut state = self.shared.state.lock_unpoisoned();
                    state.frame_generation += 1;
                    (state.generation, state.frame_generation)
                };
                FrameResult {
                    disposition: FrameDisposition::Rendered,
                    token,
                    map_update_generation: update.generation,
                    generation,
                    frame_generation,
                    needs_repaint: report.needs_repaint,
                    view: Some(MapView::from_state(&update.parameters.transform_state, f64::from(self.pixel_ratio))),
                    mode: report.mode,
                }
            }
            None => FrameResult::unrendered(token, FrameDisposition::NoUpdate, update.generation),
        };
        // The result goes out before the map hears of the frame, so a still
        // image completes with its frame's result queued.
        self.publish(result, outbox);
        self.forward(events);
    }

    /// Queues a frame result for the host, and wakes it unless it was woken
    /// since its last drain.
    fn publish(&self, result: FrameResult, outbox: &mut Outbox) {
        let wake = {
            let mut state = self.shared.state.lock_unpoisoned();
            state.results.push(result);
            let wake = !state.frame_woken;
            state.frame_woken = true;
            wake.then(|| self.shared.frame_wake.clone()).flatten()
        };
        if let Some(wake) = wake {
            outbox.push(move || wake());
        }
    }

    /// Ends every demand on its way with `disposition`.
    pub(crate) fn end_demands(&mut self, disposition: FrameDisposition, outbox: &mut Outbox) {
        let latest = self.latest_generation();
        let pending: Vec<_> = self.queue.drain(..).chain(self.waiting.drain(..).map(|(pending, _)| pending)).collect();
        for pending in pending {
            self.publish(FrameResult::unrendered(pending.demand.token, disposition, latest), outbox);
        }
    }

    // MARK: - Work

    /// Runs `task` once `resizes` of the session's resizes have landed, in
    /// the order of the work that waits.
    pub(crate) fn after_resizes(&mut self, resizes: u64, task: Task, outbox: &mut Outbox) {
        if resizes <= self.resizes_landed && self.behind_resizes.is_empty() {
            task(self, outbox);
        } else {
            self.behind_resizes.push_back((resizes, task));
        }
    }

    fn resize_landed(&mut self, outbox: &mut Outbox) {
        self.resizes_landed += 1;
        while self.behind_resizes.front().is_some_and(|(resizes, _)| *resizes <= self.resizes_landed) {
            if let Some((_, task)) = self.behind_resizes.pop_front() {
                task(self, outbox);
            }
        }
    }

    /// The target's size in logical pixels.
    pub(crate) fn size(&self) -> Size {
        self.shared.state.lock_unpoisoned().size
    }

    pub(crate) fn renderer(&mut self) -> Result<&mut Renderer, Error> {
        self.gfx.as_mut().map(|gfx| &mut gfx.renderer).ok_or_else(target_lost)
    }

    /// Resizes the target, after the map resized.
    pub(crate) fn resize(&mut self, size: Size) -> Result<(), Error> {
        let gfx = self.gfx.as_ref().ok_or_else(target_lost)?;
        gfx.backend.set_size(physical_size(size, self.pixel_ratio));
        self.has_frame = false;
        let mut state = self.shared.state.lock_unpoisoned();
        state.size = size;
        state.generation += 1;
        Ok(())
    }

    pub(crate) fn reduce_memory_use(&mut self) -> Result<(), Error> {
        self.renderer()?.reduce_memory_use();
        Ok(())
    }

    pub(crate) fn clear_data(&mut self, outbox: &mut Outbox) -> Result<(), Error> {
        self.renderer()?.clear_data();
        self.rendered_update = 0;
        let wake = {
            let mut state = self.shared.state.lock_unpoisoned();
            let wake = state.redraw_armed && state.lifecycle == RenderSessionState::Attached;
            state.redraw_armed &= !wake;
            wake.then(|| self.shared.redraw_wake.clone()).flatten()
        };
        if let Some(wake) = wake {
            outbox.push(move || wake());
        }
        Ok(())
    }

    pub(crate) fn dump_debug_logs(&mut self) -> Result<(), Error> {
        self.renderer()?.dump_debug_logs();
        Ok(())
    }

    pub(crate) fn read_texture(&mut self) -> Result<PremultipliedImage, Error> {
        let gfx = self.gfx.as_ref().ok_or_else(target_lost)?;
        if !self.has_frame {
            return Err(Error::invalid_state("the render session has no rendered frame since it attached or resized"));
        }
        let _scope = gfx.scope();
        Ok(gfx.backend.read_still_image())
    }

    /// Runs `read` on the data of the clustered GeoJSON source `source_id`
    /// of the last render, with the children of its cluster `cluster_id`.
    pub(crate) fn cluster<T>(
        &mut self,
        source_id: &str,
        cluster_id: u32,
        read: impl FnOnce(&dyn GeoJSONData, Vec<feature::Feature<f64>>) -> Result<T, Error>,
    ) -> Result<T, Error> {
        self.renderer()?;
        let source = self
            .sources
            .as_ref()
            .and_then(|sources| sources.iter().find(|source| source.base().id == source_id))
            .ok_or_else(|| Error::not_found(format!("the session's last render has no source named {source_id}")))?;
        let geojson = source
            .as_any()
            .downcast_ref::<geojson_source_impl::Impl>()
            .ok_or_else(|| Error::invalid_argument(format!("source {source_id} is not a GeoJSON source")))?;
        if !geojson.get_options().cluster {
            return Err(Error::invalid_argument(format!("GeoJSON source {source_id} does not cluster")));
        }
        let data = geojson.get_data().and_then(|data| data.upgrade());
        // A cluster's ID holds its zoom plus one in its low five bits, so an
        // ID without them names no cluster, though it would find the points
        // at zoom 0 as children. An index of no points has no clusters.
        const ZOOM_BITS: u32 = 0x1F;
        let children = data
            .as_ref()
            .filter(|_| cluster_id & ZOOM_BITS != 0)
            .and_then(|data| data.get_children(cluster_id).ok())
            .filter(|children| !children.is_empty());
        match (data, children) {
            (Some(data), Some(children)) => read(&*data, children),
            _ => Err(Error::not_found(format!("the source has no cluster with ID {cluster_id}"))),
        }
    }

    // MARK: - Lifecycle

    /// Lets go of the map's updates and the graphics objects, and ends the
    /// demands on their way.
    fn tear_down(&mut self, outbox: &mut Outbox) {
        self.end_demands(FrameDisposition::TargetNotReady, outbox);
        self.torn_down = true;
        if let Some(reader) = self.reader.take() {
            reader.set_waker(None);
        }
        self.renderer_events.set_waker(None);
        self.forward_events();
        self.gfx = None;
        self.map_events = None;
        self.sources = None;
        self.has_frame = false;
        if let Some(attach) = self.attach.take() {
            outbox.resolve(attach, Err(Error::cancelled("the render session ended before it attached")));
        }
    }

    /// Detaches the session: it lets go of its graphics objects now, and of
    /// its map once the map has unbound it.
    pub(crate) fn detach(&mut self, outbox: &mut Outbox) {
        self.tear_down(outbox);
        let map = self.shared.state.lock_unpoisoned().map.clone();
        match map {
            Some(map) => map.unbind_session(self.shared.id, self.shared.run_loop.clone(), |worker, outbox| {
                worker.finish(outbox);
            }),
            None => post(&self.shared.run_loop, |worker, outbox| worker.finish(outbox)),
        }
    }

    /// Ends the session once the map unbound it: it completes the detach and
    /// the releases, lets go of the map, and stops the session's thread.
    pub(crate) fn finish(&mut self, outbox: &mut Outbox) {
        let (waiters, map) = {
            let mut state = self.shared.state.lock_unpoisoned();
            if state.lifecycle == RenderSessionState::Detaching {
                state.lifecycle = RenderSessionState::Detached;
            }
            (std::mem::take(&mut state.detach_waiters), state.map.take())
        };
        if let Some(map) = &map {
            map.release_session_claim(self.shared.id);
        }
        for waiter in waiters {
            outbox.resolve(waiter, Ok(()));
        }
        outbox.push(move || drop(map));
        self.shared.run_loop.stop();
    }
}
