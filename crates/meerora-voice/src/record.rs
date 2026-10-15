//! Recording from the default microphone with `pw-record`, which PipeWire
//! already resamples to 16 kHz mono. The samples come through a pipe, so the
//! level is live and nothing is written to disk.

use std::io::{self, Read};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::audio::{self, WHISPER_RATE};

/// Recordings stop collecting after this long.
pub const MAX_SECONDS: u32 = 120;

/// The level of the last chunk, shared with the UI as the bits of an f32.
#[derive(Clone, Default)]
pub struct Level(Arc<AtomicU32>);

impl Level {
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }

    pub fn set(&self, value: f32) {
        self.0.store(value.to_bits(), Ordering::Relaxed);
    }
}

/// `pw-record` writing raw 16 kHz mono samples to its stdout.
pub fn pw_record() -> Command {
    let mut cmd = Command::new("pw-record");
    cmd.args(["--raw", "--rate", "16000", "--channels", "1", "--format", "s16", "--media-category", "Capture"]);
    cmd.args(["--media-role", "Communication", "-P", "{ node.name = meerora-voice, media.name = meerora }", "-"]);
    cmd
}

/// A running recording.
pub struct Recorder {
    child: Child,
    reader: Option<JoinHandle<Vec<f32>>>,
    started: Instant,
}

impl Recorder {
    /// Starts `cmd`, which must write raw little endian 16 bit mono samples
    /// at 16 kHz to its stdout, and collects them.
    pub fn start(mut cmd: Command, level: Level) -> io::Result<Recorder> {
        let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
        let mut stdout = child.stdout.take().expect("piped stdout");
        let reader = std::thread::Builder::new().name("voice-record".into()).spawn(move || {
            let limit = (MAX_SECONDS * WHISPER_RATE) as usize;
            let mut samples = Vec::with_capacity(WHISPER_RATE as usize * 10);
            // 32 ms per read keeps the level meter lively.
            let mut buf = [0u8; 1024];
            let mut odd: Option<u8> = None;
            let mut chunk = Vec::with_capacity(512);
            loop {
                let n = match stdout.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                };
                let mut bytes: Vec<u8> = Vec::with_capacity(n + 1);
                bytes.extend(odd.take());
                bytes.extend_from_slice(&buf[..n]);
                if bytes.len() % 2 == 1 {
                    odd = bytes.pop();
                }
                chunk.clear();
                audio::s16le_to_f32(&bytes, &mut chunk);
                level.set(audio::level(&chunk));
                if samples.len() < limit {
                    samples.extend_from_slice(&chunk[..chunk.len().min(limit - samples.len())]);
                }
            }
            level.set(0.0);
            samples
        })?;
        Ok(Recorder { child, reader: Some(reader), started: Instant::now() })
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Stops the recorder and returns what it heard.
    pub fn stop(mut self) -> io::Result<Vec<f32>> {
        self.end();
        let samples = self.reader.take().map(|r| r.join().unwrap_or_default()).unwrap_or_default();
        Ok(samples)
    }

    /// Asks the recorder to finish (SIGINT lets pw-record flush), then makes sure it is gone.
    fn end(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            // SAFETY: the pid belongs to our child, which was not reaped yet.
            unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGINT) };
            let deadline = Instant::now() + Duration::from_millis(500);
            while Instant::now() < deadline {
                if !matches!(self.child.try_wait(), Ok(None)) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.end();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_what_the_command_writes() {
        let dir = std::env::temp_dir().join(format!("meerora-voice-rec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("speech.raw");
        let samples: Vec<i16> = (0..16_001).map(|i| ((i % 100) as i16 - 50) * 300).collect();
        std::fs::write(&path, samples.iter().flat_map(|s| s.to_le_bytes()).collect::<Vec<u8>>()).unwrap();
        let mut cmd = Command::new("cat");
        cmd.arg(&path);
        let level = Level::default();
        let rec = Recorder::start(cmd, level.clone()).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let got = rec.stop().unwrap();
        assert_eq!(got.len(), samples.len());
        assert_eq!(got[1], samples[1] as f32 / 32768.0);
        assert_eq!(level.get(), 0.0, "the level drops when the recording ends");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stop_ends_a_command_that_never_finishes() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exec sleep 30"]);
        let rec = Recorder::start(cmd, Level::default()).unwrap();
        let t = Instant::now();
        assert!(rec.stop().unwrap().is_empty());
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn pw_record_writes_raw_mono_16k_to_stdout() {
        let cmd = pw_record();
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(cmd.get_program(), "pw-record");
        assert!(args.windows(2).any(|w| w == ["--rate", "16000"]));
        assert!(args.windows(2).any(|w| w == ["--channels", "1"]));
        assert!(args.contains(&"--raw".to_string()));
        assert_eq!(args.last().map(String::as_str), Some("-"));
    }
}
