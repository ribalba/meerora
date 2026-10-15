//! Manifests on disk: one JSON file per manifest under `<data>/manifests/`,
//! plus entries that live only in memory (the demo).
//!
//! A person's manifest only counts when the person's account published it,
//! which is when it is verified (D26). Unverified person manifests from
//! before `POST /v1/manifests` refused people are not loaded.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use meerora_protocol::{IndexEntry, Manifest, ManifestKind};
use serde::{Deserialize, Serialize};
use tracing::warn;

/// A stored manifest and what the index found out about it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub manifest: Manifest,
    pub verified: bool,
    /// Why it is verified or not.
    #[serde(default)]
    pub note: String,
    /// Unix milliseconds of the last registration.
    #[serde(default)]
    pub updated: u64,
    /// Lower-case hex SHA-256 of the manifest as it was posted, which the
    /// owner's well-known file lists (see [`crate::verify`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// SHA-256 of the edit secret that the first unverified registration got.
    /// Later unverified updates must send the secret. Verified records have
    /// none, their domain proves who may change them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit_secret_sha256: Option<String>,
}

impl Record {
    /// A record without the hash of the posted manifest and without an edit secret.
    pub fn new(manifest: Manifest, verified: bool, note: impl Into<String>) -> Self {
        Self { manifest, verified, note: note.into(), updated: crate::now_ms(), sha256: None, edit_secret_sha256: None }
    }

    /// The search result, without the figure script (the client fetches it).
    pub fn entry(&self) -> IndexEntry {
        IndexEntry { manifest: self.manifest.clone(), verified: self.verified, figure_script: None, profile: None }
    }
}

pub struct Store {
    dir: Option<PathBuf>,
    records: RwLock<BTreeMap<String, Record>>,
}

impl Store {
    /// Keeps nothing on disk.
    pub fn memory() -> Self {
        Self { dir: None, records: RwLock::new(BTreeMap::new()) }
    }

    /// Loads every manifest below `dir/manifests`. Broken files are skipped with a warning.
    pub fn open(dir: &Path) -> io::Result<Self> {
        let folder = dir.join("manifests");
        std::fs::create_dir_all(&folder)?;
        let mut records = BTreeMap::new();
        for entry in std::fs::read_dir(&folder)?.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            match std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|t| serde_json::from_str::<Record>(&t).map_err(|e| e.to_string()))
            {
                Ok(record) if record.manifest.kind == ManifestKind::Person && !record.verified => {
                    warn!("skipping {}, a person's manifest that no account published", path.display())
                }
                Ok(record) => {
                    records.insert(record.manifest.id.clone(), record);
                }
                Err(e) => warn!("skipping {} ({e})", path.display()),
            }
        }
        Ok(Self { dir: Some(dir.to_path_buf()), records: RwLock::new(records) })
    }

    pub fn len(&self) -> usize {
        self.records.read().map_or(0, |r| r.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, id: &str) -> Option<Record> {
        self.records.read().ok()?.get(id).cloned()
    }

    /// Stores the record, on disk too when `persist` is set and the store has
    /// a directory. Returns true when the id is new.
    pub fn put(&self, record: Record, persist: bool) -> io::Result<bool> {
        let mut records = self.records.write().map_err(|_| io::Error::other("the store lock is poisoned"))?;
        self.write_file(&record, persist)?;
        Ok(records.insert(record.manifest.id.clone(), record).is_none())
    }

    /// Decides and stores in one step, so two uploads of the same id cannot
    /// both see it as new. `decide` gets the current record of `id` and
    /// returns the record to store, or refuses. Returns whether the id is new.
    pub fn upsert<E>(
        &self,
        id: &str,
        persist: bool,
        decide: impl FnOnce(Option<&Record>) -> Result<Record, E>,
        io_error: impl FnOnce(io::Error) -> E,
    ) -> Result<(bool, Record), E> {
        let mut records = match self.records.write() {
            Ok(r) => r,
            Err(_) => return Err(io_error(io::Error::other("the store lock is poisoned"))),
        };
        let record = decide(records.get(id))?;
        if let Err(e) = self.write_file(&record, persist) {
            return Err(io_error(e));
        }
        let created = records.insert(record.manifest.id.clone(), record.clone()).is_none();
        Ok((created, record))
    }

    fn write_file(&self, record: &Record, persist: bool) -> io::Result<()> {
        let Some(dir) = self.dir.as_ref().filter(|_| persist) else { return Ok(()) };
        let path = dir.join("manifests").join(file_name(&record.manifest.id));
        let text = serde_json::to_string_pretty(record).map_err(io::Error::other)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)
    }

    /// Ranked results for `query` (see [`crate::search`]).
    pub fn search(&self, query: &str) -> Vec<Record> {
        let Ok(records) = self.records.read() else { return Vec::new() };
        crate::search::rank(records.values(), query, |r| (&r.manifest, r.verified)).into_iter().cloned().collect()
    }
}

/// A file name for an id. Ids only hold `a-z0-9.-_:` and one `/`, so the
/// mapping cannot collide.
fn file_name(id: &str) -> String {
    format!("{}.json", id.replace('/', "~").replace(':', "+"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::tests::manifest;

    #[test]
    fn records_survive_a_restart() {
        let dir = std::env::temp_dir().join(format!("meerora-index-store-{}-{}", std::process::id(), crate::now_ms()));
        let store = Store::open(&dir).unwrap();
        let record = Record { updated: 1, ..Record::new(manifest(), true, "listed") };
        assert!(store.put(record.clone(), true).unwrap());
        assert!(!store.put(record.clone(), true).unwrap(), "the second put updates");
        let mut demo = record.clone();
        demo.manifest.id = "demo.example/x".into();
        store.put(demo, false).unwrap();
        // A person's manifest that no account published (posted before people were refused).
        let mut squatted = Record::new(manifest(), false, "");
        squatted.manifest.kind = ManifestKind::Person;
        squatted.manifest.id = "robin/robin".into();
        store.put(squatted, true).unwrap();
        assert_eq!(store.len(), 3);
        assert!(dir.join("manifests/meridian.example~mira.json").is_file());

        let again = Store::open(&dir).unwrap();
        assert_eq!(again.len(), 1, "entries that are not persisted are gone, and so is the squatted person");
        assert_eq!(again.get("meridian.example/mira"), Some(record));
        assert_eq!(again.search("mira").len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
