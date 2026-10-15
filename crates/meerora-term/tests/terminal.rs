//! PTY tests with short-lived `/bin/sh` children.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use meerora_term::palette::{ANSI, BACKGROUND, FOREGROUND};
use meerora_term::{Cell, Snapshot, Spawn, TermSize, Terminal};

const SIZE: TermSize = TermSize { cols: 80, rows: 24 };
const TIMEOUT: Duration = Duration::from_secs(10);

fn sh(script: &str) -> Terminal {
    sh_sized(script, SIZE)
}

fn sh_sized(script: &str, size: TermSize) -> Terminal {
    let spawn = Spawn { program: Some("/bin/sh".into()), args: vec!["-c".into(), script.into()], ..Spawn::default() };
    Terminal::spawn(spawn, size).expect("spawn /bin/sh")
}

/// Polls `check` until it returns Some or the timeout passes.
fn wait_for<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(start.elapsed() < TIMEOUT, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn rows(snapshot: &Snapshot) -> Vec<String> {
    snapshot
        .cells
        .chunks(usize::from(snapshot.size.cols))
        .map(|row| row.iter().filter(|cell| !cell.spacer).map(|cell| cell.ch).collect::<String>().trim_end().to_owned())
        .collect()
}

fn text(snapshot: &Snapshot) -> String {
    rows(snapshot).join("\n")
}

/// Waits until the screen contains `needle` and returns that snapshot.
fn wait_text(term: &Terminal, needle: &str) -> Snapshot {
    let start = Instant::now();
    loop {
        let snapshot = term.snapshot();
        let screen = text(&snapshot);
        if screen.contains(needle) {
            return snapshot;
        }
        assert!(
            start.elapsed() < TIMEOUT,
            "timed out waiting for {needle:?} on screen (alive: {}), screen:\n{}",
            term.is_alive(),
            screen.trim_end()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn find(snapshot: &Snapshot, ch: char) -> Cell {
    *snapshot.cells.iter().find(|cell| cell.ch == ch).unwrap_or_else(|| panic!("{ch:?} not on screen"))
}

#[test]
fn output_shows_up() {
    let term = sh("printf hello");
    let snapshot = wait_text(&term, "hello");
    assert_eq!(snapshot.size, SIZE);
    assert_eq!(snapshot.cells.len(), 80 * 24);
    assert_eq!(rows(&snapshot)[0], "hello");
    let cell = find(&snapshot, 'h');
    assert_eq!((cell.fg, cell.bg), (FOREGROUND, BACKGROUND));
}

#[test]
fn child_gets_term_variables() {
    let term = sh("printf '%s %s' \"$TERM\" \"$COLORTERM\"; sleep 5");
    wait_text(&term, "xterm-256color truecolor");
}

#[test]
fn spawn_applies_cwd_and_env() {
    let spawn = Spawn {
        program: Some("/bin/sh".into()),
        args: vec!["-c".into(), "printf '%s:%s' \"$PWD\" \"$MEERORA_TEST\"; sleep 5".into()],
        cwd: Some("/tmp".into()),
        env: vec![("MEERORA_TEST".into(), "yes".into())],
    };
    let term = Terminal::spawn(spawn, SIZE).unwrap();
    wait_text(&term, "/tmp:yes");
}

#[test]
fn spawn_reports_missing_program() {
    let spawn = Spawn { program: Some("/nonexistent/meerora-test".into()), ..Spawn::default() };
    assert!(Terminal::spawn(spawn, SIZE).is_err());
}

#[test]
fn resize_changes_grid_and_pty() {
    let term = sh("read line; stty size; sleep 5");
    term.resize(TermSize { cols: 100, rows: 30 });
    term.write(b"\n");
    let snapshot = wait_text(&term, "30 100");
    assert_eq!(snapshot.size, TermSize { cols: 100, rows: 30 });
    assert_eq!(snapshot.cells.len(), 100 * 30);
}

#[test]
fn resize_clamps_to_minimum() {
    let term = sh("sleep 5");
    term.resize(TermSize { cols: 0, rows: 0 });
    assert_eq!(term.snapshot().size, TermSize { cols: 2, rows: 1 });
}

#[test]
fn sgr_red_uses_palette() {
    let term = sh(r"printf '\033[31mR'; sleep 5");
    let snapshot = wait_text(&term, "R");
    let cell = find(&snapshot, 'R');
    assert_eq!(cell.fg, [0xE0, 0x67, 0x5A]);
    assert_eq!(cell.bg, BACKGROUND);
}

#[test]
fn colours_resolve() {
    let script = concat!(
        r"printf '\033[1;31mB\033[0m\033[38;5;196mC\033[0m\033[38;5;9mN\033[0m\033[38;5;244mG\033[0m",
        r"\033[38;2;1;2;3mT\033[48;2;10;20;30mU\033[0m\033[48;5;21mK\033[0m\033[7mI\033[0m\033[2mD\033[0m",
        r"\033[1;38;5;2mL\033[0m\033[3;4mS\033[0m'; sleep 5",
    );
    let term = sh(script);
    let snapshot = wait_text(&term, "BCNGTUKIDLS");

    // Bold brightens the 8 base colours.
    let bold = find(&snapshot, 'B');
    assert_eq!(bold.fg, ANSI[9]);
    assert!(bold.bold);
    assert_eq!(find(&snapshot, 'L').fg, ANSI[10]);
    // 256-colour cube, palette entries and grey ramp.
    assert_eq!(find(&snapshot, 'C').fg, [255, 0, 0]);
    assert_eq!(find(&snapshot, 'N').fg, [0xF0, 0x8A, 0x7E]);
    assert_eq!(find(&snapshot, 'G').fg, [128, 128, 128]);
    assert_eq!(find(&snapshot, 'K').bg, [0, 0, 255]);
    // Truecolor.
    assert_eq!(find(&snapshot, 'T').fg, [1, 2, 3]);
    let both = find(&snapshot, 'U');
    assert_eq!((both.fg, both.bg), ([1, 2, 3], [10, 20, 30]));
    // Inverse swaps, dim darkens to two thirds.
    let inverse = find(&snapshot, 'I');
    assert_eq!((inverse.fg, inverse.bg), (BACKGROUND, FOREGROUND));
    assert_eq!(find(&snapshot, 'D').fg, [0x9B, 0x9C, 0xA0]);
    let styled = find(&snapshot, 'S');
    assert!(styled.italic && styled.underline && !styled.bold);
}

#[test]
fn wide_characters_get_a_spacer() {
    let term = sh("printf '\\344\\270\\255x'; sleep 5");
    let snapshot = wait_text(&term, "中x");
    assert_eq!(snapshot.cells[0].ch, '中');
    assert!(!snapshot.cells[0].spacer);
    assert!(snapshot.cells[1].spacer);
    assert_eq!(snapshot.cells[2].ch, 'x');
    assert_eq!(snapshot.cursor, Some((3, 0)));
}

#[test]
fn title_and_cursor_visibility() {
    let term = sh(r"printf '\033]0;meerora title\007abc'; read line; printf '\033[?25l'; sleep 5");
    let snapshot = wait_text(&term, "abc");
    assert_eq!(snapshot.title.as_deref(), Some("meerora title"));
    assert_eq!(snapshot.cursor, Some((3, 0)));

    term.write(b"\n");
    wait_for("hidden cursor", || term.snapshot().cursor.is_none().then_some(()));
}

#[test]
fn scrollback_shows_earlier_lines() {
    let term = sh("seq 1 200; sleep 5");
    let bottom = wait_text(&term, "200");
    assert_eq!(bottom.scroll_offset, 0);
    assert!(bottom.cursor.is_some());
    // 200 lines plus the empty cursor line, the last 24 of them are visible.
    assert_eq!(rows(&bottom)[0], "178");

    term.scroll(100);
    let back = term.snapshot();
    assert_eq!(back.scroll_offset, 100);
    assert_eq!(rows(&back)[0], "78");
    assert_eq!(rows(&back)[23], "101");
    assert_eq!(back.cursor, None, "cursor is scrolled away");

    term.scroll(10_000);
    let top = term.snapshot();
    assert_eq!(top.scroll_offset, 177);
    assert_eq!(rows(&top)[0], "1");

    term.scroll(-10_000);
    assert_eq!(term.snapshot().scroll_offset, 0);
}

#[test]
fn input_scrolls_to_bottom() {
    let term = sh("seq 1 100; read line; sleep 5");
    wait_text(&term, "100");
    term.scroll(10);
    assert_eq!(term.snapshot().scroll_offset, 10);
    term.write(b"x");
    assert_eq!(term.snapshot().scroll_offset, 0);
}

#[test]
fn take_dirty_semantics() {
    let term = sh("read line; seq 1 100; printf done; sleep 5");
    assert!(term.take_dirty(), "a new terminal starts out dirty");
    assert!(!term.take_dirty(), "dirty is reported once");

    term.write(b"go\n");
    wait_text(&term, "done");
    assert!(term.take_dirty(), "output marks dirty");
    assert!(!term.take_dirty());

    term.resize(TermSize { cols: 60, rows: 20 });
    assert!(term.take_dirty(), "resize marks dirty");
    assert!(!term.take_dirty());
    term.resize(TermSize { cols: 60, rows: 20 });
    assert!(!term.take_dirty(), "resize to the same size does not");

    term.scroll(5);
    assert!(term.take_dirty(), "scroll marks dirty");
    assert!(!term.take_dirty());
    term.scroll(0);
    assert!(!term.take_dirty(), "a scroll that does not move the view does not");
}

#[test]
fn wakeup_runs_on_output() {
    let term = sh("read line; printf woken; sleep 5");
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    term.set_wakeup(Box::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    }));
    term.write(b"\n");
    wait_text(&term, "woken");
    wait_for("wakeup call", || (calls.load(Ordering::SeqCst) > 0).then_some(()));
}

#[test]
fn is_alive_turns_false_after_exit() {
    let term = sh("printf bye; exit 3");
    wait_for("child exit", || (!term.is_alive()).then_some(()));
    // The last output was processed before the terminal reported the exit.
    assert!(text(&term.snapshot()).contains("bye"));
    assert!(term.take_dirty());

    let running = sh("sleep 5");
    std::thread::sleep(Duration::from_millis(200));
    assert!(running.is_alive());
}

#[cfg(target_os = "linux")]
#[test]
fn drop_hangs_up_and_reaps_the_child() {
    let term = sh("echo pid=$$; sleep 30");
    let snapshot = wait_text(&term, "pid=");
    let line = rows(&snapshot).into_iter().find(|row| row.starts_with("pid=")).unwrap();
    let proc_dir = std::path::PathBuf::from(format!("/proc/{}", &line[4..]));
    assert!(proc_dir.exists());

    let start = Instant::now();
    drop(term);
    assert!(start.elapsed() < Duration::from_millis(100), "drop must not block");
    wait_for("child to be reaped", || (!proc_dir.exists()).then_some(()));
}

/// Child that reports the bytes it receives as hex, optionally after turning
/// on bracketed paste. `od` sees end of input after 2 s without new bytes.
fn input_dump(bracketed: bool) -> Terminal {
    let enable = if bracketed { r"printf '\033[?2004h'; " } else { "" };
    sh(&format!("stty raw -echo min 0 time 20; {enable}printf ready; od -An -tx1 -v; sleep 5"))
}

#[test]
fn paste_is_bracketed_when_enabled() {
    let term = input_dump(true);
    let snapshot = wait_text(&term, "ready");
    assert!(snapshot.bracketed_paste);
    term.paste("a\x1bb");
    wait_text(&term, "1b 5b 32 30 30 7e 61 62 1b 5b 32 30 31 7e");
}

#[test]
fn paste_is_plain_when_not_enabled() {
    let term = input_dump(false);
    let snapshot = wait_text(&term, "ready");
    assert!(!snapshot.bracketed_paste);
    term.paste("a\nb\r\nc");
    let snapshot = wait_text(&term, "61 0d 62 0d 63");
    assert!(!text(&snapshot).contains("1b 5b"));
}

#[test]
fn app_cursor_mode_is_reported() {
    let term = sh(r"printf '\033[?1hX'; sleep 5");
    let snapshot = wait_text(&term, "X");
    assert!(snapshot.app_cursor);
}

/// `text` returns the screen with the scrollback above it, joins wrapped
/// rows and leaves out the empty end, for agents that read the terminal (D30).
#[test]
fn text_has_the_scrollback_and_joins_wrapped_rows() {
    let size = TermSize { cols: 20, rows: 5 };
    let long = "x".repeat(30);
    let term = sh_sized(&format!("for i in 1 2 3 4 5 6 7 8 9 10; do echo line$i; done; echo {long}; echo end; sleep 5"), size);
    wait_text(&term, "end");
    let all = term.text(100);
    let lines: Vec<&str> = all.lines().collect();
    assert_eq!(lines.first(), Some(&"line1"), "{all}");
    assert!(lines.contains(&long.as_str()), "a wrapped row is one line: {all}");
    assert_eq!(lines.last(), Some(&"end"), "no empty rows at the end: {all}");
    let short = term.text(0);
    assert!(!short.lines().any(|l| l == "line1") && short.ends_with("end"), "only the screen: {short}");
}
