//! In-process terminal server. It owns the programs behind the panes ("blocks") and the
//! authoritative layout, on its own thread: each block is a [`LocalPty`] plus a server-side
//! libghostty terminal fed with exactly the bytes the program writes, so the screen can be
//! exported as a [snapshot](ServerBlock::snapshot) for a client that attaches later. The window
//! talks to it only through messages, by way of a [`ServerHandle`]: layout mutations arrive as
//! ops ([`SplitOp`], [`LayoutClose`], ...), the server applies them, spawns and kills the
//! blocks, and reports one [`LayoutEvent::State`] per applied mutation so the window can rebuild
//! its read-only projection.
//!
//! The server-side terminal never answers the program's queries: the client's terminal does,
//! through [`PaneIo::write`], exactly as with a local PTY.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError, channel, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

use libghostty_vt::fmt::{Format, Formatter, FormatterOptions};
use libghostty_vt::{Terminal, TerminalOptions};

use crate::config::ShellConfig;
use crate::layout::{Closed, Dir, Edge, Layout, LayoutState, Limits, PaneId, Path, Rect};
use crate::pane_io::{CHANNEL_CAPACITY, IoEvent, LocalPty, Notify, PaneIo};

/// Identifies a block within its server
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    /// The program wrote something, or a paused client read: forward what is pending
    Drain(BlockId),
    /// The program exited (sent by the PTY's thread)
    Exited(BlockId),
    /// Every handle was dropped
    Shutdown,
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
    /// When the program's exit was noticed; the exit notice waits until every chunk was forwarded
    exit_requested: Option<Instant>,
    client: Client,
}

/// The server thread's state: the blocks, and the authoritative layout they belong to
pub struct Server {
    blocks: HashMap<BlockId, Block>,
    /// Which pane of the layout each block belongs to
    pane_blocks: HashMap<PaneId, BlockId>,
    next_id: u64,
    /// Pane ids are allocated here and never reused, not even after a rolled-back spawn
    next_pane_id: u32,
    layout: Option<Layout>,
    /// To queue messages to itself (from the PTYs' threads, and to split long drains)
    requests: Sender<Request>,
    /// Layout events for the window (state after every applied mutation, pane output and exits)
    events: Sender<LayoutEvent>,
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
                    next_id: 0,
                    next_pane_id: 0,
                    layout: None,
                    requests: own,
                    events,
                };
                server.run(inbox);
            })
            .expect("could not start the terminal server thread");
        let handle = ServerHandle {
            connection: Arc::new(Connection { requests }),
        };
        (handle, outgoing)
    }

    fn run(mut self, inbox: Receiver<Request>) {
        while let Ok(request) = inbox.recv() {
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
                Request::LayoutSplit(op) => self.split_pane(op),
                Request::LayoutClose { pane } => self.close_pane(pane),
                Request::LayoutSetFocus { pane } => self.apply(|layout| layout.set_focus(pane)),
                Request::LayoutFocusDir(op) => {
                    self.apply(|layout| layout.focus_dir(op.dir, op.area))
                }
                Request::LayoutFocusCycle { forward } => {
                    self.apply(|layout| layout.focus_cycle(forward));
                }
                Request::LayoutResize(op) => self
                    .apply(|layout| layout.resize(op.pane, op.dir, op.delta, op.area, op.limits)),
                Request::LayoutSetDivider(op) => self
                    .apply(|layout| layout.set_divider(&op.path, op.boundary, op.area, op.limits)),
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
                        block.pty.write(&bytes);
                    }
                }
                Request::Resize(id, cols, rows) => {
                    if let Some(block) = self.blocks.get_mut(&id) {
                        block.pty.resize(cols, rows);
                        // The server has no pixels: 1×1 cells
                        let _ = block.terminal.resize(cols, rows, 1, 1);
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
                Request::Shutdown => break,
            }
        }
        for block in self.blocks.values_mut() {
            block.pty.kill();
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
        self.blocks.insert(
            id,
            Block {
                pty,
                terminal,
                output,
                pending: None,
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
                self.next_pane_id -= 1;
                Err(error)
            }
        }
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
                let _ = self.events.send(LayoutEvent::LastPaneClosed);
            }
            Closed::Pane => {
                if let Some(id) = self.pane_blocks.remove(&pane)
                    && let Some(mut block) = self.blocks.remove(&id)
                {
                    block.pty.kill();
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

/// Sends `Shutdown` when the last handle goes away
struct Connection {
    requests: Sender<Request>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.requests.send(Request::Shutdown);
    }
}

/// Cheap, cloneable access to the server thread
#[derive(Clone)]
pub struct ServerHandle {
    connection: Arc<Connection>,
}

impl ServerHandle {
    /// Queues a message for the server thread; errors are ignored (the server may be gone)
    pub(crate) fn send(&self, request: Request) {
        let _ = self.connection.requests.send(request);
    }

    /// Asks the server for an answer and waits for it; `None` if the server is gone
    fn ask<T>(&self, request: impl FnOnce(Sender<T>) -> Request) -> Option<T> {
        let (reply, answer) = channel();
        self.send(request(reply));
        answer.recv().ok()
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
        self.ask(|reply| Request::Init {
            shell: shell.clone(),
            cols,
            rows,
            scrollback,
            reply,
        })
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
            .ask(|reply| Request::Create {
                shell: shell.clone(),
                explicit,
                cols,
                rows,
                scrollback,
                client,
                reply,
            })
            .ok_or_else(|| "the terminal server stopped".to_string())??;
        let block = ServerBlock {
            requests: self.connection.requests.clone(),
            id,
            output,
            wake_pending,
            paused,
            notify,
        };
        Ok((block, notice))
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
    /// Asks the server for an answer and waits for it; `None` if the server is gone
    fn ask<T>(&self, request: impl FnOnce(Sender<T>) -> Request) -> Option<T> {
        let (reply, answer) = channel();
        self.requests.send(request(reply)).ok()?;
        answer.recv().ok()
    }

    /// The block's screen as VT sequences (see [`snapshot`])
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "attaching clients arrive in stage 3")
    )]
    pub fn snapshot(&self) -> Vec<u8> {
        self.ask(|reply| Request::Snapshot(self.id, reply))
            .unwrap_or_default()
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

impl Drop for ServerBlock {
    fn drop(&mut self) {
        let _ = self.requests.send(Request::Close(self.id));
    }
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
}
