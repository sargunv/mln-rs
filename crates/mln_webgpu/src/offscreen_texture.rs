// Ports include/mln/webgpu/offscreen_texture.hpp and src/mln/webgpu/offscreen_texture.cpp
//
// wgpu vs. Dawn/wgpu-native: the readback maps the staging buffer and blocks
// with `Device::poll(PollType::Wait)` (C++: `wgpuInstanceWaitAny` on Dawn, a
// 1000 x 1 ms `wgpuDevicePoll` loop on wgpu-native).
//
// Port-only: the staging buffer is kept between reads, and after a read the
// next frame's own submission ends with the copy, which C++ submits
// separately on each read.
//
// TODO(port): the size-changed callback (`handleResize` / `setSizeListener`)
// that resizes the textures when someone calls `setSize` on the color
// texture; nothing on the M1 path resizes them.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use mln_gfx::offscreen_texture;
use mln_gfx::renderable::{Renderable, RenderableBase};
use mln_gfx::texture2d::{SamplerState, Texture2D as _, Texture2DPtr};
use mln_gfx::types::{TextureChannelDataType, TextureFilterType, TexturePixelType, TextureWrapType};
use mln_util::image::PremultipliedImage;
use mln_util::size::Size;

use crate::context::Context;
use crate::renderable_resource::{RenderableResource, RenderableResourceBox, downcast_resource};
use crate::texture2d::Texture2D;

const BYTES_PER_ROW_ALIGNMENT: u32 = 256;

fn linear_clamp() -> SamplerState {
    SamplerState {
        filter: TextureFilterType::Linear,
        wrap_u: TextureWrapType::Clamp,
        wrap_v: TextureWrapType::Clamp,
        ..SamplerState::default()
    }
}

pub struct OffscreenTextureResource {
    context: Rc<Context>,
    size: Size,
    type_: TextureChannelDataType,
    color_texture: RefCell<Option<Rc<RefCell<Texture2D>>>>,
    depth_texture: Option<Rc<RefCell<Texture2D>>>,
    /// The buffer the color texture is copied into to be read back, kept
    /// for the next read.
    staging_buffer: RefCell<Option<wgpu::Buffer>>,
    /// The color texture was read back: the next frame copies it out.
    read_back: Cell<bool>,
    /// The submission (`Context::submissions`) whose copy the readback
    /// buffer holds.
    copied_at: Cell<Option<u64>>,
}

impl OffscreenTextureResource {
    pub fn new(context: Rc<Context>, size: Size, type_: TextureChannelDataType, depth: bool, stencil: bool) -> Self {
        debug_assert!(size.width != 0 && size.height != 0);
        let mut res = OffscreenTextureResource {
            context,
            size,
            type_,
            color_texture: RefCell::new(None),
            depth_texture: None,
            staging_buffer: RefCell::default(),
            read_back: Cell::default(),
            copied_at: Cell::default(),
        };
        // Create color texture
        *res.color_texture.borrow_mut() = Some(res.create_color_texture());

        // Create depth/stencil texture if needed. WebGPU requires a combined
        // depth-stencil texture when both are needed.
        if depth || stencil {
            let depth_texture = res.context.create_webgpu_texture_2d();
            {
                let mut t = depth_texture.borrow_mut();
                t.set_size(size);
                // Use combined depth-stencil format when both are needed
                if depth && stencil {
                    t.set_format(TexturePixelType::Stencil, TextureChannelDataType::UnsignedByte);
                } else if depth {
                    t.set_format(TexturePixelType::Depth, TextureChannelDataType::Float);
                } else {
                    t.set_format(TexturePixelType::Stencil, TextureChannelDataType::UnsignedByte);
                }
                t.set_usage(wgpu::TextureUsages::RENDER_ATTACHMENT);
                t.set_sampler_configuration(linear_clamp());
            }
            res.depth_texture = Some(depth_texture);
        }
        res
    }

    fn create_color_texture(&self) -> Rc<RefCell<Texture2D>> {
        let texture = self.context.create_webgpu_texture_2d();
        {
            let mut t = texture.borrow_mut();
            t.set_size(self.size);
            t.set_format(TexturePixelType::RGBA, self.type_);
            t.set_usage(
                wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_DST
                    | wgpu::TextureUsages::COPY_SRC
                    | wgpu::TextureUsages::RENDER_ATTACHMENT,
            );
            t.set_sampler_configuration(linear_clamp());
        }
        texture
    }

    fn color(&self) -> Option<Rc<RefCell<Texture2D>>> {
        self.color_texture.borrow().clone()
    }

    fn row_strides(&self) -> (u32, u32) {
        let row_stride = self.size.width * 4; // RGBA8 output
        (row_stride, row_stride.next_multiple_of(BYTES_PER_ROW_ALIGNMENT))
    }

    /// Records a copy of the color texture into the readback buffer, if
    /// the texture exists.
    fn encode_copy(&self, device: &wgpu::Device, encoder: &mut wgpu::CommandEncoder) -> bool {
        let color = self.color();
        let Some(texture) = color.as_ref().and_then(|c| c.borrow().get_texture().cloned()) else {
            return false;
        };
        let size = self.size;
        let (_, aligned_row_stride) = self.row_strides();
        let mut staging = self.staging_buffer.borrow_mut();
        let staging = staging.get_or_insert_with(|| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("Readback Buffer"),
                size: u64::from(aligned_row_stride) * u64::from(size.height),
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            })
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: staging,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(aligned_row_stride),
                    rows_per_image: Some(size.height),
                },
            },
            wgpu::Extent3d { width: size.width, height: size.height, depth_or_array_layers: 1 },
        );
        true
    }

    /// Ends a frame with the copy a read asked for: a host reading frames
    /// back reads the next one too, and the copy then costs no submission
    /// of its own.
    pub(crate) fn encode_requested_copy(&self, encoder: &mut wgpu::CommandEncoder) {
        let Some(device) = self.context.get_backend().get_device() else {
            return;
        };
        if self.read_back.replace(false) && self.encode_copy(&device, encoder) {
            self.copied_at.set(Some(self.context.submissions()));
        }
    }

    /// The color texture's pixels. The copy is the one the last submission
    /// made (`encode_requested_copy`), or else one submitted now.
    pub fn read_still_image(&self) -> PremultipliedImage {
        let size = self.size;
        let (row_stride, aligned_row_stride) = self.row_strides();
        let mut data = vec![0u8; row_stride as usize * size.height as usize];
        self.read_back.set(true);

        let backend = self.context.get_backend();
        let (Some(device), Some(queue)) = (backend.get_device(), backend.get_queue()) else {
            eprintln!("WebGPU: No device or queue available for readback");
            return PremultipliedImage::from_vec(size, data);
        };

        // Any later submission may have drawn the texture again.
        if self.copied_at.take() != Some(self.context.submissions()) {
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
            if !self.encode_copy(&device, &mut encoder) {
                eprintln!("WebGPU: Color texture is null");
                return PremultipliedImage::from_vec(size, data);
            }
            queue.submit([encoder.finish()]);
        }
        let staging = self.staging_buffer.borrow();
        let Some(staging_buffer) = staging.as_ref() else {
            return PremultipliedImage::from_vec(size, data);
        };

        // Map buffer and read data (blocking)
        let (tx, rx) = std::sync::mpsc::channel();
        staging_buffer.map_async(wgpu::MapMode::Read, .., move |result| {
            let _ = tx.send(result);
        });
        if let Err(e) = device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None }) {
            eprintln!("WebGPU: Buffer mapping wait failed: {e}");
        }
        match rx.try_recv() {
            Ok(Ok(())) => match staging_buffer.get_mapped_range(..) {
                Ok(mapped) => {
                    if aligned_row_stride == row_stride {
                        let n = data.len();
                        data.copy_from_slice(&mapped[..n]);
                    } else {
                        let rs = row_stride as usize;
                        let ars = aligned_row_stride as usize;
                        for row in 0..size.height as usize {
                            data[row * rs..(row + 1) * rs].copy_from_slice(&mapped[row * ars..row * ars + rs]);
                        }
                    }
                }
                Err(e) => eprintln!("WebGPU: Failed to get mapped range: {e}"),
            },
            Ok(Err(e)) => eprintln!("WebGPU: Buffer mapping failed with status: {e}"),
            Err(_) => eprintln!("WebGPU: Buffer mapping timeout after polling"),
        }
        staging_buffer.unmap();
        PremultipliedImage::from_vec(size, data)
    }

    pub fn get_texture(&self) -> Texture2DPtr {
        self.color().expect("offscreen texture without color texture")
    }

    pub fn take_texture(&self) -> Option<Texture2DPtr> {
        let taken: Texture2DPtr = self.color_texture.borrow_mut().take()?;
        self.copied_at.set(None);
        *self.color_texture.borrow_mut() = Some(self.create_color_texture());
        Some(taken)
    }
}

impl Drop for OffscreenTextureResource {
    fn drop(&mut self) {
        self.context.rendering_stats_mut().num_frame_buffers -= 1;
    }
}

impl RenderableResource for OffscreenTextureResource {
    fn bind(&self) {
        // Create textures if not already created
        if let Some(color) = self.color() {
            color.borrow_mut().create();
        }
        if let Some(depth) = &self.depth_texture {
            depth.borrow_mut().create();
        }
    }

    fn swap(&self) {
        // Offscreen targets do not present to a surface.
    }

    fn finish_frame(&self, encoder: &mut wgpu::CommandEncoder) {
        self.encode_requested_copy(encoder);
    }

    fn get_color_texture_view(&self) -> Option<wgpu::TextureView> {
        let color = self.color()?;
        let mut texture = color.borrow_mut();
        texture.create();
        texture.get_texture_view().cloned()
    }

    fn get_color_texture_format(&self) -> Option<wgpu::TextureFormat> {
        self.color()?.borrow().get_native_format()
    }

    fn get_depth_stencil_texture_view(&self) -> Option<wgpu::TextureView> {
        let depth = self.depth_texture.as_ref()?;
        let mut texture = depth.borrow_mut();
        texture.create();
        texture.get_texture_view().cloned()
    }

    fn get_depth_stencil_texture_format(&self) -> Option<wgpu::TextureFormat> {
        self.depth_texture.as_ref()?.borrow().get_native_format()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// `webgpu::OffscreenTexture`
pub struct OffscreenTexture {
    renderable: RenderableBase,
}

impl OffscreenTexture {
    pub fn new(context: Rc<Context>, size: Size, type_: TextureChannelDataType, depth: bool, stencil: bool) -> Self {
        let resource =
            RenderableResourceBox::new_rc(OffscreenTextureResource::new(context, size, type_, depth, stencil));
        OffscreenTexture { renderable: RenderableBase::new(size, Some(resource)) }
    }

    fn with_resource<R>(&self, f: impl FnOnce(&OffscreenTextureResource) -> R) -> Option<R> {
        let resource = self.get_resource()?;
        downcast_resource::<OffscreenTextureResource>(&*resource).map(f)
    }

    pub fn take_texture(&self) -> Option<Texture2DPtr> {
        self.with_resource(|r| r.take_texture()).flatten()
    }
}

impl Renderable for OffscreenTexture {
    fn renderable_base(&self) -> &RenderableBase {
        &self.renderable
    }
}

impl offscreen_texture::OffscreenTexture for OffscreenTexture {
    /// Check if the texture resource is valid and can be rendered to
    fn is_renderable(&self) -> bool {
        self.with_resource(|r| r.color().is_some()).unwrap_or(false)
    }

    fn read_still_image(&self) -> PremultipliedImage {
        self.with_resource(|r| r.read_still_image()).unwrap_or_else(|| PremultipliedImage::new(self.get_size()))
    }

    fn get_texture(&self) -> Texture2DPtr {
        self.with_resource(|r| r.get_texture()).expect("offscreen texture without resource")
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
