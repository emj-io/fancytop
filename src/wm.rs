//! Tiling window manager: panels tile the screen in a tree of row and column splits, like i3.
//!
//! Until the layout is first changed, the tree is rebuilt every frame from the responsive auto
//! layout, so the screen looks exactly as it always has. Dragging a divider, dropping a window
//! somewhere new or using a layout key switches to manual mode, which keeps the tree (scaling it
//! with the terminal) until `reset`.

use crate::ui::Area;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Panel {
    Cpu,
    Gpu,
    Mem,
    Io,
    Procs,
}

impl Panel {
    pub fn name(self) -> &'static str {
        match self {
            Panel::Cpu => "CPU",
            Panel::Gpu => "GPU",
            Panel::Mem => "Memory",
            Panel::Io => "I/O",
            Panel::Procs => "Processes",
        }
    }
}

/// Every panel, in the order of their number keys (and of `Panel as usize`).
pub const PANELS: [Panel; 5] = [Panel::Cpu, Panel::Gpu, Panel::Mem, Panel::Io, Panel::Procs];

/// How a split arranges its children: side by side, or stacked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Row,
    Col,
}

/// A direction to move focus or a window, or the side of a window a drop lands on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
    Up,
    Down,
}

impl Side {
    fn dir(self) -> Dir {
        match self {
            Side::Left | Side::Right => Dir::Row,
            Side::Up | Side::Down => Dir::Col,
        }
    }

    fn first(self) -> bool {
        matches!(self, Side::Left | Side::Up)
    }
}

/// A panel's minimum (width, height) when it is the given width wide.
pub type MinSize = Box<dyn Fn(Panel, i32) -> (i32, i32)>;

/// The layout tree. Each child of a split has a weight: its share of the split's length.
#[derive(Clone, Debug, PartialEq)]
enum Node {
    Leaf(Panel),
    Split { dir: Dir, kids: Vec<(f64, Node)> },
}

/// The boundary between child `i` and child `i + 1` of the split at `path`, as last laid out.
#[derive(Clone, Debug)]
struct Divider {
    path: Vec<usize>,
    i: usize,
    dir: Dir,
    /// First column (row split) or row (column split) of child `i + 1`.
    pos: i32,
    span: Area,
    sizes: Vec<i32>,
    mins: Vec<i32>,
}

impl Divider {
    /// Whether a cell is on this boundary: either window's border beside it, or for stacked
    /// windows the upper one's bottom border (the lower one's top border is its title bar).
    fn hit(&self, x: i32, y: i32) -> bool {
        match self.dir {
            Dir::Row => (x == self.pos - 1 || x == self.pos) && y >= self.span.y && y < self.span.bottom(),
            Dir::Col => y == self.pos - 1 && x >= self.span.x && x < self.span.right(),
        }
    }

    fn same(&self, other: &Divider) -> bool {
        self.path == other.path && self.i == other.i
    }
}

/// Where a dragged window would land: beside `target` on `side`, swapped with it (no side), or
/// along a whole edge of the screen (no target).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Drop {
    target: Option<Panel>,
    side: Option<Side>,
}

enum Drag {
    Move { panel: Panel, drop: Option<Drop> },
    Divide { grabs: Vec<Divider>, from: (i32, i32) },
}

pub struct Wm {
    pub manual: bool,
    /// Whether the focused window fills the screen.
    pub zoom: bool,
    focus: Panel,
    tree: Option<Node>,
    /// Panels hidden in auto mode. In manual mode a panel is shown when it's in the tree.
    hidden: [bool; 5],
    /// The screen area windows tile, as of the last `layout`.
    body: Area,
    tiles: Vec<(Panel, Area)>,
    divs: Vec<Divider>,
    drag: Option<Drag>,
    min: MinSize,
}

impl Wm {
    pub fn new(min: MinSize) -> Wm {
        Wm {
            manual: false,
            zoom: false,
            focus: Panel::Cpu,
            tree: None,
            hidden: [false; 5],
            body: Area::default(),
            tiles: Vec::new(),
            divs: Vec::new(),
            drag: None,
            min,
        }
    }

    pub fn visible(&self, p: Panel) -> bool {
        if self.manual {
            self.tiles.iter().any(|t| t.0 == p)
        } else {
            !self.hidden[p as usize]
        }
    }

    /// Tile `body` for this frame. In auto mode the windows go exactly where `auto` (indexed by
    /// panel, empty for no room) puts them, and the tree is rebuilt to match.
    pub fn layout(&mut self, body: Area, auto: &[Area; 5]) {
        self.body = body;
        if self.manual {
            self.relayout();
            return;
        }
        let shown: Vec<(Panel, Area)> = PANELS
            .iter()
            .map(|&p| (p, auto[p as usize]))
            .filter(|&(p, a)| !self.hidden[p as usize] && a.w > 0 && a.h > 0)
            .collect();
        self.tree = guillotine(&shown).map(normalize);
        self.relayout();
        self.tiles = shown;
        if !self.tiles.iter().any(|t| t.0 == self.focus)
            && let Some(&(p, _)) = self.tiles.first()
        {
            self.focus = p;
        }
    }

    /// The windows to draw this frame.
    pub fn tiles(&self) -> Vec<(Panel, Area)> {
        if self.zoom && self.tiles.iter().any(|t| t.0 == self.focus) {
            vec![(self.focus, self.body)]
        } else {
            self.tiles.clone()
        }
    }

    /// The window to highlight: the focused one, once windows are being arranged.
    pub fn highlight(&self) -> Option<Panel> {
        self.manual.then_some(self.focus)
    }

    /// While a window is dragged over somewhere it can go: where it would end up, and a label.
    pub fn preview(&self) -> Option<(Area, String)> {
        let Some(Drag::Move { panel, drop: Some(drop) }) = &self.drag else {
            return None;
        };
        let tree = apply_drop(self.tree.clone()?, *panel, *drop);
        let mut tiles = Vec::new();
        place(&tree, self.body, &self.min, &mut Vec::new(), &mut tiles, &mut Vec::new());
        let area = tiles.iter().find(|t| t.0 == *panel)?.1;
        let label = match drop.target {
            Some(t) if drop.side.is_none() => format!("swap with {}", t.name()),
            _ => panel.name().to_string(),
        };
        Some((area, label))
    }

    /// Whether a divider is being dragged.
    pub fn resizing(&self) -> bool {
        matches!(self.drag, Some(Drag::Divide { .. }))
    }

    /// Left button down: focus the window under the pointer, and start moving it (on its title
    /// bar) or resizing (on a border shared with another window). Returns whether anything changed.
    pub fn press(&mut self, x: i32, y: i32) -> bool {
        if self.zoom {
            return false;
        }
        let Some(&(panel, a)) = self.tiles.iter().find(|(_, a)| a.contains(x, y)) else {
            return false;
        };
        self.focus = panel;
        if y == a.y {
            self.manual = true;
            self.drag = Some(Drag::Move { panel, drop: None });
            return true;
        }
        // At most one divider per direction, so a corner where two meet drags both.
        let mut grabs: Vec<Divider> = Vec::new();
        for d in &self.divs {
            if d.hit(x, y) && !grabs.iter().any(|g| g.dir == d.dir) {
                grabs.push(d.clone());
            }
        }
        if !grabs.is_empty() {
            self.manual = true;
            self.drag = Some(Drag::Divide { grabs, from: (x, y) });
        }
        true
    }

    pub fn drag_to(&mut self, x: i32, y: i32) -> bool {
        match self.drag.take() {
            None => false,
            Some(Drag::Move { panel, drop }) => {
                let new = self.drop_at(x, y, panel);
                self.drag = Some(Drag::Move { panel, drop: new });
                new != drop
            }
            Some(Drag::Divide { grabs, from }) => {
                for g in &grabs {
                    let d = if g.dir == Dir::Row { x - from.0 } else { y - from.1 };
                    self.set_divider(g, g.sizes[g.i] + d, true);
                }
                self.drag = Some(Drag::Divide { grabs, from });
                true
            }
        }
    }

    /// Left button up: drop a dragged window where it was pointing.
    pub fn release(&mut self) -> bool {
        match self.drag.take() {
            Some(Drag::Move { panel, drop: Some(drop) }) => {
                self.tree = self.tree.take().map(|t| apply_drop(t, panel, drop));
                self.relayout();
                true
            }
            Some(_) => true,
            None => false,
        }
    }

    /// Abandon a drag without dropping anything.
    pub fn cancel(&mut self) {
        self.drag = None;
    }

    /// Focus the next window in tiling order, or with `back` the previous one.
    pub fn cycle(&mut self, back: bool) -> bool {
        self.manual = true;
        let n = self.tiles.len();
        if n == 0 {
            return false;
        }
        let i = self.tiles.iter().position(|t| t.0 == self.focus).unwrap_or(0);
        self.focus = self.tiles[if back { (i + n - 1) % n } else { (i + 1) % n }].0;
        true
    }

    /// Focus the window beside the focused one on `side`.
    pub fn focus_toward(&mut self, side: Side) -> bool {
        self.manual = true;
        if let Some(p) = self.neighbor(side) {
            self.focus = p;
        }
        true
    }

    /// Swap the focused window with its neighbour on `side`, or with none, move it to span that
    /// edge of the screen.
    pub fn move_toward(&mut self, side: Side) -> bool {
        self.manual = true;
        self.drag = None;
        let drop = match self.neighbor(side) {
            Some(t) => Drop {
                target: Some(t),
                side: None,
            },
            None => {
                let Some(&(_, a)) = self.tiles.iter().find(|t| t.0 == self.focus) else {
                    return false;
                };
                let b = self.body;
                // Already spanning that edge: nowhere further to go.
                if match side.dir() {
                    Dir::Row => a.h == b.h,
                    Dir::Col => a.w == b.w,
                } {
                    return true;
                }
                Drop {
                    target: None,
                    side: Some(side),
                }
            }
        };
        self.tree = self.tree.take().map(|t| apply_drop(t, self.focus, drop));
        self.relayout();
        true
    }

    /// Grow or shrink the focused window's width (`Dir::Row`) or height (`Dir::Col`), moving the
    /// nearest divider that can change it.
    pub fn resize(&mut self, dir: Dir, grow: bool) -> bool {
        self.manual = true;
        let Some(path) = self.tree.as_ref().and_then(|t| path_to(t, self.focus)) else {
            return false;
        };
        let step = if dir == Dir::Row { 2 } else { 1 };
        for depth in (0..path.len()).rev() {
            let (parent, i) = (&path[..depth], path[depth]);
            let find = |j: usize| self.divs.iter().find(|d| d.dir == dir && d.path == parent && d.i == j);
            let (d, delta) = match (find(i), i.checked_sub(1).and_then(find)) {
                (Some(d), _) => (d.clone(), if grow { step } else { -step }),
                (None, Some(d)) => (d.clone(), if grow { -step } else { step }),
                _ => continue,
            };
            self.set_divider(&d, d.sizes[d.i] + delta, false);
            return true;
        }
        false
    }

    /// Switch the split holding the focused window between side by side and stacked.
    pub fn flip(&mut self) -> bool {
        self.manual = true;
        let Some(tree) = &mut self.tree else { return false };
        let Some(path) = path_to(tree, self.focus).filter(|p| !p.is_empty()) else {
            return false;
        };
        if let Node::Split { dir, .. } = node_mut(tree, &path[..path.len() - 1]) {
            *dir = if *dir == Dir::Row { Dir::Col } else { Dir::Row };
        }
        self.tree = self.tree.take().map(normalize);
        self.relayout();
        true
    }

    pub fn toggle_zoom(&mut self) -> bool {
        self.zoom = !self.zoom;
        true
    }

    /// Show or hide a panel. In manual mode a shown panel splits the focused window along its
    /// longer side and takes focus.
    pub fn toggle(&mut self, p: Panel) -> bool {
        self.drag = None;
        if !self.manual {
            self.hidden[p as usize] ^= true;
            return true;
        }
        let shown = self.visible(p);
        let tree = self.tree.take();
        self.tree = if shown {
            tree.and_then(|t| remove(t, p)).map(normalize)
        } else {
            let beside = self.tiles.iter().find(|t| t.0 == self.focus).or(self.tiles.first()).copied();
            Some(match (tree, beside) {
                (Some(t), Some((target, a))) => {
                    let side = if a.w > 2 * a.h { Side::Right } else { Side::Down };
                    normalize(insert(t, target, p, side))
                }
                _ => Node::Leaf(p),
            })
        };
        if !shown {
            self.focus = p;
        }
        self.relayout();
        if shown && self.focus == p {
            self.focus = self.tiles.first().map_or(p, |t| t.0);
        }
        true
    }

    /// Back to the auto layout with every panel shown.
    pub fn reset(&mut self) -> bool {
        self.manual = false;
        self.zoom = false;
        self.hidden = [false; 5];
        self.drag = None;
        true
    }

    fn relayout(&mut self) {
        self.tiles.clear();
        self.divs.clear();
        if let Some(t) = &self.tree {
            place(t, self.body, &self.min, &mut Vec::new(), &mut self.tiles, &mut self.divs);
        }
    }

    /// The window touching the focused one on `side` that shares the most of that edge.
    fn neighbor(&self, side: Side) -> Option<Panel> {
        let a = self.tiles.iter().find(|t| t.0 == self.focus)?.1;
        let overlap = |a0: i32, a1: i32, b0: i32, b1: i32| a1.min(b1) - a0.max(b0);
        self.tiles
            .iter()
            .filter_map(|&(p, t)| {
                let (touch, shared) = match side {
                    Side::Left => (t.right() == a.x, overlap(t.y, t.bottom(), a.y, a.bottom())),
                    Side::Right => (t.x == a.right(), overlap(t.y, t.bottom(), a.y, a.bottom())),
                    Side::Up => (t.bottom() == a.y, overlap(t.x, t.right(), a.x, a.right())),
                    Side::Down => (t.y == a.bottom(), overlap(t.x, t.right(), a.x, a.right())),
                };
                (touch && shared > 0).then_some((shared, t, p))
            })
            .min_by_key(|&(shared, t, _)| (-shared, t.x, t.y))
            .map(|(_, _, p)| p)
    }

    /// Where `panel` would land if dropped at a cell: the header, footer and outer columns span
    /// that screen edge; within another window, the middle swaps and elsewhere tiles beside it
    /// on the nearest side.
    fn drop_at(&self, x: i32, y: i32, panel: Panel) -> Option<Drop> {
        let b = self.body;
        let edge = if y < b.y {
            Some(Side::Up)
        } else if y >= b.bottom() {
            Some(Side::Down)
        } else if x <= b.x {
            Some(Side::Left)
        } else if x >= b.right() - 1 {
            Some(Side::Right)
        } else {
            None
        };
        if edge.is_some() {
            return Some(Drop {
                target: None,
                side: edge,
            });
        }
        let &(target, a) = self.tiles.iter().find(|(_, a)| a.contains(x, y))?;
        if target == panel {
            return None;
        }
        let u = ((x - a.x) as f64 + 0.5) / a.w as f64;
        let v = ((y - a.y) as f64 + 0.5) / a.h as f64;
        let side = if (0.3..0.7).contains(&u) && (0.3..0.7).contains(&v) {
            None
        } else {
            [(u, Side::Left), (1.0 - u, Side::Right), (v, Side::Up), (1.0 - v, Side::Down)]
                .into_iter()
                .min_by(|a, b| a.0.total_cmp(&b.0))
                .map(|(_, s)| s)
        };
        Some(Drop {
            target: Some(target),
            side,
        })
    }

    /// Move divider `g` (as it was when grabbed) so the child before it is `a` cells, keeping
    /// both children at least their minimum size. With `snap`, a position within a cell or two
    /// of another divider on the same axis, or of the pair's halfway point, lines up with it.
    fn set_divider(&mut self, g: &Divider, a: i32, snap: bool) {
        let (a0, b0) = (g.sizes[g.i], g.sizes[g.i + 1]);
        let (lo, hi) = (g.mins[g.i], a0 + b0 - g.mins[g.i + 1]);
        let mut a = a.min(hi).max(lo);
        if snap {
            let start = g.pos - a0;
            let reach = if g.dir == Dir::Row { 2 } else { 1 };
            let at = start + a;
            let best = self
                .divs
                .iter()
                .filter(|d| d.dir == g.dir && !d.same(g))
                .map(|d| d.pos)
                .chain([start + (a0 + b0) / 2])
                .filter(|&p| (p - at).abs() <= reach && (lo..=hi).contains(&(p - start)))
                .min_by_key(|&p| (p - at).abs());
            if let Some(p) = best {
                a = p - start;
            }
        }
        let Some(tree) = &mut self.tree else { return };
        let Node::Split { kids, .. } = node_mut(tree, &g.path) else {
            return;
        };
        // Pin every child at its current size so only this pair changes.
        for (k, &s) in kids.iter_mut().zip(&g.sizes) {
            k.0 = s as f64;
        }
        kids[g.i].0 = a as f64;
        kids[g.i + 1].0 = (a0 + b0 - a) as f64;
        self.relayout();
    }
}

// ---------------------------------------------------------------------------- layout

/// Lay `node` out in `a`, collecting each panel's area and every divider.
fn place(node: &Node, a: Area, min: &MinSize, path: &mut Vec<usize>, tiles: &mut Vec<(Panel, Area)>, divs: &mut Vec<Divider>) {
    let (dir, kids) = match node {
        Node::Leaf(p) => return tiles.push((*p, a)),
        Node::Split { dir, kids } => (*dir, kids),
    };
    let row = dir == Dir::Row;
    let mins: Vec<i32> = kids
        .iter()
        .map(|(_, k)| if row { min_w(k, min) } else { min_h(k, a.w, min) })
        .collect();
    let weights: Vec<f64> = kids.iter().map(|k| k.0).collect();
    let sizes = distribute(if row { a.w } else { a.h }, &weights, &mins);
    let mut at = if row { a.x } else { a.y };
    for (i, ((_, kid), &size)) in kids.iter().zip(&sizes).enumerate() {
        if i > 0 {
            divs.push(Divider {
                path: path.clone(),
                i: i - 1,
                dir,
                pos: at,
                span: a,
                sizes: sizes.clone(),
                mins: mins.clone(),
            });
        }
        let ka = if row {
            Area::new(at, a.y, size, a.h)
        } else {
            Area::new(a.x, at, a.w, size)
        };
        path.push(i);
        place(kid, ka, min, path, tiles, divs);
        path.pop();
        at += size;
    }
}

fn min_w(node: &Node, min: &MinSize) -> i32 {
    match node {
        Node::Leaf(p) => min(*p, 0).0,
        Node::Split { dir: Dir::Row, kids } => kids.iter().map(|(_, k)| min_w(k, min)).sum(),
        Node::Split { dir: Dir::Col, kids } => kids.iter().map(|(_, k)| min_w(k, min)).max().unwrap_or(0),
    }
}

fn min_h(node: &Node, w: i32, min: &MinSize) -> i32 {
    match node {
        Node::Leaf(p) => min(*p, w).1,
        Node::Split { dir: Dir::Col, kids } => kids.iter().map(|(_, k)| min_h(k, w, min)).sum(),
        Node::Split { dir: Dir::Row, kids } => {
            let mins: Vec<i32> = kids.iter().map(|(_, k)| min_w(k, min)).collect();
            let weights: Vec<f64> = kids.iter().map(|k| k.0).collect();
            let widths = distribute(w, &weights, &mins);
            kids.iter()
                .zip(widths)
                .map(|((_, k), kw)| min_h(k, kw, min))
                .max()
                .unwrap_or(0)
        }
    }
}

/// Share `len` cells by weight, giving each child at least its minimum when they fit. When they
/// don't, the last children are cut short.
fn distribute(len: i32, weights: &[f64], mins: &[i32]) -> Vec<i32> {
    let n = weights.len();
    let mut pinned = vec![false; n];
    let (mut free, mut wsum);
    loop {
        free = len - (0..n).filter(|&i| pinned[i]).map(|i| mins[i]).sum::<i32>();
        wsum = (0..n).filter(|&i| !pinned[i]).map(|i| weights[i].max(1e-6)).sum::<f64>();
        let short: Vec<usize> = (0..n)
            .filter(|&i| !pinned[i] && (free as f64 * weights[i].max(1e-6) / wsum) < mins[i] as f64)
            .collect();
        if short.is_empty() {
            break;
        }
        for i in short {
            pinned[i] = true;
        }
    }
    let (mut out, mut acc, mut prev) = (vec![0; n], 0.0, 0);
    for i in 0..n {
        if pinned[i] {
            out[i] = mins[i];
        } else {
            acc += weights[i].max(1e-6);
            let end = (free as f64 * acc / wsum).round() as i32;
            out[i] = end - prev;
            prev = end;
        }
    }
    let mut excess = out.iter().sum::<i32>() - len;
    for size in out.iter_mut().rev() {
        let cut = excess.min(*size).max(0);
        *size -= cut;
        excess -= cut;
    }
    out
}

// ---------------------------------------------------------------------------- tree edits

/// Rebuild a tree from tiles that cover a rectangle, by cutting it into full-length stacked
/// rows, or failing that, side-by-side columns. Each child is weighted by its size.
fn guillotine(tiles: &[(Panel, Area)]) -> Option<Node> {
    match tiles {
        [] => return None,
        [(p, _)] => return Some(Node::Leaf(*p)),
        _ => {}
    }
    for dir in [Dir::Col, Dir::Row] {
        let span = |a: Area| {
            if dir == Dir::Col {
                (a.y, a.bottom())
            } else {
                (a.x, a.right())
            }
        };
        let mut sorted = tiles.to_vec();
        sorted.sort_by_key(|t| span(t.1).0);
        let mut groups: Vec<Vec<(Panel, Area)>> = Vec::new();
        let mut reach = i32::MIN;
        for t in sorted {
            let (start, end) = span(t.1);
            if groups.is_empty() || start >= reach {
                groups.push(Vec::new());
            }
            groups.last_mut().unwrap().push(t);
            reach = reach.max(end);
        }
        if groups.len() > 1 {
            let kids = groups
                .iter()
                .filter_map(|g| {
                    let lo = g.iter().map(|t| span(t.1).0).min()?;
                    let hi = g.iter().map(|t| span(t.1).1).max()?;
                    Some(((hi - lo).max(1) as f64, guillotine(g)?))
                })
                .collect();
            return Some(Node::Split { dir, kids });
        }
    }
    // Overlapping tiles can't be cut apart; stack them instead.
    let kids = tiles.iter().map(|&(p, a)| (a.h.max(1) as f64, Node::Leaf(p))).collect();
    Some(Node::Split { dir: Dir::Col, kids })
}

/// Collapse one-child splits, and merge a split into its parent when they run the same way.
fn normalize(node: Node) -> Node {
    let Node::Split { dir, kids } = node else {
        return node;
    };
    let mut out = Vec::new();
    for (w, kid) in kids {
        match normalize(kid) {
            Node::Split { dir: d, kids: inner } if d == dir => {
                let total: f64 = inner.iter().map(|k| k.0).sum();
                out.extend(inner.into_iter().map(|(iw, k)| (w * iw / total, k)));
            }
            kid => out.push((w, kid)),
        }
    }
    if out.len() == 1 {
        out.pop().unwrap().1
    } else {
        Node::Split { dir, kids: out }
    }
}

fn apply_drop(tree: Node, panel: Panel, drop: Drop) -> Node {
    let Some(side) = drop.side else {
        return match drop.target {
            Some(t) => swap(tree, panel, t),
            None => tree,
        };
    };
    let Some(rest) = remove(tree, panel).map(normalize) else {
        return Node::Leaf(panel);
    };
    normalize(match drop.target {
        Some(t) => insert(rest, t, panel, side),
        None => wrap(rest, panel, side, 2.0),
    })
}

fn swap(node: Node, p: Panel, q: Panel) -> Node {
    match node {
        Node::Leaf(x) => Node::Leaf(if x == p {
            q
        } else if x == q {
            p
        } else {
            x
        }),
        Node::Split { dir, kids } => Node::Split {
            dir,
            kids: kids.into_iter().map(|(w, k)| (w, swap(k, p, q))).collect(),
        },
    }
}

fn remove(node: Node, p: Panel) -> Option<Node> {
    match node {
        Node::Leaf(x) => (x != p).then_some(node),
        Node::Split { dir, kids } => {
            let kids: Vec<_> = kids.into_iter().filter_map(|(w, k)| Some((w, remove(k, p)?))).collect();
            (!kids.is_empty()).then_some(Node::Split { dir, kids })
        }
    }
}

/// `new` tiled beside `target` on `side`, taking half its room. (`normalize` then merges the
/// new split into a parent running the same way.)
fn insert(node: Node, target: Panel, new: Panel, side: Side) -> Node {
    match node {
        Node::Leaf(x) if x == target => wrap(node, new, side, 1.0),
        Node::Leaf(_) => node,
        Node::Split { dir, kids } => Node::Split {
            dir,
            kids: kids.into_iter().map(|(w, k)| (w, insert(k, target, new, side))).collect(),
        },
    }
}

/// `node` with a new panel on `side`, the new one getting 1 / (1 + `weight`) of the room.
fn wrap(node: Node, new: Panel, side: Side, weight: f64) -> Node {
    let (a, b) = ((1.0, Node::Leaf(new)), (weight, node));
    Node::Split {
        dir: side.dir(),
        kids: if side.first() { vec![a, b] } else { vec![b, a] },
    }
}

fn path_to(node: &Node, p: Panel) -> Option<Vec<usize>> {
    match node {
        Node::Leaf(x) => (*x == p).then(Vec::new),
        Node::Split { kids, .. } => kids.iter().enumerate().find_map(|(i, (_, k))| {
            let mut path = path_to(k, p)?;
            path.insert(0, i);
            Some(path)
        }),
    }
}

fn node_mut<'a>(node: &'a mut Node, path: &[usize]) -> &'a mut Node {
    match (node, path.split_first()) {
        (Node::Split { kids, .. }, Some((&i, rest))) => node_mut(&mut kids[i].1, rest),
        (node, _) => node,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: Area = Area::new(0, 1, 100, 40);

    /// The auto layout's shape: CPU on top, GPU and memory side by side, I/O, then processes.
    fn auto() -> [Area; 5] {
        [
            Area::new(0, 1, 100, 12),
            Area::new(0, 13, 50, 12),
            Area::new(50, 13, 50, 12),
            Area::new(0, 25, 100, 6),
            Area::new(0, 31, 100, 10),
        ]
    }

    fn wm() -> Wm {
        let mut wm = Wm::new(Box::new(|_, _| (10, 4)));
        wm.layout(BODY, &auto());
        wm
    }

    fn area(wm: &Wm, p: Panel) -> Area {
        wm.tiles.iter().find(|t| t.0 == p).unwrap().1
    }

    #[test]
    fn tree_reproduces_auto_layout() {
        let mut wm = wm();
        wm.manual = true;
        wm.layout(BODY, &auto());
        for p in PANELS {
            assert_eq!(area(&wm, p), auto()[p as usize], "{p:?}");
        }
        // And scales with the terminal.
        wm.layout(Area::new(0, 1, 200, 80), &auto());
        assert_eq!(area(&wm, Panel::Gpu), Area::new(0, 25, 100, 24));
    }

    #[test]
    fn distribute_respects_minimums() {
        assert_eq!(distribute(10, &[1.0, 1.0], &[2, 2]), [5, 5]);
        assert_eq!(distribute(10, &[9.0, 1.0], &[2, 4]), [6, 4]);
        assert_eq!(distribute(5, &[1.0, 1.0], &[4, 4]), [4, 1]);
    }

    #[test]
    fn dragging_a_divider_resizes_only_its_neighbours() {
        let mut wm = wm();
        // GPU's right border.
        assert!(wm.press(49, 18));
        wm.drag_to(59, 18);
        assert!(wm.resizing());
        assert_eq!(area(&wm, Panel::Gpu), Area::new(0, 13, 60, 12));
        assert_eq!(area(&wm, Panel::Mem), Area::new(60, 13, 40, 12));
        assert_eq!(area(&wm, Panel::Cpu), auto()[0]);
        // Close to halfway, it snaps there.
        wm.drag_to(50, 18);
        assert_eq!(area(&wm, Panel::Gpu).w, 50);
        wm.release();
        // GPU's bottom border: I/O shrinks to its minimum, processes don't move.
        wm.press(10, 24);
        wm.drag_to(10, 30);
        assert_eq!(area(&wm, Panel::Gpu).h, 14);
        assert_eq!(area(&wm, Panel::Io), Area::new(0, 27, 100, 4));
        assert_eq!(area(&wm, Panel::Procs), auto()[4]);
        wm.release();
        // GPU's bottom-right corner drags both dividers.
        wm.press(49, 26);
        wm.drag_to(69, 25);
        assert_eq!(area(&wm, Panel::Gpu), Area::new(0, 13, 70, 13));
    }

    #[test]
    fn dropping_a_window_tiles_or_swaps_it() {
        let mut wm = wm();
        // GPU's title onto the right edge of processes.
        assert!(wm.press(10, 13));
        wm.drag_to(95, 36);
        let preview = wm.preview().unwrap();
        assert_eq!(preview, (Area::new(50, 31, 50, 10), "GPU".into()));
        wm.release();
        assert_eq!(area(&wm, Panel::Gpu), preview.0);
        assert_eq!(area(&wm, Panel::Procs), Area::new(0, 31, 50, 10));
        assert_eq!(area(&wm, Panel::Mem), Area::new(0, 13, 100, 12));
        // CPU onto the middle of memory swaps them.
        wm.press(5, 1);
        wm.drag_to(50, 19);
        assert_eq!(wm.preview().unwrap().1, "swap with Memory");
        wm.release();
        assert_eq!(area(&wm, Panel::Cpu), Area::new(0, 13, 100, 12));
        // I/O onto the left edge of the screen spans it, with a third of the width.
        wm.press(5, 25);
        wm.drag_to(0, 20);
        wm.release();
        assert_eq!(area(&wm, Panel::Io), Area::new(0, 1, 33, 40));
        // Dropped on itself, nothing happens.
        let before = wm.tiles.clone();
        wm.press(5, 1);
        wm.drag_to(10, 10);
        assert_eq!(wm.preview(), None);
        wm.release();
        assert_eq!(wm.tiles, before);
    }

    #[test]
    fn keyboard_focus_move_resize_flip() {
        let mut wm = wm();
        wm.focus_toward(Side::Down);
        assert_eq!(wm.highlight(), Some(Panel::Gpu));
        wm.move_toward(Side::Right);
        assert_eq!(area(&wm, Panel::Gpu), Area::new(50, 13, 50, 12));
        // GPU is now the right child, so growing moves the divider to its left.
        wm.resize(Dir::Row, true);
        assert_eq!(area(&wm, Panel::Gpu), Area::new(48, 13, 52, 12));
        wm.resize(Dir::Col, false);
        assert_eq!(area(&wm, Panel::Gpu).h, 11);
        wm.flip();
        assert_eq!(area(&wm, Panel::Gpu).w, 100);
        assert_eq!(area(&wm, Panel::Mem).w, 100);
        // Moving off the screen's left edge spans it.
        wm.move_toward(Side::Left);
        assert_eq!(area(&wm, Panel::Gpu).h, 40);
        wm.toggle_zoom();
        assert_eq!(wm.tiles(), [(Panel::Gpu, BODY)]);
    }

    #[test]
    fn hiding_and_showing_panels() {
        let mut wm = wm();
        wm.toggle(Panel::Gpu);
        assert!(!wm.visible(Panel::Gpu));
        wm.layout(BODY, &auto());
        wm.cycle(false);
        wm.toggle(Panel::Mem);
        assert!(!wm.visible(Panel::Mem));
        assert!(wm.focus != Panel::Mem);
        // Shown again beside the focused window, along its longer side.
        wm.focus = Panel::Cpu;
        wm.toggle(Panel::Mem);
        assert_eq!(wm.highlight(), Some(Panel::Mem));
        assert_eq!(area(&wm, Panel::Mem).x, 50);
        wm.reset();
        assert!(!wm.manual && wm.visible(Panel::Gpu));
    }
}
