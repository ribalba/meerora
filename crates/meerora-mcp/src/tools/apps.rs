//! The five app tools of D30 (docs/design/agent-apps.md): `apps_list`,
//! `app_read`, `app_screenshot`, `app_act` and `app_open`. Every agent gets
//! them. The daemon checks the capability of each call (`apps.read`,
//! `apps.control`, `apps.input`, `apps.launch`) and the app checks the
//! handle and the arguments of each action, so this module only checks the
//! shape of a call.

use meerora_protocol::ToolCall;
use serde_json::{Map, Value, json};

use super::{CallError, check_length, only, quote, required_string, tool};

/// Longest `query` of `app_read`, in characters.
pub const QUERY_MAX: usize = 200;
/// Longest app name or desktop id of `app_open`, in characters.
pub const APP_MAX: usize = 200;

/// Every action of `app_act`, over all kinds. The app says which ones a
/// kind takes when an action does not fit.
pub const ACTIONS: [&str; 15] =
    ["focus", "close", "type", "key", "run", "open", "click", "scroll", "back", "forward", "reload", "goto", "stage", "unstage", "commit"];

/// Arguments of actions that models sometimes put next to `action` instead
/// of into `args`. They are moved into `args`.
const LOOSE_ARGS: [&str; 15] =
    ["text", "keys", "command", "url", "ref", "submit", "dx", "dy", "x", "y", "button", "path", "line", "message", "args"];

/// The definitions, always in the same order.
pub fn definitions() -> Vec<Value> {
    vec![apps_list(), app_read(), app_screenshot(), app_act(), app_open()]
}

/// Validates a call of one of these tools. `Err(UnknownTool)` for other names.
pub fn parse(name: &str, args: &Map<String, Value>) -> Result<ToolCall, CallError> {
    let invalid = CallError::Invalid;
    match name {
        "apps_list" => {
            if !args.is_empty() {
                return Err(invalid("apps_list takes no arguments.".into()));
            }
            Ok(ToolCall::AppsList)
        }
        "app_read" => {
            only(name, args, &["app", "query"]).map_err(invalid)?;
            let app = handle(name, args).map_err(invalid)?;
            let query = match args.get("query") {
                None | Some(Value::Null) => None,
                Some(Value::String(q)) if q.trim().is_empty() => None,
                Some(Value::String(q)) => {
                    check_length("The query", q.trim(), QUERY_MAX).map_err(invalid)?;
                    Some(q.trim().to_string())
                }
                Some(other) => return Err(invalid(format!("`query` must be a string, not {other}."))),
            };
            Ok(ToolCall::AppRead { app, query })
        }
        "app_screenshot" => {
            only(name, args, &["app"]).map_err(invalid)?;
            Ok(ToolCall::AppScreenshot { app: handle(name, args).map_err(invalid)? })
        }
        "app_act" => {
            let allowed: Vec<&str> = ["app", "action"].into_iter().chain(LOOSE_ARGS).collect();
            only(name, args, &allowed).map_err(invalid)?;
            let app = handle(name, args).map_err(invalid)?;
            let action = required_string(name, args, "action").map_err(invalid)?.trim().to_lowercase();
            if !ACTIONS.contains(&action.as_str()) {
                return Err(invalid(format!("There is no action {}. Actions: {}.", quote(&action), ACTIONS.join(", "))));
            }
            let mut act_args = match args.get("args") {
                None | Some(Value::Null) => Map::new(),
                Some(Value::Object(map)) => map.clone(),
                Some(other) => return Err(invalid(format!("`args` must be an object, not {other}."))),
            };
            for key in LOOSE_ARGS.iter().filter(|k| **k != "args") {
                if let Some(value) = args.get(*key) {
                    act_args.entry(key.to_string()).or_insert_with(|| value.clone());
                }
            }
            Ok(ToolCall::AppAct { app, action, args: Value::Object(act_args) })
        }
        "app_open" => {
            only(name, args, &["app", "args", "url", "path"]).map_err(invalid)?;
            let app = required_string(name, args, "app").map_err(invalid)?.trim().to_string();
            if app.is_empty() || app.contains(['\n', '\r']) {
                return Err(invalid("`app` names a built-in app such as terminal or browser, or a desktop id or program.".into()));
            }
            check_length("The app", &app, APP_MAX).map_err(invalid)?;
            let mut open_args = match args.get("args") {
                None | Some(Value::Null) => Map::new(),
                Some(Value::Object(map)) => map.clone(),
                Some(Value::Array(list)) => Map::from_iter([("args".to_string(), Value::Array(list.clone()))]),
                Some(other) => return Err(invalid(format!("`args` must be an object, not {other}."))),
            };
            for key in ["url", "path"] {
                if let Some(value) = args.get(key) {
                    open_args.entry(key.to_string()).or_insert_with(|| value.clone());
                }
            }
            Ok(ToolCall::AppOpen { app, args: Value::Object(open_args) })
        }
        _ => Err(CallError::UnknownTool),
    }
}

/// The `app` handle: a kind of lowercase letters with an optional `:N`.
fn handle(tool: &str, args: &Map<String, Value>) -> Result<String, String> {
    let app = required_string(tool, args, "app")?.trim().to_lowercase();
    let (kind, number) = app.split_once(':').map_or((app.as_str(), None), |(k, n)| (k, Some(n)));
    let kind_ok = !kind.is_empty() && kind.chars().all(|c| c.is_ascii_lowercase());
    let number_ok = number.is_none_or(|n| !n.is_empty() && n.len() <= 10 && n.chars().all(|c| c.is_ascii_digit()));
    if kind_ok && number_ok {
        Ok(app)
    } else {
        Err(format!("{} is not a handle. Handles look like terminal:1, browser:2, chat or window:7, see apps_list.", quote(&app)))
    }
}

fn handle_schema() -> Value {
    json!({
        "type": "string",
        "pattern": "^[a-z]+(:[0-9]+)?$",
        "description": "The handle of an app instance from apps_list, for example terminal:1, browser:1, editor:1 or window:7.",
    })
}

fn read_only(mut definition: Value) -> Value {
    definition["annotations"]["readOnlyHint"] = json!(true);
    definition
}

fn apps_list() -> Value {
    let description = "Lists every open window of your agent view as an app instance: its handle, kind and title, whether it has \
                       the focus, and who controls it now (you, the user or nobody). Handles are terminal:1, browser:1, \
                       editor:1, files:1, viewer:1, chat, changes, activity and window:7 for a real app window. Needs the \
                       capability apps.read.";
    read_only(tool("apps_list", "List your apps", description.into(), json!({}), &[], true))
}

fn app_read() -> Value {
    let query = json!({
        "type": "string",
        "maxLength": QUERY_MAX,
        "description": "Optional words to look for. Only the lines and elements that contain them come back.",
    });
    let description = "Returns what an app instance shows, as JSON text: title, a one-line summary, the visible content (cut to \
                       a limit with a marker), the elements you can act on, each with a ref, a role, a name and maybe a value, \
                       and a few extra fields such as url or cwd. A terminal returns its screen and recent scrollback, a browser \
                       the page text with links, buttons and inputs, the editor the open file around the cursor. A real app \
                       window returns only its title, so use app_screenshot for it. Needs the capability apps.read.";
    read_only(tool("app_read", "Read an app", description.into(), json!({ "app": handle_schema(), "query": query }), &["app"], true))
}

fn app_screenshot() -> Value {
    let description = "Returns a PNG picture of a browser page or a real app window as the user sees it. Other kinds have no \
                       picture, read them with app_read. Needs the capability apps.read.";
    read_only(tool("app_screenshot", "Screenshot an app", description.into(), json!({ "app": handle_schema() }), &["app"], true))
}

fn app_act() -> Value {
    let action = json!({
        "type": "string",
        "enum": ACTIONS,
        "description": "The action. Which ones an app takes depends on its kind, see the description of the tool.",
    });
    let args = json!({
        "type": "object",
        "description": "The arguments of the action, for example {\"command\": \"cargo test\"} for run or {\"ref\": \"e3\"} for click.",
    });
    let description = "Performs one action in an app instance, which the user sees happen. The actions per kind:\n\
                       - every kind: focus (show it to the user), close\n\
                       - terminal: type {text}, key {keys} such as \"ctrl+c\", \"enter\" or \"up up enter\", run {command} (types \
                       it, presses Enter and returns the new end of the screen)\n\
                       - browser: open {url}, click {ref}, type {ref, text, submit}, scroll {dy}, back, forward, reload. Refs \
                       come from app_read and change when the page changes.\n\
                       - editor: open {path}, goto {line}. Files and viewer: open {path}. Paths are relative to your directory. \
                       Change files by writing them, the editor shows your changes to the user.\n\
                       - git: stage {path}, unstage {path}, commit {message}\n\
                       - real app window: click {x, y, button}, type {text}, key {keys}, scroll {dx, dy}, in points from the top \
                       left of its picture.\n\
                       Needs the capability apps.control, or apps.input for a real app window. When the user took over the \
                       instance, the action fails until they hand it back.";
    tool(
        "app_act",
        "Act in an app",
        description.into(),
        json!({ "app": handle_schema(), "action": action, "args": args }),
        &["app", "action"],
        false,
    )
}

fn app_open() -> Value {
    let app = json!({
        "type": "string",
        "maxLength": APP_MAX,
        "description": "A built-in app (terminal, browser, editor, files, viewer, changes, activity) or a real app by its desktop \
                        id such as org.gnome.Calculator.desktop or its program name.",
    });
    let args = json!({
        "type": "object",
        "description": "Optional: {\"url\": ...} for a browser, {\"path\": ...} for the editor, files or the viewer, {\"args\": [...]} \
                        for the program of a real app.",
    });
    let description = "Opens a new instance of a built-in app in your view, or starts a real app on your desktop, where it runs in \
                       your sandbox. Returns the new handle, for a real app once its window shows. Needs the capability \
                       apps.control for built-in apps and apps.launch for real apps.";
    tool("app_open", "Open an app", description.into(), json!({ "app": app, "args": args }), &["app"], false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    fn invalid(r: Result<ToolCall, CallError>) -> String {
        match r {
            Err(CallError::Invalid(msg)) => msg,
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    #[test]
    fn the_five_tools_have_schemas_for_the_model() {
        let defs = definitions();
        let names: Vec<&str> = defs.iter().map(|d| d["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["apps_list", "app_read", "app_screenshot", "app_act", "app_open"]);
        for d in &defs {
            let schema = &d["inputSchema"];
            assert_eq!(schema["type"], "object");
            assert_eq!(schema["additionalProperties"], false);
            let description = d["description"].as_str().unwrap();
            assert!(description.contains("apps."), "{description}");
            assert!(!description.contains('\u{2014}') && !description.contains(';'), "{description}");
        }
        let act = &defs[3]["inputSchema"];
        assert_eq!(act["required"], json!(["app", "action"]));
        assert_eq!(act["properties"]["action"]["enum"].as_array().unwrap().len(), ACTIONS.len());
        assert_eq!(act["properties"]["app"]["pattern"], "^[a-z]+(:[0-9]+)?$");
        assert_eq!(defs[1]["inputSchema"]["properties"]["query"]["maxLength"], QUERY_MAX);
        assert_eq!(defs[0]["annotations"]["readOnlyHint"], true);
        assert_eq!(defs[3]["annotations"]["readOnlyHint"], false);
    }

    #[test]
    fn calls_are_checked_and_loose_arguments_are_moved_into_args() {
        assert_eq!(parse("apps_list", &Map::new()), Ok(ToolCall::AppsList));
        assert_eq!(
            parse("app_read", &args(json!({ "app": " Terminal:1 ", "query": " error " }))),
            Ok(ToolCall::AppRead { app: "terminal:1".into(), query: Some("error".into()) })
        );
        assert_eq!(
            parse("app_read", &args(json!({ "app": "chat", "query": "" }))),
            Ok(ToolCall::AppRead { app: "chat".into(), query: None })
        );
        assert_eq!(parse("app_screenshot", &args(json!({ "app": "window:7" }))), Ok(ToolCall::AppScreenshot { app: "window:7".into() }));
        assert_eq!(
            parse("app_act", &args(json!({ "app": "terminal:1", "action": "Run", "args": { "command": "ls" } }))),
            Ok(ToolCall::AppAct { app: "terminal:1".into(), action: "run".into(), args: json!({ "command": "ls" }) })
        );
        assert_eq!(
            parse("app_act", &args(json!({ "app": "browser:1", "action": "type", "ref": "e2", "text": "hi", "submit": true }))),
            Ok(ToolCall::AppAct {
                app: "browser:1".into(),
                action: "type".into(),
                args: json!({ "ref": "e2", "text": "hi", "submit": true })
            })
        );
        assert_eq!(
            parse("app_act", &args(json!({ "app": "browser:1", "action": "back" }))),
            Ok(ToolCall::AppAct { app: "browser:1".into(), action: "back".into(), args: json!({}) })
        );
        assert_eq!(
            parse("app_open", &args(json!({ "app": "browser", "url": "https://example.org" }))),
            Ok(ToolCall::AppOpen { app: "browser".into(), args: json!({ "url": "https://example.org" }) })
        );
        assert_eq!(
            parse("app_open", &args(json!({ "app": "gnome-calculator", "args": ["--mode", "basic"] }))),
            Ok(ToolCall::AppOpen { app: "gnome-calculator".into(), args: json!({ "args": ["--mode", "basic"] }) })
        );

        assert!(invalid(parse("app_read", &args(json!({ "app": "../etc" })))).contains("is not a handle"));
        assert!(invalid(parse("app_read", &args(json!({ "app": "terminal:x" })))).contains("is not a handle"));
        assert!(invalid(parse("app_read", &args(json!({})))).contains("needs `app`"));
        assert!(invalid(parse("app_read", &args(json!({ "app": "chat", "query": "x".repeat(201) })))).contains("at most 200"));
        assert!(invalid(parse("app_act", &args(json!({ "app": "terminal:1", "action": "explode" })))).contains("There is no action"));
        assert!(
            invalid(parse("app_act", &args(json!({ "app": "terminal:1", "action": "run", "args": "ls" })))).contains("must be an object")
        );
        assert!(invalid(parse("app_act", &args(json!({ "app": "terminal:1", "action": "run", "speed": 2 })))).contains("has no argument"));
        assert_eq!(invalid(parse("apps_list", &args(json!({ "all": true })))), "apps_list takes no arguments.");
        assert!(invalid(parse("app_open", &args(json!({ "app": " " })))).contains("names a built-in app"));
        assert_eq!(parse("app_fly", &Map::new()), Err(CallError::UnknownTool));
    }
}
