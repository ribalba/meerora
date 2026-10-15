//! The tiled layout of the agent view (D11). Tiles stand in columns, and
//! each column stacks its tiles. Dragging the gap between two tiles resizes
//! them, and dragging a tile by its head onto another moves it next to that
//! tile or swaps the two. It stays tiled: nothing floats or overlaps. Each
//! agent keeps its own layout in `<data dir>/layouts.json`.
//!
//! A tile can hold several instances of an app as tabs of its head, for
//! example several shells in one terminal tile. Only one of them shows, the
//! others wait in [`Tile::more`]. Every one of them is still a tab of the
//! agent view.

use std::collections::HashMap;
use std::path::Path;

use egui::{Align2, CornerRadius, Id, LayerId, Order, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use serde::{Deserialize, Serialize};

use super::AppTab;
use crate::engine::Core;
use crate::theme::{self, c, ca};

/// Space between tiles in points.
pub const GAP: f32 = 8.0;
/// Smallest tile in points.
const MIN_W: f32 = 220.0;
const MIN_H: f32 = 120.0;
/// Width of the chat column in a new layout, in points.
const CHAT_W: f32 = 330.0;
/// A drag handle reaches this far into the tiles beside the gap.
const HANDLE_REACH: f32 = 3.0;
const FILE: &str = "layouts.json";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TileLayout {
    pub columns: Vec<Column>,
    /// The windows of `agent.toml` this layout started from. When they
    /// change, the layout starts over. The open windows may differ, because
    /// you open and close apps.
    #[serde(default)]
    pub config: Vec<AppTab>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Column {
    /// Share of the width. The shares of all columns add up to 1.
    pub width: f32,
    pub tiles: Vec<Tile>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Tile {
    /// The app instance the tile shows.
    pub app: AppTab,
    /// Share of the column height. The shares of a column add up to 1.
    pub height: f32,
    /// Further instances that share this tile as tabs of its head, shown
    /// one at a time (shell tabs). Layouts without it load as empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub more: Vec<AppTab>,
}

impl Tile {
    fn new(app: AppTab, height: f32) -> Self {
        Tile { app, height, more: Vec::new() }
    }

    fn holds(&self, app: AppTab) -> bool {
        self.app == app || self.more.contains(&app)
    }
}

/// Where a dragged tile lands on another one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Zone {
    Swap,
    Above,
    Below,
    /// A new column left or right of the target's column.
    Left,
    Right,
}

/// A drag handle: between columns `i` and `i + 1`, or between tiles `i` and
/// `i + 1` of a column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Handle {
    Column(usize),
    Row(usize, usize),
}

impl TileLayout {
    /// The first arrangement for these windows: the chat in a column on the
    /// right, one other window filling the rest, two side by side, three with
    /// the first on the left and the others stacked.
    pub fn default_for(apps: &[AppTab], width: f32) -> Self {
        let column = |tiles: &[(AppTab, f32)], width: f32| Column {
            width,
            tiles: tiles.iter().map(|&(app, height)| Tile::new(app, height)).collect(),
        };
        let others: Vec<AppTab> = apps.iter().copied().filter(|t| *t != AppTab::Chat).collect();
        if others.is_empty() {
            return Self { columns: vec![column(&[(AppTab::Chat, 1.0)], 1.0)], config: apps.to_vec() };
        }
        let chat = (CHAT_W / width.max(1.0)).clamp(0.2, 0.5);
        let rest = 1.0 - chat;
        let mut columns = match others.as_slice() {
            [one] => vec![column(&[(*one, 1.0)], rest)],
            [a, b] => vec![column(&[(*a, 1.0)], rest / 2.0), column(&[(*b, 1.0)], rest / 2.0)],
            [a, rest_of @ ..] => {
                let stacked: Vec<(AppTab, f32)> = rest_of.iter().enumerate().map(|(i, t)| (*t, if i == 0 { 0.42 } else { 0.58 })).collect();
                vec![column(&[(*a, 1.0)], rest / 2.0), column(&stacked, rest / 2.0)]
            }
            [] => unreachable!("others is not empty"),
        };
        columns.push(column(&[(AppTab::Chat, 1.0)], chat));
        let mut layout = Self { columns, config: apps.to_vec() };
        layout.normalize();
        layout
    }

    /// The windows in this layout, also those that wait as tabs of a tile.
    pub fn apps(&self) -> Vec<AppTab> {
        self.columns.iter().flat_map(|c| c.tiles.iter().flat_map(|t| std::iter::once(t.app).chain(t.more.iter().copied()))).collect()
    }

    /// The instances that share the tile of `app`, `app` included, the shown one first.
    pub fn group(&self, app: AppTab) -> Vec<AppTab> {
        self.find(app).map_or_else(Vec::new, |(k, i)| {
            let tile = &self.columns[k].tiles[i];
            std::iter::once(tile.app).chain(tile.more.iter().copied()).collect()
        })
    }

    /// The instance the tile of `app` shows.
    pub fn shown(&self, app: AppTab) -> Option<AppTab> {
        self.find(app).map(|(k, i)| self.columns[k].tiles[i].app)
    }

    /// Shows `app` in its tile when it waits there as a tab. Returns true
    /// when that changed the layout.
    pub fn reveal(&mut self, app: AppTab) -> bool {
        let Some((k, i)) = self.find(app) else { return false };
        let tile = &mut self.columns[k].tiles[i];
        if tile.app == app {
            return false;
        }
        tile.more.retain(|t| *t != app);
        tile.more.push(tile.app);
        tile.app = app;
        true
    }

    /// Shows `app` as a tab of the tile of `onto`. An open `app` leaves its
    /// own tile for it, an app that is not open opens there. Returns true
    /// when that changed the layout.
    pub fn join(&mut self, app: AppTab, onto: AppTab) -> bool {
        if app == AppTab::Chat || onto == AppTab::Chat {
            return false;
        }
        let Some((k, i)) = self.find(onto) else { return false };
        if self.columns[k].tiles[i].holds(app) {
            return self.reveal(app);
        }
        self.close(app);
        let Some((k, i)) = self.find(onto) else { return false };
        let tile = &mut self.columns[k].tiles[i];
        tile.more.push(tile.app);
        tile.app = app;
        true
    }

    /// Takes `app` out of a tile it shares and opens it in a tile of its own
    /// beside that one. Returns true when it was a shared tab.
    pub fn split(&mut self, app: AppTab, content: Rect) -> bool {
        let Some((k, i)) = self.find(app) else { return false };
        if self.columns[k].tiles[i].more.is_empty() {
            return false;
        }
        self.close(app);
        // The tile stays, because others still share it.
        let near = self.columns[k].tiles[i].app;
        self.open_beside(app, near, content);
        true
    }

    /// Starts over from [`Self::default_for`] when the agent's windows changed
    /// or the layout is broken, for example a hand-edited `layouts.json`.
    pub fn sync(&mut self, apps: &[AppTab], width: f32) {
        let mine = self.apps();
        let unique = mine.iter().enumerate().all(|(i, a)| !mine[i + 1..].contains(a));
        let sane = self.columns.iter().all(|c| c.width.is_finite() && c.width > 0.0 && !c.tiles.is_empty())
            && self.columns.iter().flat_map(|c| &c.tiles).all(|t| t.height.is_finite() && t.height > 0.0)
            && unique
            && mine.contains(&AppTab::Chat);
        if self.config != apps || !sane {
            *self = Self::default_for(apps, width);
        }
    }

    /// Opens `app` in the largest tile's place: beside it when that tile has
    /// its column alone and is wide, else below it. An open app stays.
    pub fn open(&mut self, app: AppTab, content: Rect) {
        let rects = self.rects(content);
        let Some((target, _)) = rects.iter().max_by(|a, b| a.1.area().total_cmp(&b.1.area())).copied() else { return };
        self.open_beside(app, target, content);
    }

    /// Opens `app` in the place of the tile of `near`, the same way as
    /// [`Self::open`], for example a second terminal next to the first. When
    /// `near` has no tile it opens like [`Self::open`]. An open app stays.
    pub fn open_beside(&mut self, app: AppTab, near: AppTab, content: Rect) {
        if self.find(app).is_some() {
            return;
        }
        let Some(rect) = self.rects(content).into_iter().find(|(t, _)| *t == near).map(|(_, r)| r) else {
            // `open` always passes a tile that exists, so this goes one level deep at most.
            self.open(app, content);
            return;
        };
        let alone = self.find(near).is_some_and(|(k, _)| self.columns[k].tiles.len() == 1);
        let zone = if alone && rect.width() > rect.height() * 1.2 { Zone::Right } else { Zone::Below };
        self.insert(app, near, zone);
        self.normalize();
    }

    /// Closes `app`. Its space goes to its neighbours. In a tile it shares,
    /// another of its tabs shows instead. The chat stays open (D11), so
    /// closing it does nothing. Returns true when it closed.
    pub fn close(&mut self, app: AppTab) -> bool {
        if app == AppTab::Chat {
            return false;
        }
        let Some((k, i)) = self.find(app) else { return false };
        let tile = &mut self.columns[k].tiles[i];
        if tile.app != app {
            tile.more.retain(|t| *t != app);
        } else if !tile.more.is_empty() {
            tile.app = tile.more.remove(0);
        } else {
            self.take(app);
            self.normalize();
        }
        true
    }

    /// Where each tile goes in `content`.
    pub fn rects(&self, content: Rect) -> Vec<(AppTab, Rect)> {
        let mut out = Vec::new();
        let xs = spans(self.columns.iter().map(|c| c.width), content.min.x, content.max.x);
        for (column, (x0, x1)) in self.columns.iter().zip(xs) {
            let ys = spans(column.tiles.iter().map(|t| t.height), content.min.y, content.max.y);
            for (tile, (y0, y1)) in column.tiles.iter().zip(ys) {
                out.push((tile.app, Rect::from_min_max(pos2(x0, y0), pos2(x1, y1))));
            }
        }
        out
    }

    /// The drag handles in `content`, each over its gap.
    pub fn handles(&self, content: Rect) -> Vec<(Handle, Rect, bool)> {
        let mut out = Vec::new();
        let xs = spans(self.columns.iter().map(|c| c.width), content.min.x, content.max.x);
        for (i, pair) in xs.windows(2).enumerate() {
            let gap = Rect::from_min_max(pos2(pair[0].1, content.min.y), pos2(pair[1].0, content.max.y));
            out.push((Handle::Column(i), gap.expand2(vec2(HANDLE_REACH, 0.0)), true));
        }
        for (k, (column, (x0, x1))) in self.columns.iter().zip(&xs).enumerate() {
            let ys = spans(column.tiles.iter().map(|t| t.height), content.min.y, content.max.y);
            for (i, pair) in ys.windows(2).enumerate() {
                let gap = Rect::from_min_max(pos2(*x0, pair[0].1), pos2(*x1, pair[1].0));
                out.push((Handle::Row(k, i), gap.expand2(vec2(0.0, HANDLE_REACH)), false));
            }
        }
        out
    }

    /// Moves a handle by `delta` points, keeping every tile at least
    /// [`MIN_W`] wide and [`MIN_H`] high where there is room for that.
    pub fn drag(&mut self, handle: Handle, delta: f32, content: Rect) {
        match handle {
            Handle::Column(i) if i + 1 < self.columns.len() => {
                let avail = (content.width() - GAP * (self.columns.len() as f32 - 1.0)).max(1.0);
                let mut shares: Vec<f32> = self.columns.iter().map(|c| c.width).collect();
                resize(&mut shares, i, delta / avail, MIN_W / avail);
                for (column, share) in self.columns.iter_mut().zip(shares) {
                    column.width = share;
                }
            }
            Handle::Row(k, i) if self.columns.get(k).is_some_and(|c| i + 1 < c.tiles.len()) => {
                let column = &mut self.columns[k];
                let avail = (content.height() - GAP * (column.tiles.len() as f32 - 1.0)).max(1.0);
                let mut shares: Vec<f32> = column.tiles.iter().map(|t| t.height).collect();
                resize(&mut shares, i, delta / avail, MIN_H / avail);
                for (tile, share) in column.tiles.iter_mut().zip(shares) {
                    tile.height = share;
                }
            }
            _ => {}
        }
    }

    /// Moves the tile of `app` onto the tile of `target`, with every tab it
    /// holds.
    pub fn move_tile(&mut self, app: AppTab, target: AppTab, zone: Zone) {
        let (Some((ac, ai)), Some((tc, ti))) = (self.find(app), self.find(target)) else { return };
        if (ac, ai) == (tc, ti) {
            return;
        }
        if zone == Zone::Swap {
            let a = (self.columns[ac].tiles[ai].app, std::mem::take(&mut self.columns[ac].tiles[ai].more));
            let t = (self.columns[tc].tiles[ti].app, std::mem::take(&mut self.columns[tc].tiles[ti].more));
            (self.columns[ac].tiles[ai].app, self.columns[ac].tiles[ai].more) = t;
            (self.columns[tc].tiles[ti].app, self.columns[tc].tiles[ti].more) = a;
            return;
        }
        let Some(tile) = self.take(app) else { return };
        self.insert_tile(tile, target, zone);
        self.normalize();
    }

    /// Takes the tile of `app` out, with every tab it holds. An emptied
    /// column hands its width to a neighbour, the tiles of a column share
    /// the height of a removed one.
    fn take(&mut self, app: AppTab) -> Option<Tile> {
        let (k, i) = self.find(app)?;
        let tile = self.columns[k].tiles.remove(i);
        if self.columns[k].tiles.is_empty() && self.columns.len() > 1 {
            let width = self.columns.remove(k).width;
            let n = k.saturating_sub(1).min(self.columns.len() - 1);
            self.columns[n].width += width;
        }
        Some(tile)
    }

    /// Puts a new tile for `app` into `zone` of the tile of `target`.
    fn insert(&mut self, app: AppTab, target: AppTab, zone: Zone) {
        self.insert_tile(Tile::new(app, 1.0), target, zone);
    }

    /// Puts `tile` into `zone` of the tile of `target`. Its height is set here.
    fn insert_tile(&mut self, mut tile: Tile, target: AppTab, zone: Zone) {
        let Some((tc, ti)) = self.find(target) else { return };
        match zone {
            Zone::Above | Zone::Below => {
                let column = &mut self.columns[tc];
                let half = column.tiles[ti].height / 2.0;
                column.tiles[ti].height = half;
                let at = if zone == Zone::Above { ti } else { ti + 1 };
                tile.height = half;
                column.tiles.insert(at, tile);
            }
            Zone::Left | Zone::Right => {
                let half = self.columns[tc].width / 2.0;
                self.columns[tc].width = half;
                let at = if zone == Zone::Left { tc } else { tc + 1 };
                tile.height = 1.0;
                self.columns.insert(at, Column { width: half, tiles: vec![tile] });
            }
            Zone::Swap => {}
        }
    }

    /// The column and row of the tile that shows or holds `app`.
    fn find(&self, app: AppTab) -> Option<(usize, usize)> {
        self.columns.iter().enumerate().find_map(|(k, c)| c.tiles.iter().position(|t| t.holds(app)).map(|i| (k, i)))
    }

    /// Scales the shares so each set adds up to 1.
    fn normalize(&mut self) {
        let total: f32 = self.columns.iter().map(|c| c.width).sum();
        for column in &mut self.columns {
            column.width /= total.max(1e-6);
            let total: f32 = column.tiles.iter().map(|t| t.height).sum();
            for tile in &mut column.tiles {
                tile.height /= total.max(1e-6);
            }
        }
    }
}

/// Cuts `start..end` into one span per share with [`GAP`] between them. The
/// last span ends exactly at `end`.
fn spans(shares: impl Iterator<Item = f32>, start: f32, end: f32) -> Vec<(f32, f32)> {
    let shares: Vec<f32> = shares.collect();
    let n = shares.len();
    let avail = (end - start - GAP * (n as f32 - 1.0)).max(0.0);
    let total = shares.iter().sum::<f32>().max(1e-6);
    let mut out = Vec::with_capacity(n);
    let mut at = start;
    for (i, share) in shares.iter().enumerate() {
        let size = if i + 1 == n { (end - at).max(0.0) } else { (avail * share / total).round() };
        out.push((at, at + size));
        at += size + GAP;
    }
    out
}

/// Moves the border after entry `i` by `delta`, all in shares, keeping both
/// sides at least `min` or half of the pair when the pair is smaller.
fn resize(shares: &mut [f32], i: usize, delta: f32, min: f32) {
    let pair = shares[i] + shares[i + 1];
    let min = min.min(pair / 2.0);
    let first = (shares[i] + delta).clamp(min, pair - min);
    shares[i] = first;
    shares[i + 1] = pair - first;
}

/// The zone of `rect` under `pos`: near an edge it is that side, in the
/// middle a swap.
pub fn zone_at(rect: Rect, pos: Pos2) -> Zone {
    let u = (pos.x - rect.min.x) / rect.width().max(1.0);
    let v = (pos.y - rect.min.y) / rect.height().max(1.0);
    let edges = [(u, Zone::Left), (1.0 - u, Zone::Right), (v, Zone::Above), (1.0 - v, Zone::Below)];
    let (distance, zone) = edges.into_iter().min_by(|a, b| a.0.total_cmp(&b.0)).expect("four edges");
    if distance < 0.25 { zone } else { Zone::Swap }
}

/// The part of `rect` a drop into `zone` takes, for the highlight.
fn zone_rect(rect: Rect, zone: Zone) -> Rect {
    let (w, h) = (rect.width() / 2.0, rect.height() / 2.0);
    match zone {
        Zone::Swap => rect,
        Zone::Left => Rect::from_min_size(rect.min, vec2(w, rect.height())),
        Zone::Right => Rect::from_min_max(pos2(rect.max.x - w, rect.min.y), rect.max),
        Zone::Above => Rect::from_min_size(rect.min, vec2(rect.width(), h)),
        Zone::Below => Rect::from_min_max(pos2(rect.min.x, rect.max.y - h), rect.max),
    }
}

/// The layout of agent `id` for its windows, created when missing.
/// `apps` are the windows of the agent's `agent.toml`, the start of its
/// layout. [`TileLayout::apps`] are the ones open now.
pub fn layout_for<'a>(core: &'a mut Core, id: &str, apps: &[AppTab], content: Rect) -> &'a mut TileLayout {
    let layouts = core.ui.tiles.get_or_insert_with(|| load(&core.assets.data_dir));
    let layout = layouts.entry(id.to_string()).or_insert_with(|| TileLayout::default_for(apps, content.width()));
    layout.sync(apps, content.width());
    layout
}

/// The drag handles between the tiles, and the drop target while a tile is
/// dragged by its head (see `draw_tile` in ui/agent.rs).
pub fn interact(ui: &mut Ui, core: &mut Core, id: &str, apps: &[AppTab], content: Rect, rects: &[(AppTab, Rect)]) {
    let handles = layout_for(core, id, apps, content).handles(content);
    let mut changed = false;
    for (handle, rect, vertical) in handles {
        let response = egui::Area::new(Id::new(("tile-handle", id, handle)))
            .fade_in(false)
            .fixed_pos(rect.min)
            .order(Order::Foreground)
            .show(ui.ctx(), |ui| ui.allocate_exact_size(rect.size(), Sense::drag()).1)
            .inner;
        let active = response.hovered() || response.dragged();
        if active {
            ui.ctx().set_cursor_icon(if vertical { egui::CursorIcon::ResizeHorizontal } else { egui::CursorIcon::ResizeVertical });
            let painter = ui.ctx().layer_painter(LayerId::new(Order::Foreground, Id::new(("tile-handle-line", id))));
            let line = if vertical {
                Rect::from_center_size(rect.center(), vec2(2.0, rect.height() - 24.0))
            } else {
                Rect::from_center_size(rect.center(), vec2(rect.width() - 24.0, 2.0))
            };
            painter.rect_filled(line, CornerRadius::same(1), c(theme::SYSTEM));
        }
        if response.dragged() {
            let d = response.drag_delta();
            layout_for(core, id, apps, content).drag(handle, if vertical { d.x } else { d.y }, content);
        }
        changed |= response.drag_stopped();
    }

    if let Some(app) = core.ui.tile_drag {
        let pointer = ui.input(|i| i.pointer.interact_pos());
        let released = ui.input(|i| !i.pointer.primary_down());
        let target = pointer.and_then(|p| rects.iter().find(|(t, r)| *t != app && r.contains(p)).map(|(t, r)| (*t, *r, zone_at(*r, p))));
        let painter = ui.ctx().layer_painter(LayerId::new(Order::Foreground, Id::new(("tile-drop", id))));
        if let Some((_, rect, zone)) = target {
            let area = zone_rect(rect, zone).shrink(4.0);
            painter.rect(area, CornerRadius::same(12), ca(theme::SYSTEM, 40), Stroke::new(2.0, c(theme::SYSTEM)), StrokeKind::Inside);
        }
        if let Some(p) = pointer {
            let label = super::tab_title(core, app);
            let size = vec2(painter.layout_no_wrap(label.clone(), theme::ui_semibold(14.0), c(theme::TEXT)).size().x + 28.0, 34.0);
            let ghost = Rect::from_min_size(p + vec2(12.0, 8.0), size);
            painter.rect(ghost, CornerRadius::same(10), c(theme::PANEL), Stroke::new(1.0, c(theme::SYSTEM)), StrokeKind::Inside);
            painter.text(ghost.center(), Align2::CENTER_CENTER, label, theme::ui_semibold(14.0), c(theme::TEXT));
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        }
        if released {
            core.ui.tile_drag = None;
            if let Some((target, _, zone)) = target {
                layout_for(core, id, apps, content).move_tile(app, target, zone);
                changed = true;
            }
        }
    }
    if changed {
        save_layouts(core);
    }
}

/// Writes every agent's layout to `layouts.json`. Window tiles stay out,
/// because window ids are only valid while the app runs (ui/window.rs).
pub fn save_layouts(core: &Core) {
    if let Some(layouts) = &core.ui.tiles {
        let mut layouts = layouts.clone();
        for layout in layouts.values_mut() {
            for tab in layout.apps() {
                if matches!(tab, AppTab::Window(_)) {
                    layout.close(tab);
                }
            }
        }
        save(&core.assets.data_dir, &layouts);
    }
}

fn load(data_dir: &Path) -> HashMap<String, TileLayout> {
    std::fs::read_to_string(data_dir.join(FILE)).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

fn save(data_dir: &Path, layouts: &HashMap<String, TileLayout>) {
    let path = data_dir.join(FILE);
    let written = serde_json::to_string_pretty(layouts)
        .map_err(|e| e.to_string())
        .and_then(|text| std::fs::write(&path, text).map_err(|e| e.to_string()));
    if let Err(e) = written {
        tracing::warn!("cannot save the tile layouts to {} ({e})", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [AppTab; 4] = [AppTab::Chat, AppTab::Editor(1), AppTab::Browser(1), AppTab::Terminal(1)];

    fn content() -> Rect {
        Rect::from_min_size(pos2(100.0, 60.0), vec2(1300.0, 800.0))
    }

    fn rect_of(layout: &TileLayout, app: AppTab) -> Rect {
        layout.rects(content()).into_iter().find(|(t, _)| *t == app).unwrap().1
    }

    #[test]
    fn the_default_matches_the_old_arrangement() {
        let layout = TileLayout::default_for(&ALL, content().width());
        let chat = rect_of(&layout, AppTab::Chat);
        assert!((chat.width() - CHAT_W).abs() < 10.0, "{chat:?}");
        assert_eq!(chat.max, content().max);
        let editor = rect_of(&layout, AppTab::Editor(1));
        assert_eq!(editor.height(), content().height(), "the editor fills its column");
        let (browser, terminal) = (rect_of(&layout, AppTab::Browser(1)), rect_of(&layout, AppTab::Terminal(1)));
        assert_eq!(browser.min.x, terminal.min.x, "browser and terminal share a column");
        assert!(browser.height() < terminal.height());
        assert_eq!(TileLayout::default_for(&[AppTab::Chat], 1300.0).rects(content()), vec![(AppTab::Chat, content())]);
    }

    #[test]
    fn tiles_cover_the_content_with_gaps_and_never_overlap() {
        let layout = TileLayout::default_for(&ALL, content().width());
        let rects = layout.rects(content());
        assert_eq!(rects.len(), 4);
        for (i, (_, a)) in rects.iter().enumerate() {
            assert!(content().contains_rect(*a));
            for (_, b) in &rects[i + 1..] {
                assert!(!a.shrink(0.5).intersects(b.shrink(0.5)), "{a:?} and {b:?} overlap");
            }
        }
        let area: f32 = rects.iter().map(|(_, r)| r.area()).sum();
        assert!(area < content().area() && area > content().area() * 0.95);
    }

    #[test]
    fn dragging_a_border_resizes_both_sides_within_limits() {
        let mut layout = TileLayout::default_for(&ALL, content().width());
        let before = rect_of(&layout, AppTab::Editor(1)).width();
        layout.drag(Handle::Column(0), 100.0, content());
        let after = rect_of(&layout, AppTab::Editor(1)).width();
        assert!((after - before - 100.0).abs() < 2.0, "{before} -> {after}");
        assert!((rect_of(&layout, AppTab::Chat).width() - CHAT_W).abs() < 10.0, "other columns keep their size");
        layout.drag(Handle::Column(0), 5000.0, content());
        assert!(rect_of(&layout, AppTab::Browser(1)).width() >= MIN_W - 1.0, "the neighbour keeps its minimum");
        let top = rect_of(&layout, AppTab::Browser(1)).height();
        layout.drag(Handle::Row(1, 0), 50.0, content());
        assert!((rect_of(&layout, AppTab::Browser(1)).height() - top - 50.0).abs() < 2.0);
        layout.drag(Handle::Row(1, 0), -5000.0, content());
        assert!(rect_of(&layout, AppTab::Browser(1)).height() >= MIN_H - 1.0);
    }

    #[test]
    fn tiles_move_by_swap_stack_and_new_column() {
        let mut layout = TileLayout::default_for(&ALL, content().width());
        let editor = rect_of(&layout, AppTab::Editor(1));
        layout.move_tile(AppTab::Chat, AppTab::Editor(1), Zone::Swap);
        assert_eq!(rect_of(&layout, AppTab::Chat), editor, "a swap keeps the sizes");

        let mut layout = TileLayout::default_for(&ALL, content().width());
        layout.move_tile(AppTab::Terminal(1), AppTab::Editor(1), Zone::Below);
        let (editor, terminal) = (rect_of(&layout, AppTab::Editor(1)), rect_of(&layout, AppTab::Terminal(1)));
        assert_eq!(editor.min.x, terminal.min.x);
        assert!(terminal.min.y > editor.max.y);
        assert_eq!(rect_of(&layout, AppTab::Browser(1)).height(), content().height(), "the browser has its column alone now");

        layout.move_tile(AppTab::Browser(1), AppTab::Chat, Zone::Right);
        assert_eq!(layout.columns.len(), 3, "the emptied column is gone and a new one exists");
        assert_eq!(rect_of(&layout, AppTab::Browser(1)).max.x, content().max.x);
        assert!((layout.columns.iter().map(|c| c.width).sum::<f32>() - 1.0).abs() < 1e-4);

        let before = layout.clone();
        layout.move_tile(AppTab::Chat, AppTab::Chat, Zone::Left);
        assert_eq!(layout, before, "a tile dropped on itself stays");
    }

    #[test]
    fn zones_follow_the_nearest_edge() {
        let r = Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 300.0));
        assert_eq!(zone_at(r, pos2(200.0, 150.0)), Zone::Swap);
        assert_eq!(zone_at(r, pos2(20.0, 150.0)), Zone::Left);
        assert_eq!(zone_at(r, pos2(390.0, 140.0)), Zone::Right);
        assert_eq!(zone_at(r, pos2(200.0, 10.0)), Zone::Above);
        assert_eq!(zone_at(r, pos2(200.0, 290.0)), Zone::Below);
    }

    #[test]
    fn apps_open_and_close_and_stay_that_way() {
        let config = [AppTab::Chat];
        let mut layout = TileLayout::default_for(&config, content().width());
        layout.open(AppTab::Files(1), content());
        assert_eq!(layout.apps(), [AppTab::Chat, AppTab::Files(1)], "a wide chat gets the new app beside it");
        layout.open(AppTab::Viewer(1), content());
        let (files, viewer) = (rect_of(&layout, AppTab::Files(1)), rect_of(&layout, AppTab::Viewer(1)));
        assert!(viewer.min.y > files.max.y || viewer.min.x > files.max.x || files.min.y > viewer.max.y, "the new tile does not overlap");
        let before = layout.clone();
        layout.open(AppTab::Files(1), content());
        assert_eq!(layout, before, "an open app opens once");

        layout.sync(&config, content().width());
        assert_eq!(layout, before, "opened apps survive while agent.toml stays the same");
        assert!(layout.close(AppTab::Files(1)));
        assert!(!layout.apps().contains(&AppTab::Files(1)));
        assert!(!layout.close(AppTab::Chat), "the chat stays open");
        assert!(!layout.close(AppTab::Terminal(1)), "closing a closed app does nothing");
        let rects = layout.rects(content());
        let area: f32 = rects.iter().map(|(_, r)| r.area()).sum();
        assert!(area > content().area() * 0.95, "the closed tile's space went to the others");
        layout.sync(&[AppTab::Chat, AppTab::Editor(1)], content().width());
        assert_eq!(layout.apps(), [AppTab::Editor(1), AppTab::Chat], "a new agent.toml starts over");
    }

    #[test]
    fn changed_windows_or_a_broken_file_start_over() {
        let mut layout = TileLayout::default_for(&ALL, 1300.0);
        layout.move_tile(AppTab::Chat, AppTab::Editor(1), Zone::Swap);
        let moved = layout.clone();
        layout.sync(&ALL, 1300.0);
        assert_eq!(layout, moved, "the same windows keep the layout");
        layout.sync(&[AppTab::Chat, AppTab::Terminal(1)], 1300.0);
        assert_eq!(layout.apps().len(), 2);
        let mut broken =
            TileLayout { columns: vec![Column { width: f32::NAN, tiles: vec![Tile::new(AppTab::Chat, 1.0)] }], config: vec![AppTab::Chat] };
        broken.sync(&[AppTab::Chat], 1300.0);
        assert_eq!(broken, TileLayout::default_for(&[AppTab::Chat], 1300.0));
        let json = serde_json::to_string(&moved).unwrap();
        assert_eq!(serde_json::from_str::<TileLayout>(&json).unwrap(), moved);
    }

    #[test]
    fn windows_are_tiles_that_serialise_and_close() {
        let mut layout = TileLayout::default_for(&[AppTab::Chat], content().width());
        layout.open(AppTab::Window(7), content());
        layout.open(AppTab::Window(9), content());
        assert_eq!(layout.apps(), [AppTab::Chat, AppTab::Window(7), AppTab::Window(9)]);
        let json = serde_json::to_string(&layout).unwrap();
        assert!(json.contains(r#"{"window":7}"#), "{json}");
        assert_eq!(serde_json::from_str::<TileLayout>(&json).unwrap(), layout);
        // An old layouts.json without windows still loads.
        let old: TileLayout =
            serde_json::from_str(r#"{"columns":[{"width":1.0,"tiles":[{"app":"chat","height":1.0}]}],"config":["chat"]}"#).unwrap();
        assert_eq!(old.apps(), [AppTab::Chat]);
        assert!(layout.close(AppTab::Window(7)));
        layout.sync(&[AppTab::Chat], content().width());
        assert_eq!(layout.apps(), [AppTab::Chat, AppTab::Window(9)], "a window tile survives the sync with agent.toml");
    }

    #[test]
    fn shells_share_a_tile_as_tabs() {
        let mut layout = TileLayout::default_for(&ALL, content().width());
        let terminal = rect_of(&layout, AppTab::Terminal(1));
        let tiles = layout.rects(content()).len();
        assert!(layout.join(AppTab::Terminal(2), AppTab::Terminal(1)), "a new shell opens as a tab of the tile");
        assert_eq!(layout.rects(content()).len(), tiles, "no new tile");
        assert_eq!(rect_of(&layout, AppTab::Terminal(2)), terminal, "the new shell shows in the tile");
        assert_eq!(layout.group(AppTab::Terminal(1)), [AppTab::Terminal(2), AppTab::Terminal(1)]);
        assert!(layout.apps().contains(&AppTab::Terminal(1)), "the hidden shell is still open");
        assert!(layout.reveal(AppTab::Terminal(1)));
        assert!(!layout.reveal(AppTab::Terminal(1)), "a shown tab stays");
        assert_eq!(layout.shown(AppTab::Terminal(2)), Some(AppTab::Terminal(1)));

        // A shell in its own tile joins the group and its tile goes.
        layout.open_beside(AppTab::Terminal(3), AppTab::Terminal(1), content());
        assert_eq!(layout.rects(content()).len(), tiles + 1);
        assert!(layout.join(AppTab::Terminal(3), AppTab::Terminal(1)));
        assert_eq!(layout.rects(content()).len(), tiles);
        assert_eq!(layout.group(AppTab::Terminal(3)).len(), 3);

        // Dragging the tile moves the whole group, and it survives a round trip.
        layout.move_tile(AppTab::Terminal(3), AppTab::Editor(1), Zone::Swap);
        assert_eq!(layout.group(AppTab::Editor(1)), [AppTab::Editor(1)]);
        assert_eq!(layout.group(AppTab::Terminal(1)).len(), 3);
        let json = serde_json::to_string(&layout).unwrap();
        assert!(json.contains(r#""more""#), "{json}");
        assert_eq!(serde_json::from_str::<TileLayout>(&json).unwrap(), layout);
        layout.move_tile(AppTab::Terminal(3), AppTab::Chat, Zone::Below);
        assert_eq!(layout.group(AppTab::Terminal(1)).len(), 3, "a move keeps the tabs together");

        // Closing the shown tab shows another one, the last one takes the tile.
        assert!(layout.close(AppTab::Terminal(3)));
        assert_eq!(layout.group(AppTab::Terminal(1)).len(), 2);
        assert!(layout.split(AppTab::Terminal(2), content()), "a tab gets its own tile again");
        assert_eq!(layout.group(AppTab::Terminal(2)), [AppTab::Terminal(2)]);
        assert!(!layout.split(AppTab::Terminal(2), content()), "a tile alone cannot split");
        assert!(layout.close(AppTab::Terminal(1)) && layout.close(AppTab::Terminal(2)));
        assert!(!layout.apps().iter().any(|t| matches!(t, AppTab::Terminal(_))));
        assert!(
            !layout.join(AppTab::Chat, AppTab::Editor(1)) && !layout.join(AppTab::Terminal(4), AppTab::Chat),
            "the chat shares no tile"
        );
        let mut twice = layout.clone();
        let first = twice.columns[0].tiles[0].app;
        twice.columns[0].tiles[0].more.push(first);
        twice.sync(&ALL, content().width());
        assert_eq!(twice, TileLayout::default_for(&ALL, content().width()), "a tab held twice is a broken file");
    }

    /// A `layouts.json` written before apps had instances, as the app wrote it.
    const OLD_LAYOUTS: &str = r#"{
      "ember": {
        "columns": [
          { "width": 0.37, "tiles": [{ "app": "editor", "height": 1.0 }] },
          { "width": 0.37, "tiles": [{ "app": "browser", "height": 0.42 }, { "app": "terminal", "height": 0.58 }] },
          { "width": 0.26, "tiles": [{ "app": "chat", "height": 1.0 }] }
        ],
        "config": ["chat", "editor", "terminal", "browser"]
      },
      "wren": {
        "columns": [
          { "width": 0.5, "tiles": [{ "app": "files", "height": 0.5 }, { "app": "viewer", "height": 0.5 }] },
          { "width": 0.5, "tiles": [{ "app": "chat", "height": 1.0 }] }
        ],
        "config": ["chat"]
      }
    }"#;

    #[test]
    fn old_layouts_load_as_first_instances_and_keep_their_arrangement() {
        let dir = std::env::temp_dir().join(format!("meerora-old-layouts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(FILE), OLD_LAYOUTS).unwrap();
        let mut layouts = load(&dir);
        std::fs::remove_file(dir.join(FILE)).unwrap();
        std::fs::remove_dir(&dir).unwrap();
        assert_eq!(layouts.len(), 2, "both agents' layouts loaded");
        let ember = layouts.get_mut("ember").unwrap();
        assert_eq!(ember.apps(), [AppTab::Editor(1), AppTab::Browser(1), AppTab::Terminal(1), AppTab::Chat]);
        let before = ember.clone();
        ember.sync(&[AppTab::Chat, AppTab::Editor(1), AppTab::Terminal(1), AppTab::Browser(1)], content().width());
        assert_eq!(*ember, before, "the same agent.toml keeps the old layout");
        assert_eq!(layouts["wren"].apps(), [AppTab::Files(1), AppTab::Viewer(1), AppTab::Chat]);
        // Written again, instance 1 keeps the old spelling, so older builds read it too.
        let json = serde_json::to_string(&layouts["ember"]).unwrap();
        assert!(json.contains(r#""app":"terminal""#) && !json.contains(r#"{"terminal""#), "{json}");
    }

    #[test]
    fn second_instances_open_beside_the_first_and_serialise_with_their_number() {
        let mut layout = TileLayout::default_for(&ALL, content().width());
        layout.open_beside(AppTab::Terminal(2), AppTab::Terminal(1), content());
        let (one, two) = (rect_of(&layout, AppTab::Terminal(1)), rect_of(&layout, AppTab::Terminal(2)));
        assert_eq!(one.min.x, two.min.x, "the terminal is tall, so the second one goes below it");
        assert!(two.min.y > one.max.y);
        layout.open_beside(AppTab::Browser(2), AppTab::Window(99), content());
        assert!(layout.apps().contains(&AppTab::Browser(2)), "a missing neighbour opens it in the largest tile's place");
        let json = serde_json::to_string(&layout).unwrap();
        assert!(json.contains(r#"{"terminal":2}"#) && json.contains(r#"{"browser":2}"#) && json.contains(r#""terminal""#), "{json}");
        assert_eq!(serde_json::from_str::<TileLayout>(&json).unwrap(), layout);
        assert!(layout.close(AppTab::Terminal(2)));
        assert!(layout.apps().contains(&AppTab::Terminal(1)), "closing the second terminal leaves the first");
        for bad in [r#""terminal:0""#, r#"{"terminal":0}"#, r#"{"chat":2}"#, r#""shell""#, r#"{"terminal":2,"browser":3}"#] {
            assert!(serde_json::from_str::<AppTab>(bad).is_err(), "{bad} is refused");
        }
    }
}
