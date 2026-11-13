//! Keys, actions, and events defined in config.lua, plus the Lua state that runs them.
//!
//! Lua is not `Send` and lives on the UI thread. A Lua function cannot call back into `App`
//! while `App` is borrowed, so whatever it wants to do (send text, run an action) is queued as a
//! [`Command`] and `App` executes it once the function returns.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::time::{Duration, Instant};

use mlua::{Function, HookTriggers, Lua, LuaSerdeExt, Table, Value, VmState};
use serde::Deserialize;
use winit::keyboard::{Key, NamedKey};

/// Modifier bits (independent of winit and libghostty)
pub const CTRL: u8 = 1;
pub const SHIFT: u8 = 2;
pub const ALT: u8 = 4;
pub const SUPER: u8 = 8;
/// "Leader pressed": in a key's `mods`, binds it to the key pressed after the `leader`
pub const LEADER: u8 = 16;

/// Maximum run time for a script: the config on load, or each function call. Without it a
/// `while true do end` would freeze the whole terminal, since Lua runs on the UI thread.
pub const CONFIG_TIME_LIMIT: Duration = Duration::from_secs(2);
const HOOK_TIME_LIMIT: Duration = Duration::from_millis(250);
/// How many Lua instructions run between clock checks
const TIME_CHECK_EVERY: u32 = 10_000;

/// A Lua error message without the stack traceback, which only adds noise
pub fn lua_message(error: &mlua::Error) -> String {
    let text = error.to_string();
    text.split("\nstack traceback:")
        .next()
        .unwrap_or(&text)
        .trim()
        .to_string()
}

/// Direction of a pane action (in Lua: "left", "right", "up", "down")
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// What a key or a hook can do. Built-in actions are built from Lua with
/// `terminal.action.X` (tables with an `action` key); `Lua` is a user-defined function.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "action", deny_unknown_fields)]
pub enum Action {
    /// Writes the text to the shell, as is
    SendString {
        text: String,
    },
    /// Scrolls the history: negative goes up, positive goes down
    ScrollLines {
        amount: isize,
    },
    ScrollPages {
        amount: isize,
    },
    ScrollToTop,
    ScrollToBottom,
    IncreaseFontSize,
    DecreaseFontSize,
    ResetFontSize,
    ToggleFullscreen,
    ReloadConfig,
    /// Pastes the clipboard (with bracketed paste if the program requested it)
    Paste,
    /// Copies the selected text to the clipboard (does nothing if there is no selection)
    Copy,
    ClearSelection,
    /// Selects all content, history included
    SelectAll,
    /// Splits the focused pane: the new one goes to the right
    SplitRight,
    /// Splits the focused pane: the new one goes below
    SplitDown,
    /// Closes the focused pane and ends its shell; if it is the last tiled pane, closes the window
    ClosePane,
    /// Moves focus to the neighboring pane in that direction
    FocusPane {
        dir: Direction,
    },
    /// Moves focus to the next or previous pane (tiled panes first, then floating ones)
    FocusNext,
    FocusPrevious,
    /// Moves the pane's edge `amount` cells toward `dir` (grows toward that side, or shrinks if the
    /// pane sits at the window's edge on that side)
    ResizePane {
        dir: Direction,
        amount: u16,
    },
    /// Zooms the focused pane to fill the window, or returns to the tiled layout
    ToggleZoom,
    /// Opens a floating pane with its own shell, or with `program`, that closes when it exits
    /// (like a popup). `width` and `height` are fractions of the window (by default,
    /// `panes.float_width` and `panes.float_height`); `cwd` defaults to the focused pane's
    /// directory if the shell reports it. `env` entries are merged over the shell's
    /// environment (this action's entries win over the global `shell.env`).
    NewFloat {
        #[serde(default)]
        program: Option<String>,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        width: Option<f32>,
        #[serde(default)]
        height: Option<f32>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    /// Hides or shows all floating panes without closing their shells; if there are none, opens one
    ToggleFloats,
    /// Moves the focused floating pane `amount` cells toward `dir`
    MovePane {
        dir: Direction,
        amount: u16,
    },
    Quit,
    #[serde(skip)]
    Lua(Function),
}

/// What a Lua function wants to do; `App` executes it after the call
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

/// A winit logical key (the one that accounts for the keyboard layout) in our normalized form.
/// Characters are lowercase: with Shift, `v` arrives as `V`.
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
            "unknown key {name:?} (use a character like \"v\" or a name like \"PageUp\", \"F5\", \"Enter\", \"Space\")"
        )),
    }
}

/// "CTRL|SHIFT" to bits. Accepts CTRL, SHIFT, ALT, and SUPER (also CMD and WIN).
pub fn parse_mods(text: &str) -> Result<u8, String> {
    parse_mods_with(text, false)
}

/// Like `parse_mods`, but keys in `keys` may also use `LEADER`
fn parse_binding_mods(text: &str) -> Result<u8, String> {
    parse_mods_with(text, true)
}

fn parse_mods_with(text: &str, allow_leader: bool) -> Result<u8, String> {
    let mut bits = 0;
    for part in text
        .split('|')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        bits |= match part.to_uppercase().as_str() {
            "CTRL" | "CONTROL" => CTRL,
            "SHIFT" => SHIFT,
            "ALT" | "OPTION" => ALT,
            "SUPER" | "CMD" | "WIN" | "META" => SUPER,
            "LEADER" if allow_leader => LEADER,
            _ if allow_leader => {
                return Err(format!(
                    "unknown modifier {part:?} (use CTRL, SHIFT, ALT, SUPER, or LEADER, separated by |)"
                ));
            }
            _ => {
                return Err(format!(
                    "unknown modifier {part:?} (use CTRL, SHIFT, ALT, or SUPER, separated by |)"
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

    /// The key pressed right after the leader. A character already carries SHIFT itself (`%` is
    /// Shift+5 on a US keyboard), so without an exact binding it is retried without SHIFT; `w` and
    /// `W` stay distinct because the exact combination is checked first.
    fn lookup_after_leader(&self, key: &Key, mods: u8) -> Option<&Action> {
        let id = key_id(key)?;
        if let Some(action) = self.map.get(&(id.clone(), LEADER | mods)) {
            return Some(action);
        }
        if matches!(id, KeyId::Char(_)) && mods & SHIFT != 0 {
            return self.map.get(&(id, LEADER | (mods & !SHIFT)));
        }
        None
    }

    /// Keys the terminal binds without any configuration: copy, paste, scroll the history, and the
    /// pane keys after the leader (tmux style). Entries in `keys` in config.lua replace
    /// them.
    fn defaults() -> Self {
        let mut map = HashMap::new();
        let mut bind = |key: KeyId, mods: u8, action: Action| {
            map.insert((key, mods), action);
        };
        bind(KeyId::Char('c'), CTRL | SHIFT, Action::Copy);
        bind(KeyId::Named(NamedKey::Insert), CTRL, Action::Copy);
        bind(KeyId::Char('v'), CTRL | SHIFT, Action::Paste);
        bind(KeyId::Named(NamedKey::Insert), SHIFT, Action::Paste);
        bind(
            KeyId::Named(NamedKey::PageUp),
            SHIFT,
            Action::ScrollPages { amount: -1 },
        );
        bind(
            KeyId::Named(NamedKey::PageDown),
            SHIFT,
            Action::ScrollPages { amount: 1 },
        );

        // Panes, after the leader
        bind(KeyId::Char('%'), LEADER, Action::SplitRight);
        bind(KeyId::Char('"'), LEADER, Action::SplitDown);
        bind(KeyId::Char('x'), LEADER, Action::ClosePane);
        bind(KeyId::Char('z'), LEADER, Action::ToggleZoom);
        bind(KeyId::Char('o'), LEADER, Action::FocusNext);
        bind(KeyId::Char('w'), LEADER, Action::ToggleFloats);
        bind(
            KeyId::Char('w'),
            LEADER | SHIFT,
            Action::NewFloat {
                program: None,
                args: Vec::new(),
                width: None,
                height: None,
                cwd: None,
                env: BTreeMap::new(),
            },
        );
        for (key, dir) in [
            (NamedKey::ArrowLeft, Direction::Left),
            (NamedKey::ArrowRight, Direction::Right),
            (NamedKey::ArrowUp, Direction::Up),
            (NamedKey::ArrowDown, Direction::Down),
        ] {
            bind(KeyId::Named(key), LEADER, Action::FocusPane { dir });
            bind(
                KeyId::Named(key),
                LEADER | CTRL,
                Action::ResizePane { dir, amount: 1 },
            );
            bind(
                KeyId::Named(key),
                LEADER | ALT,
                Action::ResizePane { dir, amount: 5 },
            );
            bind(
                KeyId::Named(key),
                LEADER | SHIFT,
                Action::MovePane { dir, amount: 2 },
            );
        }
        for (key, dir) in [
            ('h', Direction::Left),
            ('j', Direction::Down),
            ('k', Direction::Up),
            ('l', Direction::Right),
        ] {
            bind(KeyId::Char(key), LEADER, Action::FocusPane { dir });
        }
        Self { map }
    }
}

/// The leader key (like tmux's Ctrl+B): after pressing it, the next key is looked up among
/// those bound with `mods = "LEADER"`
pub struct Leader {
    key: KeyId,
    mods: u8,
    timeout: Duration,
}

impl Leader {
    fn matches(&self, key: &Key, mods: u8) -> bool {
        key_id(key).is_some_and(|id| id == self.key) && mods == self.mods
    }

    /// What is sent to the program when the leader is pressed twice: the control character for
    /// Ctrl+letter (Ctrl+B is 0x02). With any other leader, nothing.
    fn passthrough(&self) -> Option<Vec<u8>> {
        match (&self.key, self.mods) {
            (KeyId::Char(letter), CTRL) if letter.is_ascii_alphabetic() => {
                Some(vec![(*letter as u8) & 0x1f])
            }
            _ => None,
        }
    }
}

/// The `leader` table from config.lua as received, before checking the key and modifiers
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawLeader {
    key: String,
    mods: String,
    timeout_ms: u32,
}

impl Default for RawLeader {
    fn default() -> Self {
        Self {
            key: "b".to_string(),
            mods: "CTRL".to_string(),
            timeout_ms: 1500,
        }
    }
}

/// Parses `leader`: unset (or `true`) means Ctrl+B, `false` disables it, and a table
/// `{ key, mods, timeout_ms }` customizes it
pub fn parse_leader(value: Value) -> Result<Option<Leader>, String> {
    let raw: RawLeader = match value {
        Value::Nil | Value::Boolean(true) => RawLeader::default(),
        Value::Boolean(false) => return Ok(None),
        Value::Table(_) => serde_path_to_error::deserialize(mlua::serde::Deserializer::new(value))
            .map_err(|e| format!("leader: {e}"))?,
        other => {
            return Err(format!(
                "leader must be a table {{ key = ..., mods = ... }}, true, or false (got {})",
                other.type_name()
            ));
        }
    };
    let key = parse_key(&raw.key).map_err(|e| format!("leader.key: {e}"))?;
    let mods = parse_mods(&raw.mods).map_err(|e| format!("leader.mods: {e}"))?;
    if !(200..=10_000).contains(&raw.timeout_ms) {
        return Err(format!(
            "leader.timeout_ms must be between 200 and 10000 (got {})",
            raw.timeout_ms
        ));
    }
    Ok(Some(Leader {
        key,
        mods,
        timeout: Duration::from_millis(u64::from(raw.timeout_ms)),
    }))
}

/// Whether a leader press is waiting for the next key, and until when
#[derive(Default)]
pub struct LeaderState {
    until: Option<Instant>,
}

impl LeaderState {
    pub fn pending(&self) -> bool {
        self.until.is_some()
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.until
    }

    /// If the timeout expired, cancels the leader and returns `true`
    pub fn expire(&mut self, now: Instant) -> bool {
        if self.until.is_some_and(|until| now >= until) {
            self.until = None;
            return true;
        }
        false
    }

    pub fn clear(&mut self) {
        self.until = None;
    }
}

/// What to do with a key press depending on the leader state
#[derive(Debug)]
pub enum LeaderOutcome {
    /// Unrelated to the leader: follows the normal path (bindings, encoder, shell)
    Normal,
    /// A lone modifier or a repeat while the leader is pending: do nothing
    Ignore,
    /// The leader was pressed: now waiting for the next key
    Arm,
    /// The leader is cancelled (Escape or an unbound key); the key is discarded
    Cancel,
    /// The leader was pressed twice: this control character is sent to the program
    Passthrough(Vec<u8>),
    /// The key had a binding after the leader
    Run(Action),
}

/// Keys that are only a modifier: pressing Shift to type `%` does not cancel the leader
fn is_modifier_key(key: &Key) -> bool {
    matches!(
        key,
        Key::Named(
            NamedKey::Shift
                | NamedKey::Control
                | NamedKey::Alt
                | NamedKey::AltGraph
                | NamedKey::Super
                | NamedKey::Meta
                | NamedKey::Hyper
                | NamedKey::Fn
                | NamedKey::CapsLock
                | NamedKey::NumLock
        )
    )
}

/// Parses the `keys` table from config.lua. Modifiers must match exactly (SHIFT too when the
/// character needs it, like `+` on a US keyboard). With `defaults`, it starts from the default
/// keys: a user key replaces the default one with the same combination, but two user keys with
/// the same combination are an error.
pub fn parse_keys(lua: &Lua, value: Value, defaults: bool) -> Result<Bindings, String> {
    let table = match value {
        Value::Nil => {
            return Ok(if defaults {
                Bindings::defaults()
            } else {
                Bindings::default()
            });
        }
        Value::Table(table) => table,
        other => {
            return Err(format!(
                "keys must be a list of tables {{ key = ..., mods = ..., action = ... }} (got {})",
                other.type_name()
            ));
        }
    };

    let mut bindings = if defaults {
        Bindings::defaults()
    } else {
        Bindings::default()
    };
    let mut from_user = std::collections::HashSet::new();
    for (index, entry) in table.sequence_values::<Value>().enumerate() {
        let at = format!("keys[{}]", index + 1);
        let entry = entry.map_err(|e| format!("{at}: {}", lua_message(&e)))?;
        let Value::Table(entry) = entry else {
            return Err(format!(
                "{at} must be a table {{ key = ..., mods = ..., action = ... }}"
            ));
        };

        let name: String = entry
            .get("key")
            .map_err(|_| format!("{at}.key must be a string, like \"v\" or \"PageUp\""))?;
        let key = parse_key(&name).map_err(|e| format!("{at}.key: {e}"))?;
        let mods = match entry.get::<Value>("mods").map_err(|e| lua_message(&e))? {
            Value::Nil => 0,
            Value::String(text) => parse_binding_mods(&text.to_string_lossy())
                .map_err(|e| format!("{at}.mods: {e}"))?,
            other => {
                return Err(format!(
                    "{at}.mods must be a string, like \"CTRL|SHIFT\" (got {})",
                    other.type_name()
                ));
            }
        };
        let action = parse_action(lua, entry.get("action").map_err(|e| lua_message(&e))?)
            .map_err(|e| format!("{at}.action: {e}"))?;

        if !from_user.insert((key.clone(), mods)) {
            return Err(format!(
                "{at}: the combination {name:?} + {mods:?} is already bound"
            ));
        }
        bindings.map.insert((key, mods), action);
    }
    Ok(bindings)
}

/// An action is a user-defined function, `terminal.action.X`, or the name of a parameterless action
pub fn parse_action(lua: &Lua, value: Value) -> Result<Action, String> {
    match value {
        Value::Function(function) => Ok(Action::Lua(function)),
        Value::String(name) => {
            let table = lua.create_table().map_err(|e| lua_message(&e))?;
            table.set("action", name).map_err(|e| lua_message(&e))?;
            lua.from_value(Value::Table(table))
                .map_err(|e| lua_message(&e))
        }
        Value::Table(_) => lua.from_value(value).map_err(|e| lua_message(&e)),
        other => Err(format!(
            "must be a function, terminal.action.X, or the name of an action (got {})",
            other.type_name()
        )),
    }
}

/// Things that happen to the terminal for which a hook can be defined in `events`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    /// The program changed the title (receives the title)
    TitleChanged,
    /// The bell rang
    Bell,
    /// The shell exited
    ShellExit,
    /// The configuration was reloaded successfully
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
        Self::ALL
            .iter()
            .find(|(known, _)| *known == name)
            .map(|(_, kind)| *kind)
    }
}

#[derive(Default)]
pub struct Events {
    handlers: HashMap<EventKind, Function>,
}

/// Parses the `events` table from config.lua
pub fn parse_events(value: Value) -> Result<Events, String> {
    let table = match value {
        Value::Nil => return Ok(Events::default()),
        Value::Table(table) => table,
        other => {
            return Err(format!(
                "events must be a table {{ [\"bell\"] = function(ctx) ... end }} (got {})",
                other.type_name()
            ));
        }
    };

    let mut events = Events::default();
    for pair in table.pairs::<String, Value>() {
        let (name, handler) = pair
            .map_err(|_| "events keys must be event names (for example [\"bell\"])".to_string())?;
        let Some(kind) = EventKind::from_name(&name) else {
            let valid: Vec<_> = EventKind::ALL.iter().map(|(name, _)| *name).collect();
            return Err(format!(
                "unknown event {name:?} (valid: {})",
                valid.join(", ")
            ));
        };
        let Value::Function(function) = handler else {
            return Err(format!(
                "events[{name:?}] must be a function (got {})",
                handler.type_name()
            ));
        };
        events.handlers.insert(kind, function);
    }
    Ok(events)
}

/// The Lua state for a configuration: its interpreter, the keys, the hooks, and the `ctx` that
/// functions receive.
pub struct Script {
    lua: Lua,
    bindings: Bindings,
    leader: Option<Leader>,
    events: Events,
    ctx: Table,
    commands: Rc<RefCell<Vec<Command>>>,
    deadline: Rc<Cell<Instant>>,
}

impl Script {
    /// Creates the interpreter with the time limit already active (for loading the configuration)
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
                            "the script took too long and was aborted (an infinite loop?)",
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
            leader: None,
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

    /// Sets the leader. If its combination is already a key in `keys`, that is an error: it would
    /// be ambiguous which of the two to run.
    pub fn set_leader(&mut self, leader: Option<Leader>) -> Result<(), String> {
        if let Some(leader) = &leader
            && self
                .bindings
                .map
                .contains_key(&(leader.key.clone(), leader.mods))
        {
            return Err("leader: that key combination is already bound in keys".to_string());
        }
        self.leader = leader;
        Ok(())
    }

    /// Decides what to do with a key press depending on the leader state (see `LeaderOutcome`)
    pub fn leader_step(
        &self,
        state: &mut LeaderState,
        key: &Key,
        mods: u8,
        repeat: bool,
        now: Instant,
    ) -> LeaderOutcome {
        let Some(leader) = &self.leader else {
            state.clear();
            return LeaderOutcome::Normal;
        };
        state.expire(now);
        if state.pending() {
            if repeat || is_modifier_key(key) {
                return LeaderOutcome::Ignore;
            }
            state.clear();
            if matches!(key, Key::Named(NamedKey::Escape)) && mods == 0 {
                return LeaderOutcome::Cancel;
            }
            if leader.matches(key, mods) {
                return leader
                    .passthrough()
                    .map_or(LeaderOutcome::Cancel, LeaderOutcome::Passthrough);
            }
            return self
                .bindings
                .lookup_after_leader(key, mods)
                .cloned()
                .map_or(LeaderOutcome::Cancel, LeaderOutcome::Run);
        }
        if leader.matches(key, mods) {
            if repeat {
                return LeaderOutcome::Ignore;
            }
            state.until = Some(now + leader.timeout);
            return LeaderOutcome::Arm;
        }
        LeaderOutcome::Normal
    }

    pub fn has_handler(&self, kind: EventKind) -> bool {
        self.events.handlers.contains_key(&kind)
    }

    /// A string as a Lua value, to pass as an argument to a hook
    pub fn text(&self, text: &str) -> Value {
        self.lua
            .create_string(text)
            .map_or(Value::Nil, Value::String)
    }

    /// Calls an event's hook (if any) and returns what it asked to do
    pub fn fire(&self, kind: EventKind, args: Vec<Value>) -> Vec<Command> {
        match self.events.handlers.get(&kind) {
            Some(handler) => self.call(handler, args),
            None => Vec::new(),
        }
    }

    /// Calls a Lua function with `ctx` as the first argument. An error in the function is logged
    /// and discarded: it never brings down the terminal. Returns what the function asked to do.
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

/// The functions' `ctx` object: its methods only queue commands (see the module comment)
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

    // Runs a built-in action (`ctx:perform(terminal.action.Paste)`)
    let queue = commands.clone();
    ctx.set(
        "perform",
        lua.create_function(move |lua, (_this, action): (Value, Value)| {
            match parse_action(lua, action) {
                Ok(Action::Lua(_)) => Err(mlua::Error::runtime(
                    "ctx:perform only accepts built-in actions (terminal.action.X)",
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
        assert_eq!(
            parse_key("pagedown").unwrap(),
            KeyId::Named(NamedKey::PageDown)
        );
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
        // With Shift, the "v" key arrives as "V": it must match the same binding
        assert_eq!(key_id(&Key::Character("V".into())), Some(KeyId::Char('v')));
        assert_eq!(key_id(&Key::Character("v".into())), Some(KeyId::Char('v')));
        assert_eq!(
            key_id(&Key::Named(NamedKey::F5)),
            Some(KeyId::Named(NamedKey::F5))
        );
        // Multi-character sequences and dead keys cannot be bound
        assert_eq!(key_id(&Key::Character("ab".into())), None);
        assert_eq!(key_id(&Key::Dead(Some('´'))), None);
    }

    #[test]
    fn event_names_are_known() {
        assert_eq!(EventKind::from_name("bell"), Some(EventKind::Bell));
        assert_eq!(
            EventKind::from_name("title-changed"),
            Some(EventKind::TitleChanged)
        );
        assert_eq!(EventKind::from_name("nope"), None);
    }

    #[test]
    fn runaway_scripts_are_aborted() {
        let script = Script::new().unwrap();
        script
            .deadline
            .set(Instant::now() + Duration::from_millis(100));
        let started = Instant::now();
        let result = script.lua().load("while true do end").exec();
        assert!(result.is_err(), "an infinite loop should be aborted");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "took {:?}",
            started.elapsed()
        );
        let message = lua_message(&result.unwrap_err());
        assert!(message.contains("too long"), "{message}");
    }

    #[test]
    fn a_failing_hook_does_not_propagate_and_keeps_working_afterwards() {
        let script = Script::new().unwrap();
        let broken: Function = script
            .lua()
            .load(r#"return function(ctx) error("failed") end"#)
            .eval()
            .unwrap();
        assert!(script.call(&broken, vec![]).is_empty());
        let working: Function = script
            .lua()
            .load(r#"return function(ctx, name) ctx:send_text("hello " .. name) end"#)
            .eval()
            .unwrap();
        let commands = script.call(&working, vec![script.text("world")]);
        assert!(matches!(commands.as_slice(), [Command::SendText(text)] if text == "hello world"));
    }

    #[test]
    fn a_hook_stuck_in_a_loop_is_cut_off_and_the_next_call_still_works() {
        let script = Script::new().unwrap();
        let looping: Function = script
            .lua()
            .load("return function() while true do end end")
            .eval()
            .unwrap();
        let started = Instant::now();
        script.call(&looping, vec![]);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "took {:?}",
            started.elapsed()
        );
        let fine: Function = script
            .lua()
            .load(r#"return function(ctx) ctx:log("still alive") end"#)
            .eval()
            .unwrap();
        let commands = script.call(&fine, vec![]);
        assert!(matches!(commands.as_slice(), [Command::Log(text)] if text == "still alive"));
    }
}
