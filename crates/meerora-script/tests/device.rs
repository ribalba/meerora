//! Device scripts (D7, D8, D20): the shape, the place, the bindings, the
//! actions, the status line and the budget.

use meerora_geom::Vec3;
use meerora_script::{
    ActionKind, Built, DEVICE_BUDGET, Effect, Limits, Map, ScriptError, Source, Status, build_device, build_figure, run_script,
};
use serde_json::json;

fn limits() -> Limits {
    Limits::default()
}

/// A lamp with one toggle, placed at a station, with the rest of the spec given.
fn lamp(rest: &str) -> String {
    format!(
        r#"
const foot = cylinder(8, 2, 10);
const shade = cone(10, 8, 10, 3).at(0, 30, 0);
const bulb = sphere(3, 1).glow(1).at(0, 26, 0);
export default device({{
  name: "Lamp",
  parts: [foot, shade],
  params: {{ on: toggle(false).shows(bulb, true) }},
  place: {{ station: "desk", offset: [20, 78, 0] }},
  {rest}
}});"#
    )
}

fn invalid(src: &str) -> String {
    match build_device(src, "device.js", &limits()) {
        Err(ScriptError::Invalid(e)) | Err(ScriptError::Js(e)) => e,
        other => panic!("expected an error, got {other:?}"),
    }
}

#[test]
fn a_device_builds_with_place_bindings_actions_and_status() {
    let src = lamp(
        r#"bind: { on: "network.airplane_mode" },
  actions: { toggle: action("network.airplane_mode.write"), off: action("network.airplane_mode.write", "off", "Radios on") },
  status: { on: { true: "Shining", false: "Dark" } },"#,
    );
    let d = build_device(&src, "lamp.js", &limits()).unwrap();
    assert_eq!(d.name, "Lamp");
    assert_eq!(d.budget, DEVICE_BUDGET);
    assert_eq!(d.triangles, d.base.triangle_count() + d.param_parts[0].mesh.triangle_count());
    assert_eq!(
        (d.place.station.as_deref(), d.place.position, d.place.facing_deg, d.place.wall),
        (Some("desk"), Vec3::new(20.0, 78.0, 0.0), None, None)
    );
    assert_eq!(d.bindings.len(), 1);
    assert_eq!((d.bindings[0].param.as_str(), &d.bindings[0].source), ("on", &Source::AirplaneMode));
    assert_eq!(d.actions[0].label, "Toggle", "the default label");
    assert_eq!(d.actions[0].kind, ActionKind::Capability { capability: "network.airplane_mode.write".into(), effect: Effect::Toggle });
    assert_eq!(d.actions[1].label, "Radios on");
    assert_eq!(d.actions[1].kind, ActionKind::Capability { capability: "network.airplane_mode.write".into(), effect: Effect::Off });
    assert_eq!(d.capabilities(), ["network.airplane_mode.read", "network.airplane_mode.write"]);
    let values = [("on".to_string(), json!(true))].into();
    assert_eq!(d.status_text(&values).as_deref(), Some("Shining"));
    assert_eq!(d.status_text(&Default::default()).as_deref(), Some("Dark"), "the default counts when nothing is set");
    assert!(d.bounds.max.y > 30.0, "the bounds include the param parts");

    // run_script tells a device from a figure.
    let run = run_script(&src, "lamp.js", &limits());
    assert!(matches!(run.result, Ok(Built::Device(_))));
    assert!(matches!(build_figure(&src, "lamp.js", &limits()), Err(ScriptError::WrongKind { expected: "figure" })));
    assert!(matches!(
        build_device("export default space({ parts: [quad(10, 10)] });", "s.js", &limits()),
        Err(ScriptError::WrongKind { expected: "device" })
    ));
}

#[test]
fn bindings_map_sources_onto_params() {
    let src = r#"
const hands = count(0);
for (let i = 0; i < 12; i++) hands.shows(box(1, 10, 1).rotate(0, 0, -30 * i), i);
const leaves = choice(["full", "thin", "bare"]).shows(sphere(20, 1).at(0, 40, 0), "full");
export default device({
  name: "Garden",
  parts: [box(4, 40, 4)],
  params: { hands, leaves, used: text(""), waiting: toggle(false), letter: toggle(false) },
  place: { at: [100, 0, 100], facing: 45, wall: [0, 0, 2] },
  bind: {
    hands: from("clock.time").div(60).mod(12),
    leaves: from("system.disk").steps([0.7, 0.9], ["full", "thin", "bare"]),
    used: from("system.disk").percent(),
    waiting: from("agent.wren.ring").is("needs_you"),
    letter: "agent.wren.param.letter",
  },
  status: "{used} used, leaves {leaves}, letter {letter}",
});"#;
    let d = build_device(src, "garden.js", &limits()).unwrap();
    assert_eq!(d.place.facing_deg, Some(45.0));
    assert_eq!(d.place.wall, Some(Vec3::Z), "the wall direction is normalised along the floor");
    let by = |p: &str| d.bindings.iter().find(|b| b.param == p).unwrap();
    assert_eq!(by("hands").maps, [Map::Div(60.0), Map::Mod(12.0)]);
    assert_eq!(by("hands").apply(&json!(21 * 60 + 10)), Some(json!(9)));
    assert_eq!(by("leaves").apply(&json!(0.8)), Some(json!("thin")));
    assert_eq!(by("used").apply(&json!(0.5)), Some(json!("50%")));
    assert_eq!(by("waiting").source, Source::AgentRing { agent: "wren".into() });
    assert_eq!(by("letter").source, Source::AgentParam { agent: "wren".into(), name: "letter".into() });
    let values = [("used".to_string(), json!("50%")), ("leaves".to_string(), json!("thin"))].into();
    assert_eq!(d.status_text(&values).as_deref(), Some("50% used, leaves thin, letter off"));
    assert!(matches!(d.status, Some(Status::Text(_))));
    assert_eq!(d.capabilities(), ["system.disk.read"], "the clock and agents need no capability");
}

#[test]
fn bad_bindings_are_refused_with_the_reason() {
    let e = invalid(&lamp(r#"bind: { on: "network.airplane" },"#));
    assert!(e.contains("bind.on") && e.contains("not a source"), "{e}");
    let e = invalid(&lamp(r#"bind: { on: "system.disk" },"#));
    assert!(e.contains("gives a number, which does not fit"), "{e}");
    let e = invalid(&lamp(r#"bind: { on: from("agent.wren.ring").div(2) },"#));
    assert!(e.contains("div() needs a number"), "{e}");
    let e = invalid(&lamp(r#"bind: { lamp: "clock.time" },"#));
    assert!(e.contains("bind.lamp names no param"), "{e}");
    let e = invalid(&lamp(r#"bind: { on: 42 },"#));
    assert!(e.contains("must be a source"), "{e}");
    let choice = r#"
export default device({
  name: "Ring",
  parts: [box(2, 2, 2)],
  params: { state: choice(["working", "resting"]) },
  place: { at: [0, 0, 0] },
  bind: { state: "agent.wren.ring" },
});"#;
    assert!(invalid(choice).contains("\"needs_you\", which is not one of the choices"));
    let steps = lamp(r#"bind: { on: from("system.disk").steps([0.5, 0.2], [true, false, true]) },"#);
    assert!(invalid(&steps).contains("must grow"));
    let steps = lamp(r#"bind: { on: from("system.disk").steps([0.5], [true]) },"#);
    assert!(invalid(&steps).contains("one value more than bounds"));
}

#[test]
fn bad_actions_places_and_status_are_refused() {
    let e = invalid(&lamp(r#"actions: { toggle: action("Network Write") },"#));
    assert!(e.contains("not a capability name"), "{e}");
    let e = invalid(&lamp(r#"actions: { toggle: action("network.airplane_mode.write", "flip") },"#));
    assert!(e.contains("\"toggle\", \"on\" or \"off\""), "{e}");
    let e = invalid(&lamp(r#"actions: { Toggle: action("network.airplane_mode.write") },"#));
    assert!(e.contains("short lowercase words"), "{e}");
    let e = invalid(&lamp(r#"actions: { open: opens("terminal") },"#));
    assert!(e.contains("opens(view) knows feed"), "{e}");
    let e = invalid(&lamp(r#"actions: { run: { command: "rm -rf /" } },"#));
    assert!(e.contains("must be action(...) or opens(...)"), "devices never run commands: {e}");
    let e = invalid(&lamp(r#"status: "{brightness}","#));
    assert!(e.contains("no param brightness"), "{e}");
    let e = invalid(&lamp(r#"status: { on: { maybe: "?" } },"#));
    assert!(e.contains("not a value of the param"), "{e}");
    let e = invalid(&lamp(r#"colour: "red","#));
    assert!(e.contains("no key \"colour\""), "{e}");

    let unplaced = "export default device({ name: 'x', parts: [box(1, 1, 1)] });";
    assert!(invalid(unplaced).contains("needs a place"));
    let both = "export default device({ name: 'x', parts: [box(1, 1, 1)], place: { at: [0, 0, 0], station: 'desk' } });";
    assert!(invalid(both).contains("not both"));
    let flat = "export default device({ name: 'x', parts: [box(1, 1, 1)], place: { at: [0, 0, 0], wall: [0, 1, 0] } });";
    assert!(invalid(flat).contains("along the floor"));
    let empty = "export default device({ name: 'x', place: { at: [0, 0, 0] } });";
    assert!(invalid(empty).contains("no visible geometry"));
    let nameless = "export default device({ parts: [box(1, 1, 1)], place: { at: [0, 0, 0] } });";
    assert!(invalid(nameless).contains("name"));
}

#[test]
fn the_device_budget_holds() {
    let big = "export default device({ name: 'x', parts: [sphere(10, 4)], place: { at: [0, 0, 0] } });";
    assert!(matches!(build_device(big, "big.js", &limits()), Err(ScriptError::OverBudget { used: 5120, budget: 1500 })));
    let declared = "export default device({ name: 'x', parts: [sphere(10, 4)], place: { at: [0, 0, 0] }, budget: { triangles: 2000 } });";
    assert!(matches!(build_device(declared, "big.js", &limits()), Err(ScriptError::OverBudget { budget: 2000, .. })));
    let too_much = "export default device({ name: 'x', parts: [box(1, 1, 1)], place: { at: [0, 0, 0] }, budget: { triangles: 9000 } });";
    assert!(invalid(too_much).contains("at most 3000"));
    // Param parts count, because they can all show.
    let parts = "const p = toggle(false); for (let i = 0; i < 20; i++) p.shows(sphere(5, 2), true);
export default device({ name: 'x', parts: [box(1, 1, 1)], params: { p }, place: { at: [0, 0, 0] } });";
    assert!(matches!(build_device(parts, "parts.js", &limits()), Err(ScriptError::OverBudget { .. })));
}

/// A screen whose graph shows one of four metrics, picked on the card.
const GRAPH: &str = r##"
const screen = box(100, 60, 4);
export default device({
  name: "System graph",
  parts: [screen],
  params: {
    metric: choice(["cpu", "memory", "disk", "network"], "cpu").setting("Shows", { cpu: "CPU", memory: "Memory free" }),
    level: number(0, 1, 0),
    reading: text(""),
  },
  place: { at: [0, 100, 0] },
  bind: {
    level: per("metric", { cpu: "system.cpu", memory: from("system.memory"), disk: "system.disk", network: from("system.network").log(1e8) }),
    reading: per("metric", { cpu: from("system.cpu").percent(), network: from("system.network").bytes() }),
  },
  display: graph({ value: "level", at: [0, 30, 2.5], width: 90, height: 50, points: 40, color: "#7FE3D0" }),
  status: { metric: { cpu: "CPU in use: {reading}.", memory: "Memory free.", disk: "Disk used.", network: "{reading} per second." } },
});"##;

#[test]
fn settings_switch_bindings_and_a_graph_draws_a_param() {
    let d = build_device(GRAPH, "graph.js", &limits()).unwrap();
    assert_eq!(d.settings.len(), 1);
    let setting = &d.settings[0];
    assert_eq!((setting.param.as_str(), setting.label.as_str()), ("metric", "Shows"));
    assert_eq!(d.setting_label("metric", &json!("memory")), "Memory free");
    assert_eq!(d.setting_label("metric", &json!("disk")), "Disk", "values without a label are capitalised");
    assert_eq!(d.bindings.len(), 6);
    let active = |metric: Option<&str>| -> Vec<(String, String)> {
        let values = metric.map(|m| [("metric".to_string(), json!(m))].into()).unwrap_or_default();
        d.active_bindings(&values).map(|b| (b.param.clone(), b.source.to_string())).collect()
    };
    let pair = |p: &str, s: &str| (p.to_string(), s.to_string());
    assert_eq!(active(None), [pair("level", "system.cpu"), pair("reading", "system.cpu")], "the default value counts");
    assert_eq!(active(Some("network")), [pair("level", "system.network"), pair("reading", "system.network")]);
    assert_eq!(active(Some("disk")), [pair("level", "system.disk")], "a value without a case leaves the param alone");
    assert_eq!(
        d.capabilities(),
        ["system.cpu.read", "system.disk.read", "system.memory.read", "system.network.read"],
        "an agent's device needs every source it may bind"
    );
    let network = d.bindings.iter().find(|b| b.param == "level" && b.source == Source::Network).unwrap();
    assert_eq!(network.when, Some(("metric".to_string(), json!("network"))));
    assert_eq!(network.maps, [Map::Log(1e8)]);

    let g = d.display.as_ref().expect("the graph");
    assert_eq!((g.value.as_str(), g.at, g.width, g.height, g.points), ("level", Vec3::new(0.0, 30.0, 2.5), 90.0, 50.0, 40));
    assert_eq!((g.min, g.max, g.thickness), (0.0, 1.0, 1.6));

    // Mistakes are named.
    let bad = |from: &str, to: &str| invalid(&GRAPH.replace(from, to));
    assert!(bad(".setting(\"Shows\", { cpu: \"CPU\", memory: \"Memory free\" })", "").contains("needs a param marked with .setting()"));
    assert!(bad("network: from(\"system.network\").log(1e8) }", "net: \"system.network\" }").contains("case \"net\""));
    assert!(bad("reading: per(", "metric: \"system.cpu\", reading: per(").contains("is a setting"));
    assert!(bad("value: \"level\"", "value: \"reading\"").contains("must be a number() or count() param"));
    assert!(bad("value: \"level\"", "value: \"nothing\"").contains("names no param"));
    assert!(bad("points: 40", "points: 1").contains("between 2 and 240"));
    assert!(bad("level: number(0, 1, 0),", "level: number(0, 1, 0).setting(\"x\"),").contains("choice() and toggle()"));
    assert!(bad("display: graph({", "display: ({").contains("display must be graph"));
}

#[test]
fn the_built_in_devices_build() {
    // Every room has the same devices, each in its own shape, so device ids,
    // grants and settings mean the same thing in every room.
    let spaces = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/spaces");
    for room in ["workshop", "space-station", "office"] {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(spaces.join(room).join("devices")).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|e| e != "js") {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            let d = build_device(&source, &name, &limits()).unwrap_or_else(|e| panic!("{room}/{name}: {e}"));
            assert!(d.triangles <= d.budget);
            if name == "graph.js" {
                assert!(d.display.is_some() && d.settings.iter().any(|s| s.param == "metric"), "{room}: the graph shows a chosen metric");
            }
            if name == "tree.js" {
                assert!(d.bindings.iter().all(|b| b.source == Source::Memory), "{room}: the tree shows free memory");
            }
            names.push(name);
        }
        names.sort();
        assert_eq!(names, ["airplane.js", "clock.js", "graph.js", "mailbox.js", "noticeboard.js", "tree.js"], "{room}");
    }
}
