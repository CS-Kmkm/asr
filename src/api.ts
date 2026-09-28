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
  HistoryFilter,
  HistoryAudioPayload,
} from "./types";

const inTauri = "__TAURI_INTERNALS__" in window;

export const defaultSettings: Settings = {
  uiLanguage: "ja",
  theme: "system",
  interactionSounds: false,
  speechLocale: null,
  setupComplete: false,
  shortcuts: {
    dictate: ["Ctrl+Shift+Space"],
    translate: ["Ctrl+Shift+Y"],
    ask: ["Ctrl+Shift+A"],
    edit: ["Ctrl+Shift+E"],
  },
  translationHotkey: "Ctrl+Shift+T",
  translationInstruction: "",
  translationTargetLanguages: ["en", "ja"],
  translationTargetLanguage: "en",
  microphoneId: null,
  historyRetention: "one_month",
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
  localCorrectionBaseUrl: "http://127.0.0.1:11434/v1",
  localCorrectionModel: "qwen3:8b",
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

export async function startMicrophoneTest(deviceId: string | null): Promise<void> {
  if (!inTauri) return;
  await invoke("start_microphone_test", { deviceId });
}

export async function stopMicrophoneTest(): Promise<void> {
  if (!inTauri) return;
  await invoke("stop_microphone_test");
}

export async function startRecording(): Promise<void> {
  if (!inTauri) return;
  return invoke("start_recording");
}

export async function startVoiceTranslation(): Promise<void> {
  if (!inTauri) return;
  return invoke("start_voice_translation");
}

export async function startAsk(): Promise<void> {
  if (!inTauri) return;
  return invoke("start_ask");
}

export async function cycleVoiceTranslationTarget(): Promise<string> {
  if (!inTauri) return defaultSettings.translationTargetLanguage;
  return invoke("cycle_voice_translation_target");
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

export async function getShortcutWarning(): Promise<boolean> {
  if (!inTauri) return false;
  return invoke("get_shortcut_warning");
}

export async function updateSettings(settings: Settings): Promise<Settings> {
  if (!inTauri) return settings;
  return invoke("update_settings", { settings });
}

export async function listHistory(filter: HistoryFilter = "all"): Promise<HistoryItem[]> {
  if (!inTauri) return [];
  return invoke("list_history", { filter, limit: 100 });
}

export async function deleteHistoryItem(id: number): Promise<boolean> {
  if (!inTauri) return false;
  return invoke("delete_history_item", { id });
}

export async function deleteAllHistory(): Promise<number> {
  if (!inTauri) return 0;
  return invoke("delete_all_history");
}

export async function getHistoryAudio(id: number): Promise<HistoryAudioPayload> {
  return invoke("get_history_audio", { id });
}

export async function retryHistoryItem(id: number): Promise<RecordingResult> {
  return invoke("retry_history_item", { id });
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

export type AskAnswer = { operationId: number; payload: string };

export async function getAskAnswer(): Promise<AskAnswer | null> {
  if (!inTauri) return null;
  return invoke<AskAnswer | null>("get_ask_answer");
}

export async function dismissAskAnswer(operationId: number): Promise<boolean> {
  if (!inTauri) return false;
  return invoke<boolean>("dismiss_ask_answer", { operationId });
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
