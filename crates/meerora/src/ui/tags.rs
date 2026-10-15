//! Name tags above the figures (screen 1): the name, the status pill and the
//! activity of every agent, laid out so that no two tags cover each other.
//!
//! Every frame a small layout pass places the tags. A tag sits centred above
//! its figure when nothing is in the way. Two agents that stand together put
//! their tags side by side, each moved half the overlap away from the
//! other, so both still stand over their figures. Otherwise a tag moves up
//! or sideways to the nearest free place, no further than [`MAX_DX`] and
//! [`MAX_DY`], and a thin leader line joins it to its figure. A tag never
//! moves down, where it would cover its own figure. Agents that stand close
//! together, three or more, share one group tag that lists their names with
//! a status dot each. A tag that finds no free place joins its neighbours
//! in a group the same way.
//!
//! The pass keeps the picture calm while agents walk and the camera turns:
//! - a tag keeps last frame's place while it is free and not much worse than
//!   the best one ([`HYSTERESIS`])
//! - tags that stood at their natural place last frame are placed first, so
//!   they keep it and the moved ones move on
//! - a group stays a group until its members stand clearly apart
//! - a tag glides to a new place instead of jumping ([`SMOOTH`]).
//!
//! The hovered agent, and the agent picked in the "Ask an agent to build a
//! device" dialog, win: their tags are placed first and never join a group.

use std::collections::{BTreeSet, HashMap};

use egui::{Align2, Color32, CornerRadius, Id, LayerId, Order, Painter, Pos2, Rect, Stroke, StrokeKind, Ui, Vec2, pos2, vec2};
use meerora_geom::Vec2 as GVec2;
use meerora_protocol::RingState;

use super::{fit, status};
use crate::engine::Core;
use crate::theme::{self, c, ca};

/// Space kept between two tags.
pub const GAP: f32 = 4.0;
/// How far a tag may move sideways and up from its natural place.
pub const MAX_DX: f32 = 150.0;
pub const MAX_DY: f32 = 130.0;
/// A tag keeps last frame's place while that costs at most this much more
/// than the best free place, in points moved.
pub const HYSTERESIS: f32 = 24.0;
/// This many agents standing together share one group tag.
pub const MERGE_AT: usize = 3;
/// Time constant of the glide to a new place, in seconds.
pub const SMOOTH: f32 = 0.09;
/// Upward moves read better than sideways ones, so they cost less per point.
const UP_COST: f32 = 0.75;
/// Members of a group stay together until their tags stand this much
/// further apart than it takes to join one.
const STAY_TOGETHER: f32 = 1.3;
/// A group tag lists at most this many agents, then "and N more".
const GROUP_ROWS: usize = 6;
const GROUP_ROW_H: f32 = 20.0;
/// Room for the "EXTERNAL" badge above the tag of an imported agent (D25).
const BADGE_H: f32 = 22.0;

/// One tag to place: the agent's id, the point above its figure the tag
/// stands on, the size of the tag and its priority. Priority 0 wins (the
/// hovered or selected agent), then lower numbers first.
#[derive(Clone, Debug)]
pub struct Item {
    pub key: String,
    pub anchor: Pos2,
    pub size: Vec2,
    pub priority: u8,
}

/// A placed tag of one agent or a group of agents.
#[derive(Clone, Debug, PartialEq)]
pub struct Placed {
    /// Indices into the items. One for a single tag, more for a group.
    pub members: Vec<usize>,
    /// The memory key: the agent's id or `group:` and the members' ids.
    pub key: String,
    /// Where the layout puts the tag this frame.
    pub rect: Rect,
    /// Centred above the figure (or the group).
    pub natural: Rect,
    /// Where it is drawn, on its glide from the last place to `rect`.
    pub shown: Rect,
}

impl Placed {
    pub fn is_group(&self) -> bool {
        self.members.len() > 1
    }
}

/// What the layout remembers from the last frame.
#[derive(Clone, Debug, Default)]
pub struct Memory {
    tags: HashMap<String, Remembered>,
    time: f64,
}

#[derive(Clone, Copy, Debug, Default)]
struct Remembered {
    /// The place chosen last frame, as an offset from the natural place.
    offset: Vec2,
    /// The offset drawn last frame.
    shown: Vec2,
    /// The agent was a member of a group tag.
    grouped: bool,
}

/// Rects that overlap, touching edges excluded.
fn overlaps(a: Rect, b: Rect) -> bool {
    a.min.x < b.max.x && b.min.x < a.max.x && a.min.y < b.max.y && b.min.y < a.max.y
}

/// The tag centred above `anchor`, its bottom edge on it.
fn natural(anchor: Pos2, size: Vec2) -> Rect {
    Rect::from_min_size(pos2(anchor.x - size.x / 2.0, anchor.y - size.y), size)
}

/// What to place: a single tag or a group tag.
struct Unit {
    members: Vec<usize>,
    key: String,
    natural: Rect,
    /// 0 for winners, 1 for groups, 2 and up for the others by priority.
    class: u8,
    moved_before: bool,
}

/// Places the tags of `items`. `group_size` measures the group tag of some
/// members. `time` is in seconds and drives the glide.
pub fn layout(items: &[Item], group_size: &dyn Fn(&[usize]) -> Vec2, memory: &mut Memory, time: f64) -> Vec<Placed> {
    let winner = |i: usize| items[i].priority == 0;
    let grouped_before = |i: usize| memory.tags.get(&items[i].key).is_some_and(|m| m.grouped);
    // Who stands together: their natural tags overlap by more than 40 % of
    // their width, and in height.
    let together = |i: usize, j: usize| {
        let k = if grouped_before(i) && grouped_before(j) { STAY_TOGETHER } else { 1.0 };
        let (a, b) = (&items[i], &items[j]);
        (a.anchor.x - b.anchor.x).abs() < 0.6 * (a.size.x + b.size.x) / 2.0 * k
            && (a.anchor.y - b.anchor.y).abs() < (a.size.y + b.size.y) / 2.0 * k
    };
    let mut groups: Vec<Vec<usize>> = clusters(items.len(), |i| !winner(i), together).into_iter().filter(|c| c.len() >= MERGE_AT).collect();

    // A tag that finds no free place joins the agents whose tags it would cover.
    let mut placed = Vec::new();
    for _ in 0..4 {
        let failed;
        (placed, failed) = place(items, &groups, group_size, memory);
        let mut changed = false;
        for f in failed {
            let near: Vec<usize> = (0..items.len())
                .filter(|&j| {
                    j == f
                        || (!winner(j)
                            && overlaps(natural(items[j].anchor, items[j].size).expand(GAP), natural(items[f].anchor, items[f].size)))
                })
                .collect();
            if near.len() < 2 || winner(f) {
                continue;
            }
            let mut merged: BTreeSet<usize> = near.into_iter().collect();
            groups.retain(|g| {
                let touches = g.iter().any(|m| merged.contains(m));
                if touches {
                    merged.extend(g.iter().copied());
                }
                !touches
            });
            groups.push(merged.into_iter().collect());
            changed = true;
        }
        if !changed {
            break;
        }
    }

    // Remember the places and glide towards them.
    let dt = ((time - memory.time) as f32).clamp(0.0, 0.5);
    let alpha = 1.0 - (-dt / SMOOTH).exp();
    memory.time = time;
    let mut seen: HashMap<String, Remembered> = HashMap::new();
    for p in &mut placed {
        let offset = p.rect.min - p.natural.min;
        let shown = match memory.tags.get(&p.key) {
            Some(m) => m.shown + (offset - m.shown) * alpha,
            None => offset,
        };
        p.shown = p.natural.translate(shown);
        seen.insert(p.key.clone(), Remembered { offset, shown, grouped: false });
        if p.is_group() {
            for &m in &p.members {
                seen.insert(items[m].key.clone(), Remembered { grouped: true, ..Default::default() });
            }
        }
    }
    memory.tags = seen;
    placed
}

/// Connected groups of the `members` among `n` items under `linked`, each
/// sorted, in the order of their first item.
fn clusters(n: usize, member: impl Fn(usize) -> bool, linked: impl Fn(usize, usize) -> bool) -> Vec<Vec<usize>> {
    let mut root: Vec<usize> = (0..n).collect();
    fn find(root: &mut [usize], i: usize) -> usize {
        let mut i = i;
        while root[i] != i {
            root[i] = root[root[i]];
            i = root[i];
        }
        i
    }
    for i in (0..n).filter(|&i| member(i)) {
        for j in (i + 1..n).filter(|&j| member(j)) {
            if linked(i, j) {
                let (a, b) = (find(&mut root, i), find(&mut root, j));
                root[a.max(b)] = a.min(b);
            }
        }
    }
    let mut out: Vec<Vec<usize>> = Vec::new();
    let mut index: HashMap<usize, usize> = HashMap::new();
    for i in (0..n).filter(|&i| member(i)) {
        let r = find(&mut root, i);
        let k = *index.entry(r).or_insert_with(|| {
            out.push(Vec::new());
            out.len() - 1
        });
        out[k].push(i);
    }
    out
}

/// One pass of placement. Returns the placed tags and the items whose
/// single tag found no free place.
fn place(items: &[Item], groups: &[Vec<usize>], group_size: &dyn Fn(&[usize]) -> Vec2, memory: &Memory) -> (Vec<Placed>, Vec<usize>) {
    let in_group: BTreeSet<usize> = groups.iter().flatten().copied().collect();
    // Two single tags that overlap each other and no third one form a pair.
    let single = |i: usize| items[i].priority != 0 && !in_group.contains(&i);
    let touch = |i: usize, j: usize| {
        let half = GAP / 2.0;
        overlaps(natural(items[i].anchor, items[i].size).expand(half), natural(items[j].anchor, items[j].size).expand(half))
    };
    let pairs: Vec<[usize; 2]> = clusters(items.len(), single, touch).into_iter().filter(|c| c.len() == 2).map(|c| [c[0], c[1]]).collect();
    let moved = |key: &str| memory.tags.get(key).is_some_and(|m| m.offset.length() > 1.0);
    let mut units: Vec<Unit> = Vec::new();
    for g in groups {
        let size = group_size(g);
        let x = g.iter().map(|&m| items[m].anchor.x).sum::<f32>() / g.len() as f32;
        let y = g.iter().map(|&m| items[m].anchor.y).fold(f32::INFINITY, f32::min);
        let key = format!("group:{}", g.iter().map(|&m| items[m].key.as_str()).collect::<Vec<_>>().join("+"));
        units.push(Unit { members: g.clone(), moved_before: moved(&key), key, natural: natural(pos2(x, y), size), class: 1 });
    }
    for (i, item) in items.iter().enumerate().filter(|(i, _)| !in_group.contains(i)) {
        let class = if item.priority == 0 { 0 } else { 2 + item.priority };
        units.push(Unit {
            members: vec![i],
            key: item.key.clone(),
            natural: natural(item.anchor, item.size),
            class,
            moved_before: moved(&item.key),
        });
    }
    // Winners first, then groups, then by priority. Tags that kept their
    // natural place last frame go before the moved ones, nearer figures
    // (lower on screen) before farther ones.
    units.sort_by(|a, b| {
        (a.class, a.moved_before)
            .cmp(&(b.class, b.moved_before))
            .then(b.natural.max.y.total_cmp(&a.natural.max.y))
            .then(a.members[0].cmp(&b.members[0]))
    });

    let mut placed: Vec<Placed> = Vec::new();
    let mut failed = Vec::new();
    for unit in units {
        if placed.iter().any(|p| p.members == unit.members) {
            continue;
        }
        // The first of a pair places both side by side when there is room.
        if let [i] = unit.members[..]
            && let Some(pair) = pairs.iter().find(|p| p.contains(&i))
            && !pair.iter().any(|m| in_group.contains(m) || placed.iter().any(|p| p.members.contains(m)))
            && let Some(both) = side_by_side(items, *pair, memory, &placed)
        {
            for (m, rect) in both {
                let natural = natural(items[m].anchor, items[m].size);
                placed.push(Placed { members: vec![m], key: items[m].key.clone(), rect, natural, shown: rect });
            }
            continue;
        }
        let prev = memory.tags.get(&unit.key).map(|m| unit.natural.translate(m.offset));
        // Without a free place in reach, the tag goes as far up as it takes,
        // and a single one asks to join its neighbours on the next pass.
        let rect = free_place(unit.natural, prev, &placed, 1.0)
            .or_else(|| {
                if unit.members.len() == 1 {
                    failed.push(unit.members[0]);
                }
                free_place(unit.natural, None, &placed, f32::INFINITY)
            })
            .unwrap_or(unit.natural);
        placed.push(Placed { members: unit.members, key: unit.key, rect, natural: unit.natural, shown: rect });
    }
    (placed, failed)
}

/// The tags of two agents that stand together side by side, the left one
/// moved left and the right one moved right by half their overlap each, so
/// both still stand over their figures. `None` when one would leave its
/// figure or cover a placed tag. A pair that stood side by side last frame
/// may come closer to its tags' edges before it gives up, so it does not
/// flip between side by side and stacked.
fn side_by_side(items: &[Item], pair: [usize; 2], memory: &Memory, placed: &[Placed]) -> Option<[(usize, Rect); 2]> {
    let [a, b] = pair;
    let (l, r) = if items[a].anchor.x <= items[b].anchor.x { (a, b) } else { (b, a) };
    let prev = |i: usize| memory.tags.get(&items[i].key).map(|m| m.offset);
    let sticky = matches!((prev(l), prev(r)), (Some(pl), Some(pr)) if pl.x < 0.0 && pr.x > 0.0 && pl.y == 0.0 && pr.y == 0.0);
    let (nl, nr) = (natural(items[l].anchor, items[l].size), natural(items[r].anchor, items[r].size));
    let overlap = (nl.max.x + GAP - nr.min.x).max(0.0);
    // A tag keeps its figure under it, 12 points in from its edge.
    let margin = if sticky { 4.0 } else { 12.0 };
    let reach = |n: Rect| (n.width() / 2.0 - margin).max(0.0);
    let (reach_l, reach_r) = (reach(nl), reach(nr));
    if overlap > reach_l + reach_r {
        return None;
    }
    let shift_l = (overlap / 2.0).min(reach_l).max(overlap - reach_r);
    let (rl, rr) = (nl.translate(vec2(-shift_l, 0.0)), nr.translate(vec2(overlap - shift_l, 0.0)));
    let free = |r: Rect| placed.iter().all(|p| !overlaps(r, p.rect.expand(GAP - 0.01)));
    (free(rl) && free(rr)).then_some([(l, rl), (r, rr)])
}

/// The cheapest place for a tag whose natural place is `natural` that
/// covers none of the `placed` tags, or `None`. Candidates are the natural
/// place, last frame's place `prev` and the places that touch a placed tag
/// on one side, within `reach` times the allowed moves.
fn free_place(natural: Rect, prev: Option<Rect>, placed: &[Placed], reach: f32) -> Option<Rect> {
    let size = natural.size();
    let free = |r: Rect| placed.iter().all(|p| !overlaps(r, p.rect.expand(GAP - 0.01)));
    let allowed = |r: Rect| {
        let d = r.min - natural.min;
        d.x.abs() <= MAX_DX * reach + 0.01 && d.y <= 0.01 && -d.y <= MAX_DY * reach + 0.01
    };
    let cost = |r: Rect| {
        let d = r.min - natural.min;
        d.x.abs() + UP_COST * (-d.y).max(0.0)
    };
    let mut xs = vec![natural.min.x];
    let mut ys = vec![natural.min.y];
    if let Some(p) = prev {
        xs.push(p.min.x);
        ys.push(p.min.y);
    }
    for p in placed {
        xs.extend([p.rect.min.x - GAP - size.x, p.rect.max.x + GAP]);
        ys.extend([p.rect.min.y - GAP - size.y, p.rect.max.y + GAP]);
    }
    let mut best: Option<(Rect, f32)> = None;
    for &x in &xs {
        for &y in &ys {
            let r = Rect::from_min_size(pos2(x, y), size);
            if !allowed(r) || !free(r) {
                continue;
            }
            let c = cost(r);
            if best.is_none_or(|(_, b)| c < b - 0.01) {
                best = Some((r, c));
            }
        }
    }
    let (best, best_cost) = best?;
    match prev {
        Some(p) if allowed(p) && free(p) && cost(p) <= best_cost + HYSTERESIS => Some(p),
        _ => Some(best),
    }
}

/// The texts of one agent's tag.
struct TagText {
    name: String,
    status: String,
    color: [u8; 3],
    sub: String,
}

fn tag_text(painter: &Painter, a: &crate::world::Agent) -> TagText {
    let (status, color) = status(a);
    let station = a.station().map(|s| s.replace('_', " ")).unwrap_or_default();
    let sub = if a.activity.is_empty() { station } else { a.activity.clone() };
    TagText { name: a.name.clone(), status, color, sub: fit(painter, &sub, theme::mono(10.5), 170.0) }
}

/// The size of an agent's tag, without the badge of an imported agent.
fn tag_size(painter: &Painter, t: &TagText) -> Vec2 {
    let name_w = painter.layout_no_wrap(t.name.clone(), theme::ui_bold(14.0), c(theme::TEXT)).size().x;
    let st_w = painter.layout_no_wrap(t.status.clone(), theme::mono(9.5), c(t.color)).size().x;
    let sub_w = painter.layout_no_wrap(t.sub.clone(), theme::mono(10.5), c(theme::TEXT_2)).size().x;
    vec2((name_w + 12.0 + st_w).max(sub_w) + 20.0, if t.sub.is_empty() { 30.0 } else { 46.0 })
}

/// The agent picked in the open "Ask an agent to build a device" dialog
/// counts as selected, its tag wins like the hovered one.
fn priority(a: &crate::world::Agent, selected: Option<&str>) -> u8 {
    if a.hover || selected == Some(a.id.as_str()) {
        return 0;
    }
    match a.ring {
        RingState::NeedsYou => 1,
        RingState::Working => 2,
        RingState::Resting => 3,
    }
}

/// Lays out and draws the tags of every agent in view.
pub fn draw(ui: &mut Ui, core: &Core, camera: &meerora_scene::Camera, viewport: GVec2) {
    let painter = ui.ctx().layer_painter(LayerId::new(Order::Background, Id::new("tags")));
    let selected = super::device_list::selected_agent(ui.ctx());
    let mut agents: Vec<usize> = Vec::new();
    let mut texts: Vec<TagText> = Vec::new();
    let mut items: Vec<Item> = Vec::new();
    for (i, a) in core.world.agents.iter().enumerate() {
        let p = core.world.ring_point(a) + meerora_geom::Vec3::Y * 14.0;
        let Some(s) = camera.project(p, viewport) else { continue };
        let text = tag_text(&painter, a);
        let mut size = tag_size(&painter, &text);
        if super::index::external(core, &a.id).is_some() {
            size.y += BADGE_H;
        }
        items.push(Item { key: a.id.clone(), anchor: pos2(s.x, s.y), size, priority: priority(a, selected.as_deref()) });
        agents.push(i);
        texts.push(text);
    }
    let measure = |members: &[usize]| group_size(&painter, members.iter().map(|&m| &texts[m]).collect());
    let key = Id::new("tag-layout");
    let mut memory: Memory = ui.ctx().data_mut(|d| d.get_temp(key)).unwrap_or_default();
    let placed = layout(&items, &measure, &mut memory, core.world.time);
    ui.ctx().data_mut(|d| d.insert_temp(key, memory));

    // Leader lines first, so they never cross a tag.
    for p in &placed {
        let moved = (p.shown.min - p.natural.min).length() > 6.0;
        if !p.is_group() && !moved {
            continue;
        }
        for &m in &p.members {
            leader(&painter, items[m].anchor, p.shown, p.is_group());
        }
    }
    // Back to front, so nearer tags paint over farther ones while they glide.
    let mut order: Vec<&Placed> = placed.iter().collect();
    order.sort_by(|a, b| a.natural.max.y.total_cmp(&b.natural.max.y));
    for p in order {
        if p.is_group() {
            let members: Vec<(&TagText, bool)> =
                p.members.iter().map(|&m| (&texts[m], core.world.agents[agents[m]].ring == RingState::NeedsYou)).collect();
            draw_group(&painter, p.shown, &members);
        } else {
            let m = p.members[0];
            let a = &core.world.agents[agents[m]];
            let mut rect = p.shown;
            if super::index::external(core, &a.id).is_some() {
                rect.min.y += BADGE_H;
            }
            draw_tag(&painter, &texts[m], rect, a.ring == RingState::NeedsYou, items[m].priority == 0);
            super::index::tag_badge(&painter, core, &a.id, rect);
        }
    }
}

/// A thin line from the point above a figure to its moved tag.
fn leader(painter: &Painter, anchor: Pos2, tag: Rect, group: bool) {
    let end = pos2(anchor.x.clamp(tag.min.x + 10.0, tag.max.x - 10.0), tag.max.y);
    let color = ca(theme::TEXT_2, if group { 110 } else { 150 });
    painter.line_segment([anchor, end], Stroke::new(1.0, color));
    painter.circle_filled(anchor, 2.2, color);
}

fn draw_tag(painter: &Painter, t: &TagText, rect: Rect, waiting: bool, winner: bool) {
    let border = match (waiting, winner) {
        (true, _) => ca(theme::NEEDS_YOU, 200),
        (false, true) => c([0x4A, 0x52, 0x63]),
        (false, false) => c([0x2A, 0x30, 0x3D]),
    };
    painter.rect(rect, CornerRadius::same(8), ca([0x12, 0x16, 0x1E], 236), Stroke::new(1.0, border), StrokeKind::Inside);
    painter.text(rect.min + vec2(10.0, 7.0), Align2::LEFT_TOP, &t.name, theme::ui_bold(14.0), c(theme::TEXT));
    painter.text(pos2(rect.max.x - 10.0, rect.min.y + 10.0), Align2::RIGHT_TOP, &t.status, theme::mono(9.5), c(t.color));
    if !t.sub.is_empty() {
        painter.text(rect.min + vec2(10.0, 27.0), Align2::LEFT_TOP, &t.sub, theme::mono(10.5), c(theme::TEXT_2));
    }
}

/// The size of a group tag listing `members`.
fn group_size(painter: &Painter, members: Vec<&TagText>) -> Vec2 {
    let rows = members.len().min(GROUP_ROWS);
    let more = members.len() > GROUP_ROWS;
    let w = members
        .iter()
        .take(GROUP_ROWS)
        .map(|t| {
            let name = painter.layout_no_wrap(t.name.clone(), theme::ui_semibold(13.0), Color32::WHITE).size().x;
            let st = painter.layout_no_wrap(t.status.clone(), theme::mono(9.5), Color32::WHITE).size().x;
            22.0 + name + 14.0 + st
        })
        .fold(140.0, f32::max);
    vec2(w + 20.0, 12.0 + (rows + usize::from(more)) as f32 * GROUP_ROW_H + 6.0)
}

/// One tag for agents that stand together: a row per agent with a dot in
/// the colour of its status, its name and its status.
fn draw_group(painter: &Painter, rect: Rect, members: &[(&TagText, bool)]) {
    let waiting = members.iter().any(|(_, w)| *w);
    let border = if waiting { ca(theme::NEEDS_YOU, 200) } else { c([0x2A, 0x30, 0x3D]) };
    painter.rect(rect, CornerRadius::same(8), ca([0x12, 0x16, 0x1E], 240), Stroke::new(1.0, border), StrokeKind::Inside);
    let mut y = rect.min.y + 9.0 + GROUP_ROW_H / 2.0;
    for (t, _) in members.iter().take(GROUP_ROWS) {
        painter.circle_filled(pos2(rect.min.x + 15.0, y), 4.0, c(t.color));
        let st_w = painter.layout_no_wrap(t.status.clone(), theme::mono(9.5), c(t.color)).size().x;
        let name = fit(painter, &t.name, theme::ui_semibold(13.0), rect.width() - 52.0 - st_w);
        painter.text(pos2(rect.min.x + 27.0, y), Align2::LEFT_CENTER, name, theme::ui_semibold(13.0), c(theme::TEXT));
        painter.text(pos2(rect.max.x - 10.0, y), Align2::RIGHT_CENTER, &t.status, theme::mono(9.5), c(t.color));
        y += GROUP_ROW_H;
    }
    if members.len() > GROUP_ROWS {
        let more = format!("and {} more", members.len() - GROUP_ROWS);
        painter.text(pos2(rect.min.x + 27.0, y), Align2::LEFT_CENTER, more, theme::mono(10.5), c(theme::TEXT_2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(key: &str, x: f32, y: f32) -> Item {
        Item { key: key.into(), anchor: pos2(x, y), size: vec2(150.0, 46.0), priority: 2 }
    }

    fn group_size(members: &[usize]) -> Vec2 {
        vec2(170.0, 18.0 + members.len() as f32 * GROUP_ROW_H)
    }

    fn run(items: &[Item], memory: &mut Memory, time: f64) -> Vec<Placed> {
        layout(items, &group_size, memory, time)
    }

    fn assert_apart(placed: &[Placed]) {
        for (i, a) in placed.iter().enumerate() {
            for b in &placed[i + 1..] {
                assert!(!overlaps(a.rect.expand(GAP / 2.0 - 0.01), b.rect.expand(GAP / 2.0 - 0.01)), "{} covers {}", a.key, b.key);
            }
        }
    }

    /// Every item is in exactly one placed tag.
    fn assert_complete(placed: &[Placed], n: usize) {
        let mut all: Vec<usize> = placed.iter().flat_map(|p| p.members.clone()).collect();
        all.sort_unstable();
        assert_eq!(all, (0..n).collect::<Vec<_>>());
    }

    #[test]
    fn tags_apart_stay_where_they_are() {
        let items = [item("a", 200.0, 300.0), item("b", 600.0, 300.0)];
        let placed = run(&items, &mut Memory::default(), 0.0);
        assert!(placed.iter().all(|p| p.rect == p.natural && p.shown == p.rect && !p.is_group()));
    }

    #[test]
    fn two_crowded_tags_sit_side_by_side_over_their_figures() {
        // Claude and Ember share the bookshelf (screen 1).
        let items = [item("claude", 500.0, 260.0), item("ember", 540.0, 275.0)];
        let placed = run(&items, &mut Memory::default(), 0.0);
        assert_eq!(placed.len(), 2, "two agents keep their own tags");
        assert_apart(&placed);
        let claude = placed.iter().find(|p| p.key == "claude").unwrap();
        let ember = placed.iter().find(|p| p.key == "ember").unwrap();
        assert!(claude.rect.max.x <= ember.rect.min.x, "Claude, the left one, has the left tag");
        for (p, it) in [(claude, &items[0]), (ember, &items[1])] {
            assert_eq!(p.rect.max.y, p.natural.max.y, "{} stays at its height", p.key);
            assert!((p.rect.min.x + 12.0..=p.rect.max.x - 12.0).contains(&it.anchor.x), "{} stands over its figure", p.key);
        }
        // On one spot there is no room beside, and one tag moves up instead.
        let items = [item("x", 500.0, 300.0), item("y", 501.0, 330.0)];
        let placed = run(&items, &mut Memory::default(), 0.0);
        assert_apart(&placed);
        let moved: Vec<&Placed> = placed.iter().filter(|p| p.rect != p.natural).collect();
        assert_eq!(moved.len(), 1);
        let d = moved[0].rect.min - moved[0].natural.min;
        assert!(d.y < 0.0 && -d.y <= MAX_DY && d.x.abs() <= MAX_DX, "moved within bounds and never down: {d:?}");
    }

    #[test]
    fn a_crowd_shares_one_group_tag_and_the_hovered_agent_keeps_its_own() {
        let items: Vec<Item> = (0..5).map(|k| item(&format!("a{k}"), 500.0 + k as f32 * 18.0, 300.0 + (k % 2) as f32 * 8.0)).collect();
        let placed = run(&items, &mut Memory::default(), 0.0);
        assert_eq!(placed.len(), 1, "{placed:?}");
        assert_eq!(placed[0].members, vec![0, 1, 2, 3, 4]);
        assert!(placed[0].key.starts_with("group:a0+a1"));

        let mut items = items;
        items[2].priority = 0;
        let placed = run(&items, &mut Memory::default(), 0.0);
        assert_complete(&placed, 5);
        assert_apart(&placed);
        let hovered = placed.iter().find(|p| p.members == [2]).expect("the hovered agent has its own tag");
        assert_eq!(hovered.rect, hovered.natural, "and it keeps its place");
        assert!(placed.iter().any(|p| p.members == [0, 1, 3, 4]), "the others share a group");
    }

    #[test]
    fn many_tags_never_overlap() {
        // Agents at several stations, some crowded, some alone, as the camera turns.
        let mut memory = Memory::default();
        for step in 0..60 {
            let t = step as f32 * 0.1;
            let items: Vec<Item> = (0..8)
                .map(|k| {
                    let (cx, cy) = [(400.0, 300.0), (520.0, 330.0), (900.0, 500.0)][k % 3];
                    let x = cx + (k as f32 * 1.7 + t).sin() * 40.0;
                    let y = cy + (k as f32 * 2.3 + t).cos() * 25.0;
                    Item {
                        key: format!("a{k}"),
                        anchor: pos2(x, y),
                        size: vec2(120.0 + 9.0 * k as f32, 30.0 + 16.0 * (k % 2) as f32),
                        priority: 1 + (k % 3) as u8,
                    }
                })
                .collect();
            let placed = run(&items, &mut memory, step as f64 / 30.0);
            assert_complete(&placed, items.len());
            assert_apart(&placed);
        }
    }

    #[test]
    fn tags_hold_their_places_while_agents_walk() {
        // Two agents stand at one station, the camera turns and moves them on
        // screen a little each frame, and they swap which one is lower.
        let mut memory = Memory::default();
        let mut last_shown: HashMap<String, Rect> = HashMap::new();
        for step in 0..120 {
            let t = step as f32 / 30.0;
            let swing = (t * 1.5).sin() * 10.0;
            let items = [item("x", 500.0 + t * 20.0, 300.0 + swing), item("y", 530.0 + t * 20.0, 300.0 - swing)];
            let placed = run(&items, &mut memory, t as f64);
            assert_apart(&placed);
            let rect = |key: &str| placed.iter().find(|p| p.key == key).unwrap().rect;
            assert!(rect("x").max.x <= rect("y").min.x, "step {step}: the tags swapped sides");
            for p in &placed {
                if let Some(before) = last_shown.get(&p.key) {
                    let jump = (p.shown.min - before.min).length();
                    assert!(jump < 12.0, "step {step}: {} jumped {jump:.1} points", p.key);
                }
                last_shown.insert(p.key.clone(), p.shown);
            }
        }
        // A pair at the edge of fitting side by side does not flip to stacked and back.
        let at = |d: f32| [item("x", 500.0, 300.0), item("y", 500.0 + d, 300.0)];
        let side_by_side = |placed: &[Placed]| placed.iter().all(|p| p.rect.max.y == p.natural.max.y && p.rect != p.natural);
        assert!(side_by_side(&run(&at(30.0), &mut Memory::default(), 0.0)), "30 points apart they fit side by side");
        assert!(!side_by_side(&run(&at(26.0), &mut Memory::default(), 0.0)), "26 points apart they do not");
        let mut memory = Memory::default();
        run(&at(30.0), &mut memory, 0.0);
        assert!(side_by_side(&run(&at(26.0), &mut memory, 0.1)), "but a pair side by side stays so");
        // The same input places the same way.
        let items = [item("x", 520.0, 300.0), item("y", 550.0, 300.0)];
        let first = run(&items, &mut memory, 10.0);
        let second = run(&items, &mut memory, 10.5);
        assert_eq!(first.iter().map(|p| p.rect).collect::<Vec<_>>(), second.iter().map(|p| p.rect).collect::<Vec<_>>());
    }

    #[test]
    fn a_group_stays_until_its_members_stand_apart() {
        let mut memory = Memory::default();
        let at = |spread: f32| -> Vec<Item> { (0..3).map(|k| item(&format!("a{k}"), 500.0 + k as f32 * spread, 300.0)).collect() };
        assert!(run(&at(20.0), &mut memory, 0.0)[0].is_group());
        // 100 points apart they would not join a group, but this one stays.
        assert!(run(&at(100.0), &mut memory, 0.1).iter().any(Placed::is_group), "no flicker at the edge");
        let apart = run(&at(200.0), &mut memory, 0.2);
        assert!(apart.iter().all(|p| !p.is_group()), "clearly apart they get their own tags again");
        assert_apart(&apart);
        let fresh = run(&at(100.0), &mut Memory::default(), 0.0);
        assert!(fresh.iter().all(|p| !p.is_group()), "fresh, 100 points apart is not together");
        assert_apart(&fresh);
    }

    #[test]
    fn a_tag_without_room_joins_its_neighbours() {
        // Two agents almost on one spot, and walls of tags above and beside them.
        let mut items = vec![item("x", 500.0, 300.0), item("y", 500.0, 301.0)];
        for k in 0..8 {
            let mut wall = item(&format!("w{k}"), 270.0 + k as f32 * 66.0, 240.0);
            wall.size = vec2(60.0, 200.0);
            wall.priority = 0;
            items.push(wall);
        }
        let placed = run(&items, &mut Memory::default(), 0.0);
        assert_complete(&placed, items.len());
        assert!(placed.iter().any(|p| p.members == [0, 1]), "x and y share a tag: {:?}", placed.iter().map(|p| &p.key).collect::<Vec<_>>());
    }
}
