// Ports include/mln/webgpu/renderable_resource.hpp
//
// `gfx::Renderable` stores its resource as `Rc<dyn gfx::RenderableResource>`.
// The WebGPU resources are wrapped in `RenderableResourceBox`, the one
// concrete type the backend downcasts to (`as_webgpu`), standing in for the
// C++ `getResource<webgpu::RenderableResource>()` static_cast.
//
// `getBackend()`, `getCommandEncoder()` and `getRenderPassEncoder()` return
// dummies in every C++ implementation; they are not ported.

use std::any::Any;
use std::rc::Rc;

use mln_gfx::renderable;

pub trait RenderableResource {
    /// WebGPU binding happens at the command encoder level; a no-op.
    fn bind(&self) {}

    /// Renderable resources that require a swap function to be called
    /// explicitly can override this method.
    fn swap(&self) {}

    /// Records commands at the end of the frame `present` is about to
    /// submit, after its passes, into the same submission.
    fn finish_frame(&self, _encoder: &mut wgpu::CommandEncoder) {}

    /// The color attachment view for render passes targeting this resource,
    /// or `None` if the backend supplies it (swapchain).
    fn get_color_texture_view(&self) -> Option<wgpu::TextureView> {
        None
    }
    fn get_color_texture_format(&self) -> Option<wgpu::TextureFormat> {
        None
    }
    /// The depth/stencil view, if the resource manages one.
    fn get_depth_stencil_texture_view(&self) -> Option<wgpu::TextureView> {
        None
    }
    fn get_depth_stencil_texture_format(&self) -> Option<wgpu::TextureFormat> {
        None
    }

    fn as_any(&self) -> &dyn Any;
}

/// The `gfx::RenderableResource` of every WebGPU renderable.
pub struct RenderableResourceBox(pub Box<dyn RenderableResource>);

impl renderable::RenderableResource for RenderableResourceBox {
    fn bind(&self) {
        self.0.bind();
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl RenderableResourceBox {
    pub fn new_rc(resource: impl RenderableResource + 'static) -> Rc<dyn renderable::RenderableResource> {
        Rc::new(RenderableResourceBox(Box::new(resource)))
    }
}

/// `renderable.getResource<webgpu::RenderableResource>()`
pub fn as_webgpu(resource: &dyn renderable::RenderableResource) -> Option<&dyn RenderableResource> {
    resource.as_any().downcast_ref::<RenderableResourceBox>().map(|b| b.0.as_ref())
}

/// `renderable.getResource<T>()` for a concrete WebGPU resource type.
pub fn downcast_resource<T: 'static>(resource: &dyn renderable::RenderableResource) -> Option<&T> {
    as_webgpu(resource)?.as_any().downcast_ref::<T>()
}
