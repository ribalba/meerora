//! What the space looks like right now: the space script, the agents with
//! their figures, where they walk and which ring they show (D3, D9).
//!
//! The daemon decides what agents do. The world turns that into motion and
//! builds a [`meerora_scene::Frame`] every frame.
//!
//! This deviates from D9. Agents no longer walk the waypoint graph of the
//! space script. They plan on a grid derived from the space geometry
//! ([`crate::nav`]), so they walk around furniture instead of through it, and
//! they steer around each other on the way. The waypoint graph stays as the
//! fallback when the grid finds no path.

use std::collections::{HashMap, HashSet, VecDeque};
use std::f32::consts::{PI, TAU};

use meerora_geom::{Aabb, Mat4, Quat, Vec2, Vec3};
use meerora_protocol::{Event, RingState, Role, Trigger};
use meerora_scene::{BlobShadow, Camera, DashedPath, Draw, Frame, MeshId, PointLight, Ring};
use meerora_script::{Anim, FigureDesc, Motion, SpaceDesc, Station};

use crate::nav::{self, AGENT_RADIUS, NavGrid, xz};
use crate::theme;

/// Walking speed in cm per second.
const WALK_SPEED: f32 = 120.0;
/// Distance between agents side by side at one station, also between
/// waiting agents at the front (D3).
const SLOT_SPACING: f32 = 48.0;
/// How long the walked path stays visible after arrival.
const TRAIL_SECONDS: f32 = 10.0;
/// Longest simulation step in seconds. Longer frames are split so a walking
/// agent never skips a grid cell.
const MAX_STEP: f32 = 1.0 / 30.0;
/// Walking agents move away from agents closer than this, two radii and a margin.
const SEPARATION: f32 = 2.0 * AGENT_RADIUS + 6.0;
/// Closer than this a walking agent no longer moves towards another agent.
const CONTACT: f32 = 2.0 * AGENT_RADIUS + 2.0;
/// How far ahead on its path a walking agent looks for agents to pass.
const LOOKAHEAD: f32 = 110.0;
/// When two agents meet head on closer than this, the one added later steps
/// aside and waits.
const YIELD_RANGE: f32 = 100.0;
const YIELD_SECONDS: f32 = 0.7;
/// Seconds before the same agent yields again.
const YIELD_COOLDOWN: f32 = 2.5;
/// Seconds without progress before a walking agent plans again.
const STALL_SECONDS: f32 = 2.0;
/// Plans after getting stuck before the agent may walk through the others.
const MAX_REPLANS: u32 = 2;
/// Speed in cm per second at which standing agents that overlap move apart.
const NUDGE_SPEED: f32 = 40.0;
/// Two agents closer than this across the view cover each other on screen,
/// about two thirds of a figure's width. An arriving agent avoids such a
/// slot next to the agents already at the station (`pick_slot`).
const MIN_ACROSS: f32 = 30.0;
/// Extra walking distance in cm that each cm of cover below
/// [`MIN_ACROSS`] costs an arriving agent when it picks a slot.
const COVER_COST: f32 = 4.0;
/// Furniture this close to a slot on the camera's side hides an agent
/// standing there, which costs as much extra walking as this many cm. Less
/// than full cover by a neighbour, which hides it as well.
const HIDDEN_COST: f32 = 90.0;
const HIDDEN_REACH: f32 = 64.0;

pub struct Space {
    pub desc: SpaceDesc,
    pub mesh: MeshId,
    /// One mesh per wall of `desc.walls`, in the same order.
    pub walls: Vec<MeshId>,
    graph: WalkGraph,
    nav: Option<NavGrid>,
    /// Where agents stand at each station, in the order of `desc.stations`.
    /// A station inside an obstacle moves to the nearest free cell.
    spots: Vec<Vec3>,
    /// Places for agents sharing a station, in the order agents claim them.
    /// The first is the station spot. The others stand beside it along the
    /// station's side axis, then in rows behind and in front, all on free
    /// cells in sight of the spot.
    slots: Vec<Vec<Vec3>>,
}

impl Space {
    pub fn new(desc: SpaceDesc, mesh: MeshId, walls: Vec<MeshId>) -> Self {
        let graph = WalkGraph::new(&desc);
        let started = std::time::Instant::now();
        // Agents walk around the walls even while they are cut away.
        let nav = NavGrid::build(&desc.solid_mesh(), desc.bounds);
        if let Some(nav) = &nav {
            let (w, d) = nav.size();
            tracing::debug!("navigation grid of {w}x{d} cells built in {:.1} ms", started.elapsed().as_secs_f64() * 1000.0);
        }
        let spots: Vec<Vec3> = desc.stations.iter().map(|s| station_spot(nav.as_ref(), s)).collect();
        let slots = desc.stations.iter().zip(&spots).map(|(s, &spot)| station_slots(nav.as_ref(), s, spot)).collect();
        Self { desc, mesh, walls, graph, nav, spots, slots }
    }

    fn station_index(&self, id: &str) -> Option<usize> {
        self.desc.stations.iter().position(|s| s.id == id)
    }

    fn front_index(&self) -> Option<usize> {
        self.desc.stations.iter().position(|s| s.front)
    }

    /// Spots of `n` waiting agents side by side at the front station (D3),
    /// centred on it. The row shifts sideways when part of it would stand in
    /// a blocked cell.
    fn front_row(&self, front: usize, n: usize) -> Vec<Vec3> {
        let station = &self.desc.stations[front];
        let spot = self.spots[front];
        let side = Quat::from_rotation_y(station.facing_deg.to_radians()) * Vec3::X;
        let row = |shift: f32| -> Vec<Vec3> {
            (0..n).map(|k| spot + side * ((k as f32 - (n as f32 - 1.0) / 2.0) * SLOT_SPACING + shift)).collect()
        };
        let Some(nav) = &self.nav else { return row(0.0) };
        for step in 0..=20 {
            for shift in [step as f32 * nav::CELL, -(step as f32) * nav::CELL] {
                let places = row(shift);
                if places.iter().all(|p| nav.is_free(*p) && nav.line_free(spot, *p)) {
                    return places.into_iter().map(|p| on_floor(nav, p)).collect();
                }
            }
        }
        row(0.0).into_iter().map(|p| nav.nearest_free(p).unwrap_or(p)).collect()
    }

    /// Route from `from` to `to`, the spot of `station`. A* on the grid,
    /// keeping clear of `others` when given, else the waypoint graph (D9).
    fn plan(&self, from: Vec3, to: Vec3, station: &str, others: &[Vec3]) -> Route {
        if let Some(nav) = &self.nav {
            let path = match others {
                [] => nav.find_path(from, to),
                _ => nav.find_path_around(from, to, others, 2.0 * AGENT_RADIUS).or_else(|| nav.find_path(from, to)),
            };
            if let Some(points) = path {
                return Route { points: points.into(), on_grid: true };
            }
            tracing::debug!("no path on the grid to {station}, walking the waypoint graph");
        }
        let mut points: VecDeque<Vec3> = self.graph.route(from, station).into();
        // The graph node of the station is replaced by the exact spot (slots).
        points.pop_back();
        points.push_back(to);
        // Skip the first node when we are already closer to the second one.
        if points.len() >= 2 && from.distance(points[1]) < points[0].distance(points[1]) {
            points.pop_front();
        }
        Route { points, on_grid: false }
    }
}

/// Points to walk through, the last one is the spot.
struct Route {
    points: VecDeque<Vec3>,
    /// Planned on the grid. False for the waypoint graph fallback.
    on_grid: bool,
}

/// Where agents stand at a station. That is its position, or the nearest
/// free cell when the geometry blocks it.
fn station_spot(nav: Option<&NavGrid>, station: &Station) -> Vec3 {
    let p = station.position;
    let Some(nav) = nav else { return p };
    match nav.nearest_free(p) {
        Some(spot) if spot == p => p,
        Some(spot) => {
            tracing::warn!(
                "station {} lies in a blocked cell of the space, agents stand {:.0} cm away at the nearest free cell",
                station.id,
                xz(spot).distance(xz(p))
            );
            spot
        }
        None => {
            tracing::warn!("station {} has no free cell nearby, agents walk the waypoint graph to it", station.id);
            p
        }
    }
}

/// The slots of a station, see [`Space::slots`].
fn station_slots(nav: Option<&NavGrid>, station: &Station, spot: Vec3) -> Vec<Vec3> {
    let turn = Quat::from_rotation_y(station.facing_deg.to_radians());
    let (side, back) = (turn * Vec3::X, turn * Vec3::NEG_Z);
    let mut slots = vec![spot];
    for row in [0.0, 1.0, -1.0, 2.0, -2.0] {
        for column in [0.0, 1.0, -1.0, 2.0, -2.0, 3.0, -3.0] {
            if row == 0.0 && column == 0.0 {
                continue;
            }
            let p = spot + (side * column + back * row) * SLOT_SPACING;
            match nav {
                None => slots.push(p),
                Some(nav) if nav.is_free(p) && nav.line_free(spot, p) => slots.push(on_floor(nav, p)),
                Some(_) => {}
            }
        }
    }
    slots
}

/// The slot an agent arriving at a station takes, among the `free` slot
/// numbers of `slots`. It prefers slots near the station spot that stand
/// apart across the view from the slots already `taken`, so neighbours
/// spread over the screen instead of along the view ray, and slots that
/// furniture does not `hide` from the camera. `right` is the camera's right
/// vector on the floor, `None` when the camera looks straight down. Ties go
/// to the lower slot number, which keeps the old order (beside the spot
/// first) wherever the view does not matter.
fn pick_slot(
    slots: &[Vec3],
    free: impl Iterator<Item = usize>,
    taken: &[Vec3],
    right: Option<Vec2>,
    hidden: &dyn Fn(Vec3) -> bool,
) -> Option<usize> {
    let spot = *slots.first()?;
    let cost = |p: Vec3| {
        let cover = right.map_or(0.0, |right| taken.iter().map(|q| (MIN_ACROSS - (xz(p) - xz(*q)).dot(right).abs()).max(0.0)).sum());
        // The first agent always takes the spot, furniture only decides
        // between the slots beside the agents already there.
        let behind = !taken.is_empty() && hidden(p);
        flat_distance(p, spot) + COVER_COST * cover + if behind { HIDDEN_COST } else { 0.0 }
    };
    let mut best: Option<(usize, f32)> = None;
    for k in free.filter(|k| *k < slots.len()) {
        let c = cost(slots[k]);
        if best.is_none_or(|(_, b)| c < b - 0.01) {
            best = Some((k, c));
        }
    }
    best.map(|(k, _)| k)
}

/// Whether furniture stands between `p` and a camera in the direction
/// `toward` (on the floor, unit length), within [`HIDDEN_REACH`].
fn hidden_from(nav: &NavGrid, p: Vec3, toward: Vec2) -> bool {
    (1..=(HIDDEN_REACH / nav::CELL) as usize).any(|k| {
        let q = xz(p) + toward * (k as f32 * nav::CELL);
        !nav.is_free(Vec3::new(q.x, p.y, q.y))
    })
}

/// `p` at the height of the floor under it.
fn on_floor(nav: &NavGrid, p: Vec3) -> Vec3 {
    Vec3::new(p.x, nav.floor_y(p).unwrap_or(p.y), p.z)
}

/// Undirected walk graph over stations and waypoints.
struct WalkGraph {
    names: Vec<String>,
    positions: Vec<Vec3>,
    adjacent: Vec<Vec<usize>>,
}

impl WalkGraph {
    fn new(desc: &SpaceDesc) -> Self {
        let names: Vec<String> = desc.waypoints.iter().map(|(n, _)| n.clone()).collect();
        let positions: Vec<Vec3> = desc.waypoints.iter().map(|(_, p)| *p).collect();
        let index: HashMap<&str, usize> = names.iter().enumerate().map(|(i, n)| (n.as_str(), i)).collect();
        let mut adjacent = vec![Vec::new(); names.len()];
        for (a, b) in &desc.edges {
            if let (Some(&ia), Some(&ib)) = (index.get(a.as_str()), index.get(b.as_str())) {
                adjacent[ia].push(ib);
                adjacent[ib].push(ia);
            }
        }
        Self { names, positions, adjacent }
    }

    fn nearest(&self, p: Vec3) -> Option<usize> {
        (0..self.positions.len()).min_by(|&a, &b| self.positions[a].distance_squared(p).total_cmp(&self.positions[b].distance_squared(p)))
    }

    /// Node positions from the node nearest to `from` to the node `to`
    /// (Dijkstra, the graph is tiny).
    fn route(&self, from: Vec3, to: &str) -> Vec<Vec3> {
        let (Some(start), Some(goal)) = (self.nearest(from), self.names.iter().position(|n| n == to)) else {
            return Vec::new();
        };
        let n = self.positions.len();
        let mut dist = vec![f32::INFINITY; n];
        let mut prev = vec![usize::MAX; n];
        let mut done = vec![false; n];
        dist[start] = 0.0;
        for _ in 0..n {
            let Some(u) = (0..n).filter(|&i| !done[i] && dist[i].is_finite()).min_by(|&a, &b| dist[a].total_cmp(&dist[b])) else {
                break;
            };
            done[u] = true;
            if u == goal {
                break;
            }
            for &v in &self.adjacent[u] {
                let d = dist[u] + self.positions[u].distance(self.positions[v]);
                if d < dist[v] {
                    dist[v] = d;
                    prev[v] = u;
                }
            }
        }
        if !dist[goal].is_finite() {
            return vec![self.positions[goal]];
        }
        let mut path = vec![self.positions[goal]];
        let mut at = goal;
        while at != start {
            at = prev[at];
            path.push(self.positions[at]);
        }
        path.reverse();
        path
    }
}

/// A figure built from its script and uploaded to the renderer.
pub struct Figure {
    pub desc: FigureDesc,
    pub base: MeshId,
    pub parts: Vec<MeshId>,
}

/// One message of the agent panel conversation.
#[derive(Clone, Debug)]
pub struct ChatMessage {
    pub id: String,
    pub role: Role,
    pub text: String,
    pub done: bool,
}

/// The run an agent is working on right now.
#[derive(Clone, Debug)]
pub struct RunInfo {
    pub id: String,
    pub trigger: Trigger,
    pub task: String,
    /// Unix time in milliseconds when the run started, 0 when the daemon
    /// did not say (an older daemon).
    pub started: u64,
}

/// One logged action on your apps (D12).
#[derive(Clone, Debug)]
pub struct ActionEntry {
    pub at: f64,
    pub tool: String,
    /// One short line for the log.
    pub summary: String,
    /// The full command, path, URL or query when the summary was cut.
    pub target: Option<String>,
}

impl ActionEntry {
    /// What the action was aimed at, in full: the target, else the summary.
    /// Apps that open what the agent touched use this, never the summary.
    pub fn target(&self) -> &str {
        self.target.as_deref().unwrap_or(&self.summary)
    }
}

/// Where an agent stands or walks to.
#[derive(Clone, Debug)]
struct Target {
    station: String,
    position: Vec3,
    /// Heading while standing there, radians around y.
    facing: f32,
}

pub struct Agent {
    pub id: String,
    pub name: String,
    pub color: [u8; 3],
    pub system: bool,
    pub home: String,
    pub figure: Option<Figure>,
    pub figure_error: Option<String>,
    /// True while the daemon reports this agent.
    pub online: bool,
    pub ring: RingState,
    pub progress: Option<f32>,
    pub activity: String,
    pub params: HashMap<String, serde_json::Value>,
    pub actions: Vec<ActionEntry>,
    /// Time of the last terminal input from the agent, for "is typing" (D12).
    pub last_typed: Option<f64>,
    pub messages: Vec<ChatMessage>,
    pub run: Option<RunInfo>,
    /// Next scheduled wake and when its countdown began, unix milliseconds (D23).
    pub next_wake: Option<(u64, u64)>,
    /// Station the daemon sent the agent to. Waiting overrides it (D3).
    assigned: Option<String>,
    /// Slot claimed at a station shared with other agents: station id and
    /// slot number.
    claim: Option<(String, usize)>,
    target: Option<Target>,
    pub position: Vec3,
    heading: f32,
    route: VecDeque<Vec3>,
    /// The route comes from the grid. False for the waypoint graph fallback.
    on_grid: bool,
    /// Ground velocity of the last step, for the others to predict.
    velocity: Vec3,
    /// Remaining route length at the best progress so far.
    best_left: f32,
    /// Seconds since the remaining route last got shorter.
    stall: f32,
    replans: u32,
    /// Walks through other agents until arrival, the last resort when stuck.
    ghost: bool,
    /// Seconds left standing aside for an oncoming agent.
    yield_left: f32,
    yield_cooldown: f32,
    /// Agent being passed and the side (1 left, -1 right), so the choice
    /// does not flicker.
    passing: Option<(usize, f32)>,
    /// Seconds before the next re-plan after losing sight of the next point.
    replan_cooldown: f32,
    walk_phase: f32,
    trail: Vec<Vec3>,
    trail_left: f32,
    ring_since: f64,
    /// Highlighted because the pointer is over the figure or its list entry.
    pub hover: bool,
}

impl Agent {
    pub fn new(dir: &meerora_protocol::AgentDir) -> Self {
        let color = theme::hex_rgb(&dir.config.color).unwrap_or([0xE8, 0xEA, 0xF0]);
        Self {
            id: dir.id.clone(),
            name: dir.config.name.clone(),
            color,
            system: dir.config.capabilities.system,
            home: dir.config.home.clone(),
            figure: None,
            figure_error: None,
            online: false,
            ring: RingState::Resting,
            progress: None,
            activity: String::new(),
            params: HashMap::new(),
            actions: Vec::new(),
            last_typed: None,
            messages: Vec::new(),
            run: None,
            next_wake: None,
            assigned: None,
            claim: None,
            target: None,
            position: Vec3::ZERO,
            heading: 0.0,
            route: VecDeque::new(),
            on_grid: false,
            velocity: Vec3::ZERO,
            best_left: 0.0,
            stall: 0.0,
            replans: 0,
            ghost: false,
            yield_left: 0.0,
            yield_cooldown: 0.0,
            passing: None,
            replan_cooldown: 0.0,
            walk_phase: 0.0,
            trail: Vec::new(),
            trail_left: 0.0,
            ring_since: 0.0,
            hover: false,
        }
    }

    pub fn is_walking(&self) -> bool {
        !self.route.is_empty()
    }

    /// Station the agent is at or walking to.
    pub fn station(&self) -> Option<&str> {
        self.target.as_ref().map(|t| t.station.as_str())
    }

    pub fn is_typing(&self, now: f64) -> bool {
        self.last_typed.is_some_and(|t| now - t < 2.0)
    }

    fn param_value(&self, def: &meerora_script::ParamDef) -> serde_json::Value {
        if let Some(v) = self.params.get(&def.name) {
            return v.clone();
        }
        use meerora_script::ParamKind::*;
        match &def.kind {
            Toggle { default } => (*default).into(),
            Choice { default, .. } => default.clone().into(),
            Number { default, .. } => (*default).into(),
            Count { default } => (*default).into(),
            Text { default } => default.clone().into(),
        }
    }

    fn anim(&self) -> Option<&Anim> {
        let f = self.figure.as_ref()?;
        Some(match self.ring {
            RingState::Working => &f.desc.animations.working,
            RingState::NeedsYou => &f.desc.animations.waiting,
            RingState::Resting => &f.desc.animations.resting,
        })
    }

    /// Root transform: position and heading, without the state motion.
    fn root(&self) -> Mat4 {
        Mat4::from_rotation_translation(Quat::from_rotation_y(self.heading), self.position)
    }

    fn start_route(&mut self, route: Route) {
        self.route = route.points;
        self.on_grid = route.on_grid;
        self.best_left = route_length(self.position, &self.route);
        self.stall = 0.0;
        self.replans = 0;
        self.ghost = false;
        self.yield_left = 0.0;
        self.passing = None;
    }

    fn arrive(&mut self, spot: Vec3) {
        self.position = spot;
        self.route.clear();
        self.velocity = Vec3::ZERO;
        self.stall = 0.0;
        self.replans = 0;
        self.ghost = false;
        self.yield_left = 0.0;
        self.passing = None;
    }
}

pub struct World {
    pub space: Option<Space>,
    pub agents: Vec<Agent>,
    pub time: f64,
    /// The camera of the space script. The user may turn and move the view
    /// on top of it, see [`World::viewer`].
    pub camera: Camera,
    /// Direction from the space towards the viewer of the turned view, for
    /// agents that face the viewer. `None` uses [`World::camera`].
    pub viewer: Option<Vec3>,
    /// Agents that reached the station they walked to, with the station, for
    /// `Command::Arrived`. The app takes them after every update.
    pub arrivals: Vec<(String, String)>,
}

impl World {
    pub fn new() -> Self {
        Self {
            space: None,
            agents: Vec::new(),
            time: 0.0,
            camera: Camera {
                target: Vec3::new(400.0, 60.0, 350.0),
                azimuth_deg: 45.0,
                elevation_deg: 30.0,
                view_height: 900.0,
                aspect: 1.6,
            },
            viewer: None,
            arrivals: Vec::new(),
        }
    }

    pub fn set_space(&mut self, space: Space) {
        let hint = &space.desc.camera;
        self.camera.target = hint.target;
        self.camera.view_height = hint.view_height;
        self.camera.azimuth_deg = hint.azimuth_deg;
        self.camera.elevation_deg = hint.elevation_deg;
        let first = self.space.is_none();
        self.space = Some(space);
        self.refresh_claims();
        for (i, spot) in self.desired_spots().into_iter().enumerate() {
            let Some(spot) = spot else { continue };
            if first || self.agents[i].target.is_none() {
                self.place(i, spot);
            } else {
                // A walking agent plans again on the new geometry.
                let walking = self.agents[i].is_walking();
                self.retarget_to(i, spot, walking);
            }
        }
    }

    pub fn agent(&self, id: &str) -> Option<&Agent> {
        self.agents.iter().find(|a| a.id == id)
    }

    pub fn agent_index(&self, id: &str) -> Option<usize> {
        self.agents.iter().position(|a| a.id == id)
    }

    pub fn add_agent(&mut self, agent: Agent) -> usize {
        self.agents.push(agent);
        let i = self.agents.len() - 1;
        self.refresh_claims();
        if let Some(spot) = self.desired_spots().swap_remove(i) {
            self.place(i, spot);
        }
        i
    }

    /// Takes an agent out of the space, for example into the graveyard. The
    /// others keep their places. Returns it, so its meshes can be freed.
    pub fn remove_agent(&mut self, id: &str) -> Option<Agent> {
        let i = self.agent_index(id)?;
        let agent = self.agents.remove(i);
        // The agent being passed is an index, and the indices moved.
        for a in &mut self.agents {
            a.passing = None;
        }
        self.retarget_all();
        Some(agent)
    }

    /// Puts an agent straight onto its spot without walking.
    fn place(&mut self, i: usize, spot: Target) {
        let a = &mut self.agents[i];
        a.position = spot.position;
        a.heading = spot.facing;
        a.route.clear();
        a.target = Some(spot);
    }

    /// The station of agent `i`, the one the daemon assigned or else home.
    /// `None` while the agent waits at the front.
    fn regular_station(&self, space: &Space, i: usize) -> Option<usize> {
        let a = &self.agents[i];
        if a.ring == RingState::NeedsYou && space.front_index().is_some() {
            return None;
        }
        a.assigned.as_deref().and_then(|s| space.station_index(s)).or_else(|| space.station_index(&a.home))
    }

    /// Hands out the slots of shared stations, which generalises the front
    /// row of D3 to every station. Standing agents keep their slot. Walking
    /// agents and agents with a new station pick a free slot in the order
    /// they were added ([`pick_slot`]): near the spot and apart across the
    /// current view from the agents already there. So a slot freed by
    /// someone leaving goes to the next arrival, nobody standing has to
    /// move, and turning the camera moves nobody.
    fn refresh_claims(&mut self) {
        let right = self.view_right();
        let Some(space) = &self.space else { return };
        let stations: Vec<Option<usize>> = (0..self.agents.len()).map(|i| self.regular_station(space, i)).collect();
        let mut taken: HashSet<(usize, usize)> = HashSet::new();
        let mut keep = vec![false; self.agents.len()];
        for (i, a) in self.agents.iter().enumerate() {
            if let (Some(s), Some((id, slot))) = (stations[i], &a.claim)
                && !a.is_walking()
                && *id == space.desc.stations[s].id
                && taken.insert((s, *slot))
            {
                keep[i] = true;
            }
        }
        for (i, a) in self.agents.iter_mut().enumerate() {
            if keep[i] {
                continue;
            }
            a.claim = stations[i].map(|s| {
                let slots = &space.slots[s];
                let occupied: Vec<Vec3> = taken.iter().filter(|(t, k)| *t == s && *k < slots.len()).map(|(_, k)| slots[*k]).collect();
                let free = (0..slots.len()).filter(|k| !taken.contains(&(s, *k)));
                // More agents than slots share them as before.
                // The camera looks from the side opposite its right vector's normal.
                let hidden = |p: Vec3| match (&space.nav, right) {
                    (Some(nav), Some(r)) => hidden_from(nav, p, Vec2::new(-r.y, r.x)),
                    _ => false,
                };
                let picked = pick_slot(slots, free, &occupied, right, &hidden);
                let slot = picked.unwrap_or_else(|| (0..).find(|k| !taken.contains(&(s, *k))).unwrap_or(0));
                taken.insert((s, slot));
                (space.desc.stations[s].id.clone(), slot)
            });
        }
    }

    /// The camera's right vector on the floor: the turned view while the
    /// space view shows it ([`World::viewer`]), else the space script's
    /// camera. `None` when it looks straight down.
    fn view_right(&self) -> Option<Vec2> {
        let toward = self.viewer.unwrap_or_else(|| self.camera.toward_viewer());
        let flat = Vec2::new(toward.x, toward.z);
        (flat.length() > 1e-3).then(|| {
            let v = flat.normalize();
            Vec2::new(v.y, -v.x)
        })
    }

    /// Where every agent should be. That is the row at the front when it needs
    /// you (D3), else its slot at the station the daemon assigned, else at its
    /// home.
    fn desired_spots(&self) -> Vec<Option<Target>> {
        let Some(space) = &self.space else { return vec![None; self.agents.len()] };
        let front = space.front_index();
        let waiting: Vec<usize> = match front {
            Some(_) => (0..self.agents.len()).filter(|&j| self.agents[j].ring == RingState::NeedsYou).collect(),
            None => Vec::new(),
        };
        let row = front.map(|f| space.front_row(f, waiting.len())).unwrap_or_default();
        (0..self.agents.len())
            .map(|i| {
                if let (Some(f), Some(k)) = (front, waiting.iter().position(|&j| j == i)) {
                    let s = &space.desc.stations[f];
                    return Some(Target { station: s.id.clone(), position: row[k], facing: s.facing_deg.to_radians() });
                }
                let s = self.regular_station(space, i)?;
                let station = &space.desc.stations[s];
                let slots = &space.slots[s];
                let slot = self.agents[i].claim.as_ref().filter(|(id, _)| *id == station.id).map_or(0, |(_, k)| *k);
                Some(Target { station: station.id.clone(), position: slots[slot % slots.len()], facing: station.facing_deg.to_radians() })
            })
            .collect()
    }

    /// Starts walking to `spot` if it differs from the current target, or
    /// always when `force` is set.
    fn retarget_to(&mut self, i: usize, spot: Target, force: bool) {
        let a = &mut self.agents[i];
        if !force && a.target.as_ref().is_some_and(|t| t.station == spot.station && t.position.distance(spot.position) < 1.0) {
            return;
        }
        let Some(space) = &self.space else { return };
        let route = space.plan(a.position, spot.position, &spot.station, &[]);
        if !a.is_walking() {
            a.trail = vec![a.position];
        }
        a.trail_left = TRAIL_SECONDS;
        a.start_route(route);
        a.target = Some(spot);
    }

    /// Re-evaluates every agent's spot and starts walking where it changed.
    fn retarget_all(&mut self) {
        self.refresh_claims();
        for (i, spot) in self.desired_spots().into_iter().enumerate() {
            if let Some(spot) = spot {
                self.retarget_to(i, spot, false);
            }
        }
    }

    /// Applies one daemon event. Returns the agent id when an agent needs
    /// the user from now on.
    pub fn apply(&mut self, event: &Event) -> Option<String> {
        let now = self.time;
        let i = event.agent().and_then(|id| self.agent_index(id));
        match event {
            Event::Hello { .. } => {}
            Event::AgentSpawned { name, system, .. } => {
                if let Some(i) = i {
                    let a = &mut self.agents[i];
                    a.online = true;
                    a.name = name.clone();
                    a.system = *system;
                }
            }
            Event::AgentRemoved { .. } => {
                if let Some(i) = i {
                    self.agents[i].online = false;
                }
            }
            Event::Ring { state, progress, .. } => {
                let i = i?;
                let was = self.agents[i].ring;
                let a = &mut self.agents[i];
                a.ring = *state;
                a.progress = *progress;
                if was != *state {
                    a.ring_since = now;
                    self.retarget_all();
                    if *state == RingState::NeedsYou {
                        return Some(self.agents[i].id.clone());
                    }
                }
            }
            Event::GoTo { station, .. } => {
                let i = i?;
                self.agents[i].assigned = Some(station.clone());
                self.retarget_all();
                // Already there: it arrives without a step.
                let a = &self.agents[i];
                if !a.is_walking() && a.station() == Some(station.as_str()) {
                    self.arrivals.push((a.id.clone(), station.clone()));
                }
            }
            Event::Activity { text, .. } => {
                self.agents[i?].activity = text.clone();
            }
            Event::Param { name, value, .. } => {
                self.agents[i?].params.insert(name.clone(), value.clone());
            }
            Event::Action { tool, summary, target, .. } => {
                let i = i?;
                let a = &mut self.agents[i];
                a.actions.push(ActionEntry { at: now, tool: tool.clone(), summary: summary.clone(), target: target.clone() });
                if a.actions.len() > 200 {
                    a.actions.remove(0);
                }
                // D9: a working agent walks to the station of the tool it uses.
                let station = self.space.as_ref().and_then(|s| s.desc.station_for_action(tool)).map(|s| s.id.clone());
                if let Some(station) = station
                    && self.agents[i].ring == RingState::Working
                {
                    self.agents[i].assigned = Some(station);
                    self.retarget_all();
                }
            }
            Event::TerminalInput { .. } => {
                self.agents[i?].last_typed = Some(now);
            }
            Event::Message { id, role, text, done, .. } => {
                let a = &mut self.agents[i?];
                match a.messages.iter_mut().find(|m| m.id == *id) {
                    Some(m) => {
                        m.text = text.clone();
                        m.done = *done;
                    }
                    None => {
                        a.messages.push(ChatMessage { id: id.clone(), role: *role, text: text.clone(), done: *done });
                        if a.messages.len() > 200 {
                            a.messages.remove(0);
                        }
                    }
                }
            }
            Event::RunStarted { run, trigger, task, started, .. } => {
                self.agents[i?].run = Some(RunInfo { id: run.clone(), trigger: *trigger, task: task.clone(), started: *started });
            }
            Event::RunFinished { run, outcome, summary, .. } => {
                let a = &mut self.agents[i?];
                if a.run.as_ref().is_some_and(|r| r.id == *run) {
                    a.run = None;
                }
                if *outcome == meerora_protocol::Outcome::NeedsYou
                    && let Some(summary) = summary
                {
                    a.activity = summary.clone();
                }
            }
            Event::NextWake { at, since, .. } => {
                self.agents[i?].next_wake = at.map(|at| (at, *since));
            }
            Event::FigureDraft { .. }
            | Event::ToolResult { .. }
            | Event::CapabilityRequest { .. }
            | Event::CapabilitiesChanged { .. }
            | Event::IndexResults { .. }
            | Event::Imported { .. }
            | Event::Account { .. }
            | Event::InterviewStep { .. }
            | Event::OwnProfile { .. }
            | Event::Social { .. }
            | Event::Feed { .. }
            | Event::PostDrafts { .. }
            | Event::NewsDigest { .. }
            | Event::TemplateAdded { .. }
            | Event::Devices { .. }
            | Event::DeviceParam { .. }
            | Event::DeviceCapabilityRequest { .. }
            | Event::DeviceCapabilitiesChanged { .. }
            | Event::DeviceActionResult { .. }
            | Event::Graveyard { .. }
            | Event::AppRequest { .. }
            | Event::GraveyardResult { .. }
            | Event::Answer { .. }
            | Event::ConfigProposal { .. }
            | Event::ConfigResult { .. } => {}
        }
        None
    }

    /// Clears daemon-provided state when the connection drops.
    pub fn daemon_lost(&mut self) {
        for a in &mut self.agents {
            a.online = false;
        }
    }

    pub fn update(&mut self, dt: f32) {
        self.time += dt as f64;
        if dt > 0.0 {
            let steps = (dt / MAX_STEP).ceil().max(1.0) as usize;
            for _ in 0..steps {
                self.step(dt / steps as f32);
            }
        }
        let viewer = self.viewer.unwrap_or_else(|| self.camera.toward_viewer());
        let viewer_heading = viewer.x.atan2(viewer.z);
        for a in &mut self.agents {
            if a.is_walking() {
                continue;
            }
            a.walk_phase = 0.0;
            a.trail_left = (a.trail_left - dt).max(0.0);
            let face_viewer = a.anim().is_some_and(|anim| anim.face_viewer) && a.ring == RingState::NeedsYou;
            let goal = if face_viewer { Some(viewer_heading) } else { a.target.as_ref().map(|t| t.facing) };
            if let Some(goal) = goal {
                a.heading = turn_towards(a.heading, goal, dt * 4.0);
            }
        }
    }

    /// Moves the walking agents by one short step and moves standing agents
    /// apart that overlap.
    fn step(&mut self, dt: f32) {
        let Some(space) = &self.space else { return };
        let bodies: Vec<Body> = self
            .agents
            .iter()
            .map(|a| Body { position: xz(a.position), velocity: xz(a.velocity), walking: a.is_walking(), ghost: a.ghost })
            .collect();
        let steers: Vec<Option<Steer>> =
            self.agents.iter().enumerate().map(|(i, a)| a.is_walking().then(|| steer(space.nav.as_ref(), a, i, &bodies))).collect();
        for (i, s) in steers.into_iter().enumerate() {
            match s {
                Some(s) => {
                    walk(space, &mut self.agents[i], i, s, &bodies, dt);
                    let a = &self.agents[i];
                    if !a.is_walking()
                        && let Some(station) = a.station()
                    {
                        self.arrivals.push((a.id.clone(), station.to_string()));
                    }
                }
                None => self.agents[i].velocity = Vec3::ZERO,
            }
        }
        nudge_standing(space.nav.as_ref(), &mut self.agents, dt);
    }

    /// World transform of an agent's figure including its state motion.
    pub fn figure_transform(&self, a: &Agent) -> Mat4 {
        let t = (self.time - a.ring_since) as f32;
        let mut m = a.root();
        if a.is_walking() {
            let hop = (a.walk_phase * PI).sin().abs() * 4.0;
            return m * Mat4::from_translation(Vec3::Y * hop);
        }
        if let Some(anim) = a.anim() {
            m *= motion_matrix(&anim.motion, t);
        }
        m
    }

    pub fn ring_point(&self, a: &Agent) -> Vec3 {
        let local = a.figure.as_ref().map_or(Vec3::new(0.0, 110.0, 0.0), |f| f.desc.ring_anchor);
        self.figure_transform(a).transform_point3(local)
    }

    /// World bounds of an agent, for picking.
    pub fn bounds(&self, a: &Agent) -> Aabb {
        let local = a.figure.as_ref().map_or(Aabb::new(Vec3::new(-25.0, 0.0, -25.0), Vec3::new(25.0, 100.0, 25.0)), |f| f.desc.bounds);
        local.transformed(self.figure_transform(a))
    }

    /// The nearest agent under a picking ray.
    pub fn pick(&self, origin: Vec3, dir: Vec3) -> Option<usize> {
        self.agents
            .iter()
            .enumerate()
            .filter_map(|(i, a)| self.bounds(a).ray_hit(origin, dir).map(|t| (i, t)))
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i)
    }

    /// Everything to draw, seen through `camera`. Walls between the camera
    /// and the room are left out.
    pub fn frame(&self, camera: Camera) -> Frame {
        let mut frame = Frame {
            camera,
            clear_color: theme::linear(theme::BG),
            ambient_sky: [0.16, 0.18, 0.24],
            ambient_ground: [0.09, 0.07, 0.06],
            sun_direction: Vec3::new(-0.4, -1.0, -0.3).normalize(),
            sun_color: [1.0, 0.95, 0.88],
            sun_intensity: 0.5,
            lights: Vec::new(),
            draws: Vec::new(),
            rings: Vec::new(),
            shadows: Vec::new(),
            paths: Vec::new(),
        };
        if let Some(space) = &self.space {
            let d = &space.desc;
            frame.clear_color = d.background;
            frame.ambient_sky = d.ambient_sky;
            frame.ambient_ground = d.ambient_ground;
            frame.sun_direction = d.sun_direction;
            frame.sun_color = d.sun_color;
            frame.sun_intensity = d.sun_intensity;
            frame.lights = d
                .lights
                .iter()
                .map(|l| PointLight { position: l.position, color: l.color, intensity: l.intensity, radius: l.radius })
                .collect();
            frame.draws.push(Draw { mesh: space.mesh, transform: Mat4::IDENTITY, brightness: 1.0, emissive_scale: 1.0 });
            let toward_viewer = camera.toward_viewer();
            for (wall, mesh) in d.walls.iter().zip(&space.walls) {
                if wall.faces(toward_viewer) {
                    frame.draws.push(Draw { mesh: *mesh, transform: Mat4::IDENTITY, brightness: 1.0, emissive_scale: 1.0 });
                }
            }
        }
        for a in &self.agents {
            self.push_agent(a, &mut frame);
        }
        frame
    }

    fn push_agent(&self, a: &Agent, frame: &mut Frame) {
        let now = self.time as f32;
        let m = self.figure_transform(a);
        let resting = a.ring == RingState::Resting && !a.is_walking();
        let eyes_closed = a.anim().is_some_and(|anim| anim.eyes_closed) && resting;
        let (brightness, emissive_scale) = if resting { (0.72, if eyes_closed { 0.12 } else { 0.5 }) } else { (1.0, 1.0) };
        let brightness = if a.hover { brightness * 1.15 } else { brightness };
        let mut radius = 22.0;
        if let Some(f) = &a.figure {
            frame.draws.push(Draw { mesh: f.base, transform: m, brightness, emissive_scale });
            for (part, mesh) in f.desc.param_parts.iter().zip(&f.parts) {
                let def = f.desc.params.iter().find(|p| p.name == part.param);
                if def.is_none_or(|d| a.param_value(d) != part.when) {
                    continue;
                }
                let local = match &part.anim {
                    Some(motion) => Mat4::from_translation(part.pivot) * motion_matrix(motion, now) * Mat4::from_translation(-part.pivot),
                    None => Mat4::IDENTITY,
                };
                frame.draws.push(Draw { mesh: *mesh, transform: m * local, brightness, emissive_scale });
            }
            let size = f.desc.bounds.size();
            radius = (size.x.max(size.z) * 0.5).clamp(12.0, 60.0);
        }

        let ground = a.position;
        let lift = m.transform_point3(Vec3::ZERO).y - ground.y;
        frame.shadows.push(BlobShadow {
            center: ground,
            radius: radius * (1.0 - (lift / 60.0).clamp(0.0, 0.4)),
            opacity: 0.5 - (lift / 80.0).clamp(0.0, 0.25),
        });

        let center = self.ring_point(a);
        let r = (radius * 0.7).clamp(12.0, 22.0);
        let (color, base_intensity) = match (a.system, a.ring) {
            (true, RingState::Resting) => (theme::linear(theme::SYSTEM), 0.35),
            (true, _) => (theme::linear(theme::SYSTEM), 1.0),
            (false, RingState::Working) => (theme::linear(theme::WORKING), 1.0),
            (false, RingState::NeedsYou) => (theme::linear(theme::NEEDS_YOU), 1.0),
            (false, RingState::Resting) => (theme::linear(theme::RESTING), 0.35),
        };
        let pulse = if a.ring == RingState::NeedsYou { 0.85 + 0.3 * (now * 3.5).sin().abs() } else { 1.0 };
        let intensity = base_intensity * pulse * if a.hover { 1.3 } else { 1.0 };
        let countdown = (a.ring == RingState::Resting).then(|| wake_left(a.next_wake, unix_ms())).flatten();
        let (arc_start, arc_len) = match (a.ring, countdown) {
            (RingState::Working, _) => (now * 2.6, PI),
            (_, Some(left)) => {
                // A scheduled agent's resting ring shrinks until it wakes, over a faint full track.
                frame.rings.push(Ring { center, radius: r, width: 2.6, color, intensity: intensity * 0.3, arc_start: 0.0, arc_len: TAU });
                (0.0, TAU * left)
            }
            _ => (0.0, TAU),
        };
        // The remaining countdown is drawn at full strength so it reads against the dim track.
        let (width, intensity) = if countdown.is_some() { (3.2, if a.hover { 1.3 } else { 1.0 }) } else { (2.6, intensity) };
        frame.rings.push(Ring { center, radius: r, width, color, intensity, arc_start, arc_len });
        if a.system {
            // D3: a double violet ring marks system privileges.
            frame.rings.push(Ring {
                center: center + Vec3::Y * 4.0,
                radius: r + 5.0,
                width: 1.8,
                color,
                intensity: intensity * 0.8,
                arc_start: 0.0,
                arc_len: TAU,
            });
        }

        if a.trail.len() > 1 && a.trail_left > 0.0 {
            frame.paths.push(DashedPath {
                points: a.trail.iter().map(|p| *p + Vec3::Y * 0.6).collect(),
                width: 3.0,
                dash: 9.0,
                gap: 8.0,
                color: theme::linear(theme::SYSTEM),
                opacity: 0.85 * (a.trail_left / TRAIL_SECONDS).min(1.0),
            });
        }
    }
}

/// The share of the countdown to the next wake that is still left, 1 right
/// after a wake and 0 when the agent wakes. `None` without a schedule.
fn wake_left(next_wake: Option<(u64, u64)>, now_ms: u64) -> Option<f32> {
    let (at, since) = next_wake?;
    let total = at.saturating_sub(since).max(1) as f32;
    Some((at.saturating_sub(now_ms) as f32 / total).clamp(0.0, 1.0))
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Local transform of a state motion at `t` seconds since the state began.
pub fn motion_matrix(motion: &Motion, t: f32) -> Mat4 {
    match *motion {
        Motion::Still => Mat4::IDENTITY,
        Motion::Bob { amplitude, period } => Mat4::from_translation(Vec3::Y * amplitude * 0.5 * (1.0 - (t * TAU / period.max(0.1)).cos())),
        Motion::Hop { count, height, period } => {
            let period = period.max(0.2);
            let phase = t % period;
            let hop_time = 0.32;
            let k = (phase / hop_time) as u32;
            if k < count.max(1) {
                let local = (phase - k as f32 * hop_time) / hop_time;
                Mat4::from_translation(Vec3::Y * height * 4.0 * local * (1.0 - local))
            } else {
                Mat4::IDENTITY
            }
        }
        Motion::Breathe { period, depth } => {
            let s = (t * TAU / period.max(0.1)).sin() * 0.5 + 0.5;
            Mat4::from_scale(Vec3::new(1.0 + depth * 0.5 * s, 1.0 - depth * s, 1.0 + depth * 0.5 * s))
        }
        Motion::Spin { period } => Mat4::from_rotation_y(t * TAU / period.max(0.1)),
        Motion::Wave { period, angle_deg } => Mat4::from_rotation_z(angle_deg.to_radians() * (t * TAU / period.max(0.1)).sin()),
        Motion::Orbit { radius, period } => {
            let a = t * TAU / period.max(0.1);
            Mat4::from_translation(Vec3::new(a.cos() * radius - radius, 0.0, a.sin() * radius))
        }
    }
}

fn turn_towards(from: f32, to: f32, max_step: f32) -> f32 {
    let mut d = (to - from) % TAU;
    if d > PI {
        d -= TAU;
    } else if d < -PI {
        d += TAU;
    }
    if d.abs() <= max_step { to } else { from + d.signum() * max_step }
}

/// What a walking agent knows about the others during one step.
#[derive(Clone, Copy)]
struct Body {
    position: Vec2,
    velocity: Vec2,
    walking: bool,
    ghost: bool,
}

/// Outcome of [`steer`] for one step.
struct Steer {
    velocity: Vec2,
    /// The nearest agent ahead and the side to pass it on.
    passing: Option<(usize, f32)>,
    /// Met another agent head on and steps aside for it.
    start_yield: bool,
}

/// Velocity of walking agent `i` towards the next point of its route, with
/// local avoidance: separation from close agents, a lateral offset around
/// agents ahead on the path, yielding when two meet head on, and no motion
/// towards an agent already in contact.
fn steer(nav: Option<&NavGrid>, a: &Agent, i: usize, bodies: &[Body]) -> Steer {
    let p = bodies[i].position;
    let next = a.route.front().map_or(p, |q| xz(*q));
    let dir = (next - p).normalize_or_zero();
    let mut out = Steer { velocity: dir * WALK_SPEED, passing: None, start_yield: false };
    if a.ghost || dir == Vec2::ZERO {
        return out;
    }
    let left = Vec2::new(dir.y, -dir.x);
    let remaining = route_length(a.position, &a.route);
    let horizon = LOOKAHEAD / WALK_SPEED;
    let (mut forward, mut lateral, mut push) = (1.0f32, 0.0f32, Vec2::ZERO);
    let mut soonest = f32::INFINITY;
    for (j, b) in bodies.iter().enumerate() {
        if j == i {
            continue;
        }
        let rel = b.position - p;
        let d = rel.length();
        if d < SEPARATION {
            let away = if d > 0.01 {
                -rel / d
            } else if i < j {
                -left
            } else {
                left
            };
            // Near its own spot an agent may stand next to the neighbours of its slot.
            let fade = if b.walking { 1.0 } else { (remaining / SEPARATION).clamp(0.2, 1.0) };
            push += away * (SEPARATION - d) / SEPARATION * fade;
        }
        // Closest approach within the horizon if this agent walks straight on
        // and the other keeps its velocity. For a standing agent that is the
        // moment this one passes it.
        let ahead = rel.dot(dir);
        let closing = b.velocity - dir * WALK_SPEED;
        let t = if closing.length_squared() > 1.0 { (-rel.dot(closing) / closing.length_squared()).clamp(0.0, horizon) } else { 0.0 };
        let near = rel + closing * t;
        if ahead <= 0.0 || t <= 0.0 || near.length() >= SEPARATION || WALK_SPEED * t > remaining + AGENT_RADIUS {
            continue;
        }
        let offset = near.dot(left);
        let speed = b.velocity.length();
        let moving = b.walking && speed > 0.3 * WALK_SPEED;
        let head_on = moving && b.velocity.dot(dir) < -0.7 * speed;
        if head_on && !b.ghost && j < i && d < YIELD_RANGE && near.length() < 2.0 * AGENT_RADIUS && a.yield_cooldown <= 0.0 {
            out.start_yield = true;
        }
        let across = b.velocity.dot(left);
        let side = if head_on {
            // Meeting head on both keep right.
            -1.0
        } else if moving && across.abs() > 0.3 * speed {
            // On crossing paths the agent added first keeps its way and the
            // other passes behind it.
            if j > i {
                continue;
            }
            -across.signum()
        } else {
            // Agents that stand, walk slowly or walk the same way are passed on
            // the side away from them.
            match a.passing {
                Some((k, s)) if k == j => s,
                _ if offset.abs() > 4.0 => -offset.signum(),
                _ => free_side(nav, p, left),
            }
        };
        let urgency = (1.0 - t / horizon) * (1.0 - near.length() / SEPARATION);
        lateral += side * urgency * 1.5;
        forward = forward.min(((ahead - AGENT_RADIUS) / (LOOKAHEAD - AGENT_RADIUS)).clamp(0.3, 1.0));
        if t < soonest {
            soonest = t;
            out.passing = Some((j, side));
        }
    }
    let wish = if out.start_yield || a.yield_left > 0.0 {
        // Step aside to the right and let the other pass.
        -left * 0.25 + push * 1.5
    } else {
        dir * forward + left * lateral + push * 1.5
    };
    let mut v = (wish * WALK_SPEED).clamp_length_max(WALK_SPEED);
    for _ in 0..2 {
        for (j, b) in bodies.iter().enumerate() {
            let rel = b.position - p;
            let d = rel.length();
            if j != i && d < CONTACT && d > 0.01 {
                let n = rel / d;
                let towards = v.dot(n);
                if towards > 0.0 {
                    v -= n * towards;
                }
            }
        }
    }
    out.velocity = v;
    out
}

/// The side with room to pass, -1 for right unless only the left is free.
fn free_side(nav: Option<&NavGrid>, p: Vec2, left: Vec2) -> f32 {
    let free = |q: Vec2| nav.is_none_or(|nav| nav.is_free(Vec3::new(q.x, 0.0, q.y)));
    if free(p - left * SEPARATION) || !free(p + left * SEPARATION) { -1.0 } else { 1.0 }
}

/// Moves walking agent `a` (index `i`) one step along its steering velocity,
/// follows its route, plans again when it makes no progress and arrives.
fn walk(space: &Space, a: &mut Agent, i: usize, s: Steer, bodies: &[Body], dt: f32) {
    a.yield_cooldown = (a.yield_cooldown - dt).max(0.0);
    a.replan_cooldown = (a.replan_cooldown - dt).max(0.0);
    if s.start_yield {
        a.yield_left = YIELD_SECONDS;
        a.yield_cooldown = YIELD_COOLDOWN;
    }
    let yielding = a.yield_left > 0.0;
    a.yield_left = (a.yield_left - dt).max(0.0);
    a.passing = s.passing;
    a.trail_left = TRAIL_SECONDS;
    let nav = space.nav.as_ref().filter(|_| a.on_grid);
    let Some(&next) = a.route.front() else { return };
    let step = WALK_SPEED * dt;

    // The last step lands exactly on the spot.
    if a.route.len() == 1 && flat_distance(a.position, next) <= step {
        a.arrive(next);
        return;
    }

    let from = xz(a.position);
    let mut to = from + s.velocity * dt;
    // Never step into a blocked cell, slide along it instead. An agent that
    // stands in a blocked cell already walks straight out of it.
    let free = |nav: &NavGrid, q: Vec2| nav.is_free(Vec3::new(q.x, 0.0, q.y));
    if let Some(nav) = nav
        && free(nav, from)
        && !free(nav, to)
    {
        let d = to - from;
        let (x, z) = (Vec2::new(d.x, 0.0), Vec2::new(0.0, d.y));
        let tries = if d.x.abs() >= d.y.abs() { [x, z] } else { [z, x] };
        to = tries.into_iter().map(|t| from + t).find(|q| free(nav, *q)).unwrap_or(from);
    }
    let moved = to - from;
    a.position.x = to.x;
    a.position.z = to.y;
    match nav.and_then(|nav| nav.floor_y(a.position)) {
        Some(ground) => a.position.y += (ground - a.position.y) * (1.0 - (-dt * 20.0).exp()),
        None => a.position.y = next.y,
    }
    a.velocity = Vec3::new(moved.x, 0.0, moved.y) / dt;
    let distance = moved.length();
    if distance > 0.3 * step {
        a.heading = turn_towards(a.heading, moved.x.atan2(moved.y), dt * 8.0);
    }
    // The hop follows the distance walked and settles on the ground when the agent stops.
    a.walk_phase =
        if distance > 0.05 * step { a.walk_phase + distance / 22.0 } else { (a.walk_phase + step / 22.0).min(a.walk_phase.ceil()) };
    if a.trail.last().is_none_or(|p| p.distance(a.position) > 12.0) {
        a.trail.push(a.position);
    }

    // Pass route points that are reached or that the agent can see past.
    while a.route.len() > 1 {
        let w = a.route[0];
        let occupied = bodies.iter().enumerate().any(|(j, b)| j != i && !b.walking && b.position.distance(xz(w)) < 2.0 * AGENT_RADIUS);
        let reach = if occupied {
            CONTACT + 8.0
        } else if a.on_grid {
            step.max(1.0)
        } else {
            AGENT_RADIUS
        };
        if flat_distance(a.position, w) <= reach || nav.is_some_and(|nav| nav.line_free(a.position, a.route[1])) {
            a.route.pop_front();
        } else {
            break;
        }
    }
    // Pushed out of sight of the next point, for example around a corner.
    if let Some(nav) = nav
        && let Some(&w) = a.route.front()
        && a.replan_cooldown <= 0.0
        && nav.is_free(a.position)
        && nav.is_free(w)
        && !nav.line_free(a.position, w)
    {
        a.replan_cooldown = 0.5;
        replan(space, a, &[]);
    }

    let left = route_length(a.position, &a.route);
    if left < a.best_left - 1.0 {
        a.best_left = left;
        a.stall = 0.0;
    } else if !yielding {
        a.stall += dt;
    }
    if a.stall > STALL_SECONDS {
        a.stall = 0.0;
        a.replans += 1;
        if a.replans > MAX_REPLANS {
            tracing::debug!("{} is stuck and walks through the others", a.id);
            a.ghost = true;
        } else {
            // First around the agents standing in the way, then around everyone.
            let others: Vec<Vec3> = bodies
                .iter()
                .enumerate()
                .filter(|(j, b)| *j != i && (a.replans > 1 || !b.walking))
                .map(|(_, b)| Vec3::new(b.position.x, 0.0, b.position.y))
                .collect();
            replan(space, a, &others);
        }
    }
}

/// A new route to the current target from where the agent is now. Progress
/// counts from the new route on.
fn replan(space: &Space, a: &mut Agent, others: &[Vec3]) {
    let Some(t) = &a.target else { return };
    let route = space.plan(a.position, t.position, &t.station, others);
    a.route = route.points;
    a.on_grid = route.on_grid;
    a.best_left = route_length(a.position, &a.route);
}

/// Moves standing agents apart that overlap, for example after one walked
/// through the others or where two spots lie too close. Agents only move
/// into free cells.
fn nudge_standing(nav: Option<&NavGrid>, agents: &mut [Agent], dt: f32) {
    let min = 2.0 * AGENT_RADIUS;
    for i in 0..agents.len() {
        for j in i + 1..agents.len() {
            if agents[i].is_walking() || agents[j].is_walking() {
                continue;
            }
            let rel = xz(agents[i].position) - xz(agents[j].position);
            let d = rel.length();
            if d >= min - 0.5 {
                continue;
            }
            let dir = if d > 0.01 { rel / d } else { Vec2::from_angle((i * 7 + j * 3) as f32) };
            let amount = ((min - d) * 0.5).min(NUDGE_SPEED * dt);
            let moved_i = shift(nav, &mut agents[i], dir * amount);
            let moved_j = shift(nav, &mut agents[j], -dir * amount);
            // Against a wall the other one moves twice as far.
            if !moved_i {
                shift(nav, &mut agents[j], -dir * amount);
            } else if !moved_j {
                shift(nav, &mut agents[i], dir * amount);
            }
        }
    }
}

/// Moves a standing agent unless that takes it into a blocked cell.
fn shift(nav: Option<&NavGrid>, a: &mut Agent, by: Vec2) -> bool {
    let to = a.position + Vec3::new(by.x, 0.0, by.y);
    if nav.is_some_and(|nav| nav.is_free(a.position) && !nav.is_free(to)) {
        return false;
    }
    a.position = to;
    true
}

/// Length of the walk from `p` through every point of `route`, on the ground.
fn route_length(p: Vec3, route: &VecDeque<Vec3>) -> f32 {
    let mut at = p;
    let mut length = 0.0;
    for &q in route {
        length += flat_distance(at, q);
        at = q;
    }
    length
}

fn flat_distance(a: Vec3, b: Vec3) -> f32 {
    xz(a).distance(xz(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nav::tests::{push_box, slab};
    use meerora_script::CameraHint;

    fn space() -> SpaceDesc {
        let st = |id: &str, x: f32, front: bool| {
            let tools = if id == "b" { vec!["terminal".to_string()] } else { Vec::new() };
            Station { id: id.into(), position: Vec3::new(x, 0.0, 0.0), facing_deg: 0.0, front, tools }
        };
        SpaceDesc {
            mesh: Default::default(),
            walls: Vec::new(),
            lights: vec![],
            ambient_sky: [0.0; 3],
            ambient_ground: [0.0; 3],
            sun_direction: Vec3::NEG_Y,
            sun_color: [1.0; 3],
            sun_intensity: 1.0,
            background: [0.0; 3],
            stations: vec![st("a", 0.0, false), st("b", 400.0, false), st("front", 200.0, true)],
            waypoints: vec![
                ("a".into(), Vec3::new(0.0, 0.0, 0.0)),
                ("b".into(), Vec3::new(400.0, 0.0, 0.0)),
                ("front".into(), Vec3::new(200.0, 0.0, 0.0)),
                ("mid".into(), Vec3::new(200.0, 0.0, 100.0)),
            ],
            edges: vec![("a".into(), "mid".into()), ("mid".into(), "b".into()), ("mid".into(), "front".into())],
            camera: CameraHint { target: Vec3::ZERO, view_height: 500.0, azimuth_deg: 45.0, elevation_deg: 30.0 },
            graveyard: None,
            triangles: 0,
            budget: 0,
            bounds: Aabb::EMPTY,
        }
    }

    /// A walled room of `w` by `d` cm with a floor slab, boxes as furniture
    /// and stations given as id, x and z. Every station is a graph node, there
    /// are no graph edges.
    fn room(w: f32, d: f32, boxes: &[(Vec3, Vec3)], stations: &[(&str, f32, f32)]) -> SpaceDesc {
        let mut mesh = slab(w, d);
        for (lo, hi) in boxes {
            push_box(&mut mesh, *lo, *hi);
        }
        let stations: Vec<Station> = stations
            .iter()
            .map(|(id, x, z)| Station { id: (*id).into(), position: Vec3::new(*x, 0.0, *z), facing_deg: 0.0, front: false, tools: vec![] })
            .collect();
        SpaceDesc {
            bounds: mesh.bounds(),
            mesh,
            walls: Vec::new(),
            waypoints: stations.iter().map(|s| (s.id.clone(), s.position)).collect(),
            edges: vec![],
            stations,
            ..space()
        }
    }

    fn agent(id: &str, home: &str) -> Agent {
        let dir = meerora_protocol::AgentDir { id: id.into(), path: Default::default(), config: toml_config(home) };
        Agent::new(&dir)
    }

    fn toml_config(home: &str) -> meerora_protocol::AgentConfig {
        meerora_protocol::AgentConfig {
            name: "X".into(),
            color: "#FFFFFF".into(),
            figure: "figure.js".into(),
            home: home.into(),
            instructions: None,
            on_done: Default::default(),
            apps: Vec::new(),
            workspace: Vec::new(),
            runtime: None,
            schedule: vec![],
            budget: None,
            capabilities: Default::default(),
            external: None,
            role: None,
        }
    }

    fn world() -> World {
        let mut w = World::new();
        w.set_space(Space::new(space(), MeshId(0), Vec::new()));
        w.add_agent(agent("x", "a"));
        w.add_agent(agent("y", "b"));
        w
    }

    fn world_in(desc: SpaceDesc, agents: &[(&str, &str)]) -> World {
        let mut w = World::new();
        w.set_space(Space::new(desc, MeshId(0), Vec::new()));
        for (id, home) in agents {
            w.add_agent(agent(id, home));
        }
        w
    }

    fn go(w: &mut World, id: &str, station: &str) {
        w.apply(&Event::GoTo { agent: id.into(), station: station.into() });
    }

    /// The rooms that ship with meerora (D6). Every test of the real space runs in each.
    const ROOMS: [&str; 3] = ["workshop", "space-station", "office"];

    fn real_space(room: &str) -> Space {
        let path = format!("{}/../../assets/spaces/{room}/space.js", env!("CARGO_MANIFEST_DIR"));
        let source = std::fs::read_to_string(&path).unwrap();
        let desc = meerora_script::build_space(&source, &format!("{room}/space.js"), &meerora_script::Limits::default())
            .unwrap_or_else(|e| panic!("{room}: {e}"));
        Space::new(desc, MeshId(0), Vec::new())
    }

    #[test]
    fn route_goes_through_the_graph() {
        let w = world();
        let r = w.space.as_ref().unwrap().graph.route(Vec3::ZERO, "b");
        assert_eq!(r, vec![Vec3::ZERO, Vec3::new(200.0, 0.0, 100.0), Vec3::new(400.0, 0.0, 0.0)]);
    }

    #[test]
    fn agents_walk_to_assigned_station_and_arrive() {
        let mut w = world();
        w.apply(&Event::GoTo { agent: "x".into(), station: "b".into() });
        assert!(w.agents[0].is_walking());
        for _ in 0..600 {
            w.update(1.0 / 60.0);
        }
        assert!(!w.agents[0].is_walking());
        // y lives at b, so x takes the next slot beside it.
        assert!(w.agents[0].position.distance(Vec3::new(400.0 + SLOT_SPACING, 0.0, 0.0)) < 0.01);
        assert!(w.agents[1].position.distance(Vec3::new(400.0, 0.0, 0.0)) < 0.01);
    }

    #[test]
    fn waiting_agents_gather_at_the_front_side_by_side() {
        let mut w = world();
        let first = w.apply(&Event::Ring { agent: "x".into(), state: RingState::NeedsYou, progress: None });
        assert_eq!(first.as_deref(), Some("x"));
        w.apply(&Event::Ring { agent: "y".into(), state: RingState::NeedsYou, progress: None });
        for _ in 0..900 {
            w.update(1.0 / 60.0);
        }
        let (px, py) = (w.agents[0].position, w.agents[1].position);
        assert!((px.distance(py) - SLOT_SPACING).abs() < 0.1);
        assert!(((px + py) * 0.5).distance(Vec3::new(200.0, 0.0, 0.0)) < 0.1);
        // Back to work: the agent returns to its assigned station or home.
        w.apply(&Event::Ring { agent: "x".into(), state: RingState::Working, progress: None });
        for _ in 0..900 {
            w.update(1.0 / 60.0);
        }
        assert!(w.agents[0].position.distance(Vec3::ZERO) < 0.01);
        assert!(w.agents[1].position.distance(Vec3::new(200.0, 0.0, 0.0)) < 0.1);
    }

    #[test]
    fn working_agents_walk_to_the_station_of_their_tool() {
        let mut w = world();
        w.apply(&Event::Action { agent: "x".into(), tool: "terminal.run".into(), summary: "make".into(), target: None });
        assert!(!w.agents[0].is_walking(), "a resting agent stays put");
        w.apply(&Event::Ring { agent: "x".into(), state: RingState::Working, progress: None });
        w.apply(&Event::Action { agent: "x".into(), tool: "terminal.run".into(), summary: "make".into(), target: None });
        assert_eq!(w.agents[0].station(), Some("b"));
        w.apply(&Event::Action { agent: "x".into(), tool: "mail.read".into(), summary: "inbox".into(), target: None });
        assert_eq!(w.agents[0].station(), Some("b"), "no station for mail, so it stays");
    }

    #[test]
    fn actions_keep_their_target_and_runs_their_start() {
        let mut w = world();
        let url = "https://example.org/a/b/c?q=1";
        w.apply(&Event::Action { agent: "x".into(), tool: "browser.open".into(), summary: "https://…".into(), target: Some(url.into()) });
        w.apply(&Event::Action { agent: "x".into(), tool: "terminal.run".into(), summary: "make".into(), target: None });
        let targets: Vec<&str> = w.agents[0].actions.iter().map(ActionEntry::target).collect();
        assert_eq!(targets, [url, "make"], "the summary stands in for a missing target");
        assert_eq!(w.agents[0].actions[0].summary, "https://…", "the log keeps the short line");

        let start = |started| Event::RunStarted { agent: "x".into(), run: "r1".into(), trigger: Trigger::User, task: "t".into(), started };
        w.apply(&start(1_791_991_823_000));
        assert_eq!(w.agents[0].run.as_ref().map(|r| r.started), Some(1_791_991_823_000));
        w.apply(&start(0));
        assert_eq!(w.agents[0].run.as_ref().map(|r| r.started), Some(0), "unknown, the app picks a time itself");
    }

    #[test]
    fn the_wake_countdown_runs_from_full_to_empty() {
        assert_eq!(wake_left(None, 5), None);
        assert_eq!(wake_left(Some((61_000, 1_000)), 1_000), Some(1.0));
        assert_eq!(wake_left(Some((61_000, 1_000)), 31_000), Some(0.5));
        assert_eq!(wake_left(Some((61_000, 1_000)), 70_000), Some(0.0));
        assert_eq!(wake_left(Some((61_000, 61_000)), 0), Some(1.0), "no division by zero");
    }

    #[test]
    fn hop_motion_returns_to_the_ground() {
        let m = Motion::Hop { count: 2, height: 10.0, period: 2.0 };
        assert_eq!(motion_matrix(&m, 0.0).transform_point3(Vec3::ZERO).y, 0.0);
        assert!(motion_matrix(&m, 0.16).transform_point3(Vec3::ZERO).y > 9.0);
        assert_eq!(motion_matrix(&m, 1.5).transform_point3(Vec3::ZERO).y, 0.0);
    }

    #[test]
    fn agents_walk_around_furniture() {
        let table = (Vec3::new(250.0, 0.0, 150.0), Vec3::new(350.0, 75.0, 350.0));
        let mut w = world_in(room(600.0, 500.0, &[table], &[("w", 100.0, 250.0), ("e", 500.0, 250.0)]), &[("x", "w")]);
        go(&mut w, "x", "e");
        assert!(w.agents[0].on_grid);
        let nav = w.space.as_ref().unwrap().nav.as_ref().unwrap();
        assert!(!nav.is_free(Vec3::new(300.0, 0.0, 250.0)));
        for _ in 0..600 {
            w.update(1.0 / 60.0);
            let p = w.agents[0].position;
            assert!(
                !(table.0.x - 5.0..table.1.x + 5.0).contains(&p.x) || !(table.0.z - 5.0..table.1.z + 5.0).contains(&p.z),
                "walked into the table at {p}"
            );
        }
        assert!(w.agents[0].position.distance(Vec3::new(500.0, 0.0, 250.0)) < 0.01);
    }

    #[test]
    fn a_station_inside_an_obstacle_moves_to_a_free_cell() {
        let desk = (Vec3::new(200.0, 0.0, 100.0), Vec3::new(320.0, 75.0, 180.0));
        let space = Space::new(room(500.0, 400.0, &[desk], &[("desk", 260.0, 190.0), ("door", 100.0, 300.0)]), MeshId(0), Vec::new());
        let nav = space.nav.as_ref().unwrap();
        let (station, spot) = (space.desc.stations[0].position, space.spots[0]);
        assert!(!nav.is_free(station));
        assert!(nav.is_free(spot));
        assert!(spot.distance(station) < 30.0 && spot.z > station.z, "moved from {station} to {spot}");
        assert_eq!(space.spots[1], space.desc.stations[1].position, "a free station stays where it is");

        let mut w = World::new();
        w.set_space(space);
        w.add_agent(agent("x", "desk"));
        assert_eq!(w.agents[0].position, spot);
    }

    #[test]
    fn agents_sharing_a_home_stand_apart() {
        let desc = room(600.0, 400.0, &[], &[("bench", 300.0, 200.0)]);
        let mut w = world_in(desc, &[("x", "bench"), ("y", "bench"), ("z", "bench")]);
        for _ in 0..120 {
            w.update(1.0 / 60.0);
        }
        let nav = w.space.as_ref().unwrap().nav.as_ref().unwrap();
        for (i, a) in w.agents.iter().enumerate() {
            assert!(nav.is_free(a.position));
            assert!(!a.is_walking());
            for b in &w.agents[i + 1..] {
                assert!(a.position.distance(b.position) >= 2.0 * AGENT_RADIUS, "{} and {} overlap", a.id, b.id);
            }
        }
        // Without a grid, too.
        let mut w = world();
        w.add_agent(agent("z", "a"));
        assert!(w.agents[0].position.distance(w.agents[2].position) >= 2.0 * AGENT_RADIUS);
    }

    #[test]
    fn a_freed_slot_goes_to_the_next_arrival() {
        let desc = room(800.0, 400.0, &[], &[("bench", 300.0, 200.0), ("tree", 650.0, 200.0)]);
        let mut w = world_in(desc, &[("x", "bench"), ("y", "bench"), ("z", "tree")]);
        let y_at = w.agents[1].position;
        go(&mut w, "x", "tree");
        assert_eq!(w.agents[1].position, y_at, "y keeps its slot");
        go(&mut w, "z", "bench");
        for _ in 0..900 {
            w.update(1.0 / 60.0);
        }
        assert_eq!(w.agents[1].position, y_at);
        assert!(w.agents[2].position.distance(Vec3::new(300.0, 0.0, 200.0)) < 0.01, "z takes the slot x left");
    }

    /// Simulates two agents walking to other stations at 60 Hz. Returns the
    /// smallest distance between them and whether one of them yielded.
    fn closest_approach(desc: SpaceDesc, x: (&str, &str), y: (&str, &str)) -> (f32, bool) {
        let mut w = world_in(desc, &[("x", x.0), ("y", y.0)]);
        go(&mut w, "x", x.1);
        go(&mut w, "y", y.1);
        let mut closest = f32::INFINITY;
        let mut yielded = false;
        for _ in 0..60 * 20 {
            w.update(1.0 / 60.0);
            closest = closest.min(flat_distance(w.agents[0].position, w.agents[1].position));
            yielded |= w.agents.iter().any(|a| a.yield_left > 0.0);
        }
        for (a, station) in w.agents.iter().zip([x.1, y.1]) {
            assert!(!a.is_walking(), "{} did not arrive", a.id);
            assert!(!a.ghost);
            let space = w.space.as_ref().unwrap();
            let spot = space.desc.stations[space.station_index(station).unwrap()].position;
            assert!(a.position.distance(spot) < 0.01, "{} stands at {} instead of {spot}", a.id, a.position);
        }
        (closest, yielded)
    }

    #[test]
    fn crossing_agents_keep_their_distance() {
        let stations = [("w", 150.0, 400.0), ("e", 850.0, 400.0), ("n", 500.0, 50.0), ("s", 500.0, 750.0)];
        let (head_on, yielded) = closest_approach(room(1000.0, 800.0, &[], &stations), ("w", "e"), ("e", "w"));
        assert!(head_on >= 1.6 * AGENT_RADIUS, "head on: {head_on}");
        assert!(yielded, "meeting head on, one of them waits");
        let (across, _) = closest_approach(room(1000.0, 800.0, &[], &stations), ("w", "e"), ("n", "s"));
        assert!(across >= 1.6 * AGENT_RADIUS, "across: {across}");
        // Head on in a corridor 90 cm wide between two long tables, which
        // leaves a free band of 46 cm for the agents' centres.
        let tables =
            [(Vec3::new(250.0, 0.0, 120.0), Vec3::new(750.0, 75.0, 355.0)), (Vec3::new(250.0, 0.0, 445.0), Vec3::new(750.0, 75.0, 680.0))];
        let (corridor, _) = closest_approach(room(1000.0, 800.0, &tables, &stations), ("w", "e"), ("e", "w"));
        assert!(corridor >= 1.6 * AGENT_RADIUS, "corridor: {corridor}");
    }

    #[test]
    fn a_boxed_in_agent_still_arrives() {
        let desc = room(1000.0, 800.0, &[], &[("a", 300.0, 400.0), ("b", 800.0, 400.0), ("c", 300.0, 100.0)]);
        let mut w = world_in(desc, &[("x", "a"), ("n", "c"), ("e", "c"), ("s", "c"), ("w", "c")]);
        let centre = w.agents[0].position;
        go(&mut w, "x", "b");
        // Four agents stand around x, touching it.
        for (k, offset) in [Vec3::Z, Vec3::X, Vec3::NEG_Z, Vec3::NEG_X].into_iter().enumerate() {
            w.agents[k + 1].position = centre + offset * 2.0 * AGENT_RADIUS;
        }
        let mut seconds = 0.0;
        while w.agents[0].is_walking() && seconds < 30.0 {
            w.update(1.0 / 60.0);
            seconds += 1.0 / 60.0;
        }
        assert!(!w.agents[0].is_walking(), "still boxed in after {seconds} s");
        assert!(w.agents[0].position.distance(Vec3::new(800.0, 0.0, 400.0)) < 0.01);
        // The others stayed where they were, apart from each other.
        for a in &w.agents[1..] {
            assert!(!a.is_walking());
            assert!(a.position.distance(centre) < 2.0 * AGENT_RADIUS + 1.0);
        }
    }

    #[test]
    fn the_real_spaces_connect_every_station() {
        for room in ROOMS {
            let space = real_space(room);
            let nav = space.nav.as_ref().expect("the space has a grid");
            for (station, spot) in space.desc.stations.iter().zip(&space.spots) {
                let moved = flat_distance(station.position, *spot);
                assert!(moved <= 30.0, "{room}: station {} moved {moved:.0} cm", station.id);
                assert!(nav.is_free(*spot), "{room}: station {} is blocked", station.id);
            }
            for (i, a) in space.spots.iter().enumerate() {
                for (j, b) in space.spots.iter().enumerate() {
                    if i == j {
                        continue;
                    }
                    let (from, to) = (&space.desc.stations[i].id, &space.desc.stations[j].id);
                    let path = nav.find_path(*a, *b).unwrap_or_else(|| panic!("{room}: no path from {from} to {to}"));
                    let mut at = *a;
                    for &p in &path {
                        assert!(nav.line_free(at, p), "{room}: path from {from} to {to} crosses a blocked cell");
                        at = p;
                    }
                }
            }
            // Every station keeps room for a second agent, the front for a row of waiting ones (D3).
            for (station, slots) in space.desc.stations.iter().zip(&space.slots) {
                assert!(slots.len() >= 2, "{room}: station {} has no room for two", station.id);
            }
            let front = space.front_index().expect("a front station");
            let row = space.front_row(front, 4);
            assert!(row.iter().all(|p| nav.is_free(*p)), "{room}: four waiting agents fit at the front");
        }
    }

    /// The direction towards a viewer at `azimuth` degrees, 30 degrees up.
    fn toward(azimuth: f32) -> Vec3 {
        let (az, el) = (azimuth.to_radians(), 30f32.to_radians());
        Vec3::new(az.sin() * el.cos(), el.sin(), az.cos() * el.cos())
    }

    /// How far apart two points stand across the view from `azimuth`.
    fn across(a: Vec3, b: Vec3, azimuth: f32) -> f32 {
        let az = azimuth.to_radians();
        (xz(a) - xz(b)).dot(Vec2::new(az.cos(), -az.sin())).abs()
    }

    #[test]
    fn agents_at_a_station_stand_side_by_side_as_seen_from_the_camera() {
        // The bench's side axis is x. Seen from azimuth 90 or 270 it runs
        // along the view, where the old order put the second agent behind the first.
        let desc = room(600.0, 400.0, &[], &[("bench", 300.0, 200.0)]);
        for azimuth in [0.0, 45.0, 90.0, 135.0, 270.0] {
            let mut w = World::new();
            w.set_space(Space::new(desc.clone(), MeshId(0), Vec::new()));
            w.viewer = Some(toward(azimuth));
            for id in ["x", "y", "z"] {
                w.add_agent(agent(id, "bench"));
            }
            let p: Vec<Vec3> = w.agents.iter().map(|a| a.position).collect();
            assert_eq!(p[0], Vec3::new(300.0, 0.0, 200.0), "azimuth {azimuth}: the first stands on the spot");
            for (i, j) in [(0, 1), (0, 2), (1, 2)] {
                let apart = across(p[i], p[j], azimuth);
                assert!(
                    apart >= MIN_ACROSS,
                    "azimuth {azimuth}: {} and {} cover each other ({apart:.0} cm across)",
                    w.agents[i].id,
                    w.agents[j].id
                );
                assert!(flat_distance(p[i], p[j]) <= 2.0 * SLOT_SPACING + 0.1, "azimuth {azimuth}: they stay close together");
            }
        }
    }

    #[test]
    fn turning_the_view_moves_nobody_and_arrivals_follow_it() {
        let desc = room(800.0, 400.0, &[], &[("bench", 300.0, 200.0), ("tree", 650.0, 200.0)]);
        let mut w = World::new();
        w.set_space(Space::new(desc, MeshId(0), Vec::new()));
        w.viewer = Some(toward(0.0));
        w.add_agent(agent("x", "bench"));
        w.add_agent(agent("y", "bench"));
        w.add_agent(agent("z", "tree"));
        let (x_at, y_at) = (w.agents[0].position, w.agents[1].position);
        assert!(across(x_at, y_at, 0.0) >= MIN_ACROSS);
        // The camera turns until x and y stand behind each other. They stay.
        w.viewer = Some(toward(90.0));
        w.add_agent(agent("v", "bench"));
        go(&mut w, "z", "bench");
        for _ in 0..900 {
            w.update(1.0 / 60.0);
        }
        assert_eq!((w.agents[0].position, w.agents[1].position), (x_at, y_at), "standing agents keep their slot");
        // The walking agent and the new arrival take slots the turned view sees apart.
        let p: Vec<Vec3> = w.agents.iter().map(|a| a.position).collect();
        assert!(!w.agents[2].is_walking());
        for k in [2, 3] {
            for j in (0..4).filter(|j| *j != k) {
                let apart = across(p[k], p[j], 90.0);
                assert!(apart >= MIN_ACROSS, "{} and {} cover each other ({apart:.0} cm across)", w.agents[k].id, w.agents[j].id);
            }
        }
    }

    #[test]
    fn every_real_station_seats_a_second_agent_apart_from_any_side() {
        for room in ROOMS {
            let space = real_space(room);
            for (station, slots) in space.desc.stations.iter().zip(&space.slots) {
                for step in 0..24 {
                    let azimuth = step as f32 * 15.0;
                    let az = azimuth.to_radians();
                    let right = Some(Vec2::new(az.cos(), -az.sin()));
                    let k = pick_slot(slots, 1..slots.len(), &[slots[0]], right, &|_| false).expect("a second slot");
                    let best = slots[1..].iter().map(|p| across(*p, slots[0], azimuth)).fold(0.0, f32::max);
                    let got = across(slots[k], slots[0], azimuth);
                    assert!(
                        got >= MIN_ACROSS || best < MIN_ACROSS,
                        "{room}: at {} from azimuth {azimuth} the second agent stands {got:.0} cm across, {best:.0} was possible",
                        station.id
                    );
                }
            }
        }
    }

    #[test]
    fn arrivals_avoid_slots_behind_the_furniture() {
        // A shelf stands west of the bench's back row. Seen from the west it
        // hides that row, where the old order put the second agent, so it
        // stands in the front row, beside the first as the view sees it.
        let shelf = (Vec3::new(200.0, 0.0, 120.0), Vec3::new(240.0, 200.0, 170.0));
        let desc = room(600.0, 400.0, &[shelf], &[("bench", 300.0, 200.0)]);
        let mut w = World::new();
        w.set_space(Space::new(desc, MeshId(0), Vec::new()));
        w.viewer = Some(toward(270.0));
        let nav = w.space.as_ref().unwrap().nav.as_ref().unwrap();
        let west = Vec2::new(-1.0, 0.0);
        assert!(hidden_from(nav, Vec3::new(300.0, 0.0, 152.0), west), "the shelf hides the back row");
        assert!(!hidden_from(nav, Vec3::new(300.0, 0.0, 200.0), west) && !hidden_from(nav, Vec3::new(300.0, 0.0, 248.0), west));
        w.add_agent(agent("x", "bench"));
        w.add_agent(agent("y", "bench"));
        let (x, y) = (w.agents[0].position, w.agents[1].position);
        let nav = w.space.as_ref().unwrap().nav.as_ref().unwrap();
        assert_eq!(x, Vec3::new(300.0, 0.0, 200.0));
        assert!(!hidden_from(nav, y, west), "y stands at {y}, behind the shelf");
        assert!(across(x, y, 270.0) >= MIN_ACROSS, "y stands at {y}, behind x");
    }

    #[test]
    fn arrivals_are_reported_once() {
        let mut w = world();
        w.update(0.1);
        w.arrivals.clear();
        go(&mut w, "x", "b");
        for _ in 0..600 {
            w.update(1.0 / 30.0);
        }
        assert_eq!(w.arrivals, [("x".to_string(), "b".to_string())]);
        // A GoTo to where it stands arrives at once.
        w.arrivals.clear();
        go(&mut w, "x", "b");
        assert_eq!(w.arrivals, [("x".to_string(), "b".to_string())]);
    }

    #[test]
    fn agents_in_the_real_space_never_enter_blocked_cells() {
        for room in ROOMS {
            walk_the_real_space(room);
        }
    }

    fn walk_the_real_space(room: &str) {
        let mut w = World::new();
        w.set_space(real_space(room));
        let homes = ["door", "window", "bookshelf", "router_cabinet", "workbench", "desk", "mailbox", "garden_bench"];
        for (k, home) in homes.iter().enumerate() {
            w.add_agent(agent(&format!("a{k}"), home));
        }
        let stations: Vec<String> = w.space.as_ref().unwrap().desc.stations.iter().map(|s| s.id.clone()).collect();
        for round in 0..4 {
            for k in 0..homes.len() {
                let to = &stations[(k * 3 + round * 5 + 1) % stations.len()];
                go(&mut w, &format!("a{k}"), to);
            }
            // The rooms are large, the longest walks take about 12 s.
            for _ in 0..60 * 30 {
                let before: Vec<bool> = {
                    let nav = w.space.as_ref().unwrap().nav.as_ref().unwrap();
                    w.agents.iter().map(|a| nav.is_free(a.position)).collect()
                };
                w.update(1.0 / 60.0);
                let nav = w.space.as_ref().unwrap().nav.as_ref().unwrap();
                for (a, was_free) in w.agents.iter().zip(before) {
                    assert!(
                        !was_free || nav.is_free(a.position) || !a.on_grid,
                        "{room}: {} entered a blocked cell at {}",
                        a.id,
                        a.position
                    );
                }
            }
            for a in &w.agents {
                assert!(!a.is_walking(), "{room}: {} did not arrive in round {round}", a.id);
            }
            for (i, a) in w.agents.iter().enumerate() {
                for b in &w.agents[i + 1..] {
                    let d = flat_distance(a.position, b.position);
                    assert!(d >= 2.0 * AGENT_RADIUS - 1.0, "{room}: {} and {} overlap ({d:.0} cm) in round {round}", a.id, b.id);
                }
            }
        }
    }

    /// Timings of the grid in the real space. Run with
    /// `cargo test -p meerora nav_timings -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn nav_timings() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/spaces/workshop/space.js");
        let source = std::fs::read_to_string(path).unwrap();
        let desc = meerora_script::build_space(&source, "space.js", &meerora_script::Limits::default()).unwrap();
        let runs = 20;
        let started = std::time::Instant::now();
        for _ in 0..runs {
            std::hint::black_box(NavGrid::build(&desc.mesh, desc.bounds));
        }
        let build = started.elapsed().as_secs_f64() * 1000.0 / runs as f64;
        let nav = NavGrid::build(&desc.mesh, desc.bounds).unwrap();
        let spots: Vec<Vec3> = desc.stations.iter().map(|s| nav.nearest_free(s.position).unwrap()).collect();
        let started = std::time::Instant::now();
        let mut queries = 0;
        let mut slowest = 0.0f64;
        for a in &spots {
            for b in &spots {
                let t = std::time::Instant::now();
                std::hint::black_box(nav.find_path(*a, *b));
                slowest = slowest.max(t.elapsed().as_secs_f64() * 1000.0);
                queries += 1;
            }
        }
        let query = started.elapsed().as_secs_f64() * 1000.0 / queries as f64;
        let (w, d) = nav.size();
        println!(
            "{} triangles, grid {w}x{d}: build {build:.2} ms, path query {query:.3} ms on average, {slowest:.3} ms at most ({queries} queries)",
            desc.mesh.triangle_count()
        );
    }
}
