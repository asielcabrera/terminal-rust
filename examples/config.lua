-- Example configuration. Copy it to:
--   Windows : %APPDATA%\terminal\config.lua
--   Others  : ~/.config/terminal/config.lua  (or $XDG_CONFIG_HOME/terminal/config.lua)
-- or point to another file with the TERMINAL_CONFIG environment variable.
--
-- The file is watched: saving it live-reloads the font, size, padding, colors, cursor, title,
-- keys and hooks. If it has an error, the previous configuration is kept and the window title
-- says so (details go to stderr). Scrollback, initial size and shell are only read at startup.
--
-- Everything is optional. An unknown field is an error (so a `font_sise` is not silently ignored).
-- It is plain Lua: you can use variables, conditionals and `require` other files in this folder.
-- WARNING: this file runs code with full access to `io` and `os`; edit it with the same care as
-- a script. A script that takes too long (an infinite loop) is aborted: 2 s when loading and
-- 250 ms for each key or hook call.

-- Available in this file: terminal.os ("windows", "linux", "macos"), terminal.config_dir,
-- terminal.log("message") to write to stderr, and terminal.action.* (see below).
local size = 16
if terminal.os == "macos" then size = 14 end

-- Each system ships different fonts: a family that is not installed stops the terminal from starting
local family, fallback = "Consolas", { "Symbols Nerd Font Mono", "Segoe UI Symbol", "Microsoft YaHei", "Malgun Gothic", "Segoe UI Emoji" }
if terminal.os == "macos" then
  family, fallback = "Menlo", { "Symbols Nerd Font Mono", "Apple Symbols", "STIX Two Math", "PingFang SC", "Apple SD Gothic Neo" }
elseif terminal.os == "linux" then
  family, fallback = "DejaVu Sans Mono", { "Symbols Nerd Font Mono", "DejaVu Sans" }
end

return {
  font = {
    -- Family name as the system sees it. Variable fonts are not supported.
    family = family,
    size = size,                -- logical pixels (scaled by the monitor's DPI)
    -- Families to try, in order, for characters the main family does not cover.
    --
    -- Nerd Fonts (icons and Powerline): characters in the Private Use Area (U+E000..U+F8FF and
    -- U+F0000..U+FFFFD) are looked up in a Nerd Font first, even if another family in the list
    -- has an old glyph at that code point. It works in two ways:
    --   * as the main font: family = "JetBrainsMono Nerd Font Mono" (or whichever you have)
    --   * as a fallback: a symbols-only one, "Symbols Nerd Font Mono", which is already in the
    --     default list. If it is not installed it is skipped and any Nerd Font on the system is used.
    -- Setting `fallback` replaces the entire default list.
    fallback = fallback,
  },

  padding = 6,                   -- space around the text, in logical pixels

  cursor = {
    style = "block",             -- block | bar | underline
    -- Blinking: off by default. Even if you leave it false, a program that requests it (DECSCUSR,
    -- e.g. vim with a blinking cursor) makes the cursor blink. It stays solid while you type or
    -- output arrives, and an unfocused window shows a hollow, non-blinking cursor.
    blink = false,
    blink_ms = 530,              -- half cycle (visible or hidden), between 100 and 5000
  },

  colors = {
    -- "#rrggbb", "#rgb" or an X11 name ("tomato").
    foreground = "#d4d4d4",
    background = "#1e1e2e",
    cursor = "#f5e0dc",
    -- Selection. If unset, selected text inverts the colors of each cell.
    -- selection_foreground = "#1e1e2e",
    -- selection_background = "#f9e2af",
    -- Palette colors starting at index 0: the first 16 are the ANSI colors and the 17th to 256th
    -- (indices 16 to 255 in programs) the 256-color palette. Anything not given keeps its default.
    ansi = {
      "#45475a", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#bac2de",
      "#585b70", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#a6adc8",
    },
  },

  shell = {
    -- Without `program`, the system's default shell is used. `args` requires `program`.
    -- PowerShell only on Windows; elsewhere the system shell ($SHELL) is used.
    program = terminal.os == "windows" and "pwsh" or nil,
    args = terminal.os == "windows" and { "-NoLogo" } or nil,
    -- cwd = "C:/Users/me/projects",
    env = { EDITOR = "nvim" },
    -- PowerShell does not report its current directory (its `cd` does not change the process
    -- directory). With this it is launched with `-NoExit -Command` and a script that wraps your
    -- prompt (starship's, the built-in one...) to report it via OSC 7 on every prompt, so splits
    -- and floats start where you are. It does not change what you see. Ignored if `args` already
    -- contains `-Command`, `-File` or `-EncodedCommand`, or if the program is not pwsh/powershell.
    -- Startup only.
    integration = true,
  },

  window = {
    -- Fixed title. If removed, the window shows the title set by the program.
    -- title = "terminal",
    -- Initial size in cells. It is a request: a tiling window manager may ignore it.
    -- If unset, the window gets whatever size the system gives it.
    -- columns = 100,
    -- rows = 30,

    -- Window frame: "full" (title bar with its buttons, like any window) or "none" (no bar or
    -- border: just the terminal). Applied live when the file is saved.
    -- Without a title bar there is nothing to grab the window by, so:
    --   * hold `drag_modifiers` and drag with the left button to move it (Alt by default)
    --   * the edges (6 px) resize it: the cursor changes to an arrow when hovering over them
    -- To close it, type `exit` in the shell, or bind a key to terminal.action.Quit (there is none
    -- by default). A tiling window manager can also move and close it.
    decorations = "full",
    -- Combination of CTRL, SHIFT, ALT and SUPER joined with |. Only used with decorations = "none",
    -- and then it cannot equal mouse.rectangle_modifiers or be empty.
    drag_modifiers = "ALT",
    -- Window corners: true leaves them as the system decides (rounded on Windows 11), false makes
    -- them square. Only affects Windows 11; applied live.
    rounded_corners = true,
  },

  mouse = {
    -- Selecting with the mouse copies to the clipboard on release. If set to false, copy with
    -- terminal.action.Copy (Ctrl+Shift+C or Ctrl+Insert by default).
    copy_on_select = true,
    -- Right click: "paste" pastes the clipboard, "none" does nothing.
    right_click = "paste",
    -- Lines per wheel notch (1..100). Touchpads scroll in pixels.
    scroll_lines = 3,
    -- Dragging with these modifiers selects a rectangle instead of whole lines.
    -- Cannot be empty (dragging without modifiers already selects).
    rectangle_modifiers = "CTRL|ALT",
  },
  -- Double click selects a word and triple click the line. If a program requests the mouse (vim
  -- with `mouse=a`, tmux, htop...) it receives clicks, drags and the wheel instead of selecting;
  -- hold SHIFT to select anyway. On the alternate screen without mouse reporting (less, man) the
  -- wheel moves the cursor with the arrow keys.

  -- Panes: the window splits into several terminals (splits) and can have floating panes on
  -- top. Each pane runs its own shell.
  --   * Splits are resized by dragging the line that separates them.
  --   * A float is moved by dragging the margin of its frame (or Alt + drag from anywhere, the
  --     same gesture that moves the window) and resized by its edges and corners.
  --   * A click focuses the pane. The focused one gets a frame and the others have dimmed text.
  --   * A pane whose shell exits is closed (with `on_shell_exit = "close"`); the last one in
  --     the tiled layout closes the window.
  panes = {
    -- border_color = "#44475a",   -- dividers and float borders (default: dimmed text)
    -- focus_color = "#f8f8f2",    -- frame of the focused pane (default: the cursor color)
    -- leader_color = "#f1fa8c",   -- frame while the leader waits (default: ANSI yellow)
    inactive_dim = 0.35,           -- how much unfocused panes' text is dimmed: 0 to 1
    -- A new pane (a split or a float) starts in the active pane's directory. The terminal knows
    -- it because the shell reports it with OSC 7 (`ESC ] 7 ; file:///C:/path ST`) or with
    -- OSC 9;9 (`ESC ] 9 ; 9 ; C:\path ST`, the ConEmu and Windows Terminal one). PowerShell does
    -- not do it on its own: see `shell.integration`. With cmd, use `PROMPT $E]9;9;$P$E\$P$G`. On
    -- macOS and Linux nothing is needed: if the shell does not report it, the system is asked for
    -- the foreground program's directory (zsh, bash, fish...). If nothing provides it, `shell.cwd`
    -- is used.
    inherit_cwd = true,
    float_width = 0.7,             -- size of a new float, as a fraction of the window
    float_height = 0.7,
  },

  -- tmux-style leader: after pressing it, the next key is looked up among the bindings with
  -- `mods = "LEADER"` (below and in the default list). While it waits, the pane frame changes
  -- color; Escape or an unbound key cancels it, and pressing it twice sends the control
  -- character to the program (Ctrl+B Ctrl+B sends ^B, for a nested tmux).
  -- Defaults to Ctrl+B with a 1500 ms timeout. `leader = false` disables it.
  leader = { key = "b", mods = "CTRL", timeout_ms = 1500 },

  scrollback = 10000,            -- lines of history

  on_shell_exit = "close",       -- close: closes the pane (and the window with the last one) | hold: keeps it with a notice

  -- Keys. `key` is a character ("v") or a name: Enter, Tab, Space, Escape, Backspace, Delete,
  -- Insert, Home, End, PageUp, PageDown, ArrowUp/Down/Left/Right, F1..F24. `mods` combines CTRL,
  -- SHIFT, ALT and SUPER with |, and must match exactly (including SHIFT if the character needs
  -- it, like "+" on a US keyboard). A bound key does not reach the shell.
  --
  -- `action` is a native action (terminal.action.X, or its name in quotes if it takes no
  -- parameters) or a Lua function that receives `ctx`.
  --
  -- Bound by default: Ctrl+Shift+C and Ctrl+Insert = Copy, Ctrl+Shift+V and
  -- Shift+Insert = Paste, Shift+PageUp / Shift+PageDown = one page of history. And, after the
  -- leader: % splits right, " splits down, x closes the pane, z zooms or restores, o next pane,
  -- arrows or h/j/k/l move focus, Ctrl+arrows resize by 1 cell and Alt+arrows by 5, w hides or
  -- shows the floats (creating one if there are none), W opens another float and Shift+arrows
  -- move the float. A key of yours with the same combination replaces the default one. To keep
  -- only yours:
  -- disable_default_keys = true,
  --
  -- Selection actions: Copy, SelectAll, ClearSelection.
  -- Pane actions: SplitRight, SplitDown, ClosePane, ToggleZoom, FocusNext, FocusPrevious,
  --   FocusPane("left"|"right"|"up"|"down"), ResizePane("left", 5), MovePane("down", 2) (floats
  --   only), ToggleFloats and NewFloat{ program = "lazygit", args = {...}, width = 0.9,
  --   height = 0.9, cwd = "C:/projects" } (all optional; with `program` the float closes when
  --   the program exits, like a popup).
  keys = {
    -- Panes without and with the leader (mods = "LEADER" or "LEADER|SHIFT"...)
    { key = "Enter", mods = "CTRL",       action = terminal.action.SplitDown },
    { key = "Enter", mods = "CTRL|SHIFT", action = terminal.action.SplitRight },
    { key = "v",     mods = "LEADER",     action = terminal.action.SplitRight },
    { key = "g",     mods = "LEADER",     action = terminal.action.NewFloat({ program = "lazygit", width = 0.9, height = 0.9 }) },
    { key = "v",      mods = "CTRL|SHIFT", action = terminal.action.Paste },
    { key = "PageUp", mods = "SHIFT",      action = terminal.action.ScrollPages(-1) },  -- negative scrolls up
    { key = "PageDown", mods = "SHIFT",    action = terminal.action.ScrollPages(1) },
    { key = "Home",   mods = "SHIFT",      action = terminal.action.ScrollToTop },
    { key = "End",    mods = "SHIFT",      action = terminal.action.ScrollToBottom },
    { key = "=",      mods = "CTRL",       action = terminal.action.IncreaseFontSize },
    { key = "-",      mods = "CTRL",       action = terminal.action.DecreaseFontSize },
    { key = "0",      mods = "CTRL",       action = terminal.action.ResetFontSize },
    { key = "F11",    mods = "",           action = terminal.action.ToggleFullscreen },
    { key = "r",      mods = "CTRL|SHIFT", action = terminal.action.ReloadConfig },
    { key = "l",      mods = "CTRL|ALT",   action = terminal.action.SendString("clear\r") },
    -- A custom function: whatever it wants to do with the terminal, it asks for through `ctx`.
    {
      key = "k", mods = "CTRL|ALT",
      action = function(ctx)
        ctx:send_text("git status\r")
      end,
    },
  },

  -- Hooks. They receive `ctx` and, depending on the event, more data. `ctx` has:
  --   ctx:send_text(text)     writes text to the shell
  --   ctx:log(message)        writes to stderr
  --   ctx:perform(action)     runs a native action (terminal.action.X)
  -- While a hook runs its commands no other hooks fire (this prevents loops, e.g. a
  -- `config-reloaded` hook that asks to reload the configuration).
  events = {
    ["title-changed"]   = function(ctx, title) ctx:log("title: " .. title) end,
    ["bell"]            = function(ctx) ctx:log("bell") end,
    ["shell-exit"]      = function(ctx) ctx:log("the shell exited") end,
    ["config-reloaded"] = function(ctx) ctx:log("configuration reloaded") end,
  },
}
