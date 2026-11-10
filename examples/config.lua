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

-- Each system ships different fonts: a family that is not installed stops the terminal from starting
local family, fallback = "Consolas", { "Symbols Nerd Font Mono", "Segoe UI Symbol", "Microsoft YaHei", "Malgun Gothic", "Segoe UI Emoji" }
if terminal.os == "macos" then
  family, fallback = "Menlo", { "Symbols Nerd Font Mono", "Apple Symbols", "PingFang SC", "Apple SD Gothic Neo" }
elseif terminal.os == "linux" then
  family, fallback = "DejaVu Sans Mono", { "Symbols Nerd Font Mono", "DejaVu Sans" }
end

return {
  font = {
    -- Nombre de la familia tal como la ve el sistema. Las fuentes variables no están soportadas.
    family = family,
    size = size,                -- píxeles lógicos (se escala con el DPI del monitor)
    -- Familias a probar, en orden, para los caracteres que la principal no cubre.
    --
    -- Nerd Fonts (iconos y Powerline): los caracteres del área de uso privado (U+E000..U+F8FF y
    -- U+F0000..U+FFFFD) se buscan primero en una Nerd Font, aunque otra familia de la lista
    -- tenga un glifo viejo en ese código. Sirve de dos formas:
    --   * como fuente principal: family = "JetBrainsMono Nerd Font Mono" (o la que tengas)
    --   * como respaldo: una de solo símbolos, "Symbols Nerd Font Mono", que ya viene en la lista
    --     por defecto. Si no la tienes instalada se ignora y se usa cualquier Nerd Font del sistema.
    -- Si pones `fallback`, sustituye a la lista por defecto entera.
    fallback = fallback,
  },

  padding = 6,                   -- margen alrededor del texto, en píxeles lógicos

  cursor = {
    style = "block",             -- block | bar | underline
    -- Parpadeo: apagado por defecto. Aunque lo dejes en false, un programa que lo pida (DECSCUSR,
    -- por ejemplo el vim con cursor parpadeante) hace parpadear el cursor. Se queda fijo mientras
    -- escribes o llega salida, y la ventana sin foco muestra el cursor hueco y sin parpadear.
    blink = false,
    blink_ms = 530,              -- medio ciclo (visible u oculto), entre 100 y 5000
  },

  colors = {
    -- "#rrggbb", "#rgb" o un nombre X11 ("tomato").
    foreground = "#d4d4d4",
    background = "#1e1e2e",
    cursor = "#f5e0dc",
    -- Selección. Si no los pones, el texto seleccionado invierte los colores de cada celda.
    -- selection_foreground = "#1e1e2e",
    -- selection_background = "#f9e2af",
    -- Colores de la paleta desde el índice 0: los 16 primeros son los ANSI y del 17º al 256º
    -- (índices 16 a 255 en los programas) la paleta de 256 colores. Lo que no des queda por defecto.
    ansi = {
      "#45475a", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#bac2de",
      "#585b70", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#a6adc8",
    },
  },

  shell = {
    -- Si falta `program`, se usa el shell por defecto del sistema. `args` necesita `program`.
    -- PowerShell only on Windows; elsewhere the system shell ($SHELL) is used.
    program = terminal.os == "windows" and "pwsh" or nil,
    args = terminal.os == "windows" and { "-NoLogo" } or nil,
    -- cwd = "C:/Users/yo/proyectos",
    env = { EDITOR = "nvim" },
    -- PowerShell no dice en qué directorio está (su `cd` no cambia el del proceso). Con esto se
    -- lanza con `-NoExit -Command` y un script que envuelve tu prompt (el de starship, el de
    -- fábrica...) para informarlo por OSC 7 en cada prompt, y así los splits y los flotantes
    -- arrancan donde estás. No cambia lo que ves. Se ignora si `args` ya lleva `-Command`, `-File`
    -- o `-EncodedCommand`, y si el programa no es pwsh/powershell. Solo al arrancar.
    integration = true,
  },

  window = {
    -- Título fijo. Si lo quitas, la ventana muestra el título que pone el programa.
    -- title = "terminal",
    -- Tamaño inicial en celdas. Es una petición: un gestor de ventanas en mosaico puede ignorarla.
    -- Si no los pones, la ventana tiene el tamaño que le dé el sistema.
    -- columns = 100,
    -- rows = 30,

    -- Marco de la ventana: "full" (barra de título con sus botones, como cualquier ventana) o
    -- "none" (sin barra ni borde: solo la terminal). Se aplica en caliente al guardar el archivo.
    -- Sin barra no hay de dónde agarrar la ventana, así que:
    --   * mantén `drag_modifiers` y arrastra con el botón izquierdo para moverla (Alt por defecto)
    --   * los bordes (6 px) redimensionan: el cursor cambia a una flecha al pasar por ellos
    -- Para cerrarla escribe `exit` en el shell, o enlaza una tecla a terminal.action.Quit (no hay
    -- ninguna por defecto). Un gestor de ventanas en mosaico también puede moverla y cerrarla.
    decorations = "full",
    -- Combinación de CTRL, SHIFT, ALT y SUPER con |. Solo se usa con decorations = "none", y
    -- entonces no puede ser igual a mouse.rectangle_modifiers ni estar vacío.
    drag_modifiers = "ALT",
    -- Esquinas de la ventana: true las deja como las decide el sistema (redondeadas en Windows 11),
    -- false las pone cuadradas. Solo afecta a Windows 11; se aplica en caliente.
    rounded_corners = true,
  },

  mouse = {
    -- Seleccionar con el ratón copia al portapapeles al soltar. Si lo pones en false, copia con
    -- terminal.action.Copy (Ctrl+Shift+C o Ctrl+Insert por defecto).
    copy_on_select = true,
    -- Clic derecho: "paste" pega el portapapeles, "none" no hace nada.
    right_click = "paste",
    -- Líneas por cada muesca de la rueda (1..100). Los touchpads se desplazan en píxeles.
    scroll_lines = 3,
    -- Arrastrar con estos modificadores selecciona un rectángulo en lugar de líneas completas.
    -- No puede estar vacío (arrastrar sin modificadores ya es seleccionar).
    rectangle_modifiers = "CTRL|ALT",
  },
  -- Doble clic selecciona una palabra y triple clic la línea. Si un programa pide el ratón (vim
  -- con `mouse=a`, tmux, htop...) recibe los clics, el arrastre y la rueda en lugar de seleccionar;
  -- mantén SHIFT para seleccionar igualmente. En la pantalla alternativa sin ratón (less, man) la
  -- rueda mueve el cursor con las flechas.

  -- Paneles: la ventana se divide en varios terminales (splits) y puede tener paneles flotantes
  -- encima. Cada panel es un shell propio.
  --   * Los splits se mueven arrastrando la línea que los separa.
  --   * Un flotante se mueve arrastrando el margen de su marco (o Alt + arrastrar desde cualquier
  --     punto, el mismo gesto que mueve la ventana) y se redimensiona por los bordes y esquinas.
  --   * Un clic enfoca el panel. El enfocado lleva marco y los demás el texto apagado.
  --   * Un panel cuyo shell termina se cierra (con `on_shell_exit = "close"`); el último del
  --     mosaico cierra la ventana.
  panes = {
    -- border_color = "#44475a",   -- divisores y borde de los flotantes (por defecto, texto apagado)
    -- focus_color = "#f8f8f2",    -- marco del panel con foco (por defecto, el color del cursor)
    -- leader_color = "#f1fa8c",   -- marco mientras el prefijo espera (por defecto, amarillo ANSI)
    inactive_dim = 0.35,           -- cuánto se apaga el texto de los paneles sin foco: 0 a 1
    -- Un panel nuevo (un split o un flotante) arranca en el directorio del panel activo. La
    -- terminal lo sabe porque el shell lo informa con OSC 7 (`ESC ] 7 ; file:///C:/ruta ST`) o con
    -- OSC 9;9 (`ESC ] 9 ; 9 ; C:\ruta ST`, el de ConEmu y Windows Terminal). PowerShell no lo hace
    -- solo: ver `shell.integration`. Con cmd, se consigue con `PROMPT $E]9;9;$P$E\$P$G`. Si el shell
    -- no informa nada se usa `shell.cwd`.
    inherit_cwd = true,
    float_width = 0.7,             -- tamaño de un flotante nuevo, como fracción de la ventana
    float_height = 0.7,
  },

  -- Prefijo estilo tmux: tras pulsarlo, la tecla siguiente se busca entre las que llevan
  -- `mods = "LEADER"` (más abajo y en la lista de por defecto). Mientras espera, el marco del panel
  -- cambia de color; Escape o una tecla sin enlace lo cancelan, y pulsarlo dos veces envía el
  -- carácter de control al programa (Ctrl+B Ctrl+B manda ^B, para un tmux anidado).
  -- Por defecto es Ctrl+B con 1500 ms de plazo. `leader = false` lo desactiva.
  leader = { key = "b", mods = "CTRL", timeout_ms = 1500 },

  scrollback = 10000,            -- líneas de historial

  on_shell_exit = "close",       -- close: cierra el panel (y la ventana con el último) | hold: lo deja con el aviso

  -- Teclas. `key` es un carácter ("v") o un nombre: Enter, Tab, Space, Escape, Backspace, Delete,
  -- Insert, Home, End, PageUp, PageDown, ArrowUp/Down/Left/Right, F1..F24. `mods` combina CTRL,
  -- SHIFT, ALT y SUPER con |, y tiene que coincidir exactamente (también SHIFT si el carácter lo
  -- necesita, como "+" en un teclado US). Una tecla enlazada no llega al shell.
  --
  -- `action` es una acción nativa (terminal.action.X, o su nombre entre comillas si no lleva
  -- parámetros) o una función de Lua que recibe `ctx`.
  --
  -- Ya vienen enlazadas por defecto: Ctrl+Shift+C y Ctrl+Insert = Copy, Ctrl+Shift+V y
  -- Shift+Insert = Paste, Shift+PageUp / Shift+PageDown = una página de historial. Y, tras el
  -- prefijo: % divide a la derecha, " divide hacia abajo, x cierra el panel, z amplía o restaura,
  -- o siguiente panel, flechas o h/j/k/l mueven el foco, Ctrl+flechas redimensionan 1 celda y
  -- Alt+flechas 5, w oculta o muestra los flotantes (crea uno si no hay), W abre otro flotante y
  -- Shift+flechas mueven el flotante. Una tecla tuya con la misma combinación sustituye a la de
  -- por defecto. Para quedarte solo con las tuyas:
  -- disable_default_keys = true,
  --
  -- Acciones de selección: Copy, SelectAll, ClearSelection.
  -- Acciones de paneles: SplitRight, SplitDown, ClosePane, ToggleZoom, FocusNext, FocusPrevious,
  --   FocusPane("left"|"right"|"up"|"down"), ResizePane("left", 5), MovePane("down", 2) (solo
  --   flotantes), ToggleFloats y NewFloat{ program = "lazygit", args = {...}, width = 0.9,
  --   height = 0.9, cwd = "C:/proyectos" } (todo opcional; con `program` el flotante se cierra al
  --   terminar el programa, como un popup).
  keys = {
    -- Paneles sin prefijo y con prefijo (mods = "LEADER" o "LEADER|SHIFT"...)
    { key = "Enter", mods = "CTRL",       action = terminal.action.SplitDown },
    { key = "Enter", mods = "CTRL|SHIFT", action = terminal.action.SplitRight },
    { key = "v",     mods = "LEADER",     action = terminal.action.SplitRight },
    { key = "g",     mods = "LEADER",     action = terminal.action.NewFloat({ program = "lazygit", width = 0.9, height = 0.9 }) },
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
