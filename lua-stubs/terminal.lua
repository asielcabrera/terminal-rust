---@meta
-- Type stub for `local terminal = require("terminal")` — the runtime table is injected by the
-- app (package.loaded.terminal); this file exists for lua-language-server completion.

---@class terminal-os
---@field os '"windows"'|'"linux"'|'"macos"'

---@class terminal-ctx
---@field send_text fun(text: string) writes text to the shell of the pane that fired the event
---@field log fun(message: string) writes to stderr
---@field perform fun(action: terminal-action) runs a native action (terminal.action.X)
---@field event { name: string, payload: table, depth: integer }
---@field emit fun(name: string, payload: table?) emits an event to the bus

---@class terminal-action
---@field SendString fun(text: string): table
---@field ScrollLines fun(amount: integer): table
---@field ScrollPages fun(pages: integer): table
---@field FocusPane fun(dir: string): table
---@field ResizePane fun(dir: string, amount: integer): table
---@field MovePane fun(dir: string, amount: integer): table
---@field SplitRight table
---@field SplitDown table
---@field ClosePane table
---@field ToggleZoom table
---@field FocusNext table
---@field FocusPrevious table
---@field ToggleFloats table
---@field NewFloat fun(options: { program: string?, args: string[]?, width: number?, height: number?, cwd: string?, env: table<string, string>? }): table
---@field ReloadConfig table
---@field Quit table
---@field ToggleFullscreen table
---@field Paste table
---@field Copy table
---@field SelectAll table
---@field ClearSelection table
---@field ScrollToTop table
---@field ScrollToBottom table
---@field IncreaseFontSize table
---@field DecreaseFontSize table
---@field ResetFontSize table

---@class terminal-mouse
---@field copy_on_select boolean selecting with the mouse copies on release
---@field right_click '"paste"'|'"none"'
---@field scroll_lines integer lines per wheel notch (1..100)
---@field smooth_scroll boolean sub-line wheel animation (default true)
---@field rectangle_modifiers string modifiers for rectangle selection

---@class terminal
---@field os terminal-os
---@field mouse terminal-mouse
---@field config_dir string
---@field log fun(message: string)
---@field action terminal-action
---@field on fun(pattern: string, handler: fun(ctx: terminal-ctx, payload: table), priority: integer?) registers an event handler (exact name or glob pattern with * and ?)
---@field emit fun(name: string, payload: table?) emits an event to the bus
terminal = {}
