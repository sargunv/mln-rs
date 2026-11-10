// Ports include/mln/util/run_loop.hpp and platform/default/src/mln/util/run_loop.cpp
// (the libuv loop): `invoke`, `runOnce`, `run`, `stop`, `waitForEmpty`, and
// the async handles behind `util::AsyncTask`.
//
// libuv is replaced by a plain loop: `run_once` is `uv_run(UV_RUN_NOWAIT)`,
// i.e. it runs every async handle that was signalled, in registration order
// (the loop's own task queue first, as its handle is created in the RunLoop
// constructor), then returns without blocking. `run` is
// `uv_run(UV_RUN_DEFAULT)` with the holder handle ref'd: it waits for a
// wake-up (a task from any thread, or an AsyncTask send), runs once, and
// returns once a `stop()` task ran (C++ `stop` invokes a task that unrefs
// the holder handle).
//
// The task queues live in a `Send + Sync` core (`RunLoopHandle`), which is
// also the loop's `Scheduler` for mailboxes and `Scheduler::get_current()`:
// other threads post to it, as they post through a C++ `RunLoop*`. The async
// handles stay on the loop's thread.
//
// Timers (`util::Timer`, timer.rs) run first in `run_once`, as `uv_run` runs
// due timers before the poll phase; a blocking `run()` sleeps until the next
// timer is due. Timers do not keep `run()` alive (libuv unrefs them).
//
// TODO(port): fd watches (`addWatch`/`removeWatch`, only for libcurl),
// `invokeCancellable`, `setPlatformCallback`.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use mln_util::sync::{CondvarExt, MutexExt};

use crate::scheduler::{self, Scheduler};
use crate::timer::TimerHandle;

type Task = Box<dyn FnOnce() + Send + 'static>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    Default,
    High,
}

#[derive(Default)]
struct Queues {
    default_queue: VecDeque<Task>,
    high_priority_queue: VecDeque<Task>,
}

/// The thread-safe part of a RunLoop: the task queues (`defaultQueue`,
/// `highPriorityQueue` and their mutex in C++), the pending flag of the
/// loop's own async handle, and the wake-up a blocking `run()` waits on.
pub(crate) struct Core {
    queues: Mutex<Queues>,
    /// The RunLoop's own async handle (`impl->async`), which runs `process`.
    queue_pending: AtomicBool,
    /// Set by every wake-up; `run()` sleeps until it is set.
    signal: Mutex<bool>,
    condvar: Condvar,
    /// The holder handle is unref'd (`stop()` ran).
    stopped: AtomicBool,
    /// Port-only: told about every wake-up, for a loop that a host drives
    /// with `run_once` instead of `run` (see `RunLoop::set_waker`).
    waker: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl Core {
    fn new() -> Self {
        Core {
            queues: Mutex::new(Queues::default()),
            queue_pending: AtomicBool::new(false),
            signal: Mutex::new(false),
            condvar: Condvar::new(),
            stopped: AtomicBool::new(false),
            waker: Mutex::new(None),
        }
    }

    fn push(&self, priority: Priority, task: Task) {
        {
            let mut queues = self.queues.lock_unpoisoned();
            match priority {
                Priority::High => queues.high_priority_queue.push_back(task),
                Priority::Default => queues.default_queue.push_back(task),
            }
        }
        // wake()
        self.queue_pending.store(true, Ordering::SeqCst);
        self.wake();
    }

    /// Wakes a blocking `run()` (what `uv_async_send` does for libuv).
    pub(crate) fn wake(&self) {
        {
            let mut signalled = self.signal.lock_unpoisoned();
            *signalled = true;
            self.condvar.notify_all();
        }
        let waker = self.waker.lock_unpoisoned().clone();
        if let Some(waker) = waker {
            waker();
        }
    }

    fn wait(&self) {
        let mut signalled = self.signal.lock_unpoisoned();
        while !*signalled {
            signalled = self.condvar.wait_unpoisoned(signalled);
        }
        *signalled = false;
    }

    /// `wait`, but returns at `deadline` at the latest.
    fn wait_until(&self, deadline: mln_util::time::Instant) {
        let mut signalled = self.signal.lock_unpoisoned();
        while !*signalled {
            let now = mln_util::time::Instant::now();
            if now >= deadline {
                return;
            }
            signalled = self.condvar.wait_timeout_unpoisoned(signalled, deadline - now).0;
        }
        *signalled = false;
    }

    fn process(&self) {
        loop {
            let task = {
                let mut queues = self.queues.lock_unpoisoned();
                if let Some(task) = queues.high_priority_queue.pop_front() {
                    task
                } else if let Some(task) = queues.default_queue.pop_front() {
                    task
                } else {
                    break;
                }
            };
            task();
        }
    }

    fn remaining(&self) -> usize {
        let queues = self.queues.lock_unpoisoned();
        queues.default_queue.len() + queues.high_priority_queue.len()
    }
}

/// An async handle (`uv_async_t`): a coalescing "pending" flag plus the
/// callback that runs on the loop's thread.
pub(crate) struct AsyncHandle {
    pub(crate) pending: Arc<AtomicBool>,
    /// gtests-core: set when the AsyncTask is destroyed (`uv_close`): a
    /// signalled task that another callback of the same `run_once` destroys
    /// must not fire (test/util/async_task.test.cpp DestroyAfterSignaling).
    pub(crate) closed: Cell<bool>,
    pub(crate) task: RefCell<Box<dyn FnMut()>>,
    pub(crate) core: Arc<Core>,
}

struct Inner {
    core: Arc<Core>,
    handles: RefCell<Vec<Weak<AsyncHandle>>>,
    timers: RefCell<Vec<Weak<TimerHandle>>>,
}

thread_local! {
    static CURRENT: RefCell<Option<Rc<Inner>>> = const { RefCell::new(None) };
}

/// `util::RunLoop`. Cheap to clone: clones refer to the same loop.
#[derive(Clone)]
pub struct RunLoop {
    inner: Rc<Inner>,
}

/// A `Send + Sync` reference to a RunLoop's task queue (what other threads
/// hold as a `RunLoop*` in C++): `invoke`, `stop`, and the loop's
/// `Scheduler`.
#[derive(Clone)]
pub struct RunLoopHandle {
    core: Arc<Core>,
}

impl RunLoop {
    /// `RunLoop(Type::Default)` / `RunLoop(Type::New)`: creates a loop and
    /// makes it the current scheduler of this thread
    /// (`Scheduler::SetCurrent(this)`).
    pub fn new() -> Self {
        let inner = Rc::new(Inner {
            core: Arc::new(Core::new()),
            handles: RefCell::new(Vec::new()),
            timers: RefCell::new(Vec::new()),
        });
        CURRENT.with(|c| *c.borrow_mut() = Some(inner.clone()));
        let scheduler: Arc<dyn Scheduler + Send + Sync> = Arc::new(RunLoopHandle { core: inner.core.clone() });
        scheduler::set_current(Some(scheduler));
        RunLoop { inner }
    }

    /// `RunLoop::Get()`. C++ asserts that the thread has a current RunLoop;
    /// the port creates one on first use instead, which is what the test
    /// harnesses do first thing in C++ (and what `Scheduler::GetCurrent()`
    /// does).
    pub fn get() -> RunLoop {
        let existing = CURRENT.with(|c| c.borrow().clone());
        match existing {
            Some(inner) => RunLoop { inner },
            None => RunLoop::new(),
        }
    }

    /// The current thread's loop, if it has one.
    pub fn try_get() -> Option<RunLoop> {
        CURRENT.with(|c| c.borrow().clone()).map(|inner| RunLoop { inner })
    }

    /// A handle other threads can post to.
    pub fn handle(&self) -> RunLoopHandle {
        RunLoopHandle { core: self.inner.core.clone() }
    }

    /// Invoke `task` on this RunLoop.
    pub fn invoke(&self, task: impl FnOnce() + Send + 'static) {
        self.invoke_with_priority(Priority::Default, task);
    }

    pub fn invoke_with_priority(&self, priority: Priority, task: impl FnOnce() + Send + 'static) {
        self.inner.core.push(priority, Box::new(task));
    }

    /// Port-only: calls `waker`, from the waking thread, whenever this loop
    /// gets work (a task or an AsyncTask send), so that a host that drives
    /// the loop with `run_once` on its own thread knows to run it. Timers
    /// don't wake: such a host runs the loop at least once a frame.
    pub fn set_waker(&self, waker: Option<Arc<dyn Fn() + Send + Sync>>) {
        *self.inner.core.waker.lock_unpoisoned() = waker;
    }

    /// `uv_run(loop, UV_RUN_NOWAIT)`. Returns whether a timer, task or
    /// async handle ran.
    pub fn run_once(&self) -> bool {
        let mut did_work = self.run_timers();
        if self.inner.core.queue_pending.swap(false, Ordering::SeqCst) {
            self.inner.core.process();
            did_work = true;
        }
        // Snapshot: callbacks may create or drop async handles.
        let handles: Vec<Rc<AsyncHandle>> = {
            let mut handles = self.inner.handles.borrow_mut();
            handles.retain(|h| h.strong_count() > 0);
            handles.iter().filter_map(Weak::upgrade).collect()
        };
        for handle in handles {
            if !handle.closed.get() && handle.pending.swap(false, Ordering::SeqCst) {
                (handle.task.borrow_mut())();
                did_work = true;
            }
        }
        did_work
    }

    /// `run()`: `uv_run(loop, UV_RUN_DEFAULT)` until `stop()`. It blocks, so
    /// a host that runs on the browser's event loop doesn't call it: it
    /// drives the loop with `run_once` instead (see `set_waker`).
    pub fn run(&self) {
        let core = &self.inner.core;
        core.stopped.store(false, Ordering::SeqCst);
        loop {
            self.run_once();
            if core.stopped.load(Ordering::SeqCst) {
                return;
            }
            self.wait_for_work();
        }
    }

    /// Sleeps until this loop gets a task or an async handle send, or its
    /// next timer is due. It can return early, when a wake-up came while
    /// the loop was running.
    pub fn wait_for_work(&self) {
        let core = &self.inner.core;
        match self.next_timer_due() {
            Some(deadline) => core.wait_until(deadline),
            None => core.wait(),
        }
    }

    /// `uv__run_timers`: runs the timers that are due, earliest first (ties
    /// in start order). A callback may stop, restart or drop any timer.
    /// Returns whether any was due.
    fn run_timers(&self) -> bool {
        let now = mln_util::time::Instant::now();
        let mut due: Vec<Rc<TimerHandle>> = {
            let mut timers = self.inner.timers.borrow_mut();
            timers.retain(|t| t.strong_count() > 0);
            timers.iter().filter_map(Weak::upgrade).filter(|t| t.due.get().is_some_and(|d| d <= now)).collect()
        };
        due.sort_by_key(|t| (t.due.get(), t.start_id.get()));
        let any_due = !due.is_empty();
        for timer in due {
            // An earlier callback may have stopped or restarted it.
            if timer.due.get().is_some_and(|d| d <= now) {
                timer.fire(now);
            }
        }
        any_due
    }

    /// The earliest due time of this loop's running timers.
    fn next_timer_due(&self) -> Option<mln_util::time::Instant> {
        self.inner.timers.borrow().iter().filter_map(Weak::upgrade).filter_map(|t| t.due.get()).min()
    }

    pub(crate) fn register_timer(&self, timer: &Rc<TimerHandle>) {
        self.inner.timers.borrow_mut().push(Rc::downgrade(timer));
    }

    /// `updateTime()`: libuv caches the loop time; this loop reads the clock
    /// whenever it needs it, so there is nothing to update.
    pub fn update_time(&self) {}

    /// `stop()`: `invoke([&] { uv_unref(holder) })`.
    pub fn stop(&self) {
        self.handle().stop();
    }

    pub(crate) fn register(&self, handle: &Rc<AsyncHandle>) {
        self.inner.handles.borrow_mut().push(Rc::downgrade(handle));
    }

    pub(crate) fn core(&self) -> Arc<Core> {
        self.inner.core.clone()
    }

    /// `RunLoop::waitForEmpty`.
    pub fn wait_for_empty(&self) {
        loop {
            if self.inner.core.remaining() == 0 {
                return;
            }
            self.run_once();
        }
    }
}

impl Default for RunLoop {
    fn default() -> Self {
        RunLoop::new()
    }
}

impl Scheduler for RunLoop {
    fn schedule(&self, task: Box<dyn FnOnce() + Send + 'static>) {
        self.inner.core.push(Priority::Default, task);
    }

    fn wait_for_empty(&self) {
        RunLoop::wait_for_empty(self);
    }
}

impl RunLoopHandle {
    /// `RunLoop::invoke` from any thread.
    pub fn invoke(&self, task: impl FnOnce() + Send + 'static) {
        self.core.push(Priority::Default, Box::new(task));
    }

    pub fn invoke_with_priority(&self, priority: Priority, task: impl FnOnce() + Send + 'static) {
        self.core.push(priority, Box::new(task));
    }

    /// `RunLoop::stop()`.
    pub fn stop(&self) {
        let core = Arc::downgrade(&self.core);
        self.invoke(move || {
            if let Some(core) = core.upgrade() {
                core.stopped.store(true, Ordering::SeqCst);
            }
        });
    }
}

impl Scheduler for RunLoopHandle {
    fn schedule(&self, task: Box<dyn FnOnce() + Send + 'static>) {
        self.core.push(Priority::Default, task);
    }

    /// Must be called on the loop's own thread (as in C++, where it calls
    /// `runOnce`); elsewhere it waits for the queue to drain.
    fn wait_for_empty(&self) {
        if let Some(run_loop) = RunLoop::try_get() {
            if Arc::ptr_eq(&run_loop.inner.core, &self.core) {
                run_loop.wait_for_empty();
                return;
            }
        }
        while self.core.remaining() != 0 {
            std::thread::yield_now();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    #[test]
    fn invoke_runs_on_run_once() {
        let run_loop = RunLoop::new();
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        run_loop.invoke(move || {
            c.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(count.load(Ordering::SeqCst), 0);
        run_loop.run_once();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        run_loop.run_once();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn high_priority_first() {
        let run_loop = RunLoop::new();
        let order = Arc::new(Mutex::new(Vec::new()));
        let (a, b) = (order.clone(), order.clone());
        run_loop.invoke(move || a.lock().unwrap().push(1));
        run_loop.invoke_with_priority(Priority::High, move || b.lock().unwrap().push(2));
        run_loop.wait_for_empty();
        assert_eq!(*order.lock().unwrap(), vec![2, 1]);
    }

    #[test]
    fn run_until_stopped_from_another_thread() {
        let (tx, rx) = std::sync::mpsc::channel();
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        let thread = std::thread::spawn(move || {
            let run_loop = RunLoop::new();
            tx.send(run_loop.handle()).unwrap();
            run_loop.run();
            c.load(Ordering::SeqCst)
        });
        let handle = rx.recv().unwrap();
        for _ in 0..10 {
            let c = count.clone();
            handle.invoke(move || {
                c.fetch_add(1, Ordering::SeqCst);
            });
        }
        handle.stop();
        assert_eq!(thread.join().unwrap(), 10);
    }
}
