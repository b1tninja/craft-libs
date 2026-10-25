//! What the split tree adds around the groups: Maximize Frame, the outer edge band, hidden
//! panels, the float hint and the group menu tail.

use std::{borrow::Cow, collections::HashMap, fmt::Debug, hash::Hash};

use egui::{Context, Pos2, Rect, pos2, vec2};

use super::{GroupKind, Renderer, floating::WindowHit, group::TabGroup};
use crate::docking::{
    Action, Column, DockContent, DockEvent, Floating, Group, HidePanels, Layout, Node, Zone,
    dock_model::{Place, node_at},
};

/// How far below the pointer the float hint sits, clear of the cursor.
const HINT_OFFSET: f32 = 28.0;

/// What the renderer draws when it is not the layout itself, and the targets that depend on it.
pub(in crate::docking) struct FrameState<P> {
    /// View path to layout path of each split, when the view is not the layout.
    paths: Option<HashMap<Vec<bool>, Vec<bool>>>,
    /// The same for each floating window's splits, by the window's first shown panel.
    window_paths: HashMap<P, HashMap<Vec<bool>, Vec<bool>>>,
    /// The view node whose rect is the outer edge area; none while a frame is maximized.
    edge_path: Option<Vec<bool>>,
    pub(super) edge: Option<Rect>,
    pub(super) maximized: Option<P>,
    /// Floating windows as drawn, back to front.
    pub(super) windows: Vec<WindowHit<P>>,
}

/// The layout as shown: a maximized frame alone, and hidden panels left out.
pub(in crate::docking) fn view<'a, P: Clone + Eq + Hash>(
    layout: &'a Layout<P>,
    maximized: Option<&P>,
    hide: HidePanels<'_, P>,
    content: &dyn DockContent<P>,
) -> (Cow<'a, Layout<P>>, FrameState<P>) {
    let protected = |p: &P| content.permissions(p).is_protected();
    let hidden = |p: &P| match hide {
        HidePanels::None => false,
        HidePanels::All => !protected(p),
        HidePanels::Except(keep) => !protected(p) && !keep(p),
    };
    // Only a docked panel maximizes its frame.
    let maximized = maximized.and_then(|panel| match layout.place(panel)? {
        Place::Node(path) => Some((panel.clone(), path.clone(), layout.root.as_ref().and_then(|root| node_at(root, &path))?.clone())),
        place @ Place::Dock { .. } => {
            let group = layout.group(&place)?;
            Some((panel.clone(), path_of(&place), Node::Tabs { panels: group.panels.clone(), active: group.active }))
        }
        Place::Floating { .. } => None,
    });
    let mut state = FrameState { paths: None, window_paths: HashMap::new(), edge_path: None, edge: None, maximized: None, windows: Vec::new() };
    if maximized.is_none() && matches!(hide, HidePanels::None) {
        state.edge_path = Some(layout.edge_path());
        return (Cow::Borrowed(layout), state);
    }
    let mut paths = HashMap::new();
    let view = match maximized {
        Some((panel, path, node)) => {
            state.maximized = Some(panel);
            Layout { root: prune(&node, path, Vec::new(), &hidden, &mut paths), floating: Vec::new() }
        }
        None => {
            let root = layout.root.as_ref().and_then(|root| prune(root, Vec::new(), Vec::new(), &hidden, &mut paths));
            let mut floating = Vec::new();
            for window in &layout.floating {
                let mut paths = HashMap::new();
                let Some(root) = prune(&window.root, Vec::new(), Vec::new(), &hidden, &mut paths) else { continue };
                let shown = Floating { root, ..window.clone() };
                if let Some(key) = shown.panels().next() {
                    state.window_paths.insert(key.clone(), paths);
                }
                floating.push(shown);
            }
            Layout { root, floating }
        }
    };
    if state.maximized.is_none() {
        state.edge_path = Some(view.edge_path());
    }
    state.paths = Some(paths);
    (Cow::Owned(view), state)
}

fn path_of(place: &Place) -> Vec<bool> {
    match place {
        Place::Node(path) | Place::Dock { path, .. } => path.clone(),
        Place::Floating { .. } => Vec::new(),
    }
}

fn visible<P>(node: &Node<P>, hidden: &dyn Fn(&P) -> bool) -> bool {
    match node {
        Node::Split { first, second, .. } => visible(first, hidden) || visible(second, hidden),
        Node::Tabs { panels, .. } => panels.iter().any(|p| !hidden(p)),
        Node::Stack { entries } => entries.iter().any(|e| !hidden(&e.panel)),
        Node::Dock { columns, .. } => columns.iter().flat_map(|c| &c.groups).flat_map(|g| &g.panels).any(|p| !hidden(p)),
    }
}

/// `node` without hidden panels; a split with one visible side becomes that side. Records the
/// layout path of every split kept.
fn prune<P: Clone + PartialEq>(
    node: &Node<P>,
    path: Vec<bool>,
    at: Vec<bool>,
    hidden: &dyn Fn(&P) -> bool,
    paths: &mut HashMap<Vec<bool>, Vec<bool>>,
) -> Option<Node<P>> {
    let child = |path: &[bool], side: bool| [path, &[side]].concat();
    match node {
        Node::Split { axis, size, first, second } => match (visible(first, hidden), visible(second, hidden)) {
            (true, true) => {
                let first = prune(first, child(&path, false), child(&at, false), hidden, paths)?;
                let second = prune(second, child(&path, true), child(&at, true), hidden, paths)?;
                paths.insert(at, path);
                Some(Node::Split { axis: *axis, size: *size, first: Box::new(first), second: Box::new(second) })
            }
            (true, false) => prune(first, child(&path, false), at, hidden, paths),
            (false, true) => prune(second, child(&path, true), at, hidden, paths),
            (false, false) => None,
        },
        Node::Tabs { panels, active } => {
            let group = prune_group(&Group::new(panels.clone(), *active), hidden)?;
            Some(Node::Tabs { panels: group.panels, active: group.active })
        }
        Node::Stack { entries } => {
            let entries: Vec<_> = entries.iter().filter(|e| !hidden(&e.panel)).cloned().collect();
            (!entries.is_empty()).then_some(Node::Stack { entries })
        }
        Node::Dock { side, columns } => {
            let columns: Vec<_> = columns
                .iter()
                .filter_map(|c| {
                    let groups: Vec<_> = c.groups.iter().filter_map(|g| prune_group(g, hidden)).collect();
                    (!groups.is_empty()).then(|| Column { groups, ..c.clone() })
                })
                .collect();
            (!columns.is_empty()).then_some(Node::Dock { side: *side, columns })
        }
    }
}

/// A group without hidden panels; the selected panel stays selected when it is shown.
fn prune_group<P: Clone + PartialEq>(group: &Group<P>, hidden: &dyn Fn(&P) -> bool) -> Option<Group<P>> {
    let selected = group.active_panel();
    let panels: Vec<P> = group.panels.iter().filter(|p| !hidden(p)).cloned().collect();
    let active = selected.and_then(|s| panels.iter().position(|p| p == s)).unwrap_or(0);
    (!panels.is_empty()).then(|| Group { panels, active, ..group.clone() })
}

impl<P: Clone + Eq + Hash + Debug + Send + Sync + 'static> Renderer<'_, P> {
    pub(super) fn note_edge(&mut self, path: &[bool], rect: Rect) {
        if self.tree.is_none() && self.frame.edge_path.as_deref() == Some(path) {
            self.frame.edge = Some(rect);
        }
    }

    /// The layout path of a split drawn at view path `path`, in the tree being drawn.
    pub(super) fn original_path(&self, path: &[bool]) -> Vec<bool> {
        let paths = match &self.tree {
            None => self.frame.paths.as_ref(),
            Some((key, _)) => self.frame.window_paths.get(key),
        };
        paths.and_then(|paths| paths.get(path).cloned()).unwrap_or_else(|| path.to_vec())
    }

    /// The outer edge band under the pointer (P8): top and bottom, and left and right with
    /// [`crate::docking::DockStyle::side_edge_zones`]. Returns a docked anchor that is not
    /// moving, the zone and the preview: a line along the layout's whole edge, or with side edge
    /// zones the quarter the new frame takes. Without them, on a tab strip that takes tabs, the band reaches
    /// only [`crate::docking::targets::STRIP_REACH`] from the line.
    pub(super) fn edge_proposal(&self, members: &[P], pointer: Pos2) -> Option<(P, Zone, Rect)> {
        let area = self.frame.edge.filter(|area| area.contains(pointer))?;
        let band = if self.style.side_edge_zones { crate::docking::targets::SIDE_EDGE_BAND } else { self.style.edge_band };
        let band = band.min(area.width() / 4.0).min(area.height() / 4.0);
        let line = |zone| crate::docking::targets::edge_line(area, zone, self.style.drop_line_width);
        let mut edges = vec![(pointer.y - area.top(), Zone::Top), (area.bottom() - pointer.y, Zone::Bottom)];
        if self.style.side_edge_zones {
            edges.extend([(pointer.x - area.left(), Zone::Left), (area.right() - pointer.x, Zone::Right)]);
        }
        let (distance, zone) = edges.into_iter().min_by(|a, b| a.0.total_cmp(&b.0))?;
        if distance > band {
            return None;
        }
        let reach = |line: Rect| line.distance_to_pos(pointer) <= crate::docking::targets::STRIP_REACH;
        if !self.style.side_edge_zones && self.in_tab_strip(pointer) && !line(zone).is_some_and(reach) {
            return None;
        }
        let anchor =
            self.layout.panels().into_iter().find(|p| !members.contains(p) && !matches!(self.place(p), Some(Place::Floating { .. }) | None))?.clone();
        if !self.style.side_edge_zones {
            return Some((anchor, zone, line(zone)?));
        }
        let size = area.size() * 0.25;
        let preview = match zone {
            Zone::Left => Rect::from_min_size(area.min, vec2(size.x, area.height())),
            Zone::Right => Rect::from_min_max(pos2(area.right() - size.x, area.top()), area.max),
            Zone::Top => Rect::from_min_size(area.min, vec2(area.width(), size.y)),
            Zone::Bottom | Zone::Center => Rect::from_min_max(pos2(area.left(), area.bottom() - size.y), area.max),
        };
        Some((anchor, zone, preview))
    }

    /// Under the pointer while a drag offers a docking target: which modifier floats the drop
    /// instead. With native windows, the window under the pointer draws it, beside its preview.
    pub(super) fn paint_float_hint(&self, ctx: &Context, pointer: Pos2) {
        if !self.style.float_hint || self.style.float_hint_label.is_empty() {
            return;
        }
        let key = if ctx.os() == egui::os::OperatingSystem::Mac { "⌘" } else { "Ctrl" };
        let painter = self.preview_painter(ctx);
        let galley = painter.layout_no_wrap(self.style.float_hint_label.replace("{key}", key), self.style.font.clone(), self.style.text);
        let rect = Rect::from_center_size(pointer + vec2(0.0, HINT_OFFSET), galley.size() + vec2(12.0, 6.0));
        let rect = crate::docking::chrome::clamp(rect, self.float_bounds);
        painter.rect(rect, 4.0, self.style.floating_background, egui::Stroke::new(1.0, self.style.accent), egui::StrokeKind::Inside);
        painter.galley(rect.center() - galley.size() * 0.5, galley, self.style.text);
    }

    /// Ctrl/Cmd held when a split-tree tab drag starts: the drop floats (S9).
    pub(super) fn latch_undock(&self, ctx: &Context, kind: Option<GroupKind>) {
        let command = ctx.input(|i| i.modifiers.command || i.modifiers.ctrl || i.modifiers.mac_cmd);
        if self.style.command_press_floats && matches!(kind, Some(GroupKind::Frame | GroupKind::Floating { .. })) && command {
            ctx.data_mut(|d| {
                if let Some(drag) = d.get_temp::<crate::docking::Drag<P>>(self.id.with("drag")) {
                    d.insert_temp(self.id.with("drag"), crate::docking::Drag { undock: true, ..drag });
                }
            });
        }
    }

    /// The menu tail: Close Other Panels in Group and Maximize Frame when the style shows
    /// them.
    pub(super) fn frame_items(&mut self, ui: &mut egui::Ui, owner: &TabGroup<'_, P>, panel: &P) {
        let others = owner.panels.iter().filter(|p| *p != panel).all(|p| self.permissions(p).closable);
        if self.style.close_others_item
            && owner.panels.len() > 1
            && others
            && self.style.close_menu_items
            && self.menu_action(ui, &self.style.close_others_label.clone())
        {
            self.output.actions.push(Action::CloseOthers { panel: panel.clone() });
        }
        if self.style.maximize_item && owner.kind == GroupKind::Frame {
            let restore = self.frame.maximized.as_ref().is_some_and(|m| owner.panels.contains(m));
            let label = if restore { self.style.restore_frame_label.clone() } else { self.style.maximize_label.clone() };
            if self.menu_action(ui, &label) {
                let panel = if restore { self.frame.maximized.clone().unwrap_or_else(|| panel.clone()) } else { panel.clone() };
                self.output.events.push(DockEvent::ToggleMaximize { panel });
            }
        }
    }
}
