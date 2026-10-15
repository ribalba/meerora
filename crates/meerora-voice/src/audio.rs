//! Small audio helpers: 16 bit WAV files, resampling to the 16 kHz whisper
//! wants and a level for the recording indicator.

use std::io;
use std::path::Path;

/// The sample rate whisper works at.
pub const WHISPER_RATE: u32 = 16_000;

/// Mono samples between -1 and 1 with their rate.
#[derive(Clone, Debug, PartialEq)]
pub struct Clip {
    pub samples: Vec<f32>,
    pub rate: u32,
}

impl Clip {
    pub fn seconds(&self) -> f32 {
        self.samples.len() as f32 / self.rate.max(1) as f32
    }
}

/// Reads a PCM WAV file with 16 bit samples. Stereo is mixed down to mono.
pub fn read_wav(path: &Path) -> io::Result<Clip> {
    parse_wav(&std::fs::read(path)?)
}

/// Parses the bytes of a PCM WAV file with 16 bit samples.
pub fn parse_wav(bytes: &[u8]) -> io::Result<Clip> {
    let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, format!("not a 16 bit PCM WAV file: {what}"));
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(bad("no RIFF/WAVE header"));
    }
    let (mut channels, mut rate, mut bits) = (0u16, 0u32, 0u16);
    let mut pos = 12;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = &bytes[pos + 8..(pos + 8 + len).min(bytes.len())];
        match id {
            b"fmt " if body.len() >= 16 => {
                let format = u16::from_le_bytes([body[0], body[1]]);
                if format != 1 && format != 0xFFFE {
                    return Err(bad("compressed"));
                }
                channels = u16::from_le_bytes([body[2], body[3]]);
                rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
                bits = u16::from_le_bytes([body[14], body[15]]);
            }
            b"data" => {
                if bits != 16 || channels == 0 || rate == 0 {
                    return Err(bad("unsupported format"));
                }
                let frame = 2 * channels as usize;
                let samples = body
                    .chunks_exact(frame)
                    .map(|f| {
                        let sum: f32 = f.as_chunks::<2>().0.iter().map(|s| i16::from_le_bytes(*s) as f32 / 32768.0).sum();
                        sum / channels as f32
                    })
                    .collect();
                return Ok(Clip { samples, rate });
            }
            _ => {}
        }
        pos += 8 + len + (len & 1);
    }
    Err(bad("no data chunk"))
}

/// Writes mono 16 bit PCM.
pub fn write_wav(path: &Path, clip: &Clip) -> io::Result<()> {
    let data_len = (clip.samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&clip.rate.to_le_bytes());
    out.extend_from_slice(&(clip.rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in &clip.samples {
        out.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    std::fs::write(path, out)
}

/// Resamples with a windowed sinc filter, which also removes what the lower
/// rate cannot hold.
pub fn resample(samples: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || samples.is_empty() {
        return samples.to_vec();
    }
    const TAPS: isize = 16;
    let ratio = to as f64 / from as f64;
    // Below the lower of both Nyquist frequencies, relative to the source rate.
    let cutoff = ratio.min(1.0) * 0.95;
    let len = (samples.len() as f64 * ratio).round() as usize;
    (0..len)
        .map(|i| {
            let center = i as f64 / ratio;
            let base = center.floor() as isize;
            let mut acc = 0.0f64;
            let mut weight = 0.0f64;
            let reach = (TAPS as f64 / cutoff).ceil() as isize;
            for j in base - reach + 1..=base + reach {
                let Some(&s) = usize::try_from(j).ok().and_then(|j| samples.get(j)) else { continue };
                let x = j as f64 - center;
                let t = x * cutoff;
                let sinc = if t.abs() < 1e-9 { 1.0 } else { (std::f64::consts::PI * t).sin() / (std::f64::consts::PI * t) };
                let w = x / (reach as f64);
                let window = if w.abs() >= 1.0 { 0.0 } else { 0.5 + 0.5 * (std::f64::consts::PI * w).cos() };
                let k = sinc * window;
                acc += s as f64 * k;
                weight += k;
            }
            if weight.abs() > 1e-9 { (acc / weight) as f32 } else { 0.0 }
        })
        .collect()
}

/// Loudness of a chunk from 0 (silence, -60 dBFS and below) to 1 (full scale).
pub fn level(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt();
    if rms <= 1e-6 {
        return 0.0;
    }
    ((20.0 * rms.log10() + 60.0) / 60.0).clamp(0.0, 1.0)
}

/// Converts little endian 16 bit samples, as pw-record writes them.
pub fn s16le_to_f32(bytes: &[u8], out: &mut Vec<f32>) {
    out.extend(bytes.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b) as f32 / 32768.0));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, rate: u32, seconds: f32, amp: f32) -> Vec<f32> {
        (0..(rate as f32 * seconds) as usize).map(|i| amp * (std::f32::consts::TAU * freq * i as f32 / rate as f32).sin()).collect()
    }

    #[test]
    fn wav_round_trip() {
        let dir = std::env::temp_dir().join(format!("meerora-voice-wav-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.wav");
        let clip = Clip { samples: sine(440.0, 22_050, 0.1, 0.5), rate: 22_050 };
        write_wav(&path, &clip).unwrap();
        let back = read_wav(&path).unwrap();
        assert_eq!(back.rate, 22_050);
        assert_eq!(back.samples.len(), clip.samples.len());
        assert!(back.samples.iter().zip(&clip.samples).all(|(a, b)| (a - b).abs() < 1e-3));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stereo_is_mixed_down_and_junk_refused() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF\0\0\0\0WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&[1, 0, 2, 0]);
        bytes.extend_from_slice(&8000u32.to_le_bytes());
        bytes.extend_from_slice(&32000u32.to_le_bytes());
        bytes.extend_from_slice(&[4, 0, 16, 0]);
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(&16384i16.to_le_bytes());
        bytes.extend_from_slice(&0i16.to_le_bytes());
        let clip = parse_wav(&bytes).unwrap();
        assert_eq!((clip.rate, clip.samples.len()), (8000, 1));
        assert!((clip.samples[0] - 0.25).abs() < 1e-4);
        assert!(parse_wav(b"hello").is_err());
    }

    #[test]
    fn resampling_keeps_length_and_tone() {
        let input = sine(440.0, 22_050, 1.0, 0.5);
        let out = resample(&input, 22_050, 16_000);
        assert_eq!(out.len(), 16_000);
        // The 440 Hz tone keeps its loudness.
        let rms = |s: &[f32]| (s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32).sqrt();
        assert!((rms(&out[100..15_900]) - rms(&input)).abs() < 0.02);
        // A tone above the new Nyquist frequency is filtered out.
        let high = resample(&sine(9_000.0, 22_050, 1.0, 0.5), 22_050, 16_000);
        assert!(rms(&high[100..15_900]) < 0.05, "{}", rms(&high[100..15_900]));
    }

    #[test]
    fn level_maps_decibels() {
        assert_eq!(level(&[]), 0.0);
        assert_eq!(level(&[0.0; 100]), 0.0);
        assert!(level(&[1.0; 100]) > 0.99);
        let quiet = level(&sine(200.0, 16_000, 0.1, 0.01));
        assert!(quiet > 0.1 && quiet < 0.4, "{quiet}");
    }

    #[test]
    fn s16le_conversion() {
        let mut out = Vec::new();
        s16le_to_f32(&[0x00, 0x40, 0x00, 0xC0, 0x01], &mut out);
        assert_eq!(out, vec![0.5, -0.5]);
    }
}
