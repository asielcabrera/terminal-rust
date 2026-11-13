//! A pane: a libghostty terminal with its PTY, the threads that serve it, and the mouse
//! selection state. The app has one or more, arranged by `layout`.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};

use libghostty_vt::kitty::graphics;
use libghostty_vt::mouse;
use libghostty_vt::selection::gesture::Gesture;
use libghostty_vt::style::{Palette, PaletteIndex, RgbColor};
use libghostty_vt::terminal::{
    ConformanceLevel, CursorStyle as VtCursorStyle, DeviceAttributeFeature, DeviceAttributes,
    DeviceType, PrimaryDeviceAttributes, SecondaryDeviceAttributes, TertiaryDeviceAttributes,
};
use libghostty_vt::{RenderState, Terminal, TerminalOptions};
use portable_pty::CommandBuilder;

use crate::LuaEvent;
use crate::config::{self, Config, ShellConfig};
use crate::layout::{PaneId, Rect};
use crate::pane_io::PaneIo;

/// A PNG decoder for the kitty graphics protocol: it decodes into the 8-bit RGBA pixels
/// libghostty expects, expanding palettes and grayscales and stripping 16-bit channels. The
/// output buffer is reused across images so a burst of transmits does not reallocate every time.
#[derive(Default)]
struct PngDecoder {
    buf: Vec<u8>,
}

impl graphics::DecodePng for PngDecoder {
    fn decode_png<'alloc>(
        &mut self,
        alloc: &'alloc libghostty_vt::alloc::Allocator<'_>,
        data: &[u8],
    ) -> Option<graphics::DecodedImage<'alloc>> {
        match self.decode(alloc, data) {
            Ok(image) => Some(image),
            // The transport answers the transmit with a kitty error (when the program asked for
            // one), but it gives no cause on our side: log it so broken images are diagnosable
            Err(error) => {
                eprintln!("[terminal] kitty PNG decode failed: {error}");
                None
            }
        }
    }
}

impl PngDecoder {
    fn decode<'alloc>(
        &mut self,
        alloc: &'alloc libghostty_vt::alloc::Allocator<'_>,
        data: &[u8],
    ) -> Result<graphics::DecodedImage<'alloc>, String> {
        let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
        decoder.set_transformations(png::Transformations::ALPHA | png::Transformations::STRIP_16);

        let mut frame = decoder
            .read_info()
            .map_err(|error| format!("reading PNG header: {error}"))?;
        let size = frame
            .output_buffer_size()
            .ok_or("PNG buffer size unknown".to_string())?;
        // png checks the buffer by LENGTH, not capacity: resize it to the frame's size
        if size > self.buf.capacity() {
            self.buf.reserve(size - self.buf.capacity());
        }
        self.buf.resize(size, 0);

        let info = frame
            .next_frame(&mut self.buf)
            .map_err(|error| format!("decoding PNG pixels: {error}"))?;
        let mut bytes = libghostty_vt::alloc::Bytes::new_with_alloc(alloc, info.buffer_size())
            .map_err(|error| format!("allocating a kitty image buffer: {error}"))?;
        bytes.copy_from_slice(&self.buf[..info.buffer_size()]);
        frame
            .finish()
            .map_err(|error| format!("finishing PNG decode: {error}"))?;

        Ok(graphics::DecodedImage {
            width: info.width,
            height: info.height,
            data: bytes,
        })
    }
}

/// Registers the PNG decoder the kitty graphics protocol needs to accept PNG data. It is a
/// thread-local callback inside libghostty: call it on the window thread, before its panes are
/// created and fed.
pub fn install_png_decoder() {
    if let Err(error) = graphics::set_png_decoder(Some(Box::<PngDecoder>::default())) {
        eprintln!("[terminal] kitty PNG decoding unavailable: {error}");
    }
}

/// Enables image storage on the terminal: without a non-zero cap the kitty graphics protocol is
/// off and `ESC _G` sequences are ignored. A cap of 0 (configurable) also disables it. Also ties
/// the APC parser's per-payload byte cap to the same limit: the factory default is 65 MiB, which
/// silently drops legitimate photo transmits on configurations with a lower cap, and ours keeps
/// the buffering worst case bounded by the storage limit the user already accepted.
pub(crate) fn enable_graphics(terminal: &mut Terminal<'static, 'static>, config: &Config) {
    let _ = terminal.set_kitty_image_storage_limit(config.graphics.storage_limit);
    let _ = terminal.set_apc_max_bytes_kitty(Some(config.graphics.storage_limit as usize));
}

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
pub(crate) fn with_integration(mut shell: ShellConfig) -> ShellConfig {
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

/// What a pane is built from: the server-allocated pane id, the grid size it was spawned at,
/// the fallback notice, and the connection to the program behind it (a [`ServerBlock`]
/// in-process, an `IpcBlock` over the wire). The pane never knows which kind it got.
pub struct PaneSeed {
    pub pane: PaneId,
    pub cols: u16,
    pub rows: u16,
    pub notice: Option<String>,
    pub io: Box<dyn PaneIo>,
}

impl Pane {
    /// Builds the window-side pane around a block the server already started: a layout op
    /// reported it (a `CreatedPane` locally, a wire pane over IPC), and its [`PaneSeed`] was
    /// assembled here. `notice` is written into the terminal before anything else (e.g. the
    /// configuration error of the first pane).
    pub fn from_source(
        config: &Config,
        source: PaneSeed,
        notice: Option<(&str, &str)>,
        lua_events: Sender<(PaneId, LuaEvent)>,
    ) -> Result<Pane, String> {
        let PaneSeed {
            pane: id,
            cols,
            rows,
            notice: shell_notice,
            io,
        } = source;
        let mut terminal = Terminal::new(TerminalOptions {
            cols,
            rows,
            max_scrollback: config.scrollback,
        })
        .map_err(vt_error)?;
        apply_theme(&mut terminal, config);
        enable_graphics(&mut terminal, config);
        if let Some((tag, message)) = notice {
            write_notice(&mut terminal, tag, message);
        }
        if let Some(message) = &shell_notice {
            write_notice(&mut terminal, "shell", message);
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

        Ok(Pane {
            id,
            terminal,
            render_state: RenderState::new().map_err(vt_error)?,
            rect: Rect::new(0, 0, 0, 0),
            grid: (cols, rows),
            io,
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
    use libghostty_vt::kitty::graphics::DecodePng as _;

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

    /// A 1x1 red opaque PNG, base64: the exact payload libghostty-vt's own documentation uses
    const RED_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

    /// The kitty APC transmit-and-display payload for a tiny PNG:
    /// `ESC _G a=T,f=100,q=1;<base64 PNG data> ESC \`
    fn kitty_apc(png_base64: &str) -> String {
        format!("\x1b_Ga=T,f=100,q=1;{png_base64}\x1b\\")
    }

    #[test]
    fn a_kitty_apc_image_is_decoded_and_stored_when_graphics_are_enabled() {
        install_png_decoder();
        let mut terminal: Terminal<'static, 'static> = Terminal::new(TerminalOptions {
            cols: 10,
            rows: 3,
            max_scrollback: 0,
        })
        .unwrap();
        enable_graphics(&mut terminal, &Config::default());
        terminal.resize(10, 3, 8, 16).unwrap();
        terminal.vt_write(kitty_apc(RED_PNG_B64).as_bytes());

        let graphics = terminal.kitty_graphics().unwrap();
        let mut iter = graphics::PlacementIterator::new().unwrap();
        let mut placements = iter.update(&graphics).unwrap();
        let mut seen = 0;
        while let Some(placement) = placements.next() {
            seen += 1;
            let id = placement.image_id().unwrap();
            let image = graphics
                .image(id)
                .unwrap_or_else(|| panic!("image {id} missing"));
            assert_eq!(image.width().unwrap(), 1);
            assert_eq!(image.height().unwrap(), 1);
            assert_eq!(image.format().unwrap(), graphics::ImageFormat::Rgba);
            assert_eq!(image.data().unwrap(), [255, 0, 0, 255]);
            assert!(placement.pixel_size(&image, &terminal).unwrap().width > 0);
        }
        assert_eq!(seen, 1, "the transmitted image is displayed once");
    }

    #[test]
    fn the_kitty_storage_limit_comes_from_the_config() {
        let mut terminal: Terminal<'static, 'static> = Terminal::new(TerminalOptions {
            cols: 10,
            rows: 3,
            max_scrollback: 0,
        })
        .unwrap();
        enable_graphics(&mut terminal, &Config::default());
        assert_eq!(
            terminal.kitty_image_storage_limit().unwrap(),
            512 * 1024 * 1024
        );

        let configured = config_from("return { graphics = { storage_limit = 4096 } }");
        enable_graphics(&mut terminal, &configured);
        assert_eq!(terminal.kitty_image_storage_limit().unwrap(), 4096);
    }

    #[test]
    fn a_zero_storage_limit_turns_the_protocol_off_and_stores_nothing() {
        // libghostty's factory default cap is 10 MiB, so images work without configuration; the
        // `0` the configuration accepts is the documented way of turning the protocol off
        install_png_decoder();
        let mut terminal: Terminal<'static, 'static> = Terminal::new(TerminalOptions {
            cols: 10,
            rows: 3,
            max_scrollback: 0,
        })
        .unwrap();
        let disabled = config_from("return { graphics = { storage_limit = 0 } }");
        enable_graphics(&mut terminal, &disabled);
        assert_eq!(terminal.kitty_image_storage_limit().unwrap(), 0);
        terminal.resize(10, 3, 8, 16).unwrap();
        terminal.vt_write(kitty_apc(RED_PNG_B64).as_bytes());

        let graphics = terminal.kitty_graphics().unwrap();
        // The transmit still stamps the storage (the disabling itself counts as a mutation), but
        // nothing is kept and nothing is drawn
        let mut iter = graphics::PlacementIterator::new().unwrap();
        let mut placements = iter.update(&graphics).unwrap();
        assert!(placements.next().is_none(), "no placement rendered");
        assert!(graphics.image(1).is_none(), "the image is not stored");
    }

    #[test]
    fn a_large_photo_png_transmits_decodes_and_places() {
        use base64::Engine as _;
        install_png_decoder();
        let mut terminal: Terminal<'static, 'static> = Terminal::new(TerminalOptions {
            cols: 40,
            rows: 24,
            max_scrollback: 0,
        })
        .unwrap();
        enable_graphics(&mut terminal, &Config::default());
        terminal.resize(40, 24, 8, 16).unwrap();

        // 800x800 RGBA noise: a worst-case >1 MB photo-like PNG (2.5 MB, base64 3.3 MB)
        let png = noise_png(800, 800);
        assert!(
            png.len() > 1024 * 1024,
            "a large photo, {} bytes",
            png.len()
        );
        let b64 = base64::engine::general_purpose::STANDARD.encode(png);
        terminal.vt_write(format!("\x1b_Ga=T,f=100,q=1;{b64}\x1b\\").as_bytes());

        // It is stored whole, decoded to RGBA, and placed at its natural size: the transmit is
        // not silently dropped by the APC parser and the decoder handles it
        let graphics = terminal.kitty_graphics().unwrap();
        let mut iter = graphics::PlacementIterator::new().unwrap();
        let mut placements = iter.update(&graphics).unwrap();
        let placement = placements.next().expect("the image is placed");
        assert!(!placement.is_virtual().unwrap());
        let image = graphics.image(placement.image_id().unwrap()).unwrap();
        assert_eq!(
            (image.width().unwrap(), image.height().unwrap()),
            (800, 800)
        );
        assert_eq!(image.data().unwrap().len(), 800 * 800 * 4, "fully decoded");
        assert_eq!(placement.pixel_size(&image, &terminal).unwrap().width, 800);
        assert!(placements.next().is_none(), "one placement");
    }

    #[test]
    fn a_payload_larger_than_the_storage_limit_is_dropped_at_the_apc_parser() {
        use base64::Engine as _;
        install_png_decoder();
        let mut terminal: Terminal<'static, 'static> = Terminal::new(TerminalOptions {
            cols: 40,
            rows: 24,
            max_scrollback: 0,
        })
        .unwrap();
        // 975x975 RGBA noise: decoded 3.8 MB (fits the 4 MB limit), base64 ~5.2 MB (over the
        // APC cap the limit implies)
        let configured = config_from("return { graphics = { storage_limit = 4000000 } }");
        enable_graphics(&mut terminal, &configured);
        terminal.resize(40, 24, 8, 16).unwrap();
        let png = noise_png(975, 975);
        let b64 = base64::engine::general_purpose::STANDARD.encode(png);
        assert!(
            b64.len() > 4_000_000,
            "the payload exceeds the cap: {} bytes",
            b64.len()
        );
        terminal.vt_write(format!("\x1b_Ga=T,f=100,q=1;{b64}\x1b\\").as_bytes());

        let graphics = terminal.kitty_graphics().unwrap();
        let mut iter = graphics::PlacementIterator::new().unwrap();
        let mut placements = iter.update(&graphics).unwrap();
        assert!(
            placements.next().is_none(),
            "the oversized payload is dropped"
        );
    }

    #[test]
    fn a_small_image_still_stores_under_a_tight_apc_cap() {
        install_png_decoder();
        let mut terminal: Terminal<'static, 'static> = Terminal::new(TerminalOptions {
            cols: 10,
            rows: 3,
            max_scrollback: 0,
        })
        .unwrap();
        let configured = config_from("return { graphics = { storage_limit = 4096 } }");
        enable_graphics(&mut terminal, &configured);
        terminal.resize(10, 3, 8, 16).unwrap();
        terminal.vt_write(kitty_apc(RED_PNG_B64).as_bytes());

        let graphics = terminal.kitty_graphics().unwrap();
        let mut iter = graphics::PlacementIterator::new().unwrap();
        let mut placements = iter.update(&graphics).unwrap();
        assert!(
            placements.next().is_some(),
            "small losses nothing to the cap"
        );
    }

    /// Encodes a `width` x `height` RGBA PNG full of pseudo-random pixels (deflate cannot
    /// compress it, so its size is a worst case for both the parser and the decoder)
    fn noise_png(width: u32, height: u32) -> Vec<u8> {
        let mut words = vec![0u32; (width as usize) * (height as usize)];
        let mut state = 0x12345678u64;
        for word in &mut words {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *word = state as u32;
        }
        let pixels: Vec<u8> = words.iter().copied().flat_map(u32::to_le_bytes).collect();
        let mut encoded = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut encoded, width, height);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().unwrap();
            writer.write_image_data(&pixels).unwrap();
        }
        encoded
    }

    #[test]
    fn the_png_decoder_produces_rgba8_pixels_and_rejects_other_data() {
        // Hand-encode a 1x2 opaque gradient PNG with the same crate the decoder uses
        let mut encoded = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut encoded, 1, 2);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer
                .write_image_data(&[10, 20, 30, 40, 200, 150, 100, 50])
                .unwrap();
        }
        let alloc = libghostty_vt::alloc::Allocator::GLOBAL;
        let decoded = PngDecoder::default().decode_png(&alloc, &encoded).unwrap();
        assert_eq!(decoded.width, 1);
        assert_eq!(decoded.height, 2);
        assert_eq!(&decoded.data[..], &[10, 20, 30, 40, 200, 150, 100, 50]);

        assert!(
            PngDecoder::default()
                .decode_png(&alloc, b"not a png")
                .is_none(),
            "invalid data is rejected (kitty answers the transmit with an error)"
        );
    }
}
