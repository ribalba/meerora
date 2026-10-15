//! Navigation grid for walking agents (D9).
//!
//! Agents plan on a grid derived from the space geometry. [`NavGrid::build`]
//! rasterises the static mesh onto the xz plane over the space bounds, with
//! cells of [`CELL`] cm. A triangle that reaches above [`STEP_HEIGHT`] and
//! starts below [`HEAD_HEIGHT`] is an obstacle: walls, furniture, the tree, the
//! hedge, the mailbox post. Floors, rugs, stepping stones and low steps stay
//! walkable. A cell is blocked when its centre lies within [`AGENT_RADIUS`] of
//! an obstacle footprint, of a spot without floor or of the edge of the bounds.
//!
//! [`NavGrid::find_path`] runs A* over 8 neighbours without cutting corners
//! and shortens the result with line of sight checks, so agents walk straight
//! segments around the furniture.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, VecDeque};
use std::f32::consts::SQRT_2;

use meerora_geom::{Aabb, Mesh, Vec2, Vec3};

/// Edge length of a grid cell in cm.
pub const CELL: f32 = 8.0;
/// Radius of an agent in cm. Obstacles grow by this much.
pub const AGENT_RADIUS: f32 = 22.0;
/// Geometry reaching above this height in cm is in the way.
const STEP_HEIGHT: f32 = 25.0;
/// Geometry starting above this height in cm passes over the head.
const HEAD_HEIGHT: f32 = 160.0;
/// Largest grid along either axis, in cells.
const MAX_CELLS: usize = 2048;
/// How far [`NavGrid::nearest_free`] searches, in cells.
const SEARCH_CELLS: isize = 40;
const NO_REGION: u32 = u32::MAX;

/// The 8 neighbours of a cell with the cost of the step in cells.
const NEIGHBOURS: [(isize, isize, f32); 8] =
    [(1, 0, 1.0), (-1, 0, 1.0), (0, 1, 1.0), (0, -1, 1.0), (1, 1, SQRT_2), (1, -1, SQRT_2), (-1, 1, SQRT_2), (-1, -1, SQRT_2)];

/// Occupancy grid of a space on the xz plane.
pub struct NavGrid {
    /// World x and z of the outer corner of cell (0, 0).
    origin: Vec2,
    width: usize,
    depth: usize,
    blocked: Vec<bool>,
    /// Height of the walkable surface at each cell centre, `-inf` without floor.
    floor: Vec<f32>,
    /// Connected area of each free cell, [`NO_REGION`] for blocked cells.
    region: Vec<u32>,
    /// The largest connected area. Stations and agents belong there.
    main_region: u32,
}

impl NavGrid {
    /// Builds the grid of a space mesh. `None` when there is nothing to walk on.
    pub fn build(mesh: &Mesh, bounds: Aabb) -> Option<NavGrid> {
        if mesh.is_empty() || bounds.is_empty() {
            return None;
        }
        let width = ((bounds.max.x - bounds.min.x) / CELL).ceil() as usize;
        let depth = ((bounds.max.z - bounds.min.z) / CELL).ceil() as usize;
        if width == 0 || depth == 0 || width > MAX_CELLS || depth > MAX_CELLS {
            return None;
        }
        let n = width * depth;
        let mut grid = NavGrid {
            origin: Vec2::new(bounds.min.x, bounds.min.z),
            width,
            depth,
            blocked: vec![false; n],
            floor: vec![f32::NEG_INFINITY; n],
            region: Vec::new(),
            main_region: NO_REGION,
        };
        let mut obstacles = Vec::new();
        for tri in mesh.indices.as_chunks::<3>().0 {
            let [a, b, c] = tri.map(|i| Vec3::from(mesh.vertices[i as usize].position));
            let low = a.y.min(b.y).min(c.y);
            let high = a.y.max(b.y).max(c.y);
            if high > STEP_HEIGHT && low < HEAD_HEIGHT {
                obstacles.push([xz(a), xz(b), xz(c)]);
            } else if high <= STEP_HEIGHT && (b - a).cross(c - a).normalize_or_zero().y.abs() > 0.5 {
                grid.add_floor(a, b, c);
            }
        }
        if grid.floor.iter().all(|y| *y == f32::NEG_INFINITY) {
            grid.floor.fill(0.0);
        }
        grid.block_edges(bounds);
        for t in &obstacles {
            grid.block_near(t);
        }
        grid.label_regions();
        Some(grid)
    }

    /// A grid with the given blocked cells, origin at zero and the floor at 0.
    #[cfg(test)]
    pub fn from_cells(width: usize, depth: usize, blocked: Vec<bool>) -> NavGrid {
        assert_eq!(blocked.len(), width * depth);
        let mut grid = NavGrid {
            origin: Vec2::ZERO,
            width,
            depth,
            floor: vec![0.0; blocked.len()],
            blocked,
            region: Vec::new(),
            main_region: NO_REGION,
        };
        grid.label_regions();
        grid
    }

    /// Width and depth in cells.
    pub fn size(&self) -> (usize, usize) {
        (self.width, self.depth)
    }

    /// True when an agent may stand at `p`.
    pub fn is_free(&self, p: Vec3) -> bool {
        self.index(xz(p)).is_some_and(|i| !self.blocked[i])
    }

    /// Height of the floor under `p`.
    pub fn floor_y(&self, p: Vec3) -> Option<f32> {
        self.index(xz(p)).map(|i| self.floor[i]).filter(|y| y.is_finite())
    }

    /// True when every cell the segment touches is free (supercover line).
    pub fn line_free(&self, a: Vec3, b: Vec3) -> bool {
        self.segment_clear(xz(a), xz(b), &|i| self.blocked[i])
    }

    /// `p` itself when an agent can stand there and reach the rest of the
    /// space, else the centre of the nearest such cell.
    pub fn nearest_free(&self, p: Vec3) -> Option<Vec3> {
        let q = xz(p);
        if self.index(q).is_some_and(|i| !self.blocked[i] && self.region[i] == self.main_region) {
            return Some(p);
        }
        let i = self.nearest_free_cell(q)?;
        let c = self.centre(i);
        Some(Vec3::new(c.x, self.floor[i], c.y))
    }

    /// Points to walk from `from` to `to`, without `from` and ending exactly at
    /// `to`. `None` when the two are not connected.
    pub fn find_path(&self, from: Vec3, to: Vec3) -> Option<Vec<Vec3>> {
        self.path(from, to, None)
    }

    /// Like [`Self::find_path`] but also keeps `radius` away from `others`,
    /// for example agents that stand in the way. Cells right at the start and
    /// at the goal stay open.
    pub fn find_path_around(&self, from: Vec3, to: Vec3, others: &[Vec3], radius: f32) -> Option<Vec<Vec3>> {
        let mut extra = vec![false; self.blocked.len()];
        let keep = [xz(from), xz(to)];
        let reach = (radius / CELL).ceil() as isize + 1;
        for o in others {
            let q = xz(*o);
            let (cx, cz) = self.coords(q);
            for dz in -reach..=reach {
                for dx in -reach..=reach {
                    let Some(i) = self.cell(cx + dx, cz + dz) else { continue };
                    let c = self.centre(i);
                    if c.distance(q) <= radius && keep.iter().all(|k| c.distance(*k) > CELL * 1.5) {
                        extra[i] = true;
                    }
                }
            }
        }
        self.path(from, to, Some(&extra))
    }

    fn path(&self, from: Vec3, to: Vec3, extra: Option<&[bool]>) -> Option<Vec<Vec3>> {
        let blocked = |i: usize| self.blocked[i] || extra.is_some_and(|e| e[i]);
        let (a, b) = (xz(from), xz(to));
        let start_in = self.index(a).filter(|&i| !blocked(i));
        let goal_in = self.index(b).filter(|&i| !blocked(i));
        let start = start_in.or_else(|| self.nearest_free_cell(a))?;
        let goal = goal_in.or_else(|| self.nearest_free_cell(b))?;
        if self.region[start] != self.region[goal] {
            return None;
        }
        let cells = self.astar(start, goal, &blocked)?;

        // The start, the cell centres and the goal. A start or goal in a
        // blocked cell is joined straight to its cell and kept out of the
        // shortening, since that stretch cannot pass a line of sight check.
        let mut nodes = Vec::with_capacity(cells.len() + 2);
        nodes.push(a);
        nodes.extend(cells.iter().map(|&i| self.centre(i)));
        nodes.push(b);
        let lo = if start_in.is_some() { 0 } else { 1 };
        let hi = if goal_in.is_some() { nodes.len() - 1 } else { nodes.len() - 2 };
        let mut out = nodes[..lo].to_vec();
        out.extend(self.shorten(&nodes[lo..=hi], &blocked));
        out.extend_from_slice(&nodes[hi + 1..]);

        let last = out.len() - 1;
        let points = out
            .iter()
            .enumerate()
            .skip(1)
            .map(|(k, q)| {
                if k == last {
                    to
                } else {
                    let y = self.index(*q).map(|i| self.floor[i]).filter(|y| y.is_finite()).unwrap_or(to.y);
                    Vec3::new(q.x, y, q.y)
                }
            })
            .collect();
        Some(points)
    }

    /// Drops every node that the previous kept node can see past.
    fn shorten(&self, nodes: &[Vec2], blocked: &impl Fn(usize) -> bool) -> Vec<Vec2> {
        let mut out = vec![nodes[0]];
        let mut anchor = 0;
        for j in 2..nodes.len() {
            if !self.segment_clear(nodes[anchor], nodes[j], blocked) {
                anchor = j - 1;
                out.push(nodes[anchor]);
            }
        }
        if nodes.len() > 1 {
            out.push(nodes[nodes.len() - 1]);
        }
        out
    }

    /// A* over 8 neighbours with the octile distance as heuristic.
    fn astar(&self, start: usize, goal: usize, blocked: &impl Fn(usize) -> bool) -> Option<Vec<usize>> {
        let n = self.blocked.len();
        let w = self.width;
        let (gx, gz) = ((goal % w) as isize, (goal / w) as isize);
        let heuristic = |i: usize| {
            let dx = ((i % w) as isize - gx).abs() as f32;
            let dz = ((i / w) as isize - gz).abs() as f32;
            dx.max(dz) + (SQRT_2 - 1.0) * dx.min(dz)
        };
        let mut cost = vec![f32::INFINITY; n];
        let mut parent = vec![u32::MAX; n];
        let mut closed = vec![false; n];
        let mut open = BinaryHeap::new();
        cost[start] = 0.0;
        open.push(Open { f: heuristic(start), g: 0.0, cell: start as u32 });
        while let Some(Open { g, cell, .. }) = open.pop() {
            let cell = cell as usize;
            if closed[cell] {
                continue;
            }
            closed[cell] = true;
            if cell == goal {
                let mut path = vec![goal];
                let mut at = goal;
                while at != start {
                    at = parent[at] as usize;
                    path.push(at);
                }
                path.reverse();
                return Some(path);
            }
            for &(dx, dz, step) in &NEIGHBOURS {
                let Some(next) = self.step(cell, dx, dz, blocked) else { continue };
                if closed[next] {
                    continue;
                }
                let g_next = g + step;
                if g_next < cost[next] {
                    cost[next] = g_next;
                    parent[next] = cell as u32;
                    open.push(Open { f: g_next + heuristic(next), g: g_next, cell: next as u32 });
                }
            }
        }
        None
    }

    /// The neighbour of `cell` in direction (`dx`, `dz`) when the step is
    /// allowed. A diagonal step needs both cells beside it free, so a path
    /// never cuts the corner of a blocked cell.
    fn step(&self, cell: usize, dx: isize, dz: isize, blocked: &impl Fn(usize) -> bool) -> Option<usize> {
        let (x, z) = ((cell % self.width) as isize, (cell / self.width) as isize);
        let free = |x, z| self.cell(x, z).filter(|&i| !blocked(i));
        let next = free(x + dx, z + dz)?;
        if dx != 0 && dz != 0 && (free(x + dx, z).is_none() || free(x, z + dz).is_none()) {
            return None;
        }
        Some(next)
    }

    /// Walks the cells under the segment from `a` to `b`. Where the segment
    /// passes exactly through a cell corner, both cells beside the corner must
    /// be free as well.
    fn segment_clear(&self, a: Vec2, b: Vec2, blocked: &impl Fn(usize) -> bool) -> bool {
        let free = |x, z| self.cell(x, z).is_some_and(|i| !blocked(i));
        let pa = (a - self.origin) / CELL;
        let pb = (b - self.origin) / CELL;
        let (mut x, mut z) = (pa.x.floor() as isize, pa.y.floor() as isize);
        let (ex, ez) = (pb.x.floor() as isize, pb.y.floor() as isize);
        let d = pb - pa;
        let (sx, sz) = (d.x.signum() as isize * (d.x != 0.0) as isize, d.y.signum() as isize * (d.y != 0.0) as isize);
        let delta_x = if d.x != 0.0 { 1.0 / d.x.abs() } else { f32::INFINITY };
        let delta_z = if d.y != 0.0 { 1.0 / d.y.abs() } else { f32::INFINITY };
        let mut next_x = match sx {
            1 => (x as f32 + 1.0 - pa.x) * delta_x,
            -1 => (pa.x - x as f32) * delta_x,
            _ => f32::INFINITY,
        };
        let mut next_z = match sz {
            1 => (z as f32 + 1.0 - pa.y) * delta_z,
            -1 => (pa.y - z as f32) * delta_z,
            _ => f32::INFINITY,
        };
        let steps = (ex - x).abs() + (ez - z).abs();
        for _ in 0..=steps {
            if !free(x, z) {
                return false;
            }
            if x == ex && z == ez {
                return true;
            }
            if (next_x - next_z).abs() < 1e-6 {
                if !free(x + sx, z) || !free(x, z + sz) {
                    return false;
                }
                x += sx;
                z += sz;
                next_x += delta_x;
                next_z += delta_z;
            } else if next_x < next_z {
                x += sx;
                next_x += delta_x;
            } else {
                z += sz;
                next_z += delta_z;
            }
        }
        free(ex, ez)
    }

    /// The free cell nearest to `q` within [`SEARCH_CELLS`], preferring the
    /// main region.
    fn nearest_free_cell(&self, q: Vec2) -> Option<usize> {
        let (cx, cz) = self.coords(q);
        let mut main: Option<(usize, f32)> = None;
        let mut any: Option<(usize, f32)> = None;
        for dz in -SEARCH_CELLS..=SEARCH_CELLS {
            for dx in -SEARCH_CELLS..=SEARCH_CELLS {
                let Some(i) = self.cell(cx + dx, cz + dz) else { continue };
                if self.blocked[i] {
                    continue;
                }
                let d = self.centre(i).distance_squared(q);
                if any.is_none_or(|(_, best)| d < best) {
                    any = Some((i, d));
                }
                if self.region[i] == self.main_region && main.is_none_or(|(_, best)| d < best) {
                    main = Some((i, d));
                }
            }
        }
        main.or(any).map(|(i, _)| i)
    }

    /// Records the height of a walkable triangle at the cell centres it covers.
    fn add_floor(&mut self, a: Vec3, b: Vec3, c: Vec3) {
        let t = [xz(a), xz(b), xz(c)];
        let area = (t[1] - t[0]).perp_dot(t[2] - t[0]);
        if area.abs() < 1e-6 {
            return;
        }
        let Some((x0, z0, x1, z1)) = self.centres_within(t[0].min(t[1]).min(t[2]), t[0].max(t[1]).max(t[2])) else { return };
        for z in z0..=z1 {
            for x in x0..=x1 {
                let i = z * self.width + x;
                let p = self.centre(i);
                let w0 = (t[1] - p).perp_dot(t[2] - p) / area;
                let w1 = (t[2] - p).perp_dot(t[0] - p) / area;
                let w2 = 1.0 - w0 - w1;
                if w0 >= -1e-4 && w1 >= -1e-4 && w2 >= -1e-4 {
                    let y = w0 * a.y + w1 * b.y + w2 * c.y;
                    self.floor[i] = self.floor[i].max(y);
                }
            }
        }
    }

    /// Blocks cells near spots without floor and near the edge of the bounds,
    /// so the slab edges act as walls.
    fn block_edges(&mut self, bounds: Aabb) {
        let reach = AGENT_RADIUS + CELL * 0.5;
        let k = (reach / CELL).floor() as isize;
        let disk: Vec<(isize, isize)> = (-k..=k)
            .flat_map(|dz| (-k..=k).map(move |dx| (dx, dz)))
            .filter(|&(dx, dz)| ((dx * dx + dz * dz) as f32) * CELL * CELL <= reach * reach)
            .collect();
        for i in 0..self.floor.len() {
            if self.floor[i] != f32::NEG_INFINITY {
                continue;
            }
            let (x, z) = ((i % self.width) as isize, (i / self.width) as isize);
            for &(dx, dz) in &disk {
                if let Some(j) = self.cell(x + dx, z + dz) {
                    self.blocked[j] = true;
                }
            }
        }
        for i in 0..self.blocked.len() {
            let c = self.centre(i);
            let edge = (c.x - bounds.min.x).min(bounds.max.x - c.x).min(c.y - bounds.min.z).min(bounds.max.z - c.y);
            if edge < AGENT_RADIUS {
                self.blocked[i] = true;
            }
        }
    }

    /// Blocks every cell whose centre lies within the agent radius of the xz
    /// footprint of an obstacle triangle.
    fn block_near(&mut self, t: &[Vec2; 3]) {
        let r = Vec2::splat(AGENT_RADIUS);
        let Some((x0, z0, x1, z1)) = self.centres_within(t[0].min(t[1]).min(t[2]) - r, t[0].max(t[1]).max(t[2]) + r) else { return };
        for z in z0..=z1 {
            for x in x0..=x1 {
                let i = z * self.width + x;
                if !self.blocked[i] && distance_squared_to_triangle(self.centre(i), t) <= AGENT_RADIUS * AGENT_RADIUS {
                    self.blocked[i] = true;
                }
            }
        }
    }

    /// Groups free cells into areas connected by allowed steps.
    fn label_regions(&mut self) {
        let n = self.blocked.len();
        self.region = vec![NO_REGION; n];
        let mut sizes = Vec::new();
        let mut queue = VecDeque::new();
        let blocked = |i: usize| self.blocked[i];
        for seed in 0..n {
            if self.blocked[seed] || self.region[seed] != NO_REGION {
                continue;
            }
            let id = sizes.len() as u32;
            self.region[seed] = id;
            queue.push_back(seed);
            let mut size = 0usize;
            while let Some(cell) = queue.pop_front() {
                size += 1;
                for &(dx, dz, _) in &NEIGHBOURS {
                    if let Some(next) = self.step(cell, dx, dz, &blocked)
                        && self.region[next] == NO_REGION
                    {
                        self.region[next] = id;
                        queue.push_back(next);
                    }
                }
            }
            sizes.push(size);
        }
        self.main_region = sizes.iter().enumerate().max_by_key(|(_, s)| **s).map_or(NO_REGION, |(i, _)| i as u32);
    }

    /// Range of cells whose centres lie inside the rectangle from `lo` to `hi`.
    fn centres_within(&self, lo: Vec2, hi: Vec2) -> Option<(usize, usize, usize, usize)> {
        let first = ((lo - self.origin) / CELL - 0.5).ceil().max(Vec2::ZERO);
        let last = ((hi - self.origin) / CELL - 0.5).floor().min(Vec2::new(self.width as f32 - 1.0, self.depth as f32 - 1.0));
        if first.x > last.x || first.y > last.y {
            return None;
        }
        Some((first.x as usize, first.y as usize, last.x as usize, last.y as usize))
    }

    fn coords(&self, q: Vec2) -> (isize, isize) {
        let g = ((q - self.origin) / CELL).floor();
        (g.x as isize, g.y as isize)
    }

    fn index(&self, q: Vec2) -> Option<usize> {
        let (x, z) = self.coords(q);
        self.cell(x, z)
    }

    fn cell(&self, x: isize, z: isize) -> Option<usize> {
        (x >= 0 && z >= 0 && (x as usize) < self.width && (z as usize) < self.depth).then(|| z as usize * self.width + x as usize)
    }

    fn centre(&self, i: usize) -> Vec2 {
        self.origin + (Vec2::new((i % self.width) as f32, (i / self.width) as f32) + 0.5) * CELL
    }
}

/// The ground position of a point, x and z.
pub fn xz(p: Vec3) -> Vec2 {
    Vec2::new(p.x, p.z)
}

/// Squared distance from `p` to a triangle, zero inside. Degenerate triangles,
/// such as the footprint of a vertical wall, count as their edges.
fn distance_squared_to_triangle(p: Vec2, [a, b, c]: &[Vec2; 3]) -> f32 {
    let area = (*b - *a).perp_dot(*c - *a);
    if area.abs() > 1e-6 {
        let d = [(*b - *a).perp_dot(p - *a), (*c - *b).perp_dot(p - *b), (*a - *c).perp_dot(p - *c)];
        if d.iter().all(|v| *v >= 0.0) || d.iter().all(|v| *v <= 0.0) {
            return 0.0;
        }
    }
    distance_squared_to_segment(p, *a, *b).min(distance_squared_to_segment(p, *b, *c)).min(distance_squared_to_segment(p, *c, *a))
}

fn distance_squared_to_segment(p: Vec2, a: Vec2, b: Vec2) -> f32 {
    let ab = b - a;
    let len = ab.length_squared();
    let t = if len > 0.0 { ((p - a).dot(ab) / len).clamp(0.0, 1.0) } else { 0.0 };
    (a + ab * t).distance_squared(p)
}

/// An open A* node, ordered so the heap pops the lowest estimate first.
struct Open {
    f: f32,
    g: f32,
    cell: u32,
}

impl Ord for Open {
    fn cmp(&self, other: &Self) -> Ordering {
        other.f.total_cmp(&self.f).then_with(|| self.g.total_cmp(&other.g))
    }
}

impl PartialOrd for Open {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Open {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Open {}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Adds an axis aligned box from `lo` to `hi`, faces wound outwards.
    pub fn push_box(mesh: &mut Mesh, lo: Vec3, hi: Vec3) {
        let p =
            |x: bool, y: bool, z: bool| Vec3::new(if x { hi.x } else { lo.x }, if y { hi.y } else { lo.y }, if z { hi.z } else { lo.z });
        let faces = [
            [p(false, true, false), p(false, true, true), p(true, true, true), p(true, true, false)],
            [p(false, false, false), p(true, false, false), p(true, false, true), p(false, false, true)],
            [p(false, false, true), p(true, false, true), p(true, true, true), p(false, true, true)],
            [p(false, false, false), p(false, true, false), p(true, true, false), p(true, false, false)],
            [p(true, false, false), p(true, true, false), p(true, true, true), p(true, false, true)],
            [p(false, false, false), p(false, false, true), p(false, true, true), p(false, true, false)],
        ];
        for [a, b, c, d] in faces {
            mesh.push_quad(a, b, c, d);
        }
    }

    /// A floor slab from (0, 0) to (`w`, `d`) with its top at y = 0.
    pub fn slab(w: f32, d: f32) -> Mesh {
        let mut mesh = Mesh::new();
        push_box(&mut mesh, Vec3::new(0.0, -10.0, 0.0), Vec3::new(w, 0.0, d));
        mesh
    }

    fn room_with_box() -> NavGrid {
        let mut mesh = slab(400.0, 300.0);
        push_box(&mut mesh, Vec3::new(150.0, 0.0, 80.0), Vec3::new(250.0, 80.0, 220.0));
        NavGrid::build(&mesh, mesh.bounds()).unwrap()
    }

    #[test]
    fn paths_go_around_a_box() {
        let grid = room_with_box();
        let (from, to) = (Vec3::new(60.0, 0.0, 150.0), Vec3::new(340.0, 0.0, 150.0));
        assert!(!grid.line_free(from, to), "the box is in the way");
        let path = grid.find_path(from, to).expect("a way around");
        assert_eq!(*path.last().unwrap(), to);
        let mut at = from;
        let mut length = 0.0;
        for &p in &path {
            assert!(grid.line_free(at, p), "segment {at} -> {p} crosses a blocked cell");
            length += at.distance(p);
            at = p;
        }
        // Around the box and its inflation, shortened to a few straight segments.
        assert!(length > 300.0 && length < 420.0, "length {length}");
        assert!(path.len() <= 5, "{path:?}");
        // Nothing inside the box or closer to it than the agent radius.
        assert!(!grid.is_free(Vec3::new(200.0, 0.0, 150.0)));
        assert!(!grid.is_free(Vec3::new(140.0, 0.0, 150.0)));
        assert!(grid.is_free(Vec3::new(120.0, 0.0, 150.0)));
        // The slab edge is a wall too.
        assert!(!grid.is_free(Vec3::new(10.0, 0.0, 150.0)));
        assert!(!grid.is_free(Vec3::new(200.0, 0.0, 295.0)));
    }

    #[test]
    fn low_geometry_is_walkable_and_floating_geometry_too() {
        let mut mesh = slab(400.0, 300.0);
        // A rug, a step and a shelf above the head.
        push_box(&mut mesh, Vec3::new(50.0, 0.0, 50.0), Vec3::new(150.0, 1.0, 150.0));
        push_box(&mut mesh, Vec3::new(200.0, 0.0, 50.0), Vec3::new(300.0, 10.0, 150.0));
        push_box(&mut mesh, Vec3::new(100.0, 170.0, 200.0), Vec3::new(300.0, 180.0, 260.0));
        let grid = NavGrid::build(&mesh, mesh.bounds()).unwrap();
        assert!(grid.is_free(Vec3::new(100.0, 0.0, 100.0)));
        assert!(grid.is_free(Vec3::new(250.0, 0.0, 100.0)));
        assert!((grid.floor_y(Vec3::new(250.0, 0.0, 100.0)).unwrap() - 10.0).abs() < 1e-3);
        assert!(grid.is_free(Vec3::new(200.0, 0.0, 230.0)));
    }

    #[test]
    fn diagonal_footprints_are_rasterised_exactly() {
        // A thin wall at 45 degrees. Its bounding box covers most of the room,
        // its footprint only a band along the diagonal.
        let mut mesh = slab(400.0, 400.0);
        let (a, b) = (Vec3::new(100.0, 0.0, 100.0), Vec3::new(300.0, 0.0, 300.0));
        let up = Vec3::Y * 100.0;
        mesh.push_quad(a, b, b + up, a + up);
        let grid = NavGrid::build(&mesh, mesh.bounds()).unwrap();
        assert!(!grid.is_free(Vec3::new(200.0, 0.0, 200.0)));
        assert!(!grid.is_free(Vec3::new(210.0, 0.0, 195.0)));
        assert!(grid.is_free(Vec3::new(250.0, 0.0, 150.0)));
        assert!(grid.is_free(Vec3::new(150.0, 0.0, 250.0)));
    }

    #[test]
    fn corners_cannot_be_cut() {
        // A wall of cells along the anti-diagonal. Every gap is a corner.
        let mut blocked = vec![false; 16];
        for (x, z) in [(0, 3), (1, 2), (2, 1), (3, 0)] {
            blocked[z * 4 + x] = true;
        }
        let grid = NavGrid::from_cells(4, 4, blocked);
        let at = |x: f32, z: f32| Vec3::new((x + 0.5) * CELL, 0.0, (z + 0.5) * CELL);
        assert!(grid.find_path(at(0.0, 0.0), at(3.0, 3.0)).is_none());
        assert!(!grid.line_free(at(1.0, 1.0), at(2.0, 2.0)));
        assert!(grid.astar(0, 15, &|i| grid.blocked[i]).is_none());

        // With one blocked cell in the middle the path bends around it and
        // never passes through its corners.
        let mut blocked = vec![false; 25];
        blocked[2 * 5 + 2] = true;
        let grid = NavGrid::from_cells(5, 5, blocked);
        let cells = grid.astar(6, 18, &|i| grid.blocked[i]).unwrap();
        for pair in cells.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            let (dx, dz) = ((b % 5) as isize - (a % 5) as isize, (b / 5) as isize - (a / 5) as isize);
            if dx != 0 && dz != 0 {
                let side_a = (a / 5) * 5 + (b % 5);
                let side_b = (b / 5) * 5 + (a % 5);
                assert!(!grid.blocked[side_a] && !grid.blocked[side_b], "{cells:?} cuts a corner");
            }
        }
        assert!(!grid.line_free(at(1.0, 1.0), at(3.0, 3.0)));
    }

    #[test]
    fn nearest_free_leaves_obstacles() {
        let grid = room_with_box();
        let inside = Vec3::new(140.0, 0.0, 150.0);
        let moved = grid.nearest_free(inside).unwrap();
        assert!(grid.is_free(moved));
        assert!(moved.distance(inside) < 20.0, "{moved}");
        let free = Vec3::new(60.0, 0.0, 150.0);
        assert_eq!(grid.nearest_free(free), Some(free));
    }

    #[test]
    fn paths_avoid_others_when_asked() {
        let grid = room_with_box();
        let (from, to) = (Vec3::new(60.0, 0.0, 40.0), Vec3::new(340.0, 0.0, 40.0));
        assert_eq!(grid.find_path(from, to), Some(vec![to]), "straight along the front");
        let path = grid.find_path_around(from, to, &[Vec3::new(200.0, 0.0, 40.0)], 2.0 * AGENT_RADIUS).expect("a way behind the box");
        assert!(path.iter().any(|p| p.z > 220.0), "the other agent closes the front, so the path goes behind the box: {path:?}");
    }
}
