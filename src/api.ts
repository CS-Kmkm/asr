import { invoke } from "@tauri-apps/api/core";
import type {
  AppState,
  AudioDevice,
  GpuDiagnostics,
  HistoryItem,
  ModelStatus,
  Settings,
  RecordingResult,
  DictionaryEntry,
  DictionaryEntryInput,
} from "./types";

const inTauri = "__TAURI_INTERNALS__" in window;

export const defaultSettings: Settings = {
  setupComplete: false,
  hotkey: "Ctrl+Shift+Space",
  microphoneId: null,
  historyEnabled: true,
  historyRetentionDays: 30,
  deleteAudioAfterProcessing: true,
  autoStart: false,
  clipboardRestore: true,
  modelId: null,
  modelQuantization: "4bit",
  asrBackend: "faster-whisper",
  apiBaseUrl: "https://api.openai.com/v1",
  apiKeyEnvVar: "OPENAI_API_KEY",
  textCorrectionEnabled: false,
  correctionProvider: "openai",
  openaiCorrectionModel: "gpt-5.6-luna",
  openaiApiKeyEnvVar: "OPENAI_API_KEY",
  geminiCorrectionModel: "gemini-flash-lite-latest",
  geminiApiKeyEnvVar: "GEMINI_API_KEY",
  correctionInstruction: "",
  correctionRemoveFillers: true,
  correctionRemoveRepetitions: true,
  correctionResolveSelfCorrections: true,
  correctionAutoFormat: true,
  correctionImproveClarity: true,
  customModels: [],
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

export async function loadModel(
  modelId: string | null,
  quantization: Settings["modelQuantization"],
): Promise<ModelStatus> {
  if (!inTauri) return getModelStatus();
  return invoke("load_model", {
    request: { modelId, quantization },
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

export async function listDictionary(): Promise<DictionaryEntry[]> {
  if (!inTauri) return [];
  return invoke("list_dictionary");
}

export async function addDictionaryEntry(entry: DictionaryEntryInput): Promise<DictionaryEntry> {
  if (!inTauri) {
    return {
      id: Date.now(),
      reading: entry.reading,
      surface: entry.surface,
      category: entry.category ?? null,
      aliases: entry.aliases ?? [],
      priority: entry.priority ?? 0,
      appScope: entry.appScope ?? null,
      createdAt: new Date().toISOString(),
    };
  }
  return invoke("add_dictionary_entry", { entry });
}

export async function deleteDictionaryEntry(id: number): Promise<void> {
  if (!inTauri) return;
  await invoke("delete_dictionary_entry", { id });
}
