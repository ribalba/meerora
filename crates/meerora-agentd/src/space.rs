//! The stations of the space (D9), read from `space.js` of the room the data
//! dir uses (D6, see `meerora_protocol::space_dir`).

use std::path::Path;

use meerora_script::{Limits, build_space};

/// Stations of the milestone 1 space, used when `space.js` does not build.
pub const FALLBACK_STATIONS: &[&str] =
    &["door", "window", "desk", "bookshelf", "workbench", "router_cabinet", "front_step", "mailbox", "garden_bench", "tree", "graveyard"];

/// Station ids declared by `space.js` of the room. A room named in
/// `meerora.toml` that the data dir does not have yet is copied from its
/// template first.
pub fn stations(data_dir: &Path) -> Result<Vec<String>, String> {
    if let Err(e) = meerora_protocol::ensure_chosen_room(data_dir) {
        tracing::warn!("{e}");
    }
    let path = meerora_protocol::space_dir(data_dir).join("space.js");
    let source = std::fs::read_to_string(&path).map_err(|e| format!("cannot read {} ({e})", path.display()))?;
    let space = build_space(&source, &path.display().to_string(), &Limits::default())
        .map_err(|e| format!("{} does not build ({e})", path.display()))?;
    if space.stations.is_empty() {
        return Err(format!("{} declares no stations", path.display()));
    }
    Ok(space.stations.into_iter().map(|s| s.id).collect())
}

/// The space's stations, or the fallback list together with the reason.
pub fn stations_or_fallback(data_dir: &Path) -> (Vec<String>, Option<String>) {
    match stations(data_dir) {
        Ok(stations) => (stations, None),
        Err(e) => (FALLBACK_STATIONS.iter().map(|s| s.to_string()).collect(), Some(format!("{e}, so the built-in stations are used"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_room_has_the_known_stations() {
        let mut expected: Vec<String> = FALLBACK_STATIONS.iter().map(|s| s.to_string()).collect();
        expected.sort();
        let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets");
        let mut found = stations(&assets).unwrap();
        found.sort();
        assert_eq!(found, expected, "the developer setup uses the workshop");
        for room in meerora_protocol::TEMPLATES {
            let path = assets.join("spaces").join(room.id).join("space.js");
            let source = std::fs::read_to_string(&path).unwrap();
            let space = build_space(&source, room.id, &Limits::default()).unwrap_or_else(|e| panic!("{}: {e}", room.id));
            let mut found: Vec<String> = space.stations.into_iter().map(|s| s.id).collect();
            found.sort();
            assert_eq!(found, expected, "{} has every station, so homes and tools work in every room", room.id);
        }
    }

    #[test]
    fn a_missing_space_falls_back() {
        let (stations, warning) = stations_or_fallback(Path::new("/nowhere"));
        assert_eq!(stations.len(), FALLBACK_STATIONS.len());
        assert!(warning.unwrap().contains("cannot read"));
    }
}
