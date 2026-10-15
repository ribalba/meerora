//! Devices in the daemon (D7, D8) with a fake system: loading, two-way
//! bindings, approval of a space device through the broker, the grant file,
//! agent devices that are refused without the capability, and the greeting.
//! Nothing here touches the radios of this machine.

use meerora_protocol::{Command, DeviceInfo, Event, Role};
use serde_json::{Value, json};

use super::{Fake, Harness, write_agent};
use crate::devices::{FakeSystem, NetSample};
use crate::store::tests::TempDir;

const AIRPLANE: &str = include_str!("../../../../assets/spaces/workshop/devices/airplane.js");
const MAILBOX: &str = include_str!("../../../../assets/spaces/workshop/devices/mailbox.js");
const CLOCK: &str = include_str!("../../../../assets/spaces/workshop/devices/clock.js");
const GRAPH: &str = include_str!("../../../../assets/spaces/workshop/devices/graph.js");
const TREE: &str = include_str!("../../../../assets/spaces/workshop/devices/tree.js");
const CAP: &str = "network.airplane_mode.write";

/// A daemon on a fresh data dir with the given files and a fake system.
fn harness(files: &[(&str, &str)]) -> (Harness, FakeSystem) {
    let dir = TempDir::new("devices");
    write_agent(&dir.0, "wren", "name = \"Wren\"\nhome = \"window\"\n[runtime]\nkind = \"mock\"\n");
    for (path, text) in files {
        let path = dir.0.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    open(dir)
}

fn open(dir: TempDir) -> (Harness, FakeSystem) {
    let mut h = Harness::open(dir, Fake::default());
    let system = FakeSystem::default();
    h.daemon.devices.set_system(Box::new(system.clone()));
    (h, system)
}

/// One tick that scans and reads everything, and its events.
fn tick(h: &mut Harness) -> Vec<Event> {
    h.daemon.devices.due_now();
    h.daemon.tick();
    h.drain()
}

fn devices(events: &[Event]) -> Option<Vec<DeviceInfo>> {
    events.iter().rev().find_map(|e| match e {
        Event::Devices { devices } => Some(devices.clone()),
        _ => None,
    })
}

fn param(events: &[Event], device: &str, name: &str) -> Option<Value> {
    events.iter().rev().find_map(|e| match e {
        Event::DeviceParam { device: d, name: n, value } if d == device && n == name => Some(value.clone()),
        _ => None,
    })
}

fn result(events: &[Event]) -> Option<(bool, String)> {
    events.iter().find_map(|e| match e {
        Event::DeviceActionResult { ok, message, .. } => Some((*ok, message.clone())),
        _ => None,
    })
}

fn request(events: &[Event]) -> Option<String> {
    events.iter().find_map(|e| match e {
        Event::DeviceCapabilityRequest { id, device, capability, action } => {
            assert_eq!((device.as_str(), capability.as_str(), action.as_str()), ("airplane", CAP, "Toggle airplane mode"));
            Some(id.clone())
        }
        _ => None,
    })
}

fn click(h: &mut Harness, device: &str, action: &str) -> Vec<Event> {
    h.command(Command::DeviceAction { device: device.into(), action: action.into() })
}

/// `(device, event, decision, by)` of every line of the audit log.
fn audit(h: &Harness) -> Vec<(String, String, String, String)> {
    let text = std::fs::read_to_string(h.dir.0.join("audit.jsonl")).unwrap_or_default();
    text.lines()
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            assert!(v["time"].as_str().unwrap().contains('T'), "{v}");
            let field = |k: &str| v[k].as_str().unwrap_or("").to_string();
            (field("device"), field("event"), field("decision"), field("by"))
        })
        .collect()
}

fn line(device: &str, event: &str, decision: &str, by: &str) -> (String, String, String, String) {
    (device.into(), event.into(), decision.into(), by.into())
}

#[test]
fn a_space_device_follows_its_source_both_ways() {
    let (mut h, system) = harness(&[("space/devices/airplane.js", AIRPLANE)]);
    let greeting = h.daemon.greeting();
    let list = devices(&greeting).expect("the greeting carries the device list");
    assert_eq!(param(&greeting, "airplane", "flying"), Some(json!(false)));
    assert!(tick(&mut h).is_empty(), "nothing changed since the start");
    assert_eq!(list.len(), 1);
    let plane = &list[0];
    assert_eq!((plane.id.as_str(), plane.name.as_str(), plane.owner.as_ref()), ("airplane", "Airplane mode", None));
    assert_eq!(plane.script, "space/devices/airplane.js");
    assert_eq!(plane.actions[0].capability.as_deref(), Some(CAP));
    assert!(plane.granted.is_empty() && plane.error.is_none());

    // Switching airplane mode elsewhere makes the plane take off (D7).
    system.state.lock().unwrap().airplane = true;
    let events = tick(&mut h);
    assert_eq!(param(&events, "airplane", "flying"), Some(json!(true)));
    assert!(devices(&events).is_none(), "the list did not change");
    assert!(tick(&mut h).is_empty(), "nothing changed, nothing is sent");
    system.state.lock().unwrap().airplane = false;
    assert_eq!(param(&tick(&mut h), "airplane", "flying"), Some(json!(false)));
    assert!(system.state.lock().unwrap().writes.is_empty(), "reading never writes");
}

#[test]
fn the_action_asks_once_and_always_allow_is_remembered() {
    let (mut h, system) = harness(&[("space/devices/airplane.js", AIRPLANE)]);
    tick(&mut h);

    // Without a grant the click asks the user through the broker.
    let events = click(&mut h, "airplane", "toggle");
    let id = request(&events).expect("a capability request for the device");
    assert!(result(&events).is_none(), "the action waits for the decision");
    assert!(system.state.lock().unwrap().writes.is_empty(), "nothing runs without the grant");
    assert!(h.daemon.greeting().iter().any(|e| matches!(e, Event::DeviceCapabilityRequest { id: i, .. } if *i == id)));
    // A second click waits for the same decision.
    assert!(request(&click(&mut h, "airplane", "toggle")).is_none());

    let events = h.command(Command::DecideCapability { id: id.clone(), approve: true, remember: true });
    assert!(events.contains(&Event::DeviceCapabilitiesChanged { device: "airplane".into(), granted: vec![CAP.into()] }));
    let answers: Vec<(bool, String)> = events
        .iter()
        .filter_map(|e| match e {
            Event::DeviceActionResult { ok, message, .. } => Some((*ok, message.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(answers, [(true, "Airplane mode is on.".to_string())], "the action ran once for both clicks");
    assert_eq!(system.state.lock().unwrap().writes, [true]);
    assert_eq!(param(&events, "airplane", "flying"), Some(json!(true)), "the plane follows at once");
    assert_eq!(devices(&events).unwrap()[0].granted, [CAP], "the badge shows the grant (D8)");
    let grants = std::fs::read_to_string(h.dir.0.join("space/devices/grants.toml")).unwrap();
    assert!(grants.contains("[airplane]") && grants.contains(CAP), "{grants}");
    assert!(h.command(Command::DecideCapability { id, approve: false, remember: false }).is_empty(), "decided once");

    // Later clicks run at once, also after a restart.
    let (ok, message) = result(&click(&mut h, "airplane", "toggle")).unwrap();
    assert!(ok && message == "Airplane mode is off.", "{message}");
    let Harness { dir, .. } = h;
    let (mut h, system) = open(dir);
    tick(&mut h);
    let events = click(&mut h, "airplane", "toggle");
    assert!(request(&events).is_none(), "the grant is remembered");
    assert_eq!(result(&events), Some((true, "Airplane mode is on.".into())));
    assert_eq!(system.state.lock().unwrap().writes, [true]);
    assert_eq!(
        audit(&h),
        [
            line("airplane", "request", "", "airplane"),
            line("airplane", "decision", "always_allow", "user"),
            line("airplane", "use", "toggle", "user"),
            line("airplane", "use", "toggle", "user"),
            line("airplane", "use", "toggle", "user"),
        ]
    );
}

#[test]
fn deny_changes_nothing_and_allow_once_holds_once() {
    let (mut h, system) = harness(&[("space/devices/airplane.js", AIRPLANE)]);
    tick(&mut h);
    let id = request(&click(&mut h, "airplane", "toggle")).unwrap();
    let events = h.command(Command::DecideCapability { id, approve: false, remember: true });
    let (ok, message) = result(&events).unwrap();
    assert!(!ok && message.contains("did not allow"), "{message}");
    assert!(system.state.lock().unwrap().writes.is_empty());
    assert!(!h.dir.0.join("space/devices/grants.toml").exists(), "a denial writes no grant");

    let id = request(&click(&mut h, "airplane", "toggle")).expect("asked again after a denial");
    let events = h.command(Command::DecideCapability { id, approve: true, remember: false });
    assert_eq!(result(&events), Some((true, "Airplane mode is on.".into())));
    assert!(events.contains(&Event::DeviceCapabilitiesChanged { device: "airplane".into(), granted: vec![] }), "the card closes");
    assert!(request(&click(&mut h, "airplane", "toggle")).is_some(), "Allow once held for that click only");
    assert_eq!(system.state.lock().unwrap().writes, [true]);

    // A failing backend is reported on the card.
    let id = request(&h.daemon.greeting()).unwrap();
    system.state.lock().unwrap().fail = Some("rfkill: permission denied".into());
    let (ok, message) = result(&h.command(Command::DecideCapability { id, approve: true, remember: false })).unwrap();
    assert!(!ok && message.contains("permission denied"), "{message}");
}

#[test]
fn revoke_takes_the_grant_away() {
    let grants = format!("[airplane]\ngranted = [\"{CAP}\"]\n");
    let (mut h, system) = harness(&[("space/devices/airplane.js", AIRPLANE), ("space/devices/grants.toml", &grants)]);
    tick(&mut h);
    assert!(request(&click(&mut h, "airplane", "toggle")).is_none());
    let events = h.command(Command::RevokeDeviceCapability { device: "airplane".into(), capability: CAP.into() });
    assert!(events.contains(&Event::DeviceCapabilitiesChanged { device: "airplane".into(), granted: vec![] }));
    assert!(devices(&events).unwrap()[0].granted.is_empty());
    assert!(!std::fs::read_to_string(h.dir.0.join("space/devices/grants.toml")).unwrap().contains(CAP));
    assert!(request(&click(&mut h, "airplane", "toggle")).is_some(), "it asks again");
    assert_eq!(system.state.lock().unwrap().writes, [true]);
    assert_eq!(audit(&h)[1], line("airplane", "revoke", "", "user"));
}

#[test]
fn an_agent_device_needs_what_its_agent_holds() {
    let (mut h, system) = harness(&[]);
    write_agent(&h.dir.0, "rivet", "name = \"Rivet\"\nhome = \"router_cabinet\"\n[runtime]\nkind = \"mock\"\n");
    h.command(Command::ReloadAgents);
    tick(&mut h);

    // Rivet writes a device into its directory (D21) without holding its capabilities.
    let path = h.dir.0.join("agents/rivet/devices/plane.js");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, AIRPLANE).unwrap();
    let events = tick(&mut h);
    let list = devices(&events).unwrap();
    let plane = list.iter().find(|d| d.id == "rivet/plane").unwrap();
    assert_eq!(plane.owner.as_deref(), Some("rivet"));
    let error = plane.error.clone().expect("refused (D8)");
    assert!(error.contains("Rivet does not hold network.airplane_mode.read and network.airplane_mode.write"), "{error}");
    assert!(error.contains("request_capability"), "{error}");
    let note = events.iter().find_map(|e| match e {
        Event::Message { agent, role: Role::System, text, .. } if agent == "rivet" => Some(text.clone()),
        _ => None,
    });
    assert!(note.unwrap().starts_with("Your device plane.js did not load."));
    let events = click(&mut h, "rivet/plane", "toggle");
    assert!(!result(&events).unwrap().0 && request(&events).is_none(), "a refused device does nothing");

    // Once Rivet holds both, the device loads and its action needs no approval.
    let toml = format!(
        "name = \"Rivet\"\nhome = \"router_cabinet\"\n[runtime]\nkind = \"mock\"\n[capabilities]\ngranted = [\"network.airplane_mode.read\", \"{CAP}\"]\n"
    );
    std::fs::write(h.dir.0.join("agents/rivet/agent.toml"), toml).unwrap();
    h.command(Command::ReloadAgents);
    let events = tick(&mut h);
    let plane = devices(&events).unwrap().into_iter().find(|d| d.id == "rivet/plane").unwrap();
    assert!(plane.error.is_none(), "{:?}", plane.error);
    assert_eq!(plane.granted, ["network.airplane_mode.read", CAP]);
    assert!(events.iter().any(|e| matches!(e, Event::Message { text, .. } if text.contains("is in the space now"))));
    let events = click(&mut h, "rivet/plane", "toggle");
    assert_eq!(result(&events), Some((true, "Airplane mode is on.".into())));
    assert_eq!(system.state.lock().unwrap().writes, [true]);
}

#[test]
fn a_device_without_a_backend_or_station_does_not_load() {
    let mail = AIRPLANE.replace(CAP, "mail.send");
    let lost = MAILBOX.replace("place: { at: [995, 100, 1006] }", "place: { station: \"kitchen\" }");
    let (mut h, _) = harness(&[("space/devices/mail.js", &mail), ("space/devices/lost.js", &lost), ("space/devices/Bad Name.js", MAILBOX)]);
    let list = devices(&h.daemon.greeting()).unwrap();
    let error = |id: &str| list.iter().find(|d| d.id == id).and_then(|d| d.error.clone()).unwrap();
    assert!(error("mail").contains("no device backend for mail.send"), "{}", error("mail"));
    assert!(error("lost").contains("\"kitchen\" is not a station"), "{}", error("lost"));
    assert!(error("Bad Name").contains("not a device name"), "{}", error("Bad Name"));
    std::fs::write(h.dir.0.join("space/devices/broken.js"), "export default device({ name: 'x' });").unwrap();
    let list = devices(&tick(&mut h)).unwrap();
    assert!(list.iter().find(|d| d.id == "broken").unwrap().error.as_ref().unwrap().contains("place"));
}

#[test]
fn agent_and_clock_sources_drive_params() {
    let (mut h, system) = harness(&[("space/devices/mailbox.js", MAILBOX), ("space/devices/clock.js", CLOCK)]);
    system.state.lock().unwrap().minutes = 14 * 60 + 35;
    let events = tick(&mut h);
    assert_eq!(param(&events, "clock", "hour"), Some(json!(2)));
    assert_eq!(param(&events, "clock", "minute"), Some(json!(7)));
    assert_eq!(param(&events, "clock", "time"), Some(json!("14:35")));
    assert_eq!(param(&events, "mailbox", "up"), None, "Wren has not set letter yet");

    // Wren holds up a letter, so the flag goes up (D24).
    h.command(Command::Inject { event: Event::Param { agent: "wren".into(), name: "letter".into(), value: json!(true) } });
    assert_eq!(param(&tick(&mut h), "mailbox", "up"), Some(json!(true)));
    h.command(Command::Inject { event: Event::Param { agent: "wren".into(), name: "letter".into(), value: json!("yes") } });
    assert!(param(&tick(&mut h), "mailbox", "up").is_none(), "a value that does not fit the param is not sent");

    // The greeting carries the list and the current values.
    let greeting = h.daemon.greeting();
    assert_eq!(devices(&greeting).unwrap().len(), 2);
    assert_eq!(param(&greeting, "clock", "time"), Some(json!("14:35")));
    let (h, _) = harness(&[]);
    assert!(!h.daemon.greeting().iter().any(|e| matches!(e, Event::Devices { .. })), "no devices, no list");
}

#[test]
fn cpu_memory_and_network_drive_the_graph_and_the_tree() {
    let (mut h, system) = harness(&[("space/devices/graph.js", GRAPH), ("space/devices/tree.js", TREE)]);
    let greeting = h.daemon.greeting();
    assert_eq!(param(&greeting, "graph", "metric"), Some(json!("cpu")), "the setting goes out like a bound value");
    {
        let mut s = system.state.lock().unwrap();
        s.cpu = (100, 1000);
        s.memory = 0.6;
        s.network = NetSample { bytes: 1_000_000, at: 10.0 };
    }
    let events = tick(&mut h);
    assert_eq!(param(&events, "graph", "level"), Some(json!(0.1)), "the first reading is the share since boot");
    assert_eq!(param(&events, "graph", "reading"), Some(json!("10%")));
    assert_eq!(param(&events, "tree", "leaves"), Some(json!("full")));
    assert_eq!(param(&events, "tree", "free"), Some(json!("60%")));

    // The next readings compare with the last ones.
    {
        let mut s = system.state.lock().unwrap();
        s.cpu = (150, 1100);
        s.memory = 0.2;
        s.network = NetSample { bytes: 4_000_000, at: 13.0 };
    }
    let events = tick(&mut h);
    assert_eq!(param(&events, "graph", "level"), Some(json!(0.5)), "50 of 100 ticks were busy");
    assert_eq!(param(&events, "tree", "leaves"), Some(json!("thin")), "little memory is free");
    assert!(tick(&mut h).is_empty(), "counters that did not move keep the last reading");
    system.state.lock().unwrap().memory = 0.05;
    assert_eq!(param(&tick(&mut h), "tree", "leaves"), Some(json!("bare")));

    // The card switches the graph to the network: 1 MB/s.
    let set = |metric: &str| Command::DeviceSetting { device: "graph".into(), name: "metric".into(), value: json!(metric) };
    let events = h.command(set("network"));
    assert_eq!(param(&events, "graph", "metric"), Some(json!("network")));
    assert_eq!(param(&events, "graph", "reading"), Some(json!("1.0 MB")));
    let level = param(&events, "graph", "level").unwrap().as_f64().unwrap();
    assert!((level - (1e6f64).ln_1p() / (100e6f64).ln_1p()).abs() < 1e-9, "1 MB/s on the log scale up to 100 MB/s, got {level}");
    let settings = std::fs::read_to_string(h.dir.0.join("space/devices/settings.toml")).unwrap();
    assert!(settings.contains("[graph]") && settings.contains("metric = \"network\""), "{settings}");

    // The choice survives a restart.
    let Harness { dir, .. } = h;
    let (mut h, system) = open(dir);
    system.state.lock().unwrap().memory = 0.5;
    let greeting = h.daemon.greeting();
    assert_eq!(param(&greeting, "graph", "metric"), Some(json!("network")));
    assert_eq!(param(&tick(&mut h), "graph", "metric"), None, "nothing changed");

    // Mistakes leave everything as it was.
    let refused = |events: Vec<Event>| result(&events).is_some_and(|(ok, _)| !ok) && param(&events, "graph", "metric").is_none();
    assert!(refused(h.command(set("weather"))));
    assert!(refused(h.command(Command::DeviceSetting { device: "graph".into(), name: "level".into(), value: json!(0.3) })));
    assert!(refused(h.command(Command::DeviceSetting { device: "nothing".into(), name: "metric".into(), value: json!("cpu") })));
    // Back to the default: the file forgets the device.
    let events = h.command(set("cpu"));
    assert_eq!(param(&events, "graph", "metric"), Some(json!("cpu")));
    assert!(!std::fs::read_to_string(h.dir.0.join("space/devices/settings.toml")).unwrap().contains("[graph]"));
}

#[test]
fn an_agent_graph_needs_the_read_capabilities_of_every_metric() {
    let (mut h, _) = harness(&[]);
    write_agent(&h.dir.0, "rivet", "name = \"Rivet\"\nhome = \"router_cabinet\"\n[runtime]\nkind = \"mock\"\n");
    h.command(Command::ReloadAgents);
    let path = h.dir.0.join("agents/rivet/devices/graph.js");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, GRAPH).unwrap();
    let list = devices(&tick(&mut h)).unwrap();
    let error = list.iter().find(|d| d.id == "rivet/graph").unwrap().error.clone().unwrap();
    for cap in ["system.cpu.read", "system.disk.read", "system.memory.read", "system.network.read"] {
        assert!(error.contains(cap), "{error}");
    }
}

#[test]
fn a_room_switch_brings_its_devices_grants_and_settings() {
    let (mut h, _) = harness(&[]);
    // Two rooms, each with its own plane and its own grant.
    for room in ["workshop", "office"] {
        let devices = h.dir.0.join("spaces").join(room).join("devices");
        std::fs::create_dir_all(&devices).unwrap();
        std::fs::write(devices.parent().unwrap().join("space.js"), "// a room").unwrap();
        std::fs::write(devices.join("airplane.js"), AIRPLANE).unwrap();
    }
    std::fs::write(h.dir.0.join("spaces/office/devices/grants.toml"), format!("[airplane]\ngranted = [\"{CAP}\"]\n")).unwrap();
    std::fs::write(h.dir.0.join("meerora.toml"), "[space]\nroom = \"workshop\"\n").unwrap();
    let list = devices(&tick(&mut h)).unwrap();
    assert_eq!(list[0].script, "spaces/workshop/devices/airplane.js");
    assert!(list[0].granted.is_empty());
    std::fs::write(h.dir.0.join("meerora.toml"), "[space]\nroom = \"office\"\n").unwrap();
    h.command(Command::ReloadAgents);
    let list = devices(&tick(&mut h)).unwrap();
    assert_eq!(list[0].script, "spaces/office/devices/airplane.js");
    assert_eq!(list[0].granted, [CAP], "the grants of the office came along");
}

#[test]
fn the_built_in_space_devices_load() {
    let assets = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets");
    let dir = TempDir::new("assets-devices");
    std::fs::create_dir_all(dir.0.join("space/devices")).unwrap();
    for entry in std::fs::read_dir(assets.join("spaces/workshop/devices")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "js") {
            std::fs::copy(&path, dir.0.join("space/devices").join(path.file_name().unwrap())).unwrap();
        }
    }
    write_agent(&dir.0, "wren", "name = \"Wren\"\nhome = \"window\"\n");
    let (h, _) = open(dir);
    let list = devices(&h.daemon.greeting()).unwrap();
    let ids: Vec<&str> = list.iter().map(|d| d.id.as_str()).collect();
    assert_eq!(ids, ["airplane", "clock", "graph", "mailbox", "noticeboard", "tree"]);
    for d in &list {
        assert!(d.error.is_none(), "{}: {:?}", d.id, d.error);
    }
    let board = list.iter().find(|d| d.id == "noticeboard").unwrap();
    assert_eq!(board.actions[0].opens.as_deref(), Some("feed"));
    assert!(board.needs.is_empty(), "opening the feed needs no capability");
}
