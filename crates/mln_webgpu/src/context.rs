// Ports include/mln/webgpu/context.hpp and src/mln/webgpu/context.cpp
//
// Cycle avoidance: C++ members are references; resources here hold
// `Rc<Context>`, so the context itself keeps no `Rc` to a resource that
// points back at it (the cached tile buffers are raw wgpu buffers, the clip
// mask shader a `Weak`). The exception is the placeholder vertex buffers
// (port-only), which the backends release with `release_cached_buffers`
// when they drop the context; otherwise the context, and with it the
// device, would never be freed.
//
// TODO(port): `getOrCreateVertexBindings`, `createDynamicTexture`,
// `createRenderTarget`, render jobs of the backend thread pool
// (`runRenderJobs` in begin/end frame, cleanup). None is on the
// background-layer path.

use std::any::Any;
use std::cell::{Cell, Ref, RefCell, RefMut};
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use mln_gfx::color_mode::ColorMode;
use mln_gfx::context::{Context as GfxContext, ContextBase};
use mln_gfx::depth_mode::DepthMode;
use mln_gfx::drawable_builder::UniqueDrawableBuilder;
use mln_gfx::gfx_types::{AttributeDataType, DrawModeType};
use mln_gfx::renderer::layer_group::LayerGroupBasePtr;
use mln_gfx::rendering_stats::RenderingStats;
use mln_gfx::shader_group::StringIDSetsPair;
use mln_gfx::shader_registry::ShaderRegistry;
use mln_gfx::shaders::clipping_mask_ubo::ClipUBO;
use mln_gfx::shaders::shader_program_base::{ShaderProgramBase, ShaderProgramBasePtr};
use mln_gfx::shaders::shader_source::BuiltIn;
use mln_gfx::stencil_mode::{StencilMode, Test};
use mln_gfx::texture2d::Texture2DPtr;
use mln_gfx::types::{DepthFunctionType, DepthMaskType, StencilOpType, TextureChannelDataType};
use mln_gfx::uniform_buffer::{UniformBufferArray, UniformBufferPtr, UniqueUniformBufferArray};
use mln_gfx::util_hash::hash;
use mln_gfx::vertex_attribute::{VertexAttributeArray, VertexAttributeArrayPtr};
use mln_gfx::vertex_buffer::UniqueVertexBufferResource;
use mln_util::range::Range;
use mln_util::size::Size;

use crate::command_encoder::CommandEncoder;
use crate::drawable_builder::DrawableBuilder;
use crate::layer_group::LayerGroup;
use crate::offscreen_texture::OffscreenTexture;
use crate::render_pass::RenderPass;
use crate::renderer_backend::BackendImpl;
use crate::shaders::shader_program::{BindingType, VertexLayout, as_webgpu_shader};
use crate::shaders::shader_source::shader_source;
use crate::texture2d::Texture2D;
use crate::tile_layer_group::TileLayerGroup;
use crate::uniform_buffer::UniformBuffer;

pub struct Context {
    base: ContextBase,
    backend: Rc<BackendImpl>,
    weak_self: Weak<Context>,
    global_uniform_buffers: RefCell<UniformBufferArray>,
    // Cached buffers (aligned with Metal): (buffer, size in bytes)
    tile_vertex_buffer: RefCell<Option<(wgpu::Buffer, u64)>>,
    tile_index_buffer: RefCell<Option<(wgpu::Buffer, u64)>>,
    // Cached clipping resources
    clip_mask_shader: RefCell<Option<Weak<dyn ShaderProgramBase>>>,
    clip_mask_renderable: Cell<Option<usize>>,
    clip_mask_pipeline_hash: Cell<Option<u64>>,
    clip_mask_uniform_buffers: RefCell<Vec<wgpu::Buffer>>,
    // `defaultVertexBuffers`: the default attribute buffers of
    // UploadPass::build_attribute_bindings, one per attribute type, with the
    // vertex count each covers.
    dummy_vertex_buffers: RefCell<HashMap<AttributeDataType, (usize, UniqueVertexBufferResource)>>,
    /// Command buffers the command encoders have submitted.
    submissions: Cell<u64>,
}

/// `clipMaskStencilMode`
fn clip_mask_stencil_mode() -> StencilMode {
    StencilMode {
        test: Test::Always,
        ref_: 0,
        mask: 0xFF,
        fail: StencilOpType::Keep,
        depth_fail: StencilOpType::Keep,
        pass: StencilOpType::Replace,
    }
}

/// `clipMaskDepthMode`
fn clip_mask_depth_mode() -> DepthMode {
    DepthMode { func: DepthFunctionType::Always, mask: DepthMaskType::ReadOnly, range: Range::new(0.0, 1.0) }
}

impl Context {
    /// `Context(RendererBackend&)`
    pub fn new(backend: Rc<BackendImpl>) -> Rc<Context> {
        Rc::new_cyclic(|weak| Context {
            base: ContextBase::new(ContextBase::MINIMUM_REQUIRED_VERTEX_BINDING_COUNT),
            backend,
            weak_self: weak.clone(),
            global_uniform_buffers: RefCell::new(UniformBufferArray::new()),
            tile_vertex_buffer: RefCell::new(None),
            dummy_vertex_buffers: RefCell::default(),
            tile_index_buffer: RefCell::new(None),
            clip_mask_shader: RefCell::new(None),
            clip_mask_renderable: Cell::new(None),
            clip_mask_pipeline_hash: Cell::new(None),
            clip_mask_uniform_buffers: RefCell::new(Vec::new()),
            submissions: Cell::new(0),
        })
    }

    /// How many command buffers the command encoders have submitted,
    /// counting one as it is being submitted.
    pub(crate) fn submissions(&self) -> u64 {
        self.submissions.get()
    }

    pub(crate) fn count_submission(&self) {
        self.submissions.set(self.submissions.get() + 1);
    }

    /// The owning `Rc` (resources keep the context alive, as the C++
    /// `Context&` members assume).
    pub fn rc(&self) -> Rc<Context> {
        self.weak_self.upgrade().expect("webgpu::Context used after drop")
    }

    /// `getBackend()`
    pub fn get_backend(&self) -> &Rc<BackendImpl> {
        &self.backend
    }

    pub fn rendering_stats_mut(&self) -> RefMut<'_, RenderingStats> {
        GfxContext::rendering_stats(self)
    }

    /// `createOffscreenTexture(size, type, depth, stencil)`
    pub fn create_offscreen_texture_with(
        &self,
        size: Size,
        type_: TextureChannelDataType,
        depth: bool,
        stencil: bool,
    ) -> OffscreenTexture {
        OffscreenTexture::new(self.rc(), size, type_, depth, stencil)
    }

    /// `createTexture2D()` with the concrete type.
    pub fn create_webgpu_texture_2d(&self) -> Rc<RefCell<Texture2D>> {
        Rc::new(RefCell::new(Texture2D::new(self.rc())))
    }

    /// The placeholder vertex buffers (see `UploadPass::dummy_vertex_buffer`).
    /// Port-only: drops the placeholder vertex buffers, which hold an `Rc`
    /// to this context (see the top of this file).
    pub(crate) fn release_cached_buffers(&self) {
        self.dummy_vertex_buffers.borrow_mut().clear();
    }

    pub(crate) fn dummy_vertex_buffers(
        &self,
    ) -> &RefCell<HashMap<AttributeDataType, (usize, UniqueVertexBufferResource)>> {
        &self.dummy_vertex_buffers
    }

    /// A buffer holding `data` (zero padded to 4 bytes), as `createBuffer`
    /// makes one (usage | CopyDst, initial queue write).
    fn create_raw_buffer(&self, data: &[u8], usage: wgpu::BufferUsages) -> Option<(wgpu::Buffer, u64)> {
        let device = self.backend.get_device()?;
        let queue = self.backend.get_queue()?;
        let padded_size = (data.len() + 3) & !3usize;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Buffer Resource"),
            size: padded_size as u64,
            usage: usage | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut padded = data.to_vec();
        padded.resize(padded_size, 0);
        queue.write_buffer(&buffer, 0, &padded);
        Some((buffer, data.len() as u64))
    }

    /// `getTileVertexBuffer()`: `RenderStaticData::tileVertices()`, the four
    /// corners of the tile extent as short2 (inlined: RenderStaticData is a
    /// renderer type).
    pub fn get_tile_vertex_buffer(&self) -> Option<(wgpu::Buffer, u64)> {
        let mut cached = self.tile_vertex_buffer.borrow_mut();
        if cached.is_none() {
            const EXTENT: i16 = 8192; // util::EXTENT
            let vertices: [[i16; 2]; 4] = [[0, 0], [EXTENT, 0], [0, EXTENT], [EXTENT, EXTENT]];
            *cached = self.create_raw_buffer(bytemuck::cast_slice(&vertices), wgpu::BufferUsages::VERTEX);
        }
        cached.clone()
    }

    /// `getTileIndexBuffer()`: `RenderStaticData::quadTriangleIndices()`.
    pub fn get_tile_index_buffer(&self) -> Option<(wgpu::Buffer, u64)> {
        let mut cached = self.tile_index_buffer.borrow_mut();
        if cached.is_none() {
            let indices: [u16; 6] = [0, 1, 2, 1, 2, 3];
            *cached = self.create_raw_buffer(bytemuck::cast_slice(&indices), wgpu::BufferUsages::INDEX);
        }
        cached.clone()
    }

    fn get_clip_mask_shader(&self, shaders: &ShaderRegistry) -> Option<ShaderProgramBasePtr> {
        if let Some(shader) = self.clip_mask_shader.borrow().as_ref().and_then(Weak::upgrade) {
            return Some(shader);
        }
        let group = shaders.get_shader_group("ClippingMaskProgram")?;
        let shader = group.get_or_create_shader(self, &StringIDSetsPair::default(), "a_pos")?;
        *self.clip_mask_shader.borrow_mut() = Some(Rc::downgrade(&shader));
        Some(shader)
    }

    /// `renderTileClippingMasks(renderPass, staticData, tileUBOs)`
    fn render_tile_clipping_masks_impl(
        &self,
        render_pass: &dyn mln_gfx::render_pass::RenderPass,
        shaders: &ShaderRegistry,
        tile_ubos: &[ClipUBO],
    ) -> bool {
        if tile_ubos.is_empty() {
            return false;
        }
        let Some(shader_ptr) = self.get_clip_mask_shader(shaders) else {
            eprintln!("WebGPU: Failed to acquire clipping mask shader");
            return false;
        };
        let Some(shader) = as_webgpu_shader(&shader_ptr) else {
            eprintln!("WebGPU: Failed to acquire clipping mask shader");
            return false;
        };
        let Some(webgpu_render_pass) = render_pass.as_any().downcast_ref::<RenderPass>() else {
            return false;
        };
        let mut encoder_guard = webgpu_render_pass.get_encoder();
        let Some(encoder) = encoder_guard.as_mut() else {
            eprintln!("WebGPU: render pass encoder unavailable for clipping masks");
            return false;
        };

        let renderable = webgpu_render_pass.get_renderable_id();
        if self.clip_mask_renderable.get() != Some(renderable) {
            self.clip_mask_pipeline_hash.set(None);
            self.clip_mask_renderable.set(Some(renderable));
        }

        let (Some((vertex_buffer, vertex_size)), Some((index_buffer, index_size))) =
            (self.get_tile_vertex_buffer(), self.get_tile_index_buffer())
        else {
            eprintln!("WebGPU: Missing tile geometry buffers for clipping masks");
            return false;
        };

        let color_mode = ColorMode::disabled();
        let Some(source) = shader_source(BuiltIn::ClippingMaskProgram) else {
            return false;
        };
        let vertex_attribute = wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Sint16x2,
            offset: 0,
            shader_location: source.attributes[0].index as u32,
        };
        // sizeof(gfx::Vertex<PositionOnlyLayoutAttributes>): one short2
        let array_stride = std::mem::size_of::<[i16; 2]>() as u64;
        let vertex_layout =
            VertexLayout { array_stride, step_mode: wgpu::VertexStepMode::Vertex, attributes: vec![vertex_attribute] };
        if self.clip_mask_pipeline_hash.get().is_none() {
            self.clip_mask_pipeline_hash.set(Some(hash(&[
                color_mode.hash(),
                array_stride,
                vertex_attribute.format as u64,
                u64::from(vertex_attribute.shader_location),
            ])));
        }
        let Some(pipeline) = shader.get_render_pipeline(
            std::slice::from_ref(&vertex_layout),
            &color_mode,
            &clip_mask_depth_mode(),
            &clip_mask_stencil_mode(),
            DrawModeType::Triangles,
            self.clip_mask_pipeline_hash.get(),
        ) else {
            eprintln!("WebGPU: Failed to create clipping mask pipeline");
            return false;
        };

        encoder.set_pipeline(&pipeline);
        encoder.set_vertex_buffer(0, vertex_buffer.slice(0..vertex_size));
        encoder.set_index_buffer(index_buffer.slice(0..index_size), wgpu::IndexFormat::Uint16);

        let ubo_size = std::mem::size_of::<ClipUBO>() as u64;
        let mut uniform_buffers = self.clip_mask_uniform_buffers.borrow_mut();
        uniform_buffers.clear();
        uniform_buffers.reserve(tile_ubos.len());
        for ubo in tile_ubos {
            let Some((buffer, _)) = self.create_raw_buffer(bytemuck::bytes_of(ubo), wgpu::BufferUsages::UNIFORM) else {
                eprintln!("WebGPU: Failed to allocate uniform buffer for clipping mask tile");
                return false;
            };
            uniform_buffers.push(buffer);
        }

        let Some(device) = self.backend.get_device() else {
            eprintln!("WebGPU: Device unavailable for clipping masks");
            return false;
        };
        let Some(&bind_group_index) = shader.get_bind_group_order().first() else {
            eprintln!("WebGPU: No bind group layouts available for clipping mask shader");
            return false;
        };
        let Some(layout) = shader.get_bind_group_layout(bind_group_index) else {
            eprintln!("WebGPU: Bind group layout missing for clipping mask shader");
            return false;
        };
        let Some(uniform_binding) = shader
            .get_binding_infos_for_group(bind_group_index)
            .iter()
            .find(|info| info.type_ == BindingType::UniformBuffer)
            .map(|info| info.binding)
        else {
            eprintln!("WebGPU: Uniform binding not found for clipping mask shader");
            return false;
        };

        for (i, ubo) in tile_ubos.iter().enumerate() {
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: uniform_binding,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &uniform_buffers[i],
                        offset: 0,
                        size: wgpu::BufferSize::new(ubo_size),
                    }),
                }],
            });
            encoder.set_bind_group(bind_group_index, &bind_group, &[]);
            encoder.set_stencil_reference(ubo.stencil_ref);
            encoder.draw_indexed(0..6, 0, 0..1);
        }
        drop(encoder_guard);

        let mut stats = self.rendering_stats_mut();
        stats.num_draw_calls += tile_ubos.len() as i32;
        stats.total_draw_calls += tile_ubos.len() as i32;
        true
    }

    /// `createUniformBuffer` with the concrete type.
    pub fn create_webgpu_uniform_buffer(&self, data: &[u8]) -> Rc<UniformBuffer> {
        Rc::new(UniformBuffer::new(self, Some(data), data.len()))
    }
}

impl GfxContext for Context {
    fn context_base(&self) -> &ContextBase {
        &self.base
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn begin_frame(&self) {
        // TODO(port): backend.getThreadPool().runRenderJobs()
    }

    fn end_frame(&self) {
        // End the frame - submit WebGPU commands
    }

    fn perform_cleanup(&self) {
        // TODO(port): backend.getThreadPool().runRenderJobs()
        let mut stats = self.rendering_stats_mut();
        stats.num_draw_calls = 0;
        stats.num_frames += 1;
    }

    fn reduce_memory_usage(&self) {
        // TODO(port): backend.getThreadPool().runRenderJobs()
    }

    /// `createOffscreenTexture(size, type)`: depth, no stencil.
    fn create_offscreen_texture(
        &self,
        size: Size,
        type_: TextureChannelDataType,
    ) -> Box<dyn mln_gfx::offscreen_texture::OffscreenTexture> {
        Box::new(self.create_offscreen_texture_with(size, type_, true, false))
    }

    fn create_command_encoder(&self) -> Box<dyn mln_gfx::command_encoder::CommandEncoder> {
        // C++ also records the encoder as `currentCommandEncoder`; nothing
        // reads it on the drawable path.
        Box::new(CommandEncoder::new(self.rc()))
    }

    fn clear_stencil_buffer(&self, _value: i32) {
        // Clear stencil buffer to specified value (no-op in C++)
    }

    fn set_dirty_state(&self) {
        // Mark state as needing update (no-op in C++)
    }

    fn create_vertex_attribute_array(&self) -> VertexAttributeArrayPtr {
        VertexAttributeArray::new_ptr()
    }

    fn create_drawable_builder(&self, name: String) -> UniqueDrawableBuilder {
        Box::new(DrawableBuilder::new(name))
    }

    fn create_uniform_buffer(&self, data: &[u8], _persistent: bool, _ssbo: bool) -> UniformBufferPtr {
        self.create_webgpu_uniform_buffer(data)
    }

    fn create_layer_uniform_buffer_array(&self) -> UniqueUniformBufferArray {
        Box::new(UniformBufferArray::new())
    }

    /// Align with Metal - just get the shader from the registry, no caching.
    fn get_generic_shader(&self, registry: &ShaderRegistry, name: &str) -> Option<ShaderProgramBasePtr> {
        let shader_group = registry.get_shader_group(name)?;
        shader_group.get_or_create_shader(self, &StringIDSetsPair::default(), "a_pos")
    }

    fn create_tile_layer_group(
        &self,
        layer_index: i32,
        initial_capacity: usize,
        name: String,
    ) -> Option<LayerGroupBasePtr> {
        Some(Rc::new(RefCell::new(TileLayerGroup::new(layer_index, initial_capacity, name))))
    }

    fn create_layer_group(&self, layer_index: i32, initial_capacity: usize, name: String) -> Option<LayerGroupBasePtr> {
        Some(Rc::new(RefCell::new(LayerGroup::new(layer_index, initial_capacity, name))))
    }

    fn create_texture_2d(&self) -> Texture2DPtr {
        self.create_webgpu_texture_2d()
    }

    fn reset_state(&self, _depth_mode: DepthMode, _color_mode: ColorMode) {
        // Reset WebGPU render state (no-op in C++)
    }

    fn emplace_or_update_uniform_buffer(
        &self,
        ptr: &mut Option<UniformBufferPtr>,
        data: &[u8],
        persistent: bool,
    ) -> bool {
        match ptr {
            None => {
                *ptr = Some(self.create_uniform_buffer(data, persistent, false));
                true
            }
            Some(buffer) => {
                // Update existing buffer
                buffer.update(data);
                false
            }
        }
    }

    fn get_global_uniform_buffers(&self) -> Ref<'_, UniformBufferArray> {
        self.global_uniform_buffers.borrow()
    }

    fn mutable_global_uniform_buffers(&self) -> RefMut<'_, UniformBufferArray> {
        self.global_uniform_buffers.borrow_mut()
    }

    /// In WebGPU, buffers can't be bound globally to the render pass like in
    /// Metal; the pass refers to the context's array for the drawables' bind
    /// groups (C++ stores a pointer to it).
    fn bind_global_uniform_buffers(&self, render_pass: &dyn mln_gfx::render_pass::RenderPass) {
        if let Some(pass) = render_pass.as_any().downcast_ref::<RenderPass>() {
            pass.set_global_uniform_buffers(true);
        }
    }

    fn unbind_global_uniform_buffers(&self, render_pass: &dyn mln_gfx::render_pass::RenderPass) {
        if let Some(pass) = render_pass.as_any().downcast_ref::<RenderPass>() {
            pass.set_global_uniform_buffers(false);
        }
    }

    fn render_tile_clipping_masks(
        &self,
        render_pass: &dyn mln_gfx::render_pass::RenderPass,
        shaders: &ShaderRegistry,
        tile_ubos: &[ClipUBO],
    ) -> bool {
        self.render_tile_clipping_masks_impl(render_pass, shaders, tile_ubos)
    }
}
