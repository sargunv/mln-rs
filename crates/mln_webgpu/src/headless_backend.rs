// Ports platform/default/include/mln/webgpu/headless_backend.hpp and
// src/mln/webgpu/headless_backend.cpp (the wgpu-native branch, on the wgpu
// crate), plus `Backend::Create<Backend::Type::WebGPU>`.
//
// Deviation: C++ wgpu-native creates the instance with default options
// (all backends) and takes the HighPerformance adapter. The port restricts
// the instance to `WGPU_BACKEND` if set (wgpu's env var, e.g. "vulkan"),
// else Vulkan on Linux, so tests run on lavapipe (VK_DRIVER_FILES) like
// the C++ CI; other platforms use wgpu's defaults. A host that brings its
// own WebGPU (a browser's) renders through `SurfaceBackend::from_host`.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use mln_gfx::context::Context as GfxContext;
use mln_gfx::headless_backend;
use mln_gfx::renderable::{Renderable, RenderableBase, SwapBehaviour};
use mln_gfx::renderer_backend::{self, ContextMode, RendererBackendBase};
use mln_gfx::shader_registry::ShaderRegistry;
use mln_gfx::shaders::program_parameters::ProgramParameters;
use mln_gfx::texture2d::Texture2DPtr;
use mln_gfx::types::TextureChannelDataType;
use mln_util::image::PremultipliedImage;
use mln_util::logging::{Event, Log};
use mln_util::size::Size;

use crate::context::Context;
use crate::offscreen_texture::OffscreenTextureResource;
use crate::renderable_resource::{RenderableResource, RenderableResourceBox, downcast_resource};
use crate::renderer_backend::RendererBackend;

/// `HeadlessRenderableResource`: renders into an offscreen texture.
struct HeadlessRenderableResource {
    offscreen_texture: OffscreenTextureResource,
}

impl HeadlessRenderableResource {
    fn new(context: Rc<Context>, size: Size) -> Self {
        // Create offscreen texture using the proper WebGPU OffscreenTexture class
        // (C++ wraps it in a gfx::OffscreenTexture; only its resource is used).
        HeadlessRenderableResource {
            offscreen_texture: OffscreenTextureResource::new(
                context,
                size,
                TextureChannelDataType::UnsignedByte,
                true,
                true,
            ),
        }
    }
}

impl RenderableResource for HeadlessRenderableResource {
    fn bind(&self) {
        self.offscreen_texture.bind();
    }
    fn swap(&self) {
        // For headless rendering, we don't need to swap
    }
    fn finish_frame(&self, encoder: &mut wgpu::CommandEncoder) {
        self.offscreen_texture.encode_requested_copy(encoder);
    }
    fn get_color_texture_view(&self) -> Option<wgpu::TextureView> {
        self.offscreen_texture.get_color_texture_view()
    }
    fn get_color_texture_format(&self) -> Option<wgpu::TextureFormat> {
        self.offscreen_texture.get_color_texture_format()
    }
    fn get_depth_stencil_texture_view(&self) -> Option<wgpu::TextureView> {
        self.offscreen_texture.get_depth_stencil_texture_view()
    }
    fn get_depth_stencil_texture_format(&self) -> Option<wgpu::TextureFormat> {
        self.offscreen_texture.get_depth_stencil_texture_format()
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// `HeadlessBackend::Impl`
#[derive(Default)]
struct Impl {
    offscreen_texture: Option<wgpu::Texture>,
    offscreen_texture_view: Option<wgpu::TextureView>,
    depth_texture: Option<wgpu::Texture>,
    depth_texture_view: Option<wgpu::TextureView>,
    offscreen_texture_size: Size,
}

/// `webgpu::HeadlessBackend`: a `webgpu::RendererBackend` that is its own
/// default renderable (an offscreen texture).
pub struct HeadlessBackend {
    renderer_backend: RendererBackend,
    renderable: RenderableBase,
    impl_: RefCell<Impl>,
    active: Cell<bool>,
}

fn instance_backends() -> wgpu::Backends {
    if let Some(backends) = wgpu::Backends::from_env() {
        return backends;
    }
    if cfg!(target_os = "linux") { wgpu::Backends::VULKAN } else { wgpu::Backends::all() }
}

/// Creates the wgpu instance for `backends`. Shared with `SurfaceBackend`.
pub(crate) fn create_instance(backends: wgpu::Backends) -> wgpu::Instance {
    wgpu::Instance::new(wgpu::InstanceDescriptor { backends, ..wgpu::InstanceDescriptor::new_without_display_handle() })
}

impl HeadlessBackend {
    /// `HeadlessBackend(size = {256, 256}, swapBehaviour = NoFlush,
    /// contextMode = Unique)`
    pub fn new(size: Size, _swap_behaviour: SwapBehaviour, mode: ContextMode) -> Self {
        let backend = HeadlessBackend {
            renderer_backend: RendererBackend::new(mode),
            renderable: RenderableBase::new(size, None),
            impl_: RefCell::new(Impl::default()),
            active: Cell::new(false),
        };

        // wgpu-native backend initialization
        let instance = create_instance(instance_backends());

        // Request adapter (blocking)
        let adapter = match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        })) {
            Ok(adapter) => adapter,
            Err(e) => {
                eprintln!("WebGPU HeadlessBackend: No adapter found: {e}");
                return backend;
            }
        };

        // Request device (blocking), with the WebGPU default limits lowered to
        // what the adapter supports: the Metal of the iOS, tvOS and visionOS
        // simulators allows fewer inter-stage shader variables than they ask.
        let required_limits = wgpu::Limits::default().or_worse_values_from(&adapter.limits());
        let (device, queue) = match pollster::block_on(
            adapter.request_device(&wgpu::DeviceDescriptor { required_limits, ..Default::default() }),
        ) {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("WebGPU HeadlessBackend: Failed to create device: {e}");
                return backend;
            }
        };

        // C++ sets no uncaptured-error callback, and Dawn logs the error
        // and invalidates the object. wgpu's default handler panics; log as
        // the surface backend does (Dawn's logUncapturedError).
        device.on_uncaptured_error(std::sync::Arc::new(|error: wgpu::Error| {
            Log::error(Event::Render, format!("WebGPU validation error: {error}"));
        }));

        // Store device, queue, and instance in base class
        let backend_impl = backend.renderer_backend.backend_impl();
        backend_impl.set_device(Some(device));
        backend_impl.set_queue(Some(queue));
        backend_impl.set_adapter(Some(adapter));
        backend_impl.set_instance(Some(instance));

        // Create offscreen render texture
        backend.create_offscreen_textures();
        backend
    }

    /// `HeadlessBackend::Create(size)` with the C++ defaults.
    pub fn create(size: Size) -> Self {
        Self::new(size, SwapBehaviour::NoFlush, ContextMode::Unique)
    }

    pub fn webgpu_backend(&self) -> &RendererBackend {
        &self.renderer_backend
    }

    /// The device, for tests and tools.
    pub fn get_device(&self) -> Option<wgpu::Device> {
        self.renderer_backend.get_device()
    }

    /// Info about the adapter in use (e.g. "llvmpipe" on lavapipe).
    pub fn get_adapter_info(&self) -> Option<wgpu::AdapterInfo> {
        self.renderer_backend.backend_impl().get_adapter().map(|a| a.get_info())
    }

    /// `createOffscreenTextures()`. Quirk ported: these textures are not used
    /// for rendering; the renderable resource has its own offscreen texture.
    fn create_offscreen_textures(&self) {
        let Some(device) = self.renderer_backend.get_device() else {
            return;
        };
        let renderable_size = self.get_size();
        let mut impl_ = self.impl_.borrow_mut();
        impl_.offscreen_texture_size = renderable_size;
        let extent =
            wgpu::Extent3d { width: renderable_size.width, height: renderable_size.height, depth_or_array_layers: 1 };

        // Create offscreen color texture
        let color = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Headless Color Texture"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        impl_.offscreen_texture_view = Some(color.create_view(&wgpu::TextureViewDescriptor {
            label: Some("Headless Color TextureView"),
            format: Some(wgpu::TextureFormat::Rgba8Unorm),
            dimension: Some(wgpu::TextureViewDimension::D2),
            ..Default::default()
        }));
        impl_.offscreen_texture = Some(color);

        // Create depth/stencil texture
        let depth = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Headless Depth/Stencil Texture"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth24PlusStencil8,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        impl_.depth_texture_view = Some(depth.create_view(&wgpu::TextureViewDescriptor {
            label: Some("Headless Depth/Stencil TextureView"),
            format: Some(wgpu::TextureFormat::Depth24PlusStencil8),
            dimension: Some(wgpu::TextureViewDimension::D2),
            ..Default::default()
        }));
        impl_.depth_texture = Some(depth);
    }

    fn webgpu_context(&self) -> Rc<Context> {
        let ctx = <Self as renderer_backend::RendererBackend>::get_context(self);
        ctx.as_any().downcast_ref::<Context>().expect("webgpu context").rc()
    }

    fn with_resource<R>(&self, f: impl FnOnce(&HeadlessRenderableResource) -> R) -> Option<R> {
        let resource = self.get_resource()?;
        downcast_resource::<HeadlessRenderableResource>(&*resource).map(f)
    }

    /// `takeTexture()`: take ownership of the current texture
    pub fn take_texture(&self) -> Option<Texture2DPtr> {
        self.with_resource(|r| r.offscreen_texture.take_texture()).flatten()
    }

    /// `getFramebufferSize()`
    pub fn get_framebuffer_size(&self) -> Size {
        self.get_size()
    }
}

impl Drop for HeadlessBackend {
    fn drop(&mut self) {
        // Explicitly reset the renderable resource before the context
        self.set_resource(None);
        self.renderer_backend.reset_context();
    }
}

impl Renderable for HeadlessBackend {
    fn renderable_base(&self) -> &RenderableBase {
        &self.renderable
    }
}

impl renderer_backend::RendererBackend for HeadlessBackend {
    fn renderer_backend_base(&self) -> &RendererBackendBase {
        &self.renderer_backend.base
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn get_default_renderable(&self) -> &dyn Renderable {
        if !self.has_resource() {
            let resource = HeadlessRenderableResource::new(self.webgpu_context(), self.get_size());
            self.set_resource(Some(RenderableResourceBox::new_rc(resource)));
        }
        self
    }

    fn init_shaders(&self, registry: &ShaderRegistry, parameters: &ProgramParameters) {
        self.renderer_backend.init_shaders(registry, parameters);
    }

    fn create_context(&self) -> Rc<dyn GfxContext> {
        self.renderer_backend.create_context()
    }

    fn activate(&self) {
        self.active.set(true);
        // Ensure textures are created if size changed
        if self.impl_.borrow().offscreen_texture_size != self.get_size() {
            self.create_offscreen_textures();
        }
        self.renderer_backend.activate();
    }

    fn deactivate(&self) {
        self.active.set(false);
        self.renderer_backend.deactivate();
    }
}

impl headless_backend::HeadlessBackend for HeadlessBackend {
    fn read_still_image(&self) -> PremultipliedImage {
        self.with_resource(|r| r.offscreen_texture.read_still_image())
            .unwrap_or_else(|| PremultipliedImage::new(self.get_size()))
    }

    fn get_renderer_backend(&self) -> &dyn renderer_backend::RendererBackend {
        self
    }
}
