//! How an agent reads and drives a page (D30). Built on [`Remote::call`],
//! so every function blocks until Chrome answered or the time is up. Call
//! them from a worker thread, never from the UI thread.
//!
//! [`read`] returns the page as text together with the elements an agent can
//! act on. Each element gets a ref (`e1`, `e2`, ...), which is also written
//! into the page as the attribute `data-meerora-ref`. [`click`] and
//! [`type_into`] find the element again by that ref. Every [`read`] numbers
//! the elements anew, so refs are only valid until the next read or until
//! the page changes. Password fields are read as `••••`, so a password
//! never leaves the page.

use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{Value, json};

use crate::{Button, Key, Mods, MouseEvent, MouseKind, Remote, Status, cdp, seconds};

/// How often [`wait_ready`] and [`open`] look at the page.
const POLL: Duration = Duration::from_millis(25);
/// How often [`scroll`] looks whether the page still moves.
const SETTLE: Duration = Duration::from_millis(40);
/// At most this many looks after a scroll.
const SETTLE_POLLS: u32 = 25;

/// One element of the page an agent can act on.
#[derive(Clone, Debug, PartialEq)]
pub struct Element {
    /// `e1`, `e2`, ... in document order, valid until the next [`read`].
    pub r#ref: String,
    /// For example link, button, textbox, checkbox, radio or select.
    pub role: String,
    /// What the element is called, whitespace collapsed, about 80 characters at most.
    pub name: String,
    /// The current value of inputs, textareas and selects, `checked` or
    /// `unchecked` for checkboxes and radios, `••••` for a filled password
    /// field. None for links and buttons.
    pub value: Option<String>,
}

/// A page as text.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PageText {
    pub url: String,
    pub title: String,
    /// The visible text, about 20000 characters at most. A cut text ends with
    /// a line that says so.
    pub text: String,
    /// The visible elements an agent can act on, at most 300, in document order.
    pub elements: Vec<Element>,
}

/// Waits until the page runs and is not loading, at most `timeout`.
///
/// A navigation asked for with [`Remote::navigate`] a moment ago may not have
/// started yet, and then this returns before the new page loads. [`open`]
/// navigates and waits in one step.
pub fn wait_ready(remote: &Remote, timeout: Duration) -> Result<(), String> {
    let clock = Clock::new(timeout);
    loop {
        match remote.status() {
            Status::Failed(e) | Status::Exited(e) => return Err(e),
            Status::Starting => {}
            Status::Running => {
                let page = remote.page();
                if page.crashed {
                    return Err("The page crashed. Reload it or open another page.".into());
                }
                // The worker's view of the page can lag behind. The document itself cannot.
                if !page.loading
                    && let Ok(Value::String(state)) = evaluate(remote, &clock, "document.readyState")
                    && state != "loading"
                {
                    return Ok(());
                }
            }
        }
        let Ok(left) = clock.left() else {
            return Err(match remote.status() {
                Status::Running => format!("The page did not finish loading within {}.", seconds(timeout)),
                _ => format!("The page did not start within {}.", seconds(timeout)),
            });
        };
        std::thread::sleep(POLL.min(left));
    }
}

/// Loads `url` and waits until the new page is ready, at most `timeout`.
/// Unlike [`Remote::navigate`] it returns only after the navigation started,
/// so the next [`read`] sees the new page. It does not check `url`. Turn what
/// an agent asks for into a URL with [`crate::url::for_agent`] first.
pub fn open(remote: &Remote, url: &str, timeout: Duration) -> Result<(), String> {
    let clock = Clock::new(timeout);
    // The page takes calls only once it runs.
    loop {
        match remote.status() {
            Status::Failed(e) | Status::Exited(e) => return Err(e),
            Status::Running => break,
            Status::Starting => {}
        }
        let left = clock.left().map_err(|_| format!("The page did not start within {}.", seconds(timeout)))?;
        std::thread::sleep(POLL.min(left));
    }
    let r = call(remote, &clock, "Page.navigate", json!({ "url": url }))?;
    if let Some(e) = r.get("errorText").and_then(Value::as_str).filter(|e| !e.is_empty()) {
        return Err(format!("Chrome could not open {url} ({e})."));
    }
    wait_ready(remote, clock.left()?)
}

/// Reads the page as text with its elements and gives each element a new ref.
pub fn read(remote: &Remote, timeout: Duration) -> Result<PageText, String> {
    let clock = Clock::new(timeout);
    let v = evaluate(remote, &clock, &script(READ, None, None))?;
    page_text(&v)
}

/// Scrolls the element into view and clicks its centre with real mouse
/// events (Input.dispatchMouseEvent move, press, release). Returns the element.
pub fn click(remote: &Remote, r#ref: &str, timeout: Duration) -> Result<Element, String> {
    let clock = Clock::new(timeout);
    let v = evaluate(remote, &clock, &script(LOCATE, Some(r#ref), None))?;
    if v.is_null() {
        return Err(missing(r#ref));
    }
    let element = parse_element(&v["el"]).ok_or_else(|| missing(r#ref))?;
    let (x, y) = point(&v["at"]).ok_or_else(|| not_visible(r#ref))?;
    mouse_click(remote, &clock, x, y)?;
    Ok(element)
}

/// Focuses the element, selects what it holds, inserts `text`
/// (Input.insertText) and presses Enter when `submit`. Returns the element
/// with its new value.
///
/// An empty `text` clears the element. A select takes the option whose text
/// or value is `text` instead.
pub fn type_into(remote: &Remote, r#ref: &str, text: &str, submit: bool, timeout: Duration) -> Result<Element, String> {
    let clock = Clock::new(timeout);
    let v = evaluate(remote, &clock, &script(FOCUS, Some(r#ref), Some(text)))?;
    if v.is_null() {
        return Err(missing(r#ref));
    }
    if let Some(e) = v["error"].as_str() {
        return Err(e.to_string());
    }
    let mut element = parse_element(&v["el"]).ok_or_else(|| missing(r#ref))?;
    if v["selected"] != Value::Bool(true) {
        // Some widgets take the focus only from a real click.
        if v["focused"] != Value::Bool(true) {
            let (x, y) = point(&v["at"]).ok_or_else(|| not_visible(r#ref))?;
            mouse_click(remote, &clock, x, y)?;
            let f = evaluate(remote, &clock, &script(REFOCUS, Some(r#ref), None))?;
            if f["focused"] != Value::Bool(true) {
                return Err(format!("Element {} takes no text. Click it instead, or read the page again with app_read.", r#ref));
            }
        }
        if text.is_empty() {
            press(remote, &clock, Key::Backspace)?;
        } else {
            call(remote, &clock, "Input.insertText", json!({ "text": text }))?;
        }
        if let Ok(v) = evaluate(remote, &clock, &script(DESCRIBE, Some(r#ref), None))
            && let Some(now) = parse_element(&v)
        {
            element = now;
        }
    }
    if submit {
        press(remote, &clock, Key::Enter)?;
    }
    Ok(element)
}

/// Scrolls by `dy` CSS pixels, positive is down, with a mouse wheel event at
/// the centre of the viewport. Waits a little until the page stops moving.
pub fn scroll(remote: &Remote, dy: f32, timeout: Duration) -> Result<(), String> {
    if dy == 0.0 {
        return Ok(());
    }
    let clock = Clock::new(timeout);
    const VIEW: &str = "[innerWidth, innerHeight, scrollX, scrollY]";
    let v = evaluate(remote, &clock, VIEW)?;
    let num = |v: &Value, i: usize| v.get(i).and_then(Value::as_f64).unwrap_or(0.0);
    let mut wheel = MouseEvent::new(MouseKind::Wheel, (num(&v, 0) / 2.0) as f32, (num(&v, 1) / 2.0) as f32);
    wheel.dy = dy;
    call(remote, &clock, "Input.dispatchMouseEvent", cdp::mouse_params(&wheel))?;
    // A wheel scroll may be animated. It is done once the position holds for a few looks.
    let (mut last, mut steady) = ((num(&v, 2), num(&v, 3)), 0);
    for _ in 0..SETTLE_POLLS {
        let Ok(left) = clock.left() else { break };
        std::thread::sleep(SETTLE.min(left));
        let Ok(now) = evaluate(remote, &clock, VIEW) else { break };
        let pos = (num(&now, 2), num(&now, 3));
        if pos == last {
            steady += 1;
            if steady >= 3 {
                break;
            }
        } else {
            (last, steady) = (pos, 0);
        }
    }
    Ok(())
}

/// The viewport as PNG bytes (Page.captureScreenshot).
pub fn screenshot(remote: &Remote, timeout: Duration) -> Result<Vec<u8>, String> {
    let clock = Clock::new(timeout);
    let r = call(remote, &clock, "Page.captureScreenshot", json!({ "format": "png" }))?;
    let data = r.get("data").and_then(Value::as_str).ok_or("Chrome sent a screenshot without data.")?;
    base64::engine::general_purpose::STANDARD.decode(data).map_err(|e| format!("Chrome sent a screenshot that is not base64 ({e})."))
}

/// The time left of one operation.
struct Clock {
    deadline: Instant,
    timeout: Duration,
}

impl Clock {
    fn new(timeout: Duration) -> Self {
        let now = Instant::now();
        let deadline = now.checked_add(timeout).unwrap_or_else(|| now + Duration::from_secs(365 * 24 * 3600));
        Self { deadline, timeout }
    }

    /// The time left, or an error once it is up.
    fn left(&self) -> Result<Duration, String> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() { Err(format!("The page did not answer within {}.", seconds(self.timeout))) } else { Ok(left) }
    }
}

fn call(remote: &Remote, clock: &Clock, method: &str, params: Value) -> Result<Value, String> {
    remote.call(method, params, clock.left()?)
}

/// Runs `expression` in the page and returns its value. A JavaScript
/// exception becomes a readable error.
fn evaluate(remote: &Remote, clock: &Clock, expression: &str) -> Result<Value, String> {
    let r = call(remote, clock, "Runtime.evaluate", json!({ "expression": expression, "returnByValue": true, "awaitPromise": false }))?;
    if let Some(ex) = r.get("exceptionDetails") {
        return Err(script_error(ex));
    }
    Ok(r.get("result").and_then(|v| v.get("value")).cloned().unwrap_or(Value::Null))
}

/// The first line of the exception in CDP's `exceptionDetails`.
fn script_error(ex: &Value) -> String {
    let detail = ex
        .pointer("/exception/description")
        .and_then(Value::as_str)
        .or_else(|| ex.get("text").and_then(Value::as_str))
        .unwrap_or("unknown error");
    let first = detail.lines().next().unwrap_or(detail).trim();
    format!("A script in the page failed ({first}).")
}

/// One of the scripts below, wrapped so its names stay out of the page's
/// global scope. `REF` and `TEXT` are JavaScript constants, null when unset.
fn script(body: &str, r#ref: Option<&str>, text: Option<&str>) -> String {
    let literal = |s: Option<&str>| s.map_or_else(|| "null".to_string(), |s| Value::String(s.to_string()).to_string());
    format!("(() => {{\nconst REF = {}, TEXT = {};\n{PRELUDE}\n{body}\n}})()", literal(r#ref), literal(text))
}

fn missing(r#ref: &str) -> String {
    format!("There is no element {} on the page. Read the page again with app_read, refs change when the page changes.", r#ref)
}

fn not_visible(r#ref: &str) -> String {
    format!("Element {} is not visible on the page right now. Read the page again with app_read.", r#ref)
}

fn parse_element(v: &Value) -> Option<Element> {
    let text = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    Some(Element {
        r#ref: v.get("ref")?.as_str()?.to_string(),
        role: text("role"),
        name: text("name"),
        value: v.get("value").and_then(Value::as_str).map(str::to_string),
    })
}

fn page_text(v: &Value) -> Result<PageText, String> {
    if !v.is_object() {
        return Err("The page did not answer with its content.".into());
    }
    let text = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    let elements =
        v.get("elements").and_then(Value::as_array).map(|list| list.iter().filter_map(parse_element).collect()).unwrap_or_default();
    Ok(PageText { url: text("url"), title: text("title"), text: text("text"), elements })
}

/// `{x, y}` in CSS pixels of the viewport.
fn point(v: &Value) -> Option<(f32, f32)> {
    Some((v.get("x")?.as_f64()? as f32, v.get("y")?.as_f64()? as f32))
}

/// Moves the mouse to `x`, `y` and clicks the left button there.
fn mouse_click(remote: &Remote, clock: &Clock, x: f32, y: f32) -> Result<(), String> {
    for kind in [MouseKind::Move, MouseKind::Down, MouseKind::Up] {
        let mut event = MouseEvent::new(kind, x, y);
        if kind != MouseKind::Move {
            event.button = Button::Left;
            event.clicks = 1;
        }
        call(remote, clock, "Input.dispatchMouseEvent", cdp::mouse_params(&event))?;
    }
    Ok(())
}

/// Presses and releases `key` in the focused element.
fn press(remote: &Remote, clock: &Clock, key: Key) -> Result<(), String> {
    for params in cdp::key_events(key, Mods::default()) {
        call(remote, clock, "Input.dispatchKeyEvent", params)?;
    }
    Ok(())
}

/// Helpers every script below uses.
const PRELUDE: &str = r#"
const REF_ATTR = 'data-meerora-ref';
const MAX_NAME = 80, MAX_VALUE = 300;
const MASK = '••••';
const ROLES = ['button', 'link', 'checkbox', 'radio', 'switch', 'tab', 'menuitem', 'menuitemcheckbox', 'menuitemradio',
  'option', 'textbox', 'searchbox', 'combobox', 'slider', 'spinbutton'];
const CHECKED_ROLES = ['checkbox', 'radio', 'switch', 'menuitemcheckbox', 'menuitemradio'];
const BUTTON_TYPES = ['button', 'submit', 'reset', 'image'];
const NO_TEXT_TYPES = ['checkbox', 'radio', 'file', 'range', 'color'];
const SELECTOR = ['a[href]', 'button', 'input:not([type=hidden i])', 'textarea', 'select', 'summary',
  '[contenteditable]:not([contenteditable=false i])'].concat(ROLES.map(r => '[role~=' + r + ']')).join(',');

const clean = s => String(s == null ? '' : s).replace(/\s+/g, ' ').trim();
// Cuts after n UTF-16 units without splitting a surrogate pair.
const trunc = (s, n) => {
  if (s.length <= n) return s;
  const c = s.charCodeAt(n - 1);
  return s.slice(0, c >= 0xd800 && c <= 0xdbff ? n - 1 : n);
};
const cut = (s, n) => (s.length > n ? trunc(s, n) + '…' : s);
const textOf = el => clean(typeof el.innerText === 'string' ? el.innerText : el.textContent);

// Visits every element in document order, open shadow roots included, until visit returns false.
const each = (root, visit) => {
  const stack = [root];
  while (stack.length) {
    const node = stack.pop();
    if (node.nodeType === 1 && visit(node) === false) return;
    const kids = node.children || [];
    for (let i = kids.length - 1; i >= 0; i--) stack.push(kids[i]);
    if (node.shadowRoot) stack.push(node.shadowRoot);
  }
};

const visible = el => {
  if (el.checkVisibility && !el.checkVisibility({ visibilityProperty: true, checkVisibilityCSS: true })) return false;
  const r = el.getBoundingClientRect();
  if (!(r.width > 0 && r.height > 0)) return false;
  const st = getComputedStyle(el);
  return st.visibility !== 'hidden' && st.visibility !== 'collapse' && st.display !== 'none';
};

// The outermost element of an editable region.
const editableRoot = el => el.isContentEditable === true && !(el.parentElement && el.parentElement.isContentEditable);

const roleOf = el => {
  const attr = clean(el.getAttribute('role')).split(' ')[0].toLowerCase();
  if (ROLES.includes(attr)) return attr;
  const tag = el.localName;
  if (tag === 'a') return 'link';
  if (tag === 'button' || tag === 'summary') return 'button';
  if (tag === 'select') return 'select';
  if (tag === 'textarea') return 'textbox';
  if (tag === 'input') {
    const type = el.type;
    if (type === 'checkbox' || type === 'radio' || type === 'file' || type === 'color') return type;
    if (BUTTON_TYPES.includes(type)) return 'button';
    if (type === 'range') return 'slider';
    return 'textbox';
  }
  if (editableRoot(el)) return 'textbox';
  return attr || tag;
};

const labelledBy = el => {
  const ids = clean(el.getAttribute('aria-labelledby'));
  if (!ids) return '';
  const root = el.getRootNode();
  return clean(ids.split(' ').map(id => {
    const l = (root.getElementById ? root.getElementById(id) : null) || document.getElementById(id);
    return l ? textOf(l) : '';
  }).join(' '));
};

const nameOf = el => {
  const tag = el.localName, type = tag === 'input' ? el.type : '';
  const field = tag === 'input' || tag === 'textarea' || tag === 'select';
  const attr = a => clean(el.getAttribute(a));
  let n = attr('aria-label') || labelledBy(el);
  if (!n && field && el.labels) n = clean([...el.labels].map(textOf).join(' '));
  if (!n && BUTTON_TYPES.includes(type)) n = clean(el.value) || (type === 'submit' ? 'Submit' : type === 'reset' ? 'Reset' : '');
  if (!n && !field && !editableRoot(el)) n = textOf(el);
  n = n || attr('placeholder') || attr('aria-placeholder') || attr('data-placeholder') || attr('title') || attr('alt');
  if (!n && el.querySelector) {
    const inner = el.querySelector('img[alt]:not([alt=""]), svg title, [aria-label]');
    if (inner) n = inner.localName === 'img' ? clean(inner.getAttribute('alt'))
      : inner.localName === 'title' ? clean(inner.textContent) : clean(inner.getAttribute('aria-label'));
  }
  if (!n && tag === 'a') n = attr('href');
  return cut(n, MAX_NAME);
};

const valueOf = el => {
  const tag = el.localName;
  if (tag === 'input') {
    const type = el.type;
    if (type === 'checkbox' || type === 'radio') return el.checked ? 'checked' : 'unchecked';
    if (BUTTON_TYPES.includes(type)) return null;
    // A password never leaves the page. Only whether the field is empty does.
    if (type === 'password') return el.value ? MASK : '';
    return cut(el.value, MAX_VALUE);
  }
  if (tag === 'textarea') return cut(el.value, MAX_VALUE);
  if (tag === 'select') return cut(clean([...el.selectedOptions].map(o => o.label).join(', ')), MAX_VALUE);
  const role = roleOf(el);
  if (CHECKED_ROLES.includes(role)) {
    const c = el.getAttribute('aria-checked');
    return c === 'true' ? 'checked' : c === 'mixed' ? 'mixed' : 'unchecked';
  }
  if (editableRoot(el)) return cut(textOf(el), MAX_VALUE);
  if (role === 'slider' || role === 'spinbutton') {
    const v = el.getAttribute('aria-valuetext') || el.getAttribute('aria-valuenow');
    return v == null ? null : cut(clean(v), MAX_VALUE);
  }
  return null;
};

const describe = (el, ref) => ({ ref, role: roleOf(el), name: nameOf(el), value: valueOf(el) });

const find = ref => {
  let hit = document.querySelector('[' + REF_ATTR + '="' + CSS.escape(String(ref)) + '"]');
  if (!hit) each(document, el => {
    if (el.getAttribute(REF_ATTR) === ref) { hit = el; return false; }
  });
  return hit;
};

// The centre of the first part of the element inside the viewport, or null.
const point = el => {
  const rects = [...el.getClientRects()].filter(r => r.width > 0 && r.height > 0);
  for (const r of rects.length ? rects : [el.getBoundingClientRect()]) {
    const left = Math.max(r.left, 0), top = Math.max(r.top, 0);
    const right = Math.min(r.right, innerWidth), bottom = Math.min(r.bottom, innerHeight);
    if (right > left && bottom > top) return { x: (left + right) / 2, y: (top + bottom) / 2 };
  }
  return null;
};

const deepActive = () => {
  let a = document.activeElement;
  while (a && a.shadowRoot && a.shadowRoot.activeElement) a = a.shadowRoot.activeElement;
  return a;
};

const holds = (el, a) => !!a && (a === el || el.contains(a) || (!!el.shadowRoot && el.shadowRoot.contains(a)));

const selectAll = t => {
  if (t.localName === 'input' || t.localName === 'textarea') {
    try { t.select(); } catch (e) {}
    return;
  }
  if (t.isContentEditable) {
    let root = t;
    while (root.parentElement && root.parentElement.isContentEditable) root = root.parentElement;
    const range = document.createRange();
    range.selectNodeContents(root);
    const s = getSelection();
    s.removeAllRanges();
    s.addRange(range);
  }
};

const show = el => el.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
"#;

/// Removes the old refs and returns the page with new ones.
const READ: &str = r#"
const MAX_TEXT = 20000, MAX_ELEMENTS = 300;
const elements = [];
each(document, el => {
  if (el.hasAttribute(REF_ATTR)) el.removeAttribute(REF_ATTR);
  if (elements.length >= MAX_ELEMENTS || !el.matches(SELECTOR) || !visible(el)) return;
  const ref = 'e' + (elements.length + 1);
  el.setAttribute(REF_ATTR, ref);
  elements.push(describe(el, ref));
});
const body = document.body || document.documentElement;
let text = body ? String((typeof body.innerText === 'string' ? body.innerText : body.textContent) || '') : '';
text = text.replace(/[ \t ]+\n/g, '\n').replace(/\n{3,}/g, '\n\n').trim();
if (text.length > MAX_TEXT) text = trunc(text, MAX_TEXT) + '\n[The page has more text.]';
return { url: location.href, title: document.title, text, elements };
"#;

/// Scrolls the element into view and returns it with the point to click.
const LOCATE: &str = r#"
const el = find(REF);
if (!el) return null;
show(el);
return { el: describe(el, REF), at: point(el) };
"#;

/// Scrolls the element into view, focuses it and selects what it holds. A
/// select takes the option named by TEXT right here.
const FOCUS: &str = r#"
const el = find(REF);
if (!el) return null;
show(el);
const d = describe(el, REF);
if (el.disabled) return { error: 'Element ' + REF + ' is disabled.' };
if (el.localName === 'select') {
  const want = clean(TEXT).toLowerCase();
  const opts = [...el.options];
  const label = o => clean(o.label).toLowerCase();
  const hit = opts.find(o => label(o) === want) || opts.find(o => o.value.toLowerCase() === want)
    || (want ? opts.find(o => label(o).includes(want)) : undefined);
  if (!hit) {
    const names = opts.slice(0, 30).map(o => clean(o.label)).join(', ');
    return { error: 'The select ' + REF + ' has no option "' + clean(TEXT) + '". Its options are: ' + names + '.' };
  }
  el.focus();
  if (el.selectedIndex !== hit.index) {
    el.selectedIndex = hit.index;
    el.dispatchEvent(new Event('input', { bubbles: true }));
    el.dispatchEvent(new Event('change', { bubbles: true }));
  }
  return { selected: true, el: describe(el, REF) };
}
if (el.localName === 'input' && el.type === 'file') return { error: 'Element ' + REF + ' is a file input. Agents cannot pick files here.' };
if (el.localName === 'input' && (BUTTON_TYPES.includes(el.type) || NO_TEXT_TYPES.includes(el.type))) {
  return { error: 'Element ' + REF + ' is a ' + d.role + ' and takes no text. Click it instead.' };
}
if (el.readOnly) return { error: 'Element ' + REF + ' is read-only.' };
el.focus({ preventScroll: true });
const a = deepActive();
const focused = holds(el, a);
if (focused) selectAll(a);
return { el: d, focused, at: point(el) };
"#;

/// Runs after a click on the element and selects what the focused field holds.
const REFOCUS: &str = r#"
const el = find(REF);
const a = deepActive();
if (!el || !a || a === document.body) return { focused: false };
const editable = a.localName === 'input' || a.localName === 'textarea' || a.isContentEditable === true;
const focused = holds(el, a) || editable;
if (focused) selectAll(a);
return { focused };
"#;

const DESCRIBE: &str = r#"
const el = find(REF);
return el ? describe(el, REF) : null;
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_page_and_its_elements() {
        let v = json!({
            "url": "data:text/html,x",
            "title": "T",
            "text": "Hello",
            "elements": [
                { "ref": "e1", "role": "link", "name": "Home", "value": null },
                { "ref": "e2", "role": "textbox", "name": "Password", "value": "••••" },
                { "role": "button", "name": "without a ref is left out" },
            ],
        });
        let page = page_text(&v).unwrap();
        assert_eq!((page.url.as_str(), page.title.as_str(), page.text.as_str()), ("data:text/html,x", "T", "Hello"));
        assert_eq!(
            page.elements,
            vec![
                Element { r#ref: "e1".into(), role: "link".into(), name: "Home".into(), value: None },
                Element { r#ref: "e2".into(), role: "textbox".into(), name: "Password".into(), value: Some("••••".into()) },
            ]
        );
        assert!(page_text(&Value::Null).is_err());
        assert_eq!(page_text(&json!({})).unwrap(), PageText::default());
    }

    #[test]
    fn script_errors_are_readable() {
        let ex = json!({ "text": "Uncaught", "exception": { "description": "TypeError: x is not a function\n    at <anonymous>:1:1" } });
        assert_eq!(script_error(&ex), "A script in the page failed (TypeError: x is not a function).");
        assert_eq!(script_error(&json!({ "text": "Uncaught SyntaxError" })), "A script in the page failed (Uncaught SyntaxError).");
        assert_eq!(script_error(&json!({})), "A script in the page failed (unknown error).");
    }

    #[test]
    fn scripts_quote_their_arguments() {
        let s = script(DESCRIBE, Some("e1\"); alert(1); //"), None);
        assert!(s.starts_with("(() => {\nconst REF = \"e1\\\"); alert(1); //\", TEXT = null;\n"), "{s}");
        assert!(s.ends_with("})()"));
        let s = script(FOCUS, Some("e2"), Some("line\nbreak"));
        assert!(s.contains("const REF = \"e2\", TEXT = \"line\\nbreak\";"), "{s}");
    }

    #[test]
    fn errors_name_the_ref_and_the_way_out() {
        assert!(missing("e12").contains("There is no element e12 on the page. Read the page again with app_read"));
        assert!(not_visible("e3").contains("e3"));
        assert_eq!(point(&json!({ "x": 1.5, "y": 2 })), Some((1.5, 2.0)));
        assert_eq!(point(&Value::Null), None);
        let clock = Clock::new(Duration::ZERO);
        assert_eq!(clock.left(), Err("The page did not answer within 0 s.".to_string()));
        assert!(Clock::new(Duration::MAX).left().is_ok());
    }
}
