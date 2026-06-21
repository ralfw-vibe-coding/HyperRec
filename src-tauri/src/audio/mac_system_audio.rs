//! System audio capture via Core Audio Process Taps (macOS 14.4+).
//!
//! Builds a private, global process tap (the whole system mix) plus a
//! private aggregate device that combines that tap with the system
//! default output device (used only as a hardware clock — its own audio
//! is never recorded). An Objective-C IO block delivers PCM buffers for
//! the aggregate device, which are written straight into a WAV file.
//!
//! None of the Core Audio / Objective-C types here (`RcBlock`, tap and
//! aggregate device ids tied to an open IO proc) are `Send`-safe to keep
//! alive across threads casually, so — exactly like the cpal mic
//! recorder in `mac.rs` — everything is built, used, and torn down on
//! one dedicated thread. Only a `Send` channel handle crosses out of it.

use std::ffi::c_void;
use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::AnyThread;
use objc2_core_audio::{
    kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceIsStackedKey,
    kAudioAggregateDeviceMainSubDeviceKey, kAudioAggregateDeviceNameKey,
    kAudioAggregateDeviceSubDeviceListKey, kAudioAggregateDeviceTapAutoStartKey,
    kAudioAggregateDeviceTapListKey, kAudioAggregateDeviceUIDKey, kAudioDevicePropertyDeviceUID,
    kAudioHardwarePropertyDefaultOutputDevice, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject, kAudioSubDeviceUIDKey,
    kAudioSubTapDriftCompensationKey, kAudioSubTapUIDKey, kAudioTapPropertyFormat,
    AudioDeviceCreateIOProcIDWithBlock, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID,
    AudioDeviceStart, AudioDeviceStop, AudioHardwareCreateAggregateDevice,
    AudioHardwareCreateProcessTap, AudioHardwareDestroyAggregateDevice,
    AudioHardwareDestroyProcessTap, AudioObjectGetPropertyData, AudioObjectPropertyAddress,
    CATapDescription,
};
use objc2_core_audio_types::{AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp};
use objc2_core_foundation::{CFArray, CFBoolean, CFDictionary, CFRetained, CFString, CFType};
use objc2_foundation::{NSArray, NSNumber, NSUUID};

use super::{AudioError, RecordingResult, Result};

type WavWriter = hound::WavWriter<BufWriter<File>>;

enum RecorderCommand {
    Pause(Sender<Result<()>>),
    Resume(Sender<Result<()>>),
    Stop(Sender<Result<RecordingResult>>),
}

pub struct SystemAudioRecorder {
    control_tx: Sender<RecorderCommand>,
    join_handle: Option<JoinHandle<()>>,
}

impl SystemAudioRecorder {
    pub fn start(temp_file_path: PathBuf) -> Result<Self> {
        let (ready_tx, ready_rx) = channel();
        let (control_tx, control_rx) = channel();

        let join_handle = std::thread::Builder::new()
            .name("hyperrec-system-audio-recorder".into())
            .spawn(move || run_recorder(temp_file_path, ready_tx, control_rx))
            .map_err(|e| AudioError::Backend(e.to_string()))?;

        ready_rx
            .recv()
            .map_err(|_| AudioError::Backend("system audio recorder thread did not start".into()))??;

        Ok(Self {
            control_tx,
            join_handle: Some(join_handle),
        })
    }

    fn send(&self, command: RecorderCommand) -> Result<()> {
        self.control_tx
            .send(command)
            .map_err(|_| AudioError::Backend("system audio recorder thread is gone".into()))
    }

    pub fn pause(&self) -> Result<()> {
        let (reply_tx, reply_rx) = channel();
        self.send(RecorderCommand::Pause(reply_tx))?;
        reply_rx
            .recv()
            .map_err(|_| AudioError::Backend("system audio recorder thread did not reply".into()))?
    }

    pub fn resume(&self) -> Result<()> {
        let (reply_tx, reply_rx) = channel();
        self.send(RecorderCommand::Resume(reply_tx))?;
        reply_rx
            .recv()
            .map_err(|_| AudioError::Backend("system audio recorder thread did not reply".into()))?
    }

    pub fn stop(mut self) -> Result<RecordingResult> {
        let (reply_tx, reply_rx) = channel();
        self.send(RecorderCommand::Stop(reply_tx))?;
        let result = reply_rx
            .recv()
            .map_err(|_| AudioError::Backend("system audio recorder thread did not reply".into()))?;
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
        result
    }
}

fn cfstr(s: &str) -> CFRetained<CFString> {
    CFString::from_str(s)
}

fn cft<T: AsRef<CFType> + ?Sized>(t: &T) -> &CFType {
    t.as_ref()
}

fn cfstr_from_cstr(s: &std::ffi::CStr) -> CFRetained<CFString> {
    cfstr(s.to_str().expect("Core Audio key constants are ASCII"))
}

unsafe fn get_u32_property(object_id: u32, selector: u32) -> std::result::Result<u32, i32> {
    let address = AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut size: u32 = std::mem::size_of::<u32>() as u32;
    let mut value: u32 = 0;
    let status = AudioObjectGetPropertyData(
        object_id,
        NonNull::from(&address),
        0,
        std::ptr::null(),
        NonNull::from(&mut size),
        NonNull::new(&mut value as *mut u32 as *mut c_void).unwrap(),
    );
    if status != 0 {
        return Err(status);
    }
    Ok(value)
}

unsafe fn get_device_uid(device_id: u32) -> std::result::Result<CFRetained<CFString>, i32> {
    let address = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyDeviceUID,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut size: u32 = std::mem::size_of::<*const c_void>() as u32;
    let mut raw: *const c_void = std::ptr::null();
    let status = AudioObjectGetPropertyData(
        device_id,
        NonNull::from(&address),
        0,
        std::ptr::null(),
        NonNull::from(&mut size),
        NonNull::new(&mut raw as *mut *const c_void as *mut c_void).unwrap(),
    );
    if status != 0 {
        return Err(status);
    }
    let ptr = NonNull::new(raw as *mut CFString).ok_or(-1)?;
    Ok(CFRetained::from_raw(ptr))
}

unsafe fn get_tap_format(tap_id: u32) -> std::result::Result<AudioStreamBasicDescription, i32> {
    let address = AudioObjectPropertyAddress {
        mSelector: kAudioTapPropertyFormat,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut size: u32 = std::mem::size_of::<AudioStreamBasicDescription>() as u32;
    let mut format = std::mem::MaybeUninit::<AudioStreamBasicDescription>::zeroed();
    let status = AudioObjectGetPropertyData(
        tap_id,
        NonNull::from(&address),
        0,
        std::ptr::null(),
        NonNull::from(&mut size),
        NonNull::new(format.as_mut_ptr() as *mut c_void).unwrap(),
    );
    if status != 0 {
        return Err(status);
    }
    Ok(format.assume_init())
}

/// Creates a private global tap + private aggregate device pair.
/// The aggregate's only purpose is to host the tap's input stream and to
/// borrow a real hardware clock from the system default output device.
unsafe fn create_tap_and_aggregate() -> std::result::Result<(u32, u32, String), String> {
    let exclude: Retained<NSArray<NSNumber>> = NSArray::from_slice(&[]);
    let description =
        CATapDescription::initMonoGlobalTapButExcludeProcesses(CATapDescription::alloc(), &exclude);
    description.setPrivate(true);
    let uuid = NSUUID::new();
    description.setUUID(&uuid);
    let tap_uid_string = uuid.UUIDString().to_string();

    let mut tap_id: u32 = 0;
    let status = AudioHardwareCreateProcessTap(Some(&description), &mut tap_id);
    if status != 0 {
        return Err(format!("AudioHardwareCreateProcessTap failed: OSStatus={status}"));
    }

    let output_device_id =
        match get_u32_property(kAudioObjectSystemObject as u32, kAudioHardwarePropertyDefaultOutputDevice) {
            Ok(id) => id,
            Err(e) => {
                AudioHardwareDestroyProcessTap(tap_id);
                return Err(format!("could not resolve default output device: OSStatus={e}"));
            }
        };
    let output_uid_string = match get_device_uid(output_device_id) {
        Ok(s) => s.to_string(),
        Err(e) => {
            AudioHardwareDestroyProcessTap(tap_id);
            return Err(format!("could not resolve default output device UID: OSStatus={e}"));
        }
    };

    let sub_device_dict = CFDictionary::from_slices(
        &[&*cfstr_from_cstr(kAudioSubDeviceUIDKey)],
        &[cft(&cfstr(&output_uid_string))],
    );
    let sub_device_list = CFArray::from_objects(&[&*sub_device_dict]);

    let tap_dict = CFDictionary::from_slices(
        &[
            &*cfstr_from_cstr(kAudioSubTapUIDKey),
            &*cfstr_from_cstr(kAudioSubTapDriftCompensationKey),
        ],
        &[cft(&cfstr(&tap_uid_string)), cft(CFBoolean::new(true))],
    );
    let tap_list = CFArray::from_objects(&[&*tap_dict]);

    let aggregate_uid = format!("com.zeitgewinn.hyperrec.system-audio-tap.{tap_uid_string}");
    let aggregate_description = CFDictionary::from_slices(
        &[
            &*cfstr_from_cstr(kAudioAggregateDeviceNameKey),
            &*cfstr_from_cstr(kAudioAggregateDeviceUIDKey),
            &*cfstr_from_cstr(kAudioAggregateDeviceIsPrivateKey),
            &*cfstr_from_cstr(kAudioAggregateDeviceIsStackedKey),
            &*cfstr_from_cstr(kAudioAggregateDeviceTapAutoStartKey),
            &*cfstr_from_cstr(kAudioAggregateDeviceMainSubDeviceKey),
            &*cfstr_from_cstr(kAudioAggregateDeviceSubDeviceListKey),
            &*cfstr_from_cstr(kAudioAggregateDeviceTapListKey),
        ],
        &[
            cft(&cfstr("HyperRec System Audio Tap")),
            cft(&cfstr(&aggregate_uid)),
            cft(CFBoolean::new(true)),
            cft(CFBoolean::new(false)),
            cft(CFBoolean::new(true)),
            cft(&cfstr(&output_uid_string)),
            cft(&sub_device_list),
            cft(&tap_list),
        ],
    );

    let mut aggregate_device_id: u32 = 0;
    let agg_status = AudioHardwareCreateAggregateDevice(
        aggregate_description.as_opaque(),
        NonNull::from(&mut aggregate_device_id),
    );
    if agg_status != 0 {
        AudioHardwareDestroyProcessTap(tap_id);
        return Err(format!("AudioHardwareCreateAggregateDevice failed: OSStatus={agg_status}"));
    }

    Ok((tap_id, aggregate_device_id, tap_uid_string))
}

fn run_recorder(temp_file_path: PathBuf, ready_tx: Sender<Result<()>>, control_rx: Receiver<RecorderCommand>) {
    unsafe { run_recorder_unsafe(temp_file_path, ready_tx, control_rx) }
}

unsafe fn run_recorder_unsafe(
    temp_file_path: PathBuf,
    ready_tx: Sender<Result<()>>,
    control_rx: Receiver<RecorderCommand>,
) {
    let (tap_id, aggregate_device_id, _tap_uid) = match create_tap_and_aggregate() {
        Ok(v) => v,
        Err(e) => {
            let _ = ready_tx.send(Err(AudioError::Backend(e)));
            return;
        }
    };

    let format = match get_tap_format(tap_id) {
        Ok(f) => f,
        Err(e) => {
            AudioHardwareDestroyAggregateDevice(aggregate_device_id);
            AudioHardwareDestroyProcessTap(tap_id);
            let _ = ready_tx.send(Err(AudioError::Backend(format!(
                "could not read tap format: OSStatus={e}"
            ))));
            return;
        }
    };

    let spec = hound::WavSpec {
        channels: format.mChannelsPerFrame as u16,
        sample_rate: format.mSampleRate as u32,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let writer: WavWriter = match hound::WavWriter::create(&temp_file_path, spec) {
        Ok(w) => w,
        Err(e) => {
            AudioHardwareDestroyAggregateDevice(aggregate_device_id);
            AudioHardwareDestroyProcessTap(tap_id);
            let _ = ready_tx.send(Err(AudioError::Backend(e.to_string())));
            return;
        }
    };
    // `Option` so Stop can take the writer out under the lock instead of
    // needing `Arc::try_unwrap` (which would race the block's own clone
    // if CoreAudio is still mid-callback when we tear the device down).
    let writer = Arc::new(Mutex::new(Some(writer)));
    let writer_for_block = writer.clone();
    let samples_written = Arc::new(AtomicU64::new(0));
    let samples_for_block = samples_written.clone();
    // Pausing toggles this flag instead of stopping/restarting the
    // aggregate device: repeated AudioDeviceStop/Start cycles on a
    // tap-backed aggregate device have proven flaky in practice, while
    // the device just idling with writes skipped is solid. Same
    // "no silence for paused time" outcome as the mic recorder.
    let paused = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let paused_for_block = paused.clone();

    let block = RcBlock::new(
        move |_now: NonNull<AudioTimeStamp>,
              input_data: NonNull<AudioBufferList>,
              _input_time: NonNull<AudioTimeStamp>,
              _output_data: NonNull<AudioBufferList>,
              _output_time: NonNull<AudioTimeStamp>| {
            if paused_for_block.load(Ordering::Relaxed) {
                return;
            }
            let list = input_data.as_ref();
            if list.mNumberBuffers == 0 {
                return;
            }
            let buffer = &list.mBuffers[0];
            if buffer.mData.is_null() {
                return;
            }
            let n_samples = buffer.mDataByteSize as usize / std::mem::size_of::<f32>();
            let samples = std::slice::from_raw_parts(buffer.mData as *const f32, n_samples);
            if let Ok(mut guard) = writer_for_block.lock() {
                if let Some(w) = guard.as_mut() {
                    for &sample in samples {
                        let _ = w.write_sample(sample);
                    }
                }
            }
            samples_for_block.fetch_add(n_samples as u64, Ordering::Relaxed);
        },
    );

    let mut io_proc_id: AudioDeviceIOProcID = None;
    let create_status = AudioDeviceCreateIOProcIDWithBlock(
        NonNull::from(&mut io_proc_id),
        aggregate_device_id,
        None,
        RcBlock::as_ptr(&block) as _,
    );
    if create_status != 0 {
        AudioHardwareDestroyAggregateDevice(aggregate_device_id);
        AudioHardwareDestroyProcessTap(tap_id);
        let _ = ready_tx.send(Err(AudioError::Backend(format!(
            "AudioDeviceCreateIOProcIDWithBlock failed: OSStatus={create_status}"
        ))));
        return;
    }

    let start_status = AudioDeviceStart(aggregate_device_id, io_proc_id);
    if start_status != 0 {
        AudioDeviceDestroyIOProcID(aggregate_device_id, io_proc_id);
        AudioHardwareDestroyAggregateDevice(aggregate_device_id);
        AudioHardwareDestroyProcessTap(tap_id);
        let _ = ready_tx.send(Err(AudioError::Backend(format!(
            "AudioDeviceStart failed: OSStatus={start_status}"
        ))));
        return;
    }

    if ready_tx.send(Ok(())).is_err() {
        AudioDeviceStop(aggregate_device_id, io_proc_id);
        AudioDeviceDestroyIOProcID(aggregate_device_id, io_proc_id);
        AudioHardwareDestroyAggregateDevice(aggregate_device_id);
        AudioHardwareDestroyProcessTap(tap_id);
        return;
    }

    let channels = format.mChannelsPerFrame;
    let sample_rate = format.mSampleRate;

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
                AudioDeviceStop(aggregate_device_id, io_proc_id);
                AudioDeviceDestroyIOProcID(aggregate_device_id, io_proc_id);
                AudioHardwareDestroyAggregateDevice(aggregate_device_id);
                AudioHardwareDestroyProcessTap(tap_id);

                // CoreAudio keeps its own retained copy of the block once
                // registered, so dropping ours here (now that the IOProc
                // is torn down) is safe.
                drop(block);

                let samples = samples_written.load(Ordering::Relaxed);
                let duration_seconds = samples as f64 / channels as f64 / sample_rate;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_system_audio_to_wav_and_respects_pause() {
        let temp_dir = std::env::temp_dir().join("hyperrec_system_audio_test");
        std::fs::create_dir_all(&temp_dir).unwrap();
        let temp_file_path = temp_dir.join("system_audio.wav");

        let recorder = SystemAudioRecorder::start(temp_file_path.clone()).expect("start failed");

        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(200));
            std::process::Command::new("afplay")
                .arg("/System/Library/Sounds/Ping.aiff")
                .status()
                .ok();
        });

        std::thread::sleep(std::time::Duration::from_millis(700));
        recorder.pause().expect("pause failed");
        std::thread::sleep(std::time::Duration::from_millis(300));
        recorder.resume().expect("resume failed");
        std::thread::sleep(std::time::Duration::from_millis(700));
        let result = recorder.stop().expect("stop failed");

        assert!(result.temp_file_path.exists());
        assert!(result.duration_seconds > 0.0);

        let mut reader = hound::WavReader::open(&result.temp_file_path).expect("wav should be readable");
        let spec = reader.spec();
        let samples: Vec<f32> = reader.samples::<f32>().map(|s| s.unwrap()).collect();
        let max_amplitude = samples.iter().fold(0f32, |a, &b| a.max(b.abs()));
        println!(
            "system audio: {:.2}s, {} Hz, {} ch, max amplitude {:.3}",
            result.duration_seconds, spec.sample_rate, spec.channels, max_amplitude
        );
        assert!(max_amplitude > 0.05, "expected to capture the played ping sound, got max amplitude {max_amplitude}");

        std::fs::remove_file(&result.temp_file_path).ok();
    }

    #[test]
    fn survives_repeated_pause_resume_cycles() {
        let temp_dir = std::env::temp_dir().join("hyperrec_system_audio_repeat_test");
        std::fs::create_dir_all(&temp_dir).unwrap();
        let temp_file_path = temp_dir.join("system_audio.wav");

        let recorder = SystemAudioRecorder::start(temp_file_path.clone()).expect("start failed");

        for i in 0..3 {
            std::thread::sleep(std::time::Duration::from_millis(150));
            recorder.pause().unwrap_or_else(|e| panic!("pause #{i} failed: {e}"));
            std::thread::sleep(std::time::Duration::from_millis(100));
            recorder.resume().unwrap_or_else(|e| panic!("resume #{i} failed: {e}"));
        }

        std::thread::sleep(std::time::Duration::from_millis(150));
        let result = recorder.stop().expect("stop failed");
        assert!(result.temp_file_path.exists());
        std::fs::remove_file(&result.temp_file_path).ok();
    }
}
