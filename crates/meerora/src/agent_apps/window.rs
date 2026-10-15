//! A real app window as agents use it (D30): read returns its title, app id
//! and whether it runs in the agent's sandbox, the screenshot is the
//! compositor's image of it, and click, type, key and scroll are raw input
//! through the nested compositor, which needs `apps.input`. Its text and
//! elements come later with AT-SPI, so until then the agent looks at it
//! with `app_screenshot`.
//!
//! Coordinates are points from the top left of the window's image, the same
//! as the pixels of its screenshot. Typing maps characters to evdev codes
//! of the US layout and refuses what that layout cannot type. The agent's
//! input does not keep the keyboard: the window that had it before gets it
//! back.

use serde_json::{Map, Value, json};

use super::{Acted, AgentApp, AppText, Reply, encode_png, keys, number_arg, text_arg};
use crate::engine::Core;
use crate::ui::AppTab;

/// Longest `type` text, in characters.
const TYPE_MAX: usize = 2000;
/// The widest screenshot, in pixels.
const SHOT_MAX: u32 = 1600;
/// Linux evdev code of the left Shift key.
const KEY_LEFTSHIFT: u32 = 42;

pub struct WindowApp {
    pub agent: String,
    pub id: u32,
}

impl WindowApp {
    fn handle(&self) -> String {
        format!("window:{}", self.id)
    }

    /// The size of the window's image in points, None before it drew.
    fn size(&self, core: &mut Core) -> Option<(f32, f32)> {
        let frame = core.desktops.compositor()?.frame(self.id)?;
        Some((frame.width as f32 / frame.scale, frame.height as f32 / frame.scale))
    }

    /// Presses and releases keys on the window, then gives the keyboard
    /// back to the window that had it.
    fn keys(&self, core: &mut Core, presses: &[Vec<u32>]) {
        let before = core.desktops.focused();
        if let Some(c) = core.desktops.compositor() {
            for codes in presses {
                for code in codes {
                    c.key(self.id, *code, true);
                }
                for code in codes.iter().rev() {
                    c.key(self.id, *code, false);
                }
            }
        }
        if before != Some(self.id) {
            core.desktops.focus(before);
        }
    }
}

impl AgentApp for WindowApp {
    fn kind(&self) -> &'static str {
        "window"
    }

    fn actions(&self) -> &'static [&'static str] {
        &["click", "type", "key", "scroll"]
    }

    fn read(&mut self, core: &mut Core) -> Reply<AppText> {
        let handle = self.handle();
        let Some(w) = core.desktops.window(self.id) else { return Reply::err(format!("{handle} is closed.")) };
        let size = self.size(core);
        let mut extra = Map::new();
        extra.insert("app_id".into(), json!(w.app_id));
        extra.insert("sandboxed".into(), json!(w.sandboxed));
        extra.insert("x11".into(), json!(w.x11));
        if let Some((width, height)) = size {
            extra.insert("size".into(), json!(format!("{} by {} points", width.round(), height.round())));
        }
        let owner = if w.sandboxed { "started by you, in your sandbox" } else { "started by the user" };
        let summary = format!(
            "A real app window ({owner}). Its text cannot be read yet, so look at it with app_screenshot and act with click {{x, y}}, \
             type {{text}}, key {{keys}} and scroll {{dy}} in points of the screenshot."
        );
        let title = if w.title.is_empty() { w.app_id.clone() } else { w.title.clone() };
        Reply::ok(AppText { kind: "window".into(), title, summary, extra, ..Default::default() })
    }

    fn act(&mut self, core: &mut Core, action: &str, args: &Map<String, Value>) -> Reply<Acted> {
        let handle = self.handle();
        if core.desktops.window(self.id).is_none() {
            return Reply::err(format!("{handle} is closed."));
        }
        let Some((width, height)) = self.size(core) else {
            return Reply::err(format!("{handle} has not drawn anything yet. Try again in a moment."));
        };
        match action {
            "click" | "scroll" => {
                let default = if action == "scroll" { (Some(f64::from(width) / 2.0), Some(f64::from(height) / 2.0)) } else { (None, None) };
                let x = match number_arg(args, "x", action, default.0) {
                    Ok(v) => v as f32,
                    Err(e) => return Reply::err(e),
                };
                let y = match number_arg(args, "y", action, default.1) {
                    Ok(v) => v as f32,
                    Err(e) => return Reply::err(e),
                };
                if !(0.0..width).contains(&x) || !(0.0..height).contains(&y) {
                    return Reply::err(format!("{x}, {y} is outside {handle}, which is {} by {} points.", width.round(), height.round()));
                }
                let before = core.desktops.focused();
                if action == "click" {
                    let button = args.get("button").and_then(Value::as_str).unwrap_or("left");
                    let code = match button {
                        "left" => meerora_compositor::BTN_LEFT,
                        "right" => meerora_compositor::BTN_RIGHT,
                        "middle" => meerora_compositor::BTN_MIDDLE,
                        other => return Reply::err(format!("button is left, right or middle, not {other:?}.")),
                    };
                    if let Some(c) = core.desktops.compositor() {
                        c.pointer_motion(self.id, f64::from(x), f64::from(y));
                        c.pointer_button(self.id, code, true);
                        c.pointer_button(self.id, code, false);
                    }
                    if before != Some(self.id) {
                        core.desktops.focus(before);
                    }
                    let now = core.world.time;
                    core.agent_apps.clicks.insert((self.agent.clone(), self.id), (x, y, now));
                    let which = if button == "left" { String::new() } else { format!("{button} ") };
                    Reply::ok(Acted::new(format!("Clicked {which}at {x}, {y} in {handle}."), format!("{x}, {y}")))
                } else {
                    let dx = number_arg(args, "dx", "scroll", Some(0.0)).unwrap_or(0.0);
                    let dy = match number_arg(args, "dy", "scroll", Some(0.0)) {
                        Ok(v) => v,
                        Err(e) => return Reply::err(e),
                    };
                    if let Some(c) = core.desktops.compositor() {
                        c.pointer_motion(self.id, f64::from(x), f64::from(y));
                        c.scroll(self.id, dx, dy, None);
                    }
                    Reply::ok(Acted::new(format!("Scrolled {handle} by {dx}, {dy} points."), format!("{dx}, {dy}")))
                }
            }
            "type" => {
                let text = match text_arg(args, "text", "type") {
                    Ok(t) => t,
                    Err(e) => return Reply::err(e),
                };
                if text.chars().count() > TYPE_MAX {
                    return Reply::err(format!("type takes at most {TYPE_MAX} characters at once."));
                }
                let (layout, variant) = core.desktops.keyboard_layout();
                if layout != "us" || !variant.is_empty() {
                    return Reply::err(format!(
                        "type works with the us keyboard layout only, and the desktops use {layout} {variant}. Use key for single keys."
                    ));
                }
                let missing: Vec<char> = text.chars().filter(|c| keys::evdev_char(*c).is_none()).collect();
                if !missing.is_empty() {
                    let list: String = missing.iter().take(10).collect();
                    return Reply::err(format!(
                        "The us keyboard layout cannot type {list:?}. Type the text without these characters, or put it in a file instead."
                    ));
                }
                let presses: Vec<Vec<u32>> = text
                    .chars()
                    .filter_map(keys::evdev_char)
                    .map(|(code, shift)| if shift { vec![KEY_LEFTSHIFT, code] } else { vec![code] })
                    .collect();
                self.keys(core, &presses);
                Reply::ok(Acted::new(format!("Typed {} characters into {handle}.", text.chars().count()), text))
            }
            "key" => {
                let keys_text = match text_arg(args, "keys", "key") {
                    Ok(t) => t,
                    Err(e) => return Reply::err(e),
                };
                let combos = match keys::parse(&keys_text) {
                    Ok(c) => c,
                    Err(e) => return Reply::err(e),
                };
                let presses: Result<Vec<Vec<u32>>, String> = combos.iter().map(keys::evdev_combo).collect();
                match presses {
                    Ok(presses) => {
                        self.keys(core, &presses);
                        Reply::ok(Acted::new(format!("Pressed {} in {handle}.", keys_text.trim()), keys_text.trim()))
                    }
                    Err(e) => Reply::err(e),
                }
            }
            other => Reply::err(format!("A window has no action {other:?}.")),
        }
    }

    fn screenshot(&mut self, core: &mut Core) -> Option<Reply<Vec<u8>>> {
        let handle = self.handle();
        let Some(frame) = core.desktops.compositor().and_then(|c| c.frame(self.id)) else {
            return Some(Reply::err(format!("{handle} has not drawn anything yet, or it is closed.")));
        };
        let (width, height, scale) = (frame.width, frame.height, frame.scale);
        // The compositor keeps premultiplied alpha, PNG wants it straight.
        let mut pixels = frame.pixels.to_vec();
        for px in pixels.as_chunks_mut::<4>().0 {
            let a = u32::from(px[3]);
            if a > 0 && a < 255 {
                for c in &mut px[..3] {
                    *c = (u32::from(*c) * 255 / a).min(255) as u8;
                }
            }
        }
        let Some(image) = image::RgbaImage::from_raw(width, height, pixels) else {
            return Some(Reply::err(format!("The image of {handle} has the wrong size.")));
        };
        // One pixel of the picture is one point, as for click.
        let image = if (scale - 1.0).abs() > 0.01 {
            let (w, h) = (((width as f32) / scale).round().max(1.0) as u32, ((height as f32) / scale).round().max(1.0) as u32);
            image::imageops::resize(&image, w, h, image::imageops::FilterType::Triangle)
        } else {
            image
        };
        Some(Reply::Now(encode_png(image, SHOT_MAX)))
    }

    fn taken_over(&self, core: &mut Core) -> bool {
        core.agent_apps.is_taken(&self.agent, AppTab::Window(self.id))
    }
}
