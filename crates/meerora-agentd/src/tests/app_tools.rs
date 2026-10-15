//! The app tools of D30 in the daemon (app_tools.rs): the capability of
//! every call, the forwarding to the app client and the matching of its
//! replies, the timeout, a missing app and the audit log.

use std::time::Duration;

use meerora_protocol::{AppCall, AppLog, AppResult, Command, Event, ToolCall};
use meerora_runtime::RunEvent;
use serde_json::{Value, json};

use super::{Fake, Harness, Step, finished, write_agent};
use crate::daemon::Outgoing;
use crate::store::tests::TempDir;

/// The bridge client in these tests, as in `Harness::command`.
const BRIDGE: u64 = 7;
/// The app client.
const APP: u64 = 9;

fn harness(granted: &[&str]) -> Harness {
    let dir = TempDir::new("app-tools");
    let list: Vec<String> = granted.iter().map(|g| format!("{g:?}")).collect();
    let toml = format!(
        "name = \"Wren\"\nhome = \"window\"\non_done = \"rest\"\n\n[runtime]\nkind = \"claude-cli\"\n\n[capabilities]\ngranted = [{}]\n",
        list.join(", ")
    );
    write_agent(&dir.0, "wren", &toml);
    Harness::open(dir, Fake::default())
}

/// Sends a command as `client` and returns what went out, with the client it went to.
fn send(h: &mut Harness, client: u64, command: Command) -> Vec<Outgoing> {
    h.daemon.handle_command(client, command);
    h.daemon.take_out()
}

fn call(h: &mut Harness, id: &str, tool: ToolCall) -> Vec<Outgoing> {
    send(h, BRIDGE, Command::Tool { id: id.into(), agent: "wren".into(), call: tool })
}

/// The answer to tool call `id` sent to the bridge.
fn answer(out: &[Outgoing], id: &str) -> Option<(bool, String, Option<String>)> {
    out.iter().find_map(|o| match o {
        Outgoing::To(BRIDGE, Event::ToolResult { id: got, ok, message, image }) if got == id => Some((*ok, message.clone(), image.clone())),
        _ => None,
    })
}

/// The app request that went to the app client.
fn request(out: &[Outgoing]) -> Option<(String, AppCall, Vec<String>)> {
    out.iter().find_map(|o| match o {
        Outgoing::To(APP, Event::AppRequest { id, agent, request, granted }) => {
            assert_eq!(agent, "wren");
            Some((id.clone(), request.clone(), granted.clone()))
        }
        _ => None,
    })
}

/// `(event, capability, decision, note)` of every line of the audit log.
fn audit(h: &Harness) -> Vec<(String, String, String, String)> {
    let text = std::fs::read_to_string(h.dir.0.join("audit.jsonl")).unwrap_or_default();
    text.lines()
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            let field = |k: &str| v[k].as_str().unwrap_or("").to_string();
            (field("event"), field("capability"), field("decision"), field("note"))
        })
        .collect()
}

fn act(app: &str, action: &str, args: Value) -> ToolCall {
    ToolCall::AppAct { app: app.into(), action: action.into(), args }
}

#[test]
fn each_tool_needs_its_capability() {
    let mut h = harness(&[]);
    send(&mut h, APP, Command::ServeApps);
    let cases = [
        (ToolCall::AppsList, "apps_list", "apps.read"),
        (ToolCall::AppRead { app: "terminal:1".into(), query: None }, "app_read", "apps.read"),
        (ToolCall::AppScreenshot { app: "browser:1".into() }, "app_screenshot", "apps.read"),
        (act("terminal:1", "run", json!({ "command": "ls" })), "app_act", "apps.control"),
        (ToolCall::AppOpen { app: "browser".into(), args: json!({ "url": "https://example.org" }) }, "app_open", "apps.control"),
        (act("window:7", "type", json!({ "text": "hi" })), "app_act", "apps.input"),
        (ToolCall::AppOpen { app: "org.gnome.Calculator.desktop".into(), args: Value::Null }, "app_open", "apps.launch"),
    ];
    for (i, (tool, name, capability)) in cases.iter().enumerate() {
        let id = format!("c{i}");
        let out = call(&mut h, &id, tool.clone());
        let (ok, message, _) = answer(&out, &id).expect("refused at once");
        assert!(!ok, "{tool:?}");
        assert!(message.starts_with(&format!("{name} needs the capability {capability}")), "{message}");
        assert!(message.contains("request_capability") && message.contains(&format!("\"{capability}\"")), "{message}");
        assert!(request(&out).is_none(), "nothing reaches the app without {capability}");
    }
    let lines = audit(&h);
    assert_eq!(lines.len(), cases.len());
    assert_eq!(lines[3], ("use".into(), "apps.control".into(), "refused".into(), "app_act terminal:1 run".into()));
    assert_eq!(lines[6], ("use".into(), "apps.launch".into(), "refused".into(), "app_open org.gnome.Calculator.desktop".into()));
    assert!(lines.iter().all(|l| l.0 == "use" && l.2 == "refused"));
}

#[test]
fn each_capability_lets_its_tools_through() {
    let mut h = harness(&["apps.read", "apps.control", "apps.input", "apps.launch"]);
    send(&mut h, APP, Command::ServeApps);
    for (i, tool) in [
        ToolCall::AppsList,
        ToolCall::AppScreenshot { app: "window:3".into() },
        act("browser:1", "click", json!({ "ref": "e3" })),
        act("window:3", "key", json!({ "keys": "ctrl+s" })),
        ToolCall::AppOpen { app: "terminal".into(), args: Value::Null },
        ToolCall::AppOpen { app: "gnome-calculator".into(), args: Value::Null },
    ]
    .into_iter()
    .enumerate()
    {
        let out = call(&mut h, &format!("c{i}"), tool.clone());
        let (_, forwarded, granted) = request(&out).unwrap_or_else(|| panic!("{tool:?} reaches the app"));
        assert_eq!(Some(forwarded), AppCall::from_tool(&tool));
        assert_eq!(granted.len(), 4, "the app learns what the agent holds");
        assert!(answer(&out, &format!("c{i}")).is_none(), "the bridge waits for the app");
    }
    let lines = audit(&h);
    assert_eq!(lines.len(), 6);
    assert!(lines.iter().all(|l| l.0 == "use" && l.2.is_empty()), "{lines:?}");
    assert_eq!(lines[3], ("use".into(), "apps.input".into(), String::new(), "app_act window:3 key".into()));
}

#[test]
fn replies_are_matched_by_id_and_acts_reach_the_action_log() {
    let mut h = harness(&["apps.read", "apps.control"]);
    send(&mut h, APP, Command::ServeApps);
    let out = call(&mut h, "c1", act("terminal:1", "run", json!({ "command": "cargo test" })));
    let (first, ..) = request(&out).unwrap();
    let out = call(&mut h, "c2", ToolCall::AppScreenshot { app: "browser:1".into() });
    let (second, ..) = request(&out).unwrap();
    assert_ne!(first, second);

    // The second answers first.
    let mut shot = AppResult::text("A picture of browser:1.");
    shot.image = Some("iVBORw0KGgo=".into());
    let out = send(&mut h, APP, Command::AppReply { id: second.clone(), result: shot });
    assert_eq!(answer(&out, "c2"), Some((true, "A picture of browser:1.".into(), Some("iVBORw0KGgo=".into()))));
    assert!(answer(&out, "c1").is_none());
    assert!(!out.iter().any(|o| matches!(o, Outgoing::All(Event::Action { .. }))), "reading logs no action");

    let long = format!("cargo test {}", "x".repeat(100));
    let mut ran = AppResult::text("Ran it.");
    ran.log = Some(AppLog { tool: "terminal.run".into(), summary: long.clone() });
    let out = send(&mut h, APP, Command::AppReply { id: first.clone(), result: ran });
    assert_eq!(answer(&out, "c1"), Some((true, "Ran it.".into(), None)));
    let action = out.iter().find_map(|o| match o {
        Outgoing::All(Event::Action { agent, tool, summary, target }) => {
            Some((agent.clone(), tool.clone(), summary.clone(), target.clone()))
        }
        _ => None,
    });
    let (agent, tool, summary, target) = action.expect("an action for every client");
    assert_eq!((agent.as_str(), tool.as_str()), ("wren", "terminal.run"));
    assert!(summary.chars().count() <= 80 && summary.ends_with('…'), "{summary}");
    assert_eq!(target, Some(long), "the full command when the summary is cut");

    // A reply nobody waits for any more changes nothing.
    assert!(send(&mut h, APP, Command::AppReply { id: first, result: AppResult::text("again") }).is_empty());

    // A failed act answers with the error and logs nothing.
    let out = call(&mut h, "c3", act("terminal:1", "type", json!({ "text": "x" })));
    let (id, ..) = request(&out).unwrap();
    let mut failed = AppResult::error("the user took over this app");
    failed.log = Some(AppLog { tool: "terminal.type".into(), summary: "x".into() });
    let out = send(&mut h, APP, Command::AppReply { id, result: failed });
    assert_eq!(answer(&out, "c3"), Some((false, "the user took over this app".into(), None)));
    assert!(!out.iter().any(|o| matches!(o, Outgoing::All(Event::Action { .. }))));
}

#[test]
fn without_an_app_the_call_fails_at_once() {
    let mut h = harness(&["apps.read"]);
    let out = call(&mut h, "c1", ToolCall::AppsList);
    let (ok, message, _) = answer(&out, "c1").unwrap();
    assert!(!ok);
    assert!(message.starts_with("The meerora app is not open"), "{message}");
    assert!(audit(&h).is_empty(), "nothing was used");
}

#[test]
fn an_app_that_does_not_answer_times_out() {
    let mut h = harness(&["apps.read"]);
    h.daemon.app_tools.set_timeout(Duration::from_millis(40));
    send(&mut h, APP, Command::ServeApps);
    let out = call(&mut h, "c1", ToolCall::AppRead { app: "terminal:1".into(), query: None });
    let (id, ..) = request(&out).unwrap();
    assert!(h.daemon.next_wait() <= Duration::from_millis(40), "the daemon wakes for the deadline");
    h.daemon.tick();
    assert!(answer(&h.daemon.take_out(), "c1").is_none(), "not yet");
    std::thread::sleep(Duration::from_millis(60));
    h.daemon.tick();
    let (ok, message, _) = answer(&h.daemon.take_out(), "c1").expect("a timeout answer");
    assert!(!ok);
    assert!(message.contains("did not answer app_read within 0 seconds") || message.contains("did not answer app_read"), "{message}");
    // The late reply finds nothing to answer.
    assert!(send(&mut h, APP, Command::AppReply { id, result: AppResult::text("late") }).is_empty());
}

#[test]
fn the_newest_app_client_gets_the_requests_until_it_leaves() {
    let mut h = harness(&["apps.read"]);
    send(&mut h, 20, Command::ServeApps);
    send(&mut h, APP, Command::ServeApps);
    let out = call(&mut h, "c1", ToolCall::AppsList);
    assert!(request(&out).is_some(), "the most recent app client");
    assert!(!out.iter().any(|o| matches!(o, Outgoing::To(20, _))));

    h.daemon.client_gone(APP);
    let (ok, message, _) = answer(&h.daemon.take_out(), "c1").expect("the open request fails");
    assert!(!ok && message.starts_with("The meerora app closed before it answered apps_list"), "{message}");
    let out = call(&mut h, "c2", ToolCall::AppsList);
    assert!(out.iter().any(|o| matches!(o, Outgoing::To(20, Event::AppRequest { .. }))), "the older one takes over");
    h.daemon.client_gone(20);
    let out = call(&mut h, "c3", ToolCall::AppsList);
    assert!(answer(&out, "c3").unwrap().1.starts_with("The meerora app is not open"));
    // A client that never served apps leaving changes nothing.
    h.daemon.client_gone(BRIDGE);
}

/// Allow once of an app capability works in the run that asked, without a
/// continuation run, and ends with that run.
#[test]
fn an_app_capability_works_at_once_and_holds_for_the_run() {
    let mut h = harness(&[]);
    send(&mut h, APP, Command::ServeApps);
    h.fake.script(vec![Step::Send(RunEvent::Text { text: "Working.".into() }), Step::Sleep(300), finished("Done.")]);
    h.say("wren", "Look at the terminal");
    let ask = ToolCall::RequestCapability { capability: "apps.read".into(), reason: "to read the terminal".into() };
    let events = h.command(Command::Tool { id: "cap".into(), agent: "wren".into(), call: ask });
    let id = events
        .iter()
        .find_map(|e| match e {
            Event::CapabilityRequest { id, .. } => Some(id.clone()),
            _ => None,
        })
        .expect("a request card");
    let events = h.command(Command::DecideCapability { id, approve: true, remember: false });
    let message = events
        .iter()
        .find_map(|e| match e {
            Event::ToolResult { id, message, .. } if id == "cap" => Some(message.clone()),
            _ => None,
        })
        .unwrap();
    assert!(message.contains("use it right away"), "{message}");

    let out = call(&mut h, "c1", ToolCall::AppsList);
    assert!(request(&out).is_some(), "the grant works in the same run");
    let events = h.until_finished("wren");
    let started = events.iter().filter(|e| matches!(e, Event::RunStarted { .. })).count();
    assert_eq!(started, 0, "no continuation run");
    let out = call(&mut h, "c2", ToolCall::AppsList);
    assert!(!answer(&out, "c2").unwrap().0, "Allow once ended with the run");
}

/// The runtime reports the MCP call of an act as a tool use, but the act
/// logs its own line once the app did it, so the raw call is not logged twice.
#[test]
fn acts_are_logged_once() {
    let mut h = harness(&["apps.read", "apps.control"]);
    let tool = |name: &str| Step::Send(RunEvent::Tool { tool: name.into(), summary: "{\"app\":\"terminal:1\"}".into(), target: None });
    h.fake.script(vec![tool("meerora.app_act"), tool("meerora.app_open"), tool("meerora.app_read"), finished("Done.")]);
    h.say("wren", "Run the tests in your terminal");
    let events = h.until_finished("wren");
    let tools: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            Event::Action { tool, .. } => Some(tool.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(tools, ["meerora.app_read"], "reading stays visible, acts and opens come from the app");
}
