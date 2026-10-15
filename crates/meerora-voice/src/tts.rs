//! Text to speech with the prebuilt Piper binary. Speaking pipes Piper's raw
//! output straight into `pw-play`, so playback starts after the first line.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// A Piper voice: its model, its config and the sample rate from the config.
#[derive(Clone, Debug, PartialEq)]
pub struct VoiceModel {
    pub model: PathBuf,
    pub config: PathBuf,
    pub sample_rate: u32,
}

impl VoiceModel {
    /// Reads the sample rate from `<model>.json`.
    pub fn load(model: &Path) -> Result<VoiceModel, String> {
        let mut config = model.as_os_str().to_os_string();
        config.push(".json");
        let config = PathBuf::from(config);
        let text = std::fs::read_to_string(&config).map_err(|e| format!("cannot read {}: {e}", config.display()))?;
        let json: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", config.display()))?;
        let sample_rate = json.pointer("/audio/sample_rate").and_then(|v| v.as_u64()).unwrap_or(22_050) as u32;
        if !model.is_file() {
            return Err(format!("{} is missing", model.display()));
        }
        Ok(VoiceModel { model: model.to_path_buf(), config, sample_rate })
    }
}

/// The Piper binary.
#[derive(Clone, Debug)]
pub struct Piper {
    pub binary: PathBuf,
}

impl Piper {
    pub fn new(binary: impl Into<PathBuf>) -> Piper {
        Piper { binary: binary.into() }
    }

    fn command(&self, voice: &VoiceModel) -> Command {
        let mut cmd = Command::new(&self.binary);
        cmd.arg("--model").arg(&voice.model).arg("--config").arg(&voice.config).arg("--quiet");
        if let Some(dir) = self.binary.parent() {
            cmd.arg("--espeak_data").arg(dir.join("espeak-ng-data"));
        }
        cmd
    }

    /// Writes `text` spoken by `voice` into a WAV file, without playing it.
    pub fn synthesize(&self, voice: &VoiceModel, text: &str, out: &Path) -> Result<(), String> {
        let mut child = self
            .command(voice)
            .arg("--output_file")
            .arg(out)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", self.binary.display()))?;
        child.stdin.take().expect("piped stdin").write_all(text.as_bytes()).map_err(|e| format!("piper: {e}"))?;
        let output = child.wait_with_output().map_err(|e| format!("piper: {e}"))?;
        if !output.status.success() {
            return Err(format!("piper failed ({}): {}", output.status, String::from_utf8_lossy(&output.stderr).trim()));
        }
        Ok(())
    }

    /// Starts speaking `text` aloud. Piper's raw samples go to `pw-play`.
    pub fn speak(&self, voice: &VoiceModel, text: &str) -> Result<Speech, String> {
        let mut piper = self
            .command(voice)
            .arg("--output_raw")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", self.binary.display()))?;
        let stdout = piper.stdout.take().expect("piped stdout");
        let player = Command::new("pw-play")
            .args(["--raw", "--format", "s16", "--channels", "1", "--media-role", "Communication"])
            .arg("--rate")
            .arg(voice.sample_rate.to_string())
            .args(["-P", "{ node.name = meerora-voice, media.name = meerora }", "-"])
            .stdin(Stdio::from(stdout))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let player = match player {
            Ok(p) => p,
            Err(e) => {
                let _ = piper.kill();
                let _ = piper.wait();
                return Err(format!("cannot start pw-play: {e}"));
            }
        };
        // Piper reads line by line and speaks each line as soon as it is read.
        let mut stdin = piper.stdin.take().expect("piped stdin");
        let text = text.to_string();
        std::thread::spawn(move || {
            let _ = stdin.write_all(text.as_bytes());
            let _ = stdin.write_all(b"\n");
        });
        Ok(Speech { piper, player })
    }
}

/// Speech in progress.
pub struct Speech {
    piper: Child,
    player: Child,
}

impl Speech {
    /// `Some(Ok(()))` when playback ended, `Some(Err)` when it failed, `None` while it plays.
    pub fn poll(&mut self) -> Option<Result<(), String>> {
        match self.player.try_wait() {
            Ok(None) => {
                // A failing Piper closes the pipe and the player ends on its own.
                None
            }
            Ok(Some(_)) => {
                // The player ends after Piper closed the pipe. If it ended
                // early, Piper gets a moment and is then stopped.
                for _ in 0..50 {
                    if !matches!(self.piper.try_wait(), Ok(None)) {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                if matches!(self.piper.try_wait(), Ok(None)) {
                    let _ = self.piper.kill();
                }
                let status = self.piper.wait();
                match status {
                    Ok(s) if s.success() => Some(Ok(())),
                    Ok(s) => {
                        let mut err = String::new();
                        if let Some(mut e) = self.piper.stderr.take() {
                            let _ = e.read_to_string(&mut err);
                        }
                        Some(Err(format!("piper failed ({s}): {}", err.trim())))
                    }
                    Err(e) => Some(Err(format!("piper: {e}"))),
                }
            }
            Err(e) => Some(Err(format!("pw-play: {e}"))),
        }
    }

    /// Stops at once.
    pub fn stop(&mut self) {
        let _ = self.piper.kill();
        let _ = self.player.kill();
        let _ = self.piper.wait();
        let _ = self.player.wait();
    }
}

impl Drop for Speech {
    fn drop(&mut self) {
        if matches!(self.player.try_wait(), Ok(None)) || matches!(self.piper.try_wait(), Ok(None)) {
            self.stop();
        }
    }
}
