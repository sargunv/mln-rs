//! `render_target.h`: what every render target shares.

use crate::error::Error;
use crate::generated::types::{mln_diagnostic, mln_logical_extent, mln_open_gl_context_provider_flags, mln_status};

/// The pixels that a logical extent covers: each dimension times the scale
/// factor, rounded up so that a target never has a fraction of a pixel too
/// few.
pub(crate) fn physical_size(extent: mln_logical_extent) -> Result<(u32, u32), Error> {
    let mln_logical_extent { width, height, scale_factor } = extent;
    if width == 0 || height == 0 || !scale_factor.is_finite() || scale_factor <= 0.0 {
        return Err(Error::invalid_argument("extent dimensions and scale_factor must be positive"));
    }
    let physical = |logical: u32| {
        let pixels = (f64::from(logical) * scale_factor).ceil();
        // `as` saturates, so the range check comes first.
        (pixels <= f64::from(u32::MAX)).then_some(pixels as u32)
    };
    match (physical(width), physical(height)) {
        (Some(width), Some(height)) => Ok((width, height)),
        _ => Err(Error::invalid_argument("scaled extent dimensions are too large")),
    }
}

/// `mln_logical_extent_physical_size()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_logical_extent_physical_size(
    extent: mln_logical_extent,
    out_width: *mut u32,
    out_height: *mut u32,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, non_null(out_width, out_height), {
        let (width, height) = physical_size(extent)?;
        // SAFETY: both are non-null, and the caller's contract.
        unsafe {
            out_width.write(width);
            out_height.write(height);
        }
        Ok(())
    })
}

/// `mln_open_gl_supported_context_provider_mask()`: none, since the library
/// doesn't render with OpenGL.
#[unsafe(no_mangle)]
pub extern "C" fn mln_open_gl_supported_context_provider_mask() -> mln_open_gl_context_provider_flags {
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::types::{MLN_STATUS_INVALID_ARGUMENT, MLN_STATUS_OK};

    fn call(width: u32, height: u32, scale_factor: f64) -> (mln_status, u32, u32) {
        let (mut out_width, mut out_height) = (7, 7);
        let extent = mln_logical_extent { width, height, scale_factor };
        // SAFETY: local outputs and a null diagnostic.
        let status =
            unsafe { mln_logical_extent_physical_size(extent, &mut out_width, &mut out_height, core::ptr::null_mut()) };
        (status, out_width, out_height)
    }

    #[test]
    fn physical_sizes_round_up_and_reject_overflow() {
        assert_eq!(call(3, 5, 1.5), (MLN_STATUS_OK, 5, 8));
        assert_eq!(call(1, 3, 0.25), (MLN_STATUS_OK, 1, 1));
        assert_eq!(call(u32::MAX, 1, 1.0), (MLN_STATUS_OK, u32::MAX, 1));
        // A failed call leaves the outputs alone.
        assert_eq!(call(0x8000_0000, 1, 2.0), (MLN_STATUS_INVALID_ARGUMENT, 7, 7));
        assert_eq!(call(1, 1, f64::NAN), (MLN_STATUS_INVALID_ARGUMENT, 7, 7));
        assert_eq!(call(0, 1, 1.0), (MLN_STATUS_INVALID_ARGUMENT, 7, 7));
        let mut height = 0;
        let extent = mln_logical_extent { width: 1, height: 1, scale_factor: 1.0 };
        // SAFETY: a null output, a local one, and a null diagnostic.
        let status = unsafe {
            mln_logical_extent_physical_size(extent, core::ptr::null_mut(), &mut height, core::ptr::null_mut())
        };
        assert_eq!(status, MLN_STATUS_INVALID_ARGUMENT);
    }

    #[test]
    fn no_open_gl_provider_is_built() {
        assert_eq!(mln_open_gl_supported_context_provider_mask(), 0);
    }
}
