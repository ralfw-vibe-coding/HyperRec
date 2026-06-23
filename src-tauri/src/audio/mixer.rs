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
const TARGET_RMS: f32 = 0.07;

fn signal_stats_f32(samples: &[f32]) -> (f32, f64) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }

    let mut peak = 0.0f32;
    let mut sum = 0.0f64;
    for &sample in samples {
        let abs = sample.abs();
        if abs > peak {
            peak = abs;
        }
        sum += sample as f64 * sample as f64;
    }
    (peak, (sum / samples.len() as f64).sqrt())
}

fn signal_stats_i16(samples: &[i16]) -> (i16, f64) {
    if samples.is_empty() {
        return (0, 0.0);
    }

    let mut peak = 0i16;
    let mut sum = 0.0f64;
    for &sample in samples {
        let abs = sample.saturating_abs();
        if abs > peak {
            peak = abs;
        }
        let normalized = sample as f64 / i16::MAX as f64;
        sum += normalized * normalized;
    }
    (peak, (sum / samples.len() as f64).sqrt())
}

fn read_mono_f32(path: &Path) -> Result<(Vec<f32>, u32)> {
    let mut reader =
        hound::WavReader::open(path).map_err(|e| AudioError::Backend(e.to_string()))?;
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
/// have the system signal digitally, estimate the acoustic echo path and
/// subtract it from the mic before mixing.
///
/// MacBook speakers are not just "one delayed copy": CoreAudio, speaker
/// DSP, room reflections, and the microphone response smear the signal.
/// So this uses a small frame-wise FIR model around the detected delay
/// rather than the earlier single delay/gain estimate.
fn cancel_acoustic_echo(mic: &mut [f32], system: &[f32], sample_rate: u32) {
    const MAX_DELAY_MS: f64 = 120.0;
    const ANALYSIS_SECONDS: f64 = 4.0;
    const DELAY_SEARCH_DECIMATION: usize = 8;
    const MIN_DELAY_CORRELATION: f64 = 0.08;
    const MIN_FRAME_EXPLAINED_CORRELATION: f64 = 0.12;

    let max_delay = ((sample_rate as f64 * MAX_DELAY_MS) / 1000.0) as usize;
    let analysis_len = ((sample_rate as f64 * ANALYSIS_SECONDS) as usize)
        .min(mic.len())
        .min(system.len());
    if analysis_len == 0 || max_delay == 0 || analysis_len <= max_delay * 2 {
        return;
    }

    let (best_delay, best_corr) = estimate_echo_delay(
        &mic[..analysis_len],
        &system[..analysis_len],
        max_delay,
        DELAY_SEARCH_DECIMATION,
    );
    if best_corr.abs() < MIN_DELAY_CORRELATION {
        return;
    }

    let reflection_offsets = reflection_offsets(sample_rate);
    let frame_len = ((sample_rate as f64 * 0.05) as usize).max(512);
    let hop_len = frame_len;

    let mut start = best_delay;
    while start < mic.len() {
        let end = (start + frame_len)
            .min(mic.len())
            .min(system.len() + best_delay);
        if end <= start {
            break;
        }
        subtract_echo_frame(
            mic,
            system,
            start,
            end,
            best_delay,
            &reflection_offsets,
            MIN_FRAME_EXPLAINED_CORRELATION,
        );
        start += hop_len;
    }
}

fn align_system_bleed_to_mic(system: Vec<f32>, mic: &[f32], sample_rate: u32) -> Vec<f32> {
    const MAX_START_OFFSET_SECONDS: f64 = 3.0;
    const ANALYSIS_SECONDS: f64 = 12.0;
    const DECIMATION: usize = 64;
    const MIN_ALIGNMENT_CORRELATION: f64 = 0.05;

    let max_lag = ((sample_rate as f64 * MAX_START_OFFSET_SECONDS) as usize).min(system.len());
    let analysis_len = ((sample_rate as f64 * ANALYSIS_SECONDS) as usize)
        .min(mic.len())
        .min(system.len());
    if analysis_len < sample_rate as usize || max_lag == 0 {
        return system;
    }

    let (lag, corr) = estimate_start_delay(
        &mic[..analysis_len],
        &system[..analysis_len],
        max_lag,
        DECIMATION,
    );
    if corr.abs() < MIN_ALIGNMENT_CORRELATION {
        return system;
    }

    if lag > 0 {
        eprintln!(
            "HyperRec: aligning system audio later by {:.0}ms for acoustic echo cancellation",
            lag as f64 * 1000.0 / sample_rate as f64
        );
        let mut aligned = vec![0.0; lag as usize];
        aligned.extend(system);
        aligned
    } else {
        system
    }
}

fn estimate_start_delay(
    mic: &[f32],
    system: &[f32],
    max_lag: usize,
    decimation: usize,
) -> (usize, f64) {
    let decimation = decimation.max(1);
    let mic_len = mic.len() / decimation;
    let system_len = system.len() / decimation;
    let max_lag = max_lag / decimation;
    if mic_len == 0 || system_len == 0 {
        return (0, 0.0);
    }
    let window = mic_len.min(system_len).saturating_sub(max_lag);
    if window < 1024 {
        return (0, 0.0);
    }

    let mut best_lag = 0usize;
    let mut best_norm_corr = 0.0f64;

    for lag in 0..=max_lag {
        let mic_start = lag;
        let system_start = 0usize;
        let mut corr = 0.0f64;
        let mut energy_mic = 0.0f64;
        let mut energy_sys = 0.0f64;
        for n in 0..window {
            let m = mic[(mic_start + n) * decimation] as f64;
            let s = system[(system_start + n) * decimation] as f64;
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
            best_lag = lag;
        }
    }

    (best_lag * decimation, best_norm_corr)
}

fn estimate_echo_delay(
    mic: &[f32],
    system: &[f32],
    max_delay: usize,
    decimation: usize,
) -> (usize, f64) {
    let decimation = decimation.max(1);
    let max_delay_decimated = max_delay / decimation;
    let len_decimated = mic.len().min(system.len()) / decimation;
    if len_decimated <= max_delay_decimated + 1 {
        return (0, 0.0);
    }

    let mut best_delay = 0usize;
    let mut best_norm_corr = 0.0f64;

    for delay in 0..=max_delay_decimated {
        let mut corr = 0.0f64;
        let mut energy_mic = 0.0f64;
        let mut energy_sys = 0.0f64;
        for i in delay..len_decimated {
            let m = mic[i * decimation] as f64;
            let s = system[(i - delay) * decimation] as f64;
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
            best_delay = delay * decimation;
        }
    }

    (best_delay, best_norm_corr)
}

fn reflection_offsets(sample_rate: u32) -> Vec<usize> {
    [0.0, 0.5, 1.0, 2.0, 4.0, 8.0, 16.0, 32.0]
        .iter()
        .map(|ms| ((sample_rate as f64 * ms) / 1000.0).round() as usize)
        .collect()
}

fn subtract_echo_frame(
    mic: &mut [f32],
    system: &[f32],
    start: usize,
    end: usize,
    base_delay: usize,
    offsets: &[usize],
    min_explained_correlation: f64,
) {
    const RIDGE: f64 = 1e-5;
    const MAX_COEFFICIENT: f64 = 2.0;

    let n = offsets.len();
    if n == 0 {
        return;
    }

    let mut gram = vec![vec![0.0f64; n]; n];
    let mut rhs = vec![0.0f64; n];
    let mut mic_energy = 0.0f64;

    for i in start..end {
        let m = mic[i] as f64;
        mic_energy += m * m;
        for (a, &offset_a) in offsets.iter().enumerate() {
            let Some(sa) = i
                .checked_sub(base_delay + offset_a)
                .and_then(|idx| system.get(idx))
                .map(|&s| s as f64)
            else {
                continue;
            };
            rhs[a] += m * sa;
            for (b, &offset_b) in offsets.iter().enumerate() {
                if let Some(sb) = i
                    .checked_sub(base_delay + offset_b)
                    .and_then(|idx| system.get(idx))
                    .map(|&s| s as f64)
                {
                    gram[a][b] += sa * sb;
                }
            }
        }
    }

    if mic_energy < 1e-9 {
        return;
    }

    for (i, row) in gram.iter_mut().enumerate() {
        row[i] += RIDGE;
    }

    let Some(mut coefficients) = solve_linear_system(gram, rhs) else {
        return;
    };
    for coefficient in &mut coefficients {
        *coefficient = coefficient.clamp(-MAX_COEFFICIENT, MAX_COEFFICIENT);
    }

    let mut echo_energy = 0.0f64;
    let mut mic_echo_dot = 0.0f64;
    let mut echo = vec![0.0f64; end - start];
    for (out_idx, i) in (start..end).enumerate() {
        let mut predicted = 0.0f64;
        for (&coefficient, &offset) in coefficients.iter().zip(offsets.iter()) {
            if let Some(reference) = i
                .checked_sub(base_delay + offset)
                .and_then(|idx| system.get(idx))
            {
                predicted += coefficient * *reference as f64;
            }
        }
        echo[out_idx] = predicted;
        echo_energy += predicted * predicted;
        mic_echo_dot += mic[i] as f64 * predicted;
    }

    if echo_energy < 1e-9 {
        return;
    }
    let explained_corr = mic_echo_dot.abs() / (mic_energy.sqrt() * echo_energy.sqrt());
    if explained_corr < min_explained_correlation {
        return;
    }

    for (out_idx, i) in (start..end).enumerate() {
        mic[i] -= echo[out_idx] as f32;
    }
}

fn solve_linear_system(mut a: Vec<Vec<f64>>, mut b: Vec<f64>) -> Option<Vec<f64>> {
    let n = b.len();
    for col in 0..n {
        let pivot = (col..n).max_by(|&r1, &r2| {
            a[r1][col]
                .abs()
                .partial_cmp(&a[r2][col].abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })?;
        if a[pivot][col].abs() < 1e-12 {
            return None;
        }
        if pivot != col {
            a.swap(pivot, col);
            b.swap(pivot, col);
        }

        let pivot_value = a[col][col];
        for c in col..n {
            a[col][c] /= pivot_value;
        }
        b[col] /= pivot_value;

        for r in 0..n {
            if r == col {
                continue;
            }
            let factor = a[r][col];
            if factor.abs() < 1e-15 {
                continue;
            }
            for c in col..n {
                a[r][c] -= factor * a[col][c];
            }
            b[r] -= factor * b[col];
        }
    }

    Some(b)
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    ((sum_sq / samples.len() as f64).sqrt()) as f32
}

fn has_audible_signal(samples: &[f32]) -> bool {
    rms(samples) >= 1e-4 || samples.iter().any(|s| s.abs() >= 1e-3)
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
    let mut writer =
        hound::WavWriter::create(path, spec).map_err(|e| AudioError::Backend(e.to_string()))?;
    for &s in samples {
        let clamped = (s * 0.85).tanh().clamp(-1.0, 1.0);
        writer
            .write_sample((clamped * i16::MAX as f32) as i16)
            .map_err(|e| AudioError::Backend(e.to_string()))?;
    }
    writer
        .finalize()
        .map_err(|e| AudioError::Backend(e.to_string()))
}

/// Mixes the mic track with the system-audio track (if any) into
/// `output_path` as a 48kHz/16-bit mono WAV, deleting the two raw input
/// files afterwards. Falls back to a plain resample of the mic track
/// when there is no system audio to mix in (e.g. it failed to start).
pub fn mix_down(
    mic_path: &Path,
    system_path: Option<&Path>,
    output_path: &Path,
) -> Result<RecordingResult> {
    let (mic_samples, mic_rate) = read_mono_f32(mic_path)?;
    let (mic_peak, mic_rms) = signal_stats_f32(&mic_samples);
    eprintln!(
        "HyperRec: mic wav {:?}: samples={}, rate={}, peak={:.6}, rms={:.6}",
        mic_path,
        mic_samples.len(),
        mic_rate,
        mic_peak,
        mic_rms
    );
    let mut mic_samples = resample_linear(&mic_samples, mic_rate, TARGET_SAMPLE_RATE);

    let mixed = match system_path {
        Some(system_path) => {
            let (system_samples, system_rate) = read_mono_f32(system_path)?;
            let (system_peak, system_rms) = signal_stats_f32(&system_samples);
            eprintln!(
                "HyperRec: system wav {:?}: samples={}, rate={}, peak={:.6}, rms={:.6}",
                system_path,
                system_samples.len(),
                system_rate,
                system_peak,
                system_rms
            );
            let mut system_samples =
                resample_linear(&system_samples, system_rate, TARGET_SAMPLE_RATE);
            if !has_audible_signal(&system_samples) {
                eprintln!("HyperRec: system audio track was silent; using microphone only");
                return write_mic_only(mic_path, Some(system_path), output_path, mic_samples);
            }

            system_samples =
                align_system_bleed_to_mic(system_samples, &mic_samples, TARGET_SAMPLE_RATE);
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
        None => {
            normalize_to_rms(&mut mic_samples, TARGET_RMS);
            mic_samples
        }
    };
    let (mixed_peak, mixed_rms) = signal_stats_f32(&mixed);
    eprintln!(
        "HyperRec: mixed wav {:?}: samples={}, peak={:.6}, rms={:.6}",
        output_path,
        mixed.len(),
        mixed_peak,
        mixed_rms
    );

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

fn write_mic_only(
    mic_path: &Path,
    system_path: Option<&Path>,
    output_path: &Path,
    mut mic_samples: Vec<f32>,
) -> Result<RecordingResult> {
    normalize_to_rms(&mut mic_samples, TARGET_RMS);
    write_mono_i16_wav(output_path, &mic_samples, TARGET_SAMPLE_RATE)?;
    let (mic_peak, mic_rms) = signal_stats_f32(&mic_samples);
    eprintln!(
        "HyperRec: mic-only wav {:?}: samples={}, peak={:.6}, rms={:.6}",
        output_path,
        mic_samples.len(),
        mic_peak,
        mic_rms
    );
    std::fs::remove_file(mic_path).ok();
    if let Some(system_path) = system_path {
        std::fs::remove_file(system_path).ok();
    }
    Ok(RecordingResult {
        temp_file_path: output_path.to_path_buf(),
        duration_seconds: mic_samples.len() as f64 / TARGET_SAMPLE_RATE as f64,
    })
}

/// Encodes the final mono WAV (produced by `mix_down`) to an MP3 at a
/// fixed 96kbps — small enough to matter for multi-hour meeting
/// recordings, plenty for spoken voice. Runs only on "Download", so the
/// lossless WAV stays the source of truth for repeated saves.
pub fn encode_wav_to_mp3(wav_path: &Path, mp3_path: &Path) -> Result<()> {
    use mp3lame_encoder::{Bitrate, Builder, FlushNoGap, MonoPcm, Quality};

    let mut reader =
        hound::WavReader::open(wav_path).map_err(|e| AudioError::Backend(e.to_string()))?;
    let sample_rate = reader.spec().sample_rate;
    let samples: Vec<i16> = reader
        .samples::<i16>()
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| AudioError::Backend(e.to_string()))?;
    let (peak, rms) = signal_stats_i16(&samples);
    eprintln!(
        "HyperRec: encoding wav {:?} to mp3 {:?}: samples={}, rate={}, peak={}, rms={:.6}",
        wav_path,
        mp3_path,
        samples.len(),
        sample_rate,
        peak,
        rms
    );

    let builder = Builder::new()
        .ok_or_else(|| AudioError::Backend("could not create LAME encoder".to_string()))?;
    let builder = builder
        .with_num_channels(1)
        .map_err(|e| AudioError::Backend(e.to_string()))?;
    let builder = builder
        .with_sample_rate(sample_rate)
        .map_err(|e| AudioError::Backend(e.to_string()))?;
    let builder = builder
        .with_brate(Bitrate::Kbps96)
        .map_err(|e| AudioError::Backend(e.to_string()))?;
    let builder = builder
        .with_quality(Quality::Best)
        .map_err(|e| AudioError::Backend(e.to_string()))?;
    let mut encoder = builder
        .build()
        .map_err(|e| AudioError::Backend(e.to_string()))?;

    let mut mp3_data = Vec::with_capacity(mp3lame_encoder::max_required_buffer_size(samples.len()));
    let encoded = encoder
        .encode(MonoPcm(&samples), mp3_data.spare_capacity_mut())
        .map_err(|e| AudioError::Backend(e.to_string()))?;
    unsafe { mp3_data.set_len(mp3_data.len() + encoded) };

    mp3_data.reserve(7200);
    let flushed = encoder
        .flush::<FlushNoGap>(mp3_data.spare_capacity_mut())
        .map_err(|e| AudioError::Backend(e.to_string()))?;
    unsafe { mp3_data.set_len(mp3_data.len() + flushed) };

    std::fs::write(mp3_path, &mp3_data).map_err(AudioError::Io)
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
    fn make_complex_tone(
        len: usize,
        sample_rate: u32,
        freqs_hz: &[f32],
        amplitude: f32,
    ) -> Vec<f32> {
        let scale = amplitude / freqs_hz.len() as f32;
        (0..len)
            .map(|i| {
                freqs_hz
                    .iter()
                    .map(|&f| {
                        (2.0 * std::f32::consts::PI * f * i as f32 / sample_rate as f32).sin()
                    })
                    .sum::<f32>()
                    * scale
            })
            .collect()
    }

    fn write_float_wav(path: &Path, samples: &[f32], sample_rate: u32) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        for sample in samples {
            writer.write_sample(*sample).unwrap();
        }
        writer.finalize().unwrap();
    }

    fn make_noise(len: usize, amplitude: f32) -> Vec<f32> {
        let mut state = 0x1234_5678_9abc_def0u64;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let unit = ((state >> 40) as f32 / 0x00ff_ffffu32 as f32) * 2.0 - 1.0;
                unit * amplitude
            })
            .collect()
    }

    fn read_i16_wav(path: &Path) -> Vec<f32> {
        let mut reader = hound::WavReader::open(path).unwrap();
        reader
            .samples::<i16>()
            .map(|sample| sample.unwrap() as f32 / i16::MAX as f32)
            .collect()
    }

    #[test]
    fn cancel_acoustic_echo_removes_known_delayed_copy() {
        let sample_rate = 48_000u32;
        let len = sample_rate as usize * 2;
        let system = make_complex_tone(
            len,
            sample_rate,
            &[311.0, 547.0, 877.0, 1231.0, 1607.0, 1999.0],
            0.4,
        );
        let voice = make_complex_tone(
            len,
            sample_rate,
            &[233.0, 397.0, 661.0, 919.0, 1373.0, 1777.0],
            0.3,
        );

        let delay = 200usize;
        let leak_gain = 0.35f32;
        let mut mic = voice.clone();
        for i in delay..len {
            mic[i] += leak_gain * system[i - delay];
        }

        let mic_before = mic.clone();
        cancel_acoustic_echo(&mut mic, &system, sample_rate);

        let error_before: f64 = voice
            .iter()
            .zip(mic_before.iter())
            .map(|(a, b)| ((a - b) as f64).powi(2))
            .sum();
        let error_after: f64 = voice
            .iter()
            .zip(mic.iter())
            .map(|(a, b)| ((a - b) as f64).powi(2))
            .sum();

        println!("error_before={error_before:.4} error_after={error_after:.4}");
        assert!(
            error_after < error_before * 0.1,
            "expected echo cancellation to reduce error by >90%, before={error_before}, after={error_after}"
        );
    }

    #[test]
    fn encode_wav_to_mp3_produces_a_valid_mp3_stream() {
        let sample_rate = 48_000u32;
        let len = sample_rate as usize / 2;
        let samples = make_complex_tone(len, sample_rate, &[440.0, 880.0], 0.3);

        let temp_dir =
            std::env::temp_dir().join(format!("hyperrec_mp3_test_{}", std::process::id()));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let wav_path = temp_dir.join("in.wav");
        let mp3_path = temp_dir.join("out.mp3");
        write_mono_i16_wav(&wav_path, &samples, sample_rate).unwrap();

        encode_wav_to_mp3(&wav_path, &mp3_path).unwrap();
        let mp3_bytes = std::fs::read(&mp3_path).unwrap();
        let wav_bytes = std::fs::metadata(&wav_path).unwrap().len();
        std::fs::remove_dir_all(&temp_dir).ok();

        assert!(
            mp3_bytes.starts_with(b"ID3")
                || (mp3_bytes.len() > 1 && mp3_bytes[0] == 0xFF && (mp3_bytes[1] & 0xE0) == 0xE0),
            "output does not start with an ID3 tag or MP3 frame sync"
        );
        assert!(
            (mp3_bytes.len() as u64) < wav_bytes / 3,
            "expected MP3 at 96kbps to be noticeably smaller than the 16-bit WAV, mp3={} wav={}",
            mp3_bytes.len(),
            wav_bytes
        );
    }

    #[test]
    fn cancel_acoustic_echo_leaves_clean_mic_alone_without_echo() {
        let sample_rate = 48_000u32;
        let len = sample_rate as usize * 2;
        let system = make_complex_tone(
            len,
            sample_rate,
            &[311.0, 547.0, 877.0, 1231.0, 1607.0, 1999.0],
            0.4,
        );
        let voice = make_complex_tone(
            len,
            sample_rate,
            &[233.0, 397.0, 661.0, 919.0, 1373.0, 1777.0],
            0.3,
        );
        let mut mic = voice.clone();

        cancel_acoustic_echo(&mut mic, &system, sample_rate);

        let diff: f64 = voice
            .iter()
            .zip(mic.iter())
            .map(|(a, b)| ((a - b) as f64).powi(2))
            .sum();
        let voice_energy: f64 = voice.iter().map(|&v| (v as f64).powi(2)).sum();
        println!("diff={diff:.4} voice_energy={voice_energy:.4}");
        assert!(
            diff < voice_energy * 0.01,
            "expected mic to stay ~unchanged when there's no echo, diff={diff} energy={voice_energy}"
        );
    }

    #[test]
    fn mix_down_aligns_late_system_capture_before_echo_cancellation() {
        let sample_rate = 48_000u32;
        let len = sample_rate as usize * 5;
        let system_start_offset = sample_rate as usize;
        let acoustic_delay = 240usize;
        let system = make_noise(len, 0.2);
        let voice = make_complex_tone(
            len + system_start_offset + acoustic_delay,
            sample_rate,
            &[233.0, 397.0, 661.0, 919.0, 1373.0, 1777.0],
            0.12,
        );

        let mut mic = voice.clone();
        for i in 0..system.len() {
            let mic_index = i + system_start_offset + acoustic_delay;
            if mic_index < mic.len() {
                mic[mic_index] += 0.45 * system[i];
            }
        }

        let temp_dir = std::env::temp_dir().join(format!(
            "hyperrec_mixer_align_test_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let mic_path = temp_dir.join("mic.wav");
        let system_path = temp_dir.join("system.wav");
        let output_path = temp_dir.join("mixed.wav");
        write_float_wav(&mic_path, &mic, sample_rate);
        write_float_wav(&system_path, &system, sample_rate);

        mix_down(&mic_path, Some(&system_path), &output_path).unwrap();

        let mixed = read_i16_wav(&output_path);
        let aligned_system = align_system_bleed_to_mic(system, &mic, sample_rate);
        assert!(
            (aligned_system.len() as isize
                - (len + system_start_offset + acoustic_delay) as isize)
                .abs()
                < sample_rate as isize / 8,
            "expected system track to be aligned close to the acoustic bleed"
        );
        let mut cleaned_mic = mic.clone();
        cancel_acoustic_echo(&mut cleaned_mic, &aligned_system, sample_rate);
        normalize_to_rms(&mut cleaned_mic, TARGET_RMS);
        let mut expected_system = aligned_system.clone();
        normalize_to_rms(&mut expected_system, TARGET_RMS);

        let len = cleaned_mic.len().min(expected_system.len()).min(mixed.len());
        let duplicate_energy: f64 = (0..len)
            .map(|i| {
                let expected = ((cleaned_mic[i] + expected_system[i]) * 0.85).tanh();
                let decoded = mixed[i];
                ((decoded - expected) as f64).powi(2)
            })
            .sum();
        let signal_energy: f64 = mixed.iter().map(|&s| (s as f64).powi(2)).sum();

        std::fs::remove_file(&output_path).ok();
        std::fs::remove_dir(&temp_dir).ok();

        assert!(
            duplicate_energy < signal_energy * 0.02,
            "expected aligned mix without large duplicate echo, duplicate={duplicate_energy}, signal={signal_energy}"
        );
    }
}
