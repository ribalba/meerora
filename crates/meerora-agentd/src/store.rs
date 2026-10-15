//! What a real agent keeps in its directory (D21): `state.json` with the
//! runtime session, the last run, its params, its token use and its schedule,
//! and the conversation in `logs/messages.jsonl`.

use std::collections::BTreeMap;
use std::path::Path;

use meerora_protocol::{Outcome, RUNS_KEPT, RUNS_LOG, RingState, Role, RunRecord, Trigger, safefs};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::warn;

/// Messages replayed per agent when a client says hello.
pub const REPLAY: usize = 30;

/// Contents of `<dir>/state.json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Store {
    /// Runtime session to continue (D22).
    pub session: Option<String>,
    pub ring: Option<RingState>,
    pub activity: String,
    /// Figure params the agent set (D24).
    pub params: BTreeMap<String, Value>,
    pub last_run: Option<LastRun>,
    pub tokens: TokenDay,
    /// Last wake per schedule entry, unix milliseconds (D23).
    pub schedule: BTreeMap<String, i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LastRun {
    pub id: String,
    pub trigger: Trigger,
    pub task: String,
    pub started: u64,
    pub finished: u64,
    pub outcome: Outcome,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub tokens: u64,
}

/// Tokens used on one local day, for `budget.tokens_per_day`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenDay {
    pub day: String,
    pub used: u64,
}

impl Store {
    /// Reads `state.json`. A missing file gives an empty store and a broken
    /// one is logged and replaced on the next save.
    pub fn load(dir: &Path) -> Store {
        let path = dir.join("state.json");
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                warn!("{} is broken and will be rewritten ({e})", path.display());
                Store::default()
            }),
            Err(_) => Store::default(),
        }
    }

    /// Writes `state.json` through a temporary file, so a crash never leaves
    /// half a file, and never through a symlink the agent put in its directory.
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        let mut text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        text.push('\n');
        safefs::replace(dir, "state.json", text.as_bytes())
    }

    pub fn tokens_on(&self, day: &str) -> u64 {
        if self.tokens.day == day { self.tokens.used } else { 0 }
    }

    pub fn add_tokens(&mut self, day: &str, tokens: u64) {
        if self.tokens.day != day {
            self.tokens = TokenDay { day: day.into(), used: 0 };
        }
        self.tokens.used += tokens;
    }
}

/// One finished chat message, as kept in `logs/messages.jsonl`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredMessage {
    pub id: String,
    pub role: Role,
    pub text: String,
    pub at: u64,
}

/// The last `keep` messages of an agent. Broken lines are skipped.
pub fn load_messages(dir: &Path, keep: usize) -> Vec<StoredMessage> {
    let Ok(text) = std::fs::read_to_string(dir.join("logs").join("messages.jsonl")) else {
        return Vec::new();
    };
    let messages: Vec<StoredMessage> = text.lines().filter_map(|line| serde_json::from_str(line).ok()).collect();
    messages[messages.len().saturating_sub(keep)..].to_vec()
}

pub fn append_message(dir: &Path, message: &StoredMessage) -> std::io::Result<()> {
    let mut line = serde_json::to_string(message).map_err(std::io::Error::other)?;
    line.push('\n');
    safefs::append(dir, "logs/messages.jsonl", line.as_bytes())
}

/// Appends a finished run to `logs/runs.jsonl` for the Activity app. The
/// file keeps the last [`RUNS_KEPT`] runs. When it is full, it is rewritten
/// through a temporary file without the oldest ones.
pub fn append_run(dir: &Path, record: &RunRecord) -> std::io::Result<()> {
    let path = dir.join(RUNS_LOG);
    let mut line = serde_json::to_string(record).map_err(std::io::Error::other)?;
    line.push('\n');
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    let count = old.lines().filter(|l| !l.trim().is_empty()).count();
    if count < RUNS_KEPT {
        return safefs::append(dir, RUNS_LOG, line.as_bytes());
    }
    let kept: Vec<&str> = old.lines().filter(|l| !l.trim().is_empty()).collect();
    let mut text = kept[kept.len() + 1 - RUNS_KEPT..].join("\n");
    text.push('\n');
    text.push_str(&line);
    safefs::replace(dir, RUNS_LOG, text.as_bytes())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A directory below the system temp dir, removed again when dropped.
    pub(crate) struct TempDir(pub PathBuf);

    impl TempDir {
        pub(crate) fn new(name: &str) -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!("meerora-agentd-unit-{name}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn state_round_trips_and_tokens_reset_per_day() {
        let dir = TempDir::new("store");
        assert_eq!(Store::load(&dir.0), Store::default());
        let mut store = Store { session: Some("s1".into()), ..Store::default() };
        store.params.insert("letter".into(), Value::Bool(true));
        store.add_tokens("2026-10-14", 100);
        store.add_tokens("2026-10-14", 50);
        assert_eq!(store.tokens_on("2026-10-14"), 150);
        assert_eq!(store.tokens_on("2026-10-15"), 0);
        store.save(&dir.0).unwrap();
        assert_eq!(Store::load(&dir.0), store);
        store.add_tokens("2026-10-15", 7);
        assert_eq!(store.tokens.used, 7);
        assert!(!dir.0.join("state.json.tmp").exists());
    }

    #[test]
    fn messages_keep_the_tail() {
        let dir = TempDir::new("messages");
        for i in 0..40 {
            let m = StoredMessage { id: format!("m{i}"), role: Role::User, text: format!("hi {i}"), at: i };
            append_message(&dir.0, &m).unwrap();
        }
        let tail = load_messages(&dir.0, REPLAY);
        assert_eq!(tail.len(), 30);
        assert_eq!(tail[0].id, "m10");
        assert_eq!(tail[29].id, "m39");
    }
}
