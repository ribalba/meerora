//! What the daemon does for the apps of the agent view (D11): the run history
//! in `logs/runs.jsonl` for the Activity app and its Run now button (D23).

use meerora_protocol::{Command, Event, Outcome, RUNS_KEPT, RUNS_LOG, Role, RunRecord, Trigger};
use meerora_runtime::RunEvent;

use super::{Fake, Harness, Step, finished, finished_with, write_agent};
use crate::store::{Store, tests::TempDir};

fn runs(h: &Harness, agent: &str) -> Vec<RunRecord> {
    let text = std::fs::read_to_string(h.agent_dir(agent).join(RUNS_LOG)).unwrap_or_default();
    text.lines().map(|l| serde_json::from_str(l).expect("every line is a run")).collect()
}

fn record(i: usize) -> RunRecord {
    RunRecord {
        id: format!("old-{i}"),
        trigger: Trigger::Schedule,
        task: format!("task {i}"),
        start: i as u64,
        end: i as u64 + 1,
        outcome: Outcome::Done,
        summary: None,
        tokens: 0,
    }
}

#[test]
fn finished_runs_go_to_the_run_history() {
    let mut h = Harness::new();
    h.fake.script(vec![Step::Send(RunEvent::Usage { input_tokens: 30, output_tokens: 12 }), finished("Drafted the intro. Two parts.")]);
    h.say("quill", "Draft the intro");
    h.until_finished("quill");
    h.fake.script(vec![Step::Send(RunEvent::Finished { result: Err("claude is not installed".into()) })]);
    h.say("quill", "Again");
    h.until_finished("quill");

    let history = runs(&h, "quill");
    assert_eq!(history.len(), 2);
    let first = &history[0];
    assert!(first.id.starts_with("quill-"), "{}", first.id);
    assert_eq!((first.trigger, first.task.as_str(), first.outcome), (Trigger::User, "Draft the intro", Outcome::Done));
    assert_eq!((first.summary.as_deref(), first.tokens), (Some("Drafted the intro."), 42));
    assert!(first.start <= first.end && first.start > 0);
    assert_eq!((history[1].outcome, history[1].summary.as_deref()), (Outcome::Failed, Some("claude is not installed")));
    assert!(runs(&h, "ember").is_empty(), "mock agents have no runs");
}

#[test]
fn the_run_history_keeps_the_last_runs() {
    let tmp = TempDir::new("runs-trim");
    for i in 0..RUNS_KEPT {
        crate::store::append_run(&tmp.0, &record(i)).unwrap();
    }
    let text = std::fs::read_to_string(tmp.0.join(RUNS_LOG)).unwrap();
    assert_eq!(text.lines().count(), RUNS_KEPT, "plain appends up to the limit");

    // A daemon run on a full history drops the oldest run.
    let mut h = Harness::new();
    let path = h.agent_dir("quill").join(RUNS_LOG);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, text).unwrap();
    h.say("quill", "One more");
    h.until_finished("quill");
    let history = runs(&h, "quill");
    assert_eq!(history.len(), RUNS_KEPT);
    assert_eq!(history[0].id, "old-1", "the oldest run is gone");
    assert_eq!(history[RUNS_KEPT - 2].id, format!("old-{}", RUNS_KEPT - 1));
    assert_eq!(history[RUNS_KEPT - 1].task, "One more");
    assert!(!path.with_extension("jsonl.tmp").exists());
}

const WREN: &str = "name = \"Wren\"\nhome = \"window\"\n[runtime]\nkind = \"claude-cli\"\n[budget]\ntokens_per_day = 100\n\
                    [[schedule]]\nevery = \"5m\"\ntask = \"Check the inbox.\"\nstation = \"mailbox\"\nquiet_hours = \"22:00-07:00\"\n\
                    [[schedule]]\nat = \"07:00\"\ntask = \"Write the digest.\"\n";
const MOTH: &str = "name = \"Moth\"\nhome = \"garden_bench\"\n[runtime]\nkind = \"claude-cli\"\n\
                    [[schedule]]\nevery = \"1h\"\ntask = \"Water the plants.\"\n";

fn system_notes(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Message { role: Role::System, text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn run_now_starts_a_wake_up_within_the_budget() {
    let dir = TempDir::new("run-now");
    write_agent(&dir.0, "wren", WREN);
    write_agent(&dir.0, "moth", MOTH);
    write_agent(
        &dir.0,
        "ember",
        "name = \"Ember\"\nhome = \"workbench\"\n[runtime]\nkind = \"mock\"\n[[schedule]]\nevery = \"1m\"\ntask = \"x\"\n",
    );
    let mut h = Harness::open(dir, Fake::default());
    h.fake.script(vec![Step::Send(RunEvent::Usage { input_tokens: 90, output_tokens: 20 }), finished("Nothing new.")]);

    // 23:00 lies in Wren's quiet hours, which hold back wakes but not Run now.
    // The last 23:00 that has passed, so the test does not depend on the time of day.
    let now = jiff::Zoned::now();
    let mut late = now.with().time(jiff::civil::time(23, 0, 0, 0)).build().unwrap();
    if late > now {
        late = late.yesterday().unwrap();
    }
    h.daemon.run_now("wren", 0, &late);
    let events = h.drain();
    let started = events.iter().find_map(|e| match e {
        Event::RunStarted { trigger, task, .. } => Some((*trigger, task.clone())),
        _ => None,
    });
    assert_eq!(started, Some((Trigger::Schedule, "Check the inbox.".to_string())));
    assert!(events.contains(&Event::GoTo { agent: "wren".into(), station: "mailbox".into() }));
    let wake = events.iter().find_map(|e| match e {
        Event::NextWake { at: Some(at), since, .. } => Some((*at, *since)),
        _ => None,
    });
    let (_, since) = wake.expect("Run now restarts the countdown");
    assert_eq!(since, late.timestamp().as_millisecond() as u64);
    assert!(!events.iter().any(|e| matches!(e, Event::Message { role: Role::User, .. })), "no user message, as for a schedule");
    let events = h.until_finished("wren");
    assert_eq!(finished_with(&events).0, Outcome::Done, "it rests like a scheduled run");
    assert_eq!(Store::load(&h.agent_dir("wren")).schedule["every 5m"], late.timestamp().as_millisecond(), "the wake is recorded");

    // 110 of 100 tokens are used today, so the next Run now is refused with a note.
    let events = h.command(Command::RunNow { agent: "wren".into(), schedule: 1 });
    assert!(!events.iter().any(|e| matches!(e, Event::RunStarted { .. })));
    let notes = system_notes(&events);
    assert!(notes.len() == 1 && notes[0].contains("budget of 100 tokens"), "{notes:?}");

    // A busy agent starts nothing.
    h.fake.script(vec![Step::UntilCancelled]);
    h.say("moth", "A long job");
    let events = h.command(Command::RunNow { agent: "moth".into(), schedule: 0 });
    assert!(!events.iter().any(|e| matches!(e, Event::RunStarted { .. })));
    assert!(system_notes(&events).iter().any(|n| n.contains("busy")), "{events:?}");
    h.command(Command::Cancel { agent: "moth".into() });
    h.until_finished("moth");

    // Unknown wake-ups, unknown agents and mock agents are ignored.
    for (agent, schedule) in [("moth", 4), ("nobody", 0), ("ember", 0)] {
        let events = h.command(Command::RunNow { agent: agent.into(), schedule });
        assert!(events.is_empty(), "{agent} {schedule}: {events:?}");
    }
    assert_eq!(h.fake.requests().len(), 2, "only Wren's wake-up and Moth's job ran");
}
