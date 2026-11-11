//! `base.h`: what the library reports about itself.

use crate::generated::types::{MLN_C_VERSION, mln_render_backend_flags};

/// `mln_c_version()`: the API epoch of the headers this library implements.
#[unsafe(no_mangle)]
pub extern "C" fn mln_c_version() -> u32 {
    MLN_C_VERSION
}

/// `mln_supported_render_backend_mask()`: the native graphics API that the
/// library renders through on this platform, and so the kind of surfaces and
/// devices a host hands it: Metal on Apple platforms, WebGPU in the browser,
/// and Vulkan elsewhere.
#[unsafe(no_mangle)]
pub extern "C" fn mln_supported_render_backend_mask() -> mln_render_backend_flags {
    #[cfg(target_vendor = "apple")]
    let backend = crate::generated::types::MLN_RENDER_BACKEND_FLAG_METAL;
    #[cfg(target_family = "wasm")]
    let backend = crate::generated::types::MLN_RENDER_BACKEND_FLAG_WEB_GPU;
    #[cfg(not(any(target_vendor = "apple", target_family = "wasm")))]
    let backend = crate::generated::types::MLN_RENDER_BACKEND_FLAG_VULKAN;
    backend
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_library_reports_one_backend() {
        assert_eq!(mln_supported_render_backend_mask().count_ones(), 1);
        assert_eq!(mln_c_version(), MLN_C_VERSION);
    }
}
