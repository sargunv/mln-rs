// Ports include/mln/webgpu/buffer_resource.hpp and src/mln/webgpu/buffer_resource.cpp

use std::rc::Rc;

use crate::context::Context;

pub type VersionType = u16;

pub struct BufferResource {
    context: Rc<Context>,
    buffer: Option<wgpu::Buffer>,
    raw: Vec<u8>,
    size: usize,
    usage: wgpu::BufferUsages,
    version: VersionType,
    is_index_buffer: bool,
    persistent: bool,
}

impl BufferResource {
    /// `BufferResource(context, data, size, usage, isIndexBuffer, persistent)`;
    /// `data == None` is the C++ null pointer (size without contents).
    pub fn new(
        context: Rc<Context>,
        data: Option<&[u8]>,
        size: usize,
        usage: wgpu::BufferUsages,
        is_index_buffer: bool,
        persistent: bool,
    ) -> Self {
        let mut res = BufferResource {
            context,
            buffer: None,
            raw: Vec::new(),
            size,
            usage,
            version: 0,
            is_index_buffer,
            persistent,
        };
        if size == 0 {
            return res;
        }
        // Store raw data if provided
        if let Some(data) = data {
            res.raw = data[..size].to_vec();
        }
        let backend = res.context.get_backend();
        let Some(device) = backend.get_device() else {
            return res;
        };
        let padded_size = (size + 3) & !3usize;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Buffer Resource"),
            size: padded_size as u64,
            usage: usage | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        if let Some(data) = data {
            // Upload initial data via queue write. C++ falls back to mapping
            // when there is no queue; a device always has one here.
            if padded_size == size {
                backend.write_buffer(&buffer, 0, &data[..size]);
            } else {
                let mut padded = vec![0u8; padded_size];
                padded[..size].copy_from_slice(&data[..size]);
                backend.write_buffer(&buffer, 0, &padded);
            }
        }
        res.buffer = Some(buffer);
        res
    }

    /// `clone()`: a new buffer with the same raw contents.
    pub fn clone_resource(&self) -> Self {
        BufferResource::new(
            self.context.clone(),
            if self.raw.is_empty() { None } else { Some(&self.raw) },
            self.size,
            self.usage,
            self.is_index_buffer,
            self.persistent,
        )
    }

    pub fn update(&mut self, data: &[u8], offset: usize) {
        let update_size = data.len();
        if update_size == 0 {
            return;
        }
        // Update raw data if we have it
        if !self.raw.is_empty() && offset + update_size <= self.raw.len() {
            self.raw[offset..offset + update_size].copy_from_slice(data);
        }
        // Update GPU buffer if we have one
        if let Some(buffer) = &self.buffer {
            self.context.get_backend().write_buffer(buffer, offset as u64, data);
        }
        // Increment version to indicate change
        self.version = self.version.wrapping_add(1);
    }

    pub fn get_size_in_bytes(&self) -> usize {
        self.size
    }
    pub fn contents(&self) -> Option<&[u8]> {
        if self.raw.is_empty() { None } else { Some(&self.raw) }
    }
    pub fn get_context(&self) -> &Rc<Context> {
        &self.context
    }
    pub fn get_buffer(&self) -> Option<&wgpu::Buffer> {
        self.buffer.as_ref()
    }
    pub fn is_valid(&self) -> bool {
        self.buffer.is_some() || !self.raw.is_empty()
    }
    /// Used to detect whether buffer contents have changed
    pub fn get_version(&self) -> VersionType {
        self.version
    }
    /// Indicates whether this buffer needs to be re-bound from a previous
    /// binding at the given version
    pub fn need_re_bind(&self, version: VersionType) -> bool {
        self.version != version
    }
    pub fn get_usage(&self) -> wgpu::BufferUsages {
        self.usage
    }
    pub fn is_persistent(&self) -> bool {
        self.persistent
    }
}
