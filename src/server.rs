//! In-process terminal server. It owns the programs behind the panes ("blocks") on its own
//! thread: each block is a [`LocalPty`] plus a server-side libghostty terminal fed with exactly
//! the bytes the program writes, so the screen can be exported as a [snapshot](ServerHandle::snapshot)
//! for a client that attaches later. The window talks to it only through messages, by way of a
//! [`ServerHandle`] and the [`ServerBlock`]s it creates.
//!
//! The server-side terminal never answers the program's queries: the client's terminal does,
//! through [`PaneIo::write`], exactly as with a local PTY.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError, channel, sync_channel};
use std::thread;

use libghostty_vt::fmt::{Format, Formatter, FormatterOptions};
use libghostty_vt::{Terminal, TerminalOptions};

use crate::config::ShellConfig;
use crate::pane_io::{CHANNEL_CAPACITY, IoEvent, LocalPty, Notify, PaneIo};

/// Identifies a block within its server
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockId(pub u64);

/// Chunks forwarded per drain request; past that the block queues another request so the other
/// blocks' messages interleave
const MAX_CHUNKS_PER_DRAIN: usize = 16;

/// What the server answers to a create request: the block and its output channel, plus the
/// fallback notice
type Created = Result<(BlockId, Receiver<Vec<u8>>, Option<String>), String>;

/// Messages the server thread handles, in arrival order
enum Request {
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
struct Client {
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
    /// The program exited and the exit notice waits until every chunk was forwarded
    exiting: bool,
    client: Client,
}

/// The server thread's state
pub struct Server {
    blocks: HashMap<BlockId, Block>,
    next_id: u64,
    /// To queue messages to itself (from the PTYs' threads, and to split long drains)
    requests: Sender<Request>,
}

impl Server {
    /// Starts the server thread. It stops, killing whatever is left, when every handle is
    /// dropped.
    pub fn start() -> ServerHandle {
        let (requests, inbox) = channel();
        let own = requests.clone();
        // The terminals are not `Send`: the state is built on the thread that owns it
        thread::Builder::new()
            .name("terminal-server".to_string())
            .spawn(move || {
                let server = Server {
                    blocks: HashMap::new(),
                    next_id: 0,
                    requests: own,
                };
                server.run(inbox);
            })
            .expect("could not start the terminal server thread");
        ServerHandle {
            connection: Arc::new(Connection { requests }),
        }
    }

    fn run(mut self, inbox: Receiver<Request>) {
        while let Ok(request) = inbox.recv() {
            match request {
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
                    if let Some(block) = self.blocks.get_mut(&id) {
                        block.exiting = true;
                    }
                    let finished = self.drain(id);
                    self.maybe_exit(id, finished);
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
                exiting: false,
                client,
            },
        );
        Ok((id, receiver, notice))
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

    /// Reports the exit to the client, but only once every chunk was forwarded: `exiting` is set
    /// by the PTY's exit notice, `finished` says this drain emptied the block, and the reader
    /// thread only closes the output channel after its last chunk.
    fn maybe_exit(&mut self, id: BlockId, finished: bool) {
        let Some(block) = self.blocks.get_mut(&id) else {
            return;
        };
        if finished && block.exiting && block.pty.output_finished() {
            block.exiting = false;
            (block.client.notify)(IoEvent::Exited);
        }
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
    fn send(&self, request: Request) {
        let _ = self.connection.requests.send(request);
    }

    /// Asks the server for an answer and waits for it; `None` if the server is gone
    fn ask<T>(&self, request: impl FnOnce(Sender<T>) -> Request) -> Option<T> {
        let (reply, answer) = channel();
        self.send(request(reply));
        answer.recv().ok()
    }

    /// Starts `shell` in a new block of `cols`×`rows`, with the same fallback and errors as
    /// [`LocalPty::spawn`]. `notify` wakes the client when there is output or the program exits.
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
            server: self.clone(),
            id,
            output,
            wake_pending,
            paused,
            notify,
        };
        Ok((block, notice))
    }

    /// The block's screen as VT sequences (see [`snapshot`]); empty if the block does not exist
    pub fn snapshot(&self, id: BlockId) -> Vec<u8> {
        self.ask(|reply| Request::Snapshot(id, reply))
            .unwrap_or_default()
    }
}

/// A pane's program, running in the server
pub struct ServerBlock {
    server: ServerHandle,
    id: BlockId,
    output: Receiver<Vec<u8>>,
    wake_pending: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    notify: Notify,
}

impl ServerBlock {
    /// The block's screen as VT sequences (see [`ServerHandle::snapshot`])
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "attaching clients arrive in stage 3")
    )]
    pub fn snapshot(&self) -> Vec<u8> {
        self.server.snapshot(self.id)
    }
}

impl PaneIo for ServerBlock {
    fn write(&mut self, bytes: &[u8]) {
        self.server.send(Request::Write(self.id, bytes.to_vec()));
    }

    fn try_read(&mut self) -> Option<Vec<u8>> {
        let chunk = self.output.try_recv().ok()?;
        // There is room now: the server resumes the paused block
        if self.paused.swap(false, Ordering::SeqCst) {
            self.server.send(Request::Drain(self.id));
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
        self.server.send(Request::Resize(self.id, cols, rows));
    }

    fn foreground_dir(&self) -> Option<PathBuf> {
        self.server
            .ask(|reply| Request::ForegroundDir(self.id, reply))
            .flatten()
    }

    fn kill(&mut self) {
        self.server.send(Request::Kill(self.id));
    }
}

impl Drop for ServerBlock {
    fn drop(&mut self) {
        self.server.send(Request::Close(self.id));
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
    fn the_program_output_arrives_and_its_exit_is_reported() {
        let server = Server::start();
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
        let server = Server::start();
        let (_block, _, rx) = create(&server, &printing("wake"), true).unwrap();
        assert!(wait_for(&rx, IoEvent::Output));
    }

    #[test]
    fn killing_the_program_reports_its_exit() {
        let server = Server::start();
        let (mut block, _, rx) = create(&server, &waiting(), true).unwrap();
        block.kill();
        assert!(wait_for(&rx, IoEvent::Exited));
    }

    #[test]
    fn an_explicit_program_that_cannot_start_is_an_error() {
        let server = Server::start();
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
        let server = Server::start();
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
        let server = Server::start();
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
        let server = Server::start();
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
        let server = Server::start();
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
