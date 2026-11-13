//! In-process terminal server. It owns the programs behind the panes ("blocks") and the
//! authoritative layout, on its own thread: each block is a [`LocalPty`] plus a server-side
//! libghostty terminal fed with exactly the bytes the program writes, so the screen can be
//! exported as a [snapshot](ServerBlock::snapshot) for a client that attaches later. The window
//! talks to it only through messages, by way of a [`ServerHandle`]: layout mutations arrive as
//! ops ([`SplitOp`], [`LayoutClose`], ...), the server applies them, spawns and kills the
//! blocks, and reports one [`LayoutEvent::State`] per applied mutation so the window can rebuild
//! its read-only projection.
//!
//! The server can also be its own process (`terminal server`), reached over local IPC: the wire
//! section carries the same ops and events as JSON then, over a socket several windows share —
//! the most recently attached one drives, the others watch — and a session thread plays the
//! window's role server-side (see the "server process" section at the bottom of this file). The
//! in-process link stays for tests.
//!
//! The server-side terminal never answers the program's queries: the client's terminal does,
//! through [`PaneIo::write`], exactly as with a local PTY.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError, channel, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

use libghostty_vt::fmt::{Format, Formatter, FormatterOptions};
use libghostty_vt::{Terminal, TerminalOptions};

use terminal_protocol::config::ShellConfig;
use terminal_protocol::layout::{Closed, Layout, LayoutState, PaneId, Rect};
#[cfg(test)]
use terminal_protocol::layout::{Dir, Edge, Limits};
use terminal_protocol::pane_io::{CHANNEL_CAPACITY, IoEvent, LocalPty, Notify, PaneIo};

pub use terminal_protocol::ipc::DEFAULT_GATEWAY_PORT;
#[cfg(windows)]
use terminal_protocol::ipc::windows_pipe;

use terminal_protocol::ipc::{
    AddFloatOp, AttachOutcome, AttachedPaneInfo, BlockId, Client, ConnWrite, Created, CreatedPane,
    IpcEndpoint, IpcListener, IpcStream, LayoutEvent, Request, ServerBlock, SplitOp, WireFrame,
    control_frame, data_frame, read_typed_frame,
};
#[cfg(test)]
use terminal_protocol::ipc::{AttachAnswer, ControlDown, IpcBlock, IpcConn};
use terminal_protocol::ipc::{FRAME_CONTROL, FRAME_DATA, FRAME_SNAPSHOT};
use terminal_protocol::wire::ws;
use terminal_protocol::wire::{
    self, WIRE_VERSION, WireEvent, WireEventKind, WireOp, WireReply, WireReplyKind, WireRequest,
};

/// Chunks forwarded per drain request; past that the block queues another request so the other
/// blocks' messages interleave
const MAX_CHUNKS_PER_DRAIN: usize = 16;

/// How long the exit notice waits for the reader thread to close the output channel before it
/// reports the exit anyway (ConPTY can keep the pipe open forever after the child dies)
const EXIT_GRACE: Duration = Duration::from_millis(250);

/// A program and its server-side screen
struct Block {
    pty: LocalPty,
    terminal: Terminal<'static, 'static>,
    /// Bounded: when the client falls behind, the block stops draining its PTY, which then gets
    /// backpressure
    output: SyncSender<Vec<u8>>,
    /// The chunk that did not fit in `output`; it goes first when the client reads
    pending: Option<Vec<u8>>,
    /// The PTY's write end, owned by this block's dedicated writer thread: writes queue in the
    /// channel and are applied one by one, never blocking the server loop (one thread per block,
    /// so the write order per block is preserved)
    writer: Sender<Vec<u8>>,
    /// When the program's exit was noticed; the exit notice waits until every chunk was forwarded
    exit_requested: Option<Instant>,
    client: Client,
}

impl Block {
    /// Whether the block's program is still running: an exit was noticed, or the PTY reader
    /// closed. Windows keeps the ConPTY pipe open after the program dies (the bounded
    /// `EXIT_GRACE` caveat), so this is exact on unix and grace-bounded on Windows
    fn alive(&mut self) -> bool {
        self.exit_requested.is_none() && !self.pty.output_finished()
    }
}

/// The server thread's state: the blocks, and the authoritative layout they belong to
pub struct Server {
    blocks: HashMap<BlockId, Block>,
    /// Which pane of the layout each block belongs to
    pane_blocks: HashMap<PaneId, BlockId>,
    /// The grid size each live pane was last spawned or resized at, for an attach's live pane
    /// list. The server cannot derive a pane's grid from the layout: the window owns the pixels.
    pane_grids: HashMap<PaneId, (u16, u16)>,
    next_id: u64,
    /// Pane ids are allocated here and never reused, not even after a rolled-back spawn
    next_pane_id: u32,
    layout: Option<Layout>,
    /// To queue messages to itself (from the PTYs' threads, and to split long drains)
    requests: Sender<Request>,
    /// Layout events for the window (state after every applied mutation, pane output and exits)
    events: Sender<LayoutEvent>,
    /// Set by `Shutdown`: the run loop keeps serving until the current request is handled
    shutdown: bool,
}

impl Server {
    /// Starts the server thread. It stops, killing whatever is left, when every handle is
    /// dropped. Returns the channel the window reads [`LayoutEvent`]s from.
    pub fn start() -> (ServerHandle, Receiver<LayoutEvent>) {
        let (requests, inbox) = channel();
        let (events, outgoing) = channel();
        let own = requests.clone();
        // The terminals are not `Send`: the state is built on the thread that owns it
        thread::Builder::new()
            .name("terminal-server".to_string())
            .spawn(move || {
                let server = Server {
                    blocks: HashMap::new(),
                    pane_blocks: HashMap::new(),
                    pane_grids: HashMap::new(),
                    next_id: 0,
                    next_pane_id: 0,
                    layout: None,
                    requests: own,
                    events,
                    shutdown: false,
                };
                server.run(inbox);
            })
            .expect("could not start the terminal server thread");
        let handle = ServerHandle {
            link: Arc::new(ServerLink::InProcess(Connection { requests })),
        };
        (handle, outgoing)
    }

    fn run(mut self, inbox: Receiver<Request>) {
        while let Ok(request) = inbox.recv() {
            // A panicking request handler must not take the whole server down: the panic is
            // contained here and the thread keeps serving (advisory A4 of the review). The
            // reply channels die with the request, which every ask path already treats as
            // "the request did not go through".
            let outcome =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.handle(request)));
            if outcome.is_err() {
                eprintln!("[server] a request panicked; the server keeps serving");
            }
            if self.shutdown {
                break;
            }
        }
        for block in self.blocks.values_mut() {
            block.pty.kill();
        }
    }

    /// The live panes for the automation list: every pane of the layout that has a block, with
    /// its block token, grid and alive state, sorted by pane
    fn automation_panes(&mut self) -> Vec<wire::AutomationPane> {
        let mut panes: Vec<_> = Vec::with_capacity(self.pane_blocks.len());
        for (pane, block) in &self.pane_blocks {
            let Some((cols, rows)) = self.pane_grids.get(pane) else {
                continue;
            };
            let Some(running) = self.blocks.get_mut(block) else {
                continue;
            };
            panes.push(wire::AutomationPane {
                pane: *pane,
                block: *block,
                cols: *cols,
                rows: *rows,
                alive: running.alive(),
            });
        }
        panes.sort_unstable_by_key(|pane| pane.pane);
        panes
    }

    /// Handles one request: the single-transaction body of the run loop, wrapped in
    /// a panic guard (see `run`). `Shutdown` is remembered in `shutdown`.
    fn handle(&mut self, request: Request) {
        match request {
            Request::Init {
                shell,
                cols,
                rows,
                scrollback,
                reply,
            } => {
                let _ = reply.send(self.init_layout(&shell, cols, rows, scrollback));
            }
            Request::Attach {
                shell,
                cols,
                rows,
                scrollback,
                reply,
            } => {
                // A fresh start behaves like init; an existing layout is handed over whole
                let outcome = match self.layout.as_ref() {
                    Some(layout) => AttachOutcome::Existing(layout.state(), self.live_panes()),
                    None => AttachOutcome::Fresh(self.init_layout(&shell, cols, rows, scrollback)),
                };
                let _ = reply.send(outcome);
            }
            Request::LayoutSplit(op) => self.split_pane(op),
            Request::LayoutClose { pane } => self.close_pane(pane),
            Request::LayoutSetFocus { pane } => self.apply(|layout| layout.set_focus(pane)),
            Request::LayoutFocusDir(op) => self.apply(|layout| layout.focus_dir(op.dir, op.area)),
            Request::LayoutFocusCycle { forward } => {
                self.apply(|layout| layout.focus_cycle(forward));
            }
            Request::LayoutResize(op) => {
                self.apply(|layout| layout.resize(op.pane, op.dir, op.delta, op.area, op.limits))
            }
            Request::LayoutSetDivider(op) => {
                self.apply(|layout| layout.set_divider(&op.path, op.boundary, op.area, op.limits))
            }
            Request::LayoutToggleZoom => self.apply(Layout::toggle_zoom),
            Request::LayoutAddFloat(op) => self.add_float(op),
            Request::LayoutToggleFloats => self.apply(Layout::toggle_floats),
            Request::LayoutMoveFloat(op) => {
                self.apply(|layout| layout.move_float(op.pane, op.dx, op.dy, op.window))
            }
            Request::LayoutResizeFloatEdge(op) => self.apply(|layout| {
                layout.resize_float_edge(op.pane, op.edge, op.dx, op.dy, op.window, op.limits)
            }),
            Request::Create {
                shell,
                explicit,
                cols,
                rows,
                scrollback,
                client,
                reply,
            } => {
                let created = self.create(&shell, explicit, cols, rows, scrollback, client);
                let _ = reply.send(created);
            }
            Request::Write(id, bytes) => {
                if let Some(block) = self.blocks.get_mut(&id) {
                    // The block's own writer thread does the PTY write: it can block on a
                    // full pipe, and no block may stall the loop or the others (and one
                    // thread per block keeps the write order)
                    let _ = block.writer.send(bytes);
                }
            }
            Request::Resize(id, cols, rows) => {
                if let Some(block) = self.blocks.get_mut(&id) {
                    block.pty.resize(cols, rows);
                    // The server has no pixels: 1×1 cells
                    let _ = block.terminal.resize(cols, rows, 1, 1);
                    // Remember the size for a later attach's live pane list
                    if let Some(pane) = self
                        .pane_blocks
                        .iter()
                        .find(|(_, block)| **block == id)
                        .map(|(pane, _)| *pane)
                    {
                        self.pane_grids.insert(pane, (cols, rows));
                    }
                }
            }
            Request::Kill(id) => {
                if let Some(block) = self.blocks.get_mut(&id) {
                    block.pty.kill();
                }
            }
            Request::Close(id) => {
                if let Some(mut block) = self.blocks.remove(&id) {
                    block.pty.kill();
                    self.pane_blocks.retain(|_, &mut block_id| block_id != id);
                }
            }
            Request::ForegroundDir(id, reply) => {
                let dir = self
                    .blocks
                    .get(&id)
                    .and_then(|block| block.pty.foreground_dir());
                let _ = reply.send(dir);
            }
            Request::Snapshot(id, reply) => {
                let bytes = self
                    .blocks
                    .get(&id)
                    .map(|block| snapshot(&block.terminal))
                    .unwrap_or_default();
                let _ = reply.send(bytes);
            }
            // Automation (stage 6): served directly on the blocks for any session, deliberately
            // bypassing the S4 driver gating — an automation CLI is an explicit local act (see
            // the S6 design). A send goes straight to the block's writer thread, exactly like
            // the driver's Write
            Request::AutomationList { reply } => {
                let _ = reply.send(self.automation_panes());
            }
            Request::AutomationSend(id, bytes, reply) => {
                let outcome = match self.blocks.get_mut(&id) {
                    None => Err(format!("no block {id:?} on this server")),
                    Some(block) => {
                        if !block.alive() {
                            Err("the pane's program has exited".to_string())
                        } else {
                            // The block's own writer thread does the PTY write: it can block
                            // on a full pipe, and no block may stall the loop or the others
                            let _ = block.writer.send(bytes);
                            Ok(())
                        }
                    }
                };
                let _ = reply.send(outcome);
            }
            Request::AutomationCapture(id, reply) => {
                let text = self
                    .blocks
                    .get(&id)
                    .map(|block| plain_screen(&block.terminal))
                    .ok_or_else(|| format!("no block {id:?} on this server"));
                let _ = reply.send(text);
            }
            Request::AutomationSubscribe(id, reply) => {
                let snapshot = self.blocks.get(&id).map(|block| snapshot(&block.terminal));
                let _ = reply.send(snapshot);
            }
            Request::Drain(id) => {
                let finished = self.drain(id);
                self.maybe_exit(id, finished);
            }
            Request::Exited(id) => {
                // The last thing the program wrote goes before the exit: the notice waits
                // until every chunk was forwarded
                if let Some(block) = self.blocks.get_mut(&id)
                    && block.exit_requested.is_none()
                {
                    block.exit_requested = Some(Instant::now());
                } // The reader usually closes right after the child dies, which fires the exit
                // on the next drain. ConPTY can keep the pipe open forever, so wait a bounded
                // grace for it while the block is drained
                let grace = Instant::now() + EXIT_GRACE;
                loop {
                    let finished = self.drain(id);
                    if self.maybe_exit(id, finished) || !finished {
                        break; // reported, or paused: a later drain (a client read) re-checks
                    }
                    if Instant::now() >= grace {
                        break;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            }
            Request::Shutdown => self.shutdown = true,
            Request::Panic => panic!("the server handled a deliberate panic"),
        }
    }

    fn create(
        &mut self,
        shell: &ShellConfig,
        explicit: bool,
        cols: u16,
        rows: u16,
        scrollback: usize,
        client: Client,
    ) -> Created {
        let id = BlockId(self.next_id);
        self.next_id += 1;
        // The PTY's threads only queue a message; the server thread does the work
        let requests = self.requests.clone();
        let notify: Notify = Arc::new(move |event| {
            let _ = requests.send(match event {
                IoEvent::Output => Request::Drain(id),
                IoEvent::Exited => Request::Exited(id),
            });
        });
        let (mut pty, notice) = LocalPty::spawn(shell, explicit, cols, rows, notify)?;
        let terminal = match Terminal::new(TerminalOptions {
            cols,
            rows,
            max_scrollback: scrollback,
        }) {
            Ok(terminal) => terminal,
            Err(error) => {
                pty.kill();
                return Err(format!("libghostty: {error}"));
            }
        };
        let (output, receiver) = sync_channel(CHANNEL_CAPACITY);
        let (writer, writer_inbox) = channel::<Vec<u8>>();
        let mut pty_writer = pty.take_writer();
        thread::Builder::new()
            .name(format!("terminal-block-writer-{id:?}"))
            .spawn(move || {
                // Writes are applied one by one; a write that fails (the program is gone, or the
                // PTY was killed) is dropped: the block's kill path is already on its way
                while let Ok(bytes) = writer_inbox.recv() {
                    let _ = pty_writer.write_all(&bytes);
                    let _ = pty_writer.flush();
                }
            })
            .expect("could not start a block writer thread");
        self.blocks.insert(
            id,
            Block {
                pty,
                terminal,
                output,
                pending: None,
                writer,
                exit_requested: None,
                client,
            },
        );
        Ok((id, receiver, notice))
    }

    /// Creates the first pane and the authoritative layout. Called once, before the window's
    /// event loop runs; the window builds its projection and its pane from the answer.
    fn init_layout(
        &mut self,
        shell: &ShellConfig,
        cols: u16,
        rows: u16,
        scrollback: usize,
    ) -> Result<(LayoutState, CreatedPane), String> {
        if self.layout.is_some() {
            return Err("the layout already exists".to_string());
        }
        let pane = PaneId(self.next_pane_id);
        self.layout = Some(Layout::new(pane));
        self.next_pane_id += 1;
        match self.spawn_pane_block(pane, shell, false, cols, rows, scrollback) {
            Ok((block, notice)) => {
                self.pane_grids.insert(pane, (cols, rows));
                let state = self.layout.as_ref().unwrap().state();
                Ok((
                    state,
                    CreatedPane {
                        pane,
                        block,
                        cols,
                        rows,
                        notice,
                    },
                ))
            }
            Err(error) => {
                self.layout = None;
                // The id is burned, like every rolled-back spawn: the field doc promises ids
                // are never reused (advisory A-T2b-2)
                Err(error)
            }
        }
    }

    /// The build fact every live pane carries, for an attach (only panes with a block: a pane
    /// whose block failed to spawn was rolled back and is not live). Pane order, so the wire
    /// list is stable.
    fn live_panes(&self) -> Vec<AttachedPaneInfo> {
        let mut panes: Vec<AttachedPaneInfo> = self
            .pane_grids
            .iter()
            .filter_map(|(pane, (cols, rows))| {
                self.pane_blocks.get(pane).map(|block| AttachedPaneInfo {
                    pane: *pane,
                    block: *block,
                    cols: *cols,
                    rows: *rows,
                })
            })
            .collect();
        panes.sort_unstable_by_key(|pane| pane.pane);
        panes
    }

    /// Starts `shell` in a new block for `pane`. The block's output and exit reach the window
    /// as [`LayoutEvent::PaneOutput`] / [`LayoutEvent::PaneExited`] for that pane.
    fn spawn_pane_block(
        &mut self,
        pane: PaneId,
        shell: &ShellConfig,
        explicit: bool,
        cols: u16,
        rows: u16,
        scrollback: usize,
    ) -> Result<(ServerBlock, Option<String>), String> {
        let events = self.events.clone();
        let notify: Notify = Arc::new(move |event| {
            let _ = events.send(match event {
                IoEvent::Output => LayoutEvent::PaneOutput(pane),
                IoEvent::Exited => LayoutEvent::PaneExited(pane),
            });
        });
        let wake_pending = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let client = Client {
            notify: notify.clone(),
            wake_pending: wake_pending.clone(),
            paused: paused.clone(),
        };
        let (id, output, notice) = self.create(shell, explicit, cols, rows, scrollback, client)?;
        self.pane_blocks.insert(pane, id);
        let block = ServerBlock {
            requests: self.requests.clone(),
            id,
            output,
            wake_pending,
            paused,
            notify,
        };
        Ok((block, notice))
    }

    /// Applies a layout mutation and, when it changed something, reports one state event: the
    /// window rebuilds its projection from it
    fn apply(&mut self, mutate: impl FnOnce(&mut Layout) -> bool) {
        let Some(layout) = self.layout.as_mut() else {
            return;
        };
        if mutate(layout) {
            let state = layout.state();
            self.broadcast(state, None);
        }
    }

    /// "Split `op.pane` in two and start the new pane's block". The pane id is allocated here;
    /// a block that fails to start rolls the split back and reports only the error.
    fn split_pane(&mut self, op: SplitOp) {
        let Some(layout) = self.layout.as_mut() else {
            return;
        };
        if !layout.split(
            op.pane,
            op.dir.axis(),
            PaneId(self.next_pane_id),
            op.area,
            op.limits,
        ) {
            return; // refused: nothing changed, and the window previews the same check
        }
        self.next_pane_id += 1;
        let pane = PaneId(self.next_pane_id - 1);
        let state = layout.state();
        match self.spawn_pane_block(
            pane,
            &op.shell,
            op.explicit,
            op.cols,
            op.rows,
            op.scrollback,
        ) {
            Ok((block, notice)) => {
                self.pane_grids.insert(pane, (op.cols, op.rows));
                self.broadcast(
                    state,
                    Some(CreatedPane {
                        pane,
                        block,
                        cols: op.cols,
                        rows: op.rows,
                        notice,
                    }),
                );
            }
            Err(error) => self.rollback_spawn(pane, error),
        }
    }

    /// "Open a floating pane and start its block". Same allocation and rollback rules as a
    /// split.
    fn add_float(&mut self, op: AddFloatOp) {
        let Some(layout) = self.layout.as_mut() else {
            return;
        };
        let pane = PaneId(self.next_pane_id);
        layout.add_float(pane, op.width, op.height);
        self.next_pane_id += 1;
        let state = layout.state();
        match self.spawn_pane_block(
            pane,
            &op.shell,
            op.explicit,
            op.cols,
            op.rows,
            op.scrollback,
        ) {
            Ok((block, notice)) => {
                self.pane_grids.insert(pane, (op.cols, op.rows));
                self.broadcast(
                    state,
                    Some(CreatedPane {
                        pane,
                        block,
                        cols: op.cols,
                        rows: op.rows,
                        notice,
                    }),
                );
            }
            Err(error) => self.rollback_spawn(pane, error),
        }
    }

    /// A pane whose block failed to start leaves the layout exactly as it was: the window sees
    /// only the error (the same fallback notice the local spawn path would print)
    fn rollback_spawn(&mut self, pane: PaneId, error: String) {
        if let Some(layout) = self.layout.as_mut() {
            layout.close(pane);
        }
        let _ = self.events.send(LayoutEvent::Error(error));
    }

    /// "Close this pane and kill its block". Closing the last tiled pane kills everything and
    /// tells the window to quit.
    fn close_pane(&mut self, pane: PaneId) {
        let Some(layout) = self.layout.as_mut() else {
            return;
        };
        match layout.close(pane) {
            Closed::LastTiled => {
                for block in self.blocks.values_mut() {
                    block.pty.kill();
                }
                self.blocks.clear();
                self.pane_blocks.clear();
                self.pane_grids.clear();
                self.layout = None;
                // A session that attaches after this gets a fresh server: the old one is gone
                let _ = self.events.send(LayoutEvent::LastPaneClosed);
            }
            Closed::Pane => {
                if let Some(id) = self.pane_blocks.remove(&pane) {
                    self.pane_grids.remove(&pane);
                    if let Some(mut block) = self.blocks.remove(&id) {
                        block.pty.kill();
                    }
                }
                let state = layout.state();
                self.broadcast(state, None);
            }
            Closed::Unknown => {}
        }
    }

    /// One state event per applied mutation
    fn broadcast(&self, state: LayoutState, created: Option<CreatedPane>) {
        let _ = self.events.send(LayoutEvent::State(state, created));
    }

    /// Moves a block's output from its PTY to its client, feeding the server-side terminal on the
    /// way. Never blocks: if the client's channel is full, the block pauses (its PTY gets the
    /// backpressure) until the client reads again.
    /// Forwards what is pending. Answers whether the block has no more output to forward right
    /// now: nothing paused, and no per-drain cap hit.
    fn drain(&mut self, id: BlockId) -> bool {
        let Some(block) = self.blocks.get_mut(&id) else {
            return true;
        };
        // Re-armed BEFORE reading, as the pane does: output arriving meanwhile sends another
        // drain request
        block.pty.rearm();
        let mut forwarded = 0;
        let mut finished = true;
        loop {
            let chunk = match block.pending.take() {
                Some(chunk) => chunk,
                None => match block.pty.try_read() {
                    Some(chunk) => {
                        block.terminal.vt_write(&chunk);
                        chunk
                    }
                    None => break,
                },
            };
            if !block.forward(chunk) {
                finished = false; // the client has not read: the block is paused
                break;
            }
            forwarded += 1;
            if forwarded == MAX_CHUNKS_PER_DRAIN {
                let _ = self.requests.send(Request::Drain(id));
                finished = false; // more is waiting behind the cap
                break;
            }
        }
        if forwarded > 0 && !block.client.wake_pending.swap(true, Ordering::AcqRel) {
            (block.client.notify)(IoEvent::Output);
        }
        finished
    }

    /// Reports the exit to the client, but only once every chunk was forwarded: `exit_requested`
    /// is set by the PTY's exit notice and `finished` says this drain emptied the block. The
    /// reader thread only closes the output channel after its last chunk, but ConPTY can keep the
    /// channel open forever after the child dies, so the notice also fires after a grace period.
    fn maybe_exit(&mut self, id: BlockId, finished: bool) -> bool {
        let Some(block) = self.blocks.get_mut(&id) else {
            return false;
        };
        let Some(requested) = block.exit_requested else {
            return false;
        };
        if !finished {
            return false;
        }
        if block.pty.output_finished() || requested.elapsed() >= EXIT_GRACE {
            block.exit_requested = None;
            (block.client.notify)(IoEvent::Exited);
            return true;
        }
        false
    }
}

impl Block {
    /// Sends a chunk to the client. If its channel is full, keeps the chunk and pauses; returns
    /// whether the chunk went out.
    ///
    /// The pause/resume handshake is lock-free: the server sets `paused` before a second
    /// `try_send`, and the client clears it on its next read and then asks for a drain. It only
    /// works because std's bounded `sync_channel` uses SeqCst ordering on its internal state: a
    /// client read that happens after the flag is set sees it and resumes, and one that happened
    /// before left room for the second `try_send` to succeed.
    fn forward(&mut self, chunk: Vec<u8>) -> bool {
        let chunk = match self.output.try_send(chunk) {
            Ok(()) => return true,
            Err(TrySendError::Disconnected(_)) => return false, // the block is being closed
            Err(TrySendError::Full(chunk)) => chunk,
        };
        // Paused first, then a second try: a client read that happens after the flag is set
        // sees it and asks for a drain; one that happened before left room for this try
        self.client.paused.store(true, Ordering::SeqCst);
        match self.output.try_send(chunk) {
            Ok(()) => {
                self.client.paused.store(false, Ordering::SeqCst);
                true
            }
            Err(TrySendError::Full(chunk)) => {
                self.pending = Some(chunk);
                false
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }
}

/// The screen as plain text, for the automation capture: the same plain formatter the tests
/// read, lossy-decoded to a String
fn plain_screen(terminal: &Terminal<'static, 'static>) -> String {
    let options = FormatterOptions::new().with_format(Format::Plain);
    match Formatter::new(terminal, options).and_then(|mut f| f.format_alloc(None)) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) => {
            eprintln!("[server] could not export the screen as text: {error}");
            String::new()
        }
    }
}

/// The screen as VT sequences that rebuild it in a fresh terminal: content with scrollback,
/// styles, modes, scrolling region, directory, keyboard modes, tabstops, hyperlinks and charsets.
/// The palette is left out (the client's theme applies). An explicit cursor position goes last,
/// because the tabstop sequences move the cursor after the formatter's own.
fn snapshot(terminal: &Terminal<'static, 'static>) -> Vec<u8> {
    let options = FormatterOptions::new()
        .with_format(Format::Vt)
        .with_cursor(true)
        .with_style(true)
        .with_modes(true)
        .with_scrolling_region(true)
        .with_pwd(true)
        .with_keyboard(true)
        .with_tabstops(true)
        .with_hyperlink(true)
        .with_charsets(true)
        .with_kitty_keyboard(true)
        .with_palette(false);
    let mut bytes = Formatter::new(terminal, options)
        .and_then(|mut formatter| formatter.format_alloc(None).map(|bytes| bytes.to_vec()))
        .unwrap_or_else(|error| {
            eprintln!("[server] could not export the screen: {error}");
            Vec::new()
        });
    if let (Ok(x), Ok(y)) = (terminal.cursor_x(), terminal.cursor_y()) {
        // With origin mode (DECOM) on, CUP is relative to the scrolling region's origin (and
        // enabling DECOM homes the cursor), so the move must be written in region coordinates.
        if terminal
            .mode(libghostty_vt::terminal::Mode::ORIGIN)
            .unwrap_or(false)
        {
            let (top, left) = exported_scrolling_region(&bytes);
            bytes.extend_from_slice(
                format!(
                    "\x1b[{};{}H",
                    (y + 2).saturating_sub(top),
                    (x + 2).saturating_sub(left)
                )
                .as_bytes(),
            );
        } else {
            bytes.extend_from_slice(format!("\x1b[{};{}H", y + 1, x + 1).as_bytes());
        }
    }
    bytes
}

/// The scrolling region as the snapshot exports it: the top margin from DECSTBM (`CSI top;bottom r`)
/// and the left margin from DECSLRM (`CSI left;right s`), 1-based, or (1, 1) when unset
fn exported_scrolling_region(bytes: &[u8]) -> (u16, u16) {
    let mut region = (1u16, 1u16);
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\x1b' && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            let start = i + 2;
            let mut j = start;
            while j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j] == b';') {
                j += 1;
            }
            if j > start && j < bytes.len() && (bytes[j] == b'r' || bytes[j] == b's') {
                let params: Vec<u16> = String::from_utf8_lossy(&bytes[start..j])
                    .split(';')
                    .filter(|part| !part.is_empty())
                    .filter_map(|part| part.parse().ok())
                    .collect();
                if bytes[j] == b'r' && (params.len() == 2 || params.len() == 4) {
                    region.0 = params[0].max(1);
                }
                if bytes[j] == b's' && params.len() == 2 {
                    region.1 = params[0].max(1);
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    region
}

/// Sends `Shutdown` when the last holder of the in-process link goes away
struct Connection {
    requests: Sender<Request>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.requests.send(Request::Shutdown);
    }
}

/// How a handle reaches its server: in-process channels today. With the server process
/// (stage 3) an IPC connection joins as another arm, and the same handles keep working: every
/// fire-and-forget request crosses the link through [`ServerLink::send`], and every
/// request-with-reply through the one boundary method for its reply kind.
enum ServerLink {
    /// The server thread of this very process
    InProcess(Connection),
}

impl ServerLink {
    /// The one boundary fire-and-forget requests cross. Errors are ignored: the server may be
    /// gone, and then there is nothing to report to anyway.
    fn send(&self, request: Request) {
        match self {
            ServerLink::InProcess(connection) => {
                let _ = connection.requests.send(request);
            }
        }
    }

    /// The boundary for `Init`: creates the first pane and the authoritative layout. `None`
    /// when the server is gone; the caller words that error.
    fn ask_init(
        &self,
        shell: &ShellConfig,
        cols: u16,
        rows: u16,
        scrollback: usize,
    ) -> Option<Result<(LayoutState, CreatedPane), String>> {
        match self {
            ServerLink::InProcess(connection) => {
                let (reply, answer) = channel();
                connection
                    .requests
                    .send(Request::Init {
                        shell: shell.clone(),
                        cols,
                        rows,
                        scrollback,
                        reply,
                    })
                    .ok()?;
                answer.recv().ok()
            }
        }
    }

    /// The boundary for `Create` (tests only): starts a block outside the layout
    #[cfg(test)]
    fn ask_create(
        &self,
        client: Client,
        shell: &ShellConfig,
        explicit: bool,
        cols: u16,
        rows: u16,
        scrollback: usize,
    ) -> Option<Created> {
        match self {
            ServerLink::InProcess(connection) => {
                let (reply, answer) = channel();
                connection
                    .requests
                    .send(Request::Create {
                        shell: shell.clone(),
                        explicit,
                        cols,
                        rows,
                        scrollback,
                        client,
                        reply,
                    })
                    .ok()?;
                answer.recv().ok()
            }
        }
    }
}

/// Cheap, cloneable access to the server thread, over its [`ServerLink`]
#[derive(Clone)]
pub struct ServerHandle {
    link: Arc<ServerLink>,
}

impl ServerHandle {
    /// Queues a message for the server thread; errors are ignored (the server may be gone)
    pub(crate) fn send(&self, request: Request) {
        self.link.send(request);
    }

    /// Creates the first pane and the authoritative layout. Called once, before the event loop
    /// runs; the window builds its projection and its pane from the answer.
    pub fn init(
        &self,
        shell: &ShellConfig,
        cols: u16,
        rows: u16,
        scrollback: usize,
    ) -> Result<(LayoutState, CreatedPane), String> {
        self.link
            .ask_init(shell, cols, rows, scrollback)
            .ok_or_else(|| "the terminal server stopped".to_string())?
    }

    /// Starts `shell` in a new block of `cols`×`rows`, with the same fallback and errors as
    /// [`LocalPty::spawn`]. `notify` wakes the client when there is output or the program exits.
    /// Outside the layout: a utility for tests (layout ops spawn blocks through the server) and
    /// for attaching clients in stage 3.
    #[cfg(test)]
    pub fn create_block(
        &self,
        shell: &ShellConfig,
        explicit: bool,
        cols: u16,
        rows: u16,
        scrollback: usize,
        notify: Notify,
    ) -> Result<(ServerBlock, Option<String>), String> {
        let wake_pending = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let client = Client {
            notify: notify.clone(),
            wake_pending: wake_pending.clone(),
            paused: paused.clone(),
        };
        let (id, output, notice) = self
            .link
            .ask_create(client, shell, explicit, cols, rows, scrollback)
            .ok_or_else(|| "the terminal server stopped".to_string())??;
        let block = ServerBlock {
            requests: self.link_channel(),
            id,
            output,
            wake_pending,
            paused,
            notify,
        };
        Ok((block, notice))
    }

    /// The raw channel behind the link, for a block that sends its own requests
    #[cfg(test)]
    fn link_channel(&self) -> Sender<Request> {
        match &*self.link {
            ServerLink::InProcess(connection) => connection.requests.clone(),
        }
    }
}

// ---- The server process -------------------------------------------------------------------
//
// The server can be its own process (`terminal server`), reached over local IPC. The single local
// connection carries both channels of a session, told apart by a one-byte frame type per frame —
// the simpler of the two shapes this stage could take: one accept loop, one session table and one
// socket to keep alive, at the cost of a byte per frame.
//
// * `0` — control: the [`wire`] format, length-prefixed JSON (`wire::frame`), carrying requests,
//   replies and events;
// * `1` — data: `[length][u32 block token][bytes]`, the live output of a block;
// * `2` — data snapshot: the same shape, a block's screen as it was when its pane attached; the
//   server queues these before the block's live chunks, so the client replays the screen first.
//
// The client sends only control frames; the server answers with all three.
//
// Several windows share the server at once: every connection becomes a session, and the most
// recently attached one is the *driver* (input writer + layout mutator + resize authority). A
// session (below) plays the window's role on the server side: it receives the events the window
// would have received, and its block data is fanned out by the event router, which owns the
// block views for the whole process — the blocks and the layout outlive every window, so a
// window that closed can re-attach to the same shells.

/// What a session's coordinator is told. The session no longer holds block views (the router
/// owns them for the whole process and fans the output out to every session); it translates
/// requests and forwards events.
enum SessionMsg {
    /// A request read off the socket
    Wire(WireRequest),
    /// A wire event the window would have received (the router precomputes it), including
    /// driver changes sent directly by another session's attach or teardown
    Event(WireEvent),
    /// The writer flushed the data queue below the high-water mark: panes the router stopped
    /// draining for this session may go on
    Resume,
    /// The client closed the connection (or sent something unusable)
    Stop,
}

/// One registered session: how its events and data reach it, and whether it drives
struct SessionEntry {
    msgs: Sender<SessionMsg>,
    writer: SessionWriter,
    /// The driver mutates; viewers only read (their mutating ops are dropped server-side)
    driver: bool,
    /// Set once the session attached. Before that, its attach snapshots must not interleave
    /// with live data, so the router holds the session's data back
    receiving: bool,
}

/// State one server process shares across sessions: the registry the router fans events and
/// block output out through, the block views it owns (they outlive every session), the pane
/// exits pending in the next attach's reply, and the driver's window area.
pub(crate) struct ProcessShared {
    /// Live sessions, by connection id
    sessions: Mutex<HashMap<u64, SessionEntry>>,
    /// The next connection id; it is also the attach order (promotion picks the smallest)
    next_conn: AtomicU64,
    /// Block views owned by the event router for the whole process: blocks and views outlive
    /// every session (a pane keeps running headless with its output channel connected), so a
    /// session teardown only unregisters
    views: Mutex<HashMap<PaneId, ServerBlock>>,
    /// Panes whose block exited since the last attach delivered the list. Every exit lands
    /// here until an attach carries it (cleared once delivered, and on a fresh server), so a
    /// pane that died while detached — or whose exit another session observed — is never
    /// silently frozen in the next client
    exited: Mutex<Vec<PaneId>>,
    /// The driver's window area, from its layout ops. Dropped when the driver changes, until
    /// the new driver sends its own. Written when the driver's layout ops arrive and on every
    /// driver change; stage 5 reads it for the server-side resize authority.
    driver_area: Mutex<Option<Rect>>,
}

impl ProcessShared {
    /// Registers a session at connection time: it receives nothing until it attaches
    fn register(&self, conn: u64, msgs: Sender<SessionMsg>, writer: SessionWriter) {
        self.sessions.lock().unwrap().insert(
            conn,
            SessionEntry {
                msgs,
                writer,
                driver: false,
                receiving: false,
            },
        );
    }
}

impl Default for ProcessShared {
    fn default() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            next_conn: AtomicU64::new(0),
            views: Mutex::new(HashMap::new()),
            exited: Mutex::new(Vec::new()),
            driver_area: Mutex::new(None),
        }
    }
}

/// Frames of live data queued for the socket before the coordinator stops draining more (the
/// blocks pause server-side, which is the backpressure that ends up on the PTY)
const HIGH_WATER_FRAMES: usize = 256;

/// The one writer of a session's socket. Control frames jump ahead of queued data; the writer
/// flushes what is queued, and when the router stalled on this session's high-water mark it
/// asks (once per stall) to resume the panes' output.
#[derive(Clone)]
struct SessionWriter {
    shared: Arc<SessionWriterShared>,
}

struct SessionWriterShared {
    queues: Mutex<SessionQueues>,
    available: Condvar,
}

#[derive(Default)]
struct SessionQueues {
    control: VecDeque<Vec<u8>>,
    data: VecDeque<Vec<u8>>,
    /// The coordinator stopped at the high-water mark
    stalled: bool,
    /// One resume in flight at a time
    resume_wanted: bool,
    stopping: bool,
}

impl SessionWriter {
    fn spawn(write: Box<dyn Write + Send>, msgs: Sender<SessionMsg>) -> Self {
        let shared = Arc::new(SessionWriterShared {
            queues: Mutex::new(SessionQueues::default()),
            available: Condvar::new(),
        });
        let queues = shared.clone();
        thread::Builder::new()
            .name("terminal-session-writer".to_string())
            .spawn(move || {
                let shared = queues;
                let mut write = write;
                let mut queues = shared.queues.lock().unwrap();
                loop {
                    if queues.control.is_empty() && queues.data.is_empty() {
                        if queues.stopping {
                            return;
                        }
                        // Even with nothing to flush, a stall that happened after the last
                        // flush must not strand the panes (the stall flag is rechecked here)
                        if queues.stalled && !queues.resume_wanted {
                            queues.resume_wanted = true;
                            let _ = msgs.send(SessionMsg::Resume);
                        }
                        let (waited, _) = shared
                            .available
                            .wait_timeout(queues, Duration::from_millis(500))
                            .unwrap();
                        queues = waited;
                        continue;
                    }
                    let controls: Vec<Vec<u8>> = queues.control.drain(..).collect();
                    let datas: Vec<Vec<u8>> = queues.data.drain(..).collect();
                    let resume = queues.stalled && !queues.resume_wanted;
                    if resume {
                        queues.resume_wanted = true;
                    }
                    drop(queues);
                    for frame in controls.iter().chain(datas.iter()) {
                        if write.write_all(frame).is_err() {
                            return; // the client is gone
                        }
                    }
                    let _ = write.flush();
                    if resume {
                        let _ = msgs.send(SessionMsg::Resume);
                    }
                    queues = shared.queues.lock().unwrap();
                }
            })
            .expect("could not start the session writer thread");
        Self { shared }
    }

    fn push_control(&self, frame: Vec<u8>) {
        let mut queues = self.shared.queues.lock().unwrap();
        queues.control.push_back(frame);
        self.shared.available.notify_one();
    }

    /// Whether the data queue reached the high-water mark: the router stops draining the
    /// blocks until this session's writer caught up (it asks for a drain with `Resume`)
    fn saturated(&self) -> bool {
        let queues = self.shared.queues.lock().unwrap();
        queues.data.len() >= HIGH_WATER_FRAMES
    }

    /// Whether the frame went out, or the high-water mark stopped the drain
    fn push_data(&self, frame: Vec<u8>) -> bool {
        let mut queues = self.shared.queues.lock().unwrap();
        if queues.data.len() >= HIGH_WATER_FRAMES {
            queues.stalled = true;
            return false;
        }
        queues.data.push_back(frame);
        self.shared.available.notify_one();
        true
    }

    /// Marks the resume in flight as answered
    fn clear_resume(&self) {
        let mut queues = self.shared.queues.lock().unwrap();
        queues.resume_wanted = false;
    }

    fn stop(&self) {
        let mut queues = self.shared.queues.lock().unwrap();
        queues.stopping = true;
        self.shared.available.notify_one();
    }
}

/// The window's counterpart on the server side of one session: requests in (gated by the
/// driver role), events and pane output out. It holds no block views: the router owns them
/// for the whole process and fans the output out to every session.
struct Session {
    handle: ServerHandle,
    shared: Arc<ProcessShared>,
    /// This session's registry id (also its attach order)
    conn: u64,
    writer: SessionWriter,
    /// Whether this session drives: mirrored from the registry, updated by its own attach and
    /// by the `DriverChanged` events an attach or a teardown sends
    driver: bool,
}

/// How long a request-with-reply may take. An attach can spawn a shell; the rest are quick
const SESSION_ASK_TIMEOUT: Duration = Duration::from_secs(30);

impl Session {
    /// Runs until the client disconnects. Nothing here can strand the blocks: the views are
    /// the router's, so a panicking session only loses its own connection.
    fn pump(&mut self, inbox: Receiver<SessionMsg>) {
        while let Ok(msg) = inbox.recv() {
            match msg {
                SessionMsg::Wire(request) => {
                    if !self.on_wire(request) {
                        return;
                    }
                }
                SessionMsg::Event(event) => self.on_event(event),
                SessionMsg::Resume => self.resume(),
                SessionMsg::Stop => return,
            }
        }
    }

    /// The session ended: it unregisters, and if it was the driver, the oldest remaining
    /// session is promoted (told with a `DriverChanged` event); the demoted driver's window
    /// area is dropped with it. The views are the router's and outlive every session, so
    /// there is nothing to hand back.
    fn teardown(self) {
        self.writer.stop();
        let Self {
            shared,
            conn,
            driver,
            ..
        } = self;
        let mut sessions = shared.sessions.lock().unwrap();
        sessions.remove(&conn);
        if driver {
            *shared.driver_area.lock().unwrap() = None;
            if let Some(&oldest) = sessions.keys().min()
                && let Some(entry) = sessions.get_mut(&oldest)
            {
                entry.driver = true;
                let _ = entry.msgs.send(SessionMsg::Event(WireEvent {
                    v: WIRE_VERSION,
                    event: WireEventKind::DriverChanged { is_driver: true },
                }));
            }
        }
    }

    fn on_wire(&mut self, request: WireRequest) -> bool {
        if request.v != WIRE_VERSION {
            eprintln!(
                "[server] a session spoke protocol version {}; dropping it",
                request.v
            );
            self.writer.push_control(control_frame(&WireReply::new(
                request.rid.unwrap_or(0),
                WireReplyKind::InitFailed {
                    error: "protocol version mismatch".to_string(),
                },
            )));
            return false;
        }
        let rid = request.rid.unwrap_or(0);
        // Fire-and-forget automation sends (the attach loop's keystrokes) answer nothing
        let wants_reply = request.rid.is_some();
        match request.op {
            // A fresh start behaves like init, so the same path serves both ops
            WireOp::Init {
                shell,
                cols,
                rows,
                scrollback,
            }
            | WireOp::Attach {
                shell,
                cols,
                rows,
                scrollback,
            } => self.attach(rid, shell, cols, rows, scrollback),
            WireOp::ForegroundDir { block } => {
                let (reply, answer) = channel();
                self.handle.send(Request::ForegroundDir(block, reply));
                let dir = match answer.recv_timeout(SESSION_ASK_TIMEOUT) {
                    Ok(dir) => dir,
                    Err(_) => {
                        eprintln!("[server] a session request did not answer");
                        return false;
                    }
                };
                self.writer
                    .push_control(control_frame(&WireReply::foreground_dir(rid, dir)));
                true
            }
            WireOp::Snapshot { block } => {
                let (reply, answer) = channel();
                self.handle.send(Request::Snapshot(block, reply));
                let bytes = match answer.recv_timeout(SESSION_ASK_TIMEOUT) {
                    Ok(bytes) => bytes,
                    Err(_) => {
                        eprintln!("[server] a session request did not answer");
                        return false;
                    }
                };
                self.writer
                    .push_control(control_frame(&WireReply::snapshot(rid, bytes)));
                true
            }
            // Automation (stage 6): served for ANY connection, deliberately bypassing the
            // driver gate — an automation CLI is an explicit local act (socket access is local
            // trust), and gating its writes behind the window driver would make automation
            // useless next to a window (S6 design, locked 2026-11-12)
            WireOp::AutomationList {} => {
                let (reply, answer) = channel();
                self.handle.send(Request::AutomationList { reply });
                let panes = match answer.recv_timeout(SESSION_ASK_TIMEOUT) {
                    Ok(panes) => panes,
                    Err(_) => {
                        eprintln!("[server] a session request did not answer");
                        return false;
                    }
                };
                self.writer
                    .push_control(control_frame(&WireReply::automation_list(rid, panes)));
                true
            }
            WireOp::Send { block, bytes } => {
                let (reply, answer) = channel();
                self.handle
                    .send(Request::AutomationSend(block, bytes, reply));
                let outcome = match answer.recv_timeout(SESSION_ASK_TIMEOUT) {
                    Ok(outcome) => outcome,
                    Err(_) => {
                        eprintln!("[server] a session request did not answer");
                        return false;
                    }
                };
                if wants_reply {
                    let answer = match outcome {
                        Ok(()) => WireReply::automation_sent(rid),
                        Err(error) => WireReply::automation_failed(rid, error),
                    };
                    self.writer.push_control(control_frame(&answer));
                }
                true
            }
            WireOp::Capture { block } => {
                let (answer_tx, answer) = channel();
                self.handle
                    .send(Request::AutomationCapture(block, answer_tx));
                let text = match answer.recv_timeout(SESSION_ASK_TIMEOUT) {
                    Ok(text) => text,
                    Err(_) => {
                        eprintln!("[server] a session request did not answer");
                        return false;
                    }
                };
                let reply = match text {
                    Ok(text) => WireReply::automation_capture(rid, text),
                    Err(error) => WireReply::automation_failed(rid, error),
                };
                self.writer.push_control(control_frame(&reply));
                true
            }
            WireOp::AutomationSubscribe { block } => {
                let (answer_tx, answer) = channel();
                self.handle
                    .send(Request::AutomationSubscribe(block, answer_tx));
                let queued = match answer.recv_timeout(SESSION_ASK_TIMEOUT) {
                    Ok(queued) => queued,
                    Err(_) => {
                        eprintln!("[server] a session request did not answer");
                        return false;
                    }
                };
                match queued {
                    None => self
                        .writer
                        .push_control(control_frame(&WireReply::automation_failed(
                            rid,
                            format!("no block {block:?} on this server"),
                        ))),
                    Some(bytes) => {
                        // The snapshot is queued before live bytes start flowing: whoever reads
                        // the stream sees the full screen first, exactly like an attach. The
                        // reply arrives ahead of it (controls overtake queued data), so the
                        // client claims the feed before it reads anything
                        self.writer
                            .push_data(data_frame(FRAME_SNAPSHOT, block, bytes));
                        self.start_receiving();
                        self.writer
                            .push_control(control_frame(&WireReply::automation_subscribed(rid)));
                    }
                }
                true
            }
            op => {
                // Everything else mutates: writes, PTY sizing and the layout. Only the driver's
                // ops reach the server; a viewer's are dropped without an error — it keeps
                // receiving the driver's changes as events and renders its own scaled
                // projection from them.
                if !self.driver {
                    return true;
                }
                match op {
                    WireOp::Split { area, .. }
                    | WireOp::Resize { area, .. }
                    | WireOp::FocusDir { area, .. }
                    | WireOp::SetDivider { area, .. } => {
                        *self.shared.driver_area.lock().unwrap() = Some(area);
                    }
                    WireOp::MoveFloat { window, .. } | WireOp::ResizeFloatEdge { window, .. } => {
                        *self.shared.driver_area.lock().unwrap() = Some(window);
                    }
                    _ => {}
                }
                if let Some(request) = op.into_request() {
                    self.handle.send(request);
                }
                true
            }
        }
    }

    /// The attach flow: a fresh server runs init ("fresh behaves like init"); an existing layout
    /// hands its state and live panes over whole. Snapshots for the live panes are queued on the
    /// data channel before the reply, so the client replays their screens before any live chunk.
    /// The most recently attached session becomes the driver: any previous one is demoted with a
    /// `DriverChanged` event and keeps working as a viewer. The reply also names the panes whose
    /// program exited while unobserved; the client marks them dead, and the list is cleared once
    /// delivered.
    fn attach(
        &mut self,
        rid: u64,
        shell: wire::WireShell,
        cols: u16,
        rows: u16,
        scrollback: usize,
    ) -> bool {
        let (reply, answer) = channel();
        self.handle.send(Request::Attach {
            shell: shell.into_config(),
            cols,
            rows,
            scrollback,
            reply,
        });
        let Ok(outcome) = answer.recv_timeout(SESSION_ASK_TIMEOUT) else {
            eprintln!("[server] an attach request did not answer");
            return false;
        };
        let outcome = match outcome {
            AttachOutcome::Fresh(Ok((state, created))) => {
                // The new block joins the router's views, and this session starts receiving
                // data at once: a fresh pane has no snapshot, so even its earliest output must
                // reach the client (which shelves it until the pane is built)
                let pane = wire::IpcCreatedPane::from_created(&created);
                self.shared
                    .views
                    .lock()
                    .unwrap()
                    .insert(created.pane, created.block);
                self.start_receiving();
                self.take_over_driver();
                pump_pane(&self.shared, created.pane);
                WireReply::new(
                    rid,
                    WireReplyKind::Init {
                        state,
                        created: Some(pane),
                    },
                )
            }
            AttachOutcome::Fresh(Err(error)) => {
                WireReply::new(rid, WireReplyKind::InitFailed { error })
            }
            AttachOutcome::Existing(state, panes) => {
                // The panes whose program exited while unobserved (or while another session
                // held them) travel once with this reply
                let exited = std::mem::take(&mut *self.shared.exited.lock().unwrap());
                // The snapshots are queued while this session still receives no live data, so
                // they cannot interleave with the chunks that follow them (the chunks that
                // predate the snapshots are dropped, exactly as a one-session attach did)
                for info in &panes {
                    let (reply, answer) = channel();
                    self.handle.send(Request::Snapshot(info.block, reply));
                    let bytes = answer.recv_timeout(SESSION_ASK_TIMEOUT).unwrap_or_default();
                    self.writer
                        .push_data(data_frame(FRAME_SNAPSHOT, info.block, bytes));
                }
                self.start_receiving();
                self.take_over_driver();
                WireReply::new(
                    rid,
                    WireReplyKind::Attached {
                        state,
                        panes: panes
                            .into_iter()
                            .map(wire::IpcCreatedPane::from_attached)
                            .collect(),
                        exited,
                    },
                )
            }
        };
        self.writer.push_control(control_frame(&outcome));
        true
    }

    /// This session's data feed opens: whatever the router fans out from here on is queued for
    /// its socket
    fn start_receiving(&self) {
        if let Some(entry) = self.shared.sessions.lock().unwrap().get_mut(&self.conn) {
            entry.receiving = true;
        }
    }

    /// The most recently attached session takes the driver role: any previous driver is
    /// demoted (told with a `DriverChanged` event), and the window area it had stored is
    /// dropped until this driver sends its own
    fn take_over_driver(&mut self) {
        let demoted = WireEvent {
            v: WIRE_VERSION,
            event: WireEventKind::DriverChanged { is_driver: false },
        };
        let mut sessions = self.shared.sessions.lock().unwrap();
        for entry in sessions.values_mut() {
            if entry.driver {
                entry.driver = false;
                let _ = entry.msgs.send(SessionMsg::Event(demoted.clone()));
            }
        }
        if let Some(entry) = sessions.get_mut(&self.conn) {
            entry.driver = true;
        }
        drop(sessions);
        *self.shared.driver_area.lock().unwrap() = None;
        self.driver = true;
    }

    /// Forwards the event to the client, following the driver flag when the event reports a
    /// driver change
    fn on_event(&mut self, event: WireEvent) {
        if let WireEventKind::DriverChanged { is_driver } = &event.event {
            self.driver = *is_driver;
        }
        // The data fan-out is the router's job now: even a PaneOutput event is only the wake
        // the client's UI loop needs (its block feed wakes it directly on data too)
        self.writer.push_control(control_frame(&event));
    }

    /// This session's writer emptied its queue below the high-water mark: the panes the router
    /// stopped draining for everyone may go on. Waking them routes new PaneOutput events
    /// through the router, which drains for every session at once.
    fn resume(&mut self) {
        self.writer.clear_resume();
        let mut views = self.shared.views.lock().unwrap();
        for view in views.values_mut() {
            view.notify_again();
        }
    }
}

/// Reads the server's layout events for the whole process. It fans every event out to the
/// registry's sessions, owns the block views for the whole process — a creation parks its
/// block there, and this router is the one reader of every block's output channel, so the
/// fan-out to all sessions happens here, at event time — and records pane exits for the next
/// attach's `exited` list.
fn route_events(events: Receiver<LayoutEvent>, shared: Arc<ProcessShared>) {
    while let Ok(event) = events.recv() {
        let wire_event =
            wire::WireEvent::from_layout_event(&event).expect("every layout event travels");
        match event {
            LayoutEvent::State(state, created) => {
                // A creation parks its block with the router: the block's output channel has
                // one reader, and this thread is it for the whole process
                let mut new_pane = None;
                if let Some(created) = created {
                    shared
                        .views
                        .lock()
                        .unwrap()
                        .insert(created.pane, created.block);
                    new_pane = Some(created.pane);
                }
                // Panes that left lose their views (the server closed their blocks)
                let live = state.pane_ids();
                shared
                    .views
                    .lock()
                    .unwrap()
                    .retain(|pane, _| live.contains(pane));
                // A creation's block may already hold output: its PaneOutput event can precede
                // the state event that announced it
                if let Some(pane) = new_pane {
                    pump_pane(&shared, pane);
                }
            }
            LayoutEvent::LastPaneClosed => {
                shared.views.lock().unwrap().clear();
                // A fresh server comes next: no old pane's exit is pending
                shared.exited.lock().unwrap().clear();
            }
            LayoutEvent::PaneOutput(pane) => pump_pane(&shared, pane),
            LayoutEvent::PaneExited(pane) => {
                // Recorded until an attach delivers the list: sessions that were live got the
                // event too, and a later one marks the pane dead on attach
                shared.exited.lock().unwrap().push(pane);
            }
            LayoutEvent::Error(_) => {}
        }
        let sessions = shared.sessions.lock().unwrap();
        for entry in sessions.values() {
            let _ = entry.msgs.send(SessionMsg::Event(wire_event.clone()));
        }
    }
}

/// Drains one block's output into every receiving session. The router is the single reader of
/// a block's output channel, so the fan-out to all of them happens here: a chunk is read only
/// when every receiving session can take it (none at the high-water mark), which keeps the
/// slowest session's backpressure on the block — the same pause handshake one session applied
/// in stage 3, now serving them all without ever losing a chunk. Sessions that have not
/// attached yet receive nothing: their attach snapshots must not interleave with live data.
fn pump_pane(shared: &ProcessShared, pane: PaneId) {
    // Re-armed BEFORE reading, as every reader of a block does: output arriving during the
    // drain queues another PaneOutput event
    let block = {
        let mut views = shared.views.lock().unwrap();
        let Some(view) = views.get_mut(&pane) else {
            return;
        };
        view.rearm();
        view.block_id()
    };
    loop {
        // A session at the high-water mark stops the drain: the rest stays in the block's
        // channel (the block pauses server-side) until the session's writer asks to resume
        {
            let sessions = shared.sessions.lock().unwrap();
            if sessions
                .values()
                .filter(|entry| entry.receiving)
                .any(|entry| entry.writer.saturated())
            {
                return;
            }
        }
        let chunk = {
            let mut views = shared.views.lock().unwrap();
            let Some(view) = views.get_mut(&pane) else {
                return;
            };
            view.try_read()
        };
        let Some(chunk) = chunk else {
            return;
        };
        let frame = data_frame(FRAME_DATA, block, chunk);
        let sessions = shared.sessions.lock().unwrap();
        for entry in sessions.values() {
            if entry.receiving && !entry.writer.push_data(frame.clone()) {
                // Unreachable: the saturation check above runs in this very thread, and no
                // one else queues data frames. If it ever happens, stop draining rather than
                // keep reading into a queue that cannot take the chunk
                eprintln!("[server] a session's data queue overflowed mid-drain");
                return;
            }
        }
    }
}

/// Reads a session's request frames off the socket; anything not a control frame ends it
fn socket_reader(mut read: Box<dyn Read + Send>, msgs: Sender<SessionMsg>) {
    loop {
        let frame = match read_typed_frame(&mut *read) {
            Ok(frame) => frame,
            Err(_) => {
                let _ = msgs.send(SessionMsg::Stop);
                return;
            }
        };
        match frame {
            WireFrame::Control(json) => match serde_json::from_slice::<WireRequest>(&json) {
                Ok(request) => {
                    if msgs.send(SessionMsg::Wire(request)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    eprintln!("[server] a request did not decode: {error}");
                    let _ = msgs.send(SessionMsg::Stop);
                    return;
                }
            },
            // The client only sends control frames
            frame => {
                eprintln!("[server] the client sent a {frame:?} frame; dropping the session");
                let _ = msgs.send(SessionMsg::Stop);
                return;
            }
        }
    }
}

/// Accepts one more window: every connection becomes a session of its own. The most recently
/// attached one drives (see `Session::attach`); sessions come and go, and the blocks outlive
/// every one of them.
fn start_session(stream: IpcStream, handle: &ServerHandle, shared: &Arc<ProcessShared>) {
    let conn = shared.next_conn.fetch_add(1, Ordering::Relaxed);
    let (msgs, inbox) = channel::<SessionMsg>();
    let (read_half, write_half) = stream.into_halves();
    let writer = SessionWriter::spawn(write_half, msgs.clone());
    // Registered at once: events and data fan out through the registry, and a disconnect only
    // unregisters (the views are the router's, and they outlive the session)
    shared.register(conn, msgs.clone(), writer.clone());
    let session = Session {
        handle: handle.clone(),
        shared: shared.clone(),
        conn,
        writer,
        driver: false,
    };
    thread::Builder::new()
        .name("terminal-session".to_string())
        .spawn(move || run_session(session, inbox))
        .expect("could not start the session thread");
    thread::Builder::new()
        .name("terminal-session-reader".to_string())
        .spawn(move || socket_reader(read_half, msgs))
        .expect("could not start the session reader thread");
}

fn run_session(mut session: Session, inbox: Receiver<SessionMsg>) {
    // A panic here must not strand the panes' views: see `Session::pump`
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| session.pump(inbox)));
    if outcome.is_err() {
        eprintln!("[server] a session panicked; the session ended");
    }
    session.teardown();
}

/// Accepts windows one after another; called from `run_server_process`, and reachable directly
/// for tests that bind their own endpoint
fn accept_sessions(listener: IpcListener, handle: ServerHandle, shared: Arc<ProcessShared>) {
    loop {
        match listener.accept() {
            Ok(stream) => start_session(stream, &handle, &shared),
            Err(error) => {
                eprintln!("[server] accept failed: {error}");
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// The entry point of `terminal server`: binds the endpoint (refusing when a server already
/// runs), keeps the server thread alive, and serves sessions until killed — the most recently
/// attached window drives, the others watch. The shells inside its blocks outlive every
/// window; closing a window only ends its session.
pub fn run_server_process() {
    let endpoint = IpcEndpoint::resolve();
    let listener = match IpcListener::bind(&endpoint) {
        Ok(listener) => listener,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            eprintln!("[server] a terminal server is already listening on {endpoint:?}");
            std::process::exit(0);
        }
        Err(error) => {
            eprintln!("[server] could not listen on {endpoint:?}: {error}");
            std::process::exit(1);
        }
    };
    let (handle, events) = Server::start();
    let shared = Arc::new(ProcessShared::default());
    let router_shared = shared.clone();
    thread::Builder::new()
        .name("terminal-events".to_string())
        .spawn(move || route_events(events, router_shared))
        .expect("could not start the event router thread");
    accept_sessions(listener, handle, shared);
}

// ---- The WebSocket gateway for remote clients (stage 5a) ------------------------------------
//
// `terminal gateway` fronts the local server for ONE remote client at a time over a loopback
// HTTP port, which real deployments expose with `tailscale serve` (TLS and the identity
// headers come from tailscale). The remote protocol is the existing wire over WebSocket:
// binary messages with the 1-byte type prefix (0 control JSON, 1 data, 2 data-snapshot) and
// identical payloads. The gateway itself is a transport bridge, not a window: its connection
// to the local server is a regular S4 session, so the registry, driver policy and event fan-out
// serve a remote client unchanged (attach → the remote client drives, like any window).

/// Both halves of the gateway's own session at the local server
fn connect_server_halves(endpoint: &IpcEndpoint) -> io::Result<(Box<dyn Read + Send>, ConnWrite)> {
    #[cfg(unix)]
    match endpoint {
        IpcEndpoint::Socket(path) => {
            let stream = UnixStream::connect(path)?;
            let read = stream
                .try_clone()
                .expect("could not clone the socket for its reader half");
            Ok((Box::new(read), ConnWrite::Socket(stream)))
        }
    }
    #[cfg(windows)]
    match endpoint {
        IpcEndpoint::Pipe(name) => {
            let (read, write) = windows_pipe::connect_client(name)?;
            Ok((Box::new(read), ConnWrite::Pipe(Box::new(write))))
        }
    }
}

/// A control message as a WebSocket binary message: the 1-byte type prefix, then the JSON
fn ws_control_frame<T: serde::Serialize>(message: &T) -> Vec<u8> {
    let mut frame = vec![FRAME_CONTROL];
    if let Ok(json) = serde_json::to_vec(message) {
        frame.extend_from_slice(&json);
    }
    frame
}

/// Block data (or a snapshot) as a WebSocket binary message: the 1-byte type prefix, the
/// block token, then the bytes
fn ws_data_frame(kind: u8, token: u32, bytes: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(5 + bytes.len());
    frame.push(kind);
    frame.extend_from_slice(&token.to_le_bytes());
    frame.extend_from_slice(bytes);
    frame
}

/// A remote control message as a local control frame for the server session
fn server_control_frame(json: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(json.len() + 5);
    frame.push(FRAME_CONTROL);
    frame.extend_from_slice(&(json.len() as u32).to_le_bytes());
    frame.extend_from_slice(json);
    frame
}

/// Releases the gateway's one-remote-client slot when the bridge ends
struct BusySlotGuard<'a>(&'a AtomicBool);

impl Drop for BusySlotGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Serves one remote client over `stream`: upgrades it (or refuses it), then bridges the
/// websocket to the local server as a regular session. `slot` gates exactly one remote
/// client; it is claimed for the duration and released when the bridge ends.
fn serve_gateway_conn(
    mut stream: TcpStream,
    endpoint: &IpcEndpoint,
    allow: &[String],
    slot: &AtomicBool,
) {
    let request = match ws::read_upgrade_request(&mut stream) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("[gateway] a websocket upgrade failed: {error}");
            let _ = ws::write_bad_request(&stream);
            return;
        }
    };
    let Some(key) = request.key else {
        let _ = ws::write_bad_request(&stream);
        return;
    };
    // Identity: `Tailscale-User-Login`, falling back to `Tailscale-User-Name`. The headers are
    // trusted ONLY because `tailscale serve` injects them for trusted tailnet traffic — a
    // direct loopback hit can spoof them trivially, which is why the port binds 127.0.0.1
    // and real deployments front it with tailscale. An empty allow-list denies everyone.
    let login = request.login.clone().or_else(|| request.name.clone());
    let Some(login) = login.filter(|login| allow.iter().any(|allowed| allowed == login)) else {
        eprintln!(
            "[gateway] refused a remote client (login {:?} is not in remote.allow)",
            request.login.or(request.name)
        );
        let _ = ws::write_forbidden(&stream);
        return;
    };
    eprintln!("[gateway] {login} is attaching remotely");

    // Exactly one remote client at a time: a second one upgrades, gets a Busy event, and
    // closes right away (the local windows are unaffected)
    if slot.swap(true, Ordering::AcqRel) {
        eprintln!("[gateway] another remote client is already attached; refusing {login}");
        if ws::write_switching(&stream, &key).is_ok()
            && let Ok((reader, writer)) = ws::Ws::from_stream(stream)
        {
            let _ = writer.write_binary(&ws_control_frame(&WireEvent {
                v: WIRE_VERSION,
                event: WireEventKind::Busy,
            }));
            writer.write_close(1013);
            drop(reader);
        }
        return;
    }
    let _slot = BusySlotGuard(slot);

    // The upgrade is answered: the remote protocol starts here
    if let Err(error) = ws::write_switching(&stream, &key) {
        eprintln!("[gateway] the 101 could not go out: {error}");
        return;
    }
    let Ok((mut reader, writer)) = ws::Ws::from_stream(stream) else {
        eprintln!("[gateway] the upgraded socket could not split into halves");
        return;
    };
    // Hello: who the remote client authenticated as
    let _ = writer.write_binary(&ws_control_frame(&WireReply::hello(login.clone())));

    // The gateway's own session at the local server: a regular client of it
    let (mut server_read, mut server_write) = match connect_server_halves(endpoint) {
        Ok(halves) => halves,
        Err(error) => {
            eprintln!("[gateway] could not reach the terminal server: {error}");
            writer.write_close(1011);
            return;
        }
    };

    // Uplink: remote control frames become server control frames. A remote client never sends
    // data frames; anything else — or the close of the websocket — ends the session.
    let uplink = thread::Builder::new()
        .name("terminal-gateway-uplink".to_string())
        .spawn(move || {
            loop {
                match reader.read_message() {
                    Ok(ws::Message::Binary(bytes)) => match bytes.first() {
                        Some(&FRAME_CONTROL) => {
                            if server_write
                                .send_frame(&server_control_frame(&bytes[1..]))
                                .is_err()
                            {
                                break;
                            }
                        }
                        Some(&kind) => {
                            eprintln!(
                                "[gateway] the remote client sent a {kind} frame; only control frames travel up"
                            );
                            break;
                        }
                        None => break,
                    },
                    Ok(ws::Message::Close(code)) => {
                        if let Some(code) = code {
                            eprintln!("[gateway] the remote client closed with {code}");
                        }
                        break;
                    }
                    Err(_) => break, // the websocket is gone
                }
            }
            // The session at the server ends with the websocket: its reader gets an
            // end-of-file and frees the slot
            server_write.shutdown();
        })
        .expect("could not start the gateway uplink thread");

    // Downlink: replies, events and block data stream through to the remote client
    let downlink_writer = writer.clone();
    let downlink = thread::Builder::new()
        .name("terminal-gateway-downlink".to_string())
        .spawn(move || {
            while let Ok(frame) = read_typed_frame(&mut *server_read) {
                let message = match frame {
                    WireFrame::Control(json) => {
                        let mut message = vec![FRAME_CONTROL];
                        message.extend_from_slice(&json);
                        message
                    }
                    WireFrame::Data(token, bytes) => ws_data_frame(FRAME_DATA, token, &bytes),
                    WireFrame::Snapshot(token, bytes) => {
                        ws_data_frame(FRAME_SNAPSHOT, token, &bytes)
                    }
                };
                if downlink_writer.write_binary(&message).is_err() {
                    break; // the remote client is gone
                }
            }
            // The remote client lost its server: end the websocket in both directions
            downlink_writer.write_close(1011);
            downlink_writer.shutdown();
        })
        .expect("could not start the gateway downlink thread");

    let _ = uplink.join();
    let _ = downlink.join();
}

/// Accepts remote clients one after another; `allow` is the configured `remote.allow` list.
/// Also the entry point the tests drive against their own listener and server.
fn accept_gateway(listener: TcpListener, endpoint: IpcEndpoint, allow: Vec<String>) {
    let slot = Arc::new(AtomicBool::new(false));
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let endpoint = endpoint.clone();
                let allow = allow.clone();
                let slot = slot.clone();
                thread::Builder::new()
                    .name("terminal-gateway".to_string())
                    .spawn(move || serve_gateway_conn(stream, &endpoint, &allow, &slot))
                    .expect("could not start the gateway thread");
            }
            Err(error) => {
                eprintln!("[gateway] accept failed: {error}");
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// The entry point of `terminal gateway`: serves remote clients over a loopback WebSocket
/// until killed. `allow` is the configured `remote.allow` list — empty denies everyone.
/// Real deployments front the port with `tailscale serve`, which supplies the identity
/// headers the gateway trusts.
pub fn run_gateway_process(allow: Vec<String>, port: u16) {
    let listener = match TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("[gateway] could not listen on 127.0.0.1:{port}: {error}");
            std::process::exit(1);
        }
    };
    eprintln!(
        "[gateway] serving remote clients on ws://127.0.0.1:{port} (front it with `tailscale serve`)"
    );
    let endpoint = IpcEndpoint::resolve();
    accept_gateway(listener, endpoint, allow);
}

#[cfg(test)]
mod tests;
