//! The worker thread of one Chrome: starts it, connects to its DevTools
//! WebSocket and runs the CDP conversation for all pages of that Chrome
//! until it is closed or exits.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{BufRead, BufReader, ErrorKind};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tungstenite::{Message as WsMessage, WebSocket};

use crate::cdp::{self, Message};
use crate::{Frame, Key, Mods, MouseEvent, MouseKind, Options, Page, Size, Status, lock};

/// How long Chrome may take to print its DevTools URL.
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the worker waits for new messages before it looks at commands.
const POLL: Duration = Duration::from_millis(8);
/// Lines of Chrome's stderr kept for error messages.
const TAIL_LINES: usize = 20;
/// Frames of a page are acknowledged, and so keep coming, only while the
/// app took one of its frames this recently. A hidden tile costs no rendering.
const IDLE: Duration = Duration::from_secs(5);
/// How often the title and the history of a page are read while its frames are taken.
const TITLE_EVERY: Duration = Duration::from_secs(1);

pub(crate) enum Cmd {
    /// A command for the page with this id.
    Page(u32, PageCmd),
    /// Opens another page in the same Chrome.
    Open { page: u32, shared: Arc<Shared>, size: Size, scale: f32, url: Option<String> },
    /// The handle of this page was dropped.
    ClosePage(u32),
    /// Closes Chrome with all its pages.
    Close,
}

pub(crate) enum PageCmd {
    Navigate(String),
    /// -1 is back, 1 is forward.
    History(i64),
    Reload,
    Resize(Size, f32),
    Mouse(MouseEvent),
    Key(Key, Mods),
    Text(String),
    /// Sends `method` on the page's session and hands the answer to `reply`.
    /// A page that is not ready answers at once with an error. When the
    /// session ends, `reply` is dropped, so the caller stops waiting.
    Call {
        method: String,
        params: Value,
        reply: Sender<Result<Value, String>>,
    },
}

/// What the worker and the handle of one page share.
pub(crate) struct Shared {
    pub status: Mutex<Status>,
    pub page: Mutex<Page>,
    pub frame: Mutex<Option<Frame>>,
    /// When the app last asked for a frame of this page.
    pub taken: Mutex<Instant>,
    pub wakeup: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl Shared {
    pub fn new() -> Self {
        Self {
            status: Mutex::new(Status::Starting),
            page: Mutex::new(Page::default()),
            frame: Mutex::new(None),
            taken: Mutex::new(Instant::now()),
            wakeup: Mutex::new(None),
        }
    }

    fn wake(&self) {
        let wakeup = lock(&self.wakeup).clone();
        if let Some(w) = wakeup {
            w();
        }
    }

    fn set_status(&self, status: Status) {
        *lock(&self.status) = status;
        self.wake();
    }

    /// The status once Chrome is gone. Keeps a failure that is already set.
    fn set_final(&self, status: &Status) {
        let mut s = lock(&self.status);
        if !matches!(*s, Status::Failed(_)) {
            *s = status.clone();
        }
    }
}

/// What all pages of one Chrome share.
pub(crate) struct Common {
    /// The last status of the worker, set when it ends. Pages opened later
    /// take it over.
    end: Mutex<Option<Status>>,
    /// The worker thread ended.
    pub done: AtomicBool,
    next_page: AtomicU32,
}

impl Common {
    pub fn new() -> Self {
        Self { end: Mutex::new(None), done: AtomicBool::new(false), next_page: AtomicU32::new(0) }
    }

    pub fn next_page(&self) -> u32 {
        self.next_page.fetch_add(1, Ordering::Relaxed)
    }

    /// Hands `open` to the worker. When the worker has already ended, the
    /// page gets its final status at once instead.
    pub fn open(&self, tx: &Sender<Cmd>, shared: &Shared, open: Cmd) {
        let end = lock(&self.end);
        let status = match &*end {
            Some(status) => status.clone(),
            None => match tx.send(open) {
                Ok(()) => return,
                Err(_) => Status::Failed("The browser thread ended.".into()),
            },
        };
        shared.set_final(&status);
    }

    /// The last word of the worker: every page in `pages` and every page
    /// still waiting in `rx` gets `status`, then all handles report done.
    pub fn finish(&self, status: Status, mut pages: Vec<Arc<Shared>>, rx: Option<&Receiver<Cmd>>) {
        {
            // Under this lock no page can be opened in between, see `open`.
            let mut end = lock(&self.end);
            *end = Some(status.clone());
            while let Some(cmd) = rx.and_then(|rx| rx.try_recv().ok()) {
                if let Cmd::Open { shared, .. } = cmd {
                    pages.push(shared);
                }
            }
            for page in &pages {
                page.set_final(&status);
            }
            self.done.store(true, Ordering::Release);
        }
        for page in &pages {
            page.wake();
        }
    }
}

pub(crate) fn run(options: Options, rx: Receiver<Cmd>, page: u32, shared: Arc<Shared>, common: Arc<Common>) {
    let mut pages = vec![shared.clone()];
    let status = match launch(&options) {
        Ok(mut chrome) => {
            let result = connect(&chrome).and_then(|ws| {
                let mut s = Session::new(ws, page, shared, options.size.clamped(), options.scale);
                let result = s.run(&rx, &mut chrome);
                pages = s.into_pages();
                result
            });
            let status = match result {
                Ok(reason) => Status::Exited(reason),
                Err(e) => Status::Failed(e),
            };
            chrome.stop();
            status
        }
        Err(e) => Status::Failed(e),
    };
    common.finish(status, pages, Some(&rx));
}

/// A started Chrome with the DevTools URL it printed.
struct Chrome {
    child: Child,
    url: String,
    tail: Arc<Mutex<VecDeque<String>>>,
}

impl Chrome {
    fn tail(&self) -> String {
        lock(&self.tail).back().cloned().unwrap_or_default()
    }

    /// `Some(text)` once Chrome has exited.
    fn exited(&mut self) -> Option<String> {
        let status = self.child.try_wait().ok().flatten()?;
        let last = self.tail();
        Some(if last.is_empty() { format!("Chrome exited ({status}).") } else { format!("Chrome exited ({status}): {last}") })
    }

    /// Waits a little for Chrome to exit after `Browser.close`, then kills it.
    fn stop(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn launch(options: &Options) -> Result<Chrome, String> {
    let chrome = match &options.chrome {
        Some(p) => crate::find_chrome(Some(p)).ok_or_else(|| format!("Chrome was not found at {}.", p.display()))?,
        None => crate::find_chrome(None).ok_or_else(|| match std::env::var_os(crate::CHROME_ENV).filter(|p| !p.is_empty()) {
            Some(p) => format!("{} is set to {}, which is not a program.", crate::CHROME_ENV, p.to_string_lossy()),
            None => format!("Chrome was not found. Install google-chrome or chromium, or set {} to the browser binary.", crate::CHROME_ENV),
        })?,
    };
    std::fs::create_dir_all(&options.profile)
        .map_err(|e| format!("Cannot create the browser profile {} ({e}).", options.profile.display()))?;
    let size = options.size.clamped();
    let mut cmd = Command::new(&chrome);
    cmd.args(chrome_args(&options.profile, size, &options.args))
        .arg(options.url.as_deref().unwrap_or("about:blank"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // SAFETY: prctl is async-signal-safe and touches no memory of the parent.
    // Chrome then dies with this worker thread, so it never outlives the app.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| format!("Cannot start {} ({e}).", chrome.display()))?;
    let stderr = child.stderr.take().ok_or("Chrome has no stderr")?;
    let tail = Arc::new(Mutex::new(VecDeque::new()));
    let (url_tx, url_rx) = mpsc::channel();
    {
        let tail = tail.clone();
        // Reads stderr until Chrome closes it, so Chrome never blocks on a full pipe.
        let spawned = std::thread::Builder::new().name("meerora-browser-stderr".into()).spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                if let Some(url) = cdp::devtools_url(&line) {
                    let _ = url_tx.send(url.to_string());
                    continue;
                }
                tracing::debug!("chrome: {line}");
                let mut t = lock(&tail);
                t.push_back(line);
                if t.len() > TAIL_LINES {
                    t.pop_front();
                }
            }
        });
        if let Err(e) = spawned {
            let _ = child.kill();
            return Err(format!("Cannot read Chrome's output ({e})."));
        }
    }
    let deadline = Instant::now() + START_TIMEOUT;
    let mut chrome = Chrome { child, url: String::new(), tail };
    loop {
        match url_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(url) => {
                chrome.url = url;
                return Ok(chrome);
            }
            Err(RecvTimeoutError::Timeout) if Instant::now() < deadline => {
                if let Some(text) = chrome.exited() {
                    return Err(text);
                }
            }
            Err(_) => {
                let reason = chrome.exited().unwrap_or_else(|| "Chrome did not print its DevTools address in time.".into());
                chrome.stop();
                return Err(reason);
            }
        }
    }
}

fn chrome_args(profile: &std::path::Path, size: Size, extra: &[String]) -> Vec<String> {
    let mut args: Vec<String> = [
        "--headless",
        "--remote-debugging-port=0",
        "--remote-debugging-address=127.0.0.1",
        "--no-first-run",
        "--no-default-browser-check",
        "--disable-background-networking",
        "--disable-component-update",
        "--disable-default-apps",
        "--disable-sync",
        "--password-store=basic",
        "--mute-audio",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.push(format!("--user-data-dir={}", profile.display()));
    args.push(format!("--window-size={},{}", size.width, size.height));
    args.extend(extra.iter().cloned());
    args
}

fn connect(chrome: &Chrome) -> Result<WebSocket<TcpStream>, String> {
    let addr = cdp::ws_address(&chrome.url).ok_or_else(|| format!("Chrome printed an unexpected DevTools address {}.", chrome.url))?;
    let sock = addr
        .to_socket_addrs()
        .map_err(|e| format!("Bad DevTools address {addr} ({e})."))?
        .next()
        .ok_or_else(|| format!("Bad DevTools address {addr}."))?;
    let stream = TcpStream::connect_timeout(&sock, Duration::from_secs(5)).map_err(|e| format!("Cannot reach Chrome at {addr} ({e})."))?;
    let _ = stream.set_nodelay(true);
    let (ws, _) = tungstenite::client(chrome.url.as_str(), stream).map_err(|e| format!("The DevTools handshake failed ({e})."))?;
    ws.get_ref().set_read_timeout(Some(POLL)).map_err(|e| format!("Cannot configure the DevTools socket ({e})."))?;
    Ok(ws)
}

/// What a response answers, with the page it belongs to.
enum Pending {
    Targets,
    Create(u32),
    Attach(u32),
    /// `Page.enable` of the page and the session it was sent on.
    Enable(u32, String),
    History(u32, i64),
    Other(&'static str),
    /// A `PageCmd::Call`. The answer goes back to the caller.
    Call(Sender<Result<Value, String>>),
}

/// The DevTools connection with the commands that wait for an answer.
struct Conn {
    ws: WebSocket<TcpStream>,
    next_id: u64,
    pending: HashMap<u64, Pending>,
}

impl Conn {
    fn send(&mut self, method: &str, params: Value, session: Option<&str>, pending: Pending) -> Result<(), String> {
        let id = self.next_id;
        self.next_id += 1;
        self.pending.insert(id, pending);
        let text = cdp::encode(id, method, params, session);
        self.ws.send(WsMessage::text(text)).map_err(|e| format!("Cannot send {method} to Chrome ({e})."))
    }

    fn apply_size(&mut self, session: &str, size: Size, scale: f32) -> Result<(), String> {
        let s = Some(session);
        self.send("Emulation.setDeviceMetricsOverride", cdp::device_metrics(size, scale), s, Pending::Other("metrics"))?;
        self.send("Page.stopScreencast", json!({}), s, Pending::Other("stop screencast"))?;
        self.send("Page.startScreencast", cdp::screencast_params(size, scale), s, Pending::Other("screencast"))
    }

    fn close_target(&mut self, target: &str) -> Result<(), String> {
        self.send("Target.closeTarget", json!({ "targetId": target }), None, Pending::Other("close page"))
    }
}

/// The worker's side of one page.
struct PageState {
    shared: Arc<Shared>,
    /// Flat session of the attached page.
    session: Option<String>,
    /// `Page.enable` answered. Until then the page takes no commands, see
    /// the answer to `Pending::Enable`.
    ready: bool,
    target: Option<String>,
    main_frame: Option<String>,
    size: Size,
    scale: f32,
    /// Navigation asked for before the page was ready.
    queued_url: Option<String>,
    seq: u64,
    /// A frame acknowledgement held back while nobody takes frames.
    held_ack: Option<i64>,
    /// When the title and the history were last read.
    last_title: Instant,
    /// Chrome refused to open the page. Its commands are ignored.
    failed: bool,
}

impl PageState {
    fn new(shared: Arc<Shared>, size: Size, scale: f32, queued_url: Option<String>) -> Self {
        Self {
            shared,
            session: None,
            ready: false,
            target: None,
            main_frame: None,
            size,
            scale,
            queued_url,
            seq: 0,
            held_ack: None,
            last_title: Instant::now(),
            failed: false,
        }
    }
}

/// A mouse move followed by another move on the same page is outdated.
fn superseded(cmd: &Cmd, next: Option<&Cmd>) -> bool {
    match (cmd, next) {
        (Cmd::Page(a, PageCmd::Mouse(m)), Some(Cmd::Page(b, PageCmd::Mouse(n)))) => {
            a == b && m.kind == MouseKind::Move && n.kind == MouseKind::Move
        }
        _ => false,
    }
}

struct Session {
    conn: Conn,
    /// The live pages by id. Pages are removed when their handle is dropped.
    pages: BTreeMap<u32, PageState>,
    /// The page started with Chrome. It takes the page Chrome opened itself.
    first: u32,
    /// `Target.getTargets` answered and the page Chrome opened itself is
    /// claimed, so further pages may open.
    targets_known: bool,
    /// Commands taken from the channel and not handled yet.
    queue: VecDeque<Cmd>,
}

impl Session {
    fn new(ws: WebSocket<TcpStream>, first: u32, shared: Arc<Shared>, size: Size, scale: f32) -> Self {
        let mut pages = BTreeMap::new();
        pages.insert(first, PageState::new(shared, size, scale, None));
        Self { conn: Conn { ws, next_id: 1, pending: HashMap::new() }, pages, first, targets_known: false, queue: VecDeque::new() }
    }

    /// The pages that get the final status: the live ones and the ones whose
    /// opening was still queued when the worker stopped.
    fn into_pages(self) -> Vec<Arc<Shared>> {
        let queued = self.queue.into_iter().filter_map(|cmd| match cmd {
            Cmd::Open { shared, .. } => Some(shared),
            _ => None,
        });
        self.pages.into_values().map(|p| p.shared).chain(queued).collect()
    }

    /// Runs until the browser is closed (`Ok` with the reason) or fails.
    fn run(&mut self, rx: &Receiver<Cmd>, chrome: &mut Chrome) -> Result<String, String> {
        self.conn.send("Target.setDiscoverTargets", json!({ "discover": true }), None, Pending::Other("discover"))?;
        self.conn.send("Target.getTargets", json!({}), None, Pending::Targets)?;
        let mut last_check = Instant::now();
        loop {
            loop {
                match rx.try_recv() {
                    Ok(cmd) => self.queue.push_back(cmd),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.queue.push_back(Cmd::Close);
                        break;
                    }
                }
            }
            while let Some(cmd) = self.queue.pop_front() {
                // Only the newest of several queued mouse moves matters.
                if !superseded(&cmd, self.queue.front()) && self.command(cmd)? {
                    let _ = self.conn.send("Browser.close", json!({}), None, Pending::Other("close"));
                    return Ok("The browser was closed.".into());
                }
            }
            // Messages until the socket has nothing more, at most a few so commands stay fresh.
            for _ in 0..32 {
                match self.conn.ws.read() {
                    Ok(WsMessage::Text(text)) => self.message(text.as_str())?,
                    Ok(WsMessage::Close(_)) => {
                        return Err(chrome.exited().unwrap_or_else(|| "Chrome closed the DevTools connection.".into()));
                    }
                    Ok(_) => {}
                    Err(tungstenite::Error::Io(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => break,
                    Err(e) => {
                        std::thread::sleep(Duration::from_millis(100));
                        return Err(chrome.exited().unwrap_or_else(|| format!("The DevTools connection broke ({e}).")));
                    }
                }
            }
            for (&id, p) in self.pages.iter_mut() {
                let shown = lock(&p.shared.taken).elapsed() < IDLE;
                if p.held_ack.is_some() && shown {
                    let ack = p.held_ack.take();
                    self.conn.send("Page.screencastFrameAck", json!({ "sessionId": ack }), p.session.as_deref(), Pending::Other("ack"))?;
                }
                // Pages change their title by script without telling, so ask now and then while the tile shows.
                if p.last_title.elapsed() > TITLE_EVERY && p.ready && shown {
                    p.last_title = Instant::now();
                    self.conn.send("Page.getNavigationHistory", json!({}), p.session.as_deref(), Pending::History(id, 0))?;
                }
            }
            if last_check.elapsed() > Duration::from_millis(250) {
                last_check = Instant::now();
                if let Some(text) = chrome.exited() {
                    return Err(text);
                }
            }
        }
    }

    /// Handles one command. Returns true when Chrome is to close.
    fn command(&mut self, cmd: Cmd) -> Result<bool, String> {
        match cmd {
            Cmd::Page(id, cmd) => self.page_command(id, cmd)?,
            Cmd::Open { page, shared, size, scale, url } => {
                self.pages.insert(page, PageState::new(shared, size, scale, url));
                if self.targets_known {
                    self.create(page)?;
                }
            }
            Cmd::ClosePage(id) => {
                let Some(p) = self.pages.remove(&id) else { return Ok(false) };
                // A Remote of this page may still ask for its status.
                p.shared.set_final(&Status::Exited("The page was closed.".into()));
                // The last page is gone, so Chrome closes as a whole.
                if self.pages.is_empty() {
                    return Ok(true);
                }
                // A page that is still opening is closed when Chrome reports its target.
                if let Some(target) = p.target {
                    self.conn.close_target(&target)?;
                }
            }
            Cmd::Close => return Ok(true),
        }
        Ok(false)
    }

    fn page_command(&mut self, id: u32, cmd: PageCmd) -> Result<(), String> {
        let Some(p) = self.pages.get_mut(&id) else { return Ok(()) };
        let Some(session) = p.session.clone().filter(|_| p.ready) else {
            match cmd {
                PageCmd::Navigate(url) => p.queued_url = Some(url),
                PageCmd::Resize(size, scale) => {
                    p.size = size;
                    p.scale = scale;
                }
                PageCmd::Call { reply, .. } => {
                    let text = match &*lock(&p.shared.status) {
                        Status::Failed(e) if p.failed => e.clone(),
                        _ if p.failed => "The page could not be opened.".to_string(),
                        _ => "The page is still loading. Try again in a moment.".to_string(),
                    };
                    let _ = reply.send(Err(text));
                }
                _ => {}
            }
            return Ok(());
        };
        let s = Some(session.as_str());
        let c = &mut self.conn;
        match cmd {
            PageCmd::Navigate(url) => c.send("Page.navigate", json!({ "url": url }), s, Pending::Other("navigate"))?,
            PageCmd::History(step) => c.send("Page.getNavigationHistory", json!({}), s, Pending::History(id, step))?,
            PageCmd::Reload => c.send("Page.reload", json!({}), s, Pending::Other("reload"))?,
            PageCmd::Resize(size, scale) => {
                if (size, scale) != (p.size, p.scale) {
                    p.size = size;
                    p.scale = scale;
                    c.apply_size(&session, size, scale)?;
                }
            }
            PageCmd::Mouse(m) => c.send("Input.dispatchMouseEvent", cdp::mouse_params(&m), s, Pending::Other("mouse"))?,
            PageCmd::Key(key, mods) => {
                for params in cdp::key_events(key, mods) {
                    c.send("Input.dispatchKeyEvent", params, s, Pending::Other("key"))?;
                }
            }
            PageCmd::Text(text) => c.send("Input.insertText", json!({ "text": text }), s, Pending::Other("text"))?,
            PageCmd::Call { method, params, reply } => c.send(&method, params, s, Pending::Call(reply))?,
        }
        Ok(())
    }

    /// Asks Chrome for a new target for page `id`.
    fn create(&mut self, id: u32) -> Result<(), String> {
        let Some(p) = self.pages.get(&id) else { return Ok(()) };
        self.conn.send("Target.createTarget", cdp::create_target_params("about:blank", p.size), None, Pending::Create(id))
    }

    fn attach(&mut self, id: u32, target: String) -> Result<(), String> {
        if let Some(p) = self.pages.get_mut(&id) {
            p.target = Some(target.clone());
        }
        self.conn.send("Target.attachToTarget", json!({ "targetId": target, "flatten": true }), None, Pending::Attach(id))
    }

    /// Page `id` could not open. Without another usable page Chrome itself
    /// failed, which ends the worker with `text`.
    fn fail(&mut self, id: u32, text: String) -> Result<(), String> {
        let Some(p) = self.pages.get_mut(&id) else { return Ok(()) };
        p.failed = true;
        p.session = None;
        p.ready = false;
        let target = p.target.take();
        let shared = p.shared.clone();
        if !self.pages.values().any(|p| !p.failed) {
            return Err(text);
        }
        shared.set_status(Status::Failed(text));
        match target {
            Some(t) => self.conn.close_target(&t),
            None => Ok(()),
        }
    }

    /// The page whose flat session is `session`.
    fn page_of(&self, session: Option<&str>) -> Option<u32> {
        let session = session?;
        self.pages.iter().find(|(_, p)| p.session.as_deref() == Some(session)).map(|(&id, _)| id)
    }

    fn message(&mut self, text: &str) -> Result<(), String> {
        match cdp::decode(text) {
            Ok(Message::Response { id, result, .. }) => {
                let Some(pending) = self.conn.pending.remove(&id) else { return Ok(()) };
                self.response(pending, result)
            }
            Ok(Message::Event { method, params, session }) => self.event(&method, &params, session.as_deref()),
            Err(e) => {
                tracing::debug!("ignoring a CDP message: {e}");
                Ok(())
            }
        }
    }

    fn response(&mut self, pending: Pending, result: Result<Value, String>) -> Result<(), String> {
        match (pending, result) {
            (Pending::Targets, Ok(r)) => {
                self.targets_known = true;
                let found = r
                    .get("targetInfos")
                    .and_then(Value::as_array)
                    .and_then(|list| list.iter().find(|t| t.get("type").and_then(Value::as_str) == Some("page")))
                    .and_then(|t| t.get("targetId").and_then(Value::as_str))
                    .map(str::to_string);
                let first = self.first;
                let mut stray = None;
                match (self.pages.contains_key(&first), found) {
                    (true, Some(target)) => self.attach(first, target)?,
                    (true, None) => self.create(first)?,
                    (false, found) => stray = found,
                }
                // Pages opened before Chrome's own page was claimed.
                let waiting: Vec<u32> = self.pages.keys().copied().filter(|&id| id != first).collect();
                for id in waiting {
                    self.create(id)?;
                }
                // The first page was dropped before it opened, so nobody shows Chrome's own page.
                if let Some(target) = stray {
                    self.conn.close_target(&target)?;
                }
            }
            (Pending::Targets, Err(e)) => return Err(format!("Chrome refused to open a page ({e}).")),
            (Pending::Create(id), Ok(r)) => {
                let target = r.get("targetId").and_then(Value::as_str).map(str::to_string);
                match (self.pages.contains_key(&id), target) {
                    (true, Some(target)) => self.attach(id, target)?,
                    (true, None) => self.fail(id, "Chrome created a page without an id.".into())?,
                    // The page was dropped while it opened.
                    (false, Some(target)) => self.conn.close_target(&target)?,
                    (false, None) => {}
                }
            }
            (Pending::Create(id) | Pending::Attach(id), Err(e)) => self.fail(id, format!("Chrome refused to open a page ({e})."))?,
            (Pending::Attach(id), Ok(r)) => {
                if !self.pages.contains_key(&id) {
                    return Ok(());
                }
                let Some(session) = r.get("sessionId").and_then(Value::as_str).map(str::to_string) else {
                    return self.fail(id, "Chrome attached without a session.".into());
                };
                let Some(p) = self.pages.get_mut(&id) else { return Ok(()) };
                p.session = Some(session.clone());
                self.conn.send("Page.enable", json!({}), Some(&session), Pending::Enable(id, session.clone()))?;
            }
            // Chrome stores that the Page domain is on only once it answers. A navigation
            // sent earlier can commit in a new renderer that never heard of it, and then
            // frameNavigated, loadEventFired and dialogs of that page never arrive.
            (Pending::Enable(id, session), result) => {
                if let Err(e) = result {
                    tracing::debug!("CDP page failed: {e}");
                }
                // The page may be gone or attached anew since.
                let Some(p) = self.pages.get_mut(&id).filter(|p| p.session.as_deref() == Some(session.as_str())) else { return Ok(()) };
                p.ready = true;
                let s = Some(session.as_str());
                let c = &mut self.conn;
                c.send("Emulation.setFocusEmulationEnabled", json!({ "enabled": true }), s, Pending::Other("focus"))?;
                c.apply_size(&session, p.size, p.scale)?;
                if let Some(url) = p.queued_url.take() {
                    c.send("Page.navigate", json!({ "url": url }), s, Pending::Other("navigate"))?;
                }
                c.send("Page.getNavigationHistory", json!({}), s, Pending::History(id, 0))?;
                p.shared.set_status(Status::Running);
            }
            (Pending::History(id, step), Ok(r)) => {
                let Some(p) = self.pages.get(&id) else { return Ok(()) };
                let (entry, back, forward) = cdp::history(&r, step);
                {
                    let mut page = lock(&p.shared.page);
                    page.can_go_back = back;
                    page.can_go_forward = forward;
                    if let Some(title) = cdp::history_title(&r) {
                        page.title = title;
                    }
                }
                if let (Some(entry), Some(session)) = (entry, p.session.as_deref()) {
                    self.conn.send("Page.navigateToHistoryEntry", json!({ "entryId": entry }), Some(session), Pending::Other("history"))?;
                }
                p.shared.wake();
            }
            (Pending::History(..), Err(e)) => tracing::debug!("navigation history failed: {e}"),
            (Pending::Other(what), Err(e)) => tracing::debug!("CDP {what} failed: {e}"),
            (Pending::Other(_), Ok(_)) => {}
            // When the caller stopped waiting, nobody takes the answer.
            (Pending::Call(reply), result) => {
                let _ = reply.send(result);
            }
        }
        Ok(())
    }

    fn event(&mut self, method: &str, params: &Value, session: Option<&str>) -> Result<(), String> {
        let str_of = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        match method {
            // Browser events, routed by the target or the session they name.
            "Target.targetInfoChanged" => {
                let info = params.get("targetInfo").cloned().unwrap_or(Value::Null);
                let target = str_of(&info, "targetId");
                if let Some(p) = self.pages.values().find(|p| p.target.is_some() && p.target == target)
                    && let Some(title) = str_of(&info, "title").filter(|t| !t.is_empty())
                {
                    lock(&p.shared.page).title = title;
                    p.shared.wake();
                }
                return Ok(());
            }
            // The page closed itself (window.close()). Open a new one for the same handle.
            "Target.detachedFromTarget" => {
                let Some(id) = self.page_of(str_of(params, "sessionId").as_deref()) else { return Ok(()) };
                if let Some(p) = self.pages.get_mut(&id) {
                    p.session = None;
                    p.ready = false;
                    p.target = None;
                    p.main_frame = None;
                    p.held_ack = None;
                    p.shared.set_status(Status::Starting);
                }
                return self.create(id);
            }
            _ => {}
        }
        let Some(id) = self.page_of(session) else { return Ok(()) };
        let Some(p) = self.pages.get_mut(&id) else { return Ok(()) };
        let s = p.session.clone();
        let s = s.as_deref();
        let c = &mut self.conn;
        match method {
            "Page.screencastFrame" => {
                let frame = match cdp::screencast_frame(params) {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::debug!("{e}");
                        return Ok(());
                    }
                };
                if lock(&p.shared.taken).elapsed() < IDLE {
                    c.send("Page.screencastFrameAck", json!({ "sessionId": frame.ack }), s, Pending::Other("ack"))?;
                } else {
                    p.held_ack = Some(frame.ack);
                }
                p.seq += 1;
                *lock(&p.shared.frame) =
                    Some(Frame { jpeg: frame.jpeg, seq: p.seq, css_width: frame.css_width, css_height: frame.css_height });
                lock(&p.shared.page).frames += 1;
                p.shared.wake();
            }
            "Page.frameNavigated" => {
                let frame = params.get("frame").cloned().unwrap_or(Value::Null);
                if frame.get("parentId").is_none() {
                    p.main_frame = str_of(&frame, "id");
                    let url = str_of(&frame, "url").unwrap_or_default() + &str_of(&frame, "urlFragment").unwrap_or_default();
                    {
                        let mut page = lock(&p.shared.page);
                        page.url = url;
                        page.crashed = false;
                    }
                    c.send("Page.getNavigationHistory", json!({}), s, Pending::History(id, 0))?;
                }
            }
            "Page.navigatedWithinDocument" if str_of(params, "frameId") == p.main_frame => {
                lock(&p.shared.page).url = str_of(params, "url").unwrap_or_default();
                c.send("Page.getNavigationHistory", json!({}), s, Pending::History(id, 0))?;
            }
            "Page.frameStartedLoading" if p.main_frame.is_none() || str_of(params, "frameId") == p.main_frame => {
                lock(&p.shared.page).loading = true;
                p.shared.wake();
            }
            "Page.frameStoppedLoading" if str_of(params, "frameId") == p.main_frame => {
                lock(&p.shared.page).loading = false;
                p.shared.wake();
            }
            "Page.loadEventFired" => {
                {
                    let mut page = lock(&p.shared.page);
                    page.loading = false;
                    page.loads += 1;
                }
                // The title is known once the page loaded.
                c.send("Page.getNavigationHistory", json!({}), s, Pending::History(id, 0))?;
                p.shared.wake();
            }
            "Page.javascriptDialogOpening" => {
                // Nobody sees the dialog in a headless page, and it would freeze the page:
                // alerts are acknowledged, confirms and prompts are answered with no.
                let kind = str_of(params, "type").unwrap_or_default();
                let accept = !matches!(kind.as_str(), "confirm" | "prompt");
                c.send("Page.handleJavaScriptDialog", json!({ "accept": accept }), s, Pending::Other("dialog"))?;
            }
            "Inspector.targetCrashed" => {
                lock(&p.shared.page).crashed = true;
                p.shared.wake();
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mouse(page: u32, kind: MouseKind) -> Cmd {
        Cmd::Page(page, PageCmd::Mouse(MouseEvent::new(kind, 1.0, 2.0)))
    }

    #[test]
    fn only_the_newest_move_of_a_page_is_sent() {
        assert!(superseded(&mouse(1, MouseKind::Move), Some(&mouse(1, MouseKind::Move))));
        assert!(!superseded(&mouse(1, MouseKind::Move), Some(&mouse(2, MouseKind::Move))), "another page");
        assert!(!superseded(&mouse(1, MouseKind::Move), Some(&mouse(1, MouseKind::Down))));
        assert!(!superseded(&mouse(1, MouseKind::Down), Some(&mouse(1, MouseKind::Move))));
        assert!(!superseded(&mouse(1, MouseKind::Move), None));
        assert!(!superseded(&Cmd::Page(1, PageCmd::Reload), Some(&mouse(1, MouseKind::Move))));
    }

    #[test]
    fn a_page_opened_after_the_end_takes_the_final_status() {
        let common = Common::new();
        let (tx, rx) = mpsc::channel();
        let early = Arc::new(Shared::new());
        common.open(&tx, &early, Cmd::Open { page: 1, shared: early.clone(), size: Size { width: 16, height: 16 }, scale: 1.0, url: None });
        let first = Arc::new(Shared::new());
        common.finish(Status::Exited("gone".into()), vec![first.clone()], Some(&rx));
        assert!(common.done.load(Ordering::Acquire));
        assert_eq!(*lock(&first.status), Status::Exited("gone".into()));
        assert_eq!(*lock(&early.status), Status::Exited("gone".into()), "a page still waiting in the channel");

        let late = Shared::new();
        common.open(&tx, &late, Cmd::Close);
        assert_eq!(*lock(&late.status), Status::Exited("gone".into()), "a page opened after the end");
        assert!(rx.try_recv().is_err(), "nothing reaches the ended worker");

        let failed = Arc::new(Shared::new());
        failed.set_status(Status::Failed("refused".into()));
        Common::new().finish(Status::Exited("gone".into()), vec![failed.clone()], None);
        assert_eq!(*lock(&failed.status), Status::Failed("refused".into()), "a failure is kept");
    }
}
