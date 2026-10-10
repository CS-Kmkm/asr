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
  uiLanguage: UiLanguage;
  theme: Theme;
  interactionSounds: boolean;
  speechLocale: SpeechLocale | null;
  setupComplete: boolean;
  shortcuts: Record<ShortcutMode, string[]>;
  translationHotkey: string;
  translationInstruction: string;
  translationTargetLanguages: string[];
  translationTargetLanguage: string;
  microphoneId: string | null;
  historyRetention: HistoryRetention;
  deleteAudioAfterProcessing: boolean;
  /** Keep a failed take with its audio for 24 hours for Retry, even where History or audio retention is off. */
  keepFailedTakes: boolean;
  autoStart: boolean;
  clipboardRestore: boolean;
  liveTargetInsertion: boolean;
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
  openaiReasoningEffort: OpenAIReasoningEffort;
  openaiApiKeyEnvVar: string;
  geminiCorrectionModel: string;
  geminiApiKeyEnvVar: string;
  localCorrectionBaseUrl: string;
  localCorrectionModel: string;
  localCorrectionMaxTokens: number;
  correctionInstruction: string;
  correctionMode: CorrectionMode;
  correctionRemoveFillers: boolean;
  correctionRemoveRepetitions: boolean;
  correctionResolveSelfCorrections: boolean;
  correctionAutoFormat: boolean;
  correctionImproveClarity: boolean;
  customModels: CustomModel[];
  personalizationEnabled: boolean;
  globalStyleProfile: StyleProfile | null;
  scopedStyleProfiles: ScopedStyleProfile[];
}
export interface StyleProfile { formality: "formal" | "casual"; detail: "concise" | "detailed"; guidance?: string | null; }
export interface ScopedStyleProfile { scope: string; profile: StyleProfile; }

export const uiLocaleRegistry = [
  { tag: "en", label: "English" },
  { tag: "ja", label: "Japanese" },
] as const;
export type UiLanguage = (typeof uiLocaleRegistry)[number]["tag"];
export type Theme = "system" | "light" | "dark";
export type ShortcutMode = "dictate" | "translate" | "ask" | "edit";
export const speechLocaleRegistry = [
  { tag: "en-US", label: "English (United States)" },
  { tag: "en-GB", label: "English (United Kingdom)" },
  { tag: "zh-CN", label: "Chinese (Simplified)" },
  { tag: "zh-TW", label: "Chinese (Traditional)" },
  { tag: "es-ES", label: "Spanish (Spain)" },
  { tag: "es-MX", label: "Spanish (Mexico)" },
  { tag: "fr-FR", label: "French (France)" },
  { tag: "fr-CA", label: "French (Canada)" },
  { tag: "pt-BR", label: "Portuguese (Brazil)" },
  { tag: "pt-PT", label: "Portuguese (Portugal)" },
] as const;
export type SpeechLocale = (typeof speechLocaleRegistry)[number]["tag"];
export type HistoryRetention = "never" | "24_hours" | "one_week" | "one_month" | "one_year" | "forever";
export type HistoryFilter = "all" | "dictate" | "translate" | "edit" | "ask";

export type AsrBackend = "vibevoice" | "faster-whisper" | "openai-compatible";
export type ModelQuantization = "4bit" | "8bit" | "bf16";
export type NoiseSuppression = "off" | "low" | "medium" | "high";
export type CorrectionProvider = "openai" | "gemini" | "local";
export type CorrectionMode = "conservative" | "intent_aware";
export type OpenAIReasoningEffort = "none" | "low" | "medium" | "high" | "xhigh" | "max";

export interface CustomModel {
  asrBackend: AsrBackend;
  modelId: string;
}

export interface HistoryItem {
  id: number;
  transcriptText: string;
  processedText: string | null;
  sourceText: string | null;
  instructionText: string | null;
  actionKind: string | null;
  searchSite: string | null;
  mode: string;
  asrProvider: string;
  llmProvider: string | null;
  targetLanguage: string | null;
  appCategory: string | null;
  durationMs: number | null;
  latencyMs: number | null;
  createdAt: string;
  hasAudio: boolean;
  retryOfId: number | null;
  insertionResult: string | null;
  insertionDetail: string | null;
  /** Set only on a failed take kept for Retry outside the History settings: when it is deleted. */
  expiresAt: string | null;
}

export interface HistoryAudioPayload {
  bytes: number[];
  filename: string;
  mimeType: string;
}

export interface ModelStatus {
  modelId: string | null;
  installed: boolean;
  state: "not_configured" | "not_installed" | "not_loaded" | "loading" | "ready" | "error";
  detail: string;
}

// Progress reported while the ASR worker prepares the model. Byte counts are
// absent until the download size is known, and for backends that cannot
// measure it.
export interface ModelProgress {
  stage: "download" | "load";
  model: string | null;
  completedBytes: number | null;
  totalBytes: number | null;
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
  source: "manual" | "auto";
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

export interface DictionaryCandidate {
  id: number;
  originalSpan: string;
  preferredSpan: string;
  /** Fixed marker of the rule-based detector, not a measured probability; do not display it as one. */
  confidence: number;
  historyId: number | null;
  createdAt: string;
}
