//! The graveyard with the fake runtime: retiring moves the directory and
//! stops the agent, the figure walks to the graveyard before it leaves, a
//! restore brings it back with its schedule, ReloadAgents leaves the
//! graveyard alone and DeleteRetired only ever uses the trash.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use meerora_protocol::{
    Command, Event, GRAVEYARD_DIR, GraveyardAction, RETIRED_FILE, RUNS_LOG, RetiredAgent, RetiredFile, RingState, ToolCall, Trigger,
};
use meerora_runtime::RunEvent;

use super::{CLAUDE, Fake, Harness, Step, text, write_agent};
use crate::daemon::Msg;
use crate::graveyard::{Trash, TrashError};
use crate::store::{Store, tests::TempDir};

/// Retires `agent`, whose figure arrives at the graveyard at once.
fn retire(h: &mut Harness, agent: &str) -> Vec<Event> {
    let mut events = h.command(Command::Retire { agent: agent.into() });
    events.extend(h.command(Command::Arrived { agent: agent.into(), station: "graveyard".into() }));
    events
}

fn restore(h: &mut Harness, grave: &str) -> Vec<Event> {
    h.command(Command::Restore { agent: grave.into() })
}

/// The answer of a graveyard command as (id, error).
fn result(events: &[Event], action: GraveyardAction) -> (Option<String>, Option<String>) {
    events
        .iter()
        .find_map(|e| match e {
            Event::GraveyardResult { action: a, id, error, .. } if *a == action => Some((id.clone(), error.clone())),
            _ => None,
        })
        .expect("every graveyard command is answered")
}

/// The ids of the last `Graveyard` event, if one was sent.
fn graveyard(events: &[Event]) -> Option<Vec<RetiredAgent>> {
    events.iter().rev().find_map(|e| match e {
        Event::Graveyard { agents } => Some(agents.clone()),
        _ => None,
    })
}

fn ids(agents: &[RetiredAgent]) -> Vec<&str> {
    agents.iter().map(|a| a.id.as_str()).collect()
}

fn grave(h: &Harness, id: &str) -> PathBuf {
    h.dir.0.join(GRAVEYARD_DIR).join(id)
}

fn spawned(events: &[Event]) -> Vec<(String, String)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::AgentSpawned { agent, name, .. } => Some((agent.clone(), name.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn retire_moves_the_directory_and_takes_the_agent_out_of_the_space() {
    let mut h = Harness::new();
    let history = h.agent_dir("quill").join(RUNS_LOG);
    std::fs::create_dir_all(history.parent().unwrap()).unwrap();
    std::fs::write(&history, "{}\n{}\n\n{}\n").unwrap();
    std::fs::create_dir_all(h.agent_dir("quill").join("memory")).unwrap();
    std::fs::write(h.agent_dir("quill").join("memory/notes.md"), "keep me").unwrap();
    h.command(Command::Inject { event: Event::Activity { agent: "quill".into(), text: "draft ready".into() } });

    let events = retire(&mut h, "quill");
    assert_eq!(result(&events, GraveyardAction::Retire), (Some("quill".into()), None));
    let removed = events.iter().position(|e| *e == Event::AgentRemoved { agent: "quill".into() }).expect("AgentRemoved");
    let listed = events.iter().position(|e| matches!(e, Event::Graveyard { .. })).expect("Graveyard");
    assert!(removed < listed, "the agent leaves before the graveyard lists it");

    assert!(!h.agent_dir("quill").exists(), "moved, not copied");
    let to = grave(&h, "quill");
    assert_eq!(std::fs::read_to_string(to.join("memory/notes.md")).unwrap(), "keep me");
    assert!(to.join("agent.toml").is_file() && to.join("figure.js").is_file() && to.join(RUNS_LOG).is_file());
    let file: RetiredFile = serde_json::from_str(&std::fs::read_to_string(to.join(RETIRED_FILE)).unwrap()).unwrap();
    assert_eq!((file.agent.as_str(), file.name.as_str(), file.color.as_str()), ("quill", "Quill", "#E8EAF0"));
    assert_eq!((file.last_activity.as_deref(), file.runs), (Some("draft ready"), 3));
    assert!(file.retired_at > 0);

    let list = graveyard(&events).unwrap();
    assert_eq!(ids(&list), ["quill"]);
    assert_eq!((list[0].name.as_str(), list[0].runs, list[0].path.clone()), ("Quill", 3, to.clone()));
    assert!(list[0].figure.contains("export default figure("), "the figure for the portrait");

    // The greeting knows it is gone and lists it in the graveyard.
    let greeting = h.daemon.greeting();
    assert!(!spawned(&greeting).iter().any(|(id, _)| id == "quill"));
    assert_eq!(graveyard(&greeting).map(|l| ids(&l).join(",")), Some("quill".to_string()));
    // It takes no messages and no tool calls any more.
    let (ok, _, _) = h.tool("quill", ToolCall::Note { text: "still here?".into() });
    assert!(!ok);

    // Mock agents retire the same way, and an unknown agent is refused.
    let events = retire(&mut h, "ember");
    assert_eq!(result(&events, GraveyardAction::Retire), (Some("ember".into()), None));
    assert_eq!(ids(&graveyard(&events).unwrap()).len(), 2);
    let events = retire(&mut h, "nobody");
    let (id, error) = result(&events, GraveyardAction::Retire);
    assert!(id.is_none() && error.unwrap().contains("no agent nobody"));
    assert!(graveyard(&events).is_none(), "nothing changed");
}

#[test]
fn the_agent_walks_to_the_graveyard_before_it_leaves() {
    let mut h = Harness::new();
    h.command(Command::Inject { event: Event::Ring { agent: "quill".into(), state: RingState::NeedsYou, progress: None } });
    let events = h.command(Command::Retire { agent: "quill".into() });
    // Retired at once and completely, only the figure still walks.
    assert_eq!(result(&events, GraveyardAction::Retire), (Some("quill".into()), None));
    assert!(grave(&h, "quill").join("agent.toml").is_file() && !h.agent_dir("quill").exists());
    let ring = events.iter().position(|e| *e == Event::Ring { agent: "quill".into(), state: RingState::Resting, progress: None });
    let walk = events.iter().position(|e| *e == Event::GoTo { agent: "quill".into(), station: "graveyard".into() });
    assert!(ring.expect("it walks resting, so the front row lets it go") < walk.expect("GoTo graveyard"));
    assert!(!events.iter().any(|e| matches!(e, Event::AgentRemoved { .. } | Event::Graveyard { .. })), "{events:?}");
    assert!(!spawned(&h.daemon.greeting()).iter().any(|(id, _)| id == "quill"), "the daemon is done with it");

    // Nothing sends it elsewhere on the way, and a second retire changes nothing.
    let goto = Event::GoTo { agent: "quill".into(), station: "desk".into() };
    assert!(h.command(Command::Inject { event: goto }).is_empty());
    assert!(h.say("quill", "wait").is_empty(), "it takes no messages");
    let again = h.command(Command::Retire { agent: "quill".into() });
    assert_eq!(result(&again, GraveyardAction::Retire), (Some("quill".into()), None));
    assert!(!grave(&h, "quill-2").exists());
    assert!(h.command(Command::Arrived { agent: "quill".into(), station: "desk".into() }).is_empty(), "the wrong station");

    // It arrives: the figure leaves and the tombstone appears.
    let events = h.command(Command::Arrived { agent: "quill".into(), station: "graveyard".into() });
    let removed = events.iter().position(|e| *e == Event::AgentRemoved { agent: "quill".into() }).expect("AgentRemoved");
    let listed = events.iter().position(|e| matches!(e, Event::Graveyard { .. })).expect("Graveyard");
    assert!(removed < listed);
    assert_eq!(ids(&graveyard(&events).unwrap()), ["quill"]);
    assert!(h.command(Command::Arrived { agent: "quill".into(), station: "graveyard".into() }).is_empty(), "once");

    // A client that never reports the arrival: the figure leaves after the limit.
    h.daemon.graveyard.walk_limit = Duration::ZERO;
    let events = h.command(Command::Retire { agent: "ember".into() });
    assert!(!events.iter().any(|e| matches!(e, Event::AgentRemoved { .. })));
    h.daemon.tick();
    let events = h.drain();
    assert!(events.contains(&Event::AgentRemoved { agent: "ember".into() }), "{events:?}");
    assert_eq!(ids(&graveyard(&events).unwrap()).len(), 2);
}

#[test]
fn a_restore_during_the_walk_turns_the_agent_back() {
    let mut h = Harness::new();
    let events = h.command(Command::Retire { agent: "claude".into() });
    assert_eq!(result(&events, GraveyardAction::Retire).0.as_deref(), Some("claude"));
    let events = restore(&mut h, "claude");
    assert_eq!(result(&events, GraveyardAction::Restore), (Some("claude".into()), None));
    assert!(!events.iter().any(|e| matches!(e, Event::AgentRemoved { .. })), "the figure stays: {events:?}");
    assert!(events.contains(&Event::GoTo { agent: "claude".into(), station: "bookshelf".into() }), "back home");
    assert!(h.agent_dir("claude").join("agent.toml").is_file() && !grave(&h, "claude").exists());
    assert!(graveyard(&events).unwrap().is_empty());
    // The walk is over: no late arrival or limit takes it away.
    assert!(h.command(Command::Arrived { agent: "claude".into(), station: "graveyard".into() }).is_empty());
    h.daemon.graveyard.walk_limit = Duration::ZERO;
    h.daemon.tick();
    assert!(!h.drain().iter().any(|e| matches!(e, Event::AgentRemoved { .. })));
    assert!(spawned(&h.daemon.greeting()).iter().any(|(id, _)| id == "claude"));

    // Without a graveyard station there is no walk.
    h.daemon.opts.stations.retain(|s| s != "graveyard");
    let events = h.command(Command::Retire { agent: "quill".into() });
    assert!(events.contains(&Event::AgentRemoved { agent: "quill".into() }) && graveyard(&events).is_some());
    assert!(!events.iter().any(|e| matches!(e, Event::GoTo { .. })));
}

#[test]
fn retire_cancels_the_run_and_denies_its_open_request() {
    let mut h = Harness::new();
    h.fake.script(vec![text("Reading."), Step::UntilCancelled]);
    h.say("quill", "Read the news");
    h.say("quill", "and then this");
    h.until(|e| matches!(e, Event::Message { role: meerora_protocol::Role::Agent, .. }));
    let ask = ToolCall::RequestCapability { capability: "web".into(), reason: "to read the news".into() };
    let events = h.command(Command::Tool { id: "c1".into(), agent: "quill".into(), call: ask });
    assert!(events.iter().any(|e| matches!(e, Event::CapabilityRequest { .. })));

    let events = retire(&mut h, "quill");
    let answer = events.iter().find_map(|e| match e {
        Event::ToolResult { id, ok, message, .. } if id == "c1" => Some((*ok, message.clone())),
        _ => None,
    });
    let (ok, message) = answer.expect("the waiting tool call is answered");
    assert!(ok && message.contains("moved to the graveyard"), "{message}");
    assert!(events.contains(&Event::AgentRemoved { agent: "quill".into() }));
    assert!(!h.daemon.greeting().iter().any(|e| matches!(e, Event::CapabilityRequest { .. })));
    let audit = std::fs::read_to_string(h.dir.0.join("audit.jsonl")).unwrap();
    assert!(audit.lines().any(|l| l.contains("\"decision\":\"deny\"") && l.contains("graveyard")), "{audit}");

    // The fake run ends because it was cancelled, and its end changes nothing.
    match h.rx.recv_timeout(Duration::from_secs(5)).expect("the cancelled run reports") {
        msg @ Msg::Run { event: RunEvent::Finished { .. }, .. } => h.daemon.handle_msg(msg),
        _ => panic!("expected the end of the run"),
    }
    assert!(h.drain().is_empty(), "no RunFinished for an agent that left");
    std::thread::sleep(Duration::from_millis(150));
    assert!(h.rx.try_recv().is_err(), "the queued message did not start a run");
    assert_eq!(h.fake.requests().len(), 1);
}

#[test]
fn names_that_are_taken_count_up_in_the_graveyard_and_on_the_way_back() {
    let mut h = Harness::new();
    assert_eq!(result(&retire(&mut h, "claude"), GraveyardAction::Retire).0.as_deref(), Some("claude"));
    Store { session: Some("s1".into()), ..Store::default() }.save(&grave(&h, "claude")).unwrap();

    // A new agent takes the id and retires too.
    write_agent(&h.dir.0, "claude", &CLAUDE.replace("Claude", "Claude II"));
    assert_eq!(spawned(&h.command(Command::ReloadAgents)), [("claude".to_string(), "Claude II".to_string())]);
    let events = retire(&mut h, "claude");
    assert_eq!(result(&events, GraveyardAction::Retire).0.as_deref(), Some("claude-2"));
    assert_eq!(ids(&graveyard(&events).unwrap()).len(), 2);

    // A third one holds the id while the first comes back.
    write_agent(&h.dir.0, "claude", &CLAUDE.replace("Claude", "Claude III"));
    h.command(Command::ReloadAgents);
    let events = restore(&mut h, "claude");
    assert_eq!(result(&events, GraveyardAction::Restore), (Some("claude-2".into()), None));
    let answer = events.iter().position(|e| matches!(e, Event::GraveyardResult { .. })).unwrap();
    let back = events.iter().position(|e| matches!(e, Event::AgentSpawned { agent, .. } if agent == "claude-2")).expect("spawned");
    assert!(answer < back, "the client that asked learns the id first");
    assert_eq!(spawned(&events), [("claude-2".to_string(), "Claude".to_string())]);
    let dir = h.agent_dir("claude-2");
    assert!(dir.join("agent.toml").is_file() && !dir.join(RETIRED_FILE).exists());
    assert!(!grave(&h, "claude").exists());
    assert_eq!(Store::load(&dir).session, None, "the CLI home of the old id stays with the old id");
    assert_eq!(ids(&graveyard(&events).unwrap()), ["claude-2"]);

    // Claude II wants its old id back, which is taken twice by now.
    let events = restore(&mut h, "claude-2");
    assert_eq!(result(&events, GraveyardAction::Restore), (Some("claude-3".into()), None));
    assert_eq!(spawned(&events), [("claude-3".to_string(), "Claude II".to_string())]);
    assert!(graveyard(&events).unwrap().is_empty());
}

#[test]
fn restore_brings_the_agent_home_with_its_schedule_and_calm() {
    let dir = TempDir::new("graveyard-schedule");
    let toml = "name = \"Wren\"\nhome = \"window\"\n[runtime]\nkind = \"claude-cli\"\n\
                [[schedule]]\nevery = \"5m\"\ntask = \"Check the inbox.\"\nstation = \"mailbox\"\n";
    write_agent(&dir.0, "wren", toml);
    // Its last wake was long ago and it waits for the user.
    let store = Store { schedule: [("every 5m".to_string(), 0)].into(), ring: Some(RingState::NeedsYou), ..Store::default() };
    store.save(&dir.0.join("agents/wren")).unwrap();
    let mut h = Harness::open(dir, Fake::default());
    assert!(h.daemon.greeting().contains(&Event::Ring { agent: "wren".into(), state: RingState::NeedsYou, progress: None }));

    retire(&mut h, "wren");
    h.daemon.tick_schedules(&jiff::Zoned::now());
    assert!(!h.drain().iter().any(|e| matches!(e, Event::RunStarted { .. })), "the schedule stopped with it");
    assert_eq!(Store::load(&grave(&h, "wren")).ring, Some(RingState::Resting), "it comes back resting");

    let events = restore(&mut h, "wren");
    assert_eq!(result(&events, GraveyardAction::Restore), (Some("wren".into()), None));
    assert!(events.contains(&Event::AgentSpawned { agent: "wren".into(), name: "Wren".into(), system: false }));
    assert!(events.contains(&Event::GoTo { agent: "wren".into(), station: "window".into() }), "at its home station");
    assert!(events.contains(&Event::Ring { agent: "wren".into(), state: RingState::Resting, progress: None }));
    assert!(events.iter().any(|e| matches!(e, Event::NextWake { agent, at: Some(_), .. } if agent == "wren")));
    h.daemon.tick_schedules(&jiff::Zoned::now());
    let events = h.drain();
    assert!(
        events.iter().any(|e| matches!(e, Event::RunStarted { agent, trigger: Trigger::Schedule, .. } if agent == "wren")),
        "the schedule resumed: {events:?}"
    );
    h.until_finished("wren");
}

#[test]
fn reload_leaves_the_graveyard_alone() {
    let mut h = Harness::new();
    retire(&mut h, "quill");
    let events = h.command(Command::ReloadAgents);
    assert!(spawned(&events).is_empty(), "{events:?}");
    assert!(grave(&h, "quill").join("agent.toml").is_file());
    assert_eq!(graveyard(&h.daemon.greeting()).map(|l| ids(&l).join(",")), Some("quill".to_string()));
    let (id, error) = result(&restore(&mut h, "nobody"), GraveyardAction::Restore);
    assert!(id.is_none() && error.unwrap().contains("no nobody in the graveyard"));
    let (_, error) = result(&restore(&mut h, "../agents/claude"), GraveyardAction::Restore);
    assert!(error.unwrap().contains("not a name in the graveyard"));
}

#[test]
fn a_broken_agent_toml_stays_in_the_graveyard() {
    let mut h = Harness::new();
    retire(&mut h, "quill");
    std::fs::write(grave(&h, "quill").join("agent.toml"), "name = ").unwrap();
    let (id, error) = result(&restore(&mut h, "quill"), GraveyardAction::Restore);
    let error = error.unwrap();
    assert!(id.is_none() && error.contains("does not load") && error.contains("graveyard/quill"), "{error}");
    assert!(grave(&h, "quill").is_dir() && !h.agent_dir("quill").exists());
}

/// Records what it was asked to trash, and moves it into `into` when it succeeds.
struct FakeTrash {
    answer: Result<(), TrashError>,
    into: PathBuf,
    asked: Arc<Mutex<Vec<PathBuf>>>,
}

impl Trash for FakeTrash {
    fn trash(&self, path: &Path) -> Result<(), TrashError> {
        self.asked.lock().unwrap().push(path.to_path_buf());
        if self.answer.is_ok() {
            std::fs::create_dir_all(&self.into).unwrap();
            std::fs::rename(path, self.into.join(path.file_name().unwrap())).unwrap();
        }
        self.answer.clone()
    }
}

#[test]
fn delete_for_good_only_uses_the_trash() {
    let mut h = Harness::new();
    retire(&mut h, "quill");
    let asked = Arc::new(Mutex::new(Vec::new()));
    let trash_dir = h.dir.0.join("fake-trash");
    let use_trash = |h: &mut Harness, answer: Result<(), TrashError>| {
        h.daemon.graveyard.trash = Box::new(FakeTrash { answer, into: trash_dir.clone(), asked: asked.clone() });
        h.command(Command::DeleteRetired { agent: "quill".into() })
    };

    // Without gio the daemon refuses and names the folder.
    let events = use_trash(&mut h, Err(TrashError::Missing));
    let (id, error) = result(&events, GraveyardAction::Delete);
    let error = error.unwrap();
    assert!(id.is_none() && error.contains("gio") && error.contains(&grave(&h, "quill").display().to_string()), "{error}");
    assert!(error.contains("by hand"), "{error}");
    assert!(graveyard(&events).is_none() && grave(&h, "quill").is_dir());

    let events = use_trash(&mut h, Err(TrashError::Failed("no trash on this disk".into())));
    let error = result(&events, GraveyardAction::Delete).1.unwrap();
    assert!(error.contains("no trash on this disk") && error.contains("by hand"), "{error}");
    assert!(grave(&h, "quill").is_dir());

    let events = use_trash(&mut h, Ok(()));
    assert_eq!(result(&events, GraveyardAction::Delete), (None, None));
    assert!(graveyard(&events).unwrap().is_empty());
    assert!(!grave(&h, "quill").exists() && trash_dir.join("quill/agent.toml").is_file(), "moved to the trash, not deleted");
    assert_eq!(asked.lock().unwrap().len(), 3);

    // Gone is gone, and a path outside the graveyard never reaches the trash.
    let events = use_trash(&mut h, Ok(()));
    assert!(result(&events, GraveyardAction::Delete).1.unwrap().contains("no quill in the graveyard"));
    let events = h.command(Command::DeleteRetired { agent: "../agents".into() });
    assert!(result(&events, GraveyardAction::Delete).1.is_some());
    assert_eq!(asked.lock().unwrap().len(), 3);
    assert!(h.agent_dir("claude").is_dir());
}

#[test]
fn gio_refuses_a_path_that_does_not_exist() {
    // Never touches a real file: the path does not exist, so gio fails or is missing.
    let path = std::env::temp_dir().join(format!("meerora-no-such-dir-{}", std::process::id()));
    assert!(!path.exists());
    assert!(crate::graveyard::Gio.trash(&path).is_err());
}
