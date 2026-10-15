//! Speech to text with whisper.cpp through whisper-rs, which builds
//! whisper.cpp from source with cmake as part of `cargo build`.

use std::path::Path;

use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::audio::WHISPER_RATE;

/// What whisper heard.
#[derive(Clone, Debug, PartialEq)]
pub struct Transcript {
    pub text: String,
    /// The language whisper detected or was told, such as `en`.
    pub language: Option<String>,
}

/// A loaded whisper model. Loading takes a moment, so keep it.
pub struct Transcriber {
    ctx: WhisperContext,
    threads: i32,
}

impl Transcriber {
    /// Loads a ggml model file such as `ggml-base.bin`.
    pub fn load(model: &Path) -> Result<Transcriber, String> {
        // whisper.cpp logs through tracing (target whisper_rs) instead of stderr.
        whisper_rs::install_logging_hooks();
        let ctx = WhisperContext::new_with_params(model, WhisperContextParameters::default())
            .map_err(|e| format!("cannot load the whisper model {}: {e}", model.display()))?;
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 8) as i32;
        Ok(Transcriber { ctx, threads })
    }

    /// Transcribes mono 16 kHz samples. `language` is a code such as `de`,
    /// or `None` to let whisper detect it.
    pub fn transcribe(&self, samples: &[f32], language: Option<&str>) -> Result<Transcript, String> {
        let mut state = self.ctx.create_state().map_err(|e| format!("whisper: {e}"))?;
        let mut params = FullParams::new(SamplingStrategy::BeamSearch { beam_size: 5, patience: -1.0 });
        params.set_n_threads(self.threads);
        params.set_language(Some(language.unwrap_or("auto")));
        params.set_translate(false);
        params.set_no_context(true);
        params.set_suppress_blank(true);
        params.set_suppress_nst(true);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_special(false);
        params.set_print_timestamps(false);
        // whisper wants at least a second, so short clips get silence after them.
        let min = (WHISPER_RATE as usize * 3) / 2;
        let mut padded;
        let samples = if samples.len() < min {
            padded = samples.to_vec();
            padded.resize(min, 0.0);
            &padded[..]
        } else {
            samples
        };
        state.full(params, samples).map_err(|e| format!("whisper: {e}"))?;
        let mut parts = Vec::new();
        for segment in state.as_iter() {
            if let Ok(text) = segment.to_str_lossy() {
                parts.push(text.into_owned());
            }
        }
        let language = whisper_rs::get_lang_str(state.full_lang_id_from_state()).map(str::to_string);
        Ok(Transcript { text: clean(&parts.join(" ")), language })
    }
}

/// Joins the segments, drops markers such as `[BLANK_AUDIO]` or `(music)`
/// and collapses spaces.
pub fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut depth = 0i32;
    for c in text.chars() {
        match c {
            '[' | '(' => depth += 1,
            ']' | ')' => depth = (depth - 1).max(0),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    let words: Vec<&str> = out.split_whitespace().collect();
    let joined = words.join(" ");
    // A marker between words can leave " ," or " ." behind.
    joined.replace(" ,", ",").replace(" .", ".").trim_matches(|c: char| c == '-' || c.is_whitespace()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_drops_markers_and_spaces() {
        assert_eq!(clean(" Hello there. "), "Hello there.");
        assert_eq!(clean("[BLANK_AUDIO]"), "");
        assert_eq!(clean(" (music) Hallo  Welt [Musik] ."), "Hallo Welt.");
        assert_eq!(clean(" - Wie geht es dir?"), "Wie geht es dir?");
    }
}
