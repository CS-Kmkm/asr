import { useNoticeQueue, type Notice } from "./hooks/useNoticeQueue";
import { useSettings } from "./hooks/useSettings";
import { useAudioDevices } from "./hooks/useAudioDevices";
import { useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import {
  addDictionaryEntry,
  cancelRecording,
  copyToClipboard,
  cycleVoiceTranslationTarget,
  defaultSettings,
  deleteDictionaryEntry,
  deleteHistoryItem,
  deleteAllHistory,
  getHistoryAudio,
  retryHistoryItem,
  reprocessHistoryText,
  confirmDictionaryCandidate,
  importDictionaryCsv,
  listDictionaryCandidates,
  rejectDictionaryCandidate,
  getAppState,
  getAskAnswer,
  dismissAskAnswer,
  getGpuDiagnostics,
  getModelStatus,
  getUiPreferences,
  getStartupHotkeyWarning,
  getShortcutWarning,
  listDictionary,
  listHistory,
  listHistoryPage,
  runGpuDiagnostics,
  openDiagnosticsFolder,
  loadModel,
  startRecording,
  stopRecording,
  updateDictionaryEntry,
} from "./api";
import type {
  AppState,
  AsrBackend,
  AudioLevel,
  CustomModel,
  DictionaryEntry,
  DictionaryCandidate,
  DictionaryEntryInput,
  GpuDiagnostics,
  HistoryItem,
  HistoryFilter,
  ModelProgress,
  ModelStatus,
  Settings,
  SettingsSnapshot,
} from "./types";
import { ModelProgressBar } from "./components/ui";
import { DashboardPage } from "./pages/DashboardPage";
import { SetupPage } from "./pages/SetupPage";
import { SettingsPage } from "./pages/SettingsPage";
import { ModelsPage } from "./pages/ModelsPage";
import { HistoryPage } from "./pages/HistoryPage";
import { DictionaryPage } from "./pages/DictionaryPage";
import { PrivacyPage } from "./pages/PrivacyPage";
import { DiagnosticsPage } from "./pages/DiagnosticsPage";
import {
  I18nProvider,
  insertionDetailLabels,
  insertionOutcomeNotices,
  isAppMessage,
  translate,
  translateAppMessage,
  useI18n,
  type MessageKey,
} from "./i18n";

const asrBackendOptions: Array<{ value: AsrBackend; label: MessageKey }> = [
  { value: "faster-whisper", label: "faster-whisper (recommended / CPU supported)" },
  { value: "vibevoice", label: "VibeVoice (CUDA GPU required)" },
  { value: "openai-compatible", label: "OpenAI-compatible API" },
];

type Page =
  | "setup"
  | "dashboard"
  | "settings"
  | "models"
  | "history"
  | "dictionary"
  | "privacy"
  | "diagnostics";

const pages: Array<{ id: Page; label: MessageKey }> = [
  { id: "dashboard", label: "Status" },
  { id: "setup", label: "Setup" },
  { id: "models", label: "Models" },
  { id: "settings", label: "Settings" },
  { id: "history", label: "History" },
  { id: "dictionary", label: "Dictionary" },
  { id: "privacy", label: "Privacy" },
  { id: "diagnostics", label: "Diagnostics" },
];

const phaseMessageKeys: Record<AppState["phase"], MessageKey> = {
  idle: "Idle",
  recording: "Recording",
  processing: "Processing",
  injecting: "Injecting",
  completed: "Completed",
  error: "Error",
};

const windowLabel = "__TAURI_INTERNALS__" in window ? getCurrentWebviewWindow().label : "main";
const isRecordingOverlay = windowLabel === "recording-overlay";
const isAskAnswer = windowLabel === "ask-answer";
const OVERLAY_WAVE_BAR_COUNT = 9;
const OVERLAY_PREVIEW_CHARS = 140;

interface CorrectionPreview {
  text: string;
  stage: "draft" | "streaming" | "final" | "fallback";
}

interface VoiceModeEvent {
  // "selected_translate" is the selected-text translation, which has no recording.
  mode: "dictate" | "translate" | "edit" | "ask" | "selected_translate";
  targetLanguage: string | null;
}

// Fixed outcome and reason codes for a non-confirmed insertion.
interface InsertionNoticeEvent {
  outcome: string;
  detail: string | null;
}

// Matches the backend timer that hides the overlay window.
const INSERTION_NOTICE_MS = 5_000;

// The Processing message of a hotkey start that waits for the speech model.

// The Processing message of a selected-text translation.

// A failure or warning sentence from the backend, shown while the main
// window may be hidden. The backend chooses how long it stays up.
interface OverlayMessageEvent {
  severity: "failure" | "warning";
  message: string;
  durationMs: number;
}

// A refused hotkey press, flashed inside the live view without replacing it.
interface OverlayHintEvent {
  message: string;
  durationMs: number;
}

type OverlayNotice =
  | ({ kind: "insertion" } & InsertionNoticeEvent)
  | ({ kind: "message" } & OverlayMessageEvent);

const translationLanguageKeys: Record<string, MessageKey> = {
  en: "English",
  ja: "Japanese",
  zh: "Chinese",
  es: "Spanish",
  fr: "French",
  pt: "Portuguese",
  de: "German",
  ko: "Korean",
};

// Notices that only describe model preparation and are cleared once it ends.
const MODEL_PREPARATION_KINDS = ["model_loading", "model_downloading", "worker_setup"];

const WARNING_STATUS_KINDS = new Set([
  "artifact_cleanup_failed",
  "history_audio_unavailable",
  "history_audio_and_metric_save_failed",
  "history_save_failed",
  "autostart_update_failed",
  "clipboard_only",
  "gpu_unavailable",
  "history_and_metric_save_failed",
  "history_metric_save_failed",
  "hotkey_unavailable",
  "metric_save_failed",
  "microphone_fallback",
  "streaming_insertion_unavailable",
  "text_correction_failed",
  "voice_translation_failed",
]);

const SUCCESS_STATUS_KINDS = new Set([
  "clipboard_paste",
  "gpu_available",
  "provisional_replace",
  "success",
]);

function statusSeverity(kind: string): Notice["severity"] {
  if (kind === "error" || kind === "model_load_failed" || kind === "ai_provider_failed") return "error";
  if (WARNING_STATUS_KINDS.has(kind)) return "warning";
  if (SUCCESS_STATUS_KINDS.has(kind)) return "success";
  return "info";
}

function compactOverlayPreview(text: string): string {
  const characters = Array.from(text.trim());
  return characters.length > OVERLAY_PREVIEW_CHARS
    ? `…${characters.slice(-OVERLAY_PREVIEW_CHARS).join("")}`
    : characters.join("");
}

if (isRecordingOverlay) {
  document.body.classList.add("recording-overlay-body");
}

function AskAnswerPanel() {
  const { language, t } = useI18n();
  const [answer, setAnswer] = useState("");
  const [answerKind, setAnswerKind] = useState<"generated" | "notice">("generated");
  const [operationId, setOperationId] = useState(0);
  const operationRef = useRef(0);
  useEffect(() => {
    let active = true;
    const setup = async () => {
      const unlisten = await listen<{ operationId: number; payload: string; kind: "generated" | "notice" }>("ask-answer", ({ payload }) => {
        if (!active || payload.operationId < operationRef.current) return;
        operationRef.current = payload.operationId;
        setOperationId(payload.operationId);
        setAnswer(payload.payload);
        setAnswerKind(payload.kind);
      });
      if (!active) {
        unlisten();
        return;
      }
      const current = await getAskAnswer();
      if (active && current && current.operationId >= operationRef.current) {
        operationRef.current = current.operationId;
        setOperationId(current.operationId);
        setAnswer(current.payload);
        setAnswerKind(current.kind);
      }
      return unlisten;
    };
    let unlisten: (() => void) | undefined;
    void setup().then((cleanup) => { unlisten = cleanup; if (!active) cleanup?.(); });
    return () => { active = false; unlisten?.(); };
  }, []);
  const dismiss = async () => {
    await dismissAskAnswer(operationId);
  };
  const displayedAnswer = answerKind === "notice" ? translateAppMessage(language, answer) ?? answer : answer;
  return <main className="ask-answer-panel" aria-live="polite">
    <p className="eyebrow">{t("Ask Anything")}</p><div className="ask-answer-text">{displayedAnswer}</div>
    <div className="ask-answer-actions"><button className="secondary" type="button" disabled={!answer} onClick={() => void copyToClipboard(displayedAnswer)}>{t("Copy")}</button><button className="secondary" type="button" disabled={!answer} onClick={() => void dismiss()}>{t("Dismiss")}</button></div>
  </main>;
}

function RecordingOverlay() {
  const { language, t } = useI18n();
  const [waveform, setWaveform] = useState<number[]>(
    () => Array(OVERLAY_WAVE_BAR_COUNT).fill(0),
  );
  const [phase, setPhase] = useState<AppState["phase"]>("idle");
  const [stage, setStage] = useState<AppState["stage"]>("idle");
  const previousPhase = useRef<AppState["phase"]>("idle");
  const [message, setMessage] = useState<string | null>(null);
  const [messageCode, setMessageCode] = useState<string | null>(null);
  const [preview, setPreview] = useState<CorrectionPreview | null>(null);
  const [voiceMode, setVoiceMode] = useState<VoiceModeEvent>({
    mode: "dictate",
    targetLanguage: null,
  });
  const [notice, setNotice] = useState<OverlayNotice | null>(null);
  const noticeTimer = useRef<number | undefined>(undefined);
  const [hint, setHint] = useState<string | null>(null);
  const hintTimer = useRef<number | undefined>(undefined);

  useEffect(() => {
    // The latest notice replaces any earlier one, like the backend window.
    const showNotice = (next: OverlayNotice, durationMs: number) => {
      window.clearTimeout(noticeTimer.current);
      setNotice(next);
      noticeTimer.current = window.setTimeout(() => setNotice(null), durationMs);
    };
    const listeners = Promise.all([
      listen<InsertionNoticeEvent>("insertion-notice", ({ payload }) => {
        showNotice({ kind: "insertion", ...payload }, INSERTION_NOTICE_MS);
      }),
      listen<OverlayMessageEvent>("overlay-message", ({ payload }) => {
        showNotice({ kind: "message", ...payload }, payload.durationMs);
      }),
      listen<OverlayHintEvent>("overlay-hint", ({ payload }) => {
        window.clearTimeout(hintTimer.current);
        setHint(payload.message);
        hintTimer.current = window.setTimeout(() => setHint(null), payload.durationMs);
      }),
      listen<AudioLevel>("audio-level", ({ payload }) => {
        const rms = Number.isFinite(payload.rms) ? payload.rms : 0;
        const peak = Number.isFinite(payload.peak) ? payload.peak : 0;
        const inputLevel = Math.max(rms * 3.5, peak);
        const normalizedLevel = inputLevel < 0.015 ? 0 : Math.min(1, inputLevel);
        setWaveform((current) => [...current.slice(1), normalizedLevel]);
      }),
      listen<AppState>("app-state", ({ payload }) => {
        const recordingStarted = payload.phase === "recording" && previousPhase.current !== "recording";
        previousPhase.current = payload.phase;
        setPhase(payload.phase);
        setStage(payload.stage);
        setMessage(payload.message);
        setMessageCode(payload.messageCode);
        if (recordingStarted) {
          setWaveform(Array(OVERLAY_WAVE_BAR_COUNT).fill(0));
          setPreview(null);
          window.clearTimeout(noticeTimer.current);
          setNotice(null);
        } else if (
          payload.phase === "processing" &&
          (["preparing_model", "finishing_audio", "transcribing", "retrying"].includes(payload.stage)
            || (payload.stage === "translating" && payload.operation === "selected_text"))
        ) {
          setPreview(null);
        }
      }),
      listen<CorrectionPreview>("correction-preview", ({ payload }) => {
        setPreview((current) => {
          if (payload.stage !== "streaming") return payload;
          return {
            ...payload,
            text: current?.stage === "streaming" ? current.text + payload.text : payload.text,
          };
        });
      }),
      listen<VoiceModeEvent>("voice-mode", ({ payload }) => setVoiceMode(payload)),
    ]);

    return () => {
      window.clearTimeout(noticeTimer.current);
      window.clearTimeout(hintTimer.current);
      void listeners.then((unlisten) => unlisten.forEach((fn) => fn()));
    };
  }, []);

  if (phase !== "recording" && phase !== "processing" && phase !== "injecting" && notice?.kind === "message") {
    const failed = notice.severity === "failure";
    return (
      <div className={`recording-overlay notice message${failed ? " failed" : ""}`} role="alert" aria-live="assertive">
        <span className="notice-mark" aria-hidden="true" />
        <div className="processing-copy">
          <span className="recording-overlay-label">{t(failed ? "Not completed" : "Completed with a warning")}</span>
          <span className="notice-detail">
            {isAppMessage(notice.message)
              ? translateAppMessage(language, notice.message)
              : t("Open the app window for details.")}
          </span>
        </div>
      </div>
    );
  }

  if (phase !== "recording" && phase !== "processing" && phase !== "injecting" && notice?.kind === "insertion") {
    // A lost previous clipboard does not change how the insertion went.
    const outcome = notice.outcome.replace(/_not_restored$/, "");
    const failed = outcome === "insertion_failed";
    // The paste was sent; only its confirmation is unavailable in this field.
    const sent = outcome === "paste_unverified";
    const outcomeKey = insertionOutcomeNotices[notice.outcome];
    const detailKey = notice.detail ? insertionDetailLabels[notice.detail] : undefined;
    return (
      <div className={`recording-overlay notice${failed ? " failed" : sent ? " sent" : ""}`} role="status" aria-live="polite">
        <span className="notice-mark" aria-hidden="true" />
        <div className="processing-copy">
          <span className="recording-overlay-label">{outcomeKey ? t(outcomeKey) : notice.outcome}</span>
          {notice.detail && <span className="notice-detail">{detailKey ? t(detailKey) : notice.detail}</span>}
        </div>
      </div>
    );
  }

  // Cancels like Esc: nothing is inserted. Clicking never focuses the overlay.
  const cancelButton = (
    <button
      className="overlay-cancel"
      type="button"
      title={t("Cancel (Esc)")}
      aria-label={t("Cancel (Esc)")}
      onClick={() => void cancelRecording().catch(() => undefined)}
    >
      ×
    </button>
  );

  if (phase === "recording") {
    return (
      <div className="recording-overlay recording" role="status" aria-label={t("Recording in progress")}>
        <span className="recording-live-dot" aria-hidden="true" />
        <span
          className={`recording-overlay-label${hint ? " overlay-hint" : ""}`}
          role={hint ? "alert" : undefined}
        >
          {hint
            ? translateAppMessage(language, hint)
            : voiceMode.mode === "translate"
            ? t("Translating")
            : voiceMode.mode === "edit"
              ? t("Editing")
              : voiceMode.mode === "ask"
                ? t("Ask Anything")
              : t("Listening")}
        </span>
        <span className="recording-wave" aria-hidden="true">
          {waveform.map((amplitude, index) => (
            <i
              key={index}
              style={{
                height: `${3 + amplitude * 14}px`,
                opacity: 0.45 + amplitude * 0.55,
              }}
            />
          ))}
        </span>
        {voiceMode.mode === "translate" && voiceMode.targetLanguage && (
          <button
            className="translation-target-button"
            type="button"
            title={t("Cycle the target language for this recording.")}
            onClick={() => void cycleVoiceTranslationTarget().catch(() => undefined)}
          >
            {t(translationLanguageKeys[voiceMode.targetLanguage] ?? "Language")} ↻
          </button>
        )}
        {cancelButton}
      </div>
    );
  }

  if (phase !== "processing" && phase !== "injecting") return null;

  const compactPreview = compactOverlayPreview(preview?.text ?? "");
  // A start waiting for the model has not published its voice mode yet.
  const preparing = stage === "preparing_model";
  const label = preparing
    ? t("Preparing speech model")
    : voiceMode.mode === "selected_translate"
    ? t("Selected-text translation")
    : voiceMode.mode === "ask"
    ? t("Ask Anything")
    : voiceMode.mode === "edit"
    ? t("Editing")
    : phase === "injecting"
      ? stage === "updating_text"
        ? t("Updating text")
        : t("Inserting")
      : stage === "finishing_audio"
        ? t("Finishing audio")
        : stage === "transcribing" || stage === "retrying"
          ? t("Transcribing")
          : preview?.stage === "draft"
            ? t("Transcript ready")
            : preview?.stage === "streaming"
              ? t("AI correcting")
              : preview?.stage === "fallback"
                ? t("Using transcript")
                : t("Processing");

  return (
    <div className="recording-overlay processing" role="status" aria-live="polite">
      <span className="processing-spinner" aria-hidden="true" />
      <div className="processing-copy">
        <span className="recording-overlay-label">{label}</span>
        <span className={`correction-preview${compactPreview && !preparing ? "" : " pending"}`}>
          {preparing
            ? t("Starts when ready")
            : compactPreview || translateAppMessage(language, message, messageCode) || t("Preparing text…")}
        </span>
      </div>
      {phase === "processing" && cancelButton}
    </div>
  );
}

export default function App() {
  const [language, setLanguage] = useState<Settings["uiLanguage"] | null>(null);
  const [theme, setTheme] = useState<Settings["theme"]>(defaultSettings.theme);

  useEffect(() => {
    // The load and settings-changed events can arrive in any order; only a
    // snapshot newer than the last one applied changes the language or theme.
    let revision = -1;
    const apply = (snapshot: { settings: Pick<Settings, "uiLanguage" | "theme">; revision: number }) => {
      if (snapshot.revision <= revision) return;
      revision = snapshot.revision;
      setLanguage(snapshot.settings.uiLanguage);
      setTheme(snapshot.settings.theme);
    };
    getUiPreferences()
      .then(apply)
      .catch(() => setLanguage(defaultSettings.uiLanguage));
    let unlisten: (() => void) | undefined;
    let active = true;
    void listen<SettingsSnapshot>("settings-changed", ({ payload }) => apply(payload)).then((stop) => {
      if (active) unlisten = stop;
      else stop();
    });
    return () => {
      active = false;
      unlisten?.();
    };
  }, []);

  useEffect(() => {
    const root = document.documentElement;
    const media = window.matchMedia("(prefers-color-scheme: dark)");
    const apply = () => {
      root.dataset.theme = theme;
      root.dataset.systemTheme = media.matches ? "dark" : "light";
    };
    apply();
    media.addEventListener("change", apply);
    return () => media.removeEventListener("change", apply);
  }, [theme]);

  if (language === null) return null;
  return (
    <I18nProvider language={language}>
      {isRecordingOverlay ? (
        <RecordingOverlay />
      ) : isAskAnswer ? (
        <AskAnswerPanel />
      ) : (
        <MainAppContent onLanguageChange={setLanguage} />
      )}
    </I18nProvider>
  );
}

function MainAppContent({ onLanguageChange }: { onLanguageChange: (language: Settings["uiLanguage"]) => void }) {
  const { language, t } = useI18n();
  const [page, setPage] = useState<Page>("dashboard");
  const [state, setState] = useState<AppState>({
    phase: "idle",
    stage: "idle",
    operation: null,
    operationId: null,
    message: null,
    messageCode: null,
    lastResult: null,
    updatedAt: new Date().toISOString(),
  });
  const [history, setHistory] = useState<HistoryItem[]>([]);
  const [historyFilter, setHistoryFilter] = useState<HistoryFilter>("all");
  const historyFilterRef = useRef<HistoryFilter>("all");
  const historyRequestRef = useRef(0);
  const [historyQuery, setHistoryQuery] = useState("");
  const historyQueryRef = useRef("");
  const historyCursorRef = useRef<Pick<HistoryItem, "createdAt" | "id"> | null>(null);
  const historyLoadingRef = useRef(false);
  const [historyLoading, setHistoryLoading] = useState(false);
  const [historyFailed, setHistoryFailed] = useState(false);
  const [historyHasMore, setHistoryHasMore] = useState(false);
  const [historyReprocessing, setHistoryReprocessing] = useState(false);
  const [historyTextResult, setHistoryTextResult] = useState<{ sourceId: number; text: string } | null>(null);
  const historyReprocessingRef = useRef(false);
  const historyTextRequestRef = useRef(0);
  const historyTextPendingSourceRef = useRef<number | null>(null);
  const [model, setModel] = useState<ModelStatus | null>(null);
  const [modelLoading, setModelLoading] = useState(false);
  const [modelProgress, setModelProgress] = useState<ModelProgress | null>(null);
  const [progressNoticeHidden, setProgressNoticeHidden] = useState(false);
  const [recordingAction, setRecordingAction] = useState(false);
  const recordingActionRef = useRef(false);
  const [gpu, setGpu] = useState<GpuDiagnostics | null>(null);
  const [gpuChecking, setGpuChecking] = useState(false);
  const { notice, push: pushNotice, dismiss: dismissNotice, clear: clearNotices, removeKinds: removeNoticeKinds } = useNoticeQueue();
  const noticeOperationRef = useRef(0);
  const [startupHotkeyWarnings, setStartupHotkeyWarnings] = useState<string[]>([]);
  const [inactiveShortcuts, setInactiveShortcuts] = useState<string[]>([]);
  const showNotice = (message: string, severity: Notice["severity"] = "info") =>
    pushNotice({ message, severity });
  const { settings, settingsRef, settingsLoaded, editSettingsLocally, settingsReady, queueSettingsSave } = useSettings({
    onLanguageChange,
    onSetupNeeded: () => setPage("setup"),
    onError: (error) => showNotice(String(error), "error"),
    onNotReady: () => showNotice(t("Settings have not been loaded yet, so changes were not saved. Try again in a moment."), "warning"),
  });
  const [noticeCopied, setNoticeCopied] = useState(false);
  const { devices, refresh: refreshDevices } = useAudioDevices(page === "settings" || page === "setup", (error) => showNotice(String(error), "error"));
  useEffect(() => {
    let active = true;
    void getShortcutWarning().then((value) => { if (active) setInactiveShortcuts(value); }).catch(() => {});
    return () => { active = false; };
  }, [settings.shortcuts, settings.translationHotkey]);
  const [dictionary, setDictionary] = useState<DictionaryEntry[]>([]);
  const [dictionaryCandidates, setDictionaryCandidates] = useState<DictionaryCandidate[]>([]);
  const dictionaryRequestRef = useRef(0);

  useEffect(() => {
    historyFilterRef.current = historyFilter;
  }, [historyFilter]);

  useEffect(() => {
    void getStartupHotkeyWarning().then(setStartupHotkeyWarnings).catch(() => {});
    const applyAppState = (next: AppState) => {
      const id = next.operationId;
      if (typeof id === "number") {
        if (id < noticeOperationRef.current) return;
        if (id > noticeOperationRef.current) clearNotices();
        noticeOperationRef.current = id;
      } else if (noticeOperationRef.current > 0) return;
      setState(next);
    };
    Promise.all([
      getAppState(),
      listHistory(),
      getModelStatus(),
      getGpuDiagnostics(),
      listDictionary(),
    ])
      .then(
        ([nextState, nextHistory, nextModel, nextGpu, nextDictionary]) => {
          applyAppState(nextState);
          if (historyRequestRef.current === 0 && historyFilterRef.current === "all") {
            setHistory(nextHistory);
            const last = nextHistory.at(-1);
            historyCursorRef.current = last ? { createdAt: last.createdAt, id: last.id } : null;
            setHistoryHasMore(nextHistory.length === 100);
          }
          setModel(nextModel);
          setGpu(nextGpu);
          setDictionary(nextDictionary);
        },
      )
      .catch((error: unknown) => showNotice(String(error), "error"));
    const listeners = Promise.all([
      listen<AppState>("app-state", (event) => applyAppState(event.payload)),
      listen<ModelStatus>("model-status", (event) => {
        setModel(event.payload);
        setProgressNoticeHidden(false);
        // Preparation is over once the worker reports an outcome, so its
        // progress and its running commentary both stop here.
        if (event.payload.state !== "loading") {
          setModelProgress(null);
          removeNoticeKinds(MODEL_PREPARATION_KINDS);
        }
      }),
      listen<ModelProgress>("model-progress", (event) => setModelProgress(event.payload)),
      listen<GpuDiagnostics>("gpu-diagnostics", (event) => setGpu(event.payload)),
      listen<{ kind: string; code?: string; outcome?: string; message: string }>("status", (event) =>
        pushNotice({
          message: event.payload.message,
          code: event.payload.code,
          outcome: event.payload.outcome,
          severity: statusSeverity(event.payload.kind),
          kind: event.payload.kind,
        }),
      ),
    ]);
    return () => {
      void listeners.then((unlisten) => unlisten.forEach((fn) => fn()));
    };
  }, [onLanguageChange]);

  useEffect(() => {
    setNoticeCopied(false);
  }, [notice, startupHotkeyWarnings]);

  const statusLabel = t(phaseMessageKeys[state.phase]);
  const localizedState = useMemo(
    () => ({ ...state, message: translateAppMessage(language, state.message, state.messageCode) }),
    [language, state],
  );
  // A load started from the Models page or automatically at startup.
  const preparingModel = modelLoading || model?.state === "loading";
  // Each hotkey warning is a fixed backend sentence naming one action.
  const startupHotkeyWarning =
    startupHotkeyWarnings.length > 0
      ? startupHotkeyWarnings.map((message) => translateAppMessage(language, message)).join(" ")
      : null;
  const shownNotice: Notice | null = notice ?? (startupHotkeyWarning
    ? { message: startupHotkeyWarning, severity: "warning" }
    : null);
  const showingStartupWarning = !notice && Boolean(startupHotkeyWarning);
  const shownProgress = showingStartupWarning ? null : modelProgress;

  async function copyNotice() {
    if (!shownNotice) return;
    try {
      await copyToClipboard(translateAppMessage(language, shownNotice.message, shownNotice.code, shownNotice.outcome) ?? shownNotice.message);
      setNoticeCopied(true);
    } catch (error) {
      showNotice(String(error), "error");
      setNoticeCopied(false);
    }
  }

  async function copyHistoryText(text: string) {
    try {
      await copyToClipboard(text);
      showNotice(t("Copied"), "success");
    } catch (error) {
      showNotice(String(error), "error");
    }
  }

  async function refreshHistory(filter: HistoryFilter = historyFilterRef.current, append = false) {
    if (append && historyLoadingRef.current) return;
    const request = ++historyRequestRef.current;
    const query = historyQueryRef.current;
    const before = append ? historyCursorRef.current : null;
    historyLoadingRef.current = true;
    setHistoryLoading(true);
    setHistoryFailed(false);
    try {
      const { items, retainedIds } = await listHistoryPage(filter, { query, before, knownIds: append ? history.map((item) => item.id) : [] });
      if (request === historyRequestRef.current) {
        setHistory((previous) => {
          if (!append) return items;
          const retained = new Set(retainedIds);
          const kept = previous.filter((item) => retained.has(item.id));
          const existingIds = new Set(kept.map((item) => item.id));
          return [...kept, ...items.filter((item) => !existingIds.has(item.id))];
        });
        const last = items.at(-1);
        historyCursorRef.current = last ? { createdAt: last.createdAt, id: last.id } : null;
        setHistoryHasMore(items.length === 100);
      }
    } catch (error) {
      if (request === historyRequestRef.current) setHistoryFailed(true);
      throw error;
    } finally {
      if (request === historyRequestRef.current) {
        historyLoadingRef.current = false;
        setHistoryLoading(false);
      }
    }
  }

  async function refreshDictionary() {
    const request = ++dictionaryRequestRef.current;
    const [entries, candidates] = await Promise.all([listDictionary(), listDictionaryCandidates()]);
    if (request === dictionaryRequestRef.current) {
      setDictionary(entries);
      setDictionaryCandidates(candidates);
    }
  }

  useEffect(() => {
    // Shortcut-driven recordings finish through app-state events, without the
    // Dashboard stop handler. Read the retained data when either page opens
    // and after lifecycle or retention changes while it is visible.
    let active = true;
    if (page === "history") {
      void refreshHistory().catch((error) => { if (active) showNotice(String(error), "error"); });
    }
    if (page === "dictionary") {
      setDictionaryCandidates([]);
      void refreshDictionary().catch((error) => { if (active) showNotice(String(error), "error"); });
    }
    return () => {
      active = false;
      historyRequestRef.current += 1;
      dictionaryRequestRef.current += 1;
    };
  }, [page, state.phase, settings.historyRetention]);

  async function saveSettings(patch: Partial<Settings>): Promise<boolean> {
    if (!settingsReady()) return false;
    const outcome = await queueSettingsSave(patch);
    if (!outcome.saved) {
      showNotice(String(outcome.error), "error");
      return false;
    }
    const saved = outcome.settings;
    let hotkeyWarnings: string[] = [];
    try {
      hotkeyWarnings = await getStartupHotkeyWarning();
      setStartupHotkeyWarnings(hotkeyWarnings);
    } catch {
      // Keep the known warning if diagnostics cannot be refreshed.
    }
    // A saved hotkey that still cannot be registered does not fail the save,
    // but the notice names the action that stays unavailable.
    if (hotkeyWarnings.length > 0) showNotice(hotkeyWarnings.map((message) => translateAppMessage(saved.uiLanguage, message)).join(" "), "warning");
    if (patch.historyRetention !== undefined || patch.deleteAudioAfterProcessing !== undefined) {
      // The optimistic render can read before the settings transaction has
      // purged old rows. Refresh again after its acknowledgement so that read
      // cannot restore candidates removed by a shorter retention window.
      try {
        setDictionaryCandidates([]);
        await Promise.all([refreshHistory(), refreshDictionary()]);
      }
      catch (error) { showNotice(String(error), "error"); }
    }
    return true;
  }

  async function openLogs() {
    try { await openDiagnosticsFolder(); }
    catch { showNotice(t("The diagnostics folder could not be opened."), "error"); }
  }

  async function diagnoseGpu() {
    if (gpuChecking) return;
    setGpuChecking(true);
    showNotice(t("Running local GPU diagnostics..."));
    try {
      setGpu(await runGpuDiagnostics());
      showNotice(t("Diagnostics complete."), "success");
    } catch (error) {
      showNotice(String(error), "error");
    } finally {
      setGpuChecking(false);
    }
  }

  async function toggleRecording() {
    if (recordingActionRef.current) return;
    recordingActionRef.current = true;
    setRecordingAction(true);
    try {
      if (state.phase === "recording") {
        await stopRecording();
        try {
          await refreshHistory();
        } catch {
          showNotice(t("Recording completed, but history could not be refreshed."), "warning");
        }
      } else {
        await startRecording();
      }
    } catch (error) {
      showNotice(String(error), "error");
      try {
        setState(await getAppState());
      } catch {
        // The status event normally keeps this synchronized. If refreshing
        // fails, preserve the latest event-driven state.
      }
    } finally {
      recordingActionRef.current = false;
      setRecordingAction(false);
    }
  }

  async function prepareModel(
    configuration?: Pick<
      Settings,
      | "asrBackend"
      | "modelId"
      | "modelQuantization"
      | "apiBaseUrl"
      | "apiKeyEnvVar"
    >,
  ) {
    if (modelLoading || !settingsReady()) return;
    setModelLoading(true);
    showNotice(t("Saving ASR settings and preparing the selected model..."));
    try {
      // Flush pending Setup edits as well as explicit model configuration.
      const outcome = await queueSettingsSave(configuration ?? {});
      if (!outcome.saved) throw outcome.error;
      const saved = outcome.settings;
      setModel(await loadModel(saved.modelId, saved.modelQuantization));
      showNotice(t("Model loaded and ready."), "success");
    } catch (error) {
      showNotice(String(error), "error");
    } finally {
      setModelLoading(false);
    }
  }

  async function saveCustomModel(customModel: CustomModel): Promise<boolean> {
    if (!settingsReady()) return false;
    const customModels = [
      ...settingsRef.current.customModels.filter(
        (saved) =>
          saved.asrBackend !== customModel.asrBackend || saved.modelId !== customModel.modelId,
      ),
      customModel,
    ];
    const outcome = await queueSettingsSave({
      asrBackend: customModel.asrBackend,
      modelId: customModel.modelId,
      customModels,
    });
    if (!outcome.saved) {
      showNotice(String(outcome.error), "error");
      return false;
    }
    showNotice(t("Custom model saved locally."), "success");
    return true;
  }

  async function addDictionary(entry: DictionaryEntryInput): Promise<boolean> {
    try {
      await addDictionaryEntry(entry);
      setDictionary(await listDictionary());
      showNotice(t("Dictionary entry added."), "success");
      return true;
    } catch (error) {
      showNotice(String(error), "error");
      return false;
    }
  }

  async function removeDictionary(id: number) {
    try {
      await deleteDictionaryEntry(id);
      setDictionary(await listDictionary());
      showNotice(t("Dictionary entry removed."), "success");
    } catch (error) {
      showNotice(String(error), "error");
    }
  }

  async function removeHistory(id: number) {
    try {
      await deleteHistoryItem(id);
      if (historyTextPendingSourceRef.current === id) historyTextRequestRef.current += 1;
      setHistoryTextResult(null);
      await refreshHistory();
    }
    catch (error) { showNotice(String(error), "error"); }
  }

  async function clearHistory() {
    try {
      await deleteAllHistory();
      historyRequestRef.current += 1;
      setHistory([]);
      historyCursorRef.current = null;
      setHistoryHasMore(false);
      historyLoadingRef.current = false;
      setHistoryLoading(false);
      setHistoryFailed(false);
      setHistoryTextResult(null);
      historyTextRequestRef.current += 1;
      showNotice(t("History deleted."), "success");
    }
    catch (error) { showNotice(String(error), "error"); }
  }

  async function reportHistoryAudioFailure() {
    showNotice(t("The recording could not be loaded. History was refreshed."), "error");
    try { await refreshHistory(); }
    catch { /* The notice already reports the unavailable recording. */ }
  }

  async function retryHistory(id: number) {
    try {
      await retryHistoryItem(id);
      await refreshHistory();
      showNotice(t("History retry completed."), "success");
    } catch (error) { showNotice(String(error), "error"); }
  }

  async function reprocessHistory(id: number, action: import("./types").HistoryTextAction) {
    if (historyReprocessingRef.current) return;
    historyReprocessingRef.current = true;
    const request = ++historyTextRequestRef.current;
    historyTextPendingSourceRef.current = id;
    setHistoryReprocessing(true);
    setHistoryTextResult(null);
    try {
      const result = await reprocessHistoryText(id, action);
      if (request !== historyTextRequestRef.current) return;
      setHistoryTextResult({ sourceId: id, text: result.text });
      await refreshHistory();
      if (request !== historyTextRequestRef.current) return;
      showNotice(t("History retry completed."), "success");
    } catch (error) { if (request === historyTextRequestRef.current) showNotice(String(error), "error"); }
    finally { historyReprocessingRef.current = false; historyTextPendingSourceRef.current = null; setHistoryReprocessing(false); }
  }

  async function updateDictionary(id: number, entry: DictionaryEntryInput): Promise<boolean> {
    try {
      await updateDictionaryEntry(id, entry);
      setDictionary(await listDictionary());
      showNotice(t("Dictionary entry updated."), "success");
      return true;
    } catch (error) {
      showNotice(String(error), "error");
      return false;
    }
  }

  async function importDictionary(csv: string): Promise<boolean> {
    try {
      const count = await importDictionaryCsv(csv);
      setDictionary(await listDictionary());
      showNotice(`${t("Imported dictionary entries.")}: ${count}`, "success");
      return true;
    } catch (error) {
      showNotice(String(error), "error");
      return false;
    }
  }

  async function confirmCandidate(id: number) {
    try {
      await confirmDictionaryCandidate(id);
      await refreshDictionary();
      showNotice(t("Dictionary candidate confirmed."), "success");
    } catch (error) { showNotice(String(error), "error"); }
  }

  async function rejectCandidate(id: number) {
    try {
      await rejectDictionaryCandidate(id);
      await refreshDictionary();
    } catch (error) { showNotice(String(error), "error"); }
  }

  return (
    <div className="app-shell">
      <aside>
        <div className="brand">
          <span className="brand-mark">LV</span>
          <div>
            <strong>{t("Local Voice")}</strong>
            <small>{t("Windows input")}</small>
          </div>
        </div>
        <nav>
          {pages.map((item) => (
            <button
              key={item.id}
              className={page === item.id ? "active" : ""}
              aria-current={page === item.id ? "page" : undefined}
              onClick={() => setPage(item.id)}
            >
              {t(item.label)}
            </button>
          ))}
        </nav>
        <div className="local-badge">
          <span /> {t("Local processing default")}
        </div>
      </aside>

      <main>
        <header>
          <div>
            <p className="eyebrow">{t("LOCAL AI VOICE INPUT")}</p>
            <h1>{t(pages.find((item) => item.id === page)?.label ?? "Status")}</h1>
          </div>
          <div className={`status-pill ${state.phase}`}>
            <span />
            {statusLabel}
          </div>
        </header>

        {page === "dashboard" && (
          <DashboardPage
            state={localizedState}
            settings={settings}
            history={history}
            model={model}
            statusLabel={statusLabel}
            recordingAction={recordingAction}
            onToggleRecording={() => void toggleRecording()}
            onCancelRecording={() => void cancelRecording()}
            onCopyResult={(text) => void copyHistoryText(text)}
            inactiveShortcuts={inactiveShortcuts}
          />
        )}

        {page === "setup" && (
          <SetupPage
            appState={state}
            testsDisabled={!settingsLoaded}
            onPrepareTest={() => saveSettings({})}
            settings={settings}
            devices={devices}
            onRefreshDevices={() => void refreshDevices()}
            gpu={gpu}
            gpuChecking={gpuChecking}
            onSettingsChange={editSettingsLocally}
            onConfigureModel={() => setPage("models")}
            onLoadModel={() => void prepareModel()}
            modelLoading={preparingModel}
            modelReady={model?.state === "ready" && model.quantization === settings.modelQuantization}
            progress={modelProgress}
            onDiagnoseGpu={() => void diagnoseGpu()}
            onFinish={() => {
              void saveSettings({ setupComplete: true }).then((saved) => { if (saved) setPage("dashboard"); });
            }}
          />
        )}

        {page === "settings" && (
        <SettingsPage
            settings={settings}
            devices={devices}
            onRefreshDevices={() => void refreshDevices()}
            recording={state.phase === "recording"}
            onSave={(patch) => void saveSettings(patch)}
          />
        )}

        {page === "models" && (
          <ModelsPage
            onOpenLogs={() => void openLogs()}
            gpu={gpu}
            settings={settings}
            asrBackendOptions={asrBackendOptions}
            modelLoading={modelLoading}
            model={model}
            progress={modelProgress}
            onConfigureModel={(configuration) => void prepareModel(configuration)}
            onSaveCustomModel={saveCustomModel}
            onDiagnoseGpu={() => void diagnoseGpu()}
          />
        )}

        {page === "history" && (
          <HistoryPage
            settings={settings}
            settingsLoaded={settingsLoaded}
            history={history}
            filter={historyFilter}
            query={historyQuery}
            loading={historyLoading}
            failed={historyFailed}
            hasMore={historyHasMore}
            onSearch={(query) => {
              setHistoryQuery(query);
              historyQueryRef.current = query;
              setHistory([]);
              setHistoryHasMore(false);
              void refreshHistory().catch((error) => showNotice(String(error), "error"));
            }}
            onLoadMore={() => void refreshHistory(historyFilterRef.current, true).catch((error) => showNotice(String(error), "error"))}
            onSave={(patch) => void saveSettings(patch)}
            onCopyItem={(text) => void copyHistoryText(text)}
            onFilter={(filter) => {
              setHistoryFilter(filter);
              historyFilterRef.current = filter;
              setHistory([]);
              setHistoryHasMore(false);
              void refreshHistory(filter).catch((error) => showNotice(String(error), "error"));
            }}
            onRetry={(id) => void retryHistory(id)}
            onReprocess={(id, action) => void reprocessHistory(id, action)}
            reprocessedText={historyTextResult?.text}
            onDismissResult={() => setHistoryTextResult(null)}
            reprocessing={historyReprocessing || state.phase === "recording" || state.phase === "processing" || state.phase === "injecting"}
            onDelete={(id) => void removeHistory(id)}
            onDeleteAll={() => void clearHistory()}
            onLoadAudio={getHistoryAudio}
            onAudioError={() => void reportHistoryAudioFailure()}
            retryActive={state.operation === "retry" && (state.phase === "processing" || state.phase === "injecting")}
            onCancelRetry={() => void cancelRecording()}
          />
        )}

        {page === "dictionary" && (
          <DictionaryPage
            entries={dictionary}
            candidates={settings.historyRetention === "never" ? [] : dictionaryCandidates}
            onAdd={addDictionary}
            onUpdate={updateDictionary}
            onDelete={(id) => void removeDictionary(id)}
            onImport={importDictionary}
            onConfirmCandidate={(id) => void confirmCandidate(id)}
            onRejectCandidate={(id) => void rejectCandidate(id)}
          />
        )}

        {page === "privacy" && (
          <PrivacyPage settings={settings} settingsLoaded={settingsLoaded} onSave={(patch) => void saveSettings(patch)} />
        )}

        {page === "diagnostics" && (
          <DiagnosticsPage
            onOpenLogs={() => void openLogs()}
            state={localizedState}
            gpu={gpu}
            statusLabel={statusLabel}
            onDiagnoseGpu={() => void diagnoseGpu()}
          />
        )}
      </main>

      {(shownNotice || (shownProgress && !progressNoticeHidden)) && (
        <div
          className={`notice ${shownNotice?.severity ?? "info"}${preparingModel && !showingStartupWarning ? " loading" : ""}`}
          aria-live="polite"
          role={shownNotice?.severity === "error" ? "alert" : "status"}
        >
          {preparingModel && !showingStartupWarning && <span className="progress-ring" aria-hidden="true" />}
          <div className="notice-body">
            <button
              className="notice-message"
              onDoubleClick={() => void copyNotice()}
              title={t("Double-click to copy")}
            >
              {translateAppMessage(language, shownNotice?.message ?? null, shownNotice?.code, shownNotice?.outcome) ??
                (shownProgress?.stage === "download"
                  ? t("Downloading the speech model files.")
                  : t("Loading the speech model."))}
            </button>
            {shownProgress && !progressNoticeHidden && <ModelProgressBar progress={shownProgress} />}
          </div>
          <button type="button" className="secondary" onClick={() => void copyNotice()} aria-label={t("Copy notification")}>{t("Copy")}</button>
          {noticeCopied && <span className="notice-copied">{t("Copied")}</span>}
          <button
            className="notice-dismiss"
            onClick={() => {
              if (preparingModel) setProgressNoticeHidden(true);
              if (notice) {
                dismissNotice();
              } else setStartupHotkeyWarnings([]);
            }}
            aria-label={t("Dismiss notification")}
          >
            ×
          </button>
        </div>
      )}
    </div>
  );
}
