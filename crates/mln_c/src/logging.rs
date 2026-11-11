//! `logging.h`: the process's log records, sent to a host's callback.
//!
//! The core's log hands each record to one observer. This module installs one
//! that forwards records to the host's handler: on the thread that logged
//! them, or, for the severities of the async mask, on a log thread of its own,
//! in the order they were logged.

use core::cell::Cell;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, Once, RwLock, mpsc};

use mln_util::logging::{Event, EventSeverity, Log, Observer};

use crate::abi::{self, Fields};
use crate::buffer::LentText;
use crate::callback::Registration;
use crate::error::Error;
use crate::export::ForbidReentry;
use crate::generated::types::{
    MLN_LOG_SEVERITY_ERROR, MLN_LOG_SEVERITY_FLAG_ALL, MLN_LOG_SEVERITY_FLAG_DEFAULT, MLN_LOG_SEVERITY_INFO,
    MLN_LOG_SEVERITY_WARNING, mln_buffer_view, mln_diagnostic, mln_log_event, mln_log_handler, mln_log_severity,
    mln_log_severity_flags, mln_status,
};
use crate::{THREAD_STACK_SIZE, lock};

abi::versioned!(mln_log_handler);

/// A log callback can't call the library: the record's thread may hold the
/// core's log lock and this module's.
const REENTRY: &str = "the C API cannot be called from a log callback, wake, or release";

type Callback = unsafe extern "C" fn(*mut core::ffi::c_void, mln_log_severity, mln_log_event, i64, mln_buffer_view);

struct Handler {
    callback: Callback,
    registration: Registration,
    platform_severities: mln_log_severity_flags,
}

/// The host's handler. Calls hold it for reading, so a replacement waits for
/// the calls that are running and no call runs after it returns.
static HANDLER: RwLock<Option<Handler>> = RwLock::new(None);

/// The severities that go to the log thread.
static ASYNC_SEVERITIES: AtomicU32 = AtomicU32::new(MLN_LOG_SEVERITY_FLAG_DEFAULT);

static INSTALL: Once = Once::new();

thread_local! {
    /// Whether this thread is in a log callback, where a record that the
    /// host's code logs goes to the platform's log instead.
    static IN_CALLBACK: Cell<bool> = const { Cell::new(false) };
}

/// One record, as the log thread carries it.
struct Record {
    severity: mln_log_severity,
    event: mln_log_event,
    code: i64,
    message: String,
}

fn severity_value(severity: EventSeverity) -> Option<mln_log_severity> {
    match severity {
        EventSeverity::Debug => None,
        EventSeverity::Info => Some(MLN_LOG_SEVERITY_INFO),
        EventSeverity::Warning => Some(MLN_LOG_SEVERITY_WARNING),
        EventSeverity::Error => Some(MLN_LOG_SEVERITY_ERROR),
    }
}

/// The severity's bit in a set of severities.
fn flag(severity: mln_log_severity) -> mln_log_severity_flags {
    1 << severity
}

/// Sends `record` to the host's handler, if there is one.
fn deliver(record: Record) {
    let handler = HANDLER.read().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(handler) = &*handler else { return };
    let callback = handler.callback;
    let _forbid = ForbidReentry::enter(REENTRY);
    let previous = IN_CALLBACK.replace(true);
    let message = LentText::new(&record.message);
    // A record that the handler's released queue refuses is dropped.
    let _ = handler.registration.deliver(move |user_data| {
        // SAFETY: the host's callback, with its user data and a message that
        // lives until it returns.
        unsafe { callback(user_data.get(), record.severity, record.event, record.code, message.view()) };
    });
    IN_CALLBACK.set(previous);
}

/// The log thread's queue, once it has started.
static LOG_THREAD: Mutex<Option<mpsc::Sender<Record>>> = Mutex::new(None);

/// Sends `record` to the log thread, or returns it when the thread can't
/// start.
fn deliver_later(record: Record) -> Result<(), Record> {
    let mut thread = lock(&LOG_THREAD);
    if thread.is_none() {
        let (sender, receiver) = mpsc::channel::<Record>();
        let started = std::thread::Builder::new()
            .name("mln-log".to_owned())
            .stack_size(THREAD_STACK_SIZE)
            .spawn(move || receiver.into_iter().for_each(deliver));
        if started.is_err() {
            return Err(record);
        }
        *thread = Some(sender);
    }
    match &*thread {
        Some(sender) => sender.send(record).map_err(|mpsc::SendError(record)| record),
        None => Err(record),
    }
}

/// The core log's observer.
struct Forwarder;

impl Observer for Forwarder {
    fn on_record(&self, severity: EventSeverity, event: Event, code: i64, message: &str) -> bool {
        let Some(severity) = severity_value(severity) else { return false };
        if IN_CALLBACK.get() {
            return false;
        }
        let platform = {
            let handler = HANDLER.read().unwrap_or_else(std::sync::PoisonError::into_inner);
            match &*handler {
                Some(handler) => handler.platform_severities & flag(severity) != 0,
                None => return false,
            }
        };
        // The headers' events are the core's, in its order.
        let record = Record { severity, event: event as mln_log_event, code, message: message.to_owned() };
        if ASYNC_SEVERITIES.load(Ordering::Acquire) & flag(severity) != 0 {
            if let Err(record) = deliver_later(record) {
                deliver(record);
            }
        } else {
            deliver(record);
        }
        // The core sends a record that isn't handled to the platform's log.
        !platform
    }
}

/// Replaces the handler, and releases the one it replaces as a log callback's
/// release runs: unable to call the library.
fn replace(handler: Option<Handler>) {
    let previous =
        core::mem::replace(&mut *HANDLER.write().unwrap_or_else(std::sync::PoisonError::into_inner), handler);
    let _forbid = ForbidReentry::enter(REENTRY);
    drop(previous);
}

/// `mln_log_set_callback()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_log_set_callback(
    handler: *const mln_log_handler,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        if handler.is_null() {
            return Err(Error::invalid_argument("log handler must not be null"));
        }
        // SAFETY: the caller's contract.
        let handler = unsafe { abi::read_sized(handler) }?;
        Fields::new(handler.platform_severities, MLN_LOG_SEVERITY_FLAG_ALL, "log handler platform_severities")?;
        let Some(callback) = handler.callback else {
            return Err(Error::invalid_argument("log handler requires a callback"));
        };
        let mut registration =
            Registration::new("log handler", true, handler.user_data, handler.release_user_data, handler.queue)?;
        registration.accept();
        INSTALL.call_once(|| Log::set_observer(Box::new(Forwarder)));
        replace(Some(Handler { callback, registration, platform_severities: handler.platform_severities }));
        Ok(())
    })
}

/// `mln_log_clear_callback()`.
///
/// # Safety
///
/// `out_diagnostic` is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_log_clear_callback(out_diagnostic: *mut mln_diagnostic) -> mln_status {
    export!(out_diagnostic, {
        replace(None);
        Ok(())
    })
}

/// `mln_log_set_async_severity_mask()`.
///
/// # Safety
///
/// `out_diagnostic` is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_log_set_async_severity_mask(
    mask: mln_log_severity_flags,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        Fields::new(mask, MLN_LOG_SEVERITY_FLAG_ALL, "log async severity mask")?;
        ASYNC_SEVERITIES.store(mask, Ordering::Release);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use core::ffi::c_void;
    use std::sync::{Arc, MutexGuard};
    use std::thread::ThreadId;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::delivery::tests::queue;
    use crate::delivery::{mln_delivery_queue_dispatch, mln_delivery_queue_release};
    use crate::diagnostic::tests::{diagnostic, message};
    use crate::generated::types::{
        MLN_LOG_EVENT_GENERAL, MLN_LOG_EVENT_TIMING, MLN_LOG_SEVERITY_FLAG_ERROR, MLN_LOG_SEVERITY_FLAG_INFO,
        MLN_LOG_SEVERITY_FLAG_WARNING, MLN_STATUS_INVALID_ARGUMENT, MLN_STATUS_INVALID_STATE, MLN_STATUS_OK,
        mln_delivery_queue,
    };

    const NULL_DIAGNOSTIC: *mut mln_diagnostic = core::ptr::null_mut();

    /// The log state is the process's, so its tests take turns, and each
    /// restores the defaults when it ends.
    struct LogState(#[allow(dead_code, reason = "held for its lifetime")] MutexGuard<'static, ()>);

    impl LogState {
        fn take() -> Self {
            static TURN: Mutex<()> = Mutex::new(());
            Self(lock(&TURN))
        }
    }

    impl Drop for LogState {
        fn drop(&mut self) {
            // SAFETY: null diagnostics.
            unsafe {
                mln_log_clear_callback(NULL_DIAGNOSTIC);
                mln_log_set_async_severity_mask(MLN_LOG_SEVERITY_FLAG_DEFAULT, NULL_DIAGNOSTIC);
            }
        }
    }

    #[derive(Clone, Debug, PartialEq)]
    struct Seen {
        severity: mln_log_severity,
        event: mln_log_event,
        code: i64,
        message: String,
        thread: ThreadId,
    }

    /// What a handler saw: the records with this test's marker, the releases,
    /// and the status of a call back into the library, if it made one.
    #[derive(Default)]
    struct Probe {
        marker: String,
        records: Mutex<Vec<Seen>>,
        releases: AtomicU32,
        reentry: Mutex<Option<(mln_status, String)>>,
        call_back: bool,
    }

    impl Probe {
        fn new(marker: &str, call_back: bool) -> Arc<Self> {
            Arc::new(Self { marker: marker.to_owned(), call_back, ..Self::default() })
        }

        fn records(&self) -> Vec<Seen> {
            lock(&self.records).clone()
        }

        fn wait_for(&self, count: usize) -> Vec<Seen> {
            let deadline = Instant::now() + Duration::from_secs(10);
            while self.records().len() < count && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            self.records()
        }

        fn clear_from_callback() -> (mln_status, String) {
            let mut out = diagnostic();
            // SAFETY: a local diagnostic.
            let status = unsafe { mln_log_clear_callback(&mut *out) };
            (status, message(&out).to_owned())
        }
    }

    unsafe extern "C" fn record(
        user_data: *mut c_void,
        severity: mln_log_severity,
        event: mln_log_event,
        code: i64,
        message: mln_buffer_view,
    ) {
        // SAFETY: every test's user data is a `Probe` from `handler`, which
        // it leaks.
        let probe = unsafe { &*user_data.cast::<Probe>() };
        // SAFETY: the library's message lives until the callback returns.
        let bytes = unsafe { core::slice::from_raw_parts(message.data.cast::<u8>(), message.size) };
        let message = String::from_utf8_lossy(bytes).into_owned();
        if !message.contains(&probe.marker) {
            return;
        }
        if probe.call_back {
            *lock(&probe.reentry) = Some(Probe::clear_from_callback());
        }
        let thread = std::thread::current().id();
        lock(&probe.records).push(Seen { severity, event, code, message, thread });
    }

    unsafe extern "C" fn release(user_data: *mut c_void) {
        // SAFETY: as in `record`.
        let probe = unsafe { &*user_data.cast::<Probe>() };
        probe.releases.fetch_add(1, Ordering::SeqCst);
        if probe.call_back {
            *lock(&probe.reentry) = Some(Probe::clear_from_callback());
        }
    }

    fn handler(probe: &Arc<Probe>, queue: mln_delivery_queue) -> mln_log_handler {
        mln_log_handler {
            size: size_of::<mln_log_handler>() as u32,
            callback: Some(record),
            user_data: Arc::into_raw(Arc::clone(probe)).cast_mut().cast(),
            release_user_data: Some(release),
            queue,
            platform_severities: 0,
        }
    }

    fn set(handler: &mln_log_handler) -> mln_status {
        // SAFETY: a local handler and a null diagnostic.
        unsafe { mln_log_set_callback(handler, NULL_DIAGNOSTIC) }
    }

    fn set_async(mask: mln_log_severity_flags) -> mln_status {
        // SAFETY: a null diagnostic.
        unsafe { mln_log_set_async_severity_mask(mask, NULL_DIAGNOSTIC) }
    }

    fn clear() -> mln_status {
        // SAFETY: a null diagnostic.
        unsafe { mln_log_clear_callback(NULL_DIAGNOSTIC) }
    }

    #[test]
    fn a_handler_releases_its_user_data_once_when_replaced_or_cleared() {
        let _state = LogState::take();
        let (first, second) = (Probe::new("release", false), Probe::new("release", false));
        assert_eq!(set(&handler(&first, 0)), MLN_STATUS_OK);
        assert_eq!(first.releases.load(Ordering::SeqCst), 0);
        assert_eq!(set(&handler(&second, 0)), MLN_STATUS_OK);
        assert_eq!(first.releases.load(Ordering::SeqCst), 1);
        assert_eq!(clear(), MLN_STATUS_OK);
        assert_eq!(clear(), MLN_STATUS_OK);
        assert_eq!(second.releases.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_rejected_handler_keeps_the_installed_one() {
        let _state = LogState::take();
        let (installed, rejected) = (Probe::new("kept", false), Probe::new("kept", false));
        assert_eq!(set(&handler(&installed, 0)), MLN_STATUS_OK);
        // SAFETY: a null handler and diagnostic.
        assert_eq!(unsafe { mln_log_set_callback(core::ptr::null(), NULL_DIAGNOSTIC) }, MLN_STATUS_INVALID_ARGUMENT);
        let small = mln_log_handler { size: size_of::<mln_log_handler>() as u32 - 1, ..handler(&rejected, 0) };
        let silent = mln_log_handler { callback: None, ..handler(&rejected, 0) };
        let unknown = mln_log_handler { platform_severities: 1, ..handler(&rejected, 0) };
        let past = mln_log_handler { platform_severities: 1 << 4, ..handler(&rejected, 0) };
        for rejected in [small, silent, unknown, past] {
            assert_eq!(set(&rejected), MLN_STATUS_INVALID_ARGUMENT);
        }
        Log::warning(Event::General, "kept");
        assert_eq!(installed.wait_for(1).len(), 1);
        assert_eq!(rejected.releases.load(Ordering::SeqCst), 0);
        assert_eq!(clear(), MLN_STATUS_OK);
        assert_eq!(installed.releases.load(Ordering::SeqCst), 1);
        assert_eq!(rejected.releases.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn the_async_mask_accepts_only_severity_bits() {
        let _state = LogState::take();
        assert_eq!(set_async(MLN_LOG_SEVERITY_FLAG_ALL), MLN_STATUS_OK);
        assert_eq!(set_async(0), MLN_STATUS_OK);
        for mask in [1, 1 << 4] {
            let mut out = diagnostic();
            // SAFETY: a local diagnostic.
            assert_eq!(unsafe { mln_log_set_async_severity_mask(mask, &mut *out) }, MLN_STATUS_INVALID_ARGUMENT);
            assert!(message(&out).contains("unknown bits"), "{}", message(&out));
        }
    }

    #[test]
    fn records_arrive_on_the_logging_thread_or_in_order_on_the_log_thread() {
        let _state = LogState::take();
        let probe = Probe::new("threads", false);
        assert_eq!(set_async(MLN_LOG_SEVERITY_FLAG_WARNING), MLN_STATUS_OK);
        assert_eq!(set(&handler(&probe, 0)), MLN_STATUS_OK);
        Log::info(Event::Timing, "threads: info");
        Log::warning(Event::General, "threads: first");
        Log::warning(Event::General, "threads: second");
        Log::error(Event::General, "threads: error");
        let records = probe.wait_for(4);
        let here = std::thread::current().id();
        let sync: Vec<_> = records.iter().filter(|seen| seen.thread == here).collect();
        assert_eq!(
            sync.iter().map(|seen| seen.severity).collect::<Vec<_>>(),
            [MLN_LOG_SEVERITY_INFO, MLN_LOG_SEVERITY_ERROR]
        );
        assert_eq!(
            (sync[0].event, sync[0].code, sync[0].message.as_str()),
            (MLN_LOG_EVENT_TIMING, -1, "threads: info")
        );
        let later: Vec<_> = records.iter().filter(|seen| seen.thread != here).collect();
        assert_eq!(
            later.iter().map(|seen| seen.message.as_str()).collect::<Vec<_>>(),
            ["threads: first", "threads: second"]
        );
        assert_eq!(later[0].thread, later[1].thread);
        assert_eq!((later[0].severity, later[0].event), (MLN_LOG_SEVERITY_WARNING, MLN_LOG_EVENT_GENERAL));
    }

    #[test]
    fn a_log_callback_and_its_release_cannot_call_back_into_the_library() {
        let _state = LogState::take();
        assert_eq!(set_async(0), MLN_STATUS_OK);
        let probe = Probe::new("reentry", true);
        assert_eq!(set(&handler(&probe, 0)), MLN_STATUS_OK);
        Log::warning(Event::General, "reentry");
        let (status, text) = lock(&probe.reentry).take().unwrap();
        assert_eq!(status, MLN_STATUS_INVALID_STATE);
        assert!(text.contains("log callback"), "{text}");
        // The callback's call changed nothing.
        Log::warning(Event::General, "reentry again");
        assert_eq!(probe.records().len(), 2);

        assert_eq!(clear(), MLN_STATUS_OK);
        assert_eq!(probe.releases.load(Ordering::SeqCst), 1);
        assert_eq!(lock(&probe.reentry).take().map(|(status, _)| status), Some(MLN_STATUS_INVALID_STATE));
    }

    #[test]
    fn a_cleared_handler_receives_no_more_records() {
        let _state = LogState::take();
        let probe = Probe::new("cleared", false);
        assert_eq!(set_async(MLN_LOG_SEVERITY_FLAG_ALL), MLN_STATUS_OK);
        assert_eq!(set(&handler(&probe, 0)), MLN_STATUS_OK);
        Log::error(Event::General, "cleared: before");
        assert_eq!(probe.wait_for(1).len(), 1);
        assert_eq!(clear(), MLN_STATUS_OK);
        Log::error(Event::General, "cleared: after");
        // The log thread delivers in order, so once another handler sees a
        // later record, the cleared one would have seen this one.
        let next = Probe::new("cleared", false);
        assert_eq!(set(&handler(&next, 0)), MLN_STATUS_OK);
        Log::error(Event::General, "cleared: next");
        assert_eq!(next.wait_for(1).len(), 1);
        assert_eq!(probe.records().len(), 1);
    }

    #[test]
    fn a_queued_handler_receives_records_in_dispatch() {
        let _state = LogState::take();
        assert_eq!(set_async(0), MLN_STATUS_OK);
        let queue = queue();
        let probe = Probe::new("queued", true);
        assert_eq!(set(&handler(&probe, queue)), MLN_STATUS_OK);
        Log::info(Event::General, "queued");
        assert!(probe.records().is_empty());
        // SAFETY: a null diagnostic.
        assert_eq!(unsafe { mln_delivery_queue_dispatch(queue, NULL_DIAGNOSTIC) }, MLN_STATUS_OK);
        assert_eq!(probe.records().len(), 1);
        // From the queue's dispatch, the callback may call the library.
        assert_eq!(lock(&probe.reentry).take().map(|(status, _)| status), Some(MLN_STATUS_OK));
        mln_delivery_queue_release(queue);
    }

    #[test]
    fn the_platform_severities_go_to_the_platform_log_too() {
        let _state = LogState::take();
        assert_eq!(set_async(0), MLN_STATUS_OK);
        let probe = Probe::new("platform", false);
        let platform = MLN_LOG_SEVERITY_FLAG_ERROR | MLN_LOG_SEVERITY_FLAG_INFO;
        assert_eq!(set(&mln_log_handler { platform_severities: platform, ..handler(&probe, 0) }), MLN_STATUS_OK);
        // The core prints the records that its observer didn't handle.
        let handled = |severity| Forwarder.on_record(severity, Event::General, -1, "platform");
        assert!(!handled(EventSeverity::Info));
        assert!(handled(EventSeverity::Warning));
        assert!(!handled(EventSeverity::Error));
        assert_eq!(probe.records().len(), 3);
        assert_eq!(clear(), MLN_STATUS_OK);
        assert!(!handled(EventSeverity::Warning));
    }
}
