//! Turning what the agent logged or what you typed into a URL to load.
//!
//! Pages an agent opens (its logged `browser.open`) load only over `http`,
//! `https`, `data` or as `about:blank`. A `file://` URL or a bare path loads
//! only when it names a regular file inside the agent's own directory once
//! every symbolic link is resolved ([`for_agent`]), so an agent can show its
//! report but never a file of yours. What you type yourself may open any
//! file.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Search page used for `browser.search` and for words typed into the URL
/// field. The HTML version of DuckDuckGo needs no JavaScript.
pub const SEARCH_URL: &str = "https://html.duckduckgo.com/html/?q=";

/// Schemes that never get a `//`.
const OPAQUE: [&str; 5] = ["about", "data", "blob", "mailto", "view-source"];

/// Schemes an agent's `browser.open` may load. Local schemes such as
/// `file:`, `view-source:`, `filesystem:` or `chrome:` are not among them, so
/// an agent cannot open your files or the browser's own pages in a page it
/// may read back. Your own typing in the URL field may open them.
const AGENT_SCHEMES: [&str; 3] = ["http", "https", "data"];

/// The search page for `query`.
pub fn search_url(query: &str) -> String {
    format!("{SEARCH_URL}{}", encode_query(query.trim()))
}

/// The URL for an agent's `browser.open` summary. The action log cuts long
/// summaries with an ellipsis, which is dropped here. `None` when nothing
/// loadable is left, for `javascript:` URLs, which must not run code the
/// user never saw, and for local files and schemes (see [`agent_may_open`]).
pub fn for_open(summary: &str) -> Option<String> {
    resolve(summary).filter(|url| agent_may_open(url))
}

/// [`for_open`] for an agent whose directory is `dir`. A `file://` URL or an
/// absolute path also opens when it names a regular file inside `dir` after
/// every symbolic link is resolved, and the browser gets the resolved path.
pub fn for_agent(summary: &str, dir: Option<&Path>) -> Option<String> {
    let url = resolve(summary)?;
    if agent_may_open(&url) {
        return Some(url);
    }
    let dir = dir?.canonicalize().ok()?;
    let scheme = url.get(..7).filter(|s| s.eq_ignore_ascii_case("file://"))?;
    let rest = &url[scheme.len()..];
    let path = rest.strip_prefix("localhost").unwrap_or(rest);
    if !path.starts_with('/') {
        return None;
    }
    let decoded = percent_decode(path.split(['?', '#']).next().unwrap_or(path));
    let real = Path::new(OsStr::from_bytes(&decoded)).canonicalize().ok()?;
    (real.starts_with(&dir) && real.is_file()).then(|| file_url(&real))
}

/// A `file://` URL for an absolute path, with unsafe bytes percent-encoded.
fn file_url(path: &Path) -> String {
    let mut url = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => url.push(b as char),
            _ => url.push_str(&format!("%{b:02X}")),
        }
    }
    url
}

/// Decodes `%XX` escapes of a URL path.
fn percent_decode(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(h), Some(l)) = (bytes.get(i + 1).and_then(|b| hex(*b)), bytes.get(i + 2).and_then(|b| hex(*b)))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

/// Whether a page an agent asked for may load: `http`, `https`, `data` and
/// `about:blank`, never `file:` URLs or other local schemes.
pub fn agent_may_open(url: &str) -> bool {
    url.eq_ignore_ascii_case("about:blank") || scheme(url).is_some_and(|sc| AGENT_SCHEMES.iter().any(|a| a.eq_ignore_ascii_case(sc)))
}

/// What a summary or typed text means as a URL. Bare absolute paths become
/// `file://` URLs, `javascript:` gives None.
fn resolve(summary: &str) -> Option<String> {
    let s = summary.trim().trim_end_matches('…').trim();
    if s.is_empty() || s.contains(char::is_whitespace) {
        return None;
    }
    match scheme(s) {
        Some(sc) if sc.eq_ignore_ascii_case("javascript") => None,
        Some(_) => Some(s.to_string()),
        None if s.starts_with(':') => Some(format!("http://localhost{s}")),
        None if s.starts_with('/') => Some(format!("file://{s}")),
        None if is_local_host(s) => Some(format!("http://{s}")),
        None => Some(format!("https://{s}")),
    }
}

/// The URL for text typed into the URL field: a URL as it is, a host with
/// `https://` in front, anything else as a search.
pub fn for_input(text: &str) -> Option<String> {
    let s = text.trim();
    if s.is_empty() {
        return None;
    }
    let looks_like_url = !s.contains(char::is_whitespace)
        && (scheme(s).is_some() || s.starts_with(':') || s.starts_with('/') || is_local_host(s) || host_part(s).contains('.'));
    if looks_like_url { resolve(s) } else { Some(search_url(s)) }
}

/// The scheme of `s`, when `s` starts with one. `localhost:8050` has none.
fn scheme(s: &str) -> Option<&str> {
    let (name, rest) = s.split_once(':')?;
    let valid = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    (valid && (rest.starts_with("//") || OPAQUE.iter().any(|o| o.eq_ignore_ascii_case(name)) || name.eq_ignore_ascii_case("javascript")))
        .then_some(name)
}

fn host_part(s: &str) -> &str {
    s.split(['/', '?', '#']).next().unwrap_or(s)
}

fn is_local_host(s: &str) -> bool {
    let host = host_part(s);
    let name = host.rsplit_once(':').filter(|(_, port)| port.chars().all(|c| c.is_ascii_digit())).map_or(host, |(name, _)| name);
    matches!(name, "localhost" | "[::1]" | "0.0.0.0") || name.starts_with("127.")
}

/// Percent-encoding for a query value, with `+` for spaces.
fn encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_what_agents_log() {
        assert_eq!(for_open("https://example.org/a?b=c").as_deref(), Some("https://example.org/a?b=c"));
        assert_eq!(for_open(":8050/report").as_deref(), Some("http://localhost:8050/report"));
        assert_eq!(for_open("localhost:8050/report").as_deref(), Some("http://localhost:8050/report"));
        assert_eq!(for_open("127.0.0.1:3000").as_deref(), Some("http://127.0.0.1:3000"));
        assert_eq!(for_open("example.org/docs").as_deref(), Some("https://example.org/docs"));
        assert_eq!(for_open("data:text/html,<h1>hi</h1>").as_deref(), Some("data:text/html,<h1>hi</h1>"));
        assert_eq!(for_open("about:blank").as_deref(), Some("about:blank"));
        assert_eq!(for_open("https://example.org/very/long/pa…").as_deref(), Some("https://example.org/very/long/pa"));
        assert_eq!(for_open("javascript:alert(1)"), None);
        assert_eq!(for_open("JavaScript:alert(1)"), None);
        assert_eq!(for_open("  "), None);
        assert_eq!(for_open("two words"), None);
    }

    #[test]
    fn agents_never_open_local_files_or_schemes() {
        for summary in [
            "/home/you/.ssh/id_ed25519",
            "/tmp/report.html",
            "file:///etc/passwd",
            "FILE:///etc/passwd",
            "file://localhost/etc/passwd",
            "view-source:file:///etc/passwd",
            "chrome://settings",
            "chrome-extension://abc/page.html",
            "devtools://devtools/bundled/inspector.html",
            "blob:https://example.org/1",
            "mailto:you@example.org",
            "about:config",
        ] {
            assert_eq!(for_open(summary), None, "{summary}");
        }
        assert!(agent_may_open("about:blank") && agent_may_open("http://localhost:8050") && !agent_may_open("file:///tmp/a"));
    }

    #[test]
    fn an_agent_may_open_files_of_its_own_directory_only() {
        let root = std::env::temp_dir().join(format!("meerora-url-{}", std::process::id()));
        let dir = root.join("agents/ember");
        std::fs::create_dir_all(dir.join("artifacts")).unwrap();
        std::fs::write(dir.join("artifacts/report 1.html"), "<h1>report</h1>").unwrap();
        std::fs::write(root.join("secret.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(root.join("secret.txt"), dir.join("artifacts/leak.html")).unwrap();
        let real = dir.canonicalize().unwrap();
        let report = format!("file://{}/artifacts/report%201.html", real.display());
        assert_eq!(for_agent(&format!("file://{}/artifacts/report%201.html", dir.display()), Some(&dir)), Some(report.clone()));
        assert_eq!(for_agent(&format!("{}/artifacts/report 1.html", dir.display()), Some(&dir)), None, "a space ends the summary");
        for outside in [
            format!("file://{}/artifacts/leak.html", dir.display()),
            format!("file://{}/../../secret.txt", dir.display()),
            format!("{}/secret.txt", root.display()),
            format!("file://{}/artifacts", dir.display()),
            "file:///etc/passwd".to_string(),
        ] {
            assert_eq!(for_agent(&outside, Some(&dir)), None, "{outside}");
        }
        assert_eq!(for_agent(&report, None), None, "without a directory no file opens");
        assert_eq!(for_agent("https://example.org", Some(&dir)).as_deref(), Some("https://example.org"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn searches_for_queries() {
        assert_eq!(search_url("rust egui 0.36"), "https://html.duckduckgo.com/html/?q=rust+egui+0.36");
        assert_eq!(search_url("a&b=c/d ü"), "https://html.duckduckgo.com/html/?q=a%26b%3Dc%2Fd+%C3%BC");
    }

    #[test]
    fn reads_the_url_field() {
        assert_eq!(for_input("example.org").as_deref(), Some("https://example.org"));
        assert_eq!(for_input("http://example.org").as_deref(), Some("http://example.org"));
        assert_eq!(for_input("localhost:8080").as_deref(), Some("http://localhost:8080"));
        // What you type yourself may open your files.
        assert_eq!(for_input("/tmp/report.html").as_deref(), Some("file:///tmp/report.html"));
        assert_eq!(for_input("file:///tmp/report.html").as_deref(), Some("file:///tmp/report.html"));
        assert_eq!(for_input("egui text edit").as_deref(), Some("https://html.duckduckgo.com/html/?q=egui+text+edit"));
        assert_eq!(for_input("meerora").as_deref(), Some("https://html.duckduckgo.com/html/?q=meerora"));
        assert_eq!(for_input("javascript:alert(1)"), None);
        assert_eq!(for_input(""), None);
    }
}
