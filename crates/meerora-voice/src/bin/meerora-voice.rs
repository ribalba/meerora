//! `meerora-voice` downloads the voice models and tries speech to text and
//! text to speech outside the app.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use meerora_voice::download::{Cache, Http, Progress};
use meerora_voice::record::{Level, Recorder};
use meerora_voice::service::{on_path, voice_model_path, whisper_model_path};
use meerora_voice::stt::Transcriber;
use meerora_voice::tts::{Piper, VoiceModel};
use meerora_voice::{VoiceSettings, audio, catalog, lang};

const USAGE: &str = "\
meerora-voice status                       what is downloaded and what is missing
meerora-voice fetch                        download the models and Piper (about 300 MB)
meerora-voice say [--voice NAME] [--out FILE.wav] TEXT
                                           speak TEXT, or write it to a WAV file
meerora-voice transcribe [--language L] FILE.wav
meerora-voice listen [--seconds N] [--language L]
                                           record from the microphone, then print the text

Every command takes --data-dir DIR to read [voice] of DIR/meerora.toml. The
cache is ~/.cache/meerora/voice (MEERORA_VOICE_CACHE overrides it).";

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    match run(&mut args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("meerora-voice: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Removes `--flag VALUE` from `args` and returns the value.
fn take(args: &mut Vec<String>, flag: &str) -> Result<Option<String>, String> {
    let Some(i) = args.iter().position(|a| a == flag) else { return Ok(None) };
    if i + 1 >= args.len() {
        return Err(format!("{flag} needs a value"));
    }
    let value = args.remove(i + 1);
    args.remove(i);
    Ok(Some(value))
}

fn settings(data_dir: Option<String>) -> Result<VoiceSettings, String> {
    let dir = data_dir.map(PathBuf::from).or_else(|| std::env::var_os("MEERORA_DATA_DIR").map(PathBuf::from)).or_else(|| {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/meerora")).filter(|d| d.join("meerora.toml").is_file())
    });
    match dir {
        Some(dir) => VoiceSettings::load(&dir),
        None => Ok(VoiceSettings::default()),
    }
}

fn run(args: &mut Vec<String>) -> Result<(), String> {
    let settings = settings(take(args, "--data-dir")?)?;
    let cache = Cache::new(Cache::default_root().ok_or("HOME is not set")?);
    let command = args.remove(0);
    match command.as_str() {
        "status" => status(&settings, &cache),
        "fetch" => fetch(&settings, &cache),
        "say" => {
            let voice = take(args, "--voice")?;
            let out = take(args, "--out")?;
            say(&settings, &cache, voice, out, &args.join(" "))
        }
        "transcribe" => {
            let language = take(args, "--language")?.or_else(|| settings.spoken_language().map(str::to_string));
            let file = args.first().ok_or("transcribe needs a WAV file")?;
            let clip = audio::read_wav(file.as_ref()).map_err(|e| format!("{file}: {e}"))?;
            let samples = audio::resample(&clip.samples, clip.rate, audio::WHISPER_RATE);
            transcribe(&cache, &samples, language.as_deref())
        }
        "listen" => {
            let seconds: f32 = take(args, "--seconds")?.map_or(Ok(5.0), |s| s.parse().map_err(|_| "--seconds takes a number"))?;
            let language = take(args, "--language")?.or_else(|| settings.spoken_language().map(str::to_string));
            listen(&cache, seconds, language.as_deref())
        }
        other => Err(format!("unknown command {other:?}\n\n{USAGE}")),
    }
}

fn assets(settings: &VoiceSettings) -> Vec<meerora_voice::download::Asset> {
    let mut assets = catalog::speech_to_text();
    assets.extend(catalog::text_to_speech(&settings.all_voices()));
    assets
}

fn status(settings: &VoiceSettings, cache: &Cache) -> Result<(), String> {
    println!("cache {}", cache.root().display());
    println!("voice {}", if settings.enabled { "enabled" } else { "disabled in meerora.toml" });
    for a in assets(settings) {
        let size = a.size.map_or("size unknown".to_string(), size);
        let state = if cache.is_ready(&a) { "ok" } else { "missing" };
        println!("{state:>8}  {:<40} {size}", a.label);
    }
    for program in ["pw-record", "pw-play"] {
        println!("{:>8}  {program}", if on_path(program) { "ok" } else { "missing" });
    }
    Ok(())
}

fn fetch(settings: &VoiceSettings, cache: &Cache) -> Result<(), String> {
    let assets = assets(settings);
    let missing: u64 = cache.missing(&assets).iter().filter_map(|a| a.size).sum();
    if missing == 0 && cache.missing(&assets).is_empty() {
        println!("everything is in {}", cache.root().display());
        return Ok(());
    }
    eprintln!("downloading about {:.0} MB into {}", missing as f64 / 1e6, cache.root().display());
    let mut last = Instant::now();
    let mut shown = String::new();
    let mut report = |p: &Progress| {
        let line = match p.total {
            Some(t) => format!("[{}/{}] {} {} of {}", p.index, p.count, p.label, size(p.done), size(t)),
            None => format!("[{}/{}] {} {}", p.index, p.count, p.label, size(p.done)),
        };
        if p.label != shown || last.elapsed() > Duration::from_secs(2) || p.total == Some(p.done) {
            eprintln!("{line}");
            last = Instant::now();
            shown = p.label.clone();
        }
    };
    cache.fetch(&assets, &Http, &mut report, &AtomicBool::new(false)).map_err(|e| e.to_string())?;
    println!("done, everything is in {}", cache.root().display());
    Ok(())
}

fn say(settings: &VoiceSettings, cache: &Cache, voice: Option<String>, out: Option<String>, text: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err("say needs a text".into());
    }
    let voice = voice.unwrap_or_else(|| lang::choose_voice(settings, None, text));
    let model = VoiceModel::load(&voice_model_path(cache, &voice)).map_err(|e| format!("{e} (run meerora-voice fetch)"))?;
    let piper = Piper::new(catalog::piper_binary(cache));
    match out {
        Some(out) => {
            piper.synthesize(&model, text, out.as_ref())?;
            println!("wrote {out} with {voice}");
        }
        None => {
            let mut speech = piper.speak(&model, text)?;
            loop {
                if let Some(result) = speech.poll() {
                    return result;
                }
                std::thread::sleep(Duration::from_millis(30));
            }
        }
    }
    Ok(())
}

fn transcribe(cache: &Cache, samples: &[f32], language: Option<&str>) -> Result<(), String> {
    let started = Instant::now();
    let t = Transcriber::load(&whisper_model_path(cache)).map_err(|e| format!("{e} (run meerora-voice fetch)"))?;
    let loaded = started.elapsed();
    let tr = t.transcribe(samples, language)?;
    eprintln!(
        "{:.1} s of audio, model loaded in {:.1} s, transcribed in {:.1} s, language {}",
        samples.len() as f32 / audio::WHISPER_RATE as f32,
        loaded.as_secs_f32(),
        (started.elapsed() - loaded).as_secs_f32(),
        tr.language.as_deref().unwrap_or("?")
    );
    println!("{}", tr.text);
    Ok(())
}

fn listen(cache: &Cache, seconds: f32, language: Option<&str>) -> Result<(), String> {
    let level = Level::default();
    let rec = Recorder::start(meerora_voice::record::pw_record(), level.clone()).map_err(|e| format!("cannot start pw-record: {e}"))?;
    eprintln!("listening for {seconds} s, speak now");
    let started = Instant::now();
    while started.elapsed().as_secs_f32() < seconds {
        let bars = (level.get() * 30.0) as usize;
        eprint!("\r[{:<30}]", "#".repeat(bars));
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!();
    let samples = rec.stop().map_err(|e| e.to_string())?;
    transcribe(cache, &samples, language)
}

/// Bytes as MB, or kB for small files.
fn size(bytes: u64) -> String {
    if bytes < 1_000_000 { format!("{:.1} kB", bytes as f64 / 1e3) } else { format!("{:.1} MB", bytes as f64 / 1e6) }
}
