//! Platform-independent mixdown: combines a microphone WAV and a system
//! audio WAV (mono float32 each, from either provider) into the single
//! WAV file the user actually gets to keep.
//!
//! Reliability over fidelity: both tracks are downmixed to mono, linearly
//! resampled to one target sample rate, loudness-matched by RMS, then
//! summed and hard-clipped. No external resampling/DSP crate — the
//! source material here is voice over a meeting connection, not music.

use std::path::{Path, PathBuf};

use super::{AudioError, RecordingResult, Result};

const TARGET_SAMPLE_RATE: u32 = 48_000;
const TARGET_RMS: f32 = 0.1;

fn read_mono_f32(path: &Path) -> Result<(Vec<f32>, u32)> {
    let mut reader = hound::WavReader::open(path).map_err(|e| AudioError::Backend(e.to_string()))?;
    let spec = reader.spec();
    let channels = spec.channels as usize;
    let raw: Vec<f32> = reader
        .samples::<f32>()
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| AudioError::Backend(e.to_string()))?;

    if channels <= 1 {
        return Ok((raw, spec.sample_rate));
    }
    let mono = raw
        .chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect();
    Ok((mono, spec.sample_rate))
}

fn resample_linear(samples: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if from_rate == to_rate || samples.is_empty() {
        return samples.to_vec();
    }
    let ratio = to_rate as f64 / from_rate as f64;
    let out_len = ((samples.len() as f64) * ratio).round() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src_pos = i as f64 / ratio;
        let idx = src_pos.floor() as usize;
        let frac = (src_pos - idx as f64) as f32;
        let a = samples.get(idx).copied().unwrap_or(0.0);
        let b = samples.get(idx + 1).copied().unwrap_or(a);
        out.push(a + (b - a) * frac);
    }
    out
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    ((sum_sq / samples.len() as f64).sqrt()) as f32
}

/// Scales `samples` so their RMS matches `target`. Leaves near-silence
/// alone instead of blowing up noise floor into audible hiss.
fn normalize_to_rms(samples: &mut [f32], target: f32) {
    let current = rms(samples);
    if current < 1e-4 {
        return;
    }
    let gain = target / current;
    for s in samples.iter_mut() {
        *s *= gain;
    }
}

fn write_mono_i16_wav(path: &Path, samples: &[f32], sample_rate: u32) -> Result<()> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec).map_err(|e| AudioError::Backend(e.to_string()))?;
    for &s in samples {
        let clamped = s.clamp(-1.0, 1.0);
        writer
            .write_sample((clamped * i16::MAX as f32) as i16)
            .map_err(|e| AudioError::Backend(e.to_string()))?;
    }
    writer.finalize().map_err(|e| AudioError::Backend(e.to_string()))
}

/// Mixes the mic track with the system-audio track (if any) into
/// `output_path` as a 48kHz/16-bit mono WAV, deleting the two raw input
/// files afterwards. Falls back to a plain resample of the mic track
/// when there is no system audio to mix in (e.g. it failed to start).
pub fn mix_down(mic_path: &Path, system_path: Option<&Path>, output_path: &Path) -> Result<RecordingResult> {
    let (mic_samples, mic_rate) = read_mono_f32(mic_path)?;
    let mut mic_samples = resample_linear(&mic_samples, mic_rate, TARGET_SAMPLE_RATE);

    let mixed = match system_path {
        Some(system_path) => {
            let (system_samples, system_rate) = read_mono_f32(system_path)?;
            let mut system_samples = resample_linear(&system_samples, system_rate, TARGET_SAMPLE_RATE);

            normalize_to_rms(&mut mic_samples, TARGET_RMS);
            normalize_to_rms(&mut system_samples, TARGET_RMS);

            let len = mic_samples.len().max(system_samples.len());
            let mut mixed = Vec::with_capacity(len);
            for i in 0..len {
                let a = mic_samples.get(i).copied().unwrap_or(0.0);
                let b = system_samples.get(i).copied().unwrap_or(0.0);
                mixed.push(a + b);
            }
            mixed
        }
        None => mic_samples,
    };

    write_mono_i16_wav(output_path, &mixed, TARGET_SAMPLE_RATE)?;

    std::fs::remove_file(mic_path).ok();
    if let Some(system_path) = system_path {
        std::fs::remove_file(system_path).ok();
    }

    Ok(RecordingResult {
        temp_file_path: output_path.to_path_buf(),
        duration_seconds: mixed.len() as f64 / TARGET_SAMPLE_RATE as f64,
    })
}

pub fn timestamped_path(dir: &Path, prefix: &str) -> PathBuf {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    dir.join(format!("{prefix}_{millis}.wav"))
}
