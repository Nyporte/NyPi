//! The System screen (decision D65): the whole program as a map. Every module is a box in the
//! region it belongs to, every connection a line, and the colours say what is alive right now:
//! working, ready, not there, turned off, or missing. A connection something is going over
//! moves. Pointing at a module lights what it is connected to; selecting one explains it. The
//! service says what exists and its state (`/api/system`); this file decides where to draw it.

use std::collections::HashMap;

use leptos::prelude::*;
use leptos::task::spawn_local;
use nypi_board_proto::{EdgeKind, NodeState, TopoEdge, TopoNode, Topology};

use crate::{Board, api};

/// The map is drawn in these units and scaled to the screen.
const WIDTH: f64 = 1280.0;
const PAD: f64 = 12.0;
const GAP: f64 = 12.0;
/// Room at the top of a region for its title.
const TITLE: f64 = 32.0;
const NODE_HEIGHT: f64 = 56.0;
/// How often the map is read again while the screen is open.
const REFRESH_MS: f64 = 2500.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    fn center(self) -> (f64, f64) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    /// Where the straight line from the centre toward `to` leaves the box.
    fn edge_toward(self, to: (f64, f64)) -> (f64, f64) {
        let (cx, cy) = self.center();
        let (dx, dy) = (to.0 - cx, to.1 - cy);
        if dx == 0.0 && dy == 0.0 {
            return (cx, cy);
        }
        let scale = f64::min(
            if dx == 0.0 { f64::INFINITY } else { self.w / 2.0 / dx.abs() },
            if dy == 0.0 { f64::INFINITY } else { self.h / 2.0 / dy.abs() },
        );
        (cx + dx * scale, cy + dy * scale)
    }
}

/// Where each known region goes and how many modules it shows per row: the picture's shape.
/// Ways in on top, what is kept at the bottom, Lucia in the middle between what keeps her
/// running and what she works with.
const REGIONS: [(&str, Rect, usize); 5] = [
    ("you", Rect { x: 16.0, y: 16.0, w: 1248.0, h: 104.0 }, 6),
    ("always", Rect { x: 16.0, y: 148.0, w: 300.0, h: 324.0 }, 2),
    ("lucia", Rect { x: 344.0, y: 148.0, w: 592.0, h: 324.0 }, 4),
    ("resources", Rect { x: 964.0, y: 148.0, w: 300.0, h: 324.0 }, 2),
    ("stores", Rect { x: 16.0, y: 500.0, w: 1248.0, h: 104.0 }, 5),
];

#[derive(Clone, Debug, PartialEq)]
pub struct Region {
    pub id: String,
    pub title: String,
    pub note: String,
    pub rect: Rect,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Placed {
    pub regions: Vec<Region>,
    pub nodes: HashMap<String, Rect>,
    pub height: f64,
}

/// Put every region and module of `map` on the canvas. A region this file does not know (a
/// newer service) gets a full-width band below the others, so nothing is ever left off.
pub fn place(map: &Topology) -> Placed {
    let mut placed = Placed::default();
    let mut bottom = REGIONS.iter().map(|(_, rect, _)| rect.y + rect.h).fold(0.0, f64::max);
    for group in &map.groups {
        let members: Vec<&TopoNode> = map.nodes.iter().filter(|node| node.group == group.id).collect();
        let (rect, columns) = match REGIONS.iter().find(|(id, _, _)| *id == group.id) {
            Some((_, rect, columns)) => (*rect, *columns),
            None => {
                let columns = 6;
                let rows = members.len().div_ceil(columns).max(1) as f64;
                let rect = Rect {
                    x: 16.0,
                    y: bottom + 28.0,
                    w: WIDTH - 32.0,
                    h: TITLE + rows * (NODE_HEIGHT + GAP) + PAD - GAP,
                };
                bottom = rect.y + rect.h;
                (rect, columns)
            }
        };
        let rows = members.len().div_ceil(columns).max(1) as f64;
        let width = (rect.w - 2.0 * PAD - GAP * (columns as f64 - 1.0)) / columns as f64;
        // Rows share the room below the title evenly, so a tall region is not top-heavy.
        let pitch = (rect.h - TITLE - PAD) / rows;
        for (index, node) in members.iter().enumerate() {
            let (column, row) = ((index % columns) as f64, (index / columns) as f64);
            let node_rect = Rect {
                x: rect.x + PAD + column * (width + GAP),
                y: rect.y + TITLE + row * pitch + (pitch - NODE_HEIGHT).max(0.0) / 2.0,
                w: width,
                h: NODE_HEIGHT.min(pitch),
            };
            placed.nodes.insert(node.id.clone(), node_rect);
        }
        placed.regions.push(Region {
            id: group.id.clone(),
            title: group.title.clone(),
            note: group.note.clone(),
            rect,
        });
    }
    placed.height = bottom + 16.0;
    placed
}

/// A connection as drawn: a bent line from box to box, its arrowhead, and a place for its
/// label near each end (the label is shown at the end away from the module being looked at,
/// where the labels of a module with many connections do not pile up).
#[derive(Clone, Debug, PartialEq)]
pub struct Curve {
    pub path: String,
    pub arrow: String,
    pub label_near_from: (f64, f64),
    pub label_near_to: (f64, f64),
}

/// The point at `t` along the curve from `start` over `control` to `end`.
fn along(start: (f64, f64), control: (f64, f64), end: (f64, f64), t: f64) -> (f64, f64) {
    let (a, b, c) = ((1.0 - t) * (1.0 - t), 2.0 * (1.0 - t) * t, t * t);
    (a * start.0 + b * control.0 + c * end.0, a * start.1 + b * control.1 + c * end.1)
}

pub fn curve(from: Rect, to: Rect) -> Curve {
    let (from_centre, to_centre) = (from.center(), to.center());
    let between = (to_centre.0 - from_centre.0).abs() - (from.w + to.w) / 2.0;
    let (start, control, end) = if (from_centre.1 - to_centre.1).abs() < 1.0 && between > from.w * 0.6 {
        // Same row with boxes in between: over their tops, not behind them.
        let lift = (18.0 + between * 0.03).min(34.0);
        let (start, end) = ((from_centre.0, from.y), (to_centre.0, to.y));
        (start, ((start.0 + end.0) / 2.0, from.y - lift), end)
    } else {
        let (start, end) = (from.edge_toward(to_centre), to.edge_toward(from_centre));
        let (dx, dy) = (end.0 - start.0, end.1 - start.1);
        let length = (dx * dx + dy * dy).sqrt().max(1.0);
        // Bent to one side, so a line and its opposite do not lie on each other.
        let bend = (length * 0.12).min(26.0);
        let control = ((start.0 + end.0) / 2.0 - dy / length * bend, (start.1 + end.1) / 2.0 + dx / length * bend);
        (start, control, end)
    };
    let (tx, ty) = (end.0 - control.0, end.1 - control.1);
    let tangent = (tx * tx + ty * ty).sqrt().max(1.0);
    let (ux, uy) = (tx / tangent, ty / tangent);
    let base = (end.0 - ux * 9.0, end.1 - uy * 9.0);
    let arrow = format!(
        "{:.1},{:.1} {:.1},{:.1} {:.1},{:.1}",
        end.0,
        end.1,
        base.0 - uy * 4.2,
        base.1 + ux * 4.2,
        base.0 + uy * 4.2,
        base.1 - ux * 4.2
    );
    Curve {
        path: format!("M{:.1},{:.1} Q{:.1},{:.1} {:.1},{:.1}", start.0, start.1, control.0, control.1, base.0, base.1),
        arrow,
        label_near_from: along(start, control, end, 0.3),
        label_near_to: along(start, control, end, 0.7),
    }
}

/// `text` cut to what fits in `width` units at about `per_char` units a character.
pub fn fit(text: &str, width: f64, per_char: f64) -> String {
    let room = (width / per_char).floor().max(1.0) as usize;
    if text.chars().count() <= room {
        return text.to_string();
    }
    format!("{}…", text.chars().take(room.saturating_sub(1)).collect::<String>().trim_end())
}

fn state_class(state: NodeState) -> &'static str {
    match state {
        NodeState::Active => "active",
        NodeState::Ready => "ready",
        NodeState::Absent => "absent",
        NodeState::Off => "off",
        NodeState::Down => "down",
    }
}

fn state_word(state: NodeState) -> &'static str {
    match state {
        NodeState::Active => "Working",
        NodeState::Ready => "Ready",
        NodeState::Absent => "Not open",
        NodeState::Off => "Off",
        NodeState::Down => "Needs attention",
    }
}

fn kind_class(kind: EdgeKind) -> &'static str {
    match kind {
        EdgeKind::Message => "message",
        EdgeKind::Work => "work",
        EdgeKind::Watch => "watch",
        EdgeKind::Record => "record",
        EdgeKind::Notice => "notice",
    }
}

const KINDS: [(Option<EdgeKind>, &str); 6] = [
    (None, "All connections"),
    (Some(EdgeKind::Message), "Messages"),
    (Some(EdgeKind::Work), "Work"),
    (Some(EdgeKind::Watch), "Watching"),
    (Some(EdgeKind::Record), "Records"),
    (Some(EdgeKind::Notice), "Notices"),
];

const STATES: [NodeState; 5] =
    [NodeState::Active, NodeState::Ready, NodeState::Absent, NodeState::Off, NodeState::Down];

fn touches(edge: &TopoEdge, id: &str) -> bool {
    edge.from == id || edge.to == id
}

#[component]
pub fn SystemPage() -> impl IntoView {
    let board = expect_context::<Board>();
    api::live(board, "/api/live", |_| {});
    let map = RwSignal::new(None::<Topology>);
    let selected = RwSignal::new(None::<String>);
    let pointed = RwSignal::new(None::<String>);
    let kind = RwSignal::new(None::<EdgeKind>);
    let tick = Memo::new(move |_| (board.clock.get() / REFRESH_MS).floor() as i64);
    Effect::new(move |_| {
        tick.track();
        spawn_local(async move {
            if let Ok(found) = api::get::<Topology>(board, "/api/system").await {
                // Unchanged maps are not set again, so the picture is not redrawn for nothing.
                if map.with_untracked(|current| current.as_ref() != Some(&found)) {
                    map.set(Some(found));
                }
            }
        });
    });
    // What the lines and boxes are lit for: the module pointed at, else the one selected.
    let focus = Memo::new(move |_| pointed.get().or_else(|| selected.get()));
    let counts = move || {
        map.with(|map| {
            let nodes = map.as_ref().map(|map| map.nodes.as_slice()).unwrap_or_default();
            STATES.map(|state| (state, nodes.iter().filter(|node| node.state == state).count()))
        })
    };
    view! {
        <div class="system">
            <div class="system-head">
                <div>
                    <h1>"System"</h1>
                    <p class="muted">"Every module of NyPi, how they connect, and what is alive right now."</p>
                </div>
                <ul class="legend" aria-label="States">
                    {move || {
                        counts()
                            .into_iter()
                            .map(|(state, count)| {
                                view! {
                                    <li>
                                        <span class=format!("state-dot {}", state_class(state))></span>
                                        {state_word(state)}
                                        <span class="count">{count}</span>
                                    </li>
                                }
                            })
                            .collect_view()
                    }}
                </ul>
            </div>
            <div class="chips" role="group" aria-label="Connections shown">
                {KINDS
                    .iter()
                    .map(|(value, label)| {
                        let value = *value;
                        let class = value.map(kind_class).unwrap_or("all");
                        view! {
                            <button
                                class=format!("chip-button edge-chip {class}")
                                class:on=move || kind.get() == value
                                on:click=move |_| kind.set(value)
                            >
                                {*label}
                            </button>
                        }
                    })
                    .collect_view()}
            </div>
            {move || match map.get() {
                None => view! { <p class="loading">"Reading the system…"</p> }.into_any(),
                Some(map) => {
                    let placed = place(&map);
                    view! {
                        <Map map=map.clone() placed selected pointed focus kind/>
                        <Detail map=map.clone() selected/>
                        <Modules map selected/>
                    }
                        .into_any()
                }
            }}
        </div>
    }
}

/// The picture.
#[component]
fn Map(
    map: Topology,
    placed: Placed,
    selected: RwSignal<Option<String>>,
    pointed: RwSignal<Option<String>>,
    focus: Memo<Option<String>>,
    kind: RwSignal<Option<EdgeKind>>,
) -> impl IntoView {
    let regions = placed
        .regions
        .iter()
        .map(|region| {
            let rect = region.rect;
            // The note fits beside the title only in the wide regions.
            let note = (rect.w > 520.0).then(|| region.note.clone());
            view! {
                <g class="region">
                    <rect x=rect.x y=rect.y width=rect.w height=rect.h rx="14"></rect>
                    <text class="region-title" x=rect.x + 14.0 y=rect.y + 21.0>
                        {region.title.to_uppercase()}
                    </text>
                    {note
                        .map(|note| {
                            view! {
                                <text class="region-note" x=rect.x + rect.w - 14.0 y=rect.y + 21.0 text-anchor="end">
                                    {note}
                                </text>
                            }
                        })}
                </g>
            }
        })
        .collect_view();
    let mut labels = Vec::new();
    let edges = map
        .edges
        .iter()
        .filter_map(|edge| {
            let (from, to) = (*placed.nodes.get(&edge.from)?, *placed.nodes.get(&edge.to)?);
            let curve = curve(from, to);
            let (a, b) = (edge.from.clone(), edge.to.clone());
            let lit = {
                let (a, b) = (a.clone(), b.clone());
                move || focus.with(|focus| focus.as_deref().is_some_and(|id| id == a || id == b))
            };
            let dim = {
                let (lit, edge_kind) = (lit.clone(), edge.kind);
                move || {
                    let other_kind = kind.get().is_some_and(|wanted| wanted != edge_kind);
                    other_kind || (focus.with(Option::is_some) && !lit())
                }
            };
            // The label sits near the end away from the module in focus.
            let (near_from, near_to) = (curve.label_near_from, curve.label_near_to);
            let source = StoredValue::new(a);
            let at = move || {
                let from_focused =
                    focus.with(|focus| source.with_value(|source| focus.as_deref() == Some(source.as_str())));
                if from_focused { near_to } else { near_from }
            };
            let (label, label_lit) = (edge.label.clone(), lit.clone());
            labels.push(view! {
                <Show when=label_lit.clone()>
                    <text class="edge-label" x=move || at().0 y=move || at().1 + 4.0 text-anchor="middle">
                        {label.clone()}
                    </text>
                </Show>
            });
            Some(view! {
                <g
                    class=format!("edge kind-{}", kind_class(edge.kind))
                    class:flow=edge.active
                    class:lit=lit
                    class:dim=dim
                >
                    <path d=curve.path></path>
                    <polygon points=curve.arrow></polygon>
                </g>
            })
        })
        .collect_view();
    let neighbours: HashMap<String, Vec<String>> = map
        .nodes
        .iter()
        .map(|node| {
            let near = map
                .edges
                .iter()
                .filter(|edge| touches(edge, &node.id))
                .map(|edge| if edge.from == node.id { edge.to.clone() } else { edge.from.clone() })
                .collect();
            (node.id.clone(), near)
        })
        .collect();
    let nodes = map
        .nodes
        .iter()
        .filter_map(|node| {
            let rect = *placed.nodes.get(&node.id)?;
            let id = node.id.clone();
            let near = neighbours.get(&node.id).cloned().unwrap_or_default();
            let is_selected = {
                let id = id.clone();
                move || selected.with(|selected| selected.as_deref() == Some(id.as_str()))
            };
            let dim = {
                let id = id.clone();
                move || {
                    focus.with(|focus| {
                        focus.as_deref().is_some_and(|focus| focus != id && !near.iter().any(|n| n == focus))
                    })
                }
            };
            let (click_id, enter_id) = (id.clone(), id.clone());
            Some(view! {
                <g
                    class=format!("node {}", state_class(node.state))
                    class:selected=is_selected
                    class:dim=dim
                    on:click=move |_| {
                        selected
                            .update(|selected| {
                                *selected = if selected.as_deref() == Some(click_id.as_str()) {
                                    None
                                } else {
                                    Some(click_id.clone())
                                };
                            })
                    }
                    on:mouseenter=move |_| pointed.set(Some(enter_id.clone()))
                    on:mouseleave=move |_| pointed.set(None)
                >
                    <rect x=rect.x y=rect.y width=rect.w height=rect.h rx="10"></rect>
                    <circle class="dot" cx=rect.x + rect.w - 14.0 cy=rect.y + 17.0 r="4.5"></circle>
                    <text class="node-name" x=rect.x + 12.0 y=rect.y + 22.0>
                        {fit(&node.name, rect.w - 40.0, 7.4)}
                    </text>
                    <text class="node-status" x=rect.x + 12.0 y=rect.y + 41.0>
                        {fit(&node.status, rect.w - 22.0, 5.9)}
                    </text>
                </g>
            })
        })
        .collect_view();
    view! {
        <div class="map-scroll">
            <svg
                class="map"
                viewBox=format!("0 0 {WIDTH} {}", placed.height)
                role="img"
                aria-label="Map of NyPi's modules and their connections"
            >
                {regions}
                <g class="edges">{edges}</g>
                <g class="nodes">{nodes}</g>
                // Above the boxes, so a label is never hidden by one.
                <g class="labels">{labels}</g>
            </svg>
        </div>
    }
}

/// What the selected module is, how it is, and what it is connected to.
#[component]
fn Detail(map: Topology, selected: RwSignal<Option<String>>) -> impl IntoView {
    let map = StoredValue::new(map);
    move || {
        let Some(id) = selected.get() else {
            return view! { <p class="hint detail-hint">"Select a module to see what it does, how it is, and what it talks to."</p> }
                .into_any();
        };
        let found = map.with_value(|map| {
            let node = map.nodes.iter().find(|node| node.id == id)?.clone();
            let name = |id: &str| map.nodes.iter().find(|n| n.id == id).map(|n| n.name.clone()).unwrap_or_default();
            let acts: Vec<(String, String, String, &'static str)> = map
                .edges
                .iter()
                .filter(|edge| edge.from == id)
                .map(|edge| (edge.to.clone(), name(&edge.to), edge.label.clone(), kind_class(edge.kind)))
                .collect();
            let acted: Vec<(String, String, String, &'static str)> = map
                .edges
                .iter()
                .filter(|edge| edge.to == id)
                .map(|edge| (edge.from.clone(), name(&edge.from), edge.label.clone(), kind_class(edge.kind)))
                .collect();
            Some((node, acts, acted))
        });
        let Some((node, acts, acted)) = found else {
            return view! { <p class="hint detail-hint">"That module is no longer on the map."</p> }.into_any();
        };
        let links = move |title: &'static str, rows: Vec<(String, String, String, &'static str)>| {
            (!rows.is_empty()).then(|| {
                view! {
                    <div class="links">
                        <h3>{title}</h3>
                        <ul>
                            {rows
                                .into_iter()
                                .map(|(id, name, label, kind)| {
                                    view! {
                                        <li>
                                            <span class=format!("edge-mark {kind}")></span>
                                            <button class="link-button" on:click=move |_| selected.set(Some(id.clone()))>
                                                {name}
                                            </button>
                                            <span class="muted">{label}</span>
                                        </li>
                                    }
                                })
                                .collect_view()}
                        </ul>
                    </div>
                }
            })
        };
        view! {
            <section class="card detail">
                <header>
                    <span class="title">{node.name.clone()}</span>
                    <span class=format!("state-pill {}", state_class(node.state))>{state_word(node.state)}</span>
                </header>
                <p>{node.role.clone()}</p>
                {(!node.status.is_empty()).then(|| view! { <p class="detail-status">{node.status.clone()}</p> })}
                {(!node.facts.is_empty())
                    .then(|| {
                        view! {
                            <dl class="facts">
                                {node
                                    .facts
                                    .iter()
                                    .map(|fact| {
                                        view! {
                                            <div>
                                                <dt>{fact.label.clone()}</dt>
                                                <dd>{fact.value.clone()}</dd>
                                            </div>
                                        }
                                    })
                                    .collect_view()}
                            </dl>
                        }
                    })}
                <div class="detail-links">
                    {links("Acts on", acts)}
                    {links("Acted on by", acted)}
                </div>
                <p class="detail-foot">
                    <code>{node.source.clone()}</code>
                    {node.link.clone().map(|link| view! { <a href=link>"Open on the board"</a> })}
                </p>
            </section>
        }
        .into_any()
    }
}

/// The same modules as a list, region by region: the map for a phone, and for reading.
#[component]
fn Modules(map: Topology, selected: RwSignal<Option<String>>) -> impl IntoView {
    map.groups
        .iter()
        .map(|group| {
            let members: Vec<TopoNode> = map.nodes.iter().filter(|node| node.group == group.id).cloned().collect();
            view! {
                <section class="section module-group">
                    <h2 class="heading">{group.title.clone()} <span class="count">{members.len()}</span></h2>
                    <p class="hint">{group.note.clone()}</p>
                    <ul class="modules">
                        {members
                            .into_iter()
                            .map(|node| {
                                let id = node.id.clone();
                                let here = id.clone();
                                view! {
                                    <li>
                                        <button
                                            class="module"
                                            class:on=move || selected.with(|s| s.as_deref() == Some(here.as_str()))
                                            on:click=move |_| selected.set(Some(id.clone()))
                                        >
                                            <span class=format!("state-dot {}", state_class(node.state))></span>
                                            <span class="module-name">{node.name.clone()}</span>
                                            <span class="module-status">{node.status.clone()}</span>
                                        </button>
                                    </li>
                                }
                            })
                            .collect_view()}
                    </ul>
                </section>
            }
        })
        .collect_view()
}

#[cfg(test)]
mod tests {
    use nypi_board_proto::TopoGroup;

    use super::*;

    fn node(id: &str, group: &str) -> TopoNode {
        TopoNode {
            id: id.into(),
            group: group.into(),
            name: id.into(),
            role: String::new(),
            source: String::new(),
            state: NodeState::Ready,
            status: String::new(),
            facts: Vec::new(),
            link: None,
        }
    }

    fn group(id: &str) -> TopoGroup {
        TopoGroup { id: id.into(), title: id.into(), note: String::new() }
    }

    fn inside(inner: Rect, outer: Rect) -> bool {
        inner.x >= outer.x
            && inner.y >= outer.y
            && inner.x + inner.w <= outer.x + outer.w + 0.01
            && inner.y + inner.h <= outer.y + outer.h + 0.01
    }

    fn overlap(a: Rect, b: Rect) -> bool {
        a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
    }

    #[test]
    fn every_module_gets_a_box_inside_its_region_and_none_overlap() {
        // The real map's shape: 6, 6, 11, 6, 5 modules, and a region this page has never heard of.
        let sizes = [("you", 6), ("always", 6), ("lucia", 11), ("resources", 6), ("stores", 5), ("later", 8)];
        let map = Topology {
            groups: sizes.iter().map(|(id, _)| group(id)).collect(),
            nodes: sizes
                .iter()
                .flat_map(|(id, count)| (0..*count).map(move |i| node(&format!("{id}-{i}"), id)))
                .collect(),
            edges: Vec::new(),
        };
        let placed = place(&map);
        assert_eq!((placed.regions.len(), placed.nodes.len()), (6, 42), "nothing is left off the map");
        for region in &placed.regions {
            for node in map.nodes.iter().filter(|node| node.group == region.id) {
                let rect = placed.nodes[&node.id];
                assert!(
                    inside(rect, region.rect),
                    "{} is outside {}: {rect:?} in {:?}",
                    node.id,
                    region.id,
                    region.rect
                );
                assert!(rect.w >= 120.0 && rect.h >= 40.0, "{} is too small to read: {rect:?}", node.id);
            }
        }
        let boxes: Vec<(&String, &Rect)> = placed.nodes.iter().collect();
        for (i, (a, ra)) in boxes.iter().enumerate() {
            for (b, rb) in &boxes[i + 1..] {
                assert!(!overlap(**ra, **rb), "{a} and {b} overlap");
            }
        }
        for (i, a) in placed.regions.iter().enumerate() {
            for b in &placed.regions[i + 1..] {
                assert!(!overlap(a.rect, b.rect), "regions {} and {} overlap", a.id, b.id);
            }
        }
        let later = placed.regions.iter().find(|region| region.id == "later").unwrap();
        assert!(later.rect.y > 600.0 && placed.height > later.rect.y + later.rect.h, "an unknown region goes below");
    }

    #[test]
    fn a_connection_runs_from_edge_to_edge_and_points_at_its_target() {
        let from = Rect { x: 0.0, y: 0.0, w: 100.0, h: 50.0 };
        let to = Rect { x: 300.0, y: 0.0, w: 100.0, h: 50.0 };
        assert_eq!(from.edge_toward(to.center()), (100.0, 25.0));
        assert_eq!(to.edge_toward(from.center()), (300.0, 25.0));
        let below = Rect { x: 0.0, y: 200.0, w: 100.0, h: 50.0 };
        assert_eq!(from.edge_toward(below.center()), (50.0, 50.0));
        assert_eq!(from.edge_toward(from.center()), from.center());

        // Neighbours: a short line from side to side, bent so there and back do not coincide.
        let next = Rect { x: 112.0, y: 0.0, w: 100.0, h: 50.0 };
        let drawn = curve(from, next);
        assert!(drawn.path.starts_with("M100.0,25.0 Q106.0,"), "{}", drawn.path);
        assert!(drawn.arrow.starts_with("112.0,25.0 "), "the tip touches the target: {}", drawn.arrow);
        let back = curve(next, from);
        assert!(drawn.label_near_to.1 > 25.0 && back.label_near_to.1 < 25.0, "there and back bend to different sides");

        // Same row with boxes between: over their tops, from top to top.
        let over = curve(from, to);
        assert!(over.path.starts_with("M50.0,0.0 Q200.0,-24.0 "), "{}", over.path);
        assert!(over.arrow.starts_with("350.0,0.0 "), "{}", over.arrow);
        assert!(over.label_near_from.1 < 0.0 && over.label_near_from.0 < over.label_near_to.0);

        // The label places are on the line, a third in from each end.
        let down = curve(from, below);
        assert!(down.label_near_from.1 < down.label_near_to.1, "{down:?}");
        assert!((50.0..200.0).contains(&down.label_near_from.1) && (50.0..200.0).contains(&down.label_near_to.1));
    }

    #[test]
    fn long_text_is_cut_to_the_box() {
        assert_eq!(fit("Worker", 100.0, 7.0), "Worker");
        assert_eq!(fit("run the tests and fix what fails", 70.0, 7.0), "run the t…");
        assert_eq!(fit("完成报告并发送给团队", 28.0, 7.0), "完成报…");
        assert_eq!(fit("x", 0.0, 7.0), "x");
    }
}
