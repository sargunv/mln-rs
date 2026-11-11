// Ports the window-surface part of platform/glfw/glfw_webgpu_backend.hpp and
// platform/glfw/glfw_webgpu_backend_impl.ipp (the Linux, wgpu-native branch):
// a `webgpu::RendererBackend` that is its own default `gfx::Renderable` and
// presents to a `wgpu::Surface`. The window stays with the host (such as
// examples/desktop), which creates the surface and reports framebuffer
// resizes through `set_framebuffer_size`.
//
// Differences from C++, all structural:
// - The C++ backend reads `glfwGetFramebufferSize` where it needs the size;
//   here the window reports it (`set_framebuffer_size`, also what C++'s
//   `onFramebufferResize` calls), so `reconfigureSurface` and the depth
//   texture use the size last reported.
// - C++ reconfigures a resized surface in `swap()`, after rendering one
//   frame into the old-size swapchain texture with the new-size viewport
//   (Dawn logs a validation error for it). The port also reconfigures
//   before acquiring the next texture when a reconfigure is pending.
// - Single-threaded (the renderer runs on the main thread): the spin lock,
//   `frameInProgress` (never set in C++) and `isShuttingDown` atomics are
//   plain cells, and `waitForFrame` always succeeds immediately as in C++.
// - wgpu's default uncaptured-error handler panics; like the C++ Dawn
//   branch (`logUncapturedError`), errors are logged instead.
// - Port-only: `request_readback` copies the next presented frame into a
//   `PremultipliedImage` (the surface is configured with COPY_SRC when the
//   surface supports it), so a frame can be checked without screenshots.
//   The copy is mapped asynchronously: natively `swap` waits for it, and in
//   the browser, where nothing can wait for the GPU, `take_readback` returns
//   it once the event loop has mapped it.
// - Port-only: `set_overlay` draws over each frame before it is presented,
//   such as an app's controls over the map.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use mln_gfx::context::Context as GfxContext;
use mln_gfx::renderable::{Renderable, RenderableBase};
use mln_gfx::renderer_backend::{self, ContextMode, RendererBackendBase};
use mln_gfx::shader_registry::ShaderRegistry;
use mln_gfx::shaders::program_parameters::ProgramParameters;
use mln_util::image::PremultipliedImage;
use mln_util::logging::{Event, Log};
use mln_util::size::Size;
use mln_util::time::Instant;

use crate::renderable_resource::{RenderableResource, RenderableResourceBox};
use crate::renderer_backend::RendererBackend;

/// `maxConsecutiveErrors`
const MAX_CONSECUTIVE_ERRORS: i32 = 3;

/// `backendTypeToString`
fn backend_type_to_string(backend: wgpu::Backend) -> &'static str {
    match backend {
        wgpu::Backend::Noop => "Null",
        wgpu::Backend::BrowserWebGpu => "WebGPU",
        wgpu::Backend::Dx12 => "D3D12",
        wgpu::Backend::Metal => "Metal",
        wgpu::Backend::Vulkan => "Vulkan",
        wgpu::Backend::Gl => "OpenGL",
    }
}

/// The instance backends, as the headless backend picks them: `WGPU_BACKEND`
/// if set, else Vulkan on Linux (C++ wgpu-native: the default instance).
fn instance_backends() -> wgpu::Backends {
    if let Some(backends) = wgpu::Backends::from_env() {
        return backends;
    }
    if cfg!(target_os = "linux") { wgpu::Backends::VULKAN } else { wgpu::Backends::all() }
}

/// The usages and vsync'ed present mode to configure a surface with: COPY_SRC
/// for readback where the surface has it, and MAILBOX where it has it (see
/// `SurfaceState::vsync_present_mode`).
fn surface_options(capabilities: &wgpu::SurfaceCapabilities) -> (wgpu::TextureUsages, wgpu::PresentMode) {
    let usage = wgpu::TextureUsages::RENDER_ATTACHMENT | (capabilities.usages & wgpu::TextureUsages::COPY_SRC);
    let present_mode = if capabilities.present_modes.contains(&wgpu::PresentMode::Mailbox) {
        wgpu::PresentMode::Mailbox
    } else {
        wgpu::PresentMode::Fifo
    };
    (usage, present_mode)
}
/// Port-only: what an overlay draws into (`SurfaceBackend::set_overlay`):
/// the frame's swapchain texture, after the map and before it is presented.
pub struct OverlayTarget<'a> {
    pub device: &'a wgpu::Device,
    pub queue: &'a wgpu::Queue,
    pub view: &'a wgpu::TextureView,
    pub format: wgpu::TextureFormat,
    /// In physical pixels.
    pub size: Size,
}

/// Port-only: draws over a frame (`SurfaceBackend::set_overlay`). It submits
/// its own commands to the queue.
pub type Overlay = Box<dyn FnMut(&OverlayTarget<'_>)>;

/// The current swapchain texture and its view (`currentTexture`,
/// `currentTextureView`).
struct CurrentTexture {
    texture: wgpu::SurfaceTexture,
    view: wgpu::TextureView,
}

/// The members of `GLFWWebGPUBackend` that the renderable resource reaches
/// through its `backend` reference in C++.
pub struct SurfaceState {
    /// `None` from `SurfaceBackend::release_surface` until `set_surface`.
    surface: RefCell<Option<wgpu::Surface<'static>>>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    swap_chain_format: wgpu::TextureFormat,
    alpha_mode: wgpu::CompositeAlphaMode,
    surface_usage: Cell<wgpu::TextureUsages>,
    depth_stencil_format: Cell<Option<wgpu::TextureFormat>>,

    current: RefCell<Option<CurrentTexture>>,
    depth_stencil: RefCell<Option<(wgpu::Texture, wgpu::TextureView)>>,

    frame_presented: Cell<bool>,
    surface_configured: Cell<bool>,
    surface_needs_reconfigure: Cell<bool>,
    last_configured_size: Cell<Size>,
    consecutive_errors: Cell<i32>,
    enable_vsync: bool,
    /// symbols (user report: the window froze while hidden): the vsync'ed
    /// present mode. FIFO blocks in acquire/present on Wayland while the
    /// window is not visible (no frame callbacks), which stalls the whole
    /// event loop; MAILBOX is vsync'ed without blocking, so it is used when
    /// the surface supports it.
    vsync_present_mode: Cell<wgpu::PresentMode>,
    /// What `glfwGetFramebufferSize` returns in C++.
    framebuffer_size: Cell<Size>,
    /// `static auto lastSwapTime` in `swap()`.
    last_swap_time: Cell<Instant>,

    readback_requested: Cell<bool>,
    /// The copy of the frame `request_readback` asked for, until mapped.
    pending_readback: RefCell<Option<PendingReadback>>,
    readback: RefCell<Option<PremultipliedImage>>,
    overlay: RefCell<Option<Overlay>>,
}

/// Port-only: a frame copied into a buffer, waiting to be mapped.
struct PendingReadback {
    buffer: wgpu::Buffer,
    bytes_per_row: u32,
    size: Size,
    mapped: std::sync::mpsc::Receiver<Result<(), wgpu::BufferAsyncError>>,
}

impl SurfaceState {
    fn configuration(&self, size: Size) -> wgpu::SurfaceConfiguration {
        wgpu::SurfaceConfiguration {
            usage: self.surface_usage.get(),
            format: self.swap_chain_format,
            width: size.width,
            height: size.height,
            // Use Immediate mode (no vsync) in benchmark mode, Fifo (vsync) otherwise
            present_mode: if self.enable_vsync { self.vsync_present_mode.get() } else { wgpu::PresentMode::Immediate },
            desired_maximum_frame_latency: 2,
            alpha_mode: self.alpha_mode,
            view_formats: vec![self.swap_chain_format],
            color_space: wgpu::SurfaceColorSpace::Auto,
        }
    }

    /// `processEvents()`: `wgpuDevicePoll(device, false)`.
    fn process_events(&self) {
        let _ = self.device.poll(wgpu::PollType::Poll);
    }

    /// `swap()`: presents the current frame.
    fn swap(&self) {
        let now = Instant::now();
        let time_since_last_swap = now.duration_since(self.last_swap_time.get());

        // If it's been too long since last swap, tick the device to keep it alive
        if time_since_last_swap > Duration::from_millis(1000) {
            self.process_events();
            // Only reconfigure if really needed (> 10 seconds)
            if time_since_last_swap > Duration::from_millis(10000) && self.surface_configured.get() {
                self.surface_needs_reconfigure.set(true);
            }
        }
        self.last_swap_time.set(now);

        // Note: Command buffer is already submitted in CommandEncoder::present()
        // before swap() is called, so we don't need to submit it again here.

        // Present the current frame
        if self.surface_configured.get() {
            // Check if surface needs reconfiguration
            if self.surface_needs_reconfigure.get() {
                self.reconfigure_surface();
            }

            // Only present if we have a valid texture view and haven't presented yet
            let current = if self.frame_presented.get() { None } else { self.current.borrow_mut().take() };
            {
                if let Some(current) = current {
                    if let Some(overlay) = self.overlay.borrow_mut().as_mut() {
                        overlay(&OverlayTarget {
                            device: &self.device,
                            queue: &self.queue,
                            view: &current.view,
                            format: self.swap_chain_format,
                            size: self.last_configured_size.get(),
                        });
                    }
                    if self.readback_requested.get() {
                        self.readback_requested.set(false);
                        *self.pending_readback.borrow_mut() = self.copy_texture(&current.texture.texture);
                        // Natively this waits for the copy; in the browser it
                        // returns at once and the event loop maps it later.
                        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
                        self.finish_readback();
                    }
                    drop(current.view);
                    self.queue.present(current.texture);
                    self.frame_presented.set(true);
                }
            }

            // Reset error counter on successful present
            self.consecutive_errors.set(0);
        }

        // signalFrameComplete(): `frameInProgress` is never set (see the
        // file comment). C++ calls glfwPollEvents here; the GLFW wrapper does.
    }

    /// `getCurrentTextureView()`
    fn get_current_texture_view(&self) -> Option<wgpu::TextureView> {
        // Port: a surface that had no size to configure with yet (or was
        // just set) is configured once it has one.
        if !self.surface_configured.get() && self.surface_needs_reconfigure.get() {
            self.reconfigure_surface();
        }
        if !self.surface_configured.get() {
            return None;
        }

        // Check error threshold
        if self.consecutive_errors.get() >= MAX_CONSECUTIVE_ERRORS {
            // Too many errors, need to reconfigure
            self.surface_needs_reconfigure.set(true);
            self.reconfigure_surface();
            self.consecutive_errors.set(0);
        }

        if let Some(current) = self.current.borrow().as_ref()
            && !self.frame_presented.get()
        {
            return Some(current.view.clone());
        }

        // Always release textures before acquiring new ones to avoid "already acquired" error
        self.current.borrow_mut().take();
        self.frame_presented.set(false);

        // Port: apply a pending resize before acquiring (see the file comment).
        if self.surface_needs_reconfigure.get() {
            self.reconfigure_surface();
        }

        let acquired = self.surface.borrow().as_ref()?.get_current_texture();
        let surface_texture = match acquired {
            wgpu::CurrentSurfaceTexture::Success(texture) | wgpu::CurrentSurfaceTexture::Suboptimal(texture) => texture,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface_needs_reconfigure.set(true);
                self.consecutive_errors.set(self.consecutive_errors.get() + 1);
                return None;
            }
            wgpu::CurrentSurfaceTexture::Timeout
            | wgpu::CurrentSurfaceTexture::Occluded
            | wgpu::CurrentSurfaceTexture::Validation => {
                self.consecutive_errors.set(self.consecutive_errors.get() + 1);
                return None;
            }
        };

        // Create texture view with explicit descriptor
        let view = surface_texture.texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("SwapChain TextureView"),
            format: Some(self.swap_chain_format),
            dimension: Some(wgpu::TextureViewDimension::D2),
            usage: None,
            aspect: wgpu::TextureAspect::All,
            base_mip_level: 0,
            mip_level_count: Some(1),
            base_array_layer: 0,
            array_layer_count: Some(1),
        });

        // Store the texture to keep it alive
        *self.current.borrow_mut() = Some(CurrentTexture { texture: surface_texture, view: view.clone() });

        // Success - reset error counter
        self.consecutive_errors.set(0);
        Some(view)
    }

    /// `getDepthStencilView()`
    fn get_depth_stencil_view(&self) -> Option<wgpu::TextureView> {
        if self.depth_stencil.borrow().is_none() {
            let size = self.framebuffer_size.get();
            self.create_depth_stencil_texture(size.width, size.height);
        }
        self.depth_stencil.borrow().as_ref().map(|(_, view)| view.clone())
    }

    /// `reconfigureSurface()`
    fn reconfigure_surface(&self) {
        // Wait for any in-progress frame: never in progress (file comment).

        // Clear all texture state
        self.current.borrow_mut().take();

        let surface = self.surface.borrow();
        let Some(surface) = surface.as_ref() else {
            return;
        };

        // Get current size
        let size = self.framebuffer_size.get();

        // Skip if size hasn't changed and we're already configured
        if self.surface_configured.get() && self.last_configured_size.get() == size {
            self.surface_needs_reconfigure.set(false);
            return;
        }

        // wgpu (unlike wgpu-native) rejects a zero-sized configuration, which
        // a minimized window reports; keep the old one until it has a size.
        if size.width == 0 || size.height == 0 {
            return;
        }

        surface.configure(&self.device, &self.configuration(size));

        self.create_depth_stencil_texture(size.width, size.height);

        // Update state
        self.surface_configured.set(true);
        self.surface_needs_reconfigure.set(false);
        self.last_configured_size.set(size);
        self.consecutive_errors.set(0);
    }

    /// `createDepthStencilTexture(width, height)`
    fn create_depth_stencil_texture(&self, width: u32, height: u32) {
        self.depth_stencil.borrow_mut().take();

        let Some(format) = self.depth_stencil_format.get() else {
            return;
        };
        if width == 0 || height == 0 {
            return;
        }

        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("DepthStencilTexture"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("DepthStencilTextureView"),
            format: Some(format),
            dimension: Some(wgpu::TextureViewDimension::D2),
            usage: None,
            aspect: wgpu::TextureAspect::All,
            base_mip_level: 0,
            mip_level_count: Some(1),
            base_array_layer: 0,
            array_layer_count: Some(1),
        });
        *self.depth_stencil.borrow_mut() = Some((texture, view));
    }

    /// Port-only: copies the swapchain texture into a buffer and starts
    /// mapping it (`finish_readback` makes the image).
    fn copy_texture(&self, texture: &wgpu::Texture) -> Option<PendingReadback> {
        if !self.surface_usage.get().contains(wgpu::TextureUsages::COPY_SRC) {
            Log::warning(Event::Render, "WebGPU: the surface does not support readback (no COPY_SRC)");
            return None;
        }
        let (width, height) = (texture.width(), texture.height());
        let bytes_per_row =
            (width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Surface readback"),
            size: u64::from(bytes_per_row) * u64::from(height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder =
            self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("Surface readback") });
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        );
        self.queue.submit([encoder.finish()]);
        let (tx, mapped) = std::sync::mpsc::channel();
        buffer.map_async(wgpu::MapMode::Read, .., move |result| {
            let _ = tx.send(result);
        });
        Some(PendingReadback { buffer, bytes_per_row, size: Size::new(width, height), mapped })
    }

    /// Port-only: turns the pending copy into an RGBA image once it is
    /// mapped (drops it if mapping failed).
    fn finish_readback(&self) {
        let mut pending = self.pending_readback.borrow_mut();
        let Some(readback) = pending.take() else {
            return;
        };
        match readback.mapped.try_recv() {
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                *pending = Some(readback);
                return;
            }
            Ok(Ok(())) => {}
            Ok(Err(_)) | Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
        }
        let PendingReadback { buffer, bytes_per_row, size, .. } = readback;
        let (width, height) = (size.width, size.height);
        let bgra =
            matches!(self.swap_chain_format, wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb);
        let mut image = PremultipliedImage::new(size);
        {
            let Ok(mapped) = buffer.get_mapped_range(..) else {
                return;
            };
            for y in 0..height as usize {
                let src = &mapped[y * bytes_per_row as usize..][..width as usize * 4];
                let dst = &mut image.data[y * width as usize * 4..][..width as usize * 4];
                dst.copy_from_slice(src);
                if bgra {
                    for px in dst.chunks_exact_mut(4) {
                        px.swap(0, 2);
                    }
                }
            }
        }
        buffer.unmap();
        *self.readback.borrow_mut() = Some(image);
    }
}

/// `WebGPURenderableResource`: the swapchain as a renderable resource.
struct SurfaceRenderableResource {
    state: Rc<SurfaceState>,
}

impl RenderableResource for SurfaceRenderableResource {
    fn bind(&self) {
        // Nothing to do here for WebGPU as command encoder is managed by
        // CommandEncoder class
    }

    fn swap(&self) {
        // The command buffer submission happens in CommandEncoder::present()
        // Here we just present the surface
        self.state.swap();
    }

    fn get_color_texture_view(&self) -> Option<wgpu::TextureView> {
        self.state.get_current_texture_view()
    }

    fn get_depth_stencil_texture_view(&self) -> Option<wgpu::TextureView> {
        self.state.get_depth_stencil_view()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// What `SurfaceBackend::finish` builds on.
struct Parts {
    instance: wgpu::Instance,
    adapter: Option<wgpu::Adapter>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: Option<wgpu::Surface<'static>>,
}

/// The surface configuration `SurfaceBackend::finish` uses.
struct SurfaceChoice {
    swap_chain_format: wgpu::TextureFormat,
    alpha_mode: wgpu::CompositeAlphaMode,
    surface_usage: wgpu::TextureUsages,
    present_mode: wgpu::PresentMode,
    depth_stencil_format: wgpu::TextureFormat,
}

/// `GLFWWebGPUBackend` without the window: a `webgpu::RendererBackend` and
/// `gfx::Renderable` presenting to a surface.
pub struct SurfaceBackend {
    renderer_backend: RendererBackend,
    renderable: RenderableBase,
    state: Rc<SurfaceState>,
    adapter_info: wgpu::AdapterInfo,
}

impl SurfaceBackend {
    /// The constructor of `GLFWWebGPUBackend(window, capFrameRate)` from the
    /// instance on: adapter, device, surface (made by `create_surface` from
    /// the window's handles), format selection and configuration.
    pub fn new(
        create_surface: impl FnOnce(&wgpu::Instance) -> Result<wgpu::Surface<'static>, String>,
        framebuffer_size: Size,
        cap_frame_rate: bool,
    ) -> Result<SurfaceBackend, String> {
        let renderer_backend = RendererBackend::new(ContextMode::Unique);

        // Create the instance
        let instance = crate::headless_backend::create_instance(instance_backends());

        // Request adapter
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))
        .map_err(|e| format!("No WebGPU adapter found: {e}"))?;

        // Log adapter info
        let adapter_info = adapter.get_info();
        Log::info(
            Event::Render,
            format!(
                "WebGPU: Selected adapter backend = {}, name = {}",
                backend_type_to_string(adapter_info.backend),
                if adapter_info.name.is_empty() { "<unknown>" } else { &adapter_info.name }
            ),
        );

        let has_depth32_stencil = adapter.features().contains(wgpu::Features::DEPTH32FLOAT_STENCIL8);
        let (required_features, depth_stencil_format) = if has_depth32_stencil {
            (wgpu::Features::DEPTH32FLOAT_STENCIL8, wgpu::TextureFormat::Depth32FloatStencil8)
        } else {
            (wgpu::Features::empty(), wgpu::TextureFormat::Depth24PlusStencil8)
        };

        // The WebGPU default limits, lowered to what the adapter supports, as
        // the headless backend requests them.
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("MapLibre WebGPU Device"),
            required_features,
            required_limits: wgpu::Limits::default().or_worse_values_from(&adapter.limits()),
            ..Default::default()
        }))
        .map_err(|e| format!("Failed to create WebGPU device: {e}"))?;

        // Dawn's logUncapturedError instead of wgpu's panicking default.
        device.on_uncaptured_error(std::sync::Arc::new(|error: wgpu::Error| {
            Log::error(Event::Render, format!("WebGPU validation error: {error}"));
        }));

        renderer_backend.set_depth_stencil_format(Some(depth_stencil_format));

        // Process device events to ensure it's ready
        let _ = device.poll(wgpu::PollType::Poll);

        // Create surface for WebGPU
        let surface = create_surface(&instance)?;

        let capabilities = surface.get_capabilities(&adapter);
        let (surface_usage, present_mode) = surface_options(&capabilities);
        let preferred_formats = [
            wgpu::TextureFormat::Bgra8Unorm,
            wgpu::TextureFormat::Bgra8UnormSrgb,
            wgpu::TextureFormat::Rgba8Unorm,
            wgpu::TextureFormat::Rgba8UnormSrgb,
        ];
        let swap_chain_format = preferred_formats
            .into_iter()
            .find(|f| capabilities.formats.contains(f))
            .or_else(|| capabilities.formats.first().copied())
            .unwrap_or(wgpu::TextureFormat::Bgra8Unorm);
        Ok(Self::finish(
            renderer_backend,
            Parts { instance, adapter: Some(adapter), device, queue, surface: Some(surface) },
            // A window shows nothing behind it: wgpu's Auto picks Opaque
            // where the surface supports it.
            SurfaceChoice {
                swap_chain_format,
                alpha_mode: wgpu::CompositeAlphaMode::Auto,
                surface_usage,
                present_mode,
                depth_stencil_format,
            },
            framebuffer_size,
            cap_frame_rate,
            adapter_info,
        ))
    }

    /// Port-only: a backend on a device and surface that the host created,
    /// as maplibre-native-ffi's `mln_map_attach_web_gpu_surface` passes them,
    /// or the browser's WebGPU (wgpu's `webgpu` backend, examples/web).
    /// There is no adapter: the surface is configured with `format`, for
    /// rendering, and for readback too if `usages` (what the surface
    /// supports) has COPY_SRC; it presents in FIFO order, which every
    /// surface supports. With `alpha_mode` `PreMultiplied`, what is behind
    /// the surface (the page behind a browser canvas) shows where the map
    /// draws nothing; the map's colors are premultiplied.
    pub fn from_host(
        instance: wgpu::Instance,
        device: wgpu::Device,
        queue: wgpu::Queue,
        surface: wgpu::Surface<'static>,
        format: wgpu::TextureFormat,
        alpha_mode: wgpu::CompositeAlphaMode,
        usages: wgpu::TextureUsages,
        framebuffer_size: Size,
    ) -> SurfaceBackend {
        let renderer_backend = RendererBackend::new(ContextMode::Unique);
        let adapter_info = device.adapter_info();
        let depth_stencil_format = if device.features().contains(wgpu::Features::DEPTH32FLOAT_STENCIL8) {
            wgpu::TextureFormat::Depth32FloatStencil8
        } else {
            wgpu::TextureFormat::Depth24PlusStencil8
        };
        device.on_uncaptured_error(std::sync::Arc::new(|error: wgpu::Error| {
            Log::error(Event::Render, format!("WebGPU validation error: {error}"));
        }));
        renderer_backend.set_depth_stencil_format(Some(depth_stencil_format));
        Self::finish(
            renderer_backend,
            Parts { instance, adapter: None, device, queue, surface: Some(surface) },
            SurfaceChoice {
                swap_chain_format: format,
                alpha_mode,
                surface_usage: wgpu::TextureUsages::RENDER_ATTACHMENT | (usages & wgpu::TextureUsages::COPY_SRC),
                present_mode: wgpu::PresentMode::Fifo,
                depth_stencil_format,
            },
            framebuffer_size,
            true,
            adapter_info,
        )
    }

    /// The rest of the construction, once the device, surface and formats
    /// are there.
    fn finish(
        renderer_backend: RendererBackend,
        parts: Parts,
        choice: SurfaceChoice,
        framebuffer_size: Size,
        cap_frame_rate: bool,
        adapter_info: wgpu::AdapterInfo,
    ) -> SurfaceBackend {
        let Parts { instance, adapter, device, queue, surface } = parts;
        let SurfaceChoice { swap_chain_format, alpha_mode, surface_usage, present_mode, depth_stencil_format } = choice;
        renderer_backend.set_color_format(Some(swap_chain_format));

        let backend_impl = renderer_backend.backend_impl().clone();
        let state = Rc::new(SurfaceState {
            surface: RefCell::new(surface),
            device: device.clone(),
            queue: queue.clone(),
            swap_chain_format,
            alpha_mode,
            surface_usage: Cell::new(surface_usage),
            depth_stencil_format: Cell::new(Some(depth_stencil_format)),
            current: RefCell::new(None),
            depth_stencil: RefCell::new(None),
            frame_presented: Cell::new(true),
            surface_configured: Cell::new(false),
            surface_needs_reconfigure: Cell::new(false),
            last_configured_size: Cell::new(Size::default()),
            consecutive_errors: Cell::new(0),
            enable_vsync: cap_frame_rate,
            vsync_present_mode: Cell::new(present_mode),
            framebuffer_size: Cell::new(framebuffer_size),
            last_swap_time: Cell::new(Instant::now()),
            readback_requested: Cell::new(false),
            pending_readback: RefCell::new(None),
            readback: RefCell::new(None),
            overlay: RefCell::new(None),
        });

        // Configure surface
        state.surface_needs_reconfigure.set(true);
        state.reconfigure_surface();

        // Store WebGPU instance, device and queue in the base class
        backend_impl.set_instance(Some(instance));
        backend_impl.set_adapter(adapter);
        backend_impl.set_device(Some(device));
        backend_impl.set_queue(Some(queue));

        let resource = RenderableResourceBox::new_rc(SurfaceRenderableResource { state: state.clone() });
        SurfaceBackend {
            renderer_backend,
            renderable: RenderableBase::new(framebuffer_size, Some(resource)),
            state,
            adapter_info,
        }
    }

    pub fn webgpu_backend(&self) -> &RendererBackend {
        &self.renderer_backend
    }

    /// The adapter in use (name, backend, driver).
    pub fn get_adapter_info(&self) -> &wgpu::AdapterInfo {
        &self.adapter_info
    }

    /// The swapchain format picked from the surface capabilities.
    pub fn get_swap_chain_format(&self) -> wgpu::TextureFormat {
        self.state.swap_chain_format
    }

    /// `getFramebufferSize()`
    pub fn get_framebuffer_size(&self) -> Size {
        self.get_size()
    }

    /// `setFramebufferSize(Size)`
    pub fn set_framebuffer_size(&self, new_size: Size) {
        self.state.framebuffer_size.set(new_size);
        // Update swap chain size if needed
        if self.get_size() != new_size {
            self.set_renderable_size(new_size);
            self.state.surface_needs_reconfigure.set(true);
        }
    }

    /// `swap()`
    pub fn swap(&self) {
        self.state.swap();
    }

    /// `processEvents()`
    pub fn process_events(&self) {
        self.state.process_events();
    }

    /// Port-only: acquires the next swapchain texture ahead of a frame;
    /// false when the surface has none right now (timeout, occluded,
    /// outdated). The caller skips the frame: a render pass without a color
    /// attachment trips the drawables' debug assertions.
    pub fn acquire_next_texture(&self) -> bool {
        self.state.get_current_texture_view().is_some()
    }

    /// Port-only: copy the next presented frame (see `take_readback`).
    pub fn request_readback(&self) {
        self.state.readback_requested.set(true);
    }

    /// Port-only: the frame copied after `request_readback`, once presented
    /// and mapped (natively at once; in the browser on a later turn of the
    /// event loop).
    pub fn take_readback(&self) -> Option<PremultipliedImage> {
        self.state.finish_readback();
        self.state.readback.borrow_mut().take()
    }

    /// The device, for tests and tools.
    pub fn get_device(&self) -> wgpu::Device {
        self.state.device.clone()
    }

    /// Port-only: whether the backend has a surface to present to.
    pub fn has_surface(&self) -> bool {
        self.state.surface.borrow().is_some()
    }

    /// Port-only: drops the surface, so the host can destroy what it was
    /// made from, and keeps the device and everything built on it. Until
    /// `set_surface`, `acquire_next_texture` returns false and `swap`
    /// presents nothing.
    pub fn release_surface(&self) {
        let state = &self.state;
        // The swapchain texture goes before the surface it came from.
        state.current.borrow_mut().take();
        state.depth_stencil.borrow_mut().take();
        state.frame_presented.set(true);
        state.surface_configured.set(false);
        state.surface_needs_reconfigure.set(false);
        state.consecutive_errors.set(0);
        state.surface.borrow_mut().take();
    }

    /// Port-only: presents to `surface` from now on, at `framebuffer_size`,
    /// in place of the surface the backend has, if any, which it drops.
    /// The surface is configured with the backend's swapchain format.
    ///
    /// Errors, keeping the current surface, when the backend's adapter
    /// can't present to `surface` in that format: pipelines are built for
    /// it, so a new format needs a new backend. A backend `from_host` has
    /// no adapter to check with, and uses the usages it was made with.
    pub fn set_surface(&self, surface: wgpu::Surface<'static>, framebuffer_size: Size) -> Result<(), String> {
        let state = &self.state;
        if let Some(adapter) = self.renderer_backend.backend_impl().get_adapter() {
            let capabilities = surface.get_capabilities(&adapter);
            if !capabilities.formats.contains(&state.swap_chain_format) {
                return Err(format!(
                    "The surface does not take the {:?} format the backend renders in",
                    state.swap_chain_format
                ));
            }
            let (usage, present_mode) = surface_options(&capabilities);
            state.surface_usage.set(usage);
            state.vsync_present_mode.set(present_mode);
        }
        self.release_surface();
        *state.surface.borrow_mut() = Some(surface);
        state.framebuffer_size.set(framebuffer_size);
        if self.get_size() != framebuffer_size {
            self.set_renderable_size(framebuffer_size);
        }
        state.surface_needs_reconfigure.set(true);
        state.reconfigure_surface();
        Ok(())
    }

    /// Port-only: draws `overlay` over each frame from now on, after the map
    /// and before the frame is presented (and read back), or stops with
    /// `None`.
    pub fn set_overlay(&self, overlay: Option<Overlay>) {
        *self.state.overlay.borrow_mut() = overlay;
    }
}

impl Drop for SurfaceBackend {
    fn drop(&mut self) {
        // ~GLFWWebGPUBackend: context.reset(), release the textures, pump
        // outstanding callbacks; the surface goes before the device.
        self.renderer_backend.reset_context();
        self.state.current.borrow_mut().take();
        self.state.depth_stencil.borrow_mut().take();
        self.state.overlay.borrow_mut().take();
        self.state.process_events();
        self.set_resource(None);
    }
}

impl Renderable for SurfaceBackend {
    fn renderable_base(&self) -> &RenderableBase {
        &self.renderable
    }
}

impl renderer_backend::RendererBackend for SurfaceBackend {
    fn renderer_backend_base(&self) -> &RendererBackendBase {
        &self.renderer_backend.base
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn get_default_renderable(&self) -> &dyn Renderable {
        // We directly inherit from gfx::Renderable
        self
    }

    fn init_shaders(&self, registry: &ShaderRegistry, parameters: &ProgramParameters) {
        self.renderer_backend.init_shaders(registry, parameters);
    }

    fn create_context(&self) -> Rc<dyn GfxContext> {
        self.renderer_backend.create_context()
    }

    fn activate(&self) {
        // WebGPU doesn't need explicit context activation like OpenGL
    }

    fn deactivate(&self) {
        // WebGPU doesn't need explicit context deactivation like OpenGL
    }
}

#[cfg(test)]
mod tests {
    use mln_gfx::backend_scope::{BackendScope, ScopeType};
    use mln_gfx::renderer_backend::RendererBackend as _;

    use super::*;

    /// A backend whose surface is released, on a headless device: the
    /// tests have no window to make a surface from.
    fn released_backend(framebuffer_size: Size) -> SurfaceBackend {
        let (device, queue) = crate::HeadlessBackend::request_device(None).expect("device");
        let adapter_info = device.adapter_info();
        let format = wgpu::TextureFormat::Bgra8Unorm;
        SurfaceBackend::finish(
            RendererBackend::new(ContextMode::Unique),
            Parts {
                instance: crate::headless_backend::create_instance(instance_backends()),
                adapter: None,
                device,
                queue,
                surface: None,
            },
            SurfaceChoice {
                swap_chain_format: format,
                alpha_mode: wgpu::CompositeAlphaMode::Auto,
                surface_usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                present_mode: wgpu::PresentMode::Fifo,
                depth_stencil_format: wgpu::TextureFormat::Depth24PlusStencil8,
            },
            framebuffer_size,
            true,
            adapter_info,
        )
    }

    #[test]
    fn a_backend_without_a_surface_has_no_frame_to_render() {
        let backend = released_backend(Size::new(32, 16));
        assert!(!backend.has_surface());
        assert!(!backend.acquire_next_texture());
        backend.swap();
        assert!(!backend.acquire_next_texture());
        assert!(!backend.state.surface_configured.get());
    }

    #[test]
    fn a_released_backend_takes_resizes_for_its_next_surface() {
        let backend = released_backend(Size::new(32, 16));
        backend.release_surface();
        backend.set_framebuffer_size(Size::new(64, 48));
        assert_eq!(backend.get_framebuffer_size(), Size::new(64, 48));
        assert!(!backend.acquire_next_texture());
        assert!(!backend.has_surface());
    }

    #[test]
    fn a_released_backend_keeps_its_context() {
        let backend = released_backend(Size::new(32, 16));
        let _scope = BackendScope::new(&backend, ScopeType::Explicit);
        let context = backend.get_context_ptr().clone();
        backend.release_surface();
        assert!(Rc::ptr_eq(&context, backend.get_context_ptr()));
        assert!(backend.state.depth_stencil.borrow().is_none());
    }
}
