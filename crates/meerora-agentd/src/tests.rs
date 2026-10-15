//! Tests of the daemon with a fake runtime, so they never start a real LLM.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use meerora_protocol::{Command, DraftStatus, Event, Outcome, RingState, Role, RuntimeConfig, RuntimeKind, Settings, ToolCall, Trigger};
use meerora_runtime::{RunEvent, RunHandle, RunRequest, Runtime};
use serde_json::json;

use crate::builder::{self, BuildJob};
use crate::daemon::{Daemon, Msg, Options, Outgoing, RuntimeFactory};
use crate::scenario::Scenario;
use crate::store::{Store, tests::TempDir};

const WREN: &str = include_str!("../../../assets/agents/wren/figure.js");

/// One step of a fake run.
#[derive(Clone)]
enum Step {
    Send(RunEvent),
    Sleep(u64),
    /// Waits until the run is cancelled, then finishes with an error.
    UntilCancelled,
}

/// A runtime that plays scripted runs and records the requests.
#[derive(Clone, Default)]
struct Fake {
    scripts: Arc<Mutex<VecDeque<Vec<Step>>>>,
    requests: Arc<Mutex<Vec<RunRequest>>>,
}

impl Fake {
    fn script(&self, steps: Vec<Step>) {
        self.scripts.lock().unwrap().push_back(steps);
    }

    fn requests(&self) -> Vec<RunRequest> {
        self.requests.lock().unwrap().clone()
    }

    fn factory(&self) -> RuntimeFactory {
        let fake = self.clone();
        Arc::new(move |_: &RuntimeConfig, _: &Settings| Ok(Box::new(fake.clone()) as Box<dyn Runtime>))
    }
}

impl Runtime for Fake {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn start(&self, request: RunRequest) -> anyhow::Result<RunHandle> {
        self.requests.lock().unwrap().push(request);
        let steps = self.scripts.lock().unwrap().pop_front().unwrap_or_else(|| vec![finished("ok")]);
        let (tx, rx) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        std::thread::spawn(move || {
            for step in steps {
                if flag.load(Ordering::SeqCst) {
                    let _ = tx.send(RunEvent::Finished { result: Err("cancelled".into()) });
                    return;
                }
                match step {
                    Step::Send(event) => {
                        if tx.send(event).is_err() {
                            return;
                        }
                    }
                    Step::Sleep(ms) => std::thread::sleep(Duration::from_millis(ms)),
                    Step::UntilCancelled => {
                        while !flag.load(Ordering::SeqCst) {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        let _ = tx.send(RunEvent::Finished { result: Err("cancelled".into()) });
                        return;
                    }
                }
            }
        });
        Ok(RunHandle::new(rx, Box::new(move || cancelled.store(true, Ordering::SeqCst))))
    }
}

fn text(t: &str) -> Step {
    Step::Send(RunEvent::Text { text: t.into() })
}

fn finished(answer: &str) -> Step {
    Step::Send(RunEvent::Finished { result: Ok(answer.into()) })
}

/// A daemon on a temp data dir with two real agents and one mock.
struct Harness {
    daemon: Daemon,
    rx: Receiver<Msg>,
    fake: Fake,
    dir: TempDir,
}

const CLAUDE: &str =
    "name = \"Claude\"\nhome = \"bookshelf\"\ninstructions = \"Be brief.\"\n[runtime]\nkind = \"claude-cli\"\nmodel = \"haiku\"\n";
const QUILL: &str = "name = \"Quill\"\nhome = \"desk\"\non_done = \"rest\"\n[runtime]\nkind = \"claude-cli\"\n";
const EMBER: &str = "name = \"Ember\"\nhome = \"workbench\"\n[runtime]\nkind = \"mock\"\n";

fn write_agent(data: &Path, id: &str, toml: &str) {
    let dir = data.join("agents").join(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("agent.toml"), toml).unwrap();
    std::fs::write(dir.join("figure.js"), WREN).unwrap();
}

impl Harness {
    fn new() -> Self {
        let dir = TempDir::new("daemon");
        for (id, toml) in [("claude", CLAUDE), ("quill", QUILL), ("ember", EMBER)] {
            write_agent(&dir.0, id, toml);
        }
        Self::open(dir, Fake::default())
    }

    /// Starts a daemon on an existing data dir, as after a restart.
    fn open(dir: TempDir, fake: Fake) -> Self {
        let (tx, rx) = mpsc::channel();
        let agents = meerora_protocol::load_agents(&dir.0).into_iter().map(Result::unwrap).collect();
        let options = Options {
            data_dir: dir.0.clone(),
            socket: dir.0.join("agentd.sock"),
            settings: Settings::default(),
            stations: crate::scenario::tests::stations(),
            factory: fake.factory(),
            bridge: Some("/opt/meerora/meerora-mcp".into()),
            git: false,
            speed: 1.0,
        };
        let daemon = Daemon::new(options, Scenario::default(), agents, tx);
        Self { daemon, rx, fake, dir }
    }

    fn command(&mut self, command: Command) -> Vec<Event> {
        self.daemon.handle_command(7, command);
        self.drain()
    }

    fn say(&mut self, agent: &str, text: &str) -> Vec<Event> {
        self.command(Command::Say { agent: agent.into(), text: text.into() })
    }

    fn tool(&mut self, agent: &str, call: ToolCall) -> (bool, String, Vec<Event>) {
        let events = self.command(Command::Tool { id: "t1".into(), agent: agent.into(), call });
        let result = events.iter().find_map(|e| match e {
            Event::ToolResult { id, ok, message, .. } if id == "t1" => Some((*ok, message.clone())),
            _ => None,
        });
        let (ok, message) = result.expect("every tool call is answered");
        (ok, message, events)
    }

    fn drain(&mut self) -> Vec<Event> {
        self.daemon
            .take_out()
            .into_iter()
            .map(|o| match o {
                Outgoing::All(e) | Outgoing::To(_, e) => e,
            })
            .collect()
    }

    /// Handles messages from runs until `done` matches an event.
    fn until(&mut self, done: impl Fn(&Event) -> bool) -> Vec<Event> {
        let mut events = Vec::new();
        loop {
            let msg = self.rx.recv_timeout(Duration::from_secs(5)).expect("the run reports before the timeout");
            self.daemon.handle_msg(msg);
            let new = self.drain();
            let finished = new.iter().any(&done);
            events.extend(new);
            if finished {
                return events;
            }
        }
    }

    fn until_finished(&mut self, agent: &str) -> Vec<Event> {
        self.until(|e| matches!(e, Event::RunFinished { agent: a, .. } if a == agent))
    }

    fn agent_dir(&self, id: &str) -> std::path::PathBuf {
        self.dir.0.join("agents").join(id)
    }
}

fn finished_with(events: &[Event]) -> (Outcome, Option<String>) {
    events
        .iter()
        .find_map(|e| match e {
            Event::RunFinished { outcome, summary, .. } => Some((*outcome, summary.clone())),
            _ => None,
        })
        .expect("a RunFinished event")
}

fn rings(events: &[Event]) -> Vec<RingState> {
    events.iter().filter_map(|e| if let Event::Ring { state, .. } = e { Some(*state) } else { None }).collect()
}

#[test]
fn say_starts_a_run_and_maps_its_events() {
    let mut h = Harness::new();
    h.fake.script(vec![
        Step::Send(RunEvent::Session { id: "s1".into() }),
        text("Hello."),
        Step::Send(RunEvent::Tool { tool: "terminal.run".into(), summary: "cargo test".into(), target: None }),
        Step::Send(RunEvent::Tool {
            tool: "browser.open".into(),
            summary: "https://example.org/a/…".into(),
            target: Some("https://example.org/a/b/c".into()),
        }),
        text("All tests pass."),
        Step::Send(RunEvent::Usage { input_tokens: 10, output_tokens: 5 }),
        finished("All tests pass."),
    ]);
    let before = crate::runs::now_ms();
    let mut events = h.say("claude", "Run the tests");
    let Event::RunStarted { run, trigger, task, started, .. } = &events[0] else { panic!("RunStarted comes first: {events:?}") };
    assert_eq!((trigger, task.as_str()), (&Trigger::User, "Run the tests"));
    assert!((before..=crate::runs::now_ms()).contains(started), "the start in unix ms, not {started}");
    let run = run.clone();
    assert_eq!(events[1], Event::Ring { agent: "claude".into(), state: RingState::Working, progress: None });
    assert_eq!(events[2], Event::Activity { agent: "claude".into(), text: "Run the tests".into() });
    assert!(matches!(&events[3], Event::Message { role: Role::User, text, done: true, .. } if text == "Run the tests"));

    events.extend(h.until_finished("claude"));
    let replies: Vec<(String, bool)> = events
        .iter()
        .filter_map(|e| match e {
            Event::Message { role: Role::Agent, text, done, id, .. } => {
                assert_eq!(id, &format!("{run}-reply"), "one growing message");
                Some((text.clone(), *done))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        replies,
        [("Hello.".into(), false), ("Hello.\n\nAll tests pass.".into(), false), ("Hello.\n\nAll tests pass.".into(), true)]
    );
    assert!(events.contains(&Event::Action {
        agent: "claude".into(),
        tool: "terminal.run".into(),
        summary: "cargo test".into(),
        target: None
    }));
    assert!(
        events.contains(&Event::Action {
            agent: "claude".into(),
            tool: "browser.open".into(),
            summary: "https://example.org/a/…".into(),
            target: Some("https://example.org/a/b/c".into()),
        }),
        "the full target reaches the app"
    );
    assert!(events.contains(&Event::Activity { agent: "claude".into(), text: "running cargo test".into() }));
    assert!(!events.iter().any(|e| matches!(e, Event::GoTo { .. })), "tool use sends no GoTo (D9)");
    assert_eq!(finished_with(&events), (Outcome::NeedsYou, Some("Hello.".into())), "on_done = wait");
    assert_eq!(rings(&events).last(), Some(&RingState::NeedsYou));

    let request = &h.fake.requests()[0];
    assert_eq!(request.prompt, "Run the tests");
    assert_eq!(request.model.as_deref(), Some("haiku"));
    assert_eq!(request.dir, h.agent_dir("claude"));
    assert!(request.system.contains("You are Claude") && request.system.ends_with("Be brief."));
    assert!(request.permissions.write && !request.permissions.shell && !request.permissions.web);
    let mcp = request.mcp.as_ref().unwrap();
    assert_eq!(mcp.command, Path::new("/opt/meerora/meerora-mcp"));
    assert_eq!(mcp.args[..2], ["--agent", "claude"]);
    assert_eq!(request.session, None);

    let store = Store::load(&h.agent_dir("claude"));
    assert_eq!(store.session.as_deref(), Some("s1"));
    assert_eq!(store.tokens.used, 15);
    let last = store.last_run.unwrap();
    assert_eq!((last.outcome, last.tokens, last.task.as_str()), (Outcome::NeedsYou, 15, "Run the tests"));
    assert_eq!(store.ring, Some(RingState::NeedsYou));

    // The next run continues the session, and an ack rests the agent at home.
    h.say("claude", "Thanks");
    h.until_finished("claude");
    assert_eq!(h.fake.requests()[1].session.as_deref(), Some("s1"));
    let acked = h.command(Command::Ack { agent: "claude".into() });
    assert_eq!(rings(&acked), [RingState::Resting]);
    assert!(acked.contains(&Event::GoTo { agent: "claude".into(), station: "bookshelf".into() }));
    assert_eq!(Store::load(&h.agent_dir("claude")).ring, Some(RingState::Resting));
}

#[test]
fn rest_after_done_and_errors_fail() {
    let mut h = Harness::new();
    h.fake.script(vec![finished("Drafted the intro. It has two parts.")]);
    h.say("quill", "Draft the intro");
    let events = h.until_finished("quill");
    assert_eq!(finished_with(&events), (Outcome::Done, Some("Drafted the intro.".into())), "on_done = rest");
    assert_eq!(rings(&events), [RingState::Resting]);
    assert!(events.contains(&Event::GoTo { agent: "quill".into(), station: "desk".into() }));

    h.fake.script(vec![text("Starting."), Step::Send(RunEvent::Finished { result: Err("claude is not installed".into()) })]);
    h.say("quill", "Again");
    let events = h.until_finished("quill");
    assert_eq!(finished_with(&events).0, Outcome::Failed);
    assert!(events.iter().any(|e| matches!(e, Event::Message { role: Role::System, text, .. } if text == "claude is not installed")));
    assert!(events.iter().any(|e| matches!(e, Event::Message { role: Role::Agent, text, done: true, .. } if text == "Starting.")));
    assert_eq!(rings(&events), [RingState::Resting]);
}

#[test]
fn messages_during_a_run_are_queued_for_the_next_run() {
    let mut h = Harness::new();
    h.fake.script(vec![text("one"), Step::Sleep(300), finished("one")]);
    h.fake.script(vec![finished("two")]);
    h.say("quill", "first");
    let queued = [h.say("quill", "second"), h.say("quill", "third")].concat();
    assert!(!queued.iter().any(|e| matches!(e, Event::RunStarted { .. })), "no second run while the first goes");
    assert_eq!(queued.iter().filter(|e| matches!(e, Event::Message { role: Role::User, .. })).count(), 2, "echoed right away");

    let events = h.until(|e| matches!(e, Event::RunFinished { .. }));
    let next = events.iter().find_map(|e| match e {
        Event::RunStarted { task, trigger, .. } => Some((task.clone(), *trigger)),
        _ => None,
    });
    assert_eq!(next, Some(("second\n\nthird".to_string(), Trigger::User)));
    h.until_finished("quill");
    let prompts: Vec<String> = h.fake.requests().into_iter().map(|r| r.prompt).collect();
    assert_eq!(prompts, ["first", "second\n\nthird"]);
}

#[test]
fn cancel_stops_the_run_and_drops_the_queue() {
    let mut h = Harness::new();
    h.fake.script(vec![text("working"), Step::UntilCancelled]);
    h.say("claude", "Long task");
    h.say("claude", "and then this");
    h.until(|e| matches!(e, Event::Message { role: Role::Agent, .. }));
    h.command(Command::Cancel { agent: "claude".into() });
    let events = h.until_finished("claude");
    assert_eq!(finished_with(&events), (Outcome::Cancelled, Some("cancelled".into())));
    assert!(events.iter().any(|e| matches!(e, Event::Message { role: Role::System, text, .. } if text.contains("was dropped"))));
    assert_eq!(rings(&events), [RingState::Resting]);
    assert!(events.contains(&Event::GoTo { agent: "claude".into(), station: "bookshelf".into() }));
    std::thread::sleep(Duration::from_millis(150));
    assert!(h.rx.try_recv().is_err(), "the queued message did not start a run");
    assert_eq!(h.fake.requests().len(), 1);
}

#[test]
fn hello_during_a_run_replays_its_real_start() {
    let mut h = Harness::new();
    h.fake.script(vec![text("working"), Step::UntilCancelled]);
    let events = h.say("claude", "Long task");
    let Some(Event::RunStarted { run, started, .. }) = events.first().cloned() else { panic!("RunStarted comes first: {events:?}") };
    h.until(|e| matches!(e, Event::Message { role: Role::Agent, .. }));
    let replayed = h.daemon.greeting().into_iter().find_map(|e| match e {
        Event::RunStarted { agent, run, started, .. } if agent == "claude" => Some((run, started)),
        _ => None,
    });
    assert_eq!(replayed, Some((run, started)), "a client that connects later sees when the run began");
    assert!(started > 0);
    h.command(Command::Cancel { agent: "claude".into() });
    h.until_finished("claude");
}

#[test]
fn ask_user_ends_the_run_waiting() {
    let mut h = Harness::new();
    h.fake.script(vec![Step::Sleep(200), finished("I need your choice.")]);
    h.say("quill", "Pick a title");
    let (ok, _, events) = h.tool("quill", ToolCall::AskUser { question: "Which title do you like?".into() });
    assert!(ok, "answered right away, before the run ends");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Message { role: Role::Agent, text, done: true, .. } if text == "Which title do you like?"))
    );
    let events = h.until_finished("quill");
    assert_eq!(finished_with(&events), (Outcome::NeedsYou, Some("Which title do you like?".into())), "even with on_done = rest");
    assert_eq!(rings(&events), [RingState::NeedsYou]);
    assert!(!events.iter().any(|e| matches!(e, Event::GoTo { .. })));

    // Without a run, ask_user sets the ring at once.
    let (ok, _, events) = h.tool("claude", ToolCall::AskUser { question: "Ready?".into() });
    assert!(ok);
    assert_eq!(rings(&events), [RingState::NeedsYou]);
}

#[test]
fn tools_are_validated() {
    let mut h = Harness::new();
    let (ok, _, events) = h.tool("claude", ToolCall::FigureSet { name: "letter".into(), value: json!(true) });
    assert!(ok);
    assert!(events.contains(&Event::Param { agent: "claude".into(), name: "letter".into(), value: json!(true) }));
    assert_eq!(Store::load(&h.agent_dir("claude")).params["letter"], json!(true));

    let (ok, message, events) = h.tool("claude", ToolCall::FigureSet { name: "letter".into(), value: json!("yes") });
    assert!(!ok && message.contains("true or false"), "{message}");
    assert!(!events.iter().any(|e| matches!(e, Event::Param { .. })));
    let (ok, message, _) = h.tool("claude", ToolCall::FigureSet { name: "hat".into(), value: json!(true) });
    assert!(!ok && message.contains("no param \"hat\""), "{message}");

    let (ok, _, events) = h.tool("claude", ToolCall::GoTo { station: "mailbox".into() });
    assert!(ok);
    assert!(events.contains(&Event::GoTo { agent: "claude".into(), station: "mailbox".into() }));
    let (ok, message, _) = h.tool("claude", ToolCall::GoTo { station: "kitchen".into() });
    assert!(!ok && message.contains("door"), "{message}");

    let (ok, _, events) = h.tool("claude", ToolCall::SetActivity { text: "reading mail".into() });
    assert!(ok && events.contains(&Event::Activity { agent: "claude".into(), text: "reading mail".into() }));
    let (ok, _, events) = h.tool("claude", ToolCall::Note { text: "half done".into() });
    assert!(ok && events.iter().any(|e| matches!(e, Event::Message { role: Role::Agent, text, .. } if text == "half done")));
    let (ok, _, _) = h.tool("nobody", ToolCall::Note { text: "x".into() });
    assert!(!ok);
}

#[test]
fn hello_replays_the_conversation_and_a_restart_keeps_it() {
    let mut h = Harness::new();
    h.fake.script(vec![finished("Done. Here it is.")]);
    h.say("claude", "Do it");
    h.until_finished("claude");
    h.tool("claude", ToolCall::FigureSet { name: "letter".into(), value: json!(true) });

    let messages = |events: &[Event]| -> Vec<(Role, String, bool)> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Message { agent, role, text, done, .. } if agent == "claude" => Some((*role, text.clone(), *done)),
                _ => None,
            })
            .collect()
    };
    let expected = vec![(Role::User, "Do it".to_string(), true), (Role::Agent, "Done. Here it is.".to_string(), true)];
    let greeting = h.daemon.greeting();
    assert!(matches!(greeting[0], Event::Hello { version: meerora_protocol::PROTOCOL_VERSION, .. }));
    assert_eq!(messages(&greeting), expected);

    let Harness { dir, fake, .. } = h;
    let h = Harness::open(dir, fake);
    let greeting = h.daemon.greeting();
    assert_eq!(messages(&greeting), expected, "replayed from logs/messages.jsonl");
    assert!(
        greeting.contains(&Event::Param { agent: "claude".into(), name: "letter".into(), value: json!(true) }),
        "params from state.json"
    );
    assert!(greeting.contains(&Event::Ring { agent: "claude".into(), state: RingState::NeedsYou, progress: None }), "still waiting");
}

#[test]
fn mock_agents_answer_with_a_note() {
    let mut h = Harness::new();
    let events = h.say("ember", "hi");
    assert!(events.iter().any(|e| matches!(e, Event::Message { role: Role::System, text, .. } if text.contains("mock agent"))));
    assert!(h.fake.requests().is_empty());
    assert!(!h.agent_dir("ember").join("logs").exists(), "nothing is written for mock agents");
}

#[test]
fn reload_finds_new_changed_and_removed_agents() {
    let mut h = Harness::new();
    write_agent(&h.dir.0, "moth", "name = \"Moth\"\nhome = \"garden_bench\"\n");
    std::fs::write(h.agent_dir("quill").join("agent.toml"), QUILL.replace("Quill", "Quill II")).unwrap();
    std::fs::write(h.agent_dir("ember").join("agent.toml"), "broken = ").unwrap();
    std::fs::remove_file(h.agent_dir("claude").join("agent.toml")).unwrap();
    let events = h.command(Command::ReloadAgents);
    assert!(events.contains(&Event::AgentSpawned { agent: "moth".into(), name: "Moth".into(), system: false }));
    assert!(events.contains(&Event::GoTo { agent: "moth".into(), station: "garden_bench".into() }));
    assert!(events.contains(&Event::AgentSpawned { agent: "quill".into(), name: "Quill II".into(), system: false }));
    assert!(events.contains(&Event::AgentRemoved { agent: "claude".into() }));
    assert!(!events.contains(&Event::AgentRemoved { agent: "ember".into() }), "a broken agent.toml keeps its agent");
}

#[test]
fn schedules_wake_real_agents_within_their_budget() {
    let dir = TempDir::new("daemon-schedule");
    let toml = "name = \"Wren\"\nhome = \"window\"\n[runtime]\nkind = \"claude-cli\"\n[budget]\ntokens_per_day = 100\n\
                [[schedule]]\nevery = \"5m\"\ntask = \"Check the inbox.\"\nstation = \"mailbox\"\n";
    write_agent(&dir.0, "wren", toml);
    // The last wake was long ago, so the first one happens right away.
    let store = Store { schedule: [("every 5m".to_string(), 0)].into(), ..Store::default() };
    store.save(&dir.0.join("agents/wren")).unwrap();
    let mut h = Harness::open(dir, Fake::default());
    h.fake.script(vec![Step::Send(RunEvent::Usage { input_tokens: 90, output_tokens: 20 }), finished("Nothing new.")]);

    let greeting = h.daemon.greeting();
    assert!(
        greeting.iter().any(|e| matches!(e, Event::NextWake { agent, at: Some(_), .. } if agent == "wren")),
        "the countdown is part of the greeting"
    );
    h.daemon.tick_schedules(&jiff::Zoned::now());
    let events = h.drain();
    assert!(events.iter().any(|e| matches!(e, Event::RunStarted { trigger: Trigger::Schedule, task, .. } if task == "Check the inbox.")));
    let wake = events.iter().find_map(|e| match e {
        Event::NextWake { at: Some(at), since, .. } => Some((*at, *since)),
        _ => None,
    });
    let (at, since) = wake.expect("a wake restarts the countdown");
    assert_eq!(at - since, 300_000, "five minutes from this wake to the next");
    assert!(events.contains(&Event::GoTo { agent: "wren".into(), station: "mailbox".into() }));
    assert!(!events.iter().any(|e| matches!(e, Event::Message { role: Role::User, .. })), "no user message for a schedule");
    let events = h.until_finished("wren");
    assert_eq!(finished_with(&events).0, Outcome::Done, "scheduled runs rest");
    let recorded = Store::load(&h.agent_dir("wren")).schedule["every 5m"];
    assert!(recorded > 0);

    // 110 tokens used of 100, so the next wake is refused with one note.
    let later = jiff::Timestamp::from_millisecond(recorded + 301_000).unwrap().to_zoned(jiff::tz::TimeZone::system());
    h.daemon.tick_schedules(&later);
    let events = h.drain();
    assert!(!events.iter().any(|e| matches!(e, Event::RunStarted { .. })));
    assert!(events.iter().any(|e| matches!(e, Event::Message { role: Role::System, text, .. } if text.contains("budget"))));
    let even_later = jiff::Timestamp::from_millisecond(recorded + 602_000).unwrap().to_zoned(jiff::tz::TimeZone::system());
    h.daemon.tick_schedules(&even_later);
    assert!(h.drain().iter().all(|e| !matches!(e, Event::Message { .. })), "the note comes once a day");
    // A user run still goes.
    h.say("wren", "Check now");
    h.until_finished("wren");
    assert_eq!(h.fake.requests().len(), 2);
}

/// A builder that answers with a broken script first and a valid one second.
#[test]
fn builder_retries_until_the_script_passes() {
    let fake = Fake::default();
    fake.script(vec![Step::Send(RunEvent::Session { id: "b1".into() }), finished("```js\nexport default figure({});\n```\nA start.")]);
    fake.script(vec![finished(&format!("Here you go.\n```js\n{WREN}```\nI added a letter."))]);
    let job = BuildJob { session: "s-1".into(), prompt: "a small mail bird".into(), script: None };
    let mut events = Vec::new();
    builder::run(&job, &builder::default_runtime(), &Settings::default(), &fake.factory(), &mut |e| events.push(e));

    let statuses: Vec<DraftStatus> =
        events.iter().filter_map(|e| if let Event::FigureDraft { status, .. } = e { Some(*status) } else { None }).collect();
    assert_eq!(statuses, [DraftStatus::Working, DraftStatus::Working, DraftStatus::Ready]);
    let Event::FigureDraft { message, .. } = &events[1] else { unreachable!() };
    assert!(message.contains("validator"), "{message}");
    let Event::FigureDraft { session, script, triangles, message, .. } = &events[2] else { unreachable!() };
    assert_eq!(session, "s-1");
    assert!(script.as_deref().unwrap().contains("export default figure("));
    assert!(triangles.unwrap() > 0);
    assert_eq!(message, "Here you go. I added a letter.");

    let requests = fake.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].prompt.contains("a small mail bird"));
    assert!(requests[0].system.contains("# Modelling API"));
    assert!(requests[0].mcp.is_none() && !requests[0].permissions.write);
    assert_eq!(requests[1].session.as_deref(), Some("b1"), "the retry continues the conversation");
    assert!(requests[1].prompt.contains("validator"));
}

#[test]
fn builder_gives_up_after_three_tries() {
    let fake = Fake::default();
    for _ in 0..3 {
        fake.script(vec![finished("no code here")]);
    }
    let job = BuildJob { session: "s-2".into(), prompt: "x".into(), script: Some("export default figure({});".into()) };
    let mut events = Vec::new();
    builder::run(
        &job,
        &RuntimeConfig { kind: RuntimeKind::Ollama, model: None, url: None },
        &Settings::default(),
        &fake.factory(),
        &mut |e| events.push(e),
    );
    let last = events.last().unwrap();
    assert!(
        matches!(last, Event::FigureDraft { status: DraftStatus::Failed, message, .. } if message.contains("no ```js block")),
        "{last:?}"
    );
    assert_eq!(fake.requests().len(), 3);
    assert!(fake.requests()[0].prompt.contains("current figure script"));
    assert!(fake.requests()[2].prompt.contains("x"), "without a session the request is repeated");
}

mod app_tools;
mod apps;
mod ask;
mod capabilities;
mod devices;
mod graveyard;
mod helper;
mod social;
