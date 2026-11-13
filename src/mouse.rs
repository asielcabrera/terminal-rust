//! Mouse: selection gestures (click, double-click for words, triple-click for lines, drag and
//! autoscroll), copying, reporting to programs that request it, the wheel, and the edges of an
//! undecorated window. With several panes, each gesture goes to the pane under the pointer
//! (or to the one that captured it on press, for the duration of the drag).
//!
//! Window logic lives in `impl App`; the math that needs no window (which edge is under the
//! pointer, which cell a point maps to, how many lines a wheel turn is) lives in pure
//! functions so it can be tested.

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
use terminal_protocol::ipc::{MoveFloatOp, Request, ResizeFloatEdgeOp, SetDividerOp};

/// How often one line scrolls while a selection is dragged outside the window
pub const AUTOSCROLL_INTERVAL: Duration = Duration::from_millis(50);
/// Maximum time between clicks for them to count as a double or triple click
const MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(500);
/// Width, in logical pixels, of the edge band that resizes a window without a title bar
const RESIZE_BORDER: f64 = 6.0;
/// How many logical pixels from a divider between panes it can be grabbed
const DIVIDER_ZONE: f64 = 4.0;

/// What has captured the mouse while a button is held
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Capture {
    /// A selection or a report to that pane's program
    Pane(PaneId),
    /// Dragging a divider between panes
    Divider(Path, Axis),
    /// Moving a floating pane by grabbing the ring of its padding
    FloatMove(PaneId),
    /// Resizing a floating pane by an edge or a corner
    FloatResize(PaneId, Edge),
}

/// Mouse state that persists between events. Selection state (the gesture, whether a drag is
/// in progress, which button was reported) belongs to each pane.
pub struct MouseState {
    press: PressEvent<'static>,
    drag: DragEvent<'static>,
    release: ReleaseEvent<'static>,
    autoscroll_tick: AutoscrollTickEvent<'static>,
    encoder: mouse::Encoder<'static>,
    event: mouse::Event<'static>,
    /// Last pointer position, in physical window pixels
    pointer: (f64, f64),
    /// What holds the mouse while a button is pressed
    capture: Option<Capture>,
    /// Where the pointer was at the last step of a divider or floating-pane drag
    drag_from: (f64, f64),
    /// Fraction of a line left over from the last wheel turn (precision mice and touchpads)
    wheel_remainder: f64,
    /// Time origin for clicks
    clock: Instant,
    /// Units per second that libghostty expects in `set_time` (see `gesture_time`)
    ticks_per_second: u64,
    /// When the next autoscroll step is due, if one is in progress
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

    /// A pane was closed: if it held the mouse, release it
    pub fn forget_pane(&mut self, id: PaneId) {
        if self.capture == Some(Capture::Pane(id)) {
            self.capture = None;
            self.autoscroll_deadline = None;
        }
    }

    /// Takes the capture of an ongoing layout drag (a divider, or a floating pane being
    /// moved or resized), if there is one. The release that ends it can arrive through
    /// two channels that both fire for one physical release (the window's mouse-up and
    /// the device-level backstop), so running this twice must be a no-op.
    pub fn take_layout_drag(&mut self) -> bool {
        if matches!(
            self.capture,
            Some(Capture::Divider(..) | Capture::FloatMove(_) | Capture::FloatResize(..))
        ) {
            self.capture = None;
            true
        } else {
            false
        }
    }
}

fn divider_icon(axis: Axis) -> CursorIcon {
    match axis {
        Axis::Horizontal => CursorIcon::EwResize,
        Axis::Vertical => CursorIcon::NsResize,
    }
}

/// Resize arrow for an edge or a corner of a floating pane
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

/// Units per second of the time passed to `PressEvent::set_time`. The API says nanoseconds,
/// but on Windows libghostty stores that number as-is in a Zig `std.time.Instant`, which there
/// counts QueryPerformanceCounter ticks (10 MHz = 100 ns per tick) and multiplies them by 100
/// when comparing against the interval: uncorrected, a double click only registers if the two
/// clicks are less than 5 ms apart instead of 500 ms.
fn gesture_ticks_per_second() -> u64 {
    #[cfg(windows)]
    {
        unsafe extern "system" {
            fn QueryPerformanceFrequency(frequency: *mut i64) -> i32;
        }
        let mut frequency = 0i64;
        // SAFETY: takes a pointer to a valid integer and keeps nothing from it
        let ok = unsafe { QueryPerformanceFrequency(&mut frequency) };
        if ok != 0 && frequency > 0 {
            return frequency as u64;
        }
    }
    1_000_000_000
}

/// Converts elapsed time into what `set_time` expects, given `ticks_per_second`
pub fn gesture_time(elapsed: Duration, ticks_per_second: u64) -> Duration {
    let ticks = elapsed.as_nanos() * u128::from(ticks_per_second) / 1_000_000_000;
    Duration::from_nanos(ticks as u64)
}

/// Which edge or corner of a `width` x `height` window is under `(x, y)`, with a sensitive
/// border of `border` pixels. Corners take precedence over sides.
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

/// Cell (column, row) under the window point `(x, y)`. Outside the grid (in the padding or
/// beyond) it clamps to the nearest cell, as terminals do while dragging.
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

/// Lines corresponding to a wheel turn. Positive = up (into the history). A `LineDelta`
/// (notched wheel) is worth `scroll_lines` lines per notch; a `PixelDelta` (touchpad) is worth
/// its size in cells. Anything short of a whole line accumulates in `remainder`.
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
        // The edge band of a floating pane cannot eat all of its padding: what remains is the
        // ring used to move it
        let band = (RESIZE_BORDER * scale).min((padding * 0.6).max(3.0));
        Some(HitParams {
            divider_zone: DIVIDER_ZONE * scale,
            edge_band: band,
            move_ring: padding,
        })
    }

    /// Floating pane under the pointer, whether over its content, its padding or its edge
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

    /// What is under the pointer: a pane's content, a divider, a floating pane's edge...
    fn hit_at_pointer(&self) -> Option<Hit> {
        let area = self.window_area()?;
        let params = self.hit_params()?;
        let (x, y) = self.mouse.pointer;
        Some(self.layout.hit_test(x, y, area, params))
    }

    /// Pane whose content is under the pointer
    fn pane_at_pointer(&self) -> Option<usize> {
        match self.hit_at_pointer()? {
            Hit::Pane(id) => self.index_of(id),
            _ => None,
        }
    }

    /// Pane that receives the mouse: the one that captured it on button press or, otherwise,
    /// the one underneath. With a divider captured, none receives it.
    fn target_pane(&self) -> Option<usize> {
        match &self.mouse.capture {
            Some(Capture::Pane(id)) => self.index_of(*id),
            Some(_) => None,
            None => self.pane_at_pointer(),
        }
    }

    /// Pane that captured the mouse on button press
    fn captured_pane(&self) -> Option<usize> {
        match &self.mouse.capture {
            Some(Capture::Pane(id)) => self.index_of(*id),
            _ => None,
        }
    }

    /// Starts dragging a divider or a floating pane
    fn start_drag(&mut self, capture: Capture) {
        self.mouse.drag_from = self.mouse.pointer;
        self.mouse.capture = Some(capture);
        self.update_pointer_icon();
    }

    /// Moves (`edge` = `None`) or resizes a floating pane by however much the pointer moved since
    /// the last step. One message per step; the server answers with a state event and the
    /// window relayouts from its projection.
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
        match edge {
            None => self.server.send(Request::LayoutMoveFloat(MoveFloatOp {
                pane: id,
                dx,
                dy,
                window: area,
            })),
            Some(edge) => {
                self.server
                    .send(Request::LayoutResizeFloatEdge(ResizeFloatEdgeOp {
                        pane: id,
                        edge,
                        dx,
                        dy,
                        window: area,
                        limits,
                    }));
            }
        }
    }

    /// Moves the captured divider to where the pointer is: one message per drag step
    fn drag_divider(&mut self, path: &[bool], axis: Axis) {
        let (Some(area), Some(limits)) = (self.window_area(), self.pane_limits()) else {
            return;
        };
        let (x, y) = self.mouse.pointer;
        let boundary = match axis {
            Axis::Horizontal => x,
            Axis::Vertical => y,
        };
        self.server.send(Request::LayoutSetDivider(SetDividerOp {
            path: path.to_vec(),
            boundary: boundary.round() as i64,
            area,
            limits,
        }));
    }

    /// Pointer position relative to the pane's corner
    fn local_pointer(&self, idx: usize) -> (f64, f64) {
        let rect = self.panes[idx].rect;
        (
            self.mouse.pointer.0 - f64::from(rect.x),
            self.mouse.pointer.1 - f64::from(rect.y),
        )
    }

    /// Did that pane's program ask to receive the mouse? With Shift held the mouse always
    /// belongs to the terminal (selection), as in any emulator.
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

    /// Window edge under the pointer, only if the window is undecorated and not maximized
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

    /// Cell (column, row) of the pane under the pointer
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

    /// System cursor based on what is under the pointer: resize arrows on the edges of a window
    /// without a title bar, the normal arrow if the program owns the mouse, and the text cursor
    /// everywhere else
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
                // Window without a title bar: edges resize it, the configured gesture moves it
                if let Some(window) = self.window.as_ref().filter(|_| self.undecorated())
                    && let Some(direction) = self.edge_under_pointer()
                {
                    let _ = window.drag_resize_window(direction);
                    return;
                }
                if self.mod_bits == self.config.window.drag_modifiers.0 {
                    // The same gesture moves a floating pane under the pointer, from anywhere
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
                // A divider between panes, or a floating pane's padding and edges, can be grabbed
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
            (MouseButton::Left, false) => self.on_left_released(),
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

    /// The left button went up, whichever channel reported it: the window's mouse-up or
    /// the device-level backstop (some setups eat the window-level release, and AppKit
    /// then keeps delivering drags as if the button were still down). Ends a layout drag
    /// or a pane's report or selection; running twice for one release is a no-op.
    pub fn on_left_released(&mut self) {
        if self.mouse.take_layout_drag() {
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

    /// Once the pane no longer has a button pressed, it stops holding the mouse capture
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
            // The wheel is "button" 4 (up) or 5 (down); one notch is one event
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
            // Alternate screen without mouse (less, man...): the wheel sends arrow keys
            let key = if up { Key::ArrowUp } else { Key::ArrowDown };
            self.send_key_repeatedly(idx, key, lines.unsigned_abs());
        } else {
            self.scroll_pane(idx, ScrollViewport::Delta(-(lines as isize)));
        }
    }

    /// Sends a pane's program a mouse event encoded in the protocol it requested
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

    /// Left press: starts a selection (or extends it to a word or line on double or
    /// triple click)
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
            Err(error) => eprintln!("[mouse] could not start the selection: {error}"),
        }
        pane.selecting = true;
        self.redraw();
    }

    /// The pointer moves with the left button held: the selection follows the pointer
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

        // If the pointer left through the top or bottom, the history keeps scrolling
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

    /// One autoscroll step, when its deadline is reached
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

    /// Releasing the left button ends the selection (and copies it if `copy_on_select`)
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

    /// Copies a pane's selection to the clipboard, without soft-wrap line breaks or trailing
    /// spaces
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
            eprintln!("[copy] could not write to the clipboard: {error}");
        }
    }

    /// Copies the focused pane's selection
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

    /// Clears the focused pane's selection
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
        // padding 6, 9x19 cells, 80x24 grid
        let at = |x, y| cell_at(x, y, 6.0, 9, 19, 80, 24);
        assert_eq!(at(6.0, 6.0), (0, 0));
        assert_eq!(at(14.9, 24.9), (0, 0));
        assert_eq!(at(15.0, 25.0), (1, 1));
        assert_eq!(
            at(6.0 + 79.0 * 9.0 + 4.0, 6.0 + 23.0 * 19.0 + 4.0),
            (79, 23)
        );
        // In the padding or outside the window it clamps to the nearest cell
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
        // 1.5 s at 10 MHz is 15 million ticks, which `set_time` receives as "nanoseconds"
        assert_eq!(
            gesture_time(Duration::from_millis(1500), 10_000_000),
            Duration::from_nanos(15_000_000)
        );
        assert_eq!(gesture_time(Duration::ZERO, 10_000_000), Duration::ZERO);
    }

    #[test]
    fn a_double_click_is_inside_the_interval_on_the_real_clock() {
        // With the platform factor, 100 ms between clicks stays within 500 ms: libghostty
        // converts the tick difference to nanoseconds with the same frequency
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
        // 20 px cells: 8 px is 0.4 of a line, and it is not lost between events
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

    #[test]
    fn a_late_release_signal_clears_a_layout_drag_exactly_once() {
        // The device-level release backstop and the window release can both fire for
        // one physical release: the first clears the drag, the second must not re-arm
        // or need the first's state
        let mut mouse = MouseState::new().unwrap();
        mouse.capture = Some(Capture::Divider(vec![], Axis::Horizontal));
        assert!(mouse.take_layout_drag());
        assert_eq!(mouse.capture, None);
        assert!(!mouse.take_layout_drag());
        assert_eq!(mouse.capture, None);

        mouse.capture = Some(Capture::FloatMove(PaneId(7)));
        assert!(mouse.take_layout_drag());
        assert_eq!(mouse.capture, None);

        mouse.capture = Some(Capture::FloatResize(PaneId(7), Edge::default()));
        assert!(mouse.take_layout_drag());
        assert_eq!(mouse.capture, None);

        // A pane capture is not a layout drag: its release goes through the pane flow
        mouse.capture = Some(Capture::Pane(PaneId(3)));
        assert!(!mouse.take_layout_drag());
        assert_eq!(mouse.capture, Some(Capture::Pane(PaneId(3))));

        // Nothing held: the backstop is a no-op
        mouse.capture = None;
        assert!(!mouse.take_layout_drag());
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
        // 1000 = report presses, 1006 = SGR format
        let terminal = terminal_with(b"\x1b[?1000h\x1b[?1006h");
        assert!(terminal.is_mouse_tracking().unwrap());

        // Cell (column 2, row 1) -> 1-based coordinates: 3;2
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
        // Wheel up is button 4 (code 64) and wheel down is 5 (65)
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
