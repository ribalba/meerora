//! Drives real headless Chrome the way an agent does (D30). It reads pages
//! as text with refs, clicks, types, scrolls and takes screenshots. Skipped
//! with a note when Chrome is not installed.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use meerora_browser::agent::{self, Element, PageText};
use meerora_browser::{Browser, Options, Remote, Size, Status};
use serde_json::{Value, json};

/// Time for one agent call.
const T: Duration = Duration::from_secs(20);
/// Time for Chrome to start and load the first page.
const START: Duration = Duration::from_secs(45);

fn temp_profile(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos();
    std::env::temp_dir().join(format!("meerora-browser-agent-{name}-{}-{nanos}", std::process::id()))
}

/// One Chrome for one test. Dropping it ends Chrome and removes its
/// profile, also when the test fails.
struct Chrome {
    browser: Option<Browser>,
    profile: PathBuf,
}

impl Chrome {
    /// None when Chrome is not installed.
    fn start(name: &str) -> Option<Self> {
        let Some(chrome) = meerora_browser::find_chrome(None) else {
            eprintln!("skipping: Chrome is not installed (set {} to its path)", meerora_browser::CHROME_ENV);
            return None;
        };
        let profile = temp_profile(name);
        let mut options = Options::new(&profile);
        options.chrome = Some(chrome);
        options.size = Size { width: 480, height: 320 };
        Some(Self { browser: Some(Browser::start(options)), profile })
    }

    fn browser(&self) -> &Browser {
        self.browser.as_ref().expect("a browser")
    }

    fn remote(&self) -> Remote {
        self.browser().remote()
    }

    /// Ends Chrome and checks that it closed. Drop removes the profile.
    fn finish(mut self) {
        let browser = self.browser.take().expect("a browser");
        assert!(browser.shutdown(Duration::from_secs(10)), "Chrome closes");
    }
}

impl Drop for Chrome {
    fn drop(&mut self) {
        if let Some(browser) = self.browser.take() {
            let _ = browser.shutdown(Duration::from_secs(10));
        }
        let _ = std::fs::remove_dir_all(&self.profile);
    }
}

fn data(html: &str) -> String {
    format!("data:text/html,{html}")
}

/// The value of `expression` in the page.
fn eval(remote: &Remote, expression: &str) -> Value {
    let r = remote.call("Runtime.evaluate", json!({ "expression": expression, "returnByValue": true }), T).expect("evaluate");
    r["result"]["value"].clone()
}

fn named<'a>(page: &'a PageText, name: &str) -> &'a Element {
    page.elements.iter().find(|e| e.name == name).unwrap_or_else(|| panic!("no element {name:?} in {:?}", page.elements))
}

/// Reads the page until `done` holds, at most 10 s. A read during a
/// navigation may fail, so failures only count at the end.
fn read_until(remote: &Remote, what: &str, done: impl Fn(&PageText) -> bool) -> PageText {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let read = agent::read(remote, T);
        if let Ok(page) = &read
            && done(page)
        {
            return page.clone();
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}: {read:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Polls `expression` until `done` holds for its value, at most 10 s.
fn eval_until(remote: &Remote, expression: &str, done: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let v = eval(remote, expression);
        if done(&v) {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {expression}: {v}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn reads_text_title_and_elements_with_refs() {
    let Some(chrome) = Chrome::start("read") else { return };
    let remote = chrome.remote();
    // Before the page runs, a call answers at once.
    let early = remote.call("Runtime.evaluate", json!({ "expression": "1" }), T);
    assert!(early.as_ref().is_err_and(|e| e.contains("still loading")), "{early:?}");

    let page = data(
        "<title>Agent page</title><h1>Hello agents</h1><p>Plain text here.</p>\
         <a href='https://example.com/x'>Example link</a> <button>Press me</button>\
         <label for=n>Your name</label><input id=n value=Ada>\
         <input type=password aria-label=Secret value=hunter2>\
         <input type=hidden value=hidden-value>\
         <input type=checkbox id=c checked><label for=c>Agree</label>\
         <textarea placeholder=Notes>line</textarea>\
         <select aria-label=Color><option>Red</option><option selected>Blue</option></select>\
         <div role=button tabindex=0>Div button</div>\
         <input type=submit value=Send>\
         <button style='display:none'>Invisible</button>\
         <button style='visibility:hidden'>Hidden</button>\
         <button style='width:0;height:0;padding:0;border:0;overflow:hidden'>Zero</button>\
         <div style='display:none'><a href='/x'>Gone link</a></div>",
    );
    agent::open(&remote, &page, START).expect("the page opens");
    let read = agent::read(&remote, T).expect("read");
    assert!(read.url.starts_with("data:text/html"), "{}", read.url);
    assert_eq!(read.title, "Agent page");
    assert!(read.text.contains("Hello agents") && read.text.contains("Plain text here."), "{}", read.text);

    let got: Vec<(&str, &str, Option<&str>)> =
        read.elements.iter().map(|e| (e.role.as_str(), e.name.as_str(), e.value.as_deref())).collect();
    let want = vec![
        ("link", "Example link", None),
        ("button", "Press me", None),
        ("textbox", "Your name", Some("Ada")),
        ("textbox", "Secret", Some("••••")),
        ("checkbox", "Agree", Some("checked")),
        ("textbox", "Notes", Some("line")),
        ("select", "Color", Some("Blue")),
        ("button", "Div button", None),
        ("button", "Send", None),
    ];
    assert_eq!(got, want);
    for (i, e) in read.elements.iter().enumerate() {
        assert_eq!(e.r#ref, format!("e{}", i + 1), "refs count up in document order");
    }
    // The data: URL holds the whole page source, so only what the page shows counts.
    let all = format!("{:?} {:?} {:?}", read.title, read.text, read.elements);
    assert!(!all.contains("hunter2"), "the password stays in the page: {all}");
    assert!(!all.contains("hidden-value"), "hidden inputs are left out: {all}");
    for gone in ["Invisible", "Hidden", "Zero", "Gone link"] {
        assert!(read.elements.iter().all(|e| e.name != gone), "{gone} is not visible: {:?}", read.elements);
    }
    assert_eq!(eval(&remote, "document.querySelectorAll('[data-meerora-ref]').length"), json!(9));

    // After a new element at the top the next read numbers anew and leaves no old refs behind.
    eval(&remote, "document.body.prepend(Object.assign(document.createElement('button'), { textContent: 'New first' }))");
    let again = agent::read(&remote, T).expect("read");
    assert_eq!((again.elements[0].r#ref.as_str(), again.elements[0].name.as_str()), ("e1", "New first"));
    assert_eq!(named(&again, "Example link").r#ref, "e2");
    assert_eq!(again.elements.len(), 10);
    assert_eq!(eval(&remote, "document.querySelectorAll('[data-meerora-ref]').length"), json!(10));
    assert_eq!(eval(&remote, "document.querySelector('[data-meerora-ref=e1]').textContent"), json!("New first"));

    // A remote works from another thread, and dropping it leaves the page open.
    let other = remote.clone();
    let from_thread = std::thread::spawn(move || agent::read(&other, T)).join().expect("the thread ends");
    assert_eq!(from_thread.expect("read").elements.len(), 10);
    assert_eq!(remote.status(), Status::Running);
    assert_eq!(agent::read(&remote, T).expect("read").title, "Agent page");

    chrome.finish();
    let after = remote.call("Runtime.evaluate", json!({ "expression": "1" }), T);
    assert!(after.as_ref().is_err_and(|e| e.contains("closed")), "{after:?}");
    assert!(matches!(remote.status(), Status::Exited(_)), "{:?}", remote.status());
    assert!(agent::read(&remote, T).is_err());
}

#[test]
fn a_click_on_a_ref_reaches_the_page() {
    let Some(chrome) = Chrome::start("click") else { return };
    let remote = chrome.remote();
    // The button sits below the fold, so the click has to scroll first.
    let page = data(
        "<title>start</title><div style='height:1200px'>top</div>\
         <button onclick=\"if(event.isTrusted){window.n=(window.n||0)+1;document.title='clicked '+window.n}\">Go</button>",
    );
    remote.navigate(&page);
    let deadline = Instant::now() + START;
    while !(remote.status() == Status::Running && remote.page().loads >= 1 && remote.page().url.starts_with("data:")) {
        assert!(Instant::now() < deadline, "the page did not load: {:?} {:?}", remote.status(), remote.page());
        std::thread::sleep(Duration::from_millis(50));
    }
    agent::wait_ready(&remote, T).expect("the page is ready");

    let read = agent::read(&remote, T).expect("read");
    let go = named(&read, "Go").clone();
    let clicked = agent::click(&remote, &go.r#ref, T).expect("click");
    assert_eq!(clicked, go);
    read_until(&remote, "the click", |p| p.title == "clicked 1");
    assert!(eval(&remote, "scrollY").as_f64().unwrap() > 0.0, "the button was scrolled into view");

    agent::click(&remote, &go.r#ref, T).expect("a second click");
    read_until(&remote, "the second click", |p| p.title == "clicked 2");
    chrome.finish();
}

#[test]
fn typing_fills_fields_selects_options_and_submits() {
    let Some(chrome) = Chrome::start("type") else { return };
    let remote = chrome.remote();
    let page = data(
        "<title>start</title>\
         <form onsubmit=\"document.title='sent '+this.q.value;return false\"><input name=q aria-label=Query value=old></form>\
         <input aria-label=Other>\
         <input type=password aria-label=Pin>\
         <select aria-label=Fruit><option value=a>Apple</option><option value=g>Green grape</option></select>\
         <input type=checkbox aria-label=Box>\
         <div contenteditable=true aria-label=Editor>draft</div>",
    );
    agent::open(&remote, &page, START).expect("the page opens");
    let read = agent::read(&remote, T).expect("read");
    let r = |name: &str| named(&read, name).r#ref.clone();

    // Without submit the value shows in the next read and nothing is sent.
    let other = agent::type_into(&remote, &r("Other"), "plain", false, T).expect("type");
    assert_eq!((other.name.as_str(), other.value.as_deref()), ("Other", Some("plain")));
    let now = agent::read(&remote, T).expect("read");
    assert_eq!(named(&now, "Other").value.as_deref(), Some("plain"));
    assert_eq!(now.title, "start");

    // With submit the old value is replaced and Enter sends the form.
    let query = agent::type_into(&remote, &r("Query"), "meerora", true, T).expect("type and submit");
    assert_eq!(query.value.as_deref(), Some("meerora"));
    read_until(&remote, "the form", |p| p.title == "sent meerora");

    // An empty text clears the field.
    let cleared = agent::type_into(&remote, &r("Other"), "", false, T).expect("clear");
    assert_eq!(cleared.value.as_deref(), Some(""));
    assert_eq!(eval(&remote, "document.querySelector('[aria-label=Other]').value"), json!(""));

    // A password is typed for real but read back masked.
    let pin = agent::type_into(&remote, &r("Pin"), "1234", false, T).expect("type a password");
    assert_eq!(pin.value.as_deref(), Some("••••"));
    assert_eq!(eval(&remote, "document.querySelector('[aria-label=Pin]').value"), json!("1234"));
    assert!(!format!("{:?}", agent::read(&remote, T).expect("read")).contains("1234"));

    // A select takes the option by its text or its value.
    let fruit = agent::type_into(&remote, &r("Fruit"), "green grape", false, T).expect("select");
    assert_eq!(fruit.value.as_deref(), Some("Green grape"));
    let fruit = agent::type_into(&remote, &r("Fruit"), "a", false, T).expect("select by value");
    assert_eq!(fruit.value.as_deref(), Some("Apple"));
    let wrong = agent::type_into(&remote, &r("Fruit"), "banana", false, T);
    assert!(wrong.as_ref().is_err_and(|e| e.contains("no option") && e.contains("Apple, Green grape")), "{wrong:?}");

    // A checkbox takes no text.
    let boxed = agent::type_into(&remote, &r("Box"), "x", false, T);
    assert!(boxed.as_ref().is_err_and(|e| e.contains("checkbox") && e.contains("Click")), "{boxed:?}");

    // An editable element is replaced as a whole.
    let editor = agent::type_into(&remote, &r("Editor"), "new text", false, T).expect("type into the editor");
    assert_eq!(editor.value.as_deref(), Some("new text"));
    chrome.finish();
}

#[test]
fn unknown_refs_and_script_errors_fail_readably() {
    let Some(chrome) = Chrome::start("unknown") else { return };
    let remote = chrome.remote();
    agent::open(&remote, &data("<title>refs</title><button>One</button>"), START).expect("the page opens");
    let read = agent::read(&remote, T).expect("read");
    let one = named(&read, "One").r#ref.clone();

    for r#ref in ["e999", "e1\"]", "'); throw 1; //"] {
        let click = agent::click(&remote, r#ref, T);
        let want = format!("There is no element {} on the page. Read the page again with app_read", r#ref);
        assert!(click.as_ref().is_err_and(|e| e.contains(&want)), "{click:?}");
        let typed = agent::type_into(&remote, r#ref, "x", true, T);
        assert!(typed.as_ref().is_err_and(|e| e.contains(&want)), "{typed:?}");
    }

    // A ref of an element that left the page is gone too.
    eval(&remote, "document.querySelector('button').remove()");
    let stale = agent::click(&remote, &one, T);
    assert!(stale.as_ref().is_err_and(|e| e.contains("Read the page again")), "{stale:?}");

    // An exception in the page becomes readable text.
    eval(&remote, "CSS.escape = () => { throw new Error('boom') }");
    let broken = agent::click(&remote, "e1", T);
    assert_eq!(broken, Err("A script in the page failed (Error: boom).".to_string()));
    chrome.finish();
}

#[test]
fn scrolling_moves_the_page() {
    let Some(chrome) = Chrome::start("scroll") else { return };
    let remote = chrome.remote();
    agent::open(&remote, &data("<title>long</title><div style='height:4000px'>long</div>"), START).expect("the page opens");
    assert_eq!(eval(&remote, "scrollY"), json!(0));

    agent::scroll(&remote, 400.0, T).expect("scroll down");
    let down = eval_until(&remote, "scrollY", |v| v.as_f64().unwrap_or(0.0) > 0.0).as_f64().unwrap();
    agent::scroll(&remote, -200.0, T).expect("scroll up");
    eval_until(&remote, "scrollY", |v| v.as_f64().unwrap_or(f64::MAX) < down);
    agent::scroll(&remote, 0.0, T).expect("no scroll");
    chrome.finish();
}

#[test]
fn a_screenshot_is_a_png() {
    let Some(chrome) = Chrome::start("screenshot") else { return };
    let remote = chrome.remote();
    // The untouched first page is ready too.
    agent::wait_ready(&remote, START).expect("the first page is ready");
    agent::open(&remote, &data("<body style='background:orange'><h1>meerora</h1></body>"), T).expect("the page opens");
    let png = agent::screenshot(&remote, T).expect("screenshot");
    assert!(png.len() > 100, "{} bytes", png.len());
    assert_eq!(&png[..4], b"\x89PNG", "the screenshot is a PNG");
    chrome.finish();
}

#[test]
fn a_remote_navigates_through_the_history() {
    let Some(chrome) = Chrome::start("history") else { return };
    let remote = chrome.remote();
    agent::open(&remote, &data("<title>A</title>first"), START).expect("the first page opens");
    agent::open(&remote, &data("<title>B</title>second"), T).expect("the second page opens");
    assert_eq!(agent::read(&remote, T).expect("read").title, "B");

    remote.back();
    read_until(&remote, "back to the first page", |p| p.title == "A");
    remote.forward();
    read_until(&remote, "forward", |p| p.title == "B");
    let loads = remote.page().loads;
    remote.reload();
    let deadline = Instant::now() + Duration::from_secs(10);
    while remote.page().loads == loads {
        assert!(Instant::now() < deadline, "the page did not reload: {:?}", remote.page());
        std::thread::sleep(Duration::from_millis(50));
    }
    agent::wait_ready(&remote, T).expect("ready after the reload");
    assert_eq!(agent::read(&remote, T).expect("read").title, "B");

    // Nothing listens on port 1.
    let bad = agent::open(&remote, "http://127.0.0.1:1/", T);
    assert!(bad.as_ref().is_err_and(|e| e.contains("could not open http://127.0.0.1:1/")), "{bad:?}");
    chrome.finish();
}

#[test]
fn a_remote_of_a_closed_page_fails_readably() {
    let Some(chrome) = Chrome::start("closed-page") else { return };
    let first = chrome.remote();
    agent::open(&first, &data("<title>first</title>one"), START).expect("the first page opens");
    let second = chrome.browser().open_page(Size { width: 320, height: 200 }, 1.0, None);
    let remote = second.remote();
    agent::open(&remote, &data("<title>second</title>two"), START).expect("the second page opens");
    assert_eq!(agent::read(&remote, T).expect("read").title, "second");

    // Dropping the page closes it. The remote stays and says so.
    drop(second);
    let call = remote.call("Runtime.evaluate", json!({ "expression": "1" }), T);
    assert_eq!(call, Err("The page was closed.".to_string()));
    assert_eq!(remote.status(), Status::Exited("The page was closed.".into()));
    assert!(agent::wait_ready(&remote, T).is_err());
    assert_eq!(agent::read(&first, T).expect("the first page runs on").title, "first");
    chrome.finish();
}

#[test]
fn a_remote_is_send_sync_and_clone() {
    fn check<T: Send + Sync + Clone + 'static>() {}
    check::<Remote>();
}
