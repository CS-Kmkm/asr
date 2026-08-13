export type AppPhase =
  | "idle"
  | "recording"
  | "processing"
  | "injecting"
  | "completed"
  | "error";

export interface AppState {
  phase: AppPhase;
  message: string | null;
  lastResult: string | null;
  updatedAt: string;
}

export interface AudioDevice {
  id: string;
  name: string;
  isDefault: boolean;
}

export interface AudioLevel {
  rms: number;
  peak: number;
}

export interface RecordingResult {
  text: string;
  insertion: string;
  durationMs: number;
  latencyMs: number;
}

export interface Settings {
  setupComplete: boolean;
  hotkey: string;
  microphoneId: string | null;
  historyEnabled: boolean;
  historyRetentionDays: number;
  deleteAudioAfterProcessing: boolean;
  autoStart: boolean;
  clipboardRestore: boolean;
  noiseSuppression: NoiseSuppression;
  inputGainPercent: number;
  automaticGain: boolean;
  modelId: string | null;
  modelQuantization: ModelQuantization;
  asrBackend: AsrBackend;
  apiBaseUrl: string;
  apiKeyEnvVar: string;
  textCorrectionEnabled: boolean;
  correctionProvider: CorrectionProvider;
  openaiCorrectionModel: string;
  openaiApiKeyEnvVar: string;
  geminiCorrectionModel: string;
  geminiApiKeyEnvVar: string;
  correctionInstruction: string;
  correctionRemoveFillers: boolean;
  correctionRemoveRepetitions: boolean;
  correctionResolveSelfCorrections: boolean;
  correctionAutoFormat: boolean;
  correctionImproveClarity: boolean;
  customModels: CustomModel[];
}

export type AsrBackend = "vibevoice" | "faster-whisper" | "openai-compatible";
export type ModelQuantization = "4bit" | "8bit" | "bf16";
export type NoiseSuppression = "off" | "low" | "medium" | "high";
export type CorrectionProvider = "openai" | "gemini";

export interface CustomModel {
  asrBackend: AsrBackend;
  modelId: string;
}

export interface HistoryItem {
  id: number;
  transcriptText: string;
  processedText: string | null;
  mode: string;
  asrProvider: string;
  llmProvider: string | null;
  appCategory: string | null;
  durationMs: number | null;
  latencyMs: number | null;
  createdAt: string;
}

export interface ModelStatus {
  modelId: string | null;
  installed: boolean;
  state: "not_configured" | "not_installed" | "not_loaded" | "ready";
  detail: string;
}

export interface GpuDiagnostics {
  status: "available" | "unavailable" | "unsupported";
  adapterName: string | null;
  driverVersion: string | null;
  memoryTotalMb: number | null;
  recommendation: string;
}

export interface DictionaryEntry {
  id: number;
  reading: string;
  surface: string;
  category: string | null;
  aliases: string[];
  priority: number;
  appScope: string | null;
  createdAt: string;
}

export interface DictionaryEntryInput {
  reading: string;
  surface: string;
  category?: string | null;
  aliases?: string[];
  priority?: number;
  appScope?: string | null;
}
