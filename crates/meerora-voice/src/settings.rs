//! `[voice]` in `meerora.toml` and the speaker toggles of the agent view.
//!
//! ```toml
//! [voice]
//! enabled = true            # false hides every voice control and downloads nothing
//! key = "F8"                # hold to talk
//! language = "auto"         # what you speak: auto, en, de, ...
//! speak_replies = false     # read agent replies aloud
//!
//! [voice.voices]            # Piper voice per language of the text
//! en = "en_US-lessac-medium"
//! de = "de_DE-thorsten-medium"
//!
//! [voice.agents.wren]       # per agent
//! speak_replies = true
//! voice = "de_DE-thorsten-medium"
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// The push to talk key when `key` is unset. meerora itself uses F2, F3 and F11.
pub const DEFAULT_KEY: &str = "F8";
/// The English voice when `[voice.voices]` names none.
pub const DEFAULT_EN: &str = "en_US-lessac-medium";
/// The German voice when `[voice.voices]` names none.
pub const DEFAULT_DE: &str = "de_DE-thorsten-medium";

/// `[voice]` of `meerora.toml`. Every field has a default.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct VoiceSettings {
    /// Off hides every voice control and never downloads anything.
    pub enabled: bool,
    /// The key held to record, by its egui name, for example `F8`.
    pub key: String,
    /// The language you speak, for whisper. `auto` or a code such as `en`.
    pub language: String,
    /// Piper voice per language code of the text. Languages left out keep
    /// the defaults [`DEFAULT_EN`] and [`DEFAULT_DE`].
    pub voices: BTreeMap<String, String>,
    /// Reads agent replies aloud. Agents and the toggle in the agent view override it.
    pub speak_replies: bool,
    /// Settings for single agents, by agent id.
    pub agents: BTreeMap<String, AgentVoice>,
}

/// `[voice.agents.<id>]`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentVoice {
    /// Overrides the global `speak_replies` for this agent.
    #[serde(default)]
    pub speak_replies: Option<bool>,
    /// A fixed Piper voice for this agent, whatever the language of the text.
    #[serde(default)]
    pub voice: Option<String>,
}

impl Default for VoiceSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            key: DEFAULT_KEY.into(),
            language: "auto".into(),
            voices: BTreeMap::new(),
            speak_replies: false,
            agents: BTreeMap::new(),
        }
    }
}

impl VoiceSettings {
    /// Parses the text of a whole `meerora.toml` and keeps only `[voice]`,
    /// so the other sections never make voice fail.
    pub fn from_toml(text: &str) -> Result<VoiceSettings, String> {
        let table: toml::Table = toml::from_str(text).map_err(|e| e.to_string())?;
        match table.get("voice") {
            Some(value) => value.clone().try_into().map_err(|e: toml::de::Error| format!("[voice]: {e}")),
            None => Ok(VoiceSettings::default()),
        }
    }

    /// Reads `[voice]` of `<data_dir>/meerora.toml`. A missing file gives the defaults.
    pub fn load(data_dir: &Path) -> Result<VoiceSettings, String> {
        let path = data_dir.join("meerora.toml");
        match std::fs::read_to_string(&path) {
            Ok(text) => VoiceSettings::from_toml(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(VoiceSettings::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// The language for whisper, `None` to let it detect the language.
    pub fn spoken_language(&self) -> Option<&str> {
        let l = self.language.trim();
        (!l.is_empty() && !l.eq_ignore_ascii_case("auto")).then_some(l)
    }

    /// The Piper voice for text in `language`. Unknown languages get the
    /// voice of the spoken language, then English.
    pub fn voice_for_language(&self, language: &str) -> String {
        let lookup = |l: &str| {
            self.voices.get(l).cloned().or_else(|| match l {
                "en" => Some(DEFAULT_EN.to_string()),
                "de" => Some(DEFAULT_DE.to_string()),
                _ => None,
            })
        };
        lookup(language).or_else(|| self.spoken_language().and_then(lookup)).unwrap_or_else(|| lookup("en").unwrap_or(DEFAULT_EN.into()))
    }

    /// Every Piper voice these settings can pick, for the download.
    pub fn all_voices(&self) -> Vec<String> {
        let mut names: Vec<String> = vec![self.voice_for_language("en"), self.voice_for_language("de")];
        names.extend(self.voices.values().cloned());
        names.extend(self.agents.values().filter_map(|a| a.voice.clone()));
        names.sort();
        names.dedup();
        names
    }

    /// The fixed voice of an agent, if it has one.
    pub fn agent_voice(&self, agent: &str) -> Option<&str> {
        self.agents.get(agent).and_then(|a| a.voice.as_deref())
    }

    /// Whether replies of `agent` are read aloud. The toggle of the agent
    /// view wins, then `[voice.agents.<id>]`, then the global switch.
    pub fn speaks(&self, agent: &str, toggles: &Toggles) -> bool {
        toggles
            .speak_replies
            .get(agent)
            .copied()
            .or_else(|| self.agents.get(agent).and_then(|a| a.speak_replies))
            .unwrap_or(self.speak_replies)
    }
}

/// The speaker toggles of the agent view, kept in `<data dir>/voice.json`
/// because the app never rewrites `meerora.toml`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Toggles {
    #[serde(default)]
    pub speak_replies: BTreeMap<String, bool>,
}

impl Toggles {
    /// Reads the toggles. A missing or broken file gives none.
    pub fn load(path: &Path) -> Toggles {
        std::fs::read_to_string(path).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
    }

    /// Writes the toggles through a temporary file, so a crash never leaves half a file.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(&tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_without_a_voice_section() {
        let s = VoiceSettings::from_toml("[index]\nurl = \"http://x\"\n").unwrap();
        assert_eq!(s, VoiceSettings::default());
        assert!(s.enabled);
        assert_eq!(s.key, "F8");
        assert_eq!(s.spoken_language(), None);
        assert!(!s.speak_replies);
    }

    #[test]
    fn reads_every_field() {
        let s = VoiceSettings::from_toml(
            r#"
            [voice]
            enabled = false
            key = "F9"
            language = "de"
            speak_replies = true
            [voice.voices]
            de = "de_DE-kerstin-low"
            [voice.agents.wren]
            speak_replies = false
            voice = "en_GB-alan-medium"
            "#,
        )
        .unwrap();
        assert!(!s.enabled);
        assert_eq!(s.key, "F9");
        assert_eq!(s.spoken_language(), Some("de"));
        assert!(s.speak_replies);
        assert_eq!(s.voice_for_language("de"), "de_DE-kerstin-low");
        // English keeps its default although the table names only German.
        assert_eq!(s.voice_for_language("en"), DEFAULT_EN);
        assert_eq!(s.agent_voice("wren"), Some("en_GB-alan-medium"));
        assert_eq!(s.all_voices(), vec!["de_DE-kerstin-low", "en_GB-alan-medium", DEFAULT_EN]);
    }

    #[test]
    fn unknown_fields_are_errors() {
        let err = VoiceSettings::from_toml("[voice]\nenable = true\n").unwrap_err();
        assert!(err.contains("enable"), "{err}");
        assert!(VoiceSettings::from_toml("[voice.agents.wren]\nspeak = true\n").is_err());
    }

    #[test]
    fn unknown_language_falls_back() {
        let mut s = VoiceSettings::default();
        assert_eq!(s.voice_for_language("fr"), DEFAULT_EN);
        s.language = "de".into();
        assert_eq!(s.voice_for_language("fr"), DEFAULT_DE);
        s.voices.insert("fr".into(), "fr_FR-siwis-medium".into());
        assert_eq!(s.voice_for_language("fr"), "fr_FR-siwis-medium");
    }

    #[test]
    fn speaking_replies_toggle_wins_then_agent_then_global() {
        let mut s = VoiceSettings::default();
        let mut toggles = Toggles::default();
        assert!(!s.speaks("wren", &toggles));
        s.speak_replies = true;
        assert!(s.speaks("wren", &toggles));
        s.agents.insert("wren".into(), AgentVoice { speak_replies: Some(false), voice: None });
        assert!(!s.speaks("wren", &toggles));
        assert!(s.speaks("claude", &toggles));
        toggles.speak_replies.insert("wren".into(), true);
        assert!(s.speaks("wren", &toggles));
    }

    #[test]
    fn toggles_round_trip_through_a_file() {
        let dir = std::env::temp_dir().join(format!("meerora-voice-toggles-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("voice.json");
        assert_eq!(Toggles::load(&path), Toggles::default());
        let mut t = Toggles::default();
        t.speak_replies.insert("claude".into(), true);
        t.save(&path).unwrap();
        assert_eq!(Toggles::load(&path), t);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(Toggles::load(&path), Toggles::default());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_reads_the_data_dir() {
        let dir = std::env::temp_dir().join(format!("meerora-voice-load-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(VoiceSettings::load(&dir).unwrap(), VoiceSettings::default());
        std::fs::write(dir.join("meerora.toml"), "[voice]\nspeak_replies = true\n").unwrap();
        assert!(VoiceSettings::load(&dir).unwrap().speak_replies);
        std::fs::write(dir.join("meerora.toml"), "[voice]\nkey = 3\n").unwrap();
        assert!(VoiceSettings::load(&dir).unwrap_err().contains("meerora.toml"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
