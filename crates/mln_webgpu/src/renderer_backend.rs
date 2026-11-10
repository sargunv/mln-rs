// Ports include/mln/webgpu/renderer_backend.hpp and src/mln/webgpu/renderer_backend.cpp
//
// The C++ backend stores raw `WGPUDevice`/`WGPUQueue`/instance handles in its
// `Impl` and every resource reaches them through `Context& -> getBackend()`.
// Here the `Impl` is shared (`Rc<BackendImpl>`) by the backend and the
// context, and resources hold an `Rc<Context>`.
//
// `getDefaultRenderable()` is pure virtual and implemented by the platform
// backends (HeadlessBackend), which embed this struct.
//
// TODO(port): `setSurface` (window surfaces) and the `getCurrentTextureView`
// / `getDepthStencilView` / `getFramebufferSize` virtuals used as render pass
// fallbacks when a renderable has no views of its own; the headless
// renderable always provides them.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use mln_gfx::renderer_backend::{ContextMode, RendererBackendBase};
use mln_gfx::shader_registry::ShaderRegistry;
use mln_gfx::shaders::program_parameters::ProgramParameters;
use mln_gfx::shaders::shader_source::BuiltIn;

use crate::context::Context;
use crate::shaders::shader_group::ShaderGroup;
use crate::shaders::shader_source::shader_source;
use crate::writes::Writes;

/// `RendererBackend::Impl`
#[derive(Default)]
pub struct BackendImpl {
    instance: RefCell<Option<wgpu::Instance>>,
    adapter: RefCell<Option<wgpu::Adapter>>,
    device: RefCell<Option<wgpu::Device>>,
    queue: RefCell<Option<wgpu::Queue>>,
    /// `wgpu::TextureFormat::Undefined` is `None`.
    depth_stencil_format: Cell<Option<wgpu::TextureFormat>>,
    color_format: Cell<Option<wgpu::TextureFormat>>,
    writes: RefCell<Writes>,
}

impl BackendImpl {
    pub fn set_instance(&self, instance: Option<wgpu::Instance>) {
        *self.instance.borrow_mut() = instance;
    }
    pub fn set_adapter(&self, adapter: Option<wgpu::Adapter>) {
        *self.adapter.borrow_mut() = adapter;
    }
    pub fn set_device(&self, device: Option<wgpu::Device>) {
        *self.device.borrow_mut() = device;
        *self.writes.borrow_mut() = Writes::default();
    }
    pub fn set_queue(&self, queue: Option<wgpu::Queue>) {
        *self.queue.borrow_mut() = queue;
    }
    pub fn get_instance(&self) -> Option<wgpu::Instance> {
        self.instance.borrow().clone()
    }
    pub fn get_adapter(&self) -> Option<wgpu::Adapter> {
        self.adapter.borrow().clone()
    }
    pub fn get_device(&self) -> Option<wgpu::Device> {
        self.device.borrow().clone()
    }
    pub fn get_queue(&self) -> Option<wgpu::Queue> {
        self.queue.borrow().clone()
    }
    pub fn set_depth_stencil_format(&self, format: Option<wgpu::TextureFormat>) {
        self.depth_stencil_format.set(format);
    }
    pub fn get_depth_stencil_format(&self) -> Option<wgpu::TextureFormat> {
        self.depth_stencil_format.get()
    }
    pub fn set_color_format(&self, format: Option<wgpu::TextureFormat>) {
        self.color_format.set(format);
    }
    pub fn get_color_format(&self) -> Option<wgpu::TextureFormat> {
        self.color_format.get()
    }

    /// Port-only: writes `data` at `offset` in `buffer` ahead of the next
    /// [`submit`](Self::submit)'s commands, as `Queue::write_buffer` does,
    /// but small writes are gathered and copied into place together.
    pub(crate) fn write_buffer(&self, buffer: &wgpu::Buffer, offset: u64, data: &[u8]) {
        if let Some(queue) = self.queue.borrow().as_ref() {
            self.writes.borrow_mut().write(queue, buffer, offset, data);
        }
    }

    /// Port-only: submits `command_buffer` after the copies of the writes
    /// gathered by [`write_buffer`](Self::write_buffer).
    pub(crate) fn submit(&self, command_buffer: wgpu::CommandBuffer) {
        let (Some(device), Some(queue)) = (self.device.borrow().clone(), self.queue.borrow().clone()) else {
            return;
        };
        let writes = self.writes.borrow_mut().encode(&device, &queue);
        queue.submit(writes.into_iter().chain([command_buffer]));
    }
}

/// `webgpu::RendererBackend`: embedded by the platform backends, which
/// implement `gfx::RendererBackend` by delegating to it.
pub struct RendererBackend {
    pub base: RendererBackendBase,
    impl_: Rc<BackendImpl>,
}

/// The shader types registered by `initShaders`, in the C++ order.
const REGISTERED_SHADERS: &[BuiltIn] = &[
    BuiltIn::BackgroundShader,
    BuiltIn::BackgroundPatternShader,
    BuiltIn::CircleShader,
    BuiltIn::ClippingMaskProgram,
    BuiltIn::CollisionBoxShader,
    BuiltIn::CollisionCircleShader,
    BuiltIn::DebugShader,
    BuiltIn::FillShader,
    BuiltIn::FillOutlineShader,
    BuiltIn::FillPatternShader,
    BuiltIn::FillOutlinePatternShader,
    BuiltIn::FillOutlineTriangulatedShader,
    BuiltIn::FillExtrusionShader,
    BuiltIn::FillExtrusionPatternShader,
    BuiltIn::HeatmapShader,
    BuiltIn::HeatmapTextureShader,
    BuiltIn::HillshadeShader,
    BuiltIn::HillshadePrepareShader,
    BuiltIn::ColorReliefShader,
    BuiltIn::LineShader,
    BuiltIn::LineGradientShader,
    BuiltIn::LinePatternShader,
    BuiltIn::LineSDFShader,
    BuiltIn::LineGradientSDFShader,
    BuiltIn::LocationIndicatorShader,
    BuiltIn::LocationIndicatorTexturedShader,
    BuiltIn::RasterShader,
    BuiltIn::SymbolIconShader,
    BuiltIn::SymbolSDFShader,
    BuiltIn::SymbolTextAndIconShader,
    BuiltIn::WideVectorShader,
];

impl RendererBackend {
    pub fn new(context_mode: ContextMode) -> Self {
        RendererBackend { base: RendererBackendBase::new(context_mode), impl_: Rc::new(BackendImpl::default()) }
    }

    pub fn backend_impl(&self) -> &Rc<BackendImpl> {
        &self.impl_
    }

    /// `context.reset()`, which also releases the buffers the context caches
    /// (`Context::release_cached_buffers`).
    pub(crate) fn reset_context(&mut self) {
        if let Some(context) = self.base.reset_context()
            && let Some(context) = context.as_any().downcast_ref::<Context>()
        {
            context.release_cached_buffers();
        }
    }

    /// `createContext()`
    pub fn create_context(&self) -> Rc<Context> {
        Context::new(self.impl_.clone())
    }

    /// `activate()` / `deactivate()`: activation logic if needed (none).
    pub fn activate(&self) {}
    pub fn deactivate(&self) {}

    /// `initShaders`: register one `webgpu::ShaderGroup` per built-in shader.
    ///
    /// C++ registers all 31 types; a type whose WGSL is not part of this
    /// build is skipped (TODO(port): all sources once the
    /// `@maplibre//:webgpu-shader-headers` filegroup feeds the extractor).
    /// A duplicate registration is an error, as in C++ (which throws).
    pub fn init_shaders(&self, registry: &ShaderRegistry, program_parameters: &ProgramParameters) {
        for &shader_id in REGISTERED_SHADERS {
            let Some(source) = shader_source(shader_id) else {
                continue;
            };
            let group = Rc::new(ShaderGroup::new(shader_id, program_parameters.clone()));
            if !registry.register_shader_group(group, source.name) {
                panic!("Failed to register {} with shader registry!", source.name);
            }
        }
    }

    pub fn set_instance(&self, instance: Option<wgpu::Instance>) {
        self.impl_.set_instance(instance);
    }
    pub fn set_device(&self, device: Option<wgpu::Device>) {
        self.impl_.set_device(device);
    }
    pub fn set_queue(&self, queue: Option<wgpu::Queue>) {
        self.impl_.set_queue(queue);
    }
    pub fn get_instance(&self) -> Option<wgpu::Instance> {
        self.impl_.get_instance()
    }
    pub fn get_device(&self) -> Option<wgpu::Device> {
        self.impl_.get_device()
    }
    pub fn get_queue(&self) -> Option<wgpu::Queue> {
        self.impl_.get_queue()
    }
    pub fn set_depth_stencil_format(&self, format: Option<wgpu::TextureFormat>) {
        self.impl_.set_depth_stencil_format(format);
    }
    pub fn get_depth_stencil_format(&self) -> Option<wgpu::TextureFormat> {
        self.impl_.get_depth_stencil_format()
    }
    pub fn set_color_format(&self, format: Option<wgpu::TextureFormat>) {
        self.impl_.set_color_format(format);
    }
    pub fn get_color_format(&self) -> Option<wgpu::TextureFormat> {
        self.impl_.get_color_format()
    }
}

impl Drop for RendererBackend {
    fn drop(&mut self) {
        self.reset_context();
    }
}
