// Ports include/mln/webgpu/drawable_builder.hpp and src/mln/webgpu/drawable_builder.cpp

use std::cell::RefCell;
use std::rc::Rc;

use mln_gfx::draw_mode::DrawMode;
use mln_gfx::drawable::{DrawSegment, Drawable as _, UniqueDrawSegment, UniqueDrawable};
use mln_gfx::drawable_builder::{self, DrawableBuilderBase};
use mln_gfx::gfx_types::AttributeDataType;
use mln_gfx::index_vector::IndexVectorBase;
use mln_gfx::shaders::segment::SegmentBase;

use crate::drawable::Drawable;

pub struct DrawableBuilder {
    base: DrawableBuilderBase,
}

impl DrawableBuilder {
    pub fn new(name: String) -> Self {
        DrawableBuilder { base: DrawableBuilderBase::new(name) }
    }
}

impl drawable_builder::DrawableBuilder for DrawableBuilder {
    fn base(&self) -> &DrawableBuilderBase {
        &self.base
    }

    fn base_mut(&mut self) -> &mut DrawableBuilderBase {
        &mut self.base
    }

    fn create_drawable(&self) -> UniqueDrawable {
        Box::new(Drawable::new(self.base.drawable_name.clone()))
    }

    fn create_segment(&self, mode: DrawMode, segment: SegmentBase) -> UniqueDrawSegment {
        Box::new(DrawSegment::new(mode, segment))
    }

    fn init(&mut self) {
        let base = &mut self.base;
        let Some(current) = base.current_drawable.as_mut() else {
            eprintln!("DrawableBuilder::init called with no current drawable");
            return;
        };
        let drawable = current.as_any_mut().downcast_mut::<Drawable>().expect("webgpu::Drawable");
        let impl_ = &mut base.impl_;

        // Set the vertex attribute ID so the drawable knows which shared vertex buffer to use
        drawable.set_vertex_attr_id(base.vertex_attr_id);

        // Handle special case where rawVerticesCount is set but rawVertices is
        // empty (from fills): the vertex data is in the vertex attributes.
        if impl_.raw_vertices_count != 0 && impl_.raw_vertices.is_empty() {
            if drawable.get_vertex_attributes().is_some() && !impl_.segments.is_empty() {
                // Get the draw mode from the first segment; setIndexData below
                // sets the segments properly.
                let draw_mode = *impl_.segments[0].get_mode();
                let attrs = drawable.get_vertex_attributes().cloned();
                drawable.update_vertex_attributes(
                    attrs,
                    impl_.raw_vertices_count,
                    draw_mode,
                    impl_.shared_indexes.clone(),
                    &[],
                );
            }
        } else if impl_.raw_vertices_count != 0 && !impl_.raw_vertices.is_empty() {
            let raw = std::mem::take(&mut impl_.raw_vertices);
            drawable.set_vertices(raw, impl_.raw_vertices_count, impl_.raw_vertices_type);
        } else if !impl_.vertices.empty() {
            let verts = impl_.vertices.data();
            if !verts.is_empty() {
                let raw: Vec<u8> = bytemuck::cast_slice(&verts).to_vec();
                drawable.set_vertices(raw, verts.len(), AttributeDataType::Short2);
            }
        }

        // Set index data if available
        if impl_.shared_indexes.is_none() && !impl_.build_indexes.is_empty() {
            let indexes = std::mem::take(&mut impl_.build_indexes);
            impl_.shared_indexes = Some(Rc::new(RefCell::new(IndexVectorBase::new(indexes))));
        }
        if let Some(indexes) = impl_.shared_indexes.clone().filter(|i| i.borrow().elements() != 0) {
            let segments = std::mem::take(&mut impl_.segments);
            drawable.set_index_data(Some(indexes), segments);
        } else {
            eprintln!("  No index data to set!");
        }

        // Flush texture bindings so the shared builder doesn't leak the
        // previous tile's Texture2D handles into the next draw call.
        for texture_slot in base.textures.iter_mut() {
            *texture_slot = None;
        }
    }
}
