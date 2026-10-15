//! `figure-check <figure.js | space.js | device.js>` prints triangle count,
//! anchors, bindings and validation errors.

use std::process::ExitCode;

use meerora_geom::{Aabb, Vec3};
use meerora_script::{
    ActionKind, Anim, Built, DeviceDesc, FigureDesc, Limits, Map, Motion, ParamDef, ParamKind, SpaceDesc, Status, run_script,
};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [path] = args.as_slice() else {
        eprintln!("usage: figure-check <figure.js | space.js | device.js>");
        return ExitCode::from(2);
    };
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: cannot read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let run = run_script(&source, path, &Limits::default());
    for line in &run.console {
        println!("console    {line}");
    }
    match run.result {
        Ok(Built::Figure(f)) => print_figure(path, &f),
        Ok(Built::Space(s)) => print_space(path, &s),
        Ok(Built::Device(d)) => print_device(path, &d),
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}

fn v(p: Vec3) -> String {
    // Avoid printing -0.0 for values that are zero up to rounding.
    let c = |x: f32| if x.abs() < 0.05 { 0.0 } else { x };
    format!("[{:.1}, {:.1}, {:.1}]", c(p.x), c(p.y), c(p.z))
}

fn bounds(b: &Aabb) -> String {
    format!("{} .. {}  size {}", v(b.min), v(b.max), v(b.size()))
}

fn budget(used: usize, budget: usize) -> String {
    format!("{used} / {budget} ({:.0}%)", 100.0 * used as f64 / budget.max(1) as f64)
}

fn motion(m: &Motion) -> String {
    match m {
        Motion::Still => "still".into(),
        Motion::Bob { amplitude, period } => format!("bob({amplitude} cm, {period} s)"),
        Motion::Hop { count, height, period } => format!("hop({count} x {height} cm, {period} s)"),
        Motion::Breathe { period, depth } => format!("breathe({period} s, {depth})"),
        Motion::Spin { period } => format!("spin({period} s)"),
        Motion::Wave { period, angle_deg } => format!("wave({period} s, {angle_deg} deg)"),
        Motion::Orbit { radius, period } => format!("orbit({radius} cm, {period} s)"),
    }
}

fn anim(a: &Anim) -> String {
    let mut s = motion(&a.motion);
    if a.face_viewer {
        s.push_str(" then face viewer");
    }
    if a.eyes_closed {
        s.push_str(", eyes closed");
    }
    s
}

fn print_figure(path: &str, f: &FigureDesc) {
    println!("figure     {path}");
    println!("triangles  {}", budget(f.triangles, f.budget));
    println!("base       {} triangles", f.base.triangle_count());
    println!("bounds     {}", bounds(&f.bounds));
    println!("ring       {}", v(f.ring_anchor));
    println!("face       {}", v(f.face_anchor));
    println!("working    {}", anim(&f.animations.working));
    println!("waiting    {}", anim(&f.animations.waiting));
    println!("resting    {}", anim(&f.animations.resting));
    print_params(&f.params, &f.param_parts);
}

fn param_kind(p: &ParamDef) -> String {
    match &p.kind {
        ParamKind::Toggle { default } => format!("toggle({default})"),
        ParamKind::Choice { values, default } => format!("choice({values:?}, {default:?})"),
        ParamKind::Number { min, max, default } => format!("number({min}, {max}, {default})"),
        ParamKind::Count { default } => format!("count({default})"),
        ParamKind::Text { default } => format!("text({default:?})"),
    }
}

fn print_params(params: &[ParamDef], param_parts: &[meerora_script::ParamPart]) {
    for p in params {
        println!("param      {} {}", p.name, param_kind(p));
        for part in param_parts.iter().filter(|part| part.param == p.name) {
            let anim = part.anim.as_ref().map(|m| format!(", {}", motion(m))).unwrap_or_default();
            println!(
                "  shows    when {}: {} triangles, pivot {}{anim}, bounds {}",
                part.when,
                part.mesh.triangle_count(),
                v(part.pivot),
                bounds(&part.mesh.bounds())
            );
        }
    }
}

fn print_space(path: &str, s: &SpaceDesc) {
    println!("space      {path}");
    println!("triangles  {}", budget(s.triangles, s.budget));
    println!("bounds     {}", bounds(&s.bounds));
    for w in &s.walls {
        println!("wall       {} triangles, inside {}", w.mesh.triangle_count(), v(w.inside));
    }
    for st in &s.stations {
        let front = if st.front { ", front" } else { "" };
        let tools = if st.tools.is_empty() { String::new() } else { format!(", tools {}", st.tools.join(" ")) };
        println!("station    {} at {} facing {} deg{front}{tools}", st.id, v(st.position), st.facing_deg);
    }
    let extra = s.waypoints.iter().filter(|(id, _)| !s.stations.iter().any(|st| &st.id == id));
    for (id, p) in extra {
        println!("waypoint   {id} at {}", v(*p));
    }
    for (a, b) in &s.edges {
        println!("path       {a} - {b}");
    }
    for l in &s.lights {
        println!("light      at {} intensity {} radius {}", v(l.position), l.intensity, l.radius);
    }
    let c = &s.camera;
    println!("camera     target {} height {} azimuth {} elevation {}", v(c.target), c.view_height, c.azimuth_deg, c.elevation_deg);
    if let Some(g) = &s.graveyard {
        println!("graveyard  plot {} x {} cm, {} cm in front of {}", g.width, g.depth, g.gap, g.station);
    }
}

/// A device (D7, D8): shape, place, bindings, actions and status line.
fn print_device(path: &str, d: &DeviceDesc) {
    println!("device     {path}");
    println!("name       {}", d.name);
    println!("triangles  {}", budget(d.triangles, d.budget));
    println!("base       {} triangles", d.base.triangle_count());
    println!("bounds     {}", bounds(&d.bounds));
    let p = &d.place;
    let facing = p.facing_deg.map(|f| format!(" facing {f} deg")).unwrap_or_default();
    let wall = p.wall.map(|w| format!(", on the wall with inside {}", v(w))).unwrap_or_default();
    match &p.station {
        Some(station) => println!("place      next to {station}, offset {}{facing}{wall}", v(p.position)),
        None => println!("place      at {}{facing}{wall}", v(p.position)),
    }
    print_params(&d.params, &d.param_parts);
    for b in &d.bindings {
        let maps: Vec<String> = b
            .maps
            .iter()
            .map(|m| match m {
                Map::Div(n) => format!(".div({n})"),
                Map::Mod(n) => format!(".mod({n})"),
                Map::Steps { bounds, values } => format!(".steps({bounds:?}, {})", serde_json::Value::from(values.clone())),
                Map::Is(value) => format!(".is({value})"),
                Map::Percent => ".percent()".into(),
                Map::Time => ".time()".into(),
                Map::Log(max) => format!(".log({max})"),
                Map::Bytes => ".bytes()".into(),
            })
            .collect();
        let read = b.source.read_capability().map(|c| format!(", agent devices need {c}")).unwrap_or_default();
        let when = b.when.as_ref().map(|(setting, value)| format!(" while {setting} = {value}")).unwrap_or_default();
        println!("bind       {} <- {}{}{when}{read}", b.param, b.source, maps.join(""));
    }
    for s in &d.settings {
        println!("setting    {} {:?}", s.param, s.label);
    }
    if let Some(g) = &d.display {
        println!(
            "display    graph of {} at {}, {} x {} cm, {} points from {} to {}",
            g.value,
            v(g.at),
            g.width,
            g.height,
            g.points,
            g.min,
            g.max
        );
    }
    for a in &d.actions {
        match &a.kind {
            ActionKind::Capability { capability, effect } => {
                println!("action     {} {:?}: {} with {capability}", a.id, a.label, effect.name())
            }
            ActionKind::Opens(view) => println!("action     {} {:?}: opens the {view}", a.id, a.label),
        }
    }
    match &d.status {
        Some(Status::Text(text)) => println!("status     {text:?}"),
        Some(Status::PerValue { param, texts }) => {
            for (value, text) in texts {
                println!("status     {param} = {value}: {text:?}");
            }
        }
        None => {}
    }
    let caps = d.capabilities();
    if !caps.is_empty() {
        println!("needs      {}", caps.join(" "));
    }
}
