//! What configures the command box: `[commands]` in `meerora.toml` (the
//! key, the assistant agent and the web search) and the user's own
//! keywords in `<data dir>/commands.toml`. The example `commands.toml` and
//! the two example scripts are written into the data dir the first time
//! the box opens there, and an existing file is never replaced.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use meerora_script::CommandAction;
use serde::Deserialize;
use winit::keyboard::{KeyCode, ModifiersState};

/// The user's keywords, next to `meerora.toml`.
pub const COMMANDS_FILE: &str = "commands.toml";
/// The folder of command scripts in the data dir.
pub const SCRIPTS_DIR: &str = "commands";

pub const EXAMPLE_TOML: &str = include_str!("../../../../../assets/commands.toml");
/// The example scripts by file name.
pub const EXAMPLE_SCRIPTS: [(&str, &str); 2] = [
    ("units.js", include_str!("../../../../../assets/commands/units.js")),
    ("github.js", include_str!("../../../../../assets/commands/github.js")),
];

pub const DEFAULT_KEY: &str = "Alt+Space";
pub const DEFAULT_ASSISTANT: &str = "claude";
pub const DEFAULT_SEARCH: &str = "https://duckduckgo.com/?q={query}";

/// `[commands]` of `meerora.toml`.
#[derive(Clone, Debug, PartialEq)]
pub struct BoxSettings {
    pub key: Hotkey,
    /// The agent `?`, `ask` and `??` go to.
    pub assistant: String,
    /// The URL `g query` opens, with `{query}`.
    pub search: String,
    /// Problems with the table, shown as a row in the box.
    pub errors: Vec<String>,
}

impl Default for BoxSettings {
    fn default() -> Self {
        Self {
            key: Hotkey::parse(DEFAULT_KEY).expect("the default key parses"),
            assistant: DEFAULT_ASSISTANT.into(),
            search: DEFAULT_SEARCH.into(),
            errors: Vec::new(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSettings {
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    assistant: Option<String>,
    #[serde(default)]
    search: Option<String>,
}

impl BoxSettings {
    /// Reads `[commands]` from `meerora.toml`. Problems leave the defaults
    /// in place and are listed in `errors`.
    pub fn load(data_dir: &Path) -> Self {
        let mut out = Self::default();
        let table = match meerora_protocol::Settings::load(data_dir) {
            Ok(settings) => settings.commands,
            Err(e) => {
                out.errors.push(e);
                None
            }
        };
        let Some(table) = table else { return out };
        out.apply(table);
        out
    }

    fn apply(&mut self, table: toml::Table) {
        let raw: RawSettings = match toml::Value::Table(table).try_into() {
            Ok(raw) => raw,
            Err(e) => {
                self.errors.push(format!("[commands] in meerora.toml: {e}"));
                return;
            }
        };
        if let Some(key) = raw.key {
            match Hotkey::parse(&key) {
                Ok(k) => self.key = k,
                Err(e) => self.errors.push(format!("[commands] key: {e}")),
            }
        }
        if let Some(assistant) = raw.assistant.map(|a| a.trim().to_string()).filter(|a| !a.is_empty()) {
            self.assistant = assistant;
        }
        if let Some(search) = raw.search {
            if search.starts_with("https://") || search.starts_with("http://") {
                self.search = search;
            } else {
                self.errors.push(format!("[commands] search must be an http or https URL with {{query}}, got {search:?}"));
            }
        }
    }
}

/// The key that opens and closes the box, like `Alt+Space`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub code: KeyCode,
    pub meta: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Hotkey {
    /// Parses `Alt+Space`, `Meta+Space`, `Ctrl+Alt+K`, `Super+Slash` or `F12`.
    /// Modifiers are Meta (or Super, Logo, Win), Ctrl, Alt and Shift.
    pub fn parse(text: &str) -> Result<Hotkey, String> {
        let mut key = Hotkey { code: KeyCode::Space, meta: false, ctrl: false, alt: false, shift: false };
        let parts: Vec<&str> = text.split('+').map(str::trim).collect();
        let (name, mods) = parts.split_last().filter(|(n, _)| !n.is_empty()).ok_or_else(|| format!("{text:?} names no key"))?;
        for m in mods {
            match m.to_ascii_lowercase().as_str() {
                "meta" | "super" | "logo" | "win" | "cmd" => key.meta = true,
                "ctrl" | "control" => key.ctrl = true,
                "alt" => key.alt = true,
                "shift" => key.shift = true,
                other => return Err(format!("{other:?} in {text:?} is not Meta, Ctrl, Alt or Shift")),
            }
        }
        key.code = key_code(name).ok_or_else(|| format!("{name:?} in {text:?} is not a key name such as Space, K, F12 or Slash"))?;
        if !(key.meta || key.ctrl || key.alt)
            && !matches!(
                key.code,
                KeyCode::F1
                    | KeyCode::F2
                    | KeyCode::F3
                    | KeyCode::F4
                    | KeyCode::F5
                    | KeyCode::F6
                    | KeyCode::F7
                    | KeyCode::F8
                    | KeyCode::F9
                    | KeyCode::F10
                    | KeyCode::F11
                    | KeyCode::F12
            )
        {
            return Err(format!("{text:?} needs Meta, Ctrl or Alt, or must be a function key, so typing is not taken"));
        }
        Ok(key)
    }

    /// Whether a key event with these modifiers is this hotkey.
    pub fn matches(&self, code: KeyCode, mods: ModifiersState) -> bool {
        code == self.code
            && mods.super_key() == self.meta
            && mods.control_key() == self.ctrl
            && mods.alt_key() == self.alt
            && mods.shift_key() == self.shift
    }

    /// For the footer of the box, like "Alt+Space".
    pub fn label(&self) -> String {
        let mut parts = Vec::new();
        for (on, name) in [(self.meta, "Meta"), (self.ctrl, "Ctrl"), (self.alt, "Alt"), (self.shift, "Shift")] {
            if on {
                parts.push(name.to_string());
            }
        }
        parts.push(format!("{:?}", self.code).trim_start_matches("Key").trim_start_matches("Digit").to_string());
        parts.join("+")
    }
}

fn key_code(name: &str) -> Option<KeyCode> {
    use KeyCode::*;
    let lower = name.to_ascii_lowercase();
    let letter = |c: char| -> Option<KeyCode> {
        Some(match c {
            'a' => KeyA,
            'b' => KeyB,
            'c' => KeyC,
            'd' => KeyD,
            'e' => KeyE,
            'f' => KeyF,
            'g' => KeyG,
            'h' => KeyH,
            'i' => KeyI,
            'j' => KeyJ,
            'k' => KeyK,
            'l' => KeyL,
            'm' => KeyM,
            'n' => KeyN,
            'o' => KeyO,
            'p' => KeyP,
            'q' => KeyQ,
            'r' => KeyR,
            's' => KeyS,
            't' => KeyT,
            'u' => KeyU,
            'v' => KeyV,
            'w' => KeyW,
            'x' => KeyX,
            'y' => KeyY,
            'z' => KeyZ,
            '0' => Digit0,
            '1' => Digit1,
            '2' => Digit2,
            '3' => Digit3,
            '4' => Digit4,
            '5' => Digit5,
            '6' => Digit6,
            '7' => Digit7,
            '8' => Digit8,
            '9' => Digit9,
            _ => return None,
        })
    };
    if lower.chars().count() == 1 {
        return letter(lower.chars().next()?);
    }
    Some(match lower.as_str() {
        "space" => Space,
        "enter" | "return" => Enter,
        "tab" => Tab,
        "slash" => Slash,
        "period" | "dot" => Period,
        "comma" => Comma,
        "semicolon" => Semicolon,
        "minus" => Minus,
        "equal" | "equals" => Equal,
        "backquote" | "grave" => Backquote,
        "backslash" => Backslash,
        "f1" => F1,
        "f2" => F2,
        "f3" => F3,
        "f4" => F4,
        "f5" => F5,
        "f6" => F6,
        "f7" => F7,
        "f8" => F8,
        "f9" => F9,
        "f10" => F10,
        "f11" => F11,
        "f12" => F12,
        _ => return None,
    })
}

// ------------------------------------------------------------ commands.toml

/// One `[[command]]` of `commands.toml`.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub keyword: String,
    pub title: String,
    pub subtitle: String,
    pub template: Template,
}

/// The action of an entry, with `{query}` where the typed text goes.
#[derive(Clone, Debug, PartialEq)]
pub enum Template {
    OpenUrl(String),
    /// The program and its arguments.
    Launch(Vec<String>),
    /// A message for `agent`, the assistant when unset.
    Say {
        agent: Option<String>,
        text: String,
    },
    OpenApp(String),
    Copy(String),
    Notify(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    command: Vec<toml::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEntry {
    keyword: String,
    title: String,
    #[serde(default)]
    subtitle: Option<String>,
    #[serde(default)]
    open_url: Option<String>,
    #[serde(default)]
    launch: Option<Vec<String>>,
    #[serde(default)]
    say: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    open_app: Option<String>,
    #[serde(default)]
    copy: Option<String>,
    #[serde(default)]
    notify: Option<String>,
}

/// Reads `commands.toml`. A broken entry is left out with an error, the
/// others still load. A missing file has no entries.
pub fn load_entries(data_dir: &Path) -> (Vec<Entry>, Vec<String>) {
    let path = data_dir.join(COMMANDS_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_entries(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Vec::new(), Vec::new()),
        Err(e) => (Vec::new(), vec![format!("{}: {e}", path.display())]),
    }
}

pub fn parse_entries(text: &str) -> (Vec<Entry>, Vec<String>) {
    let file: RawFile = match toml::from_str(text) {
        Ok(f) => f,
        Err(e) => return (Vec::new(), vec![format!("{COMMANDS_FILE}: {}", e.to_string().trim())]),
    };
    let mut entries: Vec<Entry> = Vec::new();
    let mut errors = Vec::new();
    for (n, value) in file.command.into_iter().enumerate() {
        let what = format!("{COMMANDS_FILE}, command {}", n + 1);
        let raw: RawEntry = match value.try_into() {
            Ok(raw) => raw,
            Err(e) => {
                errors.push(format!("{what}: {}", e.to_string().trim()));
                continue;
            }
        };
        match entry(raw) {
            Ok(e) if entries.iter().any(|o| o.keyword == e.keyword) => {
                errors.push(format!("{what}: the keyword {:?} is taken already", e.keyword))
            }
            Ok(e) => entries.push(e),
            Err(e) => errors.push(format!("{what}: {e}")),
        }
    }
    (entries, errors)
}

fn entry(raw: RawEntry) -> Result<Entry, String> {
    let keyword = raw.keyword.trim().to_string();
    if keyword.is_empty() || keyword.chars().count() > 24 || keyword.contains(char::is_whitespace) {
        return Err(format!("keyword must be one word of at most 24 characters, got {:?}", raw.keyword));
    }
    if matches!(keyword.as_str(), "?" | "??" | "ask" | "g") || keyword.starts_with('@') {
        return Err(format!("the keyword {keyword:?} belongs to the box itself"));
    }
    if raw.title.trim().is_empty() {
        return Err("title is empty".into());
    }
    let mut templates = Vec::new();
    if let Some(url) = raw.open_url {
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err(format!("open_url must be an http or https URL, got {url:?}"));
        }
        templates.push(Template::OpenUrl(url));
    }
    if let Some(launch) = raw.launch {
        if launch.first().is_none_or(|p| p.trim().is_empty()) {
            return Err("launch needs a program, like launch = [\"gnome-calculator\"]".into());
        }
        templates.push(Template::Launch(launch));
    }
    if let Some(text) = raw.say {
        templates.push(Template::Say { agent: raw.agent.clone(), text });
    } else if raw.agent.is_some() {
        return Err("agent only goes with say".into());
    }
    if let Some(app) = raw.open_app {
        if crate::ui::AppTab::parse_handle(&app).is_none_or(|t| t.app().is_none()) {
            return Err(format!("open_app must be a built-in app such as editor, terminal or browser:2, got {app:?}"));
        }
        templates.push(Template::OpenApp(app));
    }
    if let Some(text) = raw.copy {
        templates.push(Template::Copy(text));
    }
    if let Some(text) = raw.notify {
        templates.push(Template::Notify(text));
    }
    if templates.len() != 1 {
        return Err("give exactly one of open_url, launch, say, open_app, copy or notify".into());
    }
    Ok(Entry { keyword, title: raw.title.trim().to_string(), subtitle: raw.subtitle.unwrap_or_default(), template: templates.remove(0) })
}

impl Entry {
    /// The action for `query`. URLs get the query percent-encoded, the
    /// other actions get it as typed. Launch arguments stay separate words,
    /// no shell sees them.
    pub fn action(&self, query: &str, assistant: &str) -> CommandAction {
        let fill = |t: &str| t.replace("{query}", query);
        match &self.template {
            Template::OpenUrl(url) => CommandAction::OpenUrl { url: url.replace("{query}", &encode(query)) },
            Template::Launch(words) => {
                CommandAction::Launch { program: fill(&words[0]), args: words[1..].iter().map(|w| fill(w)).collect() }
            }
            Template::Say { agent, text } => {
                CommandAction::Say { agent: agent.clone().unwrap_or_else(|| assistant.to_string()), text: fill(text) }
            }
            Template::OpenApp(app) => CommandAction::OpenApp { app: app.clone() },
            Template::Copy(text) => CommandAction::Copy { text: fill(text) },
            Template::Notify(text) => CommandAction::Notify { text: fill(text) },
        }
    }

    /// Whether the entry does something with the typed text.
    pub fn takes_query(&self) -> bool {
        match &self.template {
            Template::OpenUrl(t) | Template::Copy(t) | Template::Notify(t) | Template::Say { text: t, .. } => t.contains("{query}"),
            Template::Launch(words) => words.iter().any(|w| w.contains("{query}")),
            Template::OpenApp(_) => false,
        }
    }
}

/// Percent-encodes everything but the unreserved characters of RFC 3986.
pub fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Writes the example `commands.toml` and, when the folder is missing, the
/// `commands/` folder with the example scripts. Existing files stay as they
/// are. Returns what it wrote.
pub fn ensure_examples(data_dir: &Path) -> Vec<PathBuf> {
    let mut written = Vec::new();
    let mut create = |path: PathBuf, text: &str| {
        let file = std::fs::OpenOptions::new().write(true).create_new(true).open(&path);
        match file.and_then(|mut f| f.write_all(text.as_bytes())) {
            Ok(()) => written.push(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => tracing::warn!("cannot write {} ({e})", path.display()),
        }
    };
    if !data_dir.is_dir() {
        return written;
    }
    create(data_dir.join(COMMANDS_FILE), EXAMPLE_TOML);
    let scripts = data_dir.join(SCRIPTS_DIR);
    if !scripts.exists() {
        match std::fs::create_dir(&scripts) {
            Ok(()) => {
                for (name, text) in EXAMPLE_SCRIPTS {
                    create(scripts.join(name), text);
                }
            }
            Err(e) => tracing::warn!("cannot create {} ({e})", scripts.display()),
        }
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_example_file_parses_cleanly() {
        let (entries, errors) = parse_entries(EXAMPLE_TOML);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(entries.len() >= 5, "{entries:?}");
        let kinds: Vec<&str> = entries
            .iter()
            .map(|e| match e.template {
                Template::OpenUrl(_) => "open_url",
                Template::Launch(_) => "launch",
                Template::Say { .. } => "say",
                Template::OpenApp(_) => "open_app",
                Template::Copy(_) => "copy",
                Template::Notify(_) => "notify",
            })
            .collect();
        for kind in ["open_url", "launch", "say", "open_app", "copy"] {
            assert!(kinds.contains(&kind), "the example shows {kind}: {kinds:?}");
        }
    }

    #[test]
    fn entries_fill_in_the_query() {
        let text = r#"
            [[command]]
            keyword = "w"
            title = "Wikipedia"
            open_url = "https://en.wikipedia.org/w/index.php?search={query}"

            [[command]]
            keyword = "calc"
            title = "Calculator"
            launch = ["gnome-calculator", "--equation", "{query}"]

            [[command]]
            keyword = "standup"
            title = "Standup"
            say = "Write my standup. {query}"

            [[command]]
            keyword = "review"
            title = "Review"
            say = "Review {query}"
            agent = "ember"

            [[command]]
            keyword = "ed"
            title = "Editor"
            open_app = "editor"

            [[command]]
            keyword = "shrug"
            title = "Shrug"
            copy = "¯\\_(ツ)_/¯"
        "#;
        let (entries, errors) = parse_entries(text);
        assert!(errors.is_empty(), "{errors:?}");
        let by = |k: &str| entries.iter().find(|e| e.keyword == k).unwrap();
        assert_eq!(
            by("w").action("rust lang & more", "claude"),
            CommandAction::OpenUrl { url: "https://en.wikipedia.org/w/index.php?search=rust%20lang%20%26%20more".into() }
        );
        assert_eq!(
            by("calc").action("2 + 2; rm -rf ~", "claude"),
            CommandAction::Launch { program: "gnome-calculator".into(), args: vec!["--equation".into(), "2 + 2; rm -rf ~".into()] }
        );
        assert_eq!(by("standup").action("", "claude"), CommandAction::Say { agent: "claude".into(), text: "Write my standup. ".into() });
        assert_eq!(by("review").action("main.rs", "claude"), CommandAction::Say { agent: "ember".into(), text: "Review main.rs".into() });
        assert_eq!(by("ed").action("x", "claude"), CommandAction::OpenApp { app: "editor".into() });
        assert_eq!(by("shrug").action("", "claude"), CommandAction::Copy { text: "¯\\_(ツ)_/¯".into() });
        assert!(by("w").takes_query() && !by("ed").takes_query() && !by("shrug").takes_query());
    }

    #[test]
    fn broken_entries_are_reported_and_skipped() {
        let text = r#"
            [[command]]
            keyword = "ok"
            title = "Fine"
            copy = "x"

            [[command]]
            keyword = "two"
            title = "Two actions"
            copy = "x"
            notify = "y"

            [[command]]
            keyword = "bad url"
            title = "Spaces"
            open_url = "https://example.org"

            [[command]]
            keyword = "file"
            title = "Files"
            open_url = "file:///etc"

            [[command]]
            keyword = "typo"
            title = "Typo"
            open_ulr = "https://example.org"

            [[command]]
            keyword = "ok"
            title = "Again"
            copy = "y"

            [[command]]
            keyword = "g"
            title = "Mine"
            copy = "y"

            [[command]]
            keyword = "app"
            title = "App"
            open_app = "window:3"
        "#;
        let (entries, errors) = parse_entries(text);
        assert_eq!(entries.iter().map(|e| e.keyword.as_str()).collect::<Vec<_>>(), ["ok"]);
        assert_eq!(errors.len(), 7, "{errors:#?}");
        assert!(errors[0].contains("command 2") && errors[0].contains("exactly one"));
        assert!(errors[1].contains("one word"));
        assert!(errors[2].contains("http or https"));
        assert!(errors[3].contains("open_ulr"), "{}", errors[3]);
        assert!(errors[4].contains("taken already"));
        assert!(errors[5].contains("belongs to the box"));
        assert!(errors[6].contains("built-in app"));
        let (none, errors) = parse_entries("[[command]\nkeyword = ");
        assert!(none.is_empty() && errors.len() == 1 && errors[0].starts_with("commands.toml"), "{errors:?}");
    }

    #[test]
    fn hotkeys_parse_and_match() {
        let key = Hotkey::parse(DEFAULT_KEY).unwrap();
        assert!(key.matches(KeyCode::Space, ModifiersState::ALT), "the default is Alt+Space");
        assert_eq!(key.label(), "Alt+Space");
        let key = Hotkey::parse("Meta+Space").unwrap();
        assert!(key.matches(KeyCode::Space, ModifiersState::SUPER));
        assert!(!key.matches(KeyCode::Space, ModifiersState::SUPER | ModifiersState::SHIFT), "exactly these modifiers");
        assert!(!key.matches(KeyCode::Space, ModifiersState::empty()));
        assert_eq!(key.label(), "Meta+Space");
        let key = Hotkey::parse("ctrl + alt + k").unwrap();
        assert!(key.matches(KeyCode::KeyK, ModifiersState::CONTROL | ModifiersState::ALT));
        assert_eq!(key.label(), "Ctrl+Alt+K");
        assert!(Hotkey::parse("F12").is_ok());
        assert!(Hotkey::parse("Super+Slash").is_ok());
        assert!(Hotkey::parse("Space").unwrap_err().contains("needs Meta"));
        assert!(Hotkey::parse("Hyper+Space").unwrap_err().contains("not Meta"));
        assert!(Hotkey::parse("Meta+Banana").unwrap_err().contains("not a key name"));
        assert!(Hotkey::parse("Meta+").is_err());
    }

    #[test]
    fn settings_come_from_meerora_toml() {
        let dir = std::env::temp_dir().join(format!("meerora-commands-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(BoxSettings::load(&dir), BoxSettings::default(), "no file gives the defaults");
        std::fs::write(
            dir.join("meerora.toml"),
            "[commands]\nkey = \"Ctrl+Space\"\nassistant = \"ember\"\nsearch = \"https://www.startpage.com/do/search?q={query}\"\n",
        )
        .unwrap();
        let s = BoxSettings::load(&dir);
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        assert_eq!((s.key.label().as_str(), s.assistant.as_str()), ("Ctrl+Space", "ember"));
        assert!(s.search.starts_with("https://www.startpage.com"));
        std::fs::write(dir.join("meerora.toml"), "[commands]\nkey = \"Space\"\ncolour = 1\n").unwrap();
        let s = BoxSettings::load(&dir);
        assert_eq!(s.key, BoxSettings::default().key);
        assert_eq!(s.errors.len(), 1, "{:?}", s.errors);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn examples_are_written_once_and_never_replaced() {
        let dir = std::env::temp_dir().join(format!("meerora-commands-examples-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let written = ensure_examples(&dir);
        assert_eq!(written.len(), 3, "{written:?}");
        assert_eq!(std::fs::read_to_string(dir.join(COMMANDS_FILE)).unwrap(), EXAMPLE_TOML);
        std::fs::write(dir.join(COMMANDS_FILE), "# mine\n").unwrap();
        std::fs::remove_file(dir.join("commands/github.js")).unwrap();
        assert!(ensure_examples(&dir).is_empty(), "an existing folder is the user's");
        assert_eq!(std::fs::read_to_string(dir.join(COMMANDS_FILE)).unwrap(), "# mine\n");
        assert!(!dir.join("commands/github.js").exists(), "a deleted example stays deleted");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
