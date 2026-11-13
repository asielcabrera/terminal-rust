//! Configuration structures: the typed shape of the settings both the window and the server
//! process read.
//!
//! `config.lua` (evaluated in the window crate) returns a table converted to [`Config`] with
//! serde. Everything is optional and unknown fields are an error, so a `font_sise` is not
//! silently ignored.

use std::collections::BTreeMap;
use std::path::PathBuf;

use libghostty_vt::style::RgbColor;
use serde::{Deserialize, Deserializer, de};

pub const DEFAULT_TITLE: &str = "Terminal Emulator";

#[cfg(windows)]
const DEFAULT_FAMILY: &str = "Consolas";
// The first two are the symbols-only Nerd Fonts (icons, Powerline). If they are not installed
// they are skipped, and as a last resort any installed Nerd Font is used (see glyphs.rs).
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
    // Only outline font on macOS with media-control symbols (⏺ ⏵) used by TUIs like Claude Code
    "STIX Two Math",
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
    /// Padding around the grid, in logical pixels
    pub padding: f64,
    pub cursor: CursorConfig,
    pub colors: ColorsConfig,
    pub shell: ShellConfig,
    pub window: WindowConfig,
    pub mouse: MouseConfig,
    pub panes: PanesConfig,
    /// Lines of history (startup only)
    pub scrollback: usize,
    pub on_shell_exit: ShellExit,
    /// Drops the keys the terminal binds by default (copy, paste, scroll). Keys defined in `keys`
    /// always take precedence over the defaults.
    pub disable_default_keys: bool,
    /// Remote access over the tailnet (`terminal gateway`)
    pub remote: RemoteConfig,
    /// Kitty graphics protocol (images the shell draws with `ESC _G`)
    pub graphics: GraphicsConfig,
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
            remote: RemoteConfig::default(),
            graphics: GraphicsConfig::default(),
        }
    }
}

/// Kitty graphics protocol: images a program transmits and displays inline (kitty's `ESC _G`
/// sequence) when its PNG renderings are enabled.
///
/// Images are session-only: they live in the pane's terminal, they scroll with its content, and
/// they are GONE when the window re-attaches — a snapshot is VT text and cannot carry them.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GraphicsConfig {
    /// Total size, in bytes, of decoded images the protocol may store per pane (all of the
    /// screens: primary and alternate). When the cap is reached the oldest images are evicted
    /// (libghostty's policy). The default is 512 MiB; set it lower to bound the memory a
    /// runaway could spend on images, or to 0 to turn the protocol off. Startup only.
    pub storage_limit: u64,
}

impl Default for GraphicsConfig {
    fn default() -> Self {
        Self {
            storage_limit: 512 * 1024 * 1024,
        }
    }
}

/// Multiple panes: how the focused one is distinguished
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PanesConfig {
    /// Color of the dividers between panes. Defaults to heavily dimmed text color.
    pub border_color: Option<Color>,
    /// Color of the focused pane's frame. Defaults to the cursor color.
    pub focus_color: Option<Color>,
    /// Frame color while the leader (`leader`) waits for the next key. Defaults to the palette's
    /// yellow (index 3).
    pub leader_color: Option<Color>,
    /// How much unfocused panes' text is dimmed: 0 changes nothing, 1 makes it invisible
    pub inactive_dim: f32,
    /// A new pane (or float) starts in the focused pane's directory, if its shell reports it via
    /// OSC 7 or, on macOS and Linux, by asking the system; otherwise in `shell.cwd`
    pub inherit_cwd: bool,
    /// Size of a new float, as a fraction of the window
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

/// Remote access over the tailnet (stage 5a): `terminal gateway` fronts the local server for
/// one remote client over a loopback WebSocket, and Tailscale provides both the transport
/// (`tailscale serve`, TLS included) and the identity headers.
#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct RemoteConfig {
    /// Tailscale logins (`Tailscale-User-Login`, falling back to `Tailscale-User-Name`) that
    /// may attach through `terminal gateway`. **Empty denies everyone**: remote access stays
    /// owner-off until this names at least one login, e.g.
    /// `remote = { allow = { "you@example.com" } }`. The identity headers are only
    /// trustworthy in front of `tailscale serve` — the gateway binds 127.0.0.1 only, and a
    /// direct loopback hit could set them freely.
    pub allow: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FontConfig {
    /// Family name as the system sees it ("Consolas", "JetBrains Mono"...)
    pub family: String,
    /// Size in logical pixels (multiplied by the display scale factor)
    pub size: f32,
    /// Families to try, in order, for characters the main family does not cover
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
    /// Cursor blinking. Programs that request a blinking cursor (DECSCUSR) make it blink even if
    /// this is `false`. It only blinks while the window is focused, and stays solid while typing
    /// or while output arrives.
    pub blink: bool,
    /// Duration of each phase (visible or hidden) in milliseconds
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

/// Color as text: "#rrggbb", "#rgb" or an X11 name ("tomato")
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color(pub RgbColor);

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        RgbColor::parse(&text).map(Color).map_err(|_| {
            de::Error::custom(format!(
                "invalid color: {text:?} (use \"#rrggbb\" or an X11 name)"
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
    /// Palette colors starting at index 0 (0–15 are ANSI, 16–255 the color cube and the grays);
    /// anything not given keeps its default
    pub ansi: Option<Vec<Color>>,
    /// Colors of selected text. If missing, the screen colors are swapped (the background takes
    /// the text color and vice versa).
    pub selection_foreground: Option<Color>,
    pub selection_background: Option<Color>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShellConfig {
    /// If missing, the system's default shell is used (startup only)
    pub program: Option<String>,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    /// PowerShell does not report its current directory (its `cd` does not change the process one).
    /// With this, the terminal launches it with `-NoExit -Command` and a prompt-wrapping script
    /// that reports it via OSC 7, so new panes and floats start where you are. Not applied if `args`
    /// already contains `-Command`, `-File` or `-EncodedCommand`. Startup only.
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

/// Modifier bits (independent of winit and libghostty)
pub const CTRL: u8 = 1;
pub const SHIFT: u8 = 2;
pub const ALT: u8 = 4;
pub const SUPER: u8 = 8;
/// "Leader pressed": in a key's `mods`, binds it to the key pressed after the `leader`
pub const LEADER: u8 = 16;

/// "CTRL|SHIFT" to bits. Accepts CTRL, SHIFT, ALT, and SUPER (also CMD and WIN).
pub fn parse_mods(text: &str) -> Result<u8, String> {
    parse_mods_with(text, false)
}

/// Like `parse_mods`, with a plain-modifier set `allow_leader` widens with `LEADER`
pub fn parse_mods_with(text: &str, allow_leader: bool) -> Result<u8, String> {
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

/// Modifiers as text: "CTRL|SHIFT", "ALT"... (CTRL, SHIFT, ALT and SUPER)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Modifiers(pub u8);

impl<'de> Deserialize<'de> for Modifiers {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        parse_mods(&text).map(Modifiers).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decorations {
    /// With the system title bar and its buttons
    Full,
    /// No bar or buttons. The window is resized by its edges and moved with
    /// `window.drag_modifiers` + drag.
    None,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WindowConfig {
    /// Fixed window title. If missing, the window shows the title set by the program
    /// (`DEFAULT_TITLE` until it sets one).
    pub title: Option<String>,
    pub decorations: Decorations,
    /// With `decorations = "none"`: modifiers that, held while dragging with the left button,
    /// move the window
    pub drag_modifiers: Modifiers,
    /// Window corners rounded as the system decides (`true`) or square (`false`).
    /// Only has an effect on Windows 11.
    pub rounded_corners: bool,
    /// Initial grid size (startup only). If missing, the window gets whatever size the system
    /// gives it; if only one is set, the other is 80 columns or 24 rows. It is a request:
    /// a tiling window manager may ignore it.
    pub columns: Option<u16>,
    pub rows: Option<u16>,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            title: None,
            decorations: Decorations::Full,
            drag_modifiers: Modifiers(ALT),
            rounded_corners: true,
            columns: None,
            rows: None,
        }
    }
}

/// What right click does when the program does not capture the mouse
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RightClick {
    Paste,
    None,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MouseConfig {
    /// On button release after selecting, the text goes to the clipboard
    pub copy_on_select: bool,
    pub right_click: RightClick,
    /// Lines scrolled per wheel notch
    pub scroll_lines: u16,
    /// Modifiers that, held while dragging, make a rectangular selection
    pub rectangle_modifiers: Modifiers,
}

impl Default for MouseConfig {
    fn default() -> Self {
        Self {
            copy_on_select: true,
            right_click: RightClick::Paste,
            scroll_lines: 3,
            rectangle_modifiers: Modifiers(CTRL | ALT),
        }
    }
}

/// What to do when the shell exits
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShellExit {
    /// Close the window
    Close,
    /// Keep the window open with whatever was left on screen
    Hold,
}

impl Config {
    pub fn validate(&self) -> Result<(), String> {
        if !(self.font.size.is_finite() && (4.0..=200.0).contains(&self.font.size)) {
            return Err(format!(
                "font.size must be between 4 and 200 (got {})",
                self.font.size
            ));
        }
        if self.font.family.trim().is_empty() {
            return Err("font.family cannot be empty".to_string());
        }
        if !(self.padding.is_finite() && (0.0..=200.0).contains(&self.padding)) {
            return Err(format!(
                "padding must be between 0 and 200 (got {})",
                self.padding
            ));
        }
        if !(100..=5000).contains(&self.cursor.blink_ms) {
            return Err(format!(
                "cursor.blink_ms must be between 100 and 5000 (got {})",
                self.cursor.blink_ms
            ));
        }
        if self.window.columns == Some(0) || self.window.rows == Some(0) {
            return Err("window.columns and window.rows must be greater than 0".to_string());
        }
        if !(1..=100).contains(&self.mouse.scroll_lines) {
            return Err(format!(
                "mouse.scroll_lines must be between 1 and 100 (got {})",
                self.mouse.scroll_lines
            ));
        }
        // Dragging without modifiers is selecting: both gestures need at least one, and they must
        // differ from each other (otherwise there would be no way to tell which one is meant)
        if self.mouse.rectangle_modifiers.0 == 0 {
            return Err(
                "mouse.rectangle_modifiers cannot be empty (a plain drag already selects)"
                    .to_string(),
            );
        }
        if self.window.decorations == Decorations::None {
            if self.window.drag_modifiers.0 == 0 {
                return Err(
                    "window.drag_modifiers cannot be empty (a plain drag already selects)"
                        .to_string(),
                );
            }
            if self.window.drag_modifiers == self.mouse.rectangle_modifiers {
                return Err(
                    "window.drag_modifiers and mouse.rectangle_modifiers cannot be the same"
                        .to_string(),
                );
            }
        }
        if self.shell.program.is_none() && !self.shell.args.is_empty() {
            return Err("shell.args requires shell.program (arguments cannot be passed to the default shell)".to_string());
        }
        if !(0.0..=1.0).contains(&self.panes.inactive_dim) {
            return Err(format!(
                "panes.inactive_dim must be between 0 and 1 (got {})",
                self.panes.inactive_dim
            ));
        }
        for (name, value) in [
            ("panes.float_width", self.panes.float_width),
            ("panes.float_height", self.panes.float_height),
        ] {
            if !(0.1..=1.0).contains(&value) {
                return Err(format!("{name} must be between 0.1 and 1 (got {value})"));
            }
        }
        if let Some(ansi) = &self.colors.ansi
            && ansi.len() > 256
        {
            return Err(format!(
                "colors.ansi accepts at most 256 colors (got {})",
                ansi.len()
            ));
        }
        Ok(())
    }
}
