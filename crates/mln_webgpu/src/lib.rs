//! Ports the C++ WebGPU backend (`src/mln/webgpu`, `include/mln/webgpu`,
//! `include/mln/shaders/webgpu`, `src/mln/shaders/webgpu`, and the headless
//! backend from platform/default) onto the `wgpu` crate.
//!
//! The C++ backend runs on Dawn or wgpu-native through the WebGPU C API; this
//! port makes the same calls through wgpu's Rust API. Raw `WGPU*` handles
//! become wgpu objects (reference counted, so "release" is a drop), and the
//! `Context&` members of resources become `Rc<Context>`.
//!
//! The WGSL comes unchanged from the C++ headers: `//crates/mln_webgpu:wgsl_sources`
//! runs `tools/extract_wgsl.rs` over them at build time and generates
//! `shaders::wgsl_sources`.

pub mod buffer_resource;
pub mod command_encoder;
pub mod context;
pub mod drawable;
pub mod drawable_builder;
pub mod headless_backend;
pub mod layer_group;
pub mod offscreen_texture;
pub mod render_pass;
pub mod renderable_resource;
pub mod renderer_backend;
pub mod shaders;
pub mod surface_backend;
pub mod texture2d;
pub mod tile_layer_group;
pub mod uniform_buffer;
pub mod upload_pass;
pub mod vertex_buffer_resource;
mod writes;

pub use context::Context;
pub use headless_backend::HeadlessBackend;
pub use renderer_backend::RendererBackend;
pub use surface_backend::SurfaceBackend;
