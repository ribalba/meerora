//! The agent's files for the editor tile: the tree, loading with binary and
//! size checks, and saving.

use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

/// Files above this size open read-only.
pub const MAX_EDIT_BYTES: u64 = 1024 * 1024;
/// Lines shown of a file that is too large to edit.
pub const MAX_SHOW_LINES: usize = 5000;
/// Entries the tree reads at most, so a huge directory cannot stall it.
pub const MAX_ENTRIES: usize = 3000;
const MAX_DEPTH: usize = 8;

/// One row of the tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Relative to the agent directory.
    pub rel: PathBuf,
    pub depth: usize,
    pub dir: bool,
}

impl Entry {
    pub fn name(&self) -> String {
        self.rel.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }
}

/// Whether the tree leaves `name` out. `.git` never shows, `logs` only at
/// the top, where the daemon keeps the run logs (D21).
pub fn hidden(name: &str, depth: usize) -> bool {
    name == ".git" || (depth == 0 && name == "logs")
}

/// The tree of `dir` in display order: directories first, then files, each
/// sorted by name, children right after their directory.
pub fn tree(dir: &Path) -> Vec<Entry> {
    let mut out = Vec::new();
    walk(dir, Path::new(""), 0, &mut out);
    out
}

fn walk(root: &Path, rel: &Path, depth: usize, out: &mut Vec<Entry>) {
    if depth > MAX_DEPTH || out.len() >= MAX_ENTRIES {
        return;
    }
    let Ok(read) = std::fs::read_dir(root.join(rel)) else { return };
    let mut entries: Vec<(String, bool)> = read
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let kind = e.file_type().ok()?;
            // Symbolic links count by what they point at, but are never followed into.
            let dir = kind.is_dir();
            let file = kind.is_file() || (kind.is_symlink() && root.join(rel).join(&name).is_file());
            (!hidden(&name, depth) && (dir || file)).then_some((name, dir))
        })
        .collect();
    entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())));
    for (name, dir) in entries {
        if out.len() >= MAX_ENTRIES {
            return;
        }
        let path = rel.join(&name);
        out.push(Entry { rel: path.clone(), depth, dir });
        if dir {
            walk(root, &path, depth + 1, out);
        }
    }
}

/// `path` relative to `dir` when it names a file inside it. Accepts
/// relative and absolute paths and refuses `..`.
pub fn inside(dir: &Path, path: &str) -> Option<PathBuf> {
    let p = Path::new(path.trim());
    let rel = if p.is_absolute() {
        let canonical = dir.canonicalize().ok();
        std::iter::once(dir).chain(canonical.as_deref()).find_map(|base| p.strip_prefix(base).ok())?.to_path_buf()
    } else {
        p.to_path_buf()
    };
    let clean = rel.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    let rel: PathBuf = rel.components().filter(|c| matches!(c, Component::Normal(_))).collect();
    (clean && !rel.as_os_str().is_empty() && dir.join(&rel).is_file()).then_some(rel)
}

/// git's test: a NUL byte in the first 8000 bytes makes a file binary.
pub fn is_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(8000)].contains(&0)
}

/// A file as the editor opens it.
#[derive(Clone, Debug, PartialEq)]
pub enum Loaded {
    /// Text with `\n` line breaks. `read_only` holds the note why it cannot be edited.
    Text {
        text: String,
        crlf: bool,
        read_only: Option<String>,
    },
    /// Not shown, with the note why.
    Binary(String),
    Error(String),
}

pub fn load(path: &Path) -> Loaded {
    let size = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(e) => return Loaded::Error(format!("Cannot read {} ({e}).", path.display())),
    };
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => return Loaded::Error(format!("Cannot read {} ({e}).", path.display())),
    };
    if is_binary(&bytes) {
        return Loaded::Binary(format!("This is a binary file of {}. The editor shows text files only.", human_size(size)));
    }
    let big = size > MAX_EDIT_BYTES;
    let text = match String::from_utf8(bytes) {
        Ok(t) => t,
        Err(e) if big => {
            // A cut in the middle of a character is fine for a read-only preview.
            let valid = e.utf8_error().valid_up_to();
            let mut bytes = e.into_bytes();
            bytes.truncate(valid);
            String::from_utf8(bytes).unwrap_or_default()
        }
        Err(_) => return Loaded::Binary(format!("This file of {} is not UTF-8 text. The editor shows UTF-8 only.", human_size(size))),
    };
    let crlf = text.contains("\r\n");
    let mut text = if crlf { text.replace("\r\n", "\n") } else { text };
    let read_only = big.then(|| {
        let shown: usize = text.split_inclusive('\n').take(MAX_SHOW_LINES).map(str::len).sum();
        let cut = shown < text.len();
        text.truncate(shown);
        if cut {
            format!("This file is {}. It opens read-only and shows the first {MAX_SHOW_LINES} lines.", human_size(size))
        } else {
            format!("This file is {}. It opens read-only.", human_size(size))
        }
    });
    Loaded::Text { text, crlf, read_only }
}

/// Writes `text` back, with the line breaks the file had.
pub fn save(path: &Path, text: &str, crlf: bool) -> std::io::Result<()> {
    if crlf { std::fs::write(path, text.replace('\n', "\r\n")) } else { std::fs::write(path, text) }
}

/// Modification time and size, to notice changes on disk.
pub fn stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

pub fn human_size(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} bytes"),
        1024..1_048_576 => format!("{:.0} KB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
    }
}

#[cfg(test)]
mod tests {
    use super::super::review::tests::TempDir;
    use super::*;

    fn names(entries: &[Entry]) -> Vec<String> {
        entries.iter().map(|e| format!("{}{}{}", "  ".repeat(e.depth), e.rel.display(), if e.dir { "/" } else { "" })).collect()
    }

    #[test]
    fn the_tree_hides_git_and_logs() {
        let tmp = TempDir::new("files-tree");
        for dir in [".git/objects", "logs", "memory/deep", "artifacts"] {
            std::fs::create_dir_all(tmp.0.join(dir)).unwrap();
        }
        for file in [".git/HEAD", "logs/run.log", "memory/notes.md", "memory/deep/x.txt", "agent.toml", "B.md", "a.txt", ".gitignore"] {
            std::fs::write(tmp.0.join(file), "x").unwrap();
        }
        std::fs::create_dir_all(tmp.0.join("memory/logs")).unwrap();
        std::fs::write(tmp.0.join("memory/logs/kept.md"), "x").unwrap();
        assert_eq!(
            names(&tree(&tmp.0)),
            [
                "artifacts/",
                "memory/",
                "  memory/deep/",
                "    memory/deep/x.txt",
                "  memory/logs/",
                "    memory/logs/kept.md",
                "  memory/notes.md",
                ".gitignore",
                "a.txt",
                "agent.toml",
                "B.md"
            ]
        );
    }

    #[test]
    fn paths_must_stay_inside() {
        let tmp = TempDir::new("files-inside");
        std::fs::create_dir_all(tmp.0.join("memory")).unwrap();
        std::fs::write(tmp.0.join("memory/notes.md"), "x").unwrap();
        assert_eq!(inside(&tmp.0, "memory/notes.md"), Some(PathBuf::from("memory/notes.md")));
        assert_eq!(inside(&tmp.0, "./memory/notes.md"), Some(PathBuf::from("memory/notes.md")));
        let abs = tmp.0.join("memory/notes.md");
        assert_eq!(inside(&tmp.0, &abs.to_string_lossy()), Some(PathBuf::from("memory/notes.md")));
        assert_eq!(inside(&tmp.0, "../etc/passwd"), None);
        assert_eq!(inside(&tmp.0, "/etc/passwd"), None);
        assert_eq!(inside(&tmp.0, "memory"), None, "a directory");
        assert_eq!(inside(&tmp.0, "missing.md"), None);
    }

    #[test]
    fn binary_big_and_crlf_files() {
        let tmp = TempDir::new("files-load");
        assert!(is_binary(b"PNG\0\x01"));
        assert!(!is_binary("text with ü\n".as_bytes()));

        std::fs::write(tmp.0.join("image.bin"), [0x89, b'P', 0, 1, 2]).unwrap();
        assert!(matches!(load(&tmp.0.join("image.bin")), Loaded::Binary(note) if note.contains("binary")));
        std::fs::write(tmp.0.join("latin1.txt"), [b'a', 0xE9, b'\n']).unwrap();
        assert!(matches!(load(&tmp.0.join("latin1.txt")), Loaded::Binary(note) if note.contains("UTF-8")));

        let big = "line\n".repeat(MAX_EDIT_BYTES as usize / 5 + 10);
        std::fs::write(tmp.0.join("big.log"), &big).unwrap();
        match load(&tmp.0.join("big.log")) {
            Loaded::Text { text, read_only: Some(note), .. } => {
                assert_eq!(text.lines().count(), MAX_SHOW_LINES);
                assert!(note.contains("read-only"), "{note}");
            }
            other => panic!("{other:?}"),
        }

        std::fs::write(tmp.0.join("dos.txt"), "a\r\nb\r\n").unwrap();
        let Loaded::Text { text, crlf: true, read_only: None } = load(&tmp.0.join("dos.txt")) else { panic!() };
        assert_eq!(text, "a\nb\n");
        save(&tmp.0.join("dos.txt"), "a\nb\nc\n", true).unwrap();
        assert_eq!(std::fs::read(tmp.0.join("dos.txt")).unwrap(), b"a\r\nb\r\nc\r\n");
        save(&tmp.0.join("unix.txt"), "x\n", false).unwrap();
        assert_eq!(std::fs::read_to_string(tmp.0.join("unix.txt")).unwrap(), "x\n");
        assert!(matches!(load(&tmp.0.join("nope")), Loaded::Error(_)));
    }
}
