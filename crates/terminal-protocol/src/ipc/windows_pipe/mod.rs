//! Named pipes over the `kernel32` bindings: std has no named-pipe API, and the server must
//! stay dependency-free. Only byte mode and blocking waits are used, but every handle is
//! opened for overlapped I/O. A synchronous handle serializes all I/O on its file object,
//! duplicates included, so a reader parked in `ReadFile` would keep `WriteFile` from ever
//! running: neither end could answer the other, and every ask timed out.

use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::sync::Arc;

type Handle = isize;

const INVALID_HANDLE: Handle = -1;
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const OPEN_EXISTING: u32 = 3;
const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
// Byte mode, blocking: 0
const PIPE_MODE_BYTE_WAIT: u32 = 0;
const PIPE_UNLIMITED_INSTANCES: u32 = 255;
const BUFFER_BYTES: u32 = 64 * 1024;
const ERROR_PIPE_CONNECTED: u32 = 535;
const ERROR_PIPE_BUSY: u32 = 231;
const ERROR_IO_PENDING: u32 = 997;
const WAIT_BUSY_MS: u32 = 2_000;

/// The kernel's `OVERLAPPED`: the offset pair stands for its union with a pointer (both are
/// eight bytes), and a pipe ignores it
#[repr(C)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    event: Handle,
}

unsafe extern "system" {
    fn CreateNamedPipeW(
        name: *const u16,
        open_mode: u32,
        pipe_mode: u32,
        instances: u32,
        out_bytes: u32,
        in_bytes: u32,
        default_timeout_ms: u32,
        security: *const core::ffi::c_void,
    ) -> Handle;
    fn ConnectNamedPipe(handle: Handle, overlapped: *mut Overlapped) -> i32;
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security: *const core::ffi::c_void,
        disposition: u32,
        flags: u32,
        template: Handle,
    ) -> Handle;
    fn WaitNamedPipeW(name: *const u16, timeout_ms: u32) -> i32;
    fn ReadFile(
        handle: Handle,
        buffer: *mut u8,
        count: u32,
        read: *mut u32,
        overlapped: *mut Overlapped,
    ) -> i32;
    fn WriteFile(
        handle: Handle,
        buffer: *const u8,
        count: u32,
        written: *mut u32,
        overlapped: *mut Overlapped,
    ) -> i32;
    fn FlushFileBuffers(handle: Handle) -> i32;
    fn CloseHandle(handle: Handle) -> i32;
    fn GetLastError() -> u32;
    fn CreateEventW(
        security: *const core::ffi::c_void,
        manual_reset: i32,
        initial_state: i32,
        name: *const u16,
    ) -> Handle;
    fn GetOverlappedResult(
        handle: Handle,
        overlapped: *mut Overlapped,
        transferred: *mut u32,
        wait: i32,
    ) -> i32;
}

/// A kernel handle closed on drop. Named pipes only serve the machine's own user namespace,
/// mirroring the unix socket's 0600
struct PipeHandle(Handle);

unsafe impl Send for PipeHandle {}
unsafe impl Sync for PipeHandle {}

impl Drop for PipeHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// An event for one thread to wait on while its overlapped calls complete
fn new_event() -> io::Result<PipeHandle> {
    // Manual reset, not signalled; a failure is NULL here, unlike the file handles
    let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if event == 0 {
        return Err(last_error());
    }
    Ok(PipeHandle(event))
}

/// Runs one overlapped call to its end and returns the bytes it moved. The call either
/// completes at once or reports that it is pending; the outcome is read the same way.
fn complete(
    pipe: &PipeHandle,
    event: &PipeHandle,
    start: impl FnOnce(*mut Overlapped) -> i32,
) -> io::Result<u32> {
    let mut overlapped = Overlapped {
        internal: 0,
        internal_high: 0,
        offset: 0,
        offset_high: 0,
        event: event.0,
    };
    if start(&mut overlapped) == 0 && unsafe { GetLastError() } != ERROR_IO_PENDING {
        return Err(last_error());
    }
    let mut moved = 0u32;
    if unsafe { GetOverlappedResult(pipe.0, &mut overlapped, &mut moved, 1) } == 0 {
        return Err(last_error());
    }
    Ok(moved)
}

/// Splits a connected pipe into its reader and writer. They share the handle (the pipe
/// closes with the last of them) and each waits on an event of its own, so a parked read
/// never holds a write back.
fn halves(handle: PipeHandle) -> io::Result<(PipeReader, PipeWriter)> {
    let pipe = Arc::new(handle);
    let reader = PipeReader {
        pipe: pipe.clone(),
        event: new_event()?,
    };
    let writer = PipeWriter {
        pipe,
        event: new_event()?,
    };
    Ok((reader, writer))
}

fn last_error() -> io::Error {
    io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
}

fn wide(text: &str) -> Vec<u16> {
    OsStr::new(text).encode_wide().chain(Some(0)).collect()
}

/// The read half of a pipe
pub(crate) struct PipeReader {
    pipe: Arc<PipeHandle>,
    event: PipeHandle,
}

impl Read for PipeReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
        let target = buffer.as_mut_ptr();
        let read = complete(&self.pipe, &self.event, |overlapped| unsafe {
            ReadFile(self.pipe.0, target, count, std::ptr::null_mut(), overlapped)
        })?;
        Ok(read as usize)
    }
}

/// The write half of a pipe
pub(crate) struct PipeWriter {
    pipe: Arc<PipeHandle>,
    event: PipeHandle,
}

impl Write for PipeWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let count = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
        let source = buffer.as_ptr();
        let written = complete(&self.pipe, &self.event, |overlapped| unsafe {
            WriteFile(self.pipe.0, source, count, std::ptr::null_mut(), overlapped)
        })?;
        Ok(written as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        unsafe {
            FlushFileBuffers(self.pipe.0);
        }
        Ok(())
    }
}

pub(crate) struct PipeListener {
    name: String,
}

impl PipeListener {
    /// Readsies the named pipe. A live server already instances the name: its probe answers
    pub(crate) fn bind(full_name: &str) -> io::Result<Self> {
        if server_is_listening(full_name) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "a terminal server is already running",
            ));
        }
        Ok(Self {
            name: full_name.to_string(),
        })
    }

    /// Creates one pipe instance and waits for a client on it, like a unix accept. A client
    /// that arrives while no instance exists fails with a busy error that the client waits on
    pub(crate) fn accept(&self) -> io::Result<(PipeReader, PipeWriter)> {
        let wide = wide(&self.name);
        let handle = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
                PIPE_MODE_BYTE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                BUFFER_BYTES,
                BUFFER_BYTES,
                0,
                std::ptr::null(),
            )
        };
        if handle == INVALID_HANDLE {
            return Err(last_error());
        }
        let owned = PipeHandle(handle);
        let event = new_event()?;
        match complete(&owned, &event, |overlapped| unsafe {
            ConnectNamedPipe(owned.0, overlapped)
        }) {
            Ok(_) => {}
            // The client connected between the two calls
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_CONNECTED as i32) => {}
            Err(error) => return Err(error),
        }
        halves(owned)
    }
}

/// Whether the name has a server behind it: instances exist even when all of them are busy
fn server_is_listening(full_name: &str) -> bool {
    let wide = wide(full_name);
    let ok = unsafe { WaitNamedPipeW(wide.as_ptr(), 5) };
    ok != 0 || unsafe { GetLastError() } == ERROR_PIPE_BUSY
}

/// Connects as a client, waiting out a busy server like CreateFileW callers do
pub(crate) fn connect_client(full_name: &str) -> io::Result<(PipeReader, PipeWriter)> {
    let wide = wide(full_name);
    let mut waits = 0u32;
    loop {
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                0,
            )
        };
        if handle != INVALID_HANDLE {
            return halves(PipeHandle(handle));
        }
        if unsafe { GetLastError() } == ERROR_PIPE_BUSY {
            waits += 1;
            if waits > 50 {
                return Err(last_error());
            }
            unsafe {
                WaitNamedPipeW(wide.as_ptr(), WAIT_BUSY_MS);
            }
            continue;
        }
        return Err(last_error());
    }
}

#[cfg(test)]
mod tests;
