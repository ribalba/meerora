//! Chrome DevTools Protocol messages: encoding of commands, decoding of
//! responses and events, and the parameters of the commands the worker
//! sends. Pure functions, so they can be tested without Chrome.
//!
//! The worker talks to the browser endpoint and reaches the page through a
//! flat session (`Target.attachToTarget` with `flatten`), so page commands
//! carry a `sessionId` next to `id`, `method` and `params`.

use base64::Engine as _;
use serde_json::{Value, json};

use crate::{Button, Key, Mods, MouseEvent, MouseKind, Size};

/// A command as JSON text.
pub fn encode(id: u64, method: &str, params: Value, session: Option<&str>) -> String {
    let mut msg = json!({ "id": id, "method": method, "params": params });
    if let Some(s) = session {
        msg["sessionId"] = Value::String(s.to_string());
    }
    msg.to_string()
}

/// One message from Chrome.
#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    /// The answer to the command with the same `id`. The error is CDP's message.
    Response {
        id: u64,
        result: Result<Value, String>,
        session: Option<String>,
    },
    Event {
        method: String,
        params: Value,
        session: Option<String>,
    },
}

pub fn decode(text: &str) -> Result<Message, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("not JSON ({e})"))?;
    let session = v.get("sessionId").and_then(Value::as_str).map(str::to_string);
    if let Some(id) = v.get("id").and_then(Value::as_u64) {
        let result = match v.get("error") {
            Some(err) => Err(err.get("message").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| err.to_string())),
            None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
        };
        return Ok(Message::Response { id, result, session });
    }
    match v.get("method").and_then(Value::as_str) {
        Some(method) => Ok(Message::Event { method: method.to_string(), params: v.get("params").cloned().unwrap_or(Value::Null), session }),
        None => Err("neither a response nor an event".into()),
    }
}

/// The WebSocket URL in Chrome's `DevTools listening on ws://...` line.
pub fn devtools_url(line: &str) -> Option<&str> {
    let rest = line.trim().strip_prefix("DevTools listening on ")?;
    rest.starts_with("ws://").then_some(rest.trim())
}

/// `host:port` of a `ws://` URL.
pub fn ws_address(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("ws://")?;
    let host = rest.split('/').next()?;
    host.contains(':').then_some(host)
}

/// A decoded `Page.screencastFrame` event.
#[derive(Clone, Debug, PartialEq)]
pub struct ScreencastFrame {
    /// Goes back in `Page.screencastFrameAck`. Chrome sends no more frames
    /// until the last one is acknowledged.
    pub ack: i64,
    pub jpeg: Vec<u8>,
    pub css_width: f32,
    pub css_height: f32,
}

pub fn screencast_frame(params: &Value) -> Result<ScreencastFrame, String> {
    let ack = params.get("sessionId").and_then(Value::as_i64).ok_or("screencast frame without sessionId")?;
    let data = params.get("data").and_then(Value::as_str).ok_or("screencast frame without data")?;
    let jpeg = base64::engine::general_purpose::STANDARD.decode(data).map_err(|e| format!("screencast frame is not base64 ({e})"))?;
    let meta = params.get("metadata").cloned().unwrap_or(Value::Null);
    let num = |k: &str| meta.get(k).and_then(Value::as_f64).unwrap_or(0.0) as f32;
    Ok(ScreencastFrame { ack, jpeg, css_width: num("deviceWidth"), css_height: num("deviceHeight") })
}

/// CDP's modifier bit mask.
pub fn modifiers(m: Mods) -> u32 {
    (m.alt as u32) | (m.ctrl as u32) << 1 | (m.meta as u32) << 2 | (m.shift as u32) << 3
}

fn button_name(b: Button) -> &'static str {
    match b {
        Button::None => "none",
        Button::Left => "left",
        Button::Middle => "middle",
        Button::Right => "right",
    }
}

fn buttons_mask(b: Button) -> u32 {
    match b {
        Button::None => 0,
        Button::Left => 1,
        Button::Right => 2,
        Button::Middle => 4,
    }
}

/// Parameters of `Input.dispatchMouseEvent`.
pub fn mouse_params(e: &MouseEvent) -> Value {
    let kind = match e.kind {
        MouseKind::Move => "mouseMoved",
        MouseKind::Down => "mousePressed",
        MouseKind::Up => "mouseReleased",
        MouseKind::Wheel => "mouseWheel",
    };
    let mut p = json!({ "type": kind, "x": e.x, "y": e.y, "modifiers": modifiers(e.mods) });
    match e.kind {
        MouseKind::Wheel => {
            p["deltaX"] = json!(e.dx);
            p["deltaY"] = json!(e.dy);
        }
        MouseKind::Down | MouseKind::Up => {
            p["button"] = json!(button_name(e.button));
            p["clickCount"] = json!(e.clicks.max(1));
            p["buttons"] = json!(if e.kind == MouseKind::Down { buttons_mask(e.button) } else { 0 });
        }
        MouseKind::Move => {
            p["button"] = json!(button_name(e.button));
            p["buttons"] = json!(buttons_mask(e.button));
        }
    }
    p
}

/// DOM `key`, DOM `code`, Windows virtual key code and the text it types.
fn key_spec(key: Key) -> Option<(String, String, u32, Option<&'static str>)> {
    let named = |k: &str, vk: u32, text: Option<&'static str>| Some((k.to_string(), k.to_string(), vk, text));
    match key {
        Key::Enter => named("Enter", 13, Some("\r")),
        Key::Backspace => named("Backspace", 8, None),
        Key::Tab => named("Tab", 9, None),
        Key::Escape => named("Escape", 27, None),
        Key::Space => Some((" ".into(), "Space".into(), 32, Some(" "))),
        Key::Left => named("ArrowLeft", 37, None),
        Key::Up => named("ArrowUp", 38, None),
        Key::Right => named("ArrowRight", 39, None),
        Key::Down => named("ArrowDown", 40, None),
        Key::Home => named("Home", 36, None),
        Key::End => named("End", 35, None),
        Key::PageUp => named("PageUp", 33, None),
        Key::PageDown => named("PageDown", 34, None),
        Key::Insert => named("Insert", 45, None),
        Key::Delete => named("Delete", 46, None),
        Key::F(n @ 1..=12) => {
            let name = format!("F{n}");
            Some((name.clone(), name, 111 + n as u32, None))
        }
        Key::F(_) => None,
        Key::Char(c) if c.is_ascii_alphabetic() => {
            let upper = c.to_ascii_uppercase();
            Some((c.to_ascii_lowercase().to_string(), format!("Key{upper}"), upper as u32, None))
        }
        Key::Char(c) if c.is_ascii_digit() => Some((c.to_string(), format!("Digit{c}"), c as u32, None)),
        Key::Char(_) => None,
    }
}

/// Parameters of the `Input.dispatchKeyEvent` calls for pressing and
/// releasing `key`. Keys that type text (Enter, Space) send `keyDown` with
/// the text, the others `rawKeyDown`, as Chrome expects.
pub fn key_events(key: Key, mods: Mods) -> Vec<Value> {
    let Some((name, code, vk, text)) = key_spec(key) else { return Vec::new() };
    let m = modifiers(mods);
    let text = text.filter(|_| !mods.ctrl && !mods.alt && !mods.meta);
    let mut down = json!({
        "type": if text.is_some() { "keyDown" } else { "rawKeyDown" },
        "key": name, "code": code, "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk, "modifiers": m,
    });
    if let Some(t) = text {
        down["text"] = json!(t);
        down["unmodifiedText"] = json!(t);
    }
    let up = json!({ "type": "keyUp", "key": name, "code": code, "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk, "modifiers": m });
    vec![down, up]
}

/// Parameters of `Emulation.setDeviceMetricsOverride`.
pub fn device_metrics(size: Size, scale: f32) -> Value {
    json!({ "width": size.width, "height": size.height, "deviceScaleFactor": scale, "mobile": false })
}

/// Parameters of `Page.startScreencast`. Frames come at device resolution.
pub fn screencast_params(size: Size, scale: f32) -> Value {
    let px = |v: u32| ((v as f32 * scale).round() as u32).max(1);
    json!({ "format": "jpeg", "quality": 80, "maxWidth": px(size.width), "maxHeight": px(size.height), "everyNthFrame": 1 })
}

/// Parameters of `Target.createTarget` for a page in a window of its own,
/// `size` in CSS pixels. A tab in the window of another page would be a
/// hidden background tab, and hidden tabs paint no screencast frames.
pub fn create_target_params(url: &str, size: Size) -> Value {
    json!({ "url": url, "newWindow": true, "background": false, "width": size.width, "height": size.height })
}

/// From a `Page.getNavigationHistory` result: the entry `step` away from
/// the current one, and whether back and forward are possible.
pub fn history(result: &Value, step: i64) -> (Option<i64>, bool, bool) {
    let entries = result.get("entries").and_then(Value::as_array).cloned().unwrap_or_default();
    let current = result.get("currentIndex").and_then(Value::as_i64).unwrap_or(0);
    let target = current + step;
    let entry = (step != 0 && target >= 0 && (target as usize) < entries.len())
        .then(|| entries[target as usize].get("id").and_then(Value::as_i64))
        .flatten();
    (entry, current > 0, (current + 1) < entries.len() as i64)
}

/// Title of the current entry of a `Page.getNavigationHistory` result.
pub fn history_title(result: &Value) -> Option<String> {
    let current = result.get("currentIndex").and_then(Value::as_u64)? as usize;
    let title = result.get("entries")?.get(current)?.get("title")?.as_str()?;
    (!title.is_empty()).then(|| title.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_commands_with_and_without_session() {
        let plain: Value = serde_json::from_str(&encode(1, "Target.getTargets", json!({}), None)).unwrap();
        assert_eq!(plain, json!({ "id": 1, "method": "Target.getTargets", "params": {} }));
        let paged: Value = serde_json::from_str(&encode(7, "Page.navigate", json!({ "url": "data:," }), Some("S1"))).unwrap();
        assert_eq!(paged, json!({ "id": 7, "method": "Page.navigate", "params": { "url": "data:," }, "sessionId": "S1" }));
    }

    #[test]
    fn decodes_responses_errors_and_events() {
        assert_eq!(
            decode(r#"{"id":3,"result":{"sessionId":"AB"}}"#).unwrap(),
            Message::Response { id: 3, result: Ok(json!({ "sessionId": "AB" })), session: None }
        );
        assert_eq!(
            decode(r#"{"id":4,"error":{"code":-32000,"message":"No target with given id"},"sessionId":"AB"}"#).unwrap(),
            Message::Response { id: 4, result: Err("No target with given id".into()), session: Some("AB".into()) }
        );
        assert_eq!(
            decode(r#"{"method":"Page.loadEventFired","params":{"timestamp":1.5},"sessionId":"AB"}"#).unwrap(),
            Message::Event { method: "Page.loadEventFired".into(), params: json!({ "timestamp": 1.5 }), session: Some("AB".into()) }
        );
        assert!(decode("{}").is_err());
        assert!(decode("not json").is_err());
    }

    #[test]
    fn finds_the_devtools_url() {
        let line = "DevTools listening on ws://127.0.0.1:38695/devtools/browser/96b7e1ec-a417\n";
        assert_eq!(devtools_url(line), Some("ws://127.0.0.1:38695/devtools/browser/96b7e1ec-a417"));
        assert_eq!(devtools_url("[1:2:ERROR:gcm] Registration response error"), None);
        assert_eq!(ws_address("ws://127.0.0.1:38695/devtools/browser/x"), Some("127.0.0.1:38695"));
        assert_eq!(ws_address("http://127.0.0.1:1/"), None);
    }

    #[test]
    fn decodes_screencast_frames() {
        let data = base64::engine::general_purpose::STANDARD.encode([0xFF, 0xD8, 0xFF, 0xD9]);
        let params = json!({ "data": data, "sessionId": 5, "metadata": { "deviceWidth": 640, "deviceHeight": 400.5, "offsetTop": 0 } });
        let f = screencast_frame(&params).unwrap();
        assert_eq!(f, ScreencastFrame { ack: 5, jpeg: vec![0xFF, 0xD8, 0xFF, 0xD9], css_width: 640.0, css_height: 400.5 });
        assert!(screencast_frame(&json!({ "data": "%%%", "sessionId": 1 })).is_err());
        assert!(screencast_frame(&json!({ "data": data })).is_err());
    }

    #[test]
    fn encodes_mouse_events() {
        let mut down = MouseEvent::new(MouseKind::Down, 10.0, 20.5);
        down.button = Button::Left;
        down.clicks = 2;
        down.mods.shift = true;
        assert_eq!(
            mouse_params(&down),
            json!({ "type": "mousePressed", "x": 10.0, "y": 20.5, "modifiers": 8, "button": "left", "clickCount": 2, "buttons": 1 })
        );
        let mut wheel = MouseEvent::new(MouseKind::Wheel, 1.0, 2.0);
        wheel.dy = 120.0;
        assert_eq!(
            mouse_params(&wheel),
            json!({ "type": "mouseWheel", "x": 1.0, "y": 2.0, "modifiers": 0, "deltaX": 0.0, "deltaY": 120.0 })
        );
        let mv = MouseEvent::new(MouseKind::Move, 3.0, 4.0);
        assert_eq!(mouse_params(&mv)["type"], "mouseMoved");
        assert_eq!(mouse_params(&mv)["buttons"], 0);
    }

    #[test]
    fn encodes_keys() {
        let enter = key_events(Key::Enter, Mods::default());
        assert_eq!(enter[0]["type"], "keyDown");
        assert_eq!(enter[0]["text"], "\r");
        assert_eq!(enter[0]["windowsVirtualKeyCode"], 13);
        assert_eq!(enter[1]["type"], "keyUp");

        let back = key_events(Key::Backspace, Mods::default());
        assert_eq!(back[0]["type"], "rawKeyDown");
        assert_eq!(back[0]["key"], "Backspace");
        assert!(back[0].get("text").is_none());

        let ctrl = Mods { ctrl: true, ..Default::default() };
        let select_all = key_events(Key::Char('A'), ctrl);
        assert_eq!(select_all[0]["key"], "a");
        assert_eq!(select_all[0]["code"], "KeyA");
        assert_eq!(select_all[0]["windowsVirtualKeyCode"], 65);
        assert_eq!(select_all[0]["modifiers"], 2);
        assert_eq!(key_events(Key::F(5), Mods::default())[0]["windowsVirtualKeyCode"], 116);
        assert!(key_events(Key::F(20), Mods::default()).is_empty());
        assert!(key_events(Key::Char('é'), ctrl).is_empty());
        assert_eq!(modifiers(Mods { alt: true, ctrl: true, meta: true, shift: true }), 15);
    }

    #[test]
    fn sizes_and_history() {
        let size = Size { width: 400, height: 300 };
        assert_eq!(screencast_params(size, 2.0)["maxWidth"], 800);
        assert_eq!(device_metrics(size, 1.5)["deviceScaleFactor"], 1.5);
        assert_eq!(
            create_target_params("about:blank", size),
            json!({ "url": "about:blank", "newWindow": true, "background": false, "width": 400, "height": 300 })
        );
        let h = json!({ "currentIndex": 1, "entries": [{ "id": 10 }, { "id": 11 }, { "id": 12 }] });
        assert_eq!(history(&h, -1), (Some(10), true, true));
        assert_eq!(history(&h, 1), (Some(12), true, true));
        assert_eq!(history(&json!({ "currentIndex": 0, "entries": [{ "id": 1 }] }), -1), (None, false, false));
        assert_eq!(history(&h, 0), (None, true, true));
        let titled = json!({ "currentIndex": 1, "entries": [{ "id": 1, "title": "a" }, { "id": 2, "title": "Report" }] });
        assert_eq!(history_title(&titled).as_deref(), Some("Report"));
        assert_eq!(history_title(&h), None);
    }
}
