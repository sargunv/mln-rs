// Ports include/mln/webgpu/command_encoder.hpp and src/mln/webgpu/command_encoder.cpp
//
// Debug groups are no-ops in C++ ("handled at the render pass level"); the
// names are tracked for `visitDebugGroups` only.
//
// wgpu vs. Dawn: a render pass is recorded on the encoder through
// `wgpu::RenderPass::forget_lifetime`, so passes are owned objects; wgpu then
// rejects `finish()` while a pass is still open, as WebGPU requires (the
// renderer drops its pass before `present`).

use std::any::Any;
use std::cell::{RefCell, RefMut};
use std::rc::Rc;

use mln_gfx::command_encoder;
use mln_gfx::debug_group::DebugGroupTarget;
use mln_gfx::render_pass::RenderPassDescriptor;
use mln_gfx::renderable::Renderable;

use crate::context::Context;
use crate::render_pass::RenderPass;
use crate::renderable_resource::as_webgpu;
use crate::upload_pass::UploadPass;

pub struct CommandEncoder {
    context: Rc<Context>,
    encoder: RefCell<Option<wgpu::CommandEncoder>>,
    debug_group_names: RefCell<Vec<String>>,
}

fn new_encoder(context: &Context) -> Option<wgpu::CommandEncoder> {
    let device = context.get_backend().get_device()?;
    Some(device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("MapLibre Command Encoder") }))
}

impl CommandEncoder {
    pub fn new(context: Rc<Context>) -> Self {
        let encoder = new_encoder(&context);
        CommandEncoder { context, encoder: RefCell::new(encoder), debug_group_names: RefCell::new(Vec::new()) }
    }

    pub fn get_context(&self) -> &Rc<Context> {
        &self.context
    }

    /// `getEncoder()`
    pub fn get_encoder(&self) -> RefMut<'_, Option<wgpu::CommandEncoder>> {
        self.encoder.borrow_mut()
    }

    pub fn visit_debug_groups(&self, mut f: impl FnMut(&str)) {
        for name in self.debug_group_names.borrow().iter() {
            f(name);
        }
    }

    /// WebGPU-specific: submit the command buffer directly, then start a new
    /// encoder so subsequent passes in the same frame can continue recording.
    pub fn submit_command_buffer(&self) {
        let backend = self.context.get_backend();
        if backend.get_device().is_none() || backend.get_queue().is_none() {
            return;
        }
        let Some(encoder) = self.encoder.borrow_mut().take() else {
            return;
        };
        // Finish and submit the command buffer, after the buffer writes
        backend.submit(encoder.finish());
        // Create a new command encoder (C++ also updates
        // context.setCurrentCommandEncoder).
        *self.encoder.borrow_mut() = new_encoder(&self.context);
    }
}

impl DebugGroupTarget for CommandEncoder {
    fn push_debug_group(&self, name: &str) {
        // Debug markers are currently handled at the render pass level.
        self.debug_group_names.borrow_mut().push(name.to_owned());
    }
    fn pop_debug_group(&self) {
        self.debug_group_names.borrow_mut().pop();
    }
}

impl command_encoder::CommandEncoder for CommandEncoder {
    fn create_upload_pass(&self, name: &str, renderable: &dyn Renderable) -> Box<dyn mln_gfx::upload_pass::UploadPass> {
        Box::new(UploadPass::new(renderable, self, name))
    }

    fn create_render_pass(
        &self,
        name: &str,
        descriptor: &RenderPassDescriptor<'_>,
    ) -> Box<dyn mln_gfx::render_pass::RenderPass> {
        Box::new(RenderPass::new(self, name, descriptor))
    }

    fn present(&self, renderable: &dyn Renderable) {
        // Submit the command buffer first
        self.submit_command_buffer();
        // Then call swap on the RenderableResource to present the surface.
        // (C++ catches the exception thrown when there is no resource.)
        if let Some(resource) = renderable.get_resource()
            && let Some(resource) = as_webgpu(&*resource)
        {
            resource.swap();
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
