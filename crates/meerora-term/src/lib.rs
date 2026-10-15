//! PTY plus terminal emulation for the terminal tile of the agent view (D11,
//! D13). No rendering here. The app draws [`Snapshot`]s and feeds keys back.
//!
//! The PTY is the handle an agent will drive later (D13), which is why input
//! can come from the user ([`Terminal::write`]) and from the agent daemon
//! (`Event::TerminalInput`) alike.
//!
//! The PTY and its I/O thread come from `alacritty_terminal` (its `tty` module
//! and `EventLoop`). The loop reads with non-blocking I/O, parses into the
//! shared [`alacritty_terminal::Term`], queues input and resizes from any
//! thread through a channel and notices the child's exit through `SIGCHLD`.

mod keys;
pub mod palette;

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::tty;
use anyhow::Context as _;

/// Lines of scrollback kept above the screen.
pub const SCROLLBACK_LINES: usize = 5000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TermSize {
    pub cols: u16,
    pub rows: u16,
}

/// What to run. `program: None` runs `$SHELL` (or `/bin/sh`) as a login shell.
///
/// The child gets `TERM=xterm-256color` and `COLORTERM=truecolor`. Entries in
/// `env` are applied afterwards and may override them.
#[derive(Clone, Debug, Default)]
pub struct Spawn {
    pub program: Option<String>,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
}

/// One cell with colours already resolved (bold-bright, inverse, dim applied).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cell {
    pub ch: char,
    /// sRGB.
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    /// Second half of a wide character. Draw nothing.
    pub spacer: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub size: TermSize,
    /// Row-major, `size.rows * size.cols` cells, of the visible screen
    /// (taking the scrollback offset into account).
    pub cells: Vec<Cell>,
    /// (col, row) when the cursor is visible and not scrolled away.
    pub cursor: Option<(u16, u16)>,
    pub title: Option<String>,
    /// DECCKM, cursor keys send SS3 sequences.
    pub app_cursor: bool,
    pub bracketed_paste: bool,
    /// Lines scrolled back from the bottom.
    pub scroll_offset: usize,
}

/// Keys the app forwards. Plain text goes through [`Terminal::write`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Enter,
    Backspace,
    Tab,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    F(u8),
    /// A character pressed together with Ctrl and/or Alt.
    Char(char),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

/// xterm byte sequence for a key, None if the key sends nothing.
///
/// Only F1 to F12 are encoded. Other function keys return None.
pub fn encode_key(key: Key, mods: Mods, app_cursor: bool) -> Option<Vec<u8>> {
    keys::encode(key, mods, app_cursor)
}

/// A child program on a PTY plus the terminal state its output builds.
///
/// Every method returns quickly. The terminal state is shared with the reader
/// thread, which holds it for at most one chunk of parsing.
pub struct Terminal {
    term: Arc<FairMutex<Term<Listener>>>,
    sender: EventLoopSender,
    shared: Arc<Shared>,
}

impl Terminal {
    pub fn spawn(spawn: Spawn, size: TermSize) -> anyhow::Result<Terminal> {
        let size = clamp(size);
        let options = pty_options(spawn)?;
        let shared = Arc::new(Shared::new(size));

        let config = Config { scrolling_history: SCROLLBACK_LINES, ..Config::default() };
        let term_listener = Listener { shared: shared.clone(), owned_by_loop: false };
        let term = Arc::new(FairMutex::new(Term::new(config, &GridSize(size), term_listener)));

        let pty = tty::new(&options, window_size(size), 0).context("could not start the program")?;
        let loop_listener = Listener { shared: shared.clone(), owned_by_loop: true };
        let event_loop = EventLoop::new(term.clone(), loop_listener, pty, true, false).context("could not create the PTY event loop")?;
        let sender = event_loop.channel();
        let _ = shared.sender.set(sender.clone());
        // Detached, so the PTY is dropped (and the child reaped) on the
        // reader thread and never on the UI thread.
        drop(event_loop.spawn());

        Ok(Terminal { term, sender, shared })
    }

    /// Called from the reader thread after new output was processed, so the
    /// UI can request a repaint.
    ///
    /// The callback may run while the terminal state is locked. Keep it short
    /// (for example `egui::Context::request_repaint`) and do not call back
    /// into this terminal from it.
    pub fn set_wakeup(&self, wakeup: Box<dyn Fn() + Send + Sync>) {
        *lock(&self.shared.wakeup) = Some(Arc::from(wakeup));
    }

    /// Bytes for the child, for example typed text or [`encode_key`] output.
    ///
    /// Scrolls the view back to the bottom like xterm does on input. The
    /// bytes are queued and written by the reader thread, so this never
    /// blocks on a child that does not read.
    pub fn write(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.scroll_to_bottom();
        self.shared.write(bytes.to_vec());
    }

    /// Pastes text, wrapped in bracketed-paste markers when the program asked for them.
    ///
    /// Inside the markers ESC and ETX are dropped so the text cannot end the
    /// paste early. Without them line breaks are sent as `\r`, like Enter.
    pub fn paste(&self, text: &str) {
        let bracketed = self.with_term(|term| term.mode().contains(TermMode::BRACKETED_PASTE));
        let payload = if bracketed {
            let mut payload = String::with_capacity(text.len() + 12);
            payload.push_str("\x1b[200~");
            payload.extend(text.chars().filter(|c| !matches!(c, '\x1b' | '\x03')));
            payload.push_str("\x1b[201~");
            payload
        } else {
            text.replace("\r\n", "\r").replace('\n', "\r")
        };
        self.write(payload.as_bytes());
    }

    /// Resizes the grid and the PTY (the child gets SIGWINCH).
    ///
    /// Sizes below 2 columns or 1 row are raised to that minimum.
    pub fn resize(&self, size: TermSize) {
        let size = clamp(size);
        if self.shared.swap_size(size) == size {
            return;
        }
        // Grid first, so whatever the child prints after SIGWINCH lands in the new grid.
        self.with_term(|term| term.resize(GridSize(size)));
        let _ = self.sender.send(Msg::Resize(window_size(size)));
        self.shared.dirty.store(true, Ordering::Release);
    }

    /// Positive scrolls back into history, negative towards the bottom.
    pub fn scroll(&self, lines: i32) {
        self.scroll_display(Scroll::Delta(lines));
    }

    pub fn snapshot(&self) -> Snapshot {
        let mut snapshot = self.with_term(|term| screen(term));
        snapshot.title = lock(&self.shared.title).clone();
        snapshot
    }

    /// The text of the screen with up to `history` lines of scrollback
    /// above it, for agents that read the terminal (D30). Rows are trimmed,
    /// rows the program wrapped are joined and empty rows at the end are left
    /// out. The scroll position of the view does not matter.
    pub fn text(&self, history: usize) -> String {
        self.with_term(|term| text(term, history))
    }

    /// True once since the last call when the screen changed.
    ///
    /// New output, a resize, a scroll that moved the view, a title change and
    /// the exit of the child all count. A new terminal starts out dirty.
    pub fn take_dirty(&self) -> bool {
        self.shared.dirty.swap(false, Ordering::AcqRel)
    }

    /// False once the child exited and its last output was processed, or
    /// when the reader thread stopped.
    pub fn is_alive(&self) -> bool {
        self.shared.alive.load(Ordering::Acquire)
    }

    fn scroll_to_bottom(&self) {
        self.scroll_display(Scroll::Bottom);
    }

    fn scroll_display(&self, scroll: Scroll) {
        let moved = self.with_term(|term| {
            let before = term.grid().display_offset();
            term.scroll_display(scroll);
            term.grid().display_offset() != before
        });
        if moved {
            self.shared.dirty.store(true, Ordering::Release);
        }
    }

    /// Runs `f` with the terminal state locked.
    ///
    /// The lease is held for the whole call, not only while locking. While
    /// the reader thread holds the lease it can therefore always take the
    /// state lock without waiting. Otherwise `EventLoop::pty_read` reads
    /// again while waiting, and once the child is gone that read fails with
    /// EIO and the bytes read before are dropped unparsed.
    fn with_term<R>(&self, f: impl FnOnce(&mut Term<Listener>) -> R) -> R {
        let _lease = self.term.lease();
        let mut term = self.term.lock_unfair();
        f(&mut term)
    }
}

/// The visible screen of `term`, without the title.
fn screen(term: &Term<Listener>) -> Snapshot {
    let grid = term.grid();
    let cols = grid.columns();
    let rows = grid.screen_lines();
    let offset = grid.display_offset();
    let colors = term.colors();

    let blank = palette::resolve(&Default::default(), colors);
    let mut cells = Vec::with_capacity(cols * rows);
    for row in 0..rows {
        let line = &grid[Line(row as i32 - offset as i32)];
        let len = line.len();
        cells.extend((0..cols).map(|col| if col < len { palette::resolve(&line[Column(col)], colors) } else { blank }));
    }

    let mode = *term.mode();
    let point = grid.cursor.point;
    let cursor_row = usize::try_from(point.line.0 + offset as i32).ok().filter(|&r| r < rows);
    let cursor = match cursor_row {
        Some(row) if mode.contains(TermMode::SHOW_CURSOR) => {
            let mut col = point.column.0.min(cols - 1);
            if col > 0 && grid[point.line][Column(col)].flags.contains(Flags::WIDE_CHAR_SPACER) {
                col -= 1;
            }
            Some((col as u16, row as u16))
        }
        _ => None,
    };

    Snapshot {
        size: TermSize { cols: cols as u16, rows: rows as u16 },
        cells,
        cursor,
        title: None,
        app_cursor: mode.contains(TermMode::APP_CURSOR),
        bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
        scroll_offset: offset,
    }
}

/// See [`Terminal::text`].
fn text(term: &Term<Listener>, history: usize) -> String {
    let grid = term.grid();
    let cols = grid.columns();
    let top = -(grid.history_size().min(history) as i32);
    let mut lines: Vec<String> = Vec::new();
    let mut joined = false;
    for n in top..grid.screen_lines() as i32 {
        let row = &grid[Line(n)];
        let len = row.len().min(cols);
        let cells = (0..len).map(|col| &row[Column(col)]);
        let text: String = cells.filter(|c| !c.flags.contains(Flags::WIDE_CHAR_SPACER)).map(|c| c.c).collect();
        let wraps = len > 0 && row[Column(len - 1)].flags.contains(Flags::WRAPLINE);
        match lines.last_mut() {
            Some(last) if joined => last.push_str(&text),
            _ => lines.push(text),
        }
        if !wraps && let Some(last) = lines.last_mut() {
            last.truncate(last.trim_end().len());
        }
        joined = wraps;
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

impl Drop for Terminal {
    /// Stops the reader thread, which then hangs up the child and reaps it.
    fn drop(&mut self) {
        let _ = self.sender.send(Msg::Shutdown);
    }
}

const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<Terminal>();
};

fn pty_options(spawn: Spawn) -> anyhow::Result<tty::Options> {
    let (program, args) = match spawn.program {
        Some(program) => (program, spawn.args),
        None => {
            let shell = std::env::var("SHELL").ok().filter(|shell| !shell.is_empty()).unwrap_or_else(|| "/bin/sh".to_owned());
            let args = std::iter::once("-l".to_owned()).chain(spawn.args).collect();
            (shell, args)
        }
    };
    if let Some(cwd) = &spawn.cwd {
        anyhow::ensure!(cwd.is_dir(), "working directory {} is not a directory", cwd.display());
    }

    let mut env = HashMap::from([("TERM".to_owned(), "xterm-256color".to_owned()), ("COLORTERM".to_owned(), "truecolor".to_owned())]);
    env.extend(spawn.env);

    Ok(tty::Options { shell: Some(tty::Shell::new(program, args)), working_directory: spawn.cwd, drain_on_exit: true, env })
}

fn clamp(size: TermSize) -> TermSize {
    TermSize { cols: size.cols.max(2), rows: size.rows.max(1) }
}

fn window_size(size: TermSize) -> WindowSize {
    WindowSize { num_lines: size.rows, num_cols: size.cols, cell_width: 0, cell_height: 0 }
}

/// Grid dimensions for `Term::new` and `Term::resize`.
struct GridSize(TermSize);

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn screen_lines(&self) -> usize {
        usize::from(self.0.rows)
    }

    fn columns(&self) -> usize {
        usize::from(self.0.cols)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

type Wakeup = Arc<dyn Fn() + Send + Sync>;

/// State shared between the [`Terminal`], the event listeners and the reader thread.
struct Shared {
    dirty: AtomicBool,
    alive: AtomicBool,
    /// Columns in the high and rows in the low 16 bits.
    size: AtomicU32,
    title: Mutex<Option<String>>,
    wakeup: Mutex<Option<Wakeup>>,
    /// Set right after the event loop was created, before it runs.
    sender: OnceLock<EventLoopSender>,
}

impl Shared {
    fn new(size: TermSize) -> Self {
        Shared {
            dirty: AtomicBool::new(true),
            alive: AtomicBool::new(true),
            size: AtomicU32::new(pack(size)),
            title: Mutex::new(None),
            wakeup: Mutex::new(None),
            sender: OnceLock::new(),
        }
    }

    fn size(&self) -> TermSize {
        let packed = self.size.load(Ordering::Acquire);
        TermSize { cols: (packed >> 16) as u16, rows: packed as u16 }
    }

    /// Stores a new size and returns the previous one.
    fn swap_size(&self, size: TermSize) -> TermSize {
        let packed = self.size.swap(pack(size), Ordering::AcqRel);
        TermSize { cols: (packed >> 16) as u16, rows: packed as u16 }
    }

    fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        let bytes = bytes.into();
        if let Some(sender) = self.sender.get().filter(|_| !bytes.is_empty()) {
            let _ = sender.send(Msg::Input(bytes));
        }
    }

    fn wake(&self) {
        let wakeup = lock(&self.wakeup).clone();
        if let Some(wakeup) = wakeup {
            wakeup();
        }
    }

    /// Marks the terminal as dead. Returns true for the first call.
    fn mark_dead(&self) -> bool {
        let was_alive = self.alive.swap(false, Ordering::AcqRel);
        if was_alive {
            self.dirty.store(true, Ordering::Release);
        }
        was_alive
    }
}

fn pack(size: TermSize) -> u32 {
    (u32::from(size.cols) << 16) | u32::from(size.rows)
}

/// Receives the events of the `Term` and of the event loop.
struct Listener {
    shared: Arc<Shared>,
    /// The event loop owns this listener. Dropping it means the reader thread
    /// ended, for whatever reason, so the terminal is dead from then on.
    owned_by_loop: bool,
}

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        let shared = &self.shared;
        match event {
            TermEvent::Wakeup => {
                shared.dirty.store(true, Ordering::Release);
                shared.wake();
            }
            TermEvent::Title(title) => {
                *lock(&shared.title) = Some(title);
                shared.dirty.store(true, Ordering::Release);
            }
            TermEvent::ResetTitle => {
                *lock(&shared.title) = None;
                shared.dirty.store(true, Ordering::Release);
            }
            TermEvent::PtyWrite(text) => shared.write(text.into_bytes()),
            TermEvent::ColorRequest(index, format) => {
                let [r, g, b] = palette::slot(index);
                let rgb = alacritty_terminal::vte::ansi::Rgb { r, g, b };
                shared.write(format(rgb).into_bytes());
            }
            TermEvent::TextAreaSizeRequest(format) => {
                shared.write(format(window_size(shared.size())).into_bytes());
            }
            // Sent by the event loop after the remaining output was drained.
            // A Wakeup follows right away.
            TermEvent::Exit => {
                shared.mark_dead();
            }
            _ => {}
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        if self.owned_by_loop && self.shared.mark_dead() {
            self.shared.wake();
        }
    }
}
