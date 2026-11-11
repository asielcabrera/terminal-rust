//! Pane geometry: the tiling split tree, floating panes, focus, zoom and what lies under a
//! point. It knows nothing about windows, the GPU or PTYs: it takes rectangles in physical
//! pixels and returns rectangles, so everything can be tested with plain tests.

use serde::{Deserialize, Serialize};

/// Pane identifier; never reused during the app's lifetime
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PaneId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub const fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> u32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> u32 {
        self.y + self.h
    }

    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= f64::from(self.x)
            && x < f64::from(self.right())
            && y >= f64::from(self.y)
            && y < f64::from(self.bottom())
    }
}

/// Minimum pane size (padding included), in pixels
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub min_w: u32,
    pub min_h: u32,
}

/// How a split's children are laid out
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Axis {
    /// Side by side (the divider is a vertical line): "split right"
    Horizontal,
    /// One above the other (the divider is a horizontal line): "split down"
    Vertical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

impl Dir {
    pub(crate) fn axis(self) -> Axis {
        match self {
            Dir::Left | Dir::Right => Axis::Horizontal,
            Dir::Up | Dir::Down => Axis::Vertical,
        }
    }

    /// Does it grow toward larger coordinates?
    fn positive(self) -> bool {
        matches!(self, Dir::Right | Dir::Down)
    }
}

/// Edges of a floating pane under the pointer (two at once at a corner)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    pub left: bool,
    pub right: bool,
    pub top: bool,
    pub bottom: bool,
}

/// Path to a split: `false` = child `a` (left/top), `true` = child `b`
pub type Path = Vec<bool>;

/// Line separating a split's two children. `pos` is the boundary coordinate on the split's
/// axis and `start..start + len` is its extent on the other axis.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Divider {
    pub path: Path,
    pub axis: Axis,
    pub pos: u32,
    pub start: u32,
    pub len: u32,
}

impl Divider {
    /// Rectangle of the line, `thickness` pixels thick, centered on the boundary
    pub fn rect(&self, thickness: u32) -> Rect {
        let before = self.pos.saturating_sub(thickness / 2);
        match self.axis {
            Axis::Horizontal => Rect::new(before, self.start, thickness, self.len),
            Axis::Vertical => Rect::new(self.start, before, self.len, thickness),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hit {
    Pane(PaneId),
    Divider(Path, Axis),
    /// The padding ring of a floating pane: dragging it moves the pane
    FloatMove(PaneId),
    FloatResize(PaneId, Edge),
    None,
}

/// `hit_test` tolerances, in physical pixels
#[derive(Clone, Copy, Debug)]
pub struct HitParams {
    /// How many pixels from a divider it can be grabbed
    pub divider_zone: f64,
    /// Width of the edge band of a floating pane that resizes it
    pub edge_band: f64,
    /// Width of a floating pane's ring (the pane padding) that moves it
    pub move_ring: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Node {
    Leaf(PaneId),
    Split {
        axis: Axis,
        ratio: f32,
        a: Box<Node>,
        b: Box<Node>,
    },
}

/// Splits `total` pixels between the two children; each gets at least one if there is room
fn split_sizes(total: u32, ratio: f32) -> (u32, u32) {
    let first = (total as f32 * ratio).round() as u32;
    let first = first.clamp(1, total.saturating_sub(1).max(1)).min(total);
    (first, total - first)
}

impl Node {
    fn contains(&self, id: PaneId) -> bool {
        match self {
            Node::Leaf(leaf) => *leaf == id,
            Node::Split { a, b, .. } => a.contains(id) || b.contains(id),
        }
    }

    fn leaves(&self, out: &mut Vec<PaneId>) {
        match self {
            Node::Leaf(id) => out.push(*id),
            Node::Split { a, b, .. } => {
                a.leaves(out);
                b.leaves(out);
            }
        }
    }

    /// Minimum subtree size: the sum of the minimums on the split axis, the largest on the other
    fn min_size(&self, limits: Limits) -> (u32, u32) {
        match self {
            Node::Leaf(_) => (limits.min_w, limits.min_h),
            Node::Split { axis, a, b, .. } => {
                let (aw, ah) = a.min_size(limits);
                let (bw, bh) = b.min_size(limits);
                match axis {
                    Axis::Horizontal => (aw + bw, ah.max(bh)),
                    Axis::Vertical => (aw.max(bw), ah + bh),
                }
            }
        }
    }

    fn layout(
        &self,
        area: Rect,
        path: &mut Path,
        rects: &mut Vec<(PaneId, Rect)>,
        dividers: &mut Vec<Divider>,
    ) {
        match self {
            Node::Leaf(id) => rects.push((*id, area)),
            Node::Split { axis, ratio, a, b } => {
                let (first, second) = match axis {
                    Axis::Horizontal => split_sizes(area.w, *ratio),
                    Axis::Vertical => split_sizes(area.h, *ratio),
                };
                let (area_a, area_b, divider) = match axis {
                    Axis::Horizontal => (
                        Rect::new(area.x, area.y, first, area.h),
                        Rect::new(area.x + first, area.y, second, area.h),
                        Divider {
                            path: path.clone(),
                            axis: *axis,
                            pos: area.x + first,
                            start: area.y,
                            len: area.h,
                        },
                    ),
                    Axis::Vertical => (
                        Rect::new(area.x, area.y, area.w, first),
                        Rect::new(area.x, area.y + first, area.w, second),
                        Divider {
                            path: path.clone(),
                            axis: *axis,
                            pos: area.y + first,
                            start: area.x,
                            len: area.w,
                        },
                    ),
                };
                dividers.push(divider);
                path.push(false);
                a.layout(area_a, path, rects, dividers);
                path.pop();
                path.push(true);
                b.layout(area_b, path, rects, dividers);
                path.pop();
            }
        }
    }

    fn node_at(&self, path: &[bool]) -> Option<&Node> {
        match (path.split_first(), self) {
            (None, node) => Some(node),
            (Some((&second, rest)), Node::Split { a, b, .. }) => {
                if second { b } else { a }.node_at(rest)
            }
            _ => None,
        }
    }

    fn node_at_mut(&mut self, path: &[bool]) -> Option<&mut Node> {
        match (path.split_first(), self) {
            (None, node) => Some(node),
            (Some((&second, rest)), Node::Split { a, b, .. }) => {
                if second { b } else { a }.node_at_mut(rest)
            }
            _ => None,
        }
    }

    fn path_to(&self, id: PaneId, path: &mut Path) -> bool {
        match self {
            Node::Leaf(leaf) => *leaf == id,
            Node::Split { a, b, .. } => {
                path.push(false);
                if a.path_to(id, path) {
                    return true;
                }
                path.pop();
                path.push(true);
                if b.path_to(id, path) {
                    return true;
                }
                path.pop();
                false
            }
        }
    }

    /// Replaces leaf `target` with a split holding it and the new leaf in equal halves
    fn split_leaf(&mut self, target: PaneId, axis: Axis, new_id: PaneId) -> bool {
        if matches!(self, Node::Leaf(id) if *id == target) {
            *self = Node::Split {
                axis,
                ratio: 0.5,
                a: Box::new(Node::Leaf(target)),
                b: Box::new(Node::Leaf(new_id)),
            };
            return true;
        }
        match self {
            Node::Leaf(_) => false,
            Node::Split { a, b, .. } => {
                a.split_leaf(target, axis, new_id) || b.split_leaf(target, axis, new_id)
            }
        }
    }

    /// Removes leaf `id`: its sibling takes the split's place. `None` if the node was that leaf.
    fn remove(self, id: PaneId) -> Option<Node> {
        match self {
            Node::Leaf(leaf) => (leaf != id).then_some(Node::Leaf(leaf)),
            Node::Split { axis, ratio, a, b } => match (a.remove(id), b.remove(id)) {
                (Some(a), Some(b)) => Some(Node::Split {
                    axis,
                    ratio,
                    a: Box::new(a),
                    b: Box::new(b),
                }),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            },
        }
    }
}

/// A floating pane, in window fractions, so it stays in place when the window resizes
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Float {
    pub id: PaneId,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// The whole layout as plain data. The server owns the authoritative layout and sends this to
/// the window after every applied mutation; the window rebuilds a read-only projection from it
/// ([`Layout::from_state`]). It also travels: the wire carries it as JSON, field for field.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayoutState {
    pub root: Node,
    pub focus: PaneId,
    pub zoomed: Option<PaneId>,
    pub floats: Vec<Float>,
    pub floats_visible: bool,
    /// Focused panes, oldest to most recent. Part of the state so the projection answers
    /// recency queries (`split_target`, focus cycling) exactly like the server's layout.
    pub history: Vec<PaneId>,
}

impl LayoutState {
    /// Every pane in the state, tiled and floating
    pub fn pane_ids(&self) -> Vec<PaneId> {
        let mut ids = Vec::new();
        self.root.leaves(&mut ids);
        ids.extend(self.floats.iter().map(|pane| pane.id));
        ids
    }
}

/// What happened when closing a pane
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Closed {
    Pane,
    /// It was the last tiled pane: the window closes
    LastTiled,
    /// It did not exist
    Unknown,
}

/// How many panes the focus history remembers
const HISTORY_LIMIT: usize = 32;
/// Offset (window fraction) between new floating panes, so they do not stack exactly
const FLOAT_CASCADE: f32 = 0.03;
const MIN_FLOAT_FRACTION: f32 = 0.1;

pub struct Layout {
    root: Node,
    focus: PaneId,
    zoomed: Option<PaneId>,
    floats: Vec<Float>,
    floats_visible: bool,
    /// Focused panes, oldest to most recent
    history: Vec<PaneId>,
    /// Built by `from_state`: the window's projection of the server's layout, which must never
    /// be mutated directly (debug builds catch it in `assert_authoritative`)
    projection: bool,
}

impl Layout {
    pub fn new(first: PaneId) -> Self {
        Self {
            root: Node::Leaf(first),
            focus: first,
            zoomed: None,
            floats: Vec::new(),
            floats_visible: true,
            history: vec![first],
            projection: false,
        }
    }

    /// Snapshot of the whole layout: what the server sends after every applied mutation
    pub fn state(&self) -> LayoutState {
        LayoutState {
            root: self.root.clone(),
            focus: self.focus,
            zoomed: self.zoomed,
            floats: self.floats.clone(),
            floats_visible: self.floats_visible,
            history: self.history.clone(),
        }
    }

    /// Rebuilds a layout from plain data. The result is the window's read-only projection of
    /// the server's layout: its mutations are refused in debug builds.
    pub fn from_state(state: LayoutState) -> Self {
        Self {
            root: state.root,
            focus: state.focus,
            zoomed: state.zoomed,
            floats: state.floats,
            floats_visible: state.floats_visible,
            history: state.history,
            projection: true,
        }
    }

    /// The window's projection is rebuilt from server events; mutating it here would diverge
    /// from the server's authority
    fn assert_authoritative(&self) {
        debug_assert!(
            !self.projection,
            "the layout projection is read-only: mutations go through the server"
        );
    }

    pub fn focus(&self) -> PaneId {
        self.focus
    }

    /// The pane zoomed to fill the window, if any
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "a projection query; the window currently reads zoom through rects"
        )
    )]
    pub fn zoomed(&self) -> Option<PaneId> {
        self.zoomed
    }

    #[cfg(test)]
    pub fn floats_visible(&self) -> bool {
        self.floats_visible
    }

    pub fn has_floats(&self) -> bool {
        !self.floats.is_empty()
    }

    pub fn is_float(&self, id: PaneId) -> bool {
        self.floats.iter().any(|float| float.id == id)
    }

    pub fn contains(&self, id: PaneId) -> bool {
        self.root.contains(id) || self.is_float(id)
    }

    /// Tiled panes, in tree traversal order
    pub fn tiled_ids(&self) -> Vec<PaneId> {
        let mut ids = Vec::new();
        self.root.leaves(&mut ids);
        ids
    }

    /// Every pane that exists (tiled and floating, visible or not)
    #[cfg(test)]
    pub fn all_ids(&self) -> Vec<PaneId> {
        let mut ids = self.tiled_ids();
        ids.extend(self.floats.iter().map(|float| float.id));
        ids
    }

    /// Panes visible right now, in draw order (bottom to top): the tiled ones (when zoomed, only
    /// the zoomed one) and the floating ones on top unless hidden
    pub fn visible_ids(&self) -> Vec<PaneId> {
        let mut ids = match self.zoomed {
            Some(zoomed) => vec![zoomed],
            None => self.tiled_ids(),
        };
        if self.floats_visible {
            ids.extend(self.floats.iter().map(|float| float.id));
        }
        ids
    }

    #[cfg(test)]
    pub fn visible_count(&self) -> usize {
        self.visible_ids().len()
    }

    fn remember(&mut self, id: PaneId) {
        self.history.retain(|&other| other != id);
        self.history.push(id);
        if self.history.len() > HISTORY_LIMIT {
            self.history.remove(0);
        }
    }

    fn recency(&self, id: PaneId) -> i64 {
        self.history
            .iter()
            .position(|&other| other == id)
            .map_or(-1, |index| index as i64)
    }

    /// Gives focus to a pane. A floating pane is raised to the front; moving to another tiled pane
    /// clears zoom; focusing a hidden floating pane shows them all.
    pub fn set_focus(&mut self, id: PaneId) -> bool {
        self.assert_authoritative();
        if !self.contains(id) {
            return false;
        }
        if self.is_float(id) {
            self.floats_visible = true;
            self.raise(id);
        } else if self.zoomed.is_some_and(|zoomed| zoomed != id) {
            self.zoomed = None;
        }
        self.focus = id;
        self.remember(id);
        true
    }

    fn raise(&mut self, id: PaneId) {
        if let Some(index) = self.floats.iter().position(|float| float.id == id) {
            let float = self.floats.remove(index);
            self.floats.push(float);
        }
    }

    /// Last tiled pane that had focus (the first in the tree if there is no history)
    fn last_tiled(&self) -> PaneId {
        self.history
            .iter()
            .rev()
            .copied()
            .find(|&id| self.root.contains(id))
            .unwrap_or_else(|| self.tiled_ids()[0])
    }

    // ---- Tiling ----------------------------------------------------------------------------

    /// Tiled pane a split applies to: the focused one or, if focus is on a floating pane,
    /// the last tiled pane that had it
    pub fn split_target(&self) -> PaneId {
        if self.is_float(self.focus) {
            self.last_tiled()
        } else {
            self.focus
        }
    }

    /// Splits tiled pane `target` in two and focuses the new one. `false` if `target` is not
    /// tiled or if both halves would fall below the minimum size.
    pub fn split(
        &mut self,
        target: PaneId,
        axis: Axis,
        new_id: PaneId,
        area: Rect,
        limits: Limits,
    ) -> bool {
        self.assert_authoritative();
        let before = self.zoomed.take();
        let Some((_, rect)) = self.rects(area).into_iter().find(|(id, _)| *id == target) else {
            self.zoomed = before;
            return false;
        };
        // With zoom on, `rects` only returned the zoomed pane: recompute without zoom
        let rect = if before.is_some() {
            self.rects(area)
                .into_iter()
                .find(|(id, _)| *id == target)
                .map_or(rect, |(_, r)| r)
        } else {
            rect
        };
        let fits = match axis {
            Axis::Horizontal => rect.w / 2 >= limits.min_w,
            Axis::Vertical => rect.h / 2 >= limits.min_h,
        };
        if !fits || !self.root.split_leaf(target, axis, new_id) {
            self.zoomed = before;
            return false;
        }
        self.set_focus(new_id);
        true
    }

    /// Rectangle a new pane would occupy if `target` were split in two along `axis`, without
    /// changing anything. The window uses it to size the new pane's grid before asking the
    /// server for the split. `None` when the server would refuse the split (target unknown,
    /// or the halves would fall below the minimum size).
    pub fn split_preview(
        &self,
        target: PaneId,
        axis: Axis,
        area: Rect,
        limits: Limits,
    ) -> Option<Rect> {
        // When zoomed, `rects` only covers the zoomed pane: look at the tiling underneath
        let rects = if self.zoomed.is_some() {
            self.tiled_rects(area)
        } else {
            self.rects(area)
        };
        let (_, rect) = rects.into_iter().find(|(id, _)| *id == target)?;
        let fits = match axis {
            Axis::Horizontal => rect.w / 2 >= limits.min_w,
            Axis::Vertical => rect.h / 2 >= limits.min_h,
        };
        if !fits {
            return None;
        }
        // The new pane is the second child of the new split (right/bottom half), as `split_leaf`
        // builds it with a 0.5 ratio
        let total = match axis {
            Axis::Horizontal => rect.w,
            Axis::Vertical => rect.h,
        };
        let (first, _) = split_sizes(total, 0.5);
        Some(match axis {
            Axis::Horizontal => Rect::new(rect.x + first, rect.y, total - first, rect.h),
            Axis::Vertical => Rect::new(rect.x, rect.y + first, rect.w, total - first),
        })
    }

    /// Closes a pane. Closing the last tiled pane changes nothing and reports it so the window
    /// can close.
    pub fn close(&mut self, id: PaneId) -> Closed {
        self.assert_authoritative();
        if let Some(index) = self.floats.iter().position(|float| float.id == id) {
            self.floats.remove(index);
            self.forget(id);
            return Closed::Pane;
        }
        if !self.root.contains(id) {
            return Closed::Unknown;
        }
        if matches!(self.root, Node::Leaf(_)) {
            return Closed::LastTiled;
        }
        let root = std::mem::replace(&mut self.root, Node::Leaf(id));
        self.root = root.remove(id).unwrap_or(Node::Leaf(id));
        self.zoomed = None;
        self.forget(id);
        Closed::Pane
    }

    /// Removes a pane from the history and, if it had focus, passes it to the previous holder
    fn forget(&mut self, id: PaneId) {
        self.history.retain(|&other| other != id);
        if self.zoomed == Some(id) {
            self.zoomed = None;
        }
        if self.focus != id {
            return;
        }
        let next = self
            .history
            .iter()
            .rev()
            .copied()
            .find(|&other| self.contains(other) && (self.floats_visible || !self.is_float(other)))
            .unwrap_or_else(|| self.tiled_ids()[0]);
        self.focus = next;
        self.remember(next);
    }

    /// Rectangles of the tiled panes. When zoomed, only the zoomed one, filling the whole area.
    pub fn rects(&self, area: Rect) -> Vec<(PaneId, Rect)> {
        if let Some(zoomed) = self.zoomed {
            return vec![(zoomed, area)];
        }
        self.tiled_rects(area)
    }

    /// Rectangles of the tiling, without taking zoom into account
    fn tiled_rects(&self, area: Rect) -> Vec<(PaneId, Rect)> {
        let mut rects = Vec::new();
        let mut dividers = Vec::new();
        self.root
            .layout(area, &mut Vec::new(), &mut rects, &mut dividers);
        rects
    }

    /// Tiled dividers (none when zoomed)
    pub fn dividers(&self, area: Rect) -> Vec<Divider> {
        if self.zoomed.is_some() {
            return Vec::new();
        }
        let mut rects = Vec::new();
        let mut dividers = Vec::new();
        self.root
            .layout(area, &mut Vec::new(), &mut rects, &mut dividers);
        dividers
    }

    /// Moves focus to the neighboring pane in that direction (within the tiling). Clears zoom.
    /// `false` if focus is on a floating pane or there is no neighbor.
    pub fn focus_dir(&mut self, dir: Dir, area: Rect) -> bool {
        self.assert_authoritative();
        if self.is_float(self.focus) {
            return false;
        }
        self.zoomed = None;
        let rects = self.rects(area);
        let Some(&(_, current)) = rects.iter().find(|(id, _)| *id == self.focus) else {
            return false;
        };
        let overlap = |a0: u32, a1: u32, b0: u32, b1: u32| a1.min(b1).saturating_sub(a0.max(b0));
        let best = rects
            .iter()
            .filter(|(id, _)| *id != self.focus)
            .filter_map(|&(id, rect)| {
                let (distance, shared) = match dir {
                    Dir::Left if rect.right() <= current.x => (
                        current.x - rect.right(),
                        overlap(current.y, current.bottom(), rect.y, rect.bottom()),
                    ),
                    Dir::Right if rect.x >= current.right() => (
                        rect.x - current.right(),
                        overlap(current.y, current.bottom(), rect.y, rect.bottom()),
                    ),
                    Dir::Up if rect.bottom() <= current.y => (
                        current.y - rect.bottom(),
                        overlap(current.x, current.right(), rect.x, rect.right()),
                    ),
                    Dir::Down if rect.y >= current.bottom() => (
                        rect.y - current.bottom(),
                        overlap(current.x, current.right(), rect.x, rect.right()),
                    ),
                    _ => return None,
                };
                (shared > 0).then_some((id, distance, shared))
            })
            .min_by_key(|&(id, distance, shared)| {
                (
                    distance,
                    std::cmp::Reverse(shared),
                    std::cmp::Reverse(self.recency(id)),
                )
            })
            .map(|(id, ..)| id);
        match best {
            Some(id) => self.set_focus(id),
            None => false,
        }
    }

    /// Next (or previous) pane: tiled panes first, then visible floating panes
    pub fn focus_cycle(&mut self, forward: bool) -> bool {
        self.assert_authoritative();
        let mut order = self.tiled_ids();
        if self.floats_visible {
            order.extend(self.floats.iter().map(|float| float.id));
        }
        let Some(index) = order.iter().position(|&id| id == self.focus) else {
            return false;
        };
        let next = if forward {
            order[(index + 1) % order.len()]
        } else {
            order[(index + order.len() - 1) % order.len()]
        };
        next != self.focus && self.set_focus(next)
    }

    /// Moves the pane's edge in the given direction by `delta` pixels (growing toward that
    /// side). If the pane has no edge there (it sits at the window boundary) the opposite edge
    /// moves instead and the pane shrinks. For a floating pane, right/down grow; left/up shrink.
    pub fn resize(&mut self, id: PaneId, dir: Dir, delta: u32, area: Rect, limits: Limits) -> bool {
        self.assert_authoritative();
        if self.is_float(id) {
            let delta = delta as i64;
            let (dw, dh) = match dir {
                Dir::Right => (delta, 0),
                Dir::Left => (-delta, 0),
                Dir::Down => (0, delta),
                Dir::Up => (0, -delta),
            };
            return self.resize_float_by(id, dw, dh, area, limits);
        }
        let mut path = Vec::new();
        if self.zoomed.is_some() || !self.root.path_to(id, &mut path) {
            return false;
        }
        // Nearest to farthest: the first ancestor with that axis and the pane on the matching side
        // (first, the one that leaves the edge in the requested direction; otherwise, the opposite)
        let ancestors = (0..path.len()).rev();
        let mut chosen = None;
        for outward in [true, false] {
            chosen = ancestors.clone().find(|&depth| {
                let Some(Node::Split { axis, .. }) = self.root.node_at(&path[..depth]) else {
                    return false;
                };
                let in_second = path[depth];
                *axis == dir.axis() && (in_second == !dir.positive()) == outward
            });
            if chosen.is_some() {
                break;
            }
        }
        let Some(depth) = chosen else {
            return false;
        };
        let split_path = &path[..depth];
        let Some(rect) = self.node_rect(area, split_path) else {
            return false;
        };
        let Some(Node::Split { axis, ratio, .. }) = self.root.node_at(split_path) else {
            return false;
        };
        let total = match axis {
            Axis::Horizontal => rect.w,
            Axis::Vertical => rect.h,
        };
        let (first, _) = split_sizes(total, *ratio);
        let target = if dir.positive() {
            first as i64 + delta as i64
        } else {
            first as i64 - delta as i64
        };
        self.set_first_size(split_path, target, area, limits)
    }

    /// Rectangle occupied by the node `path` leads to
    fn node_rect(&self, area: Rect, path: &[bool]) -> Option<Rect> {
        let mut node = &self.root;
        let mut rect = area;
        for &second in path {
            let Node::Split { axis, ratio, a, b } = node else {
                return None;
            };
            let (first, rest) = match axis {
                Axis::Horizontal => split_sizes(rect.w, *ratio),
                Axis::Vertical => split_sizes(rect.h, *ratio),
            };
            rect = match (axis, second) {
                (Axis::Horizontal, false) => Rect::new(rect.x, rect.y, first, rect.h),
                (Axis::Horizontal, true) => Rect::new(rect.x + first, rect.y, rest, rect.h),
                (Axis::Vertical, false) => Rect::new(rect.x, rect.y, rect.w, first),
                (Axis::Vertical, true) => Rect::new(rect.x, rect.y + first, rect.w, rest),
            };
            node = if second { b } else { a };
        }
        Some(rect)
    }

    /// Sets how many pixels the first child of the split at `path` takes, respecting the minimum
    /// of both subtrees
    fn set_first_size(&mut self, path: &[bool], wanted: i64, area: Rect, limits: Limits) -> bool {
        let Some(rect) = self.node_rect(area, path) else {
            return false;
        };
        let Some(Node::Split { axis, ratio, a, b }) = self.root.node_at_mut(path) else {
            return false;
        };
        let (total, min_a, min_b) = match axis {
            Axis::Horizontal => (rect.w, a.min_size(limits).0, b.min_size(limits).0),
            Axis::Vertical => (rect.h, a.min_size(limits).1, b.min_size(limits).1),
        };
        if min_a + min_b > total {
            return false;
        }
        let first = wanted.clamp(i64::from(min_a), i64::from(total - min_b)) as u32;
        let new_ratio = first as f32 / total.max(1) as f32;
        let changed = split_sizes(total, new_ratio) != split_sizes(total, *ratio);
        *ratio = new_ratio;
        changed
    }

    /// Mouse drag of a divider: moves the boundary to coordinate `boundary`
    pub fn set_divider(
        &mut self,
        path: &[bool],
        boundary: i64,
        area: Rect,
        limits: Limits,
    ) -> bool {
        self.assert_authoritative();
        let Some(rect) = self.node_rect(area, path) else {
            return false;
        };
        let Some(Node::Split { axis, .. }) = self.root.node_at(path) else {
            return false;
        };
        let origin = match axis {
            Axis::Horizontal => rect.x,
            Axis::Vertical => rect.y,
        };
        self.set_first_size(path, boundary - i64::from(origin), area, limits)
    }

    /// Zooms the focused pane to fill the window, or returns to the tiling. Only with a tiled pane
    /// focused and at least one other.
    pub fn toggle_zoom(&mut self) -> bool {
        self.assert_authoritative();
        if self.zoomed.is_some() {
            self.zoomed = None;
            return true;
        }
        if self.is_float(self.focus) || self.tiled_ids().len() < 2 {
            return false;
        }
        self.zoomed = Some(self.focus);
        true
    }

    // ---- Floating panes --------------------------------------------------------------------

    /// Placement (fractions) of a new floating pane: centered, with a small offset for each
    /// existing one. Shared by `add_float` and `float_preview`.
    fn float_placement(&self, width: f32, height: f32) -> (f32, f32, f32, f32) {
        let w = width.clamp(MIN_FLOAT_FRACTION, 1.0);
        let h = height.clamp(MIN_FLOAT_FRACTION, 1.0);
        let shift = FLOAT_CASCADE * (self.floats.len() % 5) as f32;
        let x = ((1.0 - w) / 2.0 + shift).clamp(0.0, 1.0 - w);
        let y = ((1.0 - h) / 2.0 + shift).clamp(0.0, 1.0 - h);
        (x, y, w, h)
    }

    /// Adds a centered floating pane (with a small offset for each existing one), shows it
    /// and focuses it
    pub fn add_float(&mut self, id: PaneId, width: f32, height: f32) {
        self.assert_authoritative();
        let (x, y, w, h) = self.float_placement(width, height);
        self.floats.push(Float { id, x, y, w, h });
        self.floats_visible = true;
        self.set_focus(id);
    }

    /// Rectangle a new floating pane would occupy, without changing anything. The window uses
    /// it to size the new pane's grid before asking the server for the float.
    pub fn float_preview(&self, width: f32, height: f32, window: Rect) -> Rect {
        let (x, y, w, h) = self.float_placement(width, height);
        self.float_px(
            &Float {
                id: PaneId(0),
                x,
                y,
                w,
                h,
            },
            window,
        )
    }

    /// Hides or shows all floating panes. Hiding returns focus to the tiling; showing returns
    /// it to the last floating pane that had it. Returns whether any exist (else nothing to show).
    pub fn toggle_floats(&mut self) -> bool {
        self.assert_authoritative();
        if self.floats.is_empty() {
            return false;
        }
        if self.floats_visible {
            self.floats_visible = false;
            if self.is_float(self.focus) {
                let back = self.last_tiled();
                self.focus = back;
                self.remember(back);
            }
        } else {
            self.floats_visible = true;
            let last = self
                .history
                .iter()
                .rev()
                .copied()
                .find(|&id| self.is_float(id))
                .or_else(|| self.floats.last().map(|float| float.id));
            if let Some(id) = last {
                self.set_focus(id);
            }
        }
        true
    }

    fn float_px(&self, float: &Float, window: Rect) -> Rect {
        let w = ((float.w * window.w as f32).round() as u32).clamp(1, window.w.max(1));
        let h = ((float.h * window.h as f32).round() as u32).clamp(1, window.h.max(1));
        let x = ((float.x * window.w as f32).round() as u32).min(window.w.saturating_sub(w));
        let y = ((float.y * window.h as f32).round() as u32).min(window.h.saturating_sub(h));
        Rect::new(window.x + x, window.y + y, w, h)
    }

    pub fn float_rect(&self, id: PaneId, window: Rect) -> Option<Rect> {
        self.floats
            .iter()
            .find(|float| float.id == id)
            .map(|float| self.float_px(float, window))
    }

    /// Visible floating panes, from bottom-most to top-most
    pub fn float_rects(&self, window: Rect) -> Vec<(PaneId, Rect)> {
        if !self.floats_visible {
            return Vec::new();
        }
        self.floats
            .iter()
            .map(|float| (float.id, self.float_px(float, window)))
            .collect()
    }

    fn set_float_px(&mut self, id: PaneId, rect: Rect, window: Rect) {
        let Some(float) = self.floats.iter_mut().find(|float| float.id == id) else {
            return;
        };
        let (ww, wh) = (window.w.max(1) as f32, window.h.max(1) as f32);
        float.x = (rect.x - window.x) as f32 / ww;
        float.y = (rect.y - window.y) as f32 / wh;
        float.w = rect.w as f32 / ww;
        float.h = rect.h as f32 / wh;
    }

    /// Moves a floating pane without leaving the window
    pub fn move_float(&mut self, id: PaneId, dx: i64, dy: i64, window: Rect) -> bool {
        self.assert_authoritative();
        let Some(rect) = self.float_rect(id, window) else {
            return false;
        };
        let max_x = i64::from(window.right() - rect.w);
        let max_y = i64::from(window.bottom() - rect.h);
        let x = (i64::from(rect.x) + dx).clamp(i64::from(window.x), max_x.max(i64::from(window.x)))
            as u32;
        let y = (i64::from(rect.y) + dy).clamp(i64::from(window.y), max_y.max(i64::from(window.y)))
            as u32;
        if (x, y) == (rect.x, rect.y) {
            return false;
        }
        self.set_float_px(id, Rect::new(x, y, rect.w, rect.h), window);
        true
    }

    /// Resizes a floating pane by moving its bottom-right corner
    fn resize_float_by(
        &mut self,
        id: PaneId,
        dw: i64,
        dh: i64,
        window: Rect,
        limits: Limits,
    ) -> bool {
        let Some(rect) = self.float_rect(id, window) else {
            return false;
        };
        let max_w = i64::from(window.right() - rect.x);
        let max_h = i64::from(window.bottom() - rect.y);
        let w = (i64::from(rect.w) + dw).clamp(i64::from(limits.min_w).min(max_w), max_w) as u32;
        let h = (i64::from(rect.h) + dh).clamp(i64::from(limits.min_h).min(max_h), max_h) as u32;
        if (w, h) == (rect.w, rect.h) {
            return false;
        }
        self.set_float_px(id, Rect::new(rect.x, rect.y, w, h), window);
        true
    }

    /// Mouse drag of a floating pane's edge (or corner): `dx`/`dy` are how far the pointer moved
    /// since the last event
    pub fn resize_float_edge(
        &mut self,
        id: PaneId,
        edge: Edge,
        dx: i64,
        dy: i64,
        window: Rect,
        limits: Limits,
    ) -> bool {
        self.assert_authoritative();
        let Some(rect) = self.float_rect(id, window) else {
            return false;
        };
        let (mut left, mut right) = (i64::from(rect.x), i64::from(rect.right()));
        let (mut top, mut bottom) = (i64::from(rect.y), i64::from(rect.bottom()));
        let (min_w, min_h) = (i64::from(limits.min_w), i64::from(limits.min_h));
        if edge.left {
            left = (left + dx).clamp(
                i64::from(window.x),
                (right - min_w).max(i64::from(window.x)),
            );
        }
        if edge.right {
            right = (right + dx).clamp(
                (left + min_w).min(i64::from(window.right())),
                i64::from(window.right()),
            );
        }
        if edge.top {
            top = (top + dy).clamp(
                i64::from(window.y),
                (bottom - min_h).max(i64::from(window.y)),
            );
        }
        if edge.bottom {
            bottom = (bottom + dy).clamp(
                (top + min_h).min(i64::from(window.bottom())),
                i64::from(window.bottom()),
            );
        }
        let new = Rect::new(
            left as u32,
            top as u32,
            (right - left) as u32,
            (bottom - top) as u32,
        );
        if new == rect {
            return false;
        }
        self.set_float_px(id, new, window);
        true
    }

    // ---- What is under a point -------------------------------------------------------------

    /// What is at `(x, y)`: first the visible floating panes (top to bottom), then the dividers
    /// and finally the tiled panes
    pub fn hit_test(&self, x: f64, y: f64, window: Rect, params: HitParams) -> Hit {
        for (id, rect) in self.float_rects(window).into_iter().rev() {
            if !rect.contains(x, y) {
                continue;
            }
            let edge = Edge {
                left: x - f64::from(rect.x) < params.edge_band,
                right: f64::from(rect.right()) - x <= params.edge_band,
                top: y - f64::from(rect.y) < params.edge_band,
                bottom: f64::from(rect.bottom()) - y <= params.edge_band,
            };
            if edge != Edge::default() {
                return Hit::FloatResize(id, edge);
            }
            let ring = params.move_ring;
            let in_ring = x - f64::from(rect.x) < ring
                || f64::from(rect.right()) - x <= ring
                || y - f64::from(rect.y) < ring
                || f64::from(rect.bottom()) - y <= ring;
            return if in_ring {
                Hit::FloatMove(id)
            } else {
                Hit::Pane(id)
            };
        }
        for divider in self.dividers(window) {
            let (along, across) = match divider.axis {
                Axis::Horizontal => (y, x),
                Axis::Vertical => (x, y),
            };
            let inside =
                along >= f64::from(divider.start) && along < f64::from(divider.start + divider.len);
            if inside && (across - f64::from(divider.pos)).abs() <= params.divider_zone {
                return Hit::Divider(divider.path, divider.axis);
            }
        }
        match self
            .rects(window)
            .into_iter()
            .find(|(_, rect)| rect.contains(x, y))
        {
            Some((id, _)) => Hit::Pane(id),
            None => Hit::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Rect = Rect::new(0, 0, 801, 601);
    const LIMITS: Limits = Limits {
        min_w: 80,
        min_h: 60,
    };
    const PARAMS: HitParams = HitParams {
        divider_zone: 4.0,
        edge_band: 6.0,
        move_ring: 12.0,
    };

    fn id(n: u32) -> PaneId {
        PaneId(n)
    }

    fn rect_of(layout: &Layout, pane: u32) -> Rect {
        layout
            .rects(WINDOW)
            .into_iter()
            .find(|(candidate, _)| *candidate == id(pane))
            .unwrap()
            .1
    }

    /// 1 | 2 in a row
    fn two_columns() -> Layout {
        let mut layout = Layout::new(id(1));
        assert!(layout.split(id(1), Axis::Horizontal, id(2), WINDOW, LIMITS));
        layout
    }

    /// 2x2 grid: top 1 | 2, bottom 3 | 4
    fn grid() -> Layout {
        let mut layout = Layout::new(id(1));
        assert!(layout.split(id(1), Axis::Vertical, id(3), WINDOW, LIMITS));
        assert!(layout.split(id(1), Axis::Horizontal, id(2), WINDOW, LIMITS));
        assert!(layout.split(id(3), Axis::Horizontal, id(4), WINDOW, LIMITS));
        layout
    }

    #[test]
    fn a_single_pane_fills_the_window() {
        let layout = Layout::new(id(1));
        assert_eq!(layout.rects(WINDOW), vec![(id(1), WINDOW)]);
        assert!(layout.dividers(WINDOW).is_empty());
    }

    #[test]
    fn splitting_shares_the_space_exactly_with_odd_sizes() {
        let layout = two_columns();
        let (a, b) = (rect_of(&layout, 1), rect_of(&layout, 2));
        assert_eq!(a.w + b.w, WINDOW.w);
        assert_eq!(b.x, a.right());
        assert_eq!((a.h, b.h), (WINDOW.h, WINDOW.h));
        // At 801 px, the first one gets the rounded-up half
        assert_eq!((a.w, b.w), (401, 400));
    }

    #[test]
    fn nested_splits_cover_the_window_without_gaps_or_overlaps() {
        let layout = grid();
        let mut covered = vec![0u8; (WINDOW.w * WINDOW.h) as usize];
        for (_, rect) in layout.rects(WINDOW) {
            for y in rect.y..rect.bottom() {
                for x in rect.x..rect.right() {
                    covered[(y * WINDOW.w + x) as usize] += 1;
                }
            }
        }
        assert!(covered.iter().all(|&count| count == 1));
    }

    #[test]
    fn the_new_pane_gets_the_focus() {
        let layout = two_columns();
        assert_eq!(layout.focus(), id(2));
    }

    #[test]
    fn splitting_is_refused_when_the_halves_would_be_too_small() {
        let mut layout = Layout::new(id(1));
        let tiny = Rect::new(0, 0, 150, 600);
        assert!(!layout.split(id(1), Axis::Horizontal, id(2), tiny, LIMITS));
        assert_eq!(layout.tiled_ids(), vec![id(1)]);
        assert!(layout.split(id(1), Axis::Vertical, id(2), tiny, LIMITS));
        // And a pane that does not exist cannot be split
        assert!(!layout.split(id(9), Axis::Vertical, id(3), tiny, LIMITS));
    }

    #[test]
    fn closing_gives_the_space_to_the_sibling_and_moves_the_focus_back() {
        let mut layout = two_columns();
        assert_eq!(layout.close(id(2)), Closed::Pane);
        assert_eq!(layout.rects(WINDOW), vec![(id(1), WINDOW)]);
        assert_eq!(layout.focus(), id(1));
        assert_eq!(layout.close(id(1)), Closed::LastTiled);
        assert_eq!(layout.close(id(7)), Closed::Unknown);
    }

    #[test]
    fn closing_a_nested_pane_only_grows_its_sibling() {
        let mut layout = grid();
        let before = rect_of(&layout, 3);
        assert_eq!(layout.close(id(4)), Closed::Pane);
        assert_eq!(rect_of(&layout, 3).w, WINDOW.w);
        assert_eq!(rect_of(&layout, 3).y, before.y);
        assert_eq!(rect_of(&layout, 1), rect_of(&grid(), 1));
    }

    #[test]
    fn focus_moves_to_the_neighbour_in_the_direction() {
        let mut layout = grid();
        assert!(layout.set_focus(id(1)));
        assert!(layout.focus_dir(Dir::Right, WINDOW));
        assert_eq!(layout.focus(), id(2));
        assert!(layout.focus_dir(Dir::Down, WINDOW));
        assert_eq!(layout.focus(), id(4));
        assert!(layout.focus_dir(Dir::Left, WINDOW));
        assert_eq!(layout.focus(), id(3));
        assert!(layout.focus_dir(Dir::Up, WINDOW));
        assert_eq!(layout.focus(), id(1));
        // At the edge there is nowhere to go
        assert!(!layout.focus_dir(Dir::Left, WINDOW));
        assert!(!layout.focus_dir(Dir::Up, WINDOW));
        assert_eq!(layout.focus(), id(1));
    }

    #[test]
    fn with_two_neighbours_the_most_recent_one_wins_a_tie() {
        // 1 on the left, 2 and 3 stacked on the right. With an even window height both overlap
        // 1 equally and are equally close: the one that last had focus wins
        let window = Rect::new(0, 0, 800, 600);
        let mut layout = Layout::new(id(1));
        layout.split(id(1), Axis::Horizontal, id(2), window, LIMITS);
        layout.split(id(2), Axis::Vertical, id(3), window, LIMITS);
        layout.set_focus(id(2));
        layout.set_focus(id(1));
        assert!(layout.focus_dir(Dir::Right, window));
        assert_eq!(layout.focus(), id(2));
        layout.set_focus(id(3));
        layout.set_focus(id(1));
        assert!(layout.focus_dir(Dir::Right, window));
        assert_eq!(layout.focus(), id(3));
    }

    #[test]
    fn with_unequal_overlap_the_pane_sharing_more_edge_wins() {
        // At 601 px tall the top pane is 301 and the bottom one 300
        let mut layout = Layout::new(id(1));
        layout.split(id(1), Axis::Horizontal, id(2), WINDOW, LIMITS);
        layout.split(id(2), Axis::Vertical, id(3), WINDOW, LIMITS);
        layout.set_focus(id(3));
        layout.set_focus(id(1));
        assert!(layout.focus_dir(Dir::Right, WINDOW));
        assert_eq!(layout.focus(), id(2));
    }

    #[test]
    fn cycling_visits_every_pane_in_order_and_wraps() {
        let mut layout = grid();
        layout.set_focus(id(1));
        let mut seen = Vec::new();
        for _ in 0..4 {
            layout.focus_cycle(true);
            seen.push(layout.focus());
        }
        assert_eq!(seen, vec![id(2), id(3), id(4), id(1)]);
        assert!(layout.focus_cycle(false));
        assert_eq!(layout.focus(), id(4));
    }

    #[test]
    fn growing_a_pane_moves_its_border_and_respects_the_minimum() {
        let mut layout = two_columns();
        let before = rect_of(&layout, 1).w;
        assert!(layout.resize(id(1), Dir::Right, 50, WINDOW, LIMITS));
        assert_eq!(rect_of(&layout, 1).w, before + 50);
        // A pane flush with the right edge "grows to the right" by moving its left edge
        let width = rect_of(&layout, 2).w;
        assert!(layout.resize(id(2), Dir::Right, 20, WINDOW, LIMITS));
        assert_eq!(
            rect_of(&layout, 2).w,
            width - 20,
            "shrinks: it has no right edge"
        );
        // Toward the opposite side it grows
        assert!(layout.resize(id(2), Dir::Left, 20, WINDOW, LIMITS));
        assert_eq!(rect_of(&layout, 2).w, width);
        // Never below the minimum
        assert!(layout.resize(id(1), Dir::Right, 10_000, WINDOW, LIMITS));
        assert_eq!(rect_of(&layout, 2).w, LIMITS.min_w);
        assert!(!layout.resize(id(1), Dir::Right, 10, WINDOW, LIMITS));
    }

    #[test]
    fn resizing_looks_for_the_nearest_split_along_the_right_axis() {
        // Columns 1 | (2 over 3): resizing 3 vertically moves the 2/3 divider
        let mut layout = Layout::new(id(1));
        layout.split(id(1), Axis::Horizontal, id(2), WINDOW, LIMITS);
        layout.split(id(2), Axis::Vertical, id(3), WINDOW, LIMITS);
        let (top, bottom) = (rect_of(&layout, 2).h, rect_of(&layout, 3).h);
        assert!(layout.resize(id(2), Dir::Down, 30, WINDOW, LIMITS));
        assert_eq!(rect_of(&layout, 2).h, top + 30);
        assert_eq!(rect_of(&layout, 3).h, bottom - 30);
        // And horizontally the only divider is the column one
        let width = rect_of(&layout, 1).w;
        assert!(layout.resize(id(3), Dir::Left, 10, WINDOW, LIMITS));
        assert_eq!(rect_of(&layout, 1).w, width - 10);
        // With a single pane there is nothing to move
        assert!(!Layout::new(id(1)).resize(id(1), Dir::Right, 10, WINDOW, LIMITS));
    }

    #[test]
    fn nested_minimums_are_respected_when_moving_a_divider() {
        // 1 | (2 | 3): the divider of 1 cannot go past leaving minimum room for 2 and 3
        let mut layout = Layout::new(id(1));
        layout.split(id(1), Axis::Horizontal, id(2), WINDOW, LIMITS);
        layout.split(id(2), Axis::Horizontal, id(3), WINDOW, LIMITS);
        layout.resize(id(1), Dir::Right, 10_000, WINDOW, LIMITS);
        assert_eq!(
            rect_of(&layout, 2).w + rect_of(&layout, 3).w,
            2 * LIMITS.min_w
        );
        assert!(rect_of(&layout, 2).w >= LIMITS.min_w && rect_of(&layout, 3).w >= LIMITS.min_w);
    }

    #[test]
    fn dragging_a_divider_follows_the_pointer_and_clamps() {
        let mut layout = two_columns();
        let path = layout.dividers(WINDOW)[0].path.clone();
        assert!(layout.set_divider(&path, 300, WINDOW, LIMITS));
        assert_eq!(rect_of(&layout, 1).w, 300);
        assert!(layout.set_divider(&path, -50, WINDOW, LIMITS));
        assert_eq!(rect_of(&layout, 1).w, LIMITS.min_w);
        assert!(layout.set_divider(&path, 5000, WINDOW, LIMITS));
        assert_eq!(rect_of(&layout, 2).w, LIMITS.min_w);
    }

    #[test]
    fn ratios_survive_a_window_resize() {
        let mut layout = two_columns();
        let big = Rect::new(0, 0, 1602, 1202);
        let a = layout.rects(big)[0].1;
        assert_eq!(a.w, 801);
        layout.resize(id(1), Dir::Right, 0, big, LIMITS);
        assert_eq!(layout.rects(big)[0].1.w, 801);
    }

    #[test]
    fn zoom_shows_only_the_focused_pane_and_is_undone_by_layout_changes() {
        let mut layout = two_columns();
        assert!(layout.toggle_zoom());
        assert_eq!(layout.rects(WINDOW), vec![(id(2), WINDOW)]);
        assert!(layout.dividers(WINDOW).is_empty());
        assert_eq!(layout.visible_count(), 1);
        assert!(layout.toggle_zoom());
        assert_eq!(layout.rects(WINDOW).len(), 2);

        // Moving focus, splitting or closing clears it
        layout.toggle_zoom();
        assert!(layout.focus_dir(Dir::Left, WINDOW));
        assert_eq!(layout.zoomed(), None);
        layout.toggle_zoom();
        assert!(layout.split(id(1), Axis::Vertical, id(3), WINDOW, LIMITS));
        assert_eq!(layout.zoomed(), None);
        assert_eq!(layout.rects(WINDOW).len(), 3);
        layout.toggle_zoom();
        layout.close(id(3));
        assert_eq!(layout.zoomed(), None);

        // With a single pane there is nothing to zoom
        assert!(!Layout::new(id(1)).toggle_zoom());
    }

    #[test]
    fn visible_panes_are_listed_from_the_bottom_up() {
        let mut layout = two_columns();
        layout.add_float(id(3), 0.5, 0.5);
        assert_eq!(layout.visible_ids(), vec![id(1), id(2), id(3)]);
        layout.toggle_zoom();
        // With zoom on, a floating pane is not a tiled pane: it cannot be zoomed
        assert_eq!(layout.zoomed(), None);
        layout.set_focus(id(2));
        layout.toggle_zoom();
        assert_eq!(layout.visible_ids(), vec![id(2), id(3)]);
        layout.toggle_floats();
        assert_eq!(layout.visible_ids(), vec![id(2)]);
    }

    #[test]
    fn a_new_float_is_centered_and_cascades() {
        let mut layout = Layout::new(id(1));
        layout.add_float(id(2), 0.5, 0.5);
        let first = layout.float_rect(id(2), WINDOW).unwrap();
        // Half of 801x601, rounded
        assert_eq!((first.w, first.h), (401, 301));
        assert_eq!((first.x, first.y), (200, 150));
        layout.add_float(id(3), 0.5, 0.5);
        let second = layout.float_rect(id(3), WINDOW).unwrap();
        assert!(second.x > first.x && second.y > first.y);
        assert_eq!(layout.focus(), id(3));
        assert!(layout.is_float(id(3)) && !layout.is_float(id(1)));
        assert_eq!(layout.tiled_ids(), vec![id(1)]);
        assert_eq!(layout.all_ids(), vec![id(1), id(2), id(3)]);
    }

    #[test]
    fn floats_stay_inside_the_window_and_keep_their_proportions_on_resize() {
        let mut layout = Layout::new(id(1));
        layout.add_float(id(2), 0.5, 0.5);
        assert!(layout.move_float(id(2), 10_000, 10_000, WINDOW));
        let rect = layout.float_rect(id(2), WINDOW).unwrap();
        assert_eq!((rect.right(), rect.bottom()), (WINDOW.w, WINDOW.h));
        assert!(layout.move_float(id(2), -10_000, -10_000, WINDOW));
        let rect = layout.float_rect(id(2), WINDOW).unwrap();
        assert_eq!((rect.x, rect.y), (0, 0));
        assert!(
            !layout.move_float(id(2), -5, -5, WINDOW),
            "already in the corner"
        );
        let doubled = Rect::new(0, 0, 1602, 1202);
        let big = layout.float_rect(id(2), doubled).unwrap();
        assert!((i64::from(big.w) - 2 * i64::from(rect.w)).abs() <= 2);
    }

    #[test]
    fn dragging_float_edges_resizes_within_limits() {
        let mut layout = Layout::new(id(1));
        layout.add_float(id(2), 0.5, 0.5);
        let start = layout.float_rect(id(2), WINDOW).unwrap();
        let right = Edge {
            right: true,
            ..Edge::default()
        };
        assert!(layout.resize_float_edge(id(2), right, 40, 0, WINDOW, LIMITS));
        assert_eq!(layout.float_rect(id(2), WINDOW).unwrap().w, start.w + 40);
        let top_left = Edge {
            left: true,
            top: true,
            ..Edge::default()
        };
        assert!(layout.resize_float_edge(id(2), top_left, -20, -10, WINDOW, LIMITS));
        let now = layout.float_rect(id(2), WINDOW).unwrap();
        assert_eq!((now.x, now.y), (start.x - 20, start.y - 10));
        // It does not go below the minimum or leave the window
        layout.resize_float_edge(id(2), right, -10_000, 0, WINDOW, LIMITS);
        assert_eq!(layout.float_rect(id(2), WINDOW).unwrap().w, LIMITS.min_w);
        layout.resize_float_edge(id(2), right, 10_000, 0, WINDOW, LIMITS);
        assert_eq!(layout.float_rect(id(2), WINDOW).unwrap().right(), WINDOW.w);
    }

    #[test]
    fn resizing_a_float_with_keys_grows_right_and_down_and_shrinks_left_and_up() {
        let mut layout = Layout::new(id(1));
        layout.add_float(id(2), 0.5, 0.5);
        let start = layout.float_rect(id(2), WINDOW).unwrap();
        assert!(layout.resize(id(2), Dir::Right, 20, WINDOW, LIMITS));
        assert!(layout.resize(id(2), Dir::Up, 15, WINDOW, LIMITS));
        let now = layout.float_rect(id(2), WINDOW).unwrap();
        assert_eq!(
            (now.w, now.h, now.x, now.y),
            (start.w + 20, start.h - 15, start.x, start.y)
        );
    }

    #[test]
    fn hiding_floats_returns_the_focus_to_the_tiles_and_showing_restores_it() {
        let mut layout = two_columns();
        layout.set_focus(id(1));
        layout.add_float(id(3), 0.5, 0.5);
        assert_eq!(layout.focus(), id(3));
        assert!(layout.toggle_floats());
        assert!(!layout.floats_visible());
        assert_eq!(layout.focus(), id(1));
        assert!(layout.float_rects(WINDOW).is_empty());
        assert_eq!(layout.visible_count(), 2);
        assert!(layout.toggle_floats());
        assert!(layout.floats_visible());
        assert_eq!(layout.focus(), id(3));
        // Without floating panes there is nothing to toggle
        assert!(!two_columns().toggle_floats());
    }

    #[test]
    fn focusing_a_hidden_float_shows_the_floats() {
        let mut layout = Layout::new(id(1));
        layout.add_float(id(2), 0.5, 0.5);
        layout.toggle_floats();
        assert!(layout.set_focus(id(2)));
        assert!(layout.floats_visible());
    }

    #[test]
    fn closing_a_float_gives_the_focus_back() {
        let mut layout = two_columns();
        layout.set_focus(id(1));
        layout.add_float(id(3), 0.5, 0.5);
        assert_eq!(layout.close(id(3)), Closed::Pane);
        assert_eq!(layout.focus(), id(1));
        assert!(!layout.has_floats());
    }

    #[test]
    fn splits_from_a_float_go_to_the_last_tiled_pane() {
        let mut layout = two_columns();
        layout.set_focus(id(1));
        layout.add_float(id(3), 0.5, 0.5);
        assert_eq!(layout.split_target(), id(1));
        assert!(
            !layout.split(id(3), Axis::Horizontal, id(4), WINDOW, LIMITS),
            "a floating pane cannot be split"
        );
    }

    #[test]
    fn raising_a_float_changes_the_stacking_order() {
        let mut layout = Layout::new(id(1));
        layout.add_float(id(2), 0.5, 0.5);
        layout.add_float(id(3), 0.5, 0.5);
        let order = |layout: &Layout| {
            layout
                .float_rects(WINDOW)
                .into_iter()
                .map(|(id, _)| id)
                .collect::<Vec<_>>()
        };
        assert_eq!(order(&layout), vec![id(2), id(3)]);
        layout.set_focus(id(2));
        assert_eq!(order(&layout), vec![id(3), id(2)]);
    }

    #[test]
    fn cycling_includes_visible_floats_after_the_tiles() {
        let mut layout = two_columns();
        layout.add_float(id(3), 0.5, 0.5);
        layout.set_focus(id(2));
        layout.focus_cycle(true);
        assert_eq!(layout.focus(), id(3));
        layout.focus_cycle(true);
        assert_eq!(layout.focus(), id(1));
        layout.toggle_floats();
        layout.set_focus(id(2));
        layout.focus_cycle(true);
        assert_eq!(layout.focus(), id(1));
    }

    #[test]
    fn hit_testing_prefers_floats_then_dividers_then_panes() {
        let mut layout = two_columns();
        let divider_x = f64::from(rect_of(&layout, 1).right());
        assert_eq!(
            layout.hit_test(divider_x + 2.0, 100.0, WINDOW, PARAMS),
            Hit::Divider(vec![], Axis::Horizontal)
        );
        assert_eq!(
            layout.hit_test(divider_x - 100.0, 100.0, WINDOW, PARAMS),
            Hit::Pane(id(1))
        );
        assert_eq!(
            layout.hit_test(divider_x + 100.0, 100.0, WINDOW, PARAMS),
            Hit::Pane(id(2))
        );
        assert_eq!(layout.hit_test(-1.0, 10.0, WINDOW, PARAMS), Hit::None);

        layout.add_float(id(3), 0.5, 0.5);
        let float = layout.float_rect(id(3), WINDOW).unwrap();
        let (cx, cy) = (
            f64::from(float.x + float.w / 2),
            f64::from(float.y + float.h / 2),
        );
        assert_eq!(layout.hit_test(cx, cy, WINDOW, PARAMS), Hit::Pane(id(3)));
        // The ring moves, the edge band resizes
        assert_eq!(
            layout.hit_test(f64::from(float.x) + 8.0, cy, WINDOW, PARAMS),
            Hit::FloatMove(id(3))
        );
        assert_eq!(
            layout.hit_test(f64::from(float.x) + 2.0, cy, WINDOW, PARAMS),
            Hit::FloatResize(
                id(3),
                Edge {
                    left: true,
                    ..Edge::default()
                }
            )
        );
        assert_eq!(
            layout.hit_test(
                f64::from(float.right()) - 1.0,
                f64::from(float.bottom()) - 1.0,
                WINDOW,
                PARAMS
            ),
            Hit::FloatResize(
                id(3),
                Edge {
                    right: true,
                    bottom: true,
                    ..Edge::default()
                }
            )
        );
        // A divider covered by the floating pane cannot be grabbed through it
        let hidden_divider = layout.hit_test(divider_x, cy, WINDOW, PARAMS);
        assert!(matches!(
            hidden_divider,
            Hit::Pane(_) | Hit::FloatMove(_) | Hit::FloatResize(..)
        ));
        // And with the floating panes hidden, what is underneath shows again
        layout.toggle_floats();
        assert_eq!(
            layout.hit_test(divider_x, cy, WINDOW, PARAMS),
            Hit::Divider(vec![], Axis::Horizontal)
        );
    }

    #[test]
    fn divider_lines_are_centered_on_the_boundary_and_span_the_split() {
        let layout = two_columns();
        let divider = &layout.dividers(WINDOW)[0];
        let line = divider.rect(2);
        assert_eq!(line, Rect::new(divider.pos - 1, 0, 2, WINDOW.h));
        let mut stacked = Layout::new(id(1));
        stacked.split(id(1), Axis::Vertical, id(2), WINDOW, LIMITS);
        let divider = &stacked.dividers(WINDOW)[0];
        assert_eq!(divider.rect(1), Rect::new(0, divider.pos, WINDOW.w, 1));
    }

    #[test]
    fn a_zoomed_pane_has_no_dividers_to_grab() {
        let mut layout = two_columns();
        layout.toggle_zoom();
        let x = f64::from(WINDOW.w / 2);
        assert_eq!(layout.hit_test(x, 100.0, WINDOW, PARAMS), Hit::Pane(id(2)));
    }

    #[test]
    fn split_sizes_never_returns_an_empty_child_when_there_is_room() {
        for total in [2u32, 3, 10, 801] {
            for ratio in [0.0, 0.001, 0.5, 0.999, 1.0] {
                let (a, b) = split_sizes(total, ratio);
                assert_eq!(a + b, total);
                assert!(a >= 1 && b >= 1, "total={total} ratio={ratio} -> {a}+{b}");
            }
        }
    }

    #[test]
    fn split_preview_predicts_exactly_what_the_split_will_do() {
        let mut layout = Layout::new(id(1));
        let preview = layout
            .split_preview(id(1), Axis::Horizontal, WINDOW, LIMITS)
            .expect("the split fits");
        assert!(layout.split(id(1), Axis::Horizontal, id(2), WINDOW, LIMITS));
        assert_eq!(rect_of(&layout, 2), preview);

        // With zoom on, the preview still sees the real rectangles
        layout.toggle_zoom();
        let preview = layout
            .split_preview(id(1), Axis::Vertical, WINDOW, LIMITS)
            .expect("the split fits");
        assert!(layout.split(id(1), Axis::Vertical, id(3), WINDOW, LIMITS));
        assert_eq!(rect_of(&layout, 3), preview);

        // A refused split has no preview, and a pane that does not exist either
        let tiny = Rect::new(0, 0, 150, 600);
        assert!(
            Layout::new(id(1))
                .split_preview(id(1), Axis::Horizontal, tiny, LIMITS)
                .is_none()
        );
        assert!(!Layout::new(id(1)).split(id(1), Axis::Horizontal, id(2), tiny, LIMITS));
        assert!(
            layout
                .split_preview(id(9), Axis::Vertical, WINDOW, LIMITS)
                .is_none()
        );
    }

    #[test]
    fn float_preview_predicts_exactly_what_add_float_will_do() {
        let mut layout = Layout::new(id(1));
        let first = layout.float_preview(0.5, 0.5, WINDOW);
        layout.add_float(id(2), 0.5, 0.5);
        assert_eq!(layout.float_rect(id(2), WINDOW).unwrap(), first);
        // The second one accounts for the cascade offset of the first
        let second = layout.float_preview(0.5, 0.5, WINDOW);
        layout.add_float(id(3), 0.5, 0.5);
        assert_eq!(layout.float_rect(id(3), WINDOW).unwrap(), second);
        assert!(second.x > first.x && second.y > first.y);
    }

    #[test]
    #[should_panic(expected = "projection is read-only")]
    fn a_projection_refuses_mutations() {
        let layout = grid();
        let state = layout.state();
        let mut projection = Layout::from_state(state);
        // Mutating a projection would diverge it from the server's authority
        projection.set_focus(id(1));
    }

    #[test]
    fn a_layout_survives_a_state_round_trip() {
        let mut layout = grid();
        layout.add_float(id(5), 0.5, 0.5);
        layout.toggle_floats();
        layout.set_focus(id(1));
        layout.toggle_zoom();

        let state = layout.state();
        let rebuilt = Layout::from_state(state.clone());
        assert_eq!(rebuilt.state(), state);
        // The pure queries of the projection match the original
        assert_eq!(rebuilt.rects(WINDOW), layout.rects(WINDOW));
        assert_eq!(rebuilt.focus(), layout.focus());
        assert_eq!(rebuilt.zoomed(), layout.zoomed());
        assert_eq!(rebuilt.visible_ids(), layout.visible_ids());
        assert_eq!(rebuilt.split_target(), layout.split_target());
    }
}
