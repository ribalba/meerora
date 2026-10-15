//! Headless Chrome for the browser tile of the agent view (D11, D13). No
//! rendering here. The app draws [`Frame`]s and feeds input back, the way
//! meerora-term works for the terminal tile.
//!
//! [`Browser::start`] starts one Chrome process with its own profile
//! directory. Chrome starts headless with `--remote-debugging-port=0` and
//! prints its DevTools URL on stderr. A worker thread connects to it over a
//! WebSocket, attaches to the page, streams `Page.startScreencast` JPEG
//! frames and forwards navigation and input. Every method returns at once,
//! so the UI thread never waits for Chrome. The only exception is
//! [`Browser::call`], which waits for Chrome's answer and belongs on
//! another thread.
//!
//! One Chrome can show several pages, for example several browser windows
//! of one agent. Two Chromes cannot share a profile, so further pages are
//! further CDP targets in the same Chrome, each in a window of its own.
//! A [`Browser`] is the handle of one page. [`Browser::open_page`] opens
//! another page in the Chrome of an existing handle and returns its handle.
//! All pages of a Chrome share its worker thread and its connection, but
//! status, frames, the page info, the wakeup and the input are per page.
//!
//! Dropping a handle closes only its page. When the last page of a Chrome
//! is dropped, the worker closes Chrome. [`Browser::close`] and
//! [`Browser::shutdown`] close Chrome with all its pages on any handle.
//!
//! CDP is also how an agent drives a page (D13, D30). [`Browser::remote`]
//! returns a [`Remote`], a handle of the same page that can be cloned, works
//! from any thread and waits for answers with [`Remote::call`]. Dropping a
//! [`Remote`] leaves the page open. The [`agent`] module builds on it. It
//! reads a page as text with the elements an agent can act on, each with a
//! ref, and clicks, types, scrolls and takes screenshots. Whether an agent
//! may do that (the capabilities of D8) is decided by the caller.

pub mod agent;
pub mod cdp;
mod session;
pub mod url;

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use session::{Cmd, Common, PageCmd, Shared};

/// Environment variable with the path of the Chrome binary. It wins over
/// the lookup on PATH.
pub const CHROME_ENV: &str = "MEERORA_CHROME";

/// Binaries tried on PATH, in this order.
pub const CHROME_NAMES: [&str; 5] = ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser", "chrome"];

/// What to start.
#[derive(Clone, Debug)]
pub struct Options {
    /// Chrome binary. [`find_chrome`] decides when unset.
    pub chrome: Option<PathBuf>,
    /// Profile directory (`--user-data-dir`). Created when missing.
    pub profile: PathBuf,
    /// Viewport in CSS pixels.
    pub size: Size,
    /// Device pixels per CSS pixel.
    pub scale: f32,
    /// First page. `about:blank` when unset.
    pub url: Option<String>,
    /// More command line flags, for example `--no-sandbox`.
    pub args: Vec<String>,
}

impl Options {
    pub fn new(profile: impl Into<PathBuf>) -> Self {
        Self { chrome: None, profile: profile.into(), size: Size { width: 800, height: 600 }, scale: 1.0, url: None, args: Vec::new() }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

impl Size {
    /// At least 16 by 16, Chrome rejects smaller viewports.
    pub fn clamped(self) -> Self {
        Size { width: self.width.clamp(16, 8192), height: self.height.clamp(16, 8192) }
    }
}

/// The status of one page. Failed and Exited of Chrome itself reach every
/// page of that Chrome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Chrome is starting or the page is not attached yet.
    Starting,
    Running,
    /// Chrome could not start, the connection failed or Chrome refused to
    /// open this page, as readable text.
    Failed(String),
    /// Chrome exited after it ran, or the browser was closed.
    Exited(String),
}

/// The newest screencast frame.
#[derive(Clone, Debug)]
pub struct Frame {
    /// JPEG bytes as Chrome sent them.
    pub jpeg: Vec<u8>,
    /// Counts up from 1 for every frame of this page.
    pub seq: u64,
    /// Size of the page area the frame shows, in CSS pixels.
    pub css_width: f32,
    pub css_height: f32,
}

/// What the page shows right now.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Page {
    pub url: String,
    pub title: String,
    pub loading: bool,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    /// The renderer of the page crashed. A reload or a navigation recovers.
    pub crashed: bool,
    /// Frames received so far.
    pub frames: u64,
    /// Finished page loads so far.
    pub loads: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Button {
    #[default]
    None,
    Left,
    Middle,
    Right,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub alt: bool,
    pub ctrl: bool,
    pub meta: bool,
    pub shift: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseKind {
    Move,
    Down,
    Up,
    Wheel,
}

/// A mouse event in CSS pixels relative to the top left of the page.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MouseEvent {
    pub kind: MouseKind,
    pub x: f32,
    pub y: f32,
    /// The button that changed, or for a move the one held down.
    pub button: Button,
    /// 1 for a click, 2 for a double click.
    pub clicks: u32,
    /// Wheel distance in CSS pixels. Positive `dy` scrolls down.
    pub dx: f32,
    pub dy: f32,
    pub mods: Mods,
}

impl MouseEvent {
    pub fn new(kind: MouseKind, x: f32, y: f32) -> Self {
        Self { kind, x, y, button: Button::None, clicks: 0, dx: 0.0, dy: 0.0, mods: Mods::default() }
    }
}

/// Keys the app forwards. Plain text goes through [`Browser::insert_text`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Enter,
    Backspace,
    Tab,
    Escape,
    Space,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    F(u8),
    /// A letter or digit pressed together with Ctrl, Alt or Meta.
    Char(char),
}

/// The handle of one page in a headless Chrome, driven over CDP from the
/// worker thread of that Chrome.
///
/// [`Browser::start`] starts Chrome with its first page and
/// [`Browser::open_page`] opens more pages in the same Chrome. Dropping a
/// handle closes its page without waiting, and dropping the last page of a
/// Chrome closes Chrome. [`Browser::shutdown`] closes Chrome and waits.
pub struct Browser {
    tx: Sender<Cmd>,
    shared: Arc<Shared>,
    common: Arc<Common>,
    page: u32,
}

impl Browser {
    /// Starts Chrome with its first page on a worker thread and returns at
    /// once. [`Browser::status`] says when it runs or why it failed.
    pub fn start(options: Options) -> Browser {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared::new());
        let common = Arc::new(Common::new());
        let page = common.next_page();
        let (first, worker) = (shared.clone(), common.clone());
        let spawned =
            std::thread::Builder::new().name("meerora-browser".into()).spawn(move || session::run(options, rx, page, first, worker));
        if let Err(e) = spawned {
            common.finish(Status::Failed(format!("Could not start the browser thread ({e}).")), vec![shared.clone()], None);
        }
        Browser { tx, shared, common, page }
    }

    /// Opens another page in the same Chrome as `self`, in a window of its
    /// own, and returns its handle at once. `size` is the viewport in CSS
    /// pixels, `scale` the device pixels per CSS pixel and `url` the first
    /// page (`about:blank` when unset).
    ///
    /// Before Chrome runs the page waits in the worker and opens once it
    /// can. When Chrome has already failed or exited, the new handle reports
    /// that same status. The new page starts with the wakeup of `self`.
    pub fn open_page(&self, size: Size, scale: f32, url: Option<&str>) -> Browser {
        let shared = Arc::new(Shared::new());
        *lock(&shared.wakeup) = lock(&self.shared.wakeup).clone();
        let page = self.common.next_page();
        let open = Cmd::Open { page, shared: shared.clone(), size: size.clamped(), scale, url: url.map(str::to_string) };
        self.common.open(&self.tx, &shared, open);
        Browser { tx: self.tx.clone(), shared, common: self.common.clone(), page }
    }

    /// The status of this page. It is Running once this page is attached.
    pub fn status(&self) -> Status {
        lock(&self.shared.status).clone()
    }

    pub fn page(&self) -> Page {
        lock(&self.shared.page).clone()
    }

    /// The newest frame of this page since the last call, if one arrived.
    /// Older frames that were never taken are dropped. While nobody asks for
    /// frames of this page for a few seconds (the tile is hidden), Chrome is
    /// not asked for more of them.
    pub fn take_frame(&self) -> Option<Frame> {
        *lock(&self.shared.taken) = Instant::now();
        lock(&self.shared.frame).take()
    }

    /// Called from the worker thread after a new frame or a change of the
    /// status or the page, so the UI can repaint. Keep it short. It replaces
    /// the wakeup of this page only.
    pub fn set_wakeup(&self, wakeup: Box<dyn Fn() + Send + Sync>) {
        *lock(&self.shared.wakeup) = Some(Arc::from(wakeup));
    }

    /// Loads `url`. Before the page is attached the last call wins.
    pub fn navigate(&self, url: &str) {
        self.send(PageCmd::Navigate(url.to_string()));
    }

    pub fn back(&self) {
        self.send(PageCmd::History(-1));
    }

    pub fn forward(&self) {
        self.send(PageCmd::History(1));
    }

    pub fn reload(&self) {
        self.send(PageCmd::Reload);
    }

    /// New viewport in CSS pixels and device pixels per CSS pixel.
    pub fn resize(&self, size: Size, scale: f32) {
        self.send(PageCmd::Resize(size.clamped(), scale));
    }

    pub fn mouse(&self, event: MouseEvent) {
        self.send(PageCmd::Mouse(event));
    }

    /// Presses and releases `key`.
    pub fn key(&self, key: Key, mods: Mods) {
        self.send(PageCmd::Key(key, mods));
    }

    /// Types `text` into the focused element.
    pub fn insert_text(&self, text: &str) {
        if !text.is_empty() {
            self.send(PageCmd::Text(text.to_string()));
        }
    }

    /// A handle of this page for agents, see [`Remote`].
    pub fn remote(&self) -> Remote {
        Remote { tx: self.tx.clone(), page: self.page, shared: self.shared.clone() }
    }

    /// Sends the CDP command `method` to this page and waits up to
    /// `timeout` for its result, like [`Remote::call`]. Never call it on the
    /// UI thread.
    pub fn call(&self, method: &str, params: serde_json::Value, timeout: Duration) -> Result<serde_json::Value, String> {
        call(&self.tx, self.page, &self.shared, method, params, timeout)
    }

    /// Asks the worker to close Chrome with all its pages and returns at
    /// once. Every page of this Chrome then reports Exited.
    pub fn close(&self) {
        let _ = self.tx.send(Cmd::Close);
    }

    /// True once Chrome is gone and its worker thread ended. Every page of
    /// the same Chrome says the same.
    pub fn is_finished(&self) -> bool {
        self.common.done.load(Ordering::Acquire)
    }

    /// Closes Chrome with all its pages, like [`Browser::close`], and waits
    /// up to `timeout` until it is gone. Returns [`Browser::is_finished`].
    pub fn shutdown(self, timeout: Duration) -> bool {
        let _ = self.tx.send(Cmd::Close);
        let deadline = Instant::now() + timeout;
        while !self.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        self.is_finished()
    }

    fn send(&self, cmd: PageCmd) {
        let _ = self.tx.send(Cmd::Page(self.page, cmd));
    }
}

impl Drop for Browser {
    /// Asks the worker to close this page. When it was the last page of its
    /// Chrome, the worker closes Chrome. Chrome also dies with the worker
    /// thread (`PR_SET_PDEATHSIG`), so it never outlives the app.
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::ClosePage(self.page));
    }
}

/// A handle of one page for agents, made by [`Browser::remote`].
///
/// It can be cloned, sent to and used from any thread. Unlike a
/// [`Browser`] it does not own the page. Dropping it leaves the page open,
/// and once the page or its Chrome is closed every call fails with a
/// readable error. [`Remote::call`] waits for Chrome's answer. The other
/// methods return at once.
#[derive(Clone)]
pub struct Remote {
    tx: Sender<Cmd>,
    page: u32,
    shared: Arc<Shared>,
}

impl Remote {
    /// Sends the CDP command `method` with `params` to this page and waits
    /// up to `timeout` for its result. The error is readable text. It is
    /// CDP's own message or says why there was no answer. A page that is not
    /// ready yet answers at once with an error, see [`agent::wait_ready`].
    pub fn call(&self, method: &str, params: serde_json::Value, timeout: Duration) -> Result<serde_json::Value, String> {
        call(&self.tx, self.page, &self.shared, method, params, timeout)
    }

    /// The status of the page, like [`Browser::status`].
    pub fn status(&self) -> Status {
        lock(&self.shared.status).clone()
    }

    /// What the page shows, like [`Browser::page`].
    pub fn page(&self) -> Page {
        lock(&self.shared.page).clone()
    }

    /// Loads `url` and returns at once, like [`Browser::navigate`].
    /// [`agent::open`] also waits until the new page is ready.
    pub fn navigate(&self, url: &str) {
        self.send(PageCmd::Navigate(url.to_string()));
    }

    pub fn back(&self) {
        self.send(PageCmd::History(-1));
    }

    pub fn forward(&self) {
        self.send(PageCmd::History(1));
    }

    pub fn reload(&self) {
        self.send(PageCmd::Reload);
    }

    fn send(&self, cmd: PageCmd) {
        let _ = self.tx.send(Cmd::Page(self.page, cmd));
    }
}

/// Sends `method` to page `page` and waits for the answer.
fn call(
    tx: &Sender<Cmd>,
    page: u32,
    shared: &Shared,
    method: &str,
    params: serde_json::Value,
    timeout: Duration,
) -> Result<serde_json::Value, String> {
    let (reply, answer) = mpsc::channel();
    if tx.send(Cmd::Page(page, PageCmd::Call { method: method.to_string(), params, reply })).is_err() {
        return Err(gone(shared));
    }
    match answer.recv_timeout(timeout) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => Err(format!("Chrome did not answer {method} within {}.", seconds(timeout))),
        Err(RecvTimeoutError::Disconnected) => Err(gone(shared)),
    }
}

/// Why a page could not answer: Chrome is gone or the page was closed.
fn gone(shared: &Shared) -> String {
    match &*lock(&shared.status) {
        Status::Failed(e) | Status::Exited(e) => e.clone(),
        Status::Starting | Status::Running => "The page was closed.".to_string(),
    }
}

/// A duration for messages, for example "5 s" or "2.5 s".
pub(crate) fn seconds(d: Duration) -> String {
    if d.subsec_millis() == 0 { format!("{} s", d.as_secs()) } else { format!("{:.1} s", d.as_secs_f32()) }
}

/// The profile directory of an agent: `<data dir>/browser/<agent id>`.
pub fn profile_dir(data_dir: &Path, agent: &str) -> PathBuf {
    data_dir.join("browser").join(agent)
}

/// The Chrome binary: `path` when given, else [`CHROME_ENV`], else the first
/// of [`CHROME_NAMES`] on PATH.
pub fn find_chrome(path: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = path {
        return executable(p).then(|| p.to_path_buf());
    }
    if let Some(p) = std::env::var_os(CHROME_ENV).filter(|p| !p.is_empty()) {
        return which(Path::new(&p));
    }
    CHROME_NAMES.iter().find_map(|name| which(Path::new(name)))
}

fn which(name: &Path) -> Option<PathBuf> {
    if name.components().count() > 1 {
        return executable(name).then(|| name.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(name)).find(|p| executable(p))
}

fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_binaries_by_path() {
        assert_eq!(find_chrome(Some(Path::new("/bin/sh"))), Some(PathBuf::from("/bin/sh")));
        assert_eq!(find_chrome(Some(Path::new("/nonexistent/chrome"))), None);
        assert_eq!(which(Path::new("sh")).map(|p| p.file_name().unwrap().to_owned()), Some("sh".into()));
        assert_eq!(which(Path::new("no-such-binary-for-meerora")), None);
    }

    #[test]
    fn a_missing_binary_fails_readably() {
        let mut o = Options::new(std::env::temp_dir().join("meerora-browser-missing"));
        o.chrome = Some(PathBuf::from("/nonexistent/chrome"));
        let b = Browser::start(o);
        let deadline = Instant::now() + Duration::from_secs(5);
        while b.status() == Status::Starting && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        match b.status() {
            Status::Failed(text) => assert!(text.contains("/nonexistent/chrome"), "{text}"),
            other => panic!("expected a failure, got {other:?}"),
        }
        assert!(b.shutdown(Duration::from_secs(2)));
    }

    #[test]
    fn pages_of_a_failed_chrome_fail_alike() {
        let mut o = Options::new(std::env::temp_dir().join("meerora-browser-missing"));
        o.chrome = Some(PathBuf::from("/nonexistent/chrome"));
        let b = Browser::start(o);
        b.set_wakeup(Box::new(|| {}));
        let size = Size { width: 100, height: 100 };
        // Opened while the worker may still be starting Chrome.
        let early = b.open_page(size, 1.0, Some("about:blank"));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !early.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(b.is_finished() && early.is_finished(), "every handle sees the end");
        // Opened after the worker ended.
        let late = early.open_page(size, 1.0, None);
        assert!(late.is_finished());
        for page in [&b, &early, &late] {
            match page.status() {
                Status::Failed(text) => assert!(text.contains("/nonexistent/chrome"), "{text}"),
                other => panic!("expected a failure, got {other:?}"),
            }
        }

        let wakeup = |b: &Browser| lock(&b.shared.wakeup).clone().expect("a wakeup");
        let first = wakeup(&b);
        assert!(Arc::ptr_eq(&first, &wakeup(&early)), "a new page starts with the wakeup of its opener");
        early.set_wakeup(Box::new(|| {}));
        assert!(!Arc::ptr_eq(&first, &wakeup(&early)));
        assert!(Arc::ptr_eq(&first, &wakeup(&b)), "set_wakeup replaces only the wakeup of its own page");
        drop(late);
        drop(early);
        assert!(b.shutdown(Duration::from_secs(2)));
    }

    #[test]
    fn calls_on_a_failed_browser_fail_readably() {
        let mut o = Options::new(std::env::temp_dir().join("meerora-browser-missing"));
        o.chrome = Some(PathBuf::from("/nonexistent/chrome"));
        let b = Browser::start(o);
        let remote = b.remote();
        let (params, timeout) = (serde_json::json!({ "expression": "1" }), Duration::from_secs(5));
        let results = [b.call("Runtime.evaluate", params.clone(), timeout), remote.call("Runtime.evaluate", params, timeout)];
        for result in results {
            assert!(result.as_ref().is_err_and(|e| e.contains("/nonexistent/chrome")), "{result:?}");
        }
        drop(remote);
        assert!(b.shutdown(Duration::from_secs(2)));
        assert_eq!(seconds(Duration::from_secs(5)), "5 s");
        assert_eq!(seconds(Duration::from_millis(2500)), "2.5 s");
    }
}
