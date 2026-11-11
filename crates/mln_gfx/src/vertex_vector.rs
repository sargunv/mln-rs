// Ports src/mln/gfx/vertex_vector.hpp

use std::any::Any;
use std::cell::{Cell, Ref, RefCell};
use std::rc::Rc;

use mln_util::time::Instant;

/// `gfx::VertexBufferBase`: backend buffer attached to a shared vertex vector.
pub trait VertexBufferBase: Any {
    fn as_any(&self) -> &dyn Any;
}

/// The data members of `gfx::VertexVectorBase`. Interior mutability because
/// the vector is shared (`VertexVectorBasePtr`) and the upload pass attaches
/// buffers and modification times through the shared pointer.
pub struct VertexVectorBaseData {
    buffer: RefCell<Option<Box<dyn VertexBufferBase>>>,
    dirty: Cell<bool>,
    released: Cell<bool>,
    raw_freed: Cell<bool>,
    last_modified: Cell<Instant>,
}

impl Default for VertexVectorBaseData {
    fn default() -> Self {
        VertexVectorBaseData {
            buffer: RefCell::new(None),
            dirty: Cell::new(true),
            released: Cell::new(false),
            raw_freed: Cell::new(false),
            last_modified: Cell::new(Instant::now()),
        }
    }
}

/// `gfx::VertexVectorBase`
pub trait VertexVectorBase: Any {
    fn vertex_vector_base(&self) -> &VertexVectorBaseData;
    /// The raw bytes of all vertices (`getRawData()`, `getRawCount() * getRawSize()`
    /// bytes), empty once the CPU copy is freed after upload.
    fn get_raw_data(&self) -> Ref<'_, [u8]>;
    /// `sizeof(Vertex)`
    fn get_raw_size(&self) -> usize;
    /// The vertex count, which stays when the CPU copy is freed.
    fn get_raw_count(&self) -> usize;

    /// Drop the CPU copy of the data. Vectors that keep their data leave it be.
    fn free_raw_data(&self) {}

    fn get_buffer(&self) -> std::cell::Ref<'_, Option<Box<dyn VertexBufferBase>>> {
        self.vertex_vector_base().buffer.borrow()
    }
    fn set_buffer(&self, value: Option<Box<dyn VertexBufferBase>>) {
        let base = self.vertex_vector_base();
        let has_buffer = value.is_some();
        *base.buffer.borrow_mut() = value;
        // The data now lives in the buffer; a released vector will not be modified or re-uploaded.
        if base.released.get() && has_buffer {
            self.free_raw_data();
        }
    }
    /// False once the CPU copy has been freed after upload. The element count is kept.
    fn has_raw_data(&self) -> bool {
        !self.vertex_vector_base().raw_freed.get()
    }
    fn get_last_modified(&self) -> Instant {
        self.vertex_vector_base().last_modified.get()
    }
    /// Also true of a freed vector: a drawable that shares an attribute
    /// array with the one that uploaded and freed a new vector still has to
    /// bind its buffer. Uploads check `has_raw_data` before they read the data.
    fn is_modified_after(&self, t: Instant) -> bool {
        t < self.vertex_vector_base().last_modified.get()
    }
    fn update_modified(&self, force: bool) {
        let base = self.vertex_vector_base();
        if base.dirty.get() || force {
            base.last_modified.set(Instant::now());
            base.dirty.set(false);
        }
    }
    /// Indicates that the owner/producer will not modify this again
    fn is_released(&self) -> bool {
        self.vertex_vector_base().released.get()
    }
}

pub type VertexVectorBasePtr = Rc<dyn VertexVectorBase>;

/// `gfx::VertexVector<V>` for plain-old-data vertex types. The vertices sit
/// in a `RefCell` so that `set_buffer` can free them through the shared
/// pointer.
pub struct VertexVector<V: bytemuck::Pod> {
    base: VertexVectorBaseData,
    v: RefCell<Vec<V>>,
    /// The element count of the CPU data freed after upload.
    freed_count: Cell<usize>,
}

impl<V: bytemuck::Pod> Default for VertexVector<V> {
    fn default() -> Self {
        VertexVector::from_vec(Vec::new())
    }
}

impl<V: bytemuck::Pod> VertexVector<V> {
    pub fn new() -> Self {
        Self::default()
    }

    /// A vector holding `vertices` (C++ builds one with `emplace_back`; the
    /// renderer moves a bucket's vertices into a shared vector this way, see
    /// mln_renderer::bucket_geometry).
    pub fn from_vec(vertices: Vec<V>) -> Self {
        VertexVector { base: VertexVectorBaseData::default(), v: RefCell::new(vertices), freed_count: Cell::new(0) }
    }

    /// A vector holding `vertices` that will not be updated (`release()`d):
    /// its CPU copy is freed once it is in a buffer.
    pub fn from_vec_released(vertices: Vec<V>) -> Self {
        let mut result = Self::from_vec(vertices);
        result.release();
        result
    }

    pub fn emplace_back(&mut self, vertex: V) {
        debug_assert!(!self.base.released.get());
        self.v.get_mut().push(vertex);
        self.base.dirty.set(true);
    }

    pub fn extend(&mut self, n: usize, val: V) {
        debug_assert!(!self.base.released.get());
        let v = self.v.get_mut();
        v.resize(v.len() + n, val);
        self.base.dirty.set(true);
    }

    pub fn at_mut(&mut self, n: usize) -> &mut V {
        debug_assert!(!self.base.released.get());
        self.base.dirty.set(true);
        &mut self.v.get_mut()[n]
    }

    pub fn at(&self, n: usize) -> V {
        self.v.borrow()[n]
    }

    /// The element count, which stays when the CPU copy is freed.
    pub fn elements(&self) -> usize {
        self.v.borrow().len() + self.freed_count.get()
    }

    pub fn bytes(&self) -> usize {
        self.elements() * std::mem::size_of::<V>()
    }

    pub fn empty(&self) -> bool {
        self.elements() == 0
    }

    pub fn clear(&mut self) {
        self.base.dirty.set(true);
        self.v.get_mut().clear();
        self.freed_count.set(0);
        self.base.raw_freed.set(false);
    }

    pub fn reserve(&mut self, count: usize) {
        self.v.get_mut().reserve(count);
    }

    /// Indicate that this shared vertex vector instance will no longer be updated.
    pub fn release(&mut self) {
        self.base.released.set(true);
        // If we've already created a buffer, we don't need the raw data any more.
        if self.base.buffer.borrow().is_some() {
            self.free_raw_data();
        }
    }

    /// The CPU copy of the vertices, empty once it is freed after upload.
    pub fn data(&self) -> Ref<'_, [V]> {
        Ref::map(self.v.borrow(), |v| v.as_slice())
    }

    pub fn vector(&self) -> Ref<'_, Vec<V>> {
        self.v.borrow()
    }
}

impl<V: bytemuck::Pod> VertexVectorBase for VertexVector<V> {
    fn vertex_vector_base(&self) -> &VertexVectorBaseData {
        &self.base
    }
    fn get_raw_data(&self) -> Ref<'_, [u8]> {
        Ref::map(self.v.borrow(), |v| bytemuck::cast_slice(v.as_slice()))
    }
    fn get_raw_size(&self) -> usize {
        std::mem::size_of::<V>()
    }
    fn get_raw_count(&self) -> usize {
        self.elements()
    }
    fn free_raw_data(&self) {
        let mut v = self.v.borrow_mut();
        self.freed_count.set(self.freed_count.get() + v.len());
        *v = Vec::new();
        self.base.raw_freed.set(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubBuffer;

    impl VertexBufferBase for StubBuffer {
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[test]
    fn released_vector_frees_its_data_once_it_has_a_buffer() {
        let before = Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(1));
        let vec: VertexVectorBasePtr = Rc::new(VertexVector::from_vec_released(vec![[1i16, 2], [3, 4]]));
        assert!(vec.has_raw_data());
        assert_eq!(vec.get_raw_data().len(), 8);

        vec.set_buffer(Some(Box::new(StubBuffer)));
        assert!(!vec.has_raw_data());
        assert!(vec.get_raw_data().is_empty());
        assert_eq!(vec.get_raw_count(), 2);
        // A drawable that bound its attributes before the vector was made
        // still sees it as new.
        assert!(vec.is_modified_after(before));
    }

    #[test]
    fn vector_that_is_not_released_keeps_its_data() {
        let vec: VertexVectorBasePtr = Rc::new(VertexVector::from_vec(vec![[1i16, 2]]));
        vec.set_buffer(Some(Box::new(StubBuffer)));
        assert!(vec.has_raw_data());
        assert_eq!(vec.get_raw_data().len(), 4);
    }
}
