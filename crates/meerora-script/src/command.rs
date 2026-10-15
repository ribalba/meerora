//! Command scripts for the command box (Alt+Space in the app). A script in
//! `<data dir>/commands/*.js` ends with
//! `export default command({ keyword, title, run(query) })`. Unlike figure
//! scripts it stays loaded: one QuickJS runtime and context per script,
//! called once per query with its own time limit. `run` returns result items
//! whose actions are plain data ([`CommandAction`]). The script has no file,
//! network or process access, and meerora carries out an action only when
//! the user picks the item (D8). API.md describes the API.
//!
//! [`CommandScript`] is not `Send`. The app keeps each one on a thread of
//! its own.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use rquickjs::context::{EvalOptions, intrinsic};
use rquickjs::promise::PromiseState;
use rquickjs::{Context, Ctx, Exception, Function, Module, Object, Persistent, Runtime, Value};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json};

use crate::host::{describe, engine_error};
use crate::{Limits, ScriptError};

const PRELUDE: &str = include_str!("command_prelude.js");
const PRELUDE_NAME: &str = "<meerora-command>";

/// The most items one call of `run` may return. Later ones are dropped.
pub const MAX_ITEMS: usize = 50;
const MAX_TITLE: usize = 200;
const MAX_SUBTITLE: usize = 300;
const MAX_TEXT: usize = 20_000;
const MAX_ARG: usize = 4096;
const MAX_ARGS: usize = 64;

/// Like figure scripts, plus Date, because a command such as a time
/// converter needs the clock and nothing caches what a command returns.
type Intrinsics = (
    intrinsic::Date,
    intrinsic::Eval,
    intrinsic::RegExpCompiler,
    intrinsic::RegExp,
    intrinsic::Json,
    intrinsic::Proxy,
    intrinsic::MapSet,
    intrinsic::TypedArrays,
    intrinsic::Promise,
);

/// Sandbox limits of one command script.
#[derive(Clone, Debug)]
pub struct CommandLimits {
    pub memory_bytes: usize,
    pub max_stack_bytes: usize,
    /// For loading the script, once.
    pub load_timeout_ms: u64,
    /// For each call of `run`, so a slow script cannot hold up typing.
    pub run_timeout_ms: u64,
}

impl Default for CommandLimits {
    fn default() -> Self {
        Self { memory_bytes: 16 << 20, max_stack_bytes: 512 << 10, load_timeout_ms: 500, run_timeout_ms: 200 }
    }
}

impl CommandLimits {
    fn as_limits(&self, timeout_ms: u64) -> Limits {
        Limits { memory_bytes: self.memory_bytes, max_stack_bytes: self.max_stack_bytes, timeout_ms, hard_triangle_cap: 0 }
    }
}

/// What `command({...})` declared.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct CommandMeta {
    /// The word typed before the query, for example `temp`.
    pub keyword: String,
    pub title: String,
    #[serde(default)]
    pub subtitle: String,
}

/// What an item does when the user picks it. meerora checks and carries it
/// out, the script never does. The same actions serve `commands.toml`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CommandAction {
    /// Opens an http or https URL in the target agent's browser.
    OpenUrl { url: String },
    /// Starts a program with arguments, without a shell, as a real app on
    /// the target agent's desktop.
    Launch {
        program: String,
        #[serde(default)]
        args: Vec<String>,
    },
    /// Sends `text` to `agent`, like `@agent text`.
    Say { agent: String, text: String },
    /// Opens a built-in app such as `editor` or `terminal:2` in the target agent's view.
    OpenApp { app: String },
    /// Puts `text` on the clipboard.
    Copy { text: String },
    /// Shows `text` as a toast.
    Notify { text: String },
    /// Evaluates `expression` with the box's calculator and shows the result.
    Calc { expression: String },
}

impl CommandAction {
    /// Checks the limits and the shape of the action. Which apps and agents
    /// exist is checked by the app.
    pub fn check(&self) -> Result<(), String> {
        let text = |value: &str, what: &str, max: usize| -> Result<(), String> {
            if value.chars().count() > max {
                return Err(format!("{what} is longer than {max} characters"));
            }
            if value.contains('\0') {
                return Err(format!("{what} contains a NUL character"));
            }
            Ok(())
        };
        let filled = |value: &str, what: &str| if value.trim().is_empty() { Err(format!("{what} is empty")) } else { Ok(()) };
        match self {
            CommandAction::OpenUrl { url } => {
                text(url, "open_url url", MAX_ARG)?;
                let lower = url.trim().to_ascii_lowercase();
                if !(lower.starts_with("https://") || lower.starts_with("http://")) {
                    return Err(format!("open_url takes an http or https URL, got {url:?}"));
                }
                if url.trim().chars().any(|c| c.is_whitespace() || c.is_control()) {
                    return Err("open_url url contains spaces or control characters".into());
                }
            }
            CommandAction::Launch { program, args } => {
                filled(program, "launch program")?;
                text(program, "launch program", MAX_ARG)?;
                if args.len() > MAX_ARGS {
                    return Err(format!("launch takes at most {MAX_ARGS} arguments"));
                }
                for arg in args {
                    text(arg, "a launch argument", MAX_ARG)?;
                }
            }
            CommandAction::Say { agent, text: message } => {
                filled(agent, "say agent")?;
                text(agent, "say agent", 64)?;
                filled(message, "say text")?;
                text(message, "say text", MAX_TEXT)?;
            }
            CommandAction::OpenApp { app } => {
                filled(app, "open_app app")?;
                text(app, "open_app app", 32)?;
            }
            CommandAction::Copy { text: value } => text(value, "copy text", MAX_TEXT)?,
            CommandAction::Notify { text: value } => {
                filled(value, "notify text")?;
                text(value, "notify text", 2000)?;
            }
            CommandAction::Calc { expression } => {
                filled(expression, "calc expression")?;
                text(expression, "calc expression", 1000)?;
            }
        }
        Ok(())
    }
}

/// One result row from `run`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandItem {
    /// Empty only for a `calc` item, which shows its result instead.
    pub title: String,
    pub subtitle: String,
    /// One or two characters for the icon square.
    pub glyph: Option<String>,
    /// What Enter does. None shows the row without an action.
    pub action: Option<CommandAction>,
    /// What Shift+Enter does.
    pub alt: Option<CommandAction>,
}

enum Failure {
    Thrown(Persistent<Value<'static>>),
    Engine(rquickjs::Error),
}

/// A loaded command script. Fields drop in order, the saved function before
/// the context and the runtime.
pub struct CommandScript {
    runner: Persistent<Function<'static>>,
    ctx: Context,
    rt: Runtime,
    deadline: Rc<Cell<Instant>>,
    timed_out: Rc<Cell<bool>>,
    meta: CommandMeta,
    index: i32,
    source: String,
    file_name: String,
    limits: CommandLimits,
    /// A time or memory limit hit the last call. The next call loads the
    /// script again first, so a half finished call leaves nothing behind.
    spoiled: bool,
}

impl CommandScript {
    /// Evaluates `source` and reads what its `command({...})` declared.
    pub fn load(source: &str, file_name: &str, limits: &CommandLimits) -> Result<Self, ScriptError> {
        let load = limits.as_limits(limits.load_timeout_ms);
        let rt = Runtime::new().map_err(|e| engine_error(e, &load))?;
        rt.set_memory_limit(limits.memory_bytes);
        rt.set_max_stack_size(limits.max_stack_bytes);
        let deadline = Rc::new(Cell::new(Instant::now() + Duration::from_millis(limits.load_timeout_ms)));
        let timed_out = Rc::new(Cell::new(false));
        let (d, t) = (deadline.clone(), timed_out.clone());
        rt.set_interrupt_handler(Some(Box::new(move || {
            let late = Instant::now() >= d.get();
            if late {
                t.set(true);
            }
            late
        })));
        let ctx = Context::custom::<Intrinsics>(&rt).map_err(|e| engine_error(e, &load))?;
        let metas: Rc<RefCell<Vec<String>>> = Rc::default();
        let outcome = ctx.with(|ctx| match evaluate(&ctx, source, file_name, &metas) {
            Ok(v) => Ok(v),
            Err(rquickjs::Error::Exception) => Err(Failure::Thrown(Persistent::save(&ctx, ctx.catch()))),
            Err(e) => Err(Failure::Engine(e)),
        });
        let (runner, index) = match outcome {
            Ok(Some(found)) => found,
            Ok(None) => return Err(ScriptError::WrongKind { expected: "command" }),
            Err(failure) => {
                // Lift the limits so the error itself can be read.
                let near_limit = rt.memory_usage().malloc_size.max(0) as usize + (256 << 10) >= limits.memory_bytes;
                deadline.set(Instant::now() + Duration::from_secs(1));
                rt.set_memory_limit(0);
                let error = read_failure(&ctx, failure, file_name, &load, near_limit);
                return Err(if timed_out.get() { ScriptError::Timeout(limits.load_timeout_ms) } else { error });
            }
        };
        let meta = metas
            .borrow()
            .get(usize::try_from(index).unwrap_or(usize::MAX))
            .and_then(|json| serde_json::from_str::<CommandMeta>(json).ok())
            .ok_or(ScriptError::WrongKind { expected: "command" })?;
        Ok(Self {
            runner,
            ctx,
            rt,
            deadline,
            timed_out,
            meta,
            index,
            source: source.to_string(),
            file_name: file_name.to_string(),
            limits: limits.clone(),
            spoiled: false,
        })
    }

    pub fn meta(&self) -> &CommandMeta {
        &self.meta
    }

    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    /// Calls `run(query)` within the run time limit and checks what it
    /// returned. After a time or memory limit the script is loaded again
    /// before the next call.
    pub fn run(&mut self, query: &str) -> Result<Vec<CommandItem>, ScriptError> {
        if self.spoiled {
            *self = Self::load(&self.source, &self.file_name, &self.limits)?;
        }
        let limits = self.limits.as_limits(self.limits.run_timeout_ms);
        self.timed_out.set(false);
        self.deadline.set(Instant::now() + Duration::from_millis(self.limits.run_timeout_ms));
        let runner = self.runner.clone();
        let index = self.index;
        let outcome = self.ctx.with(|ctx| match call(&ctx, runner, index, query) {
            Ok(json) => Ok(json),
            Err(rquickjs::Error::Exception) => Err(Failure::Thrown(Persistent::save(&ctx, ctx.catch()))),
            Err(e) => Err(Failure::Engine(e)),
        });
        let failure = match outcome {
            Ok(json) => {
                self.rt.run_gc();
                return parse_items(&json, &self.file_name);
            }
            Err(failure) => failure,
        };
        let near_limit = self.rt.memory_usage().malloc_size.max(0) as usize + (256 << 10) >= self.limits.memory_bytes;
        let timed_out = self.timed_out.get();
        self.deadline.set(Instant::now() + Duration::from_secs(1));
        if near_limit || timed_out {
            self.rt.set_memory_limit(0);
        }
        let error = read_failure(&self.ctx, failure, &self.file_name, &limits, near_limit);
        let error = if timed_out { ScriptError::Timeout(self.limits.run_timeout_ms) } else { error };
        // The limits were lifted to read the error, so the next call starts afresh.
        self.spoiled = timed_out || near_limit || matches!(error, ScriptError::OutOfMemory(_));
        self.rt.run_gc();
        Err(error)
    }
}

/// Runs the prelude and the module, and returns the saved runner and the
/// index of the command the default export refers to.
fn evaluate<'js>(
    ctx: &Ctx<'js>,
    source: &str,
    file_name: &str,
    metas: &Rc<RefCell<Vec<String>>>,
) -> rquickjs::Result<Option<(Persistent<Function<'static>>, i32)>> {
    let native = Object::new(ctx.clone())?;
    let m = metas.clone();
    let command_fn = move |json: String| -> u32 {
        let mut list = m.borrow_mut();
        list.push(json);
        list.len() as u32 - 1
    };
    native.set("command", Function::new(ctx.clone(), command_fn)?)?;
    ctx.globals().set("__meerora_native", native)?;
    let mut opts = EvalOptions::default();
    opts.filename = Some(PRELUDE_NAME.into());
    ctx.eval_with_options::<(), _>(PRELUDE, opts)?;
    // The script cannot reach or replace the runner once it is taken out.
    let runner: Function = ctx.globals().get("__meerora_run")?;
    ctx.globals().remove("__meerora_run")?;
    let runner = Persistent::save(ctx, runner);

    let module = Module::declare(ctx.clone(), file_name, source)?;
    let (module, promise) = module.eval()?;
    while promise.state() == PromiseState::Pending && ctx.execute_pending_job() {}
    match promise.state() {
        PromiseState::Pending => {
            return Err(Exception::throw_message(ctx, "the script never finished (top-level await on a promise that never settles)"));
        }
        PromiseState::Rejected => return promise.result::<()>().unwrap_or(Err(rquickjs::Error::Exception)).map(|_| None),
        PromiseState::Resolved => {}
    }
    let default: Value = module.get("default")?;
    let Some(obj) = default.as_object() else { return Ok(None) };
    let id: Value = obj.get("__meerora_command")?;
    Ok(id.as_int().map(|i| (runner, i)))
}

/// One call of the runner. A promise is driven until it settles or the time limit stops it.
fn call<'js>(ctx: &Ctx<'js>, runner: Persistent<Function<'static>>, index: i32, query: &str) -> rquickjs::Result<String> {
    let runner = runner.restore(ctx)?;
    let out: Value = runner.call((index, query))?;
    let Some(promise) = out.as_promise() else { return out.get::<String>() };
    while promise.state() == PromiseState::Pending && ctx.execute_pending_job() {}
    match promise.state() {
        PromiseState::Pending => Err(Exception::throw_message(ctx, "run() returned a promise that never settles")),
        PromiseState::Rejected | PromiseState::Resolved => promise.result::<String>().unwrap_or(Err(rquickjs::Error::Exception)),
    }
}

fn read_failure(ctx: &Context, failure: Failure, file_name: &str, limits: &Limits, near_limit: bool) -> ScriptError {
    match failure {
        Failure::Engine(e) => engine_error(e, limits),
        Failure::Thrown(value) => ctx.with(|ctx| match value.restore(&ctx) {
            Ok(v) => describe(&v, file_name, limits, near_limit),
            Err(e) => engine_error(e, limits),
        }),
    }
}

/// Reads the JSON of the items `run` returned and checks every item.
pub fn parse_items(json: &str, file_name: &str) -> Result<Vec<CommandItem>, ScriptError> {
    let values: Vec<Json> = serde_json::from_str(json)
        .map_err(|e| ScriptError::Invalid(format!("{file_name}: run() returned something that is not a list ({e})")))?;
    values
        .into_iter()
        .take(MAX_ITEMS)
        .enumerate()
        .map(|(i, v)| item(v).map_err(|e| ScriptError::Invalid(format!("{file_name}: item {} of run(): {e}", i + 1))))
        .collect()
}

fn item(value: Json) -> Result<CommandItem, String> {
    let Json::Object(map) = value else {
        return Err(format!("an item is an object like {{ title, subtitle, action }}, got {}", kind(&value)));
    };
    const KNOWN: [&str; 5] = ["title", "subtitle", "glyph", "action", "alt"];
    if let Some(key) = map.keys().find(|k| !KNOWN.contains(&k.as_str())) {
        return Err(format!("an item has no field {key:?}. Known fields: {}", KNOWN.join(", ")));
    }
    let field = |key: &str, max: usize| -> Result<String, String> {
        match map.get(key) {
            None | Some(Json::Null) => Ok(String::new()),
            Some(v) => scalar(v).map(|s| cut(&s, max)).ok_or_else(|| format!("{key} must be a string, got {}", kind(v))),
        }
    };
    let action = |key: &str| -> Result<Option<CommandAction>, String> {
        let Some(v) = map.get(key).filter(|v| !v.is_null()) else { return Ok(None) };
        let action: CommandAction = serde_json::from_value(coerce(v.clone())).map_err(|e| format!("{key}: {e}"))?;
        action.check().map_err(|e| format!("{key}: {e}"))?;
        Ok(Some(action))
    };
    let title = field("title", MAX_TITLE)?;
    let action_value = action("action")?;
    if title.trim().is_empty() && !matches!(action_value, Some(CommandAction::Calc { .. })) {
        return Err("an item needs a title".into());
    }
    let glyph = field("glyph", 8)?;
    let glyph = (!glyph.trim().is_empty()).then(|| glyph.trim().chars().take(2).collect());
    Ok(CommandItem { title, subtitle: field("subtitle", MAX_SUBTITLE)?, glyph, action: action_value, alt: action("alt")? })
}

/// Numbers and booleans as text, as JavaScript would print them.
fn scalar(v: &Json) -> Option<String> {
    match v {
        Json::String(s) => Some(s.clone()),
        Json::Number(n) => Some(n.to_string()),
        Json::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// An action object with its numbers and booleans turned into text, so
/// `{ type: "copy", text: 42 }` works.
fn coerce(v: Json) -> Json {
    match v {
        Json::Object(map) => Json::Object(map.into_iter().map(|(k, v)| (k, coerce_field(v))).collect::<Map<_, _>>()),
        other => other,
    }
}

fn coerce_field(v: Json) -> Json {
    match v {
        Json::Number(_) | Json::Bool(_) => Json::String(scalar(&v).unwrap_or_default()),
        Json::Array(items) => Json::Array(items.into_iter().map(coerce_field).collect()),
        other => other,
    }
}

fn kind(v: &Json) -> &'static str {
    match v {
        Json::Null => "null",
        Json::Bool(_) => "a boolean",
        Json::Number(_) => "a number",
        Json::String(_) => "a string",
        Json::Array(_) => "an array",
        Json::Object(_) => "an object",
    }
}

fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(source: &str) -> Result<CommandScript, ScriptError> {
        CommandScript::load(source, "test.js", &CommandLimits::default())
    }

    const TEMP: &str = r#"
        export default command({
          keyword: "temp",
          title: "Temperatures",
          run(query) {
            const c = Number(query);
            if (!Number.isFinite(c)) return [];
            const f = c * 9 / 5 + 32;
            return [
              { title: `${f} °F`, subtitle: `${c} °C`, glyph: "°", action: copy(f), alt: notify(`${c} °C is ${f} °F`) },
              { title: "On the web", action: openUrl("https://example.org/?q=" + encodeURIComponent(query)) },
            ];
          },
        });
    "#;

    #[test]
    fn a_command_declares_itself_and_returns_items() {
        let mut script = load(TEMP).unwrap();
        assert_eq!(script.meta(), &CommandMeta { keyword: "temp".into(), title: "Temperatures".into(), subtitle: String::new() });
        let items = script.run("100").unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "212 °F");
        assert_eq!(items[0].glyph.as_deref(), Some("°"));
        assert_eq!(items[0].action, Some(CommandAction::Copy { text: "212".into() }), "numbers become text");
        assert_eq!(items[0].alt, Some(CommandAction::Notify { text: "100 °C is 212 °F".into() }));
        assert_eq!(items[1].action, Some(CommandAction::OpenUrl { url: "https://example.org/?q=100".into() }));
        assert!(script.run("warm").unwrap().is_empty(), "the context stays alive between calls");
        assert_eq!(script.run("0").unwrap()[0].title, "32 °F");
    }

    #[test]
    fn state_survives_between_calls_and_promises_are_awaited() {
        let mut script = load(
            r#"let calls = 0;
               export default command({ keyword: "n", title: "Count", async run(q) { calls += 1; return { title: `${calls} ${q}` }; } });"#,
        )
        .unwrap();
        assert_eq!(script.run("a").unwrap()[0].title, "1 a");
        assert_eq!(script.run("b").unwrap()[0].title, "2 b", "one context per script");
    }

    #[test]
    fn an_endless_loop_is_stopped_and_the_script_recovers() {
        let source = r#"export default command({ keyword: "loop", title: "Loop", run(q) { if (q === "spin") { for (;;) {} } return [{ title: q }]; } });"#;
        let mut script = load(source).unwrap();
        let started = Instant::now();
        let err = script.run("spin").unwrap_err();
        assert!(matches!(err, ScriptError::Timeout(200)), "{err:?}");
        assert!(started.elapsed() < Duration::from_secs(2), "stopped in time: {:?}", started.elapsed());
        assert_eq!(script.run("fine").unwrap()[0].title, "fine", "loaded again after the timeout");

        let err = load("for (;;) {}\nexport default command({ keyword: \"x\", title: \"X\", run() { return []; } });").err().unwrap();
        assert!(matches!(err, ScriptError::Timeout(500)), "{err:?}");
    }

    #[test]
    fn memory_is_limited() {
        let source = r#"export default command({ keyword: "big", title: "Big", run() { const a = []; for (;;) a.push(new Array(1e5).fill(1)); } });"#;
        let mut script = load(source).unwrap();
        let err = script.run("x").unwrap_err();
        assert!(matches!(err, ScriptError::OutOfMemory(_) | ScriptError::Timeout(_)), "{err:?}");
    }

    #[test]
    fn errors_name_the_file_and_line() {
        let mut script = load("export default command({ keyword: \"e\", title: \"E\", run(q) {\n  return q.nope.deeper;\n} });").unwrap();
        let err = script.run("x").unwrap_err().to_string();
        assert!(err.contains("test.js:2") && err.contains("TypeError"), "{err}");

        let syntax = load("export default command({ keyword: \"e\", title: \"E\", run(q) { return [ } });").err().unwrap().to_string();
        assert!(syntax.contains("test.js"), "{syntax}");
        let wrong = load("export default 42;").err().unwrap();
        assert!(matches!(wrong, ScriptError::WrongKind { expected: "command" }), "{wrong:?}");
        let keyword = load("export default command({ keyword: \"two words\", title: \"E\", run() {} });").err().unwrap().to_string();
        assert!(keyword.contains("keyword must be one word"), "{keyword}");
        let option = load("export default command({ keyword: \"k\", title: \"E\", run() {}, icon: 1 });").err().unwrap().to_string();
        assert!(option.contains("no option \"icon\""), "{option}");
    }

    #[test]
    fn items_and_actions_are_checked() {
        let run = |items: &str| {
            let source = format!("export default command({{ keyword: \"k\", title: \"K\", run(q) {{ return {items}; }} }});");
            load(&source).unwrap().run("q").map_err(|e| e.to_string())
        };
        assert!(run(r#"[{ title: "a", action: openUrl("file:///etc/passwd") }]"#).unwrap_err().contains("http or https"));
        assert!(run(r#"[{ title: "a", action: { type: "rm", path: "/" } }]"#).unwrap_err().contains("unknown variant"));
        assert!(run(r#"[{ title: "a", action: { type: "copy", text: "x", extra: 1 } }]"#).unwrap_err().contains("unknown field"));
        assert!(run(r#"[{ subtitle: "no title" }]"#).unwrap_err().contains("needs a title"));
        assert!(run(r#"[{ title: "a", icon: "x" }]"#).unwrap_err().contains("no field \"icon\""));
        assert!(run(r#"["just text"]"#).unwrap_err().contains("item 1"));
        assert!(run(r#"[{ title: "a", action: say("", "hi") }]"#).unwrap_err().contains("say agent is empty"));
        let items = run(r#"[{ action: calc("2+2") }, { title: "b", action: launch("echo", "a b", 3) }]"#).unwrap();
        assert_eq!(items[0].title, "", "a calc item may leave the title to its result");
        assert_eq!(items[1].action, Some(CommandAction::Launch { program: "echo".into(), args: vec!["a b".into(), "3".into()] }));
        let many = run("Array.from({ length: 80 }, (_, i) => ({ title: String(i) }))").unwrap();
        assert_eq!(many.len(), MAX_ITEMS);
        assert!(run("undefined").unwrap().is_empty());
        assert_eq!(run(r#"{ title: "one" }"#).unwrap().len(), 1, "a single item needs no list");
    }

    #[test]
    fn scripts_have_no_io() {
        let source = r#"export default command({ keyword: "io", title: "IO", run() {
            return [{ title: [typeof require, typeof fetch, typeof std, typeof os, typeof XMLHttpRequest, typeof Date].join(" ") }];
        } });"#;
        assert_eq!(load(source).unwrap().run("").unwrap()[0].title, "undefined undefined undefined undefined undefined function");
    }
}
