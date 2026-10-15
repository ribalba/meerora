//! Rivet, the helper, with the fake runtime: it cannot be retired, only it
//! gets the configuration tools, `config_read` keeps secrets, `config_change`
//! refuses what meerora could not read, and an approved change is written,
//! reloaded and logged while a discarded one changes nothing.

use std::time::{Duration, Instant};

use meerora_protocol::{Command, Event, GraveyardAction, RingState, ToolCall};

use super::{CLAUDE, Fake, Harness, Step, write_agent};
use crate::helper;
use crate::store::tests::TempDir;

/// A data dir with Claude and the helper, as `init` writes it.
fn harness() -> Harness {
    let dir = TempDir::new("helper");
    write_agent(&dir.0, "claude", CLAUDE);
    write_agent(&dir.0, "rivet", helper::AGENT_TOML);
    std::fs::write(dir.0.join("meerora.toml"), crate::init::SETTINGS).unwrap();
    Harness::open(dir, Fake::default())
}

fn read(h: &mut Harness, agent: &str, target: &str) -> (bool, String) {
    let (ok, message, _) = h.tool(agent, ToolCall::ConfigRead { target: target.into() });
    (ok, message)
}

/// The helper proposes `content` for `target` with the tool call `call`.
fn change(h: &mut Harness, call: &str, target: &str, content: &str) -> Vec<Event> {
    let tool = ToolCall::ConfigChange { target: target.into(), content: content.into(), reason: "the user asked".into(), figure: None };
    h.command(Command::Tool { id: call.into(), agent: "rivet".into(), call: tool })
}

/// The answer to the tool call `call` in `events`, if it came.
fn answer(events: &[Event], call: &str) -> Option<(bool, String)> {
    events.iter().find_map(|e| match e {
        Event::ToolResult { id, ok, message, .. } if id == call => Some((*ok, message.clone())),
        _ => None,
    })
}

/// The error of a refused proposal.
fn refused(h: &mut Harness, target: &str, content: &str) -> String {
    let events = change(h, "bad", target, content);
    assert!(!events.iter().any(|e| matches!(e, Event::ConfigProposal { .. })), "no card for a refused change");
    match answer(&events, "bad") {
        Some((false, message)) => message,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// The id and diff of the proposal in `events`.
fn proposal(events: &[Event]) -> (String, String) {
    events
        .iter()
        .find_map(|e| match e {
            Event::ConfigProposal { id, diff, .. } => Some((id.clone(), diff.clone())),
            _ => None,
        })
        .unwrap_or_else(|| panic!("a ConfigProposal in {events:?}"))
}

fn audit(h: &Harness) -> Vec<serde_json::Value> {
    std::fs::read_to_string(h.dir.0.join("audit.jsonl")).unwrap_or_default().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

#[test]
fn the_helper_cannot_be_retired() {
    let mut h = harness();
    let events = h.command(Command::Retire { agent: "rivet".into() });
    let error = events
        .iter()
        .find_map(|e| match e {
            Event::GraveyardResult { action: GraveyardAction::Retire, error, .. } => error.clone(),
            _ => None,
        })
        .expect("the refusal says why");
    assert!(error.contains("Rivet is meerora's helper"), "{error}");
    assert!(h.agent_dir("rivet").join("agent.toml").is_file(), "it stays where it is");
    assert!(!events.iter().any(|e| matches!(e, Event::GoTo { .. } | Event::AgentRemoved { .. })));
    assert!(h.daemon.agents.contains_key("rivet"));
}

#[test]
fn only_the_helper_gets_the_configuration_tools() {
    let mut h = harness();
    let (ok, message) = read(&mut h, "claude", "agents");
    assert!(!ok && message.contains("Only meerora's helper"), "{message}");
    let tool = ToolCall::ConfigChange { target: "meerora.toml".into(), content: "".into(), reason: "x".into(), figure: None };
    let (ok, message, events) = h.tool("claude", tool);
    assert!(!ok && message.contains("Only meerora's helper"), "{message}");
    assert!(!events.iter().any(|e| matches!(e, Event::ConfigProposal { .. })));

    let (ok, agents) = read(&mut h, "rivet", "agents");
    assert!(ok, "{agents}");
    assert!(agents.contains("- rivet: Rivet, claude-cli with sonnet, role helper, system agent"), "{agents}");
    assert!(agents.contains("- claude: Claude, claude-cli with haiku"), "{agents}");
    let (ok, toml) = read(&mut h, "rivet", "agent:claude");
    assert!(ok && toml.contains("home = \"bookshelf\""), "{toml}");
    let (ok, settings) = read(&mut h, "rivet", "meerora.toml");
    assert!(ok && settings.starts_with("# meerora settings."), "{settings}");
    let (ok, rooms) = read(&mut h, "rivet", "rooms");
    assert!(ok && rooms.contains("Stations: door,"), "{rooms}");
    let (ok, commands) = read(&mut h, "rivet", "commands");
    assert!(ok && commands.contains("There is no commands.toml yet"), "{commands}");
    let (ok, manual) = read(&mut h, "rivet", "readme:workspace folders");
    assert!(ok && manual.starts_with("### Workspace folders"), "{manual}");
}

#[test]
fn config_read_keeps_secrets() {
    let mut h = harness();
    let token = "tok-0123456789abcdef";
    let account = format!("{{\"index\":\"http://127.0.0.1:7878\",\"handle\":\"robin\",\"token\":\"{token}\"}}");
    std::fs::write(h.dir.0.join("index-account.json"), account).unwrap();
    std::fs::create_dir_all(h.dir.0.join("runtimes/claude")).unwrap();
    std::fs::write(h.dir.0.join("runtimes/claude/.credentials.json"), "secret").unwrap();
    // A token that ends up in a text anyway is hidden.
    let line = format!("{{\"agent\":\"claude\",\"event\":\"use\",\"note\":\"Bearer {token}\"}}\n");
    std::fs::write(h.dir.0.join("audit.jsonl"), line.repeat(60)).unwrap();
    let (ok, tail) = read(&mut h, "rivet", "audit");
    assert!(ok && tail.contains("The last 50 lines") && tail.contains("Bearer [hidden]") && !tail.contains(token), "{tail}");

    for target in
        ["index-account.json", "runtimes/claude/.credentials.json", "agent:../runtimes/claude", "agent:../../etc", "../meerora.toml"]
    {
        let (ok, message) = read(&mut h, "rivet", target);
        assert!(!ok && !message.contains("secret") && !message.contains(token), "{target}: {message}");
    }
}

#[test]
fn config_change_refuses_what_meerora_could_not_read() {
    let mut h = harness();
    let claude = std::fs::read_to_string(h.agent_dir("claude").join("agent.toml")).unwrap();
    assert!(refused(&mut h, "meerora.toml", "[commands\nkey = 1").contains("does not parse"));
    assert!(refused(&mut h, "meerora.toml", "[commands]\nkey = \"Space\"\n").contains("needs Meta, Ctrl or Alt"));
    assert!(refused(&mut h, "meerora.toml", "[commands]\nassistant = \"ember\"\n").contains("no \"ember\""));
    assert!(refused(&mut h, "meerora.toml", crate::init::SETTINGS).contains("nothing to apply"));
    assert!(refused(&mut h, "commands.toml", "[[command]]\nkeyword = \"w\"\n").contains("does not parse"));
    assert!(refused(&mut h, "agent:claude", &claude.replace("bookshelf", "moon")).contains("not \"moon\""));
    assert!(refused(&mut h, "agent:claude", &claude.replace("name = \"Claude\"", "name = \"Claude\"\nhat = 1")).contains("does not parse"));
    let grant = |list: &str| format!("{claude}\n[capabilities]\ngranted = {list}\n");
    assert!(refused(&mut h, "agent:claude", &grant("[\"Web Access\"]")).contains("not a capability name"));
    assert!(refused(&mut h, "agent:claude", &grant("[\"web\", \"web\"]")).contains("twice"));
    let helper_role = claude.replace("name = \"Claude\"", "name = \"Claude\"\nrole = \"helper\"");
    assert!(refused(&mut h, "agent:claude", &helper_role).contains("Only one agent is meerora's helper"));
    let rivet = std::fs::read_to_string(h.agent_dir("rivet").join("agent.toml")).unwrap();
    assert!(refused(&mut h, "agent:rivet", &rivet.replace("role = \"helper\"\n", "")).contains("keeps role = \"helper\""));
    let workspace = claude.replace("name = \"Claude\"", "name = \"Claude\"\nworkspace = [\"/etc\"]");
    assert!(refused(&mut h, "agent:claude", &workspace).contains("workspace folder /etc is refused"));
    assert!(refused(&mut h, "agent:ember", &claude).contains("There is no agent \"ember\""));
    assert!(refused(&mut h, "new_agent:claude", &claude).contains("exists already"));
    assert!(refused(&mut h, "new_agent:mock", "name = \"M\"\nhome = \"desk\"\n").contains("[runtime] kind"));
    assert!(refused(&mut h, "runtimes/claude", "x").contains("cannot be changed"));

    let figure = ToolCall::ConfigChange {
        target: "new_agent:mail".into(),
        content: CLAUDE.into(),
        reason: "x".into(),
        figure: Some("export default figure({});".into()),
    };
    let (ok, message, _) = h.tool("rivet", figure);
    assert!(!ok && message.contains("validator"), "{message}");
    assert!(!h.dir.0.join("agents/mail").exists());
    assert!(audit(&h).is_empty(), "refusals are no proposals");
}

#[test]
fn an_applied_change_is_written_reloaded_and_logged() {
    let mut h = harness();
    let settings = crate::init::SETTINGS.to_string();
    let wanted = format!("{settings}\n[commands]\nkey = \"Ctrl+Space\"\n");
    // The helper leaves the leading comment out, and meerora keeps it.
    let content = wanted.trim_start_matches("# meerora settings. Every value is optional, the comments show the defaults.\n");
    let events = change(&mut h, "c1", "meerora.toml", content);
    assert_eq!(answer(&events, "c1"), None, "the call waits for the user");
    let (id, diff) = proposal(&events);
    assert!(diff.starts_with("--- a/meerora.toml\n+++ b/meerora.toml\n@@ "), "{diff}");
    assert!(diff.contains("\n+[commands]\n+key = \"Ctrl+Space\"\n"), "{diff}");
    assert!(events.contains(&Event::Ring { agent: "rivet".into(), state: RingState::NeedsYou, progress: None }));
    assert!(h.daemon.greeting().iter().any(|e| matches!(e, Event::ConfigProposal { id: i, .. } if *i == id)), "sent again on hello");
    assert_eq!(std::fs::read_to_string(h.dir.0.join("meerora.toml")).unwrap(), settings, "nothing is written before Apply");

    let events = h.command(Command::DecideConfig { id: id.clone(), apply: true });
    let (ok, message) = answer(&events, "c1").expect("the waiting call is answered");
    assert!(ok && message.contains("applied") && message.contains("meerora.toml is written"), "{message}");
    assert!(events.iter().any(|e| matches!(e, Event::ConfigResult { id: i, applied: true, .. } if *i == id)));
    assert_eq!(std::fs::read_to_string(h.dir.0.join("meerora.toml")).unwrap(), wanted, "written with its first comment");
    let commands = h.daemon.opts.settings.commands.clone().expect("the daemon reloaded meerora.toml");
    assert_eq!(commands["key"].as_str(), Some("Ctrl+Space"));
    let log = audit(&h);
    let events: Vec<(&str, &str)> = log.iter().map(|l| (l["event"].as_str().unwrap(), l["by"].as_str().unwrap())).collect();
    assert_eq!(events, [("propose", "rivet"), ("decision", "user")]);
    assert_eq!((log[1]["decision"].as_str(), log[1]["config"].as_str()), (Some("apply"), Some("meerora.toml")));
    assert_eq!(log[0]["request"], log[1]["request"]);
    assert!(h.daemon.greeting().iter().all(|e| !matches!(e, Event::ConfigProposal { .. })));

    // The same decision again finds nothing.
    let events = h.command(Command::DecideConfig { id, apply: true });
    assert!(events.iter().any(|e| matches!(e, Event::ConfigResult { applied: false, message: Some(m), .. } if m.contains("unknown"))));
}

#[test]
fn a_granted_capability_and_a_new_agent_take_effect() {
    let mut h = harness();
    let claude = std::fs::read_to_string(h.agent_dir("claude").join("agent.toml")).unwrap();
    let events = change(&mut h, "c1", "agent:claude", &format!("{claude}\n[capabilities]\ngranted = [\"web\"]\n"));
    let (id, diff) = proposal(&events);
    assert!(diff.contains("+granted = [\"web\"]"), "{diff}");
    h.command(Command::DecideConfig { id, apply: true });
    assert!(h.daemon.agents["claude"].dir.config.capabilities.has("web"), "the daemon reloaded claude");

    let toml = "name = \"Penny\"\nhome = \"desk\"\napps = [\"chat\"]\n\n[runtime]\nkind = \"claude-cli\"\nmodel = \"haiku\"\n";
    let events = change(&mut h, "c2", "new_agent:penny", toml);
    let (id, diff) = proposal(&events);
    assert!(diff.contains("--- /dev/null\n+++ b/agents/penny/figure.js") && diff.contains("+++ b/agents/penny/agent.toml"), "{diff}");
    let events = h.command(Command::DecideConfig { id, apply: true });
    let (_, message) = answer(&events, "c2").unwrap();
    assert!(message.contains("The agent penny is in the space now"), "{message}");
    assert!(events.iter().any(|e| matches!(e, Event::AgentSpawned { agent, .. } if agent == "penny")), "{events:?}");
    assert_eq!(std::fs::read_to_string(h.agent_dir("penny").join("figure.js")).unwrap(), helper::NEW_AGENT_FIGURE);
}

#[test]
fn discard_cancel_conflict_and_timeout_write_nothing() {
    let mut h = harness();
    let before = std::fs::read_to_string(h.dir.0.join("meerora.toml")).unwrap();
    let content = format!("{before}[space]\nroom = \"office\"\n");

    let (id, _) = proposal(&change(&mut h, "c1", "meerora.toml", &content));
    let events = h.command(Command::DecideConfig { id, apply: false });
    let (ok, message) = answer(&events, "c1").unwrap();
    assert!(ok && message.contains("discarded"), "{message}");
    assert_eq!(std::fs::read_to_string(h.dir.0.join("meerora.toml")).unwrap(), before);

    // The file changed after the proposal, so Apply does not overwrite it.
    let (id, _) = proposal(&change(&mut h, "c2", "meerora.toml", &content));
    std::fs::write(h.dir.0.join("meerora.toml"), "# mine\n").unwrap();
    let events = h.command(Command::DecideConfig { id, apply: true });
    let (_, message) = answer(&events, "c2").unwrap();
    assert!(message.contains("changed since Rivet proposed this"), "{message}");
    assert_eq!(std::fs::read_to_string(h.dir.0.join("meerora.toml")).unwrap(), "# mine\n");

    // Cancelling the helper's run closes its proposal.
    h.fake.script(vec![Step::UntilCancelled]);
    h.say("rivet", "Switch to the office.");
    let (id, _) = proposal(&change(&mut h, "c3", "meerora.toml", "[space]\nroom = \"office\"\n"));
    let events = h.command(Command::Cancel { agent: "rivet".into() });
    let (_, message) = answer(&events, "c3").unwrap();
    assert!(message.contains("closed, because the run was cancelled"), "{message}");
    assert!(events.iter().any(|e| matches!(e, Event::ConfigResult { id: i, applied: false, .. } if *i == id)));
    h.until_finished("rivet");

    // Nobody decides in time.
    h.daemon.proposals.set_wait(Duration::from_millis(1));
    proposal(&change(&mut h, "c4", "meerora.toml", "[space]\nroom = \"office\"\n"));
    std::thread::sleep(Duration::from_millis(5));
    assert_eq!(h.daemon.proposals_wait(Instant::now()), Some(Duration::ZERO));
    h.daemon.tick();
    let events = h.drain();
    let (_, message) = answer(&events, "c4").unwrap();
    assert!(message.contains("nobody decided it in time"), "{message}");
    assert_eq!(std::fs::read_to_string(h.dir.0.join("meerora.toml")).unwrap(), "# mine\n");
    let closes = audit(&h).iter().filter(|l| l["event"] == "close").count();
    assert_eq!(closes, 2);
}

#[test]
fn the_helper_prompt_explains_meerora_and_its_tools() {
    let h = harness();
    let rivet = &h.daemon.agents["rivet"].dir;
    let prompt = crate::runs::system_prompt(rivet, &h.daemon.opts.stations);
    for needle in ["meerora's helper", "config_read", "config_change", "readme:<words>", "terminal:1", "apps.control"] {
        assert!(prompt.contains(needle), "missing {needle}");
    }
    let claude = crate::runs::system_prompt(&h.daemon.agents["claude"].dir, &h.daemon.opts.stations);
    assert!(!claude.contains("config_change"), "other agents do not hear about it");
}
