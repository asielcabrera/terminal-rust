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
#[cfg(windows)]
const DEFAULT_FALLBACK: &[&str] = &["Segoe UI Symbol", "Microsoft YaHei", "Malgun Gothic", "Segoe UI Emoji"];
#[cfg(not(windows))]
const DEFAULT_FAMILY: &str = "DejaVu Sans Mono";
#[cfg(not(windows))]
const DEFAULT_FALLBACK: &[&str] = &["DejaVu Sans"];

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
    /// Líneas de historial (solo al arrancar)
    pub scrollback: usize,
    pub on_shell_exit: ShellExit,
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
            scrollback: 10_000,
            on_shell_exit: ShellExit::Close,
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
            fallback: DEFAULT_FALLBACK.iter().map(|name| (*name).to_string()).collect(),
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
}

impl Default for CursorConfig {
    fn default() -> Self {
        Self {
            style: CursorStyle::Block,
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
    /// Los primeros colores de la paleta (0–15); el resto de la paleta queda por defecto
    pub ansi: Option<Vec<Color>>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShellConfig {
    /// Si falta, se usa el shell por defecto del sistema (solo al arrancar)
    pub program: Option<String>,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WindowConfig {
    /// Título fijo de la ventana. Si falta, la ventana muestra el título que pone el programa
    /// (`DEFAULT_TITLE` hasta que ponga uno).
    pub title: Option<String>,
    /// Tamaño inicial de la cuadrícula (solo al arrancar). Si faltan, la ventana tiene el tamaño
    /// que le dé el sistema; si solo hay uno, el otro es 80 columnas o 24 filas. Es una petición:
    /// un gestor de ventanas en mosaico puede ignorarla.
    pub columns: Option<u16>,
    pub rows: Option<u16>,
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
            return Err(format!("font.size debe estar entre 4 y 200 (recibió {})", self.font.size));
        }
        if self.font.family.trim().is_empty() {
            return Err("font.family no puede estar vacío".to_string());
        }
        if !(self.padding.is_finite() && (0.0..=200.0).contains(&self.padding)) {
            return Err(format!("padding debe estar entre 0 y 200 (recibió {})", self.padding));
        }
        if self.window.columns == Some(0) || self.window.rows == Some(0) {
            return Err("window.columns y window.rows deben ser mayores que 0".to_string());
        }
        if self.shell.program.is_none() && !self.shell.args.is_empty() {
            return Err("shell.args necesita shell.program (con el shell por defecto no se pueden pasar argumentos)".to_string());
        }
        if let Some(ansi) = &self.colors.ansi
            && ansi.len() > 16
        {
            return Err(format!(
                "colors.ansi admite como máximo 16 colores (recibió {})",
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
        Err(error) => Loaded {
            config: Config::default(),
            script: Script::new().expect("no se pudo crear el intérprete de Lua"),
            error: Some(error),
            path,
        },
    }
}

fn default_script() -> Result<(Config, Script), String> {
    let script = Script::new().map_err(|e| lua_message(&e))?;
    Ok((Config::default(), script))
}

/// Lee y evalúa un archivo de configuración. Si no existe, devuelve los valores por defecto.
pub fn load_script_path(path: &Path) -> Result<(Config, Script), String> {
    match std::fs::read_to_string(path) {
        Ok(source) => {
            let name = path
                .file_name()
                .map_or_else(|| "config.lua".to_string(), |name| name.to_string_lossy().into_owned());
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
    table.set("keys", Value::Nil).map_err(|e| lua_message(&e))?;
    table.set("events", Value::Nil).map_err(|e| lua_message(&e))?;

    let deserializer = mlua::serde::Deserializer::new(Value::Table(table));
    let config: Config = serde_path_to_error::deserialize(deserializer)
        .map_err(|e| format!("{chunk_name}: {e}"))?;
    config.validate().map_err(|e| format!("{chunk_name}: {e}"))?;

    let bindings = actions::parse_keys(script.lua(), keys)
        .map_err(|e| format!("{chunk_name}: {e}"))?;
    let events = actions::parse_events(events).map_err(|e| format!("{chunk_name}: {e}"))?;
    script.set_bindings(bindings);
    script.set_events(events);
    Ok((config, script))
}

/// Acciones nativas que se enlazan a una tecla desde config.lua (`terminal.action.Paste`...). Son
/// tablas con la clave `action`, que `actions::parse_action` convierte en [`actions::Action`].
const ACTION_PRELUDE: &str = r#"
local action = {}
action.SendString = function(text) return { action = "SendString", text = text } end
action.ScrollLines = function(amount) return { action = "ScrollLines", amount = amount } end
action.ScrollPages = function(amount) return { action = "ScrollPages", amount = amount } end
for _, name in ipairs({
  "ScrollToTop", "ScrollToBottom", "IncreaseFontSize", "DecreaseFontSize", "ResetFontSize",
  "ToggleFullscreen", "ReloadConfig", "Paste", "Quit",
}) do
  action[name] = { action = name }
end
return action
"#;

/// Lo que `config.lua` tiene a mano además de la biblioteca estándar de Lua
fn prepare(lua: &Lua, dir: Option<&Path>) -> mlua::Result<()> {
    let terminal = lua.create_table()?;
    terminal.set("os", std::env::consts::OS)?;
    terminal.set("action", lua.load(ACTION_PRELUDE).set_name("=prelude").eval::<Table>()?)?;
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
        assert_eq!((config.window.columns, config.window.rows), (Some(100), Some(30)));
        assert_eq!(config.window.title.as_deref(), Some("mi terminal"));
        assert_eq!(config.scrollback, 5000);
        assert_eq!(config.on_shell_exit, ShellExit::Hold);
    }

    #[test]
    fn empty_lua_tables_are_accepted_for_lists_and_maps() {
        let config = parse_str("return { font = { fallback = {} }, shell = { args = {}, env = {} } }").unwrap();
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
        let error = parse_str(r#"return { colors = { foreground = "no-es-un-color" } }"#).unwrap_err();
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
        assert!(parse_str("return { font = { size = 0 } }").unwrap_err().contains("font.size"));
        assert!(parse_str("return { padding = -1 }").unwrap_err().contains("padding"));
        assert!(parse_str("return { window = { columns = 0 } }").unwrap_err().contains("window"));
        assert!(parse_str("return { window = { rows = 0 } }").unwrap_err().contains("window"));
        let too_many = "return { colors = { ansi = { '#000','#000','#000','#000','#000','#000','#000','#000','#000','#000','#000','#000','#000','#000','#000','#000','#000' } } }";
        assert!(parse_str(too_many).unwrap_err().contains("colors.ansi"));
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
        assert!(parse_str(r#"return { shell = { program = "pwsh", args = { "-NoLogo" } } }"#).is_ok());
        // cwd y env sí valen con el shell por defecto
        assert!(parse_str(r#"return { shell = { cwd = "C:/", env = { A = "1" } } }"#).is_ok());
    }

    #[test]
    fn cursor_blink_is_not_an_option_yet() {
        let error = parse_str("return { cursor = { blink = true } }").unwrap_err();
        assert!(error.contains("blink"), "{error}");
    }

    fn script_from(source: &str) -> Result<Script, String> {
        parse_script(source, "config.lua", None).map(|(_config, script)| script)
    }

    fn char_key(text: &str) -> winit::keyboard::Key {
        winit::keyboard::Key::Character(text.into())
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
        assert!(matches!(script.binding(&char_key("V"), actions::CTRL | actions::SHIFT), Some(actions::Action::Paste)));
        assert!(matches!(
            script.binding(&key(winit::keyboard::NamedKey::PageUp), actions::SHIFT),
            Some(actions::Action::ScrollPages { amount: -1 })
        ));
        assert!(matches!(
            script.binding(&char_key("l"), actions::ALT),
            Some(actions::Action::SendString { text }) if text == "clear\r"
        ));
        // La forma corta con el nombre de la acción
        assert!(matches!(script.binding(&char_key("q"), actions::CTRL), Some(actions::Action::Quit)));
        // Los modificadores tienen que coincidir exactamente
        assert!(script.binding(&char_key("v"), actions::CTRL).is_none());
        assert!(script.binding(&char_key("v"), actions::CTRL | actions::SHIFT | actions::ALT).is_none());
        assert!(script.binding(&char_key("x"), 0).is_none());
    }

    #[test]
    fn a_lua_function_can_be_bound_to_a_key() {
        let script = script_from(
            r#"return { keys = { { key = "k", mods = "CTRL|ALT", action = function(ctx) ctx:send_text("ls\r") end } } }"#,
        )
        .unwrap();
        let Some(actions::Action::Lua(function)) = script.binding(&char_key("k"), actions::CTRL | actions::ALT) else {
            panic!("debería ser una función de Lua");
        };
        let commands = script.call(&function.clone(), vec![]);
        assert!(matches!(commands.as_slice(), [actions::Command::SendText(text)] if text == "ls\r"));
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
        assert!(error.contains("keys[2].key") && error.contains("PagUp"), "{error}");

        let error = script_from(r#"return { keys = { { key = "v", mods = "CTRL|HYPER", action = "Paste" } } }"#).err().unwrap();
        assert!(error.contains("keys[1].mods") && error.contains("HYPER"), "{error}");

        let error = script_from(r#"return { keys = { { key = "v", action = "Pegar" } } }"#).err().unwrap();
        assert!(error.contains("keys[1].action") && error.contains("Pegar"), "{error}");

        let error = script_from(r#"return { keys = { { key = "v" } } }"#).err().unwrap();
        assert!(error.contains("keys[1].action"), "{error}");

        let error = script_from(r#"return { keys = { { key = "v", action = "Paste" }, { key = "V", action = "Quit" } } }"#)
            .err()
            .unwrap();
        assert!(error.contains("keys[2]") && error.contains("ya estaba enlazada"), "{error}");

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

        let error = script_from(r#"return { events = { ["campana"] = function() end } }"#).err().unwrap();
        assert!(error.contains("campana") && error.contains("bell"), "{error}");
        let error = script_from(r#"return { events = { ["bell"] = "no soy una función" } }"#).err().unwrap();
        assert!(error.contains("bell") && error.contains("función"), "{error}");
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
        assert!(script.fire(actions::EventKind::ShellExit, vec![]).is_empty());
    }

    #[test]
    fn an_infinite_loop_in_the_config_is_aborted_instead_of_freezing_startup() {
        let started = std::time::Instant::now();
        let error = script_from("while true do end").err().unwrap();
        assert!(error.contains("demasiado"), "{error}");
        assert!(started.elapsed() < std::time::Duration::from_secs(10), "{:?}", started.elapsed());
    }

    #[test]
    fn the_example_config_is_valid() {
        // El ejemplo documentado debe seguir siendo válido a medida que cambia el esquema
        let (config, script) =
            parse_script(include_str!("../examples/config.lua"), "config.lua", None).unwrap();
        assert_eq!(config.colors.ansi.as_ref().map(Vec::len), Some(16));
        assert_eq!(config.shell.program.as_deref(), Some("pwsh"));
        // Las teclas y los hooks del ejemplo también se interpretan sin errores
        assert!(matches!(
            script.binding(&char_key("v"), actions::CTRL | actions::SHIFT),
            Some(actions::Action::Paste)
        ));
        assert!(script.has_handler(actions::EventKind::ConfigReloaded));
    }

    #[test]
    fn missing_file_gives_defaults() {
        let path = std::env::temp_dir().join("terminal-no-existe").join("config.lua");
        let (config, _script) = load_script_path(&path).unwrap();
        assert_eq!(config, Config::default());
    }
}
