//! One-shot questions of the command box (`?? question`, ask.rs) with the
//! fake runtime.

use meerora_protocol::{Command, Event};
use meerora_runtime::RunEvent;

use super::{Harness, Step, finished, text};

fn ask(h: &mut Harness, request: &str, agent: &str, question: &str) -> Vec<Event> {
    h.command(Command::Ask { request: request.into(), agent: agent.into(), question: question.into() })
}

fn answers(events: &[Event]) -> Vec<(String, bool, Option<String>)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Answer { text, done, error, .. } => Some((text.clone(), *done, error.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn a_quick_question_streams_its_answer_without_a_run() {
    let mut h = Harness::new();
    h.fake.script(vec![text("Paris is the capital"), Step::Sleep(20), text("of France."), finished("of France.")]);
    assert!(ask(&mut h, "q1", "claude", "  capital of France?  ").is_empty(), "the answer comes from the thread");
    let events = h.until(|e| matches!(e, Event::Answer { done: true, .. }));
    assert_eq!(
        answers(&events),
        [
            ("Paris is the capital".to_string(), false, None),
            ("Paris is the capital\n\nof France.".to_string(), false, None),
            ("Paris is the capital\n\nof France.".to_string(), true, None),
        ]
    );
    assert!(events.iter().all(|e| matches!(e, Event::Answer { request, .. } if request == "q1")), "only answers: {events:?}");

    let request = h.fake.requests().pop().unwrap();
    assert_eq!(request.prompt, "capital of France?");
    assert_eq!(request.agent, "claude");
    assert_eq!(request.model.as_deref(), Some("haiku"), "the agent's own model");
    assert!(request.session.is_none(), "no conversation");
    assert!(request.mcp.is_none(), "no meerora tools");
    assert!(!request.permissions.write && !request.permissions.shell && !request.permissions.web, "read-only");
    assert_ne!(request.dir, h.agent_dir("claude"), "a private folder, not the agent directory");
    assert!(!request.dir.exists(), "the folder is removed afterwards");
    let isolation = request.isolation.expect("the CLI home and sandbox of the agent's runs");
    assert_eq!(isolation.runtimes, h.dir.0.join("runtimes"));

    // Nothing was stored and nothing about the agent changed.
    assert!(!h.agent_dir("claude").join("logs/messages.jsonl").exists());
    let greeting = h.daemon.greeting();
    assert!(!greeting.iter().any(|e| matches!(e, Event::Message { .. } | Event::RunStarted { .. })), "{greeting:?}");
}

#[test]
fn the_final_text_counts_when_no_block_came() {
    let mut h = Harness::new();
    h.fake.script(vec![finished("42")]);
    ask(&mut h, "q2", "claude", "answer?");
    let events = h.until(|e| matches!(e, Event::Answer { done: true, .. }));
    assert_eq!(answers(&events), [("42".to_string(), true, None)]);
}

#[test]
fn failures_and_refusals_end_with_an_error() {
    let mut h = Harness::new();
    let refused = |events: Vec<Event>| match answers(&events).as_slice() {
        [(text, true, Some(error))] if text.is_empty() => error.clone(),
        other => panic!("not one refusal: {other:?}"),
    };
    assert!(refused(ask(&mut h, "a", "ember", "hi")).contains("mock agent"));
    assert!(refused(ask(&mut h, "b", "nobody", "hi")).contains("no agent called nobody"));
    assert!(refused(ask(&mut h, "c", "claude", "   ")).contains("Type a question"));
    assert!(h.fake.requests().is_empty(), "no runtime started");

    h.fake.script(vec![text("half"), Step::Send(RunEvent::Finished { result: Err("claude is not installed".into()) })]);
    ask(&mut h, "d", "claude", "hi");
    let events = h.until(|e| matches!(e, Event::Answer { done: true, .. }));
    assert_eq!(answers(&events).last().unwrap(), &("half".to_string(), true, Some("claude is not installed".to_string())));
}

#[test]
fn a_question_can_be_stopped() {
    let mut h = Harness::new();
    h.fake.script(vec![text("thinking"), Step::UntilCancelled]);
    ask(&mut h, "q3", "claude", "a long one");
    let first = h.until(|e| matches!(e, Event::Answer { done: false, .. }));
    assert_eq!(answers(&first), [("thinking".to_string(), false, None)]);
    assert!(h.command(Command::CancelAsk { request: "q3".into() }).is_empty());
    let events = h.until(|e| matches!(e, Event::Answer { done: true, .. }));
    assert_eq!(answers(&events).last().unwrap(), &("thinking".to_string(), true, Some("Stopped.".to_string())));
    // The request id is free again and a new question runs.
    h.fake.script(vec![finished("again")]);
    ask(&mut h, "q3", "claude", "once more");
    let events = h.until(|e| matches!(e, Event::Answer { done: true, .. }));
    assert_eq!(answers(&events), [("again".to_string(), true, None)]);
}

#[test]
fn the_messages_round_trip_as_json() {
    let ask = Command::Ask { request: "r".into(), agent: "claude".into(), question: "why?".into() };
    let line = meerora_protocol::encode(&ask);
    assert!(line.contains(r#""type":"ask""#), "{line}");
    assert_eq!(meerora_protocol::decode_command(&line).unwrap(), ask);
    let cancel = Command::CancelAsk { request: "r".into() };
    assert_eq!(meerora_protocol::decode_command(&meerora_protocol::encode(&cancel)).unwrap(), cancel);
    let answer = Event::Answer { request: "r".into(), text: "because".into(), done: true, error: None };
    let line = meerora_protocol::encode(&answer);
    assert!(!line.contains("error"), "no error, no field: {line}");
    assert_eq!(meerora_protocol::decode_event(&line).unwrap(), answer);
    assert_eq!(answer.agent(), None);
}
