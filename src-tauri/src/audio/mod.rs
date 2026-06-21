//! Platform-independent audio abstraction. UI and RecordingController only
//! ever talk to the `AudioProvider` trait; platform code lives behind it
//! (see `mac`, later `windows`).

#[cfg(target_os = "macos")]
pub mod mac;
#[cfg(target_os = "macos")]
pub mod mac_system_audio;
pub mod mixer;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AudioError {
    #[error("device not found: {0}")]
    DeviceNotFound(String),
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("invalid state for this operation: {0}")]
    InvalidState(String),
    #[error("audio backend error: {0}")]
    Backend(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, AudioError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionStatus {
    Granted,
    Denied,
    NotDetermined,
}

#[derive(Debug, Clone)]
pub struct RecordingConfig {
    pub input_device_id: Option<String>,
    pub output_device_id: Option<String>,
    pub temp_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingResult {
    pub temp_file_path: PathBuf,
    pub duration_seconds: f64,
}

/// Implemented once per platform (macOS now, Windows later). The UI and
/// `RecordingController` depend only on this trait, never on Core Audio,
/// cpal, or WASAPI directly.
pub trait AudioProvider: Send {
    fn list_input_devices(&self) -> Result<Vec<AudioDevice>>;
    fn list_output_devices(&self) -> Result<Vec<AudioDevice>>;

    fn default_input_device(&self) -> Result<AudioDevice>;
    fn default_output_device(&self) -> Result<AudioDevice>;

    fn check_permissions(&self) -> Result<PermissionStatus>;
    fn request_permissions(&self) -> Result<PermissionStatus>;

    fn start_recording(&mut self, config: RecordingConfig) -> Result<()>;
    fn pause_recording(&mut self) -> Result<()>;
    fn resume_recording(&mut self) -> Result<()>;
    fn stop_recording(&mut self) -> Result<RecordingResult>;
}
