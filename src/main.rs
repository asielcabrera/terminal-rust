mod actions;
mod blink;
mod config;
mod glyphs;
mod icon;
mod layout;
mod mouse;
mod pane;
mod pane_io;
#[cfg(unix)] // Windows has no API for another process' cwd; pane.rs only calls it on unix
mod proc_cwd;
mod renderer;
mod server;

use actions::{Action, Command, Direction, EventKind, LeaderOutcome, LeaderState, Script};
use blink::Blink;
use config::Config;
use layout::{Axis, Layout, LayoutState, Limits, PaneId, Rect};
use libghostty_vt::key::{Action as KeyAction, Encoder, Event as GhosttyKeyEvent, Key, Mods};
use libghostty_vt::paste;
use libghostty_vt::terminal::{Mode, ScrollViewport};
use mlua::Value;
use mouse::MouseState;
use pane::{MAX_CHUNKS_PER_WAKE, Pane, PaneSeed, apply_theme, with_integration};
use pane_io::{IoEvent, Notify, PaneIo};
use renderer::{PaneView, Renderer};
use server::{
    AttachAnswer, BlockId, ControlDown, IpcConn, IpcEndpoint, OpSender, wire,
    wire::{WireEventKind, WireShell},
};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
#[cfg(windows)]
use winit::platform::windows::{CornerPreference, WindowAttributesExtWindows, WindowExtWindows};
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::{DeviceEvent, DeviceId, ElementState, KeyEvent, StartCause, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    keyboard::{KeyCode, PhysicalKey},
    window::{Fullscreen, Window, WindowId},
};

/// Grid size when the configuration does not ask for another
const DEFAULT_COLUMNS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

/// Minimum pane size, in cells: no split happens if both halves would fall below it
const MIN_PANE_COLUMNS: u32 = 8;
const MIN_PANE_ROWS: u32 = 3;

/// Events other threads send to the UI loop to wake it up
#[derive(Debug)]
enum UserEvent {
    /// The server applied a layout mutation, or a pane's program wrote or exited
    Layout(WireEventKind),
    /// The configuration file changed on disk
    ConfigChanged,
}

/// Something that happened inside a terminal during `vt_write` that Lua hooks may care about.
/// Queued from libghostty callbacks and dispatched once `vt_write` has finished.
enum LuaEvent {
    TitleChanged(String),
    Bell,
}

/// How often config.lua is checked for changes, and how long to wait before reading it so a
/// half-finished save is not picked up
const CONFIG_POLL_INTERVAL: Duration = Duration::from_millis(500);
const CONFIG_SETTLE_DELAY: Duration = Duration::from_millis(100);

fn modified_time(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
}

/// Watches the configuration file by comparing its modification time. It is a single file, so
/// this avoids a dependency like `notify`. It ends when the event loop closes.
fn watch_config(path: PathBuf, proxy: EventLoopProxy<UserEvent>) {
    thread::spawn(move || {
        let mut last = modified_time(&path);
        loop {
            thread::sleep(CONFIG_POLL_INTERVAL);
            let current = modified_time(&path);
            if current != last {
                last = current;
                thread::sleep(CONFIG_SETTLE_DELAY);
                if proxy.send_event(UserEvent::ConfigChanged).is_err() {
                    break;
                }
            }
        }
    });
}

/// Windows 11 rounds window corners unless asked not to
#[cfg(windows)]
fn corner_preference(rounded: bool) -> CornerPreference {
    if rounded {
        CornerPreference::Default
    } else {
        CornerPreference::DoNotRound
    }
}

/// The application icon at `size`×`size` for winit (title bar and taskbar)
fn window_icon(size: u32) -> Option<winit::window::Icon> {
    winit::window::Icon::from_rgba(icon::rgba(size), size, size).ok()
}

/// Sets the icon in the Dock and the app switcher (Cmd+Tab). Without an `.app` bundle macOS
/// shows a generic executable icon; this replaces it while the application is running.
#[cfg(target_os = "macos")]
fn set_dock_icon() {
    use objc2::{AllocAnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let data = NSData::with_bytes(&icon::ico(&[256, 128, 64, 32, 16]));
    let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
        return;
    };
    // SAFETY: called on the main thread, after winit created the shared NSApplication
    unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image)) };
}

struct App {
    config: Config,
    config_path: Option<PathBuf>,
    /// Keys, hooks, and the Lua interpreter of the current configuration
    script: Script,
    /// Terminal events waiting to be passed to the Lua hooks (and the sender for new
    /// panes)
    lua_events: Receiver<(PaneId, LuaEvent)>,
    lua_tx: Sender<(PaneId, LuaEvent)>,
    /// While a hook's commands run, no other hooks fire, so one that triggers its own event
    /// (e.g. reloading the config from `config-reloaded`) does not
    /// loop forever
    in_hook: bool,
    /// Pressed modifiers (bits of `actions::CTRL`, etc.) for looking up key bindings
    mod_bits: u8,
    /// How far the font size was moved by the increase/decrease actions. A configuration
    /// reload resets it to zero.
    font_delta: f32,
    /// Cursor blink phase, and whether the last frame had a blinking cursor (only then
    /// does the loop need to wake up to toggle it)
    blink: Blink,
    cursor_blinks: bool,
    mouse: MouseState,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    focused: bool,
    /// All panes; the layout projection decides where each one goes and which one has focus.
    /// The layout is rebuilt from server state events and never mutated here.
    panes: Vec<Pane>,
    layout: Layout,
    /// The prefix (like tmux's Ctrl+B) is pressed and waiting for the next key
    leader: LeaderState,
    key_encoder: Encoder<'static>,
    mods: Mods,
    /// Owns the programs behind the panes, over the server-process connection
    conn: IpcConn,
    /// The op sender the mouse driver uses, shaped like the in-process handle it replaced
    server: OpSender,
    /// Wakes the UI loop from the reader thread when a pane's output arrives
    proxy: EventLoopProxy<UserEvent>,
    /// Panes the attach reply reported as exited while this window was detached; processed
    /// once the event loop runs (drain, hook, close-or-hold)
    attach_exited: Vec<PaneId>,
}

/// How often a starting window retries the server connection after spawning (and spawning) it
const CONNECT_RETRIES: usize = 50;

/// Connects to the server (starting it detached if it is not running), exactly like the
/// window does before attaching. The gateway dials again per remote client; this only
/// guarantees the server is up.
fn ensure_server_running(endpoint: &IpcEndpoint) {
    if IpcConn::connect(endpoint).is_ok() {
        return;
    }
    spawn_detached_server();
    for _ in 0..CONNECT_RETRIES {
        thread::sleep(Duration::from_millis(100));
        if IpcConn::connect(endpoint).is_ok() {
            return;
        }
    }
    panic!("could not start or reach the terminal server");
}

/// Reads the gateway subcommand's arguments: `--port N`, or the default port
fn gateway_port(args: &[String]) -> u16 {
    let mut port = server::DEFAULT_GATEWAY_PORT;
    let mut values = args.iter().skip(2);
    while let Some(flag) = values.next() {
        if flag != "--port" {
            eprintln!("terminal gateway: unknown argument {flag:?}");
            std::process::exit(2);
        }
        let Some(value) = values.next().and_then(|value| value.parse().ok()) else {
            eprintln!("terminal gateway: --port needs a port number");
            std::process::exit(2);
        };
        port = value;
    }
    port
}

/// Starts `terminal server` detached, when no server answered: its own process group (nothing
/// dies with this window's process group), no console window on Windows, and no inherited output.
fn spawn_detached_server() {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            eprintln!("[server] could not locate the executable: {error}");
            return;
        }
    };
    let mut command = ProcessCommand::new(exe);
    command.arg("server");
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    if let Err(error) = command.spawn() {
        eprintln!("[server] could not start the server process: {error}");
    }
}

/// Connects to the server, starting it detached when none answers (the window and the
/// automation CLI share this; the retries give the freshly spawned server time to bind)
fn connect_or_start(endpoint: &IpcEndpoint) -> Result<(IpcConn, Box<dyn Read + Send>), String> {
    match IpcConn::connect(endpoint) {
        Ok(connected) => Ok(connected),
        Err(_) => {
            spawn_detached_server();
            let mut connected = None;
            for _ in 0..CONNECT_RETRIES {
                thread::sleep(Duration::from_millis(100));
                if let Ok(connected_now) = IpcConn::connect(endpoint) {
                    connected = Some(connected_now);
                    break;
                }
            }
            connected.ok_or_else(|| "could not start or reach the terminal server".to_string())
        }
    }
}

/// Wakes the UI loop when a pane's data feed receives output (the connection's reader thread
/// calls this, exactly like a local PTY's reader thread would)
fn output_notify(proxy: EventLoopProxy<UserEvent>, pane: PaneId) -> Notify {
    Arc::new(move |event| {
        if let IoEvent::Output = event {
            let _ = proxy.send_event(UserEvent::Layout(WireEventKind::PaneOutput { pane }));
        }
    })
}

impl App {
    fn index_of(&self, id: PaneId) -> Option<usize> {
        self.panes.iter().position(|pane| pane.id == id)
    }

    /// Index in `panes` of the focused pane. Always present: closing the last one exits the app.
    fn focused_index(&self) -> usize {
        self.index_of(self.layout.focus()).unwrap_or(0)
    }

    fn focused(&self) -> &Pane {
        &self.panes[self.focused_index()]
    }

    fn focused_mut(&mut self) -> &mut Pane {
        let index = self.focused_index();
        &mut self.panes[index]
    }

    /// The configured fixed title or, if none, the one set by the focused pane's program
    /// (or the default)
    fn window_title(&self) -> String {
        if let Some(title) = &self.config.window.title {
            return title.clone();
        }
        self.focused()
            .title()
            .unwrap_or_else(|| config::DEFAULT_TITLE.to_string())
    }

    fn refresh_title(&self) {
        if let Some(window) = &self.window {
            window.set_title(&self.window_title());
        }
    }

    /// Focuses a pane (a click, an action): one message, and the state event applies it
    fn focus_pane(&mut self, id: PaneId) {
        if self.layout.focus() == id {
            return;
        }
        self.conn.send_op(wire::WireOp::SetFocus { pane: id });
    }

    fn redraw_window(&self) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Window area (physical pixels) shared among the panes
    fn window_area(&self) -> Option<Rect> {
        let size = self.window.as_ref()?.inner_size();
        Some(Rect::new(0, 0, size.width, size.height))
    }

    /// Minimum pane size with the current cell size and padding
    fn pane_limits(&self) -> Option<Limits> {
        let renderer = self.renderer.as_ref()?;
        let (cell_width, cell_height) = renderer.cell_size();
        let padding = 2 * renderer.padding();
        Some(Limits {
            min_w: MIN_PANE_COLUMNS * cell_width + padding,
            min_h: MIN_PANE_ROWS * cell_height + padding,
        })
    }

    /// Directory where a new pane starts: that of pane `from` (via OSC 7 or, on macOS and
    /// Linux, by asking the OS) if `panes.inherit_cwd` allows it; otherwise `shell.cwd`
    fn inherited_cwd(&self, from: PaneId) -> Option<PathBuf> {
        if !self.config.panes.inherit_cwd {
            return None;
        }
        self.panes
            .iter()
            .find(|pane| pane.id == from)?
            .current_dir()
    }

    /// Splits the focused pane (or the last tiled one, from a floating pane): one message to
    /// the server, which applies it, starts the block and reports both back
    fn split(&mut self, axis: Axis) {
        let (Some(area), Some(limits), Some(renderer)) =
            (self.window_area(), self.pane_limits(), &self.renderer)
        else {
            return;
        };
        let target = self.layout.split_target();
        let cwd = self.inherited_cwd(target);
        // The preview is exactly what the server will apply: the new pane's grid is sized
        // before the block starts, so the shell sees its real size from the first prompt
        let Some(rect) = self.layout.split_preview(target, axis, area, limits) else {
            eprintln!("[panes] not enough room to split the pane");
            return;
        };
        let (cols, rows) = renderer.grid_size(rect.w, rect.h);
        let mut shell = self.config.shell.clone();
        if let Some(cwd) = cwd {
            shell.cwd = Some(cwd);
        }
        self.conn.send_op(wire::WireOp::Split {
            pane: target,
            dir: match axis {
                Axis::Horizontal => layout::Dir::Right,
                Axis::Vertical => layout::Dir::Down,
            },
            shell: WireShell::from_config(&with_integration(shell)),
            explicit: false,
            cols,
            rows,
            scrollback: self.config.scrollback,
            area,
            limits,
        });
    }

    /// Opens a floating pane and focuses it. With `program` it runs that program instead of the
    /// shell (and the floating pane closes when it exits). `width`/`height` are window fractions.
    fn new_float(
        &mut self,
        program: Option<(String, Vec<String>)>,
        width: Option<f32>,
        height: Option<f32>,
        cwd: Option<String>,
        env: BTreeMap<String, String>,
    ) {
        let (Some(area), Some(renderer)) = (self.window_area(), &self.renderer) else {
            return;
        };
        let cwd = cwd
            .map(PathBuf::from)
            .or_else(|| self.inherited_cwd(self.layout.focus()));
        let panes = &self.config.panes;
        let width = width.unwrap_or(panes.float_width);
        let height = height.unwrap_or(panes.float_height);
        let rect = self.layout.float_preview(width, height, area);
        let (cols, rows) = renderer.grid_size(rect.w, rect.h);
        let mut shell = self.config.shell.clone();
        let explicit = program.is_some();
        if let Some((program, args)) = program {
            shell.program = Some(program);
            shell.args = args;
        }
        if let Some(cwd) = cwd {
            shell.cwd = Some(cwd);
        }
        self.conn.send_op(wire::WireOp::AddFloat {
            shell: WireShell::from_config(&with_integration(shell)),
            explicit,
            env,
            width,
            height,
            cols,
            rows,
            scrollback: self.config.scrollback,
        });
    }

    /// Hides or shows the floating panes; if there are none, opens one
    fn toggle_floats(&mut self) {
        if !self.layout.has_floats() {
            self.new_float(None, None, None, None, BTreeMap::new());
            return;
        }
        self.conn.send_op(wire::WireOp::ToggleFloats);
    }

    /// Moves the focused floating pane `amount` cells toward `direction`
    fn move_pane(&mut self, direction: Direction, amount: u16) {
        let (Some(area), Some(renderer)) = (self.window_area(), &self.renderer) else {
            return;
        };
        let focus = self.layout.focus();
        if !self.layout.is_float(focus) {
            return;
        }
        let (cell_width, cell_height) = renderer.cell_size();
        let (horizontal, vertical) = (
            i64::from(amount) * i64::from(cell_width),
            i64::from(amount) * i64::from(cell_height),
        );
        let (dx, dy) = match direction {
            Direction::Left => (-horizontal, 0),
            Direction::Right => (horizontal, 0),
            Direction::Up => (0, -vertical),
            Direction::Down => (0, vertical),
        };
        self.conn.send_op(wire::WireOp::MoveFloat {
            pane: focus,
            dx,
            dy,
            window: area,
        });
    }

    fn focus_direction(&mut self, direction: Direction) {
        let Some(area) = self.window_area() else {
            return;
        };
        self.conn.send_op(wire::WireOp::FocusDir {
            dir: layout_dir(direction),
            area,
        });
    }

    fn focus_cycle(&mut self, forward: bool) {
        self.conn.send_op(wire::WireOp::FocusCycle { forward });
    }

    /// Moves the focused pane's border `amount` cells toward `direction`
    fn resize_pane(&mut self, direction: Direction, amount: u16) {
        let (Some(area), Some(limits), Some(renderer)) =
            (self.window_area(), self.pane_limits(), &self.renderer)
        else {
            return;
        };
        let (cell_width, cell_height) = renderer.cell_size();
        let step = match direction {
            Direction::Left | Direction::Right => cell_width,
            Direction::Up | Direction::Down => cell_height,
        };
        self.conn.send_op(wire::WireOp::Resize {
            pane: self.layout.focus(),
            dir: layout_dir(direction),
            delta: u32::from(amount) * step,
            area,
            limits,
        });
    }

    /// Rectangles of the lines separating the tiled panes
    fn divider_rects(&self) -> Vec<Rect> {
        let (Some(area), Some(renderer)) = (self.window_area(), &self.renderer) else {
            return Vec::new();
        };
        let thickness = renderer.divider_thickness();
        self.layout
            .dividers(area)
            .iter()
            .map(|divider| divider.rect(thickness))
            .collect()
    }

    /// Closes a pane. If it was the last tiled one, the server kills everything and tells the
    /// window to quit.
    fn close_pane(&mut self, id: PaneId) {
        self.conn.send_op(wire::WireOp::Close { pane: id });
    }

    /// Rebuilds the projection from a server state event and syncs the panes with it
    fn apply_state(&mut self, state: LayoutState, created: Option<wire::IpcCreatedPane>) {
        self.layout = Layout::from_state(state);
        // Panes the server removed are gone: this window's panes do not hold blocks anymore (the
        // shells live in the server process), dropping them only frees the view
        let gone: Vec<PaneId> = self
            .panes
            .iter()
            .map(|pane| pane.id)
            .filter(|id| !self.layout.contains(*id))
            .collect();
        for id in gone {
            self.panes.retain(|pane| pane.id != id);
            self.mouse.forget_pane(id);
        }
        if let Some(created) = created {
            let pane_id = created.pane;
            let notify = output_notify(self.proxy.clone(), pane_id);
            let seed = PaneSeed {
                pane: created.pane,
                cols: created.cols,
                rows: created.rows,
                notice: created.notice,
                io: Box::new(self.conn.open_block(created.block, notify)),
            };
            match Pane::from_source(&self.config, seed, None, self.lua_tx.clone()) {
                Ok(pane) => self.panes.push(pane),
                Err(error) => {
                    eprintln!("[panes] {error}");
                    // The window could not wrap the block: close the pane server-side too
                    self.conn.send_op(wire::WireOp::Close { pane: pane_id });
                }
            }
        }
        self.refresh_title();
        self.relayout_window();
    }

    fn handle_key(&mut self, event_loop: &ActiveEventLoop, key_event: &KeyEvent) {
        if key_event.state != ElementState::Pressed {
            return;
        }
        self.reset_blink();

        // The prefix goes first: neither pressing it nor the key that follows reaches the shell
        let outcome = self.script.leader_step(
            &mut self.leader,
            &key_event.logical_key,
            self.mod_bits,
            key_event.repeat,
            Instant::now(),
        );
        match outcome {
            LeaderOutcome::Normal => {}
            LeaderOutcome::Ignore => return,
            LeaderOutcome::Arm | LeaderOutcome::Cancel => {
                // Changes (or restores) the border color that signals the prefix
                self.redraw_window();
                return;
            }
            LeaderOutcome::Passthrough(bytes) => {
                self.clear_selection();
                self.write_to_pty(&bytes);
                self.redraw_window();
                return;
            }
            LeaderOutcome::Run(action) => {
                self.redraw_window();
                self.perform(event_loop, action);
                return;
            }
        }

        // Keys bound in config.lua go before the encoder: the keypress does not reach the shell
        if let Some(action) = self
            .script
            .binding(&key_event.logical_key, self.mod_bits)
            .cloned()
        {
            self.perform(event_loop, action);
            return;
        }

        let key_action = if key_event.repeat {
            KeyAction::Repeat
        } else {
            KeyAction::Press
        };
        let key = match key_event.physical_key {
            PhysicalKey::Code(code) => map_key(code),
            _ => Key::Unidentified,
        };
        // Ghostty does not want control characters in the text: it derives them from `key` + `mods`
        let text = key_event
            .text
            .as_deref()
            .filter(|t| !t.chars().any(char::is_control));

        let Ok(mut event) = GhosttyKeyEvent::new() else {
            return;
        };
        event
            .set_action(key_action)
            .set_key(key)
            .set_mods(self.mods)
            .set_utf8(text);

        let focus = self.focused_index();
        self.key_encoder
            .set_options_from_terminal(&self.panes[focus].terminal);
        let mut seq = Vec::new();
        let encoded = self.key_encoder.encode_to_vec(&event, &mut seq).is_ok() && !seq.is_empty();

        if encoded {
            self.clear_selection();
            self.write_to_pty(&seq);
        } else if let Some(raw) = key_event.text.as_deref() {
            // Unmapped keys (e.g. Ctrl+C arrives as "\x03"): sent as-is
            self.clear_selection();
            self.write_to_pty(raw.as_bytes());
        }
    }

    /// Writes to the focused pane's shell input
    fn write_to_pty(&mut self, bytes: &[u8]) {
        self.focused_mut().write(bytes);
    }

    /// Runs an action from a key, a hook (`ctx:perform`), or a Lua function
    fn perform(&mut self, event_loop: &ActiveEventLoop, action: Action) {
        match action {
            Action::SendString { text } => {
                self.clear_selection();
                self.write_to_pty(text.as_bytes());
            }
            Action::ScrollLines { amount } => self.scroll(ScrollViewport::Delta(amount)),
            Action::ScrollPages { amount } => {
                // A page is the visible rows
                let page = self.focused().grid.1 as isize;
                self.scroll(ScrollViewport::Delta(amount.saturating_mul(page)));
            }
            Action::ScrollToTop => self.scroll(ScrollViewport::Top),
            Action::ScrollToBottom => self.scroll(ScrollViewport::Bottom),
            Action::IncreaseFontSize => self.set_font_delta(self.font_delta + 1.0),
            Action::DecreaseFontSize => self.set_font_delta(self.font_delta - 1.0),
            Action::ResetFontSize => self.set_font_delta(0.0),
            Action::ToggleFullscreen => {
                if let Some(window) = &self.window {
                    let next = match window.fullscreen() {
                        Some(_) => None,
                        None => Some(Fullscreen::Borderless(None)),
                    };
                    window.set_fullscreen(next);
                }
            }
            Action::ReloadConfig => self.reload_config(event_loop),
            Action::Paste => self.paste(),
            Action::Copy => self.copy_selection(),
            Action::ClearSelection => self.clear_selection(),
            Action::SelectAll => self.select_all(),
            Action::SplitRight => self.split(Axis::Horizontal),
            Action::SplitDown => self.split(Axis::Vertical),
            Action::ClosePane => {
                let id = self.layout.focus();
                self.close_pane(id);
            }
            Action::FocusPane { dir } => self.focus_direction(dir),
            Action::FocusNext => self.focus_cycle(true),
            Action::FocusPrevious => self.focus_cycle(false),
            Action::ResizePane { dir, amount } => self.resize_pane(dir, amount),
            Action::ToggleZoom => self.conn.send_op(wire::WireOp::ToggleZoom),
            Action::NewFloat {
                program,
                args,
                width,
                height,
                cwd,
                env,
            } => self.new_float(
                program.map(|program| (program, args)),
                width,
                height,
                cwd,
                env,
            ),
            Action::ToggleFloats => self.toggle_floats(),
            Action::MovePane { dir, amount } => self.move_pane(dir, amount),
            Action::Quit => {
                for pane in &mut self.panes {
                    pane.kill();
                }
                event_loop.exit();
            }
            Action::Lua(function) => {
                let commands = self.script.call(&function, Vec::new());
                self.run_commands(event_loop, commands);
            }
        }
    }

    /// Runs what a Lua function requested, once its call has finished
    fn run_commands(&mut self, event_loop: &ActiveEventLoop, commands: Vec<Command>) {
        self.in_hook = true;
        for command in commands {
            match command {
                Command::SendText(text) => self.write_to_pty(text.as_bytes()),
                Command::Log(message) => eprintln!("[lua] {message}"),
                Command::Perform(action) => self.perform(event_loop, action),
            }
        }
        self.in_hook = false;
    }

    /// Calls an event's hook, if the configuration defines it
    fn fire_event(&mut self, event_loop: &ActiveEventLoop, kind: EventKind, args: Vec<Value>) {
        if self.in_hook || !self.script.has_handler(kind) {
            return;
        }
        let commands = self.script.fire(kind, args);
        self.run_commands(event_loop, commands);
    }

    /// Dispatches what happened inside the terminals during `vt_write`
    fn dispatch_terminal_events(&mut self, event_loop: &ActiveEventLoop) {
        while let Ok((id, event)) = self.lua_events.try_recv() {
            match event {
                LuaEvent::TitleChanged(title) => {
                    // Usual terminal behavior: the window shows the title set by the focused pane's
                    // program, unless config.lua fixes one with `window.title`
                    if self.config.window.title.is_none()
                        && id == self.layout.focus()
                        && let Some(window) = &self.window
                    {
                        window.set_title(&title);
                    }
                    let args = vec![self.script.text(&title)];
                    self.fire_event(event_loop, EventKind::TitleChanged, args);
                }
                LuaEvent::Bell => self.fire_event(event_loop, EventKind::Bell, Vec::new()),
            }
        }
    }

    /// Scrolls the focused pane's history
    fn scroll(&mut self, scroll: ScrollViewport) {
        let index = self.focused_index();
        self.scroll_pane(index, scroll);
    }

    fn scroll_pane(&mut self, index: usize, scroll: ScrollViewport) {
        self.panes[index].terminal.scroll_viewport(scroll);
        self.redraw_window();
    }

    /// Pastes the clipboard, using bracketed paste if the program enabled it. libghostty
    /// sanitizes control bytes in the pasted text.
    fn paste(&mut self) {
        let text = match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.get_text()) {
            Ok(text) if !text.is_empty() => text,
            Ok(_) => return,
            Err(error) => {
                eprintln!("[paste] could not read the clipboard: {error}");
                return;
            }
        };
        self.clear_selection();
        let bracketed = self
            .focused()
            .terminal
            .mode(Mode::BRACKETED_PASTE)
            .unwrap_or(false);
        let mut data = text.into_bytes();
        let mut buffer = vec![0u8; data.len() + 32];
        loop {
            match paste::encode(&mut data, bracketed, &mut buffer) {
                Ok(written) => {
                    buffer.truncate(written);
                    self.write_to_pty(&buffer);
                    return;
                }
                Err(libghostty_vt::Error::OutOfSpace { required }) => {
                    let grown = required.max(buffer.len() + 1);
                    buffer.resize(grown, 0);
                }
                Err(error) => {
                    eprintln!("[paste] could not encode the text: {error}");
                    return;
                }
            }
        }
    }

    /// Changes the font size relative to the configured one (in logical pixels)
    fn set_font_delta(&mut self, delta: f32) {
        let Some(renderer) = &mut self.renderer else {
            return;
        };
        let mut settings = self.config.clone();
        settings.font.size = (self.config.font.size + delta).clamp(4.0, 200.0);
        let before = (renderer.cell_size(), renderer.padding());
        if let Err(error) = renderer.reconfigure(&settings) {
            eprintln!("[config] {error}");
            return;
        }
        self.font_delta = delta;
        if before != (renderer.cell_size(), renderer.padding()) {
            for pane in &mut self.panes {
                pane.grid = (0, 0);
            }
        }
        if let Some(size) = self.window.as_ref().map(|window| window.inner_size()) {
            self.resize(size.width, size.height);
        }
    }

    /// Rereads config.lua. If it has errors the previous configuration is kept and a warning is
    /// shown in the window title and on stderr.
    fn reload_config(&mut self, event_loop: &ActiveEventLoop) {
        let Some(path) = self.config_path.clone() else {
            return;
        };
        let result = config::load_script_path(&path)
            .and_then(|(config, script)| self.apply_config(config, script));
        match result {
            Ok(()) => {
                eprintln!("[config] reloaded");
                self.fire_event(event_loop, EventKind::ConfigReloaded, Vec::new());
            }
            Err(error) => {
                eprintln!("[config] {error} (keeping the previous configuration)");
                let first_line = error.lines().next().unwrap_or(&error);
                if let Some(window) = &self.window {
                    window.set_title(&format!(
                        "{} [config error: {first_line}]",
                        self.window_title()
                    ));
                }
            }
        }
    }

    /// Applies a new configuration live: font, size, padding, colors, cursor, and title.
    /// Scrollback, initial window size, and shell are only read at startup.
    fn apply_config(&mut self, new: Config, script: Script) -> Result<(), String> {
        // The first thing that can fail (a font that does not exist) goes before changing anything
        let metrics_changed = match &mut self.renderer {
            Some(renderer) => {
                let before = (renderer.cell_size(), renderer.padding());
                renderer.reconfigure(&new)?;
                before != (renderer.cell_size(), renderer.padding())
            }
            None => false,
        };

        for pane in &mut self.panes {
            apply_theme(&mut pane.terminal, &new);
        }
        // The title bar is removed or added live (the usable area size changes and the
        // `Resized` that follows readjusts the grid)
        if new.window.decorations != self.config.window.decorations
            && let Some(window) = &self.window
        {
            let decorated = new.window.decorations == config::Decorations::Full;
            window.set_decorations(decorated);
            #[cfg(windows)]
            window.set_undecorated_shadow(!decorated);
        }
        #[cfg(windows)]
        if new.window.rounded_corners != self.config.window.rounded_corners
            && let Some(window) = &self.window
        {
            window.set_corner_preference(corner_preference(new.window.rounded_corners));
        }
        if new.scrollback != self.config.scrollback
            || new.window.columns != self.config.window.columns
            || new.window.rows != self.config.window.rows
            || new.shell != self.config.shell
        {
            eprintln!("[config] scrollback, window.columns/rows and shell only apply at startup");
        }
        self.config = new;
        // The new keys and hooks replace the previous ones (with their Lua interpreter), and
        // the font size goes back to the configured one
        self.script = script;
        self.leader.clear();
        self.font_delta = 0.0;
        self.blink.set_interval(
            Duration::from_millis(u64::from(self.config.cursor.blink_ms)),
            Instant::now(),
        );
        if let Some(window) = &self.window {
            window.set_title(&self.window_title());
        }

        // If the cell changed, rows and columns must be recomputed and the PTYs notified
        if metrics_changed {
            for pane in &mut self.panes {
                pane.grid = (0, 0);
            }
        }
        if let Some(size) = self.window.as_ref().map(|window| window.inner_size()) {
            self.resize(size.width, size.height);
        }
        Ok(())
    }

    /// Feeds libghostty what a pane's shell wrote, answers its queries, notifies the
    /// Lua hooks, and requests a redraw
    fn drain_pty(&mut self, event_loop: &ActiveEventLoop, id: PaneId) {
        let Some(index) = self.index_of(id) else {
            return;
        };
        let chunks = self.panes[index].drain(MAX_CHUNKS_PER_WAKE);
        // Title and bell, now that `vt_write` has finished
        self.dispatch_terminal_events(event_loop);

        if chunks == MAX_CHUNKS_PER_WAKE {
            // There may be more: signal again to let other events through in between
            self.panes[index].request_more();
        }
        // If the terminal's internal state changed, redraw the frame
        if chunks > 0 {
            if id == self.layout.focus() {
                self.blink.reset(Instant::now());
            }
            self.redraw_window();
        }
    }

    fn resize(&mut self, width: u32, height: u32) {
        // Minimizing the window reports 0×0: nothing to draw or reflow
        if width == 0 || height == 0 {
            return;
        }
        let Some(renderer) = &mut self.renderer else {
            return;
        };
        renderer.resize(width, height);
        self.relayout(Rect::new(0, 0, width, height));
    }

    /// Relayouts the panes with the current window size
    fn relayout_window(&mut self) {
        let Some(size) = self.window.as_ref().map(|window| window.inner_size()) else {
            return;
        };
        if size.width > 0 && size.height > 0 {
            self.relayout(Rect::new(0, 0, size.width, size.height));
        }
    }

    /// Divides `area` among the visible panes and, for each one whose column and row count
    /// changed, notifies its PTY and libghostty (which reflows the buffer)
    fn relayout(&mut self, area: Rect) {
        let Some(renderer) = &self.renderer else {
            return;
        };
        let (cell_width, cell_height) = renderer.cell_size();
        let mut placed = self.layout.rects(area);
        placed.extend(self.layout.float_rects(area));
        for (id, rect) in placed {
            let Some(pane) = self.panes.iter_mut().find(|pane| pane.id == id) else {
                continue;
            };
            pane.rect = rect;
            let (cols, rows) = renderer.grid_size(rect.w, rect.h);
            if (cols, rows) != pane.grid {
                pane.resize(cols, rows, cell_width, cell_height);
            }
        }
        self.redraw_window();
    }

    fn render(&mut self) {
        let focus = self.layout.focus();
        let window_focused = self.focused;
        // The views, bottom to top: the tiled layout, then the floating panes. Only when there
        // are several is the focused pane (border) distinguished from the rest (dimmed text).
        let order = self.layout.visible_ids();
        let several = order.len() > 1;
        let leader_pending = self.leader.pending();
        let floating: Vec<PaneId> = self
            .panes
            .iter()
            .map(|pane| pane.id)
            .filter(|&id| self.layout.is_float(id))
            .collect();
        let dividers = self.divider_rects();
        let Some(renderer) = &mut self.renderer else {
            return;
        };
        let mut views: Vec<(usize, PaneView)> = self
            .panes
            .iter_mut()
            .filter_map(|pane| {
                let position = order.iter().position(|&id| id == pane.id)?;
                Some((
                    position,
                    PaneView {
                        terminal: &pane.terminal,
                        render_state: &mut pane.render_state,
                        rect: pane.rect,
                        focused: window_focused && pane.id == focus,
                        dim: several && pane.id != focus,
                        // With the prefix pressed there is a border even with a single pane
                        frame: (several || leader_pending) && pane.id == focus,
                        leader: leader_pending && pane.id == focus,
                        floating: floating.contains(&pane.id),
                    },
                ))
            })
            .collect();
        views.sort_by_key(|(position, _)| *position);
        let mut views: Vec<PaneView> = views.into_iter().map(|(_, view)| view).collect();
        self.cursor_blinks = renderer.draw(&mut views, &dividers, self.blink.visible());
    }

    /// A pane's program exited: drain its last output, fire the hook, and close or hold
    fn shell_exited(&mut self, event_loop: &ActiveEventLoop, id: PaneId) {
        // A pane that was already closed by hand has nothing left to do
        let Some(index) = self.index_of(id) else {
            return;
        };
        // The last thing the shell wrote before exiting
        self.drain_pty(event_loop, id);
        self.fire_event(event_loop, EventKind::ShellExit, Vec::new());
        match self.config.on_shell_exit {
            config::ShellExit::Close => self.close_pane(id),
            config::ShellExit::Hold => {
                self.panes[index]
                    .terminal
                    .vt_write(b"\r\n\x1b[2m[process exited]\x1b[0m\r\n");
                self.redraw_window();
            }
        }
    }

    /// The cursor becomes visible again and the blink cycle restarts: while typing or while
    /// output arrives it stays solid
    fn reset_blink(&mut self) {
        if self.blink.reset(Instant::now())
            && let Some(window) = &self.window
        {
            window.request_redraw();
        }
    }
}

fn layout_dir(direction: Direction) -> layout::Dir {
    match direction {
        Direction::Left => layout::Dir::Left,
        Direction::Right => layout::Dir::Right,
        Direction::Up => layout::Dir::Up,
        Direction::Down => layout::Dir::Down,
    }
}

fn map_key(code: KeyCode) -> Key {
    match code {
        KeyCode::Enter | KeyCode::NumpadEnter => Key::Enter,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Tab => Key::Tab,
        KeyCode::Escape => Key::Escape,
        KeyCode::Delete => Key::Delete,
        KeyCode::Insert => Key::Insert,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::ArrowUp => Key::ArrowUp,
        KeyCode::ArrowDown => Key::ArrowDown,
        KeyCode::ArrowLeft => Key::ArrowLeft,
        KeyCode::ArrowRight => Key::ArrowRight,
        _ => Key::Unidentified,
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            let decorated = self.config.window.decorations == config::Decorations::Full;
            let attrs = Window::default_attributes()
                .with_title(self.window_title())
                .with_decorations(decorated)
                .with_window_icon(window_icon(64));
            // The taskbar uses a large icon separate from the title bar one
            #[cfg(windows)]
            let attrs = attrs.with_taskbar_icon(window_icon(256));
            // macOS ignores the window icon: the Dock uses the application's
            #[cfg(target_os = "macos")]
            set_dock_icon();
            // Without a title bar, the system shadow separates the window from the background
            #[cfg(windows)]
            let attrs = attrs
                .with_undecorated_shadow(!decorated)
                .with_corner_preference(corner_preference(self.config.window.rounded_corners));
            let window = Arc::new(event_loop.create_window(attrs).unwrap());
            let renderer =
                Renderer::new(window.clone(), &self.config).expect("Failed to initialize wgpu");

            // If the configuration asks for a size in columns and rows, convert it to pixels using
            // the real cell size. It is a request: the window manager may ignore it.
            let mut size = window.inner_size();
            let window_config = &self.config.window;
            if window_config.columns.is_some() || window_config.rows.is_some() {
                let (cell_width, cell_height) = renderer.cell_size();
                let padding = 2 * renderer.padding();
                let columns = u32::from(window_config.columns.unwrap_or(DEFAULT_COLUMNS));
                let rows = u32::from(window_config.rows.unwrap_or(DEFAULT_ROWS));
                let wanted =
                    PhysicalSize::new(columns * cell_width + padding, rows * cell_height + padding);
                size = window.request_inner_size(wanted).unwrap_or(size);
            }

            self.renderer = Some(renderer);
            self.window = Some(window);
            // Fit the PTY and the grid to the real window size
            self.resize(size.width, size.height);

            // Panes whose program exited while this window was detached (the attach reply
            // carried them): mark them dead now that the event loop can act on it
            let exited = std::mem::take(&mut self.attach_exited);
            for id in exited {
                self.shell_exited(event_loop, id);
            }
        }
    }

    /// When the timer deadline expires, toggles the blink phase
    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: StartCause) {
        if !matches!(cause, StartCause::ResumeTimeReached { .. }) {
            return;
        }
        let now = Instant::now();
        if self.cursor_blinks
            && self.focused
            && self.blink.tick(now)
            && let Some(window) = &self.window
        {
            window.request_redraw();
        }
        // The prefix expires if the next key does not arrive
        if self.leader.expire(now) {
            self.redraw_window();
        }
        self.tick_autoscroll(now);
    }

    /// With nothing to do, the loop sleeps until the next event. It only wakes earlier if
    /// a deadline is pending: the next phase change of a blinking cursor (with the window
    /// focused) or the next step of a selection's autoscroll.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let blink = (self.cursor_blinks && self.focused).then(|| self.blink.deadline());
        let next = [
            blink,
            self.mouse.autoscroll_deadline,
            self.leader.deadline(),
        ]
        .into_iter()
        .flatten()
        .min();
        event_loop.set_control_flow(match next {
            Some(deadline) => ControlFlow::WaitUntil(deadline),
            None => ControlFlow::Wait,
        });
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Layout(WireEventKind::State { state, created }) => {
                self.apply_state(state, created);
            }
            UserEvent::Layout(WireEventKind::LastPaneClosed) => {
                for pane in &mut self.panes {
                    pane.kill();
                }
                event_loop.exit();
            }
            UserEvent::Layout(WireEventKind::Error { message }) => eprintln!("[panes] {message}"),
            UserEvent::Layout(WireEventKind::PaneOutput { pane: id }) => {
                self.drain_pty(event_loop, id)
            }
            UserEvent::Layout(WireEventKind::PaneExited { pane: id }) => {
                self.shell_exited(event_loop, id)
            }
            // Gating is server-side: a viewer's mutating ops are dropped there, so there is
            // nothing to change locally (it keeps rendering and sending ops)
            UserEvent::Layout(WireEventKind::DriverChanged { .. }) => {}
            // A gateway-only notice (another remote client holds the single remote slot): a
            // window never receives one, so there is nothing to do
            UserEvent::Layout(WireEventKind::Busy) => {}
            UserEvent::ConfigChanged => self.reload_config(event_loop),
        }
    }

    fn device_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _device_id: DeviceId,
        event: DeviceEvent,
    ) {
        // Some setups (window managers, event taps) eat the window-level mouse-up, so
        // AppKit never routes `WindowEvent::MouseInput Released` to the view and a
        // dragged divider or selection would follow the pointer forever. The device-level
        // button event is dispatched from NSApplication's sendEvent before window
        // routing, so it survives: treat a device-level left release (button 0) as the
        // authoritative release. Both channels fire for one physical release; the second
        // call is a no-op.
        if let DeviceEvent::Button {
            button: 0,
            state: ElementState::Released,
        } = event
        {
            self.on_left_released();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                // The shells and the layout outlive the window: closing ends this session and
                // leaves the blocks running headless; re-attaching rebuilds the projection
                event_loop.exit();
            }

            WindowEvent::ModifiersChanged(modifiers) => {
                let state = modifiers.state();
                let mut mods = Mods::empty();
                mods.set(Mods::SHIFT, state.shift_key());
                mods.set(Mods::CTRL, state.control_key());
                mods.set(Mods::ALT, state.alt_key());
                mods.set(Mods::SUPER, state.super_key());
                self.mods = mods;

                let mut bits = 0;
                for (pressed, bit) in [
                    (state.control_key(), actions::CTRL),
                    (state.shift_key(), actions::SHIFT),
                    (state.alt_key(), actions::ALT),
                    (state.super_key(), actions::SUPER),
                ] {
                    if pressed {
                        bits |= bit;
                    }
                }
                self.mod_bits = bits;
            }

            // Capture the user's physical keyboard input
            WindowEvent::KeyboardInput { event, .. } => self.handle_key(event_loop, &event),

            // Handle coordinated resizing
            WindowEvent::Resized(size) => self.resize(size.width, size.height),

            // A monitor with a different DPI changes the font size and, with it, the cell size
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.set_scale_factor(scale_factor);
                }
                for pane in &mut self.panes {
                    pane.grid = (0, 0);
                }
                if let Some(size) = self.window.as_ref().map(|window| window.inner_size()) {
                    self.resize(size.width, size.height);
                }
            }

            // Without focus the cursor is drawn hollow
            WindowEvent::Focused(focused) => {
                self.focused = focused;
                self.blink.reset(Instant::now());
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }

            WindowEvent::CursorMoved { position, .. } => self.on_cursor_moved(position),

            WindowEvent::MouseInput { state, button, .. } => {
                if state == ElementState::Pressed {
                    self.reset_blink();
                }
                self.on_mouse_button(state, button);
            }

            WindowEvent::MouseWheel { delta, .. } => self.on_mouse_wheel(delta),

            WindowEvent::RedrawRequested => self.render(),

            _ => {}
        }
    }
}

fn main() {
    // 0. The subcommands: `terminal server` runs the headless server process, `terminal
    //    gateway` fronts it for remote clients over the tailnet, and the automation CLI
    //    subcommands are short-lived clients of a running server (stage 6); the bare
    //    binary is the window
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("server") => {
            server::run_server_process();
            unreachable!("the server process serves until it is killed");
        }
        Some("gateway") => {
            // The allow-list lives in the configuration (empty denies everyone)
            let loaded = config::load();
            if let Some(error) = &loaded.error {
                eprintln!("[config] {error}");
            }
            // Same endpoint resolution as the window: connect, or start the server detached
            // and retry
            let endpoint = IpcEndpoint::resolve();
            ensure_server_running(&endpoint);
            server::run_gateway_process(loaded.config.remote.allow, gateway_port(&args));
            unreachable!("the gateway serves until it is killed");
        }
        Some("ls") => {
            cli_ls();
            return;
        }
        Some("send") => {
            cli_send(&args[2..]);
            return;
        }
        Some("capture") => {
            cli_capture(&args[2..]);
            return;
        }
        Some("attach") => {
            cli_attach(&args[2..]);
            return;
        }
        // Reachable for future subcommands only; today the binary is the window, the
        // server, the gateway and the automation CLI
        Some(other) => {
            eprintln!(
                "unknown subcommand {other:?}; usage: terminal [server|gateway|ls|send|capture|attach]"
            );
            std::process::exit(2);
        }
        None => {}
    }

    // 1. Load config.lua. If it fails, defaults are used and the error is written to the first
    //    pane's grid once its terminal exists.
    let loaded = config::load();
    if let Some(error) = &loaded.error {
        eprintln!("[config] {error}");
    }
    let config = loaded.config;

    // 2. Set the initial dimensions (`resumed` later fits them to the real window size)
    let cols = config.window.columns.unwrap_or(DEFAULT_COLUMNS);
    let rows = config.window.rows.unwrap_or(DEFAULT_ROWS);

    // 3. Create the winit event loop (the window is created in `resumed`). It sleeps until
    //    an event arrives; the connection's threads wake it through the proxy.
    let event_loop = EventLoop::<UserEvent>::with_user_event().build().unwrap();
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();

    // 4. The server is its own process: connect (starting it detached if it is not running),
    //    and attach. The connection's reader thread forwards events to the UI loop; it also
    //    answers the asks (attach, the working directory) by rid.
    let (lua_tx, lua_events) = channel::<(PaneId, LuaEvent)>();
    let endpoint = IpcEndpoint::resolve();
    let (conn, read) = connect_or_start(&endpoint).unwrap_or_else(|error| panic!("{error}"));
    {
        let conn = conn.clone();
        let reader_proxy = proxy.clone();
        thread::spawn(move || {
            conn.read_loop(read, move |down| match down {
                ControlDown::Event(event) => {
                    let _ = reader_proxy.send_event(UserEvent::Layout(event));
                }
                ControlDown::Reply(reply) => {
                    // The attach and the directory asks are correlated by rid; nothing
                    // uncorrelated is expected anymore (the busy gate is gone: several
                    // windows attach at once)
                    if reply.v != wire::WIRE_VERSION {
                        eprintln!(
                            "[client] the server speaks protocol version {}; dropping it",
                            reply.v
                        );
                        std::process::exit(1);
                    }
                }
            });
        });
    }
    let shell = with_integration(config.shell.clone());
    let answer = conn
        .attach(&shell, cols, rows, config.scrollback)
        .unwrap_or_else(|error| panic!("{error}"));
    let (state, panes, attach_exited) = match answer {
        AttachAnswer::Fresh { state, pane } => {
            let notify = output_notify(proxy.clone(), pane.pane);
            let seed = PaneSeed {
                pane: pane.pane,
                cols: pane.cols,
                rows: pane.rows,
                notice: pane.notice,
                io: Box::new(conn.open_block(pane.block, notify)),
            };
            let pane = Pane::from_source(
                &config,
                seed,
                loaded.error.as_deref().map(|error| ("config", error)),
                lua_tx.clone(),
            )
            .unwrap_or_else(|error| panic!("{error}"));
            (state, vec![pane], Vec::new())
        }
        AttachAnswer::Existing {
            state,
            panes: list,
            exited,
        } => {
            // Every live pane of the session rebuilds from the answer (its snapshot arrives on
            // the data channel, before any of the block's live chunks)
            let mut built = Vec::new();
            for info in list {
                let notify = output_notify(proxy.clone(), info.pane);
                let seed = PaneSeed {
                    pane: info.pane,
                    cols: info.cols,
                    rows: info.rows,
                    notice: info.notice,
                    io: Box::new(conn.open_block(info.block, notify)),
                };
                match Pane::from_source(&config, seed, None, lua_tx.clone()) {
                    Ok(pane) => built.push(pane),
                    Err(error) => {
                        eprintln!("[panes] {error}");
                        conn.send_op(wire::WireOp::Close { pane: info.pane });
                    }
                }
            }
            // Panes whose program exited while this window was detached: the event loop marks
            // them dead once it can act on it (drain, hook, close-or-hold)
            (state, built, exited)
        }
    };

    // The configuration file is watched so it is reloaded on save
    if let Some(path) = &loaded.path {
        watch_config(path.clone(), proxy.clone());
    }

    let blink = Blink::new(
        Duration::from_millis(u64::from(config.cursor.blink_ms)),
        Instant::now(),
    );
    let mut app = App {
        config,
        config_path: loaded.path,
        script: loaded.script,
        lua_events,
        lua_tx,
        in_hook: false,
        mod_bits: 0,
        font_delta: 0.0,
        blink,
        cursor_blinks: false,
        mouse: MouseState::new().expect("Failed to initialize the mouse"),
        window: None,
        renderer: None,
        focused: true,
        panes,
        layout: Layout::from_state(state),
        leader: LeaderState::default(),
        key_encoder: Encoder::new().unwrap(),
        mods: Mods::empty(),
        conn: conn.clone(),
        server: OpSender::new(conn),
        proxy,
        attach_exited,
    };

    // 5. Main event loop
    let _ = event_loop.run_app(&mut app);
}

/// One CLI connection: connects (starting the server detached when none answers) and spawns
/// the reader thread. The CLI's correlated asks answer through `IpcConn`'s pending map; the
/// reader callback stays empty unless a subcommand needs events (attach).
fn cli_conn() -> IpcConn {
    let endpoint = IpcEndpoint::resolve();
    let (conn, read) = connect_or_start(&endpoint).unwrap_or_else(|error| {
        eprintln!("terminal: {error}");
        std::process::exit(1);
    });
    {
        let conn = conn.clone();
        thread::spawn(move || conn.read_loop(read, |_down| {}));
    }
    conn
}

/// A pane id argument of a CLI subcommand
fn parse_pane_id(arg: &str) -> u32 {
    arg.parse().unwrap_or_else(|_| {
        eprintln!("terminal: {arg:?} is not a pane id (see `terminal ls`)");
        std::process::exit(2);
    })
}

/// `terminal ls`: the live panes as a table, alphabetical (numeric) by pane
fn cli_ls() {
    let conn = cli_conn();
    let panes = conn.automation_list().unwrap_or_else(|error| {
        eprintln!("terminal ls: {error}");
        std::process::exit(1);
    });
    println!("{}", ls_table(&panes));
}

/// `terminal send <pane> <text...>`: writes the text to the pane's program, without a trailing
/// newline (a shell keeps waiting for Enter unless the caller adds one, e.g. `$'echo hi\n'`)
fn cli_send(args: &[String]) {
    let (pane, text) = match args {
        [pane, rest @ ..] if !rest.is_empty() => (parse_pane_id(pane), rest.join(" ")),
        _ => {
            eprintln!("usage: terminal send <pane> <text...>");
            std::process::exit(2);
        }
    };
    let conn = cli_conn();
    let panes = conn.automation_list().unwrap_or_else(|error| {
        eprintln!("terminal send: {error}");
        std::process::exit(1);
    });
    let (_, block) = resolve_pane(&panes, pane).unwrap_or_else(|error| {
        eprintln!("terminal send: {error}");
        std::process::exit(1);
    });
    conn.automation_send_checked(block, text.as_bytes())
        .unwrap_or_else(|error| {
            eprintln!("terminal send: {error}");
            std::process::exit(1);
        });
}

/// `terminal capture <pane> [--vt]`: the pane's screen to stdout — plain text by default, the
/// raw VT snapshot with `--vt`. A dead pane is refused: its screen is still served server-side,
/// but `capture` mirrors `send` in refusing work on panes whose program has exited.
fn cli_capture(args: &[String]) {
    let mut pane = None;
    let mut vt = false;
    for arg in args {
        match arg.as_str() {
            "--vt" => vt = true,
            _ if pane.is_none() => pane = Some(parse_pane_id(arg)),
            _ => {
                eprintln!("usage: terminal capture <pane> [--vt]");
                std::process::exit(2);
            }
        }
    }
    let Some(pane) = pane else {
        eprintln!("usage: terminal capture <pane> [--vt]");
        std::process::exit(2);
    };
    let conn = cli_conn();
    let panes = conn.automation_list().unwrap_or_else(|error| {
        eprintln!("terminal capture: {error}");
        std::process::exit(1);
    });
    let (_, block) = resolve_pane(&panes, pane).unwrap_or_else(|error| {
        eprintln!("terminal capture: {error}");
        std::process::exit(1);
    });
    if vt {
        let bytes = conn.snapshot(block).unwrap_or_else(|error| {
            eprintln!("terminal capture: {error}");
            std::process::exit(1);
        });
        let _ = std::io::stdout().write_all(&bytes);
    } else {
        let text = conn.automation_capture(block).unwrap_or_else(|error| {
            eprintln!("terminal capture: {error}");
            std::process::exit(1);
        });
        let _ = std::io::stdout().write_all(text.as_bytes());
    }
}

/// `terminal attach <pane>`: subscribes the pane's block data channel (snapshot served first),
/// puts the local terminal in raw mode, forwards stdin to the program and streams its output
/// to stdout. Exits on Ctrl-C or Ctrl-D (the exact escape: 0x03/0x04 arrive as plain bytes in
/// raw mode), when the program exits, or when the server goes away. It never touches the
/// window: no attach, no layout ops, no driver. The TTY is restored on every exit path.
fn cli_attach(args: &[String]) {
    let [pane] = args else {
        eprintln!("usage: terminal attach <pane>");
        std::process::exit(2);
    };
    let pane = parse_pane_id(pane);
    let endpoint = IpcEndpoint::resolve();
    let (conn, read) = connect_or_start(&endpoint).unwrap_or_else(|error| {
        eprintln!("terminal attach: {error}");
        std::process::exit(1);
    });
    // The pane's death arrives as an event; every other event is ignored
    let (dead_tx, dead_rx) = channel::<()>();
    {
        let conn = conn.clone();
        thread::spawn(move || {
            conn.read_loop(read, move |down| match down {
                ControlDown::Event(WireEventKind::PaneExited { pane: exited })
                    if exited == PaneId(pane) =>
                {
                    let _ = dead_tx.send(());
                }
                _ => {}
            });
        });
    }
    let panes = conn.automation_list().unwrap_or_else(|error| {
        eprintln!("terminal attach: {error}");
        std::process::exit(1);
    });
    let (_, block) = resolve_pane(&panes, pane).unwrap_or_else(|error| {
        eprintln!("terminal attach: {error}");
        std::process::exit(1);
    });
    // Subscribe before claiming the feed: the snapshot is queued server-side first, and any
    // frame arriving before `open_block` lands on the orphan shelf (replayed in order)
    conn.automation_subscribe(block).unwrap_or_else(|error| {
        eprintln!("terminal attach: {error}");
        std::process::exit(1);
    });
    let (wake_tx, wake_rx) = channel::<()>();
    let notify: Notify = Arc::new(move |_| {
        let _ = wake_tx.send(());
    });
    let mut block_io = conn.open_block(block, notify);
    // Raw mode from here on; the guard restores the TTY on every exit path
    let _raw = RawGuard::new().unwrap_or_else(|error| {
        eprintln!("terminal attach: {error}");
        std::process::exit(1);
    });
    // stdin: every read is forwarded as an automation write (deliberately bypassing the
    // driver gating — this is an explicit local act); Ctrl-C/Ctrl-D and end-of-file end it
    let (stop_tx, stop_rx) = channel::<()>();
    {
        let conn = conn.clone();
        thread::spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut buf = [0u8; 4096];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        let _ = stop_tx.send(());
                        break;
                    }
                    Ok(n) => {
                        if let Some(stop) = buf[..n]
                            .iter()
                            .position(|byte| *byte == 0x03 || *byte == 0x04)
                        {
                            if stop > 0 {
                                conn.automation_send(block, &buf[..stop]);
                            }
                            let _ = stop_tx.send(());
                            break;
                        }
                        conn.automation_send(block, &buf[..n]);
                    }
                }
            }
        });
    }
    // Stream: drain the block's feed to stdout, woken by the feed's notify; the exit signals
    // are re-checked on a small cadence so nothing waits on a missing wake
    let mut stdout = std::io::stdout().lock();
    let gone =
        |rx: &Receiver<()>| matches!(rx.try_recv(), Ok(()) | Err(TryRecvError::Disconnected));
    loop {
        loop {
            block_io.rearm();
            match block_io.try_read() {
                Some(chunk) => {
                    if stdout
                        .write_all(&chunk)
                        .and_then(|_| stdout.flush())
                        .is_err()
                    {
                        return; // stdout closed: the attach is over (the guard restores the TTY)
                    }
                }
                None => break,
            }
        }
        if gone(&dead_rx) || gone(&stop_rx) {
            break;
        }
        let _ = wake_rx.recv_timeout(Duration::from_millis(50));
    }
    // The program may have written its last lines right before exiting: give the tail one
    // bounded moment, then drain before restoring the TTY
    thread::sleep(Duration::from_millis(100));
    loop {
        block_io.rearm();
        match block_io.try_read() {
            Some(chunk) => {
                let _ = stdout.write_all(&chunk);
                let _ = stdout.flush();
            }
            None => break,
        }
    }
}

/// Raw local terminal for `terminal attach`, restored on drop. Hand-rolled FFI (no new
/// dependencies), the same dependency-free style as the Windows named-pipe plumbing: termios
/// on macOS/Linux, console mode on Windows. Other unixes are refused with an error.
#[cfg(any(target_os = "macos", target_os = "linux"))]
struct RawGuard {
    saved: ffi::Termios,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl RawGuard {
    /// Puts stdin into raw mode; fails when stdin is not a terminal
    fn new() -> Result<Self, String> {
        // SAFETY: tcgetattr only reads the fd and fills the (zeroed) struct on success
        let mut saved: ffi::Termios = unsafe { std::mem::zeroed() };
        if unsafe { ffi::tcgetattr(0, &mut saved) } != 0 {
            return Err("stdin is not a terminal; attach needs one".to_string());
        }
        let mut raw = saved;
        raw.c_iflag &= !(ffi::BRKINT | ffi::ICRNL | ffi::INPCK | ffi::ISTRIP | ffi::IXON);
        raw.c_oflag &= !ffi::OPOST;
        raw.c_lflag &= !(ffi::ECHO | ffi::ICANON | ffi::IEXTEN | ffi::ISIG);
        raw.c_cc[ffi::VMIN] = 1;
        raw.c_cc[ffi::VTIME] = 0;
        // SAFETY: the struct was filled by tcgetattr just above
        if unsafe { ffi::tcsetattr(0, ffi::TCSANOW, &raw) } != 0 {
            return Err("could not put the terminal into raw mode".to_string());
        }
        Ok(Self { saved })
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl Drop for RawGuard {
    fn drop(&mut self) {
        // SAFETY: restore exactly what tcgetattr saved
        unsafe { ffi::tcsetattr(0, ffi::TCSANOW, &self.saved) };
    }
}

#[cfg(target_os = "macos")]
mod ffi {
    // macOS `struct termios` (sys/termios.h): tcflag_t and speed_t are `unsigned long`
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(crate) struct Termios {
        pub c_iflag: u64,
        pub c_oflag: u64,
        pub c_cflag: u64,
        pub c_lflag: u64,
        pub c_cc: [u8; 20],
        pub c_ispeed: u64,
        pub c_ospeed: u64,
    }
    pub(crate) const VMIN: usize = 16;
    pub(crate) const VTIME: usize = 17;
    pub(crate) const BRKINT: u64 = 0x2;
    pub(crate) const ICRNL: u64 = 0x300;
    pub(crate) const INPCK: u64 = 0x10;
    pub(crate) const ISTRIP: u64 = 0x20;
    pub(crate) const IXON: u64 = 0x400;
    pub(crate) const OPOST: u64 = 0x1;
    pub(crate) const ECHO: u64 = 0x8;
    pub(crate) const ICANON: u64 = 0x100;
    pub(crate) const IEXTEN: u64 = 0x8000;
    pub(crate) const ISIG: u64 = 0x80;
    pub(crate) const TCSANOW: i32 = 0;

    unsafe extern "C" {
        pub fn tcgetattr(fd: i32, termios: *mut Termios) -> i32;
        pub fn tcsetattr(fd: i32, actions: i32, termios: *const Termios) -> i32;
    }
}

#[cfg(target_os = "linux")]
mod ffi {
    // Linux/glibc `struct termios` (bits/termios.h): tcflag_t and speed_t are `unsigned int`
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(crate) struct Termios {
        pub c_iflag: u32,
        pub c_oflag: u32,
        pub c_cflag: u32,
        pub c_lflag: u32,
        pub c_line: u8,
        pub c_cc: [u8; 32],
        pub c_ispeed: u32,
        pub c_ospeed: u32,
    }
    pub(crate) const VMIN: usize = 6;
    pub(crate) const VTIME: usize = 5;
    pub(crate) const BRKINT: u32 = 0x2;
    pub(crate) const ICRNL: u32 = 0x100;
    pub(crate) const INPCK: u32 = 0x10;
    pub(crate) const ISTRIP: u32 = 0x20;
    pub(crate) const IXON: u32 = 0x400;
    pub(crate) const OPOST: u32 = 0x1;
    pub(crate) const ECHO: u32 = 0x8;
    pub(crate) const ICANON: u32 = 0x2;
    pub(crate) const IEXTEN: u32 = 0x8000;
    pub(crate) const ISIG: u32 = 0x1;
    pub(crate) const TCSANOW: i32 = 0;

    unsafe extern "C" {
        pub fn tcgetattr(fd: i32, termios: *mut Termios) -> i32;
        pub fn tcsetattr(fd: i32, actions: i32, termios: *const Termios) -> i32;
    }
}

#[cfg(windows)]
struct RawGuard {
    saved: u32,
    handle: isize,
}

#[cfg(windows)]
impl RawGuard {
    /// Puts the console's input into raw mode; fails when stdin is not a console
    fn new() -> Result<Self, String> {
        const STD_INPUT_HANDLE: i32 = -10;
        const ENABLE_PROCESSED_INPUT: u32 = 0x1;
        const ENABLE_LINE_INPUT: u32 = 0x2;
        const ENABLE_ECHO_INPUT: u32 = 0x4;
        const ENABLE_WINDOW_INPUT: u32 = 0x8;
        const ENABLE_MOUSE_INPUT: u32 = 0x10;
        // SAFETY: FFI per the console API, mirroring the windows_pipe plumbing
        let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        let mut saved = 0u32;
        if handle == 0 || unsafe { GetConsoleMode(handle, &mut saved) } == 0 {
            return Err("stdin is not a console; attach needs one".to_string());
        }
        let raw = saved
            & !(ENABLE_PROCESSED_INPUT
                | ENABLE_LINE_INPUT
                | ENABLE_ECHO_INPUT
                | ENABLE_WINDOW_INPUT
                | ENABLE_MOUSE_INPUT);
        if unsafe { SetConsoleMode(handle, raw) } == 0 {
            return Err("could not put the console into raw mode".to_string());
        }
        Ok(Self { saved, handle })
    }
}

#[cfg(windows)]
impl Drop for RawGuard {
    fn drop(&mut self) {
        // SAFETY: restore exactly what GetConsoleMode saved
        unsafe { SetConsoleMode(self.handle, self.saved) };
    }
}

#[cfg(windows)]
unsafe extern "system" {
    fn GetStdHandle(handle: i32) -> isize;
    fn GetConsoleMode(handle: isize, mode: *mut u32) -> i32;
    fn SetConsoleMode(handle: isize, mode: u32) -> i32;
}

// --- Automation CLI (stage 6) ---------------------------------------------------------------

/// The live pane of an automation ask, resolved for a CLI subcommand: the pane must exist and
/// its program must still be running. Errors name the pane and say why, for `terminal`'s stderr.
fn resolve_pane(panes: &[wire::AutomationPane], pane: u32) -> Result<(PaneId, BlockId), String> {
    match panes.iter().find(|entry| entry.pane == PaneId(pane)) {
        None => Err(format!(
            "unknown pane {pane}; run `terminal ls` to see the live panes"
        )),
        Some(entry) if !entry.alive => Err(format!("pane {pane}'s program has exited")),
        Some(entry) => Ok((entry.pane, entry.block)),
    }
}

/// The `terminal ls` table: `pane  block  size  state`, alphabetical (numeric) by pane
fn ls_table(panes: &[wire::AutomationPane]) -> String {
    let mut panes: Vec<_> = panes.to_vec();
    panes.sort_unstable_by_key(|pane| pane.pane);
    let pane_w = panes
        .iter()
        .map(|p| p.pane.0.to_string().len())
        .chain([4])
        .max()
        .unwrap();
    let block_w = panes
        .iter()
        .map(|p| p.block.0.to_string().len())
        .chain([5])
        .max()
        .unwrap();
    let mut lines = vec!["pane  block  size  state".to_string()];
    for p in panes {
        lines.push(format!(
            "{:<pane_w$}  {:<block_w$}  {}x{}  {}",
            p.pane.0,
            p.block.0,
            p.cols,
            p.rows,
            if p.alive { "alive" } else { "dead" },
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    fn listing(pane: u32, alive: bool) -> wire::AutomationPane {
        wire::AutomationPane {
            pane: PaneId(pane),
            block: BlockId(u64::from(pane)),
            cols: 80,
            rows: 24,
            alive,
        }
    }

    #[test]
    fn the_cli_resolves_a_pane_to_its_block() {
        let panes = vec![listing(0, true), listing(1, false)];
        assert_eq!(
            resolve_pane(&panes, 0).expect("the alive pane resolves"),
            (PaneId(0), BlockId(0))
        );
        let error = resolve_pane(&panes, 1).expect_err("a dead pane is refused");
        assert!(error.contains("exited"), "the error says why: {error}");
        let error = resolve_pane(&panes, 7).expect_err("an unknown pane is refused");
        assert!(error.contains("7"), "the error names the pane: {error}");
    }

    #[test]
    fn the_ls_table_is_sorted_and_reports_state() {
        let panes = vec![listing(3, true), listing(12, false), listing(0, true)];
        let table = ls_table(&panes);
        let lines: Vec<_> = table.lines().collect();
        assert_eq!(lines[0], "pane  block  size  state");
        assert_eq!(lines[1], "0     0      80x24  alive");
        assert_eq!(lines[2], "3     3      80x24  alive");
        assert_eq!(lines[3], "12    12     80x24  dead");
        assert_eq!(lines.len(), 4, "no trailing rows");
    }
}
