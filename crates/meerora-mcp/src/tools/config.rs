//! The tools of meerora's helper (`role = "helper"`): `config_read` and
//! `config_change`. Other agents do not get them. The daemon checks the role
//! again and checks every proposal the way meerora reads the file
//! (config_tools.rs of meerora-agentd), so a bridge started for the wrong
//! agent gains nothing.

use meerora_protocol::{AgentRole, ToolCall};
use serde_json::{Map, Value, json};

use super::{CallError, check_length, only, quote, required_string, tool};

/// What `config_read` reads.
pub const READ_TARGETS: [&str; 7] = ["meerora.toml", "agents", "rooms", "commands", "audit", "check", "readme"];
/// Most bytes of a proposed file or figure script, as the daemon allows.
pub const CONTENT_MAX: usize = 64 * 1024;
/// Longest reason, in characters.
pub const REASON_MAX: usize = 300;

/// The extra tool definitions for an agent with `role`.
pub fn definitions(role: Option<AgentRole>) -> Vec<Value> {
    match role {
        Some(AgentRole::Helper) => vec![config_read(), config_change()],
        _ => Vec::new(),
    }
}

/// Validates a call of one of these tools.
pub fn parse(role: Option<AgentRole>, name: &str, args: &Map<String, Value>) -> Result<ToolCall, CallError> {
    let invalid = CallError::Invalid;
    match (name, role) {
        ("config_read", Some(AgentRole::Helper)) => {
            only(name, args, &["target"]).map_err(invalid)?;
            let target = required_string(name, args, "target").map_err(invalid)?.trim().to_string();
            let prefixed = ["agent:", "readme:"].iter().any(|p| target.strip_prefix(p).is_some_and(|rest| !rest.trim().is_empty()));
            if !prefixed && !READ_TARGETS.contains(&target.as_str()) {
                return Err(invalid(format!(
                    "{} is not a target. Use {}, agent:<id> for one agent.toml or readme:<words> for sections of the manual.",
                    quote(&target),
                    READ_TARGETS.join(", ")
                )));
            }
            Ok(ToolCall::ConfigRead { target })
        }
        ("config_change", Some(AgentRole::Helper)) => {
            only(name, args, &["target", "content", "reason", "figure"]).map_err(invalid)?;
            let target = required_string(name, args, "target").map_err(invalid)?.trim().to_string();
            let known = matches!(target.as_str(), "meerora.toml" | "commands.toml")
                || ["agent:", "new_agent:"].iter().any(|p| target.strip_prefix(p).is_some_and(|id| !id.trim().is_empty()));
            if !known {
                return Err(invalid(format!(
                    "{} cannot be changed. Use meerora.toml, commands.toml, agent:<id> or new_agent:<id>.",
                    quote(&target)
                )));
            }
            let content = required_string(name, args, "content").map_err(invalid)?;
            if content.len() > CONTENT_MAX {
                return Err(invalid(format!("The content may have at most {CONTENT_MAX} bytes, this one has {}.", content.len())));
            }
            let reason = required_string(name, args, "reason").map_err(invalid)?.trim().to_string();
            if reason.is_empty() {
                return Err(invalid("Say in the reason why the user wants this change.".into()));
            }
            check_length("The reason", &reason, REASON_MAX).map_err(invalid)?;
            let figure = match args.get("figure") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) if s.len() > CONTENT_MAX => {
                    return Err(invalid(format!("The figure may have at most {CONTENT_MAX} bytes.")));
                }
                Some(Value::String(s)) => Some(s.clone()),
                Some(other) => return Err(invalid(format!("`figure` must be a string, not {other}."))),
            };
            if figure.is_some() && !target.starts_with("new_agent:") {
                return Err(invalid("figure is only for new_agent:<id>.".into()));
            }
            Ok(ToolCall::ConfigChange { target, content, reason, figure })
        }
        _ => Err(CallError::UnknownTool),
    }
}

fn config_read() -> Value {
    let target = json!({
        "type": "string",
        "description": "meerora.toml (the settings), agents (every agent with runtime, role and capabilities), agent:<id> (that \
                        agent's agent.toml), rooms (the current room, its stations and devices), commands (commands.toml of the \
                        command box), audit (the last 50 lines of the audit log), check (what meerora-agentd check reports), readme (the sections of \
                        meerora's manual) or readme:<words> (the sections that match).",
    });
    let description = "Reads meerora's configuration as text. Read a file before you propose a change of it. Secrets such as \
                       login tokens are never shown.";
    tool("config_read", "Read the configuration", description.into(), json!({ "target": target }), &["target"], true)
}

fn config_change() -> Value {
    let properties = json!({
        "target": {
            "type": "string",
            "description": "meerora.toml, commands.toml, agent:<id> for an agent's agent.toml, or new_agent:<id> for a new agent \
                            with a lowercase id.",
        },
        "content": {
            "type": "string",
            "maxLength": CONTENT_MAX,
            "description": "The whole new file, comments included. Keep every line the user did not ask to change.",
        },
        "reason": {
            "type": "string",
            "maxLength": REASON_MAX,
            "description": "Why, in one sentence. The user reads it on the card above the diff.",
        },
        "figure": {
            "type": "string",
            "description": "Only for new_agent: its figure script (figure.js), written against the script API. A plain figure \
                            when left out.",
        },
    });
    let description = "Proposes a change of meerora's configuration. meerora checks it and shows the user a diff with Apply and \
                       Discard, and nothing is written before they apply it. The call waits until they decide, which can take \
                       minutes. When the check fails, the answer says why, so fix that and call it again.";
    tool("config_change", "Propose a configuration change", description.into(), properties, &["target", "content", "reason"], false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn only_the_helper_gets_the_tools() {
        let names = |role| definitions(role).iter().map(|d| d["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!(names(Some(AgentRole::Helper)), ["config_read", "config_change"]);
        for role in [None, Some(AgentRole::Personal), Some(AgentRole::News)] {
            assert!(names(role).is_empty(), "{role:?}");
            assert_eq!(parse(role, "config_read", &args(json!({ "target": "agents" }))), Err(CallError::UnknownTool));
            let change = args(json!({ "target": "meerora.toml", "content": "", "reason": "x" }));
            assert_eq!(parse(role, "config_change", &change), Err(CallError::UnknownTool));
        }
        for d in definitions(Some(AgentRole::Helper)) {
            let description = d["description"].as_str().unwrap();
            assert!(!description.contains('\u{2014}') && !description.contains(';'), "{description}");
        }
    }

    #[test]
    fn the_bridge_of_the_helper_lists_them_and_others_refuse_them() {
        use crate::testing::{FIGURE, TestDir};
        use crate::tools::Tools;
        let dir = TestDir::new("config-tools");
        dir.write("agents/rivet/agent.toml", "name = \"Rivet\"\nhome = \"desk\"\nrole = \"helper\"\n");
        dir.write("agents/rivet/figure.js", FIGURE);
        dir.write("agents/wren/agent.toml", "name = \"Wren\"\nhome = \"desk\"\n");
        let read = args(json!({ "target": "agents" }));
        let helper = Tools::load(&dir.0, "rivet");
        let names: Vec<String> = helper.definitions().iter().map(|d| d["name"].as_str().unwrap().to_string()).collect();
        assert!(names.iter().any(|n| n == "config_read") && names.iter().any(|n| n == "config_change"), "{names:?}");
        assert_eq!(helper.parse("config_read", &read), Ok(ToolCall::ConfigRead { target: "agents".into() }));
        let wren = Tools::load(&dir.0, "wren");
        assert!(!wren.definitions().iter().any(|d| d["name"].as_str().unwrap().starts_with("config_")));
        assert_eq!(wren.parse("config_read", &read), Err(CallError::UnknownTool));
    }

    #[test]
    fn arguments_are_checked() {
        let helper = Some(AgentRole::Helper);
        let read = |target: &str| parse(helper, "config_read", &args(json!({ "target": target })));
        assert_eq!(read(" agents "), Ok(ToolCall::ConfigRead { target: "agents".into() }));
        assert_eq!(read("agent:claude"), Ok(ToolCall::ConfigRead { target: "agent:claude".into() }));
        assert_eq!(read("readme:command box"), Ok(ToolCall::ConfigRead { target: "readme:command box".into() }));
        assert!(matches!(read("runtimes/claude"), Err(CallError::Invalid(m)) if m.contains("not a target")));
        assert!(matches!(read("agent:"), Err(CallError::Invalid(_))));

        let change = |v: Value| parse(helper, "config_change", &args(v));
        assert_eq!(
            change(json!({ "target": "meerora.toml", "content": "[commands]\nkey = \"Ctrl+Space\"\n", "reason": " Faster. " })),
            Ok(ToolCall::ConfigChange {
                target: "meerora.toml".into(),
                content: "[commands]\nkey = \"Ctrl+Space\"\n".into(),
                reason: "Faster.".into(),
                figure: None
            })
        );
        let bad = |v: Value, needle: &str| match change(v) {
            Err(CallError::Invalid(m)) => assert!(m.contains(needle), "{m}"),
            other => panic!("expected {needle:?}, got {other:?}"),
        };
        bad(json!({ "target": "index-account.json", "content": "", "reason": "x" }), "cannot be changed");
        bad(json!({ "target": "meerora.toml", "content": "", "reason": " " }), "reason");
        bad(json!({ "target": "meerora.toml", "content": "", "reason": "x", "figure": "f" }), "only for new_agent");
        bad(json!({ "target": "meerora.toml", "content": "x".repeat(CONTENT_MAX + 1), "reason": "x" }), "at most");
        bad(json!({ "target": "meerora.toml", "reason": "x" }), "needs `content`");
        assert!(change(json!({ "target": "new_agent:mail", "content": "", "reason": "x", "figure": "f" })).is_ok());
    }
}
