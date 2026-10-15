//! The installed apps for "Your apps" in the launcher (D15 phase 2). They
//! are the `.desktop` files of the freedesktop.org desktop entry spec in
//! `~/.local/share/applications` and every `applications` folder of
//! `$XDG_DATA_DIRS`. The user's own folder wins over the system ones.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The file of the most recently used apps in the data directory.
const RECENT_FILE: &str = "recent-apps.json";

#[derive(Clone, Debug, PartialEq)]
pub struct DesktopApp {
    /// Desktop file id, for example `org.gnome.TextEditor.desktop`.
    pub id: String,
    pub name: String,
    /// Comment, else GenericName, shown beside the name.
    pub blurb: String,
    /// GenericName, Comment and Keywords, for the search.
    pub hints: String,
    /// The program and its arguments, field codes removed.
    pub exec: Vec<String>,
}

/// The scanned apps and the recently used ones, newest first.
pub struct AppList {
    pub apps: Vec<DesktopApp>,
    recent: HashMap<String, u64>,
    scanned: Instant,
    data_dir: PathBuf,
}

impl AppList {
    pub fn load(data_dir: &Path) -> AppList {
        let recent =
            std::fs::read_to_string(data_dir.join(RECENT_FILE)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        AppList { apps: scan(&search_dirs()), recent, scanned: Instant::now(), data_dir: data_dir.to_path_buf() }
    }

    /// Scans again when the last scan is older than a few seconds, so a
    /// newly installed app shows when the launcher opens again.
    pub fn refresh(&mut self) {
        if self.scanned.elapsed() > Duration::from_secs(10) {
            self.apps = scan(&search_dirs());
            self.scanned = Instant::now();
        }
    }

    /// The apps matching `query`, most recently used first, then by name.
    pub fn matching(&self, query: &str) -> Vec<&DesktopApp> {
        let query = query.trim().to_lowercase();
        let mut out: Vec<&DesktopApp> = self
            .apps
            .iter()
            .filter(|a| query.is_empty() || a.name.to_lowercase().contains(&query) || a.hints.to_lowercase().contains(&query))
            .collect();
        out.sort_by(|a, b| {
            let (ra, rb) = (self.recent.get(&a.id).copied().unwrap_or(0), self.recent.get(&b.id).copied().unwrap_or(0));
            rb.cmp(&ra).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        out
    }

    /// Remembers that `id` was just launched.
    pub fn used(&mut self, id: &str) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        self.recent.insert(id.to_string(), now);
        let path = self.data_dir.join(RECENT_FILE);
        let written = serde_json::to_string_pretty(&self.recent)
            .map_err(|e| e.to_string())
            .and_then(|t| std::fs::write(&path, t).map_err(|e| e.to_string()));
        if let Err(e) = written {
            tracing::warn!("cannot save {} ({e})", path.display());
        }
    }
}

/// `$XDG_DATA_HOME/applications` (or `~/.local/share/applications`) first,
/// then `applications` in every folder of `$XDG_DATA_DIRS`.
pub fn search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    match std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        Some(home) => dirs.push(PathBuf::from(home)),
        None => dirs.extend(std::env::var_os("HOME").map(|h| Path::new(&h).join(".local/share"))),
    }
    let system = std::env::var("XDG_DATA_DIRS").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    dirs.extend(system.split(':').filter(|d| !d.is_empty()).map(PathBuf::from));
    dirs.into_iter().map(|d| d.join("applications")).collect()
}

/// Every app in `dirs` that wants to be shown. An id found in an earlier
/// folder hides the same id in later ones, even when that entry is hidden.
pub fn scan(dirs: &[PathBuf]) -> Vec<DesktopApp> {
    let mut seen = HashSet::new();
    let mut apps = Vec::new();
    for dir in dirs {
        let mut files = Vec::new();
        collect(dir, dir, &mut files, 0);
        files.sort();
        for (id, path) in files {
            if !seen.insert(id.clone()) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            if let Some(app) = parse(&id, &text) {
                apps.push(app);
            }
        }
    }
    apps.sort_by_key(|a| a.name.to_lowercase());
    apps
}

/// The `.desktop` files below `dir` with their ids. An id is the path below
/// the `applications` folder with `/` turned into `-`.
fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>, depth: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && depth < 3 {
            collect(root, &path, out, depth + 1);
        } else if path.extension().is_some_and(|e| e == "desktop")
            && let Ok(rel) = path.strip_prefix(root)
        {
            out.push((rel.to_string_lossy().replace('/', "-"), path));
        }
    }
}

/// Parses the `[Desktop Entry]` group. Leaves out what should not show in
/// a launcher: other types, NoDisplay, Hidden, Terminal=true, entries only
/// for other desktops, a missing TryExec and an empty Exec.
pub fn parse(id: &str, text: &str) -> Option<DesktopApp> {
    let mut in_entry = false;
    let mut fields: HashMap<&str, &str> = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            fields.entry(key.trim()).or_insert(value.trim());
        }
    }
    let yes = |key: &str| fields.get(key).is_some_and(|v| *v == "true");
    if fields.get("Type").copied() != Some("Application") || yes("NoDisplay") || yes("Hidden") || yes("Terminal") {
        return None;
    }
    if fields.get("OnlyShowIn").is_some_and(|v| !v.is_empty()) {
        return None;
    }
    if let Some(try_exec) = fields.get("TryExec")
        && !on_path(&unescape(try_exec))
    {
        return None;
    }
    let exec = exec_args(fields.get("Exec")?)?;
    let name = unescape(fields.get("Name")?);
    let hints =
        ["GenericName", "Comment", "Keywords"].iter().filter_map(|k| fields.get(k)).map(|v| unescape(v)).collect::<Vec<_>>().join(" ");
    let blurb = fields.get("Comment").or_else(|| fields.get("GenericName")).map(|v| unescape(v)).unwrap_or_default();
    Some(DesktopApp { id: id.to_string(), name, blurb, hints, exec })
}

/// Splits an Exec value into arguments and drops the field codes (`%f`,
/// `%U` and the others), since meerora starts apps without files. `%%`
/// stays a percent sign. Quoting follows the desktop entry spec.
pub fn exec_args(exec: &str) -> Option<Vec<String>> {
    let exec = unescape(exec);
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut in_arg = false;
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                in_arg = true;
            }
            '\\' if quoted => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            c if c.is_whitespace() && !quoted => {
                if in_arg {
                    args.push(std::mem::take(&mut current));
                    in_arg = false;
                }
            }
            '%' => match chars.next() {
                Some('%') => {
                    current.push('%');
                    in_arg = true;
                }
                // A field code alone is dropped with its argument, inside one it is dropped.
                Some(_) => {}
                None => {}
            },
            c => {
                current.push(c);
                in_arg = true;
            }
        }
    }
    if in_arg && !current.is_empty() {
        args.push(current);
    }
    args.retain(|a| !a.is_empty());
    (!args.is_empty()).then_some(args)
}

/// The escapes of desktop entry string values.
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => {}
        }
    }
    out
}

/// True when `program` is an absolute path that exists or is found on PATH.
fn on_path(program: &str) -> bool {
    let p = Path::new(program);
    if p.is_absolute() {
        return p.is_file();
    }
    std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_codes_go_and_quotes_hold() {
        assert_eq!(exec_args("gnome-text-editor %U"), Some(vec!["gnome-text-editor".into()]));
        assert_eq!(exec_args("nautilus --new-window %U"), Some(vec!["nautilus".into(), "--new-window".into()]));
        assert_eq!(
            exec_args(r#""/opt/My App/run" --x=%f --name "a b""#),
            Some(vec!["/opt/My App/run".into(), "--x=".into(), "--name".into(), "a b".into()])
        );
        assert_eq!(exec_args("printf 100%%"), Some(vec!["printf".into(), "100%".into()]));
        assert_eq!(exec_args(r#"sh -c "echo \"hi\"""#), Some(vec!["sh".into(), "-c".into(), r#"echo "hi""#.into()]));
        assert_eq!(exec_args("%F"), None);
    }

    #[test]
    fn hidden_and_terminal_entries_are_left_out() {
        let entry = |extra: &str| {
            format!(
                "[Desktop Entry]\nType=Application\nName=Calc\nExec=gnome-calculator\n{extra}\n[Desktop Action new]\nName=Other\nExec=other\n"
            )
        };
        let app = parse("calc.desktop", &entry("Comment=Do sums")).unwrap();
        assert_eq!(
            (app.name.as_str(), app.exec.as_slice(), app.hints.as_str()),
            ("Calc", &["gnome-calculator".to_string()][..], "Do sums")
        );
        for hidden in ["NoDisplay=true", "Hidden=true", "Terminal=true", "OnlyShowIn=KDE;", "TryExec=/nonexistent/meerora-test"] {
            assert!(parse("calc.desktop", &entry(hidden)).is_none(), "{hidden}");
        }
        assert!(parse("x.desktop", "[Desktop Entry]\nType=Link\nName=Web\nURL=https://example.org\n").is_none());
        assert!(parse("x.desktop", "[Desktop Entry]\nType=Application\nName=Nothing\n").is_none());
    }

    #[test]
    fn the_users_folder_wins_and_recent_apps_come_first() {
        let root = std::env::temp_dir().join(format!("meerora-apps-{}", std::process::id()));
        let (user, system) = (root.join("user"), root.join("system"));
        for dir in [&user, &system.join("vendor")] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let write = |dir: &Path, file: &str, body: &str| {
            std::fs::write(dir.join(file), format!("[Desktop Entry]\nType=Application\n{body}\n")).unwrap()
        };
        write(&user, "editor.desktop", "Name=Hidden Editor\nExec=editor\nNoDisplay=true");
        write(&system, "editor.desktop", "Name=Editor\nExec=editor");
        write(&system, "zeta.desktop", "Name=Zeta\nExec=zeta");
        write(&system.join("vendor"), "alpha.desktop", "Name=Alpha\nExec=alpha");
        let apps = scan(&[user, system]);
        let ids: Vec<&str> = apps.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["vendor-alpha.desktop", "zeta.desktop"], "the user's hidden entry hides the system one");

        let mut list = AppList { apps, recent: HashMap::new(), scanned: Instant::now(), data_dir: root.clone() };
        assert_eq!(list.matching("")[0].name, "Alpha");
        list.used("zeta.desktop");
        assert_eq!(list.matching("")[0].name, "Zeta", "the most recently used app comes first");
        assert_eq!(list.matching("alp").len(), 1);
        let saved: HashMap<String, u64> = serde_json::from_str(&std::fs::read_to_string(root.join(RECENT_FILE)).unwrap()).unwrap();
        assert!(saved.contains_key("zeta.desktop"));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
