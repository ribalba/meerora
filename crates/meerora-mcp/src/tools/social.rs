//! The tools of the personal and the news agent (D26, D28): `draft_post`
//! and `read_feed` for the personal agent, `read_feed` and `post_digest`
//! for the news agent. Other agents do not get them. The daemon checks the
//! role again, so a bridge started for the wrong agent gains nothing.

use meerora_protocol::{AgentRole, DigestItem, ToolCall, Visibility};
use serde_json::{Map, Value, json};

use super::{CallError, check_length, only, quote, required_string, tool};

/// Longest post, in characters, the same limit as the index.
pub const POST_MAX: usize = 1000;
/// Most items of one digest.
pub const DIGEST_ITEMS: usize = 12;

/// The extra tool definitions for an agent with `role`.
pub fn definitions(role: Option<AgentRole>) -> Vec<Value> {
    match role {
        Some(AgentRole::Personal) => vec![draft_post(), read_feed()],
        Some(AgentRole::News) => vec![read_feed(), post_digest()],
        Some(AgentRole::Helper) | None => Vec::new(),
    }
}

/// Validates a call of one of these tools.
pub fn parse(role: Option<AgentRole>, name: &str, args: &Map<String, Value>) -> Result<ToolCall, CallError> {
    let invalid = CallError::Invalid;
    match (name, role) {
        ("draft_post", Some(AgentRole::Personal)) => {
            only(name, args, &["text", "visibility"]).map_err(invalid)?;
            let text = required_string(name, args, "text").map_err(invalid)?.trim().to_string();
            if text.is_empty() {
                return Err(invalid("The post is empty.".into()));
            }
            check_length("The post", &text, POST_MAX).map_err(invalid)?;
            let visibility = match required_string(name, args, "visibility").map_err(invalid)?.as_str() {
                "public" => Visibility::Public,
                "friends" => Visibility::Friends,
                other => return Err(invalid(format!("visibility is public or friends, not {}.", quote(other)))),
            };
            Ok(ToolCall::DraftPost { text, visibility })
        }
        ("read_feed", Some(AgentRole::Personal | AgentRole::News)) => {
            only(name, args, &["limit"]).map_err(invalid)?;
            let limit = match args.get("limit") {
                None | Some(Value::Null) => None,
                Some(v) => match v.as_u64().filter(|n| (1..=50).contains(n)) {
                    Some(n) => Some(n as u32),
                    None => return Err(invalid(format!("limit is a whole number from 1 to 50, not {v}."))),
                },
            };
            Ok(ToolCall::ReadFeed { limit })
        }
        ("post_digest", Some(AgentRole::News)) => {
            only(name, args, &["title", "items"]).map_err(invalid)?;
            let title = required_string(name, args, "title").map_err(invalid)?.trim().to_string();
            if title.is_empty() {
                return Err(invalid("The digest needs a title.".into()));
            }
            check_length("The title", &title, 120).map_err(invalid)?;
            let Some(list) = args.get("items").and_then(Value::as_array) else {
                return Err(invalid("post_digest needs `items`, a list of objects with title, summary, url and source.".into()));
            };
            if list.is_empty() || list.len() > DIGEST_ITEMS {
                return Err(invalid(format!("A digest has 1 to {DIGEST_ITEMS} items, this one has {}.", list.len())));
            }
            let mut items = Vec::new();
            for (i, item) in list.iter().enumerate() {
                let n = i + 1;
                let Some(fields) = item.as_object() else {
                    return Err(invalid(format!("Item {n} must be an object.")));
                };
                only(&format!("item {n}"), fields, &["title", "summary", "url", "source"]).map_err(invalid)?;
                let text = |key: &str| fields.get(key).and_then(Value::as_str).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
                let item_title = text("title").ok_or_else(|| invalid(format!("Item {n} needs a title.")))?;
                check_length(&format!("The title of item {n}"), &item_title, 160).map_err(invalid)?;
                let summary = text("summary").unwrap_or_default();
                check_length(&format!("The summary of item {n}"), &summary, 600).map_err(invalid)?;
                let url = text("url");
                if let Some(url) = &url
                    && !(url.starts_with("https://") || url.starts_with("http://"))
                {
                    return Err(invalid(format!("The url of item {n} must start with https://.")));
                }
                items.push(DigestItem {
                    id: String::new(),
                    title: item_title,
                    summary,
                    url,
                    source: text("source"),
                    reaction: None,
                    opened: false,
                });
            }
            Ok(ToolCall::PostDigest { title, items })
        }
        _ => Err(CallError::UnknownTool),
    }
}

fn draft_post() -> Value {
    let text = json!({ "type": "string", "maxLength": POST_MAX, "description": "The post, in your owner's voice or about them, one to three short paragraphs." });
    let visibility = json!({
        "type": "string",
        "enum": ["public", "friends"],
        "description": "public for anyone who follows your owner, friends for the people they befriended.",
    });
    let description = "Drafts a post about your owner for their feed. It waits for their approval and goes out only when they \
                       approve it, maybe after editing it. Never use anything their profile marks private.";
    tool(
        "draft_post",
        "Draft a post",
        description.into(),
        json!({ "text": text, "visibility": visibility }),
        &["text", "visibility"],
        false,
    )
}

fn read_feed() -> Value {
    let limit = json!({ "type": "integer", "minimum": 1, "maximum": 50, "description": "How many posts, 15 when left out." });
    let description = "Reads the newest posts of the feed: the people and organisations the user follows and their friends, newest first.";
    tool("read_feed", "Read the feed", description.into(), json!({ "limit": limit }), &[], true)
}

fn post_digest() -> Value {
    let item = json!({
        "type": "object",
        "properties": {
            "title": { "type": "string", "maxLength": 160 },
            "summary": { "type": "string", "maxLength": 600, "description": "One or two sentences." },
            "url": { "type": "string", "description": "The link, starting with https://." },
            "source": { "type": "string", "description": "Where it comes from, for example the site's name or \"your feed\"." },
        },
        "required": ["title"],
        "additionalProperties": false,
    });
    let properties = json!({
        "title": { "type": "string", "maxLength": 120, "description": "For example \"Wednesday, 14 October\"." },
        "items": { "type": "array", "minItems": 1, "maxItems": DIGEST_ITEMS, "items": item },
    });
    let description = "Stores your digest on this computer and pins it at the top of the user's feed. Call it once per digest. \
                       The user rates the items, and the ratings land in memory/reactions.jsonl.";
    tool("post_digest", "Post the digest", description.into(), properties, &["title", "items"], false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn only_the_right_roles_get_the_tools() {
        let names = |role| definitions(role).iter().map(|d| d["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!(names(Some(AgentRole::Personal)), ["draft_post", "read_feed"]);
        assert_eq!(names(Some(AgentRole::News)), ["read_feed", "post_digest"]);
        assert!(names(None).is_empty());
        let post = args(json!({ "text": "Hi", "visibility": "public" }));
        assert_eq!(parse(None, "draft_post", &post), Err(CallError::UnknownTool));
        assert_eq!(parse(Some(AgentRole::News), "draft_post", &post), Err(CallError::UnknownTool));
        for d in definitions(Some(AgentRole::Personal)).iter().chain(&definitions(Some(AgentRole::News))) {
            let description = d["description"].as_str().unwrap();
            assert!(!description.contains('\u{2014}') && !description.contains(';'), "{description}");
        }
    }

    #[test]
    fn arguments_are_checked() {
        let personal = Some(AgentRole::Personal);
        assert_eq!(
            parse(personal, "draft_post", &args(json!({ "text": " Sailed today. ", "visibility": "friends" }))),
            Ok(ToolCall::DraftPost { text: "Sailed today.".into(), visibility: Visibility::Friends })
        );
        let private = parse(personal, "draft_post", &args(json!({ "text": "x", "visibility": "private" })));
        assert!(matches!(private, Err(CallError::Invalid(m)) if m.contains("public or friends")));
        let long = parse(personal, "draft_post", &args(json!({ "text": "x".repeat(1001), "visibility": "public" })));
        assert!(matches!(long, Err(CallError::Invalid(m)) if m.contains("at most 1000")));
        assert_eq!(parse(personal, "read_feed", &args(json!({}))), Ok(ToolCall::ReadFeed { limit: None }));
        assert!(matches!(parse(personal, "read_feed", &args(json!({ "limit": 0 }))), Err(CallError::Invalid(_))));

        let news = Some(AgentRole::News);
        let digest = args(
            json!({ "title": "Wednesday", "items": [{ "title": "Ferry strike", "summary": "No boats.", "url": "https://news.example/a", "source": "News" }] }),
        );
        let Ok(ToolCall::PostDigest { title, items }) = parse(news, "post_digest", &digest) else { panic!() };
        assert_eq!(
            (title.as_str(), items[0].title.as_str(), items[0].url.as_deref()),
            ("Wednesday", "Ferry strike", Some("https://news.example/a"))
        );
        let bad = args(json!({ "title": "W", "items": [{ "title": "A", "url": "ftp://x" }] }));
        assert!(matches!(parse(news, "post_digest", &bad), Err(CallError::Invalid(m)) if m.contains("https://")));
        let empty = args(json!({ "title": "W", "items": [] }));
        assert!(matches!(parse(news, "post_digest", &empty), Err(CallError::Invalid(m)) if m.contains("1 to 12")));
    }
}
