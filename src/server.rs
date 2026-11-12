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
use std::fmt;
use std::net::{TcpListener, TcpStream};
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
use serde::{Deserialize, Serialize};

use crate::config::ShellConfig;
use crate::layout::{Closed, Dir, Edge, Layout, LayoutState, Limits, PaneId, Path, Rect};
use crate::pane_io::{CHANNEL_CAPACITY, IoEvent, LocalPty, Notify, PaneIo};

use self::wire::{
    WIRE_VERSION, WireEvent, WireEventKind, WireOp, WireReply, WireReplyKind, WireRequest,
};

/// Identifies a block within its server
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BlockId(pub u64);

/// A pane the server just created for a layout op: the server-allocated pane id, the block
/// already running behind it, the size it was spawned at, and the fallback notice (the
/// configured shell could not start; the default shell did).
pub struct CreatedPane {
    pub pane: PaneId,
    pub block: ServerBlock,
    pub cols: u16,
    pub rows: u16,
    pub notice: Option<String>,
}

impl fmt::Debug for CreatedPane {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CreatedPane")
            .field("pane", &self.pane)
            .field("cols", &self.cols)
            .field("rows", &self.rows)
            .field("notice", &self.notice)
            .finish_non_exhaustive()
    }
}

/// What the server reports to the window. One [`State`](LayoutEvent::State) per applied layout
/// mutation; pane output and exits are keyed by the pane the server allocated for the block.
#[derive(Debug)]
pub enum LayoutEvent {
    /// The authoritative layout changed: rebuild the projection from it. A layout op that
    /// spawned a pane reports it fully built.
    State(LayoutState, Option<CreatedPane>),
    /// The last tiled pane was closed: the window quits
    LastPaneClosed,
    /// A mutation could not be applied (the program failed to start); the layout rolled back
    Error(String),
    /// A pane's program wrote: the window drains it through its block
    PaneOutput(PaneId),
    /// A pane's program exited
    PaneExited(PaneId),
}

/// "Split this pane in `dir` and start `shell` in the new one"
pub(crate) struct SplitOp {
    pub pane: PaneId,
    pub dir: Dir,
    pub shell: ShellConfig,
    pub explicit: bool,
    pub cols: u16,
    pub rows: u16,
    pub scrollback: usize,
    pub area: Rect,
    pub limits: Limits,
}

/// "Open a floating pane of these window fractions and start `shell` in it"
pub(crate) struct AddFloatOp {
    pub shell: ShellConfig,
    pub explicit: bool,
    pub width: f32,
    pub height: f32,
    pub cols: u16,
    pub rows: u16,
    pub scrollback: usize,
}

/// "Move the focus to the neighboring pane in that direction"
pub(crate) struct FocusDirOp {
    pub dir: Dir,
    pub area: Rect,
}

/// "Move this pane's edge in that direction by `delta` pixels"
pub(crate) struct ResizeOp {
    pub pane: PaneId,
    pub dir: Dir,
    pub delta: u32,
    pub area: Rect,
    pub limits: Limits,
}

/// "Move the divider at `path` to `boundary"`
pub(crate) struct SetDividerOp {
    pub path: Path,
    pub boundary: i64,
    pub area: Rect,
    pub limits: Limits,
}

/// "Move this floating pane by this much"
pub(crate) struct MoveFloatOp {
    pub pane: PaneId,
    pub dx: i64,
    pub dy: i64,
    pub window: Rect,
}

/// "Resize this floating pane by dragging its edge this far"
pub(crate) struct ResizeFloatEdgeOp {
    pub pane: PaneId,
    pub edge: Edge,
    pub dx: i64,
    pub dy: i64,
    pub window: Rect,
    pub limits: Limits,
}

/// Chunks forwarded per drain request; past that the block queues another request so the other
/// blocks' messages interleave
const MAX_CHUNKS_PER_DRAIN: usize = 16;

/// How long the exit notice waits for the reader thread to close the output channel before it
/// reports the exit anyway (ConPTY can keep the pipe open forever after the child dies)
const EXIT_GRACE: Duration = Duration::from_millis(250);

/// What the server answers to a create request: the block and its output channel, plus the
/// fallback notice
pub(crate) type Created = Result<(BlockId, Receiver<Vec<u8>>, Option<String>), String>;

/// Messages the server thread handles, in arrival order
#[allow(clippy::large_enum_variant)]
pub(crate) enum Request {
    /// Creates the first pane and the authoritative layout, before the event loop runs
    Init {
        shell: ShellConfig,
        cols: u16,
        rows: u16,
        scrollback: usize,
        reply: Sender<Result<(LayoutState, CreatedPane), String>>,
    },
    /// Attaches a session: a fresh server runs init ("fresh server start behaves like Init");
    /// an existing layout answers its state and its live panes with their block tokens
    Attach {
        shell: ShellConfig,
        cols: u16,
        rows: u16,
        scrollback: usize,
        reply: Sender<AttachOutcome>,
    },
    LayoutSplit(SplitOp),
    LayoutClose {
        pane: PaneId,
    },
    LayoutSetFocus {
        pane: PaneId,
    },
    LayoutFocusDir(FocusDirOp),
    LayoutFocusCycle {
        forward: bool,
    },
    LayoutResize(ResizeOp),
    LayoutSetDivider(SetDividerOp),
    LayoutToggleZoom,
    LayoutAddFloat(AddFloatOp),
    LayoutToggleFloats,
    LayoutMoveFloat(MoveFloatOp),
    LayoutResizeFloatEdge(ResizeFloatEdgeOp),
    /// Starts a block outside the layout (tests, and attaching clients in stage 3)
    #[cfg(test)]
    Create {
        shell: ShellConfig,
        explicit: bool,
        cols: u16,
        rows: u16,
        scrollback: usize,
        client: Client,
        reply: Sender<Created>,
    },
    Write(BlockId, Vec<u8>),
    Resize(BlockId, u16, u16),
    Kill(BlockId),
    /// Kills the program and forgets the block
    Close(BlockId),
    ForegroundDir(BlockId, Sender<Option<PathBuf>>),
    Snapshot(BlockId, Sender<Vec<u8>>),
    // Automation (stage 6): served directly on the blocks. These deliberately bypass the S4
    // driver gating — an automation CLI is an explicit local act (socket access is local
    // trust), and gating its writes behind the window driver would make automation useless
    // next to a window (S6 design, locked 2026-11-12)
    AutomationList {
        reply: Sender<Vec<wire::AutomationPane>>,
    },
    /// Writes straight to the block's PTY writer, exactly like the driver's `Write`
    AutomationSend(BlockId, Vec<u8>, Sender<Result<(), String>>),
    /// The screen as plain text (the pane's held screen is served too; the CLI checks alive
    /// client-side and errors there)
    AutomationCapture(BlockId, Sender<Result<String, String>>),
    /// The snapshot to queue before live bytes start flowing (`None`: unknown block)
    AutomationSubscribe(BlockId, Sender<Option<Vec<u8>>>),
    /// The program wrote something, or a paused client read: forward what is pending
    Drain(BlockId),
    /// The program exited (sent by the PTY's thread)
    Exited(BlockId),
    /// Every handle was dropped
    Shutdown,
    /// A request handler that panics on purpose (the run loop must survive it)
    #[cfg(test)]
    Panic,
}

/// What a session is told for its attach: a fresh server ran init (the created pane's block view
/// travels in the reply), an existing one reports its state and its live panes with their block
/// tokens (the session pulls their block views from its own map)
#[derive(Debug)]
pub(crate) enum AttachOutcome {
    Fresh(Result<(LayoutState, CreatedPane), String>),
    Existing(LayoutState, Vec<AttachedPaneInfo>),
}

/// A live pane in an attach: the pane, its block token, and the size to start the pane at
#[derive(Debug, Clone, Copy)]
pub(crate) struct AttachedPaneInfo {
    pub pane: PaneId,
    pub block: BlockId,
    pub cols: u16,
    pub rows: u16,
}

impl fmt::Debug for Request {
    /// Requests carry reply channels the debug output never needs; only the kind matters
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Request::Init { .. } => "Init(..)",
            Request::Attach { .. } => "Attach(..)",
            Request::LayoutSplit(_) => "LayoutSplit(..)",
            Request::LayoutClose { .. } => "LayoutClose{..}",
            Request::LayoutSetFocus { .. } => "LayoutSetFocus{..}",
            Request::LayoutFocusDir(_) => "LayoutFocusDir(..)",
            Request::LayoutFocusCycle { .. } => "LayoutFocusCycle{..}",
            Request::LayoutResize(_) => "LayoutResize(..)",
            Request::LayoutSetDivider(_) => "LayoutSetDivider(..)",
            Request::LayoutToggleZoom => "LayoutToggleZoom",
            Request::LayoutAddFloat(_) => "LayoutAddFloat(..)",
            Request::LayoutToggleFloats => "LayoutToggleFloats",
            Request::LayoutMoveFloat(_) => "LayoutMoveFloat(..)",
            Request::LayoutResizeFloatEdge(_) => "LayoutResizeFloatEdge(..)",
            #[cfg(test)]
            Request::Create { .. } => "Create{..}",
            Request::Write(_, _) => "Write(..)",
            Request::Resize(_, _, _) => "Resize(..)",
            Request::Kill(_) => "Kill(..)",
            Request::Close(_) => "Close(..)",
            Request::ForegroundDir(_, _) => "ForegroundDir(..)",
            Request::Snapshot(_, _) => "Snapshot(..)",
            Request::AutomationList { .. } => "AutomationList(..)",
            Request::AutomationSend(_, _, _) => "AutomationSend(..)",
            Request::AutomationCapture(_, _) => "AutomationCapture(..)",
            Request::AutomationSubscribe(_, _) => "AutomationSubscribe(..)",
            Request::Drain(_) => "Drain(..)",
            Request::Exited(_) => "Exited(..)",
            Request::Shutdown => "Shutdown",
            #[cfg(test)]
            Request::Panic => "Panic",
        })
    }
}

/// The state a block shares with its client ([`ServerBlock`])
pub(crate) struct Client {
    notify: Notify,
    /// One [`IoEvent::Output`] at a time; the client re-arms it before draining
    wake_pending: Arc<AtomicBool>,
    /// The client's channel was full: the client asks for a drain after its next read
    paused: Arc<AtomicBool>,
}

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
            #[cfg(test)]
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
            #[cfg(test)]
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

// ---- The wire format (stage 3) -----------------------------------------------------------

/// The wire format of the control channel: length-prefixed JSON, versioned, carrying the same
/// ops and events the in-process channel carries. Only the mapping lives here; the transport
/// (socket and sessions) is the server process section below.
pub(crate) mod wire {
    use std::collections::BTreeMap;
    use std::fmt;
    use std::path::PathBuf;

    use serde::{Deserialize, Serialize};

    use crate::config::ShellConfig;
    use crate::layout::{Dir, Edge, LayoutState, Limits, PaneId, Path, Rect};

    use super::{
        AddFloatOp, AttachedPaneInfo, BlockId, CreatedPane, FocusDirOp, LayoutEvent, MoveFloatOp,
        Request, ResizeFloatEdgeOp, ResizeOp, SetDividerOp, SplitOp,
    };

    /// `v` of the protocol; a client and a server whose versions differ refuse to talk
    pub(crate) const WIRE_VERSION: u32 = 1;

    /// Largest control frame accepted. A serialized state of a many-pane tree and a reply fit
    /// with room to spare; anything larger is a corrupt length prefix, never a message.
    pub(crate) const MAX_FRAME: usize = 64 * 1024 * 1024;

    /// One request. A session (or a window) sends exactly one of these per wire op; attach is
    /// the one the wire adds: a fresh server runs init, an attached one hands over its state
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "op", rename_all = "snake_case")]
    pub(crate) enum WireOp {
        Init {
            shell: WireShell,
            cols: u16,
            rows: u16,
            scrollback: usize,
        },
        Attach {
            shell: WireShell,
            cols: u16,
            rows: u16,
            scrollback: usize,
        },
        Split {
            pane: PaneId,
            dir: Dir,
            shell: WireShell,
            explicit: bool,
            cols: u16,
            rows: u16,
            scrollback: usize,
            area: Rect,
            limits: Limits,
        },
        Close {
            pane: PaneId,
        },
        SetFocus {
            pane: PaneId,
        },
        FocusDir {
            dir: Dir,
            area: Rect,
        },
        FocusCycle {
            forward: bool,
        },
        Resize {
            pane: PaneId,
            dir: Dir,
            delta: u32,
            area: Rect,
            limits: Limits,
        },
        SetDivider {
            path: Path,
            boundary: i64,
            area: Rect,
            limits: Limits,
        },
        ToggleZoom,
        AddFloat {
            shell: WireShell,
            explicit: bool,
            width: f32,
            height: f32,
            cols: u16,
            rows: u16,
            scrollback: usize,
        },
        ToggleFloats,
        MoveFloat {
            pane: PaneId,
            dx: i64,
            dy: i64,
            window: Rect,
        },
        ResizeFloatEdge {
            pane: PaneId,
            edge: Edge,
            dx: i64,
            dy: i64,
            window: Rect,
            limits: Limits,
        },
        Write {
            block: BlockId,
            bytes: Vec<u8>,
        },
        ResizeBlock {
            block: BlockId,
            cols: u16,
            rows: u16,
        },
        Kill {
            block: BlockId,
        },
        CloseBlock {
            block: BlockId,
        },
        ForegroundDir {
            block: BlockId,
        },
        Snapshot {
            block: BlockId,
        },
        /// The automation CLI asks for the live panes (`terminal ls`)
        AutomationList {},
        /// The automation CLI writes bytes straight to a block's PTY writer (`terminal send`,
        /// and `terminal attach`'s stdin). Deliberately NOT the driver-gated `Write`: an
        /// automation CLI is an explicit local act (socket access is local trust), and gating
        /// it behind the window driver would make automation useless next to a window (S6)
        Send {
            block: BlockId,
            bytes: Vec<u8>,
        },
        /// The automation CLI reads a block's screen as plain text (`terminal capture`); the
        /// raw VT snapshot rides the existing `Snapshot` op instead
        Capture {
            block: BlockId,
        },
        /// The automation CLI subscribes a block's data channel (`terminal attach`): the
        /// snapshot is queued first, then the session starts receiving live bytes
        AutomationSubscribe {
            block: BlockId,
        },
    }

    /// One live pane of the automation list: the pane id, its block token, the grid it was last
    /// spawned or resized at, and whether its program is still running
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub(crate) struct AutomationPane {
        pub pane: PaneId,
        pub block: BlockId,
        pub cols: u16,
        pub rows: u16,
        pub alive: bool,
    }

    /// One request for the server, versioned. `rid` names the sender's correlation id for the
    /// requests that answer (attach/init, foreground_dir, snapshot); the rest send `None`.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub(crate) struct WireRequest {
        pub v: u32,
        pub rid: Option<u64>,
        pub op: WireOp,
    }

    impl WireOp {
        /// The wire form of a request that travels. Server-internal traffic (drains, exit
        /// notices, shutdown) and the test-only create stay in the process.
        pub(crate) fn from_request(request: &Request) -> Option<Self> {
            let op = match request {
                Request::Init {
                    shell,
                    cols,
                    rows,
                    scrollback,
                    ..
                } => Self::Init {
                    shell: WireShell::from_config(shell),
                    cols: *cols,
                    rows: *rows,
                    scrollback: *scrollback,
                },
                Request::LayoutSplit(op) => Self::Split {
                    pane: op.pane,
                    dir: op.dir,
                    shell: WireShell::from_config(&op.shell),
                    explicit: op.explicit,
                    cols: op.cols,
                    rows: op.rows,
                    scrollback: op.scrollback,
                    area: op.area,
                    limits: op.limits,
                },
                Request::LayoutClose { pane } => Self::Close { pane: *pane },
                Request::LayoutSetFocus { pane } => Self::SetFocus { pane: *pane },
                Request::LayoutFocusDir(op) => Self::FocusDir {
                    dir: op.dir,
                    area: op.area,
                },
                Request::LayoutFocusCycle { forward } => Self::FocusCycle { forward: *forward },
                Request::LayoutResize(op) => Self::Resize {
                    pane: op.pane,
                    dir: op.dir,
                    delta: op.delta,
                    area: op.area,
                    limits: op.limits,
                },
                Request::LayoutSetDivider(op) => Self::SetDivider {
                    path: op.path.clone(),
                    boundary: op.boundary,
                    area: op.area,
                    limits: op.limits,
                },
                Request::LayoutToggleZoom => Self::ToggleZoom,
                Request::LayoutAddFloat(op) => Self::AddFloat {
                    shell: WireShell::from_config(&op.shell),
                    explicit: op.explicit,
                    width: op.width,
                    height: op.height,
                    cols: op.cols,
                    rows: op.rows,
                    scrollback: op.scrollback,
                },
                Request::LayoutToggleFloats => Self::ToggleFloats,
                Request::LayoutMoveFloat(op) => Self::MoveFloat {
                    pane: op.pane,
                    dx: op.dx,
                    dy: op.dy,
                    window: op.window,
                },
                Request::LayoutResizeFloatEdge(op) => Self::ResizeFloatEdge {
                    pane: op.pane,
                    edge: op.edge,
                    dx: op.dx,
                    dy: op.dy,
                    window: op.window,
                    limits: op.limits,
                },
                Request::Write(id, bytes) => Self::Write {
                    block: *id,
                    bytes: bytes.clone(),
                },
                Request::Resize(id, cols, rows) => Self::ResizeBlock {
                    block: *id,
                    cols: *cols,
                    rows: *rows,
                },
                Request::Kill(id) => Self::Kill { block: *id },
                Request::Close(id) => Self::CloseBlock { block: *id },
                Request::ForegroundDir(id, _) => Self::ForegroundDir { block: *id },
                Request::Snapshot(id, _) => Self::Snapshot { block: *id },
                // Not on the wire: the server's own plumbing and the test-only create. The
                // automation asks travel the other way (a CLI op becomes its request in the
                // session), so they have no request-to-op form either
                Request::Drain(_) | Request::Exited(_) | Request::Shutdown => return None,
                Request::Attach { .. } => return None,
                Request::AutomationList { .. }
                | Request::AutomationSend(..)
                | Request::AutomationCapture(..)
                | Request::AutomationSubscribe(..) => return None,
                #[cfg(test)]
                Request::Panic | Request::Create { .. } => return None,
            };
            Some(op)
        }

        /// The in-process request for a fire-and-forget wire op, for a session to hand to the
        /// server thread. Ops that carry a reply (attach/init, foreground directory, snapshot)
        /// answer through their own paths and are not mapped here.
        pub(crate) fn into_request(self) -> Option<Request> {
            let request = match self {
                Self::Init { .. } | Self::Attach { .. } => return None,
                Self::Split {
                    pane,
                    dir,
                    shell,
                    explicit,
                    cols,
                    rows,
                    scrollback,
                    area,
                    limits,
                } => Request::LayoutSplit(SplitOp {
                    pane,
                    dir,
                    shell: shell.into_config(),
                    explicit,
                    cols,
                    rows,
                    scrollback,
                    area,
                    limits,
                }),
                Self::Close { pane } => Request::LayoutClose { pane },
                Self::SetFocus { pane } => Request::LayoutSetFocus { pane },
                Self::FocusDir { dir, area } => Request::LayoutFocusDir(FocusDirOp { dir, area }),
                Self::FocusCycle { forward } => Request::LayoutFocusCycle { forward },
                Self::Resize {
                    pane,
                    dir,
                    delta,
                    area,
                    limits,
                } => Request::LayoutResize(ResizeOp {
                    pane,
                    dir,
                    delta,
                    area,
                    limits,
                }),
                Self::SetDivider {
                    path,
                    boundary,
                    area,
                    limits,
                } => Request::LayoutSetDivider(SetDividerOp {
                    path,
                    boundary,
                    area,
                    limits,
                }),
                Self::ToggleZoom => Request::LayoutToggleZoom,
                Self::AddFloat {
                    shell,
                    explicit,
                    width,
                    height,
                    cols,
                    rows,
                    scrollback,
                } => Request::LayoutAddFloat(AddFloatOp {
                    shell: shell.into_config(),
                    explicit,
                    width,
                    height,
                    cols,
                    rows,
                    scrollback,
                }),
                Self::ToggleFloats => Request::LayoutToggleFloats,
                Self::MoveFloat {
                    pane,
                    dx,
                    dy,
                    window,
                } => Request::LayoutMoveFloat(MoveFloatOp {
                    pane,
                    dx,
                    dy,
                    window,
                }),
                Self::ResizeFloatEdge {
                    pane,
                    edge,
                    dx,
                    dy,
                    window,
                    limits,
                } => Request::LayoutResizeFloatEdge(ResizeFloatEdgeOp {
                    pane,
                    edge,
                    dx,
                    dy,
                    window,
                    limits,
                }),
                Self::Write { block, bytes } => Request::Write(block, bytes),
                Self::ResizeBlock { block, cols, rows } => Request::Resize(block, cols, rows),
                Self::Kill { block } => Request::Kill(block),
                Self::CloseBlock { block } => Request::Close(block),
                // The automation family is served by the session itself: the ops carry no
                // in-process request form (they are CLI asks, never window traffic)
                Self::ForegroundDir { .. } | Self::Snapshot { .. } => return None,
                Self::AutomationList {}
                | Self::Send { .. }
                | Self::Capture { .. }
                | Self::AutomationSubscribe { .. } => return None,
            };
            Some(request)
        }
    }

    /// A [`ShellConfig`] as it travels: same fields, wire-owned (config.rs stays untouched)
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub(crate) struct WireShell {
        pub program: Option<String>,
        pub args: Vec<String>,
        pub cwd: Option<PathBuf>,
        pub env: BTreeMap<String, String>,
        pub integration: bool,
    }

    impl WireShell {
        /// The wire form of a shell config
        pub(crate) fn from_config(shell: &ShellConfig) -> Self {
            Self {
                program: shell.program.clone(),
                args: shell.args.clone(),
                cwd: shell.cwd.clone(),
                env: shell.env.clone(),
                integration: shell.integration,
            }
        }

        /// Rebuilds the shell config on the server side
        pub(crate) fn into_config(self) -> ShellConfig {
            ShellConfig {
                program: self.program,
                args: self.args,
                cwd: self.cwd,
                env: self.env,
                integration: self.integration,
            }
        }
    }

    /// A pane the server just created or is handing over attached, as it travels: the pane id,
    /// its block token (data frames for it arrive under the same token), the size it was spawned
    /// at, and the fallback notice. The block itself does not ride the control channel: its
    /// screen snapshot arrives over the data channel before its live chunks.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub(crate) struct IpcCreatedPane {
        pub pane: PaneId,
        pub block: BlockId,
        pub cols: u16,
        pub rows: u16,
        pub notice: Option<String>,
    }

    impl IpcCreatedPane {
        /// The wire form of a created pane
        pub(crate) fn from_created(created: &CreatedPane) -> Self {
            Self {
                pane: created.pane,
                block: created.block.block_id(),
                cols: created.cols,
                rows: created.rows,
                notice: created.notice.clone(),
            }
        }

        /// The wire form of a live pane handed to a session that attached over the wire
        pub(crate) fn from_attached(info: AttachedPaneInfo) -> Self {
            Self {
                pane: info.pane,
                block: info.block,
                cols: info.cols,
                rows: info.rows,
                notice: None,
            }
        }
    }

    /// One answer from the server, correlated by `rid`
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub(crate) struct WireReply {
        pub v: u32,
        pub rid: u64,
        pub reply: WireReplyKind,
    }

    impl WireReply {
        /// Wraps any answer with the protocol version
        pub(crate) fn new(rid: u64, reply: WireReplyKind) -> Self {
            Self {
                v: WIRE_VERSION,
                rid,
                reply,
            }
        }

        /// The answer to `Init`: the first pane's state and its created pane, or why it refused
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "the session builds its attach replies inline; init stays for the wire tests"
            )
        )]
        pub(crate) fn init(
            rid: u64,
            result: Result<(LayoutState, Option<CreatedPane>), String>,
        ) -> Self {
            Self::new(
                rid,
                result
                    .map(|(state, created)| WireReplyKind::Init {
                        state,
                        created: created.as_ref().map(IpcCreatedPane::from_created),
                    })
                    .unwrap_or_else(|error| WireReplyKind::InitFailed { error }),
            )
        }

        /// The answer to `ForegroundDir`
        pub(crate) fn foreground_dir(rid: u64, dir: Option<PathBuf>) -> Self {
            Self::new(rid, WireReplyKind::ForegroundDir { dir })
        }

        /// The answer to `Snapshot` (a session replying to the wire op; an attach carries its
        /// snapshots over the data channel instead, but the control shape stays for simple asks)
        pub(crate) fn snapshot(rid: u64, bytes: Vec<u8>) -> Self {
            Self::new(rid, WireReplyKind::Snapshot { bytes })
        }

        /// The gateway's hello: who the remote client authenticated as. Sent right after the
        /// websocket upgrade, with `rid` 0 — gateway-initiated, no ask behind it.
        pub(crate) fn hello(login: String) -> Self {
            Self::new(0, WireReplyKind::Hello { login })
        }

        /// The answer to `AutomationList`
        pub(crate) fn automation_list(rid: u64, panes: Vec<AutomationPane>) -> Self {
            Self::new(rid, WireReplyKind::AutomationList { panes })
        }

        /// The answer to a confirmed `Send`
        pub(crate) fn automation_sent(rid: u64) -> Self {
            Self::new(rid, WireReplyKind::AutomationSent {})
        }

        /// The answer to `Capture`
        pub(crate) fn automation_capture(rid: u64, text: String) -> Self {
            Self::new(rid, WireReplyKind::AutomationCapture { text })
        }

        /// The answer to `AutomationSubscribe` (the snapshot is already queued on the data
        /// channel; the reply arrives ahead of it because controls overtake data)
        pub(crate) fn automation_subscribed(rid: u64) -> Self {
            Self::new(rid, WireReplyKind::AutomationSubscribed {})
        }

        /// A refused automation ask (unknown or dead block)
        pub(crate) fn automation_failed(rid: u64, error: String) -> Self {
            Self::new(rid, WireReplyKind::AutomationFailed { error })
        }
    }

    /// What a reply answers with
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "reply", rename_all = "snake_case")]
    pub(crate) enum WireReplyKind {
        Init {
            state: LayoutState,
            created: Option<IpcCreatedPane>,
        },
        InitFailed {
            error: String,
        },
        /// The answer to `Attach` on a server that already had a layout: its state and its live
        /// panes (their snapshots go over the data channel right before the reply), plus the
        /// panes whose program exited while no session was watching — the client marks them
        /// dead on attach, and the list is cleared once delivered
        Attached {
            state: LayoutState,
            panes: Vec<IpcCreatedPane>,
            #[serde(default)]
            exited: Vec<PaneId>,
        },
        ForegroundDir {
            dir: Option<PathBuf>,
        },
        Snapshot {
            bytes: Vec<u8>,
        },
        /// The gateway's hello: the login the remote client authenticated as (see
        /// [`WireReply::hello`])
        Hello {
            login: String,
        },
        /// The answer to `AutomationList`: the server's live panes, sorted by pane
        AutomationList {
            panes: Vec<AutomationPane>,
        },
        /// The answer to a confirmed `Send`: the bytes are on their way to the PTY
        AutomationSent {},
        /// The answer to `Capture`: the screen as plain text
        AutomationCapture {
            text: String,
        },
        /// The answer to `AutomationSubscribe`: the snapshot is queued on the data channel and
        /// live bytes follow
        AutomationSubscribed {},
        /// A refused automation ask (unknown or dead block)
        AutomationFailed {
            error: String,
        },
    }

    /// One event from the server, as the event drain maps it
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub(crate) struct WireEvent {
        pub v: u32,
        pub event: WireEventKind,
    }

    impl WireEvent {
        /// The wire form of a layout event: exactly what the server broadcasts today, with the
        /// created pane reduced to what travels (the block itself goes over the data channel)
        pub(crate) fn from_layout_event(event: &LayoutEvent) -> Option<Self> {
            let event = match event {
                LayoutEvent::State(state, created) => WireEventKind::State {
                    state: state.clone(),
                    created: created.as_ref().map(IpcCreatedPane::from_created),
                },
                LayoutEvent::LastPaneClosed => WireEventKind::LastPaneClosed,
                LayoutEvent::Error(error) => WireEventKind::Error {
                    message: error.clone(),
                },
                LayoutEvent::PaneOutput(pane) => WireEventKind::PaneOutput { pane: *pane },
                LayoutEvent::PaneExited(pane) => WireEventKind::PaneExited { pane: *pane },
            };
            Some(Self {
                v: WIRE_VERSION,
                event,
            })
        }
    }

    /// What an event reports
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "event", rename_all = "snake_case")]
    pub(crate) enum WireEventKind {
        State {
            state: LayoutState,
            created: Option<IpcCreatedPane>,
        },
        LastPaneClosed,
        Error {
            message: String,
        },
        PaneOutput {
            pane: PaneId,
        },
        PaneExited {
            pane: PaneId,
        },
        /// The driver changed: the receiving session drives when it does (`is_driver`), and is
        /// a viewer otherwise — the server drops its mutating ops from then on
        DriverChanged {
            is_driver: bool,
        },
        /// Only the gateway sends it, over the websocket: another remote client already holds
        /// the gateway's single remote slot, and the connection closes right after. A window
        /// never receives one.
        Busy,
    }

    /// What decoding a frame can answer with
    #[derive(Debug)]
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the socket reads a frame at a time; the buffer framing stays for the tests"
        )
    )]
    pub(crate) enum WireError {
        /// The length prefix is not a frame this protocol carries
        Frame,
        /// The JSON inside the frame did not decode
        Json(serde_json::Error),
    }

    impl fmt::Display for WireError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                WireError::Frame => f.write_str("the control frame's length prefix is invalid"),
                WireError::Json(error) => {
                    write!(f, "the control message's JSON did not decode: {error}")
                }
            }
        }
    }

    /// Frames one control message: 4-byte little-endian length followed by the message bytes
    pub(crate) fn frame(message: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + message.len());
        out.extend_from_slice(&(message.len() as u32).to_le_bytes());
        out.extend_from_slice(message);
        out
    }

    /// Reads the first framed message out of `buf`
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the socket reads a frame at a time; the buffer framing stays for the tests"
        )
    )]
    pub(crate) fn deframe(buf: &[u8]) -> Deframed {
        let Some(len) = buf
            .first_chunk::<4>()
            .map(|prefix| u32::from_le_bytes(*prefix) as usize)
        else {
            return Deframed::Incomplete;
        };
        if len > MAX_FRAME {
            return Deframed::Invalid;
        }
        match buf.get(4..4 + len) {
            Some(message) => Deframed::Message {
                bytes: message.to_vec(),
                consumed: 4 + len,
            },
            None => Deframed::Incomplete,
        }
    }

    /// What [`deframe`] found in the buffer
    #[derive(Debug, PartialEq, Eq)]
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the socket reads a frame at a time; the buffer framing stays for the tests"
        )
    )]
    pub(crate) enum Deframed {
        /// One complete message, plus how many bytes of the buffer it took
        Message { bytes: Vec<u8>, consumed: usize },
        /// The buffer does not hold a whole message yet: wait for more bytes
        Incomplete,
        /// The length prefix is not a frame this protocol carries: give up on the connection
        Invalid,
    }

    /// Frames one control message as JSON
    pub(crate) fn frame_json<T: serde::Serialize>(
        message: &T,
    ) -> Result<Vec<u8>, serde_json::Error> {
        Ok(frame(&serde_json::to_vec(message)?))
    }

    /// Decodes the next JSON control message out of `buf`, if a whole framed message is there;
    /// whatever follows stays for the next call
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the socket reads a frame at a time; the buffer framing stays for the tests"
        )
    )]
    pub(crate) fn deframe_json<T: serde::de::DeserializeOwned>(
        buf: &[u8],
    ) -> Result<Option<(T, usize)>, WireError> {
        match deframe(buf) {
            Deframed::Incomplete => Ok(None),
            Deframed::Invalid => Err(WireError::Frame),
            Deframed::Message { bytes, consumed } => serde_json::from_slice(&bytes)
                .map(|message| Some((message, consumed)))
                .map_err(WireError::Json),
        }
    }
}

// ---- A minimal RFC 6455 WebSocket server (the tailnet gateway's transport) -------------------
//
// Hand-rolled on the standard library (plus `sha1_smol` and `base64` for the handshake digest)
// to keep the dependency tree light: the protocol the gateway needs is small and the whole
// surface is unit-tested below. Minimal-client assumptions, by design:
//
// - Client frames MUST be masked (RFC 6455 §5.1); an unmasked one is a protocol error.
// - Data is binary only. A text frame is refused (the wire protocol has its own framing), and
//   `permessage-deflate` is not negotiated, so RSV bits set are a protocol error.
// - Fragmented binary messages are JOINED, never rejected: continuation frames accumulate into
//   one message, capped at `wire::MAX_FRAME`.
// - Pings are answered with matching pongs; pongs from the client are ignored.
// - A close frame is echoed once and reported; no half-open close handshake.
// - The upgrade request is read byte-by-byte up to the blank line, so no buffered frame bytes
//   are ever lost (real clients wait for the 101 anyway; pipelining is not supported).
//
// The identity headers (`Tailscale-User-Login` / `Tailscale-User-Name`) are ONLY trusted
// because `tailscale serve` injects them for trusted tailnet traffic. Anything hitting this
// port directly can set them at will, which is acceptable because the gateway binds 127.0.0.1
// only and real deployments front it with tailscale.
pub(crate) mod ws {
    use base64::Engine as _;
    use std::io::{self, Read, Write};
    use std::net::TcpStream;
    use std::sync::{Arc, Mutex};

    /// The WebSocket GUID the accept key hashes over (RFC 6455 §1.3)
    const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    const OP_CONTINUATION: u8 = 0x0;
    const OP_TEXT: u8 = 0x1;
    const OP_BINARY: u8 = 0x2;
    const OP_CLOSE: u8 = 0x8;
    const OP_PING: u8 = 0x9;
    const OP_PONG: u8 = 0xA;

    fn protocol_error(why: &str) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, why.to_string())
    }

    /// The upgrade request the client sent, as far as the gateway cares
    pub(crate) struct Request {
        /// `Sec-WebSocket-Key`, required to answer the upgrade
        pub(crate) key: Option<String>,
        /// `Tailscale-User-Login`, injected by `tailscale serve` for trusted traffic
        pub(crate) login: Option<String>,
        /// `Tailscale-User-Name`, the fallback identity header
        pub(crate) name: Option<String>,
    }

    /// Reads the request head byte-by-byte up to the blank line (no buffering: whatever the
    /// client sends after the head is never lost)
    fn read_head(stream: &mut TcpStream) -> io::Result<String> {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if head.len() > 16 * 1024 {
                return Err(protocol_error("the upgrade request head is too large"));
            }
            if stream.read(&mut byte)? == 0 {
                return Err(protocol_error("the upgrade request ended mid-head"));
            }
            head.push(byte[0]);
        }
        Ok(String::from_utf8_lossy(&head).into_owned())
    }

    /// Parses the upgrade request. Only what the gateway needs is read; a request without a
    /// handshake key simply carries `key: None` and is answered with a bad request.
    pub(crate) fn read_upgrade_request(stream: &mut TcpStream) -> io::Result<Request> {
        let head = read_head(stream)?;
        let mut lines = head.split("\r\n");
        let request_line = lines.next().unwrap_or_default();
        if !request_line.starts_with("GET ") {
            return Err(protocol_error("the request is not a websocket upgrade"));
        }
        let mut request = Request {
            key: None,
            login: None,
            name: None,
        };
        for line in lines {
            if line.is_empty() {
                break;
            }
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            match name.trim().to_ascii_lowercase().as_str() {
                "sec-websocket-key" => request.key = Some(value.trim().to_string()),
                "sec-websocket-version" if value.trim() != "13" => {
                    return Err(protocol_error("only websocket version 13 is spoken"));
                }
                "tailscale-user-login" => request.login = Some(value.trim().to_string()),
                "tailscale-user-name" => request.name = Some(value.trim().to_string()),
                _ => {}
            }
        }
        Ok(request)
    }

    /// The `Sec-WebSocket-Accept` value for a handshake key (RFC 6455 §1.3)
    pub(crate) fn accept_key(key: &str) -> String {
        use sha1_smol::Sha1;
        let mut sha = Sha1::new();
        sha.update(key.as_bytes());
        sha.update(GUID.as_bytes());
        base64::engine::general_purpose::STANDARD.encode(sha.digest().bytes())
    }

    fn write_response(mut stream: &TcpStream, head: &str) -> io::Result<()> {
        stream.write_all(head.as_bytes())
    }

    /// Answers the upgrade: 101 with the computed accept header
    pub(crate) fn write_switching(stream: &TcpStream, key: &str) -> io::Result<()> {
        write_response(
            stream,
            &format!(
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
                 Sec-WebSocket-Accept: {}\r\n\r\n",
                accept_key(key)
            ),
        )
    }

    /// Refuses the upgrade: the identity is not in `remote.allow`
    pub(crate) fn write_forbidden(stream: &TcpStream) -> io::Result<()> {
        write_response(
            stream,
            "HTTP/1.1 403 Forbidden\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        )
    }

    /// The upgrade request was not a websocket upgrade
    pub(crate) fn write_bad_request(stream: &TcpStream) -> io::Result<()> {
        write_response(
            stream,
            "HTTP/1.1 400 Bad Request\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        )
    }

    /// One WebSocket frame
    struct Frame {
        fin: bool,
        opcode: u8,
        payload: Vec<u8>,
    }

    /// Reads one client frame. Client frames must be masked (RFC 6455 §5.1); the payload is
    /// unmasked in place. Control frames carry at most 125 bytes and are never fragmented.
    fn read_frame<R: Read>(mut read: R) -> io::Result<Frame> {
        let mut head = [0u8; 2];
        read.read_exact(&mut head)?;
        if head[0] & 0x70 != 0 {
            return Err(protocol_error(
                "extension bits set, but no extension was negotiated",
            ));
        }
        let fin = head[0] & 0x80 != 0;
        let opcode = head[0] & 0x0f;
        if head[1] & 0x80 == 0 {
            return Err(protocol_error("unmasked client frame (RFC 6455 §5.1)"));
        }
        let len = match head[1] & 0x7f {
            126 => {
                let mut extended = [0u8; 2];
                read.read_exact(&mut extended)?;
                u16::from_be_bytes(extended) as usize
            }
            127 => {
                let mut extended = [0u8; 8];
                read.read_exact(&mut extended)?;
                let len = u64::from_be_bytes(extended);
                if len > super::wire::MAX_FRAME as u64 {
                    return Err(protocol_error(
                        "a frame exceeded the protocol's length limit",
                    ));
                }
                len as usize
            }
            len => len as usize,
        };
        if opcode >= 0x8 && (len > 125 || !fin) {
            return Err(protocol_error("a control frame is fragmented or too large"));
        }
        let mut mask = [0u8; 4];
        read.read_exact(&mut mask)?;
        let mut payload = vec![0u8; len];
        read.read_exact(&mut payload)?;
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
        Ok(Frame {
            fin,
            opcode,
            payload,
        })
    }

    /// Writes one server frame — never masked (RFC 6455 §5.1); messages go out whole
    fn write_frame<W: Write>(mut write: W, opcode: u8, payload: &[u8]) -> io::Result<()> {
        let mut frame = Vec::with_capacity(payload.len() + 10);
        frame.push(0x80 | opcode);
        match payload.len() {
            0..=125 => frame.push(payload.len() as u8),
            126..=0xFFFF => {
                frame.push(126);
                frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
            }
            _ => {
                frame.push(127);
                frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
            }
        }
        frame.extend_from_slice(payload);
        write.write_all(&frame)
    }

    /// One message off the wire: the only kinds this protocol carries
    #[derive(Debug, PartialEq, Eq)]
    pub(crate) enum Message {
        Binary(Vec<u8>),
        /// The peer closed, with its status code (if it sent one)
        Close(Option<u16>),
    }

    /// The write half of a websocket connection: whole binary messages, one close frame
    #[derive(Clone)]
    pub(crate) struct WsWriter {
        write: Arc<Mutex<TcpStream>>,
    }

    impl WsWriter {
        /// Any frame out (binary messages, pong echoes)
        fn write_frame_as(&self, opcode: u8, payload: &[u8]) -> io::Result<()> {
            let mut write = self.write.lock().unwrap();
            write_frame(&mut *write, opcode, payload)
        }

        /// A whole binary message
        pub(crate) fn write_binary(&self, bytes: &[u8]) -> io::Result<()> {
            self.write_frame_as(OP_BINARY, bytes)
        }

        /// A close frame with a status code. Best effort: callers are closing anyway.
        pub(crate) fn write_close(&self, code: u16) {
            let _ = self.write_frame_as(OP_CLOSE, &code.to_be_bytes());
        }

        /// Ends the connection in both directions
        pub(crate) fn shutdown(&self) {
            let _ = self
                .write
                .lock()
                .unwrap()
                .shutdown(std::net::Shutdown::Both);
        }
    }

    /// The read half of a websocket connection
    pub(crate) struct WsReader {
        read: TcpStream,
        /// The write half, shared for pong and close echoes from the reading thread
        write: WsWriter,
    }

    impl WsReader {
        /// The next message. Pings are answered in place through the shared write half, pongs
        /// are ignored, and fragmented binary messages are joined. Anything else that is not a
        /// binary message or a close is a protocol error; the caller ends the connection.
        pub(crate) fn read_message(&mut self) -> io::Result<Message> {
            let mut fragments: Option<Vec<u8>> = None;
            loop {
                let frame = read_frame(&mut self.read)?;
                match frame.opcode {
                    OP_BINARY => {
                        if fragments.is_some() {
                            return Err(protocol_error(
                                "a message started before its fragments ended",
                            ));
                        }
                        if frame.fin {
                            return Ok(Message::Binary(frame.payload));
                        }
                        fragments = Some(frame.payload);
                    }
                    OP_CONTINUATION => {
                        let Some(accumulated) = fragments.as_mut() else {
                            return Err(protocol_error("a continuation frame opened no message"));
                        };
                        if accumulated.len() + frame.payload.len() > super::wire::MAX_FRAME {
                            return Err(protocol_error(
                                "a message exceeded the protocol's length limit",
                            ));
                        }
                        accumulated.extend_from_slice(&frame.payload);
                        if frame.fin {
                            return Ok(Message::Binary(fragments.take().unwrap_or_default()));
                        }
                    }
                    OP_PING => self.write.write_frame_as(OP_PONG, &frame.payload)?,
                    OP_PONG => {}
                    OP_CLOSE => {
                        let code = if frame.payload.len() >= 2 {
                            Some(u16::from_be_bytes([frame.payload[0], frame.payload[1]]))
                        } else {
                            None
                        };
                        self.write.write_close(code.unwrap_or(1000));
                        return Ok(Message::Close(code));
                    }
                    OP_TEXT => {
                        return Err(protocol_error(
                            "text frames are not part of this protocol (binary only)",
                        ));
                    }
                    other => {
                        return Err(protocol_error(&format!(
                            "unknown websocket opcode: {other}"
                        )));
                    }
                }
            }
        }
    }

    /// A freshly upgraded websocket: its reader and writer halves (the writer is shared so
    /// the reader thread can answer pings and closes while data streams out)
    pub(crate) struct Ws;

    impl Ws {
        pub(crate) fn from_stream(stream: TcpStream) -> io::Result<(WsReader, WsWriter)> {
            let write = stream.try_clone()?;
            let writer = WsWriter {
                write: Arc::new(Mutex::new(write)),
            };
            let reader = WsReader {
                read: stream,
                write: writer.clone(),
            };
            Ok((reader, writer))
        }
    }
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
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the window attaches over the wire; in-process init stays for the tests"
        )
    )]
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
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the window attaches over the wire; in-process init stays for the tests"
        )
    )]
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

/// The control frame's type byte
pub(crate) const FRAME_CONTROL: u8 = 0;
/// The live-data frame's type byte
pub(crate) const FRAME_DATA: u8 = 1;
/// The attach-snapshot frame's type byte
pub(crate) const FRAME_SNAPSHOT: u8 = 2;

/// One frame read (or ready to write) on the shared connection
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WireFrame {
    /// A control message's JSON bytes (request, reply or event)
    Control(Vec<u8>),
    /// A block's live output
    Data(u32, Vec<u8>),
    /// A block's screen at attach time
    Snapshot(u32, Vec<u8>),
}

use std::io::{self, Read, Write};

/// Reads one typed frame, blocking
pub(crate) fn read_typed_frame(read: &mut dyn Read) -> io::Result<WireFrame> {
    let kind = &mut [0u8; 1];
    read.read_exact(kind)?;
    let length = &mut [0u8; 4];
    read.read_exact(length)?;
    let length = u32::from_le_bytes(*length) as usize;
    if length > wire::MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "a frame exceeded the protocol's length limit",
        ));
    }
    match kind[0] {
        FRAME_CONTROL => {
            let mut json = vec![0u8; length];
            read.read_exact(&mut json)?;
            Ok(WireFrame::Control(json))
        }
        // Data and snapshot frames both carry a block token first, so one length check covers
        // both; the four token bytes are part of the frame
        FRAME_DATA | FRAME_SNAPSHOT => {
            if length < 4 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "a data frame is shorter than its block token",
                ));
            }
            let mut payload = vec![0u8; length];
            read.read_exact(&mut payload)?;
            let token =
                u32::from_le_bytes(payload[..4].try_into().expect("the token was just read"));
            let payload = payload.split_off(4);
            if kind[0] == FRAME_DATA {
                Ok(WireFrame::Data(token, payload))
            } else {
                Ok(WireFrame::Snapshot(token, payload))
            }
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown frame type: {other}"),
        )),
    }
}

/// Frames live output (or a snapshot) of a block
fn data_frame(kind: u8, block: BlockId, bytes: Vec<u8>) -> Vec<u8> {
    let mut frame = Vec::with_capacity(1 + 4 + 4 + bytes.len());
    frame.push(kind);
    frame.extend_from_slice(&((4 + bytes.len()) as u32).to_le_bytes());
    // The token fits u32 for as long as a session can create blocks
    frame.extend_from_slice(&(block.0 as u32).to_le_bytes());
    frame.extend_from_slice(&bytes);
    frame
}

/// Frames a control message for the shared connection
fn control_frame<T: serde::Serialize>(message: &T) -> Vec<u8> {
    let mut frame = vec![FRAME_CONTROL];
    if let Ok(json) = wire::frame_json(message) {
        frame.extend_from_slice(&json);
    }
    frame
}

/// Where the endpoint lives: macOS keeps it under `~/Library/Application Support/terminal`,
/// Linux under `$XDG_RUNTIME_DIR/terminal` (falling back to `~/.cache/terminal`), and Windows
/// names a local named pipe.
#[derive(Debug, Clone)]
pub enum IpcEndpoint {
    #[cfg(unix)]
    Socket(PathBuf),
    #[cfg(windows)]
    Pipe(String),
}

impl IpcEndpoint {
    pub fn resolve() -> Self {
        #[cfg(unix)]
        {
            Self::Socket(socket_dir().join("server.sock"))
        }
        #[cfg(windows)]
        {
            Self::Pipe(r"\\.\pipe\terminal-rs".to_string())
        }
    }
}

#[cfg(unix)]
fn socket_dir() -> PathBuf {
    if cfg!(target_os = "macos") {
        std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join("Library/Application Support/terminal"))
            .unwrap_or_else(|| PathBuf::from("terminal"))
    } else {
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(|dir| PathBuf::from(dir).join("terminal"))
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache/terminal"))
            })
            .unwrap_or_else(|| PathBuf::from("terminal"))
    }
}

/// A connected endpoint, in read and write halves so a session's reader and writer threads do
/// not fight over one handle's buffering
pub(crate) enum IpcStream {
    #[cfg(unix)]
    Socket(UnixStream),
    #[cfg(windows)]
    Pipe(Box<dyn Read + Send>, Box<dyn Write + Send>),
}

impl IpcStream {
    /// Splits into independent read and write halves
    pub(crate) fn into_halves(self) -> (Box<dyn Read + Send>, Box<dyn Write + Send>) {
        match self {
            #[cfg(unix)]
            IpcStream::Socket(stream) => {
                let read = stream
                    .try_clone()
                    .expect("could not clone the socket for its reader half");
                (Box::new(read), Box::new(stream))
            }
            #[cfg(windows)]
            IpcStream::Pipe(read, write) => (read, write),
        }
    }
}

/// A listening endpoint: unix sockets on macOS and Linux, named pipes on Windows
pub(crate) enum IpcListener {
    #[cfg(unix)]
    Socket(UnixListener),
    #[cfg(windows)]
    Pipe(windows_pipe::PipeListener),
}

impl IpcListener {
    /// Binds the endpoint. A local stale socket file is probed with a connection first: a live
    /// server answers (reported as already running), a dead one is unlinked and rebound. Windows
    /// named pipes live in the kernel's namespace, so there is nothing to clean up or probe there
    /// beyond the wait a pipe name answers.
    pub(crate) fn bind(endpoint: &IpcEndpoint) -> io::Result<Self> {
        match endpoint {
            #[cfg(unix)]
            IpcEndpoint::Socket(path) => unix_listen(path).map(Self::Socket),
            #[cfg(windows)]
            IpcEndpoint::Pipe(name) => windows_pipe::PipeListener::bind(name).map(Self::Pipe),
        }
    }

    /// Waits for one connection
    pub(crate) fn accept(&self) -> io::Result<IpcStream> {
        match self {
            #[cfg(unix)]
            IpcListener::Socket(listener) => listener
                .accept()
                .map(|(stream, _)| IpcStream::Socket(stream)),
            #[cfg(windows)]
            IpcListener::Pipe(listener) => listener
                .accept()
                .map(|(read, write)| IpcStream::Pipe(Box::new(read), Box::new(write))),
        }
    }
}

#[cfg(unix)]
fn unix_listen(path: &std::path::Path) -> io::Result<UnixListener> {
    use std::os::unix::fs::PermissionsExt;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
    match UnixListener::bind(path) {
        Ok(listener) => {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
            Ok(listener)
        }
        // A socket file exists: a live server answers the probe, a stale one is removed
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => match UnixStream::connect(path) {
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "a terminal server is already running",
            )),
            Err(_) => {
                eprintln!("[server] removing the stale socket at {}", path.display());
                std::fs::remove_file(path)?;
                let listener = UnixListener::bind(path)?;
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
                Ok(listener)
            }
        },
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

#[cfg(windows)]
mod windows_pipe {
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
    mod tests {
        use super::*;
        use std::sync::mpsc::{Receiver, channel};
        use std::thread;
        use std::time::{Duration, Instant};

        const LIMIT: Duration = Duration::from_secs(10);

        /// A pipe name no other test (or server) uses
        fn unique_name(tag: &str) -> String {
            format!(r"\\.\pipe\terminal-rs-test-{}-{tag}", std::process::id())
        }

        /// Connects to `name` once the accepting thread has created its instance
        fn connect_when_listening(name: &str) -> (PipeReader, PipeWriter) {
            let deadline = Instant::now() + LIMIT;
            loop {
                match connect_client(name) {
                    Ok(halves) => return halves,
                    Err(error) if Instant::now() >= deadline => {
                        panic!("the client could not connect: {error}")
                    }
                    Err(_) => thread::sleep(Duration::from_millis(20)),
                }
            }
        }

        /// A connected pair: the client's halves and the server's
        fn connected_pair(tag: &str) -> ((PipeReader, PipeWriter), (PipeReader, PipeWriter)) {
            let name = unique_name(tag);
            let listener = PipeListener::bind(&name).expect("the name is free");
            let server = thread::spawn(move || listener.accept().expect("a client connects"));
            let client = connect_when_listening(&name);
            (client, server.join().expect("the accept thread ends"))
        }

        /// Reads exactly `len` bytes on its own thread, so a deadlock fails the test instead of
        /// hanging it
        fn read_in_background(mut read: PipeReader, len: usize) -> Receiver<Vec<u8>> {
            let (tx, rx) = channel();
            thread::spawn(move || {
                let mut bytes = vec![0u8; len];
                if read.read_exact(&mut bytes).is_ok() {
                    let _ = tx.send(bytes);
                }
            });
            rx
        }

        #[test]
        fn a_parked_read_does_not_hold_back_a_write_on_the_same_end() {
            let ((client_read, mut client_write), (mut server_read, mut server_write)) =
                connected_pair("duplex");
            // The session's reader parks on the server end, waiting for the next request...
            let parked = thread::spawn(move || {
                let mut request = [0u8; 4];
                server_read.read_exact(&mut request).map(|()| request)
            });
            thread::sleep(Duration::from_millis(200));
            // ...and the server must still answer, or the client's ask would time out
            let reply = read_in_background(client_read, 5);
            thread::spawn(move || {
                server_write
                    .write_all(b"reply")
                    .expect("the reply is written");
                server_write.flush().expect("the reply is flushed");
                server_write
            });
            assert_eq!(
                reply.recv_timeout(LIMIT).expect("the reply arrives"),
                b"reply"
            );
            // The same holds for the client's own end, which parks in its reader loop
            client_write
                .write_all(b"ping")
                .expect("the request is written");
            client_write.flush().expect("the request is flushed");
            assert_eq!(
                parked
                    .join()
                    .expect("the parked read ends")
                    .expect("it reads the request"),
                *b"ping"
            );
        }

        #[test]
        fn a_closed_peer_ends_the_pending_read_instead_of_hanging_it() {
            let (client, (mut server_read, _server_write)) = connected_pair("closed");
            let parked = thread::spawn(move || {
                let mut byte = [0u8; 1];
                server_read.read(&mut byte)
            });
            thread::sleep(Duration::from_millis(200));
            drop(client);
            assert!(
                parked.join().expect("the parked read ends").is_err(),
                "a broken pipe is reported as an error"
            );
        }

        #[test]
        fn a_second_client_connects_while_the_first_stays_attached() {
            let name = unique_name("two");
            let listener = PipeListener::bind(&name).expect("the name is free");
            let (accepted_tx, accepted_rx) = channel();
            thread::spawn(move || {
                // Like the accept loop: an instance per connection, one after another
                for _ in 0..2 {
                    accepted_tx
                        .send(listener.accept().expect("a client connects"))
                        .expect("the test still listens");
                }
            });
            let first = connect_when_listening(&name);
            let second = connect_when_listening(&name);
            let _first_server = accepted_rx
                .recv_timeout(LIMIT)
                .expect("the first is accepted");
            let (_, mut second_server_write) = accepted_rx
                .recv_timeout(LIMIT)
                .expect("the second is accepted");
            second_server_write
                .write_all(b"hi")
                .expect("the second client is served");
            assert_eq!(
                read_in_background(second.0, 2)
                    .recv_timeout(LIMIT)
                    .expect("the second client hears the server"),
                b"hi"
            );
            drop(first);
        }
    }
}

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

// ---- The client side of the server process --------------------------------------------------
//
// The window connects to the endpoint (or starts `terminal server` first; that part lives in
// `main`), sends one `Attach` and then drives its panes purely locally: each pane gets an
// [`IpcBlock`], whose queue the connection's reader thread feeds with that block's frames.

/// What the reader thread (the window's end of the connection) forwards out of the control
/// channel: replies correlated by rid, and uncorrelated ones (events)
#[derive(Debug)]
pub(crate) enum ControlDown {
    Reply(WireReply),
    Event(WireEventKind),
}

/// The client-side half of a block over the shared connection: everything the reader thread
/// already received for it (its attach snapshot and its queued chunks) plus the wake state
pub struct BlockFeed {
    queue: Mutex<VecDeque<Vec<u8>>>,
    snapshot: Mutex<Option<Vec<u8>>>,
    wake_pending: Arc<AtomicBool>,
    notify: Notify,
}

/// A chunk queued for a block before its pane registered it (the pane is built right after the
/// attach replies; output does not wait for it). Order per block is kept.
enum QueuedChunk {
    Data(Vec<u8>),
    Snapshot(Vec<u8>),
}

struct ClientShared {
    write: Mutex<ConnWrite>,
    next_rid: AtomicU64,
    pending: Mutex<HashMap<u64, Sender<WireReply>>>,
    feeds: Mutex<HashMap<BlockId, Arc<BlockFeed>>>,
    /// Chunks that arrived for blocks no pane claimed yet
    orphan: Mutex<HashMap<BlockId, Vec<QueuedChunk>>>,
}

/// The write half of the connection, concrete enough to be shut down from another thread: the
/// reader thread keeps its own half for as long as it lives, and on a unix socket a `Shutdown`
/// from the writer side ends both directions
enum ConnWrite {
    #[cfg(unix)]
    Socket(UnixStream),
    #[cfg(windows)]
    Pipe(Box<dyn Write + Send>),
}

impl ConnWrite {
    fn send_frame(&mut self, frame: &[u8]) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Self::Socket(stream) => stream.write_all(frame).and_then(|()| stream.flush()),
            #[cfg(windows)]
            Self::Pipe(pipe) => pipe.write_all(frame).and_then(|()| pipe.flush()),
        }
    }

    /// Ends the connection: the session's reader gets an end-of-file and the reader thread of
    /// this very connection stops with it (a unix `Shutdown` from the writer side ends both
    /// directions). Windows pipes only end with the client process (the session's asks time
    /// out meanwhile).
    fn shutdown(&self) {
        match self {
            #[cfg(unix)]
            Self::Socket(stream) => {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
            #[cfg(windows)]
            Self::Pipe(_) => {}
        }
    }
}

impl Drop for ClientShared {
    fn drop(&mut self) {
        self.write.lock().unwrap().shutdown();
    }
}

/// The window's connection to the server process: it sends control frames and feeds panes
#[derive(Clone)]
pub struct IpcConn {
    shared: Arc<ClientShared>,
}

/// What an attach answered
#[derive(Debug)]
pub enum AttachAnswer {
    /// The server was fresh: it ran init, and the created pane's block travels by token
    Fresh {
        state: LayoutState,
        pane: wire::IpcCreatedPane,
    },
    /// The server already had a layout; the client rebuilds every pane from the answer.
    /// `exited` names the panes whose program exited while this window was detached (or whose
    /// exit another session observed): the client marks them dead, and the server clears the
    /// list once it is delivered
    Existing {
        state: LayoutState,
        panes: Vec<wire::IpcCreatedPane>,
        exited: Vec<PaneId>,
    },
}

/// How long an ask (attach, foreground dir, snapshot) may take. An attach can spawn a shell; the
/// rest are quick
const ASK_TIMEOUT: Duration = Duration::from_secs(30);

impl IpcConn {
    /// Connects. The read half is returned for the connection's reader thread, which the caller
    /// spawns (it needs the event loop's wakeups, which this crate knows nothing about)
    pub fn connect(endpoint: &IpcEndpoint) -> io::Result<(Self, Box<dyn Read + Send>)> {
        #[cfg(unix)]
        let (read, write) = match endpoint {
            IpcEndpoint::Socket(path) => {
                let stream = UnixStream::connect(path)?;
                // Both halves of one socket: the writer half stays with the connection (so it can
                // be shut down), the reader half goes to the caller's reader thread
                let read = stream
                    .try_clone()
                    .expect("could not clone the socket for its reader half");
                (
                    Box::new(read) as Box<dyn Read + Send>,
                    ConnWrite::Socket(stream),
                )
            }
        };
        #[cfg(windows)]
        let (read, write) = match endpoint {
            IpcEndpoint::Pipe(name) => {
                let (read, write) = windows_pipe::connect_client(name)?;
                (
                    Box::new(read) as Box<dyn Read + Send>,
                    ConnWrite::Pipe(Box::new(write)),
                )
            }
        };
        let conn = Self {
            shared: Arc::new(ClientShared {
                write: Mutex::new(write),
                next_rid: AtomicU64::new(0),
                pending: Mutex::new(HashMap::new()),
                feeds: Mutex::new(HashMap::new()),
                orphan: Mutex::new(HashMap::new()),
            }),
        };
        Ok((conn, read))
    }

    /// Spawns no thread itself; the caller does, after wiring the event proxy:
    /// ```ignore
    /// let (conn, read) = IpcConn::connect(&endpoint)?;
    /// thread::spawn(move || conn.read_loop(read, |down| { ... }));
    /// ```
    /// Frames arrive in order on one socket: a control frame is a reply (delivered to the ask
    /// that waits for it, or forwarded uncorrelated) or an event; a data or snapshot frame is
    /// queued for its block and wakes the pane.
    pub fn read_loop(
        &self,
        mut read: Box<dyn Read + Send>,
        mut on: impl FnMut(ControlDown) + Send,
    ) {
        loop {
            let frame = match read_typed_frame(&mut *read) {
                Ok(frame) => frame,
                Err(_) => return, // the server (or the session) is gone
            };
            match frame {
                WireFrame::Control(json) => match serde_json::from_slice::<DownControl>(&json) {
                    Ok(DownControl::Reply(reply)) => {
                        let sender = self.shared.pending.lock().unwrap().remove(&reply.rid);
                        match sender {
                            Some(sender) => {
                                let _ = sender.send(reply);
                            }
                            None => on(ControlDown::Reply(reply)),
                        }
                    }
                    Ok(DownControl::Event(event)) => {
                        if event.v != WIRE_VERSION {
                            eprintln!(
                                "[client] the server speaks protocol version {}; dropping it",
                                event.v
                            );
                            return;
                        }
                        on(ControlDown::Event(event.event));
                    }
                    Err(error) => {
                        eprintln!("[client] a control frame did not decode: {error}");
                        return;
                    }
                },
                WireFrame::Data(token, bytes) => {
                    self.feed_block(token, QueuedChunk::Data(bytes));
                }
                WireFrame::Snapshot(token, bytes) => {
                    self.feed_block(token, QueuedChunk::Snapshot(bytes));
                }
            }
        }
    }

    /// Queues a chunk for its block, or for the orphan shelf when the pane has not claimed it yet
    fn feed_block(&self, token: u32, chunk: QueuedChunk) {
        let token = BlockId(u64::from(token));
        let feed = self.shared.feeds.lock().unwrap().get(&token).cloned();
        if let Some(feed) = feed {
            // Arrival order on the socket already puts the snapshot before any live chunk
            match chunk {
                QueuedChunk::Data(bytes) => {
                    feed.queue.lock().unwrap().push_back(bytes);
                }
                QueuedChunk::Snapshot(bytes) => {
                    *feed.snapshot.lock().unwrap() = Some(bytes);
                }
            }
            if !feed.wake_pending.swap(true, Ordering::AcqRel) {
                (feed.notify)(IoEvent::Output);
            }
            return;
        }
        self.shared
            .orphan
            .lock()
            .unwrap()
            .entry(token)
            .or_default()
            .push(chunk);
    }

    /// Registers a block's feed for the pane built over it. Chunks that arrived for the token
    /// while the pane was being built come across first, snapshot before live data, in order.
    pub fn open_block(&self, token: BlockId, notify: Notify) -> IpcBlock {
        let feed = Arc::new(BlockFeed {
            queue: Mutex::new(VecDeque::new()),
            snapshot: Mutex::new(None),
            wake_pending: Arc::new(AtomicBool::new(false)),
            notify,
        });
        self.shared
            .feeds
            .lock()
            .unwrap()
            .insert(token, feed.clone());
        if let Some(orphans) = self.shared.orphan.lock().unwrap().remove(&token) {
            for chunk in orphans {
                match chunk {
                    QueuedChunk::Data(bytes) => feed.queue.lock().unwrap().push_back(bytes),
                    QueuedChunk::Snapshot(bytes) => *feed.snapshot.lock().unwrap() = Some(bytes),
                }
            }
        }
        IpcBlock {
            token,
            feed,
            conn: self.clone(),
        }
    }

    /// Sends `Attach`: a fresh server answers like init, an existing one hands over its state
    pub fn attach(
        &self,
        shell: &ShellConfig,
        cols: u16,
        rows: u16,
        scrollback: usize,
    ) -> Result<AttachAnswer, String> {
        let reply = self.ask(wire::WireOp::Attach {
            shell: wire::WireShell::from_config(shell),
            cols,
            rows,
            scrollback,
        })?;
        match reply.reply {
            WireReplyKind::Init {
                state,
                created: Some(pane),
            } => Ok(AttachAnswer::Fresh { state, pane }),
            WireReplyKind::Init { created: None, .. } => {
                Err("the server attached without its first pane".to_string())
            }
            WireReplyKind::Attached {
                state,
                panes,
                exited,
            } => Ok(AttachAnswer::Existing {
                state,
                panes,
                exited,
            }),
            WireReplyKind::InitFailed { error } => Err(error),
            other => Err(format!("the attach got an unexpected reply: {other:?}")),
        }
    }

    /// Sends a fire-and-forget op
    pub(crate) fn send_op(&self, op: wire::WireOp) {
        let _ = self.send_wire(&WireRequest {
            v: WIRE_VERSION,
            rid: None,
            op,
        });
    }

    /// The working directory of the block's foreground program (a blocking round trip, asked
    /// under `ask`'s timeout)
    pub(crate) fn foreground_dir(&self, block: BlockId) -> Option<PathBuf> {
        match self.ask(wire::WireOp::ForegroundDir { block }) {
            Ok(WireReply {
                reply: WireReplyKind::ForegroundDir { dir },
                ..
            }) => dir,
            _ => None,
        }
    }

    /// The server's live panes, for the automation CLI (`terminal ls`): pane id, block token,
    /// grid and alive state, sorted by pane
    pub(crate) fn automation_list(&self) -> Result<Vec<wire::AutomationPane>, String> {
        match self.ask(wire::WireOp::AutomationList {}) {
            Ok(WireReply {
                reply: WireReplyKind::AutomationList { panes },
                ..
            }) => Ok(panes),
            Ok(other) => Err(format!("the list got an unexpected reply: {other:?}")),
            Err(error) => Err(error),
        }
    }

    /// Writes bytes straight to a block's PTY writer, fire-and-forget (the attach loop's
    /// stdin forwarding). Deliberately bypasses the driver gating: an automation CLI is an
    /// explicit local act, and gating it behind the window driver would make automation
    /// useless next to a window (S6 design)
    pub(crate) fn automation_send(&self, block: BlockId, bytes: &[u8]) {
        self.send_op(wire::WireOp::Send {
            block,
            bytes: bytes.to_vec(),
        });
    }

    /// One checked send: the server confirms the block exists and its program is alive
    pub(crate) fn automation_send_checked(
        &self,
        block: BlockId,
        bytes: &[u8],
    ) -> Result<(), String> {
        match self.ask(wire::WireOp::Send {
            block,
            bytes: bytes.to_vec(),
        }) {
            Ok(WireReply {
                reply: WireReplyKind::AutomationSent {},
                ..
            }) => Ok(()),
            Ok(WireReply {
                reply: WireReplyKind::AutomationFailed { error },
                ..
            }) => Err(error),
            Ok(other) => Err(format!("the send got an unexpected reply: {other:?}")),
            Err(error) => Err(error),
        }
    }

    /// The block's screen as plain text, for `terminal capture`
    pub(crate) fn automation_capture(&self, block: BlockId) -> Result<String, String> {
        match self.ask(wire::WireOp::Capture { block }) {
            Ok(WireReply {
                reply: WireReplyKind::AutomationCapture { text },
                ..
            }) => Ok(text),
            Ok(WireReply {
                reply: WireReplyKind::AutomationFailed { error },
                ..
            }) => Err(error),
            Ok(other) => Err(format!("the capture got an unexpected reply: {other:?}")),
            Err(error) => Err(error),
        }
    }

    /// The block's raw VT snapshot, for `terminal capture --vt`
    pub(crate) fn snapshot(&self, block: BlockId) -> Result<Vec<u8>, String> {
        match self.ask(wire::WireOp::Snapshot { block }) {
            Ok(WireReply {
                reply: WireReplyKind::Snapshot { bytes },
                ..
            }) => Ok(bytes),
            Ok(other) => Err(format!("the snapshot got an unexpected reply: {other:?}")),
            Err(error) => Err(error),
        }
    }

    /// Subscribes the block's data channel for `terminal attach`: the snapshot is queued
    /// first, then the connection receives live bytes (the session starts receiving)
    pub(crate) fn automation_subscribe(&self, block: BlockId) -> Result<(), String> {
        match self.ask(wire::WireOp::AutomationSubscribe { block }) {
            Ok(WireReply {
                reply: WireReplyKind::AutomationSubscribed {},
                ..
            }) => Ok(()),
            Ok(WireReply {
                reply: WireReplyKind::AutomationFailed { error },
                ..
            }) => Err(error),
            Ok(other) => Err(format!("the subscribe got an unexpected reply: {other:?}")),
            Err(error) => Err(error),
        }
    }

    /// One correlated ask: registers the reply slot, sends, and waits for the reply
    fn ask(&self, op: wire::WireOp) -> Result<WireReply, String> {
        let rid = self.shared.next_rid.fetch_add(1, Ordering::Relaxed) + 1;
        let (reply, answer) = channel();
        self.shared.pending.lock().unwrap().insert(rid, reply);
        let request = WireRequest {
            v: WIRE_VERSION,
            rid: Some(rid),
            op,
        };
        if let Err(error) = self.send_wire(&request) {
            self.shared.pending.lock().unwrap().remove(&rid);
            return Err(error);
        }
        match answer.recv_timeout(ASK_TIMEOUT) {
            Ok(reply) => Ok(reply),
            Err(_) => {
                self.shared.pending.lock().unwrap().remove(&rid);
                Err("the server did not answer".to_string())
            }
        }
    }

    fn send_wire(&self, request: &WireRequest) -> Result<(), String> {
        let json = wire::frame_json(request).map_err(|error| error.to_string())?;
        let mut frame = vec![FRAME_CONTROL];
        frame.extend_from_slice(&json);
        self.shared
            .write
            .lock()
            .unwrap()
            .send_frame(&frame)
            .map_err(|error| error.to_string())
    }

    /// Ends the connection now: the server's session sees an end-of-file and frees itself (and
    /// the blocks live on headless). The window process's exit does the same for every fd; the
    /// only caller today is the unix-socket round-trip test, so on Windows this is dead code.
    #[allow(dead_code)]
    pub fn close(&self) {
        self.shared.write.lock().unwrap().shutdown();
    }
}

/// What the window sent down along serde's untagged envelope: a reply or an event
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum DownControl {
    Reply(WireReply),
    Event(WireEvent),
}

/// A pane's program over the wire: writes and resize/kill go out as requests; reads pop the
/// queue the connection's reader thread is feeding; the attach snapshot is served first, exactly
/// once. There is no local backpressure: the local queue takes everything the session streams
/// (an unbounded queue here is what lets the PTY's own backpressure carry through the socket).
pub struct IpcBlock {
    token: BlockId,
    feed: Arc<BlockFeed>,
    conn: IpcConn,
}

impl PaneIo for IpcBlock {
    fn write(&mut self, bytes: &[u8]) {
        self.conn.send_op(wire::WireOp::Write {
            block: self.token,
            bytes: bytes.to_vec(),
        });
    }

    fn try_read(&mut self) -> Option<Vec<u8>> {
        if let Some(snapshot) = self.feed.snapshot.lock().unwrap().take() {
            return Some(snapshot);
        }
        self.feed.queue.lock().unwrap().pop_front()
    }

    fn rearm(&self) {
        self.feed.wake_pending.store(false, Ordering::Release);
    }

    fn notify_again(&self) {
        self.feed.wake_pending.store(true, Ordering::Release);
        (self.feed.notify)(IoEvent::Output);
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.conn.send_op(wire::WireOp::ResizeBlock {
            block: self.token,
            cols,
            rows,
        });
    }

    fn foreground_dir(&self) -> Option<PathBuf> {
        self.conn.foreground_dir(self.token)
    }

    fn kill(&mut self) {
        self.conn.send_op(wire::WireOp::Kill { block: self.token });
    }
}

impl Drop for IpcBlock {
    fn drop(&mut self) {
        // The pane is gone; the reader thread stops feeding it. This never closes the block:
        // closing a pane goes through `Close` ops, and exit is an event
        self.shared_feeds().remove(&self.token);
    }
}

impl IpcBlock {
    fn shared_feeds(&self) -> std::sync::MutexGuard<'_, HashMap<BlockId, Arc<BlockFeed>>> {
        self.conn.shared.feeds.lock().unwrap()
    }
}

/// The window's op sender, shaped like the in-process [`ServerHandle`] so call sites that only
/// queue layout ops (the mouse driver) keep building the same [`Request`]s they always have:
/// they just travel over the wire instead of an in-process channel.
#[derive(Clone)]
pub struct OpSender {
    conn: IpcConn,
}

impl OpSender {
    pub(crate) fn new(conn: IpcConn) -> Self {
        Self { conn }
    }

    /// Queues an op for the server thread; errors are ignored (the server may be gone)
    pub(crate) fn send(&self, request: Request) {
        if let Some(op) = wire::WireOp::from_request(&request) {
            self.conn.send_op(op);
        }
    }
}

/// A pane's program, running in the server
pub struct ServerBlock {
    requests: Sender<Request>,
    id: BlockId,
    output: Receiver<Vec<u8>>,
    wake_pending: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    notify: Notify,
}

impl ServerBlock {
    /// Asks the server for an answer and waits for it; `None` if the server is gone. The one
    /// ask boundary of a block: stage 3 keeps the same shape over IPC, with the answer
    /// correlated by rid.
    fn ask<T>(&self, request: impl FnOnce(Sender<T>) -> Request) -> Option<T> {
        let (reply, answer) = channel();
        self.requests.send(request(reply)).ok()?;
        answer.recv().ok()
    }

    /// The block's screen as VT sequences (see [`snapshot`])
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "sessions ask the server thread directly; the block view's ask stays for tests"
        )
    )]
    pub fn snapshot(&self) -> Vec<u8> {
        self.ask(|reply| Request::Snapshot(self.id, reply))
            .unwrap_or_default()
    }

    /// The wire token of the block: how sessions and data frames name it
    pub(crate) fn block_id(&self) -> BlockId {
        self.id
    }
}

impl PaneIo for ServerBlock {
    fn write(&mut self, bytes: &[u8]) {
        let _ = self.requests.send(Request::Write(self.id, bytes.to_vec()));
    }

    fn try_read(&mut self) -> Option<Vec<u8>> {
        let chunk = self.output.try_recv().ok()?;
        // There is room now: the server resumes the paused block
        if self.paused.swap(false, Ordering::SeqCst) {
            let _ = self.requests.send(Request::Drain(self.id));
        }
        Some(chunk)
    }

    fn rearm(&self) {
        self.wake_pending.store(false, Ordering::Release);
    }

    fn notify_again(&self) {
        self.wake_pending.store(true, Ordering::Release);
        (self.notify)(IoEvent::Output);
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        let _ = self.requests.send(Request::Resize(self.id, cols, rows));
    }

    fn foreground_dir(&self) -> Option<PathBuf> {
        self.ask(|reply| Request::ForegroundDir(self.id, reply))
            .flatten()
    }

    fn kill(&mut self) {
        let _ = self.requests.send(Request::Kill(self.id));
    }
}

// There is no close-on-drop: sessions hand their block views back to the server process when
// they end (the shells must outlive the window), so dropping a view is never a close. In-process
// closing goes through `Request::Close` / `LayoutClose` explicitly.

// ---- The WebSocket gateway for remote clients (stage 5a) ------------------------------------
//
// `terminal gateway` fronts the local server for ONE remote client at a time over a loopback
// HTTP port, which real deployments expose with `tailscale serve` (TLS and the identity
// headers come from tailscale). The remote protocol is the existing wire over WebSocket:
// binary messages with the 1-byte type prefix (0 control JSON, 1 data, 2 data-snapshot) and
// identical payloads. The gateway itself is a transport bridge, not a window: its connection
// to the local server is a regular S4 session, so the registry, driver policy and event fan-out
// serve a remote client unchanged (attach → the remote client drives, like any window).

/// The default loopback port `terminal gateway` serves on. Real deployments front it with
/// `tailscale serve`, which proxies a tailnet HTTPS endpoint onto this port.
pub const DEFAULT_GATEWAY_PORT: u16 = 42061;

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
mod tests {
    use super::*;
    use crate::pane_io::IoEvent;
    use libghostty_vt::fmt::{Format, Formatter, FormatterOptions};
    use libghostty_vt::{Terminal, TerminalOptions};
    use std::sync::Arc;
    use std::sync::mpsc::{Receiver, channel};
    use std::time::{Duration, Instant};

    fn shell(program: &str, args: &[&str]) -> ShellConfig {
        ShellConfig {
            program: Some(program.to_string()),
            args: args.iter().map(|arg| arg.to_string()).collect(),
            integration: false,
            ..ShellConfig::default()
        }
    }

    /// A program that runs `script` (a `sh -c` script, or a `cmd /C` line on Windows)
    fn running(unix: &str, windows: &str) -> ShellConfig {
        if cfg!(windows) {
            shell("cmd", &["/C", windows])
        } else {
            shell("sh", &["-c", unix])
        }
    }

    fn printing(text: &str) -> ShellConfig {
        running(&format!("echo {text}"), &format!("echo {text}"))
    }

    fn waiting() -> ShellConfig {
        running("sleep 30", "ping -n 30 127.0.0.1 >NUL")
    }

    fn events() -> (Notify, Receiver<IoEvent>) {
        let (tx, rx) = channel();
        let tx = std::sync::Mutex::new(tx);
        let notify: Notify = Arc::new(move |event| {
            let _ = tx.lock().unwrap().send(event);
        });
        (notify, rx)
    }

    fn create(
        server: &ServerHandle,
        shell: &ShellConfig,
        explicit: bool,
    ) -> Result<(ServerBlock, Option<String>, Receiver<IoEvent>), String> {
        let (notify, rx) = events();
        let (block, notice) = server.create_block(shell, explicit, 80, 24, 1000, notify)?;
        Ok((block, notice, rx))
    }

    /// Reads output (raw bytes) until `pending` returns false or 10 s pass, answering ConPTY's
    /// cursor position query as libghostty does in the app
    fn read_until(io: &mut ServerBlock, mut pending: impl FnMut(&str) -> bool) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut output = Vec::new();
        while pending(&String::from_utf8_lossy(&output)) && Instant::now() < deadline {
            io.rearm();
            match io.try_read() {
                Some(chunk) => {
                    if chunk.windows(4).any(|query| query == b"\x1b[6n") {
                        io.write(b"\x1b[1;1R");
                    }
                    output.extend(chunk);
                }
                None => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        output
    }

    fn wait_for(rx: &Receiver<IoEvent>, wanted: IoEvent) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(event) if event == wanted => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
        false
    }

    /// Waits for the next layout event, skipping pane output and exit notices
    fn wait_layout(events: &Receiver<LayoutEvent>) -> LayoutEvent {
        let deadline = Instant::now() + Duration::from_secs(10);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match events.recv_timeout(left) {
                Ok(LayoutEvent::PaneOutput(_) | LayoutEvent::PaneExited(_)) => continue,
                Ok(event) => return event,
                Err(_) => break,
            }
        }
        panic!("no layout event arrived");
    }

    /// Waits for a state event and splits it into its parts
    fn wait_state(events: &Receiver<LayoutEvent>) -> (LayoutState, Option<CreatedPane>) {
        match wait_layout(events) {
            LayoutEvent::State(state, created) => (state, created),
            other => panic!("expected a state event, got {other:?}"),
        }
    }

    /// A split of the first pane at `area`, starting `shell` in the new pane
    fn split(server: &ServerHandle, pane: PaneId, shell: ShellConfig, area: Rect) {
        server.send(Request::LayoutSplit(SplitOp {
            pane,
            dir: Dir::Right,
            shell,
            explicit: false,
            cols: 40,
            rows: 20,
            scrollback: 1000,
            area,
            limits: Limits {
                min_w: 80,
                min_h: 60,
            },
        }));
    }

    fn init(server: &ServerHandle) -> (LayoutState, CreatedPane) {
        server
            .init(&waiting(), 80, 24, 1000)
            .expect("the first pane starts")
    }

    fn plain_text(terminal: &Terminal<'static, 'static>) -> String {
        let options = FormatterOptions::new().with_format(Format::Plain);
        let mut formatter = Formatter::new(terminal, options).unwrap();
        String::from_utf8_lossy(&formatter.format_alloc(None).unwrap()).into_owned()
    }

    fn terminal() -> Terminal<'static, 'static> {
        Terminal::new(TerminalOptions {
            cols: 80,
            rows: 24,
            max_scrollback: 1000,
        })
        .unwrap()
    }

    #[test]
    fn a_split_creates_the_block_and_reports_it() {
        let (server, events) = Server::start();
        let (state, first) = init(&server);
        assert_eq!(first.pane, PaneId(0), "the server allocated the pane id");
        assert_eq!(state.focus, PaneId(0));
        assert_eq!(first.notice, None);
        assert_eq!((first.cols, first.rows), (80, 24));

        let program = running(
            "echo split-marker; sleep 30",
            "echo split-marker & ping -n 30 127.0.0.1 >NUL",
        );
        split(&server, PaneId(0), program, Rect::new(0, 0, 800, 600));
        let (state, created) = wait_state(&events);
        let created = created.expect("the split reports its new pane");
        assert_eq!(created.pane, PaneId(1), "the server allocated the next id");
        assert_eq!(created.notice, None);

        let projection = Layout::from_state(state);
        assert_eq!(projection.tiled_ids(), vec![PaneId(0), PaneId(1)]);
        assert_eq!(projection.focus(), PaneId(1), "the new pane gets the focus");

        // The block behind the new pane really runs: its output reaches the window's block
        let mut block = created.block;
        let output = read_until(&mut block, |text| !text.contains("split-marker"));
        assert!(
            String::from_utf8_lossy(&output).contains("split-marker"),
            "{output:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn closing_a_pane_kills_its_block() {
        let (server, events) = Server::start();
        init(&server);
        let program = running("echo $$; exec sleep 30", "unused on unix");
        split(&server, PaneId(0), program, Rect::new(0, 0, 800, 600));
        let (_, created) = wait_state(&events);
        let mut block = created.expect("the split reports its pane").block;
        let output = read_until(&mut block, |text| !text.contains('\n'));
        let pid: i32 = String::from_utf8_lossy(&output)
            .trim()
            .parse()
            .expect("the shell printed its pid");

        server.send(Request::LayoutClose { pane: PaneId(1) });
        let (state, created) = wait_state(&events);
        assert!(created.is_none(), "closing reports no new pane");
        assert_eq!(Layout::from_state(state).tiled_ids(), vec![PaneId(0)]);

        // The program behind the closed pane is really dead
        let alive = || {
            std::process::Command::new("kill")
                .arg("-0")
                .arg(pid.to_string())
                .status()
                .map(|status| status.success())
                .unwrap_or(true)
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while alive() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
        assert!(!alive(), "the block's program was killed");
    }

    #[test]
    fn closing_the_last_pane_emits_last_pane_closed() {
        let (server, events) = Server::start();
        let (_state, first) = init(&server);
        assert_eq!(first.pane, PaneId(0));
        server.send(Request::LayoutClose { pane: PaneId(0) });
        match wait_layout(&events) {
            LayoutEvent::LastPaneClosed => {}
            other => panic!("expected LastPaneClosed, got {other:?}"),
        }
    }

    #[test]
    fn a_rolled_back_init_burns_the_pane_id() {
        let (server, _events) = Server::start();
        // A 0×0 grid cannot start a block: the init rolls back
        assert!(
            server.init(&waiting(), 0, 0, 1000).is_err(),
            "a 0×0 grid does not start a block"
        );
        // The rolled-back id is burned, never handed out again (advisory A-T2b-2)
        let (state, created) = init(&server);
        assert_ne!(
            created.pane,
            PaneId(0),
            "the id was reused after the rollback"
        );
        assert_eq!(state.focus, created.pane);
        assert_eq!(Layout::from_state(state).tiled_ids(), vec![created.pane]);
    }

    #[test]
    fn a_failed_spawn_rolls_the_layout_back_and_reports_an_error() {
        let (server, events) = Server::start();
        init(&server);
        // An explicit program that cannot start is an error, like a floating pane's own program
        server.send(Request::LayoutSplit(SplitOp {
            pane: PaneId(0),
            dir: Dir::Right,
            shell: shell("no-such-program-layout", &[]),
            explicit: true,
            cols: 40,
            rows: 20,
            scrollback: 1000,
            area: Rect::new(0, 0, 800, 600),
            limits: Limits {
                min_w: 80,
                min_h: 60,
            },
        }));
        match wait_layout(&events) {
            LayoutEvent::Error(error) => assert!(
                error.starts_with("could not start \"no-such-program-layout\": "),
                "{error}"
            ),
            other => panic!("expected an error event, got {other:?}"),
        }

        // The layout rolled back: the following split finds only the original pane, and the
        // rolled-back id is never handed out again
        split(&server, PaneId(0), waiting(), Rect::new(0, 0, 800, 600));
        let (state, created) = wait_state(&events);
        let created = created.expect("the good split reports its pane");
        assert_eq!(created.pane, PaneId(2));
        assert_eq!(
            Layout::from_state(state).tiled_ids(),
            vec![PaneId(0), PaneId(2)]
        );
    }

    #[test]
    fn an_exit_never_precedes_the_state_event_for_a_spawned_pane() {
        let (server, events) = Server::start();
        let _ = init(&server);
        // The program exits immediately: its output and exit are queued while the split is
        // applied, so the event order proves State always announces a pane first (State, then
        // the pane's output, then its exit)
        split(
            &server,
            PaneId(0),
            printing("bye"),
            Rect::new(0, 0, 800, 600),
        );

        // The test plays the client's role for the spawned pane, including answering ConPTY's
        // cursor-position query: an unanswered one keeps the child's teardown from completing on
        // Windows, so the exit would never be reported
        let mut created: Option<CreatedPane> = None;
        let mut state_tick = None;
        let mut output_tick = None;
        let mut exit_tick = None;
        // 10s: a cold Windows CI takes a while to spawn cmd.exe and tear down ConPTY; the
        // assertion only needs the order, not the timing
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut tick = 0usize;
        while exit_tick.is_none() && Instant::now() < deadline {
            match events.recv_timeout(Duration::from_millis(50)) {
                Ok(LayoutEvent::State(_, mut pane)) => match pane.take() {
                    Some(created_pane) => {
                        assert_eq!(created_pane.pane, PaneId(1), "the split creates pane 1");
                        created = Some(created_pane);
                        state_tick = Some(tick);
                    }
                    None => panic!("the split reports its pane"),
                },
                Ok(LayoutEvent::PaneOutput(PaneId(1))) => output_tick = Some(tick),
                Ok(LayoutEvent::PaneExited(PaneId(1))) => exit_tick = Some(tick),
                Ok(_) => {}
                Err(_) => {}
            }
            tick += 1;
            // Drain the pane's output and answer the cursor query, as the app does
            if let Some(block) = created.as_mut().map(|created| &mut created.block) {
                #[cfg(unix)]
                block.rearm();
                while let Some(chunk) = block.try_read() {
                    if chunk.windows(4).any(|query| query == b"\x1b[6n") {
                        block.write(b"\x1b[1;1R");
                    }
                }
            }
        }
        drop(created);

        let state_tick = state_tick.expect("the split reports its pane");
        if let Some(output_tick) = output_tick {
            assert!(output_tick > state_tick, "output arrived before the state");
        }
        let exit_tick = exit_tick.expect("the exit is reported");
        assert!(exit_tick > state_tick, "the exit arrived before the state");
    }

    #[test]
    fn a_closed_twice_pane_stays_silent() {
        let (server, events) = Server::start();
        let _ = init(&server);
        split(&server, PaneId(0), waiting(), Rect::new(0, 0, 800, 600));
        let _ = wait_state(&events);

        server.send(Request::LayoutClose { pane: PaneId(1) });
        let _ = wait_state(&events);
        // Drain whatever the kill still has in flight before the silence check
        while events.try_recv().is_ok() {}

        // Closing an unknown pane again answers nothing
        server.send(Request::LayoutClose { pane: PaneId(1) });
        assert!(
            events.recv_timeout(Duration::from_millis(300)).is_err(),
            "the second close is silent"
        );
    }

    #[test]
    fn pane_ids_are_allocated_by_the_server_and_never_reused() {
        let (server, events) = Server::start();
        let (_state, first) = init(&server);
        assert_eq!(first.pane, PaneId(0));

        for expected in [1u32, 2] {
            split(&server, PaneId(0), waiting(), Rect::new(0, 0, 800, 600));
            let (_, created) = wait_state(&events);
            assert_eq!(
                created.expect("the split reports its pane").pane,
                PaneId(expected)
            );
        }

        server.send(Request::LayoutClose { pane: PaneId(1) });
        let (state, _) = wait_state(&events);
        assert_eq!(
            Layout::from_state(state).tiled_ids(),
            vec![PaneId(0), PaneId(2)]
        );

        // The closed pane's id is not handed out again
        split(&server, PaneId(2), waiting(), Rect::new(0, 0, 800, 600));
        let (state, created) = wait_state(&events);
        assert_eq!(created.expect("the split reports its pane").pane, PaneId(3));
        assert_eq!(
            Layout::from_state(state).tiled_ids(),
            vec![PaneId(0), PaneId(2), PaneId(3)]
        );
    }

    #[test]
    fn a_refused_split_changes_nothing_and_stays_silent() {
        let (server, events) = Server::start();
        init(&server);
        // Both halves would fall below the minimum: the window previews the same check and
        // never sees an event
        split(&server, PaneId(0), waiting(), Rect::new(0, 0, 150, 600));
        let deadline = Instant::now() + Duration::from_millis(300);
        while Instant::now() < deadline {
            match events.try_recv() {
                Ok(
                    LayoutEvent::State(..) | LayoutEvent::LastPaneClosed | LayoutEvent::Error(_),
                ) => {
                    panic!("a refused split must stay silent")
                }
                Ok(_) => {} // pane output noise (ConPTY) is fine
                Err(_) => thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    #[test]
    fn the_program_output_arrives_and_its_exit_is_reported() {
        let (server, _events) = Server::start();
        let (mut block, notice, rx) =
            create(&server, &printing("hello-server"), true).expect("the program starts");
        assert_eq!(notice, None);
        let output = read_until(&mut block, |text| !text.contains("hello-server"));
        assert!(
            String::from_utf8_lossy(&output).contains("hello-server"),
            "{output:?}"
        );
        assert!(wait_for(&rx, IoEvent::Exited), "the exit is reported");
    }

    #[test]
    fn output_wakes_the_client() {
        let (server, _events) = Server::start();
        let (_block, _, rx) = create(&server, &printing("wake"), true).unwrap();
        assert!(wait_for(&rx, IoEvent::Output));
    }

    #[test]
    fn killing_the_program_reports_its_exit() {
        let (server, _events) = Server::start();
        let (mut block, _, rx) = create(&server, &waiting(), true).unwrap();
        block.kill();
        assert!(wait_for(&rx, IoEvent::Exited));
    }

    #[test]
    fn an_explicit_program_that_cannot_start_is_an_error() {
        let (server, _events) = Server::start();
        let error = create(&server, &shell("no-such-program-server", &[]), true)
            .err()
            .expect("it does not start");
        assert!(
            error.starts_with("could not start \"no-such-program-server\": "),
            "{error}"
        );
    }

    #[test]
    fn a_configured_shell_that_cannot_start_falls_back_to_the_default_shell() {
        let (server, _events) = Server::start();
        let (mut block, notice, _rx) =
            create(&server, &shell("no-such-program-server", &[]), false)
                .expect("the default shell starts");
        let notice = notice.expect("a notice explains the fallback");
        assert!(
            notice.starts_with("could not start \"no-such-program-server\": "),
            "{notice}"
        );
        assert!(notice.ends_with("; using the default shell"), "{notice}");
        block.kill();
    }

    #[test]
    fn a_snapshot_keeps_the_cursor_with_origin_mode_and_a_scrolling_region() {
        // DECOM plus a scroll region makes the final CUP relative to the region's origin, so the
        // restored cursor would land somewhere else than where the client saw it
        let mut expected = terminal();
        expected.vt_write(b"hello\x1b[5;20r\x1b[?6h\x1b[3;7Hmark");
        assert!(
            expected
                .mode(libghostty_vt::terminal::Mode::ORIGIN)
                .unwrap(),
            "the repro actually sets origin mode"
        );

        let mut restored = terminal();
        restored.vt_write(&snapshot(&expected));

        assert_eq!(
            (restored.cursor_x().unwrap(), restored.cursor_y().unwrap()),
            (expected.cursor_x().unwrap(), expected.cursor_y().unwrap())
        );
        assert!(
            restored
                .mode(libghostty_vt::terminal::Mode::ORIGIN)
                .unwrap(),
            "origin mode survives the snapshot"
        );
    }

    #[test]
    fn a_snapshot_reproduces_the_screen_and_the_cursor() {
        let (server, _events) = Server::start();
        let program = running(
            "printf 'first line\\n\\tplain \\033[31mred\\033[0m end-of-snapshot'; sleep 30",
            "echo first line & echo plain red end-of-snapshot & ping -n 30 127.0.0.1 >NUL",
        );
        let (mut block, _, _rx) = create(&server, &program, true).unwrap();
        let output = read_until(&mut block, |text| !text.contains("end-of-snapshot"));

        // What the client saw, fed into its own terminal
        let mut expected = terminal();
        expected.vt_write(&output);

        let snapshot = block.snapshot();
        if cfg!(unix) {
            assert!(
                String::from_utf8_lossy(&snapshot).contains("31m")
                    || String::from_utf8_lossy(&snapshot).contains("38;5;1m"),
                "the color is kept: {snapshot:?}"
            );
        }
        let mut restored = terminal();
        restored.vt_write(&snapshot);

        assert_eq!(plain_text(&restored), plain_text(&expected));
        assert!(plain_text(&restored).contains("end-of-snapshot"));
        assert_eq!(
            (restored.cursor_x().unwrap(), restored.cursor_y().unwrap()),
            (expected.cursor_x().unwrap(), expected.cursor_y().unwrap())
        );
        block.kill();
    }

    #[test]
    fn a_client_that_does_not_read_does_not_stall_other_blocks() {
        let (server, _events) = Server::start();
        // Floods output and is never drained. Each line goes back to the top-left corner: the
        // screen never scrolls, which is slow in debug builds of libghostty.
        // windows: PowerShell's console writes only moved ~13KB in 10s through ConPTY, so flood
        // with plain cmd echoes of one 8KB literal: at ~1600 echoes/s, 200 iterations (~1.6MB)
        // finish quickly, and the raw ESC[H before each echo keeps the screen from scrolling.
        let windows_flood = format!(
            "for /L %i in (1,1,200) do @echo \u{1b}[H{}",
            "y".repeat(8000)
        );
        let flood = running("yes \"$(printf 'flood\\033[H')\"", &windows_flood);
        let (mut flooding, _, flood_events) = create(&server, &flood, true).unwrap();
        assert!(wait_for(&flood_events, IoEvent::Output));
        std::thread::sleep(Duration::from_millis(300));

        let (mut quiet, _, _rx) = create(&server, &printing("still-flowing"), true).unwrap();
        let output = read_until(&mut quiet, |text| !text.contains("still-flowing"));
        assert!(
            String::from_utf8_lossy(&output).contains("still-flowing"),
            "{output:?}"
        );

        // The flooding block resumes once its client reads
        let resumed = read_until(&mut flooding, |text| text.len() < 512 * 1024);
        assert!(resumed.len() >= 512 * 1024, "{}", resumed.len());
        flooding.kill();
    }

    /// Reads what is available right now, without waiting for more
    #[cfg(unix)] // only the exit-ordering test below uses it
    fn read_available(io: &mut ServerBlock) -> Vec<u8> {
        let mut output = Vec::new();
        loop {
            io.rearm();
            match io.try_read() {
                Some(chunk) => output.extend(chunk),
                None => break,
            }
        }
        output
    }

    #[test]
    #[cfg(unix)] // Windows floods through ConPTY too slowly for this volume in the test window
    fn the_exit_is_notified_after_all_the_output_was_forwarded() {
        let (server, _events) = Server::start();
        // 30000 fixed-width lines (~360KB, ~88 chunks) overrun the 16-chunks-per-drain cap and
        // the 64-chunk client channel: when the program exits, chunks are still queued, so the
        // exit must wait until every line was forwarded to the client. Each line homes the cursor
        // first: scrolling is slow in debug builds of libghostty.
        let program = running(
            "awk 'BEGIN{for(i=0;i<30000;i++) printf \"\\033[Hline %06d\\n\", i}'",
            "echo unused on unix",
        );
        let (mut block, _, rx) = create(&server, &program, true).unwrap();

        // A client that treats the exit as the end: polls for the exit, and reads a pass only
        // after each wait. The exit must come only when every chunk was already forwarded.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut output = Vec::new();
        loop {
            match rx.recv_timeout(Duration::from_millis(30)) {
                Ok(IoEvent::Exited) => break,
                Ok(_) => {}
                Err(_) if Instant::now() < deadline => {}
                Err(_) => panic!("the exit was never reported"),
            }
            output.extend(read_available(&mut block));
        }
        output.extend(read_available(&mut block));

        // The first and the last line must be there
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("line 000000"), "the first line is kept");
        assert!(
            text.contains("line 029999"),
            "the last line is not lost: got {} bytes",
            text.len()
        );

        // And the exit is the last notification: no output follows it
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            rx.try_recv().is_err(),
            "no output notification follows the exit"
        );
        block.kill();
    }

    // ---- Wire format (stage 3) ------------------------------------------------------------
    // The same ops and events travel over the wire as cross the in-process links today; these
    // tests pin the JSON round trip so stage 3 can reuse the mapping without behavior drift.

    use crate::layout::Axis;
    use serde::{Serialize, de::DeserializeOwned};
    use std::collections::BTreeMap;
    use wire::{
        Deframed, IpcCreatedPane, WIRE_VERSION, WireEvent, WireEventKind, WireOp, WireReply,
        WireReplyKind, WireRequest, WireShell, deframe, deframe_json, frame, frame_json,
    };

    /// Encode, decode, and check nothing changed
    fn round_trip<T>(original: T) -> T
    where
        T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_vec(&original).expect("serializes");
        serde_json::from_slice(&json).expect("deserializes")
    }

    /// Shell config exercising every field
    fn full_shell() -> ShellConfig {
        ShellConfig {
            program: Some("zsh".to_string()),
            args: vec!["-l".to_string()],
            cwd: Some(PathBuf::from("/tmp")),
            env: BTreeMap::from([("TERM".to_string(), "xterm".to_string())]),
            integration: true,
        }
    }

    /// A representative request for each op that travels over the wire
    fn wire_ops() -> Vec<WireOp> {
        let limits = Limits {
            min_w: 80,
            min_h: 60,
        };
        vec![
            WireOp::Init {
                shell: WireShell::from_config(&full_shell()),
                cols: 80,
                rows: 24,
                scrollback: 1000,
            },
            WireOp::Split {
                pane: PaneId(3),
                dir: Dir::Down,
                shell: WireShell::from_config(&full_shell()),
                explicit: true,
                cols: 40,
                rows: 20,
                scrollback: 500,
                area: Rect::new(1, 2, 300, 400),
                limits,
            },
            WireOp::Close { pane: PaneId(1) },
            WireOp::SetFocus { pane: PaneId(2) },
            WireOp::FocusDir {
                dir: Dir::Left,
                area: Rect::new(0, 0, 800, 600),
            },
            WireOp::FocusCycle { forward: false },
            WireOp::Resize {
                pane: PaneId(4),
                dir: Dir::Up,
                delta: 33,
                area: Rect::new(0, 0, 800, 600),
                limits,
            },
            WireOp::SetDivider {
                path: vec![false, true, true],
                boundary: 512,
                area: Rect::new(0, 0, 800, 600),
                limits,
            },
            WireOp::ToggleZoom,
            WireOp::AddFloat {
                shell: WireShell::from_config(&full_shell()),
                explicit: false,
                width: 0.5,
                height: 0.25,
                cols: 60,
                rows: 10,
                scrollback: 0,
            },
            WireOp::ToggleFloats,
            WireOp::MoveFloat {
                pane: PaneId(5),
                dx: -10,
                dy: 24,
                window: Rect::new(0, 0, 800, 600),
            },
            WireOp::ResizeFloatEdge {
                pane: PaneId(6),
                edge: Edge {
                    left: true,
                    right: true,
                    top: false,
                    bottom: true,
                },
                dx: 5,
                dy: -7,
                window: Rect::new(0, 0, 800, 600),
                limits,
            },
            WireOp::Write {
                block: BlockId(9),
                bytes: vec![0x1b, 0xFF, 0x00, b'a'],
            },
            WireOp::ResizeBlock {
                block: BlockId(9),
                cols: 100,
                rows: 40,
            },
            WireOp::Kill { block: BlockId(9) },
            WireOp::CloseBlock { block: BlockId(9) },
            WireOp::ForegroundDir { block: BlockId(2) },
            WireOp::Snapshot { block: BlockId(2) },
        ]
    }

    #[test]
    fn wire_requests_carry_the_same_ops_the_in_process_channel_does() {
        // A real request maps to exactly the wire op a client would have written
        let request = Request::LayoutSplit(SplitOp {
            pane: PaneId(0),
            dir: Dir::Right,
            shell: full_shell(),
            explicit: true,
            cols: 40,
            rows: 20,
            scrollback: 500,
            area: Rect::new(1, 2, 300, 400),
            limits: Limits {
                min_w: 80,
                min_h: 60,
            },
        });
        assert_eq!(
            WireOp::from_request(&request).expect("a layout split travels"),
            WireOp::Split {
                pane: PaneId(0),
                dir: Dir::Right,
                shell: WireShell::from_config(&full_shell()),
                explicit: true,
                cols: 40,
                rows: 20,
                scrollback: 500,
                area: Rect::new(1, 2, 300, 400),
                limits: Limits {
                    min_w: 80,
                    min_h: 60,
                },
            }
        );

        for request in [
            Request::LayoutSetFocus { pane: PaneId(2) },
            Request::LayoutToggleZoom,
            Request::LayoutToggleFloats,
            Request::LayoutFocusCycle { forward: true },
            Request::Write(BlockId(3), vec![0xFF]),
            Request::Resize(BlockId(3), 90, 30),
            Request::Kill(BlockId(3)),
            Request::Close(BlockId(3)),
        ] {
            WireOp::from_request(&request).unwrap_or_else(|| panic!("{request:?} travels"));
        }

        // Server-internal traffic does not travel
        assert!(
            WireOp::from_request(&Request::Shutdown).is_none(),
            "shutdown stays in the process"
        );

        // Every op that travels survives JSON wrapped in the versioned envelope
        for op in wire_ops() {
            let request = WireRequest {
                v: WIRE_VERSION,
                rid: None,
                op,
            };
            let json = serde_json::to_vec(&request).expect("serializes");
            assert!(
                String::from_utf8_lossy(&json).contains(&format!("\"v\":{WIRE_VERSION}")),
                "the protocol version is pinned: {:?}",
                String::from_utf8_lossy(&json)
            );
            assert_eq!(round_trip(request.clone()), request, "{request:?}");
        }
    }

    #[test]
    fn wire_requests_name_their_op_and_a_rid_survives_the_round_trip() {
        let request = WireRequest {
            v: WIRE_VERSION,
            rid: Some(42),
            op: WireOp::Snapshot { block: BlockId(1) },
        };
        let json = serde_json::to_string(&request).expect("serializes");
        assert!(json.starts_with("{\"v\":1"), "{json}");
        assert!(json.contains("\"op\":\"snapshot\""), "{json}");
        assert_eq!(round_trip(request.clone()), request);
    }

    #[test]
    fn the_layout_state_survives_the_wire() {
        // A nested tree with zoom, floats and history: every field of LayoutState
        let mut layout = Layout::new(PaneId(1));
        assert!(layout.split(
            PaneId(1),
            Axis::Horizontal,
            PaneId(2),
            Rect::new(0, 0, 800, 600),
            Limits {
                min_w: 80,
                min_h: 60,
            }
        ));
        assert!(layout.split(
            PaneId(2),
            Axis::Vertical,
            PaneId(3),
            Rect::new(0, 0, 800, 600),
            Limits {
                min_w: 80,
                min_h: 60,
            }
        ));
        assert!(layout.toggle_zoom());
        layout.add_float(PaneId(4), 0.5, 0.5);
        assert!(layout.toggle_floats());
        assert!(layout.set_focus(PaneId(3)));

        let state = layout.state();
        assert_eq!(state.zoomed, Some(PaneId(3)));
        assert_eq!(state.floats.len(), 1);
        assert!(!state.floats_visible);

        let json = serde_json::to_string(&state).expect("serializes");
        assert!(json.contains("\"root\""), "{json}");
        let back: LayoutState = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back, state, "the state round trips field for field");
        // And a projection rebuilt from a decoded state answers exactly like the original
        assert_eq!(Layout::from_state(back).state(), state);
    }

    #[test]
    fn wire_events_map_layout_events_without_the_block() {
        // A created pane carries its block in-process; on the wire only pane, size and notice go
        let (server, _events) = Server::start();
        let (block, notice, _rx) =
            create(&server, &shell("no-such-program-wire", &[]), false).expect("the fallback");
        let created = CreatedPane {
            pane: PaneId(7),
            block,
            cols: 80,
            rows: 24,
            notice,
        };
        let layout = Layout::new(PaneId(7));
        let state = layout.state();

        let event = WireEvent::from_layout_event(&LayoutEvent::State(state.clone(), Some(created)))
            .expect("a state event travels");
        let WireEventKind::State {
            state: wire_state,
            created: wire_created,
        } = &event.event
        else {
            panic!("mapped to a state event: {event:?}");
        };
        assert_eq!(wire_state, &state, "the state travels whole");
        let wire_created = wire_created.as_ref().expect("the created pane travels");
        assert_eq!(wire_created.pane, PaneId(7));
        assert_eq!(wire_created.cols, 80);
        assert_eq!(wire_created.rows, 24);
        let notice = wire_created
            .notice
            .clone()
            .expect("the fallback notice travels");
        assert!(notice.ends_with("; using the default shell"), "{notice}");
        assert_eq!(round_trip(event.clone()), event);
        assert!(
            serde_json::to_string(&event)
                .unwrap()
                .contains("\"event\":\"state\""),
            "events name their kind"
        );

        // A state event with no created pane, and the other kinds, map and round trip too
        for event in [
            WireEvent::from_layout_event(&LayoutEvent::State(layout.state(), None))
                .expect("travels"),
            WireEvent::from_layout_event(&LayoutEvent::LastPaneClosed).expect("travels"),
            WireEvent::from_layout_event(&LayoutEvent::Error("boom".to_string())).expect("travels"),
            WireEvent::from_layout_event(&LayoutEvent::PaneOutput(PaneId(1))).expect("travels"),
            WireEvent::from_layout_event(&LayoutEvent::PaneExited(PaneId(1))).expect("travels"),
        ] {
            assert_eq!(round_trip(event.clone()), event, "{event:?}");
        }
    }

    #[test]
    fn wire_replies_carry_the_state_with_the_pane_token() {
        let (server, _events) = Server::start();
        let (block, notice, _rx) =
            create(&server, &shell("no-such-program-wire-reply", &[]), false)
                .expect("the fallback");
        let created = CreatedPane {
            pane: PaneId(7),
            block,
            cols: 80,
            rows: 24,
            notice,
        };
        // What the wire should carry for this pane: pane, its block token, size and notice;
        // the block view itself still does not travel
        let expected = IpcCreatedPane::from_created(&created);
        let reply = WireReply::init(11, Ok((Layout::new(PaneId(7)).state(), Some(created))));
        assert_eq!(reply.rid, 11);
        assert_eq!(reply.v, WIRE_VERSION);
        let json = serde_json::to_string(&reply).expect("serializes");
        assert!(json.contains("\"reply\":\"init\""), "{json}");
        assert_eq!(round_trip(reply.clone()), reply);
        // The decoded created pane is exactly what the window needs to build its pane
        let WireReplyKind::Init {
            state,
            created: Some(wire_created),
        } = reply.reply
        else {
            panic!("an init reply");
        };
        assert_eq!(state.focus, PaneId(7));
        assert_eq!(wire_created, expected);
        assert_eq!(
            wire_created.block, expected.block,
            "the pane's block token travels: it names the block over the data channel"
        );
        let notice = wire_created.notice.expect("the fallback notice travels");
        assert!(notice.ends_with("; using the default shell"), "{notice}");

        // The failure of an init, the foreground directory and bytes that are not UTF-8 too
        let failed = WireReply::init(12, Err("did not start".to_string()));
        assert_eq!(round_trip(failed.clone()), failed);
        match failed.reply {
            WireReplyKind::InitFailed { error } => assert_eq!(error, "did not start"),
            other => panic!("an init failure: {other:?}"),
        }
        let dir = WireReply::foreground_dir(13, Some(PathBuf::from("/home")));
        assert_eq!(round_trip(dir.clone()), dir);
        let snapshot = WireReply::snapshot(14, vec![0xFF, b'\x1b', 0x00]);
        assert_eq!(round_trip(snapshot.clone()), snapshot);
    }

    #[test]
    fn frames_carry_whole_messages_and_reject_corrupt_lengths() {
        // A frame round trips
        let framed = frame(b"hello wire");
        assert_eq!(&framed[..4], &10u32.to_le_bytes(), "4-byte LE length first");
        match deframe(&framed) {
            Deframed::Message { bytes, consumed } => {
                assert_eq!(bytes, b"hello wire");
                assert_eq!(consumed, framed.len());
            }
            other => panic!("decodes the frame: {other:?}"),
        }

        // Two frames in one buffer decode one at a time
        let two = [framed.clone(), frame(b"{}")].concat();
        let (first, used) = match deframe(&two) {
            Deframed::Message { bytes, consumed } => (bytes, consumed),
            other => panic!("decodes the first: {other:?}"),
        };
        assert_eq!(first, b"hello wire");
        match deframe(&two[used..]) {
            Deframed::Message { bytes, consumed } => {
                assert_eq!(bytes, b"{}");
                assert_eq!(consumed, two.len() - used);
            }
            other => panic!("decodes the second: {other:?}"),
        }

        // Incomplete frames wait for more bytes
        assert_eq!(deframe(&framed[..3]), Deframed::Incomplete);
        assert_eq!(deframe(&framed[..framed.len() - 1]), Deframed::Incomplete);

        // A length over the protocol cap is refused, never waited for
        assert_eq!(deframe(&u32::MAX.to_le_bytes()), Deframed::Invalid);

        // And the JSON helpers compose them
        let request = WireRequest {
            v: WIRE_VERSION,
            rid: Some(1),
            op: WireOp::FocusCycle { forward: true },
        };
        let mut wire = frame_json(&request).expect("serializes");
        wire.extend_from_slice(&frame_json(&request).expect("serializes"));
        let (first, used) = deframe_json::<WireRequest>(&wire)
            .expect("decodes")
            .expect("a whole message");
        assert_eq!(first, request);
        let (second, rest) = deframe_json::<WireRequest>(&wire[used..])
            .expect("decodes")
            .expect("a whole message");
        assert_eq!(second, request);
        assert_eq!(used + rest, wire.len());
    }

    #[test]
    fn an_attach_on_a_fresh_server_behaves_like_init() {
        let (server, _events) = Server::start();
        let (reply, answer) = channel();
        server.send(Request::Attach {
            shell: waiting(),
            cols: 80,
            rows: 24,
            scrollback: 1000,
            reply,
        });
        match answer
            .recv_timeout(Duration::from_secs(10))
            .expect("the attach answers")
        {
            AttachOutcome::Fresh(Ok((state, created))) => {
                assert_eq!(created.pane, PaneId(0), "a fresh start inits the layout");
                assert_eq!(state.focus, PaneId(0));
                assert_eq!((created.cols, created.rows), (80, 24));
            }
            other => panic!("expected a fresh attach, got {other:?}"),
        }
    }

    #[test]
    fn an_attach_on_an_existing_layout_lists_its_live_panes() {
        let (server, events) = Server::start();
        init(&server);
        split(&server, PaneId(0), waiting(), Rect::new(0, 0, 800, 600));
        let (split_state, created) = wait_state(&events);
        let created = created.expect("the split reports its pane");
        let second_block = created.block.block_id();

        let (reply, answer) = channel();
        server.send(Request::Attach {
            shell: waiting(),
            cols: 80,
            rows: 24,
            scrollback: 1000,
            reply,
        });
        match answer
            .recv_timeout(Duration::from_secs(10))
            .expect("the attach answers")
        {
            AttachOutcome::Existing(attach_state, panes) => {
                assert_eq!(
                    attach_state, split_state,
                    "the attach carries the current state"
                );
                let ids: Vec<PaneId> = panes.iter().map(|pane| pane.pane).collect();
                assert_eq!(ids, vec![PaneId(0), PaneId(1)], "both live panes travel");
                assert_eq!((panes[0].cols, panes[0].rows), (80, 24));
                assert_eq!(
                    (panes[1].cols, panes[1].rows),
                    (40, 20),
                    "the split's spawn size travels"
                );
                assert_ne!(
                    panes[0].block, panes[1].block,
                    "each pane names its own block"
                );
                assert_eq!(panes[1].block, second_block);
                assert_eq!(
                    Layout::from_state(attach_state).tiled_ids(),
                    vec![PaneId(0), PaneId(1)]
                );
            }
            other => panic!("expected an existing attach, got {other:?}"),
        }
    }

    #[test]
    fn the_last_pane_closing_leaves_a_fresh_server_for_the_next_attach() {
        let (server, events) = Server::start();
        init(&server);
        server.send(Request::LayoutClose { pane: PaneId(0) });
        match wait_layout(&events) {
            LayoutEvent::LastPaneClosed => {}
            other => panic!("expected LastPaneClosed, got {other:?}"),
        }

        let (reply, answer) = channel();
        server.send(Request::Attach {
            shell: waiting(),
            cols: 80,
            rows: 24,
            scrollback: 1000,
            reply,
        });
        match answer
            .recv_timeout(Duration::from_secs(10))
            .expect("the attach answers")
        {
            AttachOutcome::Fresh(Ok((state, created))) => {
                // A fresh layout, and a fresh pane id: ids are never reused, not even now
                assert_ne!(created.pane, PaneId(0));
                assert_eq!(state.focus, created.pane);
                assert_eq!(Layout::from_state(state).tiled_ids(), vec![created.pane]);
            }
            other => panic!("expected a fresh attach after the last pane closed, got {other:?}"),
        }
    }

    #[test]
    fn a_panicking_request_does_not_stop_the_server() {
        let (server, _events) = Server::start();
        server.send(Request::Panic);
        // The run loop survives the panic and keeps answering
        let (state, created) = init(&server);
        assert_eq!(created.pane, PaneId(0));
        assert_eq!(state.focus, PaneId(0));
    }

    /// Reads a block's output until `wanted` shows up (answering ConPTY's cursor query), like
    /// the window drains its panes
    #[cfg(unix)]
    fn read_until_marker(block: &mut dyn PaneIo, wanted: &str) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut output = Vec::new();
        while !String::from_utf8_lossy(&output).contains(wanted) && Instant::now() < deadline {
            block.rearm();
            match block.try_read() {
                Some(chunk) => {
                    if chunk.windows(4).any(|query| query == b"\x1b[6n") {
                        block.write(b"\x1b[1;1R");
                    }
                    output.extend(chunk);
                }
                None => thread::sleep(Duration::from_millis(10)),
            }
        }
        assert!(
            String::from_utf8_lossy(&output).contains(wanted),
            "{wanted:?} never came back over the socket"
        );
        output
    }

    #[test]
    #[cfg(unix)]
    fn the_block_writer_thread_serializes_writes_and_never_blocks_the_server() {
        let (server, _events) = Server::start();
        // Echo off: the kernel echo and the program's own print would produce two interleaved
        // copies of every line; with echo off each write comes back exactly once, in order
        let echo_off = running("stty -echo; cat", "more");
        let (mut block, _notice, _rx) =
            create(&server, &echo_off, false).expect("the program starts");
        let id = block.block_id();

        // Each sender writes its pair back to back; the single writer thread per block applies
        // each sender's pair in order and every marker stays one whole write
        const PAIRS: usize = 12;
        let mut senders = Vec::new();
        for sender in 0..3 {
            let handle = server.clone();
            senders.push(thread::spawn(move || {
                for round in 0..PAIRS {
                    let first = format!("s{sender}-a{round}-{}", "x".repeat(40));
                    let second = format!("s{sender}-b{round}-{}", "y".repeat(40));
                    handle.send(Request::Write(id, format!("{first}\n").into_bytes()));
                    handle.send(Request::Write(id, format!("{second}\n").into_bytes()));
                }
            }));
        }

        // A snapshot must answer while the writes queue: before the per-block writer thread
        // existed, a deep enough PTY write backlog blocked the server loop itself
        thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        let (reply, answer) = channel();
        server.send(Request::Snapshot(id, reply));
        let screen = answer
            .recv_timeout(Duration::from_secs(2))
            .expect("the server answers a snapshot while writes are pending");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the server answered in {:?} with a write backlog pending",
            started.elapsed()
        );
        assert!(!screen.is_empty());
        let _ = screen;

        for sender in senders {
            sender.join().expect("the writers finished");
        }
        // Every marker arrives whole, and each sender's pairs stay in order
        let mut output = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        let complete = |text: &str| {
            (0..3).all(|sender| {
                (0..PAIRS).all(|round| {
                    text.contains(&format!("s{sender}-a{round}"))
                        && text.contains(&format!("s{sender}-b{round}"))
                })
            })
        };
        while !complete(&String::from_utf8_lossy(&output)) && Instant::now() < deadline {
            block.rearm();
            match block.try_read() {
                Some(chunk) => {
                    if chunk.windows(4).any(|query| query == b"\x1b[6n") {
                        block.write(b"\x1b[1;1R");
                    }
                    output.extend(chunk);
                }
                None => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        let text = String::from_utf8_lossy(&output).into_owned();
        for sender in 0..3 {
            for round in 0..PAIRS {
                let first = format!("s{sender}-a{round}-{}", "x".repeat(40));
                let second = format!("s{sender}-b{round}-{}", "y".repeat(40));
                let (a, b) = (
                    text.find(&first)
                        .expect("the marker arrived whole; it is one write"),
                    text.find(&second)
                        .expect("the marker arrived whole; it is one write"),
                );
                assert!(
                    a < b,
                    "sender {sender} round {round}: the pair got reordered: {text}"
                );
            }
        }
        block.kill();
    }

    #[test]
    fn typed_frames_dispatch_by_their_type_byte() {
        let request = WireRequest {
            v: WIRE_VERSION,
            rid: Some(3),
            op: WireOp::ToggleZoom,
        };
        let json = wire::frame_json(&request).expect("the request serializes");
        let mut stream: Vec<u8> = Vec::new();
        stream.push(FRAME_CONTROL);
        stream.extend_from_slice(&wire::frame(&json));
        stream.append(&mut data_frame(
            FRAME_DATA,
            BlockId(9),
            b"live-bytes".to_vec(),
        ));
        stream.append(&mut data_frame(
            FRAME_SNAPSHOT,
            BlockId(9),
            b"screen-bits".to_vec(),
        ));
        stream.push(7); // an unknown type byte ends the connection

        let mut read = &stream[..];
        match read_typed_frame(&mut read).expect("the control frame") {
            WireFrame::Control(bytes) => {
                assert_eq!(bytes, json, "the control frame carries the JSON whole");
            }
            frame => panic!("expected a control frame, got {frame:?}"),
        }
        match read_typed_frame(&mut read).expect("the data frame") {
            WireFrame::Data(token, bytes) => {
                assert_eq!(token, 9, "the block token travels");
                assert_eq!(bytes, b"live-bytes");
            }
            frame => panic!("expected a data frame, got {frame:?}"),
        }
        match read_typed_frame(&mut read).expect("the snapshot frame") {
            WireFrame::Snapshot(token, bytes) => {
                assert_eq!(token, 9);
                assert_eq!(bytes, b"screen-bits");
            }
            frame => panic!("expected a snapshot frame, got {frame:?}"),
        }
        assert!(
            read_typed_frame(&mut read).is_err(),
            "an unknown type byte is an error, not a frame"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_real_socket_client_attaches_writes_and_reads_back() {
        // The socket path must be shorter than SUN_LEN: a short /tmp name
        let dir = std::env::temp_dir().join(format!(
            "t{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("the clock works")
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("the test directory is created");
        let path = dir.join("server.sock");

        let (server, layout_events) = Server::start();
        let shared = Arc::new(ProcessShared::default());
        let router_shared = shared.clone();
        thread::spawn(move || route_events(layout_events, router_shared));
        let listener =
            IpcListener::bind(&IpcEndpoint::Socket(path.clone())).expect("the endpoint binds");
        let accept_shared = shared.clone();
        let accept_handle = server.clone();
        thread::spawn(move || accept_sessions(listener, accept_handle, accept_shared));

        // The client connects, spawns its reader, and attaches
        let (client, read) =
            IpcConn::connect(&IpcEndpoint::Socket(path.clone())).expect("the client connects");
        let (layout_tx, layout_rx) = channel::<WireEventKind>();
        {
            let client = client.clone();
            thread::spawn(move || {
                client.read_loop(read, move |down| match down {
                    ControlDown::Event(event) => {
                        let _ = layout_tx.send(event);
                    }
                    ControlDown::Reply(reply) => {
                        panic!("an unexpected reply arrived over the wire: {reply:?}")
                    }
                });
            });
        }
        // An echoing program, so the written bytes come over the data channel back
        let echo = running("cat", "more");
        let answer = client
            .attach(&echo, 80, 24, 1000)
            .expect("the attach answers");
        let AttachAnswer::Fresh { state, pane } = answer else {
            panic!("a fresh server attaches like init");
        };
        assert_eq!(state.focus, pane.pane);

        // The pane is built exactly as the window would: an IpcBlock over the connection
        let (event_tx, _event_rx) = channel::<IoEvent>();
        let event_tx = std::sync::Mutex::new(event_tx);
        let notify: Notify = Arc::new(move |event| {
            let _ = event_tx.lock().unwrap().send(event);
        });
        let mut block = client.open_block(pane.block, notify);
        assert_eq!(pane.block, BlockId(0), "the first block is token zero");

        // The echoed write comes back over the data channel, exactly like a pane drains
        block.write(b"echo-after-attach\n");
        read_until_marker(&mut block, "echo-after-attach");

        // Layout events travel as wire events too: a mutation answers with a state event. A
        // split is the guaranteed change with a single pane (it also reports its created pane)
        client.send_op(WireOp::Split {
            pane: pane.pane,
            dir: crate::layout::Dir::Right,
            shell: wire::WireShell::from_config(&waiting()),
            explicit: false,
            cols: 40,
            rows: 20,
            scrollback: 1000,
            area: Rect::new(0, 0, 800, 600),
            limits: Limits {
                min_w: 80,
                min_h: 60,
            },
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut saw_state = false;
        while Instant::now() < deadline {
            match layout_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(WireEventKind::State { .. }) => {
                    saw_state = true;
                    break;
                }
                // A created pane followed the state event; keep collecting
                Ok(_) => {}
                Err(_) => break,
            }
        }
        assert!(saw_state, "a state event traveled over the wire");

        // A second connection attaches now: the most recent attach takes over as driver and
        // the first session is demoted (and promoted again when the new driver leaves)
        let (second, read) = IpcConn::connect(&IpcEndpoint::Socket(path.clone()))
            .expect("the second client connects");
        let (second_tx, _second_rx) = channel::<WireEventKind>();
        {
            let second = second.clone();
            thread::spawn(move || {
                second.read_loop(read, move |down| match down {
                    ControlDown::Event(event) => {
                        let _ = second_tx.send(event);
                    }
                    ControlDown::Reply(reply) => {
                        panic!("an unexpected reply arrived over the wire: {reply:?}")
                    }
                });
            });
        }
        let answer = second
            .attach(&waiting(), 80, 24, 1000)
            .expect("the second attach answers");
        let AttachAnswer::Existing {
            panes: second_panes,
            exited: second_exited,
            ..
        } = answer
        else {
            panic!("an existing layout hands itself over");
        };
        assert_eq!(second_panes.len(), 2, "both live panes travel");
        assert!(second_exited.is_empty(), "nothing exited while attached");
        // The first session was demoted: it receives the DriverChanged event
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut demoted = false;
        while Instant::now() < deadline {
            match layout_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(WireEventKind::DriverChanged { is_driver: false }) => {
                    demoted = true;
                    break;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        assert!(
            demoted,
            "the first session was told it lost the driver role"
        );

        // The new driver drives: its write reaches the pane
        let (event_tx, _event_rx) = channel::<IoEvent>();
        let event_tx = std::sync::Mutex::new(event_tx);
        let notify: Notify = Arc::new(move |event| {
            let _ = event_tx.lock().unwrap().send(event);
        });
        let mut driver_block = second.open_block(second_panes[0].block, notify);
        driver_block.write(b"echo-from-the-new-driver\n");
        read_until_marker(&mut driver_block, "echo-from-the-new-driver");

        // The demoted session's writes are dropped server-side now
        block.write(b"echo-from-demoted\n");
        {
            let deadline = Instant::now() + Duration::from_millis(500);
            let mut seen = String::new();
            while Instant::now() < deadline {
                block.rearm();
                if let Some(chunk) = block.try_read() {
                    seen.push_str(&String::from_utf8_lossy(&chunk));
                }
                assert!(
                    !seen.contains("echo-from-demoted"),
                    "a viewer's write reached the pane: {seen:?}"
                );
                thread::sleep(Duration::from_millis(20));
            }
        }

        // The new driver leaves: the oldest remaining session is promoted again
        drop(driver_block);
        second.close();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut promoted = false;
        while Instant::now() < deadline {
            match layout_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(WireEventKind::DriverChanged { is_driver: true }) => {
                    promoted = true;
                    break;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        assert!(
            promoted,
            "the remaining session was promoted when the driver left"
        );
        block.write(b"echo-after-busy\n");
        read_until_marker(&mut block, "echo-after-busy");

        // The working directory ask answers over the wire too
        let _ = client.foreground_dir(pane.block);

        // The client goes away: the session ends, the block keeps running headless, and the
        // next session re-attaches to the same block with its screen
        drop(block);
        client.close();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !shared.sessions.lock().unwrap().is_empty() {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            shared.sessions.lock().unwrap().is_empty(),
            "the session ended when the client went away"
        );

        let (client, read) =
            IpcConn::connect(&IpcEndpoint::Socket(path.clone())).expect("the client reconnects");
        {
            let client = client.clone();
            thread::spawn(move || {
                client.read_loop(read, move |down| match down {
                    ControlDown::Event(_) => {}
                    ControlDown::Reply(reply) => {
                        panic!("an unexpected reply arrived over the wire: {reply:?}")
                    }
                });
            });
        }
        let answer = client
            .attach(&waiting(), 80, 24, 1000)
            .expect("the re-attach answers");
        let AttachAnswer::Existing {
            state,
            panes,
            exited,
        } = answer
        else {
            panic!("an existing layout hands itself over");
        };
        assert!(exited.is_empty(), "no pane exited while the sessions ran");
        // Both panes of the split layout are live, in pane order
        assert_eq!(
            Layout::from_state(state).tiled_ids(),
            vec![PaneId(0), PaneId(1)]
        );
        assert_eq!(panes.len(), 2, "both live panes travel");
        assert_eq!(
            panes[0].block,
            BlockId(0),
            "the first block keeps its token"
        );
        assert_eq!(
            panes[1].block,
            BlockId(1),
            "the split's block keeps its token"
        );

        // The snapshot for the re-attached pane arrives on the data channel, before any live
        // chunk, and the first read serves it
        let (event_tx, _event_rx) = channel::<IoEvent>();
        let event_tx = std::sync::Mutex::new(event_tx);
        let notify: Notify = Arc::new(move |event| {
            let _ = event_tx.lock().unwrap().send(event);
        });
        let mut second_block = client.open_block(panes[0].block, notify);
        let deadline = Instant::now() + Duration::from_secs(10);
        let snapshot = loop {
            second_block.rearm();
            if let Some(bytes) = second_block.try_read() {
                break bytes;
            }
            assert!(
                Instant::now() < deadline,
                "the snapshot never arrived on the data channel"
            );
            thread::sleep(Duration::from_millis(10));
        };
        assert!(
            !snapshot.is_empty(),
            "the re-attached pane replays its screen first"
        );

        // The shell behind the block is still alive (the window never killed it) and answers
        second_block.write(b"echo-after-reattach\n");
        read_until_marker(&mut second_block, "echo-after-reattach");

        drop(second_block);
        drop(client);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- Several sessions (stage 4) -------------------------------------------------------
    // Two and three clients over one socket: the most recently attached one drives, the
    // others watch; take-over, gating and promotion are all server-side. The socket harness
    // is unix-only; Windows exercises the same policies through the transport abstraction.

    /// The wire over a real named pipe: a window attaches to a fresh server and a second
    /// connection (like the automation CLI) attaches to the same session. Every ask needs the
    /// server to write while its session reader is parked, which a synchronous pipe handle
    /// never allowed (the asks timed out and the window panicked).
    #[cfg(windows)]
    #[test]
    fn a_window_and_a_second_client_attach_over_a_named_pipe() {
        let endpoint = IpcEndpoint::Pipe(format!(
            r"\\.\pipe\terminal-rs-test-{}-attach",
            std::process::id()
        ));
        let (server, layout_events) = Server::start();
        let shared = Arc::new(ProcessShared::default());
        let router_shared = shared.clone();
        thread::spawn(move || route_events(layout_events, router_shared));
        let listener = IpcListener::bind(&endpoint).expect("the endpoint binds");
        let accept_handle = server.clone();
        thread::spawn(move || accept_sessions(listener, accept_handle, shared));

        // The accepting thread creates its first instance on its own schedule
        let connect = || {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                match IpcConn::connect(&endpoint) {
                    Ok((conn, read)) => {
                        let reader = conn.clone();
                        thread::spawn(move || reader.read_loop(read, |_| {}));
                        return conn;
                    }
                    Err(error) if Instant::now() >= deadline => {
                        panic!("the client could not connect: {error}")
                    }
                    Err(_) => thread::sleep(Duration::from_millis(20)),
                }
            }
        };

        let window = connect();
        match window
            .attach(&waiting(), 80, 24, 1000)
            .expect("the attach answers")
        {
            AttachAnswer::Fresh { .. } => {}
            other => panic!("expected a fresh server, got {other:?}"),
        }
        let second = connect();
        match second
            .attach(&waiting(), 80, 24, 1000)
            .expect("the second attach answers")
        {
            AttachAnswer::Existing { panes, .. } => assert_eq!(panes.len(), 1),
            other => panic!("expected the existing layout, got {other:?}"),
        }
    }

    #[cfg(unix)]
    static HARNESS_SEQ: AtomicU64 = AtomicU64::new(0);

    /// A server process for the multi-session tests: its socket endpoint and shared state.
    /// The scratch directory is removed on drop.
    #[cfg(unix)]
    struct Harness {
        path: PathBuf,
        dir: PathBuf,
        shared: Arc<ProcessShared>,
        _server: ServerHandle,
    }

    #[cfg(unix)]
    impl Harness {
        fn start() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "t4-{}-{}",
                HARNESS_SEQ.fetch_add(1, Ordering::Relaxed),
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("the test directory is created");
            let path = dir.join("server.sock");
            let (server, layout_events) = Server::start();
            let shared = Arc::new(ProcessShared::default());
            let router_shared = shared.clone();
            thread::spawn(move || route_events(layout_events, router_shared));
            let listener =
                IpcListener::bind(&IpcEndpoint::Socket(path.clone())).expect("the endpoint binds");
            let accept_shared = shared.clone();
            let accept_handle = server.clone();
            thread::spawn(move || accept_sessions(listener, accept_handle, accept_shared));
            Self {
                path,
                dir,
                shared,
                _server: server,
            }
        }

        /// Connects a client and spawns its reader thread: the events land on the channel
        fn client(&self) -> (IpcConn, Receiver<WireEventKind>) {
            let (conn, read) = IpcConn::connect(&IpcEndpoint::Socket(self.path.clone()))
                .expect("the client connects");
            let (tx, rx) = channel();
            {
                let conn = conn.clone();
                thread::spawn(move || {
                    conn.read_loop(read, move |down| match down {
                        ControlDown::Event(event) => {
                            let _ = tx.send(event);
                        }
                        ControlDown::Reply(reply) => {
                            panic!("an unexpected reply arrived over the wire: {reply:?}")
                        }
                    });
                });
            }
            (conn, rx)
        }

        /// Waits until every session ended
        fn wait_no_sessions(&self) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline && !self.shared.sessions.lock().unwrap().is_empty() {
                thread::sleep(Duration::from_millis(20));
            }
            assert!(
                self.shared.sessions.lock().unwrap().is_empty(),
                "the sessions ended"
            );
        }
    }

    #[cfg(unix)]
    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Attaches to a fresh server and returns the created pane
    #[cfg(unix)]
    fn attach_fresh(conn: &IpcConn) -> wire::IpcCreatedPane {
        match conn
            .attach(&waiting(), 80, 24, 1000)
            .expect("the attach answers")
        {
            AttachAnswer::Fresh { pane, .. } => pane,
            other => panic!("expected a fresh server, got {other:?}"),
        }
    }

    /// Attaches to an existing layout and returns the live panes and the exited list
    #[cfg(unix)]
    fn attach_existing(conn: &IpcConn) -> (Vec<wire::IpcCreatedPane>, Vec<PaneId>) {
        match conn
            .attach(&waiting(), 80, 24, 1000)
            .expect("the attach answers")
        {
            AttachAnswer::Existing { panes, exited, .. } => (panes, exited),
            other => panic!("expected an existing layout, got {other:?}"),
        }
    }

    /// Builds a pane's block feed over the connection, as the window would
    #[cfg(unix)]
    fn open_feed(conn: &IpcConn, token: BlockId) -> IpcBlock {
        let (tx, _rx) = channel::<IoEvent>();
        let tx = std::sync::Mutex::new(tx);
        let notify: Notify = Arc::new(move |event| {
            let _ = tx.lock().unwrap().send(event);
        });
        conn.open_block(token, notify)
    }

    /// Polls a block's output, failing if `marker` ever shows up (a write the server must
    /// have dropped)
    #[cfg(unix)]
    fn assert_output_never(block: &mut dyn PaneIo, marker: &str) {
        let deadline = Instant::now() + Duration::from_millis(500);
        let mut seen = String::new();
        while Instant::now() < deadline {
            block.rearm();
            if let Some(chunk) = block.try_read() {
                seen.push_str(&String::from_utf8_lossy(&chunk));
                assert!(!seen.contains(marker), "dropped output came back: {seen:?}");
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits for a `DriverChanged` event with the wanted role, skipping the other events and
    /// the opposite role (a session that was demoted earlier still has that event buffered)
    #[cfg(unix)]
    fn wait_driver_changed(events: &Receiver<WireEventKind>, is_driver: bool) -> bool {
        use std::sync::mpsc::RecvTimeoutError;
        let deadline = Instant::now() + Duration::from_secs(10);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match events.recv_timeout(left) {
                Ok(WireEventKind::DriverChanged { is_driver: got }) if got == is_driver => {
                    return true;
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        false
    }

    /// Fails if a promotion (`is_driver: true`) arrives within the window; an already-buffered
    /// demotion is skipped
    #[cfg(unix)]
    fn assert_no_promotion(events: &Receiver<WireEventKind>, window: Duration) {
        let deadline = Instant::now() + window;
        while Instant::now() < deadline {
            let left = deadline.saturating_duration_since(Instant::now());
            match events.recv_timeout(left) {
                Ok(WireEventKind::DriverChanged { is_driver: true }) => {
                    panic!("an unexpected promotion arrived")
                }
                Ok(_) => {}
                Err(_) => {}
            }
        }
    }

    /// Waits for the next state event, skipping the other kinds
    #[cfg(unix)]
    fn wait_wire_state(events: &Receiver<WireEventKind>) -> LayoutState {
        let deadline = Instant::now() + Duration::from_secs(10);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match events.recv_timeout(left) {
                Ok(WireEventKind::State { state, .. }) => return state,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        panic!("no state event arrived");
    }

    /// Fails if a state event arrives within the window (a mutation the server must have
    /// dropped); the other kinds are skipped
    #[cfg(unix)]
    fn assert_no_state_event(events: &Receiver<WireEventKind>, window: Duration) {
        let deadline = Instant::now() + window;
        while Instant::now() < deadline {
            let left = deadline.saturating_duration_since(Instant::now());
            match events.recv_timeout(left.min(Duration::from_millis(100))) {
                Ok(WireEventKind::State { .. }) => {
                    panic!("a mutation the server should have dropped was applied")
                }
                Ok(_) => {}
                Err(_) => {}
            }
        }
    }

    /// A layout resize carrying `area`, the op that stores the driver's window area
    #[cfg(unix)]
    fn resize_op(pane: PaneId, area: Rect) -> WireOp {
        WireOp::Resize {
            pane,
            dir: Dir::Right,
            delta: 10,
            area,
            limits: Limits {
                min_w: 80,
                min_h: 60,
            },
        }
    }

    #[test]
    #[cfg(unix)]
    fn a_second_client_takes_over_as_driver_and_the_first_watches() {
        let harness = Harness::start();
        let (first, first_events) = harness.client();
        let _pane = attach_fresh(&first);

        // The most recent attach takes over: the second client drives, the first is demoted
        let (second, _second_events) = harness.client();
        let (panes, exited) = attach_existing(&second);
        assert_eq!(panes.len(), 1);
        assert!(
            exited.is_empty(),
            "nothing exited while the first was attached"
        );
        assert!(
            wait_driver_changed(&first_events, false),
            "the first session was told it lost the driver role"
        );

        // The driver's write reaches the pane
        let mut driver_block = open_feed(&second, panes[0].block);
        driver_block.write(b"echo-from-the-driver\n");
        read_until_marker(&mut driver_block, "echo-from-the-driver");

        // The viewer's write is dropped: nothing echoes back
        let mut viewer_block = open_feed(&first, panes[0].block);
        viewer_block.write(b"echo-from-the-viewer\n");
        assert_output_never(&mut viewer_block, "echo-from-the-viewer");

        // And the driver still gets its echo afterwards
        driver_block.write(b"echo-from-the-driver-2\n");
        read_until_marker(&mut driver_block, "echo-from-the-driver-2");
        drop(viewer_block);
        drop(driver_block);
        // An explicit close: dropping the connection cannot end the session, the client's own
        // reader thread still holds a clone of it
        first.close();
        second.close();
        harness.wait_no_sessions();
    }

    #[test]
    #[cfg(unix)]
    fn taking_over_drops_the_previous_driver_window_area() {
        let harness = Harness::start();
        let (first, _first_events) = harness.client();
        attach_fresh(&first);

        // The driver's layout op stores its window area
        let area_a = Rect::new(1, 2, 300, 400);
        first.send_op(resize_op(PaneId(0), area_a));
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline
            && *harness.shared.driver_area.lock().unwrap() != Some(area_a)
        {
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            *harness.shared.driver_area.lock().unwrap(),
            Some(area_a),
            "the driver's layout op stored its window area"
        );

        // The take-over drops it, until the new driver sends its own
        let (second, _second_events) = harness.client();
        attach_existing(&second);
        assert_eq!(
            *harness.shared.driver_area.lock().unwrap(),
            None,
            "the demoted driver's area was dropped"
        );

        // The new driver stores its own; the viewer's op changes nothing
        let area_b = Rect::new(0, 0, 500, 500);
        second.send_op(resize_op(PaneId(0), area_b));
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline
            && *harness.shared.driver_area.lock().unwrap() != Some(area_b)
        {
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(*harness.shared.driver_area.lock().unwrap(), Some(area_b));
        first.send_op(resize_op(PaneId(0), Rect::new(9, 9, 999, 999)));
        thread::sleep(Duration::from_millis(300));
        assert_eq!(
            *harness.shared.driver_area.lock().unwrap(),
            Some(area_b),
            "a viewer's layout op was dropped"
        );
    }

    #[test]
    #[cfg(unix)]
    fn layout_changes_reach_every_session_and_viewer_mutations_are_dropped() {
        let harness = Harness::start();
        let (first, first_events) = harness.client();
        attach_fresh(&first);
        let (second, second_events) = harness.client();
        attach_existing(&second);

        // The driver splits: both sessions rebuild from the same state event
        second.send_op(WireOp::Split {
            pane: PaneId(0),
            dir: Dir::Right,
            shell: WireShell::from_config(&waiting()),
            explicit: false,
            cols: 40,
            rows: 20,
            scrollback: 1000,
            area: Rect::new(0, 0, 800, 600),
            limits: Limits {
                min_w: 80,
                min_h: 60,
            },
        });
        let first_state = wait_wire_state(&first_events);
        assert_eq!(
            Layout::from_state(first_state.clone()).tiled_ids(),
            vec![PaneId(0), PaneId(1)]
        );
        let second_state = wait_wire_state(&second_events);
        assert_eq!(
            second_state, first_state,
            "both sessions saw the same change"
        );

        // The viewer's split is dropped: no session sees another state event
        first.send_op(WireOp::Split {
            pane: PaneId(0),
            dir: Dir::Right,
            shell: WireShell::from_config(&waiting()),
            explicit: false,
            cols: 40,
            rows: 20,
            scrollback: 1000,
            area: Rect::new(0, 0, 800, 600),
            limits: Limits {
                min_w: 80,
                min_h: 60,
            },
        });
        assert_no_state_event(&first_events, Duration::from_millis(600));
        assert_no_state_event(&second_events, Duration::from_millis(600));
    }

    #[test]
    #[cfg(unix)]
    fn a_driver_disconnect_promotes_the_oldest_remaining_session() {
        let harness = Harness::start();
        let (first, first_events) = harness.client();
        let pane = attach_fresh(&first);
        let (second, second_events) = harness.client();
        attach_existing(&second);
        let (third, _third_events) = harness.client();
        attach_existing(&third);

        // The driver leaves: the OLDEST remaining session (the first) is promoted, not the
        // second, and nobody else is told
        third.close();
        assert!(
            wait_driver_changed(&first_events, true),
            "the oldest remaining session was promoted"
        );
        assert_no_promotion(&second_events, Duration::from_millis(500));

        // The promoted session drives: its write reaches the pane
        let mut block = open_feed(&first, pane.block);
        block.write(b"echo-after-promotion\n");
        read_until_marker(&mut block, "echo-after-promotion");
        drop(block);
        // An explicit close: dropping the connection cannot end the session (see the take-over
        // test)
        third.close();
        second.close();
        first.close();
        harness.wait_no_sessions();
    }

    #[test]
    #[cfg(unix)]
    fn the_attach_reply_names_panes_that_exited_while_detached() {
        let harness = Harness::start();
        // The first client attaches with a program that exits right away: it sees the exit
        // live, and the pane stays (its block is kept)
        let (first, first_events) = harness.client();
        let dying = running("echo dying-marker", "echo dying-marker");
        match first
            .attach(&dying, 80, 24, 1000)
            .expect("the attach answers")
        {
            AttachAnswer::Fresh { pane, .. } => assert_eq!(pane.pane, PaneId(0)),
            other => panic!("expected a fresh server, got {other:?}"),
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut saw_exit = false;
        while Instant::now() < deadline {
            match first_events.recv_timeout(Duration::from_millis(200)) {
                Ok(WireEventKind::PaneExited { .. }) => {
                    saw_exit = true;
                    break;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        assert!(saw_exit, "the exit reached the attached session live");

        first.close();
        harness.wait_no_sessions();

        // The next attach names the exited pane, and clears the list once delivered
        let (second, _second_events) = harness.client();
        let (panes, exited) = attach_existing(&second);
        assert_eq!(panes.len(), 1, "the held pane still travels");
        assert_eq!(
            exited,
            vec![PaneId(0)],
            "the exit while detached is delivered once"
        );
        let (third, _third_events) = harness.client();
        let (_, exited) = attach_existing(&third);
        assert!(exited.is_empty(), "the list is cleared once delivered");
    }

    // ---- WebSocket gateway (stage 5a) ------------------------------------------------------

    /// A loopback TCP pair: the server end wrapped in a `Ws`, the client end raw (tests write
    /// masked frames on it, exactly what a WebSocket client must send)
    fn ws_pair() -> (ws::WsReader, ws::WsWriter, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback binds");
        let addr = listener.local_addr().expect("the address is known");
        let client = TcpStream::connect(addr).expect("the loopback client connects");
        let (server, _) = listener.accept().expect("the loopback server accepts");
        let (reader, writer) = ws::Ws::from_stream(server).expect("the websocket opens");
        (reader, writer, client)
    }

    /// A masked client frame (opcode, fin, payload): what a WebSocket client must send
    fn masked_frame(opcode: u8, fin: bool, payload: &[u8]) -> Vec<u8> {
        let mask = [0x11u8, 0x22, 0x33, 0x44];
        let mut frame = vec![(if fin { 0x80 } else { 0x00 }) | opcode];
        match payload.len() {
            0..=125 => frame.push(0x80 | payload.len() as u8),
            126..=0xFFFF => {
                frame.push(0x80 | 126);
                frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
            }
            _ => {
                frame.push(0x80 | 127);
                frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
            }
        }
        frame.extend_from_slice(&mask);
        for (index, byte) in payload.iter().enumerate() {
            frame.push(byte ^ mask[index % 4]);
        }
        frame
    }

    /// Reads one unmasked server frame off the client end: its opcode and payload
    fn read_server_frame(stream: &mut TcpStream) -> (u8, Vec<u8>) {
        let mut head = [0u8; 2];
        stream
            .read_exact(&mut head)
            .expect("the frame head arrives");
        let opcode = head[0] & 0x0f;
        let len = match head[1] & 0x7f {
            126 => {
                let mut extended = [0u8; 2];
                stream
                    .read_exact(&mut extended)
                    .expect("the length arrives");
                u16::from_be_bytes(extended) as usize
            }
            127 => {
                let mut extended = [0u8; 8];
                stream
                    .read_exact(&mut extended)
                    .expect("the length arrives");
                u64::from_be_bytes(extended) as usize
            }
            len => len as usize,
        };
        let mut payload = vec![0u8; len];
        stream
            .read_exact(&mut payload)
            .expect("the payload arrives");
        (opcode, payload)
    }

    /// Sends a WebSocket binary message from the test client: a masked frame over `bytes`
    fn ws_send(stream: &mut TcpStream, bytes: &[u8]) {
        stream
            .write_all(&masked_frame(0x2, true, bytes))
            .expect("the frame goes out");
    }

    /// Sends a control message from the test client: the 1-byte type prefix, then the JSON
    #[cfg(unix)]
    fn ws_send_control(stream: &mut TcpStream, message: &impl serde::Serialize) {
        let json = serde_json::to_vec(message).expect("serializes");
        let mut message = vec![FRAME_CONTROL];
        message.extend_from_slice(&json);
        ws_send(stream, &message);
    }

    /// Reads one WebSocket binary message off the test client (the server never fragments out);
    /// `None` when nothing arrived within the read timeout
    #[cfg(unix)]
    fn ws_recv_maybe(stream: &mut TcpStream) -> Option<Vec<u8>> {
        stream
            .set_read_timeout(Some(Duration::from_millis(250)))
            .expect("the read timeout is set");
        let mut head = [0u8; 2];
        match stream.read_exact(&mut head) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return None;
            }
            Err(error) => panic!("the server frame failed: {error}"),
        }
        let opcode = head[0] & 0x0f;
        let len = match head[1] & 0x7f {
            126 => {
                let mut extended = [0u8; 2];
                stream
                    .read_exact(&mut extended)
                    .expect("the length arrives");
                u16::from_be_bytes(extended) as usize
            }
            127 => {
                let mut extended = [0u8; 8];
                stream
                    .read_exact(&mut extended)
                    .expect("the length arrives");
                u64::from_be_bytes(extended) as usize
            }
            len => len as usize,
        };
        // Once the head arrived the payload follows whole: clear the timeout so a slow chunk
        // cannot lose the frame's tail
        stream
            .set_read_timeout(None)
            .expect("the read timeout is cleared");
        let mut payload = vec![0u8; len];
        stream
            .read_exact(&mut payload)
            .expect("the payload arrives");
        assert_eq!(opcode, 0x2, "the server sends binary frames");
        Some(payload)
    }

    /// Reads WebSocket messages until one satisfies `wanted`, with a deadline
    #[cfg(unix)]
    fn ws_recv_until(stream: &mut TcpStream, mut wanted: impl FnMut(&[u8]) -> bool) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Some(payload) = ws_recv_maybe(stream)
                && wanted(&payload)
            {
                return payload;
            }
        }
        panic!("the websocket message never arrived");
    }

    /// Reads the reply with correlation id `rid`, skipping anything else
    #[cfg(unix)]
    fn control_reply(stream: &mut TcpStream, rid: u64) -> wire::WireReply {
        let payload = ws_recv_until(stream, |payload| {
            payload.first() == Some(&FRAME_CONTROL)
                && serde_json::from_slice::<wire::WireReply>(&payload[1..])
                    .map(|reply| reply.rid == rid)
                    .unwrap_or(false)
        });
        serde_json::from_slice(&payload[1..]).expect("the reply decodes")
    }

    /// Sends the upgrade request (with an identity header when `login` is set) and returns
    /// the client stream plus the response head
    fn ws_upgrade(addr: std::net::SocketAddr, login: Option<&str>) -> (TcpStream, String) {
        let mut stream = TcpStream::connect(addr).expect("the remote client connects");
        let mut request = String::from(
            "GET /ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\n",
        );
        if let Some(login) = login {
            request.push_str(&format!("Tailscale-User-Login: {login}\r\n"));
        }
        request.push_str("\r\n");
        stream
            .write_all(request.as_bytes())
            .expect("the request goes out");
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).expect("the response arrives");
            head.push(byte[0]);
        }
        (stream, String::from_utf8_lossy(&head).into_owned())
    }

    /// A gateway accept loop in front of the harness server: loopback port and allow list
    #[cfg(unix)]
    fn start_gateway(harness: &Harness, allow: &[&str]) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("the gateway listener binds");
        let addr = listener.local_addr().expect("the gateway address is known");
        let allow: Vec<String> = allow.iter().map(|login| login.to_string()).collect();
        let endpoint = IpcEndpoint::Socket(harness.path.clone());
        thread::spawn(move || accept_gateway(listener, endpoint, allow));
        addr
    }

    #[test]
    fn the_websocket_accept_matches_the_rfc_example() {
        assert_eq!(
            ws::accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn ws_frames_round_trip_masked_in_and_unmasked_out() {
        let (mut reader, writer, mut client) = ws_pair();
        writer.write_binary(b"out").expect("the write goes out");
        let (opcode, payload) = read_server_frame(&mut client);
        assert_eq!((opcode, payload.as_slice()), (0x2, b"out".as_slice()));

        ws_send(&mut client, b"in");
        assert_eq!(
            reader.read_message().expect("the message arrives"),
            ws::Message::Binary(b"in".to_vec())
        );

        // A client frame that is not masked is a protocol error (RFC 6455 5.1)
        client
            .write_all(&[0x82, 0x01, b'x'])
            .expect("the raw frame goes out");
        assert!(
            reader.read_message().is_err(),
            "unmasked frames are refused"
        );
    }

    #[test]
    fn ws_joins_fragmented_binary_messages_and_refuses_strays() {
        let (mut reader, _writer, mut client) = ws_pair();
        client
            .write_all(&masked_frame(0x2, false, b"frag"))
            .expect("the first fragment goes out");
        client
            .write_all(&masked_frame(0x0, false, b"ment"))
            .expect("the continuation goes out");
        client
            .write_all(&masked_frame(0x0, true, b"ed"))
            .expect("the last fragment goes out");
        assert_eq!(
            reader.read_message().expect("the joined message arrives"),
            ws::Message::Binary(b"fragmented".to_vec())
        );

        // A continuation with no message in progress is a protocol error
        client
            .write_all(&masked_frame(0x0, true, b"stray"))
            .expect("the stray goes out");
        assert!(
            reader.read_message().is_err(),
            "a stray continuation is refused"
        );
    }

    #[test]
    fn ws_answers_pings_and_echoes_closes() {
        let (mut reader, _writer, mut client) = ws_pair();
        client
            .write_all(&masked_frame(0x9, true, b"xy"))
            .expect("the ping goes out");
        ws_send(&mut client, b"data");
        // Reading the next message answers the ping in place (before it returns the binary)
        assert_eq!(
            reader.read_message().expect("the message arrives"),
            ws::Message::Binary(b"data".to_vec())
        );
        let (opcode, payload) = read_server_frame(&mut client);
        assert_eq!((opcode, payload.as_slice()), (0xA, b"xy".as_slice()));

        // A close is echoed once and reported
        client
            .write_all(&masked_frame(0x8, true, &1000u16.to_be_bytes()))
            .expect("the close goes out");
        assert_eq!(
            reader.read_message().expect("the close arrives"),
            ws::Message::Close(Some(1000))
        );
        let (opcode, payload) = read_server_frame(&mut client);
        assert_eq!(
            (opcode, payload.as_slice()),
            (0x8, 1000u16.to_be_bytes().as_slice())
        );
    }

    #[test]
    fn ws_refuses_text_frames() {
        let (mut reader, _writer, mut client) = ws_pair();
        client
            .write_all(&masked_frame(0x1, true, b"text"))
            .expect("the text frame goes out");
        let error = reader
            .read_message()
            .expect_err("text frames are refused (binary only)");
        assert!(error.to_string().contains("text"), "{error}");
    }

    #[test]
    fn the_ws_handshake_computes_the_accept_and_reads_the_identity_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback binds");
        let addr = listener.local_addr().expect("the address is known");
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("the handshake connects");
            let request = ws::read_upgrade_request(&mut stream).expect("the request parses");
            assert_eq!(request.login.as_deref(), Some("dev@example.com"));
            let key = request.key.expect("the key travels");
            ws::write_switching(&stream, &key).expect("the switch goes out");
        });
        let (client, head) = ws_upgrade(addr, Some("dev@example.com"));
        assert!(head.contains("101 Switching Protocols"), "{head}");
        assert!(
            head.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo="),
            "{head}"
        );
        let _ = client.shutdown(std::net::Shutdown::Both);
    }

    #[test]
    #[cfg(unix)]
    fn a_remote_client_over_websocket_attaches_writes_and_reads_back() {
        let harness = Harness::start();
        let addr = start_gateway(&harness, &["dev@example.com"]);

        let (mut stream, head) = ws_upgrade(addr, Some("dev@example.com"));
        assert!(head.contains("101 Switching Protocols"), "{head}");
        // The gateway names who authenticated (rid 0: gateway-initiated, no ask)
        let hello = control_reply(&mut stream, 0);
        assert_eq!(
            hello.reply,
            WireReplyKind::Hello {
                login: "dev@example.com".to_string()
            }
        );

        // Attach exactly as a local window does
        ws_send_control(
            &mut stream,
            &WireRequest {
                v: WIRE_VERSION,
                rid: Some(1),
                op: WireOp::Attach {
                    shell: WireShell::from_config(&running("cat", "more")),
                    cols: 80,
                    rows: 24,
                    scrollback: 1000,
                },
            },
        );
        let reply = control_reply(&mut stream, 1);
        let WireReplyKind::Init { state, created } = reply.reply else {
            panic!("a fresh server attaches like init: {:?}", reply.reply);
        };
        let pane = created.expect("the first pane travels");
        assert_eq!(state.focus, pane.pane);

        // The write reaches the pane, and the echo streams back as a data frame
        ws_send_control(
            &mut stream,
            &WireRequest {
                v: WIRE_VERSION,
                rid: None,
                op: WireOp::Write {
                    block: pane.block,
                    bytes: b"echo-over-ws\n".to_vec(),
                },
            },
        );
        let echo = ws_recv_until(&mut stream, |payload| {
            payload.first() == Some(&FRAME_DATA)
                && payload.get(1..5) == Some(&(pane.block.0 as u32).to_le_bytes())
                && String::from_utf8_lossy(&payload[5..]).contains("echo-over-ws")
        });
        assert!(
            String::from_utf8_lossy(&echo[5..]).contains("echo-over-ws"),
            "{echo:?}"
        );

        // Events fan out too: a split answers with a state event over the websocket
        ws_send_control(
            &mut stream,
            &WireRequest {
                v: WIRE_VERSION,
                rid: Some(2),
                op: WireOp::Split {
                    pane: pane.pane,
                    dir: crate::layout::Dir::Right,
                    shell: WireShell::from_config(&waiting()),
                    explicit: false,
                    cols: 40,
                    rows: 20,
                    scrollback: 1000,
                    area: Rect::new(0, 0, 800, 600),
                    limits: Limits {
                        min_w: 80,
                        min_h: 60,
                    },
                },
            },
        );
        let event = ws_recv_until(&mut stream, |payload| {
            payload.first() == Some(&FRAME_CONTROL)
                && serde_json::from_slice::<wire::WireEvent>(&payload[1..])
                    .map(|event| matches!(event.event, WireEventKind::State { .. }))
                    .unwrap_or(false)
        });
        let event: wire::WireEvent = serde_json::from_slice(&event[1..]).expect("decodes");
        assert!(matches!(event.event, WireEventKind::State { .. }));

        // The bridge ends with the websocket: the close handshake ends the server session
        stream
            .write_all(&masked_frame(0x8, true, &1000u16.to_be_bytes()))
            .expect("the close goes out");
        harness.wait_no_sessions();
    }

    #[test]
    #[cfg(unix)]
    fn the_gateway_refuses_logins_that_remote_allow_does_not_name() {
        let harness = Harness::start();
        let addr = start_gateway(&harness, &["dev@example.com"]);

        for login in [Some("stranger@example.com"), None] {
            let (_stream, head) = ws_upgrade(addr, login);
            assert!(head.contains("403 Forbidden"), "{login:?}: {head}");
            assert!(!head.contains("101"), "{login:?}: {head}");
        }
        harness.wait_no_sessions();

        // An empty allow list denies everyone
        let addr = start_gateway(&harness, &[]);
        let (_stream, head) = ws_upgrade(addr, Some("dev@example.com"));
        assert!(head.contains("403 Forbidden"), "{head}");
    }

    #[test]
    #[cfg(unix)]
    fn a_second_remote_client_is_refused_while_one_is_active() {
        let harness = Harness::start();
        let addr = start_gateway(&harness, &["dev@example.com"]);

        let (mut first, first_head) = ws_upgrade(addr, Some("dev@example.com"));
        assert!(first_head.contains("101"), "{first_head}");
        let _hello = control_reply(&mut first, 0);

        // The second connection upgrades, gets the Busy event, and closes right after
        let (mut second, second_head) = ws_upgrade(addr, Some("dev@example.com"));
        assert!(second_head.contains("101"), "{second_head}");
        let busy = ws_recv_until(&mut second, |payload| {
            payload.first() == Some(&FRAME_CONTROL)
        });
        let busy: WireEvent = serde_json::from_slice(&busy[1..]).expect("the busy event decodes");
        assert!(matches!(busy.event, WireEventKind::Busy));
        let (opcode, _payload) = read_server_frame(&mut second);
        assert_eq!(opcode, 0x8, "the busy client is closed right after");
        assert_eq!(
            second.read(&mut [0u8; 1]).expect("the socket closes"),
            0,
            "nothing follows the close"
        );

        // The first remote client is unaffected
        ws_send_control(
            &mut first,
            &WireRequest {
                v: WIRE_VERSION,
                rid: Some(1),
                op: WireOp::Attach {
                    shell: WireShell::from_config(&waiting()),
                    cols: 80,
                    rows: 24,
                    scrollback: 1000,
                },
            },
        );
        let reply = control_reply(&mut first, 1);
        assert!(matches!(reply.reply, WireReplyKind::Init { .. }));
        // The gateway slot frees up when the websocket closes
        first
            .write_all(&masked_frame(0x8, true, &1000u16.to_be_bytes()))
            .expect("the close goes out");
        harness.wait_no_sessions();
    }

    // --- Stage 6: the automation CLI ---------------------------------------------------------

    #[test]
    #[cfg(unix)]
    fn the_automation_cli_lists_panes() {
        let harness = Harness::start();
        // A fresh server has no panes to list
        let (cli, _cli_events) = harness.client();
        assert!(
            cli.automation_list().expect("the list answers").is_empty(),
            "a fresh server lists no panes"
        );
        // After the window attaches, the pane is listed with its block token, grid and state
        let (window, _window_events) = harness.client();
        let pane = attach_fresh(&window);
        let panes = cli.automation_list().expect("the list answers");
        assert_eq!(panes.len(), 1, "the live pane is listed");
        assert_eq!(panes[0].pane, pane.pane);
        assert_eq!(panes[0].block, pane.block);
        assert_eq!((panes[0].cols, panes[0].rows), (80, 24));
        assert!(panes[0].alive, "the held program is alive");
        // The CLI never attached: the ask must not disturb the window's session
        window.close();
        cli.close();
        harness.wait_no_sessions();
    }

    #[test]
    #[cfg(unix)]
    fn the_automation_cli_send_bypasses_the_driver_gate_and_reaches_the_program() {
        let harness = Harness::start();
        let (window, _window_events) = harness.client();
        let echo = running("cat", "more");
        let pane = match window
            .attach(&echo, 80, 24, 1000)
            .expect("the attach answers")
        {
            AttachAnswer::Fresh { pane, .. } => pane,
            other => panic!("expected a fresh server, got {other:?}"),
        };
        let mut block = open_feed(&window, pane.block);
        // The CLI session never attaches, so it is never the driver; its automation send is
        // served anyway (a plain Write from the same session would be dropped by the gate)
        let (cli, _cli_events) = harness.client();
        cli.automation_send_checked(pane.block, b"echo-from-cli\n")
            .expect("the send is confirmed");
        read_until_marker(&mut block, "echo-from-cli");
        // An unknown block is refused with an error naming it
        let error = cli
            .automation_send_checked(BlockId(999), b"echo-nowhere\n")
            .expect_err("an unknown block is refused");
        assert!(error.contains("999"), "the error names the block: {error}");
        // The window still drives: its write reaches the pane after the CLI traffic
        block.write(b"echo-still-driver\n");
        read_until_marker(&mut block, "echo-still-driver");
        cli.close();
        window.close();
        harness.wait_no_sessions();
    }

    #[test]
    #[cfg(unix)]
    fn a_dead_pane_reports_dead_and_refuses_sends() {
        let harness = Harness::start();
        let (window, window_events) = harness.client();
        let dying = running("echo dying-marker", "echo dying-marker");
        let pane = match window
            .attach(&dying, 80, 24, 1000)
            .expect("the attach answers")
        {
            AttachAnswer::Fresh { pane, .. } => pane,
            other => panic!("expected a fresh server, got {other:?}"),
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut saw_exit = false;
        while Instant::now() < deadline {
            match window_events.recv_timeout(Duration::from_millis(200)) {
                Ok(WireEventKind::PaneExited { .. }) => {
                    saw_exit = true;
                    break;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        assert!(saw_exit, "the exit reached the window");
        // The list reports the pane dead, and sends to it are refused
        let (cli, _cli_events) = harness.client();
        let panes = cli.automation_list().expect("the list answers");
        assert_eq!(panes.len(), 1, "the held pane is still listed");
        assert!(!panes[0].alive, "the exited pane reports dead");
        let error = cli
            .automation_send_checked(panes[0].block, b"echo-ghost\n")
            .expect_err("a dead pane refuses sends");
        assert!(error.contains("exited"), "the error explains why: {error}");
        // The held screen is still readable: capture answers the plain text (the CLI checks
        // alive client-side and errors there; the server keeps serving the held screen)
        let text = cli
            .automation_capture(pane.block)
            .expect("the capture answers");
        assert!(
            text.contains("dying-marker"),
            "the screen is still captured: {text:?}"
        );
        cli.close();
        window.close();
        harness.wait_no_sessions();
    }

    #[test]
    #[cfg(unix)]
    fn the_automation_cli_capture_matches_the_plain_screen() {
        let harness = Harness::start();
        let (window, _window_events) = harness.client();
        let program = running(
            "printf 'capture-line\\ncapture-marker'",
            "echo capture-line & echo capture-marker",
        );
        let pane = match window
            .attach(&program, 80, 24, 1000)
            .expect("the attach answers")
        {
            AttachAnswer::Fresh { pane, .. } => pane,
            other => panic!("expected a fresh server, got {other:?}"),
        };
        let mut block = open_feed(&window, pane.block);
        let output = read_until_marker(&mut block, "capture-marker");
        // The expectation: the same output, rendered in a fresh terminal, plain-formatted
        let mut expected = terminal();
        expected.vt_write(&output);
        let (cli, _cli_events) = harness.client();
        let text = cli
            .automation_capture(pane.block)
            .expect("the capture answers");
        assert_eq!(
            text,
            plain_text(&expected),
            "the capture is the plain exported screen"
        );
        // The raw snapshot (`capture --vt`) rebuilds the same screen
        let vt = cli.snapshot(pane.block).expect("the snapshot answers");
        let mut restored = terminal();
        restored.vt_write(&vt);
        assert_eq!(plain_text(&restored), plain_text(&expected));
        cli.close();
        window.close();
        harness.wait_no_sessions();
    }

    #[test]
    #[cfg(unix)]
    fn attach_streams_the_snapshot_then_live_bytes() {
        let harness = Harness::start();
        let (window, _window_events) = harness.client();
        let echo = running("cat", "more");
        let pane = match window
            .attach(&echo, 80, 24, 1000)
            .expect("the attach answers")
        {
            AttachAnswer::Fresh { pane, .. } => pane,
            other => panic!("expected a fresh server, got {other:?}"),
        };
        let mut win_block = open_feed(&window, pane.block);
        win_block.write(b"seed-screen\n");
        read_until_marker(&mut win_block, "seed-screen");
        // The CLI subscribes to the block's data channel: the snapshot is served first
        let (cli, _cli_events) = harness.client();
        cli.automation_subscribe(pane.block)
            .expect("the subscribe answers");
        let mut cli_block = open_feed(&cli, pane.block);
        let deadline = Instant::now() + Duration::from_secs(10);
        let snapshot = loop {
            cli_block.rearm();
            match cli_block.try_read() {
                Some(chunk) => break chunk,
                None if Instant::now() > deadline => panic!("the snapshot never arrived"),
                None => thread::sleep(Duration::from_millis(20)),
            }
        };
        assert!(
            String::from_utf8_lossy(&snapshot).contains("seed-screen"),
            "the snapshot is served first: {:?}",
            String::from_utf8_lossy(&snapshot)
        );
        // ...then live bytes follow, and the window still sees its own data
        cli.automation_send(pane.block, b"echo-live\n");
        read_until_marker(&mut cli_block, "echo-live");
        read_until_marker(&mut win_block, "echo-live");
        cli.close();
        window.close();
        harness.wait_no_sessions();
    }
}
