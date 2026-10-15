//! Piper speaks a sentence into a WAV file and whisper transcribes it back,
//! in English and in German. Nothing is played aloud and no microphone is
//! needed. The test skips with a note when the models are not in the cache
//! (`meerora-voice fetch` downloads them).

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use meerora_voice::download::{Cache, Http};
use meerora_voice::service::{Options, voice_model_path, whisper_model_path};
use meerora_voice::stt::Transcriber;
use meerora_voice::tts::{Piper, VoiceModel};
use meerora_voice::{Input, Voice, VoiceEvent, VoiceSettings, audio, catalog, lang};

/// How close a transcript must come to the sentence, see [`similarity`].
const SIMILAR: f32 = 0.85;

/// The tests take turns. Each whisper runs up to eight threads that spin
/// while they wait for each other, so two at once on a machine busy with
/// builds slow each other down far more than twice.
static WHISPER: Mutex<()> = Mutex::new(());

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect()
}

/// 1 minus the edit distance between the lowercase words of both texts,
/// relative to the longer one. Piper samples noise for every sentence, so
/// every run speaks it a little differently, and whisper then hears a word
/// like "Tests im" as "Test-In" now and then. Counting whole words would
/// fail on two such slips in a short sentence, while a wrong or empty
/// transcript still ends far below [`SIMILAR`].
fn similarity(expected: &str, heard: &str) -> f32 {
    let a: Vec<char> = words(expected).join(" ").chars().collect();
    let b: Vec<char> = words(heard).join(" ").chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let next = (diagonal + usize::from(ca != cb)).min(row[j] + 1).min(row[j + 1] + 1);
            diagonal = row[j + 1];
            row[j + 1] = next;
        }
    }
    1.0 - row[b.len()] as f32 / a.len().max(b.len()).max(1) as f32
}

#[test]
fn similarity_tolerates_slips_but_not_another_sentence() {
    let sentence = "Kannst du mir bitte die Tests im Projekt zeigen?";
    assert_eq!(similarity(sentence, "kannst du mir bitte die tests im projekt zeigen"), 1.0);
    assert!(similarity(sentence, "Kannst du mir bitte die Test-In-Projekt zeigen?") >= SIMILAR);
    assert!(similarity(sentence, "Please open the editor.") < 0.5);
    assert_eq!(similarity(sentence, ""), 0.0);
}

struct Setup {
    cache: Cache,
    piper: Piper,
    transcriber: Transcriber,
    out: PathBuf,
}

fn setup(name: &str, voices: &[&str]) -> Option<Setup> {
    let cache = Cache::new(Cache::default_root()?);
    let mut assets = catalog::speech_to_text();
    assets.extend(catalog::text_to_speech(&voices.iter().map(|v| v.to_string()).collect::<Vec<_>>()));
    let missing: Vec<String> = cache.missing(&assets).iter().map(|a| a.label.clone()).collect();
    if !missing.is_empty() || catalog::piper().is_none() {
        eprintln!("skipped: {} missing in {} (run meerora-voice fetch)", missing.join(", "), cache.root().display());
        return None;
    }
    let transcriber = Transcriber::load(&whisper_model_path(&cache)).expect("loading whisper");
    let piper = Piper::new(catalog::piper_binary(&cache));
    let out = std::env::temp_dir().join(format!("meerora-voice-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&out).unwrap();
    Some(Setup { cache, piper, transcriber, out })
}

/// Speaks `sentence` with the voice that the language heuristic picks,
/// transcribes the WAV and returns the text and the detected language.
fn round_trip(s: &Setup, sentence: &str, name: &str, language: Option<&str>) -> (String, Option<String>) {
    let voice_name = lang::choose_voice(&Default::default(), None, sentence);
    let voice = VoiceModel::load(&voice_model_path(&s.cache, &voice_name)).unwrap();
    let wav = s.out.join(format!("{name}.wav"));
    s.piper.synthesize(&voice, sentence, &wav).unwrap();
    let clip = audio::read_wav(&wav).unwrap();
    assert_eq!(clip.rate, voice.sample_rate);
    assert!(clip.seconds() > 1.0, "Piper wrote {} s", clip.seconds());
    let samples = audio::resample(&clip.samples, clip.rate, audio::WHISPER_RATE);
    let t = s.transcriber.transcribe(&samples, language).unwrap();
    eprintln!("{name}: {sentence:?} came back as {:?} ({:?})", t.text, t.language);
    (t.text, t.language)
}

#[test]
fn english_and_german_round_trip() {
    let _turn = WHISPER.lock().unwrap_or_else(|e| e.into_inner());
    let Some(s) = setup("roundtrip", &["en_US-lessac-medium", "de_DE-thorsten-medium"]) else { return };

    let en = "Please open the editor and show me the latest changes in the project.";
    let (text, language) = round_trip(&s, en, "en", None);
    assert_eq!(language.as_deref(), Some("en"));
    assert!(similarity(en, &text) >= SIMILAR, "{text:?}");

    let de = "Bitte öffne den Editor und zeige mir die neuesten Änderungen im Projekt.";
    let (text, language) = round_trip(&s, de, "de", None);
    assert_eq!(language.as_deref(), Some("de"));
    assert!(similarity(de, &text) >= SIMILAR, "{text:?}");

    // A fixed language works as well.
    let (text, _) = round_trip(&s, de, "de-fixed", Some("de"));
    assert!(similarity(de, &text) >= SIMILAR, "{text:?}");

    std::fs::remove_dir_all(&s.out).unwrap();
}

/// The whole service as the app uses it, with a recording that comes from a
/// file instead of the microphone: begin, end, then the transcript arrives
/// for the field it was meant for.
#[test]
fn the_service_turns_a_recording_into_a_transcript() {
    let _turn = WHISPER.lock().unwrap_or_else(|e| e.into_inner());
    let Some(s) = setup("service", &["de_DE-thorsten-medium"]) else { return };
    let sentence = "Kannst du mir bitte die Tests im Projekt zeigen?";
    let voice = VoiceModel::load(&voice_model_path(&s.cache, "de_DE-thorsten-medium")).unwrap();
    let wav = s.out.join("recording.wav");
    s.piper.synthesize(&voice, sentence, &wav).unwrap();
    let clip = audio::read_wav(&wav).unwrap();
    let samples = audio::resample(&clip.samples, clip.rate, audio::WHISPER_RATE);
    // Raw 16 kHz mono samples, as pw-record writes them.
    let raw = s.out.join("recording.raw");
    std::fs::write(&raw, samples.iter().flat_map(|x| ((x * 32767.0) as i16).to_le_bytes()).collect::<Vec<u8>>()).unwrap();

    let raw_path = raw.clone();
    let options = Options {
        source: Arc::new(Http),
        record: Box::new(move || {
            let mut cmd = Command::new("cat");
            cmd.arg(&raw_path);
            cmd
        }),
        has_program: |_| true,
    };
    let settings = VoiceSettings { language: "de".into(), ..VoiceSettings::default() };
    let service = Voice::with_options(settings, s.cache.clone(), options);
    assert!(service.status().stt_ready);
    assert!(service.begin("chat:claude"));
    assert_eq!(service.status().input, Input::Recording);
    std::thread::sleep(Duration::from_millis(300));
    service.end();
    // Generous, because whisper on a machine busy with builds takes long.
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut events = Vec::new();
    while Instant::now() < deadline && !events.iter().any(|e| matches!(e, VoiceEvent::Transcript { .. })) {
        events.extend(service.poll());
        std::thread::sleep(Duration::from_millis(50));
    }
    let text = events
        .iter()
        .find_map(|e| match e {
            VoiceEvent::Transcript { target, text, .. } if target == "chat:claude" => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no transcript, events {events:?}"));
    eprintln!("service: {sentence:?} came back as {text:?}");
    assert!(similarity(sentence, &text) >= SIMILAR, "{text:?}");
    assert_eq!(service.status().input, Input::Idle);
    drop(service);
    std::fs::remove_dir_all(&s.out).unwrap();
}
