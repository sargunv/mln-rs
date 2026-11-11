//! The `u64` handles that name the library's objects, each kind in a table of
//! its own.
//!
//! A handle is laid out most significant bit first as
//!
//! ```text
//! bits 63..56  kind        1..255, so 0 is the null handle of every kind
//! bits 55..36  index       slot in the kind's table
//! bits 35..0   generation  how often the slot was filled, from 1
//! ```
//!
//! Releasing a handle advances its slot's generation, and a slot whose
//! generation runs out is never filled again, so no handle value ever names a
//! second object.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use crate::error::{Error, ErrorKind};
use crate::lock;

/// `MLN_HANDLE_NULL`: the handle that names no object.
pub(crate) const NULL: u64 = 0;

const GENERATION_BITS: u32 = 36;
const INDEX_BITS: u32 = 20;
const MAX_GENERATION: u64 = (1 << GENERATION_BITS) - 1;
const MAX_INDEX: u64 = (1 << INDEX_BITS) - 1;

/// The kind of object a handle names, as its top byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum HandleKind {
    Runtime = 1,
    Map = 2,
    MapProjection = 3,
    RenderSession = 4,
    ResourceRequest = 12,
    ResourceTransformRequest = 13,
    AcquiredFrame = 20,
    RenderFrameBatch = 21,
    DeliveryQueue = 23,
    StyleExpression = 25,
    OffscreenDevice = 26,
    VulkanDevice = 27,
    FramePacer = 28,
}

impl HandleKind {
    const ALL: [Self; 13] = [
        Self::Runtime,
        Self::Map,
        Self::MapProjection,
        Self::RenderSession,
        Self::ResourceRequest,
        Self::ResourceTransformRequest,
        Self::AcquiredFrame,
        Self::RenderFrameBatch,
        Self::DeliveryQueue,
        Self::StyleExpression,
        Self::OffscreenDevice,
        Self::VulkanDevice,
        Self::FramePacer,
    ];

    fn of(handle: u64) -> Option<Self> {
        let byte = (handle >> (INDEX_BITS + GENERATION_BITS)) as u8;
        Self::ALL.into_iter().find(|kind| *kind as u8 == byte)
    }

    /// The C type's name, such as `mln_map`.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Runtime => "mln_runtime",
            Self::Map => "mln_map",
            Self::MapProjection => "mln_map_projection",
            Self::RenderSession => "mln_render_session",
            Self::ResourceRequest => "mln_resource_request",
            Self::ResourceTransformRequest => "mln_resource_transform_request",
            Self::AcquiredFrame => "mln_acquired_frame",
            Self::RenderFrameBatch => "mln_render_frame_batch",
            Self::DeliveryQueue => "mln_delivery_queue",
            Self::StyleExpression => "mln_style_expression",
            Self::OffscreenDevice => "mln_offscreen_device",
            Self::VulkanDevice => "mln_vulkan_device",
            Self::FramePacer => "mln_frame_pacer",
        }
    }
}

const fn encode(kind: HandleKind, index: u64, generation: u64) -> u64 {
    ((kind as u64) << (INDEX_BITS + GENERATION_BITS)) | (index << GENERATION_BITS) | generation
}

const fn index_of(handle: u64) -> usize {
    ((handle >> GENERATION_BITS) & MAX_INDEX) as usize
}

const fn generation_of(handle: u64) -> u64 {
    handle & MAX_GENERATION
}

/// The objects of one handle kind. Its lock is never held while code outside
/// the table runs, so an object's `Drop` and host callbacks are free to use
/// any table.
pub(crate) struct HandleTable<T> {
    kind: HandleKind,
    slots: Mutex<Slots<T>>,
}

struct Slots<T> {
    slots: Vec<Slot<T>>,
    free: Vec<u32>,
}

/// `generation` is the one the slot issued last while `object` is set, and
/// the one it issues next while it is empty. A retired slot's exceeds
/// [`MAX_GENERATION`].
struct Slot<T> {
    generation: u64,
    object: Option<Arc<T>>,
}

impl<T> HandleTable<T> {
    pub(crate) const fn new(kind: HandleKind) -> Self {
        Self { kind, slots: Mutex::new(Slots { slots: Vec::new(), free: Vec::new() }) }
    }

    /// Stores `object` and returns its new handle.
    pub(crate) fn insert(&self, object: Arc<T>) -> Result<u64, Error> {
        let mut table = lock(&self.slots);
        if let Some(index) = table.free.pop() {
            let slot = &mut table.slots[index as usize];
            slot.object = Some(object);
            return Ok(encode(self.kind, u64::from(index), slot.generation));
        }
        let index = table.slots.len() as u64;
        if index > MAX_INDEX {
            drop(table);
            return Err(Error::native(format!("the {} handle table is full", self.kind.name())));
        }
        table.slots.push(Slot { generation: 1, object: Some(object) });
        Ok(encode(self.kind, index, 1))
    }

    /// The object that `handle` names.
    pub(crate) fn get(&self, handle: u64) -> Result<Arc<T>, Error> {
        let table = lock(&self.slots);
        match table.live(self.kind, handle) {
            Some(object) => Ok(Arc::clone(object)),
            None => Err(self.fault(&table, handle)),
        }
    }

    /// Retires `handle` and returns the object it named. Every copy of the
    /// handle is stale from here on.
    pub(crate) fn remove(&self, handle: u64) -> Result<Arc<T>, Error> {
        let mut table = lock(&self.slots);
        let index = index_of(handle);
        let object = match table.live_mut(self.kind, handle) {
            Some(slot) => {
                slot.generation += 1;
                slot.object.take()
            }
            None => None,
        };
        let Some(object) = object else {
            return Err(self.fault(&table, handle));
        };
        if table.slots[index].generation <= MAX_GENERATION {
            table.free.push(index as u32);
        }
        drop(table);
        Ok(object)
    }

    fn fault(&self, table: &Slots<T>, handle: u64) -> Error {
        if handle == NULL {
            return Error::invalid_argument(format!("{} handle must not be null", self.kind.name()));
        }
        let expected = self.kind.name();
        let Some(kind) = HandleKind::of(handle) else {
            return Error::invalid_argument(format!("{} is not a valid {expected} handle", hex(handle)));
        };
        if kind != self.kind {
            return Error::invalid_argument(format!(
                "handle {} is an {} handle, not an {expected} handle",
                hex(handle),
                kind.name(),
            ));
        }
        let generation = generation_of(handle);
        let issued =
            table.slots.get(index_of(handle)).is_some_and(|slot| generation != 0 && generation < slot.generation);
        if issued {
            // The handle once named a live object, so the call is out of order
            // with that object's life rather than a bad argument.
            Error::new(
                ErrorKind::InvalidState,
                format!("{expected} handle {} is stale; the object it named has been released", hex(handle)),
            )
        } else {
            Error::invalid_argument(format!("{expected} handle {} was never created by this process", hex(handle)))
        }
    }
}

impl<T> Slots<T> {
    fn live(&self, kind: HandleKind, handle: u64) -> Option<&Arc<T>> {
        if HandleKind::of(handle) != Some(kind) {
            return None;
        }
        let slot = self.slots.get(index_of(handle))?;
        slot.object.as_ref().filter(|_| slot.generation == generation_of(handle))
    }

    fn live_mut(&mut self, kind: HandleKind, handle: u64) -> Option<&mut Slot<T>> {
        if HandleKind::of(handle) != Some(kind) {
            return None;
        }
        self.slots
            .get_mut(index_of(handle))
            .filter(|slot| slot.object.is_some() && slot.generation == generation_of(handle))
    }
}

fn hex(handle: u64) -> String {
    let mut text = String::new();
    let _ = write!(text, "{handle:#x}");
    text
}

/// Checks a handle out-parameter, which must point to the null handle so that
/// a caller can't leak the handle it already holds there.
///
/// # Safety
///
/// `out` is null or points to a writable, initialized `u64`.
pub(crate) unsafe fn out_handle<'a>(out: *mut u64, name: &'static str) -> Result<&'a mut u64, Error> {
    // SAFETY: the caller's contract.
    match unsafe { out.as_mut() } {
        Some(out) if *out == NULL => Ok(out),
        _ => Err(Error::invalid_argument(format!("{name} must point to the null handle"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::types::{MLN_STATUS_INVALID_ARGUMENT, MLN_STATUS_INVALID_STATE};

    #[test]
    fn handles_carry_their_kind_and_never_repeat() {
        let table = HandleTable::new(HandleKind::Map);
        let first = table.insert(Arc::new(1)).unwrap();
        assert_eq!(first >> 56, 2);
        assert_eq!(*table.get(first).unwrap(), 1);
        assert_eq!(*table.remove(first).unwrap(), 1);

        // The freed slot comes back with the next generation.
        let second = table.insert(Arc::new(2)).unwrap();
        assert_ne!(first, second);
        assert_eq!(index_of(first), index_of(second));
        assert_eq!(*table.get(second).unwrap(), 2);

        let stale = table.get(first).unwrap_err();
        assert_eq!(stale.status(), MLN_STATUS_INVALID_STATE);
        assert!(stale.message().contains("stale"), "{stale}");
        assert_eq!(table.remove(first).unwrap_err().status(), MLN_STATUS_INVALID_STATE);

        let ahead = table.get(second + 1).unwrap_err();
        assert_eq!(ahead.status(), MLN_STATUS_INVALID_ARGUMENT);
        assert!(ahead.message().contains("never created"), "{ahead}");
    }

    #[test]
    fn faults_name_what_is_wrong() {
        let table = HandleTable::<()>::new(HandleKind::Map);
        let null = table.get(0).unwrap_err();
        assert_eq!(null.status(), MLN_STATUS_INVALID_ARGUMENT);
        assert_eq!(null.message(), "mln_map handle must not be null");

        let runtime = encode(HandleKind::Runtime, 0, 1);
        let wrong = table.get(runtime).unwrap_err();
        assert_eq!(wrong.status(), MLN_STATUS_INVALID_ARGUMENT);
        assert!(wrong.message().contains("mln_runtime") && wrong.message().contains("mln_map"), "{wrong}");

        let unissued = table.get(0x0200_000F_FFFF_FFFF).unwrap_err();
        assert_eq!(unissued.status(), MLN_STATUS_INVALID_ARGUMENT);
        assert!(unissued.message().contains("never created"), "{unissued}");

        let malformed = table.get(0xDEAD_BEEF_DEAD_BEEF).unwrap_err();
        assert_eq!(malformed.status(), MLN_STATUS_INVALID_ARGUMENT);
        assert_eq!(malformed.message(), "0xdeadbeefdeadbeef is not a valid mln_map handle");
    }

    #[test]
    fn an_exhausted_slot_is_retired() {
        let table = HandleTable::new(HandleKind::Map);
        let handle = table.insert(Arc::new(())).unwrap();
        lock(&table.slots).slots[0].generation = MAX_GENERATION;
        let last = encode(HandleKind::Map, 0, MAX_GENERATION);
        table.remove(last).unwrap();
        assert!(lock(&table.slots).free.is_empty());
        let next = table.insert(Arc::new(())).unwrap();
        assert_eq!(index_of(next), 1);
        assert_eq!(table.get(last).unwrap_err().status(), MLN_STATUS_INVALID_STATE);
        assert_ne!(handle, next);
    }

    #[test]
    fn an_out_handle_must_start_null() {
        let mut empty = 0;
        // SAFETY: each pointer is null or to a local.
        unsafe {
            assert!(out_handle(&mut empty, "out_map").is_ok());
            let mut occupied = 5;
            let error = out_handle(&mut occupied, "out_map").unwrap_err();
            assert_eq!(error.message(), "out_map must point to the null handle");
            assert!(out_handle(core::ptr::null_mut(), "out_map").is_err());
        }
    }
}
