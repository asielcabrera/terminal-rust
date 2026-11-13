//! The wire format (stage 3): the same ops and events the in-process channel carries, as
//! length-prefixed JSON over a byte stream, and the minimal RFC 6455 WebSocket transport the
//! tailnet gateway relays (only the mapping lives here; the socket and sessions are the server
//! process section).

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::ShellConfig;
use crate::layout::{Dir, Edge, LayoutState, Limits, PaneId, Path, Rect};

use crate::ipc::{
    AddFloatOp, AttachedPaneInfo, BlockId, CreatedPane, FocusDirOp, LayoutEvent, MoveFloatOp,
    Request, ResizeFloatEdgeOp, ResizeOp, SetDividerOp, SplitOp,
};

/// `v` of the protocol; a client and a server whose versions differ refuse to talk
pub const WIRE_VERSION: u32 = 1;

/// Largest control frame accepted. A serialized state of a many-pane tree and a reply fit
/// with room to spare; anything larger is a corrupt length prefix, never a message.
pub const MAX_FRAME: usize = 64 * 1024 * 1024;

/// One request. A session (or a window) sends exactly one of these per wire op; attach is
/// the one the wire adds: a fresh server runs init, an attached one hands over its state
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum WireOp {
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
        env: BTreeMap<String, String>,
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
pub struct AutomationPane {
    pub pane: PaneId,
    pub block: BlockId,
    pub cols: u16,
    pub rows: u16,
    pub alive: bool,
}

/// One request for the server, versioned. `rid` names the sender's correlation id for the
/// requests that answer (attach/init, foreground_dir, snapshot); the rest send `None`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireRequest {
    pub v: u32,
    pub rid: Option<u64>,
    pub op: WireOp,
}

impl WireOp {
    /// The wire form of a request that travels. Server-internal traffic (drains, exit
    /// notices, shutdown) and the test-only create stay in the process.
    pub fn from_request(request: &Request) -> Option<Self> {
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
                env: op.env.clone(),
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

            Request::Panic | Request::Create { .. } => return None,
        };
        Some(op)
    }

    /// The in-process request for a fire-and-forget wire op, for a session to hand to the
    /// server thread. Ops that carry a reply (attach/init, foreground directory, snapshot)
    /// answer through their own paths and are not mapped here.
    pub fn into_request(self) -> Option<Request> {
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
                env,
                width,
                height,
                cols,
                rows,
                scrollback,
            } => {
                let mut shell = shell.into_config();
                // The action's entries win over the global shell.env
                shell.env.extend(env);
                Request::LayoutAddFloat(AddFloatOp {
                    shell,
                    explicit,
                    env: BTreeMap::new(),
                    width,
                    height,
                    cols,
                    rows,
                    scrollback,
                })
            }
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
pub struct WireShell {
    pub program: Option<String>,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub integration: bool,
}

impl WireShell {
    /// The wire form of a shell config
    pub fn from_config(shell: &ShellConfig) -> Self {
        Self {
            program: shell.program.clone(),
            args: shell.args.clone(),
            cwd: shell.cwd.clone(),
            env: shell.env.clone(),
            integration: shell.integration,
        }
    }

    /// Rebuilds the shell config on the server side
    pub fn into_config(self) -> ShellConfig {
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
pub struct IpcCreatedPane {
    pub pane: PaneId,
    pub block: BlockId,
    pub cols: u16,
    pub rows: u16,
    pub notice: Option<String>,
}

impl IpcCreatedPane {
    /// The wire form of a created pane
    pub fn from_created(created: &CreatedPane) -> Self {
        Self {
            pane: created.pane,
            block: created.block.block_id(),
            cols: created.cols,
            rows: created.rows,
            notice: created.notice.clone(),
        }
    }

    /// The wire form of a live pane handed to a session that attached over the wire
    pub fn from_attached(info: AttachedPaneInfo) -> Self {
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
pub struct WireReply {
    pub v: u32,
    pub rid: u64,
    pub reply: WireReplyKind,
}

impl WireReply {
    /// Wraps any answer with the protocol version
    pub fn new(rid: u64, reply: WireReplyKind) -> Self {
        Self {
            v: WIRE_VERSION,
            rid,
            reply,
        }
    }

    /// The answer to `Init`: the first pane's state and its created pane, or why it refused
    pub fn init(rid: u64, result: Result<(LayoutState, Option<CreatedPane>), String>) -> Self {
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
    pub fn foreground_dir(rid: u64, dir: Option<PathBuf>) -> Self {
        Self::new(rid, WireReplyKind::ForegroundDir { dir })
    }

    /// The answer to `Snapshot` (a session replying to the wire op; an attach carries its
    /// snapshots over the data channel instead, but the control shape stays for simple asks)
    pub fn snapshot(rid: u64, bytes: Vec<u8>) -> Self {
        Self::new(rid, WireReplyKind::Snapshot { bytes })
    }

    /// The gateway's hello: who the remote client authenticated as. Sent right after the
    /// websocket upgrade, with `rid` 0 — gateway-initiated, no ask behind it.
    pub fn hello(login: String) -> Self {
        Self::new(0, WireReplyKind::Hello { login })
    }

    /// The answer to `AutomationList`
    pub fn automation_list(rid: u64, panes: Vec<AutomationPane>) -> Self {
        Self::new(rid, WireReplyKind::AutomationList { panes })
    }

    /// The answer to a confirmed `Send`
    pub fn automation_sent(rid: u64) -> Self {
        Self::new(rid, WireReplyKind::AutomationSent {})
    }

    /// The answer to `Capture`
    pub fn automation_capture(rid: u64, text: String) -> Self {
        Self::new(rid, WireReplyKind::AutomationCapture { text })
    }

    /// The answer to `AutomationSubscribe` (the snapshot is already queued on the data
    /// channel; the reply arrives ahead of it because controls overtake data)
    pub fn automation_subscribed(rid: u64) -> Self {
        Self::new(rid, WireReplyKind::AutomationSubscribed {})
    }

    /// A refused automation ask (unknown or dead block)
    pub fn automation_failed(rid: u64, error: String) -> Self {
        Self::new(rid, WireReplyKind::AutomationFailed { error })
    }
}

/// What a reply answers with
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum WireReplyKind {
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
pub struct WireEvent {
    pub v: u32,
    pub event: WireEventKind,
}

impl WireEvent {
    /// The wire form of a layout event: exactly what the server broadcasts today, with the
    /// created pane reduced to what travels (the block itself goes over the data channel)
    pub fn from_layout_event(event: &LayoutEvent) -> Option<Self> {
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
pub enum WireEventKind {
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
pub enum WireError {
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
pub fn frame(message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + message.len());
    out.extend_from_slice(&(message.len() as u32).to_le_bytes());
    out.extend_from_slice(message);
    out
}

/// Reads the first framed message out of `buf`
pub fn deframe(buf: &[u8]) -> Deframed {
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
pub enum Deframed {
    /// One complete message, plus how many bytes of the buffer it took
    Message { bytes: Vec<u8>, consumed: usize },
    /// The buffer does not hold a whole message yet: wait for more bytes
    Incomplete,
    /// The length prefix is not a frame this protocol carries: give up on the connection
    Invalid,
}

/// Frames one control message as JSON
pub fn frame_json<T: serde::Serialize>(message: &T) -> Result<Vec<u8>, serde_json::Error> {
    Ok(frame(&serde_json::to_vec(message)?))
}

/// Decodes the next JSON control message out of `buf`, if a whole framed message is there;
/// whatever follows stays for the next call
pub fn deframe_json<T: serde::de::DeserializeOwned>(
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

/// The WebSocket transport the gateway serves remote clients over
pub mod ws;
