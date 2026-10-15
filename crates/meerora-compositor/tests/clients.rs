//! Real clients against the compositor. Every client gets a home of its
//! own with `HISTFILE=/dev/null`, the test desktop's `WAYLAND_DISPLAY`,
//! `DISPLAY` and session bus, and never the user's, so nothing opens on
//! the host desktop or writes into the user's home. A test skips with a
//! note when its client is missing.

use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use meerora_compositor::{BTN_LEFT, BTN_MIDDLE, Compositor, Cursor, Event, LaunchOptions, Realm, SandboxOptions, X11};

/// Linux evdev key codes used below.
const KEY_ENTER: u32 = 28;
const KEY_LEFTCTRL: u32 = 29;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_SPACE: u32 = 57;
const KEY_A: u32 = 30;
const KEY_C: u32 = 46;
const KEY_M: u32 = 50;
const KEY_P: u32 = 25;
const KEY_V: u32 = 47;

fn installed(program: &str) -> bool {
    meerora_compositor::launch::installed(program)
}

fn letter(c: char) -> u32 {
    const ROWS: [(&str, u32); 3] = [("qwertyuiop", 16), ("asdfghjkl", 30), ("zxcvbnm", 44)];
    ROWS.iter().find_map(|(row, first)| row.find(c).map(|i| first + i as u32)).unwrap_or_else(|| panic!("no key for {c:?}"))
}

/// A compositor with one desktop, the clients started on it and a scratch folder.
struct Harness {
    comp: Compositor,
    agent: String,
    children: Vec<Child>,
    dir: PathBuf,
}

impl Harness {
    fn new(name: &str) -> Harness {
        let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("off"));
        let _ = tracing_subscriber::fmt().with_env_filter(filter).with_test_writer().try_init();
        let agent = format!("test-{name}-{}", std::process::id());
        let mut comp = Compositor::new().expect("compositor");
        comp.add_desktop(&agent).expect("desktop");
        let dir = std::env::temp_dir().join(format!("meerora-compositor-{agent}"));
        let _ = std::fs::create_dir_all(&dir);
        Harness { comp, agent, children: Vec::new(), dir }
    }

    /// `env` arguments that give a client a home of its own in the scratch
    /// folder, so it writes no history, session or settings into the user's home.
    fn home_env(&self) -> Vec<String> {
        let home = self.dir.join("home");
        let mut env: Vec<String> = vec![format!("HOME={}", home.display()), "HISTFILE=/dev/null".into()];
        for (name, dir) in [
            ("XDG_CONFIG_HOME", ".config"),
            ("XDG_DATA_HOME", ".local/share"),
            ("XDG_STATE_HOME", ".local/state"),
            ("XDG_CACHE_HOME", ".cache"),
        ] {
            let _ = std::fs::create_dir_all(home.join(dir));
            env.push(format!("{name}={}", home.join(dir).display()));
        }
        env
    }

    fn launch(&mut self, program: &str, args: &[&str]) -> usize {
        self.launch_with(program, args, &LaunchOptions::default())
    }

    /// Starts a client in the user realm and returns its index in `children`.
    fn launch_with(&mut self, program: &str, args: &[&str], options: &LaunchOptions) -> usize {
        let mut env = self.home_env();
        env.push(program.to_string());
        env.extend(args.iter().map(|a| a.to_string()));
        let child = self.comp.launch_with(&self.agent, "env", &env, Some(&self.dir), options).expect("launch");
        self.children.push(child);
        self.children.len() - 1
    }

    /// Dispatches and "draws" every window, as the app does each frame,
    /// until `done` holds or `timeout` passes.
    fn pump(&mut self, timeout: Duration, done: impl FnMut(&mut Compositor, &[Event]) -> bool) -> bool {
        self.pump_drawing(timeout, true, done)
    }

    fn pump_drawing(&mut self, timeout: Duration, draw: bool, mut done: impl FnMut(&mut Compositor, &[Event]) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        let mut events = Vec::new();
        loop {
            self.comp.dispatch();
            events.extend(self.comp.take_events());
            if draw {
                for w in self.comp.windows() {
                    let _ = self.comp.frame(w.id);
                    self.comp.drawn(w.id);
                }
            }
            if done(&mut self.comp, &events) {
                return true;
            }
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(8));
        }
    }

    /// The first window that opens and matches `wanted`.
    fn window_where(&mut self, timeout: Duration, wanted: impl Fn(&meerora_compositor::WindowInfo) -> bool) -> u32 {
        let mut id = None;
        let found = self.pump(timeout, |c, events| {
            id = events
                .iter()
                .filter_map(|e| if let Event::Opened(id) = e { Some(*id) } else { None })
                .find(|id| c.window(*id).is_some_and(|w| wanted(&w)));
            id.is_some()
        });
        assert!(found, "no matching window appeared within {timeout:?}, windows: {:?}", self.comp.windows());
        id.unwrap()
    }

    fn first_window(&mut self, timeout: Duration) -> u32 {
        self.window_where(timeout, |_| true)
    }

    fn frame_size(&mut self, id: u32) -> Option<(u32, u32)> {
        self.comp.frame(id).map(|f| logical(f.width, f.height, f.scale))
    }

    fn tap(&mut self, id: u32, key: u32) {
        self.comp.key(id, key, true);
        self.comp.key(id, key, false);
    }

    fn chord(&mut self, id: u32, keys: &[u32]) {
        for key in keys {
            self.comp.key(id, *key, true);
        }
        for key in keys.iter().rev() {
            self.comp.key(id, *key, false);
        }
    }

    fn type_text(&mut self, id: u32, text: &str) {
        for c in text.chars() {
            let key = if c == ' ' { KEY_SPACE } else { letter(c) };
            self.tap(id, key);
        }
    }

    fn click(&mut self, id: u32, button: u32, x: f64, y: f64) {
        self.comp.pointer_motion(id, x, y);
        self.comp.pointer_button(id, button, true);
        self.comp.pointer_button(id, button, false);
    }

    fn wait_for_file(&mut self, path: &Path) -> bool {
        let path = path.to_path_buf();
        self.pump(Duration::from_secs(15), |_, _| path.exists())
    }

    fn settle(&mut self, time: Duration) {
        self.pump(time, |_, _| false);
    }
}

impl Drop for Harness {
    /// Removing the desktop disconnects the clients, which then exit on
    /// their own, and ends its X servers and buses. What still runs after a
    /// few seconds is killed.
    fn drop(&mut self) {
        self.comp.remove_desktop(&self.agent);
        let deadline = Instant::now() + Duration::from_secs(5);
        for child in &mut self.children {
            while child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            meerora_compositor::launch::stop(child, Duration::from_millis(200));
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn logical(width: u32, height: u32, scale: f32) -> (u32, u32) {
    ((width as f32 / scale).round() as u32, (height as f32 / scale).round() as u32)
}

/// Within one character cell of the wanted size.
fn near(size: (u32, u32), want: (u32, u32)) -> bool {
    size.0.abs_diff(want.0) <= 20 && size.1.abs_diff(want.1) <= 24
}

/// The image has visible pixels and is not one flat colour.
fn not_blank(pixels: &[u8]) -> bool {
    let (px, _) = pixels.as_chunks::<4>();
    px.iter().any(|p| p[3] != 0) && px.iter().any(|p| p != &px[0])
}

#[test]
fn weston_terminal_shows_resizes_types_pastes_copies_and_closes() {
    if !installed("weston-terminal") {
        eprintln!("skipped: weston-terminal is not installed");
        return;
    }
    let mut h = Harness::new("weston");
    h.launch("weston-terminal", &["--shell=/bin/sh"]);
    let id = h.first_window(Duration::from_secs(15));
    let info = h.comp.window(id).expect("window info");
    assert!(info.desktop.starts_with("test-weston") && !info.x11 && !info.sandboxed, "{info:?}");

    // The tile size reaches the client and comes back as the frame size.
    // weston-terminal rounds to whole character cells.
    h.comp.configure(id, 640, 400);
    let ok = h.pump(Duration::from_secs(10), |c, _| c.frame(id).is_some_and(|f| near((f.width, f.height), (640, 400))));
    assert!(ok, "frame is not about 640x400 but {:?}, window {:?}", h.frame_size(id), h.comp.window(id));
    let frame = h.comp.frame(id).unwrap();
    assert!(not_blank(frame.pixels), "the frame is empty");
    let serial = frame.serial;
    h.comp.configure(id, 520, 330);
    assert!(
        h.pump(Duration::from_secs(10), |c, _| c.frame(id).is_some_and(|f| near((f.width, f.height), (520, 330)))),
        "resize did not arrive"
    );
    assert!(h.comp.frame(id).unwrap().serial > serial, "a new commit gives a new serial");
    assert!(near(h.frame_size(id).unwrap(), (520, 330)));

    // Keys reach the shell, which creates a file.
    h.comp.focus(Some(id));
    h.settle(Duration::from_millis(300));
    h.type_text(id, "touch typed");
    h.tap(id, KEY_ENTER);
    assert!(h.wait_for_file(&h.dir.join("typed")), "typed keys did not reach weston-terminal");

    // Without its tile drawn the window still gets a frame callback now and
    // then, so it keeps drawing and running while meerora is hidden.
    let before = h.comp.frame(id).unwrap().serial;
    h.tap(id, letter('y'));
    let drew = h.pump_drawing(Duration::from_secs(5), false, |c, _| c.frame(id).is_some_and(|f| f.serial > before));
    assert!(drew, "a hidden window drew nothing");

    // The app's clipboard pastes into the window (Ctrl+Shift+V in weston-terminal).
    h.tap(id, KEY_ENTER);
    h.comp.set_selection("touch pasted\n");
    h.chord(id, &[KEY_LEFTCTRL, KEY_LEFTSHIFT, KEY_V]);
    assert!(h.wait_for_file(&h.dir.join("pasted")), "the paste did not reach weston-terminal");

    // Selecting with the pointer and copying (Ctrl+Shift+C) offers the text to the app.
    h.type_text(id, "echo meerora");
    h.tap(id, KEY_ENTER);
    h.settle(Duration::from_millis(500));
    // weston-terminal draws its own frame, the text starts below its title bar.
    h.comp.pointer_motion(id, 20.0, 60.0);
    h.comp.pointer_button(id, BTN_LEFT, true);
    for x in (20..500).step_by(40) {
        h.comp.pointer_motion(id, f64::from(x), 300.0);
        h.settle(Duration::from_millis(20));
    }
    h.comp.pointer_button(id, BTN_LEFT, false);
    h.settle(Duration::from_millis(200));
    h.chord(id, &[KEY_LEFTCTRL, KEY_LEFTSHIFT, KEY_C]);
    let mut copied = Vec::new();
    h.pump(Duration::from_secs(5), |_, events| {
        copied = events.iter().filter_map(|e| if let Event::Selection(t) = e { Some(t.clone()) } else { None }).collect();
        copied.iter().any(|t| t.contains("meerora"))
    });
    assert!(copied.iter().any(|t| t.contains("meerora")), "the copy did not reach the app, got {copied:?}");

    // weston-terminal draws its own cursor image, which reaches the app.
    h.comp.pointer_motion(id, 200.0, 200.0);
    let image = h.pump(Duration::from_secs(5), |c, _| matches!(c.cursor(), Cursor::Image(i) if i.width > 0 && not_blank(i.pixels)));
    assert!(image, "no cursor image from weston-terminal");

    // The close request ends it, and the window goes away.
    h.comp.close(id);
    assert!(h.pump(Duration::from_secs(10), |_, events| events.contains(&Event::Closed(id))), "close did not remove the window");
    assert!(h.comp.window(id).is_none());
}

#[test]
fn removing_a_desktop_disconnects_its_clients() {
    if !installed("weston-terminal") {
        eprintln!("skipped: weston-terminal is not installed");
        return;
    }
    let mut h = Harness::new("remove");
    h.launch("weston-terminal", &["--shell=/bin/sh"]);
    let id = h.window_where(Duration::from_secs(15), |w| !w.x11);
    // An X11 client too, whose last events may still be queued when the desktop goes.
    let x_events = installed("Xwayland").then(|| {
        let display = h.comp.ensure_x11(&h.agent.clone(), Realm::User).unwrap();
        let rx = x11_client(display);
        h.window_where(Duration::from_secs(20), |w| w.x11);
        rx
    });
    let socket = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap().join(h.comp.socket(&h.agent).unwrap());
    let run_dir = socket.with_file_name(format!("{}.d", h.comp.socket(&h.agent).unwrap()));
    let display = h.comp.x11_display(&h.agent, Realm::User).expect("the launch started an X server");
    assert!(socket.exists() && run_dir.join("bus").exists());
    let agent = h.agent.clone();
    h.comp.remove_desktop(&agent);
    assert!(h.comp.take_events().contains(&Event::Closed(id)));
    assert!(!socket.exists(), "the socket is gone");
    assert!(!run_dir.exists(), "the bus and the runtime files are gone");
    let child = &mut h.children[0];
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(child.try_wait().unwrap().is_some(), "weston-terminal exits when its desktop goes away");
    // Dispatching on after the removal handles what the X server sent last.
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        h.comp.dispatch();
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(x_events);
    let x_socket = PathBuf::from(format!("/tmp/.X11-unix/X{display}"));
    let deadline = Instant::now() + Duration::from_secs(10);
    while x_socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!x_socket.exists(), "Xwayland ended with the desktop");
}

#[test]
fn gnome_text_editor_renders_in_software_and_sets_a_cursor_shape() {
    if !installed("gnome-text-editor") {
        eprintln!("skipped: gnome-text-editor is not installed");
        return;
    }
    let mut h = Harness::new("gte");
    // The desktop's own session bus keeps it from reaching a Text Editor
    // that runs on the host desktop.
    h.launch("gnome-text-editor", &["--ignore-session"]);
    let id = h.first_window(Duration::from_secs(30));
    h.comp.configure(id, 700, 460);
    let ok = h.pump(Duration::from_secs(15), |c, _| {
        c.frame(id).is_some_and(|f| logical(f.width, f.height, f.scale) == (700, 460) && not_blank(f.pixels))
    });
    assert!(ok, "gnome-text-editor did not show a 700x460 frame, got {:?}", h.frame_size(id));
    let info = h.comp.window(id).unwrap();
    assert!(info.app_id.contains("TextEditor"), "{info:?}");

    // GTK asks for named cursors through wp_cursor_shape_v1, so the text shows the text cursor.
    h.comp.pointer_motion(id, 350.0, 280.0);
    let mut seen = String::new();
    let text = h.pump(Duration::from_secs(10), |c, _| {
        seen = match c.cursor() {
            Cursor::Named(name) => name.to_string(),
            Cursor::Image(_) => "an image".into(),
            Cursor::Default => "the default".into(),
            Cursor::Hidden => "hidden".into(),
        };
        seen == "text"
    });
    assert!(text, "the cursor over the text is {seen}, not the text shape");

    // Unchanged, so it closes without asking to save.
    h.comp.close(id);
    assert!(h.pump(Duration::from_secs(10), |_, events| events.contains(&Event::Closed(id))), "close did not remove the window");
}

#[test]
fn a_fractional_scale_renders_sharp() {
    if !installed("gnome-text-editor") {
        eprintln!("skipped: gnome-text-editor is not installed");
        return;
    }
    let mut h = Harness::new("frac");
    h.comp.set_output(1200, 760, 1.25);
    h.launch("gnome-text-editor", &["--ignore-session"]);
    let id = h.first_window(Duration::from_secs(30));
    h.comp.configure(id, 640, 400);
    let mut seen = None;
    let ok = h.pump(Duration::from_secs(15), |c, _| {
        let f = c.frame(id).unwrap();
        seen = Some((f.width, f.height, f.scale));
        (f.width, f.height) == (800, 500) && (f.scale - 1.25).abs() < 0.01 && not_blank(f.pixels)
    });
    assert!(ok, "at 1.25 a 640x400 window should render 800x500 pixels at density 1.25, got {seen:?}");
}

#[test]
fn a_single_instance_app_opens_inside_the_desktop() {
    if !installed("gnome-text-editor") || !installed("dbus-daemon") {
        eprintln!("skipped: gnome-text-editor or dbus-daemon is not installed");
        return;
    }
    let mut h = Harness::new("single");
    h.launch("gnome-text-editor", &["--ignore-session"]);
    let first = h.window_where(Duration::from_secs(30), |w| w.app_id.contains("TextEditor"));
    assert!(h.comp.bus_address(&h.agent, Realm::User).is_some(), "the desktop has its own session bus");
    // The second launch finds the first instance on the desktop's bus and
    // hands it the new window, then exits.
    let second = h.launch("gnome-text-editor", &["--new-window", "--ignore-session"]);
    let window = h.window_where(Duration::from_secs(30), |w| w.app_id.contains("TextEditor") && w.id != first);
    let child = &mut h.children[second];
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let status = child.try_wait().unwrap();
    assert!(status.is_some_and(|s| s.success()), "the second launch handed over and exited, got {status:?}");
    assert!(h.comp.window(first).is_some() && h.comp.window(window).is_some(), "both windows are on the test desktop");
    if std::env::var_os("MEERORA_TEST_DEBUG").is_some() {
        eprintln!("windows {:?} second exit {status:?}", h.comp.windows());
    }
}

/// What the X11 test client saw.
#[derive(Debug, PartialEq)]
enum XSaw {
    Configured(u16, u16),
    Key(u8),
    Button(u8),
    /// A button went down on the menu window.
    MenuButton(u8),
    Primary(String),
    Closed,
}

/// A tiny X11 client on display `:n` in its own thread. Its window is titled
/// "meerora x11 test" and it reports what happens to it. A middle click reads
/// the primary selection, the P key takes it with the text "from x11", and
/// the M key opens or closes a black override-redirect menu at 200, 120
/// with a size of 60 by 40.
fn x11_client(display: u32) -> Receiver<XSaw> {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::Event as XEvent;
    use x11rb::protocol::xproto::{
        AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, PropMode, Rectangle,
        SelectionNotifyEvent, WindowClass,
    };
    use x11rb::wrapper::ConnectionExt as _;

    let (tx, rx) = channel();
    std::thread::spawn(move || -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (conn, screen) = x11rb::connect(Some(&format!(":{display}")))?;
        let screen = conn.setup().roots[screen].clone();
        let atom = |name: &str| -> Result<u32, Box<dyn std::error::Error + Send + Sync>> {
            Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
        };
        let (protocols, delete, utf8, net_name, targets, primary, property) = (
            atom("WM_PROTOCOLS")?,
            atom("WM_DELETE_WINDOW")?,
            atom("UTF8_STRING")?,
            atom("_NET_WM_NAME")?,
            atom("TARGETS")?,
            atom("PRIMARY")?,
            atom("MEERORA_PASTE")?,
        );
        let win = conn.generate_id()?;
        let mask = EventMask::EXPOSURE | EventMask::STRUCTURE_NOTIFY | EventMask::KEY_PRESS | EventMask::BUTTON_PRESS;
        let aux = CreateWindowAux::new().background_pixel(screen.white_pixel).event_mask(mask);
        conn.create_window(x11rb::COPY_DEPTH_FROM_PARENT, win, screen.root, 0, 0, 300, 200, 0, WindowClass::INPUT_OUTPUT, 0, &aux)?;
        conn.change_property8(PropMode::REPLACE, win, AtomEnum::WM_NAME, AtomEnum::STRING, b"meerora x11 test")?;
        conn.change_property8(PropMode::REPLACE, win, net_name, utf8, b"meerora x11 test")?;
        conn.change_property8(PropMode::REPLACE, win, AtomEnum::WM_CLASS, AtomEnum::STRING, b"x11test\0X11Test\0")?;
        conn.change_property32(PropMode::REPLACE, win, protocols, AtomEnum::ATOM, &[delete])?;
        // A hand cursor from the cursor font, which Xwayland hands over as an image.
        let font = conn.generate_id()?;
        conn.open_font(font, b"cursor")?;
        let cursor = conn.generate_id()?;
        conn.create_glyph_cursor(cursor, font, font, 60, 61, 0, 0, 0, 0xffff, 0xffff, 0xffff)?;
        conn.change_window_attributes(win, &ChangeWindowAttributesAux::new().cursor(cursor))?;
        let gc = conn.generate_id()?;
        conn.create_gc(gc, win, &CreateGCAux::new().foreground(screen.black_pixel))?;
        conn.map_window(win)?;
        conn.flush()?;
        let menu = conn.generate_id()?;
        let aux = CreateWindowAux::new().background_pixel(screen.black_pixel).override_redirect(1).event_mask(EventMask::BUTTON_PRESS);
        conn.create_window(x11rb::COPY_DEPTH_FROM_PARENT, menu, screen.root, 200, 120, 60, 40, 0, WindowClass::INPUT_OUTPUT, 0, &aux)?;
        conn.change_property32(PropMode::REPLACE, menu, AtomEnum::WM_TRANSIENT_FOR, AtomEnum::WINDOW, &[win])?;
        let (mut owned, mut menu_open) = (false, false);
        loop {
            let event = conn.wait_for_event()?;
            if std::env::var_os("MEERORA_X11_DEBUG").is_some() {
                eprintln!(
                    "x11 client: {event:?} {:?}",
                    if let XEvent::ConfigureNotify(e) = &event { Some((e.event, e.window, e.width, e.height, win)) } else { None }
                );
            }
            match event {
                XEvent::Expose(_) => {
                    conn.poly_fill_rectangle(win, gc, &[Rectangle { x: 10, y: 10, width: 60, height: 40 }])?;
                    conn.flush()?;
                }
                XEvent::ConfigureNotify(e) if e.window == win => {
                    let _ = tx.send(XSaw::Configured(e.width, e.height));
                }
                XEvent::KeyPress(e) => {
                    // A small square, so the damage of the next frame is small.
                    conn.poly_fill_rectangle(win, gc, &[Rectangle { x: 150, y: 100, width: 16, height: 16 }])?;
                    conn.flush()?;
                    let _ = tx.send(XSaw::Key(e.detail));
                    if e.detail == (KEY_P + 8) as u8 {
                        conn.set_selection_owner(win, primary, e.time)?;
                        conn.flush()?;
                        owned = true;
                    }
                    if e.detail == (KEY_M + 8) as u8 {
                        if menu_open {
                            conn.unmap_window(menu)?;
                        } else {
                            conn.map_window(menu)?;
                        }
                        conn.flush()?;
                        menu_open = !menu_open;
                    }
                }
                XEvent::ButtonPress(e) if e.event == menu => {
                    let _ = tx.send(XSaw::MenuButton(e.detail));
                }
                XEvent::ButtonPress(e) => {
                    let _ = tx.send(XSaw::Button(e.detail));
                    if e.detail == 2 {
                        conn.convert_selection(win, primary, utf8, property, e.time)?;
                        conn.flush()?;
                    }
                }
                XEvent::SelectionNotify(e) if e.property == property => {
                    let reply = conn.get_property(true, win, property, AtomEnum::ANY, 0, 1 << 20)?.reply()?;
                    let _ = tx.send(XSaw::Primary(String::from_utf8_lossy(&reply.value).into_owned()));
                }
                XEvent::SelectionRequest(r) if owned && r.selection == primary => {
                    let mut reply = SelectionNotifyEvent {
                        response_type: x11rb::protocol::xproto::SELECTION_NOTIFY_EVENT,
                        sequence: 0,
                        time: r.time,
                        requestor: r.requestor,
                        selection: r.selection,
                        target: r.target,
                        property: r.property,
                    };
                    if r.target == targets {
                        conn.change_property32(PropMode::REPLACE, r.requestor, r.property, AtomEnum::ATOM, &[targets, utf8])?;
                    } else if r.target == utf8 || r.target == u32::from(AtomEnum::STRING) {
                        conn.change_property8(PropMode::REPLACE, r.requestor, r.property, r.target, b"from x11")?;
                    } else {
                        reply.property = x11rb::NONE;
                    }
                    conn.send_event(false, r.requestor, EventMask::NO_EVENT, reply)?;
                    conn.flush()?;
                }
                XEvent::ClientMessage(m) if m.type_ == protocols && m.data.as_data32()[0] == delete => {
                    conn.destroy_window(win)?;
                    conn.flush()?;
                    let _ = tx.send(XSaw::Closed);
                    return Ok(());
                }
                _ => {}
            }
        }
    });
    rx
}

/// Waits for what the X11 client reports while the compositor runs.
fn expect_x(h: &mut Harness, rx: &Receiver<XSaw>, timeout: Duration, wanted: impl Fn(&XSaw) -> bool) -> Vec<XSaw> {
    let mut seen = Vec::new();
    let ok = h.pump(timeout, |_, _| {
        while let Ok(saw) = rx.try_recv() {
            seen.push(saw);
        }
        seen.iter().any(&wanted)
    });
    assert!(ok, "the X11 client did not report the expected event, it saw {seen:?}");
    seen
}

#[test]
fn an_x11_window_appears_resizes_takes_input_and_closes() {
    if !installed("Xwayland") {
        eprintln!("skipped: Xwayland is not installed");
        return;
    }
    let mut h = Harness::new("x11");
    let display = h.comp.ensure_x11(&h.agent.clone(), Realm::User).expect("an X display");
    let rx = x11_client(display);
    let id = h.window_where(Duration::from_secs(20), |w| w.title == "meerora x11 test");
    let info = h.comp.window(id).unwrap();
    assert!(info.x11 && !info.sandboxed && info.app_id == "X11Test", "{info:?}");

    h.comp.configure(id, 420, 260);
    expect_x(&mut h, &rx, Duration::from_secs(10), |s| *s == XSaw::Configured(420, 260));
    let ok = h.pump(Duration::from_secs(10), |c, _| c.frame(id).is_some_and(|f| (f.width, f.height) == (420, 260) && not_blank(f.pixels)));
    assert!(ok, "the X11 window did not show at 420x260, got {:?}", h.frame_size(id));

    // The window manager gives the X input focus over the X connection, and
    // the keys come over Wayland, so the focus needs a moment to settle.
    h.comp.focus(Some(id));
    h.settle(Duration::from_millis(300));
    let before = h.comp.frame(id).unwrap().serial;
    h.tap(id, KEY_A);
    expect_x(&mut h, &rx, Duration::from_secs(10), |s| *s == XSaw::Key((KEY_A + 8) as u8));

    // With damage tracking only the square that the key drew changed.
    let mut damage = None;
    h.pump(Duration::from_secs(5), |c, _| {
        let f = c.frame(id).unwrap();
        damage = (f.serial > before).then(|| (f.damage_since(before), f.width * f.height));
        damage.as_ref().is_some_and(|(d, _)| d.as_ref().is_some_and(|d| !d.is_empty()))
    });
    let (rects, area) = damage.expect("the square shows");
    let rects = rects.expect("the damage since the last image is known");
    let changed: u32 = rects.iter().map(|r| r[2] * r[3]).sum();
    assert!(changed > 0 && changed < area / 10, "a 16x16 square changed {changed} of {area} pixels: {rects:?}");
    let f = h.comp.frame(id).unwrap();
    let at = (105 * f.width as usize + 155) * 4;
    assert_eq!(&f.pixels[at..at + 3], &[0, 0, 0], "the square is in the image");
    h.click(id, BTN_LEFT, 100.0, 80.0);
    expect_x(&mut h, &rx, Duration::from_secs(10), |s| *s == XSaw::Button(1));

    // An override-redirect menu shows in the window's tile and takes clicks.
    let black_at = |c: &mut Compositor, x: usize, y: usize| {
        c.frame(id).is_some_and(|f| {
            let at = (y * f.width as usize + x) * 4;
            f.pixels.get(at..at + 3) == Some(&[0, 0, 0][..])
        })
    };
    h.tap(id, KEY_M);
    assert!(h.pump(Duration::from_secs(10), |c, _| black_at(c, 230, 140)), "the menu does not show in the tile");
    h.click(id, BTN_LEFT, 230.0, 140.0);
    expect_x(&mut h, &rx, Duration::from_secs(10), |s| *s == XSaw::MenuButton(1));
    h.tap(id, KEY_M);
    assert!(h.pump(Duration::from_secs(10), |c, _| !black_at(c, 230, 140)), "the menu does not go");

    // Its cursor arrives as an image through Xwayland.
    h.comp.pointer_motion(id, 120.0, 90.0);
    let image = h.pump(Duration::from_secs(5), |c, _| matches!(c.cursor(), Cursor::Image(i) if i.width > 0));
    assert!(image, "no cursor image from the X11 window");

    h.comp.close(id);
    expect_x(&mut h, &rx, Duration::from_secs(10), |s| *s == XSaw::Closed);
    assert!(h.pump(Duration::from_secs(10), |c, _| c.window(id).is_none()), "the window did not go");
}

#[test]
fn a_gtk_app_can_be_forced_onto_x11() {
    if !installed("gnome-calculator") || !installed("Xwayland") {
        eprintln!("skipped: gnome-calculator or Xwayland is not installed");
        return;
    }
    let mut h = Harness::new("gtkx11");
    h.launch_with("gnome-calculator", &[], &LaunchOptions { x11: X11::Force, ..LaunchOptions::default() });
    let id = h.window_where(Duration::from_secs(30), |w| w.x11);
    h.comp.configure(id, 400, 560);
    let ok = h.pump(Duration::from_secs(15), |c, _| c.frame(id).is_some_and(|f| (f.width, f.height) == (400, 560) && not_blank(f.pixels)));
    assert!(ok, "the X11 calculator did not show at 400x560, got {:?}", h.frame_size(id));
    let info = h.comp.window(id).unwrap();
    assert!(info.title.contains("Calculator") || info.app_id.to_lowercase().contains("calculator"), "{info:?}");
    h.comp.close(id);
    assert!(h.pump(Duration::from_secs(10), |_, events| events.contains(&Event::Closed(id))), "close did not remove the window");
}

#[test]
fn the_primary_selection_crosses_between_wayland_and_x11_apps() {
    if !installed("gnome-text-editor") || !installed("Xwayland") {
        eprintln!("skipped: gnome-text-editor or Xwayland is not installed");
        return;
    }
    let mut h = Harness::new("primary");
    h.launch("gnome-text-editor", &["--ignore-session"]);
    let gte = h.window_where(Duration::from_secs(30), |w| w.app_id.contains("TextEditor"));
    h.comp.configure(gte, 700, 460);
    h.pump(Duration::from_secs(15), |c, _| c.frame(gte).is_some_and(|f| logical(f.width, f.height, f.scale) == (700, 460)));

    // Selecting text in a Wayland app makes it the primary selection.
    h.click(gte, BTN_LEFT, 350.0, 250.0);
    h.settle(Duration::from_millis(500));
    h.type_text(gte, "hello primary");
    h.chord(gte, &[KEY_LEFTCTRL, KEY_A]);
    let ok = h.pump(Duration::from_secs(10), |c, _| c.primary_selection() == Some("hello primary"));
    assert!(ok, "the selection did not become the primary selection, it is {:?}", h.comp.primary_selection());

    // A middle click into an X11 app pastes it there.
    let display = h.comp.ensure_x11(&h.agent.clone(), Realm::User).expect("an X display");
    let rx = x11_client(display);
    let x = h.window_where(Duration::from_secs(20), |w| w.title == "meerora x11 test");
    h.comp.configure(x, 300, 200);
    h.settle(Duration::from_millis(300));
    h.click(x, BTN_MIDDLE, 50.0, 50.0);
    expect_x(&mut h, &rx, Duration::from_secs(10), |s| *s == XSaw::Primary("hello primary".into()));

    // The X11 app takes the primary selection, and a middle click into the
    // Wayland app pastes its text there.
    h.tap(x, KEY_P);
    let ok = h.pump(Duration::from_secs(10), |c, _| c.primary_selection() == Some("from x11"));
    assert!(ok, "the X11 selection did not arrive, the primary selection is {:?}", h.comp.primary_selection());
    h.click(gte, BTN_MIDDLE, 600.0, 400.0);
    h.settle(Duration::from_millis(500));
    h.chord(gte, &[KEY_LEFTCTRL, KEY_A]);
    h.chord(gte, &[KEY_LEFTCTRL, KEY_C]);
    let mut copied = Vec::new();
    h.pump(Duration::from_secs(10), |_, events| {
        copied = events.iter().filter_map(|e| if let Event::Selection(t) = e { Some(t.clone()) } else { None }).collect();
        copied.iter().any(|t| t.contains("from x11"))
    });
    assert!(copied.iter().any(|t| t.contains("from x11")), "the middle click did not paste into Text Editor, it holds {copied:?}");
}

#[test]
fn a_sandboxed_app_sees_its_agent_directory_and_not_the_home() {
    if !installed("bwrap") || !installed("weston-terminal") {
        eprintln!("skipped: bwrap or weston-terminal is not installed");
        return;
    }
    let mut h = Harness::new("sandbox");
    // A home of its own with a planted secret, and the agent directory inside it as in a real data directory.
    let home = h.dir.join("home");
    let agent_dir = home.join(".local/share/meerora/agents/sbx");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    std::fs::write(home.join("secret.txt"), "s3cret").unwrap();
    std::fs::write(home.join(".ssh/id_ed25519"), "private key").unwrap();
    std::fs::write(agent_dir.join("notes.md"), "agent notes").unwrap();
    let user_home = std::env::var("HOME").unwrap_or_default();
    let script = format!(
        "cat notes.md > seen-notes; cat {secret} > seen-secret 2>&1; ls -A {ssh} > seen-ssh 2>&1; ls -A '{user_home}' > seen-user-home 2>&1; \
         echo \"$HOME\" > seen-home; echo \"$WAYLAND_DISPLAY $DISPLAY\" > seen-display; \
         test -S \"$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY\" && echo yes > seen-wayland; test -S \"/tmp/.X11-unix/X${{DISPLAY#:}}\" && echo yes > seen-x; \
         test -S \"${{DBUS_SESSION_BUS_ADDRESS#unix:path=}}\" && echo yes > seen-bus; env > seen-env; \
         xdpyinfo -queryExtensions > seen-xdpyinfo 2>&1; touch done",
        secret = home.join("secret.txt").display(),
        ssh = home.join(".ssh").display(),
    );
    let options = SandboxOptions { env: vec![("HISTFILE".into(), "/dev/null".into())], ..SandboxOptions::default() };
    let agent = h.agent.clone();
    let child = h.comp.launch_sandboxed(&agent, &agent_dir, "sh", &["-c".into(), script], &options).expect("sandboxed launch");
    h.children.push(child);
    assert!(h.wait_for_file(&agent_dir.join("done")), "the sandboxed program did not run");
    let read = |name: &str| std::fs::read_to_string(agent_dir.join(name)).unwrap_or_default();
    assert_eq!(read("seen-notes"), "agent notes", "it reads the agent directory");
    assert!(!read("seen-secret").contains("s3cret") && read("seen-secret").contains("No such file"), "{}", read("seen-secret"));
    assert!(read("seen-ssh").contains("No such file"), "{}", read("seen-ssh"));
    assert!(!read("seen-user-home").contains(".bashrc") && !read("seen-user-home").contains(".ssh"), "the user's home is not visible");
    assert_eq!(read("seen-home").trim(), agent_dir.display().to_string(), "its home is the agent directory");
    assert_eq!(read("seen-wayland").trim(), "yes", "it sees its Wayland socket");
    assert_eq!(read("seen-x").trim(), "yes", "it sees its X socket");
    assert_eq!(read("seen-bus").trim(), "yes", "it sees its session bus");
    let env = read("seen-env");
    assert!(!env.contains("SSH_AUTH_SOCK=") && !env.contains("XAUTHORITY="), "only allowed variables enter: {env}");
    let agent_display = h.comp.x11_display(&agent, Realm::Agent).expect("the agent realm has an X server");
    if installed("xdpyinfo") {
        let info = read("seen-xdpyinfo");
        assert!(info.contains("XFIXES") && !info.contains("MIT-SHM"), "the agent realm's X server has no MIT-SHM: {info}");
    }
    assert!(read("seen-display").contains(&format!(":{agent_display}")), "{}", read("seen-display"));

    // A sandboxed Wayland app shows on the desktop, marked as the agent's.
    let options = SandboxOptions { env: vec![("HISTFILE".into(), "/dev/null".into())], ..SandboxOptions::default() };
    let child = h.comp.launch_sandboxed(&agent, &agent_dir, "weston-terminal", &["--shell=/bin/sh".into()], &options).expect("launch");
    h.children.push(child);
    let id = h.window_where(Duration::from_secs(20), |w| !w.x11);
    let info = h.comp.window(id).unwrap();
    assert!(info.sandboxed && info.desktop == agent, "{info:?}");
    h.comp.focus(Some(id));
    h.settle(Duration::from_millis(300));
    h.type_text(id, "touch typed");
    h.tap(id, KEY_ENTER);
    assert!(h.wait_for_file(&agent_dir.join("typed")), "keys reach the sandboxed terminal, which works in the agent directory");

    // So does a sandboxed X11 app, on the agent realm's own X server.
    if installed("xmessage") {
        let child = h.comp.launch_sandboxed(&agent, &agent_dir, "xmessage", &["sandboxed".into()], &options).expect("launch");
        h.children.push(child);
        let id = h.window_where(Duration::from_secs(20), |w| w.x11);
        assert!(h.comp.window(id).unwrap().sandboxed);
        assert_ne!(h.comp.ensure_x11(&agent, Realm::User), Some(agent_display), "the user's apps have another X server");
    }
}
