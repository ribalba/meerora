//! Devices (D7, D8, D20): objects of the space with a shape, a live state
//! binding and actions.
//!
//! A device script ends with `export default device({...})`. The shape uses
//! the figure machinery, parts and params with parts shown per value and
//! their animations (D24). On top of that a device says where it stands,
//! which state source drives which param (`bind`), what a click does
//! (`actions`) and the line its card shows (`status`).
//!
//! The script itself has no system access, like every script (D20). It only
//! names a source such as `network.airplane_mode` and a declared capability
//! such as `network.airplane_mode.write`. The agent daemon reads the sources,
//! asks for approval and runs the action (D8).

use std::collections::BTreeMap;
use std::fmt;

use meerora_geom::{Mesh, Vec3, srgb_hex};
use serde::Deserialize;
use serde_json::Value;

use crate::bake::{MotionJson, ParamKindJson, PartJson, bake_parts, budget, count, motion, parse};
use crate::host::Raw;
use crate::{Limits, ParamDef, ParamKind, ParamPart, ScriptError, SpaceWall};

/// Triangle budget of a device when the script declares none (D2).
pub const DEVICE_BUDGET: usize = 1500;
/// The largest budget a device may declare.
pub const DEVICE_BUDGET_MAX: usize = 3000;
/// Longest text a `text` param of a device takes.
const TEXT_MAX: usize = 200;
/// Longest capability name, the same as the capability broker's.
const CAPABILITY_MAX: usize = 64;
/// The views of the app an `opens()` action may open.
const VIEWS: [&str; 1] = ["feed"];
/// The values of an `agent.<id>.ring` source (D3).
pub const RING_VALUES: [&str; 3] = ["working", "needs_you", "resting"];

#[derive(Clone, Debug, PartialEq)]
pub struct DeviceDesc {
    pub name: String,
    /// Always visible geometry, standing on the origin and facing +z.
    pub base: Mesh,
    pub param_parts: Vec<ParamPart>,
    pub params: Vec<ParamDef>,
    pub place: DevicePlace,
    pub bindings: Vec<Binding>,
    pub actions: Vec<DeviceAction>,
    pub status: Option<Status>,
    /// Params the user sets on the device card, such as what a graph shows.
    pub settings: Vec<DeviceSetting>,
    /// A live graph the app draws from the recent values of a param.
    pub display: Option<GraphDisplay>,
    pub triangles: usize,
    pub budget: usize,
    /// Bounds of the base and all param parts, for picking.
    pub bounds: meerora_geom::Aabb,
}

/// A param of a device that the user sets on its card (`.setting(label)`),
/// for example which metric a graph shows. The daemon keeps the choice in
/// `devices/settings.toml` of the room and hands it to the app like a bound
/// value. Bindings made with `per()` follow it.
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceSetting {
    pub param: String,
    /// What the card writes before the choices.
    pub label: String,
    /// Card labels for some values, by value (`"true"` and `"false"` for a toggle).
    pub labels: BTreeMap<String, String>,
}

/// `display: graph({...})`: the app keeps the recent values of a number
/// param and draws them as a line inside a rectangle of the device. The
/// rectangle faces +z in the device's own axes, centred on `at`.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphDisplay {
    /// The number param whose values the line follows.
    pub value: String,
    pub at: Vec3,
    pub width: f32,
    pub height: f32,
    /// How many values the line keeps, the newest on the right.
    pub points: usize,
    /// The values at the bottom and the top edge. Values outside are clamped.
    pub min: f64,
    pub max: f64,
    /// Linear RGB of the line, which glows.
    pub color: [f32; 3],
    pub thickness: f32,
}

/// Where a device stands in the space.
#[derive(Clone, Debug, PartialEq)]
pub struct DevicePlace {
    /// A station of the space to stand next to. `position` is then the
    /// offset from the station, in the axes of the space.
    pub station: Option<String>,
    pub position: Vec3,
    /// Degrees around y with 0 = +z. A device at a station without its own
    /// facing takes the station's.
    pub facing_deg: Option<f32>,
    /// Set for a device that hangs on a wall. It is the wall's `inside`
    /// direction, a unit vector along the floor. The device is cut away together with
    /// the wall (see [`SpaceWall::faces`]).
    pub wall: Option<Vec3>,
}

impl DevicePlace {
    /// False while the device hangs on a wall that a camera looking from
    /// `toward_viewer` cuts away. The rule is the one of the walls.
    pub fn shown(&self, toward_viewer: Vec3) -> bool {
        self.wall.is_none_or(|inside| SpaceWall { mesh: Mesh::new(), inside }.faces(toward_viewer))
    }
}

/// A state source a device can bind (D7).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Source {
    /// `system.disk`, the share of the home filesystem in use, from 0 to 1.
    Disk,
    /// `network.airplane_mode`, true while every radio is off.
    AirplaneMode,
    /// `clock.time`, the local time in minutes since midnight, 0 to 1439.
    Clock,
    /// `agent.<id>.param.<name>`, a figure param of an agent (D24).
    AgentParam { agent: String, name: String },
    /// `agent.<id>.ring`, one of `working`, `needs_you` and `resting` (D3).
    AgentRing { agent: String },
    /// `system.cpu`, the share of CPU time in use since the last reading, 0 to 1.
    Cpu,
    /// `system.memory`, the share of memory that is available, 0 to 1.
    Memory,
    /// `system.network`, bytes per second received and sent by all network
    /// interfaces but loopback, since the last reading.
    Network,
}

impl Source {
    pub fn parse(text: &str) -> Result<Source, String> {
        let known = "The sources are system.disk, system.cpu, system.memory, system.network, network.airplane_mode, clock.time, \
                     agent.<id>.param.<name> and agent.<id>.ring.";
        let agent_id = |id: &str| {
            id.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
                && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        };
        let param_name = |n: &str| {
            n.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        };
        match text {
            "system.disk" => return Ok(Source::Disk),
            "system.cpu" => return Ok(Source::Cpu),
            "system.memory" => return Ok(Source::Memory),
            "system.network" => return Ok(Source::Network),
            "network.airplane_mode" => return Ok(Source::AirplaneMode),
            "clock.time" => return Ok(Source::Clock),
            _ => {}
        }
        if let Some(rest) = text.strip_prefix("agent.") {
            if let Some(agent) = rest.strip_suffix(".ring")
                && agent_id(agent)
            {
                return Ok(Source::AgentRing { agent: agent.into() });
            }
            if let Some((agent, name)) = rest.split_once(".param.")
                && agent_id(agent)
                && param_name(name)
            {
                return Ok(Source::AgentParam { agent: agent.into(), name: name.into() });
            }
        }
        Err(format!("{text:?} is not a source. {known}"))
    }

    /// The capability an agent's device needs before it may bind this
    /// source (D8). The clock and the agents are visible in the space
    /// anyway, so they need none.
    pub fn read_capability(&self) -> Option<String> {
        match self {
            Source::Disk => Some("system.disk.read".into()),
            Source::Cpu => Some("system.cpu.read".into()),
            Source::Memory => Some("system.memory.read".into()),
            Source::Network => Some("system.network.read".into()),
            Source::AirplaneMode => Some("network.airplane_mode.read".into()),
            Source::Clock | Source::AgentParam { .. } | Source::AgentRing { .. } => None,
        }
    }

    fn kind(&self) -> Kind {
        match self {
            Source::Disk | Source::Clock | Source::Cpu | Source::Memory | Source::Network => Kind::Number,
            Source::AirplaneMode => Kind::Bool,
            Source::AgentParam { .. } => Kind::Any,
            Source::AgentRing { .. } => Kind::Text(Some(RING_VALUES.iter().map(|s| s.to_string()).collect())),
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Source::Disk => f.write_str("system.disk"),
            Source::Cpu => f.write_str("system.cpu"),
            Source::Memory => f.write_str("system.memory"),
            Source::Network => f.write_str("system.network"),
            Source::AirplaneMode => f.write_str("network.airplane_mode"),
            Source::Clock => f.write_str("clock.time"),
            Source::AgentParam { agent, name } => write!(f, "agent.{agent}.param.{name}"),
            Source::AgentRing { agent } => write!(f, "agent.{agent}.ring"),
        }
    }
}

/// One step between the source and the param.
#[derive(Clone, Debug, PartialEq)]
pub enum Map {
    /// Whole times `n` fits into the number.
    Div(f64),
    /// What is left after dividing by `n`, never negative.
    Mod(f64),
    /// `values[0]` below `bounds[0]`, `values[1]` from there to `bounds[1]`
    /// and so on. The last value from the last bound on.
    Steps { bounds: Vec<f64>, values: Vec<Value> },
    /// True when the value equals this one.
    Is(Value),
    /// A share from 0 to 1 as a text like `42%`.
    Percent,
    /// Minutes since midnight as a text like `09:30`.
    Time,
    /// A number from 0 up onto 0 to 1 on a log scale that reaches 1 at
    /// `max`, for readings that span orders of magnitude like network traffic.
    Log(f64),
    /// A number of bytes as a text like `1.2 MB`, in steps of 1000.
    Bytes,
}

impl Map {
    fn apply(&self, value: &Value) -> Option<Value> {
        match self {
            Map::Div(n) => whole((value.as_f64()? / n).floor()),
            Map::Mod(n) => whole(value.as_f64()?.rem_euclid(*n)),
            Map::Steps { bounds, values } => {
                let x = value.as_f64()?;
                values.get(bounds.iter().take_while(|b| x >= **b).count()).cloned()
            }
            Map::Is(want) => Some(Value::Bool(match (value.as_f64(), want.as_f64()) {
                (Some(a), Some(b)) => a == b,
                _ => value == want,
            })),
            Map::Percent => Some(Value::String(format!("{}%", (value.as_f64()? * 100.0).round() as i64))),
            Map::Time => {
                let minutes = value.as_f64()?.floor() as i64;
                Some(Value::String(format!("{:02}:{:02}", minutes.div_euclid(60).rem_euclid(24), minutes.rem_euclid(60))))
            }
            Map::Log(max) => {
                let x = value.as_f64()?.max(0.0);
                serde_json::Number::from_f64(((1.0 + x).ln() / (1.0 + max).ln()).clamp(0.0, 1.0)).map(Value::Number)
            }
            Map::Bytes => Some(Value::String(bytes(value.as_f64()?))),
        }
    }

    /// The kind of value this step makes from `input`.
    fn kind(&self, input: &Kind) -> Result<Kind, String> {
        let number = |what: &str| match input {
            Kind::Number | Kind::Any => Ok(()),
            other => Err(format!("{what} needs a number, but gets {other}")),
        };
        Ok(match self {
            Map::Div(_) => number("div()").map(|()| Kind::Number)?,
            Map::Mod(_) => number("mod()").map(|()| Kind::Number)?,
            Map::Percent => number("percent()").map(|()| Kind::Text(None))?,
            Map::Time => number("time()").map(|()| Kind::Text(None))?,
            Map::Log(_) => number("log()").map(|()| Kind::Number)?,
            Map::Bytes => number("bytes()").map(|()| Kind::Text(None))?,
            Map::Is(_) => Kind::Bool,
            Map::Steps { values, .. } => {
                number("steps()")?;
                if values.iter().all(Value::is_boolean) {
                    Kind::Bool
                } else if values.iter().all(Value::is_number) {
                    Kind::Number
                } else if values.iter().all(Value::is_string) {
                    Kind::Text(Some(values.iter().filter_map(|v| v.as_str().map(String::from)).collect()))
                } else {
                    return Err("steps() values must all be texts, all numbers or all true and false".into());
                }
            }
        })
    }
}

/// A number of bytes as people read it, `512 B`, `1.2 kB`, `34 MB`.
fn bytes(x: f64) -> String {
    let x = x.max(0.0);
    let units = ["B", "kB", "MB", "GB", "TB"];
    let mut value = x;
    let mut unit = 0;
    while value >= 1000.0 && unit < units.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 || value >= 100.0 { format!("{} {}", value.round() as i64, units[unit]) } else { format!("{value:.1} {}", units[unit]) }
}

/// A whole number as a JSON integer, so it matches `count` params and the
/// values of `shows(part, 3)`.
fn whole(x: f64) -> Option<Value> {
    if !x.is_finite() {
        return None;
    }
    if x.fract() == 0.0 && x.abs() < 9.0e15 {
        return Some(Value::from(x as i64));
    }
    serde_json::Number::from_f64(x).map(Value::Number)
}

/// What a binding hands to a param.
#[derive(Clone, Debug, PartialEq)]
enum Kind {
    Bool,
    Number,
    /// A text, from this set when it is known.
    Text(Option<Vec<String>>),
    /// The type is known only at runtime, for example an agent's param.
    Any,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Kind::Bool => "true or false",
            Kind::Number => "a number",
            Kind::Text(_) => "a text",
            Kind::Any => "any value",
        })
    }
}

/// A param that follows a state source (D7).
#[derive(Clone, Debug, PartialEq)]
pub struct Binding {
    pub param: String,
    pub source: Source,
    pub maps: Vec<Map>,
    /// Made by `per()`: the binding holds only while this setting param has
    /// this value.
    pub when: Option<(String, Value)>,
}

impl Binding {
    /// The param value for a reading of the source. None when a step does
    /// not fit the reading, for example `div()` of a text.
    pub fn apply(&self, reading: &Value) -> Option<Value> {
        self.maps.iter().try_fold(reading.clone(), |value, map| map.apply(&value))
    }
}

/// What an action does to the state its capability writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    Toggle,
    On,
    Off,
}

impl Effect {
    fn parse(text: &str) -> Option<Effect> {
        match text {
            "toggle" => Some(Effect::Toggle),
            "on" => Some(Effect::On),
            "off" => Some(Effect::Off),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Effect::Toggle => "toggle",
            Effect::On => "on",
            Effect::Off => "off",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ActionKind {
    /// Changes the state behind a declared capability (D8), for example
    /// `network.airplane_mode.write`. The daemon runs it after approval.
    Capability { capability: String, effect: Effect },
    /// Opens a view of the app, such as the feed. No system access.
    Opens(String),
}

/// What a click on the device's card can do.
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceAction {
    pub id: String,
    pub label: String,
    pub kind: ActionKind,
}

impl DeviceAction {
    pub fn capability(&self) -> Option<&str> {
        match &self.kind {
            ActionKind::Capability { capability, .. } => Some(capability),
            ActionKind::Opens(_) => None,
        }
    }
}

/// The line of the device card that says what state it shows.
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    /// A text with `{param}` placeholders.
    Text(String),
    /// One text per value of a param, each with placeholders.
    PerValue { param: String, texts: Vec<(Value, String)> },
}

impl ParamDef {
    /// The value before anything set it.
    pub fn default_value(&self) -> Value {
        match &self.kind {
            ParamKind::Toggle { default } => (*default).into(),
            ParamKind::Choice { default, .. } => default.clone().into(),
            ParamKind::Number { default, .. } => whole(*default).unwrap_or(Value::Null),
            ParamKind::Count { default } => (*default).into(),
            ParamKind::Text { default } => default.clone().into(),
        }
    }

    /// Whether `value` is a valid value of the param.
    pub fn accepts(&self, value: &Value) -> bool {
        match &self.kind {
            ParamKind::Toggle { .. } => value.is_boolean(),
            ParamKind::Choice { values, .. } => value.as_str().is_some_and(|v| values.iter().any(|c| c == v)),
            ParamKind::Number { min, max, .. } => value.as_f64().is_some_and(|v| (*min..=*max).contains(&v)),
            ParamKind::Count { .. } => value.as_u64().is_some_and(|v| v <= u64::from(u32::MAX)),
            ParamKind::Text { .. } => value.as_str().is_some_and(|v| v.chars().count() <= TEXT_MAX),
        }
    }
}

impl DeviceDesc {
    pub fn param(&self, name: &str) -> Option<&ParamDef> {
        self.params.iter().find(|p| p.name == name)
    }

    pub fn setting(&self, name: &str) -> Option<&DeviceSetting> {
        self.settings.iter().find(|s| s.param == name)
    }

    /// The bindings that hold for these values of the settings. A setting
    /// missing from `values` counts with its default.
    pub fn active_bindings<'a>(&'a self, values: &'a BTreeMap<String, Value>) -> impl Iterator<Item = &'a Binding> + 'a {
        self.bindings.iter().filter(move |b| match &b.when {
            None => true,
            Some((setting, want)) => {
                values.get(setting).cloned().or_else(|| self.param(setting).map(ParamDef::default_value)).as_ref() == Some(want)
            }
        })
    }

    /// The card label of a value of a setting.
    pub fn setting_label(&self, setting: &str, value: &Value) -> String {
        let key = match value {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        if let Some(label) = self.setting(setting).and_then(|s| s.labels.get(&key)) {
            return label.clone();
        }
        let mut chars = key.chars();
        chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
    }

    /// Every capability the device names, for its actions and for reading
    /// its sources (D8). Sorted, each once.
    pub fn capabilities(&self) -> Vec<String> {
        let mut all: Vec<String> = self.actions.iter().filter_map(|a| a.capability().map(String::from)).collect();
        all.extend(self.bindings.iter().filter_map(|b| b.source.read_capability()));
        all.sort();
        all.dedup();
        all
    }

    /// The status line for the current param values. Params missing from
    /// `values` count with their default.
    pub fn status_text(&self, values: &BTreeMap<String, Value>) -> Option<String> {
        let value = |name: &str| values.get(name).cloned().or_else(|| self.param(name).map(ParamDef::default_value));
        let template = match self.status.as_ref()? {
            Status::Text(text) => text,
            Status::PerValue { param, texts } => {
                let current = value(param)?;
                &texts.iter().find(|(v, _)| *v == current)?.1
            }
        };
        Some(fill(template, |name| value(name).map(|v| shown(&v))))
    }
}

/// A param value as the status line shows it.
pub fn shown(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Bool(true) => "on".into(),
        Value::Bool(false) => "off".into(),
        Value::Number(n) => match n.as_f64() {
            Some(x) if x.fract() == 0.0 => format!("{}", x as i64),
            Some(x) => format!("{x:.2}").trim_end_matches('0').to_string(),
            None => n.to_string(),
        },
        other => other.to_string(),
    }
}

/// `template` with every `{name}` replaced. Unknown names stay as they are.
fn fill(template: &str, value: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('}').map(|end| (&after[..end], &after[end + 1..])) {
            Some((name, tail)) => {
                match value(name) {
                    Some(v) => out.push_str(&v),
                    None => {
                        out.push('{');
                        out.push_str(name);
                        out.push('}');
                    }
                }
                rest = tail;
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// The `{name}` placeholders of a template.
fn placeholders(template: &str) -> Vec<&str> {
    let mut names = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else { break };
        names.push(&after[..end]);
        rest = &after[end + 1..];
    }
    names
}

/// Whether `name` is a capability name as the capability broker wants it,
/// lowercase words of letters, digits and `_` joined by dots, at most 64
/// characters.
pub fn capability_name_ok(name: &str) -> bool {
    let word = |w: &str| {
        w.starts_with(|c: char| c.is_ascii_lowercase()) && w.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    };
    name.len() <= CAPABILITY_MAX && name.split('.').all(word)
}

// ------------------------------------------------------------------- baking

#[derive(Deserialize)]
struct ShowJson {
    when: Value,
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
struct PlaceJson {
    at: Option<[f32; 3]>,
    station: Option<String>,
    offset: Option<[f32; 3]>,
    facing: Option<f32>,
    wall: Option<[f32; 3]>,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
enum MapJson {
    Div { n: f64 },
    Mod { n: f64 },
    Steps { bounds: Vec<f64>, values: Vec<Value> },
    Is { value: Value },
    Percent,
    Time,
    Log { max: f64 },
    Bytes,
}

#[derive(Deserialize)]
struct WhenJson {
    setting: String,
    value: String,
}

#[derive(Deserialize)]
struct BindJson {
    param: String,
    source: String,
    maps: Vec<MapJson>,
    when: Option<WhenJson>,
}

#[derive(Deserialize)]
struct SettingJson {
    param: String,
    label: String,
    labels: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct GraphJson {
    value: String,
    at: [f32; 3],
    width: f32,
    height: f32,
    points: usize,
    min: f64,
    max: f64,
    color: String,
    thickness: f32,
}

#[derive(Deserialize)]
struct ActionJson {
    id: String,
    label: String,
    capability: Option<String>,
    does: Option<String>,
    opens: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StatusJson {
    Text { template: String },
    PerValue { param: String, texts: BTreeMap<String, String> },
}

#[derive(Deserialize)]
struct DeviceJson {
    name: String,
    base: Vec<PartJson>,
    params: Vec<ParamJson>,
    place: PlaceJson,
    bind: Vec<BindJson>,
    #[serde(default)]
    settings: Vec<SettingJson>,
    display: Option<GraphJson>,
    actions: Vec<ActionJson>,
    status: Option<StatusJson>,
    budget: Option<f64>,
}

fn param_kind(kind: ParamKindJson) -> ParamKind {
    match kind {
        ParamKindJson::Toggle { default } => ParamKind::Toggle { default },
        ParamKindJson::Choice { values, default } => ParamKind::Choice { values, default },
        ParamKindJson::Number { min, max, default } => ParamKind::Number { min, max, default },
        ParamKindJson::Count { default } => ParamKind::Count { default },
        ParamKindJson::Text { default } => ParamKind::Text { default },
    }
}

/// Whether a binding that hands over `kind` fits the param. Returns why not.
fn fits(kind: &Kind, def: &ParamDef) -> Result<(), String> {
    let ok = match (&def.kind, kind) {
        (_, Kind::Any) => true,
        (ParamKind::Toggle { .. }, Kind::Bool) => true,
        (ParamKind::Number { .. } | ParamKind::Count { .. }, Kind::Number) => true,
        (ParamKind::Text { .. }, Kind::Text(_)) => true,
        (ParamKind::Choice { values, .. }, Kind::Text(set)) => {
            if let Some(missing) = set.iter().flatten().find(|v| !values.contains(v)) {
                return Err(format!("it can give {missing:?}, which is not one of the choices {}", values.join(", ")));
            }
            true
        }
        _ => false,
    };
    if ok { Ok(()) } else { Err(format!("it gives {kind}, which does not fit the param")) }
}

/// The value a status text key stands for, for the param `def`.
fn key_value(key: &str, def: &ParamDef) -> Option<Value> {
    let value = match &def.kind {
        ParamKind::Toggle { .. } => Value::Bool(key.parse::<bool>().ok()?),
        ParamKind::Count { .. } => Value::from(key.parse::<u64>().ok()?),
        ParamKind::Number { .. } => whole(key.parse::<f64>().ok()?)?,
        ParamKind::Choice { .. } | ParamKind::Text { .. } => Value::String(key.into()),
    };
    def.accepts(&value).then_some(value)
}

pub(crate) fn bake(raw: &Raw, limits: &Limits) -> Result<DeviceDesc, ScriptError> {
    let spec: DeviceJson = parse(&raw.json)?;
    let budget = budget(spec.budget, DEVICE_BUDGET, Some(DEVICE_BUDGET_MAX))?;
    let shows = spec.params.iter().flat_map(|p| &p.shows);
    let total = count(&spec.base, &raw.meshes) + shows.map(|s| count(&s.parts, &raw.meshes)).sum::<usize>();
    if total > limits.hard_triangle_cap {
        return Err(ScriptError::HardCap(limits.hard_triangle_cap));
    }
    if total > budget {
        return Err(ScriptError::OverBudget { used: total, budget });
    }

    let base = bake_parts(&spec.base, &raw.meshes)?;
    let mut bounds = base.bounds();
    let mut params = Vec::with_capacity(spec.params.len());
    let mut param_parts = Vec::new();
    for p in spec.params {
        for s in p.shows {
            let mesh = bake_parts(&s.parts, &raw.meshes)?;
            bounds = bounds.union(mesh.bounds());
            param_parts.push(ParamPart { param: p.name.clone(), when: s.when, mesh, pivot: Vec3::from(s.pivot), anim: s.anim.map(motion) });
        }
        params.push(ParamDef { name: p.name, kind: param_kind(p.kind) });
    }
    if bounds.is_empty() {
        return Err(ScriptError::Invalid("the device has no visible geometry, list its parts in device({ parts: [...] })".into()));
    }

    let mut problems = Vec::new();
    let place = DevicePlace {
        position: Vec3::from(spec.place.at.or(spec.place.offset).unwrap_or([0.0; 3])),
        station: spec.place.station,
        facing_deg: spec.place.facing,
        wall: spec.place.wall.and_then(|w| Vec3::new(w[0], 0.0, w[2]).try_normalize()),
    };
    let param = |name: &str| params.iter().find(|p| p.name == name);

    let settings: Vec<DeviceSetting> =
        spec.settings.into_iter().map(|s| DeviceSetting { param: s.param, label: s.label, labels: s.labels }).collect();
    let is_setting = |name: &str| settings.iter().any(|s| s.param == name);

    let mut bindings = Vec::with_capacity(spec.bind.len());
    for b in spec.bind {
        if is_setting(&b.param) {
            problems.push(format!("bind.{} is a setting, which the user sets on the card, so no source may drive it", b.param));
            continue;
        }
        let when = match b.when {
            None => None,
            Some(w) => match param(&w.setting) {
                Some(def) if is_setting(&w.setting) => {
                    let value = match def.kind {
                        ParamKind::Toggle { .. } => w.value.parse::<bool>().ok().map(Value::Bool),
                        _ => Some(Value::String(w.value.clone())),
                    };
                    match value.filter(|v| def.accepts(v)) {
                        Some(v) => Some((w.setting, v)),
                        None => {
                            problems.push(format!(
                                "bind.{}: per({:?}) has a case {:?}, which is not a value of it",
                                b.param, w.setting, w.value
                            ));
                            continue;
                        }
                    }
                }
                _ => {
                    problems.push(format!("bind.{}: per({:?}) needs a param marked with .setting()", b.param, w.setting));
                    continue;
                }
            },
        };
        let source = match Source::parse(&b.source) {
            Ok(source) => source,
            Err(e) => {
                problems.push(format!("bind.{}: {e}", b.param));
                continue;
            }
        };
        let maps: Vec<Map> = b
            .maps
            .into_iter()
            .map(|m| match m {
                MapJson::Div { n } => Map::Div(n),
                MapJson::Mod { n } => Map::Mod(n),
                MapJson::Steps { bounds, values } => Map::Steps { bounds, values },
                MapJson::Is { value } => Map::Is(value),
                MapJson::Percent => Map::Percent,
                MapJson::Time => Map::Time,
                MapJson::Log { max } => Map::Log(max),
                MapJson::Bytes => Map::Bytes,
            })
            .collect();
        let kind = maps.iter().try_fold(source.kind(), |kind, map| map.kind(&kind));
        match (kind, param(&b.param)) {
            (Err(e), _) => problems.push(format!("bind.{}: {e}", b.param)),
            (Ok(_), None) => problems.push(format!("bind.{} names no param", b.param)),
            (Ok(kind), Some(def)) => {
                if let Err(e) = fits(&kind, def) {
                    problems.push(format!("bind.{} cannot drive the param {}: {e}", b.param, b.param));
                }
            }
        }
        bindings.push(Binding { param: b.param, source, maps, when });
    }
    // A param follows one source, or one per value of a setting.
    for (i, b) in bindings.iter().enumerate() {
        for other in &bindings[..i] {
            if other.param != b.param {
                continue;
            }
            match (&other.when, &b.when) {
                (Some((s1, v1)), Some((s2, v2))) if s1 == s2 && v1 != v2 => {}
                _ => problems.push(format!("bind.{} has more than one source. Use per(setting, {{...}}) to switch between them", b.param)),
            }
        }
    }

    let display = match spec.display {
        None => None,
        Some(g) => {
            match param(&g.value).map(|d| &d.kind) {
                Some(ParamKind::Number { .. } | ParamKind::Count { .. }) => {}
                Some(_) => problems.push(format!("display: graph() value {:?} must be a number() or count() param", g.value)),
                None => problems.push(format!("display: graph() value {:?} names no param", g.value)),
            }
            let color = srgb_hex(&g.color).map_err(|e| ScriptError::Invalid(e.to_string()))?;
            Some(GraphDisplay {
                value: g.value,
                at: Vec3::from(g.at),
                width: g.width,
                height: g.height,
                points: g.points,
                min: g.min,
                max: g.max,
                color,
                thickness: g.thickness,
            })
        }
    };

    let mut actions = Vec::with_capacity(spec.actions.len());
    for a in spec.actions {
        let kind = match (a.capability, a.does, a.opens) {
            (Some(capability), Some(does), None) => {
                if !capability_name_ok(&capability) {
                    problems.push(format!(
                        "actions.{}: {capability:?} is not a capability name. Use lowercase words joined by dots, like network.airplane_mode.write",
                        a.id
                    ));
                }
                match Effect::parse(&does) {
                    Some(effect) => ActionKind::Capability { capability, effect },
                    None => {
                        problems.push(format!("actions.{}: {does:?} is not toggle, on or off", a.id));
                        continue;
                    }
                }
            }
            (None, None, Some(view)) if VIEWS.contains(&view.as_str()) => ActionKind::Opens(view),
            _ => {
                problems.push(format!("actions.{} must be action(...) or opens(...)", a.id));
                continue;
            }
        };
        actions.push(DeviceAction { id: a.id, label: a.label, kind });
    }

    let check_template = |what: &str, template: &str, problems: &mut Vec<String>| {
        for name in placeholders(template) {
            if param(name).is_none() {
                problems.push(format!("{what} shows {{{name}}}, but the device has no param {name}"));
            }
        }
    };
    let status = match spec.status {
        None => None,
        Some(StatusJson::Text { template }) => {
            check_template("status", &template, &mut problems);
            Some(Status::Text(template))
        }
        Some(StatusJson::PerValue { param: name, texts }) => match param(&name) {
            None => {
                problems.push(format!("status.{name} names no param"));
                None
            }
            Some(def) => {
                let mut list = Vec::with_capacity(texts.len());
                for (key, text) in texts {
                    check_template(&format!("status.{name}.{key}"), &text, &mut problems);
                    match key_value(&key, def) {
                        Some(value) => list.push((value, text)),
                        None => problems.push(format!("status.{name} has a text for {key:?}, which is not a value of the param")),
                    }
                }
                Some(Status::PerValue { param: name, texts: list })
            }
        },
    };
    if !problems.is_empty() {
        return Err(ScriptError::Invalid(problems.join(", ")));
    }

    Ok(DeviceDesc {
        name: spec.name,
        base,
        param_parts,
        params,
        place,
        bindings,
        actions,
        status,
        settings,
        display,
        triangles: total,
        budget,
        bounds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn binding(source: Source, maps: Vec<Map>) -> Binding {
        Binding { param: "p".into(), source, maps, when: None }
    }

    #[test]
    fn sources_parse_and_print() {
        for text in [
            "system.disk",
            "system.cpu",
            "system.memory",
            "system.network",
            "network.airplane_mode",
            "clock.time",
            "agent.wren.param.letter",
            "agent.rivet-2.ring",
        ] {
            assert_eq!(Source::parse(text).unwrap().to_string(), text);
        }
        assert_eq!(Source::parse("agent.wren.param.letter").unwrap(), Source::AgentParam { agent: "wren".into(), name: "letter".into() });
        for bad in ["system", "system.disk.write", "agent..ring", "agent.Wren.ring", "agent.wren.param.", "agent.wren.param.a-b", "clock"] {
            assert!(Source::parse(bad).unwrap_err().contains("not a source"), "{bad}");
        }
        assert_eq!(Source::Disk.read_capability().as_deref(), Some("system.disk.read"));
        assert_eq!(Source::Clock.read_capability(), None);
    }

    #[test]
    fn maps_turn_readings_into_values() {
        let hour = binding(Source::Clock, vec![Map::Div(60.0), Map::Mod(12.0)]);
        assert_eq!(hour.apply(&json!(14 * 60 + 35)), Some(json!(2)));
        let minute = binding(Source::Clock, vec![Map::Mod(60.0), Map::Div(5.0)]);
        assert_eq!(minute.apply(&json!(14 * 60 + 35)), Some(json!(7)));
        assert_eq!(binding(Source::Clock, vec![Map::Time]).apply(&json!(9 * 60 + 5)), Some(json!("09:05")));
        let leaves =
            binding(Source::Disk, vec![Map::Steps { bounds: vec![0.7, 0.9], values: vec![json!("full"), json!("thin"), json!("bare")] }]);
        assert_eq!(leaves.apply(&json!(0.2)), Some(json!("full")));
        assert_eq!(leaves.apply(&json!(0.7)), Some(json!("thin")));
        assert_eq!(leaves.apply(&json!(0.95)), Some(json!("bare")));
        assert_eq!(binding(Source::Disk, vec![Map::Percent]).apply(&json!(0.424)), Some(json!("42%")));
        let waiting = binding(Source::AgentRing { agent: "wren".into() }, vec![Map::Is(json!("needs_you"))]);
        assert_eq!(waiting.apply(&json!("needs_you")), Some(json!(true)));
        assert_eq!(waiting.apply(&json!("working")), Some(json!(false)));
        assert_eq!(binding(Source::AirplaneMode, vec![]).apply(&json!(true)), Some(json!(true)));
        assert_eq!(hour.apply(&json!("noon")), None, "div() of a text gives nothing");
        let traffic = binding(Source::Network, vec![Map::Log(1e8)]);
        assert_eq!(traffic.apply(&json!(0)), Some(json!(0.0)));
        assert_eq!(traffic.apply(&json!(1e9)), Some(json!(1.0)), "clamped at the top");
        let mid = traffic.apply(&json!(1e4)).unwrap().as_f64().unwrap();
        assert!((0.45..0.55).contains(&mid), "10 kB/s sits half way on the log scale up to 100 MB/s, got {mid}");
        let text = |x: f64| binding(Source::Network, vec![Map::Bytes]).apply(&json!(x)).unwrap();
        assert_eq!(
            (text(512.0), text(1234.0), text(34_500_000.0), text(250e9)),
            (json!("512 B"), json!("1.2 kB"), json!("34.5 MB"), json!("250 GB"))
        );
        assert_eq!(Source::Memory.read_capability().as_deref(), Some("system.memory.read"));
    }

    #[test]
    fn kinds_follow_the_maps() {
        let ring = Source::AgentRing { agent: "wren".into() }.kind();
        assert!(Map::Div(2.0).kind(&ring).unwrap_err().contains("needs a number"));
        assert_eq!(Map::Is(json!("x")).kind(&ring).unwrap(), Kind::Bool);
        let mixed = Map::Steps { bounds: vec![1.0], values: vec![json!("a"), json!(2)] };
        assert!(mixed.kind(&Kind::Number).is_err());
        let toggle = ParamDef { name: "p".into(), kind: ParamKind::Toggle { default: false } };
        assert!(fits(&Kind::Bool, &toggle).is_ok());
        assert!(fits(&Kind::Number, &toggle).is_err());
        let choice = ParamDef {
            name: "p".into(),
            kind: ParamKind::Choice { values: vec!["working".into(), "resting".into()], default: "resting".into() },
        };
        assert!(fits(&ring, &choice).unwrap_err().contains("needs_you"));
    }

    #[test]
    fn templates_fill_placeholders() {
        assert_eq!(fill("{used} of the disk, {x}", |n| (n == "used").then(|| "42%".to_string())), "42% of the disk, {x}");
        assert_eq!(fill("no { end", |_| None), "no { end");
        assert_eq!(placeholders("{a} and {b}"), ["a", "b"]);
        assert_eq!(shown(&json!(3)), "3");
        assert_eq!(shown(&json!(true)), "on");
        assert_eq!(shown(&json!(0.25)), "0.25");
    }

    #[test]
    fn a_wall_device_follows_its_wall() {
        let place = DevicePlace { station: None, position: Vec3::ZERO, facing_deg: None, wall: Some(Vec3::Z) };
        assert!(place.shown(Vec3::new(0.5, 0.5, 0.7)), "seen from inside the room");
        assert!(!place.shown(Vec3::new(0.5, 0.5, -0.7)), "seen from behind the wall");
        let free = DevicePlace { wall: None, ..place };
        assert!(free.shown(Vec3::new(0.0, 0.5, -1.0)));
    }

    #[test]
    fn capability_names() {
        assert!(capability_name_ok("network.airplane_mode.write"));
        for bad in ["", "Net", "a..b", "a.b-c", &"x".repeat(65)] {
            assert!(!capability_name_ok(bad), "{bad}");
        }
    }
}
