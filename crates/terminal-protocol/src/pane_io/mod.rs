//! Where a pane's bytes come from and go to: the program running in it. A [`LocalPty`], or a
//! server link (`ServerBlock` in-process, `IpcBlock` when the server is its own process): the
//! pane, which owns the libghostty terminal, never notices the difference.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, sync_channel};
use std::thread;

use portable_pty::{ChildKiller, CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};

use crate::config::ShellConfig;

/// What a pane's byte source reports to the event loop
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoEvent {
    /// There is new output to read with [`PaneIo::try_read`]
    Output,
    /// The program exited
    Exited,
}

/// Called from the source's own threads; it must only wake the event loop
pub type Notify = Arc<dyn Fn(IoEvent) + Send + Sync>;

/// The program behind a pane: its input, its output, and its lifetime
pub trait PaneIo {
    /// Writes to the program's input
    fn write(&mut self, bytes: &[u8]);
    /// Next chunk of output, if any. Never blocks.
    fn try_read(&mut self) -> Option<Vec<u8>>;
    /// Re-arms the output notification. Call it BEFORE draining, so a chunk that arrives during
    /// the drain sends another [`IoEvent::Output`].
    fn rearm(&self);
    /// Sends another [`IoEvent::Output`] to keep draining what is left
    fn notify_again(&self);
    /// New size of the grid, in cells
    fn resize(&mut self, cols: u16, rows: u16);
    /// Working directory of the program in the foreground, if the system exposes it
    fn foreground_dir(&self) -> Option<PathBuf>;
    /// Terminates the program
    fn kill(&mut self);
}

/// Chunks that fit in the output channel before the reader thread blocks (backpressure to the PTY,
/// and to ConPTY on Windows, instead of memory growing without limit)
pub const CHANNEL_CAPACITY: usize = 64;

/// A program running in a PTY of this machine
pub struct LocalPty {
    master: Box<dyn MasterPty + Send>,
    /// Detached by [`LocalPty::take_writer`] when a dedicated writer thread owns the write end
    writer: Option<Box<dyn Write + Send>>,
    rx: Receiver<Vec<u8>>,
    /// A chunk taken out by [`LocalPty::output_finished`] to not lose it
    stashed: Option<Vec<u8>>,
    /// The reader thread sends one notification at a time; it is re-armed before draining
    wake_pending: Arc<AtomicBool>,
    /// To terminate the program (the `Child` lives in the thread that waits on it)
    killer: Box<dyn ChildKiller + Send + Sync>,
    notify: Notify,
}

impl LocalPty {
    /// Starts `shell` in a new PTY of `cols`×`rows` and the threads that serve it.
    ///
    /// When the configured program cannot start and it was not `explicit` (a floating pane's own
    /// program), the default shell starts instead and the returned notice explains why; an
    /// explicit program that fails is an error.
    pub fn spawn(
        shell: &ShellConfig,
        explicit: bool,
        cols: u16,
        rows: u16,
        notify: Notify,
    ) -> Result<(LocalPty, Option<String>), String> {
        let pair = NativePtySystem::default()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| format!("could not open the PTY: {error}"))?;

        let mut notice = None;
        let child = match pair.slave.spawn_command(build_command(shell)) {
            Ok(child) => child,
            Err(error) => {
                let Some(program) = &shell.program else {
                    return Err(format!("could not start the default shell: {error}"));
                };
                if explicit {
                    return Err(format!(
                        "could not start {program:?}: {}",
                        spawn_failure_cause(&error)
                    ));
                }
                eprintln!("[shell] could not start {program:?}: {error}");
                // On screen only the cause ("file not found"); the CreateProcessW details go to
                // stderr
                notice = Some(format!(
                    "could not start {program:?}: {}; using the default shell",
                    spawn_failure_cause(&error)
                ));
                let default_shell = ShellConfig {
                    program: None,
                    args: Vec::new(),
                    ..shell.clone()
                };
                pair.slave
                    .spawn_command(build_command(&default_shell))
                    .map_err(|error| format!("could not start the default shell: {error}"))?
            }
        };

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| format!("could not read from the PTY: {error}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| format!("could not write to the PTY: {error}"))?;

        // Reader thread: reads from the PTY and sends the bytes to the channel. The channel is
        // bounded: if the UI falls behind, `send` blocks and the PTY gets backpressure.
        let (tx, rx) = sync_channel::<Vec<u8>>(CHANNEL_CAPACITY);
        let wake_pending = Arc::new(AtomicBool::new(false));
        thread::spawn({
            let notify = notify.clone();
            let wake_pending = wake_pending.clone();
            move || {
                let mut buf = [0u8; 4096];
                loop {
                    match reader.read(&mut buf) {
                        Ok(n) if n > 0 => {
                            if tx.send(buf[..n].to_vec()).is_err() {
                                break; // the pane was closed
                            }
                            // One pending notification at a time: the handler drains the whole
                            // channel
                            if !wake_pending.swap(true, Ordering::AcqRel) {
                                notify(IoEvent::Output);
                            }
                        }
                        _ => break,
                    }
                }
                // One more notification after the reader is done, so the last drain runs after
                // every chunk and can tell the client the output really ended
                notify(IoEvent::Output);
            }
        });

        // Thread that waits on the program. PTY end-of-read is not used: it is unreliable on
        // Windows.
        let killer = child.clone_killer();
        thread::spawn({
            let notify = notify.clone();
            move || {
                let mut child = child;
                let _ = child.wait();
                notify(IoEvent::Exited);
            }
        });

        let pty = LocalPty {
            master: pair.master,
            writer: Some(writer),
            stashed: None,
            rx,
            wake_pending,
            killer,
            notify,
        };
        Ok((pty, notice))
    }
}

impl PaneIo for LocalPty {
    fn write(&mut self, bytes: &[u8]) {
        if let Some(writer) = self.writer.as_mut() {
            let _ = writer.write_all(bytes);
            let _ = writer.flush();
        }
    }

    fn try_read(&mut self) -> Option<Vec<u8>> {
        self.stashed.take().or_else(|| self.rx.try_recv().ok())
    }

    fn rearm(&self) {
        self.wake_pending.store(false, Ordering::Release);
    }

    fn notify_again(&self) {
        self.wake_pending.store(true, Ordering::Release);
        (self.notify)(IoEvent::Output);
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    #[cfg(unix)]
    fn foreground_dir(&self) -> Option<PathBuf> {
        let pid = self.master.process_group_leader()?;
        crate::proc_cwd::process_cwd(pid).filter(|path| path.is_dir())
    }

    #[cfg(not(unix))]
    fn foreground_dir(&self) -> Option<PathBuf> {
        None
    }

    fn kill(&mut self) {
        let _ = self.killer.kill();
    }
}

impl LocalPty {
    /// Detaches the write end for a dedicated writer thread: writing to a full PTY pipe can
    /// block, and the server loop must never block on one block's write. It hands the write end
    /// to one thread per block, which also keeps the write order per block. After this only the
    /// writer thread writes; [`LocalPty::write`] (tests, and a link-less pane) writes nothing.
    pub fn take_writer(&mut self) -> Box<dyn Write + Send> {
        self.writer
            .take()
            .expect("the PTY's write end was already detached")
    }

    /// Whether the output really ended: the reader thread finished (the channel is disconnected)
    /// and nothing is left, so no chunk can arrive anymore. A chunk met on the way is stashed for
    /// the next [`LocalPty::try_read`].
    pub fn output_finished(&mut self) -> bool {
        match self.rx.try_recv() {
            Ok(chunk) => {
                self.stashed = Some(chunk);
                false
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => true,
        }
    }
}

// (moved from pane.rs, which kept the window-side pieces)

fn build_command(shell: &ShellConfig) -> CommandBuilder {
    // `args` panics with the default shell; config.rs validation already guarantees
    // there are only arguments when there is a program.
    let mut command = match &shell.program {
        Some(program) => {
            let mut command = CommandBuilder::new(program);
            command.args(&shell.args);
            command
        }
        None => CommandBuilder::new_default_prog(),
    };
    if let Some(cwd) = &shell.cwd {
        command.cwd(cwd);
    }
    for (key, value) in &shell.env {
        command.env(key, value);
    }
    command
}

/// `portable-pty` returns the `CreateProcessW` failure as plain text ("CreateProcessW `...`
/// in cwd `...` failed: <cause>") with escaped NUL characters. For display this keeps only the
/// cause; if the format ever changes, it returns the full message.
fn spawn_failure_cause(error: &impl std::fmt::Display) -> String {
    let text = error.to_string();
    match text.rsplit_once(" failed: ") {
        Some((_, cause)) => cause.to_string(),
        None => text,
    }
}

/// Writes a red notice into the grid itself, before the shell starts. When launched by
/// double-click there is no console to read stderr, so the error shows up where the user is.

#[cfg(test)]
mod tests;
