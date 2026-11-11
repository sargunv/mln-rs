//! The handle of a render session, and the state that its thread shares
//! with the handle.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::ThreadId;

use mln_actor::{RunLoop, RunLoopHandle};
use mln_map::LogicalExtent;
use mln_renderer::query::{RenderedQueryOptions, SourceQueryOptions};
use mln_util::error::Error;
use mln_util::feature::Feature;
use mln_util::image::PremultipliedImage;
use mln_util::size::Size;
use mln_util::sync::MutexExt;

use super::frame::{FrameDemand, FrameResult, STILL_IMAGE_TOKEN};
use super::query::RenderedQueryGeometry;
use super::target::{AttachOptions, Driver, RenderTarget, Wake};
use super::worker::{self, Init, Worker};
use crate::completion::{Completion, Resolver};
use crate::map::{Applied, Map};
use crate::runtime::Outbox;

/// The ID of a render session, unique in the process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SessionId(u64);

/// Where a render session is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RenderSessionState {
    /// The session is starting: it builds its renderer and binds to its map.
    Attaching,
    Attached,
    /// A detach or release is on its way.
    Detaching,
    /// The session let go of its map and its graphics objects.
    Detached,
    /// The session couldn't build its renderer. Its snapshot says why.
    Failed,
    Abandoned,
}

/// The state of a render session at one moment.
#[derive(Clone, Debug, PartialEq)]
pub struct RenderSessionSnapshot {
    pub state: RenderSessionState,
    /// The target's logical size, and the map's pixel ratio.
    pub extent: LogicalExtent,
    /// Grows each time the target changes size.
    pub generation: u64,
    /// Counts the frames that the session rendered.
    pub frame_generation: u64,
    /// Why the session failed.
    pub failure: Option<Error>,
    /// Whether the session is attached and has a target to render to.
    pub has_target: bool,
}

/// What a render session can do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RenderSessionCapabilities {
    pub driver: Driver,
    /// How many textures the session's texture ring has.
    pub texture_ring_depth: u32,
    /// Whether the host can take the frames it renders.
    pub frame_acquisition: bool,
    /// Whether [`RenderSession::read_texture`] can read its pixels.
    pub readback: bool,
    /// Whether it shows frames on a surface.
    pub presentation: bool,
}

/// The state that a session's handle and its thread share.
pub(crate) struct SessionState {
    pub(crate) lifecycle: RenderSessionState,
    pub(crate) released: bool,
    pub(crate) size: Size,
    pub(crate) generation: u64,
    pub(crate) frame_generation: u64,
    pub(crate) failure: Option<Error>,
    /// The frame results that wait for a drain.
    pub(crate) results: Vec<FrameResult>,
    /// Whether the frame wake ran since the last drain.
    pub(crate) frame_woken: bool,
    /// Whether the redraw wake may run: from the last demand until it runs.
    pub(crate) redraw_armed: bool,
    /// The completions of the detach and the releases on their way.
    pub(crate) detach_waiters: Vec<Resolver<()>>,
    /// The session's map, until the session lets go of it.
    pub(crate) map: Option<Map>,
    /// Counts the resizes sent, which run on the map's thread before they
    /// reach the session's.
    pub(crate) resizes_sent: u64,
}

pub(crate) struct SessionShared {
    pub(crate) id: SessionId,
    pub(crate) run_loop: RunLoopHandle,
    pub(crate) thread: ThreadId,
    pub(crate) state: Mutex<SessionState>,
    /// Held while the session's thread runs a task, so an abandon waits for
    /// the graphics work in progress.
    pub(crate) gate: Mutex<()>,
    pub(crate) frame_wake: Option<Wake>,
    pub(crate) redraw_wake: Option<Wake>,
    pub(crate) scale_factor: f64,
    capabilities: RenderSessionCapabilities,
}

impl SessionShared {
    /// A completion that the session's thread resolves.
    fn completion<T: Send + 'static>(&self) -> (Completion<T>, Resolver<T>) {
        Completion::new(Some(self.thread))
    }

    /// Starts a detach on the session's thread, after the work sent before.
    /// The state is locked, and the session usable or failed.
    fn start_detach(&self, state: &mut SessionState) {
        state.lifecycle = RenderSessionState::Detaching;
        worker::post(&self.run_loop, |worker, outbox| worker.detach(outbox));
    }

    /// Releases the session without waiting: it detaches, unless it is
    /// detached or abandoned already, and every call on it fails after.
    fn dispose(&self, done: Option<Resolver<()>>) -> Result<(), Error> {
        let mut state = self.state.lock_unpoisoned();
        if state.released {
            return Err(released());
        }
        state.released = true;
        match state.lifecycle {
            RenderSessionState::Detached | RenderSessionState::Abandoned => {
                drop(state);
                if let Some(done) = done {
                    done.resolve(Ok(()));
                }
                return Ok(());
            }
            RenderSessionState::Detaching => {}
            RenderSessionState::Attaching | RenderSessionState::Attached | RenderSessionState::Failed => {
                self.start_detach(&mut state);
            }
        }
        state.detach_waiters.extend(done);
        Ok(())
    }
}

fn released() -> Error {
    Error::invalid_state("the render session was released")
}

/// Fails unless the session takes work: it is attaching or attached, and
/// not released.
fn check_usable(state: &SessionState) -> Result<(), Error> {
    if state.released {
        return Err(released());
    }
    match state.lifecycle {
        RenderSessionState::Attaching | RenderSessionState::Attached => Ok(()),
        RenderSessionState::Failed => {
            Err(state.failure.clone().unwrap_or_else(|| Error::native("the render session failed")))
        }
        RenderSessionState::Detaching | RenderSessionState::Detached | RenderSessionState::Abandoned => {
            Err(Error::invalid_state("the render session is not attached: it was detached or abandoned"))
        }
    }
}

/// Sizes in pixels a target of `size` logical pixels at `pixel_ratio`.
pub(crate) fn physical_size(size: Size, pixel_ratio: f32) -> Size {
    Size::new((size.width as f32 * pixel_ratio) as u32, (size.height as f32 * pixel_ratio) as u32)
}

/// A render session: a renderer that draws a map into a target on a thread
/// of its own.
///
/// The host demands frames, which finish with [`FrameResult`]s that
/// [`drain_frame_results`](Self::drain_frame_results) takes. A demand shows
/// the map's commands sent before it, except the changes that an open
/// command group holds back. The session's work runs on its thread in the
/// order it was sent, and reports through a [`Completion`]. `Clone` is
/// another handle to the same session, and any thread may use one.
///
/// The session lives until [`detach`](Self::detach),
/// [`release`](Self::release), [`dispose`](Self::dispose) or
/// [`abandon`](Self::abandon), or until its last handle drops, and it holds
/// its map until then. A call that sends work to a session that no longer
/// takes it fails with [`ErrorKind::InvalidState`](mln_util::ErrorKind), or
/// with the failure of a session that failed.
#[derive(Clone)]
pub struct RenderSession {
    handle: Arc<SessionHandle>,
}

/// Disposes of the session when the last handle drops.
struct SessionHandle {
    shared: Arc<SessionShared>,
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        let _ = self.shared.dispose(None);
    }
}

impl std::fmt::Debug for RenderSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RenderSession").field("id", &self.shared().id).finish()
    }
}

impl PartialEq for RenderSession {
    fn eq(&self, other: &RenderSession) -> bool {
        Arc::ptr_eq(&self.handle, &other.handle)
    }
}

impl Eq for RenderSession {}

/// Attaches a session to `map`, as [`Map::attach`] documents.
pub(crate) fn attach(
    map: &Map,
    target: RenderTarget,
    options: AttachOptions,
) -> Result<(RenderSession, Completion<()>), Error> {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    let RenderTarget::Offscreen { size, device } = target;
    if size.is_empty() {
        return Err(Error::invalid_argument("the target's size must not be empty"));
    }
    if options.driver != Driver::CoreWorker {
        return Err(Error::unsupported("an offscreen session renders on a core worker"));
    }
    let id = SessionId(NEXT_ID.fetch_add(1, Ordering::Relaxed));
    map.claim_session(id)?;
    let (run_loop, thread) = match start_thread() {
        Ok(started) => started,
        Err(error) => {
            map.release_session_claim(id);
            return Err(error);
        }
    };
    let pixel_ratio = map.pixel_ratio();
    let shared = Arc::new(SessionShared {
        id,
        run_loop: run_loop.clone(),
        thread,
        state: Mutex::new(SessionState {
            lifecycle: RenderSessionState::Attaching,
            released: false,
            size,
            generation: 1,
            frame_generation: 0,
            failure: None,
            results: Vec::new(),
            frame_woken: false,
            redraw_armed: true,
            detach_waiters: Vec::new(),
            map: Some(map.clone()),
            resizes_sent: 0,
        }),
        gate: Mutex::new(()),
        frame_wake: options.frame_wake,
        redraw_wake: options.redraw_wake,
        scale_factor: f64::from(pixel_ratio),
        capabilities: RenderSessionCapabilities {
            driver: Driver::CoreWorker,
            texture_ring_depth: 1,
            frame_acquisition: false,
            readback: true,
            presentation: false,
        },
    });
    let (completion, resolver) = shared.completion();
    worker::init(&shared, Init { device, size, pixel_ratio, glyph_cache: map.glyph_cache(), attach: resolver });
    let session = RenderSession { handle: Arc::new(SessionHandle { shared: shared.clone() }) };
    if let Err(error) = map.bind_session(id, run_loop, size) {
        // The map was released meanwhile: the session lets go of it.
        drop(session);
        return Err(error);
    }
    Ok((session, completion))
}

/// Starts a session's thread, which runs its loop until the session ends.
fn start_thread() -> Result<(RunLoopHandle, ThreadId), Error> {
    let (started, start) = mpsc::channel();
    let thread = mln_util::thread::builder("RenderSession")
        .spawn(move || {
            let run_loop = RunLoop::new();
            let _ = started.send(run_loop.handle());
            run_loop.run();
            worker::exit();
        })
        .map_err(|error| Error::native(format!("can't start the render session's thread: {error}")))?;
    let thread_id = thread.thread().id();
    // The thread ends by itself once the session ends, so nothing joins it.
    drop(thread);
    let run_loop = start.recv().map_err(|_| Error::native("the render session's thread stopped as it started"))?;
    Ok((run_loop, thread_id))
}

impl RenderSession {
    fn shared(&self) -> &Arc<SessionShared> {
        &self.handle.shared
    }

    /// Sends `task` to the session's thread, after the work sent before,
    /// resizes included, and completes with its result. A session that lost
    /// its graphics objects before the task runs completes it with
    /// [`ErrorKind::TargetLost`](mln_util::ErrorKind).
    fn operation<T: Send + 'static>(
        &self,
        task: impl FnOnce(&mut Worker, &mut Outbox) -> Result<T, Error> + Send + 'static,
    ) -> Result<Completion<T>, Error> {
        let shared = self.shared();
        let state = shared.state.lock_unpoisoned();
        check_usable(&state)?;
        let (completion, resolver) = shared.completion();
        let resizes = state.resizes_sent;
        worker::post(&shared.run_loop, move |worker, outbox| {
            let task: worker::Task = Box::new(move |worker, outbox| {
                let result = task(worker, outbox);
                outbox.resolve(resolver, result);
            });
            worker.after_resizes(resizes, task, outbox);
        });
        drop(state);
        Ok(completion)
    }

    /// What the session can do. Fails with
    /// [`ErrorKind::InvalidState`](mln_util::ErrorKind) once it is released.
    pub fn capabilities(&self) -> Result<RenderSessionCapabilities, Error> {
        let state = self.shared().state.lock_unpoisoned();
        if state.released {
            return Err(released());
        }
        Ok(self.shared().capabilities)
    }

    /// The session's state now. Fails with
    /// [`ErrorKind::InvalidState`](mln_util::ErrorKind) once it is released.
    pub fn snapshot(&self) -> Result<RenderSessionSnapshot, Error> {
        let state = self.shared().state.lock_unpoisoned();
        if state.released {
            return Err(released());
        }
        Ok(RenderSessionSnapshot {
            state: state.lifecycle,
            extent: LogicalExtent { size: state.size, scale_factor: self.shared().scale_factor },
            generation: state.generation,
            frame_generation: state.frame_generation,
            failure: state.failure.clone(),
            has_target: state.lifecycle == RenderSessionState::Attached,
        })
    }

    /// Asks the session to render a frame, which shows the map commands sent
    /// before the call, except the changes that an open command group holds
    /// back. A demand sent while the session attaches waits for it.
    ///
    /// Every demand finishes with one [`FrameResult`] that carries its token.
    /// A demand that isn't [`DemandMode::Force`](super::DemandMode::Force)
    /// replaces the demands with the same mode still on their way, which
    /// finish as [`FrameDisposition::Superseded`](super::FrameDisposition).
    ///
    /// Fails with [`ErrorKind::InvalidArgument`](mln_util::ErrorKind) for the
    /// token [`STILL_IMAGE_TOKEN`].
    pub fn demand_frame(&self, demand: FrameDemand) -> Result<(), Error> {
        if demand.token == STILL_IMAGE_TOKEN {
            return Err(Error::invalid_argument("the token is reserved for still images"));
        }
        let shared = self.shared();
        let mut state = shared.state.lock_unpoisoned();
        check_usable(&state)?;
        let sequence = state.map.as_ref().map_or(0, Map::sent_sequence);
        state.redraw_armed = true;
        worker::post(&shared.run_loop, move |worker, outbox| worker.demand(demand, sequence, outbox));
        Ok(())
    }

    /// Takes the frame results that wait, oldest first. Fails with
    /// [`ErrorKind::InvalidState`](mln_util::ErrorKind) once the session is
    /// released.
    pub fn drain_frame_results(&self) -> Result<Vec<FrameResult>, Error> {
        let mut state = self.shared().state.lock_unpoisoned();
        if state.released {
            return Err(released());
        }
        state.frame_woken = false;
        Ok(std::mem::take(&mut state.results))
    }

    /// Resizes the target, and the map with it. The session renders at the
    /// map's pixel ratio. A later demand shows the new size, and one sent
    /// before shows the old.
    ///
    /// Fails with [`ErrorKind::InvalidArgument`](mln_util::ErrorKind) for an
    /// empty size.
    pub fn resize(&self, size: Size) -> Result<Completion<Applied>, Error> {
        if size.is_empty() {
            return Err(Error::invalid_argument("the target's size must not be empty"));
        }
        let shared = self.shared();
        let mut state = shared.state.lock_unpoisoned();
        check_usable(&state)?;
        let map = state.map.clone().ok_or_else(|| Error::invalid_state("the render session has no map"))?;
        let (completion, resolver) = shared.completion();
        // The work sent after waits for the resize to land, which it does
        // even when the map drops it.
        state.resizes_sent += 1;
        let landing = worker::ResizeLanding::new(shared.run_loop.clone());
        map.resize_for_session(size, shared.run_loop.clone(), move |worker, result, outbox| {
            let result = result.and_then(|applied| worker.resize(size).map(|()| applied));
            outbox.resolve(resolver, result);
            landing.land(worker, outbox);
        })?;
        drop(state);
        Ok(completion)
    }

    /// Lets go of the target and keeps the rest of the session. An offscreen
    /// session's target is its own, so this fails with
    /// [`ErrorKind::Unsupported`](mln_util::ErrorKind).
    pub fn release_target(&self) -> Result<Completion<()>, Error> {
        check_usable(&self.shared().state.lock_unpoisoned())?;
        Err(Error::unsupported("an offscreen session keeps its target"))
    }

    /// Ends the frame demands on their way, including those that wait for a
    /// map update, which finish as
    /// [`FrameDisposition::NoUpdate`](super::FrameDisposition), and completes
    /// once the work sent before it has run.
    pub fn barrier(&self) -> Result<Completion<()>, Error> {
        self.operation(|worker, outbox| {
            worker.end_demands(super::FrameDisposition::NoUpdate, outbox);
            Ok(())
        })
    }

    /// Frees memory that the renderer can rebuild, such as caches.
    pub fn reduce_memory_use(&self) -> Result<Completion<()>, Error> {
        self.operation(|worker, _| worker.reduce_memory_use())
    }

    /// Clears what the renderer holds, such as loaded tiles, so that the
    /// next frame loads and draws the map again.
    pub fn clear_data(&self) -> Result<Completion<()>, Error> {
        self.operation(|worker, outbox| worker.clear_data(outbox))
    }

    /// Logs the renderer's sources and tiles.
    pub fn dump_debug_logs(&self) -> Result<Completion<()>, Error> {
        self.operation(|worker, _| worker.dump_debug_logs())
    }

    /// The pixels of the last frame that the session rendered: premultiplied
    /// RGBA, in rows from top to bottom, at the target's size in pixels.
    ///
    /// Completes with [`ErrorKind::InvalidState`](mln_util::ErrorKind) when
    /// the session hasn't rendered a frame since it attached or resized.
    pub fn read_texture(&self) -> Result<Completion<PremultipliedImage>, Error> {
        self.operation(|worker, _| worker.read_texture())
    }

    /// The features that the session drew in an area of the screen, in its
    /// last frame. A feature that several layers draw appears once for each.
    /// A box covers only its part inside the target, so one past every edge
    /// covers the whole target.
    ///
    /// Fails with [`ErrorKind::InvalidArgument`](mln_util::ErrorKind) for a
    /// point that isn't finite or a line string without points.
    pub fn query_rendered_features(
        &self,
        geometry: RenderedQueryGeometry,
        options: RenderedQueryOptions,
    ) -> Result<Completion<Vec<Feature>>, Error> {
        geometry.points()?;
        self.operation(move |worker, _| match geometry.within(worker.size()).map(|geometry| geometry.points()) {
            Some(points) => worker.renderer()?.query_rendered_features(&points?, &options),
            None => Ok(Vec::new()),
        })
    }

    /// The features of the source `source_id` in the tiles that it loaded
    /// for the current view. A source loads tiles only while a visible layer
    /// uses it.
    ///
    /// Fails with [`ErrorKind::InvalidArgument`](mln_util::ErrorKind) for an
    /// empty `source_id`.
    pub fn query_source_features(
        &self,
        source_id: impl Into<String>,
        options: SourceQueryOptions,
    ) -> Result<Completion<Vec<Feature>>, Error> {
        let source_id = source_id_of(source_id)?;
        self.operation(move |worker, _| worker.renderer()?.query_source_features(&source_id, &options))
    }

    /// The clusters and points that a cluster of a clustered GeoJSON source
    /// splits into at the next zoom level. `cluster_id` is a cluster
    /// feature's `cluster_id` property.
    ///
    /// The query reads the source as the session's most recent render
    /// attempt had it. A cluster ID belongs to one version of the source's
    /// data, so query the features again after the data changes.
    ///
    /// Completes with [`ErrorKind::NotFound`](mln_util::ErrorKind) when that
    /// render had no source `source_id`, or the source has no cluster
    /// `cluster_id`, and with [`ErrorKind::InvalidArgument`](mln_util::ErrorKind)
    /// when the source isn't a clustered GeoJSON source.
    pub fn cluster_children(
        &self,
        source_id: impl Into<String>,
        cluster_id: u32,
    ) -> Result<Completion<Vec<feature::Feature<f64>>>, Error> {
        let source_id = source_id_of(source_id)?;
        self.operation(move |worker, _| worker.cluster(&source_id, cluster_id, |_, children| Ok(children)))
    }

    /// The points in a cluster of a clustered GeoJSON source: it skips the
    /// first `offset`, then holds up to `limit`. It fails as
    /// [`cluster_children`](Self::cluster_children) does.
    pub fn cluster_leaves(
        &self,
        source_id: impl Into<String>,
        cluster_id: u32,
        limit: u32,
        offset: u32,
    ) -> Result<Completion<Vec<feature::Feature<f64>>>, Error> {
        let source_id = source_id_of(source_id)?;
        self.operation(move |worker, _| {
            worker.cluster(&source_id, cluster_id, |index, _| index.get_leaves(cluster_id, limit, offset))
        })
    }

    /// The zoom level at which a cluster of a clustered GeoJSON source splits
    /// into two or more clusters or points. It fails as
    /// [`cluster_children`](Self::cluster_children) does.
    pub fn cluster_expansion_zoom(
        &self,
        source_id: impl Into<String>,
        cluster_id: u32,
    ) -> Result<Completion<u8>, Error> {
        let source_id = source_id_of(source_id)?;
        self.operation(move |worker, _| {
            worker.cluster(&source_id, cluster_id, |index, _| index.get_cluster_expansion_zoom(cluster_id))
        })
    }

    /// Detaches the session from its map and frees its graphics objects. The
    /// demands on their way finish as
    /// [`FrameDisposition::TargetNotReady`](super::FrameDisposition). Once
    /// the detach completes, another session can attach to the map.
    pub fn detach(&self) -> Result<Completion<()>, Error> {
        let shared = self.shared();
        let mut state = shared.state.lock_unpoisoned();
        if !matches!(state.lifecycle, RenderSessionState::Failed) {
            check_usable(&state)?;
        } else if state.released {
            return Err(released());
        }
        let (completion, resolver) = shared.completion();
        state.detach_waiters.push(resolver);
        shared.start_detach(&mut state);
        Ok(completion)
    }

    /// Stops the session at once, without its pending graphics work: that
    /// work completes with [`ErrorKind::TargetLost`](mln_util::ErrorKind),
    /// and the demands on their way finish as
    /// [`FrameDisposition::TargetNotReady`](super::FrameDisposition). Once it
    /// returns, the session renders no more, and another session can attach
    /// to the map. Release the session afterward.
    ///
    /// It waits for a frame that the session is rendering. Fails with
    /// [`ErrorKind::Busy`](mln_util::ErrorKind) on the session's own thread
    /// while that runs, and with [`ErrorKind::InvalidState`](mln_util::ErrorKind)
    /// once the session is detached, abandoned or released.
    pub fn abandon(&self) -> Result<(), Error> {
        let shared = self.shared();
        let _gate = if std::thread::current().id() == shared.thread {
            shared.gate.try_lock().map_err(|_| Error::busy("the render session is rendering on this thread"))?
        } else {
            shared.gate.lock_unpoisoned()
        };
        let (waiters, map) = {
            let mut state = shared.state.lock_unpoisoned();
            if state.released {
                return Err(released());
            }
            match state.lifecycle {
                RenderSessionState::Attaching
                | RenderSessionState::Attached
                | RenderSessionState::Detaching
                | RenderSessionState::Failed => {}
                RenderSessionState::Detached | RenderSessionState::Abandoned => {
                    return Err(Error::invalid_state("the render session was detached or abandoned"));
                }
            }
            state.lifecycle = RenderSessionState::Abandoned;
            (std::mem::take(&mut state.detach_waiters), state.map.take())
        };
        match &map {
            // The map unbinds the session before another can attach.
            Some(map) => {
                map.unbind_session(shared.id, shared.run_loop.clone(), |worker, outbox| worker.finish(outbox));
                map.release_session_claim(shared.id);
            }
            None => worker::post(&shared.run_loop, |worker, outbox| worker.finish(outbox)),
        }
        drop(_gate);
        for waiter in waiters {
            waiter.resolve(Err(Error::target_lost("the render session was abandoned")));
        }
        drop(map);
        Ok(())
    }

    /// Releases the session, detaching it first when it is attached. The
    /// completion reports when the session has let go of its map and its
    /// graphics objects. Every call on the session fails with
    /// [`ErrorKind::InvalidState`](mln_util::ErrorKind) after.
    pub fn release(&self) -> Result<Completion<()>, Error> {
        let (completion, resolver) = self.shared().completion();
        self.shared().dispose(Some(resolver))?;
        Ok(completion)
    }

    /// Releases the session in any state, without waiting. Dropping the last
    /// handle does the same.
    pub fn dispose(&self) -> Result<(), Error> {
        self.shared().dispose(None)
    }
}

fn source_id_of(source_id: impl Into<String>) -> Result<String, Error> {
    let source_id = source_id.into();
    if source_id.is_empty() {
        return Err(Error::invalid_argument("the source ID must not be empty"));
    }
    Ok(source_id)
}

// Handles cross threads; the renderer never leaves its own.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<RenderSession>();
};
