//! Behavior tests for the pane host, driven by egui pointer and key events in an 800 × 600
//! window. The harness's root ui has an 8 pt margin, so the host spans x 8..792, y 8..592.

use std::collections::HashMap;

use egui::{Event, Id, Key, Modifiers, PointerButton, Pos2, Rect, Ui, pos2, vec2};
use egui_kittest::{Harness, kittest::Queryable};

use super::*;

const L: f32 = 8.0;
const R: f32 = 792.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Pane {
    Nav,
    Styles,
    Comments,
    /// A context pane, like PivotTable Fields: not closable, not movable.
    Pivot,
}

struct Content;

impl PaneContent<Pane> for Content {
    fn title(&self, pane: &Pane) -> String {
        format!("{pane:?}")
    }
    fn body(&mut self, ui: &mut Ui, pane: &Pane) {
        ui.label(format!("{pane:?} body"));
    }
    fn default_state(&self, pane: &Pane) -> PaneState {
        let edge = if matches!(pane, Pane::Comments | Pane::Pivot) { Edge::End } else { Edge::Start };
        PaneState::docked(edge, 250.0)
    }
    fn movable(&self, pane: &Pane) -> bool {
        *pane != Pane::Pivot
    }
    fn closable(&self, pane: &Pane) -> bool {
        *pane != Pane::Pivot
    }
}

/// The app side: open list, persisted states and every request it received.
struct App {
    open: Vec<Pane>,
    states: HashMap<Pane, PaneState>,
    log: Vec<PaneRequest<Pane>>,
    focused: Option<Pane>,
    to_document: usize,
    rtl: bool,
    policy: SidePolicy,
}

impl App {
    fn new(open: &[Pane]) -> Self {
        Self { open: open.to_vec(), states: HashMap::new(), log: Vec::new(), focused: None, to_document: 0, rtl: false, policy: SidePolicy::Columns }
    }

    fn frame(&mut self, ui: &mut Ui) {
        let states = self.states.clone();
        let out = PaneHost::new("panes").rtl(self.rtl).policy(self.policy).show(ui, &self.open, |p| states.get(p).copied(), &mut Content);
        egui::CentralPanel::default().show(ui, |ui| ui.label("Document"));
        for request in &out.requests {
            match request {
                PaneRequest::Close(p) => self.open.retain(|o| o != p),
                PaneRequest::Focus(_) => {}
                other => {
                    let pane = *other.pane();
                    let state = self.states.entry(pane).or_insert_with(|| Content.default_state(&pane));
                    state.apply(other);
                }
            }
        }
        self.log.extend(out.requests);
        self.focused = out.focused;
        self.to_document += usize::from(out.to_document);
    }

    fn place(&self, pane: Pane) -> Place {
        self.states.get(&pane).map_or_else(|| Content.default_state(&pane).place, |s| s.place)
    }
}

fn harness(app: App) -> Harness<'static, App> {
    let mut h = Harness::builder().with_size(vec2(800.0, 600.0)).build_ui_state(|ui, app: &mut App| app.frame(ui), app);
    h.input_mut().focused = true;
    h.run_steps(2);
    h
}

fn pane_id(pane: Pane) -> Id {
    Id::new("panes").with(("pane", &pane))
}

fn rect(h: &Harness<'_, App>, id: Id) -> Option<Rect> {
    h.ctx.read_response(id).map(|r| r.rect)
}

fn title(h: &Harness<'_, App>, pane: Pane) -> Rect {
    rect(h, pane_id(pane).with("title")).unwrap()
}

/// The x of a side pane's inner edge.
fn edge(h: &Harness<'_, App>, pane: Pane) -> f32 {
    rect(h, pane_id(pane).with("resize")).unwrap().center().x
}

fn button(pos: Pos2, pressed: bool) -> Event {
    Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE }
}

fn send(h: &mut Harness<'_, App>, events: Vec<Event>) {
    h.input_mut().events.extend(events);
    h.run_steps(1);
}

fn click(h: &mut Harness<'_, App>, at: Pos2) {
    send(h, vec![Event::PointerMoved(at), button(at, true)]);
    send(h, vec![button(at, false)]);
    h.run_steps(1);
}

fn hold(h: &mut Harness<'_, App>, from: Pos2, to: Pos2) {
    send(h, vec![Event::PointerMoved(from), button(from, true)]);
    send(h, vec![Event::PointerMoved(from + vec2(8.0, 6.0))]);
    send(h, vec![Event::PointerMoved(to)]);
}

fn drag(h: &mut Harness<'_, App>, from: Pos2, to: Pos2) {
    hold(h, from, to);
    send(h, vec![button(to, false)]);
    h.run_steps(2);
}

fn key(h: &mut Harness<'_, App>, key: Key, modifiers: Modifiers) {
    send(h, vec![Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }]);
    h.run_steps(1);
}

/// The requests that change a pane's state or open list.
fn changes(app: &App) -> Vec<PaneRequest<Pane>> {
    app.log.iter().filter(|r| !matches!(r, PaneRequest::Focus(_))).cloned().collect()
}

fn last_resize(app: &App, pane: Pane) -> Option<f32> {
    app.log.iter().rev().find_map(|r| match r {
        PaneRequest::Resize(p, size) if *p == pane => Some(*size),
        _ => None,
    })
}

#[test]
fn columns_sit_side_by_side_resize_and_move_to_end() {
    let mut h = harness(App::new(&[Pane::Nav, Pane::Styles]));
    assert_eq!(edge(&h, Pane::Nav), L + 250.0);
    assert_eq!(edge(&h, Pane::Styles), L + 500.0);
    assert!(title(&h, Pane::Nav).left() < L + 2.0);

    drag(&mut h, pos2(L + 250.0, 300.0), pos2(L + 300.0, 300.0));
    assert_eq!(last_resize(h.state(), Pane::Nav), Some(300.0));
    assert_eq!(edge(&h, Pane::Nav), L + 300.0);
    assert_eq!(edge(&h, Pane::Styles), L + 550.0, "the second column follows");

    let at = rect(&h, pane_id(Pane::Nav).with("title").with("menu")).unwrap().center();
    click(&mut h, at);
    for label in ["Move to Start", "Move to End", "Float", "Close"] {
        assert!(h.query_all_by_label(label).count() >= 1, "{label}");
    }
    h.get_by_label("Move to End").click();
    h.run_steps(3);
    assert!(h.state().log.contains(&PaneRequest::Place(Pane::Nav, Place::Docked(Edge::End))));
    assert_eq!(edge(&h, Pane::Nav), R - 300.0, "a 300 pt pane at the right edge");
    assert_eq!(edge(&h, Pane::Styles), L + 250.0);
}

#[test]
fn a_bottom_pane_dragged_to_a_side_previews_the_width_it_docks_at() {
    let mut app = App::new(&[Pane::Comments]);
    app.states.insert(Pane::Comments, PaneState::docked(Edge::Bottom, 300.0));
    let mut h = harness(app);
    let from = title(&h, Pane::Comments).center();
    let to = pos2(L + 10.0, 200.0);
    hold(&mut h, from, to);
    let previewed = h.output().shapes.iter().any(|clipped| match &clipped.shape {
        egui::Shape::Rect(shape) => shape.rect.left() == L + 1.0 && shape.rect.width() == 298.0,
        _ => false,
    });
    assert!(previewed, "a 300 pt side pane");
    send(&mut h, vec![button(to, false)]);
    h.run_steps(2);
    assert_eq!(h.state().place(Pane::Comments), Place::Docked(Edge::Start));
    assert_eq!(edge(&h, Pane::Comments), L + 300.0);
}

#[test]
fn title_dragged_out_floats_and_a_drop_near_an_edge_docks() {
    // Drag-out-to-float and the 32 pt edge snap follow wordcraft#2584.
    let mut h = harness(App::new(&[Pane::Styles]));
    let from = title(&h, Pane::Styles).center();
    // Within DOCK_SNAP of the pane: nothing happens.
    drag(&mut h, from, pos2(L + 270.0, 300.0));
    assert!(changes(h.state()).is_empty(), "{:?}", h.state().log);

    drag(&mut h, from, pos2(400.0, 300.0));
    assert_eq!(
        changes(h.state()),
        vec![PaneRequest::Place(Pane::Styles, Place::Floating), PaneRequest::FloatRect(Pane::Styles, [360.0, 112.0, 270.0, 480.0])],
        "floats under the pointer, clamped to the window"
    );
    assert_eq!(title(&h, Pane::Styles).min, pos2(360.0, 116.0));

    // Dropped within 32 pt of the right edge: docked at End.
    h.state_mut().log.clear();
    let from = title(&h, Pane::Styles).center();
    drag(&mut h, from, pos2(780.0, 300.0));
    assert_eq!(h.state().log.last(), Some(&PaneRequest::Place(Pane::Styles, Place::Docked(Edge::End))));
    assert_eq!(edge(&h, Pane::Styles), R - 250.0);
    // The floating rect's size is kept for the next float.
    assert_eq!(h.state().states[&Pane::Styles].float_rect.map(|r| [r[2], r[3]]), Some([270.0, 480.0]));
}

#[test]
fn escape_and_focus_loss_restore_a_floating_pane_during_a_move() {
    let mut app = App::new(&[Pane::Styles]);
    let start = [300.0, 100.0, 270.0, 400.0];
    app.states.insert(Pane::Styles, PaneState { place: Place::Floating, size: None, float_rect: Some(start) });
    let mut h = harness(app);
    let from = title(&h, Pane::Styles).center();
    let float_rect = |h: &Harness<'_, App>| h.state().states[&Pane::Styles].float_rect;

    hold(&mut h, from, from + vec2(40.0, 30.0));
    assert_eq!(float_rect(&h), Some([340.0, 130.0, 270.0, 400.0]), "follows the pointer");
    key(&mut h, Key::Escape, Modifiers::NONE);
    assert_eq!(float_rect(&h), Some(start));
    send(&mut h, vec![Event::PointerMoved(pos2(790.0, 300.0))]);
    send(&mut h, vec![button(pos2(790.0, 300.0), false)]);
    h.run_steps(1);
    assert_eq!(h.state().place(Pane::Styles), Place::Floating, "the rest of the drag is ignored");
    assert_eq!(float_rect(&h), Some(start));

    hold(&mut h, from, from + vec2(-60.0, 20.0));
    assert_eq!(float_rect(&h), Some([240.0, 120.0, 270.0, 400.0]));
    h.input_mut().focused = false;
    h.run_steps(1);
    h.input_mut().focused = true;
    send(&mut h, vec![button(from + vec2(-60.0, 20.0), false)]);
    assert_eq!(float_rect(&h), Some(start));
}

#[test]
fn f6_cycles_through_the_panes_and_back_to_the_document() {
    let mut h = harness(App::new(&[Pane::Nav, Pane::Comments]));
    let focused_widget = |h: &Harness<'_, App>| h.ctx.memory(|m| m.focused());
    key(&mut h, Key::F6, Modifiers::NONE);
    assert_eq!(h.state().focused, Some(Pane::Nav));
    assert_eq!(focused_widget(&h), Some(pane_id(Pane::Nav).with("title")));
    key(&mut h, Key::F6, Modifiers::NONE);
    assert_eq!(h.state().focused, Some(Pane::Comments));
    key(&mut h, Key::F6, Modifiers::NONE);
    assert_eq!((h.state().focused, h.state().to_document), (None, 1));
    assert_eq!(focused_widget(&h), None);
    key(&mut h, Key::F6, Modifiers::SHIFT);
    assert_eq!(h.state().focused, Some(Pane::Comments), "Shift+F6 from the document goes to the last pane");
    let focus: Vec<_> = h.state().log.iter().filter(|r| matches!(r, PaneRequest::Focus(_))).cloned().collect();
    assert_eq!(focus, vec![PaneRequest::Focus(Pane::Nav), PaneRequest::Focus(Pane::Comments), PaneRequest::Focus(Pane::Comments)]);

    // A click in the document clears it; a click in a pane sets it.
    click(&mut h, pos2(400.0, 300.0));
    assert_eq!(h.state().focused, None);
    click(&mut h, pos2(100.0, 300.0));
    assert_eq!(h.state().focused, Some(Pane::Nav));
}

#[test]
fn one_slot_shows_the_last_pane_per_edge_beside_context_panes() {
    let mut app = App::new(&[Pane::Pivot, Pane::Comments, Pane::Styles]);
    app.policy = SidePolicy::OneSlot;
    app.states.insert(Pane::Styles, PaneState::docked(Edge::End, 300.0));
    let mut h = harness(app);
    assert!(rect(&h, pane_id(Pane::Comments).with("title")).is_none());
    assert_eq!(edge(&h, Pane::Pivot), R - 250.0, "the context pane stays, outermost");
    assert_eq!(edge(&h, Pane::Styles), R - 550.0);
    assert!(rect(&h, pane_id(Pane::Pivot).with("title").with("close")).is_none());

    let at = rect(&h, pane_id(Pane::Styles).with("title").with("close")).unwrap().center();
    click(&mut h, at);
    h.run_steps(1);
    assert_eq!(h.state().open, vec![Pane::Pivot, Pane::Comments]);
    assert_eq!(edge(&h, Pane::Comments), R - 500.0, "the slot shows the remaining pane");
}

#[test]
fn rtl_mirrors_sides_and_edge_snaps() {
    let mut app = App::new(&[Pane::Nav, Pane::Comments]);
    app.rtl = true;
    let mut h = harness(app);
    assert_eq!(edge(&h, Pane::Nav), R - 250.0, "Start is the right edge");
    assert_eq!(edge(&h, Pane::Comments), L + 250.0);

    let from = title(&h, Pane::Nav).center();
    drag(&mut h, from, pos2(10.0, 300.0));
    assert_eq!(changes(h.state()), vec![PaneRequest::Place(Pane::Nav, Place::Docked(Edge::End))], "the left edge is End");
    // Both on End, outermost first in open order.
    assert_eq!(edge(&h, Pane::Nav), L + 250.0);
    assert_eq!(edge(&h, Pane::Comments), L + 500.0);
}

#[test]
fn float_rect_is_finite_and_inside_the_bounds() {
    let bounds = Rect::from_min_size(pos2(0.0, 100.0), vec2(800.0, 500.0));
    let min = vec2(200.0, 160.0);
    assert_eq!(float_rect(Some([300.0, 150.0, 280.0, 300.0]), bounds, min), Rect::from_min_size(pos2(300.0, 150.0), vec2(280.0, 300.0)));
    for junk in [[f32::NAN, 0.0, 1.0, -3.0], [5000.0, -5000.0, 1.0e9, 1.0e9], [f32::NEG_INFINITY, f32::INFINITY, f32::NAN, 0.0]] {
        let r = float_rect(Some(junk), bounds, min);
        assert!(r.is_finite() && bounds.contains_rect(r) && r.width() >= 200.0, "{junk:?} -> {r:?}");
    }
    assert_eq!(float_rect(None, bounds, min).size(), FLOAT_DEFAULT.min(bounds.size()));
}

#[cfg(feature = "serde")]
#[test]
fn pane_state_round_trips_through_json() {
    let state = PaneState { place: Place::Docked(Edge::End), size: Some(280.0), float_rect: Some([1.0, 2.0, 270.0, 480.0]) };
    let json = serde_json::to_string(&state).unwrap();
    assert_eq!(serde_json::from_str::<PaneState>(&json).unwrap(), state);
}

#[test]
fn a_body_wider_than_its_pane_is_clipped_and_the_document_starts_at_the_pane_edge() {
    struct Wide;
    impl PaneContent<Pane> for Wide {
        fn title(&self, pane: &Pane) -> String {
            format!("{pane:?}")
        }
        fn body(&mut self, ui: &mut Ui, _: &Pane) {
            ui.allocate_exact_size(vec2(400.0, 40.0), Sense::hover());
        }
        fn default_state(&self, pane: &Pane) -> PaneState {
            PaneState::docked(if *pane == Pane::Comments { Edge::End } else { Edge::Start }, 250.0)
        }
    }
    let mut h = Harness::builder().with_size(vec2(800.0, 600.0)).build_ui_state(
        |ui, document: &mut Rect| {
            PaneHost::new("panes").show(ui, &[Pane::Nav, Pane::Comments], |_| None, &mut Wide);
            *document = ui.available_rect_before_wrap();
        },
        Rect::NOTHING,
    );
    h.run_steps(2);
    let document = *h.state();
    assert_eq!(document.left(), L + 250.0, "{document:?}");
    assert_eq!(document.right(), R - 250.0, "{document:?}");
}

#[test]
fn each_title_bar_is_named_after_its_pane() {
    let h = harness(App::new(&[Pane::Nav, Pane::Comments]));
    assert_eq!(h.query_all_by_label("Move Nav pane").count(), 1);
    assert_eq!(h.query_all_by_label("Move Comments pane").count(), 1);
}
