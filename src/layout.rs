//! Geometría de los paneles: el árbol de splits del mosaico, los paneles flotantes, el foco, el
//! zoom y qué hay bajo un punto. No sabe nada de ventanas, GPU ni PTY: recibe rectángulos en
//! pixeles físicos y devuelve rectángulos, para poder probarlo todo con tests normales.

/// Identificador de un panel; no se reutiliza durante la vida de la app
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PaneId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

/// Tamaño mínimo de un panel (con su margen incluido), en pixeles
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub min_w: u32,
    pub min_h: u32,
}

/// Cómo se colocan los hijos de un split
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    /// Uno al lado del otro (el divisor es una línea vertical): "dividir a la derecha"
    Horizontal,
    /// Uno encima del otro (el divisor es una línea horizontal): "dividir hacia abajo"
    Vertical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

impl Dir {
    fn axis(self) -> Axis {
        match self {
            Dir::Left | Dir::Right => Axis::Horizontal,
            Dir::Up | Dir::Down => Axis::Vertical,
        }
    }

    /// ¿Crece hacia coordenadas mayores?
    fn positive(self) -> bool {
        matches!(self, Dir::Right | Dir::Down)
    }
}

/// Bordes de un flotante que están bajo el puntero (dos a la vez en una esquina)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Edge {
    pub left: bool,
    pub right: bool,
    pub top: bool,
    pub bottom: bool,
}

/// Camino hasta un split: `false` = hijo `a` (izquierda/arriba), `true` = hijo `b`
pub type Path = Vec<bool>;

/// Línea que separa los dos hijos de un split. `pos` es la coordenada del límite en el eje del
/// split y `start..start + len` lo que ocupa en el otro eje.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Divider {
    pub path: Path,
    pub axis: Axis,
    pub pos: u32,
    pub start: u32,
    pub len: u32,
}

impl Divider {
    /// Rectángulo de la línea, de `thickness` pixeles de grosor centrada en el límite
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
    /// El anillo de margen de un flotante: arrastrarlo lo mueve
    FloatMove(PaneId),
    FloatResize(PaneId, Edge),
    None,
}

/// Tolerancias de `hit_test`, en pixeles físicos
#[derive(Clone, Copy, Debug)]
pub struct HitParams {
    /// A cuántos pixeles de un divisor se agarra
    pub divider_zone: f64,
    /// Grosor de la banda de los bordes de un flotante que lo redimensiona
    pub edge_band: f64,
    /// Grosor del anillo de un flotante (margen del panel) que lo mueve
    pub move_ring: f64,
}

#[derive(Debug, Clone)]
enum Node {
    Leaf(PaneId),
    Split {
        axis: Axis,
        ratio: f32,
        a: Box<Node>,
        b: Box<Node>,
    },
}

/// Reparte `total` pixeles entre los dos hijos; cada uno recibe al menos uno si hay sitio
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

    /// Tamaño mínimo del subárbol: la suma de los mínimos en el eje del split y el mayor en el otro
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

    /// Sustituye la hoja `target` por un split con ella y la nueva a partes iguales
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
            Node::Split { a, b, .. } => a.split_leaf(target, axis, new_id) || b.split_leaf(target, axis, new_id),
        }
    }

    /// Quita la hoja `id`: su hermano ocupa el sitio del split. `None` si el nodo era esa hoja.
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

/// Un panel flotante, en fracciones de la ventana: así sigue en su sitio cuando esta cambia de tamaño
#[derive(Clone, Debug, PartialEq)]
pub struct Float {
    pub id: PaneId,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

/// Lo que pasó al cerrar un panel
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Closed {
    Pane,
    /// Era el último panel del mosaico: la ventana se cierra
    LastTiled,
    /// No existía
    Unknown,
}

/// Cuántos paneles recuerda el historial de foco
const HISTORY_LIMIT: usize = 32;
/// Desplazamiento (fracción de la ventana) entre flotantes nuevos, para que no queden uno sobre otro
const FLOAT_CASCADE: f32 = 0.03;
const MIN_FLOAT_FRACTION: f32 = 0.1;

pub struct Layout {
    root: Node,
    focus: PaneId,
    zoomed: Option<PaneId>,
    floats: Vec<Float>,
    floats_visible: bool,
    /// Paneles con foco, del más antiguo al más reciente
    history: Vec<PaneId>,
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
        }
    }

    pub fn focus(&self) -> PaneId {
        self.focus
    }

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

    /// Paneles del mosaico, en orden de recorrido del árbol
    pub fn tiled_ids(&self) -> Vec<PaneId> {
        let mut ids = Vec::new();
        self.root.leaves(&mut ids);
        ids
    }

    /// Todos los paneles que existen (mosaico y flotantes, estén o no visibles)
    #[cfg(test)]
    pub fn all_ids(&self) -> Vec<PaneId> {
        let mut ids = self.tiled_ids();
        ids.extend(self.floats.iter().map(|float| float.id));
        ids
    }

    /// Paneles que se ven ahora mismo, en orden de dibujo (de abajo arriba): los del mosaico (con
    /// zoom, solo el ampliado) y encima los flotantes si no están ocultos
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
        self.history.iter().position(|&other| other == id).map_or(-1, |index| index as i64)
    }

    /// Da el foco a un panel. Un flotante sube al primer plano; pasar a otro panel del mosaico
    /// quita el zoom; dar el foco a un flotante oculto los muestra.
    pub fn set_focus(&mut self, id: PaneId) -> bool {
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

    /// Último panel del mosaico que tuvo el foco (el primero del árbol si no hay historial)
    fn last_tiled(&self) -> PaneId {
        self.history
            .iter()
            .rev()
            .copied()
            .find(|&id| self.root.contains(id))
            .unwrap_or_else(|| self.tiled_ids()[0])
    }

    // ---- Mosaico ---------------------------------------------------------------------------

    /// Panel del mosaico al que afecta un split: el enfocado o, si el foco está en un flotante,
    /// el último del mosaico que lo tuvo
    pub fn split_target(&self) -> PaneId {
        if self.is_float(self.focus) { self.last_tiled() } else { self.focus }
    }

    /// Divide el panel `target` del mosaico en dos y da el foco al nuevo. `false` si `target` no
    /// está en el mosaico o si las dos mitades quedarían por debajo del tamaño mínimo.
    pub fn split(&mut self, target: PaneId, axis: Axis, new_id: PaneId, area: Rect, limits: Limits) -> bool {
        let before = self.zoomed.take();
        let Some((_, rect)) = self.rects(area).into_iter().find(|(id, _)| *id == target) else {
            self.zoomed = before;
            return false;
        };
        // Con el zoom puesto `rects` solo devolvía el ampliado: se recalcula sin zoom
        let rect = if before.is_some() {
            self.rects(area).into_iter().find(|(id, _)| *id == target).map_or(rect, |(_, r)| r)
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

    /// Cierra un panel. Al cerrar el último del mosaico no se toca nada y se avisa para que la
    /// ventana se cierre.
    pub fn close(&mut self, id: PaneId) -> Closed {
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

    /// Quita un panel del historial y, si tenía el foco, lo pasa al último que lo tuvo
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

    /// Rectángulos de los paneles del mosaico. Con zoom, solo el ampliado, que ocupa todo el área.
    pub fn rects(&self, area: Rect) -> Vec<(PaneId, Rect)> {
        if let Some(zoomed) = self.zoomed {
            return vec![(zoomed, area)];
        }
        let mut rects = Vec::new();
        let mut dividers = Vec::new();
        self.root.layout(area, &mut Vec::new(), &mut rects, &mut dividers);
        rects
    }

    /// Divisores del mosaico (ninguno con zoom)
    pub fn dividers(&self, area: Rect) -> Vec<Divider> {
        if self.zoomed.is_some() {
            return Vec::new();
        }
        let mut rects = Vec::new();
        let mut dividers = Vec::new();
        self.root.layout(area, &mut Vec::new(), &mut rects, &mut dividers);
        dividers
    }

    /// Mueve el foco al panel vecino en esa dirección (dentro del mosaico). Quita el zoom. `false`
    /// si el foco está en un flotante o no hay vecino.
    pub fn focus_dir(&mut self, dir: Dir, area: Rect) -> bool {
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
            .min_by_key(|&(id, distance, shared)| (distance, std::cmp::Reverse(shared), std::cmp::Reverse(self.recency(id))))
            .map(|(id, ..)| id);
        match best {
            Some(id) => self.set_focus(id),
            None => false,
        }
    }

    /// Siguiente (o anterior) panel: primero los del mosaico y luego los flotantes visibles
    pub fn focus_cycle(&mut self, forward: bool) -> bool {
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

    /// Mueve el borde del panel en la dirección dada `delta` pixeles (agranda hacia ese lado). Si el
    /// panel no tiene borde por ahí (está en el límite de la ventana) se mueve el del lado contrario,
    /// y el panel encoge. Con un flotante, derecha y abajo agrandan; izquierda y arriba encogen.
    pub fn resize(&mut self, id: PaneId, dir: Dir, delta: u32, area: Rect, limits: Limits) -> bool {
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
        // De cerca a lejos: el primer ancestro con ese eje y el panel del lado que corresponde
        // (primero, el que deja el borde en la dirección pedida; si no, el contrario)
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

    /// Rectángulo que ocupa el nodo al que lleva `path`
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

    /// Fija cuántos pixeles ocupa el primer hijo del split de `path`, respetando el mínimo de los
    /// dos subárboles
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

    /// Arrastre de un divisor con el ratón: lleva el límite a la coordenada `boundary`
    pub fn set_divider(&mut self, path: &[bool], boundary: i64, area: Rect, limits: Limits) -> bool {
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

    /// Ampliar el panel enfocado a toda la ventana, o volver al mosaico. Solo con un panel del
    /// mosaico enfocado y al menos otro más.
    pub fn toggle_zoom(&mut self) -> bool {
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

    // ---- Flotantes -------------------------------------------------------------------------

    /// Añade un flotante centrado (con un pequeño desplazamiento por cada uno que ya hay), lo
    /// muestra y le da el foco
    pub fn add_float(&mut self, id: PaneId, width: f32, height: f32) {
        let w = width.clamp(MIN_FLOAT_FRACTION, 1.0);
        let h = height.clamp(MIN_FLOAT_FRACTION, 1.0);
        let shift = FLOAT_CASCADE * (self.floats.len() % 5) as f32;
        let x = ((1.0 - w) / 2.0 + shift).clamp(0.0, 1.0 - w);
        let y = ((1.0 - h) / 2.0 + shift).clamp(0.0, 1.0 - h);
        self.floats.push(Float { id, x, y, w, h });
        self.floats_visible = true;
        self.set_focus(id);
    }

    /// Oculta o muestra todos los flotantes. Al ocultarlos, el foco vuelve al mosaico; al mostrarlos,
    /// al último flotante que lo tuvo. Devuelve si hay alguno (si no, no hay nada que mostrar).
    pub fn toggle_floats(&mut self) -> bool {
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

    /// Flotantes visibles, del que está al fondo al que está encima
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

    /// Mueve un flotante sin sacarlo de la ventana
    pub fn move_float(&mut self, id: PaneId, dx: i64, dy: i64, window: Rect) -> bool {
        let Some(rect) = self.float_rect(id, window) else {
            return false;
        };
        let max_x = i64::from(window.right() - rect.w);
        let max_y = i64::from(window.bottom() - rect.h);
        let x = (i64::from(rect.x) + dx).clamp(i64::from(window.x), max_x.max(i64::from(window.x))) as u32;
        let y = (i64::from(rect.y) + dy).clamp(i64::from(window.y), max_y.max(i64::from(window.y))) as u32;
        if (x, y) == (rect.x, rect.y) {
            return false;
        }
        self.set_float_px(id, Rect::new(x, y, rect.w, rect.h), window);
        true
    }

    /// Cambia el tamaño de un flotante moviendo su esquina inferior derecha
    fn resize_float_by(&mut self, id: PaneId, dw: i64, dh: i64, window: Rect, limits: Limits) -> bool {
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

    /// Arrastre del borde (o esquina) de un flotante con el ratón: `dx`/`dy` son lo que se movió
    /// el puntero desde el último evento
    pub fn resize_float_edge(&mut self, id: PaneId, edge: Edge, dx: i64, dy: i64, window: Rect, limits: Limits) -> bool {
        let Some(rect) = self.float_rect(id, window) else {
            return false;
        };
        let (mut left, mut right) = (i64::from(rect.x), i64::from(rect.right()));
        let (mut top, mut bottom) = (i64::from(rect.y), i64::from(rect.bottom()));
        let (min_w, min_h) = (i64::from(limits.min_w), i64::from(limits.min_h));
        if edge.left {
            left = (left + dx).clamp(i64::from(window.x), (right - min_w).max(i64::from(window.x)));
        }
        if edge.right {
            right = (right + dx).clamp((left + min_w).min(i64::from(window.right())), i64::from(window.right()));
        }
        if edge.top {
            top = (top + dy).clamp(i64::from(window.y), (bottom - min_h).max(i64::from(window.y)));
        }
        if edge.bottom {
            bottom = (bottom + dy).clamp((top + min_h).min(i64::from(window.bottom())), i64::from(window.bottom()));
        }
        let new = Rect::new(left as u32, top as u32, (right - left) as u32, (bottom - top) as u32);
        if new == rect {
            return false;
        }
        self.set_float_px(id, new, window);
        true
    }

    // ---- Qué hay bajo un punto -------------------------------------------------------------

    /// Qué hay en `(x, y)`: primero los flotantes visibles (de arriba abajo), luego los divisores y
    /// por último los paneles del mosaico
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
            return if in_ring { Hit::FloatMove(id) } else { Hit::Pane(id) };
        }
        for divider in self.dividers(window) {
            let (along, across) = match divider.axis {
                Axis::Horizontal => (y, x),
                Axis::Vertical => (x, y),
            };
            let inside = along >= f64::from(divider.start) && along < f64::from(divider.start + divider.len);
            if inside && (across - f64::from(divider.pos)).abs() <= params.divider_zone {
                return Hit::Divider(divider.path, divider.axis);
            }
        }
        match self.rects(window).into_iter().find(|(_, rect)| rect.contains(x, y)) {
            Some((id, _)) => Hit::Pane(id),
            None => Hit::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Rect = Rect::new(0, 0, 801, 601);
    const LIMITS: Limits = Limits { min_w: 80, min_h: 60 };
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

    /// 1 | 2 en una fila
    fn two_columns() -> Layout {
        let mut layout = Layout::new(id(1));
        assert!(layout.split(id(1), Axis::Horizontal, id(2), WINDOW, LIMITS));
        layout
    }

    /// Cuadrícula de 2x2: arriba 1 | 2, abajo 3 | 4
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
        // Con 801 px, el primero se queda con el redondeo hacia arriba
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
        // Y no se puede dividir un panel que no existe
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
        // En el borde no hay a dónde ir
        assert!(!layout.focus_dir(Dir::Left, WINDOW));
        assert!(!layout.focus_dir(Dir::Up, WINDOW));
        assert_eq!(layout.focus(), id(1));
    }

    #[test]
    fn with_two_neighbours_the_most_recent_one_wins_a_tie() {
        // 1 a la izquierda, 2 y 3 apilados a la derecha. Con una ventana de altura par los dos
        // solapan lo mismo con 1 y están igual de cerca: gana el último que tuvo el foco
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
        // Con 601 px de alto el panel de arriba mide 301 y el de abajo 300
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
        // Un panel pegado al borde derecho "crece hacia la derecha" moviendo su borde izquierdo
        let width = rect_of(&layout, 2).w;
        assert!(layout.resize(id(2), Dir::Right, 20, WINDOW, LIMITS));
        assert_eq!(rect_of(&layout, 2).w, width - 20, "encoge: no tiene borde a la derecha");
        // Hacia el lado contrario crece
        assert!(layout.resize(id(2), Dir::Left, 20, WINDOW, LIMITS));
        assert_eq!(rect_of(&layout, 2).w, width);
        // Nunca por debajo del mínimo
        assert!(layout.resize(id(1), Dir::Right, 10_000, WINDOW, LIMITS));
        assert_eq!(rect_of(&layout, 2).w, LIMITS.min_w);
        assert!(!layout.resize(id(1), Dir::Right, 10, WINDOW, LIMITS));
    }

    #[test]
    fn resizing_looks_for_the_nearest_split_along_the_right_axis() {
        // Columnas 1 | (2 sobre 3): redimensionar 3 en vertical mueve el divisor de 2/3
        let mut layout = Layout::new(id(1));
        layout.split(id(1), Axis::Horizontal, id(2), WINDOW, LIMITS);
        layout.split(id(2), Axis::Vertical, id(3), WINDOW, LIMITS);
        let (top, bottom) = (rect_of(&layout, 2).h, rect_of(&layout, 3).h);
        assert!(layout.resize(id(2), Dir::Down, 30, WINDOW, LIMITS));
        assert_eq!(rect_of(&layout, 2).h, top + 30);
        assert_eq!(rect_of(&layout, 3).h, bottom - 30);
        // Y en horizontal el único divisor es el de las columnas
        let width = rect_of(&layout, 1).w;
        assert!(layout.resize(id(3), Dir::Left, 10, WINDOW, LIMITS));
        assert_eq!(rect_of(&layout, 1).w, width - 10);
        // Con un solo panel no hay nada que mover
        assert!(!Layout::new(id(1)).resize(id(1), Dir::Right, 10, WINDOW, LIMITS));
    }

    #[test]
    fn nested_minimums_are_respected_when_moving_a_divider() {
        // 1 | (2 | 3): el divisor de 1 no puede pasar de dejar sitio mínimo a 2 y 3
        let mut layout = Layout::new(id(1));
        layout.split(id(1), Axis::Horizontal, id(2), WINDOW, LIMITS);
        layout.split(id(2), Axis::Horizontal, id(3), WINDOW, LIMITS);
        layout.resize(id(1), Dir::Right, 10_000, WINDOW, LIMITS);
        assert_eq!(rect_of(&layout, 2).w + rect_of(&layout, 3).w, 2 * LIMITS.min_w);
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

        // Mover el foco, dividir o cerrar lo quitan
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

        // Con un solo panel no hay nada que ampliar
        assert!(!Layout::new(id(1)).toggle_zoom());
    }

    #[test]
    fn visible_panes_are_listed_from_the_bottom_up() {
        let mut layout = two_columns();
        layout.add_float(id(3), 0.5, 0.5);
        assert_eq!(layout.visible_ids(), vec![id(1), id(2), id(3)]);
        layout.toggle_zoom();
        // Con el zoom puesto, un flotante no es un panel del mosaico: no se puede ampliar
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
        // La mitad de 801x601, redondeada
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
        assert!(!layout.move_float(id(2), -5, -5, WINDOW), "ya está en la esquina");
        let doubled = Rect::new(0, 0, 1602, 1202);
        let big = layout.float_rect(id(2), doubled).unwrap();
        assert!((i64::from(big.w) - 2 * i64::from(rect.w)).abs() <= 2);
    }

    #[test]
    fn dragging_float_edges_resizes_within_limits() {
        let mut layout = Layout::new(id(1));
        layout.add_float(id(2), 0.5, 0.5);
        let start = layout.float_rect(id(2), WINDOW).unwrap();
        let right = Edge { right: true, ..Edge::default() };
        assert!(layout.resize_float_edge(id(2), right, 40, 0, WINDOW, LIMITS));
        assert_eq!(layout.float_rect(id(2), WINDOW).unwrap().w, start.w + 40);
        let top_left = Edge { left: true, top: true, ..Edge::default() };
        assert!(layout.resize_float_edge(id(2), top_left, -20, -10, WINDOW, LIMITS));
        let now = layout.float_rect(id(2), WINDOW).unwrap();
        assert_eq!((now.x, now.y), (start.x - 20, start.y - 10));
        // No baja del mínimo ni se sale de la ventana
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
        assert_eq!((now.w, now.h, now.x, now.y), (start.w + 20, start.h - 15, start.x, start.y));
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
        // Sin flotantes no hay nada que alternar
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
        assert!(!layout.split(id(3), Axis::Horizontal, id(4), WINDOW, LIMITS), "un flotante no se divide");
    }

    #[test]
    fn raising_a_float_changes_the_stacking_order() {
        let mut layout = Layout::new(id(1));
        layout.add_float(id(2), 0.5, 0.5);
        layout.add_float(id(3), 0.5, 0.5);
        let order = |layout: &Layout| layout.float_rects(WINDOW).into_iter().map(|(id, _)| id).collect::<Vec<_>>();
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
        assert_eq!(layout.hit_test(divider_x + 2.0, 100.0, WINDOW, PARAMS), Hit::Divider(vec![], Axis::Horizontal));
        assert_eq!(layout.hit_test(divider_x - 100.0, 100.0, WINDOW, PARAMS), Hit::Pane(id(1)));
        assert_eq!(layout.hit_test(divider_x + 100.0, 100.0, WINDOW, PARAMS), Hit::Pane(id(2)));
        assert_eq!(layout.hit_test(-1.0, 10.0, WINDOW, PARAMS), Hit::None);

        layout.add_float(id(3), 0.5, 0.5);
        let float = layout.float_rect(id(3), WINDOW).unwrap();
        let (cx, cy) = (f64::from(float.x + float.w / 2), f64::from(float.y + float.h / 2));
        assert_eq!(layout.hit_test(cx, cy, WINDOW, PARAMS), Hit::Pane(id(3)));
        // El anillo mueve, la banda del borde redimensiona
        assert_eq!(layout.hit_test(f64::from(float.x) + 8.0, cy, WINDOW, PARAMS), Hit::FloatMove(id(3)));
        assert_eq!(
            layout.hit_test(f64::from(float.x) + 2.0, cy, WINDOW, PARAMS),
            Hit::FloatResize(id(3), Edge { left: true, ..Edge::default() })
        );
        assert_eq!(
            layout.hit_test(f64::from(float.right()) - 1.0, f64::from(float.bottom()) - 1.0, WINDOW, PARAMS),
            Hit::FloatResize(id(3), Edge { right: true, bottom: true, ..Edge::default() })
        );
        // Un divisor tapado por el flotante no se agarra a través de él
        let hidden_divider = layout.hit_test(divider_x, cy, WINDOW, PARAMS);
        assert!(matches!(hidden_divider, Hit::Pane(_) | Hit::FloatMove(_) | Hit::FloatResize(..)));
        // Y ocultos los flotantes, se vuelve a ver lo de debajo
        layout.toggle_floats();
        assert_eq!(layout.hit_test(divider_x, cy, WINDOW, PARAMS), Hit::Divider(vec![], Axis::Horizontal));
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
}
