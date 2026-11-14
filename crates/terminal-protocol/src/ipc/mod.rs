//! The local transport between the window and the server process: the shared control-plane
//! types both ends speak (the in-process request channel and the layout ops), the typed frame
//! codec, the endpoint plumbing (unix socket and Windows named pipe), and the client-side
//! connection machinery (`IpcConn`, its blocks and the op sender).
//!
//! The server side of the process (sessions, the event router, the server thread) stays in the
//! server crate; it consumes these types through `terminal-protocol`.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::ShellConfig;
use crate::layout::{Dir, Edge, LayoutState, Limits, PaneId, Path, Rect};
use crate::pane_io::IoEvent;
use crate::pane_io::{Notify, PaneIo};
use crate::wire::{
    self, WIRE_VERSION, WireEvent, WireEventKind, WireReply, WireReplyKind, WireRequest,
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
pub struct SplitOp {
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
pub struct AddFloatOp {
    pub shell: ShellConfig,
    pub explicit: bool,
    /// Entries merged over the shell's environment (the action's win over the global shell.env)
    pub env: BTreeMap<String, String>,
    pub width: f32,
    pub height: f32,
    pub cols: u16,
    pub rows: u16,
    pub scrollback: usize,
}

/// "Move the focus to the neighboring pane in that direction"
pub struct FocusDirOp {
    pub dir: Dir,
    pub area: Rect,
}

/// "Move this pane's edge in that direction by `delta` pixels"
pub struct ResizeOp {
    pub pane: PaneId,
    pub dir: Dir,
    pub delta: u32,
    pub area: Rect,
    pub limits: Limits,
}

/// "Move the divider at `path` to `boundary"`
pub struct SetDividerOp {
    pub path: Path,
    pub boundary: i64,
    pub area: Rect,
    pub limits: Limits,
}

/// "Move this floating pane by this much"
pub struct MoveFloatOp {
    pub pane: PaneId,
    pub dx: i64,
    pub dy: i64,
    pub window: Rect,
}

/// "Resize this floating pane by dragging its edge this far"
pub struct ResizeFloatEdgeOp {
    pub pane: PaneId,
    pub edge: Edge,
    pub dx: i64,
    pub dy: i64,
    pub window: Rect,
    pub limits: Limits,
}

/// What the server answers to a create request: the block and its output channel, plus the
/// fallback notice
pub type Created = Result<(BlockId, Receiver<Vec<u8>>, Option<String>), String>;

/// Messages the server thread handles, in arrival order
#[allow(clippy::large_enum_variant)]
pub enum Request {
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
    Panic,
}

/// What a session is told for its attach: a fresh server ran init (the created pane's block view
/// travels in the reply), an existing one reports its state and its live panes with their block
/// tokens (the session pulls their block views from its own map)
#[derive(Debug)]
pub enum AttachOutcome {
    Fresh(Result<(LayoutState, CreatedPane), String>),
    Existing(LayoutState, Vec<AttachedPaneInfo>),
}

/// A live pane in an attach: the pane, its block token, and the size to start the pane at
#[derive(Debug, Clone, Copy)]
pub struct AttachedPaneInfo {
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

            Request::Panic => "Panic",
        })
    }
}

/// The state a block shares with its client ([`ServerBlock`])
pub struct Client {
    pub notify: Notify,
    /// One [`IoEvent::Output`] at a time; the client re-arms it before draining
    pub wake_pending: Arc<AtomicBool>,
    /// The client's channel was full: the client asks for a drain after its next read
    pub paused: Arc<AtomicBool>,
}

/// The control frame's type byte
pub const FRAME_CONTROL: u8 = 0;
/// The live-data frame's type byte
pub const FRAME_DATA: u8 = 1;
/// The attach-snapshot frame's type byte
pub const FRAME_SNAPSHOT: u8 = 2;

/// One frame read (or ready to write) on the shared connection
#[derive(Debug, PartialEq, Eq)]
pub enum WireFrame {
    /// A control message's JSON bytes (request, reply or event)
    Control(Vec<u8>),
    /// A block's live output
    Data(u32, Vec<u8>),
    /// A block's screen at attach time
    Snapshot(u32, Vec<u8>),
}

/// Reads one typed frame, blocking
pub fn read_typed_frame(read: &mut dyn Read) -> io::Result<WireFrame> {
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
pub fn data_frame(kind: u8, block: BlockId, bytes: Vec<u8>) -> Vec<u8> {
    let mut frame = Vec::with_capacity(1 + 4 + 4 + bytes.len());
    frame.push(kind);
    frame.extend_from_slice(&((4 + bytes.len()) as u32).to_le_bytes());
    // The token fits u32 for as long as a session can create blocks
    frame.extend_from_slice(&(block.0 as u32).to_le_bytes());
    frame.extend_from_slice(&bytes);
    frame
}

/// Frames a control message for the shared connection
pub fn control_frame<T: serde::Serialize>(message: &T) -> Vec<u8> {
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
pub enum IpcStream {
    #[cfg(unix)]
    Socket(UnixStream),
    #[cfg(windows)]
    Pipe(Box<dyn Read + Send>, Box<dyn Write + Send>),
}

impl IpcStream {
    /// Splits into independent read and write halves
    pub fn into_halves(self) -> (Box<dyn Read + Send>, Box<dyn Write + Send>) {
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
pub enum IpcListener {
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
    pub fn bind(endpoint: &IpcEndpoint) -> io::Result<Self> {
        match endpoint {
            #[cfg(unix)]
            IpcEndpoint::Socket(path) => unix_listen(path).map(Self::Socket),
            #[cfg(windows)]
            IpcEndpoint::Pipe(name) => windows_pipe::PipeListener::bind(name).map(Self::Pipe),
        }
    }

    /// Waits for one connection
    pub fn accept(&self) -> io::Result<IpcStream> {
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

/// The default loopback port `terminal gateway` serves on. Real deployments front it with
/// `tailscale serve`, which proxies a tailnet HTTPS endpoint onto this port.
pub const DEFAULT_GATEWAY_PORT: u16 = 42061;

// ---- The client side of the server process --------------------------------------------------
//
// The window connects to the endpoint (or starts `terminal server` first; that part lives in
// `main`), sends one `Attach` and then drives its panes purely locally: each pane gets an
// [`IpcBlock`], whose queue the connection's reader thread feeds with that block's frames.

/// What the reader thread (the window's end of the connection) forwards out of the control
/// channel: replies correlated by rid, and uncorrelated ones (events)
#[derive(Debug)]
pub enum ControlDown {
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
pub enum ConnWrite {
    #[cfg(unix)]
    Socket(UnixStream),
    #[cfg(windows)]
    Pipe(Box<dyn Write + Send>),
}

impl ConnWrite {
    pub fn send_frame(&mut self, frame: &[u8]) -> io::Result<()> {
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
    pub fn shutdown(&self) {
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
pub const ASK_TIMEOUT: Duration = Duration::from_secs(30);

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
    pub fn send_op(&self, op: wire::WireOp) {
        let _ = self.send_wire(&WireRequest {
            v: WIRE_VERSION,
            rid: None,
            op,
        });
    }

    /// The working directory of the block's foreground program (a blocking round trip, asked
    /// under `ask`'s timeout)
    pub fn foreground_dir(&self, block: BlockId) -> Option<PathBuf> {
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
    pub fn automation_list(&self) -> Result<Vec<wire::AutomationPane>, String> {
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
    pub fn automation_send(&self, block: BlockId, bytes: &[u8]) {
        self.send_op(wire::WireOp::Send {
            block,
            bytes: bytes.to_vec(),
        });
    }

    /// One checked send: the server confirms the block exists and its program is alive
    pub fn automation_send_checked(&self, block: BlockId, bytes: &[u8]) -> Result<(), String> {
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
    pub fn automation_capture(&self, block: BlockId) -> Result<String, String> {
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
    pub fn snapshot(&self, block: BlockId) -> Result<Vec<u8>, String> {
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
    pub fn automation_subscribe(&self, block: BlockId) -> Result<(), String> {
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
    pub fn new(conn: IpcConn) -> Self {
        Self { conn }
    }

    /// Queues an op for the server thread; errors are ignored (the server may be gone)
    pub fn send(&self, request: Request) {
        if let Some(op) = wire::WireOp::from_request(&request) {
            self.conn.send_op(op);
        }
    }
}

/// A pane's program, running in the server
pub struct ServerBlock {
    pub requests: Sender<Request>,
    pub id: BlockId,
    pub output: Receiver<Vec<u8>>,
    pub wake_pending: Arc<AtomicBool>,
    pub paused: Arc<AtomicBool>,
    pub notify: Notify,
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

    /// The block's screen as VT sequences (see [`snapshot`]). The server's sessions answer
    /// snapshot asks directly; this ask boundary stays for the tests.
    pub fn snapshot(&self) -> Vec<u8> {
        self.ask(|reply| Request::Snapshot(self.id, reply))
            .unwrap_or_default()
    }

    /// The wire token of the block: how sessions and data frames name it
    pub fn block_id(&self) -> BlockId {
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

#[cfg(windows)]
pub mod windows_pipe;
