//! Checks a manifest before the index stores it (D25, section 5 of the brief).
//!
//! Every string has a limit, so one manifest stays small (at most
//! [`MAX_MANIFEST_BYTES`] as posted). Text that people read must not hold
//! control characters or the bidirectional overrides that can make a name
//! look like another one. URLs must use https and name a public domain, never
//! an IP address or an internal name, unless the policy allows
//! `http://127.0.0.1:<port>` for tests and the demo.

use meerora_protocol::{Manifest, ManifestKind};

/// The manifest format this index understands.
pub const FORMAT: &str = "0.1";
/// The only endpoint protocol so far (Q9).
pub const PROTOCOL: &str = "a2a/1.0";
/// At most this many search terms (D25).
pub const MAX_TERMS: usize = 5;
/// The largest manifest the index takes, in bytes as posted.
pub const MAX_MANIFEST_BYTES: usize = 16 * 1024;
/// The longest URL in a manifest.
pub const MAX_URL: usize = 512;
/// The longest owner name, in characters.
pub const MAX_OWNER_NAME: usize = 80;
/// At most this many languages.
pub const MAX_LANGUAGES: usize = 16;
/// The longest manifest id, so its file name stays below the 255 bytes a
/// file system allows (`<domain>~<name>.json.tmp`).
pub const MAX_ID: usize = 200;

/// Top-level names that only exist inside networks or are reserved for
/// special use (RFC 6761, RFC 6762, RFC 8375), so no domain below them can
/// be verified. `.example` stays allowed, the demo answers for it.
const INTERNAL_TLDS: [&str; 13] =
    ["localhost", "local", "localdomain", "internal", "intranet", "lan", "home", "corp", "private", "arpa", "test", "invalid", "onion"];

/// What the index accepts besides public HTTPS URLs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Policy {
    /// `http://127.0.0.1:<port>` and `http://localhost:<port>` stand in for
    /// a domain, for tests and the demo.
    pub allow_http_localhost: bool,
}

/// Every problem of the manifest, or nothing.
pub fn validate(m: &Manifest, policy: Policy) -> Result<(), Vec<String>> {
    let mut problems = Vec::new();
    let mut problem = |p: String| problems.push(p);

    if m.meerora_manifest != FORMAT {
        problem(format!("meerora_manifest must be {FORMAT:?}, not {:?}", short(&m.meerora_manifest)));
    }
    for (what, value, max, lines) in
        [("name", &m.name, 60, false), ("owner.name", &m.owner.name, MAX_OWNER_NAME, false), ("welcome", &m.welcome, 500, true)]
    {
        if let Some(p) = text_problem(what, value, max, lines, true) {
            problem(p);
        }
    }

    let owner = m.owner.domain.as_str();
    match m.kind {
        ManifestKind::Organisation if !is_domain(owner) && !(policy.allow_http_localhost && is_local_host(owner)) => {
            problem(format!("owner.domain {owner:?} is not a domain name"))
        }
        ManifestKind::Person if !is_handle(owner) => {
            problem(format!("owner.domain {owner:?} is not an index handle (2 to 32 of a-z, 0-9, - and _)"))
        }
        _ => {}
    }
    if m.id.len() > MAX_ID {
        problem(format!("id is longer than {MAX_ID} characters"));
    }
    match m.id.split_once('/') {
        Some((prefix, name)) => {
            if prefix != owner {
                problem(format!(
                    "id must start with the owner's {} {owner:?}",
                    if m.kind == ManifestKind::Person { "handle" } else { "domain" }
                ));
            }
            if !is_agent_name(name) {
                problem(format!("the name part of id {:?} must be 1 to 64 of a-z, 0-9 and -, starting with a letter or digit", m.id));
            }
        }
        None => problem(format!("id {:?} must look like <domain or handle>/<name>", short(&m.id))),
    }

    if m.search_terms.len() > MAX_TERMS {
        problem(format!("{} search terms, at most {MAX_TERMS} are allowed", m.search_terms.len()));
    }
    for term in &m.search_terms {
        if let Some(p) = text_problem("a search term", term, 40, false, true) {
            problem(p);
        }
    }

    if m.endpoint.url.trim().is_empty() {
        problem("endpoint.url is empty".into());
    } else if let Err(e) = check_url(&m.endpoint.url, policy) {
        problem(format!("endpoint.url {e}"));
    }
    if m.endpoint.protocol != PROTOCOL {
        problem(format!("endpoint.protocol must be {PROTOCOL:?}, not {:?}", short(&m.endpoint.protocol)));
    }
    if let Err(e) = check_url(&m.figure.url, policy) {
        problem(format!("figure.url {e}"));
    }
    if m.figure.sha256.len() != 64 || !m.figure.sha256.chars().all(|c| c.is_ascii_hexdigit()) {
        problem("figure.sha256 must be 64 hex digits".into());
    }
    if let Some(picture) = &m.picture
        && let Err(e) = check_url(picture, policy)
    {
        problem(format!("picture {e}"));
    }
    if let Some(card) = &m.a2a_card
        && let Err(e) = check_url(card, policy)
    {
        problem(format!("a2a_card {e}"));
    }
    if let Some(privacy) = &m.privacy
        && let Some(p) = text_problem("privacy", privacy, 500, true, false)
    {
        problem(p);
    }
    if m.languages.len() > MAX_LANGUAGES {
        problem(format!("{} languages, at most {MAX_LANGUAGES} are allowed", m.languages.len()));
    }
    for lang in m.languages.iter().take(MAX_LANGUAGES) {
        if lang.is_empty() || lang.len() > 16 || !lang.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            problem(format!("language {:?} is not a language tag", short(lang)));
        }
    }
    if problems.is_empty() { Ok(()) } else { Err(problems) }
}

/// The first characters of `s`, so a problem never repeats a whole field.
fn short(s: &str) -> String {
    let mut out: String = s.chars().take(64).collect();
    if out.len() < s.len() {
        out.push('…');
    }
    out
}

/// Control characters, and the bidirectional marks and overrides that can
/// turn the order of the text around (Unicode TR 9).
pub fn is_unsafe_char(c: char) -> bool {
    c.is_control() || matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// The problem of a text field that people read, if any. `lines` allows line
/// breaks, `required` refuses an empty value.
pub fn text_problem(what: &str, value: &str, max: usize, lines: bool, required: bool) -> Option<String> {
    if value.trim().is_empty() {
        return required.then(|| format!("{what} is empty"));
    }
    if value.chars().count() > max {
        return Some(format!("{what} is longer than {max} characters"));
    }
    if value.chars().any(|c| is_unsafe_char(c) && !(lines && c == '\n')) {
        return Some(format!("{what} contains control or bidirectional characters"));
    }
    None
}

/// An `https` URL on a public domain, or with the policy an `http` URL on
/// this machine. User names, IP addresses and internal names are refused.
pub fn check_url(url: &str, policy: Policy) -> Result<(), String> {
    if url.len() > MAX_URL {
        return Err(format!("is longer than {MAX_URL} characters"));
    }
    let shown = short(url);
    if url.chars().any(|c| c.is_whitespace() || is_unsafe_char(c)) {
        return Err(format!("{shown:?} contains spaces or control characters"));
    }
    let (scheme, rest) = url.split_once("://").ok_or_else(|| format!("{shown:?} is not a URL"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return Err(format!("{shown:?} has no valid host"));
    }
    match scheme {
        "https" => {
            let host = host_of(authority).ok_or_else(|| format!("{shown:?} has no valid port"))?;
            if is_domain(host) { Ok(()) } else { Err(format!("{shown:?} must name a public domain in lower case, not {:?}", short(host))) }
        }
        "http" if policy.allow_http_localhost && is_local_host(authority) => Ok(()),
        "http" => Err(format!("{shown:?} must use https")),
        _ => Err(format!("{shown:?} does not use https")),
    }
}

/// The host of `host[:port]`, None when the port is not a number.
fn host_of(authority: &str) -> Option<&str> {
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.contains(']') => port.parse::<u16>().ok().map(|_| host),
        _ => Some(authority),
    }
}

/// The host of a URL, without the port, for comparing it with a domain.
pub fn url_host(url: &str) -> Option<&str> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.starts_with('[') {
        return authority.split_inclusive(']').next();
    }
    host_of(authority)
}

/// Whether `host` is `domain` itself or a name below it.
pub fn is_within(host: &str, domain: &str) -> bool {
    host == domain || host.strip_suffix(domain).is_some_and(|rest| rest.ends_with('.'))
}

/// A lower-case public host name with at least one dot, such as
/// `meridian.example`. IP addresses in any notation are refused: their last
/// label is a number (`127.0.0.1`, `127.1`, `0x7f.1`), and no top-level
/// domain is all digits. Names below internal or special-use top-level
/// names such as `.local`, `.internal` or `.arpa` are refused too.
pub fn is_domain(s: &str) -> bool {
    let labels_ok = s.len() <= 253
        && s.contains('.')
        && s.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        });
    let tld = s.rsplit('.').next().unwrap_or("");
    labels_ok && !is_numeric_label(tld) && !INTERNAL_TLDS.contains(&tld)
}

/// A label that reads as a number: decimal, octal or `0x` hex.
fn is_numeric_label(label: &str) -> bool {
    let hex = label.strip_prefix("0x").is_some_and(|h| h.chars().all(|c| c.is_ascii_hexdigit()));
    hex || label.chars().all(|c| c.is_ascii_digit())
}

/// `127.0.0.1`, `localhost` or `[::1]`, with an optional port.
pub fn is_local_host(host_port: &str) -> bool {
    let host = match host_port.strip_prefix('[') {
        Some(rest) => rest.split(']').next().map(|h| format!("[{h}]")).unwrap_or_default(),
        None => host_port.split(':').next().unwrap_or("").to_string(),
    };
    let port_ok = match host_port.strip_prefix(host.as_str()) {
        Some("") => true,
        Some(rest) => rest.strip_prefix(':').is_some_and(|p| !p.is_empty() && p.parse::<u16>().is_ok()),
        None => false,
    };
    port_ok && matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]")
}

/// An index handle of a person, for example `alex`.
pub fn is_handle(s: &str) -> bool {
    (2..=32).contains(&s.len())
        && s.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn is_agent_name(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use meerora_protocol::{ManifestEndpoint, ManifestFigure, ManifestOwner};

    pub fn manifest() -> Manifest {
        Manifest {
            meerora_manifest: "0.1".into(),
            id: "meridian.example/mira".into(),
            kind: ManifestKind::Organisation,
            name: "Mira".into(),
            owner: ManifestOwner { name: "Meridian Software".into(), domain: "meridian.example".into() },
            picture: Some("https://meridian.example/meerora/mira.png".into()),
            figure: ManifestFigure { url: "https://meridian.example/meerora/mira.js".into(), sha256: "ab".repeat(32) },
            welcome: "Hi, I'm Mira from Meridian. Ask me about plans, seats and data residency.".into(),
            search_terms: vec!["software licensing".into(), "seat pricing".into(), "EU data residency".into()],
            endpoint: ManifestEndpoint { url: "https://meridian.example/a2a".into(), protocol: "a2a/1.0".into() },
            a2a_card: Some("https://meridian.example/.well-known/agent-card.json".into()),
            privacy: Some("Meridian sees what you say to Mira and nothing else.".into()),
            languages: vec!["en".into(), "de".into()],
        }
    }

    fn problems(m: &Manifest) -> String {
        validate(m, Policy::default()).err().unwrap_or_default().join("\n")
    }

    #[test]
    fn the_manifest_from_the_brief_is_valid() {
        assert_eq!(validate(&manifest(), Policy::default()), Ok(()));
    }

    #[test]
    fn rejects_broken_manifests() {
        let mut m = manifest();
        m.meerora_manifest = "0.2".into();
        m.name = "  ".into();
        m.welcome = String::new();
        m.search_terms = (0..6).map(|i| format!("term {i}")).collect();
        m.endpoint.url = String::new();
        let p = problems(&m);
        for needle in ["meerora_manifest", "name is empty", "welcome is empty", "6 search terms", "endpoint.url is empty"] {
            assert!(p.contains(needle), "{needle} missing in {p}");
        }

        let mut m = manifest();
        m.id = "other.example/mira".into();
        assert!(problems(&m).contains("must start with the owner's domain"));
        m.id = "mira".into();
        assert!(problems(&m).contains("<domain or handle>/<name>"));
        m.id = "meridian.example/Mira Bot".into();
        assert!(problems(&m).contains("name part"));

        let mut m = manifest();
        m.endpoint.url = "http://meridian.example/a2a".into();
        m.figure.sha256 = "xyz".into();
        m.endpoint.protocol = "mcp".into();
        let p = problems(&m);
        assert!(p.contains("must use https") && p.contains("64 hex digits") && p.contains("a2a/1.0"), "{p}");

        let mut m = manifest();
        m.owner.domain = "Meridian".into();
        m.id = "Meridian/mira".into();
        assert!(problems(&m).contains("is not a domain name"));
    }

    #[test]
    fn people_use_handles_and_localhost_needs_the_policy() {
        let mut m = manifest();
        m.kind = ManifestKind::Person;
        m.owner = ManifestOwner { name: "Alex Example".into(), domain: "alex".into() };
        m.id = "alex/alex".into();
        assert_eq!(validate(&m, Policy::default()), Ok(()));
        m.owner.domain = "Alex Example".into();
        assert!(problems(&m).contains("index handle"));

        let mut m = manifest();
        m.owner.domain = "127.0.0.1:7878".into();
        m.id = "127.0.0.1:7878/mira".into();
        m.endpoint.url = "http://127.0.0.1:7879/a2a/mira".into();
        assert!(validate(&m, Policy::default()).is_err());
        assert_eq!(validate(&m, Policy { allow_http_localhost: true }), Ok(()));
    }

    #[test]
    fn hosts() {
        assert!(is_local_host("127.0.0.1:7878") && is_local_host("localhost") && is_local_host("[::1]:80"));
        assert!(!is_local_host("127.0.0.2:80") && !is_local_host("localhost:x") && !is_local_host("evil.example"));
        assert!(is_domain("meridian.example") && !is_domain("meridian") && !is_domain("-a.example") && !is_domain("a..b"));
        assert!(check_url("https://a.example/x", Policy::default()).is_ok());
        assert!(check_url("https://a.example:8443/x", Policy::default()).is_ok());
        assert!(check_url("ftp://a.example/x", Policy::default()).is_err());
        assert!(check_url("https://user@a.example/x", Policy::default()).is_err());
        assert_eq!(url_host("https://a.example:8443/x?y"), Some("a.example"));
        assert!(is_within("cdn.a.example", "a.example") && is_within("a.example", "a.example"));
        assert!(!is_within("evila.example", "a.example") && !is_within("a.example.evil.org", "a.example"));
    }

    #[test]
    fn ip_literals_and_internal_names_are_not_domains() {
        for host in ["127.0.0.1", "127.1", "0x7f.1", "0177.0.0.1", "10.0.0.1", "1.2.3.0x4", "printer.local", "db.internal", "x.localhost"] {
            assert!(!is_domain(host), "{host} must not count as a domain");
        }
        for host in ["163.com", "1password.com", "meridian.example", "a-b.co.uk"] {
            assert!(is_domain(host), "{host} is a domain");
        }
        for url in ["https://127.0.0.1/a", "https://[::1]/a", "https://169.254.169.254/latest", "https://router.lan/x", "https://0x7f.1/"] {
            assert!(check_url(url, Policy::default()).is_err(), "{url}");
            assert!(check_url(url, Policy { allow_http_localhost: true }).is_err(), "{url} with the policy");
        }
        assert!(check_url("http://127.0.0.1:7879/a2a", Policy { allow_http_localhost: true }).is_ok());
    }

    #[test]
    fn every_field_has_a_limit_and_no_hidden_characters() {
        let mut m = manifest();
        m.id = format!("meridian.example/{}", "a".repeat(190));
        assert!(problems(&m).contains("id is longer than 200"));
        let mut m = manifest();
        m.owner.name = "x".repeat(81);
        m.privacy = Some("p".repeat(501));
        m.languages = (0..17).map(|_| "en".into()).collect();
        m.picture = Some(format!("https://meridian.example/{}", "a".repeat(600)));
        let p = problems(&m);
        for needle in ["owner.name is longer than 80", "privacy is longer than 500", "17 languages", "picture is longer than 512"] {
            assert!(p.contains(needle), "{needle} missing in {p}");
        }

        // A right-to-left override would show "Mira" as something else.
        let mut m = manifest();
        m.name = "Mi\u{202E}ra".into();
        m.owner.name = "Meridian\u{7}".into();
        m.search_terms = vec!["seat\u{2066}pricing".into()];
        let p = problems(&m);
        assert!(p.contains("name contains control or bidirectional") && p.contains("owner.name contains"), "{p}");
        assert!(p.contains("a search term contains"), "{p}");
        let mut m = manifest();
        m.welcome = "Hi.\nAsk me anything.".into();
        assert_eq!(validate(&m, Policy::default()), Ok(()), "line breaks are fine in the welcome text");
    }
}
