//! `texture.h`: reading back the pixels that a session rendered. Borrowed
//! textures and texture rings aren't built.

use mln_util::image::PremultipliedImage;

use crate::completion::CompletionValue;
use crate::generated::types::{
    mln_buffer_view, mln_completion, mln_diagnostic, mln_render_session, mln_status, mln_texture_image_info,
    mln_texture_readback_result,
};
use crate::render_session;

/// The pixels of a readback, which its result lends.
struct Readback(PremultipliedImage);

impl CompletionValue for Readback {
    type Item = mln_texture_readback_result;

    fn lend<R>(&self, f: impl FnOnce(&[mln_texture_readback_result]) -> R) -> R {
        let image = &self.0;
        let result = mln_texture_readback_result {
            data: mln_buffer_view { data: image.data.as_ptr().cast(), size: image.data.len() },
            info: mln_texture_image_info {
                width: image.size.width,
                height: image.size.height,
                stride: image.size.width * PremultipliedImage::CHANNELS as u32,
                byte_length: image.data.len(),
            },
        };
        f(&[result])
    }
}

/// `mln_render_session_read_texture()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_render_session_read_texture(
    session: mln_render_session,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        unsafe { render_session::operation(session, completion, ::mln_runtime::RenderSession::read_texture, Readback) }
    })
}
