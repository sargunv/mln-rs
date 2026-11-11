//! `runtime.h`: runtimes, their event handlers, and what applies to every
//! runtime in the process, such as the network status.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ::mln_runtime::{MapId, Runtime, RuntimeEvent, RuntimeOptions};
use mln_storage::network_status::{NetworkStatus, Status};

use crate::callback::Registration;
use crate::completion::{self, Generations, PendingCompletion};
use crate::error::Error;
use crate::events::EventBatch;
use crate::generated::types::{
    MLN_NETWORK_STATUS_OFFLINE, MLN_NETWORK_STATUS_ONLINE, mln_completion, mln_diagnostic, mln_network_status,
    mln_runtime, mln_runtime_event_handler, mln_runtime_options, mln_status,
};
use crate::handles::{self, HandleKind, HandleTable};
use crate::{abi, buffer, lock};

abi::versioned!(mln_runtime_options, mln_runtime_event_handler);

/// The handles of a runtime's maps, by the runtime layer's IDs, which its
/// events name. A map's entry goes once its release has delivered the map's
/// last events.
pub(crate) type MapHandles = Mutex<HashMap<MapId, u64>>;

pub(crate) struct RuntimeObject {
    pub(crate) runtime: Runtime,
    pub(crate) maps: Arc<MapHandles>,
}

static RUNTIMES: HandleTable<RuntimeObject> = HandleTable::new(HandleKind::Runtime);

/// The runtime that `runtime` names.
pub(crate) fn resolve(runtime: mln_runtime) -> Result<Arc<RuntimeObject>, Error> {
    RUNTIMES.get(runtime)
}

/// The path that `view` names, or `None` for an empty one.
///
/// # Safety
///
/// As for [`buffer::text`].
unsafe fn path(view: crate::generated::types::mln_buffer_view, name: &str) -> Result<Option<PathBuf>, Error> {
    // SAFETY: the caller's contract.
    let text = unsafe { buffer::text(view, name) }?;
    Ok((!text.is_empty()).then(|| PathBuf::from(text)))
}

/// `mln_runtime_create()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_runtime_create(
    options: *const mln_runtime_options,
    out_runtime: *mut mln_runtime,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        let options = if options.is_null() {
            RuntimeOptions::default()
        } else {
            // SAFETY: the caller's contract.
            let options = unsafe { abi::read_sized(options) }?;
            // SAFETY: the views of the caller's options.
            unsafe {
                RuntimeOptions {
                    asset_path: path(options.asset_path, "mln_runtime_options.asset_path")?,
                    cache_path: path(options.cache_path, "mln_runtime_options.cache_path")?,
                }
            }
        };
        // SAFETY: the caller's contract.
        let out_runtime = unsafe { handles::out_handle(out_runtime, "out_runtime") }?;
        let runtime = Runtime::new(options)?;
        *out_runtime =
            RUNTIMES.insert(Arc::new(RuntimeObject { runtime, maps: Arc::new(Mutex::new(HashMap::new())) }))?;
        Ok(())
    })
}

/// `mln_runtime_release()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_runtime_release(
    runtime: mln_runtime,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let pending = unsafe { PendingCompletion::new(completion) }?;
        let object = resolve(runtime)?;
        let released = object.runtime.release()?;
        // Another release can win the race to the table, and then reported
        // the runtime as released already.
        let _ = RUNTIMES.remove(runtime);
        completion::forward(released, Some(pending.accept()), |result| {
            (result.map_err(Error::from), Generations::default())
        });
        Ok(())
    })
}

/// `mln_runtime_dispose()`.
///
/// # Safety
///
/// `out_diagnostic` is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_runtime_dispose(runtime: mln_runtime, out_diagnostic: *mut mln_diagnostic) -> mln_status {
    export!(out_diagnostic, {
        let object = RUNTIMES.remove(runtime)?;
        object.runtime.dispose()?;
        Ok(())
    })
}

/// Sends a runtime command's result to its optional completion.
pub(crate) fn complete_command(
    command: Result<::mln_runtime::Completion<()>, mln_util::Error>,
    pending: Option<PendingCompletion>,
) -> Result<(), Error> {
    let command = command?;
    completion::forward(command, pending.map(PendingCompletion::accept), |result| {
        (result.map_err(Error::from), Generations::default())
    });
    Ok(())
}

/// `mln_runtime_set_event_handler()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_runtime_set_event_handler(
    runtime: mln_runtime,
    handler: *const mln_runtime_event_handler,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        let object = resolve(runtime)?;
        // SAFETY: the caller's contract.
        let handler = unsafe { abi::read_sized(handler) }?;
        let Some(callback) = handler.callback else {
            return Err(Error::invalid_argument("event handler requires a callback"));
        };
        let registration = Arc::new(Registration::new(
            "event handler",
            true,
            handler.user_data,
            handler.release_user_data,
            handler.queue,
        )?);
        // SAFETY: the caller's contract.
        let pending = unsafe { PendingCompletion::optional(completion) }?;
        let maps = Arc::clone(&object.maps);
        let delivery = Arc::clone(&registration);
        // The registration owns the user data from before the command goes
        // out, since the runtime may replace the handler, and drop it, before
        // this call returns.
        registration.accept();
        let command = object.runtime.set_event_handler(move |events: &[RuntimeEvent]| {
            let batch = {
                let maps = lock(&maps);
                EventBatch::new(events, runtime, |id| maps.get(&id).copied().unwrap_or(handles::NULL))
            };
            delivery.deliver(move |user_data| {
                batch.lend(|view| {
                    // SAFETY: the host's event callback, with its user data
                    // and a batch that stays valid until it returns.
                    unsafe { callback(user_data.get(), view) }
                });
            });
        });
        if command.is_err() {
            registration.revoke();
        }
        complete_command(command, pending)
    })
}

/// `mln_runtime_clear_event_handler()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_runtime_clear_event_handler(
    runtime: mln_runtime,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        let object = resolve(runtime)?;
        // SAFETY: the caller's contract.
        let pending = unsafe { PendingCompletion::optional(completion) }?;
        complete_command(object.runtime.clear_event_handler(), pending)
    })
}

/// `mln_network_get_status()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_network_get_status(
    out_status: *mut mln_network_status,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, non_null(out_status), {
        let status = match NetworkStatus::get() {
            Status::Online => MLN_NETWORK_STATUS_ONLINE,
            Status::Offline => MLN_NETWORK_STATUS_OFFLINE,
        };
        // SAFETY: non-null, and the caller's contract.
        unsafe { out_status.write(status) };
        Ok(())
    })
}

/// `mln_network_set_status()`.
///
/// # Safety
///
/// `out_diagnostic` is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_network_set_status(
    status: mln_network_status,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        NetworkStatus::set(match status {
            MLN_NETWORK_STATUS_ONLINE => Status::Online,
            MLN_NETWORK_STATUS_OFFLINE => Status::Offline,
            _ => return Err(Error::invalid_argument(format!("network status {status} names no status"))),
        });
        Ok(())
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use core::ffi::c_void;

    use super::*;
    use crate::delivery::tests::Log;
    use crate::diagnostic::tests::{diagnostic, message};
    use crate::generated::types::{
        MLN_STATUS_INVALID_ARGUMENT, MLN_STATUS_INVALID_STATE, MLN_STATUS_OK, mln_completion_result,
    };

    const NO_DIAGNOSTIC: *mut mln_diagnostic = core::ptr::null_mut();

    fn current() -> mln_network_status {
        let mut status = 0;
        // SAFETY: a local output and a null diagnostic.
        assert_eq!(unsafe { mln_network_get_status(&mut status, NO_DIAGNOSTIC) }, MLN_STATUS_OK);
        status
    }

    #[test]
    fn the_network_status_changes_and_rejects_a_value_that_names_no_status() {
        // SAFETY: local and null diagnostics, and a null output.
        unsafe {
            assert_eq!(mln_network_set_status(MLN_NETWORK_STATUS_OFFLINE, NO_DIAGNOSTIC), MLN_STATUS_OK);
            assert_eq!(current(), MLN_NETWORK_STATUS_OFFLINE);
            let mut out = diagnostic();
            assert_eq!(mln_network_set_status(0, &mut *out), MLN_STATUS_INVALID_ARGUMENT);
            assert!(message(&out).contains("network status"));
            assert_eq!(current(), MLN_NETWORK_STATUS_OFFLINE);
            assert_eq!(mln_network_get_status(core::ptr::null_mut(), &mut *out), MLN_STATUS_INVALID_ARGUMENT);
            assert!(message(&out).contains("out_status"));
            assert_eq!(mln_network_set_status(MLN_NETWORK_STATUS_ONLINE, NO_DIAGNOSTIC), MLN_STATUS_OK);
            assert_eq!(current(), MLN_NETWORK_STATUS_ONLINE);
        }
    }

    /// A completion whose status a channel receives.
    pub(crate) struct Waiter {
        receiver: std::sync::mpsc::Receiver<mln_status>,
        pub(crate) descriptor: mln_completion,
    }

    unsafe extern "C" fn send_status(user_data: *mut c_void, result: *const mln_completion_result) {
        // SAFETY: the sender that `Waiter::new` leaked, and the library's
        // result.
        let sender = unsafe { &*user_data.cast::<std::sync::mpsc::Sender<mln_status>>() };
        // SAFETY: as above.
        let _ = sender.send(unsafe { (*result).status });
    }

    unsafe extern "C" fn drop_sender(user_data: *mut c_void) {
        // SAFETY: the sender that `Waiter::new` leaked, released once.
        drop(unsafe { Box::from_raw(user_data.cast::<std::sync::mpsc::Sender<mln_status>>()) });
    }

    impl Waiter {
        pub(crate) fn new() -> Self {
            let (sender, receiver) = std::sync::mpsc::channel();
            let descriptor = mln_completion {
                size: size_of::<mln_completion>() as u32,
                callback: Some(send_status),
                user_data: Box::into_raw(Box::new(sender)).cast(),
                release_user_data: Some(drop_sender),
                queue: 0,
            };
            Waiter { receiver, descriptor }
        }

        pub(crate) fn status(self) -> mln_status {
            self.receiver.recv_timeout(std::time::Duration::from_secs(30)).expect("the completion ran")
        }
    }

    pub(crate) fn create() -> mln_runtime {
        let mut runtime = 0;
        // SAFETY: null options, a local out-handle and a null diagnostic.
        assert_eq!(unsafe { mln_runtime_create(core::ptr::null(), &mut runtime, NO_DIAGNOSTIC) }, MLN_STATUS_OK);
        runtime
    }

    pub(crate) fn release(runtime: mln_runtime) {
        let waiter = Waiter::new();
        // SAFETY: a local completion and a null diagnostic.
        assert_eq!(unsafe { mln_runtime_release(runtime, &waiter.descriptor, NO_DIAGNOSTIC) }, MLN_STATUS_OK);
        assert_eq!(waiter.status(), MLN_STATUS_OK);
    }

    #[test]
    fn a_runtime_is_created_and_released_once() {
        let runtime = create();
        release(runtime);
        let waiter = Waiter::new();
        let mut out = diagnostic();
        // SAFETY: local arguments.
        unsafe {
            assert_eq!(mln_runtime_release(runtime, &waiter.descriptor, &mut *out), MLN_STATUS_INVALID_STATE);
            assert!(message(&out).contains("stale"), "{}", message(&out));
            assert_eq!(mln_runtime_dispose(runtime, NO_DIAGNOSTIC), MLN_STATUS_INVALID_STATE);
            // The rejected release left the user data with the caller.
            drop_sender(waiter.descriptor.user_data);
        }
    }

    #[test]
    fn creation_checks_its_options_and_output() {
        let options = mln_runtime_options { size: 4, ..unsafe { core::mem::zeroed::<mln_runtime_options>() } };
        let mut runtime = 0;
        let mut out = diagnostic();
        // SAFETY: local arguments.
        unsafe {
            assert_eq!(mln_runtime_create(&options, &mut runtime, &mut *out), MLN_STATUS_INVALID_ARGUMENT);
            assert!(message(&out).contains("size"));
            let mut occupied = 7;
            assert_eq!(mln_runtime_create(core::ptr::null(), &mut occupied, &mut *out), MLN_STATUS_INVALID_ARGUMENT);
            assert_eq!(occupied, 7);
            assert_eq!(runtime, 0);
        }
    }

    unsafe extern "C" fn ignore_events(_: *mut c_void, _: *const crate::generated::types::mln_event_batch_view) {}

    #[test]
    fn a_handler_releases_its_user_data_once_it_is_replaced() {
        let runtime = create();
        let log = Log::new();
        let handler = mln_runtime_event_handler {
            size: size_of::<mln_runtime_event_handler>() as u32,
            callback: Some(ignore_events),
            user_data: log.pointer('h'),
            release_user_data: Some(Log::release),
            queue: 0,
        };
        let waiter = Waiter::new();
        // SAFETY: local arguments.
        unsafe {
            let rejected = mln_runtime_event_handler { callback: None, ..handler };
            assert_eq!(
                mln_runtime_set_event_handler(runtime, &rejected, core::ptr::null(), NO_DIAGNOSTIC),
                MLN_STATUS_INVALID_ARGUMENT
            );
            assert_eq!(
                mln_runtime_set_event_handler(runtime, &handler, core::ptr::null(), NO_DIAGNOSTIC),
                MLN_STATUS_OK
            );
            assert_eq!(mln_runtime_clear_event_handler(runtime, &waiter.descriptor, NO_DIAGNOSTIC), MLN_STATUS_OK);
        }
        assert_eq!(waiter.status(), MLN_STATUS_OK);
        assert_eq!(log.take(), "hr");
        release(runtime);
    }
}
