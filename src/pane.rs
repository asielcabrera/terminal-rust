//! Un panel: un terminal de libghostty con su PTY, los hilos que lo atienden y el estado de la
//! selección con el ratón. La app tiene uno o varios, colocados por `layout`.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel, sync_channel};
use std::thread;

use libghostty_vt::mouse;
use libghostty_vt::selection::gesture::Gesture;
use libghostty_vt::style::{Palette, PaletteIndex, RgbColor};
use libghostty_vt::terminal::{
    ConformanceLevel, CursorStyle as VtCursorStyle, DeviceAttributeFeature, DeviceAttributes,
    DeviceType, PrimaryDeviceAttributes, SecondaryDeviceAttributes, TertiaryDeviceAttributes,
};
use libghostty_vt::{RenderState, Terminal, TerminalOptions};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use winit::event_loop::EventLoopProxy;

use crate::config::{self, Config, ShellConfig};
use crate::layout::{PaneId, Rect};
use crate::{LuaEvent, UserEvent};

/// Bloques del shell que se procesan por despertar. Con mucha salida (`cat` de un archivo grande)
/// así los eventos de la ventana (redibujar, cerrar) se intercalan en vez de esperar al final.
pub const MAX_CHUNKS_PER_WAKE: usize = 256;
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
const DEFAULT_FOREGROUND: RgbColor = RgbColor {
    r: 255,
    g: 255,
    b: 255,
};
const DEFAULT_BACKGROUND: RgbColor = RgbColor { r: 0, g: 0, b: 0 };

/// Colores y cursor por defecto de la configuración.
///
/// El texto y el fondo se fijan SIEMPRE los dos y con valores explícitos, por dos peculiaridades
/// de libghostty-vt que se comprobaron: (1) solo se aplican si están los dos a la vez, y (2) pasar
/// `None` no se refleja en un `RenderState` ya creado (se queda con los colores anteriores).
/// Así, quitar una línea de config.lua al recargar devuelve los colores de fábrica.
pub fn apply_theme(terminal: &mut Terminal<'static, 'static>, config: &Config) {
    let colors = &config.colors;
    let _ =
        terminal.set_default_fg_color(Some(colors.foreground.map_or(DEFAULT_FOREGROUND, |c| c.0)));
    let _ =
        terminal.set_default_bg_color(Some(colors.background.map_or(DEFAULT_BACKGROUND, |c| c.0)));
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
    let _ = terminal.set_default_cursor_blink(Some(config.cursor.blink));
}

/// Script que, ejecutado dentro de un PowerShell interactivo (ya cargado su perfil), hace que
/// informe su directorio actual por OSC 7 en cada prompt. Envuelve la función `prompt` que haya
/// (la de starship, la de fábrica...): la llama PRIMERO, porque lo primero que ejecuta un prompt
/// como el de starship es leer `$?`, y cualquier otra instrucción antes lo borraría; su texto se
/// devuelve intacto. Va en una sola línea y sin comillas dobles ni barras al final, para que
/// sobreviva al paso por la línea de comandos de Windows.
const PWSH_OSC7: &str = "$global:__terminal_prompt = $function:prompt; \
function global:prompt { \
$text = & $global:__terminal_prompt; \
$location = $executionContext.SessionState.Path.CurrentLocation; \
if ($location.Provider.Name -eq 'FileSystem') { \
$path = $location.ProviderPath.Replace('%', '%25').Replace(' ', '%20').Replace('#', '%23').Replace([string][char]92, '/'); \
[Console]::Write([string][char]27 + ']7;file://localhost/' + $path + [char]27 + [char]92) }; \
$text }";

fn is_powershell(program: &str) -> bool {
    std::path::Path::new(program)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| matches!(stem.to_ascii_lowercase().as_str(), "pwsh" | "powershell"))
}

/// ¿Los argumentos de PowerShell ya piden ejecutar un comando o un archivo?
fn runs_a_command(args: &[String]) -> bool {
    args.iter().any(|arg| {
        let name = arg.trim_start_matches(['-', '/']).to_ascii_lowercase();
        matches!(
            name.as_str(),
            "c" | "command"
                | "commandwithargs"
                | "cwa"
                | "f"
                | "file"
                | "e"
                | "ec"
                | "encodedcommand"
        )
    })
}

/// El shell con la integración de directorio (ver `ShellConfig::integration`) si procede: es
/// PowerShell, está activada y los argumentos no traen ya un comando
fn with_integration(mut shell: ShellConfig) -> ShellConfig {
    let powershell = shell.program.as_deref().is_some_and(is_powershell);
    if shell.integration && powershell && !runs_a_command(&shell.args) {
        shell.args.extend([
            "-NoExit".to_string(),
            "-Command".to_string(),
            PWSH_OSC7.to_string(),
        ]);
    }
    shell
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
pub fn write_notice(terminal: &mut Terminal<'static, 'static>, tag: &str, message: &str) {
    // El terminal necesita retorno de carro con cada salto de línea; cualquier otro carácter de
    // control (que podría venir de un mensaje de error) se descarta
    let clean: String = message
        .chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .collect();
    let notice = format!(
        "\x1b[1;31m[{tag}]\x1b[0m {}\r\n",
        clean.replace('\n', "\r\n")
    );
    terminal.vt_write(notice.as_bytes());
}

fn vt_error(error: libghostty_vt::Error) -> String {
    format!("libghostty: {error}")
}

/// El directorio que informó un shell: el URI de OSC 7 o, como hacen ConEmu y Windows Terminal con
/// OSC 9;9 (muy usado en el prompt de cmd), una ruta absoluta tal cual, a veces entre comillas
fn reported_path(value: &str) -> Option<String> {
    if value.starts_with("file://") {
        return osc7_path(value);
    }
    let path = value.trim().trim_matches('"');
    std::path::Path::new(path)
        .is_absolute()
        .then(|| path.to_string())
}

/// La ruta de un URI de directorio de OSC 7 ("file://host/C:/ruta%20con%20espacios", con o sin
/// host), decodificada. En Windows quita la barra que precede a la unidad ("/C:/ruta").
fn osc7_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    // Lo que va entre "file://" y la primera barra es el host
    let path = &rest[rest.find('/')?..];
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = path.get(index + 1..index + 3)?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    let text = String::from_utf8(decoded).ok()?;
    let drive_path = text.as_bytes();
    let windows_drive =
        cfg!(windows) && drive_path.len() >= 3 && drive_path[0] == b'/' && drive_path[2] == b':';
    Some(if windows_drive {
        text[1..].to_string()
    } else {
        text
    })
}

/// Directorio que el shell de un terminal informó con OSC 7, si existe
fn terminal_dir(terminal: &Terminal<'static, 'static>) -> Option<PathBuf> {
    let path = PathBuf::from(reported_path(terminal.pwd().ok()?)?);
    path.is_dir().then_some(path)
}

/// Qué lanzar y dónde para un panel nuevo
pub struct Spawn<'a> {
    pub id: PaneId,
    pub cols: u16,
    pub rows: u16,
    /// Programa y argumentos en lugar del shell de la configuración (un flotante lanzado con
    /// `program`). Si no arranca no se cae al shell por defecto: el panel no se crea.
    pub program: Option<(String, Vec<String>)>,
    /// Directorio de trabajo; si falta, el de `shell.cwd`
    pub cwd: Option<PathBuf>,
    /// Aviso en rojo (etiqueta y mensaje) que se escribe antes de que arranque el shell
    pub notice: Option<(&'a str, &'a str)>,
}

pub struct Pane {
    pub id: PaneId,
    pub terminal: Terminal<'static, 'static>,
    /// Estado de libghostty que solo se usa para dibujar este panel
    pub render_state: RenderState<'static>,
    /// Rectángulo que ocupa en la ventana (pixeles físicos), con su margen
    pub rect: Rect,
    /// Columnas y filas con las que se notificó por última vez al PTY y a libghostty
    pub grid: (u16, u16),
    master: Box<dyn MasterPty + Send>,
    pty_writer: Box<dyn Write + Send>,
    rx: Receiver<Vec<u8>>,
    /// Respuestas del terminal a las consultas del shell (posición del cursor, atributos...)
    replies: Receiver<Vec<u8>>,
    /// El hilo lector solo manda un aviso a la vez; se rearma antes de leer el canal
    wake_pending: Arc<AtomicBool>,
    /// Para terminar el shell (el `Child` vive en el hilo que lo espera)
    killer: Box<dyn ChildKiller + Send + Sync>,
    /// Estado del gesto de selección (clic, doble clic, arrastre) en este terminal
    pub gesture: Gesture<'static>,
    /// Hay un arrastre de selección en curso
    pub selecting: bool,
    /// Botón cuya pulsación se envió al programa (el movimiento y la suelta también van a él)
    pub reported: Option<mouse::Button>,
}

impl Pane {
    /// Crea el terminal, abre el PTY, lanza el shell y arranca los hilos que lo atienden. Los
    /// hilos despiertan el bucle de eventos con `UserEvent::PtyOutput(id)` / `ShellExited(id)`.
    pub fn spawn(
        config: &Config,
        spawn: Spawn,
        lua_events: Sender<(PaneId, LuaEvent)>,
        proxy: EventLoopProxy<UserEvent>,
    ) -> Result<Pane, String> {
        let id = spawn.id;
        let mut terminal = Terminal::new(TerminalOptions {
            cols: spawn.cols,
            rows: spawn.rows,
            max_scrollback: config.scrollback,
        })
        .map_err(vt_error)?;
        apply_theme(&mut terminal, config);
        if let Some((tag, message)) = spawn.notice {
            write_notice(&mut terminal, tag, message);
        }

        // Por defecto libghostty ignora las consultas del shell. Sus respuestas se encolan aquí y el
        // bucle de eventos las escribe en el PTY. Los eventos de título y campana se encolan igual
        // para despacharlos a Lua cuando `vt_write` ya terminó (no se llama a Lua dentro de los
        // callbacks).
        let (reply_tx, replies) = channel::<Vec<u8>>();
        terminal
            .on_pty_write(move |_, data| {
                let _ = reply_tx.send(data.to_vec());
            })
            .map_err(vt_error)?
            .on_device_attributes(|_| Some(DEVICE_ATTRIBUTES))
            .map_err(vt_error)?
            .on_title_changed({
                let lua_events = lua_events.clone();
                move |terminal| {
                    if let Ok(title) = terminal.title() {
                        let _ = lua_events.send((id, LuaEvent::TitleChanged(title.to_owned())));
                    }
                }
            })
            .map_err(vt_error)?
            .on_bell(move |_| {
                let _ = lua_events.send((id, LuaEvent::Bell));
            })
            .map_err(vt_error)?;

        // El PTY (pseudo-terminal) del sistema operativo
        let pty_pair = NativePtySystem::default()
            .openpty(PtySize {
                rows: spawn.rows,
                cols: spawn.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| format!("no se pudo abrir el PTY: {error}"))?;

        // El shell (o el programa pedido) dentro del PTY. Si el programa de la configuración no se
        // puede iniciar, se avisa y se usa el shell por defecto.
        let explicit = spawn.program.is_some();
        let mut shell = config.shell.clone();
        if let Some((program, args)) = spawn.program {
            shell.program = Some(program);
            shell.args = args;
        }
        if let Some(cwd) = spawn.cwd {
            shell.cwd = Some(cwd);
        }
        let shell = with_integration(shell);
        let child = match pty_pair.slave.spawn_command(build_command(&shell)) {
            Ok(child) => child,
            Err(error) => {
                let Some(program) = &shell.program else {
                    return Err(format!("no se pudo iniciar el shell por defecto: {error}"));
                };
                if explicit {
                    return Err(format!(
                        "no se pudo iniciar {program:?}: {}",
                        spawn_failure_cause(&error)
                    ));
                }
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
                    ..shell.clone()
                };
                pty_pair
                    .slave
                    .spawn_command(build_command(&default_shell))
                    .map_err(|error| format!("no se pudo iniciar el shell por defecto: {error}"))?
            }
        };

        let mut pty_reader = pty_pair
            .master
            .try_clone_reader()
            .map_err(|error| format!("no se pudo leer del PTY: {error}"))?;
        let pty_writer = pty_pair
            .master
            .take_writer()
            .map_err(|error| format!("no se pudo escribir en el PTY: {error}"))?;

        // Hilo lector: lee del PTY y manda los bytes al canal. El canal es acotado: si la UI se
        // atrasa, `send` bloquea y ConPTY recibe contrapresión en vez de acumularse memoria sin límite.
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
                                break; // el panel se cerró
                            }
                            // Un solo aviso pendiente a la vez: el manejador drena todo el canal
                            if !wake_pending.swap(true, Ordering::AcqRel) {
                                let _ = proxy.send_event(UserEvent::PtyOutput(id));
                            }
                        }
                        _ => break,
                    }
                }
            }
        });

        // Hilo que espera al shell. No se usa el fin de lectura del PTY: en Windows no es fiable.
        let killer = child.clone_killer();
        thread::spawn(move || {
            let mut child = child;
            let _ = child.wait();
            let _ = proxy.send_event(UserEvent::ShellExited(id));
        });

        Ok(Pane {
            id,
            terminal,
            render_state: RenderState::new().map_err(vt_error)?,
            rect: Rect::new(0, 0, 0, 0),
            grid: (spawn.cols, spawn.rows),
            master: pty_pair.master,
            pty_writer,
            rx,
            replies,
            wake_pending,
            killer,
            gesture: Gesture::new().map_err(vt_error)?,
            selecting: false,
            reported: None,
        })
    }

    /// Escribe en la entrada del shell
    pub fn write(&mut self, bytes: &[u8]) {
        let _ = self.pty_writer.write_all(bytes);
        let _ = self.pty_writer.flush();
    }

    /// Pasa a libghostty lo que el shell escribió (hasta `max_chunks` bloques) y contesta sus
    /// consultas. Devuelve cuántos bloques procesó.
    pub fn drain(&mut self, max_chunks: usize) -> usize {
        // Se rearma ANTES de leer: un bloque que llegue durante el drenado dispara otro aviso
        self.wake_pending.store(false, Ordering::Release);

        let mut chunks = 0;
        while chunks < max_chunks
            && let Ok(bytes) = self.rx.try_recv()
        {
            // Escribir los bytes crudos en libghostty para que procese los escapes ANSI
            self.terminal.vt_write(&bytes);
            chunks += 1;
        }

        // Contestar las consultas que el shell hizo en esos bytes
        while let Ok(reply) = self.replies.try_recv() {
            self.write(&reply);
        }
        chunks
    }

    /// Pide otro despertar para seguir drenando lo que quedó en el canal
    pub fn request_more(&self, proxy: &EventLoopProxy<UserEvent>) {
        self.wake_pending.store(true, Ordering::Release);
        let _ = proxy.send_event(UserEvent::PtyOutput(self.id));
    }

    /// Avisa al PTY y a libghostty (que hace reflow del buffer) del nuevo tamaño
    pub fn resize(&mut self, cols: u16, rows: u16, cell_width: u32, cell_height: u32) {
        self.grid = (cols, rows);
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        let _ = self.terminal.resize(cols, rows, cell_width, cell_height);
    }

    /// Título que puso el programa, si hay alguno
    pub fn title(&self) -> Option<String> {
        match self.terminal.title() {
            Ok(title) if !title.is_empty() => Some(title.to_string()),
            _ => None,
        }
    }

    /// Directorio actual del shell, si lo informa con OSC 7 y existe
    pub fn current_dir(&self) -> Option<PathBuf> {
        terminal_dir(&self.terminal)
    }

    pub fn kill(&mut self) {
        let _ = self.killer.kill();
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        // Libera los pins que el gesto tiene en el terminal y termina el shell si seguía vivo
        self.gesture.reset(&self.terminal);
        let _ = self.killer.kill();
    }
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
    ) -> (
        Rgb,
        Rgb,
        Rgb,
        Option<libghostty_vt::render::CursorVisualStyle>,
    ) {
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
        apply_theme(
            &mut terminal,
            &config_from(r##"return { colors = { background = "#1e1e2e" } }"##),
        );
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
        assert_eq!(
            custom.3,
            Some(libghostty_vt::render::CursorVisualStyle::Bar)
        );

        // Quitar todo (recargar una config vacía) devuelve el aspecto de fábrica
        apply_theme(&mut terminal, &Config::default());
        assert_eq!(seen(&mut state, &terminal), factory);
    }

    fn shell(program: Option<&str>, args: &[&str], integration: bool) -> ShellConfig {
        ShellConfig {
            program: program.map(String::from),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            integration,
            ..ShellConfig::default()
        }
    }

    #[test]
    fn powershell_gets_the_directory_integration_appended_to_its_arguments() {
        for program in [
            "pwsh",
            "pwsh.exe",
            "C:/Program Files/PowerShell/7/pwsh.exe",
            "PowerShell.EXE",
        ] {
            let result = with_integration(shell(
                Some(program),
                &["-NoLogo", "-ExecutionPolicy", "Bypass"],
                true,
            ));
            assert_eq!(
                result.args[..3],
                ["-NoLogo", "-ExecutionPolicy", "Bypass"],
                "{program}"
            );
            assert_eq!(result.args[3..5], ["-NoExit", "-Command"], "{program}");
            assert_eq!(result.args[5], PWSH_OSC7);
            assert_eq!(result.args.len(), 6);
        }
    }

    #[test]
    fn the_integration_is_left_out_when_it_does_not_apply() {
        // No es PowerShell, o no hay programa (shell por defecto)
        for program in [Some("cmd.exe"), Some("lazygit"), Some("nu"), None] {
            let original = shell(program, &["/k"], true);
            assert_eq!(with_integration(original.clone()), original, "{program:?}");
        }
        // Desactivada
        let off = shell(Some("pwsh"), &["-NoLogo"], false);
        assert_eq!(with_integration(off.clone()), off);
        // Ya trae un comando o un archivo: no se le pisa
        for args in [
            &["-Command", "todos"][..],
            &["-c", "todos"],
            &["-File", "x.ps1"],
            &["/command", "x"],
            &["-EncodedCommand", "AAAA"],
            &["-NoLogo", "-ec", "AAAA"],
        ] {
            let original = shell(Some("pwsh"), args, true);
            assert_eq!(with_integration(original.clone()), original, "{args:?}");
        }
        // `-ExecutionPolicy` empieza por -e pero no es un comando
        assert!(
            with_integration(shell(Some("pwsh"), &["-ExecutionPolicy", "Bypass"], true))
                .args
                .len()
                > 2
        );
    }

    #[test]
    fn the_powershell_script_survives_the_windows_command_line() {
        assert!(!PWSH_OSC7.contains('\n'), "una sola línea");
        assert!(!PWSH_OSC7.contains('"'), "sin comillas dobles");
        assert!(
            !PWSH_OSC7.ends_with('\\'),
            "una barra al final se comería la comilla que cierra el argumento"
        );
        // La función `prompt` original se llama antes que nada: starship lee `$?` en su primera línea
        let call = PWSH_OSC7
            .find("$text = & $global:__terminal_prompt")
            .unwrap();
        let first_statement_in_prompt = PWSH_OSC7.find("function global:prompt {").unwrap();
        assert!(call > first_statement_in_prompt);
        assert!(
            !PWSH_OSC7[first_statement_in_prompt..call].contains(';'),
            "nada entre la entrada y la llamada"
        );
        assert!(PWSH_OSC7.contains("]7;file://localhost/"));
    }

    #[test]
    fn osc7_uris_become_paths() {
        assert_eq!(
            osc7_path("file:///usr/local/bin").as_deref(),
            Some("/usr/local/bin")
        );
        assert_eq!(
            osc7_path("file://mi-pc/home/yo").as_deref(),
            Some("/home/yo")
        );
        assert_eq!(
            osc7_path("file:///home/mi%20carpeta/%C3%B1").as_deref(),
            Some("/home/mi carpeta/ñ")
        );
        assert_eq!(osc7_path("http://example.com/x"), None);
        assert_eq!(osc7_path("file://sin-ruta"), None);
        assert_eq!(osc7_path("file:///mal%2"), None);
        assert_eq!(osc7_path("file:///mal%zz"), None);
        assert_eq!(osc7_path("file:///%ff"), None, "no es UTF-8");
    }

    #[test]
    fn a_plain_absolute_path_is_accepted_as_osc_9_9_does() {
        let absolute = if cfg!(windows) {
            r"C:\Users\yo"
        } else {
            "/home/yo"
        };
        assert_eq!(reported_path(absolute).as_deref(), Some(absolute));
        assert_eq!(
            reported_path(&format!("\"{absolute}\"")).as_deref(),
            Some(absolute)
        );
        assert_eq!(reported_path("relativa/ruta"), None);
        assert_eq!(reported_path(""), None);
        // Un URI inválido no se interpreta como ruta
        assert_eq!(reported_path("file://sin-ruta"), None);
    }

    #[test]
    fn the_directory_from_osc_9_9_reaches_pwd_and_is_used() {
        let mut terminal: Terminal<'static, 'static> = Terminal::new(TerminalOptions {
            cols: 10,
            rows: 3,
            max_scrollback: 0,
        })
        .unwrap();
        let here = std::env::current_dir().unwrap();
        terminal.vt_write(format!("\x1b]9;9;\"{}\"\x1b\\", here.display()).as_bytes());
        assert_eq!(terminal_dir(&terminal).as_deref(), Some(here.as_path()));
    }

    #[cfg(windows)]
    #[test]
    fn osc7_windows_drive_paths_lose_the_leading_slash() {
        assert_eq!(
            osc7_path("file:///C:/Users/yo").as_deref(),
            Some("C:/Users/yo")
        );
        assert_eq!(
            osc7_path("file://PC/C:/Users/yo%20x").as_deref(),
            Some("C:/Users/yo x")
        );
    }

    #[test]
    fn a_terminal_reports_the_directory_set_with_osc7() {
        let mut terminal: Terminal<'static, 'static> = Terminal::new(TerminalOptions {
            cols: 10,
            rows: 3,
            max_scrollback: 0,
        })
        .unwrap();
        assert_eq!(terminal_dir(&terminal), None, "sin OSC 7 no hay directorio");
        // El directorio de pruebas de cargo existe en cualquier plataforma
        let here = std::env::current_dir().unwrap();
        let uri = if cfg!(windows) {
            format!("file:///{}", here.display().to_string().replace('\\', "/"))
        } else {
            format!("file://{}", here.display())
        };
        terminal.vt_write(format!("\x1b]7;{uri}\x07").as_bytes());
        assert_eq!(terminal_dir(&terminal).as_deref(), Some(here.as_path()));
        // Un directorio que no existe no se usa
        terminal.vt_write(b"\x1b]7;file:///no/existe/de/verdad\x07");
        assert_eq!(terminal_dir(&terminal), None);
    }

    #[test]
    fn palette_entries_past_the_ansi_colors_reach_the_renderer() {
        let mut terminal: Terminal<'static, 'static> = Terminal::new(TerminalOptions {
            cols: 10,
            rows: 3,
            max_scrollback: 0,
        })
        .unwrap();
        let mut state = RenderState::new().unwrap();
        let entry = |state: &mut RenderState<'static>,
                     terminal: &Terminal<'static, 'static>,
                     index: usize| {
            let snapshot = state.update(terminal).unwrap();
            let color = snapshot.colors().unwrap().palette[index];
            (color.r, color.g, color.b)
        };
        apply_theme(&mut terminal, &Config::default());
        let factory = (
            entry(&mut state, &terminal, 16),
            entry(&mut state, &terminal, 17),
        );

        apply_theme(
            &mut terminal,
            &config_from(
                r##"local t = {} for i = 1, 16 do t[i] = "#000000" end t[17] = "#ffa066" t[18] = "#ff5d62"
                    return { colors = { ansi = t } }"##,
            ),
        );
        assert_eq!(entry(&mut state, &terminal, 16), (0xff, 0xa0, 0x66));
        assert_eq!(entry(&mut state, &terminal, 17), (0xff, 0x5d, 0x62));

        // Al quitar la paleta propia vuelven los de fábrica
        apply_theme(&mut terminal, &Config::default());
        assert_eq!(
            (
                entry(&mut state, &terminal, 16),
                entry(&mut state, &terminal, 17)
            ),
            factory
        );
    }
}
