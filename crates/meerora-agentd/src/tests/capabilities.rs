//! The capability broker (D8, Q4) with the fake runtime: requests, the three
//! answers, revocation, the audit log, the greeting, the continuation run and
//! the bridge socket.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use meerora_protocol::{Command, Event, Outcome, PROTOCOL_VERSION, RingState, Role, ToolCall, encode};
use meerora_runtime::{Isolation, RunEvent};
use serde_json::Value;

use super::{Fake, Harness, Step, finished, finished_with, rings, text, write_agent};
use crate::daemon::Msg;
use crate::store::tests::TempDir;

/// Wren rests after a run and has comments that must survive edits.
const WREN_TOML: &str = "# Wren reads the mail.\n# It lives by the window.\nname = \"Wren\"\nhome = \"window\"\non_done = \"rest\"\n\n[runtime]\nkind = \"claude-cli\"\n";

fn harness(toml: &str) -> Harness {
    let dir = TempDir::new("caps");
    write_agent(&dir.0, "wren", toml);
    Harness::open(dir, Fake::default())
}

/// Wren's bridge asks for `capability` with the tool call `call`.
fn ask(h: &mut Harness, call: &str, capability: &str) -> Vec<Event> {
    let request = ToolCall::RequestCapability { capability: capability.into(), reason: "to read the release notes".into() };
    h.command(Command::Tool { id: call.into(), agent: "wren".into(), call: request })
}

/// The request id of the first `CapabilityRequest` in `events`.
fn request_id(events: &[Event]) -> String {
    events
        .iter()
        .find_map(|e| match e {
            Event::CapabilityRequest { id, .. } => Some(id.clone()),
            _ => None,
        })
        .expect("a CapabilityRequest")
}

/// The answer to the tool call `call`, if one was sent.
fn answer(events: &[Event], call: &str) -> Option<(bool, String)> {
    events.iter().find_map(|e| match e {
        Event::ToolResult { id, ok, message, .. } if id == call => Some((*ok, message.clone())),
        _ => None,
    })
}

fn granted(events: &[Event]) -> Vec<Vec<String>> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::CapabilitiesChanged { granted, .. } => Some(granted.clone()),
            _ => None,
        })
        .collect()
}

/// `(event, decision, by)` of every line of the audit log.
fn audit(h: &Harness) -> Vec<(String, String, String)> {
    let text = std::fs::read_to_string(h.dir.0.join("audit.jsonl")).unwrap_or_default();
    text.lines()
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            assert_eq!(v["agent"], "wren");
            assert!(v["time"].as_str().unwrap().contains('T'), "{v}");
            let field = |k: &str| v[k].as_str().unwrap_or("").to_string();
            (field("event"), field("decision"), field("by"))
        })
        .collect()
}

fn line(event: &str, decision: &str, by: &str) -> (String, String, String) {
    (event.into(), decision.into(), by.into())
}

fn agent_toml(h: &Harness) -> String {
    std::fs::read_to_string(h.agent_dir("wren").join("agent.toml")).unwrap()
}

#[test]
fn allow_once_continues_the_conversation_and_ends_with_that_run() {
    let mut h = harness(WREN_TOML);
    h.fake.script(vec![Step::Send(RunEvent::Session { id: "s1".into() }), text("Let me look."), Step::Sleep(300), finished("Continuing.")]);
    h.fake.script(vec![finished("The notes say 2.1 is out.")]);
    h.say("wren", "What is new?");

    let events = ask(&mut h, "c1", "web");
    assert_eq!(answer(&events, "c1"), None, "the call waits for the user");
    let id = request_id(&events);
    assert!(events.contains(&Event::CapabilityRequest {
        id: id.clone(),
        agent: "wren".into(),
        capability: "web".into(),
        reason: "to read the release notes".into()
    }));
    assert_eq!(rings(&events), [RingState::NeedsYou], "the figure waits with a full ring (D3)");
    assert!(events.contains(&Event::Activity { agent: "wren".into(), text: "asks for web".into() }));
    // Opening the agent view sends an ack, which must not end the run.
    assert!(rings(&h.command(Command::Ack { agent: "wren".into() })).is_empty());

    let events = h.command(Command::DecideCapability { id: id.clone(), approve: true, remember: false });
    let (ok, message) = answer(&events, "c1").expect("the waiting call is answered");
    assert!(ok && message.contains("End your turn"), "{message}");
    assert_eq!(granted(&events), [vec!["web".to_string()]]);
    assert_eq!(rings(&events), [RingState::Working]);
    assert!(h.command(Command::DecideCapability { id, approve: false, remember: false }).is_empty(), "decided once");

    // The run ends its turn, and a new run continues the same conversation with web.
    let events = h.until(|e| matches!(e, Event::RunStarted { .. }));
    assert_eq!(finished_with(&events), (Outcome::Done, Some("continues with web".into())));
    assert!(!events.iter().any(|e| matches!(e, Event::GoTo { .. })), "the figure keeps working at its place");
    assert!(events.iter().any(|e| matches!(e, Event::RunStarted { task, .. } if task == "The user allowed you web. Continue the task.")));
    let events = h.until_finished("wren");
    assert_eq!(granted(&events), [Vec::<String>::new()], "Allow once ends with the run that used it");

    let requests = h.fake.requests();
    assert_eq!(requests.len(), 2);
    assert!(!requests[0].permissions.web && requests[1].permissions.web);
    assert_eq!(requests[1].session.as_deref(), Some("s1"), "the same conversation");
    assert!(requests[1].system.contains("fetch web pages"));
    h.say("wren", "And now?");
    h.until_finished("wren");
    assert!(!h.fake.requests()[2].permissions.web, "the grant is gone");
    assert_eq!(agent_toml(&h), WREN_TOML, "agent.toml stays as it is");
    assert_eq!(audit(&h), [line("request", "", "wren"), line("decision", "allow_once", "user"), line("expire", "", "meerora")]);
}

#[test]
fn always_allow_writes_agent_toml_and_a_stuck_run_is_continued() {
    let mut h = harness(WREN_TOML);
    h.fake.script(vec![Step::Send(RunEvent::Session { id: "s1".into() }), text("Fetching."), Step::UntilCancelled]);
    h.fake.script(vec![finished("Done with web.")]);
    h.say("wren", "Fetch the page");
    h.until(|e| matches!(e, Event::Message { role: Role::Agent, .. }));
    let id = request_id(&ask(&mut h, "c1", "web"));
    let events = h.command(Command::DecideCapability { id, approve: true, remember: true });
    assert_eq!(granted(&events), [vec!["web".to_string()]]);
    let toml = agent_toml(&h);
    assert!(toml.starts_with("# Wren reads the mail.\n# It lives by the window.\n"), "{toml}");
    assert!(toml.ends_with("[capabilities]\ngranted = [\"web\"]\n"), "{toml}");
    let config: meerora_protocol::AgentConfig = toml::from_str(&toml).unwrap();
    assert_eq!(config.capabilities.granted, ["web"]);

    // The run does not end its turn, so it is stopped once the grace time is over.
    if let Some(run) = h.daemon.agents.get_mut("wren").and_then(|a| a.live.as_mut()).and_then(|l| l.run.as_mut()) {
        run.continue_by = Some(Instant::now() - Duration::from_secs(1));
    }
    assert_eq!(h.daemon.next_wait(), Duration::ZERO);
    h.daemon.tick_continuations(Instant::now());
    let events = h.until(|e| matches!(e, Event::RunStarted { .. }));
    assert_eq!(finished_with(&events).0, Outcome::Done, "a continuation, not a cancel");
    assert!(!events.iter().any(|e| matches!(e, Event::Message { role: Role::System, .. })));
    h.until_finished("wren");
    h.say("wren", "Again");
    h.until_finished("wren");
    let requests = h.fake.requests();
    assert!(requests[1].permissions.web && requests[2].permissions.web, "always");
    assert_eq!(requests[1].session.as_deref(), Some("s1"));
    assert_eq!(audit(&h), [line("request", "", "wren"), line("decision", "always_allow", "user")]);

    // Asking again for what it holds is answered at once.
    h.fake.script(vec![Step::Sleep(200), finished("ok")]);
    h.say("wren", "Once more");
    let events = ask(&mut h, "c2", "web");
    assert_eq!(answer(&events, "c2"), Some((true, "You already hold web.".into())));
    h.until_finished("wren");
}

#[test]
fn deny_answers_and_the_run_goes_on_without_it() {
    let mut h = harness(WREN_TOML);
    h.fake.script(vec![Step::Sleep(300), finished("I could not run the tests.")]);
    h.say("wren", "Run the tests");
    let events = ask(&mut h, "c1", "shell");
    let id = request_id(&events);
    // A second call for the same capability waits for the same decision.
    assert!(ask(&mut h, "c2", "shell").is_empty());
    let events = h.command(Command::DecideCapability { id, approve: false, remember: true });
    for call in ["c1", "c2"] {
        let (ok, message) = answer(&events, call).unwrap();
        assert!(ok && message.contains("did not allow shell"), "{message}");
    }
    assert_eq!(granted(&events), [Vec::<String>::new()], "the card closes");
    assert_eq!(rings(&events), [RingState::Working]);
    let events = h.until_finished("wren");
    assert_eq!(finished_with(&events).0, Outcome::Done);
    assert_eq!(h.fake.requests().len(), 1, "no continuation");
    assert_eq!(agent_toml(&h), WREN_TOML);
    assert_eq!(audit(&h), [line("request", "", "wren"), line("decision", "deny", "user")]);
}

#[test]
fn bad_requests_are_refused_at_once() {
    let mut h = harness(WREN_TOML);
    let (ok, message) = answer(&ask(&mut h, "c1", "Web Access"), "c1").unwrap();
    assert!(!ok && message.contains("not a capability name"), "{message}");
    let call = ToolCall::RequestCapability { capability: "web".into(), reason: "  ".into() };
    let events = h.command(Command::Tool { id: "c2".into(), agent: "wren".into(), call });
    assert!(!answer(&events, "c2").unwrap().0);
    let call = ToolCall::RequestCapability { capability: "web".into(), reason: "x".into() };
    let events = h.command(Command::Tool { id: "c3".into(), agent: "nobody".into(), call });
    assert!(!answer(&events, "c3").unwrap().0);
    assert!(audit(&h).is_empty());
}

#[test]
fn revoke_rewrites_agent_toml_and_stops_a_run_that_uses_it() {
    let mut h = harness(&format!("{WREN_TOML}\n[capabilities]\n# granted by hand\ngranted = [\"web\", \"mail.read\"]\n"));
    h.fake.script(vec![text("Reading."), Step::UntilCancelled]);
    h.say("wren", "Read the news");
    h.until(|e| matches!(e, Event::Message { role: Role::Agent, .. }));
    assert!(h.fake.requests()[0].permissions.web);

    let events = h.command(Command::RevokeCapability { agent: "wren".into(), capability: "web".into() });
    assert_eq!(granted(&events), [vec!["mail.read".to_string()]]);
    let toml = agent_toml(&h);
    assert!(toml.starts_with("# Wren reads the mail.\n") && toml.contains("# granted by hand\ngranted = [\"mail.read\"]\n"), "{toml}");
    assert!(events.iter().any(|e| matches!(e, Event::Message { role: Role::System, text, .. } if text.contains("web was revoked"))));
    let events = h.until_finished("wren");
    assert_eq!(finished_with(&events).0, Outcome::Cancelled);

    // A device capability does not stop anything, and revoking twice changes nothing.
    h.command(Command::RevokeCapability { agent: "wren".into(), capability: "mail.read".into() });
    let events = h.command(Command::RevokeCapability { agent: "wren".into(), capability: "mail.read".into() });
    assert_eq!(granted(&events), [Vec::<String>::new()]);
    assert_eq!(audit(&h), [line("revoke", "", "user"), line("revoke", "", "user")]);
}

#[test]
fn open_requests_come_with_the_greeting_and_a_cancel_denies_them() {
    let mut h = harness(WREN_TOML);
    h.fake.script(vec![Step::UntilCancelled]);
    h.say("wren", "Work");
    let id = request_id(&ask(&mut h, "c1", "web"));
    let greeting = h.daemon.greeting();
    let changed = greeting.iter().position(|e| matches!(e, Event::CapabilitiesChanged { agent, .. } if agent == "wren"));
    let request = greeting.iter().position(|e| matches!(e, Event::CapabilityRequest { id: i, .. } if *i == id));
    assert!(changed.unwrap() < request.unwrap(), "the open request follows the agent's capabilities");

    let events = h.command(Command::Cancel { agent: "wren".into() });
    let (ok, message) = answer(&events, "c1").unwrap();
    assert!(ok && message.contains("was cancelled"), "{message}");
    assert_eq!(granted(&events), [Vec::<String>::new()]);
    assert!(!h.daemon.greeting().iter().any(|e| matches!(e, Event::CapabilityRequest { .. })));
    h.until_finished("wren");
    assert_eq!(audit(&h), [line("request", "", "wren"), line("decision", "deny", "meerora")]);
}

#[test]
fn runs_get_their_cli_home_sandbox_and_bridge_socket() {
    let mut h = harness(WREN_TOML);
    h.say("wren", "hi");
    h.until_finished("wren");
    let request = &h.fake.requests()[0];
    let socket = h.dir.0.join("bridges/wren.sock");
    assert_eq!(
        request.isolation,
        Some(Isolation {
            runtimes: h.dir.0.join("runtimes"),
            sandbox: true,
            // The room, and the script API for writing devices (runs.rs).
            read: vec![h.dir.0.join("spaces/workshop"), h.dir.0.join("docs/API.md")],
            sockets: vec![socket.clone()],
            workspace: Vec::new(),
        })
    );
    let args = &request.mcp.as_ref().unwrap().args;
    assert_eq!(args[args.len() - 2..], ["--socket".to_string(), socket.display().to_string()], "the bridge uses its own socket");
    let space = h.dir.0.join("spaces/workshop").display().to_string();
    assert_eq!(args[args.len() - 4..args.len() - 2], ["--space".to_string(), space], "the bridge reads the stations of the room");

    // The bridge socket speaks only for Wren and refuses decisions.
    let mut stream = UnixStream::connect(&socket).unwrap();
    let decide = Command::DecideCapability { id: "cap-1".into(), approve: true, remember: true };
    let note = Command::Tool { id: "t1".into(), agent: "claude".into(), call: ToolCall::Note { text: "hi".into() } };
    let hello = Command::Hello { version: PROTOCOL_VERSION, client: "test".into() };
    for command in [&hello, &decide, &note] {
        stream.write_all(encode(command).as_bytes()).unwrap();
    }
    let mut commands = Vec::new();
    while commands.len() < 2 {
        match h.rx.recv_timeout(Duration::from_secs(5)).expect("the bridge socket reports") {
            Msg::Connected { scoped, .. } => assert!(scoped),
            Msg::Command { command, .. } => commands.push(command),
            _ => {}
        }
    }
    let expected = Command::Tool { id: "t1".into(), agent: "wren".into(), call: ToolCall::Note { text: "hi".into() } };
    assert_eq!(commands, [hello, expected]);
}

/// Workspace folders from agent.toml reach the run checked, and the preamble
/// names them. A refused folder is left out.
#[test]
fn runs_get_the_checked_workspace_folders() {
    let project = TempDir::new("ws-project");
    let toml = format!("name = \"Wren\"\nhome = \"window\"\nworkspace = [{:?}, \"/etc\"]\n\n[runtime]\nkind = \"claude-cli\"\n", project.0);
    let mut h = harness(&toml);
    h.say("wren", "hi");
    h.until_finished("wren");
    let request = &h.fake.requests()[0];
    let folder = project.0.canonicalize().unwrap();
    assert_eq!(request.isolation.as_ref().map(|i| i.workspace.clone()), Some(vec![folder.clone()]));
    assert!(request.system.contains(&format!("workspace folders: {}", folder.display())), "{}", request.system);
    assert!(!request.system.contains("/etc"));
}

/// The daemon records which repositories of a workspace folder existed
/// before its first run (trusted) and which appeared while the agent could
/// write there (untrusted), at the end of that run or at the next start.
#[test]
fn workspace_repositories_get_a_trust_record() {
    use meerora_protocol::workspace::{Trust, Verdict};
    let project = TempDir::new("ws-trust");
    std::fs::create_dir_all(project.0.join(".git")).unwrap();
    let toml = format!("name = \"Wren\"\nhome = \"window\"\nworkspace = [{:?}]\n\n[runtime]\nkind = \"claude-cli\"\n", project.0);
    let mut h = harness(&toml);
    let folder = project.0.canonicalize().unwrap();
    h.say("wren", "hi");
    // The agent makes a repository while its run goes.
    std::fs::create_dir_all(folder.join("lib/.git")).unwrap();
    h.until_finished("wren");
    let trust = Trust::load(&h.dir.0);
    assert!(trust.trusted.contains(&folder.join(".git")), "present before the first run: {trust:?}");
    assert_eq!(trust.verdict(&folder.join("lib/.git")), Verdict::Untrusted("wren".into()));
    assert_eq!(trust.untrusted.get(&folder.join("lib/.git")).map(String::as_str), Some("wren"), "recorded when the run ended");

    // One that appears between runs is untrusted at the next start.
    std::fs::create_dir_all(folder.join("docs/.git")).unwrap();
    h.say("wren", "again");
    h.until_finished("wren");
    assert!(Trust::load(&h.dir.0).untrusted.contains_key(&folder.join("docs/.git")));
}
