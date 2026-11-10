//! Configuración en Lua: carga, validación y valores por defecto.
//!
//! `config.lua` devuelve una tabla que se convierte a [`Config`] con serde. Todo es opcional y
//! los campos desconocidos son un error, para que un `font_sise` no se ignore en silencio.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use libghostty_vt::style::RgbColor;
use mlua::{Lua, Table, Value};
use serde::{Deserialize, Deserializer, de};

use crate::actions::{self, Script, lua_message};

pub const DEFAULT_TITLE: &str = "Ghostty Rust Terminal Emulator";

#[cfg(windows)]
const DEFAULT_FAMILY: &str = "Consolas";
// Las dos primeras son las fuentes de solo símbolos de Nerd Fonts (iconos, Powerline). Si no están
// instaladas se ignoran, y como último recurso se usa cualquier Nerd Font que haya (ver glyphs.rs).
#[cfg(windows)]
const DEFAULT_FALLBACK: &[&str] = &[
    "Symbols Nerd Font Mono",
    "Symbols Nerd Font",
    "Segoe UI Symbol",
    "Microsoft YaHei",
    "Malgun Gothic",
    "Segoe UI Emoji",
];
#[cfg(target_os = "macos")]
const DEFAULT_FAMILY: &str = "Menlo";
// Apple Color Emoji is left out: its glyphs are color bitmaps (sbix) that fontdue cannot rasterize
#[cfg(target_os = "macos")]
const DEFAULT_FALLBACK: &[&str] = &[
    "Symbols Nerd Font Mono",
    "Symbols Nerd Font",
    "Apple Symbols",
    "PingFang SC",
    "Apple SD Gothic Neo",
];
#[cfg(not(any(windows, target_os = "macos")))]
const DEFAULT_FAMILY: &str = "DejaVu Sans Mono";
#[cfg(not(any(windows, target_os = "macos")))]
const DEFAULT_FALLBACK: &[&str] = &["Symbols Nerd Font Mono", "Symbols Nerd Font", "DejaVu Sans"];

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub font: FontConfig,
    /// Margen alrededor de la cuadrícula, en pixeles lógicos
    pub padding: f64,
    pub cursor: CursorConfig,
    pub colors: ColorsConfig,
    pub shell: ShellConfig,
    pub window: WindowConfig,
    pub mouse: MouseConfig,
    pub panes: PanesConfig,
    /// Líneas de historial (solo al arrancar)
    pub scrollback: usize,
    pub on_shell_exit: ShellExit,
    /// Quita las teclas que la terminal enlaza por defecto (copiar, pegar, desplazar). Las que
    /// definas en `keys` siempre mandan sobre las de por defecto.
    pub disable_default_keys: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font: FontConfig::default(),
            padding: 6.0,
            cursor: CursorConfig::default(),
            colors: ColorsConfig::default(),
            shell: ShellConfig::default(),
            window: WindowConfig::default(),
            mouse: MouseConfig::default(),
            panes: PanesConfig::default(),
            scrollback: 10_000,
            on_shell_exit: ShellExit::Close,
            disable_default_keys: false,
        }
    }
}

/// Varios paneles: cómo se distingue cuál tiene el foco
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PanesConfig {
    /// Color de los divisores entre paneles. Por defecto, el del texto muy atenuado.
    pub border_color: Option<Color>,
    /// Color del marco del panel con foco. Por defecto, el del cursor.
    pub focus_color: Option<Color>,
    /// Color del marco mientras el prefijo (`leader`) espera la tecla siguiente. Por defecto, el
    /// amarillo de la paleta (índice 3).
    pub leader_color: Option<Color>,
    /// Cuánto se apaga el texto de los paneles sin foco: 0 no cambia nada, 1 lo hace invisible
    pub inactive_dim: f32,
    /// Un panel nuevo (o un flotante) arranca en el directorio del panel con foco, si su shell lo
    /// informa con OSC 7; si no, en `shell.cwd`
    pub inherit_cwd: bool,
    /// Tamaño de un flotante nuevo, como fracción de la ventana
    pub float_width: f32,
    pub float_height: f32,
}

impl Default for PanesConfig {
    fn default() -> Self {
        Self {
            border_color: None,
            focus_color: None,
            leader_color: None,
            inactive_dim: 0.35,
            inherit_cwd: true,
            float_width: 0.7,
            float_height: 0.7,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FontConfig {
    /// Nombre de la familia tal como la ve el sistema ("Consolas", "JetBrains Mono"...)
    pub family: String,
    /// Tamaño en pixeles lógicos (se multiplica por el factor de escala de la pantalla)
    pub size: f32,
    /// Familias a probar, en orden, para los caracteres que la principal no cubre
    pub fallback: Vec<String>,
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            family: DEFAULT_FAMILY.to_string(),
            size: 16.0,
            fallback: DEFAULT_FALLBACK
                .iter()
                .map(|name| (*name).to_string())
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CursorStyle {
    Block,
    Bar,
    Underline,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CursorConfig {
    pub style: CursorStyle,
    /// Parpadeo del cursor. Los programas que piden un cursor parpadeante (DECSCUSR) lo hacen
    /// parpadear aunque esto sea `false`. Solo parpadea con la ventana enfocada, y se queda fijo
    /// mientras se escribe o llega salida.
    pub blink: bool,
    /// Milisegundos que dura cada fase (visible u oculto)
    pub blink_ms: u32,
}

impl Default for CursorConfig {
    fn default() -> Self {
        Self {
            style: CursorStyle::Block,
            blink: false,
            blink_ms: 530,
        }
    }
}

/// Color en texto: "#rrggbb", "#rgb" o un nombre X11 ("tomato")
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color(pub RgbColor);

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        RgbColor::parse(&text).map(Color).map_err(|_| {
            de::Error::custom(format!(
                "color no válido: {text:?} (usa \"#rrggbb\" o un nombre X11)"
            ))
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ColorsConfig {
    pub foreground: Option<Color>,
    pub background: Option<Color>,
    pub cursor: Option<Color>,
    /// Colores de la paleta desde el índice 0 (0–15 son los ANSI, 16–255 el cubo de color y los
    /// grises); lo que no se da queda por defecto
    pub ansi: Option<Vec<Color>>,
    /// Colores del texto seleccionado. Si faltan, se invierten los de la pantalla (el fondo toma el
    /// color del texto y viceversa).
    pub selection_foreground: Option<Color>,
    pub selection_background: Option<Color>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShellConfig {
    /// Si falta, se usa el shell por defecto del sistema (solo al arrancar)
    pub program: Option<String>,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    /// PowerShell no cuenta en qué directorio está (su `cd` no cambia el del proceso). Con esto la
    /// terminal lo lanza con `-NoExit -Command` y un script que envuelve el prompt para informarlo
    /// por OSC 7, y así los paneles nuevos y los flotantes arrancan donde estás. No se aplica si
    /// `args` ya lleva `-Command`, `-File` o `-EncodedCommand`. Solo al arrancar.
    pub integration: bool,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            program: None,
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            integration: true,
        }
    }
}

/// Modificadores en texto: "CTRL|SHIFT", "ALT"... (CTRL, SHIFT, ALT y SUPER)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Modifiers(pub u8);

impl<'de> Deserialize<'de> for Modifiers {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        actions::parse_mods(&text)
            .map(Modifiers)
            .map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decorations {
    /// Con la barra de título y sus botones del sistema
    Full,
    /// Sin barra ni botones. La ventana se redimensiona por los bordes y se mueve con
    /// `window.drag_modifiers` + arrastrar.
    None,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WindowConfig {
    /// Título fijo de la ventana. Si falta, la ventana muestra el título que pone el programa
    /// (`DEFAULT_TITLE` hasta que ponga uno).
    pub title: Option<String>,
    pub decorations: Decorations,
    /// Con `decorations = "none"`: modificadores que, mantenidos al arrastrar con el botón
    /// izquierdo, mueven la ventana
    pub drag_modifiers: Modifiers,
    /// Esquinas redondeadas de la ventana, como decide el sistema (`true`) o cuadradas (`false`).
    /// Solo tiene efecto en Windows 11.
    pub rounded_corners: bool,
    /// Tamaño inicial de la cuadrícula (solo al arrancar). Si faltan, la ventana tiene el tamaño
    /// que le dé el sistema; si solo hay uno, el otro es 80 columnas o 24 filas. Es una petición:
    /// un gestor de ventanas en mosaico puede ignorarla.
    pub columns: Option<u16>,
    pub rows: Option<u16>,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            title: None,
            decorations: Decorations::Full,
            drag_modifiers: Modifiers(actions::ALT),
            rounded_corners: true,
            columns: None,
            rows: None,
        }
    }
}

/// Qué hace el clic derecho cuando el programa no se queda con el ratón
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RightClick {
    Paste,
    None,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MouseConfig {
    /// Al soltar el botón tras seleccionar, el texto va al portapapeles
    pub copy_on_select: bool,
    pub right_click: RightClick,
    /// Líneas que baja o sube cada giro de la rueda
    pub scroll_lines: u16,
    /// Modificadores que, mantenidos al arrastrar, hacen una selección rectangular
    pub rectangle_modifiers: Modifiers,
}

impl Default for MouseConfig {
    fn default() -> Self {
        Self {
            copy_on_select: true,
            right_click: RightClick::Paste,
            scroll_lines: 3,
            rectangle_modifiers: Modifiers(actions::CTRL | actions::ALT),
        }
    }
}

/// Qué hacer cuando el shell termina
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShellExit {
    /// Cerrar la ventana
    Close,
    /// Dejar la ventana abierta con lo que quedó en pantalla
    Hold,
}

impl Config {
    fn validate(&self) -> Result<(), String> {
        if !(self.font.size.is_finite() && (4.0..=200.0).contains(&self.font.size)) {
            return Err(format!(
                "font.size debe estar entre 4 y 200 (recibió {})",
                self.font.size
            ));
        }
        if self.font.family.trim().is_empty() {
            return Err("font.family no puede estar vacío".to_string());
        }
        if !(self.padding.is_finite() && (0.0..=200.0).contains(&self.padding)) {
            return Err(format!(
                "padding debe estar entre 0 y 200 (recibió {})",
                self.padding
            ));
        }
        if !(100..=5000).contains(&self.cursor.blink_ms) {
            return Err(format!(
                "cursor.blink_ms debe estar entre 100 y 5000 (recibió {})",
                self.cursor.blink_ms
            ));
        }
        if self.window.columns == Some(0) || self.window.rows == Some(0) {
            return Err("window.columns y window.rows deben ser mayores que 0".to_string());
        }
        if !(1..=100).contains(&self.mouse.scroll_lines) {
            return Err(format!(
                "mouse.scroll_lines debe estar entre 1 y 100 (recibió {})",
                self.mouse.scroll_lines
            ));
        }
        // Arrastrar sin modificadores es seleccionar: los dos gestos necesitan al menos uno, y
        // distinto entre sí (si no, no habría forma de saber cuál se pide)
        if self.mouse.rectangle_modifiers.0 == 0 {
            return Err(
                "mouse.rectangle_modifiers no puede estar vacío (arrastrar solo ya selecciona)"
                    .to_string(),
            );
        }
        if self.window.decorations == Decorations::None {
            if self.window.drag_modifiers.0 == 0 {
                return Err(
                    "window.drag_modifiers no puede estar vacío (arrastrar solo ya selecciona)"
                        .to_string(),
                );
            }
            if self.window.drag_modifiers == self.mouse.rectangle_modifiers {
                return Err(
                    "window.drag_modifiers y mouse.rectangle_modifiers no pueden ser iguales"
                        .to_string(),
                );
            }
        }
        if self.shell.program.is_none() && !self.shell.args.is_empty() {
            return Err("shell.args necesita shell.program (con el shell por defecto no se pueden pasar argumentos)".to_string());
        }
        if !(0.0..=1.0).contains(&self.panes.inactive_dim) {
            return Err(format!(
                "panes.inactive_dim debe estar entre 0 y 1 (recibió {})",
                self.panes.inactive_dim
            ));
        }
        for (name, value) in [
            ("panes.float_width", self.panes.float_width),
            ("panes.float_height", self.panes.float_height),
        ] {
            if !(0.1..=1.0).contains(&value) {
                return Err(format!("{name} debe estar entre 0,1 y 1 (recibió {value})"));
            }
        }
        if let Some(ansi) = &self.colors.ansi
            && ansi.len() > 256
        {
            return Err(format!(
                "colors.ansi admite como máximo 256 colores (recibió {})",
                ansi.len()
            ));
        }
        Ok(())
    }
}

/// Resultado de cargar la configuración al arrancar: nunca falla; si hay un error se usan los
/// valores por defecto y el motivo viaja en `error` para mostrarlo.
pub struct Loaded {
    pub config: Config,
    /// Teclas, hooks y el intérprete de Lua que los ejecuta
    pub script: Script,
    pub error: Option<String>,
    pub path: Option<PathBuf>,
}

/// `TERMINAL_CONFIG`, o `%APPDATA%\terminal\config.lua` en Windows, o
/// `$XDG_CONFIG_HOME`/`~/.config` + `terminal/config.lua` en el resto.
pub fn config_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("TERMINAL_CONFIG") {
        return Some(PathBuf::from(path));
    }
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    base.map(|base| base.join("terminal").join("config.lua"))
}

pub fn load() -> Loaded {
    let path = config_path();
    let result = match &path {
        Some(path) => load_script_path(path),
        None => default_script(),
    };
    match result {
        Ok((config, script)) => Loaded {
            config,
            script,
            error: None,
            path,
        },
        Err(error) => {
            let (config, script) = default_script().expect("no se pudo crear el intérprete de Lua");
            Loaded {
                config,
                script,
                error: Some(error),
                path,
            }
        }
    }
}

/// La configuración de fábrica (la de un archivo que devuelve una tabla vacía): también con sus
/// teclas por defecto y su prefijo
fn default_script() -> Result<(Config, Script), String> {
    parse_script("return {}", "config.lua", None)
}

/// Lee y evalúa un archivo de configuración. Si no existe, devuelve los valores por defecto.
pub fn load_script_path(path: &Path) -> Result<(Config, Script), String> {
    match std::fs::read_to_string(path) {
        Ok(source) => {
            let name = path.file_name().map_or_else(
                || "config.lua".to_string(),
                |name| name.to_string_lossy().into_owned(),
            );
            parse_script(&source, &name, path.parent())
        }
        Err(e) if e.kind() == ErrorKind::NotFound => default_script(),
        Err(e) => Err(format!("no se pudo leer {}: {e}", path.display())),
    }
}

/// Solo la parte declarativa (sin teclas ni hooks); lo usan las pruebas
#[cfg(test)]
pub fn parse(source: &str, chunk_name: &str, dir: Option<&Path>) -> Result<Config, String> {
    parse_script(source, chunk_name, dir).map(|(config, _script)| config)
}

/// Evalúa `source` (el contenido de config.lua) y lo convierte en [`Config`] más el [`Script`]
/// con sus teclas y hooks. `dir` es la carpeta desde la que `require` busca módulos.
pub fn parse_script(
    source: &str,
    chunk_name: &str,
    dir: Option<&Path>,
) -> Result<(Config, Script), String> {
    let mut script = Script::new().map_err(|e| lua_message(&e))?;
    prepare(script.lua(), dir).map_err(|e| lua_message(&e))?;

    let value: Value = script
        .lua()
        .load(source)
        .set_name(format!("={chunk_name}"))
        .eval()
        .map_err(|e| lua_message(&e))?;
    let Value::Table(table) = value else {
        return Err(format!(
            "{chunk_name} debe terminar con `return {{ ... }}` (devolvió {})",
            value.type_name()
        ));
    };

    // `keys` y `events` llevan funciones, que serde no puede deserializar: se sacan de la tabla
    // antes y se interpretan aparte
    let keys: Value = table.get("keys").map_err(|e| lua_message(&e))?;
    let events: Value = table.get("events").map_err(|e| lua_message(&e))?;
    let leader: Value = table.get("leader").map_err(|e| lua_message(&e))?;
    table.set("keys", Value::Nil).map_err(|e| lua_message(&e))?;
    table
        .set("events", Value::Nil)
        .map_err(|e| lua_message(&e))?;
    table
        .set("leader", Value::Nil)
        .map_err(|e| lua_message(&e))?;

    let deserializer = mlua::serde::Deserializer::new(Value::Table(table));
    let config: Config =
        serde_path_to_error::deserialize(deserializer).map_err(|e| format!("{chunk_name}: {e}"))?;
    config
        .validate()
        .map_err(|e| format!("{chunk_name}: {e}"))?;

    let bindings = actions::parse_keys(script.lua(), keys, !config.disable_default_keys)
        .map_err(|e| format!("{chunk_name}: {e}"))?;
    let events = actions::parse_events(events).map_err(|e| format!("{chunk_name}: {e}"))?;
    let leader = actions::parse_leader(leader).map_err(|e| format!("{chunk_name}: {e}"))?;
    script.set_bindings(bindings);
    script.set_events(events);
    script
        .set_leader(leader)
        .map_err(|e| format!("{chunk_name}: {e}"))?;
    Ok((config, script))
}

/// Acciones nativas que se enlazan a una tecla desde config.lua (`terminal.action.Paste`...). Son
/// tablas con la clave `action`, que `actions::parse_action` convierte en [`actions::Action`].
const ACTION_PRELUDE: &str = r#"
local action = {}
action.SendString = function(text) return { action = "SendString", text = text } end
action.ScrollLines = function(amount) return { action = "ScrollLines", amount = amount } end
action.ScrollPages = function(amount) return { action = "ScrollPages", amount = amount } end
action.FocusPane = function(dir) return { action = "FocusPane", dir = dir } end
action.ResizePane = function(dir, amount) return { action = "ResizePane", dir = dir, amount = amount or 1 } end
action.MovePane = function(dir, amount) return { action = "MovePane", dir = dir, amount = amount or 1 } end
action.NewFloat = function(options)
  local result = { action = "NewFloat" }
  for key, value in pairs(options or {}) do result[key] = value end
  return result
end
for _, name in ipairs({
  "ScrollToTop", "ScrollToBottom", "IncreaseFontSize", "DecreaseFontSize", "ResetFontSize",
  "ToggleFullscreen", "ReloadConfig", "Paste", "Copy", "ClearSelection", "SelectAll", "Quit",
  "SplitRight", "SplitDown", "ClosePane", "FocusNext", "FocusPrevious", "ToggleZoom",
  "ToggleFloats",
}) do
  action[name] = { action = name }
end
return action
"#;

/// Lo que `config.lua` tiene a mano además de la biblioteca estándar de Lua
fn prepare(lua: &Lua, dir: Option<&Path>) -> mlua::Result<()> {
    let terminal = lua.create_table()?;
    terminal.set("os", std::env::consts::OS)?;
    terminal.set(
        "action",
        lua.load(ACTION_PRELUDE)
            .set_name("=prelude")
            .eval::<Table>()?,
    )?;
    terminal.set(
        "log",
        lua.create_function(|_, message: String| {
            eprintln!("[config] {message}");
            Ok(())
        })?,
    )?;

    if let Some(dir) = dir {
        let dir = dir.display().to_string().replace('\\', "/");
        terminal.set("config_dir", dir.clone())?;
        // Para dividir la configuración en módulos con `require`
        let package: Table = lua.globals().get("package")?;
        let current: String = package.get("path")?;
        package.set("path", format!("{dir}/?.lua;{dir}/?/init.lua;{current}"))?;
    }

    lua.globals().set("terminal", terminal)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(source: &str) -> Result<Config, String> {
        parse(source, "config.lua", None)
    }

    fn rgb(r: u8, g: u8, b: u8) -> Option<Color> {
        Some(Color(RgbColor { r, g, b }))
    }

    #[test]
    fn empty_table_gives_defaults() {
        assert_eq!(parse_str("return {}").unwrap(), Config::default());
    }

    #[test]
    fn defaults_match_the_previous_hardcoded_behavior() {
        let config = Config::default();
        assert_eq!(config.font.size, 16.0);
        assert_eq!(config.padding, 6.0);
        // Sin tamaño pedido, la ventana conserva el que le da el sistema
        assert_eq!((config.window.columns, config.window.rows), (None, None));
        // Sin título fijo, la ventana sigue al programa (y arranca con DEFAULT_TITLE)
        assert_eq!(config.window.title, None);
        assert_eq!(DEFAULT_TITLE, "Ghostty Rust Terminal Emulator");
        assert_eq!(config.scrollback, 10_000);
        assert_eq!(config.on_shell_exit, ShellExit::Close);
        assert!(config.shell.program.is_none());
    }

    #[test]
    fn full_table_is_read() {
        let config = parse_str(
            r##"
            return {
              font = { family = "JetBrains Mono", size = 14, fallback = { "Segoe UI Symbol" } },
              padding = 10,
              cursor = { style = "bar" },
              colors = { foreground = "#d4d4d4", background = "#1e1e1e", cursor = "#ffffff",
                         ansi = { "#000000", "#cd3131" } },
              shell = { program = "pwsh", args = { "-NoLogo" }, cwd = "C:/work", env = { A = "1" } },
              window = { title = "mi terminal", columns = 100, rows = 30 },
              scrollback = 5000,
              on_shell_exit = "hold",
            }
            "##,
        )
        .unwrap();
        assert_eq!(config.font.family, "JetBrains Mono");
        assert_eq!(config.font.size, 14.0);
        assert_eq!(config.font.fallback, vec!["Segoe UI Symbol"]);
        assert_eq!(config.cursor.style, CursorStyle::Bar);
        assert_eq!(config.colors.foreground, rgb(0xd4, 0xd4, 0xd4));
        assert_eq!(config.colors.background, rgb(0x1e, 0x1e, 0x1e));
        assert_eq!(config.colors.ansi.as_ref().map(Vec::len), Some(2));
        assert_eq!(config.shell.program.as_deref(), Some("pwsh"));
        assert_eq!(config.shell.args, vec!["-NoLogo"]);
        assert_eq!(config.shell.env.get("A").map(String::as_str), Some("1"));
        assert_eq!(
            (config.window.columns, config.window.rows),
            (Some(100), Some(30))
        );
        assert_eq!(config.window.title.as_deref(), Some("mi terminal"));
        assert_eq!(config.scrollback, 5000);
        assert_eq!(config.on_shell_exit, ShellExit::Hold);
    }

    #[test]
    fn empty_lua_tables_are_accepted_for_lists_and_maps() {
        let config =
            parse_str("return { font = { fallback = {} }, shell = { args = {}, env = {} } }")
                .unwrap();
        assert!(config.font.fallback.is_empty());
        assert!(config.shell.args.is_empty());
        assert!(config.shell.env.is_empty());
    }

    #[test]
    fn lua_logic_can_decide_values() {
        let config = parse_str(
            r#"
            local size = 12 + 2
            if terminal.os == "windows" then size = size + 2 end
            return { font = { size = size }, window = { title = terminal.os } }
            "#,
        )
        .unwrap();
        let expected = if cfg!(windows) { 16.0 } else { 14.0 };
        assert_eq!(config.font.size, expected);
        assert_eq!(config.window.title.as_deref(), Some(std::env::consts::OS));
    }

    #[test]
    fn unknown_field_is_an_error_naming_it() {
        let error = parse_str("return { font_sise = 12 }").unwrap_err();
        assert!(error.contains("font_sise"), "{error}");
        let error = parse_str("return { font = { sise = 12 } }").unwrap_err();
        assert!(error.contains("sise"), "{error}");
    }

    #[test]
    fn wrong_type_reports_the_field_path() {
        let error = parse_str(r#"return { font = { size = "grande" } }"#).unwrap_err();
        assert!(error.contains("font.size"), "{error}");
    }

    #[test]
    fn invalid_color_reports_path_and_value() {
        let error =
            parse_str(r#"return { colors = { foreground = "no-es-un-color" } }"#).unwrap_err();
        assert!(error.contains("colors.foreground"), "{error}");
        assert!(error.contains("no-es-un-color"), "{error}");
    }

    #[test]
    fn invalid_enum_value_lists_the_options() {
        let error = parse_str(r#"return { cursor = { style = "rombo" } }"#).unwrap_err();
        assert!(error.contains("cursor.style"), "{error}");
        assert!(error.contains("block"), "{error}");
    }

    #[test]
    fn x11_color_names_are_accepted() {
        let config = parse_str(r#"return { colors = { background = "black" } }"#).unwrap();
        assert_eq!(config.colors.background, rgb(0, 0, 0));
    }

    #[test]
    fn syntax_error_mentions_file_and_line() {
        let error = parse_str("local x = 1\nreturn { font_size = }").unwrap_err();
        assert!(error.contains("config.lua:2"), "{error}");
        assert!(!error.contains("stack traceback"), "{error}");
    }

    #[test]
    fn runtime_error_mentions_file_and_line() {
        let error = parse_str("local x = nil\nreturn { padding = x.y }").unwrap_err();
        assert!(error.contains("config.lua:2"), "{error}");
    }

    #[test]
    fn missing_return_is_explained() {
        let error = parse_str("local x = 1").unwrap_err();
        assert!(error.contains("return"), "{error}");
    }

    #[test]
    fn returning_a_non_table_is_an_error() {
        assert!(parse_str("return 42").is_err());
    }

    #[test]
    fn out_of_range_values_are_rejected() {
        assert!(
            parse_str("return { font = { size = 0 } }")
                .unwrap_err()
                .contains("font.size")
        );
        assert!(
            parse_str("return { padding = -1 }")
                .unwrap_err()
                .contains("padding")
        );
        assert!(
            parse_str("return { window = { columns = 0 } }")
                .unwrap_err()
                .contains("window")
        );
        assert!(
            parse_str("return { window = { rows = 0 } }")
                .unwrap_err()
                .contains("window")
        );
        let palette = |count: u32| {
            format!(
                "local t = {{}} for i = 1, {count} do t[i] = '#000' end return {{ colors = {{ ansi = t }} }}"
            )
        };
        assert!(parse_str(&palette(256)).is_ok());
        assert!(
            parse_str(&palette(257))
                .unwrap_err()
                .contains("colors.ansi")
        );
    }

    #[test]
    fn the_palette_can_go_past_the_sixteen_ansi_colors() {
        // Kanagawa define los índices 16 y 17 además de los 16 ANSI
        let config = parse_str(
            "local t = {} for i = 1, 16 do t[i] = '#000000' end t[17] = '#ffa066' t[18] = '#ff5d62' \
             return { colors = { ansi = t } }",
        )
        .unwrap();
        let ansi = config.colors.ansi.unwrap();
        assert_eq!(ansi.len(), 18);
        assert_eq!(Some(ansi[16]), rgb(0xff, 0xa0, 0x66));
        assert_eq!(Some(ansi[17]), rgb(0xff, 0x5d, 0x62));
    }

    #[test]
    fn require_finds_modules_next_to_the_config() {
        let dir = std::env::temp_dir().join(format!("terminal-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("theme.lua"), r##"return { fg = "#102030" }"##).unwrap();
        let config = parse(
            r#"local theme = require("theme"); return { colors = { foreground = theme.fg } }"#,
            "config.lua",
            Some(&dir),
        );
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(config.unwrap().colors.foreground, rgb(0x10, 0x20, 0x30));
    }

    #[test]
    fn shell_args_without_a_program_are_rejected() {
        let error = parse_str(r#"return { shell = { args = { "-NoLogo" } } }"#).unwrap_err();
        assert!(error.contains("shell.args"), "{error}");
        assert!(
            parse_str(r#"return { shell = { program = "pwsh", args = { "-NoLogo" } } }"#).is_ok()
        );
        // cwd y env sí valen con el shell por defecto
        assert!(parse_str(r#"return { shell = { cwd = "C:/", env = { A = "1" } } }"#).is_ok());
    }

    #[test]
    fn cursor_blink_is_off_by_default_and_configurable() {
        let defaults = Config::default();
        assert!(!defaults.cursor.blink);
        assert_eq!(defaults.cursor.blink_ms, 530);

        let config =
            parse_str("return { cursor = { style = 'bar', blink = true, blink_ms = 300 } }")
                .unwrap();
        assert!(config.cursor.blink);
        assert_eq!(config.cursor.blink_ms, 300);
    }

    #[test]
    fn blink_interval_must_be_sensible() {
        for bad in ["50", "99", "5001", "100000"] {
            let error =
                parse_str(&format!("return {{ cursor = {{ blink_ms = {bad} }} }}")).unwrap_err();
            assert!(error.contains("cursor.blink_ms"), "{error}");
        }
        assert!(parse_str("return { cursor = { blink_ms = 100 } }").is_ok());
        assert!(parse_str("return { cursor = { blink_ms = 5000 } }").is_ok());
    }

    fn script_from(source: &str) -> Result<Script, String> {
        parse_script(source, "config.lua", None).map(|(_config, script)| script)
    }

    fn char_key(text: &str) -> winit::keyboard::Key {
        winit::keyboard::Key::Character(text.into())
    }

    /// Pulsa una tecla ante el prefijo de un script; `mods` son los bits de `actions::*`
    fn step(
        script: &Script,
        state: &mut actions::LeaderState,
        key: &winit::keyboard::Key,
        mods: u8,
    ) -> actions::LeaderOutcome {
        script.leader_step(state, key, mods, false, std::time::Instant::now())
    }

    fn named(key: winit::keyboard::NamedKey) -> winit::keyboard::Key {
        winit::keyboard::Key::Named(key)
    }

    #[test]
    fn the_default_leader_is_ctrl_b_and_a_split_follows_it() {
        use actions::{Action, LeaderOutcome, LeaderState};
        let script = script_from("return {}").unwrap();
        let mut state = LeaderState::default();
        // Sin el prefijo, el % es una tecla normal
        assert!(matches!(
            step(&script, &mut state, &char_key("%"), actions::SHIFT),
            LeaderOutcome::Normal
        ));
        // Ctrl+B arma el prefijo y no llega al shell
        assert!(matches!(
            step(&script, &mut state, &char_key("b"), actions::CTRL),
            LeaderOutcome::Arm
        ));
        assert!(state.pending());
        // % llega como Shift+5: el enlace es `%` con LEADER, sin SHIFT
        assert!(matches!(
            step(&script, &mut state, &char_key("%"), actions::SHIFT),
            LeaderOutcome::Run(Action::SplitRight)
        ));
        assert!(!state.pending(), "el prefijo se consume con una tecla");
        assert!(matches!(
            step(&script, &mut state, &char_key("%"), actions::SHIFT),
            LeaderOutcome::Normal
        ));
    }

    #[test]
    fn keys_after_the_leader_run_pane_actions() {
        use actions::{Action, Direction, LeaderOutcome, LeaderState};
        use winit::keyboard::NamedKey;
        let script = script_from("return {}").unwrap();
        let mut state = LeaderState::default();
        let mut after = |key: winit::keyboard::Key, mods: u8| {
            assert!(matches!(
                step(&script, &mut state, &char_key("b"), actions::CTRL),
                LeaderOutcome::Arm
            ));
            step(&script, &mut state, &key, mods)
        };
        assert!(matches!(
            after(char_key("\""), actions::SHIFT),
            LeaderOutcome::Run(Action::SplitDown)
        ));
        assert!(matches!(
            after(char_key("x"), 0),
            LeaderOutcome::Run(Action::ClosePane)
        ));
        assert!(matches!(
            after(char_key("z"), 0),
            LeaderOutcome::Run(Action::ToggleZoom)
        ));
        assert!(matches!(
            after(char_key("o"), 0),
            LeaderOutcome::Run(Action::FocusNext)
        ));
        assert!(matches!(
            after(char_key("h"), 0),
            LeaderOutcome::Run(Action::FocusPane {
                dir: Direction::Left
            })
        ));
        assert!(matches!(
            after(named(NamedKey::ArrowDown), 0),
            LeaderOutcome::Run(Action::FocusPane {
                dir: Direction::Down
            })
        ));
        assert!(matches!(
            after(named(NamedKey::ArrowLeft), actions::CTRL),
            LeaderOutcome::Run(Action::ResizePane {
                dir: Direction::Left,
                amount: 1
            })
        ));
        assert!(matches!(
            after(named(NamedKey::ArrowRight), actions::ALT),
            LeaderOutcome::Run(Action::ResizePane {
                dir: Direction::Right,
                amount: 5
            })
        ));
        assert!(matches!(
            after(named(NamedKey::ArrowUp), actions::SHIFT),
            LeaderOutcome::Run(Action::MovePane {
                dir: Direction::Up,
                amount: 2
            })
        ));
        // w y W son distintas aunque se pruebe sin SHIFT cuando no hay enlace exacto
        assert!(matches!(
            after(char_key("w"), 0),
            LeaderOutcome::Run(Action::ToggleFloats)
        ));
        assert!(matches!(
            after(char_key("W"), actions::SHIFT),
            LeaderOutcome::Run(Action::NewFloat { program: None, .. })
        ));
    }

    #[test]
    fn the_leader_ignores_lone_modifiers_and_repeats_and_cancels_on_unknown_keys() {
        use actions::{LeaderOutcome, LeaderState};
        use winit::keyboard::NamedKey;
        let script = script_from("return {}").unwrap();
        let mut state = LeaderState::default();
        let now = std::time::Instant::now();

        // Mantener Ctrl+B pulsado no lo arma dos veces ni lo reenvía
        assert!(matches!(
            step(&script, &mut state, &char_key("b"), actions::CTRL),
            LeaderOutcome::Arm
        ));
        assert!(matches!(
            script.leader_step(&mut state, &char_key("b"), actions::CTRL, true, now),
            LeaderOutcome::Ignore
        ));
        // Pulsar Shift para escribir % no cancela
        assert!(matches!(
            step(&script, &mut state, &named(NamedKey::Shift), actions::SHIFT),
            LeaderOutcome::Ignore
        ));
        assert!(state.pending());
        assert!(matches!(
            step(&script, &mut state, &char_key("%"), actions::SHIFT),
            LeaderOutcome::Run(_)
        ));

        // Escape cancela y no hace nada más
        step(&script, &mut state, &char_key("b"), actions::CTRL);
        assert!(matches!(
            step(&script, &mut state, &named(NamedKey::Escape), 0),
            LeaderOutcome::Cancel
        ));
        assert!(!state.pending());
        // Una tecla sin enlace cancela y se descarta
        step(&script, &mut state, &char_key("b"), actions::CTRL);
        assert!(matches!(
            step(&script, &mut state, &char_key("q"), 0),
            LeaderOutcome::Cancel
        ));
        assert!(!state.pending());
        // Con el prefijo ya cancelado, la siguiente tecla es normal
        assert!(matches!(
            step(&script, &mut state, &char_key("x"), 0),
            LeaderOutcome::Normal
        ));
    }

    #[test]
    fn pressing_the_leader_twice_sends_its_control_character() {
        use actions::{LeaderOutcome, LeaderState};
        let script = script_from("return {}").unwrap();
        let mut state = LeaderState::default();
        step(&script, &mut state, &char_key("b"), actions::CTRL);
        match step(&script, &mut state, &char_key("b"), actions::CTRL) {
            LeaderOutcome::Passthrough(bytes) => assert_eq!(bytes, vec![0x02]),
            other => panic!("{other:?}"),
        }
        assert!(!state.pending());
        // Un prefijo que no es Ctrl+letra no tiene carácter que reenviar: solo cancela
        let script = script_from(r#"return { leader = { key = "Space", mods = "ALT" } }"#).unwrap();
        step(
            &script,
            &mut state,
            &named(winit::keyboard::NamedKey::Space),
            actions::ALT,
        );
        assert!(matches!(
            step(
                &script,
                &mut state,
                &named(winit::keyboard::NamedKey::Space),
                actions::ALT
            ),
            LeaderOutcome::Cancel
        ));
    }

    #[test]
    fn the_leader_expires_after_its_timeout() {
        use actions::{LeaderOutcome, LeaderState};
        let script =
            script_from(r#"return { leader = { key = "b", mods = "CTRL", timeout_ms = 500 } }"#)
                .unwrap();
        let mut state = LeaderState::default();
        let start = std::time::Instant::now();
        assert!(matches!(
            script.leader_step(&mut state, &char_key("b"), actions::CTRL, false, start),
            LeaderOutcome::Arm
        ));
        assert_eq!(
            state.deadline(),
            Some(start + std::time::Duration::from_millis(500))
        );
        // El temporizador de la app lo cancela al vencer
        assert!(!state.expire(start + std::time::Duration::from_millis(499)));
        assert!(state.expire(start + std::time::Duration::from_millis(500)));
        assert!(!state.pending());
        // Y una pulsación tardía, aún sin que el temporizador haya corrido, es una tecla normal
        script.leader_step(&mut state, &char_key("b"), actions::CTRL, false, start);
        assert!(matches!(
            script.leader_step(
                &mut state,
                &char_key("x"),
                0,
                false,
                start + std::time::Duration::from_secs(2)
            ),
            LeaderOutcome::Normal
        ));
    }

    #[test]
    fn a_custom_or_disabled_leader() {
        use actions::{LeaderOutcome, LeaderState};
        let mut state = LeaderState::default();

        let custom =
            script_from(r#"return { leader = { key = "a", mods = "CTRL|SHIFT" } }"#).unwrap();
        assert!(matches!(
            step(
                &custom,
                &mut state,
                &char_key("a"),
                actions::CTRL | actions::SHIFT
            ),
            LeaderOutcome::Arm
        ));
        state.clear();
        assert!(matches!(
            step(&custom, &mut state, &char_key("b"), actions::CTRL),
            LeaderOutcome::Normal
        ));

        let off = script_from("return { leader = false }").unwrap();
        assert!(matches!(
            step(&off, &mut state, &char_key("b"), actions::CTRL),
            LeaderOutcome::Normal
        ));
        let on = script_from("return { leader = true }").unwrap();
        assert!(matches!(
            step(&on, &mut state, &char_key("b"), actions::CTRL),
            LeaderOutcome::Arm
        ));
    }

    #[test]
    fn user_keys_with_leader_replace_the_defaults() {
        use actions::{Action, LeaderOutcome, LeaderState};
        let script = script_from(
            r#"return { keys = {
              { key = "x", mods = "LEADER", action = "Quit" },
              { key = "v", mods = "LEADER", action = terminal.action.SplitRight },
              { key = "g", mods = "LEADER|CTRL", action = terminal.action.NewFloat({ program = "lazygit", args = { "--version" }, width = 0.9 }) },
            } }"#,
        )
        .unwrap();
        let mut state = LeaderState::default();
        step(&script, &mut state, &char_key("b"), actions::CTRL);
        assert!(matches!(
            step(&script, &mut state, &char_key("x"), 0),
            LeaderOutcome::Run(Action::Quit)
        ));
        step(&script, &mut state, &char_key("b"), actions::CTRL);
        assert!(matches!(
            step(&script, &mut state, &char_key("v"), 0),
            LeaderOutcome::Run(Action::SplitRight)
        ));
        step(&script, &mut state, &char_key("b"), actions::CTRL);
        match step(&script, &mut state, &char_key("g"), actions::CTRL) {
            LeaderOutcome::Run(Action::NewFloat {
                program,
                args,
                width,
                height,
                cwd,
            }) => {
                assert_eq!(program.as_deref(), Some("lazygit"));
                assert_eq!(args, vec!["--version"]);
                assert_eq!(width, Some(0.9));
                assert_eq!((height, cwd), (None, None));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn leader_errors_explain_themselves() {
        let error = script_from(r#"return { leader = { tecla = "b" } }"#)
            .err()
            .unwrap();
        assert!(error.contains("tecla"), "{error}");
        let error = script_from(r#"return { leader = { key = "NoExiste" } }"#)
            .err()
            .unwrap();
        assert!(error.contains("leader.key"), "{error}");
        let error = script_from(r#"return { leader = { mods = "LEADER" } }"#)
            .err()
            .unwrap();
        assert!(
            error.contains("leader.mods") && error.contains("LEADER"),
            "{error}"
        );
        for bad in ["100", "20000"] {
            let error = script_from(&format!("return {{ leader = {{ timeout_ms = {bad} }} }}"))
                .err()
                .unwrap();
            assert!(error.contains("leader.timeout_ms"), "{error}");
        }
        let error = script_from("return { leader = 3 }").err().unwrap();
        assert!(error.contains("leader debe ser"), "{error}");
        // Una tecla de `keys` con la misma combinación que el prefijo: no se sabría cuál hacer
        let error =
            script_from(r#"return { keys = { { key = "b", mods = "CTRL", action = "Paste" } } }"#)
                .err()
                .unwrap();
        assert!(
            error.contains("leader") && error.contains("enlazada"),
            "{error}"
        );
        // ...salvo que el prefijo esté desactivado
        assert!(script_from(r#"return { leader = false, keys = { { key = "b", mods = "CTRL", action = "Paste" } } }"#).is_ok());
    }

    #[test]
    fn the_leader_modifier_is_only_for_keys() {
        assert!(
            actions::parse_mods("LEADER")
                .unwrap_err()
                .contains("LEADER")
        );
        let error =
            parse_str(r#"return { window = { decorations = "none", drag_modifiers = "LEADER" } }"#)
                .unwrap_err();
        assert!(error.contains("LEADER"), "{error}");
        assert!(
            script_from(
                r#"return { keys = { { key = "x", mods = "LEADER|SHIFT", action = "Quit" } } }"#
            )
            .is_ok()
        );
    }

    #[test]
    fn without_a_config_file_the_defaults_still_have_their_keys_and_leader() {
        use actions::{LeaderOutcome, LeaderState};
        let path = std::env::temp_dir()
            .join("terminal-no-existe-2")
            .join("config.lua");
        let (_config, script) = load_script_path(&path).unwrap();
        assert!(matches!(
            script.binding(&char_key("C"), actions::CTRL | actions::SHIFT),
            Some(actions::Action::Copy)
        ));
        let mut state = LeaderState::default();
        assert!(matches!(
            step(&script, &mut state, &char_key("b"), actions::CTRL),
            LeaderOutcome::Arm
        ));
    }

    #[test]
    fn keys_bind_native_actions_with_exact_modifiers() {
        let script = script_from(
            r#"
            return { keys = {
              { key = "v", mods = "CTRL|SHIFT", action = terminal.action.Paste },
              { key = "PageUp", mods = "SHIFT", action = terminal.action.ScrollPages(-1) },
              { key = "l", mods = "ALT", action = terminal.action.SendString("clear\r") },
              { key = "q", mods = "CTRL", action = "Quit" },
            } }
            "#,
        )
        .unwrap();
        let key = |k: winit::keyboard::NamedKey| winit::keyboard::Key::Named(k);

        // Con Shift la tecla llega en mayúscula y aun así coincide
        assert!(matches!(
            script.binding(&char_key("V"), actions::CTRL | actions::SHIFT),
            Some(actions::Action::Paste)
        ));
        assert!(matches!(
            script.binding(&key(winit::keyboard::NamedKey::PageUp), actions::SHIFT),
            Some(actions::Action::ScrollPages { amount: -1 })
        ));
        assert!(matches!(
            script.binding(&char_key("l"), actions::ALT),
            Some(actions::Action::SendString { text }) if text == "clear\r"
        ));
        // La forma corta con el nombre de la acción
        assert!(matches!(
            script.binding(&char_key("q"), actions::CTRL),
            Some(actions::Action::Quit)
        ));
        // Los modificadores tienen que coincidir exactamente
        assert!(script.binding(&char_key("v"), actions::CTRL).is_none());
        assert!(
            script
                .binding(
                    &char_key("v"),
                    actions::CTRL | actions::SHIFT | actions::ALT
                )
                .is_none()
        );
        assert!(script.binding(&char_key("x"), 0).is_none());
    }

    #[test]
    fn a_lua_function_can_be_bound_to_a_key() {
        let script = script_from(
            r#"return { keys = { { key = "k", mods = "CTRL|ALT", action = function(ctx) ctx:send_text("ls\r") end } } }"#,
        )
        .unwrap();
        let Some(actions::Action::Lua(function)) =
            script.binding(&char_key("k"), actions::CTRL | actions::ALT)
        else {
            panic!("debería ser una función de Lua");
        };
        let commands = script.call(&function.clone(), vec![]);
        assert!(
            matches!(commands.as_slice(), [actions::Command::SendText(text)] if text == "ls\r")
        );
    }

    #[test]
    fn mouse_and_window_defaults_follow_the_chosen_behavior() {
        let config = Config::default();
        assert!(config.mouse.copy_on_select, "copiar al seleccionar");
        assert_eq!(config.mouse.right_click, RightClick::Paste);
        assert_eq!(config.mouse.scroll_lines, 3);
        assert_eq!(
            config.mouse.rectangle_modifiers,
            Modifiers(actions::CTRL | actions::ALT)
        );
        assert_eq!(config.window.decorations, Decorations::Full);
        assert_eq!(config.window.drag_modifiers, Modifiers(actions::ALT));
        assert!(
            config.window.rounded_corners,
            "esquinas como las del sistema"
        );
        assert_eq!(
            (
                config.colors.selection_foreground,
                config.colors.selection_background
            ),
            (None, None)
        );
        assert!(!config.disable_default_keys);
    }

    #[test]
    fn mouse_and_window_options_are_read() {
        let config = parse_str(
            r##"
            return {
              window = { decorations = "none", drag_modifiers = "SUPER", rounded_corners = false },
              mouse = { copy_on_select = false, right_click = "none", scroll_lines = 5,
                        rectangle_modifiers = "CTRL|SHIFT" },
              colors = { selection_foreground = "#000000", selection_background = "#ffd866" },
              disable_default_keys = true,
            }
            "##,
        )
        .unwrap();
        assert_eq!(config.window.decorations, Decorations::None);
        assert_eq!(config.window.drag_modifiers, Modifiers(actions::SUPER));
        assert!(!config.window.rounded_corners);
        assert!(!config.mouse.copy_on_select);
        assert_eq!(config.mouse.right_click, RightClick::None);
        assert_eq!(config.mouse.scroll_lines, 5);
        assert_eq!(
            config.mouse.rectangle_modifiers,
            Modifiers(actions::CTRL | actions::SHIFT)
        );
        assert_eq!(config.colors.selection_background, rgb(0xff, 0xd8, 0x66));
        assert!(config.disable_default_keys);
    }

    #[test]
    fn bad_mouse_and_window_options_explain_themselves() {
        let error = parse_str(r#"return { window = { decorations = "borde" } }"#).unwrap_err();
        assert!(
            error.contains("window.decorations") && error.contains("full"),
            "{error}"
        );

        let error =
            parse_str(r#"return { window = { drag_modifiers = "ALT|HYPER" } }"#).unwrap_err();
        assert!(
            error.contains("window.drag_modifiers") && error.contains("HYPER"),
            "{error}"
        );

        let error = parse_str(r#"return { mouse = { right_click = "menu" } }"#).unwrap_err();
        assert!(
            error.contains("mouse.right_click") && error.contains("paste"),
            "{error}"
        );

        for bad in ["0", "101"] {
            let error =
                parse_str(&format!("return {{ mouse = {{ scroll_lines = {bad} }} }}")).unwrap_err();
            assert!(error.contains("mouse.scroll_lines"), "{error}");
        }
    }

    #[test]
    fn the_two_drag_gestures_cannot_be_empty_or_collide() {
        // Arrastrar sin modificadores ya es seleccionar
        let error = parse_str(r#"return { mouse = { rectangle_modifiers = "" } }"#).unwrap_err();
        assert!(error.contains("mouse.rectangle_modifiers"), "{error}");
        let error =
            parse_str(r#"return { window = { decorations = "none", drag_modifiers = "" } }"#)
                .unwrap_err();
        assert!(error.contains("window.drag_modifiers"), "{error}");
        // Si son iguales no habría forma de saber cuál se pide
        let error = parse_str(
            r#"return { window = { decorations = "none", drag_modifiers = "CTRL|ALT" } }"#,
        )
        .unwrap_err();
        assert!(error.contains("no pueden ser iguales"), "{error}");
        // Con barra de título, el gesto de mover no se usa: no choca con nada
        assert!(parse_str(r#"return { window = { drag_modifiers = "CTRL|ALT" } }"#).is_ok());
    }

    #[test]
    fn default_keys_copy_paste_and_scroll() {
        use winit::keyboard::{Key, NamedKey};
        let script = script_from("return {}").unwrap();
        let named = |key: NamedKey| Key::Named(key);
        assert!(matches!(
            script.binding(&char_key("C"), actions::CTRL | actions::SHIFT),
            Some(actions::Action::Copy)
        ));
        assert!(matches!(
            script.binding(&named(NamedKey::Insert), actions::CTRL),
            Some(actions::Action::Copy)
        ));
        assert!(matches!(
            script.binding(&char_key("v"), actions::CTRL | actions::SHIFT),
            Some(actions::Action::Paste)
        ));
        assert!(matches!(
            script.binding(&named(NamedKey::Insert), actions::SHIFT),
            Some(actions::Action::Paste)
        ));
        assert!(matches!(
            script.binding(&named(NamedKey::PageUp), actions::SHIFT),
            Some(actions::Action::ScrollPages { amount: -1 })
        ));
        assert!(matches!(
            script.binding(&named(NamedKey::PageDown), actions::SHIFT),
            Some(actions::Action::ScrollPages { amount: 1 })
        ));
        // Ctrl+C sigue siendo para el programa
        assert!(script.binding(&char_key("c"), actions::CTRL).is_none());
    }

    #[test]
    fn default_keys_can_be_disabled_or_overridden() {
        let disabled = script_from("return { disable_default_keys = true }").unwrap();
        assert!(
            disabled
                .binding(&char_key("v"), actions::CTRL | actions::SHIFT)
                .is_none()
        );

        // Con las de por defecto desactivadas, las del usuario siguen funcionando
        let only_mine = script_from(
            r#"return { disable_default_keys = true,
                        keys = { { key = "v", mods = "CTRL", action = "Paste" } } }"#,
        )
        .unwrap();
        assert!(matches!(
            only_mine.binding(&char_key("v"), actions::CTRL),
            Some(actions::Action::Paste)
        ));
        assert!(
            only_mine
                .binding(&char_key("v"), actions::CTRL | actions::SHIFT)
                .is_none()
        );

        // Una tecla del usuario sustituye a la de por defecto con la misma combinación
        let overridden = script_from(
            r#"return { keys = { { key = "c", mods = "CTRL|SHIFT", action = terminal.action.SendString("copiar\r") } } }"#,
        )
        .unwrap();
        assert!(matches!(
            overridden.binding(&char_key("c"), actions::CTRL | actions::SHIFT),
            Some(actions::Action::SendString { text }) if text == "copiar\r"
        ));
        // ...y el resto de las de por defecto se conservan
        assert!(matches!(
            overridden.binding(&char_key("v"), actions::CTRL | actions::SHIFT),
            Some(actions::Action::Paste)
        ));

        // Dos teclas del usuario con la misma combinación siguen siendo un error
        let error = script_from(
            r#"return { keys = { { key = "c", mods = "CTRL|SHIFT", action = "Copy" },
                                 { key = "C", mods = "CTRL|SHIFT", action = "Quit" } } }"#,
        )
        .err()
        .unwrap();
        assert!(error.contains("ya estaba enlazada"), "{error}");
    }

    #[test]
    fn the_shell_integration_is_on_by_default_and_can_be_turned_off() {
        assert!(Config::default().shell.integration);
        assert!(parse_str("return {}").unwrap().shell.integration);
        let config =
            parse_str(r#"return { shell = { program = "pwsh", integration = false } }"#).unwrap();
        assert!(!config.shell.integration);
        let error = parse_str(r#"return { shell = { integracion = true } }"#).unwrap_err();
        assert!(error.contains("integracion"), "{error}");
    }

    #[test]
    fn pane_options_have_defaults_and_are_read() {
        let defaults = Config::default();
        assert_eq!(
            (defaults.panes.border_color, defaults.panes.focus_color),
            (None, None)
        );
        assert!((defaults.panes.inactive_dim - 0.35).abs() < 1e-6);

        let config = parse_str(
            r##"return { panes = { border_color = "#444444", focus_color = "#ff8800", inactive_dim = 0.6 } }"##,
        )
        .unwrap();
        assert_eq!(config.panes.border_color, rgb(0x44, 0x44, 0x44));
        assert_eq!(config.panes.focus_color, rgb(0xff, 0x88, 0x00));
        assert!((config.panes.inactive_dim - 0.6).abs() < 1e-6);

        assert!(parse_str("return { panes = { inactive_dim = 0 } }").is_ok());
        assert!(parse_str("return { panes = { inactive_dim = 1 } }").is_ok());
        for bad in ["-0.1", "1.5"] {
            let error =
                parse_str(&format!("return {{ panes = {{ inactive_dim = {bad} }} }}")).unwrap_err();
            assert!(error.contains("panes.inactive_dim"), "{error}");
        }
        let error = parse_str("return { panes = { borde = 1 } }").unwrap_err();
        assert!(error.contains("borde"), "{error}");
    }

    #[test]
    fn pane_actions_exist_in_lua() {
        use actions::{Action, Direction};
        let script = script_from(
            r#"return { keys = {
              { key = "d", mods = "CTRL|ALT", action = terminal.action.SplitRight },
              { key = "e", mods = "CTRL|ALT", action = "SplitDown" },
              { key = "h", mods = "CTRL|ALT", action = terminal.action.FocusPane("left") },
              { key = "l", mods = "CTRL|ALT", action = terminal.action.ResizePane("right", 5) },
              { key = "k", mods = "CTRL|ALT", action = terminal.action.ResizePane("up") },
              { key = "z", mods = "CTRL|ALT", action = terminal.action.ToggleZoom },
              { key = "x", mods = "CTRL|ALT", action = "ClosePane" },
              { key = "n", mods = "CTRL|ALT", action = terminal.action.FocusNext },
              { key = "p", mods = "CTRL|ALT", action = terminal.action.FocusPrevious },
            } }"#,
        )
        .unwrap();
        let at = |key: &str| {
            script
                .binding(&char_key(key), actions::CTRL | actions::ALT)
                .cloned()
        };
        assert!(matches!(at("d"), Some(Action::SplitRight)));
        assert!(matches!(at("e"), Some(Action::SplitDown)));
        assert!(matches!(
            at("h"),
            Some(Action::FocusPane {
                dir: Direction::Left
            })
        ));
        assert!(matches!(
            at("l"),
            Some(Action::ResizePane {
                dir: Direction::Right,
                amount: 5
            })
        ));
        assert!(matches!(
            at("k"),
            Some(Action::ResizePane {
                dir: Direction::Up,
                amount: 1
            })
        ));
        assert!(matches!(at("z"), Some(Action::ToggleZoom)));
        assert!(matches!(at("x"), Some(Action::ClosePane)));
        assert!(matches!(at("n"), Some(Action::FocusNext)));
        assert!(matches!(at("p"), Some(Action::FocusPrevious)));
    }

    #[test]
    fn a_bad_pane_direction_names_the_wrong_value() {
        let error = script_from(
            r#"return { keys = { { key = "h", mods = "CTRL", action = terminal.action.FocusPane("diagonal") } } }"#,
        )
        .err()
        .unwrap();
        assert!(error.contains("diagonal"), "{error}");
    }

    #[test]
    fn selection_actions_exist_in_lua() {
        let script = script_from(
            r#"return { keys = {
              { key = "a", mods = "CTRL|ALT", action = terminal.action.SelectAll },
              { key = "x", mods = "CTRL|ALT", action = terminal.action.ClearSelection },
              { key = "y", mods = "CTRL|ALT", action = "Copy" },
            } }"#,
        )
        .unwrap();
        assert!(matches!(
            script.binding(&char_key("a"), actions::CTRL | actions::ALT),
            Some(actions::Action::SelectAll)
        ));
        assert!(matches!(
            script.binding(&char_key("x"), actions::CTRL | actions::ALT),
            Some(actions::Action::ClearSelection)
        ));
        assert!(matches!(
            script.binding(&char_key("y"), actions::CTRL | actions::ALT),
            Some(actions::Action::Copy)
        ));
    }

    #[test]
    fn keys_and_events_are_optional_and_do_not_trip_unknown_field_check() {
        assert!(script_from("return {}").is_ok());
        assert!(script_from("return { keys = {}, events = {} }").is_ok());
    }

    #[test]
    fn bad_keys_explain_where_and_why() {
        let error = script_from(r#"return { keys = { { key = "v", action = "Paste" }, { key = "PagUp", action = "Paste" } } }"#)
            .err()
            .unwrap();
        assert!(
            error.contains("keys[2].key") && error.contains("PagUp"),
            "{error}"
        );

        let error = script_from(
            r#"return { keys = { { key = "v", mods = "CTRL|HYPER", action = "Paste" } } }"#,
        )
        .err()
        .unwrap();
        assert!(
            error.contains("keys[1].mods") && error.contains("HYPER"),
            "{error}"
        );

        let error = script_from(r#"return { keys = { { key = "v", action = "Pegar" } } }"#)
            .err()
            .unwrap();
        assert!(
            error.contains("keys[1].action") && error.contains("Pegar"),
            "{error}"
        );

        let error = script_from(r#"return { keys = { { key = "v" } } }"#)
            .err()
            .unwrap();
        assert!(error.contains("keys[1].action"), "{error}");

        let error = script_from(r#"return { keys = { { key = "v", action = "Paste" }, { key = "V", action = "Quit" } } }"#)
            .err()
            .unwrap();
        assert!(
            error.contains("keys[2]") && error.contains("ya estaba enlazada"),
            "{error}"
        );

        let error = script_from(r#"return { keys = "nada" }"#).err().unwrap();
        assert!(error.contains("keys debe ser"), "{error}");
    }

    #[test]
    fn events_accept_known_names_with_functions_only() {
        let script = script_from(
            r#"return { events = { ["bell"] = function(ctx) end, ["title-changed"] = function(ctx, title) end } }"#,
        )
        .unwrap();
        assert!(script.has_handler(actions::EventKind::Bell));
        assert!(script.has_handler(actions::EventKind::TitleChanged));
        assert!(!script.has_handler(actions::EventKind::ShellExit));

        let error = script_from(r#"return { events = { ["campana"] = function() end } }"#)
            .err()
            .unwrap();
        assert!(
            error.contains("campana") && error.contains("bell"),
            "{error}"
        );
        let error = script_from(r#"return { events = { ["bell"] = "no soy una función" } }"#)
            .err()
            .unwrap();
        assert!(
            error.contains("bell") && error.contains("función"),
            "{error}"
        );
    }

    #[test]
    fn a_hook_can_queue_text_logs_and_native_actions() {
        let script = script_from(
            r#"
            return { events = { ["title-changed"] = function(ctx, title)
              ctx:log("título: " .. title)
              ctx:send_text("echo " .. title .. "\r")
              ctx:perform(terminal.action.ScrollToTop)
            end } }
            "#,
        )
        .unwrap();
        let commands = script.fire(actions::EventKind::TitleChanged, vec![script.text("hola")]);
        assert!(matches!(
            commands.as_slice(),
            [
                actions::Command::Log(log),
                actions::Command::SendText(text),
                actions::Command::Perform(actions::Action::ScrollToTop),
            ] if log == "título: hola" && text == "echo hola\r"
        ));
    }

    #[test]
    fn perform_rejects_lua_functions_and_bad_actions() {
        let script = script_from(
            r#"return { events = { ["bell"] = function(ctx) ctx:perform(function() end) end,
                                  ["shell-exit"] = function(ctx) ctx:perform("Nope") end } }"#,
        )
        .unwrap();
        // El error del hook se registra y no sale: lo que pidió antes de fallar es lo único que queda
        assert!(script.fire(actions::EventKind::Bell, vec![]).is_empty());
        assert!(
            script
                .fire(actions::EventKind::ShellExit, vec![])
                .is_empty()
        );
    }

    #[test]
    fn an_infinite_loop_in_the_config_is_aborted_instead_of_freezing_startup() {
        let started = std::time::Instant::now();
        let error = script_from("while true do end").err().unwrap();
        assert!(error.contains("demasiado"), "{error}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn the_example_config_is_valid() {
        // El ejemplo documentado debe seguir siendo válido a medida que cambia el esquema
        let (config, script) =
            parse_script(include_str!("../examples/config.lua"), "config.lua", None).unwrap();
        assert_eq!(config.colors.ansi.as_ref().map(Vec::len), Some(16));
        // The example only picks PowerShell on Windows; elsewhere it falls back to $SHELL
        let expected_shell = if cfg!(windows) { Some("pwsh") } else { None };
        assert_eq!(config.shell.program.as_deref(), expected_shell);
        // Las teclas y los hooks del ejemplo también se interpretan sin errores
        assert!(matches!(
            script.binding(&char_key("v"), actions::CTRL | actions::SHIFT),
            Some(actions::Action::Paste)
        ));
        assert!(script.has_handler(actions::EventKind::ConfigReloaded));
        // Lo documentado de ratón, ventana y cursor coincide con los valores por defecto, y las
        // teclas de por defecto siguen ahí porque el ejemplo no las desactiva
        let defaults = Config::default();
        assert_eq!(config.mouse, defaults.mouse);
        assert_eq!(config.window.decorations, defaults.window.decorations);
        assert_eq!(config.window.drag_modifiers, defaults.window.drag_modifiers);
        assert_eq!(config.cursor.blink, defaults.cursor.blink);
        assert_eq!(config.cursor.blink_ms, defaults.cursor.blink_ms);
        assert!(!config.disable_default_keys);
        assert!(matches!(
            script.binding(&char_key("C"), actions::CTRL | actions::SHIFT),
            Some(actions::Action::Copy)
        ));
        // Lo de paneles también: los valores documentados son los de por defecto, el prefijo
        // arma con Ctrl+B y las teclas de paneles del ejemplo se interpretan
        assert_eq!(config.panes, defaults.panes);
        let mut state = actions::LeaderState::default();
        assert!(matches!(
            step(&script, &mut state, &char_key("b"), actions::CTRL),
            actions::LeaderOutcome::Arm
        ));
        assert!(matches!(
            step(&script, &mut state, &char_key("v"), 0),
            actions::LeaderOutcome::Run(actions::Action::SplitRight)
        ));
        assert!(matches!(
            script.binding(&named(winit::keyboard::NamedKey::Enter), actions::CTRL),
            Some(actions::Action::SplitDown)
        ));
        step(&script, &mut state, &char_key("b"), actions::CTRL);
        assert!(matches!(
            step(&script, &mut state, &char_key("g"), 0),
            actions::LeaderOutcome::Run(actions::Action::NewFloat {
                program: Some(_),
                ..
            })
        ));
    }

    #[test]
    fn missing_file_gives_defaults() {
        let path = std::env::temp_dir()
            .join("terminal-no-existe")
            .join("config.lua");
        let (config, _script) = load_script_path(&path).unwrap();
        assert_eq!(config, Config::default());
    }
}
