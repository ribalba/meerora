//! The voice service. Recording, transcription, speech and downloads run on
//! their own threads behind one handle. A UI calls [`Voice::begin`] and
//! [`Voice::end`] around push to talk, reads [`Voice::status`] for its
//! indicator and drains [`Voice::poll`] for transcripts once per frame.
//! Every call returns at once.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::download::{Asset, Cache, Http, Progress, Source};
use crate::record::{self, Level, Recorder};
use crate::settings::VoiceSettings;
use crate::stt::Transcriber;
use crate::tts::{Piper, Speech, VoiceModel};
use crate::{audio, catalog, lang};

/// Called from the voice threads whenever the status changes, for example
/// to request a repaint.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// Replies longer than this are cut after a sentence before they are spoken.
pub const MAX_SPOKEN_CHARS: usize = 1500;

/// What the microphone side is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    Idle,
    Recording,
    Transcribing,
}

/// A snapshot for the UI.
#[derive(Clone, Debug, PartialEq)]
pub struct Status {
    pub input: Input,
    /// The field the recording is for, as given to [`Voice::begin`].
    pub target: Option<String>,
    /// How long the current recording runs.
    pub recording_for: Duration,
    /// Loudness of the microphone from 0 to 1 while recording.
    pub level: f32,
    /// Who is speaking right now (the `who` of [`Voice::speak`]), `Some("")` for no one in particular.
    pub speaking: Option<String>,
    /// Utterances waiting after the current one.
    pub queued: usize,
    /// The running download.
    pub download: Option<Progress>,
    /// The whisper model is there and pw-record works.
    pub stt_ready: bool,
    /// Piper, the voices and pw-play are there.
    pub tts_ready: bool,
}

/// Something for the UI to act on.
#[derive(Clone, Debug, PartialEq)]
pub enum VoiceEvent {
    /// Text for the field `target` (as given to [`Voice::begin`]). Never sent on its own.
    Transcript { target: String, text: String, language: Option<String> },
    /// A message for a toast.
    Notice { text: String, error: bool },
}

/// How the service reaches the outside world. Tests replace the parts.
pub struct Options {
    pub source: Arc<dyn Source>,
    /// Builds the recording command (normally [`record::pw_record`]).
    pub record: Box<dyn Fn() -> Command + Send>,
    /// Whether a program is installed (normally a look at `PATH`).
    pub has_program: fn(&str) -> bool,
}

impl Default for Options {
    fn default() -> Self {
        Options { source: Arc::new(Http), record: Box::new(record::pw_record), has_program: on_path }
    }
}

/// True when `name` is an executable file in a folder of `PATH`.
pub fn on_path(name: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path)
            .any(|dir| std::fs::metadata(dir.join(name)).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
    })
}

enum InputCmd {
    Begin(String),
    End,
    Cancel,
}

struct State {
    input: Input,
    target: Option<String>,
    started: Option<Instant>,
    download: Option<Progress>,
    downloading: bool,
    stt_ready: bool,
    tts_ready: bool,
}

struct Utterance {
    text: String,
    voice: String,
    who: String,
}

#[derive(Default)]
struct SpeechQueue {
    queue: VecDeque<Utterance>,
    current: Option<Speech>,
    speaking: Option<String>,
    /// Counts stops, so speech started before a stop is stopped too.
    generation: u64,
}

struct Shared {
    settings: VoiceSettings,
    cache: Cache,
    source: Arc<dyn Source>,
    has_program: fn(&str) -> bool,
    state: Mutex<State>,
    level: Level,
    events: Mutex<Vec<VoiceEvent>>,
    waker: Mutex<Option<Waker>>,
    speech: Mutex<SpeechQueue>,
    speech_cv: Condvar,
    cancel_download: AtomicBool,
    shutdown: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    fn wake(&self) {
        let waker = lock(&self.waker).clone();
        if let Some(w) = waker {
            w();
        }
    }

    fn notice(&self, text: impl Into<String>, error: bool) {
        let text = text.into();
        if error {
            tracing::warn!("voice: {text}");
        } else {
            tracing::info!("voice: {text}");
        }
        lock(&self.events).push(VoiceEvent::Notice { text, error });
        self.wake();
    }

    fn set_input(&self, input: Input, target: Option<String>) {
        let mut s = lock(&self.state);
        s.input = input;
        if input == Input::Idle {
            s.target = None;
            s.started = None;
        } else if target.is_some() {
            s.target = target;
        }
        if input == Input::Recording {
            s.started = Some(Instant::now());
        }
        drop(s);
        self.wake();
    }

    fn stt_assets(&self) -> Vec<Asset> {
        catalog::speech_to_text()
    }

    fn tts_assets(&self) -> Vec<Asset> {
        catalog::text_to_speech(&self.settings.all_voices())
    }

    /// Looks at the cache and the programs again.
    fn refresh_ready(&self) {
        let stt = self.stt_assets().iter().all(|a| self.cache.is_ready(a)) && (self.has_program)("pw-record");
        let tts = catalog::piper().is_some()
            && self.tts_assets().iter().all(|a| self.cache.is_ready(a))
            && catalog::piper_binary(&self.cache).is_file()
            && (self.has_program)("pw-play");
        let mut s = lock(&self.state);
        s.stt_ready = stt;
        s.tts_ready = tts;
    }
}

/// The handle. Dropping it stops speech and recording.
pub struct Voice {
    shared: Arc<Shared>,
    input: mpsc::Sender<InputCmd>,
}

impl Voice {
    /// Starts the threads with the real downloads, `pw-record` and `pw-play`.
    pub fn start(settings: VoiceSettings, cache: Cache) -> Voice {
        Voice::with_options(settings, cache, Options::default())
    }

    pub fn with_options(settings: VoiceSettings, cache: Cache, options: Options) -> Voice {
        let shared = Arc::new(Shared {
            settings,
            cache,
            source: options.source,
            has_program: options.has_program,
            state: Mutex::new(State {
                input: Input::Idle,
                target: None,
                started: None,
                download: None,
                downloading: false,
                stt_ready: false,
                tts_ready: false,
            }),
            level: Level::default(),
            events: Mutex::new(Vec::new()),
            waker: Mutex::new(None),
            speech: Mutex::new(SpeechQueue::default()),
            speech_cv: Condvar::new(),
            cancel_download: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
        });
        shared.refresh_ready();
        let (tx, rx) = mpsc::channel();
        {
            let shared = shared.clone();
            let record = options.record;
            std::thread::Builder::new()
                .name("voice-input".into())
                .spawn(move || input_loop(&shared, rx, record))
                .expect("spawning the voice input thread");
        }
        {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("voice-speech".into())
                .spawn(move || speech_loop(&shared))
                .expect("spawning the voice speech thread");
        }
        Voice { shared, input: tx }
    }

    pub fn settings(&self) -> &VoiceSettings {
        &self.shared.settings
    }

    pub fn cache(&self) -> &Cache {
        &self.shared.cache
    }

    /// Called whenever the status changes or an event waits.
    pub fn set_waker(&self, waker: Waker) {
        *lock(&self.shared.waker) = Some(waker);
    }

    pub fn status(&self) -> Status {
        let s = lock(&self.shared.state);
        let q = lock(&self.shared.speech);
        Status {
            input: s.input,
            target: s.target.clone(),
            recording_for: s.started.map_or(Duration::ZERO, |t| t.elapsed()),
            level: if s.input == Input::Recording { self.shared.level.get() } else { 0.0 },
            speaking: q.speaking.clone(),
            queued: q.queue.len(),
            download: s.download.clone(),
            stt_ready: s.stt_ready,
            tts_ready: s.tts_ready,
        }
    }

    /// Takes the events that arrived since the last call.
    pub fn poll(&self) -> Vec<VoiceEvent> {
        std::mem::take(&mut *lock(&self.shared.events))
    }

    /// Starts recording for `target`, any name the caller recognises in the
    /// transcript later. Without the models it starts their download
    /// instead and returns false.
    pub fn begin(&self, target: impl Into<String>) -> bool {
        let (ready, input) = {
            let s = lock(&self.shared.state);
            (s.stt_ready, s.input)
        };
        if !ready {
            if !(self.shared.has_program)("pw-record") {
                self.shared.notice("Voice needs pw-record from PipeWire, which is not installed", true);
                return false;
            }
            self.fetch();
            return false;
        }
        if input != Input::Idle {
            return false;
        }
        let target = target.into();
        self.shared.set_input(Input::Recording, Some(target.clone()));
        let _ = self.input.send(InputCmd::Begin(target));
        true
    }

    /// Stops recording and transcribes what was said.
    pub fn end(&self) {
        let recording = lock(&self.shared.state).input == Input::Recording;
        if recording {
            self.shared.set_input(Input::Transcribing, None);
            let _ = self.input.send(InputCmd::End);
        }
    }

    /// Stops recording and forgets it.
    pub fn cancel(&self) {
        let _ = self.input.send(InputCmd::Cancel);
    }

    /// Queues `text` (Markdown is fine) to be spoken. `who` names the
    /// speaker for [`Status::speaking`] and picks its fixed voice from
    /// `[voice.agents]`. Without Piper or the voices it starts their
    /// download instead.
    pub fn speak(&self, text: &str, who: Option<&str>) {
        let spoken = lang::speakable(text, MAX_SPOKEN_CHARS);
        if spoken.is_empty() {
            return;
        }
        if !lock(&self.shared.state).tts_ready {
            if catalog::piper().is_none() {
                self.shared.notice("Piper has no prebuilt release for this system, so replies cannot be spoken", true);
            } else if !(self.shared.has_program)("pw-play") {
                self.shared.notice("Speaking replies needs pw-play from PipeWire, which is not installed", true);
            } else {
                self.fetch();
            }
            return;
        }
        let voice = lang::choose_voice(&self.shared.settings, who, &spoken);
        let mut q = lock(&self.shared.speech);
        q.queue.push_back(Utterance { text: spoken, voice, who: who.unwrap_or_default().to_string() });
        drop(q);
        self.shared.speech_cv.notify_all();
        self.shared.wake();
    }

    /// Stops speaking at once and drops what was queued.
    pub fn stop_speaking(&self) {
        let mut q = lock(&self.shared.speech);
        q.queue.clear();
        q.generation += 1;
        if let Some(mut s) = q.current.take() {
            s.stop();
        }
        q.speaking = None;
        drop(q);
        self.shared.wake();
    }

    /// What a download would fetch now, in bytes, as far as the sizes are known.
    pub fn missing_bytes(&self) -> u64 {
        let mut assets = self.shared.stt_assets();
        assets.extend(self.shared.tts_assets());
        self.shared.cache.missing(&assets).iter().filter_map(|a| a.size).sum()
    }

    /// Downloads whatever is missing in the background, unless a download runs already.
    pub fn fetch(&self) {
        {
            let mut s = lock(&self.shared.state);
            if s.downloading {
                return;
            }
            s.downloading = true;
        }
        let mb = self.missing_bytes() as f64 / 1e6;
        self.shared.notice(format!("Downloading the voice models (about {mb:.0} MB) into {}", self.shared.cache.root().display()), false);
        let shared = self.shared.clone();
        std::thread::Builder::new()
            .name("voice-download".into())
            .spawn(move || {
                let mut assets = shared.stt_assets();
                assets.extend(shared.tts_assets());
                let mut last = Instant::now() - Duration::from_secs(1);
                let result = shared.cache.fetch(
                    &assets,
                    shared.source.as_ref(),
                    &mut |p| {
                        lock(&shared.state).download = Some(p.clone());
                        if last.elapsed() > Duration::from_millis(100) {
                            last = Instant::now();
                            shared.wake();
                        }
                    },
                    &shared.cancel_download,
                );
                {
                    let mut s = lock(&shared.state);
                    s.downloading = false;
                    s.download = None;
                }
                shared.refresh_ready();
                match result {
                    Ok(()) => {
                        let key = &shared.settings.key;
                        shared.notice(format!("Voice is ready. Hold {key} or the microphone button to talk."), false)
                    }
                    Err(e) => shared.notice(format!("The voice download failed ({e})"), true),
                }
            })
            .expect("spawning the voice download thread");
    }
}

impl Drop for Voice {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Relaxed);
        self.shared.cancel_download.store(true, Ordering::Relaxed);
        self.stop_speaking();
        self.shared.speech_cv.notify_all();
        let _ = self.input.send(InputCmd::Cancel);
    }
}

/// Shortest recording that is transcribed.
const MIN_SAMPLES: usize = audio::WHISPER_RATE as usize / 4;
/// Peak below this counts as silence (about -40 dBFS).
const SILENCE: f32 = 0.01;

fn input_loop(shared: &Shared, rx: mpsc::Receiver<InputCmd>, record: Box<dyn Fn() -> Command + Send>) {
    let mut transcriber: Option<Transcriber> = None;
    let mut recording: Option<Recorder> = None;
    loop {
        let cmd = match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(cmd) => Some(cmd),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        // A recording that hits the limit ends as if the key was released.
        let too_long = recording.as_ref().is_some_and(|r| r.elapsed() >= Duration::from_secs(record::MAX_SECONDS as u64));
        let cmd = match cmd {
            None if too_long => {
                shared.set_input(Input::Transcribing, None);
                InputCmd::End
            }
            None => continue,
            Some(cmd) => cmd,
        };
        match cmd {
            InputCmd::Begin(target) => {
                if recording.is_some() {
                    continue;
                }
                match Recorder::start(record(), shared.level.clone()) {
                    Ok(r) => recording = Some(r),
                    Err(e) => {
                        shared.set_input(Input::Idle, None);
                        shared.notice(format!("Cannot record ({e})"), true);
                        continue;
                    }
                }
                tracing::debug!("recording for {target}");
                // Load the model while the user speaks.
                if transcriber.is_none() {
                    transcriber = load_transcriber(shared);
                }
            }
            InputCmd::Cancel => {
                recording = None;
                shared.set_input(Input::Idle, None);
            }
            InputCmd::End => {
                let Some(r) = recording.take() else {
                    shared.set_input(Input::Idle, None);
                    continue;
                };
                let target = lock(&shared.state).target.clone().unwrap_or_default();
                let samples = r.stop().unwrap_or_default();
                if shared.shutdown.load(Ordering::Relaxed) {
                    break;
                }
                let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
                let key = &shared.settings.key;
                let mut transcript = None;
                if samples.len() < MIN_SAMPLES {
                    shared.notice(format!("Too short. Hold {key} or the microphone button while you speak."), false);
                } else if peak < SILENCE {
                    shared.notice("Nothing heard. Is the microphone muted?", false);
                } else {
                    if transcriber.is_none() {
                        transcriber = load_transcriber(shared);
                    }
                    if let Some(t) = &transcriber {
                        let started = Instant::now();
                        match t.transcribe(&samples, shared.settings.spoken_language()) {
                            Ok(tr) if tr.text.is_empty() => shared.notice("Nothing understood", false),
                            Ok(tr) => {
                                tracing::info!(
                                    "transcribed {:.1} s of speech in {:.1} s ({})",
                                    samples.len() as f32 / audio::WHISPER_RATE as f32,
                                    started.elapsed().as_secs_f32(),
                                    tr.language.as_deref().unwrap_or("?")
                                );
                                transcript = Some(VoiceEvent::Transcript { target, text: tr.text, language: tr.language });
                            }
                            Err(e) => shared.notice(e, true),
                        }
                    }
                }
                // Idle first, so a UI never sees the text while it still shows "Transcribing".
                shared.set_input(Input::Idle, None);
                if let Some(event) = transcript {
                    lock(&shared.events).push(event);
                    shared.wake();
                }
            }
        }
    }
}

fn load_transcriber(shared: &Shared) -> Option<Transcriber> {
    let model = shared.cache.path(&catalog::whisper_model());
    match Transcriber::load(&model) {
        Ok(t) => Some(t),
        Err(e) => {
            shared.notice(e, true);
            None
        }
    }
}

fn speech_loop(shared: &Shared) {
    let mut models: HashMap<String, VoiceModel> = HashMap::new();
    loop {
        let (utterance, generation) = {
            let mut q = lock(&shared.speech);
            loop {
                if shared.shutdown.load(Ordering::Relaxed) {
                    return;
                }
                if let Some(u) = q.queue.pop_front() {
                    q.speaking = Some(u.who.clone());
                    break (u, q.generation);
                }
                q = shared.speech_cv.wait(q).unwrap_or_else(|e| e.into_inner());
            }
        };
        shared.wake();
        let model = match models.get(&utterance.voice) {
            Some(m) => Ok(m.clone()),
            None => VoiceModel::load(&voice_model_path(&shared.cache, &utterance.voice)),
        };
        let started = model.and_then(|m| {
            models.insert(utterance.voice.clone(), m.clone());
            Piper::new(catalog::piper_binary(&shared.cache)).speak(&m, &utterance.text)
        });
        match started {
            Ok(mut speech) => {
                let mut q = lock(&shared.speech);
                if q.generation == generation {
                    q.current = Some(speech);
                } else {
                    speech.stop();
                }
            }
            Err(e) => shared.notice(format!("Cannot speak ({e})"), true),
        }
        loop {
            let mut q = lock(&shared.speech);
            let Some(speech) = q.current.as_mut() else { break };
            match speech.poll() {
                None => {
                    drop(q);
                    std::thread::sleep(Duration::from_millis(30));
                }
                Some(result) => {
                    q.current = None;
                    drop(q);
                    if let Err(e) = result {
                        shared.notice(format!("Speaking failed ({e})"), true);
                    }
                    break;
                }
            }
        }
        let mut q = lock(&shared.speech);
        if q.queue.is_empty() {
            q.speaking = None;
        }
        drop(q);
        shared.wake();
    }
}

/// The model file of a voice in a cache, for tools and tests.
pub fn voice_model_path(cache: &Cache, name: &str) -> PathBuf {
    cache.root().join("voices").join(format!("{name}.onnx"))
}

/// The whisper model in a cache, for tools and tests.
pub fn whisper_model_path(cache: &Cache) -> PathBuf {
    cache.path(&catalog::whisper_model())
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    /// Fails every download after a moment, like a slow server that is down.
    struct NoNetwork;

    impl Source for NoNetwork {
        fn open(&self, url: &str) -> io::Result<(Option<u64>, Box<dyn io::Read + Send>)> {
            std::thread::sleep(Duration::from_millis(300));
            Err(io::Error::other(format!("no network in tests ({url})")))
        }
    }

    fn wait_for(voice: &Voice, mut done: impl FnMut(&[VoiceEvent]) -> bool) -> Vec<VoiceEvent> {
        let mut events = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            events.extend(voice.poll());
            if done(&events) {
                return events;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out, events so far: {events:?}");
    }

    fn temp_cache(name: &str) -> Cache {
        let dir = std::env::temp_dir().join(format!("meerora-voice-svc-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Cache::new(dir)
    }

    #[test]
    fn begin_without_models_downloads_instead_of_recording() {
        let cache = temp_cache("missing");
        let root = cache.root().to_path_buf();
        let options = Options { source: Arc::new(NoNetwork), record: Box::new(|| Command::new("false")), has_program: |_| true };
        let voice = Voice::with_options(VoiceSettings::default(), cache, options);
        let status = voice.status();
        assert!(!status.stt_ready && !status.tts_ready);
        assert!(!voice.begin("talk"));
        assert_eq!(voice.status().input, Input::Idle);
        let events = wait_for(&voice, |e| e.iter().any(|e| matches!(e, VoiceEvent::Notice { error: true, .. })));
        assert!(matches!(&events[0], VoiceEvent::Notice { text, error: false } if text.starts_with("Downloading the voice models")));
        assert!(events.iter().any(|e| matches!(e, VoiceEvent::Notice { text, .. } if text.contains("no network in tests"))));
        assert!(voice.missing_bytes() > 290_000_000);
        drop(voice);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn missing_pw_record_is_reported() {
        let options = Options { source: Arc::new(NoNetwork), record: Box::new(|| Command::new("false")), has_program: |_| false };
        let voice = Voice::with_options(VoiceSettings::default(), temp_cache("nopw"), options);
        assert!(!voice.begin("talk"));
        let events = voice.poll();
        assert!(matches!(&events[..], [VoiceEvent::Notice { text, error: true }] if text.contains("pw-record")), "{events:?}");
    }

    #[test]
    fn speaking_without_piper_starts_the_download_once() {
        let options = Options { source: Arc::new(NoNetwork), record: Box::new(|| Command::new("false")), has_program: |_| true };
        let voice = Voice::with_options(VoiceSettings::default(), temp_cache("speak"), options);
        voice.speak("Hello there.", Some("claude"));
        voice.speak("Hello again.", Some("claude"));
        let events = wait_for(&voice, |e| e.iter().any(|e| matches!(e, VoiceEvent::Notice { error: true, .. })));
        let downloads = events.iter().filter(|e| matches!(e, VoiceEvent::Notice { text, .. } if text.starts_with("Downloading"))).count();
        assert_eq!(downloads, 1, "{events:?}");
        assert_eq!(voice.status().speaking, None);
        // Code only is never spoken and starts nothing.
        voice.speak("```\nfn main() {}\n```", None);
        assert!(voice.poll().is_empty());
    }
}
