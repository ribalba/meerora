//! The six meerora tools: their MCP definitions, built from the agent's
//! figure (D24) and the stations of the space (D9), and the validation of
//! their arguments before anything reaches the daemon. The personal and the
//! news agent get a few more, see `tools/social.rs` (D26, D28), and so does
//! the helper, see `tools/config.rs`. Every agent also gets the five app
//! tools of D30, see `tools/apps.rs`.

mod apps;
mod config;
mod social;

use std::path::Path;

use meerora_protocol::{AgentRole, ToolCall};
use meerora_script::{Limits, ParamDef, ParamKind};
use serde_json::{Map, Value, json};

/// Longest activity line, in characters.
pub const ACTIVITY_MAX: usize = 60;
/// Longest question for `ask_user`, in characters.
pub const QUESTION_MAX: usize = 300;
/// Longest note, in characters.
pub const NOTE_MAX: usize = 500;
/// Longest value of a text param, in characters.
pub const TEXT_PARAM_MAX: usize = 80;
/// Longest capability name for `request_capability`.
pub const CAPABILITY_MAX: usize = 64;
/// Longest reason for `request_capability`, in characters.
pub const REASON_MAX: usize = 300;

/// What the tools know about the agent's figure and the space. A part that
/// failed to build is `None`, and the matching tool then checks less.
#[derive(Clone, Debug, PartialEq)]
pub struct Tools {
    params: Option<Vec<ParamDef>>,
    stations: Option<Vec<String>>,
    /// The personal and the news agent get more tools (D26, D28, `social.rs`).
    role: Option<AgentRole>,
}

/// Why a `tools/call` was not forwarded.
#[derive(Clone, Debug, PartialEq)]
pub enum CallError {
    /// No tool has this name. A protocol error.
    UnknownTool,
    /// The arguments do not fit. Reported to the model so it can retry.
    Invalid(String),
}

impl Tools {
    #[cfg(test)]
    pub fn new(params: Option<Vec<ParamDef>>, stations: Option<Vec<String>>) -> Self {
        Self { params, stations, role: None }
    }

    /// Builds the agent's figure and the space below `data_dir`. Failures are
    /// logged and leave the matching tool without constraints.
    pub fn load(data_dir: &Path, agent: &str) -> Self {
        Self::load_from(data_dir, &meerora_protocol::space_dir(data_dir), agent)
    }

    /// Like [`Tools::load`] with the space of the room in `space`, the folder
    /// the daemon passes with `--space` because the sandbox shows no `meerora.toml`.
    pub fn load_from(data_dir: &Path, space: &Path, agent: &str) -> Self {
        let params = load_params(data_dir, agent)
            .inspect_err(|e| crate::log!("figure_set checks no names or values, because the figure of {agent} did not build. {e}"))
            .ok();
        let stations =
            load_stations(space).inspect_err(|e| crate::log!("go_to accepts any station, because the space did not build. {e}")).ok();
        let role = meerora_protocol::load_agent(&data_dir.join("agents").join(agent)).ok().and_then(|d| d.config.role);
        Self { params, stations, role }
    }

    /// The `tools` array of a `tools/list` result, always in the same order.
    pub fn definitions(&self) -> Vec<Value> {
        let mut tools = vec![self.figure_set(), self.go_to(), set_activity(), ask_user(), note(), request_capability()];
        tools.extend(social::definitions(self.role));
        tools.extend(config::definitions(self.role));
        // Every agent gets the app tools, the daemon checks their capabilities (D30).
        tools.extend(apps::definitions());
        tools
    }

    /// Validates a call and turns it into the command for the daemon.
    pub fn parse(&self, name: &str, args: &Map<String, Value>) -> Result<ToolCall, CallError> {
        let invalid = CallError::Invalid;
        match name {
            "figure_set" => {
                only(name, args, &["name", "value"]).map_err(invalid)?;
                let param = required_string(name, args, "name").map_err(invalid)?;
                let value = args.get("value").ok_or_else(|| invalid(format!("{name} needs `value`.")))?;
                let value = self.check_param(&param, value).map_err(invalid)?;
                Ok(ToolCall::FigureSet { name: param, value })
            }
            "go_to" => {
                only(name, args, &["station"]).map_err(invalid)?;
                let station = required_string(name, args, "station").map_err(invalid)?;
                if let Some(stations) = &self.stations
                    && !stations.contains(&station)
                {
                    return Err(invalid(format!("There is no station {}. Stations: {}.", quote(&station), stations.join(", "))));
                }
                Ok(ToolCall::GoTo { station })
            }
            "set_activity" => {
                only(name, args, &["text"]).map_err(invalid)?;
                let text = required_string(name, args, "text").map_err(invalid)?.trim().to_string();
                if text.contains(['\n', '\r']) {
                    return Err(invalid("The activity must be one line.".into()));
                }
                check_length("The activity", &text, ACTIVITY_MAX).map_err(invalid)?;
                Ok(ToolCall::SetActivity { text })
            }
            "ask_user" => {
                only(name, args, &["question"]).map_err(invalid)?;
                let question = required_string(name, args, "question").map_err(invalid)?.trim().to_string();
                if question.is_empty() {
                    return Err(invalid("The question is empty.".into()));
                }
                check_length("The question", &question, QUESTION_MAX).map_err(invalid)?;
                Ok(ToolCall::AskUser { question })
            }
            "note" => {
                only(name, args, &["text"]).map_err(invalid)?;
                let text = required_string(name, args, "text").map_err(invalid)?.trim().to_string();
                if text.is_empty() {
                    return Err(invalid("The note is empty.".into()));
                }
                check_length("The note", &text, NOTE_MAX).map_err(invalid)?;
                Ok(ToolCall::Note { text })
            }
            "request_capability" => {
                only(name, args, &["capability", "reason"]).map_err(invalid)?;
                let capability = required_string(name, args, "capability").map_err(invalid)?.trim().to_string();
                check_capability(&capability).map_err(invalid)?;
                let reason = required_string(name, args, "reason").map_err(invalid)?.trim().to_string();
                if reason.is_empty() {
                    return Err(invalid("Say in the reason why you need it.".into()));
                }
                check_length("The reason", &reason, REASON_MAX).map_err(invalid)?;
                Ok(ToolCall::RequestCapability { capability, reason })
            }
            name if name.starts_with("app") => apps::parse(name, args),
            "config_read" | "config_change" => config::parse(self.role, name, args),
            _ => social::parse(self.role, name, args),
        }
    }

    /// Checks a figure param and returns its value in the canonical JSON
    /// type. Strings such as `"true"` or `"3"` are accepted for toggles,
    /// numbers and counts, because models sometimes quote them.
    fn check_param(&self, name: &str, value: &Value) -> Result<Value, String> {
        let Some(params) = &self.params else {
            return match value {
                Value::Bool(_) | Value::Number(_) | Value::String(_) => Ok(value.clone()),
                _ => Err("The value must be true, false, a number or a string.".into()),
            };
        };
        if params.is_empty() {
            return Err("Your figure declares no params, so there is nothing to set.".into());
        }
        let Some(param) = params.iter().find(|p| p.name == name) else {
            let names: Vec<&str> = params.iter().map(|p| p.name.as_str()).collect();
            return Err(format!("Your figure has no param {}. Params: {}.", quote(name), names.join(", ")));
        };
        let wrong = || format!("{name} takes {}, not {value}.", expected(&param.kind));
        match &param.kind {
            ParamKind::Toggle { .. } => match value {
                Value::Bool(b) => Ok(Value::Bool(*b)),
                Value::String(s) if s == "true" || s == "false" => Ok(Value::Bool(s == "true")),
                _ => Err(wrong()),
            },
            ParamKind::Choice { values, .. } => match value {
                Value::String(s) if values.contains(s) => Ok(value.clone()),
                _ => Err(wrong()),
            },
            ParamKind::Number { min, max, .. } => {
                let n = number(value).filter(|n| n.is_finite() && (*min..=*max).contains(n)).ok_or_else(wrong)?;
                Ok(json!(n))
            }
            ParamKind::Count { .. } => {
                let n = number(value).filter(|n| n.fract() == 0.0 && (0.0..=f64::from(u32::MAX)).contains(n)).ok_or_else(wrong)?;
                Ok(json!(n as u32))
            }
            ParamKind::Text { .. } => match value {
                Value::String(s) if s.contains(['\n', '\r']) => Err(format!("{name} must be one line.")),
                Value::String(s) => {
                    check_length(name, s, TEXT_PARAM_MAX)?;
                    Ok(value.clone())
                }
                _ => Err(wrong()),
            },
        }
    }

    fn figure_set(&self) -> Value {
        const BASE: &str = "Sets one param of your own figure in the meerora space. Params switch parts or animations of the figure, \
                            so use them to show your state at a glance.";
        let (description, name, value) = match &self.params {
            None => (
                format!(
                    "{BASE} The params could not be read when this server started, so meerora checks the name and value when you call it."
                ),
                json!({ "type": "string", "description": "Name of the param." }),
                json!({ "description": "New value, which may be true, false, a number or a string." }),
            ),
            Some(params) if params.is_empty() => (
                "Your figure in the meerora space declares no params, so this tool has nothing to set.".to_string(),
                json!({ "type": "string", "description": "Name of the param. Your figure has none." }),
                json!({ "description": "New value." }),
            ),
            Some(params) => {
                let names: Vec<&str> = params.iter().map(|p| p.name.as_str()).collect();
                let lines: Vec<String> = params.iter().map(|p| format!("- {}: {}", p.name, describe(&p.kind))).collect();
                let mut value = json!({ "description": format!("New value, depending on the param:\n{}", lines.join("\n")) });
                if let Some(ty) = common_type(params) {
                    value["type"] = json!(ty);
                }
                (BASE.to_string(), json!({ "type": "string", "enum": names, "description": "Name of the param." }), value)
            }
        };
        tool("figure_set", "Set figure param", description, json!({ "name": name, "value": value }), &["name", "value"], true)
    }

    fn go_to(&self) -> Value {
        let description = "Walks your figure to a station in the meerora space. Go to the station that fits your current task, \
                           so the user sees what you are doing.";
        let station = match &self.stations {
            Some(ids) => json!({ "type": "string", "enum": ids, "description": "Id of the station." }),
            None => json!({
                "type": "string",
                "description": "Id of the station. The stations could not be read when this server started, so meerora checks the id.",
            }),
        };
        tool("go_to", "Go to station", description.into(), json!({ "station": station }), &["station"], true)
    }
}

fn set_activity() -> Value {
    let text = json!({
        "type": "string",
        "maxLength": ACTIVITY_MAX,
        "description": "A few words, for example \"checking mail\" or \"fixing wifi\".",
    });
    let description = "Sets the short line under your figure's name in the meerora space. Keep it current while you work. \
                       An empty text clears it.";
    tool("set_activity", "Set activity", description.into(), json!({ "text": text }), &["text"], true)
}

fn ask_user() -> Value {
    let question = json!({
        "type": "string",
        "maxLength": QUESTION_MAX,
        "description": "The question or the result to show, in one or two sentences.",
    });
    let description = "Asks the user a question and ends your turn waiting for the answer. Your figure comes to the front \
                       with a full ring. Use it when you need a decision or want to show a result. Stop working after this call.";
    tool("ask_user", "Ask the user", description.into(), json!({ "question": question }), &["question"], false)
}

fn note() -> Value {
    let text = json!({ "type": "string", "maxLength": NOTE_MAX, "description": "One or two sentences." });
    let description = "Posts a short progress note in your panel in meerora, so the user can follow your work. Notes need no answer.";
    tool("note", "Post a note", description.into(), json!({ "text": text }), &["text"], false)
}

/// Asks the user for a capability (D8). The call waits for the decision.
fn request_capability() -> Value {
    let capability = json!({
        "type": "string",
        "maxLength": CAPABILITY_MAX,
        "pattern": "^[a-z][a-z0-9_]*(\\.[a-z][a-z0-9_]*)*$",
        "description": "The capability: shell for commands in your directory, web for fetching pages and searching, or a dotted \
                        name such as network.airplane_mode.write.",
    });
    let reason = json!({
        "type": "string",
        "maxLength": REASON_MAX,
        "description": "Why the task needs it, in one sentence. The user reads it on the card.",
    });
    let description = "Asks the user for a capability you do not hold. The user sees your reason on a card and allows it once, \
                       always, or not at all. The call waits until they decide, which can take minutes. When they allow it, end \
                       your turn as the answer says, and meerora continues this conversation in a new run that has it.";
    tool(
        "request_capability",
        "Ask for a capability",
        description.into(),
        json!({ "capability": capability, "reason": reason }),
        &["capability", "reason"],
        false,
    )
}

/// Lowercase words of letters, digits and `_`, joined by dots. The daemon checks the same.
fn check_capability(name: &str) -> Result<(), String> {
    let word = |w: &str| {
        w.starts_with(|c: char| c.is_ascii_lowercase()) && w.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    };
    if name.len() <= CAPABILITY_MAX && name.split('.').all(word) {
        Ok(())
    } else {
        Err(format!(
            "{} is not a capability name. Use lowercase words joined by dots, for example web, shell or network.airplane_mode.write.",
            quote(name)
        ))
    }
}

fn tool(name: &str, title: &str, description: String, properties: Value, required: &[&str], idempotent: bool) -> Value {
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        },
        "annotations": {
            "title": title,
            "readOnlyHint": false,
            "destructiveHint": false,
            "idempotentHint": idempotent,
            "openWorldHint": false,
        },
    })
}

/// What a param takes, for the schema and for error messages.
fn expected(kind: &ParamKind) -> String {
    match kind {
        ParamKind::Toggle { .. } => "true or false".into(),
        ParamKind::Choice { values, .. } => {
            let quoted: Vec<String> = values.iter().map(|v| quote(v)).collect();
            format!("one of {}", quoted.join(", "))
        }
        ParamKind::Number { min, max, .. } => format!("a number from {min} to {max}"),
        ParamKind::Count { .. } => "a whole number of 0 or more".into(),
        ParamKind::Text { .. } => format!("a short text of at most {TEXT_PARAM_MAX} characters"),
    }
}

fn describe(kind: &ParamKind) -> String {
    let default = match kind {
        ParamKind::Toggle { default } => default.to_string(),
        ParamKind::Choice { default, .. } | ParamKind::Text { default } => quote(default),
        ParamKind::Number { default, .. } => default.to_string(),
        ParamKind::Count { default } => default.to_string(),
    };
    format!("{} (default {default})", expected(kind))
}

/// The JSON type shared by all params, if there is one.
fn common_type(params: &[ParamDef]) -> Option<&'static str> {
    let ty = |p: &ParamDef| match p.kind {
        ParamKind::Toggle { .. } => "boolean",
        ParamKind::Choice { .. } | ParamKind::Text { .. } => "string",
        ParamKind::Number { .. } => "number",
        ParamKind::Count { .. } => "integer",
    };
    let first = ty(params.first()?);
    let numeric = |t: &str| t == "number" || t == "integer";
    params.iter().map(ty).try_fold(first, |acc, t| match (acc, t) {
        _ if acc == t => Some(acc),
        _ if numeric(acc) && numeric(t) => Some("number"),
        _ => None,
    })
}

fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn quote(s: &str) -> String {
    serde_json::to_string(s).expect("strings always serialize")
}

fn only(tool: &str, args: &Map<String, Value>, allowed: &[&str]) -> Result<(), String> {
    match args.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(key) => Err(format!("{tool} has no argument {}. It takes: {}.", quote(key), allowed.join(", "))),
        None => Ok(()),
    }
}

fn required_string(tool: &str, args: &Map<String, Value>, key: &str) -> Result<String, String> {
    match args.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(other) => Err(format!("`{key}` must be a string, not {other}.")),
        None => Err(format!("{tool} needs `{key}`.")),
    }
}

fn check_length(what: &str, text: &str, max: usize) -> Result<(), String> {
    let len = text.chars().count();
    if len > max {
        return Err(format!("{what} may have at most {max} characters, this one has {len}."));
    }
    Ok(())
}

fn load_params(data_dir: &Path, agent: &str) -> Result<Vec<ParamDef>, String> {
    let dir = meerora_protocol::load_agent(&data_dir.join("agents").join(agent))?;
    let path = dir.figure_path();
    let source = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let figure =
        meerora_script::build_figure(&source, &dir.config.figure, &Limits::default()).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(figure.params)
}

fn load_stations(space: &Path) -> Result<Vec<String>, String> {
    let path = space.join("space.js");
    let source = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let space = meerora_script::build_space(&source, "space.js", &Limits::default()).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(space.stations.into_iter().map(|s| s.id).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{TestDir, write_data_dir};

    fn all_kinds() -> Tools {
        let params = vec![
            ParamDef { name: "letter".into(), kind: ParamKind::Toggle { default: false } },
            ParamDef {
                name: "mood".into(),
                kind: ParamKind::Choice { values: vec!["calm".into(), "busy".into()], default: "calm".into() },
            },
            ParamDef { name: "level".into(), kind: ParamKind::Number { min: 0.0, max: 1.0, default: 0.5 } },
            ParamDef { name: "birds".into(), kind: ParamKind::Count { default: 0 } },
            ParamDef { name: "sign".into(), kind: ParamKind::Text { default: "hi".into() } },
        ];
        Tools::new(Some(params), Some(vec!["desk".into(), "door".into()]))
    }

    fn args(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    fn set(tools: &Tools, name: &str, value: Value) -> Result<ToolCall, CallError> {
        tools.parse("figure_set", &args(json!({ "name": name, "value": value })))
    }

    fn invalid(r: Result<ToolCall, CallError>) -> String {
        match r {
            Err(CallError::Invalid(msg)) => msg,
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    fn def<'a>(defs: &'a [Value], name: &str) -> &'a Value {
        defs.iter().find(|d| d["name"] == name).unwrap_or_else(|| panic!("no tool {name}"))
    }

    #[test]
    fn schemas_come_from_the_figure_and_the_space() {
        let dir = TestDir::new("schemas");
        write_data_dir(&dir);
        let tools = Tools::load(&dir.0, "wren");
        let defs = tools.definitions();
        let names: Vec<&str> = defs.iter().map(|d| d["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "figure_set",
                "go_to",
                "set_activity",
                "ask_user",
                "note",
                "request_capability",
                "apps_list",
                "app_read",
                "app_screenshot",
                "app_act",
                "app_open"
            ]
        );
        let request = &def(&defs, "request_capability")["inputSchema"];
        assert_eq!(request["required"], json!(["capability", "reason"]));
        assert_eq!(request["properties"]["reason"]["maxLength"], REASON_MAX);

        let figure = &def(&defs, "figure_set")["inputSchema"];
        assert_eq!(figure["properties"]["name"]["enum"], json!(["letter"]));
        assert_eq!(figure["properties"]["value"]["type"], "boolean");
        assert!(figure["properties"]["value"]["description"].as_str().unwrap().contains("letter: true or false (default false)"));
        assert_eq!(figure["required"], json!(["name", "value"]));
        assert_eq!(figure["additionalProperties"], false);

        let go_to = &def(&defs, "go_to")["inputSchema"];
        assert_eq!(go_to["properties"]["station"]["enum"], json!(["desk", "door"]));

        assert_eq!(def(&defs, "set_activity")["inputSchema"]["properties"]["text"]["maxLength"], ACTIVITY_MAX);
        for d in &defs {
            assert_eq!(d["inputSchema"]["type"], "object");
            let description = d["description"].as_str().unwrap();
            assert!(!description.contains('\u{2014}') && !description.contains(';'), "{description}");
        }
    }

    #[test]
    fn every_param_kind_is_described() {
        let defs = all_kinds().definitions();
        let value = &def(&defs, "figure_set")["inputSchema"]["properties"]["value"];
        assert!(value.get("type").is_none(), "mixed kinds have no single type");
        let text = value["description"].as_str().unwrap();
        for line in [
            "- letter: true or false (default false)",
            "- mood: one of \"calm\", \"busy\" (default \"calm\")",
            "- level: a number from 0 to 1 (default 0.5)",
            "- birds: a whole number of 0 or more (default 0)",
            "- sign: a short text of at most 80 characters (default \"hi\")",
        ] {
            assert!(text.contains(line), "{line:?} missing in {text:?}");
        }
    }

    #[test]
    fn params_are_validated_and_normalised() {
        let tools = all_kinds();
        assert_eq!(set(&tools, "letter", json!(true)), Ok(ToolCall::FigureSet { name: "letter".into(), value: json!(true) }));
        assert_eq!(set(&tools, "letter", json!("false")), Ok(ToolCall::FigureSet { name: "letter".into(), value: json!(false) }));
        assert_eq!(set(&tools, "mood", json!("busy")), Ok(ToolCall::FigureSet { name: "mood".into(), value: json!("busy") }));
        assert_eq!(set(&tools, "level", json!("0.25")), Ok(ToolCall::FigureSet { name: "level".into(), value: json!(0.25) }));
        assert_eq!(set(&tools, "birds", json!(3.0)), Ok(ToolCall::FigureSet { name: "birds".into(), value: json!(3) }));
        assert_eq!(set(&tools, "sign", json!("open")), Ok(ToolCall::FigureSet { name: "sign".into(), value: json!("open") }));

        assert_eq!(invalid(set(&tools, "letter", json!("maybe"))), "letter takes true or false, not \"maybe\".");
        assert_eq!(invalid(set(&tools, "mood", json!("angry"))), "mood takes one of \"calm\", \"busy\", not \"angry\".");
        assert_eq!(invalid(set(&tools, "level", json!(3))), "level takes a number from 0 to 1, not 3.");
        assert!(invalid(set(&tools, "birds", json!(-1))).contains("a whole number of 0 or more"));
        assert!(invalid(set(&tools, "birds", json!(1.5))).contains("a whole number of 0 or more"));
        assert!(invalid(set(&tools, "sign", json!("x".repeat(81)))).contains("at most 80 characters"));
        assert_eq!(invalid(set(&tools, "hat", json!(true))), "Your figure has no param \"hat\". Params: letter, mood, level, birds, sign.");
        assert_eq!(invalid(tools.parse("figure_set", &args(json!({ "name": "letter" })))), "figure_set needs `value`.");
    }

    #[test]
    fn a_figure_without_params_still_has_the_tool() {
        let tools = Tools::new(Some(Vec::new()), None);
        let defs = tools.definitions();
        assert!(def(&defs, "figure_set")["description"].as_str().unwrap().contains("declares no params"));
        assert!(invalid(set(&tools, "letter", json!(true))).contains("declares no params"));
    }

    #[test]
    fn unknown_figure_and_space_accept_more() {
        let tools = Tools::new(None, None);
        let defs = tools.definitions();
        assert!(def(&defs, "figure_set")["inputSchema"]["properties"]["name"].get("enum").is_none());
        assert!(def(&defs, "go_to")["inputSchema"]["properties"]["station"].get("enum").is_none());
        assert_eq!(set(&tools, "anything", json!(2)), Ok(ToolCall::FigureSet { name: "anything".into(), value: json!(2) }));
        assert!(invalid(set(&tools, "anything", json!([1]))).contains("true, false, a number or a string"));
        assert_eq!(tools.parse("go_to", &args(json!({ "station": "moon" }))), Ok(ToolCall::GoTo { station: "moon".into() }));
    }

    #[test]
    fn broken_scripts_are_survived() {
        let dir = TestDir::new("broken");
        write_data_dir(&dir);
        dir.write("agents/wren/figure.js", "export default figure({ parts: [");
        dir.write("space/space.js", "throw new Error('nope')");
        assert_eq!(Tools::load(&dir.0, "wren"), Tools::new(None, None));
        assert_eq!(Tools::load(&dir.0, "nobody"), Tools::new(None, None));
    }

    #[test]
    fn other_tools_are_validated() {
        let tools = all_kinds();
        let call = |name: &str, v: Value| tools.parse(name, &args(v));
        assert_eq!(call("go_to", json!({ "station": "desk" })), Ok(ToolCall::GoTo { station: "desk".into() }));
        assert_eq!(invalid(call("go_to", json!({ "station": "moon" }))), "There is no station \"moon\". Stations: desk, door.");
        assert_eq!(invalid(call("go_to", json!({}))), "go_to needs `station`.");
        assert_eq!(invalid(call("go_to", json!({ "station": 3 }))), "`station` must be a string, not 3.");
        assert_eq!(invalid(call("go_to", json!({ "station": "desk", "speed": 2 }))), "go_to has no argument \"speed\". It takes: station.");

        assert_eq!(call("set_activity", json!({ "text": " checking mail " })), Ok(ToolCall::SetActivity { text: "checking mail".into() }));
        assert_eq!(call("set_activity", json!({ "text": "" })), Ok(ToolCall::SetActivity { text: String::new() }));
        assert_eq!(invalid(call("set_activity", json!({ "text": "a\nb" }))), "The activity must be one line.");
        assert!(invalid(call("set_activity", json!({ "text": "x".repeat(61) }))).contains("at most 60 characters"));

        assert_eq!(call("ask_user", json!({ "question": "Archive it?" })), Ok(ToolCall::AskUser { question: "Archive it?".into() }));
        assert_eq!(invalid(call("ask_user", json!({ "question": "  " }))), "The question is empty.");

        assert_eq!(call("note", json!({ "text": "hello" })), Ok(ToolCall::Note { text: "hello".into() }));
        assert_eq!(invalid(call("note", json!({ "text": "" }))), "The note is empty.");
        assert!(invalid(call("note", json!({ "text": "ä".repeat(501) }))).contains("at most 500 characters, this one has 501"));

        let ask = |capability: &str, reason: &str| call("request_capability", json!({ "capability": capability, "reason": reason }));
        assert_eq!(
            ask(" web ", " to read the docs "),
            Ok(ToolCall::RequestCapability { capability: "web".into(), reason: "to read the docs".into() })
        );
        assert!(ask("network.airplane_mode.write", "the plane").is_ok());
        assert!(invalid(ask("Web Access", "x")).contains("not a capability name"));
        assert!(invalid(ask("web", " ")).contains("why you need it"));
        assert!(invalid(ask("web", &"x".repeat(301))).contains("at most 300 characters"));
        assert!(invalid(call("request_capability", json!({ "capability": "web" }))).contains("needs `reason`"));

        assert_eq!(call("fly", json!({})), Err(CallError::UnknownTool));
    }
}
