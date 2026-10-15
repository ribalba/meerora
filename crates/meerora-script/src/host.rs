//! The QuickJS sandbox. Every run gets a fresh runtime and context with a
//! memory limit, a stack limit and a deadline. The prelude (`prelude.js`)
//! implements the modelling API on top of a few native helpers that build
//! primitive meshes into a per-run arena.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use meerora_geom::{Finish, Mesh, Vec2, Vec3, prim, srgb_hex};
use rquickjs::context::{EvalOptions, intrinsic};
use rquickjs::promise::PromiseState;
use rquickjs::{Coerced, Context, Ctx, Exception, Function, Module, Object, Persistent, Runtime, Value};

use crate::{Limits, ScriptError};

const PRELUDE: &str = include_str!("prelude.js");
const PRELUDE_NAME: &str = "<meerora>";
const MAX_CONSOLE_LINES: usize = 500;
const MAX_CONSOLE_BYTES: usize = 64 << 10;

/// Everything except Date, Performance and WeakRef, which would make runs
/// depend on the clock or the garbage collector.
type Intrinsics = (
    intrinsic::Eval,
    intrinsic::RegExpCompiler,
    intrinsic::RegExp,
    intrinsic::Json,
    intrinsic::Proxy,
    intrinsic::MapSet,
    intrinsic::TypedArrays,
    intrinsic::Promise,
);

/// A finished run: the kind named by the default export, its JSON description
/// and the arena its part entries point into.
pub(crate) struct Raw {
    pub kind: Option<String>,
    pub json: String,
    pub meshes: Vec<Mesh>,
}

#[derive(Default)]
struct State {
    meshes: Vec<Mesh>,
    triangles: usize,
    cap: usize,
    cap_hit: bool,
    console: Vec<String>,
    console_bytes: usize,
    console_full: bool,
    /// Descriptions registered by `figure()` and `space()`. The default export
    /// only carries an index into this list, so a script cannot hand the host
    /// a description that skipped the prelude's checks.
    built: Vec<(String, String)>,
}

impl State {
    fn charge(&mut self, triangles: u64) -> Result<(), String> {
        let total = (self.triangles as u64).saturating_add(triangles);
        if total > self.cap as u64 {
            self.cap_hit = true;
            return Err(format!("the script built more than {} triangles", self.cap));
        }
        self.triangles = total as usize;
        Ok(())
    }

    fn log(&mut self, line: String) {
        if self.console_full {
            return;
        }
        if self.console.len() >= MAX_CONSOLE_LINES || self.console_bytes + line.len() > MAX_CONSOLE_BYTES {
            self.console.push("(console output truncated)".into());
            self.console_full = true;
            return;
        }
        self.console_bytes += line.len();
        self.console.push(line);
    }
}

enum Failure {
    Thrown(Persistent<Value<'static>>),
    Engine(rquickjs::Error),
}

/// Runs `source` as an ES module and returns its default export together with
/// everything the script printed through `console.log`.
pub(crate) fn run(source: &str, file_name: &str, limits: &Limits) -> (Result<Raw, ScriptError>, Vec<String>) {
    let state = Rc::new(RefCell::new(State { cap: limits.hard_triangle_cap, ..State::default() }));
    let result = sandboxed(source, file_name, limits, &state);
    let mut st = state.borrow_mut();
    let console = std::mem::take(&mut st.console);
    let result = result.map(|id| {
        let (kind, json) = id.and_then(|i| st.built.get(i).cloned()).map_or((None, String::new()), |(k, j)| (Some(k), j));
        Raw { kind, json, meshes: std::mem::take(&mut st.meshes) }
    });
    (result, console)
}

fn sandboxed(source: &str, file_name: &str, limits: &Limits, state: &Rc<RefCell<State>>) -> Result<Option<usize>, ScriptError> {
    let rt = Runtime::new().map_err(|e| engine_error(e, limits))?;
    rt.set_memory_limit(limits.memory_bytes);
    rt.set_max_stack_size(limits.max_stack_bytes);
    let timed_out = Rc::new(Cell::new(false));
    let flag = timed_out.clone();
    let deadline = Instant::now() + Duration::from_millis(limits.timeout_ms);
    rt.set_interrupt_handler(Some(Box::new(move || {
        let late = Instant::now() >= deadline;
        if late {
            flag.set(true);
        }
        late
    })));
    let ctx = Context::custom::<Intrinsics>(&rt).map_err(|e| engine_error(e, limits))?;

    let outcome = ctx.with(|ctx| match evaluate(&ctx, source, file_name, state) {
        Ok(v) => Ok(v),
        Err(rquickjs::Error::Exception) => Err(Failure::Thrown(Persistent::save(&ctx, ctx.catch()))),
        Err(e) => Err(Failure::Engine(e)),
    });
    let failure = match outcome {
        Ok(_) if state.borrow().cap_hit => return Err(ScriptError::HardCap(limits.hard_triangle_cap)),
        Ok(v) => return Ok(v),
        Err(f) => f,
    };

    let near_limit = rt.memory_usage().malloc_size.max(0) as usize + (256 << 10) >= limits.memory_bytes;
    // Lift the limits so the error itself can be read.
    rt.set_interrupt_handler(None);
    rt.set_memory_limit(0);
    let cap_hit = state.borrow().cap_hit;
    let error = match failure {
        Failure::Engine(e) => engine_error(e, limits),
        Failure::Thrown(value) => ctx.with(|ctx| match value.restore(&ctx) {
            Ok(v) => describe(&v, file_name, limits, near_limit),
            Err(e) => engine_error(e, limits),
        }),
    };
    Err(if timed_out.get() {
        ScriptError::Timeout(limits.timeout_ms)
    } else if cap_hit {
        ScriptError::HardCap(limits.hard_triangle_cap)
    } else {
        error
    })
}

/// Returns the index of the description the default export refers to.
fn evaluate<'js>(ctx: &Ctx<'js>, source: &str, file_name: &str, state: &Rc<RefCell<State>>) -> rquickjs::Result<Option<usize>> {
    install_natives(ctx, state)?;
    let mut opts = EvalOptions::default();
    opts.filename = Some(PRELUDE_NAME.into());
    ctx.eval_with_options::<(), _>(PRELUDE, opts)?;

    let module = Module::declare(ctx.clone(), file_name, source)?;
    let (module, promise) = module.eval()?;
    while promise.state() == PromiseState::Pending && ctx.execute_pending_job() {}
    match promise.state() {
        PromiseState::Pending => {
            return Err(Exception::throw_message(ctx, "the script never finished (top-level await on a promise that never settles)"));
        }
        PromiseState::Rejected => return promise.result::<()>().unwrap_or(Err(rquickjs::Error::Exception)).map(|_| None),
        PromiseState::Resolved => {}
    }

    let default: Value = module.get("default")?;
    let Some(obj) = default.as_object() else { return Ok(None) };
    let id: Value = obj.get("__meerora")?;
    Ok(id.as_int().and_then(|i| usize::try_from(i).ok()))
}

fn install_natives<'js>(ctx: &Ctx<'js>, state: &Rc<RefCell<State>>) -> rquickjs::Result<()> {
    let n = Object::new(ctx.clone())?;

    let s = state.clone();
    let prim_fn = move |ctx: Ctx<'js>, kind: String, nums: Vec<f64>, pts: Vec<Vec<f64>>| -> rquickjs::Result<u32> {
        let mut st = s.borrow_mut();
        let estimate = estimate(&kind, &nums, &pts).map_err(|e| Exception::throw_type(&ctx, &e))?;
        st.charge(estimate).map_err(|e| Exception::throw_range(&ctx, &e))?;
        let mesh = build(&kind, &nums, &pts);
        // The estimate is an upper bound. Refund the difference.
        st.triangles -= (estimate as usize).saturating_sub(mesh.triangle_count()).min(st.triangles);
        st.meshes.push(mesh);
        Ok(st.meshes.len() as u32 - 1)
    };
    n.set("prim", Function::new(ctx.clone(), prim_fn)?)?;

    let s = state.clone();
    let retain_fn = move |ctx: Ctx<'js>, id: u32| -> rquickjs::Result<()> {
        let mut st = s.borrow_mut();
        let count = st.meshes.get(id as usize).map(|m| m.triangle_count()).unwrap_or(0);
        st.charge(count as u64).map_err(|e| Exception::throw_range(&ctx, &e))
    };
    n.set("retain", Function::new(ctx.clone(), retain_fn)?)?;

    let s = state.clone();
    let bounds_fn = move |ctx: Ctx<'js>, id: u32, m: Vec<f64>| -> rquickjs::Result<Vec<f64>> {
        let st = s.borrow();
        let Some(mesh) = st.meshes.get(id as usize) else { return Err(Exception::throw_range(&ctx, "unknown mesh")) };
        if m.len() != 16 {
            return Err(Exception::throw_type(&ctx, "a transform has 16 numbers"));
        }
        let mut b = [f64::INFINITY, f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
        for v in &mesh.vertices {
            let p = v.position.map(f64::from);
            for r in 0..3 {
                let x = m[r] * p[0] + m[4 + r] * p[1] + m[8 + r] * p[2] + m[12 + r];
                b[r] = b[r].min(x);
                b[r + 3] = b[r + 3].max(x);
            }
        }
        Ok(b.to_vec())
    };
    n.set("bounds", Function::new(ctx.clone(), bounds_fn)?)?;

    n.set("color", Function::new(ctx.clone(), |hex: String| srgb_hex(&hex).is_ok())?)?;
    n.set("finish", Function::new(ctx.clone(), |name: String| Finish::parse(&name).is_some())?)?;

    let s = state.clone();
    n.set("log", Function::new(ctx.clone(), move |line: String| s.borrow_mut().log(line))?)?;

    let s = state.clone();
    let built_fn = move |kind: String, json: String| -> u32 {
        let mut st = s.borrow_mut();
        st.built.push((kind, json));
        st.built.len() as u32 - 1
    };
    n.set("built", Function::new(ctx.clone(), built_fn)?)?;

    ctx.globals().set("__meerora_native", n)
}

/// Upper bound of the triangles a primitive call builds, computed before the
/// mesh exists so a huge request fails without allocating it.
fn estimate(kind: &str, nums: &[f64], pts: &[Vec<f64>]) -> Result<u64, String> {
    let n = |i: usize, min: u64| (nums.get(i).copied().unwrap_or(0.0).clamp(0.0, u32::MAX as f64) as u64).max(min);
    let len = pts.len() as u64;
    Ok(match kind {
        "box" => 12,
        "bevel_box" => 44,
        "cylinder" => 4 * n(2, 3),
        "frustum" => 4 * n(3, 3),
        "sphere" => 20 << (2 * n(1, 0).min(4)),
        "capsule" => {
            let s = n(2, 4);
            2 * s * (2 * (s / 4).max(1) + 1)
        }
        "egg" => {
            let s = n(2, 4);
            2 * s * (s / 2).max(3)
        }
        "lathe" => 2 * n(0, 3) * (len + 1),
        "torus" => 2 * n(2, 3) * n(3, 3),
        "tube" => 2 * n(1, 3) * (len + 1),
        "extrude" => 4 * len,
        "quad" => 2,
        _ => return Err(format!("unknown shape {kind:?}")),
    })
}

fn build(kind: &str, nums: &[f64], pts: &[Vec<f64>]) -> Mesh {
    let f = |i: usize| nums.get(i).copied().unwrap_or(0.0) as f32;
    let u = |i: usize| nums.get(i).copied().unwrap_or(0.0).clamp(0.0, u32::MAX as f64) as u32;
    let c = |p: &Vec<f64>, i: usize| p.get(i).copied().unwrap_or(0.0) as f32;
    let v2 = || pts.iter().map(|p| Vec2::new(c(p, 0), c(p, 1))).collect::<Vec<_>>();
    let v3 = || pts.iter().map(|p| Vec3::new(c(p, 0), c(p, 1), c(p, 2))).collect::<Vec<_>>();
    match kind {
        "box" => prim::cuboid(f(0), f(1), f(2)),
        "bevel_box" => prim::bevel_box(f(0), f(1), f(2), f(3)),
        "cylinder" => prim::cylinder(f(0), f(1), u(2)),
        "frustum" => prim::frustum(f(0), f(1), f(2), u(3)),
        "sphere" => prim::sphere(f(0), u(1)),
        "capsule" => prim::capsule(f(0), f(1), u(2)),
        "egg" => prim::egg(f(0), f(1), u(2)),
        "lathe" => prim::lathe(&v2(), u(0)),
        "torus" => prim::torus(f(0), f(1), u(2), u(3)),
        "tube" => prim::tube(&v3(), f(0), u(1)),
        "extrude" => prim::extrude(&v2(), f(0)),
        "quad" => prim::quad(f(0), f(1)),
        _ => Mesh::new(),
    }
}

pub(crate) fn engine_error(e: rquickjs::Error, limits: &Limits) -> ScriptError {
    match e {
        rquickjs::Error::Allocation => ScriptError::OutOfMemory(limits.memory_bytes),
        e => ScriptError::Js(e.to_string()),
    }
}

/// Turns a thrown value into an error message with file and line.
pub(crate) fn describe(v: &Value<'_>, file_name: &str, limits: &Limits, near_limit: bool) -> ScriptError {
    let Some(obj) = v.as_object().filter(|o| o.is_error()) else {
        if (v.is_null() || v.is_undefined()) && near_limit {
            return ScriptError::OutOfMemory(limits.memory_bytes);
        }
        let shown = v.get::<Coerced<String>>().map(|c| c.0).unwrap_or_else(|_| v.type_name().to_string());
        return ScriptError::Js(format!("{file_name}: the script threw {shown}"));
    };
    let text = |key: &str| obj.get::<_, Option<Coerced<String>>>(key).ok().flatten().map(|c| c.0).unwrap_or_default();
    let (name, message, stack) = (text("name"), text("message"), text("stack"));
    if message.contains("out of memory") {
        return ScriptError::OutOfMemory(limits.memory_bytes);
    }
    let name = if name.is_empty() { "Error".to_string() } else { name };
    match location(&stack, file_name) {
        Some(line) => ScriptError::Js(format!("{file_name}:{line}: {name}: {message}")),
        None => ScriptError::Js(format!("{file_name}: {name}: {message}")),
    }
}

/// The line of the innermost stack frame inside the script itself.
fn location(stack: &str, file_name: &str) -> Option<u32> {
    let needle = format!("{file_name}:");
    stack.lines().find_map(|frame| {
        let at = frame.find(&needle)? + needle.len();
        let digits: String = frame[at..].chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    })
}
