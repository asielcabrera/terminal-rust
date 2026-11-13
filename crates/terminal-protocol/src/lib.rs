//! Shared between the window (client) and the server process: the wire protocol, the
//! pane byte-source pieces, layout geometry, and configuration structures.

pub mod config;
pub mod layout;
pub mod pane_io;
#[cfg(unix)] // Windows has no API for another process' cwd; pane_io.rs only calls it on unix
pub mod proc_cwd;
