//! Ratón: selección con gestos (clic, doble clic por palabra, triple por línea, arrastre y
//! autodesplazamiento), copia, reporte a los programas que lo piden, rueda y los bordes de una
//! ventana sin decoraciones. Con varios paneles, cada gesto va al panel que hay bajo el puntero
//! (o al que lo capturó al pulsar, mientras dure el arrastre).
//!
//! La lógica de ventana vive en `impl App`; las cuentas que no necesitan una ventana (qué borde
//! está bajo el puntero, a qué celda corresponde un punto, cuántas líneas es un giro de rueda) son
//! funciones puras para poder probarlas.

use std::time::{Duration, Instant};

use libghostty_vt::key::{Action as KeyAction, Event as GhosttyKeyEvent, Key, Mods};
use libghostty_vt::mouse::{self, EncoderSize, Position};
use libghostty_vt::screen::Screen;
use libghostty_vt::selection::FormatOptions;
use libghostty_vt::selection::gesture::{
    Autoscroll, AutoscrollTickEvent, DragEvent, Geometry, PressEvent, ReleaseEvent,
};
use libghostty_vt::terminal::{Mode, Point, PointCoordinate, ScrollViewport};
use winit::dpi::PhysicalPosition;
use winit::event::{ElementState, MouseButton, MouseScrollDelta};
use winit::window::{CursorIcon, ResizeDirection};

use crate::App;
use crate::actions;
use crate::config::{Decorations, RightClick};
use crate::layout::{Axis, Edge, Hit, HitParams, PaneId, Path};

/// Cada cuánto se desplaza una línea mientras se arrastra una selección fuera de la ventana
pub const AUTOSCROLL_INTERVAL: Duration = Duration::from_millis(50);
/// Tiempo máximo entre clics para que cuenten como doble o triple clic
const MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(500);
/// Grosor, en pixeles lógicos, de la zona de los bordes que redimensiona una ventana sin barra
const RESIZE_BORDER: f64 = 6.0;
/// A cuántos pixeles lógicos de un divisor entre paneles se agarra
const DIVIDER_ZONE: f64 = 4.0;

/// Qué tiene capturado el ratón mientras hay un botón pulsado
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Capture {
    /// Una selección o un reporte al programa de ese panel
    Pane(PaneId),
    /// El arrastre de un divisor entre paneles
    Divider(Path, Axis),
    /// Mover un flotante agarrándolo por el anillo de su margen
    FloatMove(PaneId),
    /// Redimensionar un flotante por un borde o una esquina
    FloatResize(PaneId, Edge),
}

/// Estado del ratón que persiste entre eventos. El estado de la selección (el gesto, si hay un
/// arrastre en curso, qué botón se reportó) es de cada panel.
pub struct MouseState {
    press: PressEvent<'static>,
    drag: DragEvent<'static>,
    release: ReleaseEvent<'static>,
    autoscroll_tick: AutoscrollTickEvent<'static>,
    encoder: mouse::Encoder<'static>,
    event: mouse::Event<'static>,
    /// Última posición del puntero, en pixeles físicos de la ventana
    pointer: (f64, f64),
    /// Lo que tiene el ratón mientras hay un botón pulsado
    capture: Option<Capture>,
    /// Dónde estaba el puntero en el último paso de un arrastre de divisor o de flotante
    drag_from: (f64, f64),
    /// Fracción de línea que sobró del último giro de rueda (ratones de precisión y touchpads)
    wheel_remainder: f64,
    /// Origen del tiempo de los clics
    clock: Instant,
    /// Unidades por segundo que libghostty espera en `set_time` (ver `gesture_time`)
    ticks_per_second: u64,
    /// Cuándo toca el siguiente paso del autodesplazamiento, si hay uno en curso
    pub autoscroll_deadline: Option<Instant>,
    icon: CursorIcon,
}

impl MouseState {
    pub fn new() -> Result<Self, libghostty_vt::Error> {
        Ok(Self {
            press: PressEvent::new()?,
            drag: DragEvent::new()?,
            release: ReleaseEvent::new()?,
            autoscroll_tick: AutoscrollTickEvent::new()?,
            encoder: mouse::Encoder::new()?,
            event: mouse::Event::new()?,
            pointer: (0.0, 0.0),
            capture: None,
            drag_from: (0.0, 0.0),
            wheel_remainder: 0.0,
            clock: Instant::now(),
            ticks_per_second: gesture_ticks_per_second(),
            autoscroll_deadline: None,
            icon: CursorIcon::Default,
        })
    }

    /// Un panel se cerró: si tenía el ratón, lo suelta
    pub fn forget_pane(&mut self, id: PaneId) {
        if self.capture == Some(Capture::Pane(id)) {
            self.capture = None;
            self.autoscroll_deadline = None;
        }
    }
}

fn divider_icon(axis: Axis) -> CursorIcon {
    match axis {
        Axis::Horizontal => CursorIcon::EwResize,
        Axis::Vertical => CursorIcon::NsResize,
    }
}

/// Flecha de redimensionado para un borde o una esquina de un flotante
fn edge_icon(edge: Edge) -> CursorIcon {
    match (edge.left, edge.right, edge.top, edge.bottom) {
        (true, _, true, _) | (_, true, _, true) => CursorIcon::NwseResize,
        (_, true, true, _) | (true, _, _, true) => CursorIcon::NeswResize,
        (true, ..) | (_, true, ..) => CursorIcon::EwResize,
        _ => CursorIcon::NsResize,
    }
}

fn capture_icon(capture: &Capture) -> Option<CursorIcon> {
    match capture {
        Capture::Pane(_) => None,
        Capture::Divider(_, axis) => Some(divider_icon(*axis)),
        Capture::FloatMove(_) => Some(CursorIcon::Move),
        Capture::FloatResize(_, edge) => Some(edge_icon(*edge)),
    }
}

/// Unidades por segundo del tiempo que se le da a `PressEvent::set_time`. La API dice nanosegundos,
/// pero en Windows libghostty guarda ese número tal cual en un `std.time.Instant` de Zig, que ahí
/// cuenta ticks de QueryPerformanceCounter (10 MHz = 100 ns por tick) y al compararlo con el
/// intervalo los multiplica por 100: sin corregirlo, un doble clic solo cuenta si los dos clics
/// están a menos de 5 ms en lugar de 500 ms.
fn gesture_ticks_per_second() -> u64 {
    #[cfg(windows)]
    {
        unsafe extern "system" {
            fn QueryPerformanceFrequency(frequency: *mut i64) -> i32;
        }
        let mut frequency = 0i64;
        // SAFETY: recibe un puntero a un entero válido y no guarda nada de él
        let ok = unsafe { QueryPerformanceFrequency(&mut frequency) };
        if ok != 0 && frequency > 0 {
            return frequency as u64;
        }
    }
    1_000_000_000
}

/// Convierte el tiempo transcurrido en lo que `set_time` espera, dado `ticks_per_second`
pub fn gesture_time(elapsed: Duration, ticks_per_second: u64) -> Duration {
    let ticks = elapsed.as_nanos() * u128::from(ticks_per_second) / 1_000_000_000;
    Duration::from_nanos(ticks as u64)
}

/// Qué borde o esquina de una ventana de `width` x `height` está bajo `(x, y)`, con un borde
/// sensible de `border` pixeles. Las esquinas mandan sobre los lados.
pub fn resize_direction(
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    border: f64,
) -> Option<ResizeDirection> {
    let left = x < border;
    let right = x >= width - border;
    let top = y < border;
    let bottom = y >= height - border;
    match (left, right, top, bottom) {
        (true, _, true, _) => Some(ResizeDirection::NorthWest),
        (_, true, true, _) => Some(ResizeDirection::NorthEast),
        (true, _, _, true) => Some(ResizeDirection::SouthWest),
        (_, true, _, true) => Some(ResizeDirection::SouthEast),
        (true, ..) => Some(ResizeDirection::West),
        (_, true, ..) => Some(ResizeDirection::East),
        (_, _, true, _) => Some(ResizeDirection::North),
        (_, _, _, true) => Some(ResizeDirection::South),
        _ => None,
    }
}

fn resize_icon(direction: ResizeDirection) -> CursorIcon {
    match direction {
        ResizeDirection::East | ResizeDirection::West => CursorIcon::EwResize,
        ResizeDirection::North | ResizeDirection::South => CursorIcon::NsResize,
        ResizeDirection::NorthWest | ResizeDirection::SouthEast => CursorIcon::NwseResize,
        ResizeDirection::NorthEast | ResizeDirection::SouthWest => CursorIcon::NeswResize,
    }
}

/// Celda (columna, fila) bajo el punto `(x, y)` de la ventana. Fuera de la cuadrícula (en el margen
/// o más allá) se queda en la celda más cercana, como hacen los terminales al arrastrar.
pub fn cell_at(
    x: f64,
    y: f64,
    padding: f64,
    cell_width: u32,
    cell_height: u32,
    columns: u16,
    rows: u16,
) -> (u16, u16) {
    let column = ((x - padding) / f64::from(cell_width.max(1))).floor();
    let row = ((y - padding) / f64::from(cell_height.max(1))).floor();
    let clamp = |value: f64, count: u16| value.clamp(0.0, f64::from(count.max(1) - 1)) as u16;
    (clamp(column, columns), clamp(row, rows))
}

fn viewport_point(column: u16, row: u16) -> Point {
    Point::Viewport(PointCoordinate {
        x: column,
        y: u32::from(row),
    })
}

/// Líneas que corresponden a un giro de rueda. Positivo = hacia arriba (ver el historial). Un
/// `LineDelta` (rueda con muescas) vale `scroll_lines` líneas por muesca; un `PixelDelta` (touchpad)
/// vale lo que mida en celdas. Lo que no llega a una línea entera se acumula en `remainder`.
pub fn wheel_lines(
    delta: MouseScrollDelta,
    scroll_lines: u16,
    cell_height: u32,
    remainder: &mut f64,
) -> i32 {
    let lines = match delta {
        MouseScrollDelta::LineDelta(_, y) => f64::from(y) * f64::from(scroll_lines),
        MouseScrollDelta::PixelDelta(position) => position.y / f64::from(cell_height.max(1)),
    };
    *remainder += lines;
    let whole = remainder.trunc();
    *remainder -= whole;
    whole as i32
}

impl App {
    fn undecorated(&self) -> bool {
        self.config.window.decorations == Decorations::None
    }

    fn hit_params(&self) -> Option<HitParams> {
        let scale = self.window.as_ref()?.scale_factor();
        let padding = f64::from(self.renderer.as_ref()?.padding());
        // La banda de los bordes de un flotante no puede comerse todo su margen: lo que queda
        // es el anillo con el que se mueve
        let band = (RESIZE_BORDER * scale).min((padding * 0.6).max(3.0));
        Some(HitParams {
            divider_zone: DIVIDER_ZONE * scale,
            edge_band: band,
            move_ring: padding,
        })
    }

    /// Flotante bajo el puntero, esté sobre su contenido, su margen o su borde
    fn float_under_pointer(&self) -> Option<PaneId> {
        match self.hit_at_pointer()? {
            Hit::Pane(id) | Hit::FloatMove(id) | Hit::FloatResize(id, _)
                if self.layout.is_float(id) =>
            {
                Some(id)
            }
            _ => None,
        }
    }

    /// Qué hay bajo el puntero: el contenido de un panel, un divisor, el borde de un flotante...
    fn hit_at_pointer(&self) -> Option<Hit> {
        let area = self.window_area()?;
        let params = self.hit_params()?;
        let (x, y) = self.mouse.pointer;
        Some(self.layout.hit_test(x, y, area, params))
    }

    /// Panel cuyo contenido está bajo el puntero
    fn pane_at_pointer(&self) -> Option<usize> {
        match self.hit_at_pointer()? {
            Hit::Pane(id) => self.index_of(id),
            _ => None,
        }
    }

    /// Panel que recibe el ratón: el que lo capturó al pulsar un botón o, si no, el de debajo. Con
    /// un divisor capturado no lo recibe ninguno.
    fn target_pane(&self) -> Option<usize> {
        match &self.mouse.capture {
            Some(Capture::Pane(id)) => self.index_of(*id),
            Some(_) => None,
            None => self.pane_at_pointer(),
        }
    }

    /// Panel que capturó el ratón al pulsar un botón
    fn captured_pane(&self) -> Option<usize> {
        match &self.mouse.capture {
            Some(Capture::Pane(id)) => self.index_of(*id),
            _ => None,
        }
    }

    /// Empieza el arrastre de un divisor o de un flotante
    fn start_drag(&mut self, capture: Capture) {
        self.mouse.drag_from = self.mouse.pointer;
        self.mouse.capture = Some(capture);
        self.update_pointer_icon();
    }

    /// Mueve (`edge` = `None`) o redimensiona un flotante lo que se movió el puntero desde el
    /// último paso
    fn drag_float(&mut self, id: PaneId, edge: Option<Edge>) {
        let (Some(area), Some(limits)) = (self.window_area(), self.pane_limits()) else {
            return;
        };
        let (x, y) = self.mouse.pointer;
        let dx = (x - self.mouse.drag_from.0).round() as i64;
        let dy = (y - self.mouse.drag_from.1).round() as i64;
        if dx == 0 && dy == 0 {
            return;
        }
        self.mouse.drag_from = (x, y);
        let changed = match edge {
            None => self.layout.move_float(id, dx, dy, area),
            Some(edge) => self
                .layout
                .resize_float_edge(id, edge, dx, dy, area, limits),
        };
        if changed {
            self.relayout(area);
        }
    }

    /// Lleva el divisor capturado hasta donde está el puntero
    fn drag_divider(&mut self, path: &[bool], axis: Axis) {
        let (Some(area), Some(limits)) = (self.window_area(), self.pane_limits()) else {
            return;
        };
        let (x, y) = self.mouse.pointer;
        let boundary = match axis {
            Axis::Horizontal => x,
            Axis::Vertical => y,
        };
        if self
            .layout
            .set_divider(path, boundary.round() as i64, area, limits)
        {
            self.relayout(area);
        }
    }

    /// Posición del puntero respecto a la esquina del panel
    fn local_pointer(&self, idx: usize) -> (f64, f64) {
        let rect = self.panes[idx].rect;
        (
            self.mouse.pointer.0 - f64::from(rect.x),
            self.mouse.pointer.1 - f64::from(rect.y),
        )
    }

    /// ¿El programa de ese panel pidió recibir el ratón? Con Shift mantenido el ratón es siempre de
    /// la terminal (selección), como en cualquier emulador.
    fn app_wants_mouse_at(&self, idx: usize) -> bool {
        self.panes[idx]
            .terminal
            .is_mouse_tracking()
            .unwrap_or(false)
            && self.mod_bits & actions::SHIFT == 0
    }

    fn rectangle_requested(&self) -> bool {
        self.mod_bits == self.config.mouse.rectangle_modifiers.0
    }

    /// Borde de la ventana bajo el puntero, solo si no tiene decoraciones y no está maximizada
    fn edge_under_pointer(&self) -> Option<ResizeDirection> {
        let window = self.window.as_ref()?;
        if !self.undecorated() || window.is_maximized() || window.fullscreen().is_some() {
            return None;
        }
        let size = window.inner_size();
        let (x, y) = self.mouse.pointer;
        resize_direction(
            x,
            y,
            f64::from(size.width),
            f64::from(size.height),
            RESIZE_BORDER * window.scale_factor(),
        )
    }

    /// Celda (columna, fila) del panel bajo el puntero
    fn cell_in_pane(&self, idx: usize) -> Option<(u16, u16)> {
        let renderer = self.renderer.as_ref()?;
        let pane = self.panes.get(idx)?;
        let (cell_width, cell_height) = renderer.cell_size();
        let (x, y) = self.local_pointer(idx);
        Some(cell_at(
            x,
            y,
            f64::from(renderer.padding()),
            cell_width,
            cell_height,
            pane.grid.0,
            pane.grid.1,
        ))
    }

    fn encoder_size(&self, idx: usize) -> Option<EncoderSize> {
        let renderer = self.renderer.as_ref()?;
        let rect = self.panes.get(idx)?.rect;
        let (cell_width, cell_height) = renderer.cell_size();
        let padding = renderer.padding();
        Some(EncoderSize {
            screen_width: rect.w,
            screen_height: rect.h,
            cell_width,
            cell_height,
            padding_top: padding,
            padding_bottom: padding,
            padding_right: padding,
            padding_left: padding,
        })
    }

    /// Cursor del sistema según lo que haya bajo el puntero: flechas de redimensionado en los
    /// bordes de una ventana sin barra, flecha normal si el programa se queda con el ratón, y el
    /// cursor de texto en el resto
    fn update_pointer_icon(&mut self) {
        let icon = if let Some(direction) = self.edge_under_pointer() {
            resize_icon(direction)
        } else if let Some(icon) = self.mouse.capture.as_ref().and_then(capture_icon) {
            icon
        } else {
            match self.hit_at_pointer() {
                Some(Hit::Divider(_, axis)) => divider_icon(axis),
                Some(Hit::FloatMove(_)) => CursorIcon::Move,
                Some(Hit::FloatResize(_, edge)) => edge_icon(edge),
                Some(Hit::Pane(id))
                    if self
                        .index_of(id)
                        .is_some_and(|idx| self.app_wants_mouse_at(idx)) =>
                {
                    CursorIcon::Default
                }
                _ => CursorIcon::Text,
            }
        };
        if icon != self.mouse.icon {
            self.mouse.icon = icon;
            if let Some(window) = &self.window {
                window.set_cursor(icon);
            }
        }
    }

    pub fn on_cursor_moved(&mut self, position: PhysicalPosition<f64>) {
        self.mouse.pointer = (position.x, position.y);
        match self.mouse.capture.clone() {
            Some(Capture::Divider(path, axis)) => {
                self.drag_divider(&path, axis);
                return;
            }
            Some(Capture::FloatMove(id)) => {
                self.drag_float(id, None);
                return;
            }
            Some(Capture::FloatResize(id, edge)) => {
                self.drag_float(id, Some(edge));
                return;
            }
            _ => {}
        }
        if let Some(idx) = self.target_pane() {
            if self.panes[idx].selecting {
                self.drag_selection(idx);
                return;
            }
            if self.app_wants_mouse_at(idx) {
                let button = self.panes[idx].reported;
                self.send_mouse(idx, mouse::Action::Motion, button);
            }
        }
        self.update_pointer_icon();
    }

    pub fn on_mouse_button(&mut self, state: ElementState, button: MouseButton) {
        let pressed = state == ElementState::Pressed;
        let hover = self.pane_at_pointer();
        let wants_mouse = hover.is_some_and(|idx| self.app_wants_mouse_at(idx));

        match (button, pressed) {
            (MouseButton::Left, true) => {
                // Ventana sin barra: los bordes redimensionan y el gesto configurado la mueve
                if let Some(window) = self.window.as_ref().filter(|_| self.undecorated())
                    && let Some(direction) = self.edge_under_pointer()
                {
                    let _ = window.drag_resize_window(direction);
                    return;
                }
                if self.mod_bits == self.config.window.drag_modifiers.0 {
                    // El mismo gesto mueve un flotante si hay uno bajo el puntero, desde donde sea
                    if let Some(id) = self.float_under_pointer() {
                        self.focus_pane(id);
                        self.start_drag(Capture::FloatMove(id));
                        return;
                    }
                    if let Some(window) = self.window.as_ref().filter(|_| self.undecorated()) {
                        let _ = window.drag_window();
                        return;
                    }
                }
                // Un divisor entre paneles o el margen y los bordes de un flotante se agarran
                match self.hit_at_pointer() {
                    Some(Hit::Divider(path, axis)) => {
                        self.start_drag(Capture::Divider(path, axis));
                        return;
                    }
                    Some(Hit::FloatMove(id)) => {
                        self.focus_pane(id);
                        self.start_drag(Capture::FloatMove(id));
                        return;
                    }
                    Some(Hit::FloatResize(id, edge)) => {
                        self.focus_pane(id);
                        self.start_drag(Capture::FloatResize(id, edge));
                        return;
                    }
                    _ => {}
                }
                let Some(idx) = hover else {
                    return;
                };
                let id = self.panes[idx].id;
                self.focus_pane(id);
                self.mouse.capture = Some(Capture::Pane(id));
                if wants_mouse {
                    self.panes[idx].reported = Some(mouse::Button::Left);
                    self.send_mouse(idx, mouse::Action::Press, Some(mouse::Button::Left));
                } else {
                    self.begin_selection(idx);
                }
            }
            (MouseButton::Left, false) => {
                if matches!(
                    self.mouse.capture,
                    Some(Capture::Divider(..) | Capture::FloatMove(_) | Capture::FloatResize(..))
                ) {
                    self.mouse.capture = None;
                    self.update_pointer_icon();
                    return;
                }
                let Some(idx) = self.captured_pane() else {
                    return;
                };
                if self.panes[idx].reported == Some(mouse::Button::Left) {
                    self.send_mouse(idx, mouse::Action::Release, Some(mouse::Button::Left));
                    self.panes[idx].reported = None;
                } else if self.panes[idx].selecting {
                    self.finish_selection(idx);
                }
                self.release_capture(idx);
            }
            (MouseButton::Right, true) => {
                let Some(idx) = hover else {
                    return;
                };
                let id = self.panes[idx].id;
                self.focus_pane(id);
                if wants_mouse {
                    self.mouse.capture = Some(Capture::Pane(id));
                    self.panes[idx].reported = Some(mouse::Button::Right);
                    self.send_mouse(idx, mouse::Action::Press, Some(mouse::Button::Right));
                } else if self.config.mouse.right_click == RightClick::Paste {
                    self.paste();
                }
            }
            (MouseButton::Middle, true) => {
                if let Some(idx) = hover.filter(|_| wants_mouse) {
                    self.mouse.capture = Some(Capture::Pane(self.panes[idx].id));
                    self.panes[idx].reported = Some(mouse::Button::Middle);
                    self.send_mouse(idx, mouse::Action::Press, Some(mouse::Button::Middle));
                }
            }
            (MouseButton::Right, false) | (MouseButton::Middle, false) => {
                let released = if button == MouseButton::Right {
                    mouse::Button::Right
                } else {
                    mouse::Button::Middle
                };
                let Some(idx) = self.captured_pane() else {
                    return;
                };
                if self.panes[idx].reported == Some(released) {
                    self.send_mouse(idx, mouse::Action::Release, Some(released));
                    self.panes[idx].reported = None;
                    self.release_capture(idx);
                }
            }
            _ => {}
        }
    }

    /// Cuando el panel ya no tiene un botón pulsado, deja de tener el ratón capturado
    fn release_capture(&mut self, idx: usize) {
        let pane = &self.panes[idx];
        if !pane.selecting && pane.reported.is_none() {
            self.mouse.capture = None;
        }
        self.update_pointer_icon();
    }

    pub fn on_mouse_wheel(&mut self, delta: MouseScrollDelta) {
        let Some(idx) = self.target_pane() else {
            return;
        };
        let Some(renderer) = &self.renderer else {
            return;
        };
        let (_, cell_height) = renderer.cell_size();
        let scroll_lines = self.config.mouse.scroll_lines;
        let lines = wheel_lines(
            delta,
            scroll_lines,
            cell_height,
            &mut self.mouse.wheel_remainder,
        );
        if lines == 0 {
            return;
        }
        let up = lines > 0;

        if self.app_wants_mouse_at(idx) {
            // La rueda es el "botón" 4 (arriba) o 5 (abajo); una muesca es un evento
            let button = if up {
                mouse::Button::Four
            } else {
                mouse::Button::Five
            };
            let notches = (lines.unsigned_abs() / u32::from(scroll_lines)).max(1);
            for _ in 0..notches {
                self.send_mouse(idx, mouse::Action::Press, Some(button));
            }
        } else if self.mod_bits & actions::SHIFT == 0
            && self.panes[idx].terminal.active_screen().unwrap_or_default() == Screen::Alternate
            && self.panes[idx]
                .terminal
                .mode(Mode::ALT_SCROLL)
                .unwrap_or(false)
        {
            // Pantalla alternativa sin ratón (less, man...): la rueda mueve el cursor con flechas
            let key = if up { Key::ArrowUp } else { Key::ArrowDown };
            self.send_key_repeatedly(idx, key, lines.unsigned_abs());
        } else {
            self.scroll_pane(idx, ScrollViewport::Delta(-(lines as isize)));
        }
    }

    /// Manda al programa de un panel un evento de ratón codificado según el protocolo que haya pedido
    fn send_mouse(&mut self, idx: usize, action: mouse::Action, button: Option<mouse::Button>) {
        let Some(size) = self.encoder_size(idx) else {
            return;
        };
        let (x, y) = self.local_pointer(idx);
        let mods = self.mods;
        let pane = &mut self.panes[idx];
        let any_button = pane.reported.is_some();
        let mouse = &mut self.mouse;
        mouse
            .encoder
            .set_options_from_terminal(&pane.terminal)
            .set_size(size)
            .set_any_button_pressed(any_button)
            .set_track_last_cell(true);
        mouse
            .event
            .set_action(action)
            .set_button(button)
            .set_mods(mods)
            .set_position(Position {
                x: x as f32,
                y: y as f32,
            });
        let mut bytes = Vec::new();
        if mouse
            .encoder
            .encode_to_vec(&mouse.event, &mut bytes)
            .is_ok()
            && !bytes.is_empty()
        {
            pane.write(&bytes);
        }
    }

    fn send_key_repeatedly(&mut self, idx: usize, key: Key, count: u32) {
        let Ok(mut event) = GhosttyKeyEvent::new() else {
            return;
        };
        event
            .set_action(KeyAction::Press)
            .set_key(key)
            .set_mods(Mods::empty())
            .set_utf8(None::<&str>);
        let pane = &mut self.panes[idx];
        self.key_encoder.set_options_from_terminal(&pane.terminal);
        let mut sequence = Vec::new();
        if self
            .key_encoder
            .encode_to_vec(&event, &mut sequence)
            .is_ok()
            && !sequence.is_empty()
        {
            for _ in 0..count {
                pane.write(&sequence);
            }
        }
    }

    fn gesture_geometry(&self, idx: usize) -> Option<Geometry> {
        let renderer = self.renderer.as_ref()?;
        let pane = self.panes.get(idx)?;
        let (cell_width, _) = renderer.cell_size();
        Some(Geometry {
            columns: u32::from(pane.grid.0.max(1)),
            cell_width,
            padding_left: renderer.padding(),
            screen_height: pane.rect.h.max(1),
        })
    }

    /// Pulsación izquierda: empieza una selección (o la amplía a palabra o línea con doble o
    /// triple clic)
    fn begin_selection(&mut self, idx: usize) {
        let Some((column, row)) = self.cell_in_pane(idx) else {
            return;
        };
        let Some(renderer) = &self.renderer else {
            return;
        };
        let (cell_width, _) = renderer.cell_size();
        let (x, y) = self.local_pointer(idx);
        let pane = &mut self.panes[idx];
        let Ok(grid_ref) = pane.terminal.grid_ref(viewport_point(column, row)) else {
            return;
        };

        let mouse = &mut self.mouse;
        let _ = mouse.press.set_position(x, y);
        let _ = mouse.press.set_repeat_distance(f64::from(cell_width));
        let _ = mouse
            .press
            .set_time(gesture_time(mouse.clock.elapsed(), mouse.ticks_per_second));
        let _ = mouse.press.set_repeat_interval(MULTI_CLICK_INTERVAL);
        match mouse
            .press
            .apply(&mut pane.gesture, &pane.terminal, grid_ref)
        {
            Ok(selection) => {
                let _ = pane.terminal.set_selection(selection.as_ref());
            }
            Err(error) => eprintln!("[mouse] no se pudo iniciar la selección: {error}"),
        }
        pane.selecting = true;
        self.redraw();
    }

    /// El puntero se mueve con el botón izquierdo pulsado: la selección sigue al puntero
    fn drag_selection(&mut self, idx: usize) {
        let Some((column, row)) = self.cell_in_pane(idx) else {
            return;
        };
        let Some(geometry) = self.gesture_geometry(idx) else {
            return;
        };
        let rectangle = self.rectangle_requested();
        let (x, y) = self.local_pointer(idx);
        let pane = &mut self.panes[idx];
        let Ok(grid_ref) = pane.terminal.grid_ref(viewport_point(column, row)) else {
            return;
        };

        let mouse = &mut self.mouse;
        let _ = mouse.drag.set_position(x, y);
        let _ = mouse.drag.set_rectangle(rectangle);
        if let Ok(Some(selection)) =
            mouse
                .drag
                .apply(&mut pane.gesture, &pane.terminal, grid_ref, geometry)
        {
            let _ = pane.terminal.set_selection(Some(&selection));
        }

        // Si el puntero salió por arriba o por abajo, el historial sigue desplazándose
        let direction = pane
            .gesture
            .autoscroll(&pane.terminal)
            .unwrap_or(Autoscroll::None);
        match direction {
            Autoscroll::Up | Autoscroll::Down => {
                mouse
                    .autoscroll_deadline
                    .get_or_insert_with(|| Instant::now() + AUTOSCROLL_INTERVAL);
            }
            _ => mouse.autoscroll_deadline = None,
        }
        self.redraw();
    }

    /// Un paso del autodesplazamiento, cuando vence su plazo
    pub fn tick_autoscroll(&mut self, now: Instant) {
        let Some(deadline) = self.mouse.autoscroll_deadline else {
            return;
        };
        if now < deadline {
            return;
        }
        let Some(idx) = self.captured_pane() else {
            self.mouse.autoscroll_deadline = None;
            return;
        };
        let direction = self.panes[idx]
            .gesture
            .autoscroll(&self.panes[idx].terminal)
            .unwrap_or(Autoscroll::None);
        let step = match direction {
            Autoscroll::Up => -1,
            Autoscroll::Down => 1,
            _ => {
                self.mouse.autoscroll_deadline = None;
                return;
            }
        };
        self.panes[idx]
            .terminal
            .scroll_viewport(ScrollViewport::Delta(step));

        let rectangle = self.rectangle_requested();
        let (Some((column, row)), Some(geometry)) =
            (self.cell_in_pane(idx), self.gesture_geometry(idx))
        else {
            return;
        };
        let (x, y) = self.local_pointer(idx);
        let pane = &mut self.panes[idx];
        let mouse = &mut self.mouse;
        let _ = mouse.autoscroll_tick.set_position(x, y);
        let _ = mouse.autoscroll_tick.set_rectangle(rectangle);
        let point = PointCoordinate {
            x: column,
            y: u32::from(row),
        };
        if let Ok(Some(selection)) =
            mouse
                .autoscroll_tick
                .apply(&mut pane.gesture, &pane.terminal, point, geometry)
        {
            let _ = pane.terminal.set_selection(Some(&selection));
        }
        mouse.autoscroll_deadline = Some(now + AUTOSCROLL_INTERVAL);
        self.redraw();
    }

    /// Soltar el botón izquierdo termina la selección (y la copia si `copy_on_select`)
    fn finish_selection(&mut self, idx: usize) {
        let grid_ref_cell = self.cell_in_pane(idx);
        let pane = &mut self.panes[idx];
        let grid_ref = grid_ref_cell
            .and_then(|(column, row)| pane.terminal.grid_ref(viewport_point(column, row)).ok());
        let mouse = &mut self.mouse;
        let _ = mouse
            .release
            .apply(&mut pane.gesture, &pane.terminal, grid_ref);
        pane.selecting = false;
        mouse.autoscroll_deadline = None;
        if self.config.mouse.copy_on_select {
            self.copy_selection_at(idx);
        }
    }

    /// Copia al portapapeles la selección de un panel, sin saltos de línea de ajuste ni espacios
    /// finales
    fn copy_selection_at(&mut self, idx: usize) {
        let options = FormatOptions::new().with_unwrap(true).with_trim(true);
        let text = match self.panes[idx]
            .terminal
            .format_selection_alloc(None, options)
        {
            Ok(Some(bytes)) => String::from_utf8_lossy(&bytes).into_owned(),
            _ => return,
        };
        if text.is_empty() {
            return;
        }
        if let Err(error) =
            arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_text(text))
        {
            eprintln!("[copy] no se pudo escribir en el portapapeles: {error}");
        }
    }

    /// Copia la selección del panel con foco
    pub fn copy_selection(&mut self) {
        let idx = self.focused_index();
        self.copy_selection_at(idx);
    }

    pub fn select_all(&mut self) {
        let idx = self.focused_index();
        let terminal = &mut self.panes[idx].terminal;
        if let Ok(Some(selection)) = terminal.select_all() {
            let _ = terminal.set_selection(Some(&selection));
            self.redraw();
        }
    }

    /// Quita la selección del panel con foco
    pub fn clear_selection(&mut self) {
        let idx = self.focused_index();
        self.clear_selection_at(idx);
    }

    pub fn clear_selection_at(&mut self, idx: usize) {
        let _ = self.panes[idx].terminal.set_selection(None);
        self.redraw();
    }

    fn redraw(&self) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corners_win_over_sides_and_the_inside_is_not_an_edge() {
        let (w, h, b) = (800.0, 600.0, 6.0);
        assert_eq!(
            resize_direction(0.0, 0.0, w, h, b),
            Some(ResizeDirection::NorthWest)
        );
        assert_eq!(
            resize_direction(799.0, 0.0, w, h, b),
            Some(ResizeDirection::NorthEast)
        );
        assert_eq!(
            resize_direction(0.0, 599.0, w, h, b),
            Some(ResizeDirection::SouthWest)
        );
        assert_eq!(
            resize_direction(799.0, 599.0, w, h, b),
            Some(ResizeDirection::SouthEast)
        );
        assert_eq!(
            resize_direction(3.0, 300.0, w, h, b),
            Some(ResizeDirection::West)
        );
        assert_eq!(
            resize_direction(797.0, 300.0, w, h, b),
            Some(ResizeDirection::East)
        );
        assert_eq!(
            resize_direction(400.0, 2.0, w, h, b),
            Some(ResizeDirection::North)
        );
        assert_eq!(
            resize_direction(400.0, 597.0, w, h, b),
            Some(ResizeDirection::South)
        );
        assert_eq!(resize_direction(400.0, 300.0, w, h, b), None);
    }

    #[test]
    fn the_edge_zone_is_exactly_the_border_wide() {
        let (w, h, b) = (800.0, 600.0, 6.0);
        assert_eq!(
            resize_direction(5.9, 300.0, w, h, b),
            Some(ResizeDirection::West)
        );
        assert_eq!(resize_direction(6.0, 300.0, w, h, b), None);
        assert_eq!(
            resize_direction(794.0, 300.0, w, h, b),
            Some(ResizeDirection::East)
        );
        assert_eq!(resize_direction(793.9, 300.0, w, h, b), None);
    }

    #[test]
    fn resize_icons_match_their_direction() {
        assert_eq!(resize_icon(ResizeDirection::West), CursorIcon::EwResize);
        assert_eq!(resize_icon(ResizeDirection::South), CursorIcon::NsResize);
        assert_eq!(
            resize_icon(ResizeDirection::NorthWest),
            CursorIcon::NwseResize
        );
        assert_eq!(
            resize_icon(ResizeDirection::SouthWest),
            CursorIcon::NeswResize
        );
    }

    #[test]
    fn float_edges_get_the_matching_resize_arrow() {
        let edge = |left, right, top, bottom| Edge {
            left,
            right,
            top,
            bottom,
        };
        assert_eq!(
            edge_icon(edge(true, false, false, false)),
            CursorIcon::EwResize
        );
        assert_eq!(
            edge_icon(edge(false, true, false, false)),
            CursorIcon::EwResize
        );
        assert_eq!(
            edge_icon(edge(false, false, true, false)),
            CursorIcon::NsResize
        );
        assert_eq!(
            edge_icon(edge(false, false, false, true)),
            CursorIcon::NsResize
        );
        assert_eq!(
            edge_icon(edge(true, false, true, false)),
            CursorIcon::NwseResize
        );
        assert_eq!(
            edge_icon(edge(false, true, false, true)),
            CursorIcon::NwseResize
        );
        assert_eq!(
            edge_icon(edge(false, true, true, false)),
            CursorIcon::NeswResize
        );
        assert_eq!(
            edge_icon(edge(true, false, false, true)),
            CursorIcon::NeswResize
        );
    }

    #[test]
    fn dragging_has_an_icon_but_a_selection_does_not() {
        assert_eq!(capture_icon(&Capture::Pane(PaneId(1))), None);
        assert_eq!(
            capture_icon(&Capture::FloatMove(PaneId(1))),
            Some(CursorIcon::Move)
        );
        assert_eq!(
            capture_icon(&Capture::Divider(vec![], Axis::Vertical)),
            Some(CursorIcon::NsResize)
        );
    }

    #[test]
    fn points_map_to_cells_and_clamp_to_the_grid() {
        // padding 6, celdas de 9x19, rejilla de 80x24
        let at = |x, y| cell_at(x, y, 6.0, 9, 19, 80, 24);
        assert_eq!(at(6.0, 6.0), (0, 0));
        assert_eq!(at(14.9, 24.9), (0, 0));
        assert_eq!(at(15.0, 25.0), (1, 1));
        assert_eq!(
            at(6.0 + 79.0 * 9.0 + 4.0, 6.0 + 23.0 * 19.0 + 4.0),
            (79, 23)
        );
        // En el margen o fuera de la ventana se queda en la celda más cercana
        assert_eq!(at(0.0, 0.0), (0, 0));
        assert_eq!(at(-50.0, -50.0), (0, 0));
        assert_eq!(at(5000.0, 5000.0), (79, 23));
    }

    #[test]
    fn gesture_time_is_nanoseconds_where_the_clock_counts_nanoseconds() {
        let elapsed = Duration::from_millis(1500);
        assert_eq!(gesture_time(elapsed, 1_000_000_000), elapsed);
    }

    #[test]
    fn gesture_time_counts_ticks_for_a_10_mhz_clock() {
        // 1,5 s a 10 MHz son 15 millones de ticks, que `set_time` recibe como "nanosegundos"
        assert_eq!(
            gesture_time(Duration::from_millis(1500), 10_000_000),
            Duration::from_nanos(15_000_000)
        );
        assert_eq!(gesture_time(Duration::ZERO, 10_000_000), Duration::ZERO);
    }

    #[test]
    fn a_double_click_is_inside_the_interval_on_the_real_clock() {
        // Con el factor de la plataforma, 100 ms entre clics siguen dentro de 500 ms: libghostty
        // convierte la diferencia de ticks a nanosegundos con la misma frecuencia
        let ticks = gesture_ticks_per_second();
        let a = gesture_time(Duration::from_secs(3), ticks).as_nanos();
        let b = gesture_time(Duration::from_millis(3100), ticks).as_nanos();
        let since_ns = (b - a) * 1_000_000_000 / u128::from(ticks);
        assert!(since_ns.abs_diff(100_000_000) < 1_000_000, "{since_ns}");
        assert!(since_ns <= MULTI_CLICK_INTERVAL.as_nanos());
    }

    #[test]
    fn a_one_by_one_grid_never_underflows() {
        assert_eq!(cell_at(100.0, 100.0, 0.0, 9, 19, 1, 1), (0, 0));
        assert_eq!(cell_at(100.0, 100.0, 0.0, 9, 19, 0, 0), (0, 0));
    }

    #[test]
    fn notched_wheels_scroll_scroll_lines_per_notch() {
        let mut rest = 0.0;
        assert_eq!(
            wheel_lines(MouseScrollDelta::LineDelta(0.0, 1.0), 3, 19, &mut rest),
            3
        );
        assert_eq!(
            wheel_lines(MouseScrollDelta::LineDelta(0.0, -2.0), 3, 19, &mut rest),
            -6
        );
        assert_eq!(rest, 0.0);
    }

    #[test]
    fn touchpads_accumulate_fractions_of_a_line() {
        let mut rest = 0.0;
        let pixels = |y: f64| MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, y));
        // Celdas de 20 px: 8 px son 0,4 línea, y no se pierden entre eventos
        assert_eq!(wheel_lines(pixels(8.0), 3, 20, &mut rest), 0);
        assert_eq!(wheel_lines(pixels(8.0), 3, 20, &mut rest), 0);
        assert_eq!(wheel_lines(pixels(8.0), 3, 20, &mut rest), 1);
        assert!((rest - 0.2).abs() < 1e-9, "{rest}");
        assert_eq!(wheel_lines(pixels(-100.0), 3, 20, &mut rest), -4);
    }

    #[test]
    fn a_wheel_event_of_zero_height_does_nothing() {
        let mut rest = 0.0;
        assert_eq!(
            wheel_lines(MouseScrollDelta::LineDelta(0.0, 0.0), 3, 19, &mut rest),
            0
        );
        assert_eq!(
            wheel_lines(
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(5.0, 0.0)),
                3,
                19,
                &mut rest
            ),
            0
        );
    }

    fn terminal_with(input: &[u8]) -> libghostty_vt::Terminal<'static, 'static> {
        let mut terminal = libghostty_vt::Terminal::new(libghostty_vt::TerminalOptions {
            cols: 40,
            rows: 10,
            max_scrollback: 100,
        })
        .unwrap();
        terminal.vt_write(input);
        terminal
    }

    fn encoded(
        terminal: &libghostty_vt::Terminal<'static, 'static>,
        action: mouse::Action,
        button: Option<mouse::Button>,
        x: f32,
        y: f32,
    ) -> Vec<u8> {
        let mut encoder = mouse::Encoder::new().unwrap();
        encoder
            .set_options_from_terminal(terminal)
            .set_size(EncoderSize {
                screen_width: 400,
                screen_height: 200,
                cell_width: 10,
                cell_height: 20,
                padding_top: 0,
                padding_bottom: 0,
                padding_right: 0,
                padding_left: 0,
            })
            .set_track_last_cell(true);
        let mut event = mouse::Event::new().unwrap();
        event
            .set_action(action)
            .set_button(button)
            .set_mods(Mods::empty())
            .set_position(Position { x, y });
        let mut bytes = Vec::new();
        encoder.encode_to_vec(&event, &mut bytes).unwrap();
        bytes
    }

    #[test]
    fn a_terminal_without_mouse_mode_reports_nothing() {
        let terminal = terminal_with(b"");
        assert!(!terminal.is_mouse_tracking().unwrap());
        assert!(
            encoded(
                &terminal,
                mouse::Action::Press,
                Some(mouse::Button::Left),
                15.0,
                25.0
            )
            .is_empty()
        );
    }

    #[test]
    fn sgr_mouse_mode_encodes_presses_releases_and_wheel() {
        // 1000 = reportar pulsaciones, 1006 = formato SGR
        let terminal = terminal_with(b"\x1b[?1000h\x1b[?1006h");
        assert!(terminal.is_mouse_tracking().unwrap());

        // Celda (columna 2, fila 1) -> coordenadas de 1 en 1: 3;2
        let press = encoded(
            &terminal,
            mouse::Action::Press,
            Some(mouse::Button::Left),
            25.0,
            25.0,
        );
        assert_eq!(String::from_utf8_lossy(&press), "\x1b[<0;3;2M");
        let release = encoded(
            &terminal,
            mouse::Action::Release,
            Some(mouse::Button::Left),
            25.0,
            25.0,
        );
        assert_eq!(String::from_utf8_lossy(&release), "\x1b[<0;3;2m");
        // La rueda hacia arriba es el botón 4 (código 64) y hacia abajo el 5 (65)
        let up = encoded(
            &terminal,
            mouse::Action::Press,
            Some(mouse::Button::Four),
            25.0,
            25.0,
        );
        assert_eq!(String::from_utf8_lossy(&up), "\x1b[<64;3;2M");
        let down = encoded(
            &terminal,
            mouse::Action::Press,
            Some(mouse::Button::Five),
            25.0,
            25.0,
        );
        assert_eq!(String::from_utf8_lossy(&down), "\x1b[<65;3;2M");
    }
}
