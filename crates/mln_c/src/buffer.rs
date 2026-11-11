//! Bytes and text across the C boundary: `mln_buffer_view` inputs, text that
//! the library lends to callbacks, caller-sized output buffers, and arrays
//! whose item size the library reports.

use core::ffi::{c_char, c_void};

use crate::error::Error;
use crate::generated::types::mln_buffer_view;

/// The bytes that a caller's view names.
///
/// # Safety
///
/// `view.data` is null or points to `view.size` bytes that stay readable and
/// unchanged for `'a`.
#[cfg_attr(not(test), expect(dead_code, reason = "no implemented function takes a buffer view yet"))]
pub(crate) unsafe fn bytes<'a>(view: mln_buffer_view, name: &str) -> Result<&'a [u8], Error> {
    if view.size == 0 {
        return Ok(&[]);
    }
    if view.data.is_null() {
        return Err(Error::invalid_argument(format!("{name} is null with a nonzero size")));
    }
    // SAFETY: non-null, and the caller's contract.
    Ok(unsafe { core::slice::from_raw_parts(view.data.cast::<u8>(), view.size) })
}

/// The UTF-8 text that a caller's view names.
///
/// # Safety
///
/// As for [`bytes`].
#[cfg_attr(not(test), expect(dead_code, reason = "no implemented function takes text yet"))]
pub(crate) unsafe fn text<'a>(view: mln_buffer_view, name: &str) -> Result<&'a str, Error> {
    // SAFETY: the caller's contract.
    let bytes = unsafe { bytes(view, name) }?;
    core::str::from_utf8(bytes).map_err(|error| Error::invalid_argument(format!("{name} is not valid UTF-8: {error}")))
}

/// Text that the library lends to a callback: its view ends before a NUL
/// terminator, which C callers may rely on, and an empty one still points at
/// that terminator rather than null.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LentText(Vec<u8>);

impl LentText {
    pub(crate) fn new(text: &str) -> Self {
        let mut bytes = Vec::with_capacity(text.len() + 1);
        bytes.extend_from_slice(text.as_bytes());
        bytes.push(0);
        Self(bytes)
    }

    /// The view, valid while `self` is.
    pub(crate) fn view(&self) -> mln_buffer_view {
        match self.0.split_last() {
            Some((_, text)) => mln_buffer_view { data: self.0.as_ptr().cast(), size: text.len() },
            None => mln_buffer_view { data: c"".as_ptr().cast(), size: 0 },
        }
    }
}

/// Writes `text` and a NUL to a caller's buffer of `capacity` bytes when it
/// has room for both, and its length without the NUL to `out_length` always,
/// so a caller with too small a buffer learns the size to retry with.
///
/// # Safety
///
/// `out` is null or points to `capacity` writable bytes, and `out_length` is
/// null or points to a writable `usize`.
#[cfg_attr(not(test), expect(dead_code, reason = "no implemented function writes text out yet"))]
pub(crate) unsafe fn write_text(
    text: &str,
    out: *mut c_char,
    capacity: usize,
    out_length: *mut usize,
    names: (&str, &str),
) -> Result<(), Error> {
    let (out_name, length_name) = names;
    if out_length.is_null() {
        return Err(Error::invalid_argument(format!("{length_name} must not be null")));
    }
    if out.is_null() && capacity != 0 {
        return Err(Error::invalid_argument(format!("{out_name} is null with a nonzero capacity")));
    }
    // SAFETY: non-null, and the caller's contract.
    unsafe { out_length.write(text.len()) };
    if capacity > text.len() {
        // SAFETY: `text.len() + 1 <= capacity` bytes, which the caller's
        // contract makes writable, and `text` is ours, so they don't overlap.
        unsafe {
            core::ptr::copy_nonoverlapping(text.as_ptr(), out.cast::<u8>(), text.len());
            out.add(text.len()).write(0);
        }
    }
    Ok(())
}

/// An array that the library lends, with the stride that `MLN_STRIDED_AT`
/// steps by: the item size of this library, which a caller compiled against
/// older headers may know as smaller. Empty, it is null with a zero stride.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Strided {
    pub(crate) items: *const c_void,
    pub(crate) count: usize,
    pub(crate) stride: u32,
}

impl Strided {
    pub(crate) fn new<T>(items: &[T]) -> Self {
        if items.is_empty() {
            return Self { items: core::ptr::null(), count: 0, stride: 0 };
        }
        Self { items: items.as_ptr().cast(), count: items.len(), stride: size_of::<T>() as u32 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(bytes: &[u8]) -> mln_buffer_view {
        mln_buffer_view { data: bytes.as_ptr().cast(), size: bytes.len() }
    }

    #[test]
    fn views_become_bytes_and_text() {
        // SAFETY: each view names a live slice or nothing.
        unsafe {
            assert_eq!(text(view(b"places"), "json").unwrap(), "places");
            assert_eq!(text(mln_buffer_view { data: core::ptr::null(), size: 0 }, "json").unwrap(), "");
            let null = bytes(mln_buffer_view { data: core::ptr::null(), size: 3 }, "data").unwrap_err();
            assert_eq!(null.message(), "data is null with a nonzero size");
            let invalid = text(view(b"\xff"), "json").unwrap_err();
            assert!(invalid.message().starts_with("json is not valid UTF-8"), "{invalid}");
        }
    }

    #[test]
    fn lent_text_is_terminated() {
        for text in ["", "message"] {
            let lent = LentText::new(text);
            let view = lent.view();
            assert!(!view.data.is_null());
            assert_eq!(view.size, text.len());
            // SAFETY: the view's bytes and the terminator after them are live.
            let terminated = unsafe { core::slice::from_raw_parts(view.data.cast::<u8>(), view.size + 1) };
            assert_eq!(&terminated[..text.len()], text.as_bytes());
            assert_eq!(terminated[text.len()], 0);
        }
        let empty = LentText::default().view();
        assert!(!empty.data.is_null());
        assert_eq!(empty.size, 0);
    }

    #[test]
    fn text_out_reports_its_full_length() {
        let mut out = [b'x' as c_char; 8];
        let mut length = 0;
        // SAFETY: `out` has the capacity passed, and `length` is a local.
        unsafe {
            write_text("toolong!", out.as_mut_ptr(), out.len(), &mut length, ("out", "out_length")).unwrap();
            assert_eq!(length, 8);
            assert_eq!(out[0], b'x' as c_char);

            write_text("fits", out.as_mut_ptr(), out.len(), &mut length, ("out", "out_length")).unwrap();
            assert_eq!(length, 4);
            assert_eq!(&out[..5], b"fits\0".map(|byte| byte as c_char));

            write_text("measure", core::ptr::null_mut(), 0, &mut length, ("out", "out_length")).unwrap();
            assert_eq!(length, 7);
            assert!(write_text("x", core::ptr::null_mut(), 4, &mut length, ("out", "out_length")).is_err());
            assert!(write_text("x", out.as_mut_ptr(), 4, core::ptr::null_mut(), ("out", "out_length")).is_err());
        }
    }

    #[test]
    fn strided_reports_the_item_size() {
        let items = [1u64, 2, 3];
        let strided = Strided::new(&items);
        assert_eq!((strided.count, strided.stride), (3, 8));
        let empty = Strided::new::<u64>(&[]);
        assert!(empty.items.is_null());
        assert_eq!((empty.count, empty.stride), (0, 0));
    }
}
