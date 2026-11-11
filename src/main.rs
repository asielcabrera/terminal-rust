mod actions;
mod blink;
mod config;
mod glyphs;
mod icon;
mod layout;
mod mouse;
mod pane;
#[cfg(unix)] // Windows has no API for another process' cwd; pane.rs only calls it on unix
mod proc_cwd;
mod renderer;

use actions::{Action, Command, Direction, EventKind, LeaderOutcome, LeaderState, Script};
use blink::Blink;
use config::Config;
use layout::{Axis, Closed, Layout, Limits, PaneId, Rect};
use libghostty_vt::key::{Action as KeyAction, Encoder, Event as GhosttyKeyEvent, Key, Mods};
use libghostty_vt::paste;
use libghostty_vt::terminal::{Mode, ScrollViewport};
use mlua::Value;
use mouse::MouseState;
use pane::{MAX_CHUNKS_PER_WAKE, Pane, Spawn, apply_theme};
use renderer::{PaneView, Renderer};
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

/// Tamaño de la cuadrícula cuando la configuración no pide otro
const DEFAULT_COLUMNS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

/// Tamaño mínimo de un panel, en celdas: no se divide si las dos mitades quedarían por debajo
const MIN_PANE_COLUMNS: u32 = 8;
const MIN_PANE_ROWS: u32 = 3;

/// Eventos que otros hilos mandan al bucle de la UI para despertarlo
#[derive(Debug)]
enum UserEvent {
    /// Hay bytes nuevos del shell de un panel en su canal
    PtyOutput(PaneId),
    /// El proceso hijo de un panel terminó
    ShellExited(PaneId),
    /// El archivo de configuración cambió en disco
    ConfigChanged,
}

/// Lo que pasó dentro de un terminal durante `vt_write` y puede interesar a los hooks de Lua. Se
/// encola desde los callbacks de libghostty y se despacha cuando `vt_write` ya terminó.
enum LuaEvent {
    TitleChanged(String),
    Bell,
}

/// Cada cuánto se mira si config.lua cambió, y cuánto se espera antes de leerlo para no pillar
/// un guardado a medias
const CONFIG_POLL_INTERVAL: Duration = Duration::from_millis(500);
const CONFIG_SETTLE_DELAY: Duration = Duration::from_millis(100);

fn modified_time(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
}

/// Vigila el archivo de configuración comparando su fecha de modificación. Es un solo archivo, así
/// que esto evita una dependencia como `notify`. Termina cuando el bucle de eventos se cierra.
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

/// Windows 11 redondea las esquinas de las ventanas salvo que se le pida lo contrario
#[cfg(windows)]
fn corner_preference(rounded: bool) -> CornerPreference {
    if rounded {
        CornerPreference::Default
    } else {
        CornerPreference::DoNotRound
    }
}

/// El icono de la aplicación a `size`×`size` para winit (barra de título y de tareas)
fn window_icon(size: u32) -> Option<winit::window::Icon> {
    winit::window::Icon::from_rgba(icon::rgba(size), size, size).ok()
}

/// Pone el icono en el Dock y en el selector de aplicaciones (Cmd+Tab). Sin un bundle `.app` macOS
/// muestra el de un ejecutable genérico; esto lo cambia mientras la aplicación está abierta.
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
    /// Teclas, hooks y el intérprete de Lua de la configuración actual
    script: Script,
    /// Eventos de los terminales pendientes de pasar a los hooks de Lua (y el extremo para los
    /// paneles nuevos)
    lua_events: Receiver<(PaneId, LuaEvent)>,
    lua_tx: Sender<(PaneId, LuaEvent)>,
    /// Mientras se ejecutan las órdenes de un hook no se disparan otros hooks, para que uno que
    /// provoca el evento que lo dispara (p. ej. recargar la config desde `config-reloaded`) no
    /// entre en bucle
    in_hook: bool,
    /// Modificadores pulsados (bits de `actions::CTRL`, etc.) para buscar enlaces de teclas
    mod_bits: u8,
    /// Cuánto se movió el tamaño de la fuente con las acciones de aumentar/reducir. Una recarga
    /// de la configuración lo pone a cero.
    font_delta: f32,
    /// Fase del parpadeo del cursor, y si el último frame tenía un cursor parpadeante (solo
    /// entonces hace falta despertar el bucle para alternarla)
    blink: Blink,
    cursor_blinks: bool,
    mouse: MouseState,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    focused: bool,
    /// Todos los paneles; dónde va cada uno y cuál tiene el foco lo decide `layout`
    panes: Vec<Pane>,
    layout: Layout,
    next_pane_id: u32,
    /// El prefijo (tipo Ctrl+B de tmux) está pulsado y espera la tecla siguiente
    leader: LeaderState,
    key_encoder: Encoder<'static>,
    mods: Mods,
    proxy: EventLoopProxy<UserEvent>,
}

impl App {
    fn index_of(&self, id: PaneId) -> Option<usize> {
        self.panes.iter().position(|pane| pane.id == id)
    }

    /// Posición en `panes` del panel con foco. Siempre hay uno: al cerrarse el último, la app sale.
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

    /// El título fijo de la configuración o, si no hay, el que puso el programa del panel con foco
    /// (o el de fábrica)
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

    /// Da el foco a un panel (un clic, una acción). Si el diseño cambia (un flotante sube, el zoom
    /// se quita) se recoloca todo.
    fn focus_pane(&mut self, id: PaneId) {
        if self.layout.focus() == id {
            return;
        }
        let zoomed_before = self.layout.zoomed();
        if !self.layout.set_focus(id) {
            return;
        }
        self.refresh_title();
        if self.layout.zoomed() != zoomed_before {
            self.relayout_window();
        }
        self.redraw_window();
    }

    fn redraw_window(&self) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Área de la ventana (pixeles físicos) que se reparte entre los paneles
    fn window_area(&self) -> Option<Rect> {
        let size = self.window.as_ref()?.inner_size();
        Some(Rect::new(0, 0, size.width, size.height))
    }

    /// Tamaño mínimo de un panel con el tamaño de celda y el margen actuales
    fn pane_limits(&self) -> Option<Limits> {
        let renderer = self.renderer.as_ref()?;
        let (cell_width, cell_height) = renderer.cell_size();
        let padding = 2 * renderer.padding();
        Some(Limits {
            min_w: MIN_PANE_COLUMNS * cell_width + padding,
            min_h: MIN_PANE_ROWS * cell_height + padding,
        })
    }

    /// Directorio donde arranca un panel nuevo: el del panel `from` (por OSC 7 o, en macOS y
    /// Linux, preguntándole al sistema) si `panes.inherit_cwd` lo permite; si no, el de `shell.cwd`
    fn inherited_cwd(&self, from: PaneId) -> Option<PathBuf> {
        if !self.config.panes.inherit_cwd {
            return None;
        }
        self.panes
            .iter()
            .find(|pane| pane.id == from)?
            .current_dir()
    }

    /// Divide el panel con foco (o, desde un flotante, el último del mosaico) y da el foco al nuevo
    fn split(&mut self, axis: Axis) {
        let (Some(area), Some(limits)) = (self.window_area(), self.pane_limits()) else {
            return;
        };
        let target = self.layout.split_target();
        let cwd = self.inherited_cwd(target);
        let id = PaneId(self.next_pane_id);
        if !self.layout.split(target, axis, id, area, limits) {
            eprintln!("[panes] no hay sitio para dividir el panel");
            return;
        }
        let Some(renderer) = &self.renderer else {
            return;
        };
        let rect = self
            .layout
            .rects(area)
            .into_iter()
            .find(|(pane, _)| *pane == id)
            .map(|(_, rect)| rect)
            .unwrap_or(area);
        let (cols, rows) = renderer.grid_size(rect.w, rect.h);
        let spawn = Spawn {
            id,
            cols,
            rows,
            program: None,
            cwd,
            notice: None,
        };
        match Pane::spawn(&self.config, spawn, self.lua_tx.clone(), self.proxy.clone()) {
            Ok(pane) => {
                self.next_pane_id += 1;
                self.panes.push(pane);
                self.refresh_title();
                self.relayout(area);
            }
            Err(error) => {
                eprintln!("[panes] {error}");
                // Se deshace el reparto: el panel anterior recupera su espacio y el foco
                self.layout.close(id);
            }
        }
    }

    /// Abre un panel flotante y le da el foco. Con `program` ejecuta ese programa en lugar del
    /// shell (y el flotante se cierra al terminar). `width` y `height` son fracciones de la ventana.
    fn new_float(
        &mut self,
        program: Option<(String, Vec<String>)>,
        width: Option<f32>,
        height: Option<f32>,
        cwd: Option<String>,
    ) {
        let Some(area) = self.window_area() else {
            return;
        };
        let cwd = cwd
            .map(PathBuf::from)
            .or_else(|| self.inherited_cwd(self.layout.focus()));
        let id = PaneId(self.next_pane_id);
        let panes = &self.config.panes;
        self.layout.add_float(
            id,
            width.unwrap_or(panes.float_width),
            height.unwrap_or(panes.float_height),
        );
        let Some(rect) = self.layout.float_rect(id, area) else {
            return;
        };
        let Some(renderer) = &self.renderer else {
            return;
        };
        let (cols, rows) = renderer.grid_size(rect.w, rect.h);
        let spawn = Spawn {
            id,
            cols,
            rows,
            program,
            cwd,
            notice: None,
        };
        match Pane::spawn(&self.config, spawn, self.lua_tx.clone(), self.proxy.clone()) {
            Ok(pane) => {
                self.next_pane_id += 1;
                self.panes.push(pane);
                self.refresh_title();
                self.relayout(area);
            }
            Err(error) => {
                eprintln!("[panes] {error}");
                self.layout.close(id);
                self.refresh_title();
                self.redraw_window();
            }
        }
    }

    /// Oculta o muestra los flotantes; si no hay ninguno, abre uno
    fn toggle_floats(&mut self) {
        if !self.layout.has_floats() {
            self.new_float(None, None, None, None);
            return;
        }
        self.layout.toggle_floats();
        self.refresh_title();
        // Al mostrarlos hay que ponerles el tamaño actual de la ventana
        self.relayout_window();
    }

    /// Mueve el flotante con foco `amount` celdas hacia `direction`
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
        if self.layout.move_float(focus, dx, dy, area) {
            self.relayout(area);
        }
    }

    /// Tras cambiar el foco: título, y recolocar si el zoom se quitó
    fn after_focus_change(&mut self, zoom_before: Option<PaneId>) {
        self.refresh_title();
        if self.layout.zoomed() != zoom_before {
            self.relayout_window();
        }
        self.redraw_window();
    }

    fn focus_direction(&mut self, direction: Direction) {
        let Some(area) = self.window_area() else {
            return;
        };
        let zoomed = self.layout.zoomed();
        self.layout.focus_dir(layout_dir(direction), area);
        self.after_focus_change(zoomed);
    }

    fn focus_cycle(&mut self, forward: bool) {
        let zoomed = self.layout.zoomed();
        self.layout.focus_cycle(forward);
        self.after_focus_change(zoomed);
    }

    /// Mueve el borde del panel con foco `amount` celdas hacia `direction`
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
        let focus = self.layout.focus();
        if self.layout.resize(
            focus,
            layout_dir(direction),
            u32::from(amount) * step,
            area,
            limits,
        ) {
            self.relayout(area);
        }
    }

    /// Rectángulos de las líneas que separan los paneles del mosaico
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

    /// Cierra un panel. Si era el último del mosaico, la ventana se cierra.
    fn close_pane(&mut self, event_loop: &ActiveEventLoop, id: PaneId) {
        match self.layout.close(id) {
            Closed::LastTiled => {
                for pane in &mut self.panes {
                    pane.kill();
                }
                event_loop.exit();
            }
            Closed::Pane => {
                // Al soltar el `Pane` se termina su shell si seguía vivo
                self.panes.retain(|pane| pane.id != id);
                self.mouse.forget_pane(id);
                self.refresh_title();
                self.relayout_window();
            }
            Closed::Unknown => {}
        }
    }

    fn handle_key(&mut self, event_loop: &ActiveEventLoop, key_event: &KeyEvent) {
        if key_event.state != ElementState::Pressed {
            return;
        }
        self.reset_blink();

        // El prefijo va primero: pulsarlo, o la tecla que sigue, nunca llega al shell
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
                // Cambia (o vuelve) el color del marco que indica el prefijo
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

        // Las teclas enlazadas en config.lua van antes que el encoder: la pulsación no llega al shell
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
        // Ghostty no quiere caracteres de control en el texto: los deriva de `key` + `mods`
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
            // Teclas sin mapeo (p. ej. Ctrl+C llega como "\x03"): se envían tal cual
            self.clear_selection();
            self.write_to_pty(raw.as_bytes());
        }
    }

    /// Escribe en la entrada del shell del panel con foco
    fn write_to_pty(&mut self, bytes: &[u8]) {
        self.focused_mut().write(bytes);
    }

    /// Ejecuta una acción de una tecla, de un hook (`ctx:perform`) o de una función de Lua
    fn perform(&mut self, event_loop: &ActiveEventLoop, action: Action) {
        match action {
            Action::SendString { text } => {
                self.clear_selection();
                self.write_to_pty(text.as_bytes());
            }
            Action::ScrollLines { amount } => self.scroll(ScrollViewport::Delta(amount)),
            Action::ScrollPages { amount } => {
                // Una página son las filas visibles
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
                self.close_pane(event_loop, id);
            }
            Action::FocusPane { dir } => self.focus_direction(dir),
            Action::FocusNext => self.focus_cycle(true),
            Action::FocusPrevious => self.focus_cycle(false),
            Action::ResizePane { dir, amount } => self.resize_pane(dir, amount),
            Action::ToggleZoom => {
                if self.layout.toggle_zoom() {
                    self.relayout_window();
                }
            }
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

    /// Ejecuta lo que pidió una función de Lua, ya terminada su llamada
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

    /// Llama al hook de un evento, si la configuración lo define
    fn fire_event(&mut self, event_loop: &ActiveEventLoop, kind: EventKind, args: Vec<Value>) {
        if self.in_hook || !self.script.has_handler(kind) {
            return;
        }
        let commands = self.script.fire(kind, args);
        self.run_commands(event_loop, commands);
    }

    /// Despacha lo que ocurrió dentro de los terminales durante `vt_write`
    fn dispatch_terminal_events(&mut self, event_loop: &ActiveEventLoop) {
        while let Ok((id, event)) = self.lua_events.try_recv() {
            match event {
                LuaEvent::TitleChanged(title) => {
                    // Lo habitual en un terminal: la ventana muestra el título que pone el
                    // programa del panel con foco, salvo que config.lua fije uno con `window.title`
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

    /// Desplaza el historial del panel con foco
    fn scroll(&mut self, scroll: ScrollViewport) {
        let index = self.focused_index();
        self.scroll_pane(index, scroll);
    }

    fn scroll_pane(&mut self, index: usize, scroll: ScrollViewport) {
        self.panes[index].terminal.scroll_viewport(scroll);
        self.redraw_window();
    }

    /// Pega el portapapeles con el pegado entre corchetes si el programa lo activó. libghostty
    /// sanea los bytes de control del texto pegado.
    fn paste(&mut self) {
        let text = match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.get_text()) {
            Ok(text) if !text.is_empty() => text,
            Ok(_) => return,
            Err(error) => {
                eprintln!("[paste] no se pudo leer el portapapeles: {error}");
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
                    eprintln!("[paste] no se pudo codificar el texto: {error}");
                    return;
                }
            }
        }
    }

    /// Cambia el tamaño de la fuente respecto al de la configuración (en pixeles lógicos)
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

    /// Relee config.lua. Si tiene errores se conserva la configuración anterior y se avisa en el
    /// título de la ventana y por stderr.
    fn reload_config(&mut self, event_loop: &ActiveEventLoop) {
        let Some(path) = self.config_path.clone() else {
            return;
        };
        let result = config::load_script_path(&path)
            .and_then(|(config, script)| self.apply_config(config, script));
        match result {
            Ok(()) => {
                eprintln!("[config] recargada");
                self.fire_event(event_loop, EventKind::ConfigReloaded, Vec::new());
            }
            Err(error) => {
                eprintln!("[config] {error} (se conserva la configuración anterior)");
                let first_line = error.lines().next().unwrap_or(&error);
                if let Some(window) = &self.window {
                    window.set_title(&format!(
                        "{} [error de config: {first_line}]",
                        self.window_title()
                    ));
                }
            }
        }
    }

    /// Aplica una configuración nueva en caliente: fuente, tamaño, margen, colores, cursor y título.
    /// El scrollback, el tamaño inicial de la ventana y el shell solo se leen al arrancar.
    fn apply_config(&mut self, new: Config, script: Script) -> Result<(), String> {
        // Lo primero que puede fallar (una fuente que no existe) va antes de cambiar nada
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
        // La barra de título se quita o se pone en vivo (el tamaño del área útil cambia y el
        // `Resized` que sigue reajusta la cuadrícula)
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
            eprintln!(
                "[config] scrollback, window.columns/rows y shell solo se aplican al arrancar"
            );
        }
        self.config = new;
        // Las teclas y los hooks nuevos sustituyen a los anteriores (con su intérprete de Lua), y
        // el tamaño de fuente vuelve al de la configuración
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

        // Si cambió la celda, hay que recalcular filas y columnas y avisar a los PTY
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

    /// Pasa a libghostty lo que el shell de un panel escribió, contesta sus consultas, avisa a los
    /// hooks de Lua y pide redibujar
    fn drain_pty(&mut self, event_loop: &ActiveEventLoop, id: PaneId) {
        let Some(index) = self.index_of(id) else {
            return;
        };
        let chunks = self.panes[index].drain(MAX_CHUNKS_PER_WAKE);
        // Título y campana, ya con `vt_write` terminado
        self.dispatch_terminal_events(event_loop);

        if chunks == MAX_CHUNKS_PER_WAKE {
            // Puede quedar más: se vuelve a avisar para dejar pasar otros eventos entre medias
            self.panes[index].request_more(&self.proxy);
        }
        // Si la terminal cambió de estado interno, redibujamos el frame
        if chunks > 0 {
            if id == self.layout.focus() {
                self.blink.reset(Instant::now());
            }
            self.redraw_window();
        }
    }

    fn resize(&mut self, width: u32, height: u32) {
        // Minimizar la ventana reporta 0×0: no hay nada que dibujar ni que reflujar
        if width == 0 || height == 0 {
            return;
        }
        let Some(renderer) = &mut self.renderer else {
            return;
        };
        renderer.resize(width, height);
        self.relayout(Rect::new(0, 0, width, height));
    }

    /// Recoloca los paneles con el tamaño actual de la ventana
    fn relayout_window(&mut self) {
        let Some(size) = self.window.as_ref().map(|window| window.inner_size()) else {
            return;
        };
        if size.width > 0 && size.height > 0 {
            self.relayout(Rect::new(0, 0, size.width, size.height));
        }
    }

    /// Reparte `area` entre los paneles visibles y, a cada uno cuyo número de columnas y filas
    /// cambió, avisa a su PTY y a libghostty (que hace reflow del buffer)
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
        // Las vistas, de abajo arriba: el mosaico y encima los flotantes. Solo cuando hay varios
        // se distingue el panel con foco (marco) de los demás (texto apagado).
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
                        // Con el prefijo pulsado hay marco aunque haya un solo panel
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

    /// El cursor vuelve a verse y el ciclo de parpadeo empieza de nuevo: mientras se escribe o
    /// llega salida se queda fijo
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
            // La barra de tareas usa un icono grande aparte del de la barra de título
            #[cfg(windows)]
            let attrs = attrs.with_taskbar_icon(window_icon(256));
            // macOS ignora el icono de la ventana: el del Dock es el de la aplicación
            #[cfg(target_os = "macos")]
            set_dock_icon();
            // Sin barra, la sombra del sistema es lo que separa la ventana del fondo
            #[cfg(windows)]
            let attrs = attrs
                .with_undecorated_shadow(!decorated)
                .with_corner_preference(corner_preference(self.config.window.rounded_corners));
            let window = Arc::new(event_loop.create_window(attrs).unwrap());
            let renderer =
                Renderer::new(window.clone(), &self.config).expect("Error al inicializar wgpu");

            // Si la configuración pide un tamaño en columnas y filas, se traduce a pixeles con el
            // tamaño real de la celda. Es una petición: el gestor de ventanas puede ignorarla.
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
            // Ajusta el PTY y la cuadrícula al tamaño real de la ventana
            self.resize(size.width, size.height);
        }
    }

    /// Cuando vence el plazo del temporizador, alterna la fase del parpadeo
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
        // El prefijo caduca si no llega la tecla siguiente
        if self.leader.expire(now) {
            self.redraw_window();
        }
        self.tick_autoscroll(now);
    }

    /// Sin nada que hacer, el bucle duerme hasta el siguiente evento. Solo se despierta antes si
    /// hay un plazo pendiente: el siguiente cambio de fase de un cursor parpadeante (con la ventana
    /// enfocada) o el siguiente paso del autodesplazamiento de una selección.
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
            UserEvent::PtyOutput(id) => self.drain_pty(event_loop, id),
            UserEvent::ShellExited(id) => {
                // Un panel que ya se cerró a mano no tiene nada más que hacer
                let Some(index) = self.index_of(id) else {
                    return;
                };
                // Lo último que escribió el shell antes de salir
                self.drain_pty(event_loop, id);
                self.fire_event(event_loop, EventKind::ShellExit, Vec::new());
                match self.config.on_shell_exit {
                    config::ShellExit::Close => self.close_pane(event_loop, id),
                    config::ShellExit::Hold => {
                        self.panes[index]
                            .terminal
                            .vt_write(b"\r\n\x1b[2m[proceso terminado]\x1b[0m\r\n");
                        self.redraw_window();
                    }
                }
            }
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

            // Capturar la entrada del teclado físico del usuario
            WindowEvent::KeyboardInput { event, .. } => self.handle_key(event_loop, &event),

            // Gestionar el redimensionado (Resize) coordinado
            WindowEvent::Resized(size) => self.resize(size.width, size.height),

            // Al cambiar de monitor con otro DPI cambia el tamaño de la fuente y, con él, el de la celda
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

            // Sin foco el cursor se dibuja hueco
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
    // 0. Cargar config.lua. Si falla se usan los valores por defecto y el error se escribe en la
    // cuadrícula del primer panel, cuando ya existe el terminal.
    let loaded = config::load();
    if let Some(error) = &loaded.error {
        eprintln!("[config] {error}");
    }
    let config = loaded.config;

    // 1. Configurar dimensiones iniciales (luego `resumed` las ajusta al tamaño real de la ventana)
    let cols = config.window.columns.unwrap_or(DEFAULT_COLUMNS);
    let rows = config.window.rows.unwrap_or(DEFAULT_ROWS);

    // 2. Crear el bucle de eventos de Winit (la ventana se crea en `resumed`). Duerme hasta que
    // llega un evento; los hilos de cada panel lo despiertan a través del proxy.
    let event_loop = EventLoop::<UserEvent>::with_user_event().build().unwrap();
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();

    // 3. El primer panel: terminal, PTY, shell y los hilos que los atienden
    let (lua_tx, lua_events) = channel::<(PaneId, LuaEvent)>();
    let first = PaneId(0);
    let pane = Pane::spawn(
        &config,
        Spawn {
            id: first,
            cols,
            rows,
            program: None,
            cwd: None,
            notice: loaded.error.as_deref().map(|error| ("config", error)),
        },
        lua_tx.clone(),
        proxy.clone(),
    )
    .unwrap_or_else(|error| panic!("{error}"));

    // El archivo de configuración se vigila para recargarlo al guardar
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
        mouse: MouseState::new().expect("Error al inicializar el ratón"),
        window: None,
        renderer: None,
        focused: true,
        panes: vec![pane],
        layout: Layout::new(first),
        next_pane_id: 1,
        leader: LeaderState::default(),
        key_encoder: Encoder::new().unwrap(),
        mods: Mods::empty(),
        proxy,
    };

    // 4. Bucle Principal de Eventos (Event Loop)
    let _ = event_loop.run_app(&mut app);
}
