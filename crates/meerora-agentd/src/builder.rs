//! The figure builder of screen 3. A runtime writes a figure script from the
//! user's description, the script host validates it, and errors go back to
//! the runtime for at most [`ATTEMPTS`] tries in total.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use meerora_protocol::{DraftStatus, Event, RuntimeConfig, RuntimeKind, Settings};
use meerora_runtime::{Permissions, RunEvent, RunRequest, Runtime};
use meerora_script::{Limits, build_figure};

use crate::daemon::RuntimeFactory;
use crate::runs::panic_text;

const API: &str = include_str!("../../meerora-script/API.md");
const GUIDE: &str = include_str!("../../meerora-script/BUILDER.md");
const WREN: &str = include_str!("../../../assets/agents/wren/figure.js");
const RIVET: &str = include_str!("../../../assets/agents/rivet/figure.js");

/// Tries per request, the first one included.
pub const ATTEMPTS: usize = 3;
/// The longest one answer of the builder may take.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(300);

/// One `Command::Build`.
#[derive(Clone, Debug)]
pub struct BuildJob {
    pub session: String,
    pub prompt: String,
    pub script: Option<String>,
}

/// The builder runtime when `meerora.toml` names none.
pub fn default_runtime() -> RuntimeConfig {
    RuntimeConfig { kind: RuntimeKind::ClaudeCli, model: Some("sonnet".into()), url: None }
}

/// The builder's system prompt with the modelling API, the guide and two examples.
pub fn system_prompt() -> String {
    format!(
        "You write figure scripts for meerora, a desktop where every agent is a small low-poly figure in a shared space. A figure \
script is JavaScript that runs once in a sandbox and describes the figure with the modelling API below. You cannot run tools or \
read files.

Answer with exactly one ```js block that holds the complete figure script, followed by one or two sentences about what you made or \
changed. Only use what the API below documents.

# Modelling API

{API}

# Guide

{GUIDE}

# Example: Wren, a mail agent

```js
{wren}
```

# Example: Rivet, a system robot

```js
{rivet}
```
",
        wren = WREN.trim_end(),
        rivet = RIVET.trim_end(),
    )
}

/// Runs one build request and reports `FigureDraft` events through `emit`.
pub fn run(job: &BuildJob, config: &RuntimeConfig, settings: &Settings, factory: &RuntimeFactory, emit: &mut dyn FnMut(Event)) {
    let draft = |status, message: String, script: Option<String>, triangles: Option<usize>| Event::FigureDraft {
        session: job.session.clone(),
        status,
        script,
        message,
        triangles,
    };
    emit(draft(DraftStatus::Working, "Asking the builder for a figure.".into(), None, None));
    let dir = match WorkDir::new(&job.session) {
        Ok(dir) => dir,
        Err(e) => {
            emit(draft(DraftStatus::Failed, format!("meerora cannot create a folder for the builder ({e})."), None, None));
            return;
        }
    };
    let mut retry = |message: String| emit(draft(DraftStatus::Working, message, None, None));
    match attempts(job, config, settings, factory, &dir.0, &mut retry) {
        Ok(ready) => emit(draft(DraftStatus::Ready, ready.message, Some(ready.script), Some(ready.triangles))),
        Err(error) => emit(draft(DraftStatus::Failed, error, None, None)),
    }
}

struct Ready {
    script: String,
    triangles: usize,
    message: String,
}

fn attempts(
    job: &BuildJob,
    config: &RuntimeConfig,
    settings: &Settings,
    factory: &RuntimeFactory,
    dir: &Path,
    retry: &mut dyn FnMut(String),
) -> Result<Ready, String> {
    let system = system_prompt();
    let first = first_prompt(job);
    let mut prompt = first.clone();
    let mut session = None;
    let mut error = String::new();
    for attempt in 1..=ATTEMPTS {
        if attempt > 1 {
            retry(format!("Try {} did not pass, so the builder tries again. {error}", attempt - 1));
        }
        let answer = ask(config, settings, factory, dir, &system, &prompt, session.clone())?;
        session = answer.session.or(session);
        let mut broken = None;
        match extract_js(&answer.text).or_else(|| extract_js(&answer.blocks)) {
            None => error = "The answer had no ```js block.".into(),
            Some(script) => match build_figure(&script, "figure.js", &Limits::default()) {
                Ok(figure) => {
                    let source = if extract_js(&answer.text).is_some() { &answer.text } else { &answer.blocks };
                    return Ok(Ready { script, triangles: figure.triangles, message: sentences(source) });
                }
                Err(e) => {
                    error = format!("The script does not pass the validator ({e}).");
                    broken = Some(script);
                }
            },
        }
        prompt = match (&session, broken) {
            (Some(_), _) => format!("{error} Send the complete corrected script in one ```js block, then one or two sentences."),
            (None, Some(script)) => format!(
                "{first}\n\nYour last script did not pass. {error}\n\n```js\n{script}\n```\n\nSend the complete corrected script in one ```js block, then one or two sentences."
            ),
            (None, None) => format!("{first}\n\n{error} Send the complete script in one ```js block, then one or two sentences."),
        };
    }
    Err(error)
}

fn first_prompt(job: &BuildJob) -> String {
    match &job.script {
        Some(script) => {
            format!("This is the current figure script.\n\n```js\n{script}\n```\n\nChange it as follows. {}", job.prompt.trim())
        }
        None => format!("Make a figure for this agent. {}", job.prompt.trim()),
    }
}

struct Answer {
    /// The final answer of the run.
    text: String,
    /// All text blocks of the run, in case the script was in an earlier one.
    blocks: String,
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
) -> Result<Answer, String> {
    let request = RunRequest {
        agent: "builder".into(),
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
        Ok(Err(e)) => return Err(format!("The builder cannot start ({e:#}).")),
        Err(panic) => return Err(format!("The builder runtime crashed ({}).", panic_text(&*panic))),
    };
    let deadline = Instant::now() + ANSWER_TIMEOUT;
    let (mut blocks, mut session) = (Vec::new(), None);
    loop {
        match handle.events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(RunEvent::Session { id }) => session = Some(id),
            Ok(RunEvent::Text { text }) => blocks.push(text),
            Ok(RunEvent::Finished { result: Ok(text) }) => return Ok(Answer { text, blocks: blocks.join("\n\n"), session }),
            Ok(RunEvent::Finished { result: Err(e) }) => return Err(format!("The builder failed ({e}).")),
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {
                handle.cancel();
                return Err("The builder took longer than 5 minutes.".into());
            }
            Err(RecvTimeoutError::Disconnected) => return Err("The builder stopped without an answer.".into()),
        }
    }
}

/// The first ```js block, or a bare ``` block that builds a figure.
pub fn extract_js(text: &str) -> Option<String> {
    let mut blocks: Vec<(String, String)> = Vec::new();
    let mut current: Option<(String, String)> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        match current.as_mut() {
            None if trimmed.starts_with("```") => current = Some((trimmed.trim_start_matches('`').trim().to_lowercase(), String::new())),
            None => {}
            Some(_) if trimmed == "```" => blocks.extend(current.take()),
            Some((_, body)) => {
                body.push_str(line);
                body.push('\n');
            }
        }
    }
    let is_js = |lang: &str| lang == "js" || lang == "javascript";
    blocks
        .iter()
        .find(|(lang, body)| is_js(lang) && !body.trim().is_empty())
        .or_else(|| blocks.iter().find(|(lang, body)| lang.is_empty() && body.contains("figure(")))
        .map(|(_, body)| body.clone())
}

/// What the builder said outside its code blocks, on one line.
fn sentences(text: &str) -> String {
    let mut words = Vec::new();
    let mut in_block = false;
    for line in text.lines() {
        if line.trim().starts_with("```") {
            in_block = !in_block;
        } else if !in_block {
            words.extend(line.split_whitespace());
        }
    }
    let said = words.join(" ");
    if said.is_empty() { "Here is the figure.".into() } else { crate::runs::cut(&said, 300) }
}

/// A private folder for one builder session, removed again afterwards.
struct WorkDir(PathBuf);

impl WorkDir {
    fn new(session: &str) -> std::io::Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let tag: String = session.chars().filter(|c| c.is_ascii_alphanumeric()).take(24).collect();
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("meerora-builder-{}-{tag}-{n}", std::process::id()));
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
    fn extracts_the_script_block() {
        let text = "Here it is.\n```js\nexport default figure({});\n```\nI made it small.";
        assert_eq!(extract_js(text).as_deref(), Some("export default figure({});\n"));
        assert_eq!(sentences(text), "Here it is. I made it small.");
        assert_eq!(extract_js("```\nconst a = figure({});\n```").as_deref(), Some("const a = figure({});\n"));
        assert_eq!(extract_js("```toml\nx = 1\n```"), None);
        assert_eq!(extract_js("no code"), None);
    }

    #[test]
    fn the_system_prompt_holds_api_guide_and_examples() {
        let prompt = system_prompt();
        assert!(prompt.contains("# Modelling API") && prompt.contains("# Guide"));
        assert!(prompt.contains("export default figure("));
        assert!(prompt.matches("```js").count() >= 2);
    }
}
