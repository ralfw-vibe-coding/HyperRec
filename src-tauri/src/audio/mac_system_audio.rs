//! System audio capture via Core Audio Process Taps (macOS 14.4+).
//!
//! Builds a private, global process tap (the whole system mix) plus a
//! private *tap-only* aggregate device — no real subdevice. An earlier
//! version included the current output device as a subdevice purely to
//! "borrow its clock", but that can make the aggregate actually drive
//! that device's hardware: when the borrowed device is the one the user
//! is listening through, the result is an audible echo. A tap-only
//! aggregate clocks itself off the tap and has no such side effect, and
//! it also sidesteps the unrelated issue where a Bluetooth headset's
//! clock glitches when macOS switches it from A2DP to HFP. An
//! Objective-C IO block delivers PCM buffers for the aggregate device,
//! which are written straight into a WAV file.
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
use std::time::Duration;

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::ffi::NSInteger;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::AnyThread;
use objc2::{define_class, msg_send, DefinedClass};
use objc2_core_audio::{
    kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceIsStackedKey,
    kAudioAggregateDeviceNameKey, kAudioAggregateDeviceTapAutoStartKey,
    kAudioAggregateDeviceTapListKey, kAudioAggregateDeviceUIDKey, kAudioDevicePropertyDeviceUID,
    kAudioHardwarePropertyDevices, kAudioObjectPropertyElementMain, kAudioObjectPropertyName,
    kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject, kAudioSubTapDriftCompensationKey,
    kAudioSubTapUIDKey, kAudioTapPropertyFormat, AudioDeviceCreateIOProcIDWithBlock,
    AudioDeviceDestroyIOProcID, AudioDeviceIOProcID, AudioDeviceStart, AudioDeviceStop,
    AudioHardwareCreateAggregateDevice, AudioHardwareCreateProcessTap,
    AudioHardwareDestroyAggregateDevice, AudioHardwareDestroyProcessTap,
    AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectPropertyAddress,
    CATapDescription,
};
use objc2_core_audio_types::{
    kAudioFormatFlagIsBigEndian, kAudioFormatFlagIsFloat, kAudioFormatFlagIsSignedInteger,
    kAudioFormatLinearPCM,
};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp,
};
use objc2_core_foundation::{CFArray, CFBoolean, CFDictionary, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMAudioFormatDescriptionGetStreamBasicDescription, CMBlockBuffer, CMSampleBuffer,
};
use objc2_foundation::{NSArray, NSError, NSNumber, NSObject, NSObjectProtocol, NSString, NSUUID};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamOutput,
    SCStreamOutputType,
};

use super::{AudioDevice, AudioError, RecordingResult, Result};

type WavWriter = hound::WavWriter<BufWriter<File>>;

enum RecorderCommand {
    #[allow(dead_code)]
    Start(PathBuf, Sender<Result<()>>),
    Pause(Sender<Result<()>>),
    Resume(Sender<Result<()>>),
    Stop(Sender<Result<RecordingResult>>),
}

#[allow(dead_code)]
static SCREEN_CAPTURE_DAEMON: Mutex<Option<Sender<RecorderCommand>>> = Mutex::new(None);

pub struct SystemAudioRecorder {
    control_tx: Sender<RecorderCommand>,
    join_handle: Option<JoinHandle<()>>,
}

impl SystemAudioRecorder {
    pub fn start(temp_file_path: PathBuf, output_device_uid: Option<String>) -> Result<Self> {
        let (ready_tx, ready_rx) = channel();
        let (control_tx, control_rx) = channel();

        let join_handle = std::thread::Builder::new()
            .name("hyperrec-system-audio-recorder".into())
            .spawn(move || run_recorder(temp_file_path, output_device_uid, ready_tx, control_rx))
            .map_err(|e| AudioError::Backend(e.to_string()))?;

        ready_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| {
                AudioError::Backend("system audio recorder thread did not become ready".into())
            })??;

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
        let result = reply_rx.recv().map_err(|_| {
            AudioError::Backend("system audio recorder thread did not reply".into())
        })?;
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
        result
    }
}

#[allow(dead_code)]
fn cfstr(s: &str) -> CFRetained<CFString> {
    CFString::from_str(s)
}

#[allow(dead_code)]
fn cft<T: AsRef<CFType> + ?Sized>(t: &T) -> &CFType {
    t.as_ref()
}

#[allow(dead_code)]
fn cfstr_from_cstr(s: &std::ffi::CStr) -> CFRetained<CFString> {
    cfstr(s.to_str().expect("Core Audio key constants are ASCII"))
}

unsafe fn cfstring_property(object_id: u32, selector: u32, scope: u32) -> Option<String> {
    let address = AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut size = std::mem::size_of::<CFRetained<CFString>>() as u32;
    let mut value = std::mem::MaybeUninit::<CFRetained<CFString>>::zeroed();
    let status = AudioObjectGetPropertyData(
        object_id,
        NonNull::from(&address),
        0,
        std::ptr::null(),
        NonNull::from(&mut size),
        NonNull::new(value.as_mut_ptr() as *mut c_void).unwrap(),
    );
    if status != 0 {
        return None;
    }
    Some(value.assume_init().to_string())
}

unsafe fn all_audio_device_ids() -> Result<Vec<u32>> {
    let address = AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyDevices,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut size = 0u32;
    let size_status = AudioObjectGetPropertyDataSize(
        kAudioObjectSystemObject as u32,
        NonNull::from(&address),
        0,
        std::ptr::null(),
        NonNull::from(&mut size),
    );
    if size_status != 0 {
        return Err(AudioError::Backend(format!(
            "AudioObjectGetPropertyDataSize(devices) failed: OSStatus={size_status}"
        )));
    }

    let count = size as usize / std::mem::size_of::<u32>();
    let mut devices = vec![0u32; count];
    let data_status = AudioObjectGetPropertyData(
        kAudioObjectSystemObject as u32,
        NonNull::from(&address),
        0,
        std::ptr::null(),
        NonNull::from(&mut size),
        NonNull::new(devices.as_mut_ptr() as *mut c_void).unwrap(),
    );
    if data_status != 0 {
        return Err(AudioError::Backend(format!(
            "AudioObjectGetPropertyData(devices) failed: OSStatus={data_status}"
        )));
    }

    Ok(devices)
}

pub fn list_coreaudio_output_devices(default_name: Option<&str>) -> Result<Vec<AudioDevice>> {
    unsafe {
        let mut devices = Vec::new();
        for device_id in all_audio_device_ids()? {
            let Some(uid) = cfstring_property(
                device_id,
                kAudioDevicePropertyDeviceUID,
                kAudioObjectPropertyScopeGlobal,
            ) else {
                continue;
            };
            let Some(name) = cfstring_property(
                device_id,
                kAudioObjectPropertyName,
                kAudioObjectPropertyScopeGlobal,
            ) else {
                continue;
            };
            devices.push(AudioDevice {
                id: uid,
                is_default: default_name == Some(name.as_str()),
                name,
            });
        }
        Ok(devices)
    }
}

#[allow(dead_code)]
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

fn format_id_to_string(format_id: u32) -> String {
    String::from_utf8_lossy(&format_id.to_be_bytes()).into_owned()
}

#[derive(Debug, Clone, Copy)]
struct PcmFormat {
    channels_per_frame: usize,
    bytes_per_sample: usize,
    is_float: bool,
    is_signed_int: bool,
    is_big_endian: bool,
}

impl PcmFormat {
    fn from_asbd(format: &AudioStreamBasicDescription) -> Result<Self> {
        if format.mFormatID != kAudioFormatLinearPCM {
            return Err(AudioError::Backend(format!(
                "unsupported system audio format: {}",
                format_id_to_string(format.mFormatID)
            )));
        }

        let channels_per_frame = format.mChannelsPerFrame as usize;
        let bits_per_channel = format.mBitsPerChannel as usize;
        let bytes_per_sample = bits_per_channel / 8;
        if channels_per_frame == 0 || bytes_per_sample == 0 || bits_per_channel % 8 != 0 {
            return Err(AudioError::Backend(format!(
                "unsupported system audio PCM shape: {} channels, {} bits",
                channels_per_frame, bits_per_channel
            )));
        }

        Ok(Self {
            channels_per_frame,
            bytes_per_sample,
            is_float: format.mFormatFlags & kAudioFormatFlagIsFloat != 0,
            is_signed_int: format.mFormatFlags & kAudioFormatFlagIsSignedInteger != 0,
            is_big_endian: format.mFormatFlags & kAudioFormatFlagIsBigEndian != 0,
        })
    }

    fn decode_sample(self, bytes: &[u8]) -> Option<f32> {
        match (self.is_float, self.is_signed_int, self.bytes_per_sample) {
            (true, _, 4) => {
                Some(f32::from_bits(read_u32(bytes, self.is_big_endian)?).clamp(-1.0, 1.0))
            }
            (true, _, 8) => {
                Some((f64::from_bits(read_u64(bytes, self.is_big_endian)?) as f32).clamp(-1.0, 1.0))
            }
            (false, true, 2) => Some(read_i16(bytes, self.is_big_endian)? as f32 / i16::MAX as f32),
            (false, true, 3) => Some(read_i24(bytes, self.is_big_endian)? as f32 / 8_388_607.0),
            (false, true, 4) => Some(read_i32(bytes, self.is_big_endian)? as f32 / i32::MAX as f32),
            _ => None,
        }
    }
}

fn read_u32(bytes: &[u8], big_endian: bool) -> Option<u32> {
    let bytes: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
    Some(if big_endian {
        u32::from_be_bytes(bytes)
    } else {
        u32::from_le_bytes(bytes)
    })
}

fn read_u64(bytes: &[u8], big_endian: bool) -> Option<u64> {
    let bytes: [u8; 8] = bytes.get(..8)?.try_into().ok()?;
    Some(if big_endian {
        u64::from_be_bytes(bytes)
    } else {
        u64::from_le_bytes(bytes)
    })
}

fn read_i16(bytes: &[u8], big_endian: bool) -> Option<i16> {
    let bytes: [u8; 2] = bytes.get(..2)?.try_into().ok()?;
    Some(if big_endian {
        i16::from_be_bytes(bytes)
    } else {
        i16::from_le_bytes(bytes)
    })
}

fn read_i24(bytes: &[u8], big_endian: bool) -> Option<i32> {
    let bytes = bytes.get(..3)?;
    let unsigned = if big_endian {
        ((bytes[0] as i32) << 16) | ((bytes[1] as i32) << 8) | bytes[2] as i32
    } else {
        ((bytes[2] as i32) << 16) | ((bytes[1] as i32) << 8) | bytes[0] as i32
    };
    Some((unsigned << 8) >> 8)
}

fn read_i32(bytes: &[u8], big_endian: bool) -> Option<i32> {
    let bytes: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
    Some(if big_endian {
        i32::from_be_bytes(bytes)
    } else {
        i32::from_le_bytes(bytes)
    })
}

unsafe fn audio_buffers<'a>(list: &'a AudioBufferList) -> &'a [AudioBuffer] {
    std::slice::from_raw_parts(list.mBuffers.as_ptr(), list.mNumberBuffers as usize)
}

unsafe fn bytes_for_buffer(buffer: &AudioBuffer) -> Option<&[u8]> {
    if buffer.mData.is_null() || buffer.mDataByteSize == 0 {
        return None;
    }
    Some(std::slice::from_raw_parts(
        buffer.mData as *const u8,
        buffer.mDataByteSize as usize,
    ))
}

unsafe fn write_audio_buffer_list(
    writer: &mut WavWriter,
    list: &AudioBufferList,
    pcm: PcmFormat,
) -> u64 {
    if list.mNumberBuffers == 0 || pcm.channels_per_frame == 0 {
        return 0;
    }

    let buffers = audio_buffers(list);
    if buffers.len() == 1 {
        let Some(bytes) = bytes_for_buffer(&buffers[0]) else {
            return 0;
        };
        let n_samples = bytes.len() / pcm.bytes_per_sample;
        for sample_index in 0..n_samples {
            let start = sample_index * pcm.bytes_per_sample;
            let Some(sample) = pcm.decode_sample(&bytes[start..start + pcm.bytes_per_sample])
            else {
                return 0;
            };
            let _ = writer.write_sample(sample);
        }
        return n_samples as u64;
    }

    let mut buffer_slices = Vec::with_capacity(buffers.len());
    let mut total_buffer_channels = 0usize;
    let mut frames = usize::MAX;
    for buffer in buffers {
        let Some(bytes) = bytes_for_buffer(buffer) else {
            return 0;
        };
        let buffer_channels = (buffer.mNumberChannels as usize).max(1);
        let buffer_frames = bytes.len() / pcm.bytes_per_sample / buffer_channels;
        if buffer_frames == 0 {
            return 0;
        }
        frames = frames.min(buffer_frames);
        total_buffer_channels += buffer_channels;
        buffer_slices.push((bytes, buffer_channels));
    }

    if frames == usize::MAX || total_buffer_channels == 0 {
        return 0;
    }

    let mut written = 0u64;
    for frame in 0..frames {
        let mut channels_written_for_frame = 0usize;
        for (bytes, buffer_channels) in &buffer_slices {
            for channel in 0..*buffer_channels {
                if channels_written_for_frame >= pcm.channels_per_frame {
                    break;
                }
                let sample_index = frame * *buffer_channels + channel;
                let start = sample_index * pcm.bytes_per_sample;
                let Some(sample) = pcm.decode_sample(&bytes[start..start + pcm.bytes_per_sample])
                else {
                    return written;
                };
                let _ = writer.write_sample(sample);
                channels_written_for_frame += 1;
                written += 1;
            }
        }
        while channels_written_for_frame < pcm.channels_per_frame {
            let _ = writer.write_sample(0.0f32);
            channels_written_for_frame += 1;
            written += 1;
        }
    }

    written
}

/// Creates a private global tap + private aggregate device pair.
/// The aggregate's only purpose is to host the tap's input stream and to
/// borrow a real hardware clock — preferably a built-in device, since a
/// Bluetooth headset's clock can glitch when its profile switches (see
/// `find_stable_clock_device`).
#[allow(dead_code)]
unsafe fn create_tap_and_aggregate(
    output_device_uid: Option<&str>,
) -> std::result::Result<(u32, u32, String), String> {
    let exclude: Retained<NSArray<NSNumber>> = NSArray::from_slice(&[]);
    let description = if let Some(output_device_uid) = output_device_uid {
        let device_uid = NSString::from_str(output_device_uid);
        let stream_index = 1 as NSInteger;
        eprintln!(
            "HyperRec: creating device-specific system tap for output uid {output_device_uid}, stream {stream_index}"
        );
        CATapDescription::initExcludingProcesses_andDeviceUID_withStream(
            CATapDescription::alloc(),
            &exclude,
            &device_uid,
            stream_index,
        )
    } else {
        CATapDescription::initStereoGlobalTapButExcludeProcesses(
            CATapDescription::alloc(),
            &exclude,
        )
    };
    description.setPrivate(true);
    let uuid = NSUUID::new();
    description.setUUID(&uuid);
    let tap_uid_string = uuid.UUIDString().to_string();

    let mut tap_id: u32 = 0;
    let status = AudioHardwareCreateProcessTap(Some(&description), &mut tap_id);
    if status != 0 {
        return Err(format!(
            "AudioHardwareCreateProcessTap failed: OSStatus={status}"
        ));
    }

    let tap_dict = CFDictionary::from_slices(
        &[
            &*cfstr_from_cstr(kAudioSubTapUIDKey),
            &*cfstr_from_cstr(kAudioSubTapDriftCompensationKey),
        ],
        &[cft(&cfstr(&tap_uid_string)), cft(CFBoolean::new(true))],
    );
    let tap_list = CFArray::from_objects(&[&*tap_dict]);

    let aggregate_uid = format!("com.zeitgewinn.hyperrec.system-audio-tap.{tap_uid_string}");

    // No real subdevice on purpose: including one can make the aggregate
    // actually drive that device's hardware, which is audible as
    // echo/duplication and can hang some headset routes. Device-specific
    // capture is handled by CATapDescription above, not by subdevices.
    let aggregate_description = CFDictionary::from_slices(
        &[
            &*cfstr_from_cstr(kAudioAggregateDeviceNameKey),
            &*cfstr_from_cstr(kAudioAggregateDeviceUIDKey),
            &*cfstr_from_cstr(kAudioAggregateDeviceIsPrivateKey),
            &*cfstr_from_cstr(kAudioAggregateDeviceIsStackedKey),
            &*cfstr_from_cstr(kAudioAggregateDeviceTapAutoStartKey),
            &*cfstr_from_cstr(kAudioAggregateDeviceTapListKey),
        ],
        &[
            cft(&cfstr("HyperRec System Audio Tap")),
            cft(&cfstr(&aggregate_uid)),
            cft(CFBoolean::new(true)),
            cft(CFBoolean::new(false)),
            cft(CFBoolean::new(true)),
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
        return Err(format!(
            "AudioHardwareCreateAggregateDevice failed: OSStatus={agg_status}"
        ));
    }
    Ok((tap_id, aggregate_device_id, tap_uid_string))
}

fn run_recorder(
    temp_file_path: PathBuf,
    output_device_uid: Option<String>,
    ready_tx: Sender<Result<()>>,
    control_rx: Receiver<RecorderCommand>,
) {
    unsafe { run_screen_capture_recorder(temp_file_path, output_device_uid, ready_tx, control_rx) }
}

struct ScreenCaptureOutputIvars {
    writer: Arc<Mutex<Option<WavWriter>>>,
    samples_written: Arc<AtomicU64>,
    paused: Arc<std::sync::atomic::AtomicBool>,
    pcm: Mutex<Option<PcmFormat>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[ivars = ScreenCaptureOutputIvars]
    struct ScreenCaptureOutput;

    unsafe impl NSObjectProtocol for ScreenCaptureOutput {}

    unsafe impl SCStreamOutput for ScreenCaptureOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        #[allow(non_snake_case)]
        fn stream_didOutputSampleBuffer_ofType(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            output_type: SCStreamOutputType,
        ) {
            if output_type != SCStreamOutputType::Audio {
                return;
            }
            let ivars = self.ivars();
            if ivars.paused.load(Ordering::Relaxed) {
                return;
            }
            unsafe {
                write_screen_capture_sample(
                    sample_buffer,
                    &ivars.writer,
                    &ivars.samples_written,
                    &ivars.pcm,
                );
            }
        }
    }
);

impl ScreenCaptureOutput {
    fn new(
        writer: Arc<Mutex<Option<WavWriter>>>,
        samples_written: Arc<AtomicU64>,
        paused: Arc<std::sync::atomic::AtomicBool>,
    ) -> Retained<Self> {
        let this = Self::alloc().set_ivars(ScreenCaptureOutputIvars {
            writer,
            samples_written,
            paused,
            pcm: Mutex::new(None),
        });
        unsafe { msg_send![super(this), init] }
    }
}

unsafe fn write_screen_capture_sample(
    sample_buffer: &CMSampleBuffer,
    writer: &Arc<Mutex<Option<WavWriter>>>,
    samples_written: &Arc<AtomicU64>,
    pcm_cache: &Mutex<Option<PcmFormat>>,
) {
    let Some(format_description) = sample_buffer.format_description() else {
        return;
    };
    let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(format_description.as_ref());
    let Some(asbd) = asbd.as_ref() else {
        return;
    };
    let pcm = {
        let mut guard = match pcm_cache.lock() {
            Ok(guard) => guard,
            Err(_) => return,
        };
        if let Some(pcm) = *guard {
            pcm
        } else {
            let Ok(pcm) = PcmFormat::from_asbd(asbd) else {
                return;
            };
            *guard = Some(pcm);
            pcm
        }
    };

    let mut needed = 0usize;
    let status = sample_buffer.audio_buffer_list_with_retained_block_buffer(
        &mut needed,
        std::ptr::null_mut(),
        0,
        None,
        None,
        0,
        std::ptr::null_mut(),
    );
    if status != 0 || needed == 0 {
        return;
    }

    let mut storage = vec![0u8; needed];
    let list_ptr = storage.as_mut_ptr() as *mut AudioBufferList;
    let mut block_buffer: *mut CMBlockBuffer = std::ptr::null_mut();
    let status = sample_buffer.audio_buffer_list_with_retained_block_buffer(
        std::ptr::null_mut(),
        list_ptr,
        needed,
        None,
        None,
        0,
        &mut block_buffer,
    );
    if status != 0 {
        return;
    }
    let _block_buffer = NonNull::new(block_buffer).map(|ptr| CFRetained::from_raw(ptr));

    let list = &*list_ptr;
    if let Ok(mut guard) = writer.lock() {
        if let Some(w) = guard.as_mut() {
            let written = write_audio_buffer_list(w, list, pcm);
            samples_written.fetch_add(written, Ordering::Relaxed);
        }
    }
}

unsafe fn shareable_content() -> Result<Retained<SCShareableContent>> {
    let (tx, rx) = channel();
    let block = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
        let result = if !error.is_null() {
            Err(AudioError::Backend(format!(
                "ScreenCaptureKit shareable content failed: {}",
                &*error
            )))
        } else {
            Retained::retain(content).ok_or_else(|| {
                AudioError::Backend(
                    "ScreenCaptureKit did not return shareable display content".into(),
                )
            })
        };
        let _ = tx.send(result);
    });
    // HyperRec records system audio only; it does not need to enumerate the
    // user's windows. The current-process variant returns redacted content
    // without triggering the full Screen Recording TCC consent UI on every
    // recording start, while still giving us a display to attach the stream to.
    SCShareableContent::getCurrentProcessShareableContentWithCompletionHandler(&block);
    rx.recv_timeout(Duration::from_secs(5)).map_err(|_| {
        AudioError::Backend("ScreenCaptureKit shareable content request timed out".into())
    })?
}

unsafe fn wait_for_screen_capture_start(stream: &SCStream) -> Result<()> {
    let (tx, rx) = channel();
    let block = RcBlock::new(move |error: *mut NSError| {
        let result = if error.is_null() {
            Ok(())
        } else {
            Err(AudioError::Backend(format!(
                "ScreenCaptureKit start failed: {}",
                &*error
            )))
        };
        let _ = tx.send(result);
    });
    stream.startCaptureWithCompletionHandler(Some(&block));
    rx.recv_timeout(Duration::from_secs(5))
        .map_err(|_| AudioError::Backend("ScreenCaptureKit start timed out".into()))?
}

unsafe fn stop_screen_capture(stream: &SCStream) {
    let (tx, rx) = channel();
    let block = RcBlock::new(move |_error: *mut NSError| {
        let _ = tx.send(());
    });
    stream.stopCaptureWithCompletionHandler(Some(&block));
    let _ = rx.recv_timeout(Duration::from_secs(2));
}

fn start_screen_capture_session(temp_file_path: PathBuf) -> Result<Sender<RecorderCommand>> {
    let control_tx = screen_capture_daemon_tx()?;
    let (reply_tx, reply_rx) = channel();
    control_tx
        .send(RecorderCommand::Start(temp_file_path, reply_tx))
        .map_err(|_| AudioError::Backend("system audio recorder thread is gone".into()))?;
    reply_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| AudioError::Backend("system audio recorder thread did not reply".into()))??;
    Ok(control_tx)
}

fn screen_capture_daemon_tx() -> Result<Sender<RecorderCommand>> {
    let mut daemon = SCREEN_CAPTURE_DAEMON
        .lock()
        .map_err(|_| AudioError::Backend("system audio daemon mutex poisoned".into()))?;

    if let Some(control_tx) = daemon.as_ref() {
        return Ok(control_tx.clone());
    }

    let (ready_tx, ready_rx) = channel();
    let (control_tx, control_rx) = channel();
    std::thread::Builder::new()
        .name("hyperrec-system-audio-recorder".into())
        .spawn(move || unsafe { run_screen_capture_daemon(ready_tx, control_rx) })
        .map_err(|e| AudioError::Backend(e.to_string()))?;

    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| AudioError::Backend("system audio recorder thread did not become ready".into()))??;

    *daemon = Some(control_tx.clone());
    Ok(control_tx)
}

fn create_screen_capture_writer(temp_file_path: &PathBuf) -> Result<WavWriter> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 48_000,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    hound::WavWriter::create(temp_file_path, spec).map_err(|e| AudioError::Backend(e.to_string()))
}

unsafe fn run_screen_capture_daemon(
    ready_tx: Sender<Result<()>>,
    control_rx: Receiver<RecorderCommand>,
) {
    let content = match shareable_content() {
        Ok(content) => content,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };
    let displays = content.displays();
    let Some(display) = displays.firstObject() else {
        let _ = ready_tx.send(Err(AudioError::Backend(
            "ScreenCaptureKit found no display to attach audio capture to".into(),
        )));
        return;
    };

    let excluded_windows: Retained<NSArray<objc2_screen_capture_kit::SCWindow>> =
        NSArray::from_slice(&[]);
    let filter = SCContentFilter::initWithDisplay_excludingWindows(
        SCContentFilter::alloc(),
        &display,
        &excluded_windows,
    );
    let config = SCStreamConfiguration::new();
    config.setCapturesAudio(true);
    config.setExcludesCurrentProcessAudio(false);
    config.setCaptureMicrophone(false);
    config.setSampleRate(48_000);
    config.setChannelCount(2);
    config.setWidth(2);
    config.setHeight(2);
    config.setQueueDepth(3);

    let writer = Arc::new(Mutex::new(None));
    let current_path = Arc::new(Mutex::new(None::<PathBuf>));
    let samples_written = Arc::new(AtomicU64::new(0));
    let paused = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let output = ScreenCaptureOutput::new(writer.clone(), samples_written.clone(), paused.clone());
    let stream =
        SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &config, None);
    let queue = DispatchQueue::new("com.zeitgewinn.hyperrec.screen-capture-audio", None);
    if let Err(e) = stream.addStreamOutput_type_sampleHandlerQueue_error(
        ProtocolObject::from_ref::<ScreenCaptureOutput>(&output),
        SCStreamOutputType::Audio,
        Some(&queue),
    ) {
        let _ = ready_tx.send(Err(AudioError::Backend(format!(
            "ScreenCaptureKit add audio output failed: {e}"
        ))));
        return;
    }

    eprintln!("HyperRec: starting persistent ScreenCaptureKit system audio capture");
    if let Err(e) = wait_for_screen_capture_start(&stream) {
        let _ = ready_tx.send(Err(e));
        return;
    }
    if ready_tx.send(Ok(())).is_err() {
        stop_screen_capture(&stream);
        return;
    }

    while let Ok(command) = control_rx.recv() {
        match command {
            RecorderCommand::Start(temp_file_path, reply) => {
                let result = writer
                    .lock()
                    .map_err(|_| AudioError::Backend("wav writer mutex poisoned".into()))
                    .and_then(|mut guard| {
                        if guard.is_some() {
                            return Err(AudioError::InvalidState(
                                "system audio recording already active".into(),
                            ));
                        }
                        let wav_writer = create_screen_capture_writer(&temp_file_path)?;
                        samples_written.store(0, Ordering::Relaxed);
                        paused.store(false, Ordering::Relaxed);
                        *guard = Some(wav_writer);
                        *current_path
                            .lock()
                            .map_err(|_| AudioError::Backend("wav path mutex poisoned".into()))? =
                            Some(temp_file_path);
                        Ok(())
                    });
                let _ = reply.send(result);
            }
            RecorderCommand::Pause(reply) => {
                paused.store(true, Ordering::Relaxed);
                let _ = reply.send(Ok(()));
            }
            RecorderCommand::Resume(reply) => {
                paused.store(false, Ordering::Relaxed);
                let _ = reply.send(Ok(()));
            }
            RecorderCommand::Stop(reply) => {
                paused.store(true, Ordering::Relaxed);
                let samples = samples_written.load(Ordering::Relaxed);
                let duration_seconds = samples as f64 / 2.0 / 48_000.0;
                eprintln!(
                    "HyperRec: persistent ScreenCaptureKit audio segment stopped after {:.2}s, wrote {} samples",
                    duration_seconds, samples
                );
                let result = writer
                    .lock()
                    .map_err(|_| AudioError::Backend("wav writer mutex poisoned".into()))
                    .and_then(|mut guard| {
                        let path = current_path
                            .lock()
                            .map_err(|_| AudioError::Backend("wav path mutex poisoned".into()))?
                            .take()
                            .ok_or_else(|| AudioError::Backend("wav path already taken".into()))?;
                        let writer = guard
                            .take()
                            .ok_or_else(|| AudioError::Backend("wav writer already taken".into()))?;
                        Ok((path, writer))
                    })
                    .and_then(|(path, w)| {
                        w.finalize()
                            .map_err(|e| AudioError::Backend(e.to_string()))
                            .map(|_| path)
                    })
                    .map(|path| RecordingResult {
                        temp_file_path: path,
                        duration_seconds,
                    });
                let _ = reply.send(result);
            }
        }
    }

    stop_screen_capture(&stream);
}

unsafe fn run_screen_capture_recorder(
    temp_file_path: PathBuf,
    _output_device_uid: Option<String>,
    ready_tx: Sender<Result<()>>,
    control_rx: Receiver<RecorderCommand>,
) {
    let content = match shareable_content() {
        Ok(content) => content,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };
    let displays = content.displays();
    let Some(display) = displays.firstObject() else {
        let _ = ready_tx.send(Err(AudioError::Backend(
            "ScreenCaptureKit found no display to attach audio capture to".into(),
        )));
        return;
    };

    let excluded_windows: Retained<NSArray<objc2_screen_capture_kit::SCWindow>> =
        NSArray::from_slice(&[]);
    let filter = SCContentFilter::initWithDisplay_excludingWindows(
        SCContentFilter::alloc(),
        &display,
        &excluded_windows,
    );
    let config = SCStreamConfiguration::new();
    config.setCapturesAudio(true);
    config.setExcludesCurrentProcessAudio(false);
    config.setCaptureMicrophone(false);
    config.setSampleRate(48_000);
    config.setChannelCount(2);
    config.setWidth(2);
    config.setHeight(2);
    config.setQueueDepth(3);

    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 48_000,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let writer = match hound::WavWriter::create(&temp_file_path, spec) {
        Ok(writer) => writer,
        Err(e) => {
            let _ = ready_tx.send(Err(AudioError::Backend(e.to_string())));
            return;
        }
    };
    let writer = Arc::new(Mutex::new(Some(writer)));
    let samples_written = Arc::new(AtomicU64::new(0));
    let paused = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let output = ScreenCaptureOutput::new(writer.clone(), samples_written.clone(), paused.clone());
    let stream =
        SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &config, None);
    let queue = DispatchQueue::new("com.zeitgewinn.hyperrec.screen-capture-audio", None);
    if let Err(e) = stream.addStreamOutput_type_sampleHandlerQueue_error(
        ProtocolObject::from_ref::<ScreenCaptureOutput>(&output),
        SCStreamOutputType::Audio,
        Some(&queue),
    ) {
        let _ = ready_tx.send(Err(AudioError::Backend(format!(
            "ScreenCaptureKit add audio output failed: {e}"
        ))));
        return;
    }

    eprintln!("HyperRec: starting ScreenCaptureKit system audio capture");
    if let Err(e) = wait_for_screen_capture_start(&stream) {
        let _ = ready_tx.send(Err(e));
        return;
    }
    if ready_tx.send(Ok(())).is_err() {
        stop_screen_capture(&stream);
        return;
    }

    let channels = 2u32;
    let sample_rate = 48_000f64;
    while let Ok(command) = control_rx.recv() {
        match command {
            RecorderCommand::Start(_, reply) => {
                let _ = reply.send(Err(AudioError::InvalidState(
                    "system audio recording already active".into(),
                )));
            }
            RecorderCommand::Pause(reply) => {
                paused.store(true, Ordering::Relaxed);
                let _ = reply.send(Ok(()));
            }
            RecorderCommand::Resume(reply) => {
                paused.store(false, Ordering::Relaxed);
                let _ = reply.send(Ok(()));
            }
            RecorderCommand::Stop(reply) => {
                stop_screen_capture(&stream);
                let samples = samples_written.load(Ordering::Relaxed);
                let duration_seconds = samples as f64 / channels as f64 / sample_rate;
                eprintln!(
                    "HyperRec: ScreenCaptureKit audio stopped after {:.2}s, wrote {} samples",
                    duration_seconds, samples
                );
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

#[allow(dead_code)]
unsafe fn run_recorder_unsafe(
    temp_file_path: PathBuf,
    output_device_uid: Option<String>,
    ready_tx: Sender<Result<()>>,
    control_rx: Receiver<RecorderCommand>,
) {
    let (tap_id, aggregate_device_id, _tap_uid) =
        match create_tap_and_aggregate(output_device_uid.as_deref()) {
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
    eprintln!(
        "HyperRec: system tap format {} rate={} channels={} bits={} flags=0x{:x} bytes/frame={}",
        format_id_to_string(format.mFormatID),
        format.mSampleRate,
        format.mChannelsPerFrame,
        format.mBitsPerChannel,
        format.mFormatFlags,
        format.mBytesPerFrame
    );
    let pcm = match PcmFormat::from_asbd(&format) {
        Ok(pcm) => pcm,
        Err(e) => {
            AudioHardwareDestroyAggregateDevice(aggregate_device_id);
            AudioHardwareDestroyProcessTap(tap_id);
            let _ = ready_tx.send(Err(e));
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
            if let Ok(mut guard) = writer_for_block.lock() {
                if let Some(w) = guard.as_mut() {
                    let written = write_audio_buffer_list(w, list, pcm);
                    samples_for_block.fetch_add(written, Ordering::Relaxed);
                }
            }
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
            RecorderCommand::Start(_, reply) => {
                let _ = reply.send(Err(AudioError::InvalidState(
                    "system audio recording already active".into(),
                )));
            }
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
                eprintln!(
                    "HyperRec: system tap stopped after {:.2}s, wrote {} samples",
                    duration_seconds, samples
                );

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

        let recorder =
            SystemAudioRecorder::start(temp_file_path.clone(), None).expect("start failed");

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

        let mut reader =
            hound::WavReader::open(&result.temp_file_path).expect("wav should be readable");
        let spec = reader.spec();
        let samples: Vec<f32> = reader.samples::<f32>().map(|s| s.unwrap()).collect();
        let max_amplitude = samples.iter().fold(0f32, |a, &b| a.max(b.abs()));
        println!(
            "system audio: {:.2}s, {} Hz, {} ch, max amplitude {:.3}",
            result.duration_seconds, spec.sample_rate, spec.channels, max_amplitude
        );
        assert!(
            max_amplitude > 0.05,
            "expected to capture the played ping sound, got max amplitude {max_amplitude}"
        );

        std::fs::remove_file(&result.temp_file_path).ok();
    }

    #[test]
    fn survives_repeated_pause_resume_cycles() {
        let temp_dir = std::env::temp_dir().join("hyperrec_system_audio_repeat_test");
        std::fs::create_dir_all(&temp_dir).unwrap();
        let temp_file_path = temp_dir.join("system_audio.wav");

        let recorder =
            SystemAudioRecorder::start(temp_file_path.clone(), None).expect("start failed");

        for i in 0..3 {
            std::thread::sleep(std::time::Duration::from_millis(150));
            recorder
                .pause()
                .unwrap_or_else(|e| panic!("pause #{i} failed: {e}"));
            std::thread::sleep(std::time::Duration::from_millis(100));
            recorder
                .resume()
                .unwrap_or_else(|e| panic!("resume #{i} failed: {e}"));
        }

        std::thread::sleep(std::time::Duration::from_millis(150));
        let result = recorder.stop().expect("stop failed");
        assert!(result.temp_file_path.exists());
        std::fs::remove_file(&result.temp_file_path).ok();
    }
}
