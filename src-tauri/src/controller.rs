//! Platform-independent recording state machine. Owns an `AudioProvider`
//! and enforces valid state transitions; the UI never talks to the
//! provider directly.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::audio::{AudioError, AudioProvider, RecordingConfig, RecordingResult, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingState {
    Idle,
    Recording,
    Paused,
    Stopped,
}

pub struct RecordingController {
    provider: Box<dyn AudioProvider>,
    state: RecordingState,
    started_at: Option<Instant>,
    paused_at: Option<Instant>,
    accumulated_pause: Duration,
}

impl RecordingController {
    pub fn new(provider: Box<dyn AudioProvider>) -> Self {
        Self {
            provider,
            state: RecordingState::Idle,
            started_at: None,
            paused_at: None,
            accumulated_pause: Duration::ZERO,
        }
    }

    pub fn state(&self) -> RecordingState {
        self.state
    }

    pub fn provider(&self) -> &dyn AudioProvider {
        self.provider.as_ref()
    }

    /// Time actually spent recording: wall-clock time since start, minus
    /// every pause span (completed or in progress). Mirrors the WAV
    /// output, which never contains silence for paused time either.
    pub fn elapsed_seconds(&self) -> f64 {
        let Some(started_at) = self.started_at else {
            return 0.0;
        };
        let now = Instant::now();
        let total = now.duration_since(started_at);
        let ongoing_pause = self.paused_at.map(|p| now.duration_since(p)).unwrap_or(Duration::ZERO);
        total
            .saturating_sub(self.accumulated_pause)
            .saturating_sub(ongoing_pause)
            .as_secs_f64()
    }

    pub fn start(&mut self, config: RecordingConfig) -> Result<()> {
        if self.state != RecordingState::Idle && self.state != RecordingState::Stopped {
            return Err(AudioError::InvalidState(format!(
                "cannot start recording from state {:?}",
                self.state
            )));
        }
        self.provider.start_recording(config)?;
        self.state = RecordingState::Recording;
        self.started_at = Some(Instant::now());
        self.paused_at = None;
        self.accumulated_pause = Duration::ZERO;
        Ok(())
    }

    pub fn pause(&mut self) -> Result<()> {
        if self.state != RecordingState::Recording {
            return Err(AudioError::InvalidState(format!(
                "cannot pause from state {:?}",
                self.state
            )));
        }
        self.provider.pause_recording()?;
        self.state = RecordingState::Paused;
        self.paused_at = Some(Instant::now());
        Ok(())
    }

    pub fn resume(&mut self) -> Result<()> {
        if self.state != RecordingState::Paused {
            return Err(AudioError::InvalidState(format!(
                "cannot resume from state {:?}",
                self.state
            )));
        }
        self.provider.resume_recording()?;
        self.state = RecordingState::Recording;
        if let Some(paused_at) = self.paused_at.take() {
            self.accumulated_pause += Instant::now().duration_since(paused_at);
        }
        Ok(())
    }

    pub fn stop(&mut self) -> Result<RecordingResult> {
        if self.state != RecordingState::Recording && self.state != RecordingState::Paused {
            return Err(AudioError::InvalidState(format!(
                "cannot stop from state {:?}",
                self.state
            )));
        }
        let result = self.provider.stop_recording()?;
        self.state = RecordingState::Stopped;
        self.started_at = None;
        self.paused_at = None;
        self.accumulated_pause = Duration::ZERO;
        Ok(result)
    }
}
