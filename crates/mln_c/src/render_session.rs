//! `render_session.h`: the life of a render session, its frame demands and
//! results, and the work it runs on its driver.

use core::ffi::c_char;
use std::sync::Arc;

use ::mln_runtime::{
    AttachOptions, DemandMode, Driver, FrameDemand, FrameDisposition, FrameResult, RenderSession, RenderSessionState,
    RenderTarget,
};
use mln_util::size::Size;

use crate::abi::{self, Fields};
use crate::callback::Wake;
use crate::completion::{self, CompletionValue, Generations, PendingCompletion};
use crate::error::{Error, ErrorKind};
use crate::generated::types::{
    MLN_FRAME_DEMAND_FORCE, MLN_FRAME_DEMAND_WAIT_FOR_UPDATE, MLN_GPU_SYNC_CPU_COMPLETE,
    MLN_GPU_SYNC_KIND_FLAG_CPU_COMPLETE, MLN_GPU_SYNC_KIND_FLAG_D3D12_FENCE, MLN_GPU_SYNC_KIND_FLAG_METAL_SHARED_EVENT,
    MLN_GPU_SYNC_KIND_FLAG_OPEN_GL_FENCE, MLN_GPU_SYNC_KIND_FLAG_QUEUE_ORDER, MLN_GPU_SYNC_KIND_FLAG_SYNC_FD,
    MLN_GPU_SYNC_KIND_FLAG_VULKAN_TIMELINE_SEMAPHORE, MLN_RENDER_ABANDON_DISPOSITION_CLEAN,
    MLN_RENDER_DRIVER_CALLER_GRAPHICS_THREAD, MLN_RENDER_DRIVER_CORE_WORKER, MLN_RENDER_RESULT_FAILED,
    MLN_RENDER_RESULT_NO_UPDATE, MLN_RENDER_RESULT_RENDERED, MLN_RENDER_RESULT_SUPERSEDED,
    MLN_RENDER_RESULT_TARGET_NOT_READY, MLN_RENDER_SESSION_CAPABILITY_FRAME_ACQUISITION,
    MLN_RENDER_SESSION_CAPABILITY_PRESENTATION, MLN_RENDER_SESSION_CAPABILITY_READBACK,
    MLN_RENDER_SESSION_STATE_ABANDONED, MLN_RENDER_SESSION_STATE_ATTACHED, MLN_RENDER_SESSION_STATE_ATTACHING,
    MLN_RENDER_SESSION_STATE_DETACHED, MLN_RENDER_SESSION_STATE_DETACHING, MLN_RENDER_SESSION_STATE_FAILED,
    MLN_STATUS_NATIVE_ERROR, MLN_STATUS_OK, MLN_STATUS_TARGET_LOST, mln_completion, mln_diagnostic, mln_frame_demand,
    mln_frame_pacer, mln_logical_extent, mln_logical_size, mln_map_view, mln_render_abandon_result,
    mln_render_frame_batch, mln_render_frame_batch_view, mln_render_frame_result, mln_render_session,
    mln_render_session_attach_options, mln_render_session_capabilities, mln_render_session_snapshot, mln_status,
};
use crate::handles::{self, HandleKind, HandleTable};
use crate::map::{self, MapObject};
use crate::{buffer, events, projection, render_target};

abi::versioned!(
    mln_render_session_attach_options,
    mln_render_session_capabilities,
    mln_render_session_snapshot,
    mln_frame_demand,
    mln_render_frame_batch_view,
    mln_render_abandon_result,
);

pub(crate) struct SessionObject {
    pub(crate) session: RenderSession,
    /// The session's map, whose snapshot gives the generations of the
    /// session's queries.
    map: ::mln_runtime::Map,
    /// The map's scale factor as the caller gave it, which the session's
    /// extents and views report.
    scale_factor: f64,
    has_redraw_wake: bool,
    /// Held so that its user data lives as long as the session's handle: a
    /// core worker never runs it.
    _driver_work_wake: Arc<Wake>,
}

static SESSIONS: HandleTable<SessionObject> = HandleTable::new(HandleKind::RenderSession);

/// The results of one drain, which their batch handle owns.
struct FrameBatch(Box<[mln_render_frame_result]>);

// SAFETY: the results are plain values; their views point nowhere.
unsafe impl Send for FrameBatch {}
// SAFETY: as for `Send`.
unsafe impl Sync for FrameBatch {}

static BATCHES: HandleTable<FrameBatch> = HandleTable::new(HandleKind::RenderFrameBatch);

/// The session that `session` names.
pub(crate) fn resolve(session: mln_render_session) -> Result<Arc<SessionObject>, Error> {
    SESSIONS.get(session)
}

// MARK: - Attach

const KNOWN_SYNC_KINDS: u32 = MLN_GPU_SYNC_KIND_FLAG_CPU_COMPLETE
    | MLN_GPU_SYNC_KIND_FLAG_OPEN_GL_FENCE
    | MLN_GPU_SYNC_KIND_FLAG_QUEUE_ORDER
    | MLN_GPU_SYNC_KIND_FLAG_VULKAN_TIMELINE_SEMAPHORE
    | MLN_GPU_SYNC_KIND_FLAG_SYNC_FD
    | MLN_GPU_SYNC_KIND_FLAG_METAL_SHARED_EVENT
    | MLN_GPU_SYNC_KIND_FLAG_D3D12_FENCE;

/// A caller's checked `mln_render_session_attach_options`.
pub(crate) struct AttachRequest {
    driver: Driver,
    frame_wake: Arc<Wake>,
    redraw_wake: Arc<Wake>,
    has_redraw_wake: bool,
    driver_work_wake: Arc<Wake>,
    has_queue_lock: bool,
}

/// Checks a caller's attach options.
///
/// # Safety
///
/// `options` is null or points to a readable struct of the size it declares.
pub(crate) unsafe fn attach_options(options: *const mln_render_session_attach_options) -> Result<AttachRequest, Error> {
    // SAFETY: the caller's contract.
    let options = unsafe { abi::read_sized(options) }?;
    let driver = match options.driver {
        MLN_RENDER_DRIVER_CORE_WORKER => Driver::CoreWorker,
        MLN_RENDER_DRIVER_CALLER_GRAPHICS_THREAD => Driver::CallerThread,
        _ => return Err(Error::invalid_argument("render driver kind is invalid")),
    };
    Fields::new(options.producer_sync_kinds, KNOWN_SYNC_KINDS, "producer_sync_kinds")?;
    let lock = options.queue_lock;
    if lock.lock.is_some() != lock.unlock.is_some() {
        return Err(Error::invalid_argument("a queue lock sets both lock and unlock, or neither"));
    }
    Ok(AttachRequest {
        driver,
        frame_wake: Arc::new(Wake::new(&options.frame_wake)?),
        redraw_wake: Arc::new(Wake::new(&options.redraw_wake)?),
        has_redraw_wake: options.redraw_wake.callback.is_some(),
        driver_work_wake: Arc::new(Wake::new(&options.driver_work_wake)?),
        has_queue_lock: lock.lock.is_some(),
    })
}

impl AttachRequest {
    /// Fails with `MLN_STATUS_UNSUPPORTED` unless the caller asked for a core
    /// worker.
    pub(crate) fn require_core_worker(&self, message: &'static str) -> Result<(), Error> {
        if self.driver == Driver::CoreWorker { Ok(()) } else { Err(Error::new(ErrorKind::Unsupported, message)) }
    }

    /// Fails with `MLN_STATUS_UNSUPPORTED` when the caller set a queue lock.
    pub(crate) fn refuse_queue_lock(&self, message: &'static str) -> Result<(), Error> {
        if self.has_queue_lock { Err(Error::new(ErrorKind::Unsupported, message)) } else { Ok(()) }
    }

    fn wakes(&self) -> [&Wake; 3] {
        [&self.frame_wake, &self.redraw_wake, &self.driver_work_wake]
    }
}

/// The runtime's form of a caller's wake.
fn runtime_wake(wake: &Arc<Wake>) -> ::mln_runtime::Wake {
    let wake = Arc::clone(wake);
    Arc::new(move || wake.notify())
}

/// Attaches a session to `map` and gives it a handle.
pub(crate) fn attach(
    map: &MapObject,
    target: RenderTarget,
    request: AttachRequest,
) -> Result<(mln_render_session, ::mln_runtime::Completion<()>), Error> {
    let options = AttachOptions {
        driver: request.driver,
        frame_wake: Some(runtime_wake(&request.frame_wake)),
        redraw_wake: request.has_redraw_wake.then(|| runtime_wake(&request.redraw_wake)),
    };
    // Accepted first, so that a wake that the session runs as it starts
    // reaches the caller.
    request.wakes().iter().for_each(|wake| wake.accept());
    let revoke = |request: &AttachRequest| request.wakes().iter().for_each(|wake| wake.revoke());
    let (session, attached) = match map.map.attach(target, options) {
        Ok(attached) => attached,
        Err(error) => {
            revoke(&request);
            return Err(error.into());
        }
    };
    let object = SessionObject {
        session: session.clone(),
        map: map.map.clone(),
        scale_factor: map.scale_factor,
        has_redraw_wake: request.has_redraw_wake,
        _driver_work_wake: Arc::clone(&request.driver_work_wake),
    };
    match SESSIONS.insert(Arc::new(object)) {
        Ok(handle) => Ok((handle, attached)),
        Err(error) => {
            revoke(&request);
            let _ = session.dispose();
            Err(error)
        }
    }
}

// MARK: - State

fn state(state: RenderSessionState) -> u32 {
    match state {
        RenderSessionState::Attaching => MLN_RENDER_SESSION_STATE_ATTACHING,
        RenderSessionState::Attached => MLN_RENDER_SESSION_STATE_ATTACHED,
        RenderSessionState::Detaching => MLN_RENDER_SESSION_STATE_DETACHING,
        RenderSessionState::Detached => MLN_RENDER_SESSION_STATE_DETACHED,
        RenderSessionState::Failed => MLN_RENDER_SESSION_STATE_FAILED,
        RenderSessionState::Abandoned => MLN_RENDER_SESSION_STATE_ABANDONED,
    }
}

/// The status that a session's failure reports.
fn failure_status(failure: Option<&mln_util::Error>) -> mln_status {
    match failure {
        None => MLN_STATUS_OK,
        Some(error) if error.kind == mln_util::ErrorKind::TargetLost => MLN_STATUS_TARGET_LOST,
        Some(_) => MLN_STATUS_NATIVE_ERROR,
    }
}

/// `mln_render_session_get_capabilities()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_render_session_get_capabilities(
    session: mln_render_session,
    out_capabilities: *mut mln_render_session_capabilities,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, non_null(out_capabilities), {
        let capabilities = resolve(session)?.session.capabilities()?;
        let flag = |set: bool, bit: u32| if set { bit } else { 0 };
        let capabilities = mln_render_session_capabilities {
            size: size_of::<mln_render_session_capabilities>() as u32,
            driver: match capabilities.driver {
                Driver::CoreWorker => MLN_RENDER_DRIVER_CORE_WORKER,
                Driver::CallerThread => MLN_RENDER_DRIVER_CALLER_GRAPHICS_THREAD,
            },
            texture_ring_depth: capabilities.texture_ring_depth,
            flags: flag(capabilities.frame_acquisition, MLN_RENDER_SESSION_CAPABILITY_FRAME_ACQUISITION)
                | flag(capabilities.readback, MLN_RENDER_SESSION_CAPABILITY_READBACK)
                | flag(capabilities.presentation, MLN_RENDER_SESSION_CAPABILITY_PRESENTATION),
            // No frame leaves the session, so there is nothing to wait on.
            producer_sync_kind: MLN_GPU_SYNC_CPU_COMPLETE,
            consumer_sync_kinds: 0,
        };
        // SAFETY: the caller's contract.
        unsafe { abi::write_sized(out_capabilities, &capabilities) }
    })
}

/// `mln_render_session_get_snapshot()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_render_session_get_snapshot(
    session: mln_render_session,
    out_snapshot: *mut mln_render_session_snapshot,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, non_null(out_snapshot), {
        let object = resolve(session)?;
        let snapshot = object.session.snapshot()?;
        let snapshot = mln_render_session_snapshot {
            size: size_of::<mln_render_session_snapshot>() as u32,
            state: state(snapshot.state),
            extent: mln_logical_extent {
                width: snapshot.extent.size.width,
                height: snapshot.extent.size.height,
                scale_factor: object.scale_factor,
            },
            generation: snapshot.generation,
            frame_generation: snapshot.frame_generation,
            acquired_frame_count: 0,
            failure_status: failure_status(snapshot.failure.as_ref()),
            has_target: snapshot.has_target,
        };
        // SAFETY: the caller's contract.
        unsafe { abi::write_sized(out_snapshot, &snapshot) }
    })
}

/// `mln_render_session_get_failure()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_render_session_get_failure(
    session: mln_render_session,
    out_message: *mut c_char,
    message_capacity: usize,
    out_message_length: *mut usize,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        let snapshot = resolve(session)?.session.snapshot()?;
        let message = snapshot.failure.as_ref().map_or("", |error| error.message.as_str());
        let names = ("out_message", "out_message_length");
        // SAFETY: the caller's contract.
        unsafe { buffer::write_text(message, out_message, message_capacity, out_message_length, names) }
    })
}

// MARK: - Frames

/// `mln_render_session_demand_frame()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_render_session_demand_frame(
    session: mln_render_session,
    demand: *const mln_frame_demand,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let demand = unsafe { abi::read_sized(demand) }?;
        let flags = Fields::new(
            demand.flags,
            MLN_FRAME_DEMAND_FORCE | MLN_FRAME_DEMAND_WAIT_FOR_UPDATE,
            "mln_frame_demand.flags",
        )?;
        let mode = match (flags.get(MLN_FRAME_DEMAND_FORCE, ()), flags.get(MLN_FRAME_DEMAND_WAIT_FOR_UPDATE, ())) {
            (None, None) => DemandMode::IfChanged,
            (Some(()), None) => DemandMode::Force,
            (None, Some(())) => DemandMode::WaitForUpdate,
            (Some(()), Some(())) => {
                return Err(Error::invalid_argument("a frame demand that waits for a map update cannot force"));
            }
        };
        let object = resolve(session)?;
        object.session.demand_frame(FrameDemand::new(mode, demand.token))?;
        Ok(())
    })
}

/// `mln_render_session_set_frame_pacer()`: only the null pacer, since this
/// library creates no pacers.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_render_session_set_frame_pacer(
    session: mln_render_session,
    pacer: mln_frame_pacer,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        if pacer != handles::NULL {
            return Err(Error::invalid_argument(format!(
                "mln_frame_pacer handle {pacer:#x} was never created by this process"
            )));
        }
        let object = resolve(session)?;
        let snapshot = object.session.snapshot()?;
        match snapshot.state {
            RenderSessionState::Attaching | RenderSessionState::Attached => {}
            RenderSessionState::Failed => {
                let failure = snapshot.failure.unwrap_or_else(|| mln_util::Error::native("the render session failed"));
                return Err(failure.into());
            }
            _ => return Err(Error::invalid_state("the render session is not attached: it was detached or abandoned")),
        }
        if object.has_redraw_wake {
            return Err(Error::invalid_state("a render session with a redraw wake takes no pacer"));
        }
        Ok(())
    })
}

fn disposition(disposition: FrameDisposition) -> u32 {
    match disposition {
        FrameDisposition::Rendered => MLN_RENDER_RESULT_RENDERED,
        FrameDisposition::NoUpdate => MLN_RENDER_RESULT_NO_UPDATE,
        FrameDisposition::TargetNotReady => MLN_RENDER_RESULT_TARGET_NOT_READY,
        FrameDisposition::Superseded => MLN_RENDER_RESULT_SUPERSEDED,
        FrameDisposition::Failed => MLN_RENDER_RESULT_FAILED,
    }
}

fn frame_result(result: &FrameResult, scale_factor: f64) -> mln_render_frame_result {
    let view = match &result.view {
        Some(view) => {
            let mut view = projection::from_map_view(view);
            view.extent.scale_factor = scale_factor;
            view
        }
        // SAFETY: zeros are a valid `mln_map_view`, of no size, as a result
        // that rendered nothing has.
        None => unsafe { core::mem::zeroed::<mln_map_view>() },
    };
    mln_render_frame_result {
        size: size_of::<mln_render_frame_result>() as u32,
        disposition: disposition(result.disposition),
        token: result.token,
        map_update_generation: result.map_update_generation,
        generation: result.generation,
        frame_generation: result.frame_generation,
        needs_repaint: result.needs_repaint,
        view,
        mode: events::render_mode(result.mode),
    }
}

/// `mln_render_session_drain_frame_results()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_render_session_drain_frame_results(
    session: mln_render_session,
    out_batch: *mut mln_render_frame_batch,
    out_view: *mut mln_render_frame_batch_view,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, non_null(out_view), {
        // SAFETY: the caller's contract.
        let out_batch = unsafe { handles::out_handle(out_batch, "out_batch") }?;
        let object = resolve(session)?;
        let mut view = mln_render_frame_batch_view {
            size: size_of::<mln_render_frame_batch_view>() as u32,
            result_size: size_of::<mln_render_frame_result>() as u32,
            results: core::ptr::null(),
            result_count: 0,
        };
        // A view too small fails before the drain takes the results.
        // SAFETY: the caller's contract.
        unsafe { abi::write_sized(out_view, &view) }?;
        let results = object.session.drain_frame_results()?;
        if results.is_empty() {
            return Ok(());
        }
        let results: Box<[_]> = results.iter().map(|result| frame_result(result, object.scale_factor)).collect();
        let batch = Arc::new(FrameBatch(results));
        view.results = batch.0.as_ptr();
        view.result_count = batch.0.len();
        *out_batch = BATCHES.insert(batch)?;
        // SAFETY: the caller's contract.
        unsafe { abi::write_sized(out_view, &view) }
    })
}

/// `mln_render_frame_batch_release()`.
#[unsafe(no_mangle)]
pub extern "C" fn mln_render_frame_batch_release(batch: mln_render_frame_batch) {
    if batch != handles::NULL {
        let _ = BATCHES.remove(batch);
    }
}

// MARK: - Work

/// Sends a session operation, whose completion `value` gives the result of,
/// with the generations of the map's latest snapshot.
///
/// # Safety
///
/// `completion` is null or valid for the header's use of it.
pub(crate) unsafe fn operation<T: Send + 'static, V: CompletionValue>(
    session: mln_render_session,
    completion: *const mln_completion,
    send: impl FnOnce(&RenderSession) -> Result<::mln_runtime::Completion<T>, mln_util::Error>,
    value: impl FnOnce(T) -> V + Send + 'static,
) -> Result<(), Error> {
    let object = resolve(session)?;
    // SAFETY: the caller's contract.
    let pending = unsafe { PendingCompletion::new(completion) }?;
    let sent = send(&object.session)?;
    let map = object.map.clone();
    completion::forward(sent, Some(pending.accept()), move |result| {
        let generations = map.snapshot().map_or(Generations::default(), |snapshot| Generations {
            snapshot: snapshot.generation,
            render_update: snapshot.update_generation,
        });
        (result.map(value).map_err(Error::from), generations)
    });
    Ok(())
}

/// `mln_render_session_resize()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_render_session_resize(
    session: mln_render_session,
    logical_size: mln_logical_size,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        if logical_size.width == 0 || logical_size.height == 0 {
            return Err(Error::invalid_argument("render target dimensions must be positive"));
        }
        let object = resolve(session)?;
        render_target::physical_size(mln_logical_extent {
            width: logical_size.width,
            height: logical_size.height,
            scale_factor: object.scale_factor,
        })?;
        // SAFETY: the caller's contract.
        let pending = unsafe { PendingCompletion::optional(completion) }?;
        let resized = object.session.resize(Size::new(logical_size.width, logical_size.height))?;
        let map = object.map.clone();
        completion::forward(resized, pending.map(PendingCompletion::accept), move |result| {
            let generations = map::generations(&map, &result);
            (result.map(drop).map_err(Error::from), generations)
        });
        Ok(())
    })
}

/// `mln_render_session_release_target()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_render_session_release_target(
    session: mln_render_session,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        unsafe { operation(session, completion, RenderSession::release_target, |()| ()) }
    })
}

/// `mln_render_session_barrier()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_render_session_barrier(
    session: mln_render_session,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        unsafe { operation(session, completion, RenderSession::barrier, |()| ()) }
    })
}

/// `mln_render_session_reduce_memory_use()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_render_session_reduce_memory_use(
    session: mln_render_session,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        unsafe { operation(session, completion, RenderSession::reduce_memory_use, |()| ()) }
    })
}

/// `mln_render_session_clear_data()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_render_session_clear_data(
    session: mln_render_session,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        unsafe { operation(session, completion, RenderSession::clear_data, |()| ()) }
    })
}

/// `mln_render_session_dump_debug_logs()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_render_session_dump_debug_logs(
    session: mln_render_session,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        unsafe { operation(session, completion, RenderSession::dump_debug_logs, |()| ()) }
    })
}

/// `mln_render_session_service_driver_work()`: every session that this
/// library attaches renders on a core worker, which needs no service.
///
/// # Safety
///
/// `out_diagnostic` is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_render_session_service_driver_work(
    session: mln_render_session,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        resolve(session)?;
        Err(Error::invalid_state("render session is driven by its own core worker"))
    })
}

// MARK: - Lifecycle

/// `mln_render_session_detach()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_render_session_detach(
    session: mln_render_session,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        let object = resolve(session)?;
        // SAFETY: the caller's contract.
        let pending = unsafe { PendingCompletion::new(completion) }?;
        let detached = object.session.detach()?;
        completion::forward(detached, Some(pending.accept()), |result| {
            (result.map_err(Error::from), Generations::default())
        });
        Ok(())
    })
}

/// `mln_render_session_abandon()`: this library's sessions keep no graphics
/// objects past an abandon.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_render_session_abandon(
    session: mln_render_session,
    out_result: *mut mln_render_abandon_result,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, non_null(out_result), {
        let result = mln_render_abandon_result {
            size: size_of::<mln_render_abandon_result>() as u32,
            disposition: MLN_RENDER_ABANDON_DISPOSITION_CLEAN,
            quarantined_resource_count: 0,
            reserved: 0,
        };
        // Checked before the abandon, which can't be undone.
        // SAFETY: the caller's contract.
        unsafe { abi::write_sized(out_result, &result) }?;
        resolve(session)?.session.abandon()?;
        Ok(())
    })
}

/// `mln_render_session_release()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_render_session_release(
    session: mln_render_session,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        resolve(session)?;
        // SAFETY: the caller's contract.
        let pending = unsafe { PendingCompletion::new(completion) }?;
        let object = SESSIONS.remove(session)?;
        let released = object.session.release()?;
        completion::forward(released, Some(pending.accept()), |result| {
            (result.map_err(Error::from), Generations::default())
        });
        Ok(())
    })
}

/// `mln_render_session_dispose()`.
///
/// # Safety
///
/// `out_diagnostic` is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_render_session_dispose(
    session: mln_render_session,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        SESSIONS.remove(session)?.session.dispose()?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use core::ffi::c_void;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::diagnostic::tests::{diagnostic, message};
    use crate::generated::types::{
        MLN_STATUS_INVALID_ARGUMENT, MLN_STATUS_INVALID_STATE, MLN_STATUS_UNSUPPORTED, mln_completion_result,
        mln_offscreen_descriptor, mln_queue_lock, mln_texture_readback_result, mln_wake,
    };
    use crate::map::mln_map_set_style_json;
    use crate::map::tests::{create_map, options, release_map, view};
    use crate::offscreen::mln_map_attach_offscreen;
    use crate::runtime::tests::{Waiter, create, release};
    use crate::texture::mln_render_session_read_texture;

    const NO_DIAGNOSTIC: *mut mln_diagnostic = core::ptr::null_mut();

    /// A header struct of zeros but its `size`.
    fn sized<T: abi::Versioned>() -> T {
        // SAFETY: zeros are a valid `T` (the trait's contract).
        let mut value: T = unsafe { core::mem::zeroed() };
        // SAFETY: `size` is the first field (the trait's contract).
        unsafe { (&raw mut value).cast::<u32>().write(size_of::<T>() as u32) };
        value
    }

    fn attach_options() -> mln_render_session_attach_options {
        mln_render_session_attach_options { driver: MLN_RENDER_DRIVER_CORE_WORKER, ..sized() }
    }

    fn snapshot(session: mln_render_session) -> Result<mln_render_session_snapshot, mln_status> {
        let mut snapshot = sized::<mln_render_session_snapshot>();
        // SAFETY: a local snapshot.
        match unsafe { mln_render_session_get_snapshot(session, &mut snapshot, NO_DIAGNOSTIC) } {
            MLN_STATUS_OK => Ok(snapshot),
            status => Err(status),
        }
    }

    /// Drains until a result arrives.
    fn wait_for_result(session: mln_render_session) -> mln_render_frame_result {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let mut batch = 0;
            let mut view = sized::<mln_render_frame_batch_view>();
            // SAFETY: local outputs.
            let status =
                unsafe { mln_render_session_drain_frame_results(session, &mut batch, &mut view, NO_DIAGNOSTIC) };
            assert_eq!(status, MLN_STATUS_OK);
            if batch != handles::NULL {
                assert_eq!((view.result_count, view.result_size as usize), (1, size_of::<mln_render_frame_result>()));
                // SAFETY: the batch's one result, live until its release.
                let result = unsafe { *view.results };
                mln_render_frame_batch_release(batch);
                return result;
            }
            assert!(view.results.is_null());
            assert!(Instant::now() < deadline, "no frame result arrived");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    type Pixel = (mln_status, Option<(u32, u32, [u8; 4])>);

    unsafe extern "C" fn send_pixel(user_data: *mut c_void, result: *const mln_completion_result) {
        // SAFETY: the sender that `read_pixel` passed, and the library's
        // result, with one readback when it succeeded.
        unsafe {
            let sender = &*user_data.cast::<mpsc::Sender<Pixel>>();
            let result = &*result;
            let pixel = (result.status == MLN_STATUS_OK).then(|| {
                let readback = *result.value.cast::<mln_texture_readback_result>();
                let data = core::slice::from_raw_parts(readback.data.data.cast::<u8>(), 4);
                (readback.info.width, readback.info.height, [data[0], data[1], data[2], data[3]])
            });
            let _ = sender.send((result.status, pixel));
        }
    }

    /// The status of a readback, and the size and first pixel that it read.
    fn read_pixel(session: mln_render_session) -> Pixel {
        let (sender, receiver) = mpsc::channel::<Pixel>();
        let completion = mln_completion {
            size: size_of::<mln_completion>() as u32,
            callback: Some(send_pixel),
            user_data: core::ptr::from_ref(&sender).cast_mut().cast(),
            release_user_data: None,
            queue: 0,
        };
        // SAFETY: local arguments, which outlive the completion's call.
        assert_eq!(unsafe { mln_render_session_read_texture(session, &completion, NO_DIAGNOSTIC) }, MLN_STATUS_OK);
        receiver.recv_timeout(Duration::from_secs(30)).unwrap()
    }

    fn demand(session: mln_render_session, flags: u32, token: u64) -> mln_status {
        let demand = mln_frame_demand { flags, token, ..sized() };
        // SAFETY: a local demand.
        unsafe { mln_render_session_demand_frame(session, &demand, NO_DIAGNOSTIC) }
    }

    #[test]
    fn an_offscreen_session_renders_reads_back_and_releases() {
        let runtime = create();
        let map = create_map(runtime, &options(1.0));
        let styled = Waiter::new();
        let style = r#"{"version":8,"sources":{},"layers":[{"id":"bg","type":"background","paint":{"background-color":"red"}}]}"#;
        // SAFETY: local arguments.
        let status = unsafe { mln_map_set_style_json(map, view(style), &styled.descriptor, NO_DIAGNOSTIC) };
        assert_eq!((status, styled.status()), (MLN_STATUS_OK, MLN_STATUS_OK));

        let descriptor =
            mln_offscreen_descriptor { logical_size: mln_logical_size { width: 64, height: 32 }, ..sized() };
        let mut session = 0;
        let attached = Waiter::new();
        // SAFETY: local arguments.
        let status = unsafe {
            let options = attach_options();
            mln_map_attach_offscreen(map, &descriptor, &options, 0, &mut session, &attached.descriptor, NO_DIAGNOSTIC)
        };
        assert_eq!((status, attached.status()), (MLN_STATUS_OK, MLN_STATUS_OK));
        let attached = snapshot(session).unwrap();
        assert_eq!(attached.state, MLN_RENDER_SESSION_STATE_ATTACHED);
        assert_eq!((attached.extent.width, attached.extent.height, attached.failure_status), (64, 32, MLN_STATUS_OK));
        assert!(attached.has_target);
        let mut capabilities = sized::<mln_render_session_capabilities>();
        // SAFETY: local capabilities.
        let status = unsafe { mln_render_session_get_capabilities(session, &mut capabilities, NO_DIAGNOSTIC) };
        assert_eq!((status, capabilities.flags), (MLN_STATUS_OK, MLN_RENDER_SESSION_CAPABILITY_READBACK));

        assert_eq!(read_pixel(session).0, MLN_STATUS_INVALID_STATE);
        let both = MLN_FRAME_DEMAND_FORCE | MLN_FRAME_DEMAND_WAIT_FOR_UPDATE;
        assert_eq!(demand(session, both, 1), MLN_STATUS_INVALID_ARGUMENT);
        assert_eq!(demand(session, 0, u64::MAX), MLN_STATUS_INVALID_ARGUMENT);
        assert_eq!(demand(session, MLN_FRAME_DEMAND_FORCE, 7), MLN_STATUS_OK);
        let result = wait_for_result(session);
        assert_eq!((result.token, result.disposition), (7, MLN_RENDER_RESULT_RENDERED));
        assert_eq!((result.view.extent.width, result.view.extent.scale_factor), (64, 1.0));
        assert_eq!(read_pixel(session), (MLN_STATUS_OK, Some((64, 32, [255, 0, 0, 255]))));
        let mut out = diagnostic();
        // SAFETY: a local diagnostic.
        let serviced = unsafe { mln_render_session_service_driver_work(session, &mut *out) };
        assert_eq!(serviced, MLN_STATUS_INVALID_STATE);
        assert!(message(&out).contains("core worker"), "{}", message(&out));

        let released = Waiter::new();
        // SAFETY: a local completion.
        assert_eq!(unsafe { mln_render_session_release(session, &released.descriptor, NO_DIAGNOSTIC) }, MLN_STATUS_OK);
        assert_eq!(released.status(), MLN_STATUS_OK);
        assert_eq!(snapshot(session).unwrap_err(), MLN_STATUS_INVALID_STATE);
        release_map(map);
        release(runtime);
    }

    #[test]
    fn attach_options_check_their_driver_sync_kinds_and_queue_lock() {
        unsafe extern "C" fn lock(_: *mut c_void) {}
        // SAFETY: each pointer is to a local.
        let check = |options: mln_render_session_attach_options| unsafe { super::attach_options(&options) };
        assert!(check(attach_options()).is_ok());
        let driver = check(mln_render_session_attach_options { driver: 9, ..attach_options() });
        assert!(driver.err().unwrap().message().contains("driver"));
        let kinds = check(mln_render_session_attach_options { producer_sync_kinds: 1 << 20, ..attach_options() });
        assert_eq!(kinds.err().unwrap().status(), MLN_STATUS_INVALID_ARGUMENT);
        let half = mln_queue_lock {
            lock: Some(lock),
            unlock: None,
            user_data: core::ptr::null_mut(),
            release_user_data: None,
        };
        let half_locked = check(mln_render_session_attach_options { queue_lock: half, ..attach_options() });
        assert!(half_locked.err().unwrap().message().contains("queue lock"));
        let queued = mln_wake { callback: None, user_data: core::ptr::null_mut(), release_user_data: None, queue: 5 };
        let wake = check(mln_render_session_attach_options { frame_wake: queued, ..attach_options() });
        assert_eq!(wake.err().unwrap().status(), MLN_STATUS_INVALID_ARGUMENT);
        // SAFETY: null.
        assert!(unsafe { super::attach_options(core::ptr::null()) }.is_err());

        let locked = check(mln_render_session_attach_options {
            queue_lock: mln_queue_lock { unlock: Some(lock), ..half },
            ..attach_options()
        });
        let refused = locked.ok().unwrap().refuse_queue_lock("no lock").unwrap_err();
        assert_eq!(refused.status(), MLN_STATUS_UNSUPPORTED);
    }

    #[test]
    fn a_result_that_rendered_nothing_has_no_view() {
        let result = FrameResult {
            disposition: FrameDisposition::Superseded,
            token: 3,
            map_update_generation: 4,
            generation: 0,
            frame_generation: 0,
            needs_repaint: false,
            view: None,
            mode: ::mln_map::RenderMode::Partial,
        };
        let converted = frame_result(&result, 2.0);
        let fields = (converted.disposition, converted.token, converted.map_update_generation);
        assert_eq!(fields, (MLN_RENDER_RESULT_SUPERSEDED, 3, 4));
        assert_eq!(converted.view.size, 0);
    }
}
