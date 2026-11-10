//! Teclas, acciones y eventos que se definen desde config.lua, y el estado de Lua que las ejecuta.
//!
//! Lua no es `Send` y vive en el hilo de la UI. Una función de Lua no puede llamar de vuelta a
//! `App` mientras `App` está prestada, así que lo que quiere hacer (enviar texto, ejecutar una
//! acción) se encola como [`Command`] y `App` lo ejecuta cuando la función termina.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use mlua::{Function, HookTriggers, Lua, LuaSerdeExt, Table, Value, VmState};
use serde::Deserialize;
use winit::keyboard::{Key, NamedKey};

/// Bits de modificadores (independientes de winit y de libghostty)
pub const CTRL: u8 = 1;
pub const SHIFT: u8 = 2;
pub const ALT: u8 = 4;
pub const SUPER: u8 = 8;

/// Tiempo máximo de un script: la configuración al cargar, o cada llamada a una función. Sin esto
/// un `while true do end` congelaría la terminal entera, porque Lua corre en el hilo de la UI.
pub const CONFIG_TIME_LIMIT: Duration = Duration::from_secs(2);
const HOOK_TIME_LIMIT: Duration = Duration::from_millis(250);
/// Cada cuántas instrucciones de Lua se mira el reloj
const TIME_CHECK_EVERY: u32 = 10_000;

/// Mensaje de un error de Lua sin la traza de pila, que solo añade ruido
pub fn lua_message(error: &mlua::Error) -> String {
    let text = error.to_string();
    text.split("\nstack traceback:")
        .next()
        .unwrap_or(&text)
        .trim()
        .to_string()
}

/// Lo que una tecla o un hook pueden hacer. Las nativas se construyen desde Lua con
/// `terminal.action.X` (tablas con la clave `action`); `Lua` es una función propia.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "action")]
pub enum Action {
    /// Escribe el texto en el shell, tal cual
    SendString { text: String },
    /// Desplaza el historial: negativo sube, positivo baja
    ScrollLines { amount: isize },
    ScrollPages { amount: isize },
    ScrollToTop,
    ScrollToBottom,
    IncreaseFontSize,
    DecreaseFontSize,
    ResetFontSize,
    ToggleFullscreen,
    ReloadConfig,
    /// Pega el portapapeles (con pegado entre corchetes si el programa lo pidió)
    Paste,
    Quit,
    #[serde(skip)]
    Lua(Function),
}

/// Qué quiere hacer una función de Lua; `App` lo ejecuta después de la llamada
pub enum Command {
    SendText(String),
    Log(String),
    Perform(Action),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum KeyId {
    Named(NamedKey),
    Char(char),
}

/// Tecla lógica de winit (la que tiene en cuenta la distribución del teclado) en nuestra forma
/// normalizada. Los caracteres van en minúscula: con Shift, `v` llega como `V`.
fn key_id(key: &Key) -> Option<KeyId> {
    match key {
        Key::Named(named) => Some(KeyId::Named(*named)),
        Key::Character(text) => {
            let mut chars = text.chars();
            let first = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            Some(KeyId::Char(first.to_lowercase().next().unwrap_or(first)))
        }
        _ => None,
    }
}

const FUNCTION_KEYS: [NamedKey; 24] = [
    NamedKey::F1,
    NamedKey::F2,
    NamedKey::F3,
    NamedKey::F4,
    NamedKey::F5,
    NamedKey::F6,
    NamedKey::F7,
    NamedKey::F8,
    NamedKey::F9,
    NamedKey::F10,
    NamedKey::F11,
    NamedKey::F12,
    NamedKey::F13,
    NamedKey::F14,
    NamedKey::F15,
    NamedKey::F16,
    NamedKey::F17,
    NamedKey::F18,
    NamedKey::F19,
    NamedKey::F20,
    NamedKey::F21,
    NamedKey::F22,
    NamedKey::F23,
    NamedKey::F24,
];

fn parse_key(name: &str) -> Result<KeyId, String> {
    let lower = name.to_lowercase();
    let named = match lower.as_str() {
        "enter" | "return" => Some(NamedKey::Enter),
        "tab" => Some(NamedKey::Tab),
        "space" | " " => Some(NamedKey::Space),
        "escape" | "esc" => Some(NamedKey::Escape),
        "backspace" => Some(NamedKey::Backspace),
        "delete" | "del" => Some(NamedKey::Delete),
        "insert" => Some(NamedKey::Insert),
        "home" => Some(NamedKey::Home),
        "end" => Some(NamedKey::End),
        "pageup" => Some(NamedKey::PageUp),
        "pagedown" => Some(NamedKey::PageDown),
        "arrowup" | "up" => Some(NamedKey::ArrowUp),
        "arrowdown" | "down" => Some(NamedKey::ArrowDown),
        "arrowleft" | "left" => Some(NamedKey::ArrowLeft),
        "arrowright" | "right" => Some(NamedKey::ArrowRight),
        other => other
            .strip_prefix('f')
            .and_then(|number| number.parse::<usize>().ok())
            .filter(|number| (1..=FUNCTION_KEYS.len()).contains(number))
            .map(|number| FUNCTION_KEYS[number - 1]),
    };
    if let Some(named) = named {
        return Ok(KeyId::Named(named));
    }
    let mut chars = name.chars();
    match (chars.next(), chars.next()) {
        (Some(only), None) => Ok(KeyId::Char(only.to_lowercase().next().unwrap_or(only))),
        _ => Err(format!(
            "tecla desconocida {name:?} (usa un carácter como \"v\" o un nombre como \"PageUp\", \"F5\", \"Enter\", \"Space\")"
        )),
    }
}

/// "CTRL|SHIFT" a bits. Admite CTRL, SHIFT, ALT y SUPER (también CMD y WIN).
pub fn parse_mods(text: &str) -> Result<u8, String> {
    let mut bits = 0;
    for part in text.split('|').map(str::trim).filter(|part| !part.is_empty()) {
        bits |= match part.to_uppercase().as_str() {
            "CTRL" | "CONTROL" => CTRL,
            "SHIFT" => SHIFT,
            "ALT" | "OPTION" => ALT,
            "SUPER" | "CMD" | "WIN" | "META" => SUPER,
            _ => {
                return Err(format!(
                    "modificador desconocido {part:?} (usa CTRL, SHIFT, ALT o SUPER, separados por |)"
                ));
            }
        };
    }
    Ok(bits)
}

#[derive(Default)]
pub struct Bindings {
    map: HashMap<(KeyId, u8), Action>,
}

impl Bindings {
    pub fn lookup(&self, key: &Key, mods: u8) -> Option<&Action> {
        self.map.get(&(key_id(key)?, mods))
    }
}

/// Convierte la tabla `keys` de config.lua. Los modificadores tienen que coincidir exactamente
/// (también SHIFT cuando el carácter lo necesita, como `+` en un teclado US).
pub fn parse_keys(lua: &Lua, value: Value) -> Result<Bindings, String> {
    let table = match value {
        Value::Nil => return Ok(Bindings::default()),
        Value::Table(table) => table,
        other => {
            return Err(format!(
                "keys debe ser una lista de tablas {{ key = ..., mods = ..., action = ... }} (recibió {})",
                other.type_name()
            ));
        }
    };

    let mut bindings = Bindings::default();
    for (index, entry) in table.sequence_values::<Value>().enumerate() {
        let at = format!("keys[{}]", index + 1);
        let entry = entry.map_err(|e| format!("{at}: {}", lua_message(&e)))?;
        let Value::Table(entry) = entry else {
            return Err(format!("{at} debe ser una tabla {{ key = ..., mods = ..., action = ... }}"));
        };

        let name: String = entry
            .get("key")
            .map_err(|_| format!("{at}.key debe ser un texto, como \"v\" o \"PageUp\""))?;
        let key = parse_key(&name).map_err(|e| format!("{at}.key: {e}"))?;
        let mods = match entry.get::<Value>("mods").map_err(|e| lua_message(&e))? {
            Value::Nil => 0,
            Value::String(text) => {
                parse_mods(&text.to_string_lossy()).map_err(|e| format!("{at}.mods: {e}"))?
            }
            other => {
                return Err(format!("{at}.mods debe ser un texto, como \"CTRL|SHIFT\" (recibió {})", other.type_name()));
            }
        };
        let action = parse_action(lua, entry.get("action").map_err(|e| lua_message(&e))?)
            .map_err(|e| format!("{at}.action: {e}"))?;

        if bindings.map.insert((key, mods), action).is_some() {
            return Err(format!("{at}: la combinación {name:?} + {mods:?} ya estaba enlazada"));
        }
    }
    Ok(bindings)
}

/// Una acción es una función propia, `terminal.action.X` o el nombre de una acción sin parámetros
pub fn parse_action(lua: &Lua, value: Value) -> Result<Action, String> {
    match value {
        Value::Function(function) => Ok(Action::Lua(function)),
        Value::String(name) => {
            let table = lua.create_table().map_err(|e| lua_message(&e))?;
            table.set("action", name).map_err(|e| lua_message(&e))?;
            lua.from_value(Value::Table(table)).map_err(|e| lua_message(&e))
        }
        Value::Table(_) => lua.from_value(value).map_err(|e| lua_message(&e)),
        other => Err(format!(
            "debe ser una función, terminal.action.X o el nombre de una acción (recibió {})",
            other.type_name()
        )),
    }
}

/// Cosas que le pasan al terminal y para las que se puede definir un hook en `events`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    /// El programa cambió el título (recibe el título)
    TitleChanged,
    /// Sonó la campana
    Bell,
    /// El shell terminó
    ShellExit,
    /// La configuración se recargó con éxito
    ConfigReloaded,
}

impl EventKind {
    const ALL: [(&'static str, EventKind); 4] = [
        ("title-changed", EventKind::TitleChanged),
        ("bell", EventKind::Bell),
        ("shell-exit", EventKind::ShellExit),
        ("config-reloaded", EventKind::ConfigReloaded),
    ];

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().find(|(known, _)| *known == name).map(|(_, kind)| *kind)
    }
}

#[derive(Default)]
pub struct Events {
    handlers: HashMap<EventKind, Function>,
}

/// Convierte la tabla `events` de config.lua
pub fn parse_events(value: Value) -> Result<Events, String> {
    let table = match value {
        Value::Nil => return Ok(Events::default()),
        Value::Table(table) => table,
        other => {
            return Err(format!(
                "events debe ser una tabla {{ [\"bell\"] = function(ctx) ... end }} (recibió {})",
                other.type_name()
            ));
        }
    };

    let mut events = Events::default();
    for pair in table.pairs::<String, Value>() {
        let (name, handler) = pair.map_err(|_| {
            "las claves de events deben ser nombres de evento (por ejemplo [\"bell\"])".to_string()
        })?;
        let Some(kind) = EventKind::from_name(&name) else {
            let valid: Vec<_> = EventKind::ALL.iter().map(|(name, _)| *name).collect();
            return Err(format!("evento desconocido {name:?} (válidos: {})", valid.join(", ")));
        };
        let Value::Function(function) = handler else {
            return Err(format!("events[{name:?}] debe ser una función (recibió {})", handler.type_name()));
        };
        events.handlers.insert(kind, function);
    }
    Ok(events)
}

/// El estado de Lua de una configuración: su intérprete, las teclas, los hooks y el `ctx` que
/// reciben las funciones.
pub struct Script {
    lua: Lua,
    bindings: Bindings,
    events: Events,
    ctx: Table,
    commands: Rc<RefCell<Vec<Command>>>,
    deadline: Rc<Cell<Instant>>,
}

impl Script {
    /// Crea el intérprete con el límite de tiempo ya activo (para la carga de la configuración)
    pub fn new() -> mlua::Result<Self> {
        let lua = Lua::new();
        let deadline = Rc::new(Cell::new(Instant::now() + CONFIG_TIME_LIMIT));
        lua.set_hook(
            HookTriggers {
                every_nth_instruction: Some(TIME_CHECK_EVERY),
                ..HookTriggers::default()
            },
            {
                let deadline = deadline.clone();
                move |_lua, _debug| {
                    if Instant::now() > deadline.get() {
                        Err(mlua::Error::runtime(
                            "el script tardó demasiado y se abortó (¿un bucle infinito?)",
                        ))
                    } else {
                        Ok(VmState::Continue)
                    }
                }
            },
        )?;

        let commands = Rc::new(RefCell::new(Vec::new()));
        let ctx = make_ctx(&lua, &commands)?;
        Ok(Self {
            lua,
            bindings: Bindings::default(),
            events: Events::default(),
            ctx,
            commands,
            deadline,
        })
    }

    pub fn lua(&self) -> &Lua {
        &self.lua
    }

    pub fn set_bindings(&mut self, bindings: Bindings) {
        self.bindings = bindings;
    }

    pub fn set_events(&mut self, events: Events) {
        self.events = events;
    }

    pub fn binding(&self, key: &Key, mods: u8) -> Option<&Action> {
        self.bindings.lookup(key, mods)
    }

    pub fn has_handler(&self, kind: EventKind) -> bool {
        self.events.handlers.contains_key(&kind)
    }

    /// Un texto como valor de Lua, para pasarlo como argumento a un hook
    pub fn text(&self, text: &str) -> Value {
        self.lua
            .create_string(text)
            .map_or(Value::Nil, Value::String)
    }

    /// Llama al hook de un evento (si lo hay) y devuelve lo que pidió hacer
    pub fn fire(&self, kind: EventKind, args: Vec<Value>) -> Vec<Command> {
        match self.events.handlers.get(&kind) {
            Some(handler) => self.call(handler, args),
            None => Vec::new(),
        }
    }

    /// Llama a una función de Lua con `ctx` como primer argumento. Un error en la función se
    /// registra y se descarta: nunca tumba la terminal. Devuelve lo que la función pidió hacer.
    pub fn call(&self, function: &Function, args: Vec<Value>) -> Vec<Command> {
        self.deadline.set(Instant::now() + HOOK_TIME_LIMIT);
        let arguments: mlua::MultiValue = std::iter::once(Value::Table(self.ctx.clone()))
            .chain(args)
            .collect();
        if let Err(error) = function.call::<()>(arguments) {
            eprintln!("[lua] {}", lua_message(&error));
        }
        self.commands.borrow_mut().drain(..).collect()
    }
}

/// El objeto `ctx` de las funciones: sus métodos solo encolan órdenes (ver el comentario del módulo)
fn make_ctx(lua: &Lua, commands: &Rc<RefCell<Vec<Command>>>) -> mlua::Result<Table> {
    let ctx = lua.create_table()?;

    let queue = commands.clone();
    ctx.set(
        "send_text",
        lua.create_function(move |_, (_this, text): (Value, String)| {
            queue.borrow_mut().push(Command::SendText(text));
            Ok(())
        })?,
    )?;

    let queue = commands.clone();
    ctx.set(
        "log",
        lua.create_function(move |_, (_this, message): (Value, String)| {
            queue.borrow_mut().push(Command::Log(message));
            Ok(())
        })?,
    )?;

    // Ejecuta una acción nativa (`ctx:perform(terminal.action.Paste)`)
    let queue = commands.clone();
    ctx.set(
        "perform",
        lua.create_function(move |lua, (_this, action): (Value, Value)| {
            match parse_action(lua, action) {
                Ok(Action::Lua(_)) => Err(mlua::Error::runtime(
                    "ctx:perform solo admite acciones nativas (terminal.action.X)",
                )),
                Ok(action) => {
                    queue.borrow_mut().push(Command::Perform(action));
                    Ok(())
                }
                Err(message) => Err(mlua::Error::runtime(message)),
            }
        })?,
    )?;
    Ok(ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mods_are_parsed_and_combined() {
        assert_eq!(parse_mods("").unwrap(), 0);
        assert_eq!(parse_mods("CTRL").unwrap(), CTRL);
        assert_eq!(parse_mods("ctrl | shift").unwrap(), CTRL | SHIFT);
        assert_eq!(parse_mods("CTRL|ALT|SUPER").unwrap(), CTRL | ALT | SUPER);
        assert_eq!(parse_mods("CMD").unwrap(), SUPER);
        let error = parse_mods("CTRL|HYPER").unwrap_err();
        assert!(error.contains("HYPER"), "{error}");
    }

    #[test]
    fn key_names_map_to_winit_keys() {
        assert_eq!(parse_key("PageUp").unwrap(), KeyId::Named(NamedKey::PageUp));
        assert_eq!(parse_key("pagedown").unwrap(), KeyId::Named(NamedKey::PageDown));
        assert_eq!(parse_key("F5").unwrap(), KeyId::Named(NamedKey::F5));
        assert_eq!(parse_key("f24").unwrap(), KeyId::Named(NamedKey::F24));
        assert_eq!(parse_key("Enter").unwrap(), KeyId::Named(NamedKey::Enter));
        assert_eq!(parse_key("space").unwrap(), KeyId::Named(NamedKey::Space));
        assert_eq!(parse_key(" ").unwrap(), KeyId::Named(NamedKey::Space));
        assert_eq!(parse_key("V").unwrap(), KeyId::Char('v'));
        assert_eq!(parse_key("=").unwrap(), KeyId::Char('='));
        assert_eq!(parse_key("ñ").unwrap(), KeyId::Char('ñ'));
    }

    #[test]
    fn unknown_or_out_of_range_keys_are_errors() {
        assert!(parse_key("F25").is_err());
        assert!(parse_key("F0").is_err());
        assert!(parse_key("").is_err());
        let error = parse_key("PagUp").unwrap_err();
        assert!(error.contains("PagUp"), "{error}");
    }

    #[test]
    fn winit_keys_are_normalized_for_lookup() {
        // Con Shift, la tecla "v" llega como "V": tiene que coincidir con el mismo enlace
        assert_eq!(key_id(&Key::Character("V".into())), Some(KeyId::Char('v')));
        assert_eq!(key_id(&Key::Character("v".into())), Some(KeyId::Char('v')));
        assert_eq!(key_id(&Key::Named(NamedKey::F5)), Some(KeyId::Named(NamedKey::F5)));
        // Secuencias de varios caracteres y teclas muertas no se pueden enlazar
        assert_eq!(key_id(&Key::Character("ab".into())), None);
        assert_eq!(key_id(&Key::Dead(Some('´'))), None);
    }

    #[test]
    fn event_names_are_known() {
        assert_eq!(EventKind::from_name("bell"), Some(EventKind::Bell));
        assert_eq!(EventKind::from_name("title-changed"), Some(EventKind::TitleChanged));
        assert_eq!(EventKind::from_name("nope"), None);
    }

    #[test]
    fn runaway_scripts_are_aborted() {
        let script = Script::new().unwrap();
        script.deadline.set(Instant::now() + Duration::from_millis(100));
        let started = Instant::now();
        let result = script.lua().load("while true do end").exec();
        assert!(result.is_err(), "un bucle infinito debería abortarse");
        assert!(started.elapsed() < Duration::from_secs(2), "tardó {:?}", started.elapsed());
        let message = lua_message(&result.unwrap_err());
        assert!(message.contains("demasiado"), "{message}");
    }

    #[test]
    fn a_failing_hook_does_not_propagate_and_keeps_working_afterwards() {
        let script = Script::new().unwrap();
        let broken: Function = script.lua().load(r#"return function(ctx) error("fallé") end"#).eval().unwrap();
        assert!(script.call(&broken, vec![]).is_empty());
        let working: Function = script
            .lua()
            .load(r#"return function(ctx, name) ctx:send_text("hola " .. name) end"#)
            .eval()
            .unwrap();
        let commands = script.call(&working, vec![script.text("mundo")]);
        assert!(matches!(commands.as_slice(), [Command::SendText(text)] if text == "hola mundo"));
    }

    #[test]
    fn a_hook_stuck_in_a_loop_is_cut_off_and_the_next_call_still_works() {
        let script = Script::new().unwrap();
        let looping: Function = script.lua().load("return function() while true do end end").eval().unwrap();
        let started = Instant::now();
        script.call(&looping, vec![]);
        assert!(started.elapsed() < Duration::from_secs(3), "tardó {:?}", started.elapsed());
        let fine: Function = script.lua().load(r#"return function(ctx) ctx:log("sigo vivo") end"#).eval().unwrap();
        let commands = script.call(&fine, vec![]);
        assert!(matches!(commands.as_slice(), [Command::Log(text)] if text == "sigo vivo"));
    }
}
