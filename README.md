# terminal

[![CI](https://github.com/cabrerasiel/terminal/actions/workflows/ci.yml/badge.svg)](https://github.com/cabrerasiel/terminal/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

A GPU-accelerated, cross-platform terminal emulator written in Rust. Terminal emulation is
handled by [libghostty-vt](https://github.com/ghostty-org/ghostty) (the VT core of Ghostty),
rendering by [wgpu](https://wgpu.rs), and configuration by a hot-reloaded Lua file.

Runs on **macOS**, **Linux** and **Windows**.

## Features

- **GPU rendering** with wgpu (Metal, Vulkan, DirectX 12) and antialiased glyphs.
- **Ghostty's VT engine**: accurate escape-sequence handling, 256 colors and true color,
  alternate screen, mouse reporting and scrollback.
- **Splits and floating panes** with a tmux-style leader key (`Ctrl+B` by default): split,
  zoom, resize, move focus, and open floating panes for tools such as `lazygit`.
- **New panes open where you are**: the working directory comes from OSC 7 / OSC 9;9 when the
  shell reports it, and is queried from the OS otherwise (macOS and Linux), so it works with
  any shell out of the box.
- **Lua configuration** with live reload: fonts, colors, cursor, padding, shell, window,
  mouse, key bindings and event hooks (`title-changed`, `bell`, `shell-exit`,
  `config-reloaded`).
- **Font fallback** across system fonts, with Nerd Font icons and Powerline glyphs routed to an
  installed Nerd Font.
- **Mouse selection**: word and line selection, rectangular selection, copy on select.
- **Borderless mode** with drag-to-move and edge resizing.

## Installation

### Prebuilt binaries

Download the archive for your platform from the
[Releases](https://github.com/cabrerasiel/terminal/releases) page:

| Platform | Archive |
| --- | --- |
| macOS (Apple Silicon) | `terminal-<version>-aarch64-apple-darwin.tar.gz` |
| Linux (x86_64) | `terminal-<version>-x86_64-unknown-linux-gnu.tar.gz` |
| Windows (x86_64) | `terminal-<version>-x86_64-pc-windows-msvc.zip` |

### Build from source

Requirements:

- [Rust](https://rustup.rs) (stable)
- [Zig](https://ziglang.org) **0.15.2** exactly. `libghostty-vt` is built with Zig and rejects
  other versions.

```sh
git clone https://github.com/cabrerasiel/terminal.git
cd terminal
cargo run --release
```

Platform notes:

- **macOS**: install Zig with `brew install zig@0.15` (it is keg-only, so add
  `$(brew --prefix zig@0.15)/bin` to your `PATH` or run `brew link --force zig@0.15`).
  If linking fails with `Argument list too long`, Xcode is using a non-default toolchain:
  select the default one in Xcode and run `xcrun --kill-cache`.
- **Linux** (Debian/Ubuntu):
  ```sh
  sudo apt-get install libxkbcommon-dev libwayland-dev libx11-dev libxcursor-dev \
    libxrandr-dev libxi-dev libfontconfig1-dev
  ```
- **Windows**: use the MSVC toolchain. The application icon is embedded in the executable at
  build time.

## Configuration

The configuration is a Lua script that returns a table. Every option is optional. It is read
from the first of:

1. the path in the `TERMINAL_CONFIG` environment variable
2. Windows: `%APPDATA%\terminal\config.lua`
3. macOS and Linux: `$XDG_CONFIG_HOME/terminal/config.lua`, or `~/.config/terminal/config.lua`

Changes are applied as soon as the file is saved. Start from the documented example, which
lists every option:

```sh
mkdir -p ~/.config/terminal
cp examples/config.lua ~/.config/terminal/config.lua
```

A minimal configuration:

```lua
return {
  font = { family = "JetBrainsMono Nerd Font Mono", size = 14 },
  colors = { background = "#1f1f28", foreground = "#dcd7ba" },
  keys = {
    { key = "Enter", mods = "CTRL|SHIFT", action = terminal.action.SplitRight },
  },
}
```

The script can branch on `terminal.os` (`"windows"`, `"linux"` or `"macos"`) to share one file
across machines.

## Default key bindings

| Keys | Action |
| --- | --- |
| `Ctrl+Shift+C`, `Ctrl+Insert` | Copy |
| `Ctrl+Shift+V`, `Shift+Insert` | Paste |
| `Shift+PageUp` / `Shift+PageDown` | Scroll one page |
| `Ctrl+B` then `%` / `"` | Split right / down |
| `Ctrl+B` then `x` | Close pane |
| `Ctrl+B` then `z` | Zoom or restore pane |
| `Ctrl+B` then `o` | Focus next pane |
| `Ctrl+B` then arrows or `h` `j` `k` `l` | Move focus |
| `Ctrl+B` then `Ctrl+arrows` / `Alt+arrows` | Resize by 1 / 5 cells |
| `Ctrl+B` then `w` / `W` | Toggle floating panes / new floating pane |
| `Ctrl+B` then `Shift+arrows` | Move floating pane |

Pressing the leader twice sends it to the program (for nested tmux). Bindings in your
configuration replace the defaults with the same combination; set
`disable_default_keys = true` to keep only yours.

## Contributing

Contributions are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for the workflow: branch from
`master`, run `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings` and `cargo test`,
and open a pull request with a [Conventional Commits](https://www.conventionalcommits.org)
title. Bug reports and feature requests go to
[Issues](https://github.com/cabrerasiel/terminal/issues).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion
in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above,
without any additional terms or conditions.

## Acknowledgements

- [Ghostty](https://github.com/ghostty-org/ghostty) for libghostty-vt.
- [wgpu](https://wgpu.rs), [winit](https://github.com/rust-windowing/winit),
  [mlua](https://github.com/mlua-rs/mlua) and [portable-pty](https://github.com/wezterm/wezterm).

## Contributors

<!-- readme: contributors -start -->
<table>
	<tbody>
		<tr>
            <td align="center">
                <a href="https://github.com/cabrerasiel">
                    <img src="https://avatars.githubusercontent.com/u/296233108?v=4" width="100;" alt="cabrerasiel"/>
                    <br />
                    <sub><b>Asiel Cabrera</b></sub>
                </a>
            </td>
            <td align="center">
                <a href="https://github.com/asielcabrera">
                    <img src="https://avatars.githubusercontent.com/u/24819851?v=4" width="100;" alt="asielcabrera"/>
                    <br />
                    <sub><b>Asiel Cabrera</b></sub>
                </a>
            </td>
		</tr>
	<tbody>
</table>
<!-- readme: contributors -end -->
