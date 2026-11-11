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
use pane::{MAX_CHUNKS_PER_WAKE, Pane, apply_theme, with_integration};
use renderer::{PaneView, Renderer};
use server::{
    AddFloatOp, CreatedPane, FocusDirOp, LayoutEvent, MoveFloatOp, Request, ResizeOp, Server,
    ServerHandle, SplitOp,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
#[cfg(windows)]
use winit::platform::windows::{CornerPreference, WindowAttributesExtWindows, WindowExtWindows};
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::{ElementState, KeyEvent, StartCause, WindowEvent},
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
    Layout(LayoutEvent),
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
    /// Owns the programs behind the panes
    server: ServerHandle,
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
        self.server.send(Request::LayoutSetFocus { pane: id });
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
        self.server.send(Request::LayoutSplit(SplitOp {
            pane: target,
            dir: match axis {
                Axis::Horizontal => layout::Dir::Right,
                Axis::Vertical => layout::Dir::Down,
            },
            shell: with_integration(shell),
            explicit: false,
            cols,
            rows,
            scrollback: self.config.scrollback,
            area,
            limits,
        }));
    }

    /// Opens a floating pane and focuses it. With `program` it runs that program instead of the
    /// shell (and the floating pane closes when it exits). `width`/`height` are window fractions.
    fn new_float(
        &mut self,
        program: Option<(String, Vec<String>)>,
        width: Option<f32>,
        height: Option<f32>,
        cwd: Option<String>,
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
        self.server.send(Request::LayoutAddFloat(AddFloatOp {
            shell: with_integration(shell),
            explicit,
            width,
            height,
            cols,
            rows,
            scrollback: self.config.scrollback,
        }));
    }

    /// Hides or shows the floating panes; if there are none, opens one
    fn toggle_floats(&mut self) {
        if !self.layout.has_floats() {
            self.new_float(None, None, None, None);
            return;
        }
        self.server.send(Request::LayoutToggleFloats);
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
        self.server.send(Request::LayoutMoveFloat(MoveFloatOp {
            pane: focus,
            dx,
            dy,
            window: area,
        }));
    }

    fn focus_direction(&mut self, direction: Direction) {
        let Some(area) = self.window_area() else {
            return;
        };
        self.server.send(Request::LayoutFocusDir(FocusDirOp {
            dir: layout_dir(direction),
            area,
        }));
    }

    fn focus_cycle(&mut self, forward: bool) {
        self.server.send(Request::LayoutFocusCycle { forward });
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
        self.server.send(Request::LayoutResize(ResizeOp {
            pane: self.layout.focus(),
            dir: layout_dir(direction),
            delta: u32::from(amount) * step,
            area,
            limits,
        }));
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
        self.server.send(Request::LayoutClose { pane: id });
    }

    /// Rebuilds the projection from a server state event and syncs the panes with it
    fn apply_state(&mut self, state: LayoutState, created: Option<CreatedPane>) {
        self.layout = Layout::from_state(state);
        // Panes the server removed are gone: dropping them kills their blocks
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
            match Pane::from_block(&self.config, created, None, self.lua_tx.clone()) {
                Ok(pane) => self.panes.push(pane),
                Err(error) => {
                    eprintln!("[panes] {error}");
                    // The window could not wrap the block: close the pane server-side too
                    self.server.send(Request::LayoutClose { pane: pane_id });
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
            Action::ToggleZoom => self.server.send(Request::LayoutToggleZoom),
            Action::NewFloat {
                program,
                args,
                width,
                height,
                cwd,
            } => self.new_float(program.map(|program| (program, args)), width, height, cwd),
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
            UserEvent::Layout(LayoutEvent::State(state, created)) => {
                self.apply_state(state, created);
            }
            UserEvent::Layout(LayoutEvent::LastPaneClosed) => {
                for pane in &mut self.panes {
                    pane.kill();
                }
                event_loop.exit();
            }
            UserEvent::Layout(LayoutEvent::Error(error)) => eprintln!("[panes] {error}"),
            UserEvent::Layout(LayoutEvent::PaneOutput(id)) => self.drain_pty(event_loop, id),
            UserEvent::Layout(LayoutEvent::PaneExited(id)) => self.shell_exited(event_loop, id),
            UserEvent::ConfigChanged => self.reload_config(event_loop),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                for pane in &mut self.panes {
                    pane.kill();
                }
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
    // 0. Load config.lua. If it fails, defaults are used and the error is written to the
    // first pane's grid once its terminal exists.
    let loaded = config::load();
    if let Some(error) = &loaded.error {
        eprintln!("[config] {error}");
    }
    let config = loaded.config;

    // 1. Set the initial dimensions (`resumed` later fits them to the real window size)
    let cols = config.window.columns.unwrap_or(DEFAULT_COLUMNS);
    let rows = config.window.rows.unwrap_or(DEFAULT_ROWS);

    // 2. Create the winit event loop (the window is created in `resumed`). It sleeps until
    // an event arrives; each pane's threads wake it through the proxy.
    let event_loop = EventLoop::<UserEvent>::with_user_event().build().unwrap();
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();

    // 3. The server owns the layout and the blocks. The first pane is created through it, and
    //    its events (layout state after every mutation, pane output and exits) wake the UI loop.
    let (lua_tx, lua_events) = channel::<(PaneId, LuaEvent)>();
    let (server, layout_events) = Server::start();
    {
        let proxy = proxy.clone();
        thread::spawn(move || {
            while let Ok(event) = layout_events.recv() {
                if proxy.send_event(UserEvent::Layout(event)).is_err() {
                    break;
                }
            }
        });
    }
    let shell = with_integration(config.shell.clone());
    let (state, created) = server
        .init(&shell, cols, rows, config.scrollback)
        .unwrap_or_else(|error| panic!("{error}"));
    let pane = Pane::from_block(
        &config,
        created,
        loaded.error.as_deref().map(|error| ("config", error)),
        lua_tx.clone(),
    )
    .unwrap_or_else(|error| panic!("{error}"));

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
        panes: vec![pane],
        layout: Layout::from_state(state),
        leader: LeaderState::default(),
        key_encoder: Encoder::new().unwrap(),
        mods: Mods::empty(),
        server,
    };

    // 4. Main event loop
    let _ = event_loop.run_app(&mut app);
}
