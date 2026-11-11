//! `runtime.h`: what applies to every runtime in the process, such as the
//! network status.

use mln_storage::network_status::{NetworkStatus, Status};

use crate::error::Error;
use crate::generated::types::{
    MLN_NETWORK_STATUS_OFFLINE, MLN_NETWORK_STATUS_ONLINE, mln_diagnostic, mln_network_status, mln_status,
};

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
mod tests {
    use super::*;
    use crate::diagnostic::tests::{diagnostic, message};
    use crate::generated::types::{MLN_STATUS_INVALID_ARGUMENT, MLN_STATUS_OK};

    fn current() -> mln_network_status {
        let mut status = 0;
        // SAFETY: a local output and a null diagnostic.
        assert_eq!(unsafe { mln_network_get_status(&mut status, core::ptr::null_mut()) }, MLN_STATUS_OK);
        status
    }

    #[test]
    fn the_network_status_changes_and_rejects_a_value_that_names_no_status() {
        // SAFETY: local and null diagnostics, and a null output.
        unsafe {
            assert_eq!(mln_network_set_status(MLN_NETWORK_STATUS_OFFLINE, core::ptr::null_mut()), MLN_STATUS_OK);
            assert_eq!(current(), MLN_NETWORK_STATUS_OFFLINE);
            let mut out = diagnostic();
            assert_eq!(mln_network_set_status(0, &mut *out), MLN_STATUS_INVALID_ARGUMENT);
            assert!(message(&out).contains("network status"));
            assert_eq!(current(), MLN_NETWORK_STATUS_OFFLINE);
            assert_eq!(mln_network_get_status(core::ptr::null_mut(), &mut *out), MLN_STATUS_INVALID_ARGUMENT);
            assert!(message(&out).contains("out_status"));
            assert_eq!(mln_network_set_status(MLN_NETWORK_STATUS_ONLINE, core::ptr::null_mut()), MLN_STATUS_OK);
            assert_eq!(current(), MLN_NETWORK_STATUS_ONLINE);
        }
    }
}
