import { invoke } from "@tauri-apps/api/core";
import type {
  AppState,
  AudioDevice,
  GpuDiagnostics,
  HistoryItem,
  ModelStatus,
  Settings,
  RecordingResult,
} from "./types";

const inTauri = "__TAURI_INTERNALS__" in window;

export const defaultSettings: Settings = {
  setupComplete: false,
  hotkey: "Ctrl+Shift+Space",
  microphoneId: null,
  historyEnabled: true,
  historyRetentionDays: 30,
  deleteAudioAfterProcessing: true,
  cloudEnabled: false,
  autoStart: false,
  clipboardRestore: true,
  modelId: null,
  asrBackend: "vibevoice",
};

export async function getAppState(): Promise<AppState> {
  if (!inTauri) {
    return { phase: "idle", message: null, lastResult: null, updatedAt: new Date().toISOString() };
  }
  return invoke("get_app_state");
}

export async function listAudioDevices(): Promise<AudioDevice[]> {
  if (!inTauri) return [];
  return invoke("list_audio_devices");
}

export async function startRecording(): Promise<void> {
  if (!inTauri) return;
  return invoke("start_recording");
}

export async function stopRecording(): Promise<RecordingResult | null> {
  if (!inTauri) return null;
  return invoke("stop_recording");
}

export async function cancelRecording(): Promise<void> {
  if (!inTauri) return;
  return invoke("cancel_recording");
}

export async function getSettings(): Promise<Settings> {
  if (!inTauri) return defaultSettings;
  return invoke("get_settings");
}

export async function updateSettings(settings: Settings): Promise<Settings> {
  if (!inTauri) return settings;
  return invoke("update_settings", { settings });
}

export async function listHistory(): Promise<HistoryItem[]> {
  if (!inTauri) return [];
  return invoke("list_history", { limit: 100 });
}

export async function copyHistoryItem(id: number): Promise<void> {
  if (!inTauri) return;
  await invoke("copy_history_item", { id });
}

export async function getModelStatus(): Promise<ModelStatus> {
  if (!inTauri) {
    return {
      modelId: null,
      installed: false,
      state: "not_loaded",
      detail: "Choose a local ASR model during setup.",
    };
  }
  return invoke("get_model_status");
}

export async function loadModel(quantization = "4bit"): Promise<ModelStatus> {
  if (!inTauri) return getModelStatus();
  return invoke("load_model", {
    request: { modelId: "microsoft/VibeVoice-ASR-HF", quantization },
  });
}

export async function runGpuDiagnostics(): Promise<GpuDiagnostics> {
  if (!inTauri) {
    return {
      status: "unsupported",
      adapterName: null,
      driverVersion: null,
      memoryTotalMb: null,
      recommendation: "GPU diagnostics run in the Windows desktop application.",
    };
  }
  return invoke("run_gpu_diagnostics");
}
