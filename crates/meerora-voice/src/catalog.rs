//! What voice downloads and from where. Sources are pinned to a commit or
//! a release, with their sizes and SHA-256 checksums.
//!
//! | File | Source | Size |
//! | --- | --- | --- |
//! | whisper `ggml-base.bin` (multilingual) | huggingface.co/ggerganov/whisper.cpp | 148 MB |
//! | Piper 2023.11.14-2 for Linux x86_64 | github.com/rhasspy/piper releases | 26 MB |
//! | `en_US-lessac-medium` voice | huggingface.co/rhasspy/piper-voices | 63 MB |
//! | `de_DE-thorsten-medium` voice | huggingface.co/rhasspy/piper-voices | 63 MB |

use std::path::{Path, PathBuf};

use crate::download::{Asset, Cache, Kind};

const WHISPER_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/ggml-base.bin";
const WHISPER_SHA256: &str = "60ed5bc3dd14eea856493d334349b405782ddcaf0028d4b5df4088345fba2efe";
const WHISPER_SIZE: u64 = 147_951_465;

const PIPER_URL: &str = "https://github.com/rhasspy/piper/releases/download/2023.11.14-2/piper_linux_x86_64.tar.gz";
const PIPER_SHA256: &str = "a50cb45f355b7af1f6d758c1b360717877ba0a398cc8cbe6d2a7a3a26e225992";
const PIPER_SIZE: u64 = 26_460_462;

/// The commit of rhasspy/piper-voices the pinned voices come from.
const VOICES_COMMIT: &str = "c10ece1aade47bb51c153c893d14e5bf8e5b7117";

/// Pinned voices: name, model SHA-256 and size, config SHA-256 and size.
const PINNED_VOICES: &[(&str, &str, u64, &str, u64)] = &[
    (
        "en_US-lessac-medium",
        "5efe09e69902187827af646e1a6e9d269dee769f9877d17b16b1b46eeaaf019f",
        63_201_294,
        "efe19c417bed055f2d69908248c6ba650fa135bc868b0e6abb3da181dab690a0",
        4885,
    ),
    (
        "de_DE-thorsten-medium",
        "7e64762d8e5118bb578f2eea6207e1a35a8e0c30595010b666f983fc87bb7819",
        63_201_294,
        "974adee790533adb273a1ac88f49027d2a1b8f0f2cf4905954a4791e79264e85",
        4819,
    ),
];

/// The multilingual whisper base model.
pub fn whisper_model() -> Asset {
    Asset {
        label: "whisper base model".into(),
        url: WHISPER_URL.into(),
        sha256: Some(WHISPER_SHA256.into()),
        size: Some(WHISPER_SIZE),
        path: "models/ggml-base.bin".into(),
        kind: Kind::File,
    }
}

/// The prebuilt Piper release, only published here for Linux on x86_64.
pub fn piper() -> Option<Asset> {
    (cfg!(target_os = "linux") && cfg!(target_arch = "x86_64")).then(|| Asset {
        label: "Piper".into(),
        url: PIPER_URL.into(),
        sha256: Some(PIPER_SHA256.into()),
        size: Some(PIPER_SIZE),
        path: "piper".into(),
        kind: Kind::TarGz,
    })
}

/// The Piper binary inside the unpacked release.
pub fn piper_binary(cache: &Cache) -> PathBuf {
    cache.root().join("piper").join("piper").join("piper")
}

/// The model and its config for a Piper voice such as `de_DE-thorsten-medium`.
/// The pinned voices are checked, other names follow the layout of the
/// piper-voices repository and come without a checksum. `None` for names
/// that do not look like `xx_YY-name-quality`.
pub fn voice(name: &str) -> Option<[Asset; 2]> {
    let (locale, rest) = name.split_once('-')?;
    let (speaker, quality) = rest.rsplit_once('-')?;
    let lang = locale.split_once('_')?.0;
    let valid = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !(valid(locale) && valid(speaker) && valid(quality) && valid(lang)) {
        return None;
    }
    let pinned = PINNED_VOICES.iter().find(|v| v.0 == name);
    let commit = if pinned.is_some() { VOICES_COMMIT } else { "main" };
    let base = format!("https://huggingface.co/rhasspy/piper-voices/resolve/{commit}/{lang}/{locale}/{speaker}/{quality}/{name}");
    let model = Asset {
        label: format!("voice {name}"),
        url: format!("{base}.onnx"),
        sha256: pinned.map(|v| v.1.to_string()),
        size: pinned.map(|v| v.2),
        path: Path::new("voices").join(format!("{name}.onnx")),
        kind: Kind::File,
    };
    let config = Asset {
        label: format!("voice {name} config"),
        url: format!("{base}.onnx.json"),
        sha256: pinned.map(|v| v.3.to_string()),
        size: pinned.map(|v| v.4),
        path: Path::new("voices").join(format!("{name}.onnx.json")),
        kind: Kind::File,
    };
    Some([model, config])
}

/// Everything speech to text needs.
pub fn speech_to_text() -> Vec<Asset> {
    vec![whisper_model()]
}

/// Everything text to speech needs for `voices`. Unknown voice names are left out.
pub fn text_to_speech(voices: &[String]) -> Vec<Asset> {
    let mut assets: Vec<Asset> = piper().into_iter().collect();
    assets.extend(voices.iter().filter_map(|v| voice(v)).flatten());
    assets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_voices_use_the_commit_and_carry_checksums() {
        let [model, config] = voice("de_DE-thorsten-medium").unwrap();
        assert_eq!(
            model.url,
            "https://huggingface.co/rhasspy/piper-voices/resolve/c10ece1aade47bb51c153c893d14e5bf8e5b7117/de/de_DE/thorsten/medium/de_DE-thorsten-medium.onnx"
        );
        assert_eq!(model.size, Some(63_201_294));
        assert!(model.sha256.is_some() && config.sha256.is_some());
        assert_eq!(config.path, Path::new("voices/de_DE-thorsten-medium.onnx.json"));
    }

    #[test]
    fn other_voices_follow_the_repository_layout() {
        let [model, config] = voice("en_GB-northern_english_male-medium").unwrap();
        assert_eq!(
            model.url,
            "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_GB/northern_english_male/medium/en_GB-northern_english_male-medium.onnx"
        );
        assert_eq!((model.sha256, model.size, config.sha256), (None, None, None));
        assert!(voice("lessac").is_none());
        assert!(voice("en-lessac-medium").is_none());
        assert!(voice("../x_Y-a-b").is_none());
    }

    #[test]
    fn text_to_speech_lists_piper_and_both_files_per_voice() {
        let assets = text_to_speech(&["en_US-lessac-medium".into(), "nonsense".into()]);
        let piper = usize::from(piper().is_some());
        assert_eq!(assets.len(), piper + 2);
        let total: u64 = text_to_speech(&["en_US-lessac-medium".into(), "de_DE-thorsten-medium".into()])
            .iter()
            .chain(speech_to_text().iter())
            .filter_map(|a| a.size)
            .sum();
        // About 300 MB, as the README says.
        assert!((290_000_000..310_000_000).contains(&total), "{total}");
    }
}
