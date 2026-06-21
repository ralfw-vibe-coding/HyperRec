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

/// Without headphones, the microphone picks up the speaker acoustically
/// in addition to the system tap capturing it digitally — the same
/// audio ends up in the mix twice, heard as an echo. Since we already
/// have the system signal digitally, we can estimate how it leaked into
/// the mic (a fixed acoustic delay + gain, reasonable for a stationary
/// laptop/desk setup) via cross-correlation over a short window, then
/// subtract a delayed+scaled copy of it from the mic track before
/// mixing. Not full adaptive AEC, but needs no extra dependencies and
/// runs once as a post-processing step.
fn cancel_acoustic_echo(mic: &mut [f32], system: &[f32], sample_rate: u32) {
    const MAX_DELAY_MS: f64 = 40.0;
    const ANALYSIS_SECONDS: f64 = 2.0;
    // Picking whichever delay has the single largest raw correlation is
    // not enough: over ~2000 candidate delays, pure chance alone throws
    // up a "biggest" one even for two genuinely unrelated signals. Only
    // trust it as real echo if the *normalized* (Pearson-style)
    // correlation clears a real threshold, comfortably above chance
    // level and comfortably below what real leaked echo produces.
    const MIN_NORMALIZED_CORRELATION: f64 = 0.15;

    let max_delay = ((sample_rate as f64 * MAX_DELAY_MS) / 1000.0) as usize;
    let window = ((sample_rate as f64 * ANALYSIS_SECONDS) as usize).min(mic.len()).min(system.len());
    if window == 0 || max_delay == 0 || window <= max_delay * 2 {
        return;
    }

    let mic_win = &mic[..window];
    let sys_win = &system[..window];

    let mut best_delay = 0usize;
    let mut best_norm_corr = 0.0f64;
    let mut best_corr = 0.0f64;
    let mut best_energy_sys = 0.0f64;

    for delay in 0..max_delay {
        let mut corr = 0.0f64;
        let mut energy_mic = 0.0f64;
        let mut energy_sys = 0.0f64;
        for i in delay..window {
            let m = mic_win[i] as f64;
            let s = sys_win[i - delay] as f64;
            corr += m * s;
            energy_mic += m * m;
            energy_sys += s * s;
        }
        if energy_mic < 1e-9 || energy_sys < 1e-9 {
            continue;
        }
        let norm_corr = corr / (energy_mic.sqrt() * energy_sys.sqrt());
        if norm_corr.abs() > best_norm_corr.abs() {
            best_norm_corr = norm_corr;
            best_delay = delay;
            best_corr = corr;
            best_energy_sys = energy_sys;
        }
    }

    // Not echo-like enough (e.g. recording with headphones, nothing to
    // remove) — leave the mic alone rather than risk subtracting noise.
    if best_norm_corr.abs() < MIN_NORMALIZED_CORRELATION || best_energy_sys < 1e-9 {
        return;
    }

    let gain = best_corr / best_energy_sys;
    if gain <= 0.0 {
        return;
    }

    for (i, sample) in mic.iter_mut().enumerate() {
        if i >= best_delay {
            if let Some(&reference) = system.get(i - best_delay) {
                *sample -= (gain * reference as f64) as f32;
            }
        }
    }
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

            cancel_acoustic_echo(&mut mic_samples, &system_samples, TARGET_SAMPLE_RATE);

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

#[cfg(test)]
mod tests {
    use super::*;

    /// A sum of several non-harmonically-related tones. A *single* pure
    /// tone has a periodic self-correlation (ambiguous at every multiple
    /// of its wavelength, which broke an earlier version of this test by
    /// making the delay search lock onto an aliased lag instead of the
    /// real one) — mixing in several frequencies gives a sharp, unique
    /// correlation peak like real broadband audio does, while staying
    /// fully deterministic (unlike "two seeds of one PRNG", which can
    /// carry subtle shared structure despite looking unrelated).
    fn make_complex_tone(len: usize, sample_rate: u32, freqs_hz: &[f32], amplitude: f32) -> Vec<f32> {
        let scale = amplitude / freqs_hz.len() as f32;
        (0..len)
            .map(|i| {
                freqs_hz
                    .iter()
                    .map(|&f| (2.0 * std::f32::consts::PI * f * i as f32 / sample_rate as f32).sin())
                    .sum::<f32>()
                    * scale
            })
            .collect()
    }

    #[test]
    fn cancel_acoustic_echo_removes_known_delayed_copy() {
        let sample_rate = 48_000u32;
        let len = sample_rate as usize * 2;
        let system = make_complex_tone(len, sample_rate, &[311.0, 547.0, 877.0, 1231.0, 1607.0, 1999.0], 0.4);
        let voice = make_complex_tone(len, sample_rate, &[233.0, 397.0, 661.0, 919.0, 1373.0, 1777.0], 0.3);

        let delay = 200usize;
        let leak_gain = 0.35f32;
        let mut mic = voice.clone();
        for i in delay..len {
            mic[i] += leak_gain * system[i - delay];
        }

        let mic_before = mic.clone();
        cancel_acoustic_echo(&mut mic, &system, sample_rate);

        let error_before: f64 = voice.iter().zip(mic_before.iter()).map(|(a, b)| ((a - b) as f64).powi(2)).sum();
        let error_after: f64 = voice.iter().zip(mic.iter()).map(|(a, b)| ((a - b) as f64).powi(2)).sum();

        println!("error_before={error_before:.4} error_after={error_after:.4}");
        assert!(
            error_after < error_before * 0.1,
            "expected echo cancellation to reduce error by >90%, before={error_before}, after={error_after}"
        );
    }

    #[test]
    fn cancel_acoustic_echo_leaves_clean_mic_alone_without_echo() {
        let sample_rate = 48_000u32;
        let len = sample_rate as usize * 2;
        let system = make_complex_tone(len, sample_rate, &[311.0, 547.0, 877.0, 1231.0, 1607.0, 1999.0], 0.4);
        let voice = make_complex_tone(len, sample_rate, &[233.0, 397.0, 661.0, 919.0, 1373.0, 1777.0], 0.3);
        let mut mic = voice.clone();

        cancel_acoustic_echo(&mut mic, &system, sample_rate);

        let diff: f64 = voice.iter().zip(mic.iter()).map(|(a, b)| ((a - b) as f64).powi(2)).sum();
        let voice_energy: f64 = voice.iter().map(|&v| (v as f64).powi(2)).sum();
        println!("diff={diff:.4} voice_energy={voice_energy:.4}");
        assert!(
            diff < voice_energy * 0.01,
            "expected mic to stay ~unchanged when there's no echo, diff={diff} energy={voice_energy}"
        );
    }
}
