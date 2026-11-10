//! Working directory of another process, asked to the operating system. It is the fallback for
//! shells that do not report their directory with OSC 7: any shell works without integration.
//! Windows has no public API for it (it would mean reading the other process' memory), so there
//! the directory still depends on the shell reporting it.

use std::path::PathBuf;

/// Current directory of the process `pid`, if the system exposes it
#[cfg(target_os = "linux")]
pub fn process_cwd(pid: i32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// Current directory of the process `pid`, if the system exposes it
#[cfg(target_os = "macos")]
pub fn process_cwd(pid: i32) -> Option<PathBuf> {
    use std::ffi::CStr;
    use std::os::unix::ffi::OsStrExt;

    let mut info = std::mem::MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    // SAFETY: the buffer is a zeroed `proc_vnodepathinfo` and `size` is exactly its length
    let written = unsafe {
        libc::proc_pidinfo(pid, libc::PROC_PIDVNODEPATHINFO, 0, info.as_mut_ptr().cast(), size)
    };
    if written != size {
        return None;
    }
    // SAFETY: proc_pidinfo filled the whole struct (it returned its size)
    let info = unsafe { info.assume_init() };
    // `vip_path` is a NUL-terminated `[c_char; MAXPATHLEN]` split in 32 blocks of 32 bytes
    let raw: &[u8; 1024] = unsafe { &*(&info.pvi_cdir.vip_path as *const _ as *const [u8; 1024]) };
    let path = CStr::from_bytes_until_nul(raw).ok()?.to_bytes();
    (!path.is_empty()).then(|| PathBuf::from(std::ffi::OsStr::from_bytes(path)))
}

/// Current directory of the process `pid`, if the system exposes it
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn process_cwd(_pid: i32) -> Option<PathBuf> {
    None
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
    use std::time::{Duration, Instant};

    #[test]
    fn the_directory_of_this_process_is_found() {
        let expected = std::env::current_dir().unwrap().canonicalize().unwrap();
        let found = process_cwd(std::process::id() as i32).expect("directory of this process");
        assert_eq!(found.canonicalize().unwrap(), expected);
    }

    #[test]
    fn an_unknown_process_has_no_directory() {
        assert_eq!(process_cwd(-1), None);
    }

    #[test]
    fn the_directory_of_the_program_in_a_pty_is_found_without_osc_7() {
        let dir = std::env::temp_dir().join(format!("terminal-proc-cwd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let pair = NativePtySystem::default()
            .openpty(PtySize { rows: 5, cols: 20, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        let mut command = CommandBuilder::new("sleep");
        command.arg("5");
        command.cwd(&dir);
        let mut child = pair.slave.spawn_command(command).unwrap();

        // The child becomes the foreground process group once it has started
        let deadline = Instant::now() + Duration::from_secs(5);
        let found = loop {
            let found = pair.master.process_group_leader().and_then(process_cwd);
            if found.is_some() || Instant::now() > deadline {
                break found;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(found, Some(dir));
    }
}
