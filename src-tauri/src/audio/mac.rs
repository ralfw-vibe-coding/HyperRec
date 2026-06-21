//! macOS implementation of `AudioProvider`. Device enumeration lands here
//! via cpal; recording itself combines a cpal input stream with a Core
//! Audio Process Tap for system audio (see project tasks).
//!
//! cpal's CoreAudio `Stream` is not `Send` (it owns an Objective-C
//! property-listener closure), but `AudioProvider` must be. So the stream
//! is built and lives entirely on one dedicated recorder thread; the
//! provider only ever holds a `Send` channel handle to that thread.

use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use super::mac_system_audio::SystemAudioRecorder;
use super::{
    AudioDevice, AudioError, AudioProvider, PermissionStatus, RecordingConfig, RecordingResult,
    Result,
};

type WavWriter = hound::WavWriter<BufWriter<File>>;

enum RecorderCommand {
    Pause(Sender<Result<()>>),
    Resume(Sender<Result<()>>),
    Stop(Sender<Result<RecordingResult>>),
}

struct RecorderHandle {
    control_tx: Sender<RecorderCommand>,
    join_handle: Option<JoinHandle<()>>,
}

impl RecorderHandle {
    fn send(&self, command: RecorderCommand) -> Result<()> {
        self.control_tx
            .send(command)
            .map_err(|_| AudioError::Backend("recorder thread is gone".into()))
    }
}

struct PendingSystemAudio {
    cancel: Arc<AtomicBool>,
    rx: Receiver<Result<SystemAudioRecorder>>,
}

#[derive(Default)]
pub struct MacAudioProvider {
    mic: Option<RecorderHandle>,
    system_audio: Option<SystemAudioRecorder>,
    pending_system_audio: Option<PendingSystemAudio>,
    final_path: Option<PathBuf>,
}

impl MacAudioProvider {
    pub fn new() -> Self {
        Self::default()
    }

    fn collect_ready_system_audio(&mut self) {
        let Some(pending) = self.pending_system_audio.take() else {
            return;
        };

        match pending.rx.try_recv() {
            Ok(Ok(recorder)) => {
                self.system_audio = Some(recorder);
            }
            Ok(Err(e)) => {
                eprintln!("HyperRec: system audio capture unavailable: {e}");
            }
            Err(TryRecvError::Empty) => {
                self.pending_system_audio = Some(pending);
            }
            Err(TryRecvError::Disconnected) => {}
        }
    }

    fn find_input_device(device_id: &Option<String>) -> Result<cpal::Device> {
        let host = cpal::default_host();
        match device_id {
            Some(id) => host
                .input_devices()
                .map_err(|e| AudioError::Backend(e.to_string()))?
                .find(|d| d.name().map(|n| n == *id).unwrap_or(false))
                .ok_or_else(|| AudioError::DeviceNotFound(id.clone())),
            None => host
                .default_input_device()
                .ok_or_else(|| AudioError::DeviceNotFound("no default input device".into())),
        }
    }
}

fn stop_and_discard_system_audio(recorder: Option<SystemAudioRecorder>) {
    if let Some(recorder) = recorder {
        if let Ok(result) = recorder.stop() {
            std::fs::remove_file(result.temp_file_path).ok();
        }
    }
}

fn start_system_audio_in_background(
    temp_file_path: PathBuf,
    output_device_uid: Option<String>,
) -> PendingSystemAudio {
    let (tx, rx) = channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_for_thread = cancel.clone();

    std::thread::Builder::new()
        .name("hyperrec-system-audio-starter".into())
        .spawn(move || match SystemAudioRecorder::start(temp_file_path, output_device_uid) {
            Ok(recorder) if cancel_for_thread.load(Ordering::Relaxed) => {
                stop_and_discard_system_audio(Some(recorder));
            }
            Ok(recorder) => {
                if let Err(err) = tx.send(Ok(recorder)) {
                    if let Ok(recorder) = err.0 {
                        stop_and_discard_system_audio(Some(recorder));
                    }
                }
            }
            Err(e) => {
                let _ = tx.send(Err(e));
            }
        })
        .ok();

    PendingSystemAudio { cancel, rx }
}

// cpal has no stable hardware device ID on macOS without dropping to Core
// Audio directly, so the device name doubles as id for V1. Names are
// unique per host in practice (System Default vs. concrete devices).
fn to_audio_device(device: &cpal::Device, default_name: Option<&str>) -> Result<AudioDevice> {
    let name = device
        .name()
        .map_err(|e| AudioError::Backend(e.to_string()))?;
    let is_default = default_name == Some(name.as_str());
    Ok(AudioDevice {
        id: name.clone(),
        name,
        is_default,
    })
}

/// Builds the cpal input stream, plays it, and then blocks handling
/// Pause/Resume/Stop until told to stop. Everything cpal-related is
/// created and dropped on this thread, never crossing into another one.
fn run_recorder(
    device: cpal::Device,
    stream_config: cpal::StreamConfig,
    sample_format: cpal::SampleFormat,
    temp_file_path: PathBuf,
    spec: hound::WavSpec,
    ready_tx: Sender<Result<()>>,
    control_rx: std::sync::mpsc::Receiver<RecorderCommand>,
) {
    let writer: WavWriter = match hound::WavWriter::create(&temp_file_path, spec) {
        Ok(w) => w,
        Err(e) => {
            let _ = ready_tx.send(Err(AudioError::Backend(e.to_string())));
            return;
        }
    };
    // `Option` so Stop can take the writer out under the lock instead of
    // needing `Arc::try_unwrap` (which demands the audio callback's clone
    // be gone first — a real race, since cpal's stream teardown does not
    // guarantee the last in-flight callback has returned by the time
    // `drop(stream)` below returns).
    let writer = Arc::new(Mutex::new(Some(writer)));
    let writer_for_stream = writer.clone();

    let samples_written = Arc::new(AtomicU64::new(0));
    let samples_for_stream = samples_written.clone();

    // Pausing toggles this flag instead of calling cpal's stream.pause()/
    // play() again later: restarting a Bluetooth headset's input unit
    // (e.g. re-negotiating the SCO link) can block for a long time or
    // hang outright. Leaving the stream running and just skipping writes
    // is the reliable option and gives the same "no silence while
    // paused" result.
    let paused = Arc::new(AtomicBool::new(false));
    let paused_for_stream = paused.clone();

    let err_fn = |err: cpal::StreamError| eprintln!("HyperRec: input stream error: {err}");

    let stream = if sample_format == cpal::SampleFormat::F32 {
        device.build_input_stream(
            &stream_config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                if paused_for_stream.load(Ordering::Relaxed) {
                    return;
                }
                if let Ok(mut guard) = writer_for_stream.lock() {
                    if let Some(w) = guard.as_mut() {
                        for &sample in data {
                            let _ = w.write_sample(sample);
                        }
                    }
                }
                samples_for_stream.fetch_add(data.len() as u64, Ordering::Relaxed);
            },
            err_fn,
            None,
        )
    } else {
        let _ = ready_tx.send(Err(AudioError::Backend(format!(
            "unsupported input sample format: {sample_format:?}"
        ))));
        return;
    };

    let stream = match stream {
        Ok(s) => s,
        Err(e) => {
            let _ = ready_tx.send(Err(AudioError::Backend(e.to_string())));
            return;
        }
    };

    if let Err(e) = stream.play() {
        let _ = ready_tx.send(Err(AudioError::Backend(e.to_string())));
        return;
    }

    if ready_tx.send(Ok(())).is_err() {
        return;
    }

    let channels = stream_config.channels;
    let sample_rate = stream_config.sample_rate.0;

    while let Ok(command) = control_rx.recv() {
        match command {
            RecorderCommand::Pause(reply) => {
                paused.store(true, Ordering::Relaxed);
                let _ = reply.send(Ok(()));
            }
            RecorderCommand::Resume(reply) => {
                paused.store(false, Ordering::Relaxed);
                let _ = reply.send(Ok(()));
            }
            RecorderCommand::Stop(reply) => {
                drop(stream);
                let samples = samples_written.load(Ordering::Relaxed);
                let duration_seconds = samples as f64 / channels as f64 / sample_rate as f64;

                let result = writer
                    .lock()
                    .map_err(|_| AudioError::Backend("wav writer mutex poisoned".into()))
                    .and_then(|mut guard| {
                        guard
                            .take()
                            .ok_or_else(|| AudioError::Backend("wav writer already taken".into()))
                    })
                    .and_then(|w| w.finalize().map_err(|e| AudioError::Backend(e.to_string())))
                    .map(|_| RecordingResult {
                        temp_file_path: temp_file_path.clone(),
                        duration_seconds,
                    });
                let _ = reply.send(result);
                return;
            }
        }
    }
}

impl AudioProvider for MacAudioProvider {
    fn list_input_devices(&self) -> Result<Vec<AudioDevice>> {
        let host = cpal::default_host();
        let default_name = host.default_input_device().and_then(|d| d.name().ok());
        let devices = host
            .input_devices()
            .map_err(|e| AudioError::Backend(e.to_string()))?;
        devices
            .map(|d| to_audio_device(&d, default_name.as_deref()))
            .collect()
    }

    fn list_output_devices(&self) -> Result<Vec<AudioDevice>> {
        let host = cpal::default_host();
        let default_name = host.default_output_device().and_then(|d| d.name().ok());
        super::mac_system_audio::list_coreaudio_output_devices(default_name.as_deref())
    }

    fn default_input_device(&self) -> Result<AudioDevice> {
        let device = cpal::default_host()
            .default_input_device()
            .ok_or_else(|| AudioError::DeviceNotFound("no default input device".into()))?;
        let mut audio_device = to_audio_device(&device, None)?;
        audio_device.is_default = true;
        Ok(audio_device)
    }

    fn default_output_device(&self) -> Result<AudioDevice> {
        let device = cpal::default_host()
            .default_output_device()
            .ok_or_else(|| AudioError::DeviceNotFound("no default output device".into()))?;
        let mut audio_device = to_audio_device(&device, None)?;
        audio_device.is_default = true;
        Ok(audio_device)
    }

    fn check_permissions(&self) -> Result<PermissionStatus> {
        Ok(PermissionStatus::NotDetermined)
    }

    fn request_permissions(&self) -> Result<PermissionStatus> {
        Ok(PermissionStatus::NotDetermined)
    }

    fn start_recording(&mut self, config: RecordingConfig) -> Result<()> {
        if self.mic.is_some() {
            return Err(AudioError::InvalidState("recording already active".into()));
        }

        std::fs::create_dir_all(&config.temp_dir)?;

        let mut system_audio = None;

        let device = match Self::find_input_device(&config.input_device_id) {
            Ok(device) => device,
            Err(e) => {
                stop_and_discard_system_audio(system_audio.take());
                return Err(e);
            }
        };
        let supported_config = match device.default_input_config() {
            Ok(config) => config,
            Err(e) => {
                stop_and_discard_system_audio(system_audio.take());
                return Err(AudioError::Backend(e.to_string()));
            }
        };
        let sample_format = supported_config.sample_format();
        let stream_config: cpal::StreamConfig = supported_config.into();

        let mic_temp_path = super::mixer::timestamped_path(&config.temp_dir, "mic");

        let spec = hound::WavSpec {
            channels: stream_config.channels,
            sample_rate: stream_config.sample_rate.0,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };

        let (ready_tx, ready_rx) = channel();
        let (control_tx, control_rx) = channel();

        let join_handle = std::thread::Builder::new()
            .name("hyperrec-mic-recorder".into())
            .spawn(move || {
                run_recorder(
                    device,
                    stream_config,
                    sample_format,
                    mic_temp_path,
                    spec,
                    ready_tx,
                    control_rx,
                )
            })
            .map_err(|e| {
                stop_and_discard_system_audio(system_audio.take());
                AudioError::Backend(e.to_string())
            })?;

        match ready_rx
            .recv()
            .map_err(|_| AudioError::Backend("recorder thread did not start".into()))
            .and_then(|result| result)
        {
            Ok(()) => {}
            Err(e) => {
                let _ = join_handle.join();
                stop_and_discard_system_audio(system_audio.take());
                return Err(e);
            }
        }

        self.mic = Some(RecorderHandle {
            control_tx,
            join_handle: Some(join_handle),
        });
        self.system_audio = system_audio.take();
        self.pending_system_audio = Some(start_system_audio_in_background(
            super::mixer::timestamped_path(&config.temp_dir, "system"),
            config.output_device_id.clone(),
        ));

        self.final_path = Some(super::mixer::timestamped_path(&config.temp_dir, "hyperrec"));
        Ok(())
    }

    fn pause_recording(&mut self) -> Result<()> {
        self.collect_ready_system_audio();
        let mic = self
            .mic
            .as_ref()
            .ok_or_else(|| AudioError::InvalidState("no active recording".into()))?;
        let (reply_tx, reply_rx) = channel();
        mic.send(RecorderCommand::Pause(reply_tx))?;
        reply_rx
            .recv()
            .map_err(|_| AudioError::Backend("recorder thread did not reply".into()))??;

        if let Some(system_audio) = &self.system_audio {
            system_audio.pause()?;
        }
        Ok(())
    }

    fn resume_recording(&mut self) -> Result<()> {
        self.collect_ready_system_audio();
        let mic = self
            .mic
            .as_ref()
            .ok_or_else(|| AudioError::InvalidState("no active recording".into()))?;
        let (reply_tx, reply_rx) = channel();
        mic.send(RecorderCommand::Resume(reply_tx))?;
        reply_rx
            .recv()
            .map_err(|_| AudioError::Backend("recorder thread did not reply".into()))??;

        if let Some(system_audio) = &self.system_audio {
            system_audio.resume()?;
        }
        Ok(())
    }

    fn stop_recording(&mut self) -> Result<RecordingResult> {
        self.collect_ready_system_audio();
        let mut mic = self
            .mic
            .take()
            .ok_or_else(|| AudioError::InvalidState("no active recording".into()))?;

        let mic_outcome: Result<RecordingResult> = (|| {
            let (reply_tx, reply_rx) = channel();
            mic.send(RecorderCommand::Stop(reply_tx))?;
            reply_rx
                .recv()
                .map_err(|_| AudioError::Backend("recorder thread did not reply".into()))?
        })();
        if let Some(handle) = mic.join_handle.take() {
            let _ = handle.join();
        }

        if let Some(pending) = self.pending_system_audio.take() {
            pending.cancel.store(true, Ordering::Relaxed);
            match pending.rx.recv_timeout(Duration::from_millis(1500)) {
                Ok(Ok(recorder)) => {
                    eprintln!("HyperRec: system audio recorder became ready before stop");
                    self.system_audio = Some(recorder);
                }
                Ok(Err(e)) => {
                    eprintln!("HyperRec: system audio capture unavailable: {e}");
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    eprintln!("HyperRec: system audio recorder was not ready before stop");
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    eprintln!("HyperRec: system audio starter disappeared before stop");
                }
            }
        }

        // Always tear down system audio, even if the mic failed, so its
        // recorder thread (tap + aggregate device) is never left running.
        let system_outcome = self.system_audio.take().map(|r| r.stop());

        let mic_result = mic_outcome?;

        let system_path = match system_outcome {
            Some(Ok(result)) => Some(result.temp_file_path),
            Some(Err(e)) => {
                eprintln!("HyperRec: stopping system audio failed, keeping microphone-only recording: {e}");
                None
            }
            None => None,
        };

        let final_path = self
            .final_path
            .take()
            .ok_or_else(|| AudioError::Backend("missing output path".into()))?;
        super::mixer::mix_down(
            &mic_result.temp_file_path,
            system_path.as_deref(),
            &final_path,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_microphone_to_wav_and_respects_pause() {
        let mut provider = MacAudioProvider::new();
        let temp_dir = std::env::temp_dir().join("hyperrec_test");
        let config = RecordingConfig {
            input_device_id: None,
            output_device_id: None,
            temp_dir: temp_dir.clone(),
        };

        provider
            .start_recording(config)
            .expect("start_recording failed");
        std::thread::sleep(std::time::Duration::from_millis(500));
        provider.pause_recording().expect("pause_recording failed");
        std::thread::sleep(std::time::Duration::from_millis(300));
        provider
            .resume_recording()
            .expect("resume_recording failed");
        std::thread::sleep(std::time::Duration::from_millis(500));
        let result = provider.stop_recording().expect("stop_recording failed");

        assert!(result.temp_file_path.exists(), "wav file should exist");
        assert!(result.duration_seconds > 0.0, "duration should be > 0");

        let reader =
            hound::WavReader::open(&result.temp_file_path).expect("wav should be readable");
        let spec = reader.spec();
        assert!(spec.sample_rate > 0);
        println!(
            "recorded {} seconds, {} channels, {} Hz, file: {:?}",
            result.duration_seconds, spec.channels, spec.sample_rate, result.temp_file_path
        );

        std::fs::remove_file(&result.temp_file_path).ok();
    }

    #[test]
    fn mixes_microphone_and_system_audio_into_one_file() {
        let mut provider = MacAudioProvider::new();
        let temp_dir = std::env::temp_dir().join("hyperrec_mix_test");
        let config = RecordingConfig {
            input_device_id: None,
            output_device_id: None,
            temp_dir: temp_dir.clone(),
        };

        provider
            .start_recording(config)
            .expect("start_recording failed");
        assert!(
            provider.system_audio.is_some(),
            "system audio should have started"
        );

        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(200));
            std::process::Command::new("afplay")
                .arg("/System/Library/Sounds/Ping.aiff")
                .status()
                .ok();
        });
        std::thread::sleep(std::time::Duration::from_millis(1200));

        let result = provider.stop_recording().expect("stop_recording failed");
        assert!(result.temp_file_path.exists());

        // The two raw per-source temp files must be gone; only the mixed
        // output remains, so the app leaves no extra files behind.
        let leftovers: Vec<_> = std::fs::read_dir(&temp_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        assert_eq!(leftovers, vec![result.temp_file_path.clone()]);

        let mut reader =
            hound::WavReader::open(&result.temp_file_path).expect("mixed wav should be readable");
        let spec = reader.spec();
        assert_eq!(spec.sample_rate, 48_000);
        assert_eq!(spec.bits_per_sample, 16);
        let samples: Vec<i16> = reader.samples::<i16>().map(|s| s.unwrap()).collect();
        let max_amplitude = samples.iter().map(|&s| s.unsigned_abs()).max().unwrap_or(0);
        println!("mixed file: {:?}, max amplitude {}", spec, max_amplitude);
        assert!(
            max_amplitude > 1000,
            "expected audible signal in mixed output, got {max_amplitude}"
        );

        std::fs::remove_file(&result.temp_file_path).ok();
    }

    #[test]
    fn survives_repeated_pause_resume_cycles() {
        let mut provider = MacAudioProvider::new();
        let temp_dir = std::env::temp_dir().join("hyperrec_repeat_test");
        let config = RecordingConfig {
            input_device_id: None,
            output_device_id: None,
            temp_dir: temp_dir.clone(),
        };

        provider
            .start_recording(config)
            .expect("start_recording failed");

        for i in 0..4 {
            std::thread::sleep(std::time::Duration::from_millis(150));
            provider
                .pause_recording()
                .unwrap_or_else(|e| panic!("pause #{i} failed: {e}"));
            std::thread::sleep(std::time::Duration::from_millis(100));
            provider
                .resume_recording()
                .unwrap_or_else(|e| panic!("resume #{i} failed: {e}"));
        }

        std::thread::sleep(std::time::Duration::from_millis(150));
        let result = provider.stop_recording().expect("stop_recording failed");
        assert!(result.temp_file_path.exists());
        std::fs::remove_file(&result.temp_file_path).ok();
    }
}
