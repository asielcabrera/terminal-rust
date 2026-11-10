-- Ejemplo de configuración. Cópialo a:
--   Windows : %APPDATA%\terminal\config.lua
--   Otros   : ~/.config/terminal/config.lua  (o $XDG_CONFIG_HOME/terminal/config.lua)
-- o apunta a otro archivo con la variable de entorno TERMINAL_CONFIG.
--
-- El archivo se vigila: al guardarlo se recargan en vivo la fuente, el tamaño, el margen, los colores,
-- el cursor, el título, las teclas y los hooks. Si tiene un error se conserva la configuración
-- anterior y el título de la ventana lo avisa (el detalle sale por stderr). Scrollback, tamaño
-- inicial y shell solo se leen al arrancar.
--
-- Todo es opcional. Un campo desconocido es un error (así un `font_sise` no se ignora en silencio).
-- Es Lua normal: puedes usar variables, condicionales y `require` de otros archivos de esta carpeta.
-- OJO: este archivo ejecuta código con acceso total a `io` y `os`; edítalo con el mismo cuidado que
-- un script. Un script que tarde demasiado (un bucle infinito) se aborta: 2 s al cargar y 250 ms
-- por cada llamada a una tecla o un hook.

-- Disponible en el archivo: terminal.os ("windows", "linux", "macos"), terminal.config_dir,
-- terminal.log("mensaje") para escribir en stderr y terminal.action.* (más abajo).
local size = 16
if terminal.os == "macos" then size = 14 end

return {
  font = {
    -- Nombre de la familia tal como la ve el sistema. Las fuentes variables no están soportadas.
    family = "Consolas",
    size = size,                 -- píxeles lógicos (se escala con el DPI del monitor)
    -- Familias a probar, en orden, para los caracteres que la principal no cubre. Para los glifos
    -- de Powerline o los iconos, añade aquí una Nerd Font instalada.
    fallback = { "Segoe UI Symbol", "Microsoft YaHei", "Malgun Gothic", "Segoe UI Emoji" },
  },

  padding = 6,                   -- margen alrededor del texto, en píxeles lógicos

  cursor = {
    style = "block",             -- block | bar | underline
  },

  colors = {
    -- "#rrggbb", "#rgb" o un nombre X11 ("tomato").
    foreground = "#d4d4d4",
    background = "#1e1e2e",
    cursor = "#f5e0dc",
    -- Los primeros colores de la paleta (0-15); el resto queda por defecto. Máximo 16.
    ansi = {
      "#45475a", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#bac2de",
      "#585b70", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#a6adc8",
    },
  },

  shell = {
    -- Si falta `program`, se usa el shell por defecto del sistema. `args` necesita `program`.
    program = "pwsh",
    args = { "-NoLogo" },
    -- cwd = "C:/Users/yo/proyectos",
    env = { EDITOR = "nvim" },
  },

  window = {
    -- Título fijo. Si lo quitas, la ventana muestra el título que pone el programa.
    -- title = "terminal",
    -- Tamaño inicial en celdas. Es una petición: un gestor de ventanas en mosaico puede ignorarla.
    -- Si no los pones, la ventana tiene el tamaño que le dé el sistema.
    -- columns = 100,
    -- rows = 30,
  },

  scrollback = 10000,            -- líneas de historial

  on_shell_exit = "close",       -- close: cierra la ventana | hold: la deja abierta con el aviso

  -- Teclas. `key` es un carácter ("v") o un nombre: Enter, Tab, Space, Escape, Backspace, Delete,
  -- Insert, Home, End, PageUp, PageDown, ArrowUp/Down/Left/Right, F1..F24. `mods` combina CTRL,
  -- SHIFT, ALT y SUPER con |, y tiene que coincidir exactamente (también SHIFT si el carácter lo
  -- necesita, como "+" en un teclado US). Una tecla enlazada no llega al shell.
  --
  -- `action` es una acción nativa (terminal.action.X, o su nombre entre comillas si no lleva
  -- parámetros) o una función de Lua que recibe `ctx`.
  keys = {
    { key = "v",      mods = "CTRL|SHIFT", action = terminal.action.Paste },
    { key = "PageUp", mods = "SHIFT",      action = terminal.action.ScrollPages(-1) },  -- negativo sube
    { key = "PageDown", mods = "SHIFT",    action = terminal.action.ScrollPages(1) },
    { key = "Home",   mods = "SHIFT",      action = terminal.action.ScrollToTop },
    { key = "End",    mods = "SHIFT",      action = terminal.action.ScrollToBottom },
    { key = "=",      mods = "CTRL",       action = terminal.action.IncreaseFontSize },
    { key = "-",      mods = "CTRL",       action = terminal.action.DecreaseFontSize },
    { key = "0",      mods = "CTRL",       action = terminal.action.ResetFontSize },
    { key = "F11",    mods = "",           action = terminal.action.ToggleFullscreen },
    { key = "r",      mods = "CTRL|SHIFT", action = terminal.action.ReloadConfig },
    { key = "l",      mods = "CTRL|ALT",   action = terminal.action.SendString("clear\r") },
    -- Una función propia: lo que quiera hacer con la terminal lo pide por `ctx`.
    {
      key = "k", mods = "CTRL|ALT",
      action = function(ctx)
        ctx:send_text("git status\r")
      end,
    },
  },

  -- Hooks. Reciben `ctx` y, según el evento, más datos. `ctx` tiene:
  --   ctx:send_text(texto)    escribe texto en el shell
  --   ctx:log(mensaje)        escribe en stderr
  --   ctx:perform(accion)     ejecuta una acción nativa (terminal.action.X)
  -- Mientras un hook ejecuta sus órdenes no se disparan otros hooks (evita bucles, por ejemplo un
  -- hook de `config-reloaded` que pida recargar la configuración).
  events = {
    ["title-changed"]   = function(ctx, title) ctx:log("título: " .. title) end,
    ["bell"]            = function(ctx) ctx:log("campana") end,
    ["shell-exit"]      = function(ctx) ctx:log("el shell terminó") end,
    ["config-reloaded"] = function(ctx) ctx:log("configuración recargada") end,
  },
}
