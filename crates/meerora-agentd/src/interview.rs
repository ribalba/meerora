//! The profile interview of the personal agent (D26). Like the builder of
//! screen 3, a runtime answers one request at a time: it reads the
//! interview so far and the profile so far, and answers with one JSON
//! object that holds the next question and the whole profile in the format
//! of section 5. The answer is validated, and the problems go back to the
//! runtime for at most [`ATTEMPTS`] tries in total.
//!
//! The interview is stateless here. The app keeps the questions and answers
//! and sends them with every request.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use meerora_protocol::{DraftStatus, Event, InterviewTurn, PROFILE_FORMAT, Profile, RuntimeConfig, Settings};
use meerora_runtime::{Permissions, RunEvent, RunRequest, Runtime};
use serde::Deserialize;

use crate::daemon::RuntimeFactory;
use crate::runs::panic_text;

/// Tries per question, the first one included.
pub const ATTEMPTS: usize = 3;
/// After this many answers the interviewer has to finish.
pub const MAX_TURNS: usize = 12;
const ANSWER_TIMEOUT: Duration = Duration::from_secs(180);

/// One `Command::Interview`.
#[derive(Clone, Debug)]
pub struct InterviewJob {
    pub session: String,
    pub turns: Vec<InterviewTurn>,
    pub profile: Option<Profile>,
    /// The account handle, written into the profile. Empty without an account.
    pub handle: String,
}

/// What the interviewer answered, after validation.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub question: Option<String>,
    pub done: bool,
    pub profile: Profile,
}

#[derive(Deserialize)]
struct Answer {
    #[serde(default)]
    question: Option<String>,
    #[serde(default)]
    done: bool,
    profile: Profile,
}

const EXAMPLE: &str = r#"{
  "meerora_profile": "0.1",
  "handle": "example",
  "name": { "value": "Alex Example", "visibility": "public" },
  "headline": { "value": "Writes software, sails on weekends", "visibility": "public" },
  "education": [
    { "school": "Example Gymnasium", "from": 1998, "to": 2007, "visibility": "friends" },
    { "school": "Example University", "degree": "MSc Computer Science", "from": 2007, "to": 2013, "visibility": "public" }
  ],
  "work": [{ "org": "Example GmbH", "role": "Founder", "from": 2020, "visibility": "public" }],
  "places": [{ "value": "Example City", "visibility": "friends" }],
  "interests": [{ "value": "Sailing", "visibility": "public" }],
  "links": [{ "url": "https://example.org", "visibility": "public" }]
}"#;

pub fn system_prompt() -> String {
    format!(
        "You interview the user of meerora for the profile of their personal agent. The personal agent represents them: others \
find it by name and see the profile. You cannot run tools or read files.

Ask one short, friendly question at a time and build the profile from the answers. Cover these topics in this order, with one or \
two questions each, and move on when the user does not want to share something: their name and a one-line headline about \
themselves, photos (they add photos in the next step, so only ask whether they want some and which kind), schools, university, \
work, places where they live or lived, interests, and links to their own pages. Never invent anything. Only put into the profile \
what the user said.

Give every field a visibility: public for anyone, friends for people they befriend, private for nobody but them. Propose public \
for the name, the headline, current work and interests, friends for schools, places and personal details, and private for \
anything sensitive such as health, religion, family or an exact address. The user can change every visibility in the next step.

When you have covered every topic, or after {MAX_TURNS} questions, set done to true and leave the question empty.

Answer with exactly one JSON object and nothing else:

{{\"question\": \"<your next question>\", \"done\": false, \"profile\": <the whole profile so far>}}

The profile uses this format. Leave out fields you know nothing about. education entries have school, an optional degree and \
the years from and to. work entries have org, an optional role and the years. places and interests have a value. links have a \
url that starts with https://. Lists have at most 12 entries. Leave photos out.

{EXAMPLE}"
    )
}

/// The request for the next question.
pub fn prompt(job: &InterviewJob) -> String {
    let mut profile = job.profile.clone().unwrap_or_default();
    profile.meerora_profile = PROFILE_FORMAT.into();
    profile.handle = job.handle.clone();
    // Photos are the app's business, the interviewer only asks about them.
    profile.photos.clear();
    let profile = serde_json::to_string_pretty(&profile).unwrap_or_else(|_| "{}".into());
    if job.turns.is_empty() {
        return format!("Start the interview with your first question. The profile so far:\n\n{profile}");
    }
    let mut text = String::from("The interview so far:\n\n");
    for (i, t) in job.turns.iter().enumerate() {
        text.push_str(&format!("Question {}: {}\nAnswer: {}\n\n", i + 1, t.question.trim(), t.answer.trim()));
    }
    text.push_str(&format!("The profile so far:\n\n{profile}\n\n"));
    if job.turns.len() >= MAX_TURNS {
        text.push_str("That was the last answer. Put it into the profile, set done to true and leave the question empty.");
    } else {
        text.push_str("Put the last answer into the profile and ask the next question, or finish when every topic is covered.");
    }
    text
}

/// The JSON object of an answer: a ```json block, or the text from the
/// first `{` to the last `}`.
pub fn extract_json(text: &str) -> Option<&str> {
    if let Some(start) = text.find("```json") {
        let body = &text[start + 7..];
        if let Some(end) = body.find("```") {
            return Some(body[..end].trim());
        }
    }
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (end > start).then(|| &text[start..=end])
}

/// Parses and validates one answer of the interviewer.
pub fn parse(text: &str, handle: &str) -> Result<Step, String> {
    let json = extract_json(text).ok_or("The answer had no JSON object.")?;
    let answer: Answer = serde_json::from_str(json).map_err(|e| format!("The JSON object does not fit the format ({e})."))?;
    let mut profile = answer.profile;
    profile.handle = handle.to_string();
    let problems = profile.problems();
    if !problems.is_empty() {
        return Err(format!("The profile has problems: {}.", problems.join(". ")));
    }
    let question = answer.question.map(|q| q.trim().to_string()).filter(|q| !q.is_empty());
    if question.is_none() && !answer.done {
        return Err("The question is empty and done is false. Ask the next question, or set done to true.".into());
    }
    if question.as_ref().is_some_and(|q| q.chars().count() > 400) {
        return Err("The question is longer than 400 characters. Ask one short question.".into());
    }
    Ok(Step { question: if answer.done { None } else { question }, done: answer.done, profile })
}

/// Runs one interview request and reports `InterviewStep` events through `emit`.
pub fn run(job: &InterviewJob, config: &RuntimeConfig, settings: &Settings, factory: &RuntimeFactory, emit: &mut dyn FnMut(Event)) {
    let event = |status, message: String, question: Option<String>, profile: Option<Profile>, done: bool| Event::InterviewStep {
        session: job.session.clone(),
        status,
        question,
        profile: profile.map(Box::new),
        done,
        message,
    };
    emit(event(DraftStatus::Working, "Thinking about the next question".into(), None, None, false));
    let dir = match WorkDir::new(&job.session) {
        Ok(dir) => dir,
        Err(e) => {
            emit(event(DraftStatus::Failed, format!("meerora cannot create a folder for the interview ({e})."), None, None, false));
            return;
        }
    };
    let system = system_prompt();
    let first = prompt(job);
    let mut request = first.clone();
    let mut session = None;
    let mut error = String::new();
    for attempt in 1..=ATTEMPTS {
        if attempt > 1 {
            emit(event(
                DraftStatus::Working,
                format!("Try {} did not pass, so the interviewer tries again. {error}", attempt - 1),
                None,
                None,
                false,
            ));
        }
        let answer = match ask(config, settings, factory, &dir.0, &system, &request, session.clone()) {
            Ok(answer) => answer,
            Err(e) => {
                emit(event(DraftStatus::Failed, e, None, None, false));
                return;
            }
        };
        session = answer.session.or(session);
        match parse(&answer.text, &job.handle) {
            Ok(step) => {
                let message =
                    step.question.clone().unwrap_or_else(|| "That is everything I wanted to ask. Review your profile now.".into());
                emit(event(DraftStatus::Ready, message, step.question, Some(step.profile), step.done));
                return;
            }
            Err(e) => error = e,
        }
        request = match &session {
            Some(_) => format!("{error} Send the complete JSON object again, with the question and the whole profile."),
            None => format!("{first}\n\nYour last answer did not pass. {error} Send exactly one JSON object."),
        };
    }
    emit(event(DraftStatus::Failed, error, None, None, false));
}

struct AskAnswer {
    text: String,
    session: Option<String>,
}

fn ask(
    config: &RuntimeConfig,
    settings: &Settings,
    factory: &RuntimeFactory,
    dir: &Path,
    system: &str,
    prompt: &str,
    session: Option<String>,
) -> Result<AskAnswer, String> {
    let request = RunRequest {
        agent: "interviewer".into(),
        dir: dir.to_path_buf(),
        prompt: prompt.into(),
        system: system.into(),
        session,
        model: config.model.clone(),
        permissions: Permissions { write: false, shell: false, web: false },
        mcp: None,
        isolation: None,
    };
    let started = catch_unwind(AssertUnwindSafe(|| -> anyhow::Result<_> {
        let runtime: Box<dyn Runtime> = factory(config, settings)?;
        let handle = runtime.start(request)?;
        Ok((runtime, handle))
    }));
    let (_runtime, handle) = match started {
        Ok(Ok(started)) => started,
        Ok(Err(e)) => return Err(format!("The interviewer cannot start ({e:#}).")),
        Err(panic) => return Err(format!("The interviewer's runtime crashed ({}).", panic_text(&*panic))),
    };
    let deadline = Instant::now() + ANSWER_TIMEOUT;
    let (mut blocks, mut session) = (Vec::new(), None);
    loop {
        match handle.events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(RunEvent::Session { id }) => session = Some(id),
            Ok(RunEvent::Text { text }) => blocks.push(text),
            Ok(RunEvent::Finished { result: Ok(text) }) => {
                // The JSON may come in an earlier text block than the final answer.
                let text = if extract_json(&text).is_some() { text } else { blocks.join("\n\n") };
                return Ok(AskAnswer { text, session });
            }
            Ok(RunEvent::Finished { result: Err(e) }) => return Err(format!("The interviewer failed ({e}).")),
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {
                handle.cancel();
                return Err("The interviewer took longer than 3 minutes.".into());
            }
            Err(RecvTimeoutError::Disconnected) => return Err("The interviewer stopped without an answer.".into()),
        }
    }
}

/// A private folder for one request, removed again afterwards.
struct WorkDir(PathBuf);

impl WorkDir {
    fn new(session: &str) -> std::io::Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let tag: String = session.chars().filter(|c| c.is_ascii_alphanumeric()).take(24).collect();
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("meerora-interview-{}-{tag}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        Ok(Self(dir))
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_parsed_and_checked() {
        let ok = r#"Sure. {"question": "Where did you go to school?", "done": false, "profile": {"name": {"value": "Kim", "visibility": "public"}}}"#;
        let step = parse(ok, "kim").unwrap();
        assert_eq!(step.question.as_deref(), Some("Where did you go to school?"));
        assert_eq!(step.profile.handle, "kim", "the handle comes from the account");
        assert_eq!(step.profile.name.unwrap().value, "Kim");

        let fenced = "```json\n{\"done\": true, \"profile\": {}}\n```";
        assert!(parse(fenced, "").unwrap().done);
        assert!(parse("no json at all", "").unwrap_err().contains("no JSON object"));
        let no_visibility = r#"{"question": "Next?", "profile": {"name": {"value": "Kim"}}}"#;
        assert!(parse(no_visibility, "").unwrap_err().contains("does not fit"));
        let backwards = r#"{"question": "Next?", "profile": {"work": [{"org": "X", "from": 2020, "to": 2010, "visibility": "public"}]}}"#;
        assert!(parse(backwards, "").unwrap_err().contains("runs backwards"));
        assert!(parse(r#"{"question": "", "done": false, "profile": {}}"#, "").unwrap_err().contains("question is empty"));
    }

    #[test]
    fn the_prompt_carries_the_turns_and_the_profile() {
        let job = InterviewJob {
            session: "s".into(),
            turns: vec![InterviewTurn { question: "What is your name?".into(), answer: "Kim Example".into() }],
            profile: None,
            handle: "kim".into(),
        };
        let p = prompt(&job);
        assert!(p.contains("Question 1: What is your name?\nAnswer: Kim Example"));
        assert!(p.contains("\"handle\": \"kim\""));
        let last = InterviewJob { turns: vec![job.turns[0].clone(); MAX_TURNS], ..job };
        assert!(prompt(&last).contains("set done to true"));
        assert!(system_prompt().contains("\"visibility\": \"friends\""));
    }
}
