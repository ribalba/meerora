//! Devices in the app (D7, D8): their shapes, where they stand, their params
//! and what the daemon says about them.
//!
//! The app builds every device script itself, like the figures, and draws
//! the devices of the space even while the daemon is away. The daemon is the
//! authority on what a device may do. It sends the list of devices with
//! their actions and grants ([`Event::Devices`]) and the param values that
//! follow the state sources ([`Event::DeviceParam`]). A device an agent made
//! shows only once the daemon loaded it, because a device that names a
//! capability its agent lacks must not appear (D8).
//!
//! A device with `display: graph({...})` keeps the recent values of its graph
//! param as they arrive and draws them as a glowing line inside its
//! rectangle. The line is a mesh of its own, built again whenever a value
//! arrives. When a setting of the device changes, such as the metric of the
//! system graph, the line starts over.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::Path;

use meerora_geom::{Aabb, Mat4, Material, Mesh, Quat, Vec3, prim};
use meerora_protocol::{DeviceInfo, Event, device_files};
use meerora_scene::{Draw, Frame, MeshId, Renderer};
use meerora_script::{DeviceDesc, GraphDisplay, Limits, build_device};
use serde_json::Value;

use crate::world::{World, motion_matrix};

/// A device built from its script and uploaded to the renderer.
pub struct Device {
    pub id: String,
    pub owner: Option<String>,
    pub desc: DeviceDesc,
    pub base: MeshId,
    pub parts: Vec<MeshId>,
    /// Param values from the daemon. Missing ones take their default.
    pub params: BTreeMap<String, Value>,
    /// Highlighted because the pointer is over it.
    pub hover: bool,
    /// The recent values of the graph param, from 0 (`min`) to 1 (`max`), the newest last.
    pub history: VecDeque<f32>,
    /// The line of the graph, while it has two values or more.
    pub graph: Option<MeshId>,
    /// The line needs building again.
    graph_dirty: bool,
}

impl Device {
    pub fn value(&self, name: &str) -> Option<Value> {
        self.params.get(name).cloned().or_else(|| self.desc.param(name).map(|p| p.default_value()))
    }

    /// Takes a value from the daemon. Returns true when the graph changed.
    pub fn set(&mut self, name: &str, value: Value) -> bool {
        let before = self.value(name);
        self.params.insert(name.to_string(), value.clone());
        let Some(g) = &self.desc.display else { return false };
        if self.desc.setting(name).is_some() && before.as_ref() != Some(&value) {
            // Another metric: the old line means something else.
            self.history.clear();
            self.graph_dirty = true;
            return true;
        }
        if name != g.value {
            return false;
        }
        let Some(x) = value.as_f64() else { return false };
        let level = ((x - g.min) / (g.max - g.min)).clamp(0.0, 1.0) as f32;
        self.history.push_back(level);
        while self.history.len() > g.points.max(2) {
            self.history.pop_front();
        }
        self.graph_dirty = true;
        true
    }
}

/// The line of a graph through `history` (0 to 1, the newest last) in the
/// device's own axes: the newest value on the right edge of the rectangle,
/// one step of `width / (points - 1)` between values. Thin glowing bars, 12
/// triangles per step. Empty with fewer than two values.
pub fn graph_mesh(g: &GraphDisplay, history: &VecDeque<f32>) -> Mesh {
    let mut mesh = Mesh::new();
    if history.len() < 2 {
        return mesh;
    }
    let material = Material { color: g.color, emissive: 1.0, ..Default::default() };
    let step = g.width / (g.points.max(2) - 1) as f32;
    let right = g.at.x + g.width / 2.0;
    let n = history.len();
    let point = |i: usize| {
        let x = right - (n - 1 - i) as f32 * step;
        let y = g.at.y - g.height / 2.0 + history[i] * g.height;
        Vec3::new(x, y, g.at.z)
    };
    for i in 1..n {
        let (a, b) = (point(i - 1), point(i));
        let d = b - a;
        // A little longer than the step, so the bars meet at the corners.
        let length = d.length() + g.thickness * 0.5;
        let bar = prim::cuboid(length, g.thickness, g.thickness * 0.5).painted(&material);
        let m = Mat4::from_translation((a + b) / 2.0)
            * Mat4::from_rotation_z(d.y.atan2(d.x))
            * Mat4::from_translation(Vec3::new(0.0, -g.thickness / 2.0, 0.0));
        mesh.append(&bar.transformed(m));
    }
    mesh
}

#[derive(Default)]
pub struct Devices {
    pub items: Vec<Device>,
    /// What the daemon said about each device, since its last hello.
    pub infos: HashMap<String, DeviceInfo>,
    /// The device whose card is open.
    pub card: Option<String>,
    /// Build errors by script, shown once each.
    errors: HashMap<String, String>,
    /// The scripts need building again.
    dirty: bool,
}

impl Devices {
    pub fn device(&self, id: &str) -> Option<&Device> {
        self.items.iter().find(|d| d.id == id)
    }

    pub fn info(&self, id: &str) -> Option<&DeviceInfo> {
        self.infos.get(id)
    }

    /// Builds the scripts again on the next step.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// The scripts need building or a graph its line.
    pub fn is_dirty(&self) -> bool {
        self.dirty || self.items.iter().any(|d| d.graph_dirty)
    }

    /// Builds every device script of the data dir and uploads the shapes,
    /// or only the lines of the graphs that changed when no script did.
    /// Returns the errors that are new, for a toast.
    pub fn rebuild(&mut self, scene: &mut Renderer, device: &wgpu::Device, data_dir: &Path) -> Vec<String> {
        if !self.dirty && self.items.iter().any(|d| d.graph_dirty) {
            self.update_graphs(scene, device);
            return Vec::new();
        }
        self.dirty = false;
        let mut built = Vec::new();
        let mut errors = HashMap::new();
        for file in device_files(data_dir) {
            let name = file.path.strip_prefix(data_dir).unwrap_or(&file.path).display().to_string();
            match build(&file.path, &name) {
                Ok(desc) => {
                    let base = scene.upload_mesh(device, &desc.base);
                    let parts = desc.param_parts.iter().map(|p| scene.upload_mesh(device, &p.mesh)).collect();
                    let old = self.device(&file.id);
                    let params = old.map(|d| d.params.clone()).unwrap_or_default();
                    // The line goes on while the graph shows the same param.
                    let same_graph =
                        old.is_some_and(|d| d.desc.display.as_ref().map(|g| &g.value) == desc.display.as_ref().map(|g| &g.value));
                    let history = if same_graph { old.map(|d| d.history.clone()).unwrap_or_default() } else { VecDeque::new() };
                    let graph_dirty = desc.display.is_some();
                    built.push(Device {
                        id: file.id,
                        owner: file.owner,
                        desc,
                        base,
                        parts,
                        params,
                        hover: false,
                        history,
                        graph: None,
                        graph_dirty,
                    });
                }
                Err(e) => {
                    tracing::warn!("device {name} failed: {e}");
                    errors.insert(name, e);
                }
            }
        }
        for old in std::mem::replace(&mut self.items, built) {
            scene.remove_mesh(old.base);
            for part in old.parts {
                scene.remove_mesh(part);
            }
            if let Some(graph) = old.graph {
                scene.remove_mesh(graph);
            }
        }
        self.update_graphs(scene, device);
        if self.card.as_ref().is_some_and(|id| self.device(id).is_none()) {
            self.card = None;
        }
        let new: Vec<String> =
            errors.iter().filter(|(name, e)| self.errors.get(*name) != Some(e)).map(|(name, e)| format!("{name}: {e}")).collect();
        self.errors = errors;
        new
    }

    /// Builds the lines of the graphs that got a value since.
    fn update_graphs(&mut self, scene: &mut Renderer, device: &wgpu::Device) {
        for d in self.items.iter_mut().filter(|d| d.graph_dirty) {
            d.graph_dirty = false;
            if let Some(old) = d.graph.take() {
                scene.remove_mesh(old);
            }
            let Some(g) = &d.desc.display else { continue };
            let mesh = graph_mesh(g, &d.history);
            if !mesh.is_empty() {
                d.graph = Some(scene.upload_mesh(device, &mesh));
            }
        }
    }

    /// Takes a daemon event. The result of an action comes back as the text
    /// of a toast and whether it is an error.
    pub fn apply(&mut self, event: &Event) -> Option<(String, bool)> {
        match event {
            // The greeting sends the device list again, if there are devices.
            Event::Hello { .. } => {
                self.infos.clear();
                for d in &mut self.items {
                    d.params.clear();
                }
            }
            Event::Devices { devices } => {
                let changed = devices.iter().any(|i| self.infos.get(&i.id).is_none_or(|old| old.revision != i.revision))
                    || devices.iter().any(|i| self.device(&i.id).is_none() && i.error.is_none());
                if changed {
                    self.dirty = true;
                }
                self.infos = devices.iter().map(|i| (i.id.clone(), i.clone())).collect();
            }
            Event::DeviceParam { device, name, value } => {
                if let Some(d) = self.items.iter_mut().find(|d| d.id == *device) {
                    d.set(name, value.clone());
                }
            }
            Event::DeviceCapabilitiesChanged { device, granted } => {
                if let Some(info) = self.infos.get_mut(device) {
                    info.granted = granted.clone();
                }
            }
            Event::DeviceActionResult { message, ok, .. } if !message.is_empty() => return Some((message.clone(), !ok)),
            _ => {}
        }
        None
    }

    /// Whether the device is in the space: built, not refused by the
    /// daemon, and for an agent's device loaded by the daemon (D8).
    pub fn shown(&self, d: &Device) -> bool {
        match (self.infos.get(&d.id), &d.owner) {
            (Some(info), _) => info.error.is_none(),
            (None, owner) => owner.is_none(),
        }
    }

    /// The devices in the space, with their transform.
    pub fn placed<'a>(&'a self, world: &'a World) -> impl Iterator<Item = (usize, &'a Device, Mat4)> + 'a {
        self.items.iter().enumerate().filter(|(_, d)| self.shown(d)).filter_map(|(i, d)| Some((i, d, transform(world, d)?)))
    }

    /// The nearest device under a picking ray and how far along the ray it is.
    pub fn pick(&self, world: &World, toward_viewer: Vec3, origin: Vec3, dir: Vec3) -> Option<(usize, f32)> {
        let now = world.time as f32;
        self.placed(world)
            .filter(|(_, d, _)| d.desc.place.shown(toward_viewer))
            .filter_map(|(i, d, m)| {
                visible_boxes(d, m, now).iter().filter_map(|b| b.ray_hit(origin, dir)).min_by(f32::total_cmp).map(|t| (i, t))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
    }

    /// Adds the visible devices to the frame. Devices on a wall that is cut
    /// away are left out with the wall.
    pub fn push_draws(&self, world: &World, frame: &mut Frame) {
        let toward_viewer = frame.camera.toward_viewer();
        let now = world.time as f32;
        for (_, d, m) in self.placed(world) {
            if !d.desc.place.shown(toward_viewer) {
                continue;
            }
            let brightness = if d.hover { 1.18 } else { 1.0 };
            frame.draws.push(Draw { mesh: d.base, transform: m, brightness, emissive_scale: 1.0 });
            for (part, mesh) in d.desc.param_parts.iter().zip(&d.parts) {
                if d.value(&part.param).as_ref() != Some(&part.when) {
                    continue;
                }
                let local = match &part.anim {
                    Some(motion) => Mat4::from_translation(part.pivot) * motion_matrix(motion, now) * Mat4::from_translation(-part.pivot),
                    None => Mat4::IDENTITY,
                };
                frame.draws.push(Draw { mesh: *mesh, transform: m * local, brightness, emissive_scale: 1.0 });
            }
            if let Some(graph) = d.graph {
                frame.draws.push(Draw { mesh: graph, transform: m, brightness: 1.0, emissive_scale: 1.0 });
            }
        }
    }
}

fn build(path: &Path, name: &str) -> Result<DeviceDesc, String> {
    let source = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    build_device(&source, name, &Limits::default()).map_err(|e| e.to_string())
}

/// Where a device stands, at its place or next to its station. `None` while
/// the station is not in the space.
pub fn transform(world: &World, d: &Device) -> Option<Mat4> {
    let place = &d.desc.place;
    let (position, facing) = match &place.station {
        None => (place.position, place.facing_deg.unwrap_or(0.0)),
        Some(id) => {
            let station = world.space.as_ref()?.desc.stations.iter().find(|s| s.id == *id)?;
            (station.position + place.position, place.facing_deg.unwrap_or(station.facing_deg))
        }
    };
    Some(Mat4::from_rotation_translation(Quat::from_rotation_y(facing.to_radians()), position))
}

/// World boxes of what a device shows right now: its base and the param
/// parts of the current values, where their animation has them at `now`.
/// Picking uses these instead of the bounds of every state, so the flight
/// path of the paper plane does not cover the agents near the window. Each
/// box gets a little margin so small devices are easy to hit.
pub fn visible_boxes(d: &Device, m: Mat4, now: f32) -> Vec<Aabb> {
    let grow = Vec3::splat(4.0);
    let padded = |b: Aabb| Aabb::new(b.min - grow, b.max + grow);
    let mut boxes = Vec::new();
    if !d.desc.base.is_empty() {
        boxes.push(padded(d.desc.base.bounds().transformed(m)));
    }
    for part in &d.desc.param_parts {
        if part.mesh.is_empty() || d.value(&part.param).as_ref() != Some(&part.when) {
            continue;
        }
        let local = match &part.anim {
            Some(motion) => Mat4::from_translation(part.pivot) * motion_matrix(motion, now) * Mat4::from_translation(-part.pivot),
            None => Mat4::IDENTITY,
        };
        boxes.push(padded(part.mesh.bounds().transformed(m * local)));
    }
    boxes
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn assets() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets")
    }

    fn device(id: &str, owner: Option<&str>) -> Device {
        let path = assets().join("spaces/workshop/devices/airplane.js");
        let desc = build(&path, "airplane.js").unwrap();
        Device {
            id: id.into(),
            owner: owner.map(String::from),
            desc,
            base: MeshId(0),
            parts: vec![MeshId(1), MeshId(2)],
            params: Default::default(),
            hover: false,
            history: VecDeque::new(),
            graph: None,
            graph_dirty: false,
        }
    }

    fn info(id: &str, error: Option<&str>) -> DeviceInfo {
        DeviceInfo {
            id: id.into(),
            name: "Airplane mode".into(),
            owner: None,
            script: String::new(),
            revision: "r1".into(),
            actions: Vec::new(),
            needs: Vec::new(),
            granted: Vec::new(),
            error: error.map(String::from),
        }
    }

    #[test]
    fn agent_devices_wait_for_the_daemon() {
        let mut devices = Devices { items: vec![device("airplane", None), device("rivet/plane", Some("rivet"))], ..Default::default() };
        let shown = |d: &Devices| d.items.iter().filter(|x| d.shown(x)).map(|x| x.id.clone()).collect::<Vec<_>>();
        assert_eq!(shown(&devices), ["airplane"], "the space's own devices show without the daemon");
        devices.apply(&Event::Devices { devices: vec![info("airplane", None), info("rivet/plane", Some("Rivet does not hold it"))] });
        assert_eq!(shown(&devices), ["airplane"], "refused by the daemon (D8)");
        devices.apply(&Event::Devices { devices: vec![info("airplane", Some("no backend")), info("rivet/plane", None)] });
        assert_eq!(shown(&devices), ["rivet/plane"]);
    }

    #[test]
    fn params_follow_the_daemon_and_the_card_reads_them() {
        let mut devices = Devices { items: vec![device("airplane", None)], ..Default::default() };
        let d = &devices.items[0];
        assert_eq!(d.value("flying"), Some(Value::Bool(false)), "the default");
        assert_eq!(d.desc.status_text(&d.params).as_deref(), Some("Off. The plane is parked on the window sill."));
        devices.apply(&Event::DeviceParam { device: "airplane".into(), name: "flying".into(), value: Value::Bool(true) });
        let d = &devices.items[0];
        assert_eq!(d.desc.status_text(&d.params).as_deref(), Some("On. The plane circles the room."));
        let toast = devices.apply(&Event::DeviceActionResult {
            device: "airplane".into(),
            action: "toggle".into(),
            ok: false,
            message: "no".into(),
        });
        assert_eq!(toast, Some(("no".into(), true)));
        devices.apply(&Event::Hello { version: 3, daemon: "d".into() });
        assert!(devices.items[0].params.is_empty(), "a new greeting sends the values again");
    }

    #[test]
    fn the_clock_disappears_with_the_back_wall() {
        let space = std::fs::read_to_string(assets().join("spaces/workshop/space.js")).unwrap();
        let space = meerora_script::build_space(&space, "space.js", &Limits::default()).unwrap();
        let clock = build(&assets().join("spaces/workshop/devices/clock.js"), "clock.js").unwrap();
        let wall = clock.place.wall.expect("the clock hangs on a wall");
        let back = space.walls.iter().find(|w| w.inside.distance(wall) < 1e-3).expect("a wall with the clock's inside");
        for step in 0..48 {
            let azimuth = (step as f32 * 7.5).to_radians();
            for elevation in [12f32, 30.0, 80.0] {
                let e = elevation.to_radians();
                let toward = Vec3::new(azimuth.sin() * e.cos(), e.sin(), azimuth.cos() * e.cos());
                assert_eq!(clock.place.shown(toward), back.faces(toward), "azimuth {} elevation {elevation}", step as f32 * 7.5);
            }
        }
    }

    #[test]
    fn picking_sees_only_what_the_device_shows() {
        let mut plane = device("airplane", None);
        let all = plane.desc.bounds;
        let parked = visible_boxes(&plane, Mat4::IDENTITY, 0.0);
        let parked_size: f32 = parked.iter().map(|b| b.size().length()).fold(0.0, f32::max);
        assert!(parked_size < all.size().length() * 0.6, "the parked plane is much smaller than every state together");
        // A ray straight down through the far corner of the bounds of every
        // state (3 m of flight path) hits nothing the parked plane shows.
        let corner = Vec3::new(all.max.x - 2.0, all.max.y + 100.0, all.max.z - 2.0);
        let down = Vec3::NEG_Y;
        assert!(all.ray_hit(corner, down).is_some());
        assert!(parked.iter().all(|b| b.ray_hit(corner, down).is_none()), "the old bounds caught clicks meant for agents");
        // In flight the plane is picked where it is.
        let flying = plane.desc.params.iter().find(|p| matches!(p.kind, meerora_script::ParamKind::Toggle { .. })).map(|p| p.name.clone());
        if let Some(name) = flying {
            plane.params.insert(name, Value::Bool(true));
            assert!(visible_boxes(&plane, Mat4::IDENTITY, 0.0).len() > parked.len().saturating_sub(1));
        }
    }

    fn graph_device() -> Device {
        let path = assets().join("spaces/workshop/devices/graph.js");
        let mut d = device("graph", None);
        d.desc = build(&path, "graph.js").unwrap();
        d
    }

    #[test]
    fn the_graph_keeps_recent_values_and_draws_a_line() {
        let mut devices = Devices { items: vec![graph_device()], ..Default::default() };
        let g = devices.items[0].desc.display.clone().expect("a graph");
        assert!(!devices.is_dirty());
        devices.apply(&Event::DeviceParam { device: "graph".into(), name: "level".into(), value: serde_json::json!(0.25) });
        assert!(devices.is_dirty(), "a value builds the line again");
        assert!(graph_mesh(&g, &devices.items[0].history).is_empty(), "one value is no line yet");
        for i in 0..(g.points + 10) {
            let value = serde_json::json!((i % 10) as f64 / 10.0);
            devices.apply(&Event::DeviceParam { device: "graph".into(), name: "level".into(), value });
        }
        let d = &devices.items[0];
        assert_eq!(d.history.len(), g.points, "only the recent values stay");
        assert_eq!(d.history.back(), Some(&0.9));
        let mesh = graph_mesh(&g, &d.history);
        assert_eq!(mesh.triangle_count(), (g.points - 1) * 12);
        // The line stays inside the rectangle of the screen, with room for its thickness.
        let b = mesh.bounds();
        let pad = g.thickness * 1.5;
        assert!(b.min.x >= g.at.x - g.width / 2.0 - pad && b.max.x <= g.at.x + g.width / 2.0 + pad, "{b:?}");
        assert!(b.min.y >= g.at.y - g.height / 2.0 - pad && b.max.y <= g.at.y + g.height / 2.0 + pad, "{b:?}");
        assert!(mesh.vertices.iter().all(|v| v.emissive > 0.99), "the line glows");
        // Values outside min and max are clamped, values of other params ignored.
        devices.apply(&Event::DeviceParam { device: "graph".into(), name: "level".into(), value: serde_json::json!(7.0) });
        assert_eq!(devices.items[0].history.back(), Some(&1.0));
        devices.apply(&Event::DeviceParam { device: "graph".into(), name: "reading".into(), value: serde_json::json!("42%") });
        assert_eq!(devices.items[0].history.len(), g.points);
        // Another metric starts the line over.
        let metric = |m: &str| Event::DeviceParam { device: "graph".into(), name: "metric".into(), value: serde_json::json!(m) };
        devices.apply(&metric("cpu"));
        assert_eq!(devices.items[0].history.len(), g.points, "the same metric keeps the line");
        devices.apply(&metric("network"));
        assert!(devices.items[0].history.is_empty());
        let d = &devices.items[0];
        assert_eq!(d.desc.status_text(&d.params).as_deref(), Some("Network traffic: 42% per second."));
    }

    #[test]
    fn a_new_revision_builds_again() {
        let mut devices = Devices { items: vec![device("airplane", None)], ..Default::default() };
        devices.apply(&Event::Devices { devices: vec![info("airplane", None)] });
        assert!(devices.is_dirty(), "the first list may name another version");
        devices.dirty = false;
        devices.apply(&Event::Devices { devices: vec![info("airplane", None)] });
        assert!(!devices.is_dirty());
        let mut changed = info("airplane", None);
        changed.revision = "r2".into();
        devices.apply(&Event::Devices { devices: vec![changed] });
        assert!(devices.is_dirty());
    }
}
