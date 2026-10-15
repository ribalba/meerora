//! The graveyard in the space: one small tombstone for each retired agent,
//! in rows on the plot that the space script declares.
//!
//! The daemon sends the retired agents ([`Event::Graveyard`]). The app builds
//! a rounded stone with a band in the agent's colour for each of them from
//! meerora-geom primitives, like its other meshes, and stands them on the
//! plot. The plot is `graveyard: { station, gap, width, depth }` of the space
//! script ([`meerora_script::GraveyardPlot`]): it starts `gap` in front of
//! the station in the direction the station faces, and the script puts its
//! fence around the same rectangle. A space without the declaration, such as
//! an older user `space.js`, has no plot. Then the tombstones stay out and
//! the graveyard view still lists every agent.
//!
//! Picking works like devices: the pointer hits the box of a stone, and the
//! nearest of stone, device and agent wins.

use meerora_geom::{Aabb, Mat4, Material, Mesh, Quat, Vec3, prim};
use meerora_protocol::{Event, RetiredAgent};
use meerora_scene::{Draw, Frame, MeshId, Renderer};
use meerora_script::SpaceDesc;

/// Size of a tombstone: width, the height of its straight part and thickness.
/// The rounded top adds half the width.
const STONE_W: f32 = 20.0;
const BODY_H: f32 = 24.0;
const STONE_D: f32 = 6.0;
/// The plinth under the stone, wider and deeper than the stone.
const PLINTH: (f32, f32, f32) = (STONE_W + 6.0, 3.0, STONE_D + 8.0);
/// Distance between the centres of neighbouring stones across and along the plot.
const COLUMN: f32 = 30.0;
const ROW: f32 = 34.0;
/// Room kept free along the edges of the plot.
const EDGE: f32 = 4.0;
/// Small turns of each stone in degrees, so the rows look laid by hand.
const TURNS: [f32; 6] = [-3.0, 2.5, 4.0, -2.0, 1.0, -4.0];

/// The rectangle where tombstones stand.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Plot {
    /// The middle of the edge next to the station, on the floor.
    pub near: Vec3,
    /// Unit vector on the floor from the station into the plot.
    pub forward: Vec3,
    /// The station's facing in degrees around y, 0 = +z.
    pub facing_deg: f32,
    pub width: f32,
    pub depth: f32,
}

/// Where one tombstone stands and which way its face looks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub position: Vec3,
    pub facing_deg: f32,
}

impl Plot {
    /// The plot the space script declares, `None` without one.
    pub fn of(desc: &SpaceDesc) -> Option<Plot> {
        let plot = desc.graveyard.as_ref()?;
        let station = desc.stations.iter().find(|s| s.id == plot.station)?;
        let forward = Quat::from_rotation_y(station.facing_deg.to_radians()) * Vec3::Z;
        Some(Plot {
            near: Vec3::new(station.position.x, 0.0, station.position.z) + forward * plot.gap,
            forward,
            facing_deg: station.facing_deg,
            width: plot.width,
            depth: plot.depth,
        })
    }

    /// Unit vector across the plot.
    fn side(&self) -> Vec3 {
        Vec3::new(self.forward.z, 0.0, -self.forward.x)
    }

    /// How many stones fit across and along the plot.
    pub fn grid(&self) -> (usize, usize) {
        let fit = |room: f32, size: f32, step: f32| -> usize {
            let room = room - 2.0 * EDGE;
            if room < size { 0 } else { 1 + ((room - size) / step).floor() as usize }
        };
        (fit(self.width, PLINTH.0, COLUMN), fit(self.depth, PLINTH.2, ROW))
    }

    pub fn capacity(&self) -> usize {
        let (columns, rows) = self.grid();
        columns * rows
    }
}

/// Where the first `n` tombstones stand: in rows across the plot, filled
/// from the far end, so the fence and the gate next to the station hide
/// none of the first stones. Never more than the plot holds.
pub fn layout(plot: &Plot, n: usize) -> Vec<Placement> {
    let (columns, rows) = plot.grid();
    let side = plot.side();
    // The grid sits in the middle of the plot, its first row furthest from the station.
    let span = (rows.max(1) - 1) as f32 * ROW;
    let first = (plot.depth + span) / 2.0;
    (0..n.min(plot.capacity()))
        .map(|i| {
            let (row, column) = (i / columns, i % columns);
            let across = (column as f32 - (columns as f32 - 1.0) / 2.0) * COLUMN;
            let along = first - row as f32 * ROW;
            // Faces turn towards the station, where visitors stand.
            Placement {
                position: plot.near + plot.forward * along + side * across,
                facing_deg: plot.facing_deg + 180.0 + TURNS[i % TURNS.len()],
            }
        })
        .collect()
}

/// A rounded stone on a plinth with a band in `color` (linear RGB). Its face
/// looks along +z, its foot stands at the origin. About 140 triangles.
pub fn tombstone(color: [f32; 3]) -> Mesh {
    let stone = Material { color: meerora_geom::srgb_hex("#C4BEB1").unwrap_or([0.5; 3]), ..Default::default() };
    let dark = Material { color: meerora_geom::srgb_hex("#8A857A").unwrap_or([0.25; 3]), ..Default::default() };
    let band = Material { color, emissive: 0.12, ..Default::default() };
    let mut mesh = prim::bevel_box(PLINTH.0, PLINTH.1, PLINTH.2, 1.0).painted(&dark);
    let lift = Mat4::from_translation(Vec3::Y * PLINTH.1);
    mesh.append(&prim::bevel_box(STONE_W, BODY_H, STONE_D, 1.2).painted(&stone).transformed(lift));
    // The round top is a short cylinder lying across, its lower half inside the body.
    let top =
        Mat4::from_translation(Vec3::new(0.0, PLINTH.1 + BODY_H, -STONE_D / 2.0)) * Mat4::from_rotation_x(std::f32::consts::FRAC_PI_2);
    mesh.append(&prim::cylinder(STONE_W / 2.0, STONE_D, 10).painted(&stone).transformed(top));
    let at = Mat4::from_translation(Vec3::Y * (PLINTH.1 + BODY_H * 0.58));
    mesh.append(&prim::cuboid(STONE_W + 1.0, 3.0, STONE_D + 1.0).painted(&band).transformed(at));
    mesh
}

/// The key of a retired agent's portrait in [`crate::portraits::Portraits`].
/// A slash never appears in an agent id, so it never meets an agent's portrait.
pub fn portrait_key(id: &str) -> String {
    format!("graveyard/{id}")
}

struct Stone {
    /// The graveyard id of its agent.
    id: String,
    mesh: MeshId,
    transform: Mat4,
    bounds: Aabb,
}

/// The retired agents and their tombstones.
#[derive(Default)]
pub struct Graveyard {
    /// Newest first, as the daemon sends them.
    pub agents: Vec<RetiredAgent>,
    stones: Vec<Stone>,
    /// Portrait keys built for the agents.
    pub portraits: Vec<String>,
    /// The meshes and portraits need building again.
    dirty: bool,
    /// The graveyard id of the tombstone under the pointer.
    pub hover: Option<String>,
}

impl Graveyard {
    /// Takes the daemon's list. Returns true for the events it owns.
    pub fn apply(&mut self, event: &Event) -> bool {
        match event {
            // The greeting sends the list again. An old daemon sends none.
            Event::Hello { .. } => {
                self.set(Vec::new());
                false
            }
            Event::Graveyard { agents } => {
                self.set(agents.clone());
                true
            }
            _ => false,
        }
    }

    pub fn set(&mut self, agents: Vec<RetiredAgent>) {
        if agents != self.agents {
            self.agents = agents;
            self.dirty = true;
        }
    }

    pub fn agent(&self, id: &str) -> Option<&RetiredAgent> {
        self.agents.iter().find(|a| a.id == id)
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Builds the tombstones again, after the list or the space changed.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Uploads one tombstone per retired agent that fits on the plot of
    /// `space`, the oldest first, so a stone keeps its place when another
    /// agent retires.
    pub fn rebuild(&mut self, scene: &mut Renderer, device: &wgpu::Device, space: Option<&SpaceDesc>) {
        self.dirty = false;
        for old in self.stones.drain(..) {
            scene.remove_mesh(old.mesh);
        }
        let Some(plot) = space.and_then(Plot::of) else { return };
        let mut oldest: Vec<&RetiredAgent> = self.agents.iter().collect();
        oldest.sort_by(|a, b| a.retired_at.cmp(&b.retired_at).then_with(|| a.id.cmp(&b.id)));
        let places = layout(&plot, oldest.len());
        for (agent, place) in oldest.into_iter().zip(places) {
            let color = meerora_geom::srgb_hex(&agent.color).unwrap_or([0.8, 0.8, 0.8]);
            let mesh = tombstone(color);
            let transform = Mat4::from_rotation_translation(Quat::from_rotation_y(place.facing_deg.to_radians()), place.position);
            let grow = Vec3::splat(4.0);
            let b = mesh.bounds().transformed(transform);
            let bounds = Aabb::new(b.min - grow, b.max + grow);
            self.stones.push(Stone { id: agent.id.clone(), mesh: scene.upload_mesh(device, &mesh), transform, bounds });
        }
    }

    pub fn push_draws(&self, frame: &mut Frame) {
        for s in &self.stones {
            let brightness = if self.hover.as_deref() == Some(s.id.as_str()) { 1.3 } else { 1.0 };
            frame.draws.push(Draw { mesh: s.mesh, transform: s.transform, brightness, emissive_scale: 1.0 });
        }
    }

    /// The nearest tombstone under a picking ray, its graveyard id and how far along the ray it is.
    pub fn pick(&self, origin: Vec3, dir: Vec3) -> Option<(String, f32)> {
        self.stones.iter().filter_map(|s| s.bounds.ray_hit(origin, dir).map(|t| (s.id.clone(), t))).min_by(|a, b| a.1.total_cmp(&b.1))
    }

    /// The top of a tombstone, where its hover label goes.
    pub fn top(&self, id: &str) -> Option<Vec3> {
        let s = self.stones.iter().find(|s| s.id == id)?;
        Some(Vec3::new((s.bounds.min.x + s.bounds.max.x) / 2.0, s.bounds.max.y, (s.bounds.min.z + s.bounds.max.z) / 2.0))
    }

    /// How many tombstones stand in the space.
    pub fn stones(&self) -> usize {
        self.stones.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plot(width: f32, depth: f32) -> Plot {
        Plot { near: Vec3::new(100.0, 0.0, 200.0), forward: Vec3::NEG_Z, facing_deg: 180.0, width, depth }
    }

    /// The corners of a stone's plinth on the floor, in plot coordinates (across, along).
    fn footprint(plot: &Plot, p: &Placement) -> [(f32, f32); 4] {
        let turn = Quat::from_rotation_y(p.facing_deg.to_radians());
        let (hx, hz) = (PLINTH.0 / 2.0, PLINTH.2 / 2.0);
        [(-hx, -hz), (hx, -hz), (hx, hz), (-hx, hz)].map(|(x, z)| {
            let w = p.position + turn * Vec3::new(x, 0.0, z) - plot.near;
            (w.dot(plot.side()), w.dot(plot.forward))
        })
    }

    #[test]
    fn stones_fill_rows_from_the_far_end_without_overlap() {
        let plot = plot(64.0, 176.0);
        assert_eq!(plot.grid(), (2, 5));
        let places = layout(&plot, 3);
        assert_eq!(places.len(), 3);
        let along = |p: &Placement| (p.position - plot.near).dot(plot.forward);
        let across = |p: &Placement| (p.position - plot.near).dot(plot.side());
        assert!((along(&places[0]) - along(&places[1])).abs() < 1e-3, "the first two share the front row");
        assert!(along(&places[0]) > plot.depth / 2.0, "the first row is the one furthest from the station");
        assert!(along(&places[2]) < along(&places[0]) - ROW + 1e-3, "the third starts the next row, nearer the station");
        assert!((across(&places[0]) + across(&places[1])).abs() < 1e-3, "a row sits in the middle");
        // Faces look back at the station, give or take the hand-laid turn.
        for p in &places {
            let face = Quat::from_rotation_y(p.facing_deg.to_radians()) * Vec3::Z;
            assert!(face.dot(-plot.forward) > 0.99, "{face}");
        }

        let all = layout(&plot, 50);
        assert_eq!(all.len(), plot.capacity(), "never more than the plot holds");
        let feet: Vec<[(f32, f32); 4]> = all.iter().map(|p| footprint(&plot, p)).collect();
        for f in &feet {
            for (x, z) in f {
                assert!(x.abs() <= plot.width / 2.0 && (0.0..=plot.depth).contains(z), "({x}, {z}) is off the plot");
            }
        }
        let range = |f: &[(f32, f32); 4], k: usize| {
            let v = f.map(|c| if k == 0 { c.0 } else { c.1 });
            (v.iter().copied().fold(f32::INFINITY, f32::min), v.iter().copied().fold(f32::NEG_INFINITY, f32::max))
        };
        for (i, a) in feet.iter().enumerate() {
            for b in &feet[i + 1..] {
                let apart = (0..2).any(|k| {
                    let ((a0, a1), (b0, b1)) = (range(a, k), range(b, k));
                    a1 < b0 || b1 < a0
                });
                assert!(apart, "two plinths overlap");
            }
        }
    }

    #[test]
    fn a_plot_without_room_holds_nothing() {
        assert!(layout(&plot(PLINTH.0, 176.0), 3).is_empty(), "the edges leave no room across");
        assert!(layout(&plot(64.0, 10.0), 3).is_empty());
        let narrow = plot(PLINTH.0 + 2.0 * EDGE, PLINTH.2 + 2.0 * EDGE);
        assert_eq!(narrow.grid(), (1, 1));
        assert_eq!(layout(&narrow, 4).len(), 1);
    }

    #[test]
    fn a_tombstone_is_small_and_stands_on_the_floor() {
        let mesh = tombstone([1.0, 0.0, 0.0]);
        assert!(mesh.triangle_count() <= 160, "{} triangles", mesh.triangle_count());
        let b = mesh.bounds();
        assert!(b.min.y.abs() < 1e-3 && (b.max.y - (PLINTH.1 + BODY_H + STONE_W / 2.0)).abs() < 0.5, "{b:?}");
        assert!(mesh.vertices.iter().any(|v| v.color == [1.0, 0.0, 0.0]), "the band has the agent's colour");
    }

    fn real_space(room: &str) -> SpaceDesc {
        let path = format!("{}/../../assets/spaces/{room}/space.js", env!("CARGO_MANIFEST_DIR"));
        let source = std::fs::read_to_string(path).unwrap();
        meerora_script::build_space(&source, "space.js", &meerora_script::Limits::default()).unwrap()
    }

    #[test]
    fn the_plot_comes_from_the_space_script() {
        let mut desc = real_space("workshop");
        let declared = desc.graveyard.clone().expect("the workshop declares its plot");
        let plot = Plot::of(&desc).unwrap();
        let station = desc.stations.iter().find(|s| s.id == declared.station).unwrap();
        assert_eq!((plot.width, plot.depth), (declared.width, declared.depth));
        assert!((plot.near.distance(Vec3::new(station.position.x, 0.0, station.position.z)) - declared.gap).abs() < 1e-3);
        // A bigger plot in the script holds more stones, with no constant in the app.
        desc.graveyard.as_mut().unwrap().width = declared.width + 60.0;
        assert!(Plot::of(&desc).unwrap().capacity() > plot.capacity());
        desc.graveyard = None;
        assert_eq!(Plot::of(&desc), None, "an older space.js without the declaration has no plot");
    }

    #[test]
    fn the_real_plots_are_fenced_off() {
        for room in ["workshop", "space-station", "office"] {
            let desc = real_space(room);
            let plot = Plot::of(&desc).unwrap_or_else(|| panic!("{room} declares a graveyard"));
            assert!(plot.capacity() >= 12, "{room} holds {}", plot.capacity());
            let nav = crate::nav::NavGrid::build(&desc.solid_mesh(), desc.bounds).unwrap();
            let station = desc.stations.iter().find(|s| s.id == "graveyard").unwrap().position;
            assert!(nav.is_free(station), "{room}");
            for p in layout(&plot, plot.capacity()) {
                // An agent can stand at a spot and reach the rest only when nearest_free gives the spot back.
                assert_ne!(nav.nearest_free(p.position), Some(p.position), "{room}: agents can walk onto the plot at {}", p.position);
            }
        }
    }
}
