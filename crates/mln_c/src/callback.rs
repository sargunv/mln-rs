//! Callback registrations: the host's function pointers, the `user_data` they
//! take, and the `release_user_data` that frees it exactly once, after the
//! last call. A registration that names a delivery queue runs its calls and
//! its release from the queue's dispatch instead of on library threads.
//!
//! A registration is created unaccepted: dropping it then releases nothing,
//! because the function that took it failed and the caller still owns its
//! `user_data`. Accepting it hands that ownership to the library.

use core::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::delivery::{self, Call, Hold, Queue};
use crate::error::Error;
use crate::generated::types::{mln_delivery_queue, mln_user_data_release, mln_wake, mln_wake_callback};
use crate::handles;

/// A host's `user_data`, which the library passes back to the host's
/// callbacks on whichever thread runs them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct UserData(*mut c_void);

// SAFETY: the headers let a registration's callbacks run on any thread, so
// making `user_data` safe to use there is the host's contract; the library
// only carries the pointer and never reads through it.
unsafe impl Send for UserData {}
// SAFETY: as for `Send`.
unsafe impl Sync for UserData {}

impl UserData {
    pub(crate) fn new(pointer: *mut c_void) -> Self {
        Self(pointer)
    }

    /// The pointer. Closures that run elsewhere call this rather than reading
    /// the field, so that they capture the `Send` wrapper.
    pub(crate) fn get(self) -> *mut c_void {
        self.0
    }
}

/// Calls `release` with `user_data`.
fn call_release(release: unsafe extern "C" fn(*mut c_void), user_data: UserData) {
    // SAFETY: the host's release function, called once with the user data it
    // was registered with, as the headers promise.
    unsafe { release(user_data.get()) }
}

/// Frees an accepted registration's user data: now, or as the last call of
/// its queue, which reports it abandoned instead when it is released first.
fn retire(user_data: UserData, release: mln_user_data_release, queue: Option<&Arc<Queue>>, hold: Option<Hold>) {
    let Some(release) = release else { return };
    match queue {
        None => call_release(release, user_data),
        Some(queue) => {
            // A released queue refuses the call, and reported the user data
            // abandoned when it closed.
            let _ = queue.push_last(Call::new(move || call_release(release, user_data)), hold);
        }
    }
}

/// Resolves a registration's `queue` member: none for the null handle.
pub(crate) fn resolve_queue(queue: mln_delivery_queue) -> Result<Option<Arc<Queue>>, Error> {
    if queue == handles::NULL { Ok(None) } else { delivery::resolve(queue).map(Some) }
}

/// A registration whose calls take its user data, such as a resource
/// provider or an event handler.
pub(crate) struct Registration {
    user_data: UserData,
    release: mln_user_data_release,
    queue: Option<Arc<Queue>>,
    hold: Option<Hold>,
    accepted: AtomicBool,
}

impl Registration {
    /// Checks a registration's members: `name` names it in diagnostics, and
    /// `has_callback` says whether it sets the callback it needs.
    pub(crate) fn new(
        name: &str,
        has_callback: bool,
        user_data: *mut c_void,
        release: mln_user_data_release,
        queue: mln_delivery_queue,
    ) -> Result<Self, Error> {
        if !has_callback {
            return Err(Error::invalid_argument(format!("{name} requires a callback")));
        }
        let queue = resolve_queue(queue)?;
        let user_data = UserData::new(user_data);
        let hold = match (&queue, release) {
            (Some(queue), Some(_)) => queue.hold(user_data),
            _ => None,
        };
        Ok(Self { user_data, release, queue, hold, accepted: AtomicBool::new(false) })
    }

    /// Takes ownership of the user data: dropping the registration from here
    /// on releases it.
    pub(crate) fn accept(&self) {
        self.accepted.store(true, Ordering::Release);
    }

    /// Gives the user data back to the caller, for a call that fails after
    /// it accepted: dropping the registration then releases nothing.
    pub(crate) fn revoke(&self) {
        self.accepted.store(false, Ordering::Release);
    }

    /// Runs `call` with the user data: on this thread now, or from the queue's
    /// dispatch. Returns false when the released queue refuses it.
    pub(crate) fn deliver(&self, call: impl FnOnce(UserData) + Send + 'static) -> bool {
        let user_data = self.user_data;
        match &self.queue {
            None => {
                call(user_data);
                true
            }
            Some(queue) => queue.push(Call::new(move || call(user_data))).is_ok(),
        }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if *self.accepted.get_mut() {
            retire(self.user_data, self.release, self.queue.as_ref(), self.hold.take());
        }
    }
}

/// A host's `mln_wake`: tells the host that work is ready. One that names a
/// queue pushes a call there instead, with at most one call pending.
pub(crate) struct Wake {
    callback: mln_wake_callback,
    user_data: UserData,
    release: mln_user_data_release,
    queue: Option<Arc<Queue>>,
    hold: Option<Hold>,
    pending: Arc<AtomicBool>,
    accepted: AtomicBool,
    closed: AtomicBool,
}

impl Wake {
    /// Checks a host's wake. A wake without a callback is off, and then must
    /// neither retain user data nor name a queue.
    pub(crate) fn new(wake: &mln_wake) -> Result<Self, Error> {
        let queue = match wake.callback {
            None if wake.release_user_data.is_some() => {
                return Err(Error::invalid_argument("a disabled wake must not retain user data"));
            }
            None if wake.queue != handles::NULL => {
                return Err(Error::invalid_argument("a disabled wake must not name a queue"));
            }
            None => None,
            Some(_) => resolve_queue(wake.queue)?,
        };
        let user_data = UserData::new(wake.user_data);
        let hold = match (&queue, wake.release_user_data) {
            (Some(queue), Some(_)) => queue.hold(user_data),
            _ => None,
        };
        Ok(Self {
            callback: wake.callback,
            user_data,
            release: wake.release_user_data,
            queue,
            hold,
            pending: Arc::new(AtomicBool::new(false)),
            accepted: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        })
    }

    /// Takes ownership of the user data, and turns the wake on.
    pub(crate) fn accept(&self) {
        self.accepted.store(true, Ordering::Release);
    }

    /// Turns the wake off for good. A call already running finishes.
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    pub(crate) fn notify(&self) {
        if !self.accepted.load(Ordering::Acquire) || self.closed.load(Ordering::Acquire) {
            return;
        }
        let Some(callback) = self.callback else { return };
        let user_data = self.user_data;
        let Some(queue) = &self.queue else {
            // SAFETY: the host's wake callback, with its user data.
            unsafe { callback(user_data.get()) };
            return;
        };
        if self.pending.swap(true, Ordering::AcqRel) {
            return;
        }
        let pending = Arc::clone(&self.pending);
        let call = Call::new(move || {
            // A notify from inside the callback queues the next call.
            pending.store(false, Ordering::Release);
            // SAFETY: as above.
            unsafe { callback(user_data.get()) };
        });
        if queue.push(call).is_err() {
            self.pending.store(false, Ordering::Release);
        }
    }
}

impl Drop for Wake {
    fn drop(&mut self) {
        if *self.accepted.get_mut() {
            retire(self.user_data, self.release, self.queue.as_ref(), self.hold.take());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delivery::tests::{Log, queue, queue_with_abandon};
    use crate::delivery::{mln_delivery_queue_dispatch, mln_delivery_queue_release};
    use crate::generated::types::MLN_STATUS_OK;

    fn dispatch(queue: mln_delivery_queue) {
        // SAFETY: a null diagnostic.
        assert_eq!(unsafe { mln_delivery_queue_dispatch(queue, core::ptr::null_mut()) }, MLN_STATUS_OK);
    }

    #[test]
    fn a_direct_registration_calls_now_and_releases_once_on_drop() {
        let log = Log::new();
        let registration =
            Registration::new("handler", true, log.pointer('h'), Some(Log::release), handles::NULL).unwrap();
        registration.accept();
        assert!(registration.deliver(|user_data| Log::record(user_data, 'c')));
        assert_eq!(log.take(), "hc");
        drop(registration);
        assert_eq!(log.take(), "hr");
    }

    #[test]
    fn a_rejected_registration_keeps_its_user_data_with_the_caller() {
        let log = Log::new();
        let (queue, _) = queue_with_abandon(Some(Log::abandon));
        let registration = Registration::new("handler", true, log.pointer('h'), Some(Log::release), queue).unwrap();
        drop(registration);
        mln_delivery_queue_release(queue);
        assert_eq!(log.take(), "");
    }

    #[test]
    fn registrations_check_their_callback_and_queue() {
        let missing = Registration::new("provider", false, core::ptr::null_mut(), None, handles::NULL);
        assert_eq!(missing.err().unwrap().message(), "provider requires a callback");
        let stale = queue();
        mln_delivery_queue_release(stale);
        assert!(Registration::new("provider", true, core::ptr::null_mut(), None, stale).is_err());
    }

    #[test]
    fn a_queued_registration_runs_its_calls_then_its_release_in_dispatch() {
        let log = Log::new();
        let queue = queue();
        let registration = Registration::new("handler", true, log.pointer('h'), Some(Log::release), queue).unwrap();
        registration.accept();
        assert!(registration.deliver(|user_data| Log::record(user_data, 'c')));
        drop(registration);
        assert_eq!(log.take(), "");
        dispatch(queue);
        assert_eq!(log.take(), "hchr");
        mln_delivery_queue_release(queue);
    }

    #[test]
    fn a_released_queue_reports_a_held_registration_once() {
        let log = Log::new();
        let (queue, _) = queue_with_abandon(Some(Log::abandon));
        let registration = Registration::new("handler", true, log.pointer('h'), Some(Log::release), queue).unwrap();
        registration.accept();
        mln_delivery_queue_release(queue);
        assert_eq!(log.take(), "ha");
        assert!(!registration.deliver(|user_data| Log::record(user_data, 'c')));
        drop(registration);
        assert_eq!(log.take(), "");
    }

    #[test]
    fn wakes_check_their_members() {
        let off = mln_wake { callback: None, user_data: core::ptr::null_mut(), release_user_data: None, queue: 0 };
        assert!(Wake::new(&off).is_ok());
        let retaining = mln_wake { release_user_data: Some(Log::release), ..off };
        assert_eq!(Wake::new(&retaining).err().unwrap().message(), "a disabled wake must not retain user data");
        let queued = mln_wake { queue: queue(), ..off };
        assert_eq!(Wake::new(&queued).err().unwrap().message(), "a disabled wake must not name a queue");
    }

    #[test]
    fn a_wake_is_off_until_accepted_and_after_close() {
        let log = Log::new();
        let wake = Wake::new(&mln_wake {
            callback: Some(Log::wake),
            user_data: log.pointer('w'),
            release_user_data: Some(Log::release),
            queue: 0,
        })
        .unwrap();
        wake.notify();
        assert_eq!(log.take(), "");
        wake.accept();
        wake.notify();
        assert_eq!(log.take(), "ww");
        wake.close();
        wake.notify();
        drop(wake);
        assert_eq!(log.take(), "wr");

        // An unaccepted wake never releases.
        let wake = Wake::new(&mln_wake {
            callback: Some(Log::wake),
            user_data: log.pointer('u'),
            release_user_data: Some(Log::release),
            queue: 0,
        })
        .unwrap();
        drop(wake);
        assert_eq!(log.take(), "");
    }

    #[test]
    fn a_queued_wake_keeps_one_call_pending_and_releases_in_dispatch() {
        let log = Log::new();
        let queue = queue();
        let wake = Wake::new(&mln_wake {
            callback: Some(Log::wake),
            user_data: log.pointer('w'),
            release_user_data: Some(Log::release),
            queue,
        })
        .unwrap();
        wake.accept();
        wake.notify();
        wake.notify();
        dispatch(queue);
        assert_eq!(log.take(), "ww");
        wake.notify();
        drop(wake);
        assert_eq!(log.take(), "");
        dispatch(queue);
        assert_eq!(log.take(), "wwwr");
        mln_delivery_queue_release(queue);
    }
}
