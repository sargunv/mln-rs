//! `offscreen.h`: the GPU devices that offscreen sessions render on, and the
//! attach of an offscreen session.

use ::mln_runtime::{OffscreenDevice, OffscreenDeviceOptions, RenderTarget};
use mln_util::size::Size;

use crate::abi::{self, Fields};
use crate::completion::{self, Generations, PendingCompletion};
use crate::error::{Error, ErrorKind};
use crate::generated::types::{
    MLN_OFFSCREEN_DEVICE_OPTION_ADAPTER_INDEX, MLN_OFFSCREEN_DEVICE_OPTION_CLIENT_API,
    MLN_OFFSCREEN_DEVICE_OPTION_SINGLE_CONTEXT, MLN_OPEN_GL_CLIENT_API_GL, MLN_OPEN_GL_CLIENT_API_GLES, mln_completion,
    mln_diagnostic, mln_map, mln_offscreen_descriptor, mln_offscreen_device, mln_offscreen_device_options,
    mln_render_session, mln_render_session_attach_options, mln_status,
};
use crate::handles::{self, HandleKind, HandleTable};
use crate::{map, render_session};

abi::versioned!(mln_offscreen_device_options, mln_offscreen_descriptor);

static DEVICES: HandleTable<OffscreenDevice> = HandleTable::new(HandleKind::OffscreenDevice);

/// The runtime layer's options for a caller's `mln_offscreen_device_options`.
///
/// # Safety
///
/// `options` is null or points to a readable struct of the size it declares.
unsafe fn device_options(options: *const mln_offscreen_device_options) -> Result<OffscreenDeviceOptions, Error> {
    if options.is_null() {
        return Ok(OffscreenDeviceOptions::default());
    }
    // SAFETY: non-null, and the caller's contract.
    let options = unsafe { abi::read_sized(options) }?;
    let fields = Fields::new(
        options.fields,
        MLN_OFFSCREEN_DEVICE_OPTION_ADAPTER_INDEX
            | MLN_OFFSCREEN_DEVICE_OPTION_SINGLE_CONTEXT
            | MLN_OFFSCREEN_DEVICE_OPTION_CLIENT_API,
        "mln_offscreen_device_options.fields",
    )?;
    // Only an EGL device reads `single_context`, so it changes nothing here.
    if let Some(client_api) = fields.get(MLN_OFFSCREEN_DEVICE_OPTION_CLIENT_API, options.client_api) {
        if client_api != MLN_OPEN_GL_CLIENT_API_GL && client_api != MLN_OPEN_GL_CLIENT_API_GLES {
            return Err(Error::invalid_argument("mln_offscreen_device_options.client_api must name an OpenGL API"));
        }
        return Err(Error::new(
            ErrorKind::Unsupported,
            "this library renders with no OpenGL API, so it takes no client_api",
        ));
    }
    let adapter_index = fields.get(MLN_OFFSCREEN_DEVICE_OPTION_ADAPTER_INDEX, options.adapter_index);
    Ok(OffscreenDeviceOptions { adapter_index: adapter_index.map(|index| index as usize) })
}

/// The status that `offscreen.h` gives a device that can't be created: a
/// missing GPU is a native error there, as the platform's refusal is.
fn device_error(error: mln_util::Error) -> Error {
    match error.kind {
        mln_util::ErrorKind::NotFound => Error::native(error.message),
        _ => error.into(),
    }
}

/// `mln_offscreen_device_create()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_offscreen_device_create(
    options: *const mln_offscreen_device_options,
    out_device: *mut mln_offscreen_device,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let options = unsafe { device_options(options) }?;
        // SAFETY: the caller's contract.
        let out_device = unsafe { handles::out_handle(out_device, "out_device") }?;
        let device = OffscreenDevice::new(options).map_err(device_error)?;
        *out_device = DEVICES.insert(device.into())?;
        Ok(())
    })
}

/// `mln_offscreen_device_release()`. Sessions on the device hold it until
/// they end.
#[unsafe(no_mangle)]
pub extern "C" fn mln_offscreen_device_release(device: mln_offscreen_device) {
    if device != handles::NULL {
        let _ = DEVICES.remove(device);
    }
}

/// `mln_map_attach_offscreen()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_attach_offscreen(
    map: mln_map,
    descriptor: *const mln_offscreen_descriptor,
    options: *const mln_render_session_attach_options,
    device: mln_offscreen_device,
    out_session: *mut mln_render_session,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        let map = map::resolve(map)?;
        let device = if device == handles::NULL { None } else { Some(OffscreenDevice::clone(&*DEVICES.get(device)?)) };
        // SAFETY: the caller's contract.
        let descriptor = unsafe { abi::read_sized(descriptor) }?;
        let size = descriptor.logical_size;
        if size.width == 0 || size.height == 0 {
            return Err(Error::invalid_argument("offscreen dimensions must be positive"));
        }
        // SAFETY: the caller's contract.
        let options = unsafe { render_session::attach_options(options) }?;
        // SAFETY: the caller's contract.
        let out_session = unsafe { handles::out_handle(out_session, "out_session") }?;
        // SAFETY: the caller's contract.
        let pending = unsafe { PendingCompletion::new(completion) }?;
        options.require_core_worker("an offscreen session renders on a core worker")?;
        options.refuse_queue_lock("an offscreen session takes no queue lock")?;
        let target = RenderTarget::Offscreen { size: Size::new(size.width, size.height), device };
        let (handle, attached) = render_session::attach(&map, target, options)?;
        // The completion may run before this call returns, and the caller may
        // read the handle there.
        *out_session = handle;
        completion::forward(attached, Some(pending.accept()), |result| {
            (result.map_err(device_error), Generations::default())
        });
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::types::{MLN_STATUS_INVALID_ARGUMENT, MLN_STATUS_UNSUPPORTED};

    fn options(fields: u32, client_api: u32) -> mln_offscreen_device_options {
        mln_offscreen_device_options {
            size: size_of::<mln_offscreen_device_options>() as u32,
            fields,
            adapter_index: 3,
            single_context: true,
            client_api,
        }
    }

    #[test]
    fn device_options_take_an_adapter_and_refuse_open_gl() {
        // SAFETY: local options.
        unsafe {
            let index = device_options(&options(MLN_OFFSCREEN_DEVICE_OPTION_ADAPTER_INDEX, 0)).unwrap();
            assert_eq!(index.adapter_index, Some(3));
            let unset = device_options(&options(MLN_OFFSCREEN_DEVICE_OPTION_SINGLE_CONTEXT, 0)).unwrap();
            assert_eq!(unset.adapter_index, None);
            assert_eq!(device_options(core::ptr::null()).unwrap(), OffscreenDeviceOptions::default());

            let gl = device_options(&options(MLN_OFFSCREEN_DEVICE_OPTION_CLIENT_API, MLN_OPEN_GL_CLIENT_API_GL));
            assert_eq!(gl.unwrap_err().status(), MLN_STATUS_UNSUPPORTED);
            let unnamed = device_options(&options(MLN_OFFSCREEN_DEVICE_OPTION_CLIENT_API, 0));
            assert_eq!(unnamed.unwrap_err().status(), MLN_STATUS_INVALID_ARGUMENT);
            let unknown = device_options(&options(1 << 31, 0));
            assert_eq!(unknown.unwrap_err().status(), MLN_STATUS_INVALID_ARGUMENT);
        }
    }

    #[test]
    fn a_missing_gpu_is_a_native_error() {
        let error = device_error(mln_util::Error::not_found("no adapter 9"));
        assert_eq!(error.status(), crate::generated::types::MLN_STATUS_NATIVE_ERROR);
        assert_eq!(device_error(mln_util::Error::unsupported("no GPU")).status(), MLN_STATUS_UNSUPPORTED);
    }
}
