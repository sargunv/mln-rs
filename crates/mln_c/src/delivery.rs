//! `delivery.h`: queues that hold callback calls until the host dispatches
//! them on a thread it chooses.
//!
//! Calls run in push order. A queue wakes its host when it goes from empty to
//! nonempty, and a dispatch takes every call queued by then, so a host that
//! dispatches after each wake misses none. Registrations hold their queue, so
//! a released queue lives on, closed, until the last of them goes; it refuses
//! their calls, and reports the user data that it leaves unreleased to its
//! `release_abandoned_user_data`.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::callback::{UserData, Wake};
use crate::error::{Error, ErrorKind};
use crate::generated::types::{
    mln_delivery_queue, mln_delivery_queue_options, mln_diagnostic, mln_status, mln_user_data_release, mln_wake,
};
use crate::handles::{self, HandleKind, HandleTable};
use crate::{abi, lock};

/// One queued call. Dropping it without running it drops what it owns.
pub(crate) struct Call {
    run: Box<dyn FnOnce() + Send>,
    /// For a registration's last call, which owns its user data: what the
    /// queue reports abandoned if the call never runs.
    abandons: Option<UserData>,
}

impl Call {
    pub(crate) fn new(run: impl FnOnce() + Send + 'static) -> Self {
        Self { run: Box::new(run), abandons: None }
    }
}

/// A registration's claim on its queue's abandon hook, from before the
/// registration is accepted until its last call is queued
/// ([`Queue::push_last`]). Dropping it withdraws the claim, as a rejected
/// registration must.
pub(crate) struct Hold {
    queue: Arc<Queue>,
    id: Option<u64>,
}

impl Drop for Hold {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            lock(&self.queue.state).held.remove(&id);
        }
    }
}

pub(crate) struct Queue {
    state: Mutex<State>,
    abandon: mln_user_data_release,
    // Set while a dispatch runs calls. A dispatch that finds one running sets
    // `dispatch_requested`, which makes the running one take the calls again.
    dispatching: AtomicBool,
    dispatch_requested: AtomicBool,
}

#[derive(Default)]
struct State {
    pending: VecDeque<Call>,
    /// The calls that the running dispatch took and hasn't run yet, which a
    /// release from inside one of them abandons.
    running: VecDeque<Call>,
    held: HashMap<u64, UserData>,
    next_hold: u64,
    closed: bool,
    wake: Option<Arc<Wake>>,
}

impl Queue {
    fn new(wake: Option<Arc<Wake>>, abandon: mln_user_data_release) -> Self {
        Self {
            state: Mutex::new(State { wake, ..State::default() }),
            abandon,
            dispatching: AtomicBool::new(false),
            dispatch_requested: AtomicBool::new(false),
        }
    }

    /// Holds `user_data` for the abandon hook, if the queue has one and is
    /// open.
    pub(crate) fn hold(self: &Arc<Self>, user_data: UserData) -> Option<Hold> {
        self.abandon?;
        let mut state = lock(&self.state);
        if state.closed {
            return None;
        }
        let id = state.next_hold;
        state.next_hold += 1;
        state.held.insert(id, user_data);
        drop(state);
        Some(Hold { queue: Arc::clone(self), id: Some(id) })
    }

    /// Queues a call, or gives it back when the queue is released.
    pub(crate) fn push(&self, call: Call) -> Result<(), Call> {
        self.append(call, None)
    }

    /// Queues a registration's last call and settles its hold, as one step
    /// against a release.
    pub(crate) fn push_last(&self, call: Call, hold: Option<Hold>) -> Result<(), Call> {
        let settled = hold.and_then(|mut hold| {
            debug_assert!(core::ptr::eq(&*hold.queue, self), "a hold settles on its own queue");
            hold.id.take()
        });
        self.append(call, settled)
    }

    fn append(&self, mut call: Call, settled: Option<u64>) -> Result<(), Call> {
        let wake = {
            let mut state = lock(&self.state);
            // A released queue already reported the settled hold.
            if state.closed {
                return Err(call);
            }
            if let Some(id) = settled {
                call.abandons = state.held.remove(&id);
            }
            let was_empty = state.pending.is_empty();
            state.pending.push_back(call);
            // The clone keeps the wake alive across the call, even when the
            // host releases the queue from inside it.
            if was_empty { state.wake.clone() } else { None }
        };
        if let Some(wake) = wake {
            wake.notify();
        }
        Ok(())
    }

    /// Runs the queued calls on this thread, in push order. Returns false
    /// when another dispatch is running, which then runs them instead.
    pub(crate) fn dispatch(&self) -> bool {
        self.dispatch_requested.store(true, Ordering::Release);
        let mut acquired = false;
        while !self.dispatching.swap(true, Ordering::AcqRel) {
            acquired = true;
            while self.dispatch_requested.swap(false, Ordering::AcqRel) {
                self.run_calls();
            }
            // A swap rather than a store, so that a dispatch that saw this one
            // running has its request visible to the check after it.
            self.dispatching.swap(false, Ordering::AcqRel);
            if !self.dispatch_requested.load(Ordering::Acquire) {
                break;
            }
        }
        acquired
    }

    fn run_calls(&self) {
        {
            let mut state = lock(&self.state);
            if state.closed {
                return;
            }
            let State { pending, running, .. } = &mut *state;
            running.append(pending);
        }
        loop {
            let call = {
                let mut state = lock(&self.state);
                if state.closed {
                    return;
                }
                match state.running.pop_front() {
                    Some(call) => call,
                    None => return,
                }
            };
            (call.run)();
        }
    }

    /// Refuses later calls, turns the wake off, drops the queued calls, and
    /// reports the user data of the held registrations and of the abandoned
    /// last calls.
    fn close(&self) {
        let (running, pending, held, wake) = {
            let mut state = lock(&self.state);
            state.closed = true;
            (
                core::mem::take(&mut state.running),
                core::mem::take(&mut state.pending),
                core::mem::take(&mut state.held),
                state.wake.take(),
            )
        };
        if let Some(wake) = wake {
            wake.close();
            // This releases the wake's user data, or the push whose notify
            // still holds the wake does when it returns.
            drop(wake);
        }
        let mut abandoned: Vec<UserData> = Vec::new();
        for call in running.into_iter().chain(pending) {
            abandoned.extend(call.abandons);
            drop(call.run);
        }
        abandoned.extend(held.into_values());
        if let Some(abandon) = self.abandon {
            for user_data in abandoned {
                // SAFETY: the host's abandon hook, with the user data of a
                // registration whose release will never run.
                unsafe { abandon(user_data.get()) };
            }
        }
    }
}

impl Drop for Queue {
    fn drop(&mut self) {
        self.close();
    }
}

static QUEUES: HandleTable<Queue> = HandleTable::new(HandleKind::DeliveryQueue);

/// The queue that a registration's `queue` member names.
pub(crate) fn resolve(queue: mln_delivery_queue) -> Result<Arc<Queue>, Error> {
    QUEUES.get(queue)
}

/// `mln_delivery_queue_create()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_delivery_queue_create(
    options: *const mln_delivery_queue_options,
    out_queue: *mut mln_delivery_queue,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        let options = if options.is_null() {
            mln_delivery_queue_options {
                size: size_of::<mln_delivery_queue_options>() as u32,
                wake: mln_wake {
                    callback: None,
                    user_data: core::ptr::null_mut(),
                    release_user_data: None,
                    queue: handles::NULL,
                },
                release_abandoned_user_data: None,
            }
        } else {
            // SAFETY: the caller's contract.
            unsafe { abi::read_sized(options) }?
        };
        if options.wake.queue != handles::NULL {
            return Err(Error::invalid_argument("a delivery queue's own wake must not name a queue"));
        }
        let wake = Arc::new(Wake::new(&options.wake)?);
        // SAFETY: the caller's contract.
        let out_queue = unsafe { handles::out_handle(out_queue, "out_queue") }?;
        let queue = Arc::new(Queue::new(Some(Arc::clone(&wake)), options.release_abandoned_user_data));
        *out_queue = QUEUES.insert(queue)?;
        wake.accept();
        Ok(())
    })
}

/// `mln_delivery_queue_release()`.
#[unsafe(no_mangle)]
pub extern "C" fn mln_delivery_queue_release(queue: mln_delivery_queue) {
    if let Ok(queue) = QUEUES.remove(queue) {
        queue.close();
    }
}

/// `mln_delivery_queue_dispatch()`.
///
/// # Safety
///
/// `out_diagnostic` is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_delivery_queue_dispatch(
    queue: mln_delivery_queue,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        if QUEUES.get(queue)?.dispatch() {
            Ok(())
        } else {
            Err(Error::new(ErrorKind::Busy, "the delivery queue is already dispatching"))
        }
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use core::ffi::c_void;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;
    use crate::generated::types::{
        MLN_STATUS_BUSY, MLN_STATUS_INVALID_ARGUMENT, MLN_STATUS_INVALID_STATE, MLN_STATUS_OK,
    };

    /// What a test's callbacks did, as a string of a tag and a kind per call.
    #[derive(Clone)]
    pub(crate) struct Log(Arc<Mutex<String>>);

    struct Entry {
        log: Log,
        tag: char,
    }

    impl Log {
        pub(crate) fn new() -> Self {
            Self(Arc::default())
        }

        /// User data that names this log and `tag`. Leaked, as the calls
        /// that take it may outlive a failing test.
        pub(crate) fn user_data(&self, tag: char) -> UserData {
            UserData::new(self.pointer(tag))
        }

        pub(crate) fn pointer(&self, tag: char) -> *mut c_void {
            Box::into_raw(Box::new(Entry { log: self.clone(), tag })).cast()
        }

        pub(crate) fn take(&self) -> String {
            core::mem::take(&mut *lock(&self.0))
        }

        pub(crate) fn record(user_data: UserData, kind: char) {
            // SAFETY: every test's user data is an `Entry` from `pointer`.
            let entry = unsafe { &*user_data.get().cast::<Entry>() };
            let mut log = lock(&entry.log.0);
            log.push(entry.tag);
            log.push(kind);
        }

        pub(crate) unsafe extern "C" fn release(user_data: *mut c_void) {
            Self::record(UserData::new(user_data), 'r');
        }

        pub(crate) unsafe extern "C" fn abandon(user_data: *mut c_void) {
            Self::record(UserData::new(user_data), 'a');
        }

        pub(crate) unsafe extern "C" fn wake(user_data: *mut c_void) {
            Self::record(UserData::new(user_data), 'w');
        }
    }

    /// A queue without a wake.
    pub(crate) fn queue() -> mln_delivery_queue {
        let mut queue = 0;
        // SAFETY: null options, a local out-handle and a null diagnostic.
        let status = unsafe { mln_delivery_queue_create(core::ptr::null(), &mut queue, core::ptr::null_mut()) };
        assert_eq!(status, MLN_STATUS_OK);
        queue
    }

    /// A queue whose wake counts, with `abandon` as its abandon hook.
    pub(crate) fn queue_with_abandon(abandon: mln_user_data_release) -> (mln_delivery_queue, &'static AtomicUsize) {
        unsafe extern "C" fn count(user_data: *mut c_void) {
            // SAFETY: the counter that `queue_with_abandon` leaked.
            unsafe { &*user_data.cast::<AtomicUsize>() }.fetch_add(1, Ordering::SeqCst);
        }
        let wakes: &'static AtomicUsize = Box::leak(Box::default());
        let options = mln_delivery_queue_options {
            size: size_of::<mln_delivery_queue_options>() as u32,
            wake: mln_wake {
                callback: Some(count),
                user_data: core::ptr::from_ref(wakes).cast_mut().cast(),
                release_user_data: None,
                queue: 0,
            },
            release_abandoned_user_data: abandon,
        };
        let mut queue = 0;
        // SAFETY: local options and out-handle, and a null diagnostic.
        let status = unsafe { mln_delivery_queue_create(&options, &mut queue, core::ptr::null_mut()) };
        assert_eq!(status, MLN_STATUS_OK);
        (queue, wakes)
    }

    fn dispatch(queue: mln_delivery_queue) -> mln_status {
        // SAFETY: a null diagnostic.
        unsafe { mln_delivery_queue_dispatch(queue, core::ptr::null_mut()) }
    }

    fn push(queue: mln_delivery_queue, call: impl FnOnce() + Send + 'static) {
        assert!(resolve(queue).unwrap().push(Call::new(call)).is_ok());
    }

    /// Runs `f` on another thread and fails the test when it doesn't finish,
    /// such as when it deadlocks.
    fn within_deadline(f: impl FnOnce() + Send + 'static) {
        let (done, finished) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            f();
            let _ = done.send(());
        });
        assert!(finished.recv_timeout(Duration::from_secs(10)).is_ok(), "the calls deadlocked");
        thread.join().unwrap();
    }

    #[test]
    fn creation_and_release_check_their_arguments() {
        mln_delivery_queue_release(handles::NULL);
        let mut unused = 0;
        let mut options = mln_delivery_queue_options {
            size: 1,
            wake: mln_wake { callback: None, user_data: core::ptr::null_mut(), release_user_data: None, queue: 0 },
            release_abandoned_user_data: None,
        };
        // SAFETY: local options and out-handles, and null diagnostics.
        unsafe {
            let create = |options: &mln_delivery_queue_options, out: *mut u64| {
                mln_delivery_queue_create(options, out, core::ptr::null_mut())
            };
            assert_eq!(create(&options, &mut unused), MLN_STATUS_INVALID_ARGUMENT);
            options.size = size_of::<mln_delivery_queue_options>() as u32;
            options.wake.release_user_data = Some(Log::release);
            assert_eq!(create(&options, &mut unused), MLN_STATUS_INVALID_ARGUMENT);
            options.wake.release_user_data = None;
            assert_eq!(create(&options, core::ptr::null_mut()), MLN_STATUS_INVALID_ARGUMENT);

            let mut queue = 0;
            assert_eq!(create(&options, &mut queue), MLN_STATUS_OK);
            let mut occupied = queue;
            assert_eq!(create(&options, &mut occupied), MLN_STATUS_INVALID_ARGUMENT);

            // A queue's own wake can't name a queue.
            options.wake = mln_wake {
                callback: Some(Log::wake),
                user_data: core::ptr::null_mut(),
                release_user_data: None,
                queue,
            };
            let mut other = 0;
            assert_eq!(create(&options, &mut other), MLN_STATUS_INVALID_ARGUMENT);
            assert_eq!(other, 0);

            mln_delivery_queue_release(queue);
            mln_delivery_queue_release(queue);
            assert_eq!(dispatch(queue), MLN_STATUS_INVALID_STATE);
            assert_eq!(dispatch(handles::NULL), MLN_STATUS_INVALID_ARGUMENT);
        }
    }

    #[test]
    fn the_wake_fires_once_per_empty_to_nonempty_transition() {
        let log = Log::new();
        let (queue, wakes) = queue_with_abandon(None);
        let first = log.user_data('1');
        let second = log.user_data('2');
        push(queue, move || Log::record(first, 'c'));
        push(queue, move || Log::record(second, 'c'));
        assert_eq!(wakes.load(Ordering::SeqCst), 1);
        assert_eq!(dispatch(queue), MLN_STATUS_OK);
        assert_eq!(log.take(), "1c2c");
        assert_eq!(dispatch(queue), MLN_STATUS_OK);
        push(queue, move || Log::record(first, 'c'));
        assert_eq!(wakes.load(Ordering::SeqCst), 2);
        mln_delivery_queue_release(queue);
        assert_eq!(log.take(), "");
    }

    #[test]
    fn the_wake_releases_its_user_data_when_the_queue_is_released() {
        let log = Log::new();
        let options = mln_delivery_queue_options {
            size: size_of::<mln_delivery_queue_options>() as u32,
            wake: mln_wake {
                callback: Some(Log::wake),
                user_data: log.pointer('w'),
                release_user_data: Some(Log::release),
                queue: 0,
            },
            release_abandoned_user_data: None,
        };
        let mut queue = 0;
        // SAFETY: local options and out-handle, and a null diagnostic.
        assert_eq!(unsafe { mln_delivery_queue_create(&options, &mut queue, core::ptr::null_mut()) }, MLN_STATUS_OK);
        push(queue, || {});
        mln_delivery_queue_release(queue);
        assert_eq!(log.take(), "wwwr");
    }

    #[test]
    fn a_dispatch_inside_a_call_is_busy_and_the_running_one_takes_later_calls() {
        let log = Log::new();
        let queue = queue();
        let tag = log.user_data('A');
        let later = log.user_data('L');
        let statuses = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&statuses);
        push(queue, move || {
            Log::record(tag, 'c');
            let nested = dispatch(queue);
            let concurrent = std::thread::spawn(move || dispatch(queue)).join().unwrap();
            lock(&seen).extend([nested, concurrent]);
            push(queue, move || Log::record(later, 'c'));
        });
        assert_eq!(dispatch(queue), MLN_STATUS_OK);
        assert_eq!(*lock(&statuses), [MLN_STATUS_BUSY, MLN_STATUS_BUSY]);
        assert_eq!(log.take(), "AcLc");
        mln_delivery_queue_release(queue);
    }

    #[test]
    fn concurrent_dispatches_run_each_call_exactly_once() {
        const CALLS: usize = 64;
        let queue = queue();
        let seen: Arc<[AtomicUsize; CALLS]> = Arc::new(core::array::from_fn(|_| AtomicUsize::new(0)));
        let total = Arc::new(AtomicUsize::new(0));
        let dispatchers: Vec<_> = (0..4)
            .map(|_| {
                let total = Arc::clone(&total);
                std::thread::spawn(move || {
                    while total.load(Ordering::SeqCst) < CALLS {
                        let _ = dispatch(queue);
                    }
                })
            })
            .collect();
        for index in 0..CALLS {
            let (seen, total) = (Arc::clone(&seen), Arc::clone(&total));
            push(queue, move || {
                seen[index].fetch_add(1, Ordering::SeqCst);
                total.fetch_add(1, Ordering::SeqCst);
            });
        }
        for dispatcher in dispatchers {
            dispatcher.join().unwrap();
        }
        assert!(seen.iter().all(|count| count.load(Ordering::SeqCst) == 1));
        mln_delivery_queue_release(queue);
    }

    #[test]
    fn a_call_that_releases_the_queue_abandons_the_rest() {
        let log = Log::new();
        let (queue, _) = queue_with_abandon(Some(Log::abandon));
        let state = resolve(queue).unwrap();
        let first = log.user_data('1');
        let second = log.user_data('2');
        let first_hold = state.hold(first);
        let second_hold = state.hold(second);
        let run_first = Call::new(move || {
            Log::record(first, 'c');
            mln_delivery_queue_release(queue);
            Log::record(first, 'r');
        });
        assert!(state.push_last(run_first, first_hold).is_ok());
        assert!(state.push_last(Call::new(move || Log::record(second, 'c')), second_hold).is_ok());
        drop(state);
        assert_eq!(dispatch(queue), MLN_STATUS_OK);
        assert_eq!(log.take(), "1c2a1r");
        assert_eq!(dispatch(queue), MLN_STATUS_INVALID_STATE);
    }

    #[test]
    fn a_release_reports_each_abandoned_user_data_once() {
        let log = Log::new();
        let (queue, _) = queue_with_abandon(Some(Log::abandon));
        let state = resolve(queue).unwrap();
        // Held, and never queued.
        let live = state.hold(log.user_data('l'));
        // Queued as a last call.
        let queued = log.user_data('q');
        let queued_hold = state.hold(queued);
        assert!(state.push_last(Call::new(move || Log::record(queued, 'r')), queued_hold).is_ok());
        // Withdrawn, as a rejected registration's.
        drop(state.hold(log.user_data('w')));
        // Held at the release, and refused after it.
        let late = log.user_data('t');
        let late_hold = state.hold(late);
        mln_delivery_queue_release(queue);
        assert!(state.push_last(Call::new(move || Log::record(late, 'r')), late_hold).is_err());
        let mut reports: Vec<char> = log.take().chars().collect();
        reports.sort_unstable();
        assert_eq!(reports.into_iter().collect::<String>(), "aaalqt");
        drop(live);
        drop(state);
        assert_eq!(log.take(), "");
    }

    #[test]
    fn callbacks_may_call_the_library_without_deadlocking() {
        within_deadline(|| {
            let log = Log::new();
            let (queue, _) = queue_with_abandon(Some(Log::abandon));
            let other = super::tests::queue();
            let tag = log.user_data('c');
            push(queue, move || {
                // Push to, dispatch and release queues from inside a call.
                push(queue, move || Log::record(tag, 'n'));
                push(other, move || Log::record(tag, 'o'));
                assert_eq!(dispatch(other), MLN_STATUS_OK);
                mln_delivery_queue_release(other);
                let _ = super::tests::queue();
                Log::record(tag, 'c');
            });
            assert_eq!(dispatch(queue), MLN_STATUS_OK);
            assert_eq!(log.take(), "cocc");
            // A call queued during a dispatch waits for the next one.
            assert_eq!(dispatch(queue), MLN_STATUS_OK);
            assert_eq!(log.take(), "cn");
            mln_delivery_queue_release(queue);
        });
    }

    #[test]
    fn releasing_from_the_wake_neither_deadlocks_nor_wakes_again() {
        struct Releasing {
            queue: std::sync::atomic::AtomicU64,
            calls: AtomicUsize,
            releases: AtomicUsize,
        }
        unsafe extern "C" fn release_from_wake(user_data: *mut c_void) {
            // SAFETY: the test's leaked state.
            let state = unsafe { &*user_data.cast::<Releasing>() };
            if state.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                mln_delivery_queue_release(state.queue.load(Ordering::SeqCst));
            }
        }
        unsafe extern "C" fn count_release(user_data: *mut c_void) {
            // SAFETY: as above.
            unsafe { &*user_data.cast::<Releasing>() }.releases.fetch_add(1, Ordering::SeqCst);
        }
        within_deadline(|| {
            let state: &'static Releasing = Box::leak(Box::new(Releasing {
                queue: Default::default(),
                calls: AtomicUsize::new(0),
                releases: AtomicUsize::new(0),
            }));
            let options = mln_delivery_queue_options {
                size: size_of::<mln_delivery_queue_options>() as u32,
                wake: mln_wake {
                    callback: Some(release_from_wake),
                    user_data: core::ptr::from_ref(state).cast_mut().cast(),
                    release_user_data: Some(count_release),
                    queue: 0,
                },
                release_abandoned_user_data: None,
            };
            let mut queue = 0;
            // SAFETY: local options and out-handle, and a null diagnostic.
            let status = unsafe { mln_delivery_queue_create(&options, &mut queue, core::ptr::null_mut()) };
            assert_eq!(status, MLN_STATUS_OK);
            state.queue.store(queue, Ordering::SeqCst);
            let shared = resolve(queue).unwrap();
            assert!(shared.push(Call::new(|| {})).is_ok());
            assert_eq!(state.releases.load(Ordering::SeqCst), 1);
            assert!(shared.push(Call::new(|| {})).is_err());
            assert_eq!(state.calls.load(Ordering::SeqCst), 1);
        });
    }
}
