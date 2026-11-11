// Ports include/mln/webgpu/upload_pass.hpp, src/mln/webgpu/upload_pass.cpp
// and src/mln/webgpu/vertex_attribute.cpp (`VertexAttribute::getBuffer`)

use std::any::Any;
use std::rc::Rc;

use mln_gfx::attribute::{AttributeBinding, AttributeBindingArray, AttributeDescriptor};
use mln_gfx::debug_group::DebugGroupTarget;
use mln_gfx::gfx_types::AttributeDataType;
use mln_gfx::renderable::Renderable;
use mln_gfx::types::BufferUsageType;
use mln_gfx::upload_pass;
use mln_gfx::vertex_attribute::{VertexAttribute, VertexAttributeArray};
use mln_gfx::vertex_buffer::{self, UniqueVertexBufferResource};
use mln_gfx::vertex_vector::{VertexBufferBase, VertexVectorBasePtr};
use mln_util::time::Instant;

use crate::buffer_resource::BufferResource;
use crate::command_encoder::CommandEncoder;
use crate::context::Context;
use crate::vertex_buffer_resource::{IndexBufferResource, VertexBufferResource};

pub struct UploadPass {
    context: Rc<Context>,
}

/// `struct VertexBuffer : gfx::VertexBufferBase`: the buffer attached to a
/// shared vertex vector.
struct VertexBuffer {
    resource: Rc<VertexBufferResource>,
}

impl VertexBufferBase for VertexBuffer {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl UploadPass {
    /// C++ pushes the encoder's debug groups and `name` on the pass; WebGPU
    /// upload-pass debug groups are no-ops.
    pub fn new(_renderable: &dyn Renderable, command_encoder: &CommandEncoder, _name: &str) -> Self {
        UploadPass { context: command_encoder.get_context().clone() }
    }

    pub fn webgpu_context(&self) -> &Rc<Context> {
        &self.context
    }

    fn create_vertex_buffer(&self, data: &[u8], persistent: bool) -> Rc<VertexBufferResource> {
        let buffer = BufferResource::new(
            self.context.clone(),
            Some(data),
            data.len(),
            wgpu::BufferUsages::VERTEX,
            /*is_index_buffer=*/ false,
            persistent,
        );
        if buffer.get_buffer().is_none() {
            eprintln!("  Failed to create vertex buffer!");
        }
        Rc::new(VertexBufferResource::new(buffer))
    }

    /// `getBuffer(const VertexVectorBasePtr&, usage, forceUpdate)`
    pub fn get_buffer(
        &self,
        vec: Option<&VertexVectorBasePtr>,
        usage: BufferUsageType,
        force_update: bool,
    ) -> Option<UniqueVertexBufferResource> {
        let vec = vec?;
        let raw_size = vec.get_raw_count() * vec.get_raw_size();
        let existing = vec
            .get_buffer()
            .as_ref()
            .and_then(|b| b.as_any().downcast_ref::<VertexBuffer>())
            .map(|b| b.resource.clone());
        if let Some(resource) = existing
            && raw_size <= resource.get_size_in_bytes()
        {
            let modified = match resource.get_last_updated() {
                Some(t) => vec.is_modified_after(t),
                None => true,
            };
            // A freed vector's data is already in the buffer.
            if vec.has_raw_data() && (force_update || modified) {
                resource.update(&vec.get_raw_data()[..raw_size]);
                resource.set_last_updated(vec.get_last_modified());
            }
            return Some(resource);
        }
        let _ = usage;
        if raw_size > 0 {
            debug_assert!(vec.has_raw_data());
            let resource = self.create_vertex_buffer(&vec.get_raw_data()[..raw_size], false);
            // A released vector frees its CPU copy here.
            vec.set_buffer(Some(Box::new(VertexBuffer { resource: resource.clone() })));
            return Some(resource);
        }
        None
    }

    /// `Context::getDefaultVertexBuffer(type, size)`: a shared buffer of at
    /// least `vertex_count` default values of `data_type` for an attribute
    /// the drawable doesn't provide: white for colors (Float4), full opacity
    /// (Float2), zeros otherwise. The context keeps one per type, grown
    /// geometrically. Each binding holds the buffer it was given, so a
    /// replaced smaller one lives until no binding points at it.
    fn dummy_vertex_buffer(&self, data_type: AttributeDataType, vertex_count: usize) -> UniqueVertexBufferResource {
        let mut cache = self.context.dummy_vertex_buffers().borrow_mut();
        let cached_count = cache.get(&data_type).map_or(0, |(count, _)| *count);
        if let Some((count, buffer)) = cache.get(&data_type)
            && *count >= vertex_count
        {
            return buffer.clone();
        }
        let count = vertex_count.max(2 * cached_count).max(1);
        let mut data = vec![0u8; VertexAttribute::get_stride_of(data_type) * count];
        if matches!(data_type, AttributeDataType::Float4 | AttributeDataType::Float2) {
            for v in data.chunks_exact_mut(4) {
                v.copy_from_slice(&1.0f32.to_ne_bytes());
            }
        }
        let buffer: UniqueVertexBufferResource = self.create_vertex_buffer(&data, false);
        cache.insert(data_type, (count, buffer.clone()));
        buffer
    }

    /// `webgpu::VertexAttribute::getBuffer(attrib, uploadPass, usage, forceUpdate)`
    pub fn get_attribute_buffer(
        &self,
        attrib: &mut VertexAttribute,
        usage: BufferUsageType,
        force_update: bool,
    ) -> Option<UniqueVertexBufferResource> {
        if attrib.get_buffer().is_none() {
            if let Some(shared) = attrib.get_shared_raw_data().cloned() {
                return self.get_buffer(Some(&shared), usage, force_update);
            } else if !attrib.get_raw_data().is_empty() {
                let buffer: UniqueVertexBufferResource = self.create_vertex_buffer(attrib.get_raw_data(), false);
                attrib.set_buffer(Some(buffer));
                attrib.set_raw_data(Vec::new());
                attrib.set_dirty(false);
            } else {
                debug_assert!(false, "vertex attribute without data");
            }
        }
        attrib.get_buffer().cloned()
    }
}

impl DebugGroupTarget for UploadPass {
    fn push_debug_group(&self, _name: &str) {
        // WebGPU debug groups are typically set on command encoders; no-op.
    }
    fn pop_debug_group(&self) {}
}

impl upload_pass::UploadPass for UploadPass {
    fn get_context(&self) -> &dyn mln_gfx::context::Context {
        &*self.context
    }

    fn build_attribute_bindings(
        &self,
        vertex_count: usize,
        _vertex_type: AttributeDataType,
        _vertex_attribute_index: usize,
        _vertex_data: &[u8],
        defaults: &VertexAttributeArray,
        overrides: &mut VertexAttributeArray,
        usage: BufferUsageType,
        last_update: Option<Instant>,
        vertex_buffers: &mut Vec<UniqueVertexBufferResource>,
    ) -> AttributeBindingArray {
        let mut bindings: AttributeBindingArray = vec![None; defaults.allocated_size()];

        let resolve_attr = |_id: usize, default: &VertexAttribute, override_: Option<&mut VertexAttribute>| {
            let index = default.get_index() as usize;
            if bindings.len() < index + 1 {
                bindings.resize(index + 1, None);
            }

            let Some(effective) = override_ else {
                // No attribute was provided. WebGPU requires all vertex
                // attributes to have buffers bound, so bind a dummy buffer
                // with default values.
                let stride = VertexAttribute::get_stride_of(default.get_data_type());
                let dummy_buffer = self.dummy_vertex_buffer(default.get_data_type(), vertex_count);
                bindings[index] = Some(AttributeBinding {
                    attribute: AttributeDescriptor { data_type: default.get_data_type(), offset: 0 },
                    vertex_stride: stride as u32,
                    vertex_buffer_resource: Some(dummy_buffer.clone()),
                    vertex_offset: 0,
                    buffer_index: 0,
                });
                // Store the buffer so it doesn't get destroyed
                vertex_buffers.push(dummy_buffer);
                return;
            };

            // If the attribute references data shared with a bucket, get the
            // corresponding buffer.
            if let Some(buffer) = self.get_buffer(effective.get_shared_raw_data(), usage, last_update.is_none()) {
                let shared = effective.get_shared_raw_data().unwrap();
                debug_assert!(
                    (effective.get_shared_stride() as usize) * (effective.get_shared_vertex_offset() as usize)
                        < shared.get_raw_size() * shared.get_raw_count()
                );
                bindings[index] = Some(AttributeBinding {
                    attribute: AttributeDescriptor {
                        data_type: effective.get_shared_type(),
                        offset: effective.get_shared_offset(),
                    },
                    vertex_stride: effective.get_shared_stride(),
                    vertex_buffer_resource: Some(buffer),
                    vertex_offset: effective.get_shared_vertex_offset(),
                    buffer_index: 0,
                });
                return;
            }

            debug_assert!(effective.get_stride() > 0);
            // Otherwise, turn the data managed by the attribute into a buffer.
            if let Some(buffer) =
                self.get_attribute_buffer(effective, BufferUsageType::StaticDraw, last_update.is_none())
            {
                bindings[index] = Some(AttributeBinding {
                    attribute: AttributeDescriptor { data_type: effective.get_data_type(), offset: 0 },
                    vertex_stride: effective.get_stride() as u32,
                    vertex_buffer_resource: Some(buffer),
                    vertex_offset: 0,
                    buffer_index: 0,
                });
                return;
            }

            eprintln!("Failed to create buffer for attribute");
            debug_assert!(false);
        };

        // This version is called when the attribute is available, but isn't
        // being used by the shader
        let missing_attr = |_id: usize, attr: &mut VertexAttribute| {
            attr.set_dirty(false);
        };

        defaults.resolve(overrides, resolve_attr, missing_attr);
        bindings
    }

    fn create_vertex_buffer_resource(
        &self,
        data: &[u8],
        _usage: BufferUsageType,
        persistent: bool,
    ) -> UniqueVertexBufferResource {
        self.create_vertex_buffer(data, persistent)
    }

    fn update_vertex_buffer_resource(&self, resource: &dyn vertex_buffer::VertexBufferResource, data: &[u8]) {
        if let Some(buffer) = resource.as_any().downcast_ref::<VertexBufferResource>() {
            buffer.update(data);
        }
    }

    fn create_index_buffer_resource(
        &self,
        data: &[u8],
        _usage: BufferUsageType,
        persistent: bool,
    ) -> Box<dyn vertex_buffer::IndexBufferResource> {
        let buffer = BufferResource::new(
            self.context.clone(),
            Some(data),
            data.len(),
            wgpu::BufferUsages::INDEX,
            /*is_index_buffer=*/ true,
            persistent,
        );
        Box::new(IndexBufferResource::new(buffer))
    }

    fn update_index_buffer_resource(&self, resource: &dyn vertex_buffer::IndexBufferResource, data: &[u8]) {
        if let Some(buffer) = resource.as_any().downcast_ref::<IndexBufferResource>() {
            buffer.update(data);
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
