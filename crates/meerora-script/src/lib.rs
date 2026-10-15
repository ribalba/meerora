//! Sandboxed construction scripts (D4, D5, D20, D24).
//!
//! A script is JavaScript run once on QuickJS with a memory limit, a time
//! limit and no I/O. It describes geometry through the modelling API (see
//! `API.md` next to this file) and ends with `export default figure({...})`,
//! `export default space({...})` or `export default device({...})` (D7). The
//! host bakes the result into meshes in local space and validates it.

use meerora_geom::{Aabb, Mesh, Vec3};

mod bake;
mod command;
mod device;
mod host;

pub use command::{CommandAction, CommandItem, CommandLimits, CommandMeta, CommandScript, MAX_ITEMS as MAX_COMMAND_ITEMS};
pub use device::{
    ActionKind, Binding, DEVICE_BUDGET, DEVICE_BUDGET_MAX, DeviceAction, DeviceDesc, DevicePlace, DeviceSetting, Effect, GraphDisplay, Map,
    RING_VALUES, Source, Status, capability_name_ok, shown,
};

/// Sandbox limits for one script run.
#[derive(Clone, Debug)]
pub struct Limits {
    pub memory_bytes: usize,
    pub max_stack_bytes: usize,
    pub timeout_ms: u64,
    /// Runs abort as soon as the geometry built so far exceeds this, even
    /// before the declared budget is checked. Protects the host.
    pub hard_triangle_cap: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self { memory_bytes: 32 << 20, max_stack_bytes: 512 << 10, timeout_ms: 500, hard_triangle_cap: 200_000 }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ScriptError {
    /// Syntax or runtime error inside the script, with file:line when known.
    #[error("{0}")]
    Js(String),
    #[error("script ran longer than {0} ms")]
    Timeout(u64),
    #[error("script used more than {0} bytes of memory")]
    OutOfMemory(usize),
    #[error("script built more than {0} triangles")]
    HardCap(usize),
    #[error("{used} triangles exceed the budget of {budget}")]
    OverBudget { used: usize, budget: usize },
    /// D5: every missing anchor or animation slot, all at once.
    #[error("missing {}", .0.join(", "))]
    MissingSlots(Vec<String>),
    #[error("expected `export default {expected}({{...}})`")]
    WrongKind { expected: &'static str },
    #[error("{0}")]
    Invalid(String),
}

/// Procedural motion (D9). Evaluated by the app every frame.
#[derive(Clone, Debug, PartialEq)]
pub enum Motion {
    Still,
    /// Up and down by `amplitude` cm.
    Bob {
        amplitude: f32,
        period: f32,
    },
    /// `count` hops of `height` cm, then a pause, repeating every `period` s.
    Hop {
        count: u32,
        height: f32,
        period: f32,
    },
    /// Slow vertical squash by `depth` (fraction, e.g. 0.02).
    Breathe {
        period: f32,
        depth: f32,
    },
    Spin {
        period: f32,
    },
    /// Rocks back and forth by `angle_deg` around the part's pivot.
    Wave {
        period: f32,
        angle_deg: f32,
    },
    Orbit {
        radius: f32,
        period: f32,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Anim {
    pub motion: Motion,
    /// Turn towards the camera when the motion settles (`.then(face("viewer"))`).
    pub face_viewer: bool,
    /// Dim emissive parts such as eyes and screens (`.eyes("closed")`).
    pub eyes_closed: bool,
}

/// The three animations every figure must provide (D5).
#[derive(Clone, Debug, PartialEq)]
pub struct Animations {
    pub working: Anim,
    pub waiting: Anim,
    pub resting: Anim,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ParamKind {
    Toggle { default: bool },
    Choice { values: Vec<String>, default: String },
    Number { min: f64, max: f64, default: f64 },
    Count { default: u32 },
    Text { default: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ParamDef {
    pub name: String,
    pub kind: ParamKind,
}

/// Geometry shown only while a param has a given value (D24), for example the
/// envelope Wren holds when `letter` is true.
#[derive(Clone, Debug, PartialEq)]
pub struct ParamPart {
    pub param: String,
    pub when: serde_json::Value,
    pub mesh: Mesh,
    /// Point the part's animation rotates around, in figure space.
    pub pivot: Vec3,
    pub anim: Option<Motion>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FigureDesc {
    /// Always visible geometry. Feet at the origin, facing +z.
    pub base: Mesh,
    pub param_parts: Vec<ParamPart>,
    /// Where the status ring floats (D3, D5).
    pub ring_anchor: Vec3,
    /// Centre of the expression surface: eyes, screen or glow (D5).
    pub face_anchor: Vec3,
    pub animations: Animations,
    pub params: Vec<ParamDef>,
    pub triangles: usize,
    pub budget: usize,
    /// Bounds of base plus all param parts, for picking and blob shadows.
    pub bounds: Aabb,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LightDesc {
    pub position: Vec3,
    /// Linear RGB.
    pub color: [f32; 3],
    pub intensity: f32,
    /// Distance in cm where the light fades to zero.
    pub radius: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Station {
    pub id: String,
    pub position: Vec3,
    /// Direction the agent faces while working here, degrees around y, 0 = +z.
    pub facing_deg: f32,
    /// A spot where waiting agents gather (D3: "a waiting agent comes to the front").
    pub front: bool,
    /// Action categories whose work happens here (D9), for example `terminal`
    /// or `mail`. A category is the part of an action name before the dot, so
    /// `terminal.run` belongs to `terminal`. No two stations share a category.
    pub tools: Vec<String>,
}

/// The category of an action name, the part before the first dot
/// (`browser.open` gives `browser`).
pub fn action_category(action: &str) -> &str {
    action.split_once('.').map_or(action, |(category, _)| category)
}

#[derive(Clone, Debug, PartialEq)]
pub struct CameraHint {
    pub target: Vec3,
    /// Visible world height in cm (orthographic).
    pub view_height: f32,
    pub azimuth_deg: f32,
    pub elevation_deg: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SpaceDesc {
    /// All static geometry merged, without the walls.
    pub mesh: Mesh,
    /// Walls that the app hides while the camera looks at them from outside.
    pub walls: Vec<SpaceWall>,
    pub lights: Vec<LightDesc>,
    /// Linear RGB.
    pub ambient_sky: [f32; 3],
    pub ambient_ground: [f32; 3],
    pub sun_direction: Vec3,
    pub sun_color: [f32; 3],
    pub sun_intensity: f32,
    pub background: [f32; 3],
    pub stations: Vec<Station>,
    /// Stations and extra waypoints are all nodes of the walk graph (D9).
    pub waypoints: Vec<(String, Vec3)>,
    pub edges: Vec<(String, String)>,
    pub camera: CameraHint,
    /// The plot of the graveyard, `None` when the script declares none.
    pub graveyard: Option<GraveyardPlot>,
    pub triangles: usize,
    pub budget: usize,
    pub bounds: Aabb,
}

/// `graveyard: { station, gap, width, depth }` of a space script: the
/// rectangle where the app stands a tombstone for each retired agent. It
/// starts `gap` cm in front of the station in the direction the station
/// faces, is `width` cm wide across that direction and `depth` cm deep
/// along it. The script builds its fence around the same rectangle.
#[derive(Clone, Debug, PartialEq)]
pub struct GraveyardPlot {
    /// A station of the space, `graveyard` when the script names none.
    pub station: String,
    pub gap: f32,
    pub width: f32,
    pub depth: f32,
}

/// A wall of the space and everything that hangs on it, as its own mesh.
#[derive(Clone, Debug, PartialEq)]
pub struct SpaceWall {
    pub mesh: Mesh,
    /// Unit vector on the floor plane from the wall into the room.
    pub inside: Vec3,
}

impl SpaceWall {
    /// False while a camera looking from `toward_viewer` stands outside the
    /// wall, where the wall would hide the room.
    pub fn faces(&self, toward_viewer: Vec3) -> bool {
        Vec3::new(toward_viewer.x, 0.0, toward_viewer.z).dot(self.inside) >= -1e-3
    }
}

impl SpaceDesc {
    /// The geometry including the walls, for collision and navigation.
    pub fn solid_mesh(&self) -> Mesh {
        let mut mesh = self.mesh.clone();
        for wall in &self.walls {
            mesh.append(&wall.mesh);
        }
        mesh
    }

    /// The station where a working agent goes for an action such as
    /// `terminal.run` (D9). `None` when no station lists the category.
    pub fn station_for_action(&self, action: &str) -> Option<&Station> {
        let category = action_category(action);
        self.stations.iter().find(|s| s.tools.iter().any(|t| t == category))
    }
}

/// Runs a figure script and validates it (budget, D5 anchors and animations).
pub fn build_figure(source: &str, file_name: &str, limits: &Limits) -> Result<FigureDesc, ScriptError> {
    let (raw, _console) = host::run(source, file_name, limits);
    let raw = raw?;
    match raw.kind.as_deref() {
        Some("figure") => bake::figure(&raw, limits),
        _ => Err(ScriptError::WrongKind { expected: "figure" }),
    }
}

/// Runs a space script and validates it (budget, walk graph references).
pub fn build_space(source: &str, file_name: &str, limits: &Limits) -> Result<SpaceDesc, ScriptError> {
    let (raw, _console) = host::run(source, file_name, limits);
    let raw = raw?;
    match raw.kind.as_deref() {
        Some("space") => bake::space(&raw, limits),
        _ => Err(ScriptError::WrongKind { expected: "space" }),
    }
}

/// Runs a device script and validates it (budget, bindings, actions, D7, D8).
pub fn build_device(source: &str, file_name: &str, limits: &Limits) -> Result<DeviceDesc, ScriptError> {
    let (raw, _console) = host::run(source, file_name, limits);
    let raw = raw?;
    match raw.kind.as_deref() {
        Some("device") => device::bake(&raw, limits),
        _ => Err(ScriptError::WrongKind { expected: "device" }),
    }
}

/// What a script built, whichever kind its default export is.
#[derive(Clone, Debug, PartialEq)]
pub enum Built {
    Figure(FigureDesc),
    Space(SpaceDesc),
    Device(DeviceDesc),
}

/// Result of [`run_script`].
#[derive(Debug)]
pub struct ScriptRun {
    pub result: Result<Built, ScriptError>,
    /// Lines the script printed with `console.log`, also when it failed. The
    /// output is cut off after 500 lines or 64 KiB.
    pub console: Vec<String>,
}

/// Runs a figure, space or device script, detected from its default export, and keeps
/// its console output. Used by tools such as `figure-check`.
pub fn run_script(source: &str, file_name: &str, limits: &Limits) -> ScriptRun {
    let (raw, console) = host::run(source, file_name, limits);
    let result = raw.and_then(|raw| match raw.kind.as_deref() {
        Some("figure") => bake::figure(&raw, limits).map(Built::Figure),
        Some("space") => bake::space(&raw, limits).map(Built::Space),
        Some("device") => device::bake(&raw, limits).map(Built::Device),
        _ => Err(ScriptError::Invalid(
            "expected `export default figure({...})`, `export default space({...})` or `export default device({...})`".into(),
        )),
    });
    ScriptRun { result, console }
}
