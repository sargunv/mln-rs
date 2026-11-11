//! `mln_completion`: the callback that receives an asynchronous function's
//! result, exactly once, with its `user_data` released after it.
//!
//! A function checks its completion first ([`PendingCompletion::new`]) and
//! accepts it only once the call succeeds, so a failed call never runs it and
//! leaves `user_data` with the caller. An accepted [`Completion`] that is
//! dropped unresolved delivers `MLN_STATUS_CANCELLED`.

use core::ffi::c_void;
use std::sync::Arc;

use crate::abi;
use crate::buffer::{LentText, Strided};
use crate::callback::{self, UserData};
use crate::delivery::{Call, Hold, Queue};
use crate::error::{Error, ErrorKind};
use crate::generated::types::{MLN_STATUS_OK, mln_buffer_view, mln_completion, mln_completion_result};

/// The generations that a map command's completion reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Generations {
    /// The first `mln_map_snapshot` generation that includes the change.
    pub(crate) snapshot: u64,
    /// The first render update generation that includes the change.
    pub(crate) render_update: u64,
}

/// A completion's result: owned data that lends the callback the C items
/// that its function's header names, as `value`, until the callback returns.
pub(crate) trait CompletionValue: Send + 'static {
    /// The C type of `value`'s items.
    type Item;

    /// Calls `f` with the items. A result without a value lends none.
    fn lend<R>(&self, f: impl FnOnce(&[Self::Item]) -> R) -> R;
}

/// No value: `value` is null.
impl CompletionValue for () {
    type Item = ();

    fn lend<R>(&self, f: impl FnOnce(&[()]) -> R) -> R {
        f(&[])
    }
}

/// Text: `value` is one `mln_buffer_view`, which ends before a NUL.
impl CompletionValue for LentText {
    type Item = mln_buffer_view;

    fn lend<R>(&self, f: impl FnOnce(&[mln_buffer_view]) -> R) -> R {
        f(&[self.view()])
    }
}

type Callback = unsafe extern "C" fn(*mut c_void, *const mln_completion_result);

struct Sink {
    callback: Callback,
    user_data: UserData,
    release: crate::generated::types::mln_user_data_release,
    queue: Option<Arc<Queue>>,
    hold: Option<Hold>,
}

impl Sink {
    fn deliver<V: CompletionValue>(self, result: Result<V, Error>, generations: Generations) {
        let Self { callback, user_data, release, queue, hold } = self;
        let run = move || {
            invoke(callback, user_data, &result, generations);
            if let Some(release) = release {
                // SAFETY: the host's release function, once, after the last
                // call of the completion.
                unsafe { release(user_data.get()) };
            }
        };
        match queue {
            None => run(),
            // A released queue refuses the call, and reported the user data
            // abandoned when it closed.
            Some(queue) => drop(queue.push_last(Call::new(run), hold)),
        }
    }
}

fn invoke<V: CompletionValue>(
    callback: Callback,
    user_data: UserData,
    result: &Result<V, Error>,
    generations: Generations,
) {
    let (status, diagnostic) = match result {
        Ok(_) => (MLN_STATUS_OK, LentText::default()),
        Err(error) => (error.status(), LentText::new(error.message())),
    };
    let call = |items: &[V::Item]| {
        let value = Strided::new(items);
        let result = mln_completion_result {
            size: size_of::<mln_completion_result>() as u32,
            status,
            value_size: value.stride,
            generation: generations.snapshot,
            render_update_generation: generations.render_update,
            diagnostic: diagnostic.view(),
            value: value.items,
            value_count: value.count,
        };
        // SAFETY: the host's completion callback with its user data, and a
        // result whose views stay valid until it returns.
        unsafe { callback(user_data.get(), &result) };
    };
    match result {
        Ok(value) => value.lend(call),
        Err(_) => call(&[]),
    }
}

/// A checked completion that its function hasn't accepted yet. Dropping it
/// runs nothing and releases nothing.
pub(crate) struct PendingCompletion(Sink);

impl PendingCompletion {
    /// Checks a host's completion.
    ///
    /// # Safety
    ///
    /// `descriptor` is null or points to a readable `mln_completion` of the
    /// size it declares.
    pub(crate) unsafe fn new(descriptor: *const mln_completion) -> Result<Self, Error> {
        if descriptor.is_null() {
            return Err(Error::invalid_argument("completion must not be null"));
        }
        // SAFETY: the caller's contract.
        let descriptor = unsafe { abi::read_sized(descriptor) }?;
        let Some(callback) = descriptor.callback else {
            return Err(Error::invalid_argument("completion requires a callback"));
        };
        let queue = callback::resolve_queue(descriptor.queue)?;
        let user_data = UserData::new(descriptor.user_data);
        // The result owns the user data, even without a release, so a queue
        // that never runs it reports the user data abandoned.
        let hold = queue.as_ref().and_then(|queue| queue.hold(user_data));
        Ok(Self(Sink { callback, user_data, release: descriptor.release_user_data, queue, hold }))
    }

    /// As [`Self::new`], for a command, which takes null when its caller
    /// doesn't need the result.
    ///
    /// # Safety
    ///
    /// As for [`Self::new`].
    pub(crate) unsafe fn optional(descriptor: *const mln_completion) -> Result<Option<Self>, Error> {
        if descriptor.is_null() {
            return Ok(None);
        }
        // SAFETY: the caller's contract.
        unsafe { Self::new(descriptor) }.map(Some)
    }

    /// Commits to running the completion once.
    pub(crate) fn accept(self) -> Completion {
        Completion(Some(self.0))
    }
}

/// An accepted completion, which runs once: with the result that
/// [`Self::complete`] gives it, or cancelled when it is dropped. Either may
/// run the host's callback on this thread, so drop no library lock while
/// holding it.
pub(crate) struct Completion(Option<Sink>);

impl Completion {
    /// Delivers the result: to the callback now, or through its queue.
    pub(crate) fn complete<V: CompletionValue>(mut self, result: Result<V, Error>, generations: Generations) {
        if let Some(sink) = self.0.take() {
            sink.deliver(result, generations);
        }
    }

    /// Takes the acceptance back, for a call that fails after it accepted.
    #[cfg_attr(not(test), expect(dead_code, reason = "no implemented function fails after it accepts"))]
    pub(crate) fn revoke(mut self) -> Option<PendingCompletion> {
        self.0.take().map(PendingCompletion)
    }
}

/// Delivers the result of a runtime command to `sink` once it arrives:
/// `finish` turns it into the completion's result and generations, on the
/// thread that resolves it. Without a sink the result goes nowhere.
pub(crate) fn forward<T: Send + 'static, V: CompletionValue>(
    completion: mln_runtime::Completion<T>,
    sink: Option<Completion>,
    finish: impl FnOnce(Result<T, mln_util::Error>) -> (Result<V, Error>, Generations) + Send + 'static,
) {
    let Some(sink) = sink else { return };
    completion.on_complete(move |result| {
        let (result, generations) = finish(result);
        sink.complete(result, generations);
    });
}

impl Drop for Completion {
    fn drop(&mut self) {
        if let Some(sink) = self.0.take() {
            let cancelled = Error::new(ErrorKind::Cancelled, "asynchronous work was abandoned");
            sink.deliver::<()>(Err(cancelled), Generations::default());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::delivery::tests::{Log, queue, queue_with_abandon};
    use crate::delivery::{mln_delivery_queue_dispatch, mln_delivery_queue_release};
    use crate::generated::types::{MLN_STATUS_CANCELLED, MLN_STATUS_NOT_FOUND, mln_status};
    use crate::lock;

    /// What a completion callback saw.
    #[derive(Debug, PartialEq)]
    struct Seen {
        status: mln_status,
        diagnostic: String,
        terminated: bool,
        values: Vec<u64>,
        value_size: u32,
        generations: Generations,
    }

    static SEEN: Mutex<Vec<(usize, Seen)>> = Mutex::new(Vec::new());

    unsafe extern "C" fn record(user_data: *mut c_void, result: *const mln_completion_result) {
        // SAFETY: the library's result, valid for this call.
        let result = unsafe { &*result };
        let diagnostic = result.diagnostic;
        // SAFETY: the lent diagnostic and its terminator.
        let bytes = unsafe { core::slice::from_raw_parts(diagnostic.data.cast::<u8>(), diagnostic.size + 1) };
        let values = (0..result.value_count)
            .map(|index| {
                // SAFETY: `value_count` items, `value_size` bytes apart.
                unsafe { result.value.cast::<u8>().add(index * result.value_size as usize).cast::<u64>().read() }
            })
            .collect();
        let seen = Seen {
            status: result.status,
            diagnostic: String::from_utf8(bytes[..diagnostic.size].to_vec()).unwrap(),
            terminated: bytes[diagnostic.size] == 0,
            values,
            value_size: result.value_size,
            generations: Generations { snapshot: result.generation, render_update: result.render_update_generation },
        };
        lock(&SEEN).push((user_data as usize, seen));
        Log::record(UserData::new(user_data), 'c');
    }

    fn seen(user_data: *mut c_void) -> Vec<Seen> {
        let mut all = lock(&SEEN);
        let (mine, others) = core::mem::take(&mut *all).into_iter().partition(|(key, _)| *key == user_data as usize);
        *all = others;
        mine.into_iter().map(|(_, seen)| seen).collect::<Vec<_>>()
    }

    struct Values(Vec<u64>);

    impl CompletionValue for Values {
        type Item = u64;

        fn lend<R>(&self, f: impl FnOnce(&[u64]) -> R) -> R {
            f(&self.0)
        }
    }

    fn descriptor(user_data: *mut c_void, queue: u64) -> mln_completion {
        mln_completion {
            size: size_of::<mln_completion>() as u32,
            callback: Some(record),
            user_data,
            release_user_data: Some(Log::release),
            queue,
        }
    }

    fn pending(descriptor: &mln_completion) -> PendingCompletion {
        // SAFETY: a local descriptor.
        unsafe { PendingCompletion::new(descriptor) }.ok().unwrap()
    }

    #[test]
    fn an_accepted_completion_runs_and_releases_exactly_once() {
        let log = Log::new();
        let user_data = log.pointer('1');
        let generations = Generations { snapshot: 3, render_update: 4 };
        pending(&descriptor(user_data, 0)).accept().complete(Ok(Values(vec![7, 8])), generations);
        assert_eq!(log.take(), "1c1r");
        assert_eq!(
            seen(user_data),
            [Seen {
                status: MLN_STATUS_OK,
                diagnostic: String::new(),
                terminated: true,
                values: vec![7, 8],
                value_size: 8,
                generations,
            }]
        );
    }

    #[test]
    fn a_failure_lends_its_terminated_message() {
        let log = Log::new();
        let user_data = log.pointer('1');
        let error = Error::new(ErrorKind::NotFound, "no such layer");
        pending(&descriptor(user_data, 0)).accept().complete::<()>(Err(error), Generations::default());
        let [seen] = <[Seen; 1]>::try_from(seen(user_data)).unwrap();
        assert_eq!(
            (seen.status, seen.diagnostic.as_str(), seen.terminated),
            (MLN_STATUS_NOT_FOUND, "no such layer", true)
        );
        assert_eq!((seen.values.len(), seen.value_size), (0, 0));
    }

    #[test]
    fn a_dropped_completion_is_cancelled() {
        let log = Log::new();
        let user_data = log.pointer('1');
        drop(pending(&descriptor(user_data, 0)).accept());
        assert_eq!(log.take(), "1c1r");
        assert_eq!(seen(user_data)[0].status, MLN_STATUS_CANCELLED);
    }

    #[test]
    fn a_rejected_completion_leaves_its_user_data_with_the_caller() {
        let log = Log::new();
        let (queue, _) = queue_with_abandon(Some(Log::abandon));
        drop(pending(&descriptor(log.pointer('1'), 0)));
        drop(pending(&descriptor(log.pointer('2'), queue)));
        let revoked = pending(&descriptor(log.pointer('3'), queue)).accept().revoke();
        drop(revoked);
        mln_delivery_queue_release(queue);
        assert_eq!(log.take(), "");
    }

    #[test]
    fn completions_check_their_members() {
        let log = Log::new();
        let check = |descriptor: &mln_completion| {
            // SAFETY: a local descriptor.
            unsafe { PendingCompletion::new(descriptor) }.err().unwrap().message().to_owned()
        };
        let valid = descriptor(log.pointer('1'), 0);
        assert_eq!(check(&mln_completion { size: 4, ..valid }), "mln_completion.size is too small");
        assert_eq!(check(&mln_completion { callback: None, ..valid }), "completion requires a callback");
        let released = queue();
        mln_delivery_queue_release(released);
        assert!(check(&mln_completion { queue: released, ..valid }).contains("stale"));
        // SAFETY: null.
        unsafe {
            assert_eq!(
                PendingCompletion::new(core::ptr::null()).err().unwrap().message(),
                "completion must not be null"
            );
            assert!(PendingCompletion::optional(core::ptr::null()).unwrap().is_none());
        }
        assert_eq!(log.take(), "");
    }

    #[test]
    fn a_queued_completion_runs_in_dispatch_with_a_copy_of_its_value() {
        let log = Log::new();
        let queue = queue();
        let user_data = log.pointer('1');
        pending(&descriptor(user_data, queue)).accept().complete(Ok(Values(vec![5])), Generations::default());
        assert_eq!(log.take(), "");
        // SAFETY: a null diagnostic.
        assert_eq!(unsafe { mln_delivery_queue_dispatch(queue, core::ptr::null_mut()) }, MLN_STATUS_OK);
        assert_eq!(log.take(), "1c1r");
        assert_eq!(seen(user_data)[0].values, [5]);
        mln_delivery_queue_release(queue);
    }

    #[test]
    fn a_released_queue_abandons_its_completions() {
        let log = Log::new();
        let (queue, _) = queue_with_abandon(Some(Log::abandon));
        // Queued, and accepted but unresolved; neither has a release.
        let queued = mln_completion { release_user_data: None, ..descriptor(log.pointer('1'), queue) };
        pending(&queued).accept().complete(Ok(()), Generations::default());
        let in_flight =
            pending(&mln_completion { release_user_data: None, ..descriptor(log.pointer('2'), queue) }).accept();
        mln_delivery_queue_release(queue);
        let mut reports: Vec<char> = log.take().chars().collect();
        reports.sort_unstable();
        assert_eq!(reports.into_iter().collect::<String>(), "12aa");
        drop(in_flight);
        assert_eq!(log.take(), "");
    }
}
