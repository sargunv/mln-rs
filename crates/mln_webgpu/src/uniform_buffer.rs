// Ports include/mln/webgpu/uniform_buffer.hpp and src/mln/webgpu/uniform_buffer.cpp
//
// `webgpu::UniformBufferArray` only overrides `bind` (a no-op: bind groups
// are built per drawable in `Drawable::draw`) and `copy`, so the port uses
// `gfx::UniformBufferArray` directly.

use std::any::Any;
use std::rc::Rc;

use mln_gfx::uniform_buffer;

use crate::context::Context;
use crate::renderer_backend::BackendImpl;

const UNIFORM_BUFFER_ALIGNMENT: usize = 256;

const fn aligned_uniform_size(size: usize) -> usize {
    if size == 0 {
        return 0;
    }
    size.div_ceil(UNIFORM_BUFFER_ALIGNMENT) * UNIFORM_BUFFER_ALIGNMENT
}

pub struct UniformBuffer {
    /// The size of the data, which `get_size` reports. The GPU buffer is
    /// padded to 256 bytes. C++ reports the padded size, so
    /// `UniformBufferArray::createOrUpdate`, which compares it to the data
    /// size, creates a new GPU buffer on every update of a UBO whose size
    /// isn't a multiple of 256 bytes. Reporting the data size updates it in
    /// place. Bind groups still bind the whole padded buffer, because some
    /// UBOs hold less data than their shader struct.
    size: usize,
    /// C++ keeps `Context&`; only device and queue are used. Holding the
    /// backend state rather than `Rc<Context>` avoids an Rc cycle through the
    /// context's global uniform buffer array.
    backend: Rc<BackendImpl>,
    buffer: Option<wgpu::Buffer>,
}

impl UniformBuffer {
    /// `UniformBuffer(context, data, size)`; `data == None` is the C++ null
    /// pointer.
    pub fn new(context: &Context, data: Option<&[u8]>, size: usize) -> Self {
        Self::with_backend(context.get_backend().clone(), data, size)
    }

    fn with_backend(backend: Rc<BackendImpl>, data: Option<&[u8]>, size: usize) -> Self {
        let aligned_size = aligned_uniform_size(size);
        let mut buffer = None;
        if let Some(device) = backend.get_device()
            && aligned_size > 0
        {
            // C++ creates the buffer mapped and copies the data (zero padded);
            // a queue write of the padded data has the same effect for the
            // next submission.
            let b = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("Uniform Buffer"),
                size: aligned_size as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            if let Some(data) = data {
                let mut padded = vec![0u8; aligned_size];
                padded[..size].copy_from_slice(&data[..size]);
                backend.write_buffer(&b, 0, &padded);
            }
            buffer = Some(b);
        }
        UniformBuffer { size, backend, buffer }
    }

    /// `clone()`: a new buffer of the same size; contents are not copied
    /// (C++: "We can't directly copy buffer contents in WebGPU without a
    /// command encoder").
    pub fn clone_buffer(&self) -> Self {
        UniformBuffer::with_backend(self.backend.clone(), None, self.size)
    }

    pub fn get_buffer(&self) -> Option<&wgpu::Buffer> {
        self.buffer.as_ref()
    }
}

impl uniform_buffer::UniformBuffer for UniformBuffer {
    fn update(&self, data: &[u8]) {
        debug_assert!(data.len() <= aligned_uniform_size(self.size), "update larger than the uniform buffer");
        if !data.is_empty()
            && let Some(buffer) = &self.buffer
        {
            self.backend.write_buffer(buffer, 0, data);
        }
    }

    fn get_size(&self) -> usize {
        self.size
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
