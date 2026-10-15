//! Rooms: which space script the data dir uses (D6).
//!
//! meerora ships three rooms as templates in `assets/spaces/<room>/`: the
//! workshop with its garden, a space station and an office. Each is a
//! complete space script with the same station ids, so every agent's home
//! and every tool station works in every room, plus its own devices.
//!
//! `[space] room` in `meerora.toml` picks the room. A room lives in
//! `spaces/<room>/` of the data dir with its `space.js` and `devices/`, and
//! the grants and settings of its devices stay beside them. Choosing a room
//! that the data dir does not have yet copies its template there, and from
//! then on the copy belongs to the user. Switching rooms only changes the
//! setting, so it never touches the files of another room.
//!
//! The room `custom` is the `space/` folder of data dirs from before rooms.
//! Without the setting a data dir that has `space/` keeps using it, and one
//! without it uses the workshop. meerora never moves, overwrites or deletes
//! `space/`, it stays the room `custom` for as long as it exists.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Settings;

/// The room of the `space/` folder from before rooms.
pub const CUSTOM_ROOM: &str = "custom";
/// The room of a data dir that has neither the setting nor `space/`.
pub const DEFAULT_ROOM: &str = "workshop";
/// Files of a room that belong to one user and never travel with a template.
const PRIVATE_FILES: [&str; 2] = ["grants.toml", "settings.toml"];

/// `[space]` of `meerora.toml`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpaceSettings {
    /// `workshop`, `space-station`, `office`, `custom` or the name of another
    /// folder in `spaces/`. Unset means `space/` when it exists, else the workshop.
    #[serde(default)]
    pub room: Option<String>,
}

/// A room that ships with meerora.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Template {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
}

/// The rooms in `assets/spaces/`, in the order the picker lists them.
pub const TEMPLATES: [Template; 3] = [
    Template {
        id: "workshop",
        name: "Workshop and garden",
        description: "A cutaway workshop that opens onto a walled garden with a tree and a bench.",
    },
    Template {
        id: "space-station",
        name: "Space station",
        description: "A ring module with an airlock, a viewport, server racks and a hydroponics bay.",
    },
    Template { id: "office", name: "Office", description: "An open office with a reception, a sofa corner and a quiet memorial corner." },
];

/// One entry of the room picker.
#[derive(Clone, Debug, PartialEq)]
pub struct Room {
    pub id: String,
    pub name: String,
    pub description: String,
    /// Where its `space.js` is, or will be once it is chosen.
    pub path: PathBuf,
    /// The data dir has its own copy already.
    pub present: bool,
    /// The room the data dir uses.
    pub current: bool,
    /// For `custom`: the template whose scripts it still equals, if any.
    pub same_as: Option<String>,
}

/// The shipped rooms, `spaces/` of the assets: the installed ones or
/// `assets/spaces/` of the repository (see [`crate::assets_dir`]).
pub fn templates_dir() -> PathBuf {
    crate::assets_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets")).join("spaces")
}

/// Whether `room` can name a room: lowercase letters, digits, `-` and `_`.
pub fn room_name_ok(room: &str) -> bool {
    room.len() <= 40
        && room.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && room.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// The room the data dir uses. A broken `meerora.toml` counts as unset.
pub fn current_room(data_dir: &Path) -> String {
    let set = Settings::load(data_dir).ok().and_then(|s| s.space.room).filter(|r| room_name_ok(r));
    match set {
        Some(room) => room,
        None if data_dir.join("space").is_dir() => CUSTOM_ROOM.into(),
        None => DEFAULT_ROOM.into(),
    }
}

/// The folder of `room` in the data dir.
pub fn room_dir(data_dir: &Path, room: &str) -> PathBuf {
    if room == CUSTOM_ROOM { data_dir.join("space") } else { data_dir.join("spaces").join(room) }
}

/// The folder of the room the data dir uses, with `space.js` and `devices/`.
pub fn space_dir(data_dir: &Path) -> PathBuf {
    room_dir(data_dir, &current_room(data_dir))
}

/// Copies the template of `room` into the data dir unless the data dir has
/// the room already. Returns the room's folder. It never overwrites a file:
/// the copy goes to a temporary folder first and takes the room's name in
/// one rename, so two processes that ensure the same room at once leave one
/// complete copy.
pub fn ensure_room(data_dir: &Path, room: &str) -> Result<PathBuf, String> {
    if !room_name_ok(room) {
        return Err(format!("{room:?} is not a room name. Use lowercase letters, digits, - and _."));
    }
    let dir = room_dir(data_dir, room);
    if dir.join("space.js").is_file() || room == CUSTOM_ROOM {
        return Ok(dir);
    }
    let template = templates_dir().join(room);
    if !template.join("space.js").is_file() {
        return Err(format!("there is no room {room:?}: neither {} nor {} has a space.js", dir.display(), template.display()));
    }
    if dir.exists() {
        return Err(format!("{} exists without a space.js, so meerora leaves it alone", dir.display()));
    }
    let parent = data_dir.join("spaces");
    std::fs::create_dir_all(&parent).map_err(|e| format!("cannot create {} ({e})", parent.display()))?;
    let tmp = parent.join(format!(".{room}.{}.tmp", std::process::id()));
    let copied = copy_room(&template, &tmp).and_then(|()| std::fs::rename(&tmp, &dir).map_err(|e| e.to_string()));
    if let Err(e) = copied {
        // Only the temporary copy this call made is removed.
        let _ = std::fs::remove_dir_all(&tmp);
        if dir.join("space.js").is_file() {
            return Ok(dir);
        }
        return Err(format!("cannot copy the room {room} to {} ({e})", dir.display()));
    }
    Ok(dir)
}

fn copy_room(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| format!("cannot create {} ({e})", to.display()))?;
    let entries = std::fs::read_dir(from).map_err(|e| format!("cannot read {} ({e})", from.display()))?;
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name();
        if PRIVATE_FILES.iter().any(|p| name == *p) || name.to_string_lossy().starts_with('.') {
            continue;
        }
        let (source, dest) = (entry.path(), to.join(&name));
        if source.is_dir() {
            copy_room(&source, &dest)?;
        } else {
            std::fs::copy(&source, &dest).map_err(|e| format!("cannot copy {} ({e})", source.display()))?;
        }
    }
    Ok(())
}

/// [`ensure_room`] for the room `meerora.toml` names, so a room set by hand
/// arrives too. Without the setting nothing is copied, because the data dir
/// then uses `space/` or the workshop it already has.
pub fn ensure_chosen_room(data_dir: &Path) -> Result<(), String> {
    match Settings::load(data_dir).ok().and_then(|s| s.space.room) {
        Some(room) => ensure_room(data_dir, &room).map(|_| ()),
        None => Ok(()),
    }
}

/// Chooses `room`: copies its template when the data dir lacks it, then
/// writes `[space] room` into `meerora.toml`. Everything else in the file
/// stays as it is, comments included. Nothing of the other rooms changes.
pub fn set_room(data_dir: &Path, room: &str) -> Result<PathBuf, String> {
    let dir = ensure_room(data_dir, room)?;
    if room == CUSTOM_ROOM && !dir.join("space.js").is_file() {
        return Err(format!("there is no custom room, because {} has no space.js", dir.display()));
    }
    let path = data_dir.join("meerora.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("cannot read {} ({e})", path.display())),
    };
    let new = with_room(&text, room);
    toml::from_str::<Settings>(&new).map_err(|e| format!("{} would not parse after the change ({e})", path.display()))?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, new).and_then(|()| std::fs::rename(&tmp, &path)).map_err(|e| format!("cannot write {} ({e})", path.display()))?;
    Ok(dir)
}

/// `text` with `room = "<room>"` in its `[space]` table, which is added when missing.
fn with_room(text: &str, room: &str) -> String {
    let line = format!("room = \"{room}\"");
    let mut out: Vec<String> = Vec::new();
    let (mut in_space, mut done) = (false, false);
    for l in text.lines() {
        let trimmed = l.trim();
        if trimmed.starts_with('[') {
            if in_space && !done {
                // The table ends without a room line.
                out.push(line.clone());
                done = true;
            }
            in_space = trimmed.trim_start_matches('[').trim_end_matches(']').trim() == "space";
        } else if in_space && !done && trimmed.split('=').next().is_some_and(|k| k.trim() == "room") && trimmed.contains('=') {
            out.push(line.clone());
            done = true;
            continue;
        }
        out.push(l.to_string());
    }
    if in_space && !done {
        out.push(line.clone());
        done = true;
    }
    if !done {
        if out.last().is_some_and(|l| !l.trim().is_empty()) {
            out.push(String::new());
        }
        out.push("# The room of the space (D6): workshop, space-station, office or custom.".into());
        out.push("[space]".into());
        out.push(line);
    }
    let mut new = out.join("\n");
    new.push('\n');
    new
}

/// The rooms for the picker: the templates, other rooms in `spaces/` and
/// `custom` when `space/` exists.
pub fn rooms(data_dir: &Path) -> Vec<Room> {
    let current = current_room(data_dir);
    let mut list: Vec<Room> = TEMPLATES
        .iter()
        .map(|t| {
            let path = room_dir(data_dir, t.id);
            Room {
                id: t.id.into(),
                name: t.name.into(),
                description: t.description.into(),
                present: path.join("space.js").is_file(),
                path,
                current: current == t.id,
                same_as: None,
            }
        })
        .collect();
    let mut others: Vec<String> = std::fs::read_dir(data_dir.join("spaces"))
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().to_str().map(String::from))
                .filter(|n| room_name_ok(n) && n != CUSTOM_ROOM && !TEMPLATES.iter().any(|t| t.id == n))
                .filter(|n| room_dir(data_dir, n).join("space.js").is_file())
                .collect()
        })
        .unwrap_or_default();
    others.sort();
    for id in others {
        list.push(Room {
            name: id.replace(['-', '_'], " "),
            description: format!("Your room in spaces/{id}/."),
            path: room_dir(data_dir, &id),
            present: true,
            current: current == id,
            same_as: None,
            id,
        });
    }
    let custom = room_dir(data_dir, CUSTOM_ROOM);
    if custom.join("space.js").is_file() || current == CUSTOM_ROOM {
        let same_as = TEMPLATES.iter().find(|t| same_scripts(&custom, &templates_dir().join(t.id))).map(|t| t.id.to_string());
        list.push(Room {
            id: CUSTOM_ROOM.into(),
            name: "Custom".into(),
            description: "Your own space/ folder from before rooms, kept as it is.".into(),
            present: custom.join("space.js").is_file(),
            path: custom,
            current: current == CUSTOM_ROOM,
            same_as,
        });
    }
    list
}

/// Whether two room folders hold the same scripts, byte for byte. Grants,
/// settings and other files do not count.
pub fn same_scripts(a: &Path, b: &Path) -> bool {
    let scripts = |dir: &Path| -> Option<Vec<(String, Vec<u8>)>> {
        let mut out = Vec::new();
        let mut stack = vec![(dir.to_path_buf(), String::new())];
        while let Some((d, prefix)) = stack.pop() {
            for entry in std::fs::read_dir(&d).ok()?.filter_map(|e| e.ok()) {
                let name = entry.file_name().to_string_lossy().to_string();
                let path = entry.path();
                if path.is_dir() {
                    stack.push((path, format!("{prefix}{name}/")));
                } else if name.ends_with(".js") {
                    out.push((format!("{prefix}{name}"), std::fs::read(&path).ok()?));
                }
            }
        }
        out.sort();
        Some(out)
    };
    match (scripts(a), scripts(b)) {
        (Some(x), Some(y)) => !x.is_empty() && x == y,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("meerora-rooms-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Dir(path)
        }

        fn write(&self, rel: &str, text: &str) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_room_follows_the_setting_then_space_then_the_workshop() {
        let dir = Dir::new("resolve");
        assert_eq!(current_room(&dir.0), "workshop");
        assert_eq!(space_dir(&dir.0), dir.0.join("spaces/workshop"));
        dir.write("space/space.js", "// mine");
        assert_eq!(current_room(&dir.0), "custom", "a data dir from before rooms keeps its space/");
        assert_eq!(space_dir(&dir.0), dir.0.join("space"));
        dir.write("meerora.toml", "[space]\nroom = \"office\"\n");
        assert_eq!(space_dir(&dir.0), dir.0.join("spaces/office"));
        dir.write("meerora.toml", "[space]\nroom = \"../etc\"\n");
        assert_eq!(current_room(&dir.0), "custom", "a bad name counts as unset");
    }

    #[test]
    fn the_setting_is_written_into_meerora_toml_keeping_the_rest() {
        assert_eq!(
            with_room("", "office"),
            "# The room of the space (D6): workshop, space-station, office or custom.\n[space]\nroom = \"office\"\n"
        );
        let text = "# settings\n[index]\nurl = \"http://x\"\n";
        let new = with_room(text, "space-station");
        assert!(new.starts_with(text) && new.ends_with("[space]\nroom = \"space-station\"\n"), "{new}");
        let again = with_room(&new, "workshop");
        assert_eq!(again.matches("room =").count(), 1, "{again}");
        assert!(again.contains("room = \"workshop\"") && again.contains("url = \"http://x\""), "{again}");
        let middle = "[space]\n# a note\n[index]\nurl = \"http://x\"\n";
        assert_eq!(with_room(middle, "office"), "[space]\n# a note\nroom = \"office\"\n[index]\nurl = \"http://x\"\n");
        // A commented example of the init file is not the setting.
        let commented = "# [space]\n# room = \"workshop\"\n";
        assert!(with_room(commented, "office").ends_with("[space]\nroom = \"office\"\n"));
    }

    #[test]
    fn switching_rooms_copies_the_template_once_and_keeps_a_custom_space() {
        let dir = Dir::new("switch");
        // A space/ from before rooms with the user's own edits.
        let mine = "// my own room\nexport default space({ parts: [quad(100, 100)] });\n";
        dir.write("space/space.js", mine);
        dir.write("space/devices/lamp.js", "// my lamp");
        dir.write("space/devices/grants.toml", "[lamp]\ngranted = []\n");
        dir.write("meerora.toml", "# my settings\n[index]\nurl = \"http://127.0.0.1:7878\"\n");
        let before = rooms(&dir.0);
        let custom = before.iter().find(|r| r.id == "custom").expect("space/ is the room custom");
        assert!(custom.current && custom.present && custom.same_as.is_none(), "{custom:?}");

        let office = set_room(&dir.0, "office").unwrap();
        assert_eq!(office, dir.0.join("spaces/office"));
        assert!(office.join("space.js").is_file() && office.join("devices").is_dir(), "the template was copied");
        assert_eq!(space_dir(&dir.0), office);
        assert_eq!(std::fs::read_to_string(dir.0.join("space/space.js")).unwrap(), mine, "space/ is untouched");
        assert!(dir.0.join("space/devices/lamp.js").is_file() && dir.0.join("space/devices/grants.toml").is_file());
        let settings = std::fs::read_to_string(dir.0.join("meerora.toml")).unwrap();
        assert!(settings.starts_with("# my settings\n[index]\n") && settings.contains("room = \"office\""), "{settings}");

        // The user edits the office. Switching away and back keeps the edit.
        let edited = format!("{}\n// my desk moved\n", std::fs::read_to_string(office.join("space.js")).unwrap());
        std::fs::write(office.join("space.js"), &edited).unwrap();
        set_room(&dir.0, "workshop").unwrap();
        set_room(&dir.0, "office").unwrap();
        assert_eq!(std::fs::read_to_string(office.join("space.js")).unwrap(), edited, "a room is copied once");

        // Custom is still listed and can be chosen again.
        let list = rooms(&dir.0);
        let ids: Vec<&str> = list.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["workshop", "space-station", "office", "custom"]);
        assert!(list.iter().find(|r| r.id == "office").unwrap().current);
        set_room(&dir.0, "custom").unwrap();
        assert_eq!(space_dir(&dir.0), dir.0.join("space"));
        assert_eq!(std::fs::read_to_string(dir.0.join("space/space.js")).unwrap(), mine);

        assert!(set_room(&dir.0, "castle").unwrap_err().contains("no room \"castle\""));
        // A room named by hand arrives on the next build, nothing arrives without the setting.
        std::fs::write(dir.0.join("meerora.toml"), "[space]\nroom = \"space-station\"\n").unwrap();
        ensure_chosen_room(&dir.0).unwrap();
        assert!(dir.0.join("spaces/space-station/space.js").is_file());
        std::fs::write(dir.0.join("meerora.toml"), "[space]\nroom = \"custom\"\n").unwrap();
        assert!(set_room(&dir.0, "Not A Room").is_err());
        assert_eq!(current_room(&dir.0), "custom", "a refused switch changes nothing");
    }

    #[test]
    fn a_custom_space_that_equals_a_template_says_so() {
        let dir = Dir::new("same");
        copy_room(&templates_dir().join("office"), &dir.0.join("space")).unwrap();
        let list = rooms(&dir.0);
        assert_eq!(list.iter().find(|r| r.id == "custom").unwrap().same_as.as_deref(), Some("office"));
        assert!(same_scripts(&templates_dir().join("office"), &templates_dir().join("office")));
        assert!(!same_scripts(&templates_dir().join("office"), &templates_dir().join("workshop")));
    }

    #[test]
    fn private_files_of_a_template_stay_behind() {
        let dir = Dir::new("private");
        let template = dir.0.join("template");
        std::fs::create_dir_all(template.join("devices")).unwrap();
        std::fs::write(template.join("space.js"), "x").unwrap();
        std::fs::write(template.join("devices/lamp.js"), "y").unwrap();
        std::fs::write(template.join("devices/grants.toml"), "[lamp]\ngranted = [\"network.airplane_mode.write\"]\n").unwrap();
        std::fs::write(template.join("devices/settings.toml"), "[graph]\nmetric = \"cpu\"\n").unwrap();
        copy_room(&template, &dir.0.join("copy")).unwrap();
        assert!(dir.0.join("copy/devices/lamp.js").is_file());
        assert!(!dir.0.join("copy/devices/grants.toml").exists(), "a grant of the developer never reaches a user");
        assert!(!dir.0.join("copy/devices/settings.toml").exists());
    }

    #[test]
    fn every_template_has_a_space_and_devices() {
        for t in TEMPLATES {
            let dir = templates_dir().join(t.id);
            assert!(dir.join("space.js").is_file(), "{}", t.id);
            assert!(dir.join("devices").is_dir(), "{}", t.id);
            assert!(room_name_ok(t.id));
        }
    }
}
