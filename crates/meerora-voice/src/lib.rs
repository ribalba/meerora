//! Local voice for meerora (D16). Speech to text runs on whisper.cpp and
//! text to speech on Piper, both on this machine.
//!
//! [`Voice`] is the handle a UI holds. It records from the default
//! microphone with `pw-record`, transcribes with whisper (through
//! whisper-rs, which builds whisper.cpp with cmake), speaks with the
//! prebuilt Piper binary into `pw-play` and downloads the models on first
//! use into `~/.cache/meerora/voice/`. Nothing here knows egui, so the agent
//! view, the talk field and a command box can all use the same handle.

pub mod audio;
pub mod catalog;
pub mod download;
pub mod lang;
pub mod record;
pub mod service;
pub mod settings;
pub mod stt;
pub mod tts;

pub use service::{Input, Status, Voice, VoiceEvent, Waker};
pub use settings::{AgentVoice, Toggles, VoiceSettings};
