//! Lua configuration loading: `config.lua` to [`Config`], plus the [`Script`] (keys, hooks,
//! and the Lua interpreter) that travels with it.
//!
//! The configuration *structures* live in `terminal-protocol` (both the window and the server
//! process read them); this module re-exports them so callers can keep saying `config::`.

pub use terminal_protocol::config::*;

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::actions::{self, Script, lua_message};
use mlua::{Lua, Table, Value};

/// Result of loading the configuration at startup: it never fails; on error the defaults are
/// used and the reason travels in `error` so it can be shown.
pub struct Loaded {
    pub config: Config,
    /// Keys, hooks and the Lua interpreter that runs them
    pub script: Script,
    pub error: Option<String>,
    pub path: Option<PathBuf>,
}

/// `TERMINAL_CONFIG`, or `%APPDATA%\terminal\config.lua` on Windows, or
/// `$XDG_CONFIG_HOME`/`~/.config` + `terminal/config.lua` elsewhere.
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
            let (config, script) = default_script().expect("failed to create the Lua interpreter");
            Loaded {
                config,
                script,
                error: Some(error),
                path,
            }
        }
    }
}

/// The factory configuration (that of a file returning an empty table): including its
/// default keys and its leader
fn default_script() -> Result<(Config, Script), String> {
    parse_script("return {}", "config.lua", None)
}

/// Reads and evaluates a configuration file. If it does not exist, returns the defaults.
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
        Err(e) => Err(format!("could not read {}: {e}", path.display())),
    }
}

/// Only the declarative part (no keys or hooks); used by tests
#[cfg(test)]
pub fn parse(source: &str, chunk_name: &str, dir: Option<&Path>) -> Result<Config, String> {
    parse_script(source, chunk_name, dir).map(|(config, _script)| config)
}

/// Evaluates `source` (the contents of config.lua) and turns it into [`Config`] plus the [`Script`]
/// with its keys and hooks. `dir` is the folder `require` searches for modules.
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
            "{chunk_name} must end with `return {{ ... }}` (returned {})",
            value.type_name()
        ));
    };

    // `keys` and `events` hold functions, which serde cannot deserialize: they are taken out of the
    // table first and interpreted separately
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

/// Native actions bound to a key from config.lua (`terminal.action.Paste`...). They are
/// tables with an `action` key, which `actions::parse_action` turns into [`actions::Action`].
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

/// What `config.lua` has at hand besides the Lua standard library
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
        // To split the configuration into modules with `require`
        let package: Table = lua.globals().get("package")?;
        let current: String = package.get("path")?;
        package.set("path", format!("{dir}/?.lua;{dir}/?/init.lua;{current}"))?;
    }

    lua.globals().set("terminal", terminal)?;
    Ok(())
}

#[cfg(test)]
mod tests;
