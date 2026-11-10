//! The Map view: the people and things in the facts known now, joined by the facts that link them.
//! It builds and lays out the graph the same way the web page's map does, then draws it on a braille
//! canvas. Keys hop from node to node instead of a mouse clicking one.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};

use memoryos_core::FactView;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Line as Stroke};
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Wrap};
use ratatui::Frame;
use uuid::Uuid;

use super::app::{clean, fit, humanize, ACCENT, MUTED};

/// How many facts the map loads, as on the web page.
pub const LIMIT: usize = 300;
/// Above this many nodes, a node with fewer than two links shows its name only when it is selected
/// or next to the selected one, as on the web page.
const ALL_LABELS_UP_TO: usize = 70;
/// Columns kept free right of the rightmost node, so its name fits when the map is fitted.
const LABEL_ROOM: f64 = 20.0;
const NAME_WIDTH: usize = 24;
const MAX_ZOOM: f64 = 20.0;
const KIND_COLORS: [Color; 6] = [
    Color::Blue,
    Color::Magenta,
    Color::Green,
    Color::Yellow,
    Color::LightRed,
    Color::LightCyan,
];
const EDGE: Color = Color::Indexed(240);

/// A person or thing named in a fact.
pub struct Node {
    pub id: Uuid,
    pub name: String,
    pub kind: String,
    x: f64,
    y: f64,
    pub degree: usize,
    /// Indexes into [`Graph::facts`] of the facts about it or pointing at it.
    pub facts: Vec<usize>,
}

/// A fact whose object is another node.
struct Edge {
    fact: usize,
    from: usize,
    to: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

pub struct Graph {
    pub facts: Vec<FactView>,
    pub nodes: Vec<Node>,
    edges: Vec<Edge>,
    kinds: Vec<String>,
    limited: bool,
    selected: usize,
    /// 1 shows the whole map.
    zoom: f64,
    /// The world point at the middle of the canvas, or `None` for the middle of the whole map.
    center: Option<(f64, f64)>,
    /// The canvas size at the last draw, so a key can tell whether a node is on screen.
    canvas: Cell<(u16, u16)>,
}

impl Graph {
    pub fn new(facts: Vec<FactView>) -> Self {
        let mut nodes: Vec<Node> = Vec::new();
        let mut index: HashMap<Uuid, usize> = HashMap::new();
        let mut touch = |nodes: &mut Vec<Node>, id: Uuid, name: &str, kind: &str| {
            *index.entry(id).or_insert_with(|| {
                nodes.push(Node {
                    id,
                    name: name.to_owned(),
                    kind: kind.to_owned(),
                    x: 0.0,
                    y: 0.0,
                    degree: 0,
                    facts: Vec::new(),
                });
                nodes.len() - 1
            })
        };
        let mut edges = Vec::new();
        for (at, view) in facts.iter().enumerate() {
            let from = touch(
                &mut nodes,
                view.fact.subject_id,
                &view.subject_name,
                &view.subject_kind,
            );
            nodes[from].facts.push(at);
            if let Some(object) = view.fact.object_id {
                let to = touch(
                    &mut nodes,
                    object,
                    view.object_name.as_deref().unwrap_or("unnamed"),
                    view.object_kind.as_deref().unwrap_or("thing"),
                );
                if to != from {
                    nodes[to].facts.push(at);
                }
                nodes[from].degree += 1;
                nodes[to].degree += 1;
                edges.push(Edge { fact: at, from, to });
            }
        }
        layout(&mut nodes, &edges);
        let mut kinds: Vec<String> = Vec::new();
        for node in &nodes {
            if !kinds.contains(&node.kind) {
                kinds.push(node.kind.clone());
            }
        }
        // The most linked node first, so the map opens on its middle.
        let selected = nodes
            .iter()
            .enumerate()
            .max_by_key(|(at, node)| (node.degree, std::cmp::Reverse(*at)))
            .map_or(0, |(at, _)| at);
        Self {
            limited: facts.len() >= LIMIT,
            facts,
            nodes,
            edges,
            kinds,
            selected,
            zoom: 1.0,
            center: None,
            canvas: Cell::new((0, 0)),
        }
    }

    pub fn selected(&self) -> Option<&Node> {
        self.nodes.get(self.selected)
    }

    /// Selects the node with this id when it is still on the map, after a reload.
    pub fn keep_selection(&mut self, id: Uuid) {
        if let Some(at) = self.nodes.iter().position(|node| node.id == id) {
            self.selected = at;
        }
    }

    /// Moves the selection to the nearest node in `direction`, favoring nodes straight ahead.
    pub fn hop(&mut self, direction: Direction) {
        let Some(from) = self.selected() else {
            return;
        };
        let (dx, dy) = match direction {
            Direction::Left => (-1.0, 0.0),
            Direction::Right => (1.0, 0.0),
            Direction::Up => (0.0, 1.0),
            Direction::Down => (0.0, -1.0),
        };
        let best = self
            .nodes
            .iter()
            .enumerate()
            .filter(|(at, _)| *at != self.selected)
            .filter_map(|(at, node)| {
                let (x, y) = (node.x - from.x, node.y - from.y);
                let along = x * dx + y * dy;
                (along > 0.0).then(|| (at, along + 2.0 * (x * dy - y * dx).abs()))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(at, _)| at);
        if let Some(at) = best {
            self.select(at);
        }
    }

    /// Selects the most linked node whose name contains `text`. False when none does.
    pub fn find(&mut self, text: &str) -> bool {
        let needle = text.trim().to_lowercase();
        if needle.is_empty() {
            return false;
        }
        let found = self
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| node.name.to_lowercase().contains(&needle))
            .max_by_key(|(at, node)| (node.degree, std::cmp::Reverse(*at)))
            .map(|(at, _)| at);
        found.inspect(|at| self.select(*at)).is_some()
    }

    /// Zooms in on the selected node.
    pub fn zoom_in(&mut self) {
        self.zoom = (self.zoom * 1.5).min(MAX_ZOOM);
        self.center = self.selected().map(|node| (node.x, node.y));
    }

    pub fn zoom_out(&mut self) {
        self.zoom /= 1.5;
        if self.zoom <= 1.0 {
            self.fit();
        }
    }

    pub fn fit(&mut self) {
        self.zoom = 1.0;
        self.center = None;
    }

    fn select(&mut self, at: usize) {
        self.selected = at;
        let (width, height) = self.canvas.get();
        let node = &self.nodes[at];
        if width == 0 || height == 0 {
            return;
        }
        let (x, y) = self.bounds(width, height);
        let inside = node.x >= x[0] && node.x <= x[1] && node.y >= y[0] && node.y <= y[1];
        if !inside {
            self.center = Some((node.x, node.y));
        }
    }

    /// The world rectangle a canvas of `width` by `height` cells shows. A cell is about twice as
    /// tall as it is wide, so it covers twice the world height, which keeps the map's shape.
    fn bounds(&self, width: u16, height: u16) -> ([f64; 2], [f64; 2]) {
        let (width, height) = (f64::from(width), f64::from(height));
        let fold = |(low, high): (f64, f64), value: f64| (low.min(value), high.max(value));
        let empty = (f64::INFINITY, f64::NEG_INFINITY);
        let (min_x, max_x) = self.nodes.iter().map(|node| node.x).fold(empty, fold);
        let (min_y, max_y) = self.nodes.iter().map(|node| node.y).fold(empty, fold);
        let (min_x, max_x, min_y, max_y) = if self.nodes.is_empty() {
            (0.0, 0.0, 0.0, 0.0)
        } else {
            (min_x, max_x, min_y, max_y)
        };
        let whole = ((max_x - min_x).max(1.0) / (width - LABEL_ROOM - 2.0).max(4.0))
            .max((max_y - min_y).max(1.0) / ((height - 1.0) * 2.0).max(2.0));
        let scale = whole / self.zoom;
        let (cx, cy) = self.center.unwrap_or((
            (min_x + max_x) / 2.0 + LABEL_ROOM / 2.0 * whole,
            (min_y + max_y) / 2.0,
        ));
        (
            [cx - width / 2.0 * scale, cx + width / 2.0 * scale],
            [cy - height * scale, cy + height * scale],
        )
    }

    /// The selected node and the nodes one link away from it.
    fn near(&self) -> HashSet<usize> {
        let mut near = HashSet::from([self.selected]);
        for edge in &self.edges {
            if edge.from == self.selected {
                near.insert(edge.to);
            }
            if edge.to == self.selected {
                near.insert(edge.from);
            }
        }
        near
    }

    fn color(&self, node: &Node) -> Color {
        let at = self.kinds.iter().position(|kind| *kind == node.kind);
        KIND_COLORS[at.unwrap_or(0) % KIND_COLORS.len()]
    }

    /// The map, with the selected node's card beside it when `side` is true.
    pub fn draw(&self, frame: &mut Frame, area: Rect, scope: Line<'static>, side: bool) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(MUTED))
            .title(Span::styled(
                format!(" Map · {} people and things ", self.nodes.len()),
                Style::new().bold(),
            ))
            .title_top(scope)
            .title_bottom(self.legend());
        if self.nodes.is_empty() {
            frame.render_widget(
                Paragraph::new(
                    "\n  Nothing to draw yet. The map shows the people and things in the facts known now.",
                )
                .fg(MUTED)
                .block(block),
                area,
            );
            return;
        }
        let (map, card) = if side {
            let [map, card] =
                Layout::horizontal([Constraint::Percentage(66), Constraint::Percentage(34)])
                    .areas(area);
            (map, Some(card))
        } else {
            (area, None)
        };
        let inner = block.inner(map);
        self.canvas.set((inner.width, inner.height));
        let (x_bounds, y_bounds) = self.bounds(inner.width, inner.height);
        let near = self.near();
        let canvas = Canvas::default()
            .block(block)
            .marker(Marker::Braille)
            .x_bounds(x_bounds)
            .y_bounds(y_bounds)
            .paint(|ctx| {
                let lit = |edge: &Edge| edge.from == self.selected || edge.to == self.selected;
                let stroke = |edge: &Edge, color| Stroke {
                    x1: self.nodes[edge.from].x,
                    y1: self.nodes[edge.from].y,
                    x2: self.nodes[edge.to].x,
                    y2: self.nodes[edge.to].y,
                    color,
                };
                for edge in self.edges.iter().filter(|edge| !lit(edge)) {
                    ctx.draw(&stroke(edge, EDGE));
                }
                ctx.layer();
                for edge in self.edges.iter().filter(|edge| lit(edge)) {
                    ctx.draw(&stroke(edge, ACCENT));
                }
                // Labels print in order and a later one overwrites an earlier one, so the links'
                // names go first, then the far nodes, the near ones, and the selected node last.
                for edge in self.edges.iter().filter(|edge| lit(edge)) {
                    let (from, to) = (&self.nodes[edge.from], &self.nodes[edge.to]);
                    let predicate = humanize(&self.facts[edge.fact].fact.predicate);
                    ctx.print(
                        (from.x + to.x) / 2.0,
                        (from.y + to.y) / 2.0,
                        Span::styled(fit(&predicate, 18), Style::new().fg(MUTED).italic()),
                    );
                }
                let mut order: Vec<usize> = (0..self.nodes.len()).collect();
                order.sort_by_key(|at| (*at == self.selected, near.contains(at)));
                let all_labels = self.nodes.len() <= ALL_LABELS_UP_TO;
                for at in order {
                    let node = &self.nodes[at];
                    let (dot, name_style) = if at == self.selected {
                        (
                            Span::styled("◉", Style::new().fg(ACCENT).bold()),
                            Style::new().fg(ACCENT).bold(),
                        )
                    } else if near.contains(&at) {
                        (
                            Span::styled("●", Style::new().fg(self.color(node))),
                            Style::new(),
                        )
                    } else {
                        (
                            Span::styled("●", Style::new().fg(self.color(node))),
                            Style::new().fg(MUTED),
                        )
                    };
                    let mut spans = vec![dot];
                    if all_labels || node.degree >= 2 || near.contains(&at) {
                        spans.push(Span::styled(
                            format!(" {}", fit(&clean(&node.name), NAME_WIDTH)),
                            name_style,
                        ));
                    }
                    ctx.print(node.x, node.y, Line::from(spans));
                }
            });
        frame.render_widget(canvas, map);
        if let Some(card) = card {
            self.draw_card(frame, card);
        }
    }

    fn legend(&self) -> Line<'static> {
        let mut spans = vec![Span::raw(" ")];
        for (at, kind) in self.kinds.iter().take(KIND_COLORS.len()).enumerate() {
            spans.push(Span::styled("● ", Style::new().fg(KIND_COLORS[at])));
            spans.push(Span::styled(
                format!("{}  ", humanize(kind)),
                Style::new().fg(MUTED),
            ));
        }
        if self.limited {
            spans.push(Span::styled(
                format!("newest {LIMIT} facts "),
                Style::new().fg(MUTED),
            ));
        }
        Line::from(spans)
    }

    fn draw_card(&self, frame: &mut Frame, area: Rect) {
        let Some(node) = self.selected() else {
            return;
        };
        let field = |name: &str, value: String| {
            Line::from(vec![
                Span::styled(format!("{name:<8}"), Style::new().fg(MUTED)),
                Span::raw(value),
            ])
        };
        let mut lines = vec![
            Line::from(clean(&node.name)).fg(ACCENT).bold(),
            Line::default(),
            field("Kind", humanize(&node.kind)),
            field("Links", node.degree.to_string()),
            field("Facts", node.facts.len().to_string()),
            Line::default(),
        ];
        for at in &node.facts {
            lines.push(Line::from(format!(
                "• {}",
                clean(&self.facts[*at].fact.statement)
            )));
        }
        lines.push(Line::default());
        lines.push(Line::from("Enter lists them to open one").fg(MUTED));
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::new().fg(MUTED))
                    .padding(Padding::horizontal(1))
                    .title(" Selected "),
            ),
            area,
        );
    }
}

/// The web page's force layout, run to the end before drawing so the map appears settled. Same
/// constants, so a scope has the same shape in both.
fn layout(nodes: &mut [Node], edges: &[Edge]) {
    let golden = std::f64::consts::PI * (3.0 - 5f64.sqrt());
    for (at, node) in nodes.iter_mut().enumerate() {
        let radius = 26.0 * ((at + 1) as f64).sqrt();
        node.x = radius * (at as f64 * golden).cos();
        node.y = radius * (at as f64 * golden).sin();
    }
    let mut vx = vec![0.0; nodes.len()];
    let mut vy = vec![0.0; nodes.len()];
    let rounds = if nodes.len() > 200 { 140 } else { 280 };
    for round in 0..rounds {
        let heat = 1.0 - f64::from(round) / f64::from(rounds);
        for a in 0..nodes.len() {
            for b in a + 1..nodes.len() {
                let dx = nodes[a].x - nodes[b].x;
                let dy = nodes[a].y - nodes[b].y;
                let square = (dx * dx + dy * dy).max(64.0);
                let push = 16000.0 / square / square.sqrt();
                vx[a] += dx * push;
                vy[a] += dy * push;
                vx[b] -= dx * push;
                vy[b] -= dy * push;
            }
        }
        for edge in edges {
            let dx = nodes[edge.to].x - nodes[edge.from].x;
            let dy = nodes[edge.to].y - nodes[edge.from].y;
            let distance = dx.hypot(dy).max(1.0);
            let pull = (distance - 170.0) * 0.03;
            vx[edge.from] += dx / distance * pull;
            vy[edge.from] += dy / distance * pull;
            vx[edge.to] -= dx / distance * pull;
            vy[edge.to] -= dy / distance * pull;
        }
        for (at, node) in nodes.iter_mut().enumerate() {
            vx[at] -= node.x * 0.012;
            vy[at] -= node.y * 0.012;
            node.x += vx[at].clamp(-30.0, 30.0) * heat;
            node.y += vy[at].clamp(-30.0, 30.0) * heat;
            vx[at] *= 0.55;
            vy[at] *= 0.55;
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use memoryos_core::{Fact, FactStatus, Scope, Trust};

    /// `subject` `predicate` `object`, with ids that stay the same for the same name.
    fn link(subject: &str, predicate: &str, object: Option<&str>) -> FactView {
        let id = |name: &str| {
            Uuid::from_u128(name.bytes().fold(1u128, |hash, byte| {
                hash.wrapping_mul(31).wrapping_add(u128::from(byte))
            }))
        };
        let at = DateTime::parse_from_rfc3339("2026-11-01T00:00:00Z")
            .expect("date")
            .with_timezone(&Utc);
        FactView {
            fact: Fact {
                id: Uuid::now_v7(),
                scope: "agent:coding-agent".parse::<Scope>().expect("scope"),
                subject_id: id(subject),
                predicate: predicate.into(),
                object_id: object.map(id),
                statement: format!("{subject} {predicate} {}", object.unwrap_or("something")),
                trust: Trust::Agent,
                status: FactStatus::Active,
                valid_from: at,
                valid_to: None,
                expires_at: None,
                recorded_at: at,
                invalidated_at: None,
                superseded_by: None,
                pending_supersede_of: None,
                source_episode_ids: vec![],
            },
            subject_name: subject.into(),
            subject_kind: "person".into(),
            object_name: object.map(Into::into),
            object_kind: object.map(|_| "place".into()),
        }
    }

    pub(in crate::tui) fn sample() -> Vec<FactView> {
        vec![
            link("Ada", "lives_in", Some("Bandung")),
            link("Ada", "works_on", Some("MemoryOS")),
            link("Ada", "likes", None),
            link("MemoryOS", "runs_on", Some("VPS")),
        ]
    }

    #[test]
    fn people_and_things_become_nodes_and_linking_facts_become_edges() {
        let graph = Graph::new(sample());
        let names: Vec<&str> = graph.nodes.iter().map(|node| node.name.as_str()).collect();
        assert_eq!(names, ["Ada", "Bandung", "MemoryOS", "VPS"]);
        assert_eq!(graph.edges.len(), 3);
        // A fact with no object stays with its subject and draws no line.
        assert_eq!(graph.nodes[0].facts.len(), 3);
        assert_eq!(graph.kinds, ["person", "place"]);
        // The map opens on the most linked node.
        assert_eq!(graph.selected().map(|node| node.name.as_str()), Some("Ada"));
    }

    #[test]
    fn hop_moves_to_the_nearest_node_in_that_direction() {
        let mut graph = Graph::new(sample());
        for (node, (x, y)) in
            graph
                .nodes
                .iter_mut()
                .zip([(0.0, 0.0), (100.0, 10.0), (0.0, 100.0), (300.0, 0.0)])
        {
            node.x = x;
            node.y = y;
        }
        graph.selected = 0;
        graph.hop(Direction::Right);
        assert_eq!(graph.selected().map(|n| n.name.as_str()), Some("Bandung"));
        graph.hop(Direction::Right);
        assert_eq!(graph.selected().map(|n| n.name.as_str()), Some("VPS"));
        // Nothing further right: the selection stays.
        graph.hop(Direction::Right);
        assert_eq!(graph.selected().map(|n| n.name.as_str()), Some("VPS"));
        graph.selected = 0;
        graph.hop(Direction::Up);
        assert_eq!(graph.selected().map(|n| n.name.as_str()), Some("MemoryOS"));
    }

    #[test]
    fn find_selects_by_name_and_reload_keeps_the_selection() {
        let mut graph = Graph::new(sample());
        assert!(graph.find("vp"));
        let id = graph.selected().expect("node").id;
        assert!(!graph.find("nobody"));
        let mut again = Graph::new(sample());
        again.keep_selection(id);
        assert_eq!(again.selected().map(|n| n.name.as_str()), Some("VPS"));
    }

    #[test]
    fn layout_spreads_nodes_apart() {
        let graph = Graph::new(sample());
        for (at, a) in graph.nodes.iter().enumerate() {
            assert!(a.x.is_finite() && a.y.is_finite());
            for b in &graph.nodes[at + 1..] {
                assert!(
                    (a.x - b.x).hypot(a.y - b.y) > 20.0,
                    "{} and {} overlap",
                    a.name,
                    b.name
                );
            }
        }
    }
}
