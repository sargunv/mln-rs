//! Ports `platform/glfw`: the GLFW example app (`mln-glfw`) on the WebGPU
//! backend. `main.rs` is the binary (main.cpp); the library holds the view,
//! the renderer frontend and the backends so tests and tools can use them.

pub mod glfw_backend;
pub mod glfw_renderer_frontend;
pub mod glfw_view;
pub mod glfw_webgpu_backend;
pub mod settings_json;
pub mod system_fonts;
