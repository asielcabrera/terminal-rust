mod actions;
mod config;
mod glyphs;
mod renderer;

use actions::{Action, Command, EventKind, Script};
use config::{Config, ShellConfig};
use libghostty_vt::key::{Action as KeyAction, Encoder, Event as GhosttyKeyEvent, Key, Mods};
use libghostty_vt::paste;
use libghostty_vt::style::{Palette, PaletteIndex, RgbColor};
use libghostty_vt::terminal::{
    ConformanceLevel, CursorStyle as VtCursorStyle, DeviceAttributeFeature, DeviceAttributes,
    DeviceType, Mode, PrimaryDeviceAttributes, ScrollViewport, SecondaryDeviceAttributes,
    TertiaryDeviceAttributes,
};
use libghostty_vt::{Terminal, TerminalOptions};
use mlua::Value;
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use renderer::Renderer;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel, sync_channel};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime};
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::{ElementState, KeyEvent, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    keyboard::{KeyCode, PhysicalKey},
    window::{Fullscreen, Window, WindowId},
};

/// Tamaño de la cuadrícula cuando la configuración no pide otro
const DEFAULT_COLUMNS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

/// Eventos que otros hilos mandan al bucle de la UI para despertarlo
#[derive(Debug)]
enum UserEvent {
    /// Hay bytes nuevos del shell en el canal
    PtyOutput,
    /// El proceso hijo terminó
    ShellExited,
    /// El archivo de configuración cambió en disco
    ConfigChanged,
}

/// Lo que pasó dentro del terminal durante `vt_write` y puede interesar a los hooks de Lua. Se
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
    std::fs::metadata(path).and_then(|meta| meta.modified()).ok()
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

/// Bloques del shell que se procesan por despertar. Con mucha salida (`cat` de un archivo grande)
/// así los eventos de la ventana (redibujar, cerrar) se intercalan en vez de esperar al final.
const MAX_CHUNKS_PER_WAKE: usize = 256;
/// Bloques que caben en el canal del PTY antes de frenar al hilo lector (contrapresión a ConPTY)
const PTY_CHANNEL_CAPACITY: usize = 64;

// Respuesta a la consulta de atributos (CSI c). ConPTY la lanza al arrancar, junto con la de la
// posición del cursor, y espera hasta 3 s a que el terminal conteste antes de mostrar nada.
const DEVICE_ATTRIBUTES: DeviceAttributes = DeviceAttributes {
    primary: PrimaryDeviceAttributes::new(
        ConformanceLevel::LEVEL_2,
        &[DeviceAttributeFeature::ANSI_COLOR],
    ),
    secondary: SecondaryDeviceAttributes {
        device_type: DeviceType::VT220,
        firmware_version: 1,
        rom_cartridge: 0,
    },
    tertiary: TertiaryDeviceAttributes { unit_id: 0 },
};

// Colores de fábrica de libghostty (medidos: texto blanco sobre fondo negro)
const DEFAULT_FOREGROUND: RgbColor = RgbColor { r: 255, g: 255, b: 255 };
const DEFAULT_BACKGROUND: RgbColor = RgbColor { r: 0, g: 0, b: 0 };

/// Colores y cursor por defecto de la configuración.
///
/// El texto y el fondo se fijan SIEMPRE los dos y con valores explícitos, por dos peculiaridades
/// de libghostty-vt que se comprobaron: (1) solo se aplican si están los dos a la vez, y (2) pasar
/// `None` no se refleja en un `RenderState` ya creado (se queda con los colores anteriores).
/// Así, quitar una línea de config.lua al recargar devuelve los colores de fábrica.
fn apply_theme(terminal: &mut Terminal<'static, 'static>, config: &Config) {
    let colors = &config.colors;
    let _ = terminal.set_default_fg_color(Some(colors.foreground.map_or(DEFAULT_FOREGROUND, |c| c.0)));
    let _ = terminal.set_default_bg_color(Some(colors.background.map_or(DEFAULT_BACKGROUND, |c| c.0)));
    let _ = terminal.set_default_cursor_color(colors.cursor.map(|c| c.0));

    let palette = colors.ansi.as_ref().map(|ansi| {
        let mut palette = Palette::default();
        for (index, color) in ansi.iter().enumerate() {
            palette.set(PaletteIndex(index as u8), color.0);
        }
        palette
    });
    let _ = terminal.set_default_color_palette(palette);

    let style = match config.cursor.style {
        config::CursorStyle::Block => VtCursorStyle::Block,
        config::CursorStyle::Bar => VtCursorStyle::Bar,
        config::CursorStyle::Underline => VtCursorStyle::Underline,
    };
    let _ = terminal.set_default_cursor_style(Some(style));
}

fn build_command(shell: &ShellConfig) -> CommandBuilder {
    // `args` entra en pánico con el shell por defecto; la validación de config.rs ya garantiza
    // que solo hay argumentos si hay programa.
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

/// `portable-pty` devuelve el fallo de `CreateProcessW` como un texto plano ("CreateProcessW `...`
/// in cwd `...` failed: <causa>") con caracteres NUL escapados. Para mostrarlo se queda con la
/// causa; si el formato cambiara, devuelve el mensaje completo.
fn spawn_failure_cause(error: &impl std::fmt::Display) -> String {
    let text = error.to_string();
    match text.rsplit_once(" failed: ") {
        Some((_, cause)) => cause.to_string(),
        None => text,
    }
}

/// Escribe un aviso en rojo en la propia cuadrícula, antes de que arranque el shell. Con doble
/// clic no hay consola donde mirar stderr, y así el error se ve donde está el usuario.
fn write_notice(terminal: &mut Terminal<'static, 'static>, tag: &str, message: &str) {
    // El terminal necesita retorno de carro con cada salto de línea; cualquier otro carácter de
    // control (que podría venir de un mensaje de error) se descarta
    let clean: String = message
        .chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .collect();
    let notice = format!("\x1b[1;31m[{tag}]\x1b[0m {}\r\n", clean.replace('\n', "\r\n"));
    terminal.vt_write(notice.as_bytes());
}

struct App {
    config: Config,
    config_path: Option<PathBuf>,
    /// Teclas, hooks y el intérprete de Lua de la configuración actual
    script: Script,
    /// Eventos del terminal pendientes de pasar a los hooks de Lua
    lua_events: Receiver<LuaEvent>,
    /// Mientras se ejecutan las órdenes de un hook no se disparan otros hooks, para que uno que
    /// provoca el evento que lo dispara (p. ej. recargar la config desde `config-reloaded`) no
    /// entre en bucle
    in_hook: bool,
    /// Modificadores pulsados (bits de `actions::CTRL`, etc.) para buscar enlaces de teclas
    mod_bits: u8,
    /// Cuánto se movió el tamaño de la fuente con las acciones de aumentar/reducir. Una recarga
    /// de la configuración lo pone a cero.
    font_delta: f32,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    focused: bool,
    // Columnas y filas con las que se notificó por última vez al PTY y a libghostty
    grid: (u16, u16),
    terminal: Terminal<'static, 'static>,
    key_encoder: Encoder<'static>,
    mods: Mods,
    master: Box<dyn MasterPty + Send>,
    pty_writer: Box<dyn Write + Send>,
    rx: Receiver<Vec<u8>>,
    // Respuestas del terminal a las consultas del shell (posición del cursor, atributos...)
    replies: Receiver<Vec<u8>>,
    // El hilo lector solo manda un aviso a la vez; se rearma antes de leer el canal
    wake_pending: Arc<AtomicBool>,
    proxy: EventLoopProxy<UserEvent>,
    // Para terminar el shell al cerrar la ventana (el `Child` vive en el hilo que lo espera)
    killer: Box<dyn ChildKiller + Send + Sync>,
}

impl App {
    /// El título fijo de la configuración o, si no hay, el que puso el programa (o el de fábrica)
    fn window_title(&self) -> String {
        if let Some(title) = &self.config.window.title {
            return title.clone();
        }
        match self.terminal.title() {
            Ok(title) if !title.is_empty() => title.to_string(),
            _ => config::DEFAULT_TITLE.to_string(),
        }
    }

    fn handle_key(&mut self, event_loop: &ActiveEventLoop, key_event: &KeyEvent) {
        if key_event.state != ElementState::Pressed {
            return;
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

        self.key_encoder.set_options_from_terminal(&self.terminal);
        let mut seq = Vec::new();
        let encoded = self.key_encoder.encode_to_vec(&event, &mut seq).is_ok() && !seq.is_empty();

        if encoded {
            self.write_to_pty(&seq);
        } else if let Some(raw) = key_event.text.as_deref() {
            // Teclas sin mapeo (p. ej. Ctrl+C llega como "\x03"): se envían tal cual
            self.write_to_pty(raw.as_bytes());
        }
    }

    fn write_to_pty(&mut self, bytes: &[u8]) {
        let _ = self.pty_writer.write_all(bytes);
        let _ = self.pty_writer.flush();
    }

    /// Ejecuta una acción de una tecla, de un hook (`ctx:perform`) o de una función de Lua
    fn perform(&mut self, event_loop: &ActiveEventLoop, action: Action) {
        match action {
            Action::SendString { text } => self.write_to_pty(text.as_bytes()),
            Action::ScrollLines { amount } => self.scroll(ScrollViewport::Delta(amount)),
            Action::ScrollPages { amount } => {
                // Una página son las filas visibles
                let page = self.grid.1 as isize;
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
            Action::Quit => {
                let _ = self.killer.kill();
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

    /// Despacha lo que ocurrió dentro del terminal durante `vt_write`
    fn dispatch_terminal_events(&mut self, event_loop: &ActiveEventLoop) {
        while let Ok(event) = self.lua_events.try_recv() {
            match event {
                LuaEvent::TitleChanged(title) => {
                    // Lo habitual en un terminal: la ventana muestra el título que pone el
                    // programa, salvo que config.lua fije uno con `window.title`
                    if self.config.window.title.is_none()
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

    fn scroll(&mut self, scroll: ScrollViewport) {
        self.terminal.scroll_viewport(scroll);
        if let Some(window) = &self.window {
            window.request_redraw();
        }
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
        let bracketed = self.terminal.mode(Mode::BRACKETED_PASTE).unwrap_or(false);
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
            self.grid = (0, 0);
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

        apply_theme(&mut self.terminal, &new);
        if new.scrollback != self.config.scrollback
            || new.window.columns != self.config.window.columns
            || new.window.rows != self.config.window.rows
            || new.shell != self.config.shell
        {
            eprintln!("[config] scrollback, window.columns/rows y shell solo se aplican al arrancar");
        }
        self.config = new;
        // Las teclas y los hooks nuevos sustituyen a los anteriores (con su intérprete de Lua), y
        // el tamaño de fuente vuelve al de la configuración
        self.script = script;
        self.font_delta = 0.0;
        if let Some(window) = &self.window {
            window.set_title(&self.window_title());
        }

        // Si cambió la celda, hay que recalcular filas y columnas y avisar al PTY
        if metrics_changed {
            self.grid = (0, 0);
        }
        if let Some(size) = self.window.as_ref().map(|window| window.inner_size()) {
            self.resize(size.width, size.height);
        }
        Ok(())
    }

    /// Pasa a libghostty lo que el shell escribió, contesta sus consultas, avisa a los hooks de Lua
    /// y pide redibujar
    fn drain_pty(&mut self, event_loop: &ActiveEventLoop) {
        // Se rearma ANTES de leer: un bloque que llegue durante el drenado dispara otro aviso
        self.wake_pending.store(false, Ordering::Release);

        let mut chunks = 0;
        while chunks < MAX_CHUNKS_PER_WAKE
            && let Ok(bytes) = self.rx.try_recv()
        {
            // Escribir los bytes crudos en libghostty para que procese los escapes ANSI
            self.terminal.vt_write(&bytes);
            chunks += 1;
        }

        // Contestar las consultas que el shell hizo en esos bytes
        while let Ok(reply) = self.replies.try_recv() {
            self.write_to_pty(&reply);
        }
        // Título y campana, ya con `vt_write` terminado
        self.dispatch_terminal_events(event_loop);

        if chunks == MAX_CHUNKS_PER_WAKE {
            // Puede quedar más: se vuelve a avisar para dejar pasar otros eventos entre medias
            self.wake_pending.store(true, Ordering::Release);
            let _ = self.proxy.send_event(UserEvent::PtyOutput);
        }
        // Si la terminal cambió de estado interno, redibujamos el frame
        if chunks > 0
            && let Some(window) = &self.window
        {
            window.request_redraw();
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

        let (cols, rows) = renderer.grid_size(width, height);
        if (cols, rows) != self.grid {
            self.grid = (cols, rows);
            let (cell_width, cell_height) = renderer.cell_size();

            // A. Notificar al Kernel (PTY)
            let _ = self.master.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });

            // B. Notificar a libghostty-vt para que haga reflow del buffer
            let _ = self.terminal.resize(cols, rows, cell_width, cell_height);
        }

        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn render(&mut self) {
        if let Some(renderer) = &mut self.renderer {
            renderer.draw(&self.terminal, self.focused);
        }
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
            let attrs = Window::default_attributes().with_title(self.window_title());
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
                let wanted = PhysicalSize::new(
                    columns * cell_width + padding,
                    rows * cell_height + padding,
                );
                size = window.request_inner_size(wanted).unwrap_or(size);
            }

            self.renderer = Some(renderer);
            self.window = Some(window);
            // Ajusta el PTY y la cuadrícula al tamaño real de la ventana
            self.resize(size.width, size.height);
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::PtyOutput => self.drain_pty(event_loop),
            UserEvent::ShellExited => {
                // Lo último que escribió el shell antes de salir
                self.drain_pty(event_loop);
                self.fire_event(event_loop, EventKind::ShellExit, Vec::new());
                match self.config.on_shell_exit {
                    config::ShellExit::Close => event_loop.exit(),
                    config::ShellExit::Hold => {
                        self.terminal
                            .vt_write(b"\r\n\x1b[2m[proceso terminado]\x1b[0m\r\n");
                        if let Some(window) = &self.window {
                            window.request_redraw();
                        }
                    }
                }
            }
            UserEvent::ConfigChanged => self.reload_config(event_loop),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                let _ = self.killer.kill();
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
                self.grid = (0, 0);
                if let Some(size) = self.window.as_ref().map(|window| window.inner_size()) {
                    self.resize(size.width, size.height);
                }
            }

            // Sin foco el cursor se dibuja hueco
            WindowEvent::Focused(focused) => {
                self.focused = focused;
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }

            WindowEvent::RedrawRequested => self.render(),

            _ => {}
        }
    }
}

fn main() {
    // 0. Cargar config.lua. Si falla se usan los valores por defecto y el error se escribe en la
    // cuadrícula, más abajo, cuando ya existe el terminal.
    let loaded = config::load();
    if let Some(error) = &loaded.error {
        eprintln!("[config] {error}");
    }
    let config = loaded.config;

    // 1. Configurar dimensiones iniciales (luego `resumed` las ajusta al tamaño real de la ventana)
    let cols = config.window.columns.unwrap_or(DEFAULT_COLUMNS);
    let rows = config.window.rows.unwrap_or(DEFAULT_ROWS);

    // 2. Inicializar el motor de emulación libghostty-vt
    let mut terminal = Terminal::new(TerminalOptions {
        cols,
        rows,
        max_scrollback: config.scrollback,
    })
    .expect("Error al inicializar libghostty");
    apply_theme(&mut terminal, &config);
    if let Some(error) = &loaded.error {
        write_notice(&mut terminal, "config", error);
    }

    // Por defecto libghostty ignora las consultas del shell. Sus respuestas se encolan aquí y el
    // bucle de eventos las escribe en el PTY. Los eventos de título y campana se encolan igual para
    // despacharlos a Lua cuando `vt_write` ya terminó (no se llama a Lua dentro de los callbacks).
    let (reply_tx, replies) = channel::<Vec<u8>>();
    let (lua_tx, lua_events) = channel::<LuaEvent>();
    terminal
        .on_pty_write(move |_, data| {
            let _ = reply_tx.send(data.to_vec());
        })
        .expect("Error al registrar on_pty_write")
        .on_device_attributes(|_| Some(DEVICE_ATTRIBUTES))
        .expect("Error al registrar on_device_attributes")
        .on_title_changed({
            let lua_tx = lua_tx.clone();
            move |terminal| {
                if let Ok(title) = terminal.title() {
                    let _ = lua_tx.send(LuaEvent::TitleChanged(title.to_owned()));
                }
            }
        })
        .expect("Error al registrar on_title_changed")
        .on_bell(move |_| {
            let _ = lua_tx.send(LuaEvent::Bell);
        })
        .expect("Error al registrar on_bell");

    // 3. Crear el par PTY (Pseudo-Terminal) del sistema operativo
    let pty_system = NativePtySystem::default();
    let pty_pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    // 4. Lanzar el Shell (Bash/Zsh/cmd/PowerShell) dentro del PTY slave. Si el programa de la
    // configuración no se puede iniciar, se avisa y se usa el shell por defecto.
    let child = match pty_pair.slave.spawn_command(build_command(&config.shell)) {
        Ok(child) => child,
        Err(error) => {
            let Some(program) = &config.shell.program else {
                panic!("No se pudo iniciar el shell por defecto: {error}");
            };
            eprintln!("[shell] no se pudo iniciar {program:?}: {error}");
            // En pantalla solo la causa ("no se encuentra el archivo"); el detalle de
            // CreateProcessW queda en stderr
            write_notice(
                &mut terminal,
                "shell",
                &format!(
                    "no se pudo iniciar {program:?}: {}; se usa el shell por defecto",
                    spawn_failure_cause(&error)
                ),
            );
            let default_shell = ShellConfig {
                program: None,
                args: Vec::new(),
                ..config.shell.clone()
            };
            pty_pair
                .slave
                .spawn_command(build_command(&default_shell))
                .expect("No se pudo iniciar el shell por defecto")
        }
    };

    // Clonar canales de comunicación del PTY master
    let mut pty_reader = pty_pair.master.try_clone_reader().unwrap();
    let pty_writer = pty_pair.master.take_writer().unwrap();

    // 5. Crear el bucle de eventos de Winit (la ventana se crea en `resumed`). Duerme hasta que
    // llega un evento; los hilos de abajo lo despiertan a través del proxy.
    let event_loop = EventLoop::<UserEvent>::with_user_event().build().unwrap();
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();

    // 6. Hilo lector: leer del PTY (Shell) y mandar los bytes al canal de la app principal. El
    // canal es acotado: si la UI se atrasa, `send` bloquea y ConPTY recibe contrapresión en vez
    // de acumularse memoria sin límite.
    let (tx, rx) = sync_channel::<Vec<u8>>(PTY_CHANNEL_CAPACITY);
    let wake_pending = Arc::new(AtomicBool::new(false));
    thread::spawn({
        let proxy = proxy.clone();
        let wake_pending = wake_pending.clone();
        move || {
            let mut buf = [0u8; 4096];
            loop {
                match pty_reader.read(&mut buf) {
                    Ok(n) if n > 0 => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break; // la app se cerró
                        }
                        // Un solo aviso pendiente a la vez: el manejador drena todo el canal
                        if !wake_pending.swap(true, Ordering::AcqRel) {
                            let _ = proxy.send_event(UserEvent::PtyOutput);
                        }
                    }
                    _ => break,
                }
            }
        }
    });

    // 7. Hilo que espera al shell. No se usa el fin de lectura del PTY: en Windows no es fiable.
    // El `killer` se queda en la app para terminar el shell al cerrar la ventana.
    let killer = child.clone_killer();
    thread::spawn({
        let proxy = proxy.clone();
        move || {
            let mut child = child;
            let _ = child.wait();
            let _ = proxy.send_event(UserEvent::ShellExited);
        }
    });

    // El archivo de configuración se vigila para recargarlo al guardar
    if let Some(path) = &loaded.path {
        watch_config(path.clone(), proxy.clone());
    }

    let mut app = App {
        config,
        config_path: loaded.path,
        script: loaded.script,
        lua_events,
        in_hook: false,
        mod_bits: 0,
        font_delta: 0.0,
        window: None,
        renderer: None,
        focused: true,
        grid: (cols, rows),
        terminal,
        key_encoder: Encoder::new().unwrap(),
        mods: Mods::empty(),
        master: pty_pair.master,
        pty_writer,
        rx,
        replies,
        wake_pending,
        proxy,
        killer,
    };

    // 8. Bucle Principal de Eventos (Event Loop)
    let _ = event_loop.run_app(&mut app);
}


#[cfg(test)]
mod tests {
    use super::*;
    use libghostty_vt::RenderState;

    type Rgb = (u8, u8, u8);

    /// Lo que ve el renderer. El `RenderState` es persistente, como en la app: así se comprueba
    /// que los cambios (y sobre todo las supresiones) de tema se reflejan en uno ya creado.
    fn seen(
        state: &mut RenderState<'static>,
        terminal: &Terminal<'static, 'static>,
    ) -> (Rgb, Rgb, Rgb, Option<libghostty_vt::render::CursorVisualStyle>) {
        let snapshot = state.update(terminal).unwrap();
        let colors = snapshot.colors().unwrap();
        let rgb = |c: RgbColor| (c.r, c.g, c.b);
        (
            rgb(colors.foreground),
            rgb(colors.background),
            rgb(colors.palette[1]),
            snapshot.cursor_visual_style().ok(),
        )
    }

    fn config_from(source: &str) -> Config {
        config::parse(source, "config.lua", None).unwrap()
    }

    #[test]
    fn theme_applies_with_only_a_background_and_reverts_when_removed() {
        let mut terminal: Terminal<'static, 'static> = Terminal::new(TerminalOptions {
            cols: 10,
            rows: 3,
            max_scrollback: 0,
        })
        .unwrap();
        let mut state = RenderState::new().unwrap();
        apply_theme(&mut terminal, &Config::default());
        let factory = seen(&mut state, &terminal);

        // Solo fondo: libghostty lo ignoraría si el texto no se fijara también
        apply_theme(&mut terminal, &config_from(r##"return { colors = { background = "#1e1e2e" } }"##));
        let themed = seen(&mut state, &terminal);
        assert_eq!(themed.1, (0x1e, 0x1e, 0x2e));
        assert_eq!(themed.0, factory.0, "el texto conserva el color de fábrica");

        // Paleta y cursor propios
        apply_theme(
            &mut terminal,
            &config_from(
                r##"return { cursor = { style = "bar" }, colors = { ansi = { "#000000", "#010203" } } }"##,
            ),
        );
        let custom = seen(&mut state, &terminal);
        assert_eq!(custom.2, (1, 2, 3));
        assert_eq!(custom.3, Some(libghostty_vt::render::CursorVisualStyle::Bar));

        // Quitar todo (recargar una config vacía) devuelve el aspecto de fábrica
        apply_theme(&mut terminal, &Config::default());
        assert_eq!(seen(&mut state, &terminal), factory);
    }
}
