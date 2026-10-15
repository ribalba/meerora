use std::time::{Duration, Instant};

use meerora_geom::{Vec3, srgb_hex};
use meerora_script::{Built, Limits, Motion, ParamKind, ScriptError, action_category, build_figure, build_space, run_script};

const ROBOT: &str = r##"
const body = box(48, 56, 40).material("enamel", "#E9E4D8").bevel(6);
body.add(
  vents(3).at(body.front.offset(-10, 14, 0)),
  dial().at(body.front.offset(10, 6, 0)),
  emblem("lock").at(body.front.offset(0, -18, 0)),
);
const head = box(48, 34, 36).material("enamel", "#E9E4D8").bevel(5).above(body, 8);
const display = screen(38, 22).at(head.front.offset(0, -11, 0));
const neck = cylinder(6, 8).color("#9A927F").on(body);
const shoulder = body.right.offset(0, 18, 0);
const arm = tube([shoulder, shoulder.offset(10, -16, 4), shoulder.offset(12, -34, 10)], 2.5).color("#8A8F9A");
const tool = wrench(12).rotate(180, 0, 0).at(shoulder.offset(12, -46, 10));
const ant = bentRod(16).on(head, { x: 10 });
const tip = bulb("#F2B45A").at(ant.tip.offset(0, -4, 0));
const tail = tube([body.back.offset(0, -20, 0), body.back.offset(0, -26, -14), [0, 1, -44]], 1.5).color("#3B404C");
const base = treads({ wheels: 3, width: 54 });
body.move(0, base.top[1], 0);
for (const p of [head, display, neck, arm, tool, ant, tip, tail]) p.move(0, base.top[1], 0);

export default figure({
  parts: [base, body, neck, head, display, arm, tool, ant, tip, tail],
  anchors: { ring: tip.top.above(12), face: display },
  animations: {
    working: bob(1, 1.2),
    waiting: hop(2).then(face("viewer")),
    resting: breathe(4).eyes("closed"),
  },
  budget: { triangles: 3000 },
});
"##;

fn limits() -> Limits {
    Limits::default()
}

fn figure_with(body: &str) -> String {
    format!(
        r#"{body}
export default figure({{
  parts: [main],
  anchors: {{ ring: main.top.above(10), face: main }},
  animations: {{ working: bob(), waiting: hop(), resting: breathe() }},
}});"#
    )
}

#[test]
fn robot_builds() {
    let f = build_figure(ROBOT, "robot.js", &limits()).unwrap();
    assert!(f.triangles > 500 && f.triangles <= 3000, "{} triangles", f.triangles);
    assert_eq!(f.triangles, f.base.triangle_count());
    assert_eq!(f.budget, 3000);
    assert!(f.bounds.min.y.abs() < 1e-3, "feet at {}", f.bounds.min.y);
    assert!(f.bounds.max.y > 110.0 && f.bounds.max.y < 140.0, "height {}", f.bounds.max.y);
    assert!(f.ring_anchor.y > f.bounds.max.y, "ring {} below the top {}", f.ring_anchor.y, f.bounds.max.y);
    assert!(f.face_anchor.z > 15.0, "face anchor {} is not on the front", f.face_anchor);
    assert_eq!(f.animations.working.motion, Motion::Bob { amplitude: 1.0, period: 1.2 });
    assert_eq!(f.animations.waiting.motion, Motion::Hop { count: 2, height: 12.0, period: 2.4 });
    assert!(f.animations.waiting.face_viewer && !f.animations.waiting.eyes_closed);
    assert!(f.animations.resting.eyes_closed);
    // The bulb glows amber and the body is enamel coloured.
    let amber = srgb_hex("#F2B45A").unwrap();
    assert!(f.base.vertices.iter().any(|v| v.color == amber && v.emissive == 1.0));
    let enamel = srgb_hex("#E9E4D8").unwrap();
    assert!(f.base.vertices.iter().any(|v| v.color == enamel && v.emissive == 0.0));
    for v in &f.base.vertices {
        let n = Vec3::from(v.normal);
        assert!((n.length() - 1.0).abs() < 1e-3, "normal {n}");
    }
    assert!(f.params.is_empty() && f.param_parts.is_empty());
}

#[test]
fn over_budget() {
    let src = figure_with("const main = sphere(10, 4);");
    match build_figure(&src, "big.js", &limits()) {
        Err(ScriptError::OverBudget { used: 5120, budget: 3000 }) => {}
        other => panic!("expected OverBudget, got {other:?}"),
    }
    // A larger declared budget is accepted up to 6000.
    let src = src.replace("resting: breathe() }", "resting: breathe() }, budget: { triangles: 6000 }");
    assert_eq!(build_figure(&src, "big.js", &limits()).unwrap().budget, 6000);
    let src = src.replace("6000", "8000");
    assert!(matches!(build_figure(&src, "big.js", &limits()), Err(ScriptError::Invalid(_))));
}

#[test]
fn missing_slots_are_listed_together() {
    let src = r#"
const main = box(10, 10, 10);
export default figure({ parts: [main], anchors: { ring: main.top }, animations: { working: bob() } });
"#;
    match build_figure(src, "slots.js", &limits()) {
        Err(ScriptError::MissingSlots(slots)) => assert_eq!(slots, ["anchors.face", "animations.waiting", "animations.resting"]),
        other => panic!("expected MissingSlots, got {other:?}"),
    }
}

#[test]
fn endless_loop_times_out() {
    let start = Instant::now();
    match build_figure("while (true) {}", "loop.js", &limits()) {
        Err(ScriptError::Timeout(500)) => {}
        other => panic!("expected Timeout, got {other:?}"),
    }
    assert!(start.elapsed() < Duration::from_secs(2), "took {:?}", start.elapsed());
    // Catching the interruption does not help.
    let src = "try { while (true) {} } catch (e) {}\nwhile (true) {}";
    assert!(matches!(build_figure(src, "loop.js", &limits()), Err(ScriptError::Timeout(_))));
}

#[test]
fn huge_allocation_runs_out_of_memory() {
    for src in ["const a = new Array(1e8).fill(0);", "const a = [];\nwhile (true) a.push(new Array(100000).fill(1));"] {
        match build_figure(src, "oom.js", &limits()) {
            Err(ScriptError::OutOfMemory(bytes)) => assert_eq!(bytes, 32 << 20),
            other => panic!("expected OutOfMemory, got {other:?}"),
        }
    }
}

#[test]
fn syntax_error_has_a_line() {
    match build_figure("const a = 1;\nconst b = ;\n", "broken.js", &limits()) {
        Err(ScriptError::Js(msg)) => assert!(msg.starts_with("broken.js:2:") && msg.contains("SyntaxError"), "{msg}"),
        other => panic!("expected Js, got {other:?}"),
    }
}

#[test]
fn runtime_error_points_at_the_script() {
    match build_figure("const a = 1;\n\nconst b = box(1, -2, 3);\n", "bad.js", &limits()) {
        Err(ScriptError::Js(msg)) => assert!(msg.starts_with("bad.js:3:") && msg.contains("box(h)"), "{msg}"),
        other => panic!("expected Js, got {other:?}"),
    }
    match build_figure("export default figure({ part: [] });", "typo.js", &limits()) {
        Err(ScriptError::Js(msg)) => assert!(msg.contains("\"part\""), "{msg}"),
        other => panic!("expected Js, got {other:?}"),
    }
}

#[test]
fn wrong_kind() {
    let src = "export default space({ parts: [quad(100, 100)] });";
    assert!(matches!(build_figure(src, "space.js", &limits()), Err(ScriptError::WrongKind { expected: "figure" })));
    assert!(matches!(build_figure("export default 42;", "n.js", &limits()), Err(ScriptError::WrongKind { expected: "figure" })));
    assert!(matches!(build_figure("const a = 1;", "none.js", &limits()), Err(ScriptError::WrongKind { expected: "figure" })));
    assert!(matches!(build_space(ROBOT, "robot.js", &limits()), Err(ScriptError::WrongKind { expected: "space" })));
    // A hand-made export that did not go through figure() is not accepted.
    let forged = r#"export default { __meerora: "figure", __json: "{}" };"#;
    assert!(matches!(build_figure(forged, "forged.js", &limits()), Err(ScriptError::WrongKind { expected: "figure" })));
    assert!(matches!(build_figure("export default { __meerora: 0 };", "forged.js", &limits()), Err(ScriptError::WrongKind { .. })));
}

#[test]
fn toggle_param_keeps_its_part_out_of_base() {
    let src = r##"
const body = egg(52, 80).material("felt", "#D9A441");
const arm = capsule(3, 20).color("#C8933F").at(-24, 40, 0);
const letter = box(24, 16, 1).color("#F4F1EA").above(arm);
arm.add(letter);
const shoulder = [-22, 42, 0];
export default figure({
  parts: [body, arm],
  anchors: { ring: body.top.above(14), face: body.front },
  animations: { working: bob(0.6), waiting: hop(2), resting: breathe(4) },
  params: { letter: toggle(false).shows(arm, true, { pivot: shoulder }).animate(wave(1.5)) },
});
"##;
    let f = build_figure(src, "wren.js", &limits()).unwrap();
    // Only the egg (12 segments, 6 rings) is always visible.
    assert_eq!(f.base.triangle_count(), 2 * 12 + 4 * 24);
    assert_eq!(f.param_parts.len(), 1);
    let p = &f.param_parts[0];
    assert_eq!(p.param, "letter");
    assert_eq!(p.when, serde_json::Value::Bool(true));
    assert_eq!(p.pivot, Vec3::new(-22.0, 42.0, 0.0));
    assert_eq!(p.anim, Some(Motion::Wave { period: 1.5, angle_deg: 8.0 }));
    // The arm and the letter it carries are both in the param part.
    assert_eq!(p.mesh.triangle_count(), f.triangles - f.base.triangle_count());
    assert!(p.mesh.bounds().max.y > 75.0);
    assert_eq!(f.params[0].name, "letter");
    assert_eq!(f.params[0].kind, ParamKind::Toggle { default: false });
    assert!(f.bounds.max.y >= p.mesh.bounds().max.y);
}

#[test]
fn param_kinds() {
    let src = figure_with("const main = box(10, 10, 10);\nconst hat = cone(4, 6).on(main);\nconst sign = box(4, 4, 1).on(hat);").replace(
        "resting: breathe() },",
        r#"resting: breathe() },
  params: {
    mood: choice(["calm", "busy"], "busy").shows(hat, "busy"),
    level: number(0, 10, 5),
    letters: count(3).shows(sign, 2),
    label: text("hi"),
  },"#,
    );
    let f = build_figure(&src, "params.js", &limits()).unwrap();
    let kinds: Vec<_> = f.params.iter().map(|p| (p.name.as_str(), p.kind.clone())).collect();
    assert_eq!(
        kinds,
        [
            ("mood", ParamKind::Choice { values: vec!["calm".into(), "busy".into()], default: "busy".into() }),
            ("level", ParamKind::Number { min: 0.0, max: 10.0, default: 5.0 }),
            ("letters", ParamKind::Count { default: 3 }),
            ("label", ParamKind::Text { default: "hi".into() }),
        ]
    );
    assert_eq!(f.param_parts.len(), 2);
    assert_eq!(f.param_parts[0].when, serde_json::json!("busy"));
    assert_eq!(f.param_parts[1].when, serde_json::json!(2));
    assert_eq!(f.base.triangle_count(), 12);
    let bad = src.replace(r#".shows(hat, "busy")"#, r#".shows(hat, "sleepy")"#);
    assert!(matches!(build_figure(&bad, "params.js", &limits()), Err(ScriptError::Js(_))));
}

#[test]
fn placement() {
    let src = r#"
const a = box(10, 20, 10);
const b = box(4, 4, 4).on(a, { x: 2 });
const c = box(4, 4, 4).above(a, 5);
const d = box(4, 4, 4).beside(a, "right", 1);
const e = box(4, 4, 4).below(c, 1);
const f = box(4, 4, 4).at(30, 0, 30).rotate(0, 90, 0).scale(2);
console.log(JSON.stringify([b.bottom, c.bottom, d.bounds, e.top, f.bounds]));
export default 0;
"#;
    let run = run_script(src, "place.js", &limits());
    assert!(run.result.is_err());
    let v: serde_json::Value = serde_json::from_str(&run.console[0]).unwrap();
    let close = |a: &serde_json::Value, b: &[f64]| {
        let a: Vec<f64> = serde_json::from_value(a.clone()).unwrap();
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-6)
    };
    assert!(close(&v[0], &[2.0, 20.0, 0.0]), "{}", v[0]);
    assert!(close(&v[1], &[0.0, 25.0, 0.0]), "{}", v[1]);
    assert!(close(&v[2]["min"], &[6.0, 0.0, -2.0]), "{}", v[2]);
    assert!(close(&v[3], &[0.0, 24.0, 0.0]), "{}", v[3]);
    assert!(close(&v[4]["min"], &[26.0, 0.0, 26.0]) && close(&v[4]["max"], &[34.0, 8.0, 34.0]), "{}", v[4]);
}

#[test]
fn children_follow_their_parent_and_clones_are_independent() {
    let src = figure_with(
        r#"const main = box(10, 10, 10);
const hat = box(2, 2, 2).on(main);
main.add(hat);
main.move(100, 0, 0);
const twin = main.clone().move(0, 0, 50);
console.log(JSON.stringify([hat.bottom, twin.bounds.max, main.bounds.max]));"#,
    );
    let run = run_script(&src, "kids.js", &limits());
    let Built::Figure(f) = run.result.unwrap() else { panic!("not a figure") };
    assert_eq!(run.console[0], "[[100,10,0],[105,12,55],[105,12,5]]");
    // The twin is not listed in parts, so only main and its hat are built.
    assert_eq!(f.base.triangle_count(), 24);
}

#[test]
fn hard_cap() {
    let src = "for (let i = 0; i < 1000; i++) sphere(1, 4);";
    assert!(matches!(build_figure(src, "cap.js", &limits()), Err(ScriptError::HardCap(200_000))));
    // Catching the error does not hide it.
    let src = "try { cylinder(1, 1, 1e9); } catch (e) {}\nexport default 1;";
    assert!(matches!(build_figure(src, "cap.js", &limits()), Err(ScriptError::HardCap(_))));
}

#[test]
fn sandbox_has_no_clock_imports_or_io() {
    let src = r#"console.log(typeof Date, typeof performance, typeof WeakRef, typeof std, typeof os, typeof require, typeof Math.sin);"#;
    let run = run_script(src, "env.js", &limits());
    assert_eq!(run.console, ["undefined undefined undefined undefined undefined undefined function"]);
    match build_figure("import x from 'os';\nexport default x;", "imp.js", &limits()) {
        Err(ScriptError::Js(msg)) => assert!(msg.contains("could not load module"), "{msg}"),
        other => panic!("expected Js, got {other:?}"),
    }
}

#[test]
fn runs_are_deterministic() {
    let src = figure_with(
        "const main = group();\nfor (let i = 0; i < 8; i++) main.add(box(1 + Math.random() * 4, 2, 2).move(Math.random() * 30, 0, 0));",
    );
    let a = build_figure(&src, "rand.js", &limits()).unwrap();
    let b = build_figure(&src, "rand.js", &limits()).unwrap();
    assert_eq!(a, b);
    assert_eq!(build_figure(ROBOT, "robot.js", &limits()).unwrap(), build_figure(ROBOT, "robot.js", &limits()).unwrap());
}

const SPACE: &str = r##"
const floor = quad(800, 600).color("#6E553F");
const desk = box(120, 70, 60).color("#7A5638").at(300, 0, 380);
export default space({
  parts: [floor, desk],
  lights: [light([120, 220, 160], "#FFD9A0", 1.2, 320)],
  stations: {
    desk: station([300, 0, 330], { facing: 180, tools: ["terminal", "editor", "files"] }),
    door: station([0, 0, 0], { front: true, tools: ["browser"] }),
    bench: [-200, 0, 100],
  },
  waypoints: { hall: [100, 0, 200] },
  paths: [["desk", "hall"], ["hall", "door"], ["door", "bench"]],
  camera: { viewHeight: 900 },
});
"##;

#[test]
fn space_builds() {
    let s = build_space(SPACE, "space.js", &limits()).unwrap();
    assert_eq!(s.triangles, 14);
    assert_eq!(s.budget, 60000);
    let ids: Vec<&str> = s.waypoints.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(ids, ["desk", "door", "bench", "hall"]);
    assert_eq!(s.stations.len(), 3);
    assert_eq!(s.stations[0].facing_deg, 180.0);
    assert!(s.stations[1].front && !s.stations[2].front);
    assert_eq!(s.stations[0].tools, ["terminal", "editor", "files"]);
    assert!(s.stations[2].tools.is_empty());
    assert_eq!(s.edges.len(), 3);
    assert_eq!(s.lights[0].radius, 320.0);
    assert_eq!(s.lights[0].color, srgb_hex("#FFD9A0").unwrap());
    assert!((s.sun_direction.length() - 1.0).abs() < 1e-5);
    assert_eq!(s.camera.view_height, 900.0);
    assert_eq!(s.camera.target, s.bounds.center());
    assert_eq!(s.background, srgb_hex("#0B0D12").unwrap());
}

#[test]
fn station_tools_route_actions() {
    let s = build_space(SPACE, "space.js", &limits()).unwrap();
    let at = |action: &str| s.station_for_action(action).map(|st| st.id.as_str());
    assert_eq!(at("terminal.run"), Some("desk"));
    assert_eq!(at("editor.patch"), Some("desk"));
    assert_eq!(at("browser.search"), Some("door"));
    assert_eq!(at("browser"), Some("door"));
    assert_eq!(at("mail.read"), None);
    assert_eq!(action_category("meerora.figure.set"), "meerora");
    assert_eq!(action_category("files"), "files");
}

#[test]
fn bad_station_tools_fail() {
    let cases = [
        (r#"tools: ["terminal.run"]"#, "not action names"),
        (r#"tools: ["Terminal"]"#, "short lowercase names"),
        (r#"tools: ["mail", "mail"]"#, "twice"),
        (r#"tools: "terminal""#, "must be an array"),
    ];
    for (tools, expected) in cases {
        let src = SPACE.replace(r#"tools: ["browser"]"#, tools);
        match build_space(&src, "space.js", &limits()) {
            Err(ScriptError::Js(msg)) => assert!(msg.contains(expected) && msg.starts_with("space.js:9:"), "{msg}"),
            other => panic!("expected Js for {tools}, got {other:?}"),
        }
    }
    let src = SPACE.replace(r#"tools: ["browser"]"#, r#"tools: ["browser", "terminal"]"#);
    match build_space(&src, "space.js", &limits()) {
        Err(ScriptError::Invalid(msg)) => assert!(msg.contains("`terminal` is listed at both `desk` and `door`"), "{msg}"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn walls_are_separate_meshes() {
    let src = SPACE.replace(
        "parts: [floor, desk],",
        "parts: [floor, desk],\n  walls: [{ inside: [2, 5, 0], parts: [box(10, 200, 400).at(-5, 0, 200).add(box(4, 50, 50).at(2, 100, 200))] }],",
    );
    let s = build_space(&src, "space.js", &limits()).unwrap();
    assert_eq!(s.triangles, 14 + 24, "the walls count towards the budget");
    assert_eq!(s.mesh.triangle_count(), 14);
    assert_eq!(s.walls.len(), 1);
    assert_eq!(s.walls[0].mesh.triangle_count(), 24, "the wall and the part hanging on it");
    assert_eq!(s.walls[0].inside, Vec3::X, "flattened onto the floor and normalised");
    assert_eq!(s.solid_mesh().triangle_count(), 38);
    assert!(s.bounds.min.x <= -10.0, "the bounds include the walls");
    // Seen from +x the wall stands behind the room, from -x in front of it.
    assert!(s.walls[0].faces(Vec3::new(0.7, 0.5, 0.5)));
    assert!(!s.walls[0].faces(Vec3::new(-0.7, 0.5, 0.5)));

    // A part listed both as a wall and in parts is only drawn with the wall.
    let src = SPACE.replace("parts: [floor, desk],", "parts: [floor, desk],\n  walls: [{ inside: [0, 0, 1], parts: desk }],");
    let s = build_space(&src, "space.js", &limits()).unwrap();
    assert_eq!((s.mesh.triangle_count(), s.walls[0].mesh.triangle_count()), (2, 12));

    for (walls, expected) in [
        ("walls: [{ inside: [0, 1, 0], parts: [desk] }],", "must point along the floor"),
        ("walls: [{ parts: [desk] }],", "walls[0].inside must be a point"),
        ("walls: [{ inside: [1, 0, 0], parts: [desk], door: 1 }],", "door"),
        ("walls: { inside: [1, 0, 0] },", "walls must be an array"),
    ] {
        let src = SPACE.replace("parts: [floor, desk],", &format!("parts: [floor, desk],\n  {walls}"));
        match build_space(&src, "space.js", &limits()) {
            Err(ScriptError::Js(msg)) => assert!(msg.contains(expected), "{walls}: {msg}"),
            other => panic!("expected Js for {walls}, got {other:?}"),
        }
    }
}

#[test]
fn the_graveyard_plot_comes_from_the_space_script() {
    assert_eq!(build_space(SPACE, "space.js", &limits()).unwrap().graveyard, None, "a space without a plot");
    let src = SPACE.replace(
        "camera: { viewHeight: 900 },",
        "camera: { viewHeight: 900 },\n  graveyard: { station: \"bench\", gap: 40, width: 80, depth: 120 },",
    );
    let plot = build_space(&src, "space.js", &limits()).unwrap().graveyard.expect("the declared plot");
    assert_eq!((plot.station.as_str(), plot.gap, plot.width, plot.depth), ("bench", 40.0, 80.0, 120.0));
    // The station defaults to `graveyard` and the gap to 0.
    let src =
        SPACE.replace("bench: [-200, 0, 100],", "graveyard: [-200, 0, 100],").replace(r#"["door", "bench"]"#, r#"["door", "graveyard"]"#);
    let src = src.replace("camera: { viewHeight: 900 },", "camera: { viewHeight: 900 },\n  graveyard: { width: 64, depth: 176 },");
    let plot = build_space(&src, "space.js", &limits()).unwrap().graveyard.unwrap();
    assert_eq!((plot.station.as_str(), plot.gap), ("graveyard", 0.0));

    let unknown = SPACE.replace(
        "camera: { viewHeight: 900 },",
        "camera: { viewHeight: 900 },\n  graveyard: { station: \"crypt\", width: 64, depth: 176 },",
    );
    match build_space(&unknown, "space.js", &limits()) {
        Err(ScriptError::Invalid(msg)) => assert!(msg.contains("graveyard.station names `crypt`"), "{msg}"),
        other => panic!("expected Invalid, got {other:?}"),
    }
    for (plot, expected) in [
        ("graveyard: { station: \"bench\", depth: 10 },", "graveyard.width must be a number"),
        ("graveyard: { station: \"bench\", width: -1, depth: 10 },", "graveyard.width must be greater than 0"),
        ("graveyard: { station: \"bench\", width: 10, depth: 10, fence: 1 },", "fence"),
    ] {
        let src = SPACE.replace("camera: { viewHeight: 900 },", &format!("camera: {{ viewHeight: 900 }},\n  {plot}"));
        match build_space(&src, "space.js", &limits()) {
            Err(ScriptError::Js(msg)) => assert!(msg.contains(expected), "{plot}: {msg}"),
            other => panic!("expected Js for {plot}, got {other:?}"),
        }
    }
}

#[test]
fn space_with_unknown_path_id_fails() {
    let src = SPACE.replace(r#"["hall", "door"]"#, r#"["hall", "dor"]"#);
    match build_space(&src, "space.js", &limits()) {
        Err(ScriptError::Invalid(msg)) => assert!(msg.contains("unknown id `dor`"), "{msg}"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn space_with_unreachable_station_fails() {
    let src = SPACE.replace(r#", ["door", "bench"]"#, "");
    match build_space(&src, "space.js", &limits()) {
        Err(ScriptError::Invalid(msg)) => assert!(msg.contains("`bench` cannot be reached"), "{msg}"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn console_output_is_kept_on_failure() {
    let run = run_script("console.log('one', 2, [3]);\nconsole.log({ a: 1 });\nthrow new Error('boom');", "log.js", &limits());
    assert_eq!(run.console, ["one 2 [3]", r#"{"a":1}"#]);
    match run.result {
        Err(ScriptError::Js(msg)) => assert_eq!(msg, "log.js:3: Error: boom"),
        other => panic!("expected Js, got {other:?}"),
    }
}
