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
  HistoryFilter,
  HistoryAudioPayload,
  HistoryRetention,
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
  liveTargetInsertion: false,
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
  localCorrectionMaxTokens: 4096,
  correctionInstruction: "",
  correctionMode: "conservative",
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

export async function getStartupHotkeyWarning(): Promise<string[]> {
  if (!inTauri) return [];
  const warning = await invoke<string | null>("get_startup_hotkey_warning");
  return warning === null ? [] : [warning];
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

// Lists the saved shortcuts that are inactive since startup, as
// "Action (chord)" entries; an empty list means every shortcut registered.
export async function getShortcutWarning(): Promise<string[]> {
  if (!inTauri) return [];
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

/** What saving a retention would delete now: History rows and retained recordings. */
export interface HistoryPurgePreview {
  historyItems: number;
  recordings: number;
}

export async function previewHistoryRetentionPurge(retention: HistoryRetention): Promise<HistoryPurgePreview> {
  // The browser preview keeps no History, so nothing would be deleted.
  if (!inTauri) return { historyItems: 0, recordings: 0 };
  return invoke("preview_history_retention_purge", { retention });
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
