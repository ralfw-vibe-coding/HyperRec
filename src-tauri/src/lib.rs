mod audio;
mod controller;

use std::path::PathBuf;
use std::sync::Mutex;

use audio::{mac::MacAudioProvider, AudioDevice, PermissionStatus, RecordingConfig, RecordingResult};
use chrono::{DateTime, Local};
use controller::{RecordingController, RecordingState};
use tauri::Manager;
use tauri_plugin_dialog::DialogExt;

struct AppState {
    controller: Mutex<RecordingController>,
    last_recording: Mutex<Option<PathBuf>>,
    recording_started_at: Mutex<Option<DateTime<Local>>>,
}

fn default_file_name(started_at: Option<DateTime<Local>>) -> String {
    let when = started_at.unwrap_or_else(Local::now);
    format!("{}.mp3", when.format("%Y-%m-%d_%H-%M-%S"))
}

/// Opens the native save dialog and, if the user picked a destination,
/// encodes the (lossless, temp) WAV `source` to MP3 there. The temp WAV
/// itself is left untouched so the recording can still be saved again.
async fn prompt_save_as(app: &tauri::AppHandle, source: &std::path::Path, default_name: &str) -> Option<String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_file_name(default_name)
        .add_filter("MP3", &["mp3"])
        .save_file(move |path| {
            let _ = tx.send(path);
        });

    let chosen_path = rx.await.ok().flatten()?.into_path().ok()?;
    audio::mixer::encode_wav_to_mp3(source, &chosen_path).ok()?;
    Some(chosen_path.display().to_string())
}

#[tauri::command]
fn list_input_devices(state: tauri::State<AppState>) -> Result<Vec<AudioDevice>, String> {
    state
        .controller
        .lock()
        .unwrap()
        .provider()
        .list_input_devices()
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn list_output_devices(state: tauri::State<AppState>) -> Result<Vec<AudioDevice>, String> {
    state
        .controller
        .lock()
        .unwrap()
        .provider()
        .list_output_devices()
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn check_permissions(state: tauri::State<AppState>) -> Result<PermissionStatus, String> {
    state
        .controller
        .lock()
        .unwrap()
        .provider()
        .check_permissions()
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn request_permissions(state: tauri::State<AppState>) -> Result<PermissionStatus, String> {
    state
        .controller
        .lock()
        .unwrap()
        .provider()
        .request_permissions()
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn recording_state(state: tauri::State<AppState>) -> RecordingState {
    state.controller.lock().unwrap().state()
}

#[tauri::command]
fn recording_elapsed_seconds(state: tauri::State<AppState>) -> f64 {
    state.controller.lock().unwrap().elapsed_seconds()
}

#[tauri::command]
fn start_recording(
    app: tauri::AppHandle,
    state: tauri::State<AppState>,
    input_device_id: String,
    output_device_id: String,
) -> Result<(), String> {
    let temp_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?
        .join("tmp");
    std::fs::create_dir_all(&temp_dir).map_err(|e| e.to_string())?;
    let config = RecordingConfig {
        input_device_id: Some(input_device_id),
        output_device_id: Some(output_device_id),
        temp_dir,
    };
    state
        .controller
        .lock()
        .unwrap()
        .start(config)
        .map_err(|e| e.to_string())?;

    *state.recording_started_at.lock().unwrap() = Some(Local::now());
    Ok(())
}

#[tauri::command]
fn pause_recording(state: tauri::State<AppState>) -> Result<(), String> {
    state.controller.lock().unwrap().pause().map_err(|e| e.to_string())
}

#[tauri::command]
fn resume_recording(state: tauri::State<AppState>) -> Result<(), String> {
    state.controller.lock().unwrap().resume().map_err(|e| e.to_string())
}

#[tauri::command]
fn stop_recording(state: tauri::State<AppState>) -> Result<RecordingResult, String> {
    let result = state.controller.lock().unwrap().stop().map_err(|e| e.to_string())?;
    *state.last_recording.lock().unwrap() = Some(result.temp_file_path.clone());
    Ok(result)
}

/// "Download": saves the last finished recording via the native save
/// dialog. Can be called more than once — the temp file isn't consumed.
#[tauri::command]
async fn save_recording_as(app: tauri::AppHandle, state: tauri::State<'_, AppState>) -> Result<Option<String>, String> {
    let source = state
        .last_recording
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| "keine Aufnahme zum Speichern vorhanden".to_string())?;
    let started_at = *state.recording_started_at.lock().unwrap();
    let default_name = default_file_name(started_at);
    Ok(prompt_save_as(&app, &source, &default_name).await)
}

/// "Verwerfen": deletes the temp recording immediately, no confirmation
/// dialog — the UI's trash button already is that confirmation.
#[tauri::command]
fn discard_recording(state: tauri::State<AppState>) -> Result<(), String> {
    if let Some(path) = state.last_recording.lock().unwrap().take() {
        std::fs::remove_file(&path).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Closing is the only way to quit now that there's no native close
/// button (decorations are off) — the custom title bar's "x" calls this.
#[tauri::command]
fn quit_app(app: tauri::AppHandle) {
    app.exit(0);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let provider = Box::new(MacAudioProvider::new());
            app.manage(AppState {
                controller: Mutex::new(RecordingController::new(provider)),
                last_recording: Mutex::new(None),
                recording_started_at: Mutex::new(None),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_input_devices,
            list_output_devices,
            check_permissions,
            request_permissions,
            recording_state,
            recording_elapsed_seconds,
            start_recording,
            pause_recording,
            resume_recording,
            stop_recording,
            save_recording_as,
            discard_recording,
            quit_app,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            // The window is the app: closing it quits, which is the
            // default. Just make sure no scratch files survive that.
            if let tauri::RunEvent::Exit = event {
                if let Ok(temp_dir) = app_handle.path().app_data_dir() {
                    let _ = std::fs::remove_dir_all(temp_dir.join("tmp"));
                }
            }
        });
}
