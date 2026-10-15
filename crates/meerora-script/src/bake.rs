//! Turns the JSON description produced by `figure()` and `space()` into
//! meshes and validates it (budget, D5 slots, walk graph).

use std::collections::{HashMap, VecDeque};

use meerora_geom::{Finish, Mat4, Material, Mesh, Vec3, srgb_hex};
use serde::Deserialize;

use crate::host::Raw;
use crate::{
    Anim, Animations, CameraHint, FigureDesc, GraveyardPlot, LightDesc, Limits, Motion, ParamDef, ParamKind, ParamPart, ScriptError,
    SpaceDesc, SpaceWall, Station,
};

const FIGURE_BUDGET: usize = 3000;
const FIGURE_BUDGET_MAX: usize = 6000;
const SPACE_BUDGET: usize = 60000;

#[derive(Deserialize)]
pub(crate) struct PartJson {
    mesh: usize,
    m: Vec<f64>,
    color: String,
    finish: String,
    glow: f64,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(crate) enum MotionJson {
    Still,
    Bob { amplitude: f32, period: f32 },
    Hop { count: u32, height: f32, period: f32 },
    Breathe { period: f32, depth: f32 },
    Spin { period: f32 },
    Wave { period: f32, angle: f32 },
    Orbit { radius: f32, period: f32 },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnimJson {
    motion: MotionJson,
    face_viewer: bool,
    eyes_closed: bool,
}

#[derive(Deserialize)]
struct AnchorsJson {
    ring: Option<[f32; 3]>,
    face: Option<[f32; 3]>,
}

#[derive(Deserialize)]
struct AnimationsJson {
    working: Option<AnimJson>,
    waiting: Option<AnimJson>,
    resting: Option<AnimJson>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(crate) enum ParamKindJson {
    Toggle { default: bool },
    Choice { values: Vec<String>, default: String },
    Number { min: f64, max: f64, default: f64 },
    Count { default: u32 },
    Text { default: String },
}

#[derive(Deserialize)]
struct ShowJson {
    when: serde_json::Value,
    pivot: [f32; 3],
    anim: Option<MotionJson>,
    parts: Vec<PartJson>,
}

#[derive(Deserialize)]
struct ParamJson {
    name: String,
    kind: ParamKindJson,
    shows: Vec<ShowJson>,
}

#[derive(Deserialize)]
struct FigureJson {
    base: Vec<PartJson>,
    anchors: AnchorsJson,
    animations: AnimationsJson,
    params: Vec<ParamJson>,
    budget: Option<f64>,
}

#[derive(Deserialize)]
struct LightJson {
    position: [f32; 3],
    color: String,
    intensity: f32,
    radius: f32,
}

#[derive(Deserialize)]
struct AmbientJson {
    sky: String,
    ground: String,
}

#[derive(Deserialize)]
struct SunJson {
    direction: [f32; 3],
    color: String,
    intensity: f32,
}

#[derive(Deserialize)]
struct StationJson {
    id: String,
    position: [f32; 3],
    facing: f32,
    front: bool,
    tools: Vec<String>,
}

#[derive(Deserialize)]
struct WaypointJson {
    id: String,
    position: [f32; 3],
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CameraJson {
    target: Option<[f32; 3]>,
    view_height: Option<f32>,
    azimuth: Option<f32>,
    elevation: Option<f32>,
}

#[derive(Deserialize)]
struct GraveyardJson {
    station: String,
    gap: f32,
    width: f32,
    depth: f32,
}

#[derive(Deserialize)]
struct WallJson {
    inside: [f32; 3],
    parts: Vec<PartJson>,
}

#[derive(Deserialize)]
struct SpaceJson {
    base: Vec<PartJson>,
    #[serde(default)]
    walls: Vec<WallJson>,
    lights: Vec<LightJson>,
    ambient: AmbientJson,
    sun: SunJson,
    background: String,
    stations: Vec<StationJson>,
    waypoints: Vec<WaypointJson>,
    paths: Vec<(String, String)>,
    camera: CameraJson,
    #[serde(default)]
    graveyard: Option<GraveyardJson>,
    budget: Option<f64>,
}

pub(crate) fn parse<'a, T: Deserialize<'a>>(json: &'a str) -> Result<T, ScriptError> {
    serde_json::from_str(json).map_err(|e| ScriptError::Invalid(format!("could not read the script result ({e})")))
}

fn color(hex: &str) -> Result<[f32; 3], ScriptError> {
    srgb_hex(hex).map_err(|e| ScriptError::Invalid(e.to_string()))
}

pub(crate) fn budget(declared: Option<f64>, default: usize, max: Option<usize>) -> Result<usize, ScriptError> {
    let Some(b) = declared else { return Ok(default) };
    if b.is_nan() || b < 1.0 {
        return Err(ScriptError::Invalid(format!("budget.triangles must be at least 1, got {b}")));
    }
    match max {
        Some(max) if b > max as f64 => Err(ScriptError::Invalid(format!("budget.triangles is {b}, at most {max} are allowed"))),
        _ => Ok(b.min(usize::MAX as f64) as usize),
    }
}

pub(crate) fn count(parts: &[PartJson], meshes: &[Mesh]) -> usize {
    parts.iter().map(|p| meshes.get(p.mesh).map_or(0, Mesh::triangle_count)).sum()
}

pub(crate) fn bake_parts(parts: &[PartJson], meshes: &[Mesh]) -> Result<Mesh, ScriptError> {
    let mut out = Mesh::new();
    for p in parts {
        let src = meshes.get(p.mesh).ok_or_else(|| ScriptError::Invalid(format!("unknown mesh {}", p.mesh)))?;
        let cols: [f32; 16] =
            p.m.iter()
                .map(|&v| v as f32)
                .collect::<Vec<_>>()
                .try_into()
                .map_err(|_| ScriptError::Invalid("a part transform needs 16 numbers".into()))?;
        let material = Material {
            color: color(&p.color)?,
            finish: Finish::parse(&p.finish).ok_or_else(|| ScriptError::Invalid(format!("unknown finish {:?}", p.finish)))?,
            emissive: p.glow.clamp(0.0, 1.0) as f32,
        };
        out.append(&src.transformed(Mat4::from_cols_array(&cols)).painted(&material));
    }
    Ok(out)
}

pub(crate) fn motion(m: MotionJson) -> Motion {
    match m {
        MotionJson::Still => Motion::Still,
        MotionJson::Bob { amplitude, period } => Motion::Bob { amplitude, period },
        MotionJson::Hop { count, height, period } => Motion::Hop { count, height, period },
        MotionJson::Breathe { period, depth } => Motion::Breathe { period, depth },
        MotionJson::Spin { period } => Motion::Spin { period },
        MotionJson::Wave { period, angle } => Motion::Wave { period, angle_deg: angle },
        MotionJson::Orbit { radius, period } => Motion::Orbit { radius, period },
    }
}

fn anim(a: AnimJson) -> Anim {
    Anim { motion: motion(a.motion), face_viewer: a.face_viewer, eyes_closed: a.eyes_closed }
}

pub(crate) fn figure(raw: &Raw, limits: &Limits) -> Result<FigureDesc, ScriptError> {
    let spec: FigureJson = parse(&raw.json)?;

    let slots = [
        ("anchors.ring", spec.anchors.ring.is_some()),
        ("anchors.face", spec.anchors.face.is_some()),
        ("animations.working", spec.animations.working.is_some()),
        ("animations.waiting", spec.animations.waiting.is_some()),
        ("animations.resting", spec.animations.resting.is_some()),
    ];
    let missing: Vec<String> = slots.iter().filter(|(_, ok)| !ok).map(|(name, _)| name.to_string()).collect();
    let (a, n) = (spec.anchors, spec.animations);
    let (Some(ring), Some(face), Some(working), Some(waiting), Some(resting)) = (a.ring, a.face, n.working, n.waiting, n.resting) else {
        return Err(ScriptError::MissingSlots(missing));
    };
    let budget = budget(spec.budget, FIGURE_BUDGET, Some(FIGURE_BUDGET_MAX))?;

    let shows = spec.params.iter().flat_map(|p| &p.shows);
    let total = count(&spec.base, &raw.meshes) + shows.map(|s| count(&s.parts, &raw.meshes)).sum::<usize>();
    if total > limits.hard_triangle_cap {
        return Err(ScriptError::HardCap(limits.hard_triangle_cap));
    }
    if total > budget {
        return Err(ScriptError::OverBudget { used: total, budget });
    }

    let base = bake_parts(&spec.base, &raw.meshes)?;
    if base.is_empty() {
        return Err(ScriptError::Invalid("the figure has no visible geometry, list its parts in figure({ parts: [...] })".into()));
    }
    let mut bounds = base.bounds();
    let mut params = Vec::with_capacity(spec.params.len());
    let mut param_parts = Vec::new();
    for p in spec.params {
        for s in p.shows {
            let mesh = bake_parts(&s.parts, &raw.meshes)?;
            bounds = bounds.union(mesh.bounds());
            param_parts.push(ParamPart { param: p.name.clone(), when: s.when, mesh, pivot: Vec3::from(s.pivot), anim: s.anim.map(motion) });
        }
        let kind = match p.kind {
            ParamKindJson::Toggle { default } => ParamKind::Toggle { default },
            ParamKindJson::Choice { values, default } => ParamKind::Choice { values, default },
            ParamKindJson::Number { min, max, default } => ParamKind::Number { min, max, default },
            ParamKindJson::Count { default } => ParamKind::Count { default },
            ParamKindJson::Text { default } => ParamKind::Text { default },
        };
        params.push(ParamDef { name: p.name, kind });
    }

    Ok(FigureDesc {
        base,
        param_parts,
        ring_anchor: Vec3::from(ring),
        face_anchor: Vec3::from(face),
        animations: Animations { working: anim(working), waiting: anim(waiting), resting: anim(resting) },
        params,
        triangles: total,
        budget,
        bounds,
    })
}

pub(crate) fn space(raw: &Raw, limits: &Limits) -> Result<SpaceDesc, ScriptError> {
    let spec: SpaceJson = parse(&raw.json)?;
    let budget = budget(spec.budget, SPACE_BUDGET, None)?;
    let total = count(&spec.base, &raw.meshes) + spec.walls.iter().map(|w| count(&w.parts, &raw.meshes)).sum::<usize>();
    if total > limits.hard_triangle_cap {
        return Err(ScriptError::HardCap(limits.hard_triangle_cap));
    }
    if total > budget {
        return Err(ScriptError::OverBudget { used: total, budget });
    }

    let mut waypoints: Vec<(String, Vec3)> = Vec::with_capacity(spec.stations.len() + spec.waypoints.len());
    waypoints.extend(spec.stations.iter().map(|s| (s.id.clone(), Vec3::from(s.position))));
    waypoints.extend(spec.waypoints.iter().map(|w| (w.id.clone(), Vec3::from(w.position))));
    let mut index: HashMap<&str, usize> = HashMap::new();
    for (i, (id, _)) in waypoints.iter().enumerate() {
        if index.insert(id.as_str(), i).is_some() {
            return Err(ScriptError::Invalid(format!("the id `{id}` is used by a station and a waypoint")));
        }
    }

    let mut problems = Vec::new();
    let mut adjacent = vec![Vec::new(); waypoints.len()];
    for (i, (a, b)) in spec.paths.iter().enumerate() {
        for id in [a, b] {
            if !index.contains_key(id.as_str()) {
                problems.push(format!("paths[{i}] names the unknown id `{id}`"));
            }
        }
        if let (Some(&x), Some(&y)) = (index.get(a.as_str()), index.get(b.as_str())) {
            adjacent[x].push(y);
            adjacent[y].push(x);
        }
    }
    if problems.is_empty() && !spec.stations.is_empty() {
        let mut seen = vec![false; waypoints.len()];
        let mut queue = VecDeque::from([0]);
        seen[0] = true;
        while let Some(i) = queue.pop_front() {
            for &j in &adjacent[i] {
                if !seen[j] {
                    seen[j] = true;
                    queue.push_back(j);
                }
            }
        }
        let from = &spec.stations[0].id;
        problems.extend(
            spec.stations
                .iter()
                .enumerate()
                .filter(|&(i, _)| !seen[i])
                .map(|(_, s)| format!("the station `{}` cannot be reached from `{from}`", s.id)),
        );
    }
    if let Some(g) = &spec.graveyard
        && !spec.stations.iter().any(|s| s.id == g.station)
    {
        problems.push(format!("graveyard.station names `{}`, which is not a station", g.station));
    }
    let mut tool_home: HashMap<&str, &str> = HashMap::new();
    for s in &spec.stations {
        for tool in &s.tools {
            if let Some(other) = tool_home.insert(tool.as_str(), s.id.as_str()) {
                problems.push(format!("the tool `{tool}` is listed at both `{other}` and `{}`", s.id));
            }
        }
    }
    if !problems.is_empty() {
        return Err(ScriptError::Invalid(problems.join(", ")));
    }

    let mesh = bake_parts(&spec.base, &raw.meshes)?;
    let mut walls = Vec::with_capacity(spec.walls.len());
    for (i, w) in spec.walls.iter().enumerate() {
        let inside = Vec3::new(w.inside[0], 0.0, w.inside[2])
            .try_normalize()
            .ok_or_else(|| ScriptError::Invalid(format!("walls[{i}].inside must point along the floor into the room")))?;
        walls.push(SpaceWall { mesh: bake_parts(&w.parts, &raw.meshes)?, inside });
    }
    let bounds = walls.iter().fold(mesh.bounds(), |b, w| b.union(w.mesh.bounds()));
    if bounds.is_empty() {
        return Err(ScriptError::Invalid("the space has no geometry, list its parts in space({ parts: [...] })".into()));
    }
    let lights = spec
        .lights
        .iter()
        .map(|l| Ok(LightDesc { position: Vec3::from(l.position), color: color(&l.color)?, intensity: l.intensity, radius: l.radius }))
        .collect::<Result<Vec<_>, ScriptError>>()?;
    let default_sun = Vec3::new(-0.4, -1.0, -0.3).normalize();
    let camera = CameraHint {
        target: spec.camera.target.map(Vec3::from).unwrap_or_else(|| bounds.center()),
        view_height: spec.camera.view_height.unwrap_or_else(|| bounds.size().max_element() * 1.2),
        azimuth_deg: spec.camera.azimuth.unwrap_or(45.0),
        elevation_deg: spec.camera.elevation.unwrap_or(30.0),
    };

    Ok(SpaceDesc {
        lights,
        ambient_sky: color(&spec.ambient.sky)?,
        ambient_ground: color(&spec.ambient.ground)?,
        sun_direction: Vec3::from(spec.sun.direction).try_normalize().unwrap_or(default_sun),
        sun_color: color(&spec.sun.color)?,
        sun_intensity: spec.sun.intensity,
        background: color(&spec.background)?,
        stations: spec
            .stations
            .into_iter()
            .map(|s| Station { id: s.id, position: Vec3::from(s.position), facing_deg: s.facing, front: s.front, tools: s.tools })
            .collect(),
        waypoints,
        edges: spec.paths,
        camera,
        graveyard: spec.graveyard.map(|g| GraveyardPlot { station: g.station, gap: g.gap, width: g.width, depth: g.depth }),
        triangles: total,
        budget,
        bounds,
        mesh,
        walls,
    })
}
