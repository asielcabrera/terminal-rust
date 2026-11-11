//! A pane: a libghostty terminal with its PTY, the threads that serve it, and the mouse
//! selection state. The app has one or more, arranged by `layout`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use libghostty_vt::mouse;
use libghostty_vt::selection::gesture::Gesture;
use libghostty_vt::style::{Palette, PaletteIndex, RgbColor};
use libghostty_vt::terminal::{
    ConformanceLevel, CursorStyle as VtCursorStyle, DeviceAttributeFeature, DeviceAttributes,
    DeviceType, PrimaryDeviceAttributes, SecondaryDeviceAttributes, TertiaryDeviceAttributes,
};
use libghostty_vt::{RenderState, Terminal, TerminalOptions};
use portable_pty::CommandBuilder;
use winit::event_loop::EventLoopProxy;

use crate::config::{self, Config, ShellConfig};
use crate::layout::{PaneId, Rect};
use crate::pane_io::{IoEvent, LocalPty, Notify, PaneIo};
use crate::{LuaEvent, UserEvent};

/// Shell chunks processed per wakeup. With heavy output (`cat` of a large file) this lets window
/// events (redraw, close) interleave instead of waiting until the end.
pub const MAX_CHUNKS_PER_WAKE: usize = 256;

// Reply to the device attributes query (CSI c). ConPTY sends it at startup, along with the cursor
// position query, and waits up to 3 s for the terminal to answer before showing anything.
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

// libghostty factory colors (measured: white text on a black background)
const DEFAULT_FOREGROUND: RgbColor = RgbColor {
    r: 255,
    g: 255,
    b: 255,
};
const DEFAULT_BACKGROUND: RgbColor = RgbColor { r: 0, g: 0, b: 0 };

/// Default colors and cursor from the configuration.
///
/// Foreground and background are ALWAYS set together and with explicit values, because of two
/// verified quirks of libghostty-vt: (1) they only apply when both are set at once, and (2) passing
/// `None` is not reflected in an existing `RenderState` (it keeps the previous colors).
/// This way, removing a line from config.lua on reload restores the factory colors.
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

/// Script that, run inside an interactive PowerShell (with its profile already loaded), makes it
/// report its current directory via OSC 7 on every prompt. It wraps whatever `prompt` function
/// exists (starship's, the default one...): it calls it FIRST, because the first thing a prompt
/// like starship's does is read `$?`, and any earlier statement would clobber it; its text is
/// returned unchanged. It is a single line with no double quotes or trailing backslashes, so it
/// survives being passed through the Windows command line.
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

/// Do the PowerShell arguments already ask to run a command or a file?
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

/// The shell with directory integration (see `ShellConfig::integration`) when applicable: it is
/// PowerShell, integration is enabled, and the arguments do not already include a command
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

pub(crate) fn build_command(shell: &ShellConfig) -> CommandBuilder {
    // `args` panics with the default shell; config.rs validation already guarantees
    // there are only arguments when there is a program.
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

/// `portable-pty` returns the `CreateProcessW` failure as plain text ("CreateProcessW `...`
/// in cwd `...` failed: <cause>") with escaped NUL characters. For display this keeps only the
/// cause; if the format ever changes, it returns the full message.
pub(crate) fn spawn_failure_cause(error: &impl std::fmt::Display) -> String {
    let text = error.to_string();
    match text.rsplit_once(" failed: ") {
        Some((_, cause)) => cause.to_string(),
        None => text,
    }
}

/// Writes a red notice into the grid itself, before the shell starts. When launched by
/// double-click there is no console to read stderr, so the error shows up where the user is.
pub fn write_notice(terminal: &mut Terminal<'static, 'static>, tag: &str, message: &str) {
    // The terminal needs a carriage return with each line feed; any other control character
    // (which could come from an error message) is dropped
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

/// The directory a shell reported: the OSC 7 URI or, as ConEmu and Windows Terminal do with
/// OSC 9;9 (common in cmd prompts), an absolute path as is, sometimes quoted
fn reported_path(value: &str) -> Option<String> {
    if value.starts_with("file://") {
        return osc7_path(value);
    }
    let path = value.trim().trim_matches('"');
    std::path::Path::new(path)
        .is_absolute()
        .then(|| path.to_string())
}

/// The path of an OSC 7 directory URI ("file://host/C:/path%20with%20spaces", with or without
/// host), decoded. On Windows it strips the slash before the drive letter ("/C:/path").
fn osc7_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    // Whatever sits between "file://" and the first slash is the host
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

/// Directory the terminal's shell reported via OSC 7, if it exists
fn terminal_dir(terminal: &Terminal<'static, 'static>) -> Option<PathBuf> {
    let path = PathBuf::from(reported_path(terminal.pwd().ok()?)?);
    path.is_dir().then_some(path)
}

/// What to launch, and where, for a new pane
pub struct Spawn<'a> {
    pub id: PaneId,
    pub cols: u16,
    pub rows: u16,
    /// Program and arguments instead of the configured shell (a floating pane launched with
    /// `program`). If it fails to start, it does not fall back to the default shell: no pane opens.
    pub program: Option<(String, Vec<String>)>,
    /// Working directory; if missing, `shell.cwd`
    pub cwd: Option<PathBuf>,
    /// Red notice (label and message) written before the shell starts
    pub notice: Option<(&'a str, &'a str)>,
}

pub struct Pane {
    pub id: PaneId,
    pub terminal: Terminal<'static, 'static>,
    /// libghostty state used only to draw this pane
    pub render_state: RenderState<'static>,
    /// Rectangle it occupies in the window (physical pixels), including its padding
    pub rect: Rect,
    /// Columns and rows last reported to the PTY and to libghostty
    pub grid: (u16, u16),
    /// The program behind the pane: its input, output, and lifetime
    io: Box<dyn PaneIo>,
    /// Terminal replies to the shell's queries (cursor position, attributes...)
    replies: Receiver<Vec<u8>>,
    /// State of the selection gesture (click, double-click, drag) in this terminal
    pub gesture: Gesture<'static>,
    /// A selection drag is in progress
    pub selecting: bool,
    /// Button whose press was sent to the program (motion and release go to it as well)
    pub reported: Option<mouse::Button>,
}

impl Pane {
    /// Creates the terminal, opens the PTY, launches the shell, and starts the threads serving it.
    /// The threads wake the event loop with `UserEvent::PtyOutput(id)` / `ShellExited(id)`.
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

        // By default libghostty ignores the shell's queries. Their replies are queued here and the
        // event loop writes them to the PTY. Title and bell events are queued the same way to
        // dispatch them to Lua once `vt_write` has finished (Lua is never called inside the
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

        // The shell (or the requested program). If the configured program cannot be started, a
        // notice is shown and the default shell is used.
        let explicit = spawn.program.is_some();
        let mut shell = config.shell.clone();
        if let Some((program, args)) = spawn.program {
            shell.program = Some(program);
            shell.args = args;
        }
        if let Some(cwd) = spawn.cwd {
            shell.cwd = Some(cwd);
        }
        let notify: Notify = Arc::new(move |event| {
            let _ = proxy.send_event(match event {
                IoEvent::Output => UserEvent::PtyOutput(id),
                IoEvent::Exited => UserEvent::ShellExited(id),
            });
        });
        let (io, fallback) = LocalPty::spawn(
            &with_integration(shell),
            explicit,
            spawn.cols,
            spawn.rows,
            notify,
        )?;
        if let Some(message) = fallback {
            write_notice(&mut terminal, "shell", &message);
        }

        Ok(Pane {
            id,
            terminal,
            render_state: RenderState::new().map_err(vt_error)?,
            rect: Rect::new(0, 0, 0, 0),
            grid: (spawn.cols, spawn.rows),
            io: Box::new(io),
            replies,
            gesture: Gesture::new().map_err(vt_error)?,
            selecting: false,
            reported: None,
        })
    }

    /// Writes to the shell's input
    pub fn write(&mut self, bytes: &[u8]) {
        self.io.write(bytes);
    }

    /// Feeds libghostty what the shell wrote (up to `max_chunks` chunks) and answers its
    /// queries. Returns how many chunks were processed.
    pub fn drain(&mut self, max_chunks: usize) -> usize {
        // Re-armed BEFORE reading: a chunk arriving during the drain triggers another notification
        self.io.rearm();

        let mut chunks = 0;
        while chunks < max_chunks
            && let Some(bytes) = self.io.try_read()
        {
            // Write the raw bytes into libghostty so it processes the ANSI escapes
            self.terminal.vt_write(&bytes);
            chunks += 1;
        }

        // Answer the queries the shell made in those bytes
        while let Ok(reply) = self.replies.try_recv() {
            self.write(&reply);
        }
        chunks
    }

    /// Requests another wakeup to keep draining what is left in the channel
    pub fn request_more(&self) {
        self.io.notify_again();
    }

    /// Notifies the PTY and libghostty (which reflows the buffer) of the new size
    pub fn resize(&mut self, cols: u16, rows: u16, cell_width: u32, cell_height: u32) {
        self.grid = (cols, rows);
        self.io.resize(cols, rows);
        let _ = self.terminal.resize(cols, rows, cell_width, cell_height);
    }

    /// Title set by the program, if any
    pub fn title(&self) -> Option<String> {
        match self.terminal.title() {
            Ok(title) if !title.is_empty() => Some(title.to_string()),
            _ => None,
        }
    }

    /// The pane's current directory: the one the shell reported via OSC 7 (also works over ssh) or,
    /// if it reported nothing, the foreground process's directory according to the OS (macOS and
    /// Linux), so it works with any shell without integration
    pub fn current_dir(&self) -> Option<PathBuf> {
        terminal_dir(&self.terminal).or_else(|| self.io.foreground_dir())
    }

    pub fn kill(&mut self) {
        self.io.kill();
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        // Releases the gesture's pins in the terminal and kills the shell if it is still alive
        self.gesture.reset(&self.terminal);
        self.io.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libghostty_vt::RenderState;

    type Rgb = (u8, u8, u8);

    /// What the renderer sees. The `RenderState` is persistent, as in the app: this checks that
    /// theme changes (and especially removals) are reflected in an existing one.
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

        // Background only: libghostty would ignore it unless the foreground were set too
        apply_theme(
            &mut terminal,
            &config_from(r##"return { colors = { background = "#1e1e2e" } }"##),
        );
        let themed = seen(&mut state, &terminal);
        assert_eq!(themed.1, (0x1e, 0x1e, 0x2e));
        assert_eq!(
            themed.0, factory.0,
            "the foreground keeps the factory color"
        );

        // Custom palette and cursor
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

        // Removing everything (reloading an empty config) restores the factory look
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
        // Not PowerShell, or no program (default shell)
        for program in [Some("cmd.exe"), Some("lazygit"), Some("nu"), None] {
            let original = shell(program, &["/k"], true);
            assert_eq!(with_integration(original.clone()), original, "{program:?}");
        }
        // Disabled
        let off = shell(Some("pwsh"), &["-NoLogo"], false);
        assert_eq!(with_integration(off.clone()), off);
        // Already includes a command or a file: leave it alone
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
        // `-ExecutionPolicy` starts with -e but is not a command
        assert!(
            with_integration(shell(Some("pwsh"), &["-ExecutionPolicy", "Bypass"], true))
                .args
                .len()
                > 2
        );
    }

    #[test]
    fn the_powershell_script_survives_the_windows_command_line() {
        assert!(!PWSH_OSC7.contains('\n'), "a single line");
        assert!(!PWSH_OSC7.contains('"'), "no double quotes");
        assert!(
            !PWSH_OSC7.ends_with('\\'),
            "a trailing backslash would swallow the quote that closes the argument"
        );
        // The original `prompt` function is called first: starship reads `$?` on its first line
        let call = PWSH_OSC7
            .find("$text = & $global:__terminal_prompt")
            .unwrap();
        let first_statement_in_prompt = PWSH_OSC7.find("function global:prompt {").unwrap();
        assert!(call > first_statement_in_prompt);
        assert!(
            !PWSH_OSC7[first_statement_in_prompt..call].contains(';'),
            "nothing between the entry and the call"
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
            osc7_path("file://my-pc/home/me").as_deref(),
            Some("/home/me")
        );
        assert_eq!(
            osc7_path("file:///home/my%20folder/%C3%B1").as_deref(),
            Some("/home/my folder/ñ")
        );
        assert_eq!(osc7_path("http://example.com/x"), None);
        assert_eq!(osc7_path("file://no-path"), None);
        assert_eq!(osc7_path("file:///bad%2"), None);
        assert_eq!(osc7_path("file:///bad%zz"), None);
        assert_eq!(osc7_path("file:///%ff"), None, "not UTF-8");
    }

    #[test]
    fn a_plain_absolute_path_is_accepted_as_osc_9_9_does() {
        let absolute = if cfg!(windows) {
            r"C:\Users\me"
        } else {
            "/home/me"
        };
        assert_eq!(reported_path(absolute).as_deref(), Some(absolute));
        assert_eq!(
            reported_path(&format!("\"{absolute}\"")).as_deref(),
            Some(absolute)
        );
        assert_eq!(reported_path("relative/path"), None);
        assert_eq!(reported_path(""), None);
        // An invalid URI is not interpreted as a path
        assert_eq!(reported_path("file://no-path"), None);
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
            osc7_path("file:///C:/Users/me").as_deref(),
            Some("C:/Users/me")
        );
        assert_eq!(
            osc7_path("file://PC/C:/Users/me%20x").as_deref(),
            Some("C:/Users/me x")
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
        assert_eq!(terminal_dir(&terminal), None, "no directory without OSC 7");
        // cargo's test directory exists on every platform
        let here = std::env::current_dir().unwrap();
        let uri = if cfg!(windows) {
            format!("file:///{}", here.display().to_string().replace('\\', "/"))
        } else {
            format!("file://{}", here.display())
        };
        terminal.vt_write(format!("\x1b]7;{uri}\x07").as_bytes());
        assert_eq!(terminal_dir(&terminal).as_deref(), Some(here.as_path()));
        // A directory that does not exist is not used
        terminal.vt_write(b"\x1b]7;file:///does/not/really/exist\x07");
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

        // Removing the custom palette restores the factory colors
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
