import { invoke } from "@tauri-apps/api/core";
import type {
  AppState,
  AudioDevice,
  GpuDiagnostics,
  HistoryItem,
  ModelStatus,
  Settings,
  RecordingResult,
  DictionaryCandidate, DictionaryEntry,
  DictionaryEntryInput,
} from "./types";

const inTauri = "__TAURI_INTERNALS__" in window;

export const defaultSettings: Settings = {
  uiLanguage: "ja",
  setupComplete: false,
  hotkey: "Ctrl+Shift+Space",
  translationHotkey: "Ctrl+Shift+T",
  translationInstruction: "",
  microphoneId: null,
  historyEnabled: true,
  historyRetentionDays: 30,
  deleteAudioAfterProcessing: true,
  autoStart: false,
  clipboardRestore: true,
  noiseSuppression: "medium",
  inputGainPercent: 100,
  automaticGain: true,
  modelId: null,
  modelQuantization: "4bit",
  asrBackend: "faster-whisper",
  apiBaseUrl: "https://api.openai.com/v1",
  apiKeyEnvVar: "OPENAI_API_KEY",
  textCorrectionEnabled: false,
  correctionProvider: "openai",
  openaiCorrectionModel: "gpt-5.6-luna",
  openaiReasoningEffort: "none",
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
  personalizationEnabled: false,
  globalStyleProfile: null,
  scopedStyleProfiles: [],
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

export async function copyToClipboard(text: string): Promise<void> {
  if (inTauri) {
    await invoke("copy_to_clipboard", { text });
    return;
  }
  await navigator.clipboard.writeText(text);
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

export async function getGpuDiagnostics(): Promise<GpuDiagnostics> {
  if (!inTauri) return runGpuDiagnostics();
  return invoke("get_gpu_diagnostics");
}

export async function listDictionary(query?: string, source?: "manual" | "auto" | "all"): Promise<DictionaryEntry[]> {
  if (!inTauri) return [];
  return invoke("list_dictionary", { query: query || null, source: source === "all" ? null : source || null });
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
      source: "manual",
      createdAt: new Date().toISOString(),
    };
  }
  return invoke("add_dictionary_entry", { entry });
}

export async function updateDictionaryEntry(id: number, entry: DictionaryEntryInput): Promise<DictionaryEntry> {
  if (!inTauri) return { ...(await addDictionaryEntry(entry)), id };
  return invoke("update_dictionary_entry", { id, entry });
}

export async function importDictionaryCsv(csv: string): Promise<number> {
  if (!inTauri) return 0;
  return invoke("import_dictionary_csv", { input: { csv } });
}

export async function listDictionaryCandidates(): Promise<DictionaryCandidate[]> {
  if (!inTauri) return [];
  return invoke("list_dictionary_candidates");
}

export async function confirmDictionaryCandidate(id: number): Promise<DictionaryEntry> {
  if (!inTauri) throw new Error("Dictionary candidates require the desktop application.");
  return invoke("confirm_dictionary_candidate", { id });
}

export async function rejectDictionaryCandidate(id: number): Promise<void> {
  if (!inTauri) return;
  await invoke("reject_dictionary_candidate", { id });
}

export async function deleteDictionaryEntry(id: number): Promise<void> {
  if (!inTauri) return;
  await invoke("delete_dictionary_entry", { id });
}
