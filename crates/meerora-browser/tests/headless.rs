//! Starts real headless Chrome, loads a `data:` URL and waits for a
//! screencast frame. Also runs several pages in one Chrome. Skipped with a
//! note when Chrome is not installed.

use std::time::{Duration, Instant};

use meerora_browser::{Browser, Frame, Options, Size, Status};

fn temp_profile(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos();
    std::env::temp_dir().join(format!("meerora-browser-{name}-{}-{nanos}", std::process::id()))
}

#[test]
fn streams_a_screencast_frame_of_a_data_url() {
    let Some(chrome) = meerora_browser::find_chrome(None) else {
        eprintln!("skipping: Chrome is not installed (set {} to its path)", meerora_browser::CHROME_ENV);
        return;
    };
    let profile = temp_profile("frame");
    let mut options = Options::new(&profile);
    options.chrome = Some(chrome);
    options.size = Size { width: 480, height: 320 };
    let browser = Browser::start(options);
    let url = "data:text/html,<body style='background:%23c7784a'><h1>meerora</h1></body>";
    browser.navigate(url);

    let deadline = Instant::now() + Duration::from_secs(45);
    let mut frame = None;
    while Instant::now() < deadline {
        if let Status::Failed(e) | Status::Exited(e) = browser.status() {
            panic!("Chrome failed: {e}");
        }
        let page = browser.page();
        if let Some(f) = browser.take_frame()
            && page.url.starts_with("data:text/html")
            && !page.loading
        {
            frame = Some(f);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let frame = frame.expect("a screencast frame of the data: page within 45 s");
    assert_eq!(&frame.jpeg[..2], &[0xFF, 0xD8], "the frame is a JPEG");
    assert!(frame.seq >= 1);
    assert_eq!(frame.css_width.round() as u32, 480);
    assert!(browser.page().frames >= 1);

    browser.resize(Size { width: 300, height: 200 }, 1.0);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut resized = false;
    while Instant::now() < deadline && !resized {
        resized = browser.take_frame().is_some_and(|f| f.css_width.round() as u32 == 300);
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(resized, "a frame at the new size after resize");

    assert!(browser.shutdown(Duration::from_secs(10)), "Chrome closes");
    let _ = std::fs::remove_dir_all(&profile);
}

#[test]
fn clicks_text_and_keys_reach_the_page() {
    let Some(chrome) = meerora_browser::find_chrome(None) else {
        eprintln!("skipping: Chrome is not installed (set {} to its path)", meerora_browser::CHROME_ENV);
        return;
    };
    let profile = temp_profile("input");
    let mut options = Options::new(&profile);
    options.chrome = Some(chrome);
    options.size = Size { width: 400, height: 200 };
    let browser = Browser::start(options);
    // Enter in the field puts its text into the title.
    let page = "data:text/html,<title>start</title><input style='position:absolute;left:0;top:0;width:300px;height:40px' \
                onkeydown=\"if(event.key==='Enter')document.title='got '+this.value\">";
    browser.navigate(page);
    let wait = |what: &str, done: &dyn Fn() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && !done() {
            if let Status::Failed(e) | Status::Exited(e) = browser.status() {
                panic!("Chrome failed: {e}");
            }
            // Like a tile on screen, which takes every frame.
            let _ = browser.take_frame();
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(done(), "timed out waiting for {what}: {:?}", browser.page());
    };
    wait("the page", &|| browser.page().loads >= 1 && browser.page().url.starts_with("data:"));

    for kind in [meerora_browser::MouseKind::Down, meerora_browser::MouseKind::Up] {
        let mut click = meerora_browser::MouseEvent::new(kind, 20.0, 20.0);
        click.button = meerora_browser::Button::Left;
        click.clicks = 1;
        browser.mouse(click);
    }
    browser.insert_text("meerora");
    browser.key(meerora_browser::Key::Enter, meerora_browser::Mods::default());
    wait("the title", &|| browser.page().title == "got meerora");

    assert!(browser.shutdown(Duration::from_secs(10)), "Chrome closes");
    let _ = std::fs::remove_dir_all(&profile);
}

#[test]
fn an_alert_does_not_freeze_the_page() {
    let Some(chrome) = meerora_browser::find_chrome(None) else {
        eprintln!("skipping: Chrome is not installed (set {} to its path)", meerora_browser::CHROME_ENV);
        return;
    };
    let profile = temp_profile("alert");
    let mut options = Options::new(&profile);
    options.chrome = Some(chrome);
    options.size = Size { width: 320, height: 200 };
    let browser = Browser::start(options);
    browser.navigate("data:text/html,<title>before</title><script>alert('hi');document.title='after'</script>");
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline && browser.page().title != "after" {
        let _ = browser.take_frame();
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(browser.page().title, "after", "the script ran on after the alert");
    assert!(browser.shutdown(Duration::from_secs(10)), "Chrome closes");
    let _ = std::fs::remove_dir_all(&profile);
}

/// The URLs of the page targets of the Chrome that runs on `profile`, read
/// from its DevTools HTTP endpoint.
fn page_targets(profile: &std::path::Path) -> Vec<String> {
    use std::io::{Read as _, Write as _};
    let Ok(active) = std::fs::read_to_string(profile.join("DevToolsActivePort")) else { return Vec::new() };
    let Some(port) = active.lines().next().and_then(|p| p.trim().parse::<u16>().ok()) else { return Vec::new() };
    let Ok(mut stream) = std::net::TcpStream::connect(("127.0.0.1", port)) else { return Vec::new() };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    // Chrome answers only HTTP/1.1 and keeps the connection open, so read up to the length it announces.
    let _ = stream.write_all(b"GET /json/list HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
    let (mut reply, mut buf) = (Vec::new(), [0u8; 8192]);
    let body = loop {
        match stream.read(&mut buf) {
            Ok(n) if n > 0 => reply.extend_from_slice(&buf[..n]),
            _ => return Vec::new(),
        }
        let Some(end) = reply.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
        let head = String::from_utf8_lossy(&reply[..end]).to_ascii_lowercase();
        let len = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse::<usize>().ok());
        if len.is_some_and(|len| reply.len() >= end + 4 + len) {
            break &reply[end + 4..];
        }
    };
    let list: Vec<serde_json::Value> = serde_json::from_slice(body).unwrap_or_default();
    list.iter().filter(|t| t["type"] == "page").filter_map(|t| t["url"].as_str().map(str::to_string)).collect()
}

/// Waits until the Chrome on `profile` has exactly the page targets `urls`, in any order.
fn wait_for_targets(profile: &std::path::Path, urls: &[&str]) {
    let mut want: Vec<&str> = urls.to_vec();
    want.sort();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut have = page_targets(profile);
    while Instant::now() < deadline {
        have.sort();
        if have == want {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
        have = page_targets(profile);
    }
    panic!("Chrome has the pages {have:?}, expected {want:?}");
}

/// A page with `title` and a spinning square, so it keeps painting and its
/// screencast keeps sending frames.
fn spinning(title: &str, color: &str) -> String {
    format!(
        "data:text/html,<title>{title}</title><style>@keyframes s{{to{{transform:rotate(360deg)}}}}\
         div{{animation:s 1s linear infinite;width:40px;height:40px;background:{color}}}</style><div></div>"
    )
}

/// Takes the frames of several pages in one loop, like tiles on screen.
struct Tiles<'a> {
    pages: Vec<&'a Browser>,
    /// Frames taken per page since the last reset.
    count: Vec<u64>,
    /// The newest frame taken per page.
    last: Vec<Option<Frame>>,
}

impl<'a> Tiles<'a> {
    fn new(pages: &[&'a Browser]) -> Self {
        Self { pages: pages.to_vec(), count: vec![0; pages.len()], last: vec![None; pages.len()] }
    }

    fn tick(&mut self) {
        for (i, b) in self.pages.iter().enumerate() {
            if let Status::Failed(e) | Status::Exited(e) = b.status() {
                panic!("page {i} failed: {e}");
            }
            if let Some(f) = b.take_frame() {
                self.count[i] += 1;
                self.last[i] = Some(f);
            }
        }
    }

    /// The width in CSS pixels of the newest frame of page `i`.
    fn width(&self, i: usize) -> Option<u32> {
        self.last[i].as_ref().map(|f| f.css_width.round() as u32)
    }

    fn reset(&mut self) {
        self.count.iter_mut().for_each(|c| *c = 0);
        self.last.iter_mut().for_each(|f| *f = None);
    }

    fn wait(&mut self, what: &str, secs: u64, done: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            self.tick();
            if done(self) {
                return;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        let pages: Vec<_> = self.pages.iter().map(|b| (b.status(), b.page())).collect();
        panic!("timed out waiting for {what}: frames {:?}, pages {pages:?}", self.count);
    }

    /// Takes frames for `secs` seconds.
    fn run(&mut self, secs: u64) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            self.tick();
            std::thread::sleep(Duration::from_millis(30));
        }
    }
}

#[test]
fn two_pages_share_one_chrome() {
    let Some(chrome) = meerora_browser::find_chrome(None) else {
        eprintln!("skipping: Chrome is not installed (set {} to its path)", meerora_browser::CHROME_ENV);
        return;
    };
    let profile = temp_profile("pages");
    let mut options = Options::new(&profile);
    options.chrome = Some(chrome);
    options.size = Size { width: 480, height: 320 };
    let a = Browser::start(options);
    // Typing into the field puts its text into the title.
    let url_a = spinning("one", "red")
        + "<input style='position:absolute;left:0;top:100px;width:300px;height:40px' oninput=\"document.title='one '+this.value\">";
    a.navigate(&url_a);
    let url_b = spinning("two", "blue");
    let b = a.open_page(Size { width: 360, height: 240 }, 1.0, Some(&url_b));

    let mut tiles = Tiles::new(&[&a, &b]);
    tiles.wait("both pages", 45, |t| {
        let (pa, pb) = (a.page(), b.page());
        a.status() == Status::Running
            && b.status() == Status::Running
            && (pa.url.as_str(), pa.title.as_str(), pa.loading) == (url_a.as_str(), "one", false)
            && (pb.url.as_str(), pb.title.as_str(), pb.loading) == (url_b.as_str(), "two", false)
            && t.width(0) == Some(480)
            && t.width(1) == Some(360)
    });
    for f in tiles.last.iter().flatten() {
        assert_eq!(&f.jpeg[..2], &[0xFF, 0xD8], "the frame is a JPEG");
    }
    wait_for_targets(&profile, &[&url_a, &url_b]);

    // Both pages stream at the same time.
    tiles.reset();
    tiles.run(2);
    assert!(tiles.count[0] >= 3 && tiles.count[1] >= 3, "both pages stream: {:?}", tiles.count);
    assert_eq!((tiles.width(0), tiles.width(1)), (Some(480), Some(360)));

    // Input on the first page does not reach the second.
    for kind in [meerora_browser::MouseKind::Down, meerora_browser::MouseKind::Up] {
        let mut click = meerora_browser::MouseEvent::new(kind, 20.0, 120.0);
        click.button = meerora_browser::Button::Left;
        click.clicks = 1;
        a.mouse(click);
    }
    a.insert_text("typed");
    tiles.wait("the typed title", 30, |_| a.page().title == "one typed");
    assert_eq!((b.page().url, b.page().title), (url_b.clone(), "two".to_string()), "the second page is unchanged");

    // Navigation and history on the second page do not touch the first.
    let url_c = spinning("three", "green");
    b.navigate(&url_c);
    tiles.wait("the second page navigated", 30, |_| b.page().title == "three" && b.page().url == url_c && b.page().can_go_back);
    b.back();
    tiles.wait("the second page went back", 30, |_| b.page().title == "two" && b.page().url == url_b);
    assert_eq!((a.page().url, a.page().title), (url_a.clone(), "one typed".to_string()), "the first page is unchanged");

    // A resize of the second page leaves the first alone.
    b.resize(Size { width: 300, height: 200 }, 1.0);
    tiles.reset();
    tiles.wait("the second page resized", 20, |t| t.width(1) == Some(300) && t.width(0) == Some(480));

    // Dropping the second page closes only that page.
    drop(tiles);
    drop(b);
    wait_for_targets(&profile, &[&url_a]);
    let mut tiles = Tiles::new(&[&a]);
    tiles.run(1);
    tiles.reset();
    tiles.run(2);
    assert_eq!(a.status(), Status::Running, "the first page runs on");
    assert!(tiles.count[0] >= 3, "the first page still streams: {:?}", tiles.count);
    assert!(!a.is_finished());

    assert!(a.shutdown(Duration::from_secs(10)), "Chrome closes");
    let _ = std::fs::remove_dir_all(&profile);
}

#[test]
fn a_page_opened_right_after_start_opens() {
    let Some(chrome) = meerora_browser::find_chrome(None) else {
        eprintln!("skipping: Chrome is not installed (set {} to its path)", meerora_browser::CHROME_ENV);
        return;
    };
    let profile = temp_profile("early-page");
    let mut options = Options::new(&profile);
    options.chrome = Some(chrome);
    options.size = Size { width: 400, height: 300 };
    let a = Browser::start(options);
    let url_b = spinning("early", "orange");
    let b = a.open_page(Size { width: 320, height: 200 }, 1.0, Some(&url_b));
    assert_eq!(b.status(), Status::Starting);

    let mut tiles = Tiles::new(&[&a, &b]);
    tiles.wait("both pages", 45, |t| {
        a.status() == Status::Running && b.status() == Status::Running && b.page().title == "early" && t.width(1) == Some(320)
    });
    assert_eq!(b.page().url, url_b);
    wait_for_targets(&profile, &["about:blank", &url_b]);

    // The first page can go first. Chrome runs on for the second.
    drop(tiles);
    drop(a);
    wait_for_targets(&profile, &[&url_b]);
    let mut tiles = Tiles::new(&[&b]);
    tiles.run(1);
    tiles.reset();
    tiles.run(2);
    assert_eq!(b.status(), Status::Running, "the second page runs on");
    assert!(tiles.count[0] >= 3, "the second page still streams: {:?}", tiles.count);
    drop(tiles);

    // Closing on any page closes Chrome, and later pages report that.
    b.close();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !b.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(b.is_finished(), "Chrome closes");
    assert!(matches!(b.status(), Status::Exited(_)), "{:?}", b.status());
    let c = b.open_page(Size { width: 100, height: 100 }, 1.0, None);
    assert!(c.is_finished());
    assert_eq!(c.status(), b.status());
    let _ = std::fs::remove_dir_all(&profile);
}

#[test]
fn a_page_outlives_an_opener_dropped_at_once() {
    let Some(chrome) = meerora_browser::find_chrome(None) else {
        eprintln!("skipping: Chrome is not installed (set {} to its path)", meerora_browser::CHROME_ENV);
        return;
    };
    let profile = temp_profile("orphan-page");
    let mut options = Options::new(&profile);
    options.chrome = Some(chrome);
    let a = Browser::start(options);
    let url_b = spinning("orphan", "purple");
    let b = a.open_page(Size { width: 320, height: 200 }, 1.0, Some(&url_b));
    // Gone before Chrome even runs. Its page closes and Chrome runs on for the second page.
    drop(a);

    let mut tiles = Tiles::new(&[&b]);
    tiles.wait("the second page", 45, |t| b.status() == Status::Running && b.page().title == "orphan" && t.width(0) == Some(320));
    wait_for_targets(&profile, &[&url_b]);
    drop(tiles);
    assert!(b.shutdown(Duration::from_secs(10)), "Chrome closes");
    let _ = std::fs::remove_dir_all(&profile);
}
