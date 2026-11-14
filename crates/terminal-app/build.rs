//! Embeds the application icon (drawn in `src/icon.rs`) in the Windows executable, so Explorer,
//! shortcuts and the taskbar show it. Other systems need nothing at build time.

#[path = "../terminal-client/src/icon.rs"]
#[allow(dead_code)]
mod icon;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../terminal-client/src/icon.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_windows_icon();
    }
}

#[cfg(windows)]
fn embed_windows_icon() {
    let out_dir = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let path = out_dir.join("terminal.ico");
    std::fs::write(&path, icon::ico(&[256, 64, 48, 32, 24, 16])).expect("write terminal.ico");
    let mut resource = winresource::WindowsResource::new();
    resource.set_icon(path.to_str().expect("OUT_DIR is valid UTF-8"));
    resource
        .compile()
        .expect("embed the icon in the executable");
}

/// Building for Windows from another system: there is no resource compiler, so no embedded icon
/// (the window still gets it at runtime)
#[cfg(not(windows))]
fn embed_windows_icon() {}
