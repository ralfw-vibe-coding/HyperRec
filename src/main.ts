import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow, LogicalSize } from "@tauri-apps/api/window";

type UiState = "ready" | "recording" | "paused" | "recorded";

interface AudioDevice {
  id: string;
  name: string;
  is_default: boolean;
}

interface RecordingResult {
  temp_file_path: string;
  duration_seconds: number;
}

const PAUSE_ICON = `<svg viewBox="0 0 32 32" fill="none" stroke="currentColor" stroke-width="3" stroke-linecap="round" aria-hidden="true"><path d="M11 7v18M21 7v18"/></svg>`;
const RESUME_ICON = `<svg viewBox="0 0 32 32" fill="none" stroke="currentColor" stroke-width="2.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M11 7l13 9-13 9z"/></svg>`;

// The card is taller in ready/paused (device selects visible) than in
// recording/recorded (selects hidden) — the window has to follow that,
// otherwise one state always leaves leftover space below the card.
const WINDOW_WIDTH = 250;
const WINDOW_HEIGHT_WITH_SELECTS = 98;
const WINDOW_HEIGHT_WITHOUT_SELECTS = 46;

let recorderEl: HTMLElement | null;
let timerEl: HTMLElement | null;
let recordButton: HTMLButtonElement | null;
let stopButton: HTMLButtonElement | null;
let pauseResumeButton: HTMLButtonElement | null;
let downloadButton: HTMLButtonElement | null;
let trashButton: HTMLButtonElement | null;
let inputSelect: HTMLSelectElement | null;
let outputSelect: HTMLSelectElement | null;
let statusMessageEl: HTMLElement | null;

let uiState: UiState = "ready";
let pollHandle: number | null = null;
let pollGeneration = 0;
let busy = false;

function formatElapsed(totalSeconds: number): string {
  const seconds = Math.max(0, Math.floor(totalSeconds));
  const h = Math.floor(seconds / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  const s = seconds % 60;
  const pad = (n: number) => n.toString().padStart(2, "0");
  return `${pad(h)}:${pad(m)}:${pad(s)}`;
}

function setUiState(next: UiState) {
  uiState = next;
  if (recorderEl) recorderEl.className = `recorder ${next}`;
  recordButton?.classList.toggle("hidden", next !== "ready");
  stopButton?.classList.toggle("hidden", next === "ready" || next === "recorded");
  pauseResumeButton?.classList.toggle("hidden", next === "ready" || next === "recorded");
  downloadButton?.classList.toggle("hidden", next !== "recorded");
  trashButton?.classList.toggle("hidden", next !== "recorded");

  const showsDeviceSelects = next === "ready" || next === "paused";
  const height = showsDeviceSelects ? WINDOW_HEIGHT_WITH_SELECTS : WINDOW_HEIGHT_WITHOUT_SELECTS;
  applyWindowSize(height);
}

async function applyWindowSize(height: number) {
  const appWindow = getCurrentWindow();
  const size = new LogicalSize(WINDOW_WIDTH, height);
  try {
    await appWindow.setResizable(false);
    await appWindow.setMinSize(null);
    await appWindow.setMaxSize(null);
    await appWindow.setSize(size);
    await appWindow.setMinSize(size);
    await appWindow.setMaxSize(size);
    await appWindow.setResizable(false);
  } catch {
    // window may not be ready yet on the very first call; harmless
  }
}

function setPauseIcon(isPaused: boolean) {
  if (!pauseResumeButton) return;
  pauseResumeButton.setAttribute("aria-label", isPaused ? "Aufnahme fortsetzen" : "Aufnahme pausieren");
  pauseResumeButton.innerHTML = isPaused ? RESUME_ICON : PAUSE_ICON;
}

function flashSuccess(button: HTMLButtonElement | null) {
  if (!button) return;
  button.classList.add("flash-success");
  setTimeout(() => button.classList.remove("flash-success"), 700);
}

// The status line takes up zero space until it actually has something
// to say (see .status-message in styles.css) — never reserve a blank row.
function setStatus(text: string) {
  if (!statusMessageEl) return;
  statusMessageEl.textContent = text;
  statusMessageEl.classList.toggle("visible", text.length > 0);
}

function fillSelect(select: HTMLSelectElement, devices: AudioDevice[]) {
  select.innerHTML = "";
  for (const device of devices) {
    const option = document.createElement("option");
    option.value = device.id;
    option.textContent = device.name;
    if (device.is_default) option.selected = true;
    select.appendChild(option);
  }
}

async function loadDevices() {
  if (!inputSelect || !outputSelect) return;
  try {
    const [inputDevices, outputDevices] = await Promise.all([
      invoke<AudioDevice[]>("list_input_devices"),
      invoke<AudioDevice[]>("list_output_devices"),
    ]);
    fillSelect(inputSelect, inputDevices);
    fillSelect(outputSelect, outputDevices);
  } catch (error) {
    setStatus(`Geräte konnten nicht geladen werden: ${error}`);
  }
}

async function pollTimer(generation: number) {
  try {
    const elapsed = await invoke<number>("recording_elapsed_seconds");
    if (generation !== pollGeneration || pollHandle === null) return;
    if (uiState !== "recording" && uiState !== "paused") return;
    if (timerEl) timerEl.textContent = formatElapsed(elapsed);
  } catch {
    // ignore transient errors while the backend is mid-transition
  }
}

function startPolling() {
  stopPolling();
  const generation = ++pollGeneration;
  pollHandle = window.setInterval(() => pollTimer(generation), 250);
  pollTimer(generation);
}

function stopPolling() {
  pollGeneration++;
  if (pollHandle !== null) {
    window.clearInterval(pollHandle);
    pollHandle = null;
  }
}

async function withBusyGuard(action: () => Promise<void>) {
  if (busy) return;
  busy = true;
  try {
    await action();
  } finally {
    busy = false;
  }
}

async function startRecording() {
  await withBusyGuard(async () => {
    if (!inputSelect || !outputSelect) return;
    try {
      if (timerEl) timerEl.textContent = "00:00:00";
      await invoke("start_recording", {
        inputDeviceId: inputSelect.value,
        outputDeviceId: outputSelect.value,
      });
      setStatus("");
      setUiState("recording");
      setPauseIcon(false);
      startPolling();
    } catch (error) {
      setStatus(`Aufnahme konnte nicht gestartet werden: ${error}`);
    }
  });
}

async function togglePauseResume() {
  await withBusyGuard(async () => {
    try {
      if (uiState === "paused") {
        await invoke("resume_recording");
        setUiState("recording");
        setPauseIcon(false);
      } else {
        await invoke("pause_recording");
        setUiState("paused");
        setPauseIcon(true);
      }
    } catch (error) {
      setStatus(String(error));
    }
  });
}

async function stopRecording() {
  await withBusyGuard(async () => {
    try {
      stopPolling();
      const result = await invoke<RecordingResult>("stop_recording");
      setUiState("recorded");
      if (timerEl) timerEl.textContent = formatElapsed(result.duration_seconds);
      setStatus("");
    } catch (error) {
      if (uiState === "recording" || uiState === "paused") startPolling();
      setStatus(`Stop fehlgeschlagen: ${error}`);
    }
  });
}

async function downloadRecording() {
  await withBusyGuard(async () => {
    try {
      const savedTo = await invoke<string | null>("save_recording_as");
      if (savedTo) {
        setStatus("");
        flashSuccess(downloadButton);
      } else {
        setStatus("Speichern abgebrochen.");
      }
    } catch (error) {
      setStatus(`Speichern fehlgeschlagen: ${error}`);
    }
  });
}

async function discardRecording() {
  await withBusyGuard(async () => {
    try {
      await invoke("discard_recording");
      if (timerEl) timerEl.textContent = "00:00:00";
      setUiState("ready");
      setStatus("");
    } catch (error) {
      setStatus(`Verwerfen fehlgeschlagen: ${error}`);
    }
  });
}

window.addEventListener("DOMContentLoaded", () => {
  recorderEl = document.querySelector("#recorder");
  timerEl = document.querySelector("#timer");
  recordButton = document.querySelector("#record-button");
  stopButton = document.querySelector("#stop-button");
  pauseResumeButton = document.querySelector("#pause-resume-button");
  downloadButton = document.querySelector("#download-button");
  trashButton = document.querySelector("#trash-button");
  inputSelect = document.querySelector("#input-device-select");
  outputSelect = document.querySelector("#output-device-select");
  statusMessageEl = document.querySelector("#status-message");

  recordButton?.addEventListener("click", startRecording);
  stopButton?.addEventListener("click", stopRecording);
  pauseResumeButton?.addEventListener("click", togglePauseResume);
  downloadButton?.addEventListener("click", downloadRecording);
  trashButton?.addEventListener("click", discardRecording);
  document.querySelector("#close-button")?.addEventListener("click", () => invoke("quit_app"));

  setUiState("ready");
  loadDevices();
});
