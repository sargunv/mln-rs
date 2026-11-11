//! What every status-returning C entry point shares: the [`export!`] body that
//! turns a `Result` into an `mln_status` and a diagnostic, and the scopes in
//! which a thread may not call back into the library.

use core::cell::Cell;
use core::marker::PhantomData;

use crate::diagnostic;
use crate::error::Error;
use crate::generated::types::{MLN_STATUS_OK, mln_diagnostic, mln_status};

/// The body of a C entry point that returns `mln_status`:
///
/// ```ignore
/// export!(out_diagnostic, non_null(out_queue), {
///     // ... returns Result<(), Error>, so `?` works
/// })
/// ```
///
/// It fails with `MLN_STATUS_INVALID_ARGUMENT` when a pointer that
/// `non_null(...)` lists is null, and with `MLN_STATUS_INVALID_STATE`, without
/// running the body, inside a [`ForbidReentry`] scope. Then it writes the
/// error's message to `out_diagnostic`, or clears it on success, and returns
/// the status.
macro_rules! export {
    ($diagnostic:ident, $(non_null($($pointer:ident),+ $(,)?),)? $body:block) => {{
        let result: ::core::result::Result<(), $crate::error::Error> = match $crate::export::refusal() {
            ::core::option::Option::Some(error) => ::core::result::Result::Err(error),
            ::core::option::Option::None => (|| -> ::core::result::Result<(), $crate::error::Error> {
                $($(
                    if $pointer.is_null() {
                        return ::core::result::Result::Err($crate::error::Error::invalid_argument(
                            concat!(stringify!($pointer), " must not be null"),
                        ));
                    }
                )+)?
                $body
            })(),
        };
        // SAFETY: every entry point's contract: `out_diagnostic` is null, or
        // points to an `mln_diagnostic` whose `size` is initialized and
        // declares bytes the caller can write.
        unsafe { $crate::export::finish($diagnostic, result) }
    }};
}

/// Writes `result`'s diagnostic, an empty one on success, and returns its
/// status.
///
/// # Safety
///
/// As for [`diagnostic::write`].
pub(crate) unsafe fn finish(out_diagnostic: *mut mln_diagnostic, result: Result<(), Error>) -> mln_status {
    match result {
        Ok(()) => {
            // SAFETY: the caller's contract.
            unsafe { diagnostic::write(out_diagnostic, "") };
            MLN_STATUS_OK
        }
        Err(error) => {
            // SAFETY: the caller's contract.
            unsafe { diagnostic::write(out_diagnostic, error.message()) };
            error.status()
        }
    }
}

thread_local! {
    static REFUSAL: Cell<Option<&'static str>> = const { Cell::new(None) };
}

/// A scope around a host callback that must not call back into the library,
/// because the thread holds something that the call could need again, such as
/// a lock that isn't reentrant. Until it ends, every [`export!`] entry point on
/// this thread fails with `MLN_STATUS_INVALID_STATE` and its diagnostic.
#[cfg_attr(not(test), expect(dead_code, reason = "no implemented callback forbids reentry yet"))]
pub(crate) struct ForbidReentry {
    previous: Option<&'static str>,
    // The scope belongs to the thread that entered it.
    _thread: PhantomData<*const ()>,
}

#[cfg_attr(not(test), expect(dead_code, reason = "no implemented callback forbids reentry yet"))]
impl ForbidReentry {
    pub(crate) fn enter(diagnostic: &'static str) -> Self {
        Self { previous: REFUSAL.replace(Some(diagnostic)), _thread: PhantomData }
    }
}

impl Drop for ForbidReentry {
    fn drop(&mut self) {
        REFUSAL.set(self.previous);
    }
}

/// The failure of a call inside a [`ForbidReentry`] scope, if any.
pub(crate) fn refusal() -> Option<Error> {
    REFUSAL.get().map(Error::invalid_state)
}

#[cfg(test)]
mod tests {
    use core::ffi::CStr;

    use super::*;
    use crate::generated::types::{MLN_STATUS_INVALID_ARGUMENT, MLN_STATUS_INVALID_STATE, MLN_STATUS_NOT_FOUND};

    fn diagnostic() -> Box<mln_diagnostic> {
        let mut diagnostic = Box::new(mln_diagnostic { size: size_of::<mln_diagnostic>() as u32, message: [0; 4096] });
        diagnostic.message[0] = b'x' as _;
        diagnostic.message[1] = 0;
        diagnostic
    }

    fn message(diagnostic: &mln_diagnostic) -> &str {
        // SAFETY: `message` holds a NUL within its capacity.
        unsafe { CStr::from_ptr(diagnostic.message.as_ptr()) }.to_str().unwrap()
    }

    fn entry(pointer: *const u8, fail: bool, out_diagnostic: *mut mln_diagnostic) -> mln_status {
        export!(out_diagnostic, non_null(pointer), {
            if fail {
                return Err(Error::new(crate::error::ErrorKind::NotFound, "nothing is there"));
            }
            Ok(())
        })
    }

    #[test]
    fn success_clears_the_diagnostic() {
        let mut out = diagnostic();
        assert_eq!(entry(&0, false, &mut *out), MLN_STATUS_OK);
        assert_eq!(message(&out), "");
    }

    #[test]
    fn a_failure_writes_its_status_and_message() {
        let mut out = diagnostic();
        assert_eq!(entry(&0, true, &mut *out), MLN_STATUS_NOT_FOUND);
        assert_eq!(message(&out), "nothing is there");
        assert_eq!(entry(&0, true, core::ptr::null_mut()), MLN_STATUS_NOT_FOUND);
    }

    #[test]
    fn a_listed_null_pointer_is_an_invalid_argument() {
        let mut out = diagnostic();
        assert_eq!(entry(core::ptr::null(), false, &mut *out), MLN_STATUS_INVALID_ARGUMENT);
        assert_eq!(message(&out), "pointer must not be null");
    }

    #[test]
    fn a_forbidden_scope_refuses_calls_until_it_ends() {
        let mut out = diagnostic();
        {
            let _outer = ForbidReentry::enter("outer callback");
            {
                let _inner = ForbidReentry::enter("inner callback");
                assert_eq!(entry(&0, false, &mut *out), MLN_STATUS_INVALID_STATE);
                assert_eq!(message(&out), "inner callback");
            }
            assert_eq!(entry(&0, false, &mut *out), MLN_STATUS_INVALID_STATE);
            assert_eq!(message(&out), "outer callback");
            // Another thread isn't in the scope.
            std::thread::spawn(|| assert_eq!(entry(&0, false, core::ptr::null_mut()), MLN_STATUS_OK)).join().unwrap();
        }
        assert_eq!(entry(&0, false, &mut *out), MLN_STATUS_OK);
    }
}
