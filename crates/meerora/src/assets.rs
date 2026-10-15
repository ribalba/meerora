//! Loads the space and figure scripts from the data dir (D21) and watches
//! them, so saving a script rebuilds that object while the app runs. The
//! space is the room `meerora.toml` names (D6, `meerora_protocol::space_dir`),
//! and a change of `meerora.toml` rebuilds it, so a room switch shows at once.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use meerora_protocol::AgentDir;
use meerora_script::{FigureDesc, Limits, SpaceDesc};
use notify::{RecursiveMode, Watcher};

/// What needs rebuilding after files changed.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Change {
    Space,
    /// Figure script or agent.toml of this agent.
    Agent(String),
}

pub struct Assets {
    pub data_dir: PathBuf,
    _watcher: Option<notify::RecommendedWatcher>,
    rx: Receiver<PathBuf>,
    pending: HashMap<Change, Instant>,
}

/// Editors save in several steps. Waiting this long avoids building half-written files.
const SETTLE: Duration = Duration::from_millis(150);

impl Assets {
    pub fn new(data_dir: PathBuf, wake: impl Fn() + Send + 'static) -> Self {
        let (tx, rx) = channel();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res
                && !event.kind.is_access()
            {
                for p in event.paths {
                    let _ = tx.send(p);
                }
                wake();
            }
        })
        .and_then(|mut w| w.watch(&data_dir, RecursiveMode::Recursive).map(|()| w));
        let watcher = match watcher {
            Ok(w) => Some(w),
            Err(e) => {
                tracing::warn!("hot reload disabled, watching {} failed: {e}", data_dir.display());
                None
            }
        };
        Self { data_dir, _watcher: watcher, rx, pending: HashMap::new() }
    }

    /// Without a watcher, for snapshots.
    pub fn without_watcher(data_dir: PathBuf) -> Self {
        let (_tx, rx) = channel();
        Self { data_dir, _watcher: None, rx, pending: HashMap::new() }
    }

    /// `space.js` of the room the data dir uses.
    pub fn space_path(&self) -> PathBuf {
        meerora_protocol::space_dir(&self.data_dir).join("space.js")
    }

    /// Rebuilds the space, its devices and the graveyard plot on the next
    /// poll, as if a file had changed. The room picker uses it, which also
    /// works without a file watcher.
    pub fn reload_space(&mut self) {
        let now = Instant::now();
        self.pending.insert(Change::Space, now.checked_sub(SETTLE).unwrap_or(now));
    }

    /// Changes whose files have settled.
    pub fn poll(&mut self) -> Vec<Change> {
        while let Ok(path) = self.rx.try_recv() {
            if let Some(change) = self.classify(&path) {
                self.pending.insert(change, Instant::now());
            }
        }
        let now = Instant::now();
        let ready: Vec<Change> = self.pending.iter().filter(|(_, t)| now.duration_since(**t) >= SETTLE).map(|(c, _)| c.clone()).collect();
        for c in &ready {
            self.pending.remove(c);
        }
        ready
    }

    fn classify(&self, path: &Path) -> Option<Change> {
        let rel = path.strip_prefix(&self.data_dir).ok()?;
        if rel == Path::new("meerora.toml") {
            // It may name another room.
            return Some(Change::Space);
        }
        let mut parts = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned());
        match parts.next()?.as_str() {
            "space" | "spaces" if path.extension().is_some_and(|e| e == "js") => {
                path.starts_with(meerora_protocol::space_dir(&self.data_dir)).then_some(Change::Space)
            }
            "agents" => {
                let id = parts.next()?;
                let file = parts.next()?;
                (file.ends_with(".js") || file == "agent.toml").then_some(Change::Agent(id))
            }
            _ => None,
        }
    }

    /// Builds `space.js` of the room. A room that `meerora.toml` names and the
    /// data dir lacks is copied from its template first.
    pub fn build_space(&self) -> Result<SpaceDesc, String> {
        meerora_protocol::ensure_chosen_room(&self.data_dir)?;
        let path = self.space_path();
        let source = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        meerora_script::build_space(&source, &display_name(&path, &self.data_dir), &Limits::default()).map_err(|e| e.to_string())
    }

    pub fn build_figure(&self, dir: &AgentDir) -> Result<FigureDesc, String> {
        let path = dir.figure_path();
        let source = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        meerora_script::build_figure(&source, &display_name(&path, &self.data_dir), &Limits::default()).map_err(|e| e.to_string())
    }
}

fn display_name(path: &Path, base: &Path) -> String {
    path.strip_prefix(base).unwrap_or(path).display().to_string()
}
