//! The failure that a C entry point reports as an `mln_status` and a
//! diagnostic message.

use std::borrow::Cow;
use std::fmt;

use crate::generated::types::{
    MLN_STATUS_ALREADY_EXISTS, MLN_STATUS_BUSY, MLN_STATUS_CANCELLED, MLN_STATUS_INVALID_ARGUMENT,
    MLN_STATUS_INVALID_STATE, MLN_STATUS_NATIVE_ERROR, MLN_STATUS_NOT_FOUND, MLN_STATUS_TARGET_LOST,
    MLN_STATUS_UNSUPPORTED, MLN_STATUS_WRONG_THREAD, mln_status,
};

/// What went wrong, one kind for each failing `mln_status` but
/// `MLN_STATUS_WRONG_THREAD`, which only the C layer reports
/// ([`Error::wrong_thread`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ErrorKind {
    InvalidArgument,
    InvalidState,
    Unsupported,
    Native,
    Cancelled,
    Busy,
    TargetLost,
    NotFound,
    AlreadyExists,
}

impl ErrorKind {
    pub(crate) const fn status(self) -> mln_status {
        match self {
            Self::InvalidArgument => MLN_STATUS_INVALID_ARGUMENT,
            Self::InvalidState => MLN_STATUS_INVALID_STATE,
            Self::Unsupported => MLN_STATUS_UNSUPPORTED,
            Self::Native => MLN_STATUS_NATIVE_ERROR,
            Self::Cancelled => MLN_STATUS_CANCELLED,
            Self::Busy => MLN_STATUS_BUSY,
            Self::TargetLost => MLN_STATUS_TARGET_LOST,
            Self::NotFound => MLN_STATUS_NOT_FOUND,
            Self::AlreadyExists => MLN_STATUS_ALREADY_EXISTS,
        }
    }
}

impl From<mln_util::ErrorKind> for ErrorKind {
    fn from(kind: mln_util::ErrorKind) -> Self {
        use mln_util::ErrorKind as Core;
        match kind {
            Core::InvalidArgument => Self::InvalidArgument,
            Core::InvalidState => Self::InvalidState,
            Core::Unsupported => Self::Unsupported,
            Core::Native => Self::Native,
            Core::Cancelled => Self::Cancelled,
            Core::Busy => Self::Busy,
            Core::TargetLost => Self::TargetLost,
            Core::NotFound => Self::NotFound,
            Core::AlreadyExists => Self::AlreadyExists,
        }
    }
}

impl From<ErrorKind> for mln_status {
    fn from(kind: ErrorKind) -> Self {
        kind.status()
    }
}

/// A failure: the status a C function returns or a completion carries, and
/// the message its diagnostic shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Error {
    status: mln_status,
    message: Cow<'static, str>,
}

impl Error {
    pub(crate) fn new(kind: ErrorKind, message: impl Into<Cow<'static, str>>) -> Self {
        Self { status: kind.status(), message: message.into() }
    }

    pub(crate) fn invalid_argument(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(ErrorKind::InvalidArgument, message)
    }

    pub(crate) fn invalid_state(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(ErrorKind::InvalidState, message)
    }

    pub(crate) fn native(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(ErrorKind::Native, message)
    }

    /// A call from a thread that can't make it.
    #[cfg_attr(not(test), expect(dead_code, reason = "no implemented function is bound to a thread yet"))]
    pub(crate) fn wrong_thread(message: impl Into<Cow<'static, str>>) -> Self {
        Self { status: MLN_STATUS_WRONG_THREAD, message: message.into() }
    }

    /// The status, never `MLN_STATUS_OK`.
    pub(crate) fn status(&self) -> mln_status {
        self.status
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

impl From<mln_util::Error> for Error {
    fn from(error: mln_util::Error) -> Self {
        Self::new(error.kind.into(), error.message)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_kind_maps_to_its_status() {
        let kinds = [
            (ErrorKind::InvalidArgument, MLN_STATUS_INVALID_ARGUMENT),
            (ErrorKind::InvalidState, MLN_STATUS_INVALID_STATE),
            (ErrorKind::Unsupported, MLN_STATUS_UNSUPPORTED),
            (ErrorKind::Native, MLN_STATUS_NATIVE_ERROR),
            (ErrorKind::Cancelled, MLN_STATUS_CANCELLED),
            (ErrorKind::Busy, MLN_STATUS_BUSY),
            (ErrorKind::TargetLost, MLN_STATUS_TARGET_LOST),
            (ErrorKind::NotFound, MLN_STATUS_NOT_FOUND),
            (ErrorKind::AlreadyExists, MLN_STATUS_ALREADY_EXISTS),
        ];
        for (kind, status) in kinds {
            assert_eq!(mln_status::from(kind), status);
            assert_eq!(Error::new(kind, "message").status(), status);
        }
        assert_eq!(Error::wrong_thread("elsewhere").status(), MLN_STATUS_WRONG_THREAD);
    }

    #[test]
    fn a_core_error_keeps_its_kind_and_message() {
        let error = Error::from(mln_util::Error::not_found("no layer"));
        assert_eq!((error.status(), error.message()), (MLN_STATUS_NOT_FOUND, "no layer"));
    }
}
