//! The versioning rules of the headers' structs: a leading `size` that says
//! how much of the struct the caller knows, `fields` bits that say which
//! option members are set, and enum values from a closed range.

use core::ops::RangeInclusive;

use crate::error::Error;
use crate::generated::types::{mln_completion, mln_delivery_queue_options};

/// A header struct that starts with a `uint32_t size`.
///
/// # Safety
///
/// The type is a `#[repr(C)]` struct whose first field is `size: u32`, and
/// every bit pattern of zeros is a valid value of it, as for the headers'
/// structs of integers, pointers and nullable function pointers.
pub(crate) unsafe trait Versioned: Copy {
    /// The C type's name, for diagnostics.
    const NAME: &'static str;
    /// The smallest `size` that a caller may declare: the struct's size when
    /// the headers first published it.
    const FIRST_SIZE: usize = size_of::<Self>();
}

macro_rules! versioned {
    ($($ty:ident),+ $(,)?) => {$(
        const _: () = assert!(core::mem::offset_of!($ty, size) == 0);
        // SAFETY: a bindgen struct of the headers, whose first field is
        // `size` (asserted above) and whose fields are integers, pointers and
        // nullable function pointers.
        unsafe impl Versioned for $ty {
            const NAME: &'static str = stringify!($ty);
        }
    )+};
}

versioned!(mln_completion, mln_delivery_queue_options);

/// The `size` that `pointer`'s struct declares, checked against
/// [`Versioned::FIRST_SIZE`].
///
/// # Safety
///
/// `pointer` is non-null, aligned, and its first four bytes are initialized.
unsafe fn declared_size<T: Versioned>(pointer: *const T) -> Result<usize, Error> {
    // SAFETY: the caller's contract; `size` is the first field.
    let size = unsafe { pointer.cast::<u32>().read() } as usize;
    if size < T::FIRST_SIZE {
        return Err(Error::invalid_argument(format!("{}.size is too small", T::NAME)));
    }
    Ok(size)
}

/// Copies a caller's struct: the members its `size` covers, with every later
/// member zero, as a caller compiled against older headers expects.
///
/// # Safety
///
/// `pointer` is null, or points to a readable struct of the `size` it
/// declares.
pub(crate) unsafe fn read_sized<T: Versioned>(pointer: *const T) -> Result<T, Error> {
    if pointer.is_null() {
        return Err(Error::invalid_argument(format!("{} must not be null", T::NAME)));
    }
    // SAFETY: non-null, and the caller's contract.
    let size = unsafe { declared_size(pointer) }?;
    // SAFETY: zeros are a valid `T` (the trait's contract).
    let mut value: T = unsafe { core::mem::zeroed() };
    // SAFETY: `min(size, size_of::<T>())` bytes are readable from the caller's
    // struct and writable in `value`, and the two don't overlap.
    unsafe {
        core::ptr::copy_nonoverlapping(pointer.cast::<u8>(), (&raw mut value).cast::<u8>(), size.min(size_of::<T>()));
    }
    Ok(value)
}

/// Writes `value` to a caller's struct, as much of it as the caller's `size`
/// declares, which stays as the caller set it.
///
/// # Safety
///
/// `pointer` is null, or points to a writable struct of the `size` it
/// declares, with `size` initialized.
#[cfg_attr(not(test), expect(dead_code, reason = "no implemented function writes a versioned struct yet"))]
pub(crate) unsafe fn write_sized<T: Versioned>(pointer: *mut T, value: &T) -> Result<(), Error> {
    if pointer.is_null() {
        return Err(Error::invalid_argument(format!("{} must not be null", T::NAME)));
    }
    // SAFETY: non-null, and the caller's contract.
    let size = unsafe { declared_size(pointer) }?;
    let length = size.min(size_of::<T>());
    let skip = size_of::<u32>();
    // SAFETY: the bytes after `size` up to `length` are writable in the
    // caller's struct and readable in `value`, and the two don't overlap.
    unsafe {
        core::ptr::copy_nonoverlapping(
            (value as *const T).cast::<u8>().add(skip),
            pointer.cast::<u8>().add(skip),
            length - skip,
        );
    }
    Ok(())
}

/// The `fields` bits of an option struct, which say which of its members are
/// set.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(not(test), expect(dead_code, reason = "no implemented function takes an option struct yet"))]
pub(crate) struct Fields(u32);

#[cfg_attr(not(test), expect(dead_code, reason = "no implemented function takes an option struct yet"))]
impl Fields {
    /// Checks that `bits` sets only bits of `known`.
    pub(crate) fn new(bits: u32, known: u32, name: &str) -> Result<Self, Error> {
        if bits & !known != 0 {
            return Err(Error::invalid_argument(format!("{name} has unknown bits {:#x}", bits & !known)));
        }
        Ok(Self(bits))
    }

    /// `value` when `bit` is set.
    pub(crate) fn get<T>(self, bit: u32, value: T) -> Option<T> {
        (self.0 & bit != 0).then_some(value)
    }
}

/// Checks that an enum value is one that the headers define.
#[cfg_attr(not(test), expect(dead_code, reason = "no implemented function takes an enum yet"))]
pub(crate) fn enum_value(value: u32, valid: RangeInclusive<u32>, name: &str) -> Result<u32, Error> {
    if valid.contains(&value) {
        Ok(value)
    } else {
        Err(Error::invalid_argument(format!("{name} {value} is out of range")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Options {
        size: u32,
        first: u32,
        second: u64,
    }

    // SAFETY: `size` is first, and the fields are integers.
    unsafe impl Versioned for Options {
        const NAME: &'static str = "options";
        const FIRST_SIZE: usize = 8;
    }

    #[test]
    fn a_read_zeroes_what_an_older_caller_left_out() {
        let older = Options { size: 8, first: 3, second: 9 };
        // SAFETY: a local of the size it declares.
        let read = unsafe { read_sized(&older) }.unwrap();
        assert_eq!(read, Options { size: 8, first: 3, second: 0 });

        let current = Options { size: size_of::<Options>() as u32, ..older };
        // SAFETY: as above.
        assert_eq!(unsafe { read_sized(&current) }.unwrap(), current);
    }

    #[test]
    fn a_read_rejects_a_size_below_the_first_and_null() {
        let small = Options { size: 4, first: 0, second: 0 };
        // SAFETY: a local, and null.
        unsafe {
            assert_eq!(read_sized(&small).unwrap_err().message(), "options.size is too small");
            assert_eq!(read_sized::<Options>(core::ptr::null()).unwrap_err().message(), "options must not be null");
        }
    }

    #[test]
    fn a_write_stays_within_the_callers_size() {
        let value = Options { size: 99, first: 1, second: 2 };
        let mut older = Options { size: 8, first: 0, second: 7 };
        // SAFETY: a local of the size it declares.
        unsafe { write_sized(&mut older, &value) }.unwrap();
        assert_eq!(older, Options { size: 8, first: 1, second: 7 });

        let mut current = Options { size: size_of::<Options>() as u32, first: 0, second: 0 };
        // SAFETY: as above.
        unsafe { write_sized(&mut current, &value) }.unwrap();
        assert_eq!(current, Options { size: size_of::<Options>() as u32, first: 1, second: 2 });

        let mut small = Options { size: 0, first: 0, second: 0 };
        // SAFETY: as above.
        assert!(unsafe { write_sized(&mut small, &value) }.is_err());
    }

    #[test]
    fn fields_select_the_set_members() {
        let fields = Fields::new(0b101, 0b111, "options.fields").unwrap();
        assert_eq!(fields.get(0b001, "a"), Some("a"));
        assert_eq!(fields.get(0b010, "b"), None);
        let error = Fields::new(0b1001, 0b111, "options.fields").unwrap_err();
        assert_eq!(error.message(), "options.fields has unknown bits 0x8");
    }

    #[test]
    fn enum_values_stay_in_range() {
        assert_eq!(enum_value(2, 1..=3, "mode").unwrap(), 2);
        assert_eq!(enum_value(0, 1..=3, "mode").unwrap_err().message(), "mode 0 is out of range");
        assert!(enum_value(4, 1..=3, "mode").is_err());
    }
}
