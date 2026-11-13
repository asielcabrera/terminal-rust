//! The window side: the winit event loop and renderer (the GPU stack lives here), the pane
//! views, the mouse and keyboard drivers, and the automation CLI helpers. Everything it shares
//! with the server process comes from `terminal-protocol`.

pub mod actions;
pub mod app;
pub mod blink;
pub mod config;
pub mod glyphs;
pub mod icon;
pub mod mouse;
pub mod pane;
pub mod renderer;

// The moved modules kept referring to these as `crate::layout` / `crate::pane_io`; keep those
// paths working now that the modules live here.
pub use terminal_protocol::{layout, pane_io};

// The app module holds the window's types (`App`, `LuaEvent`); the modules that grew out of
// `main.rs` still reach them from the crate root.
pub use app::{App, LuaEvent};
